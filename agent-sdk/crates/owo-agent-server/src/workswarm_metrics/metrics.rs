use super::sanitize::*;
use super::util::*;
use async_trait::async_trait;
use owo_agent_core::gateway::{ModelProvider, TokenUsage};
use owo_agent_core::goal::Worker;
use owo_agent_core::workswarm::{TeamCoordinator, WorkSwarmError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 单次 worker 执行（span）指标记录。一条 JSONL 行 = 一个 span。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpanRecord {
    pub span_id: String,
    pub team_id: String,
    pub member_id: String,
    pub role: String,
    /// 内层 worker 类型（agent / echo / sleep / fail…）。
    pub worker_kind: String,
    pub step_id: String,
    pub started_at: String,
    pub ended_at: String,
    #[serde(default)]
    pub started_at_ms: u64,
    #[serde(default)]
    pub ended_at_ms: u64,
    pub wall_ms: u64,
    /// succeeded | failed
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 本次执行的模型调用次数（MeasuredProvider 精确计数；非模型 worker 为 0）。
    #[serde(default)]
    pub model_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub cost_usd: f64,
    /// 该步骤的第几次尝试（1 起；>1 = 返工/重试 span）。
    #[serde(default)]
    pub attempt: u32,
    /// 成功后解析到的本次登记输出 Artifact（版本化）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<SpanArtifact>,
}

/// span 关联的输出 Artifact 摘要（不含内容；内容经 CAS ref 另行读取）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpanArtifact {
    pub artifact_id: String,
    pub kind: String,
    pub version: u32,
}

// ---------------------------------------------------------------------------
// JSONL 指标日志（TeamRun 数据目录持久化；重启可读）
// ---------------------------------------------------------------------------

/// TeamRun 指标日志：`<run_dir>/<team_id>-metrics.jsonl`（追加写；读侧逐行解析，坏行跳过）。
#[derive(Clone)]
pub struct MetricsJournal {
    path: Arc<PathBuf>,
    /// 追加写串行化（并发 span 落盘互斥）。
    write_lock: Arc<Mutex<()>>,
}

impl MetricsJournal {
    pub fn for_team(run_dir: &Path, team_id: &str) -> Self {
        Self {
            path: Arc::new(run_dir.join(format!("{team_id}-metrics.jsonl"))),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 追加一条 span 记录（目录不存在自动创建；写失败由调用方记日志不中断运行）。
    pub fn append(&self, record: &WorkerSpanRecord) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(record)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*self.path)?;
        file.write_all(line.as_bytes())
    }

    /// 读取全部 span 记录（逐行解析；坏行/半行跳过——崩溃中断写安全）。
    pub fn read_records(&self) -> Vec<WorkerSpanRecord> {
        let Ok(text) = std::fs::read_to_string(&*self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    return None;
                }
                serde_json::from_str(trimmed).ok()
            })
            .collect()
    }

    /// 指定步骤已有的 span 数（尝试序数 = 已有数 + 1）。
    pub(crate) fn count_step_spans(&self, step_id: &str) -> u32 {
        self.read_records()
            .iter()
            .filter(|r| r.step_id == step_id)
            .count() as u32
    }
}

// ---------------------------------------------------------------------------
// 模型调用计数装饰器（per-span 精确 model_calls）
// ---------------------------------------------------------------------------

/// ModelProvider 计数装饰器：`complete`/`complete_stream` 各计一次（每次模型调用
/// 恰好经过其一），`usage_snapshot` 透传内层（token 快照差值归因不受影响）。
pub struct MeasuredProvider {
    inner: Arc<dyn ModelProvider>,
    calls: Arc<AtomicU64>,
}

impl MeasuredProvider {
    pub fn new(inner: Arc<dyn ModelProvider>, calls: Arc<AtomicU64>) -> Self {
        Self { inner, calls }
    }
}

