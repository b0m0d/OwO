//! WorkSwarm TeamRun 指标、预算与脱敏（五期 · 第三路）。
//!
//! 组成：
//! - [`MeasuredRoleWorker`]：角色 worker 包装层（在 [`crate::workswarm_api`] 的
//!   `build_run_registry` 包装 `RoleWorker`）。记录每次执行的开始/结束时间、墙钟、
//!   终态与失败原因、尝试序数、输出 Artifact；`model_calls` 经
//!   [`MeasuredProvider`] 逐 span 精确计数，token 按共享 provider 累计快照差值归因。
//! - [`MetricsJournal`]：指标以 JSONL 追加落盘到 TeamRun 数据目录
//!   （`<run_dir>/<team_id>-metrics.jsonl`，与 `<team_id>-meta.json` / 运行状态文件同目录），
//!   重启后仍可读取（端点每次从文件聚合，无进程内账本）。
//! - 聚合与预算：[`aggregate_metrics`] 产出团队/角色两级汇总；预算语义在
//!   `TeamRun.budget` JSON 上做 **additive** 扩展（可选 `max_cost_usd` / `max_wall_secs`，
//!   此前未知键被忽略），超限 → 运行循环停止调度下一阶段（见 [`crate::workswarm_api`]）。
//! - 脱敏：[`sanitize_text`] / [`sanitize_value`] 统一脱敏诊断导出
//!   （凭据类键值、bearer/sk- 等令牌、超长文本截断）。
//!
//! 归因口径（诚实备案）：token/费用按共享 provider 的累计快照差值归因到当前
//! worker span——relay 串行链下精确；`max_parallel` 并发批次下逐角色归因为近似
//! （团队总量守恒）。`model_calls` 经 per-span 计数装饰器精确统计，不受并发影响。

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
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// 指标记录（JSONL 行结构）
// ---------------------------------------------------------------------------

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
    fn count_step_spans(&self, step_id: &str) -> u32 {
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

// ---------------------------------------------------------------------------
// 脱敏（诊断导出统一入口；无 regex 依赖的词级扫描）
// ---------------------------------------------------------------------------

/// 凭据类键名单（词级匹配；`*_tokens` 复数 = 用量计数，明确排除）。
const SENSITIVE_KEY_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "authorization",
];

const SENSITIVE_KEY_COMPOUNDS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "auth_header",
    "bearer_token",
];

const SECRET_TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "github_pat_",
    "xoxb-",
    "xoxa-",
    "xoxp-",
    "pat_",
    "npm_",
    "shpat_",
    "gl-",
];

/// 键名是否凭据类（`prompt_tokens`/`completion_tokens`/`total_tokens` 等用量复数不命中）。
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.trim().to_lowercase();
    if lower.is_empty() || lower.ends_with("tokens") {
        return false;
    }
    if SENSITIVE_KEY_COMPOUNDS.iter().any(|c| lower.contains(c)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| SENSITIVE_KEY_WORDS.contains(&word))
}

/// 词级脱敏：键值赋值（key=value / key:value / key：value）、令牌前缀词
/// （sk-/ghp_/xox*/gl-/pat_…）、`Bearer` 后随词；`*_tokens` 计数不受影响。
pub fn sanitize_text(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    let mut redact_next = false;
    while !rest.is_empty() {
        let lead = match rest.find(|c: char| !c.is_whitespace()) {
            Some(i) => i,
            None => {
                result.push_str(rest);
                break;
            }
        };
        result.push_str(&rest[..lead]);
        rest = &rest[lead..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        rest = tail;

        let (redacted, hunt_next) = sanitize_word(word, redact_next);
        result.push_str(&redacted);
        redact_next = hunt_next;
    }
    truncate_chars(&result, 400)
}

/// 处理单个词：返回 (输出词, 是否脱敏下一个词)。
fn sanitize_word(word: &str, redact_next: bool) -> (String, bool) {
    let lower = word.to_lowercase();
    // Bearer <token>：Bearer 词后随词脱敏。
    if lower == "bearer" {
        return (word.to_string(), true);
    }
    if redact_next {
        return ("[REDACTED]".to_string(), false);
    }
    // 令牌前缀（含尾随标点剥离后判断）。
    let stripped =
        lower.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    if SECRET_TOKEN_PREFIXES
        .iter()
        .any(|p| stripped.starts_with(p) && stripped.len() > p.len() + 4)
    {
        return ("[REDACTED]".to_string(), false);
    }
    // 词内键值：key=value / key:value / key：value。
    let sep = word
        .char_indices()
        .find(|(_, c)| *c == '=' || *c == ':' || *c == '：');
    if let Some((sep_idx, sep_char)) = sep {
        let key = &word[..sep_idx];
        if is_sensitive_key(key) {
            // 值紧随分隔符（同词内）→ 就地替换；值为空（空格分隔，值在下一词）
            // → 同样替换并令下一词脱敏（`password: hunter2` 场景）。
            let mut out = String::with_capacity(word.len());
            out.push_str(key);
            out.push(sep_char);
            out.push_str("[REDACTED]");
            let value_same_word = !word[sep_idx + sep_char.len_utf8()..].trim().is_empty();
            return (out, !value_same_word);
        }
        // 键不敏感：对分隔符之后的残余部分递归续扫（覆盖 `失败原因：api_key=sk-…`
        // 这类无空格连写的复合词——首个分隔符的键非凭据，但词内还有凭据键值段）。
        let rest = &word[sep_idx + sep_char.len_utf8()..];
        if !rest.is_empty() {
            let (redacted_rest, hunt_next) = sanitize_word(rest, false);
            let mut out = String::with_capacity(word.len());
            out.push_str(key);
            out.push(sep_char);
            out.push_str(&redacted_rest);
            return (out, hunt_next);
        }
        return (word.to_string(), false);
    }
    // 裸敏感键词（如独立出现的 `token`）：保守把后随词视为值脱敏（误杀方向安全）。
    if is_sensitive_key(word) {
        return (word.to_string(), true);
    }
    (word.to_string(), false)
}

/// 结构化脱敏：凭据类键 → `[REDACTED]`；字符串值 → [`sanitize_text`]；其余递归。
pub fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_sensitive_key(k) {
                        (k.clone(), json!("[REDACTED]"))
                    } else {
                        (k.clone(), sanitize_value(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        Value::String(s) => Value::String(sanitize_text(s)),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn sum_opt(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut any = false;
    let mut total = 0u64;
    for v in values {
        any = true;
        total = total.saturating_add(v);
    }
    any.then_some(total)
}

fn add_opt(base: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (base, value) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn price_env(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
}

/// 成本估算（美元）：单价取 `OWO_MODEL_INPUT_PRICE_PER_MTOK` /
/// `OWO_MODEL_OUTPUT_PRICE_PER_MTOK`（$/百万 token；未配置按 0 计——与既有
/// 用量/预算口径一致，tokens 仍真实落盘，cost=0 即「未配置单价」信号）。
pub fn estimate_cost_usd(prompt_tokens: u64, completion_tokens: u64) -> f64 {
    let input_price = price_env("OWO_MODEL_INPUT_PRICE_PER_MTOK").unwrap_or(0.0);
    let output_price = price_env("OWO_MODEL_OUTPUT_PRICE_PER_MTOK")
        .or_else(|| price_env("OWO_MODEL_INPUT_PRICE_PER_MTOK"))
        .unwrap_or(0.0);
    round6(
        prompt_tokens as f64 / 1_000_000.0 * input_price
            + completion_tokens as f64 / 1_000_000.0 * output_price,
    )
}

/// 截断长文本（按字符；附截断标记）。
pub fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let kept: String = input.chars().take(max_chars).collect();
    let dropped = input.chars().count() - max_chars;
    format!("{kept}…[截断 {dropped} 字符]")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// 模块内单测（HTTP 集成面见 tests/workswarm_api_tests.rs）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use owo_agent_core::gateway::{ChatMessage, ModelOutput};
    use owo_agent_core::tools::ToolSpec;

    #[tokio::test]
    async fn measured_provider_counts_each_model_call() {
        struct StubProvider;
        #[async_trait]
        impl ModelProvider for StubProvider {
            async fn complete(
                &self,
                _messages: &[ChatMessage],
                _tools: &[ToolSpec],
            ) -> Result<ModelOutput, String> {
                Ok(ModelOutput::Text("ok".to_string()))
            }
        }
        let calls = Arc::new(AtomicU64::new(0));
        let provider = MeasuredProvider::new(Arc::new(StubProvider), Arc::clone(&calls));
        let msgs: Vec<ChatMessage> = Vec::new();
        let _ = provider.complete(&msgs, &[]).await;
        let _ = provider.complete(&msgs, &[]).await;
        let mut deltas: Vec<String> = Vec::new();
        let _ = provider
            .complete_stream(&msgs, &[], &mut |d: String| deltas.push(d))
            .await;
        assert_eq!(
            calls.load(Ordering::Relaxed),
            3,
            "每次模型调用恰好计一次（complete ×2 + complete_stream ×1）"
        );
        // usage_snapshot 透传（stub 无用量上报 → 零）。
        assert_eq!(provider.usage_snapshot().total_tokens, 0);
    }

    fn span(role: &str, step: &str, cost: f64, attempt: u32, started_ms: u64) -> WorkerSpanRecord {
        WorkerSpanRecord {
            span_id: format!("span-{role}-{attempt}"),
            team_id: "team-t".to_string(),
            member_id: format!("m-{role}"),
            role: role.to_string(),
            worker_kind: "echo".to_string(),
            step_id: step.to_string(),
            started_at: "2026-08-28T00:00:00+00:00".to_string(),
            ended_at: "2026-08-28T00:00:01+00:00".to_string(),
            started_at_ms: started_ms,
            ended_at_ms: started_ms + 1000,
            wall_ms: 1000,
            outcome: "succeeded".to_string(),
            error: None,
            model_calls: 0,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cost_usd: cost,
            attempt,
            artifact: Some(SpanArtifact {
                artifact_id: format!("team-t:{role}:v{attempt}"),
                kind: "document".to_string(),
                version: attempt,
            }),
        }
    }

    #[test]
    fn journal_roundtrip_and_corrupt_line_skip() {
        let dir = std::env::temp_dir().join(format!(
            "owo-metrics-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let journal = MetricsJournal::for_team(&dir, "team-j");
        assert!(journal.read_records().is_empty(), "无文件 = 空记录");
        journal.append(&span("planner", "s1", 0.0, 1, 1)).unwrap();
        journal.append(&span("builder", "s2", 0.5, 1, 2)).unwrap();
        // 追加半行/坏行（崩溃中断写安全）。
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(journal.path())
                .unwrap();
            writeln!(f, "{{\"broken json").unwrap();
        }
        let records = journal.read_records();
        assert_eq!(records.len(), 2, "坏行应被跳过：{records:?}");
        assert_eq!(records[1].role, "builder");
        assert_eq!(journal.count_step_spans("s2"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aggregate_reports_roles_slowest_and_rework() {
        let records = vec![
            span("planner", "s1", 0.0, 1, 100),
            span("builder", "s2", 0.25, 1, 200),
            span("builder", "s2", 0.25, 2, 300),
        ];
        let agg = aggregate_metrics("team-t", &records, &json!({}));
        let summary = &agg["summary"];
        assert_eq!(summary["span_count"], 3);
        assert_eq!(summary["succeeded_spans"], 3);
        assert_eq!(summary["failed_spans"], 0);
        assert_eq!(summary["rework_count"], 1, "attempt>1 记返工");
        assert_eq!(summary["artifact_versions"], 3);
        assert_eq!(summary["model_calls"], 0);
        assert_eq!(summary["cost_usd"], 0.5);
        assert_eq!(summary["slowest_worker"]["role"], "planner");
        let roles = agg["roles"].as_array().unwrap();
        assert_eq!(roles.len(), 2);
        let builder = roles.iter().find(|r| r["role"] == "builder").unwrap();
        assert_eq!(builder["spans"], 2);
        assert_eq!(builder["rework"], 1);
        assert_eq!(builder["artifact_versions"], 2);
        assert_eq!(agg["workers"].as_array().unwrap().len(), 3);
        assert_eq!(agg["budget"]["exceeded"], false);
        assert_eq!(agg["team_id"], "team-t");
    }

    #[test]
    fn aggregate_empty_is_well_formed() {
        let agg = aggregate_metrics("team-empty", &[], &json!({}));
        assert_eq!(agg["summary"]["span_count"], 0);
        assert!(agg["roles"].as_array().unwrap().is_empty());
        assert_eq!(agg["summary"]["slowest_worker"], Value::Null);
        assert_eq!(agg["summary"]["total_tokens"], Value::Null);
    }

    #[test]
    fn budget_exhaustion_cost_and_wall() {
        let records = vec![span("planner", "s1", 12.5, 1, 1_000)];
        // 费用超限（严格大于）。
        let reason =
            budget_exhaustion_reason(&json!({"max_cost_usd": 10.0}), &records, 2_000).unwrap();
        assert!(reason.contains("费用预算耗尽"), "{reason}");
        // 未超限。
        assert!(
            budget_exhaustion_reason(&json!({"max_cost_usd": 12.5}), &records, 2_000).is_none()
        );
        // 墙钟超限（窗口 1999ms > 1s）。
        let reason =
            budget_exhaustion_reason(&json!({"max_wall_secs": 1}), &records, 3_000).unwrap();
        assert!(reason.contains("墙钟预算耗尽"), "{reason}");
        // 未配置 → 不启用。
        assert!(budget_exhaustion_reason(&json!({}), &records, 9_000).is_none());
        // 空记录 → 永不耗尽。
        assert!(budget_exhaustion_reason(&json!({"max_cost_usd": 0.0}), &[], 9_000).is_none());
    }

    #[test]
    fn sensitive_keys_exclude_usage_token_counts() {
        assert!(is_sensitive_key("api_key"));
        assert!(is_sensitive_key("OPENAI_API_KEY"));
        assert!(is_sensitive_key("access_token"));
        assert!(is_sensitive_key("authorization"));
        assert!(is_sensitive_key("password"));
        assert!(is_sensitive_key("client_secret"));
        // 用量计数复数不是凭据。
        assert!(!is_sensitive_key("prompt_tokens"));
        assert!(!is_sensitive_key("completion_tokens"));
        assert!(!is_sensitive_key("total_tokens"));
        assert!(!is_sensitive_key("artifact_versions"));
    }

    #[test]
    fn sanitize_text_redacts_credentials_but_keeps_usage_counts() {
        let text = "失败原因：api_key=sk-abcdef123456 password: hunter2 Bearer abc.def.ghi";
        let out = sanitize_text(text);
        assert!(!out.contains("sk-abcdef"), "{out}");
        assert!(!out.contains("hunter2"), "{out}");
        assert!(!out.contains("abc.def.ghi"), "{out}");
        assert!(out.contains("[REDACTED]"), "{out}");
        // 用量计数保留。
        let usage = sanitize_text("total_tokens: 12345 completion_tokens=678");
        assert!(usage.contains("12345"), "{usage}");
        assert!(usage.contains("678"), "{usage}");
        // CAS ref 保留。
        let cas = sanitize_text("content_ref cas://sha256:0123456789abcdef0123456789abcdef");
        assert!(cas.contains("cas://sha256:"), "{cas}");
    }

    #[test]
    fn sanitize_value_redacts_nested_sensitive_keys_and_truncates() {
        let value = json!({
            "budget": { "api_key": "sk-secret-0001", "max_cost_usd": 1.0 },
            "note": "x".repeat(1000),
            "items": [ { "password": "hunter2", "role": "builder" } ]
        });
        let out = sanitize_value(&value);
        assert_eq!(out["budget"]["api_key"], "[REDACTED]");
        assert_eq!(out["budget"]["max_cost_usd"], 1.0);
        assert_eq!(out["items"][0]["password"], "[REDACTED]");
        assert_eq!(out["items"][0]["role"], "builder");
        let note = out["note"].as_str().unwrap();
        assert!(note.len() < 500, "长文本应截断：{}", note.len());
        assert!(note.contains("[截断"), "{note}");
    }
}