#[async_trait]
impl ModelProvider for MeasuredProvider {
    async fn complete(
        &self,
        messages: &[owo_agent_core::ChatMessage],
        tools: &[owo_agent_core::tools::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.complete(messages, tools).await
    }

    async fn complete_stream(
        &self,
        messages: &[owo_agent_core::ChatMessage],
        tools: &[owo_agent_core::tools::ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<owo_agent_core::ModelOutput, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inner.complete_stream(messages, tools, on_delta).await
    }

    fn usage_snapshot(&self) -> TokenUsage {
        self.inner.usage_snapshot()
    }
}

// ---------------------------------------------------------------------------
// MeasuredRoleWorker（角色 worker 指标包装层）
// ---------------------------------------------------------------------------

/// 角色 worker 包装层：span 级起止/墙钟/终态/失败原因/尝试序数/输出 Artifact，
/// 以及 model_calls（MeasuredProvider 计数）与 token/费用（provider 快照差值）。
/// 指标追加落盘到 [`MetricsJournal`]，写失败仅记 tracing 警告（不中断运行）。
pub struct MeasuredRoleWorker {
    inner: Arc<dyn Worker>,
    coordinator: Arc<TeamCoordinator>,
    journal: MetricsJournal,
    team_id: String,
    member_id: String,
    role: String,
    worker_kind: String,
    /// agent 角色 = 共享 provider（token 快照差值归因）；内置演示 worker = None。
    provider: Option<Arc<dyn ModelProvider>>,
    /// agent 角色 = per-span 模型调用计数（由 AgentSubagentWorker 递增）。
    model_calls: Option<Arc<AtomicU64>>,
}

impl MeasuredRoleWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        inner: Arc<dyn Worker>,
        coordinator: Arc<TeamCoordinator>,
        journal: MetricsJournal,
        team_id: String,
        member_id: String,
        role: String,
        worker_kind: String,
        provider: Option<Arc<dyn ModelProvider>>,
        model_calls: Option<Arc<AtomicU64>>,
    ) -> Self {
        Self {
            inner,
            coordinator,
            journal,
            team_id,
            member_id,
            role,
            worker_kind,
            provider,
            model_calls,
        }
    }

    /// 成功后解析本次登记的输出 Artifact（该角色当前最高版本）。
    async fn resolve_output_artifact(&self) -> Option<SpanArtifact> {
        let team = self.coordinator.get_team_run(&self.team_id).await.ok()?;
        let space_id = team.project_space_id.as_deref()?;
        let space = self.coordinator.get_project_space(space_id).await.ok()?;
        let artifacts = self.coordinator.list_artifacts(&space).await.ok()?;
        artifacts
            .iter()
            .filter(|a| a.producer == self.member_id)
            .max_by_key(|a| a.version)
            .map(|a| SpanArtifact {
                artifact_id: a.artifact_id.clone(),
                kind: a.kind.clone(),
                version: a.version,
            })
    }
}

#[async_trait]
impl Worker for MeasuredRoleWorker {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let step_id = input
            .get("_workswarm")
            .and_then(|w| w.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let attempt = self.journal.count_step_spans(&step_id).saturating_add(1);
        let started_at_ms = now_ms();
        let started_at = rfc3339();
        let usage_before = self.provider.as_ref().map(|p| p.usage_snapshot());
        let started = Instant::now();

        let result = self.inner.run(input).await;

        let wall_ms = started.elapsed().as_millis() as u64;
        let ended_at_ms = now_ms();
        let ended_at = rfc3339();
        let usage_delta = match (&usage_before, self.provider.as_ref()) {
            (Some(before), Some(provider)) => {
                let after = provider.usage_snapshot();
                let delta = after.saturating_sub(before);
                // 并发批次下快照差值可能为 0 但确实有调用（无 usage 上报的 provider）：
                // 仅有调用且差值为零时也记 None，避免把「未知用量」伪装成「零用量」。
                (delta.total_tokens > 0 || delta.prompt_tokens > 0 || delta.completion_tokens > 0)
                    .then_some(delta)
            }
            _ => None,
        };
        let model_calls = self
            .model_calls
            .as_ref()
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0);
        let cost_usd = usage_delta
            .as_ref()
            .map(|d| estimate_cost_usd(d.prompt_tokens, d.completion_tokens))
            .unwrap_or(0.0);
        let (outcome, error) = match &result {
            Ok(_) => ("succeeded", None),
            Err(e) => ("failed", Some(sanitize_text(&truncate_chars(e, 500)))),
        };
        let artifact = if result.is_ok() {
            self.resolve_output_artifact().await
        } else {
            None
        };
        let record = WorkerSpanRecord {
            span_id: format!("span-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            team_id: self.team_id.clone(),
            member_id: self.member_id.clone(),
            role: self.role.clone(),
            worker_kind: self.worker_kind.clone(),
            step_id,
            started_at,
            ended_at,
            started_at_ms,
            ended_at_ms,
            wall_ms,
            outcome: outcome.to_string(),
            error,
            model_calls,
            prompt_tokens: usage_delta.as_ref().map(|d| d.prompt_tokens),
            completion_tokens: usage_delta.as_ref().map(|d| d.completion_tokens),
            total_tokens: usage_delta.as_ref().map(|d| d.total_tokens),
            cost_usd,
            attempt,
            artifact,
        };
        if let Err(e) = self.journal.append(&record) {
            tracing::warn!(
                team_id = %self.team_id,
                role = %self.role,
                error = %e,
                "workswarm 指标落盘失败（不影响运行）"
            );
        }
        result
    }
}

// ---------------------------------------------------------------------------
// 聚合与预算
// ---------------------------------------------------------------------------

/// 团队指标聚合：summary（总览）/ roles（按角色）/ workers（span 明细）/ budget（预算状态）。
pub fn aggregate_metrics(team_id: &str, records: &[WorkerSpanRecord], budget: &Value) -> Value {
    let now = now_ms();
    let mut spans = records.to_vec();
    spans.sort_by_key(|r| (r.started_at_ms, r.span_id.clone()));

    let span_count = spans.len() as u64;
    let succeeded_spans = spans.iter().filter(|r| r.outcome == "succeeded").count() as u64;
    let failed_spans = spans.iter().filter(|r| r.outcome == "failed").count() as u64;
    let rework_count = spans.iter().filter(|r| r.attempt > 1).count() as u64;
    let worker_wall_ms_sum: u64 = spans.iter().map(|r| r.wall_ms).sum();
    let wall_window_ms = match spans.first() {
        Some(first) => now.saturating_sub(first.started_at_ms),
        None => 0,
    };
    let model_calls: u64 = spans.iter().map(|r| r.model_calls).sum();
    let prompt_tokens = sum_opt(spans.iter().filter_map(|r| r.prompt_tokens));
    let completion_tokens = sum_opt(spans.iter().filter_map(|r| r.completion_tokens));
    let total_tokens = sum_opt(spans.iter().filter_map(|r| r.total_tokens));
    let cost_usd = round6(spans.iter().map(|r| r.cost_usd).sum());
    let slowest = spans
        .iter()
        .max_by_key(|r| (r.wall_ms, r.span_id.clone()))
        .map(|r| {
            json!({
                "span_id": r.span_id,
                "role": r.role,
                "step_id": r.step_id,
                "wall_ms": r.wall_ms,
            })
        })
        .unwrap_or(Value::Null);
    let artifact_versions: BTreeSet<&str> = spans
        .iter()
        .filter_map(|r| r.artifact.as_ref().map(|a| a.artifact_id.as_str()))
        .collect();

    // 按角色聚合（BTreeMap = 角色名稳定排序）。
    #[derive(Default)]
    struct RoleAgg {
        spans: u64,
        succeeded: u64,
        failed: u64,
        rework: u64,
        wall_ms_sum: u64,
        model_calls: u64,
        prompt_tokens: Option<u64>,
        completion_tokens: Option<u64>,
        total_tokens: Option<u64>,
        cost_micro: u64,
        artifacts: BTreeSet<String>,
    }
    let mut roles: BTreeMap<String, RoleAgg> = BTreeMap::new();
    for r in &spans {
        let agg = roles.entry(r.role.clone()).or_default();
        agg.spans += 1;
        if r.outcome == "succeeded" {
            agg.succeeded += 1;
        } else {
            agg.failed += 1;
        }
        if r.attempt > 1 {
            agg.rework += 1;
        }
        agg.wall_ms_sum += r.wall_ms;
        agg.model_calls += r.model_calls;
        agg.prompt_tokens = add_opt(agg.prompt_tokens, r.prompt_tokens);
        agg.completion_tokens = add_opt(agg.completion_tokens, r.completion_tokens);
        agg.total_tokens = add_opt(agg.total_tokens, r.total_tokens);
        agg.cost_micro += (r.cost_usd * 1_000_000.0).round() as u64;
        if let Some(a) = &r.artifact {
            agg.artifacts.insert(a.artifact_id.clone());
        }
    }
    let roles_json: Vec<Value> = roles
        .into_iter()
        .map(|(role, agg)| {
            json!({
                "role": role,
                "spans": agg.spans,
                "succeeded": agg.succeeded,
                "failed": agg.failed,
                "rework": agg.rework,
                "wall_ms_sum": agg.wall_ms_sum,
                "model_calls": agg.model_calls,
                "prompt_tokens": agg.prompt_tokens,
                "completion_tokens": agg.completion_tokens,
                "total_tokens": agg.total_tokens,
                "cost_usd": round6(agg.cost_micro as f64 / 1_000_000.0),
                "artifact_versions": agg.artifacts.len(),
            })
        })
        .collect();

    let workers_json: Vec<Value> = spans
        .iter()
        .map(|r| serde_json::to_value(r).unwrap_or_else(|_| json!({})))
        .collect();

    json!({
        "team_id": team_id,
        "generated_at": rfc3339(),
        "attribution_note": "token/费用按共享 provider 快照差值归因：串行链精确，并发批次下逐角色近似（团队总量守恒）；model_calls 逐 span 精确",
        "summary": {
            "span_count": span_count,
            "succeeded_spans": succeeded_spans,
            "failed_spans": failed_spans,
            "rework_count": rework_count,
            "wall_window_ms": wall_window_ms,
            "worker_wall_ms_sum": worker_wall_ms_sum,
            "model_calls": model_calls,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
            "cost_usd": cost_usd,
            "slowest_worker": slowest,
            "artifact_versions": artifact_versions.len(),
        },
        "roles": roles_json,
        "workers": workers_json,
        "budget": budget_state(budget, records, now),
    })
}

/// 预算状态（对比 TeamRun.budget 的 additive 扩展字段与当前累计指标）。
pub fn budget_state(budget: &Value, records: &[WorkerSpanRecord], now_ms: u64) -> Value {
    let spent = round6(records.iter().map(|r| r.cost_usd).sum());
    let window = records
        .iter()
        .map(|r| r.started_at_ms)
        .min()
        .map(|first| now_ms.saturating_sub(first))
        .unwrap_or(0);
    let reason = budget_exhaustion_reason(budget, records, now_ms);
    json!({
        "max_cost_usd": budget.get("max_cost_usd").and_then(Value::as_f64),
        "spent_usd": spent,
        "max_wall_secs": budget.get("max_wall_secs").and_then(Value::as_u64),
        "wall_window_ms": window,
        "exceeded": reason.is_some(),
        "reason": reason,
    })
}

/// 预算耗尽判定（运行循环预算门 + metrics budget.reason 复查共用）。
///
/// `max_cost_usd`（f64）：累计估算费用 `spent > limit` → 耗尽；
/// `max_wall_secs`（u64，>0 生效）：团队指标窗口（首 span 起 → 现在）超限 → 耗尽；
/// 未配置 → None（不启用该门；与既有 GoalBudget 步数/重试熔断正交互补）。
pub fn budget_exhaustion_reason(
    budget: &Value,
    records: &[WorkerSpanRecord],
    now_ms: u64,
) -> Option<String> {
    if let Some(limit) = budget.get("max_cost_usd").and_then(Value::as_f64) {
        let spent: f64 = records.iter().map(|r| r.cost_usd).sum();
        if spent > limit {
            return Some(format!(
                "费用预算耗尽：累计估算 {spent:.6} USD > 上限 {limit:.6} USD（已停止调度下一阶段；可调整预算后 continue 恢复）"
            ));
        }
    }
    if let Some(secs) = budget
        .get("max_wall_secs")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
    {
        let first = records.iter().map(|r| r.started_at_ms).min();
        if let Some(first) = first {
            let window_ms = now_ms.saturating_sub(first);
            if window_ms > secs.saturating_mul(1000) {
                return Some(format!(
                    "墙钟预算耗尽：团队已运行 {:.1}s > 上限 {secs}s（已停止调度下一阶段；可调整预算后 continue 恢复）",
                    window_ms as f64 / 1000.0
                ));
            }
        }
    }
    None
}

/// 运行循环预算门：读 TeamRun 预算 + 指标日志，返回耗尽原因（None = 继续调度）。
pub async fn team_budget_exhaustion(
    coordinator: &Arc<TeamCoordinator>,
    team_id: &str,
) -> Result<Option<String>, WorkSwarmError> {
    let team = coordinator.get_team_run(team_id).await?;
    let journal = MetricsJournal::for_team(coordinator.run_dir(), team_id);
    let records = journal.read_records();
    Ok(budget_exhaustion_reason(&team.budget, &records, now_ms()))
}
