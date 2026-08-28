//! WorkSwarm 真实团队评测适配器（V1-R2 多 Agent 对照执行器）。
//!
//! 与生成式执行器的本质区别：`multi` 不再用"规划→生成→评审"三段 Prompt 模拟协作，
//! 而是为每个评测任务建立**真实 TeamRun**（`TeamCoordinator` + ProjectSpace + CAS +
//! Handoff），由 `GoalRunner` 按 DAG 驱动 ≤3 个真实 Agent Worker：
//!
//! - producer（builder/researcher/writer，按任务分类）→ 产出主交付物；
//! - critic（只读）→ 结构化评审；
//! - leader → 最终裁决并产出最终交付物；
//! - "Coordinator" 职责由 TeamCoordinator 承担：拆分任务（角色 DAG + 交接契约）与控制预算。
//!
//! 中间结果一律通过**版本化 Artifact ref + Handoff** 传递（内容落 CAS）；
//! 最终 Artifact 从 ProjectSpace/CAS 复制进评测沙盒后，用与单 Agent **完全相同**的
//! 检查器判定。采集 TeamRun 总耗时、各 Worker 模型调用/耗时、retry/Handoff/Artifact
//! 版本、失败 Worker 与失败步骤、token/费用汇总。取消令牌贯通 TeamRun 与所有 Worker。

use crate::agent::{Agent, AgentConfig, TurnEvent};
use crate::cas_store::CasStore;
use crate::gateway::{ChatMessage, ModelOutput, ModelProvider, TokenUsage};
use crate::goal::{Worker, WorkerRegistry};
use crate::permissions::{AutoApprover, Policy};
use crate::plan::StepStatus;
use crate::product_eval::{AgentMode, CaseExecutor, ExecContext, RawExecOutcome};
use crate::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use crate::session::Session;
use crate::team_strategy::{TaskProfile, TeamPlan, TeamSelectionMode, TeamStrategyEngine};
use crate::tools::ToolRegistry;
use crate::workswarm::{
    CreateTeamRequest, PhaseOutcome, RoleSpec, RoleWorker, SteerCommand, TeamCoordinator,
};
use async_trait::async_trait;
use owo_agent_protocol::{Artifact, TeamMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 单个 Worker 的运行观测（独立运行记录；correlation 在 [`TeamRunObservation`] 上）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerObservation {
    pub member_id: String,
    pub role: String,
    pub attempts: u32,
    pub model_calls: u32,
    pub tool_calls: u32,
    pub permission_denied: u32,
    pub tool_failures: u32,
    pub wall_ms: u64,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub error: Option<String>,
    /// 输出契约定向修复次数（R4：解析失败只允许一次）。
    #[serde(default)]
    pub output_repairs: u32,
}

/// 版本化 Artifact 观测。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactObservation {
    pub artifact_id: String,
    pub kind: String,
    pub version: u32,
    pub producer: String,
    pub content_ref: String,
    /// 评审状态（Approved/Draft/PendingReview/Superseded…；旧观测文件缺省 Draft）。
    #[serde(default)]
    pub review_state: String,
}

/// 自适应组队判定快照（进 observation 供冒烟/报告取证与 UI 展示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyObservation {
    pub mode: String,
    pub requested: String,
    pub roles: Vec<String>,
    pub budget_calls_total: usize,
    pub json_repair: bool,
    pub reasons: Vec<String>,
}

/// 一次 WorkSwarm TeamRun 的完整观测。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRunObservation {
    pub team_id: String,
    pub correlation_id: String,
    /// TeamRun 终态（succeeded/failed/cancelled/…）。
    pub status: String,
    pub wall_ms: u64,
    pub workers: Vec<WorkerObservation>,
    pub handoff_count: usize,
    pub artifacts: Vec<ArtifactObservation>,
    /// 交付清单（finalize_success 产物；未收尾为 None）。
    pub delivery_manifest_ref: Option<String>,
    /// 局部 retry 使用次数（复用 `SteerCommand::Retry`，只重置失败步骤及其下游）。
    pub retries_used: u32,
    pub cancelled: bool,
    pub failed_steps: Vec<String>,
    /// 最终被选中的交付 Artifact（版本化 ref）。
    pub final_artifact_ref: Option<String>,
    /// 自适应组队判定（R3；旧观测文件缺失时为 None）。
    #[serde(default)]
    pub strategy: Option<StrategyObservation>,
}

impl TeamRunObservation {
    fn empty() -> Self {
        Self {
            team_id: String::new(),
            correlation_id: String::new(),
            status: "not_started".to_string(),
            wall_ms: 0,
            workers: Vec::new(),
            handoff_count: 0,
            artifacts: Vec::new(),
            delivery_manifest_ref: None,
            retries_used: 0,
            cancelled: false,
            failed_steps: Vec::new(),
            final_artifact_ref: None,
            strategy: None,
        }
    }
}

/// Worker 内层统计累积器（member 内共享；跨多次 attempt 累加）。
#[derive(Default)]
struct WorkerStats {
    attempts: Mutex<u32>,
    model_calls: Mutex<u32>,
    tool_calls: Mutex<u32>,
    permission_denied: Mutex<u32>,
    tool_failures: Mutex<u32>,
    output_repairs: Mutex<u32>,
    wall_ms: Mutex<u64>,
    usage: Mutex<TokenUsage>,
    usage_known: Mutex<bool>,
    error: Mutex<Option<String>>,
}

impl WorkerStats {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn bump(model_calls: &Mutex<u32>) {
        if let Ok(mut value) = model_calls.lock() {
            *value = value.saturating_add(1);
        }
    }

    fn observe_turn(&self, event: &TurnEvent) {
        match event {
            TurnEvent::ModelCall => Self::bump(&self.model_calls),
            TurnEvent::ToolStart { .. } => Self::bump(&self.tool_calls),
            TurnEvent::PermissionRequest(request) => {
                // 最终拒绝信号：Policy::decision 以 reason 前缀「拒绝」表达
                //（越界路径/危险命令/只读策略下的写请求等）。
                if request.reason.starts_with("拒绝") {
                    Self::bump(&self.permission_denied);
                }
            }
            TurnEvent::ToolResult { ok, .. } if !ok => Self::bump(&self.tool_failures),
            _ => {}
        }
    }

    fn record_success(&self, wall_ms: u64, usage: TokenUsage) {
        if let Ok(mut value) = self.wall_ms.lock() {
            *value = value.saturating_add(wall_ms);
        }
        if let Ok(mut value) = self.attempts.lock() {
            *value = value.saturating_add(1);
        }
        if usage.total_tokens > 0 {
            if let Ok(mut known) = self.usage_known.lock() {
                *known = true;
            }
            if let Ok(mut value) = self.usage.lock() {
                value.add(&usage);
            }
        }
    }

    fn record_failure(&self, wall_ms: u64, error: String) {
        self.record_success(wall_ms, TokenUsage::default());
        if let Ok(mut slot) = self.error.lock() {
            *slot = Some(error);
        }
    }

    fn snapshot(&self, member_id: &str, role: &str) -> WorkerObservation {
        let usage = self.usage.lock().map(|u| *u).unwrap_or_default();
        let counter = |mutex: &Mutex<u32>| mutex.lock().map(|value| *value).unwrap_or(0);
        let wall = self.wall_ms.lock().map(|value| *value).unwrap_or(0);
        WorkerObservation {
            member_id: member_id.to_string(),
            role: role.to_string(),
            attempts: counter(&self.attempts),
            model_calls: counter(&self.model_calls),
            tool_calls: counter(&self.tool_calls),
            permission_denied: counter(&self.permission_denied),
            tool_failures: counter(&self.tool_failures),
            output_repairs: counter(&self.output_repairs),
            wall_ms: wall,
            prompt_tokens: (usage.prompt_tokens > 0).then_some(usage.prompt_tokens),
            completion_tokens: (usage.completion_tokens > 0).then_some(usage.completion_tokens),
            total_tokens: (usage.total_tokens > 0).then_some(usage.total_tokens),
            error: self.error.lock().ok().and_then(|e| e.clone()),
        }
    }
}

/// 内层 Agent worker：`build_enriched_input` 的 agent 路径注入 `prompt` + `read_only`，
/// 本 worker 用**独立 Session + 只读工具集 + 只读策略**执行一轮 `Agent::run_turn`，
/// 产出文本经 RoleWorker 登记为版本化 Artifact + Handoff（内容落 CAS）。
struct EvalAgentWorker {
    provider: Arc<dyn ModelProvider>,
    model: String,
    workspace: PathBuf,
    max_turns: usize,
    /// ProductEval 取消令牌（贯通到 Agent 回合循环）。
    cancel: Arc<AtomicBool>,
    stats: Arc<WorkerStats>,
}

#[async_trait]
impl Worker for EvalAgentWorker {
    fn name(&self) -> &str {
        "agent"
    }

    async fn run(&self, input: &serde_json::Value) -> Result<String, String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err("已取消（进入前检测）".to_string());
        }
        let prompt = input
            .get("prompt")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "agent 步骤缺少 prompt 参数".to_string())?;
        let read_only = input
            .get("read_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);

        // 评测 Worker 不继承真实仓库宽权限：工具集恒为只读，
        // 策略根 = 本团队独立工作区（写入类工具根本不在注册表内）。
        let policy = if read_only {
            Policy::read_only(self.workspace.clone())
        } else {
            Policy::new(self.workspace.clone())
        };
        let registry = ToolRegistry::read_only();
        let config = AgentConfig {
            max_turns: self.max_turns,
            ..AgentConfig::default()
        };
        let agent = Agent::new(Arc::clone(&self.provider), registry, policy, config);
        // 输出契约（V1）：Worker 必须返回 WorkerOutputV1 结构化 JSON
        //（producer 带 artifact；critic 只给评审结论）。
        let system = format!(
            "你是 WorkSwarm 团队中的角色成员。严格依据交接契约处理上游上下文；不要输出与交付物无关的过程解释。\n{}",
            crate::workswarm_output::contract_system_prompt(read_only)
        );
        let mut session = Session::new(&self.workspace, &self.model, Some(system));
        let started = Instant::now();
        let outcome = agent
            .run_turn(
                &mut session,
                prompt,
                &AutoApprover { allow: true },
                &self.cancel,
                &mut |event| self.stats.observe_turn(event),
            )
            .await;
        let wall_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(turn) => {
                self.stats.record_success(wall_ms, turn.usage);
                let text = turn.final_text.unwrap_or_default();
                self.enforce_output_contract(&text, read_only).await
            }
            Err(error) => {
                let message = format!("agent 回合失败：{error}");
                self.stats.record_failure(wall_ms, message.clone());
                Err(message)
            }
        }
    }
}

impl EvalAgentWorker {
    /// 输出契约执行：非契约输出（自由文本/坏 JSON/字段缺失/critic 越权带 artifact）
    /// → **恰好一次**定向修复；仍不合规 → `output_contract_invalid`
    ///（可定位失败原因，禁止无限重试）。
    async fn enforce_output_contract(&self, text: &str, is_critic: bool) -> Result<String, String> {
        let parse = crate::workswarm_output::parse_worker_output(text);
        let contract_ok = match &parse {
            crate::workswarm_output::WorkerOutputParse::Parsed(output) => {
                // 角色规则在执行层判定：critic 不得带 artifact；producer 必须带。
                if is_critic {
                    output.validate_critic().is_ok()
                } else {
                    output.validate().is_ok()
                }
            }
            _ => false,
        };
        if contract_ok {
            return Ok(text.to_string());
        }
        // 一次定向修复（直接 provider 调用；计入该 worker 的 model_calls/repairs）。
        WorkerStats::bump(&self.stats.model_calls);
        if let Ok(mut value) = self.stats.output_repairs.lock() {
            *value = value.saturating_add(1);
        }
        let role_rule = if is_critic {
            "你是评审角色：禁止提交 artifact 字段，评审结论 JSON 放 summary"
        } else {
            "你是交付角色：status=done 必须携带 artifact（content=交付物正文本体）"
        };
        let repair_prompt = format!(
            "你上一次的回复不符合 WorkerOutputV1 输出契约（必须是合法 JSON：status/summary/\
artifact{{kind,format,content}}/evidence/open_issues/handoff）。{role_rule}。\
请修正后**只输出**符合契约的 JSON 本体。\n原始输出：\n{text}\n\n请重新输出符合契约的 JSON："
        );
        let messages = [ChatMessage {
            role: "user".to_string(),
            content: Some(repair_prompt),
            tool_calls: None,
            tool_call_id: None,
        }];
        let repaired = match self.provider.complete(&messages, &[]).await {
            Ok(ModelOutput::Text(text)) => strip_code_fences(&text),
            _ => String::new(),
        };
        match crate::workswarm_output::parse_worker_output(&repaired) {
            crate::workswarm_output::WorkerOutputParse::Parsed(output) => {
                let valid = if is_critic {
                    output.validate_critic().is_ok()
                } else {
                    true
                };
                if valid {
                    Ok(repaired)
                } else {
                    Err("output_contract_invalid:定向修复一次后仍不符合契约".to_string())
                }
            }
            _ => Err(
                "output_contract_invalid:定向修复一次后仍不符合契约（自由文本冒充交付物或字段缺失）"
                    .to_string(),
            ),
        }
    }
}

/// 执行器配置（也承担默认值来源）。
#[derive(Clone)]
pub struct WorkSwarmExecutorConfig {
    /// 每个 Agent Worker 的最大回合数（角色无专属预算时的退回值）。
    pub max_turns_per_worker: usize,
    /// TeamRun 失败后允许的局部 retry 次数（复用 R2 `SteerCommand::Retry`，
    /// 只重置失败步骤及其未完成下游，已成功 Worker 不重跑）。
    pub max_retries_on_failure: u32,
    /// 组队模式选择（R3 自适应组队）：single 强制单 Agent、team 强制完整流水线、
    /// auto（默认）按任务画像判定——简单任务只建单角色团队。
    pub selection: TeamSelectionMode,
}

impl Default for WorkSwarmExecutorConfig {
    fn default() -> Self {
        Self {
            max_turns_per_worker: 8,
            max_retries_on_failure: 1,
            selection: TeamSelectionMode::default(),
        }
    }
}

/// WorkSwarm 真实 TeamRun 评测执行器（实现 ProductEval `CaseExecutor`）。
pub struct WorkSwarmExecutor {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    /// TeamRun 运行目录根（每个单元格建独立子目录：CAS / sqlite / run 状态互不串扰）。
    pub work_root: PathBuf,
    pub config: WorkSwarmExecutorConfig,
}

impl WorkSwarmExecutor {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
        work_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            work_root: work_root.into(),
            config: WorkSwarmExecutorConfig::default(),
        }
    }

    /// 任务画像 → 策略引擎输入（R3 自适应组队）。
    /// 结构化 JSON 任务（期望 .json 产物）的格式风险走**一次修复机会**（expects_json），
    /// 不再映射为高风险评审组队——critic 读 JSON 修不了格式问题。
    fn profile_of(case: &crate::product_eval::ProductEvalCase) -> TaskProfile {
        let expects_json = case
            .expected_artifacts
            .iter()
            .any(|path| path.to_ascii_lowercase().ends_with(".json"));
        let risk = if case.category == crate::product_eval::EvalCategory::Code {
            crate::team_strategy::RiskLevel::Normal
        } else {
            crate::team_strategy::RiskLevel::Low
        };
        TaskProfile {
            category: Some(case.category.as_str().to_string()),
            artifact_count: case.expected_artifacts.len(),
            input_count: case.inputs.len(),
            needs_independent_review: false,
            risk,
            single_agent_success_rate: None,
            expects_json,
        }
    }

    /// 组队计划 → 角色 DAG（producer → [critic] → [leader]，按计划裁剪）。
    fn roles_from_plan(
        case: &crate::product_eval::ProductEvalCase,
        plan: &TeamPlan,
    ) -> Vec<RoleSpec> {
        let mut specs: Vec<RoleSpec> = Vec::new();
        let producer_role = plan
            .roles
            .first()
            .map(|r| r.role.clone())
            .unwrap_or_else(|| "producer".to_string());
        let mut producer = RoleSpec::agent(producer_role.clone());
        producer.handoff_contract = Some(format!(
            "依据团队目标与任务说明产出主交付物正文。必须产出的交付物将登记为版本化 Artifact：{}。直接输出交付物内容本身。",
            case.expected_artifacts.join("、")
        ));
        producer.verify = Some("non_empty".to_string());
        specs.push(producer);

        for role_plan in &plan.roles[1..] {
            match role_plan.role.as_str() {
                "critic" => {
                    let mut critic = RoleSpec::agent("critic");
                    critic.depends_on = vec![producer_role.clone()];
                    critic.handoff_contract = Some(
                        "只读评审上游交付物草稿（不修改原文）：检查完整性、一致性、与任务要求的符合度，输出 JSON {\"approved\":bool,\"score\":0-100,\"comments\":[..]}。"
                            .to_string(),
                    );
                    critic.verify = Some("non_empty".to_string());
                    specs.push(critic);
                }
                "leader" => {
                    let upstream = specs
                        .last()
                        .map(|s| s.role.clone())
                        .unwrap_or_else(|| producer_role.clone());
                    let mut leader = RoleSpec::agent("leader");
                    leader.depends_on = vec![upstream];
                    leader.handoff_contract = Some(format!(
                        "综合上游交付物草稿（与评审意见）采纳或修正，输出最终交付物正文（{}）。上游未通过的问题必须修正。",
                        case.expected_artifacts.join("、")
                    ));
                    leader.verify = Some("non_empty".to_string());
                    specs.push(leader);
                }
                other => {
                    tracing::warn!(role = %other, "策略计划中的未知角色被忽略");
                }
            }
        }
        specs
    }

    fn objective_of(case: &crate::product_eval::ProductEvalCase) -> String {
        format!(
            "[{}] {}\n\n{}\n\n最终交付物（登记为版本化 Artifact）：{}",
            case.category.as_str(),
            case.title,
            case.instruction.trim(),
            case.expected_artifacts.join("、")
        )
    }

    /// 带完整观测的执行入口（报告/测试用；`CaseExecutor::execute` 是其薄封装）。
    pub async fn execute_observed<'ctx>(
        &self,
        ctx: &mut ExecContext<'ctx>,
    ) -> Result<(RawExecOutcome, TeamRunObservation), crate::product_eval::ProductEvalError> {
        use crate::product_eval::ProductEvalError;
        let case = ctx.case;
        if ctx.cancelled() {
            let mut observation = TeamRunObservation::empty();
            observation.cancelled = true;
            observation.status = "cancelled".to_string();
            return Ok((
                RawExecOutcome {
                    aborted: true,
                    ..RawExecOutcome::default()
                },
                observation,
            ));
        }

        // —— 每单元格独立目录：CAS / sqlite / 运行状态 / worker 工作区 ——
        let slug = format!(
            "{}-{}-{}",
            case.id,
            ctx.mode.as_str(),
            uuid::Uuid::new_v4().simple()
        );
        let root = self.work_root.join(&slug);
        let cas_dir = root.join("cas");
        let runs_dir = root.join("runs");
        let ws_dir = root.join("workspace");
        for dir in [&cas_dir, &runs_dir, &ws_dir] {
            std::fs::create_dir_all(dir)
                .map_err(|e| ProductEvalError(format!("创建 TeamRun 目录失败：{e}")))?;
        }
        let cas =
            CasStore::new(cas_dir).map_err(|e| ProductEvalError(format!("CAS 初始化失败：{e}")))?;
        let store = Arc::new(
            SqliteProjectSpaceStore::open(&root.join("projects.db"))
                .map_err(|e| ProductEvalError(format!("ProjectSpace 存储初始化失败：{e}")))?,
        );
        let templates = Arc::new(crate::workswarm::TeamTemplateRegistry::new(
            root.join("templates"),
        ));
        let coordinator = Arc::new(TeamCoordinator::new(
            Arc::clone(&store) as Arc<dyn ProjectSpaceStoreBackend>,
            templates,
            cas,
            runs_dir,
        ));

        // —— 输入 fixture 预写进团队工作区（只写 allow_read 范围内的路径）——
        for input in &case.inputs {
            let Ok(rel) = crate::product_eval::sanitize_rel_path(&input.path) else {
                continue;
            };
            if !crate::product_eval::in_scope(&case.allow_read, &rel) {
                ctx.record_failed_step(format!("setup_skipped:{}（超出 allow_read）", input.path));
                continue;
            }
            let target = ws_dir.join(&rel);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&target, &input.content);
        }

        // —— 自适应组队：策略引擎判定 single/team + 角色 DAG + 每角色调用预算 ——
        let strategy = TeamStrategyEngine::default();
        let plan = strategy.decide(self.config.selection, &Self::profile_of(case));
        tracing::info!(
            case = %case.id,
            mode = %plan.mode,
            roles = plan.roles.iter().map(|r| r.role.as_str()).collect::<Vec<_>>().join("+"),
            budget = plan.budget_calls_total,
            "自适应组队判定"
        );

        // —— 组队：角色 DAG + 预算（timeout/预算映射 GoalBudget）——
        let mut request = CreateTeamRequest::new(
            Self::objective_of(case),
            if plan.is_single() {
                TeamMode::Single
            } else {
                TeamMode::Team
            },
        );
        request.roles = Self::roles_from_plan(case, &plan);
        request.budget = serde_json::json!({
            "max_steps": 64,
            "max_retries_per_step": 1,
            // GoalRunner 把每个步骤的 attempts 计入 total_retries（≥角色数），
            // 因此该值必须 ≥ 步骤尝试总数；真正的重试上限由执行器层的
            // max_retries_on_failure（SteerCommand::Retry）控制。
            "max_total_retries": 64,
            "max_replans": 0,
            "max_duration_secs": ctx.timeout_secs,
        });
        let team = coordinator
            .create_team_run(&request)
            .await
            .map_err(|e| ProductEvalError(format!("TeamRun 创建失败：{e}")))?;
        let team_id = team.team_id.clone();
        let correlation_id = coordinator
            .load_run_meta(&team_id)
            .map(|meta| meta.correlation_id)
            .unwrap_or_default();

        // —— worker 注册表：每成员独立 EvalAgentWorker + 独立统计 ——
        let meta = coordinator
            .load_run_meta(&team_id)
            .map_err(|e| ProductEvalError(format!("运行元数据缺失：{e}")))?;
        let registry = WorkerRegistry::new();
        let mut stats_by_member: BTreeMap<String, (String, Arc<WorkerStats>)> = BTreeMap::new();
        for spec in &meta.roles {
            let member_id = format!("m-{}", spec.role);
            let stats = WorkerStats::new();
            // 每角色调用预算来自策略计划（未知角色退回 producer 缺省 4）。
            let role_budget = plan.budget_for(&spec.role);
            registry.register(Arc::new(RoleWorker::new(
                Arc::clone(&coordinator),
                team_id.clone(),
                member_id.clone(),
                spec.role.clone(),
                Arc::new(EvalAgentWorker {
                    provider: Arc::clone(&self.provider),
                    model: self.model.clone(),
                    workspace: ws_dir.clone(),
                    max_turns: role_budget,
                    cancel: Arc::clone(&ctx.cancel),
                    stats: Arc::clone(&stats),
                }),
            )));
            stats_by_member.insert(member_id, (spec.role.clone(), stats));
        }

        // —— 取消桥：ProductEval 令牌 → TeamRun CancelToken ——
        let team_cancel = coordinator.cancel_token(&team_id);
        let bridge_source = Arc::clone(&ctx.cancel);
        let bridge_token = team_cancel.clone();
        let watcher = tokio::spawn(async move {
            loop {
                if bridge_source.load(Ordering::Relaxed) {
                    bridge_token.cancel();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        });

        // —— 阶段驱动循环（镜像 server 运行循环；无 human 角色 → 无门闩分支）——
        let started = Instant::now();
        let mut retries_used = 0u32;
        // loop-with-value：终态标签在每个 break 点产生（无未读初始化）。
        let mut run_error: Option<String> = None;
        let terminal: String = loop {
            let mut phase_error_strikes = 0u32;
            coordinator.set_loop_alive(&team_id, true);
            if ctx.cancelled() {
                team_cancel.cancel();
            }
            match coordinator.run_phase(&team_id, &registry).await {
                Ok(PhaseOutcome::MoreReady) => {
                    continue;
                }
                Ok(PhaseOutcome::Done) => {
                    if let Err(e) = coordinator.finalize_success(&team_id).await {
                        tracing::warn!(team_id = %team_id, error = %e, "TeamRun 收尾失败（产物已登记）");
                    }
                    break "succeeded".to_string();
                }
                Ok(PhaseOutcome::Failed) => {
                    // 局部 retry：只重置失败步骤及其未完成下游（已成功 Worker/Artifact 不动）。
                    if retries_used < self.config.max_retries_on_failure {
                        if let Some(step_id) = first_failed_step(&coordinator, &team_id).await {
                            let applied = coordinator
                                .apply_steer(
                                    &team_id,
                                    &SteerCommand::Retry {
                                        step_id: step_id.clone(),
                                        note: "product-eval：局部重试失败步骤".to_string(),
                                    },
                                )
                                .await;
                            if applied.is_ok() {
                                retries_used += 1;
                                tracing::info!(team_id = %team_id, step = %step_id, "TeamRun 局部重试");
                                continue;
                            }
                        }
                    }
                    run_error = Some(format!(
                        "TeamRun 失败（重试预算 {} 已用尽或无可重试步骤）",
                        retries_used
                    ));
                    break "failed".to_string();
                }
                Ok(PhaseOutcome::Aborted) | Ok(PhaseOutcome::Finished) => {
                    break if ctx.cancelled() || team_cancel.is_cancelled() {
                        "cancelled".to_string()
                    } else {
                        "aborted".to_string()
                    };
                }
                Ok(PhaseOutcome::AwaitingHuman { .. }) => {
                    run_error = Some("评测团队不应包含 human 角色".to_string());
                    break "awaiting_human".to_string();
                }
                Err(error) => {
                    phase_error_strikes += 1;
                    if phase_error_strikes >= 3 {
                        let message = format!("run_phase 连续失败：{error}");
                        run_error = Some(message.clone());
                        break message;
                    }
                    tokio::time::sleep(Duration::from_millis(300 * phase_error_strikes as u64))
                        .await;
                }
            }
        };
        coordinator.set_loop_alive(&team_id, false);
        watcher.abort();
        let wall_ms = started.elapsed().as_millis() as u64;
        let cancelled = ctx.cancelled() || team_cancel.is_cancelled();

        // —— 观测汇总：workers / handoffs / artifacts / 失败步骤 ——
        let workers: Vec<WorkerObservation> = stats_by_member
            .iter()
            .map(|(member_id, (role, stats))| stats.snapshot(member_id, role))
            .collect();
        let (handoff_count, artifacts, delivery_manifest_ref, team_status_label) =
            match coordinator.get_team_run(&team_id).await {
                Ok(team) => {
                    let space_id = team.project_space_id.clone().unwrap_or_default();
                    let handoffs = match coordinator.list_handoffs(&team_id).await {
                        Ok(list) => list.len(),
                        Err(_) => 0,
                    };
                    let space = coordinator.get_project_space(&space_id).await.ok();
                    let artifacts = match &space {
                        Some(space) => coordinator.list_artifacts(space).await.unwrap_or_default(),
                        None => Vec::new(),
                    };
                    let observations: Vec<ArtifactObservation> = artifacts
                        .iter()
                        .map(|artifact: &Artifact| ArtifactObservation {
                            artifact_id: artifact.artifact_id.clone(),
                            kind: artifact.kind.clone(),
                            version: artifact.version,
                            producer: artifact.producer.clone(),
                            content_ref: artifact.content_ref.clone(),
                            review_state: format!("{:?}", artifact.review_state),
                        })
                        .collect();
                    (
                        handoffs,
                        observations,
                        space.map(|s| s.delivery_manifest_ref).unwrap_or(None),
                        format!("{:?}", team.status),
                    )
                }
                Err(_) => (0, Vec::new(), None, terminal.clone()),
            };
        let mut failed_steps: Vec<String> = Vec::new();
        if let Ok(state) = coordinator.load_run_state(&team_id) {
            for (step_id, record) in &state.records {
                if matches!(record.status, StepStatus::Failed | StepStatus::Aborted) {
                    failed_steps.push(format!(
                        "{step_id}:{}:{}",
                        match record.status {
                            StepStatus::Failed => "failed",
                            _ => "aborted",
                        },
                        record.error.clone().unwrap_or_default()
                    ));
                }
            }
        }

        let mut observation = TeamRunObservation {
            team_id,
            correlation_id,
            status: if cancelled {
                "cancelled".to_string()
            } else {
                team_status_label
            },
            wall_ms,
            workers,
            handoff_count,
            artifacts,
            delivery_manifest_ref,
            retries_used,
            cancelled,
            failed_steps: failed_steps.clone(),
            final_artifact_ref: None,
            strategy: Some(StrategyObservation {
                mode: plan.mode.clone(),
                requested: plan.requested.clone(),
                roles: plan.roles.iter().map(|r| r.role.clone()).collect(),
                budget_calls_total: plan.budget_calls_total,
                json_repair: plan.json_repair,
                reasons: plan.reasons.clone(),
            }),
        };

        // —— 取消：立即返回，不再产生任何模型/工具调用 ——
        persist_observation(&root, &observation);
        if cancelled {
            return Ok((
                RawExecOutcome {
                    aborted: true,
                    model_calls: observation.workers.iter().map(|w| w.model_calls).sum(),
                    usage: TokenUsage::default(),
                    usage_known: false,
                    retries: retries_used,
                    error: None,
                    tool_log: Vec::new(),
                },
                observation,
            ));
        }

        // —— 最终交付：从 ProjectSpace/CAS 复制**版本化 Artifact 内容**进沙盒 ——
        // 选择顺序：kind=final（leader 裁决）> producer 角色 kind > 最新版本。
        let final_ref = pick_final_artifact(&observation.artifacts);
        let mut outcome = RawExecOutcome {
            aborted: false,
            error: None,
            model_calls: observation.workers.iter().map(|w| w.model_calls).sum(),
            usage: TokenUsage {
                prompt_tokens: observation
                    .workers
                    .iter()
                    .filter_map(|w| w.prompt_tokens)
                    .sum(),
                completion_tokens: observation
                    .workers
                    .iter()
                    .filter_map(|w| w.completion_tokens)
                    .sum(),
                total_tokens: observation
                    .workers
                    .iter()
                    .filter_map(|w| w.total_tokens)
                    .sum(),
            },
            usage_known: observation.workers.iter().any(|w| w.total_tokens.is_some()),
            retries: retries_used,
            tool_log: Vec::new(),
        };

        let Some(final_ref) = final_ref else {
            persist_observation(&root, &observation);
            // R4 可定位失败原因：producer 版本链为空（排除 critic 评审产物后无交付物）。
            outcome.error = Some(run_error.unwrap_or_else(|| {
                "artifact_missing:producer 版本链无交付物（评审产物不参与最终交付）".to_string()
            }));
            return Ok((outcome, observation));
        };
        let content = coordinator
            .resolve_content_text(&final_ref.content_ref)
            .ok_or_else(|| {
                ProductEvalError(format!(
                    "最终 Artifact {} 的 CAS 内容缺失（{}）",
                    final_ref.artifact_id, final_ref.content_ref
                ))
            })?;

        // —— 结构化 JSON 任务：确定性格式检查 + 一次修复机会（R3 第 8 条）——
        // 解析失败只修一次；修复后仍非法则如实交给检查器判定（不做无据通过）。
        let mut content = content;
        if plan.json_repair
            && case
                .expected_artifacts
                .iter()
                .any(|path| path.to_ascii_lowercase().ends_with(".json"))
        {
            if serde_json::from_str::<serde_json::Value>(&content).is_ok() {
                outcome
                    .tool_log
                    .push("json_format_check:ok（无需修复）".to_string());
            } else {
                let (repaired, calls, ok) = self.repair_json_once(&content).await;
                outcome.model_calls += calls;
                if ok {
                    outcome
                        .tool_log
                        .push("json_repair:attempt=1 result=parsed".to_string());
                    content = repaired;
                } else {
                    outcome.tool_log.push(
                        "json_repair:attempt=1 result=still_invalid（交由检查器判定）".to_string(),
                    );
                }
            }
        }
        observation.final_artifact_ref = Some(final_ref.artifact_id.clone());

        // 写入走 ExecContext 受控通道（allow_write 范围强制）；
        // 多预期产物时：首个承载最终交付文本，其余如实登记为不支持（单一最终文本）。
        let mut first = true;
        for artifact_path in &case.expected_artifacts {
            if first {
                first = false;
                if let Err(write_err) = ctx.write_file(artifact_path, &content) {
                    outcome.error = Some(write_err);
                    return Ok((outcome, observation));
                }
            } else {
                ctx.record_failed_step(format!(
                    "workswarm:最终交付为单一文本，无法填充额外预期产物 {artifact_path}"
                ));
            }
        }
        if terminal == "failed" || terminal == "error" {
            // 有产物但终态失败：交给检查器判定（不做无据通过）。
            for step in &failed_steps {
                ctx.record_failed_step(format!("teamrun:{step}"));
            }
        }
        persist_observation(&root, &observation);
        Ok((outcome, observation))
    }

    /// 结构化 JSON 任务的一次修复机会：确定性检查失败后，发起**恰好一次**修复调用，
    /// 只接受修复结果能通过 `serde_json` 解析的输出；否则原样返回交由检查器判定。
    /// 返回 (最终文本, 模型调用次数, 是否修复成功)。
    async fn repair_json_once(&self, broken: &str) -> (String, u32, bool) {
        let prompt = format!(
            "下面的文本本应是合法 JSON，但解析失败。请修正为合法 JSON：保持原有字段名与取值语义完全不变，\
只修复格式问题（缺失引号/多余逗号/未闭合括号/代码围栏包裹等）。只输出修正后的 JSON 本体，\
不要任何解释、注释或代码围栏。\n\n{broken}"
        );
        let messages = [ChatMessage {
            role: "user".to_string(),
            content: Some(prompt),
            tool_calls: None,
            tool_call_id: None,
        }];
        let output = match self.provider.complete(&messages, &[]).await {
            Ok(output) => output,
            Err(_) => return (broken.to_string(), 1, false),
        };
        let text = match output {
            ModelOutput::Text(text) => text,
            _ => return (broken.to_string(), 1, false),
        };
        let cleaned = strip_code_fences(&text);
        if serde_json::from_str::<serde_json::Value>(&cleaned).is_ok() {
            (cleaned, 1, true)
        } else {
            (broken.to_string(), 1, false)
        }
    }
}

/// 把 TeamRun 观测落盘为 `observation.json`（取证/报告用；失败不阻断执行）。
fn persist_observation(root: &std::path::Path, observation: &TeamRunObservation) {
    let Ok(text) = serde_json::to_string_pretty(observation) else {
        return;
    };
    let _ = std::fs::write(root.join("observation.json"), text);
}

/// 去掉模型输出常见的 ```json 围栏（只处理首尾成对围栏）。
fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // 跳过语言标记行（```json / ```JSON5 等）。
    let body = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => rest,
    };
    body.trim()
        .strip_suffix("```")
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().to_string())
}

/// 最终交付选择（R4 输出契约）：只从 **producer 版本链**挑选——
/// 排除 critic 评审产物（kind=review / producer=m-critic，评审无权成为交付物）；
/// 优先 review_state=Approved 的最高版本（approved head），否则取最高版本；
/// 同版本时 leader 综合产物（kind=final）优先于草稿。空链返回 None
///（上游记 `artifact_missing`）。
fn pick_final_artifact(artifacts: &[ArtifactObservation]) -> Option<ArtifactObservation> {
    let chain: Vec<&ArtifactObservation> = artifacts
        .iter()
        .filter(|a| a.kind != "review")
        .filter(|a| a.producer_role() != Some("critic"))
        .collect();
    if chain.is_empty() {
        return None;
    }
    let approved = |a: &ArtifactObservation| a.review_state == "Approved";
    chain
        .into_iter()
        .max_by(|a, b| {
            approved(a)
                .cmp(&approved(b))
                .then(a.version.cmp(&b.version))
                .then_with(|| (a.kind == "final").cmp(&(b.kind == "final")))
        })
        .cloned()
}

impl ArtifactObservation {
    /// 从 producer（member_id `m-{role}`）还原角色名。
    fn producer_role(&self) -> Option<&str> {
        self.producer.strip_prefix("m-")
    }
}

async fn first_failed_step(coordinator: &TeamCoordinator, team_id: &str) -> Option<String> {
    let state = coordinator.load_run_state(team_id).ok()?;
    state
        .records
        .into_iter()
        .find(|(_, record)| matches!(record.status, StepStatus::Failed | StepStatus::Aborted))
        .map(|(step_id, _)| step_id)
}

#[async_trait]
impl CaseExecutor for WorkSwarmExecutor {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        match self.execute_observed(ctx).await {
            Ok((outcome, _)) => outcome,
            Err(error) => RawExecOutcome {
                aborted: false,
                error: Some(error.0),
                ..RawExecOutcome::default()
            },
        }
    }
}

/// 供诊断/报告使用的模式标签。
pub fn agent_mode_label(mode: AgentMode) -> &'static str {
    mode.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{ChatMessage, ModelOutput};
    use crate::product_eval::{ArtifactChecker, EvalCategory, InputFixture, ProductEvalCase};
    use crate::tools::ToolSpec;
    use std::collections::VecDeque;
    use std::path::Path;

    // ---------------------------------------------------------------------
    // 脚本化 Provider：按序回放文本；调用前计数（等价"已计费"）。
    // ---------------------------------------------------------------------
    struct ScriptedProvider {
        outputs: Mutex<VecDeque<String>>,
        calls: std::sync::atomic::AtomicU32,
        delay_ms: u64,
    }

    impl ScriptedProvider {
        fn new(outputs: &[&str], delay_ms: u64) -> Arc<Self> {
            Arc::new(Self {
                outputs: Mutex::new(
                    outputs
                        .iter()
                        .map(|text| (*text).to_string())
                        .collect::<VecDeque<_>>(),
                ),
                calls: std::sync::atomic::AtomicU32::new(0),
                delay_ms,
            })
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl ModelProvider for ScriptedProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            // 先计费再执行：取消后计数不再增长 = 零继续计费。
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            let text = self
                .outputs
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front())
                .ok_or_else(|| "脚本输出耗尽".to_string())?;
            Ok(ModelOutput::Text(text))
        }
    }

    fn eval_case(id: &str, category: EvalCategory) -> ProductEvalCase {
        let artifact = "out/report.md";
        ProductEvalCase {
            schema_version: crate::product_eval::PRODUCT_EVAL_SCHEMA_VERSION,
            id: id.to_string(),
            category,
            title: "WorkSwarm 适配器冒烟任务".to_string(),
            instruction: "依据输入材料产出最终交付物正文。".to_string(),
            inputs: vec![InputFixture {
                path: "inputs/brief.md".to_string(),
                content: "输入材料：关键结论 A。".to_string(),
            }],
            allow_read: vec!["inputs/**".to_string()],
            allow_write: vec!["out/**".to_string()],
            expected_artifacts: vec![artifact.to_string()],
            checkers: vec![
                ArtifactChecker::Exists {
                    path: artifact.to_string(),
                },
                ArtifactChecker::Contains {
                    path: artifact.to_string(),
                    text: "交付完成".to_string(),
                },
            ],
            reference_outputs: BTreeMap::new(),
            timeout_secs: Some(30),
            max_model_calls: Some(12),
            repetitions: Some(1),
            allow_commands: Vec::new(),
        }
    }

    const LEADER_FINAL: &str =
        "# 最终交付\n交付完成：关键结论 A 已核验。\n## 结论\n采纳草稿并修正措辞。";

    // —— WorkerOutputV1 契约信封（六期一路：worker 必须返回结构化 JSON，正文在
    //    artifact.content；与 tests/product_eval_workswarm_tests.rs 同款写法）——
    const BUILDER_CONTRACT: &str = r###"{"status":"done","summary":"交付完成","artifact":{"kind":"document","format":"markdown","content":"## 草稿\n关键结论 A 的初稿，结构完整，待评审。"},"evidence":[],"open_issues":[]}"###;
    const CRITIC_CONTRACT: &str = r###"{"status":"done","summary":"{\"approved\":true,\"score\":88,\"comments\":[\"结构完整\",\"证据充分\"]}","evidence":[],"open_issues":[]}"###;
    const LEADER_CONTRACT: &str = r###"{"status":"done","summary":"最终交付","artifact":{"kind":"final","format":"markdown","content":"# 最终交付\n交付完成：关键结论 A 已核验。\n## 结论\n采纳草稿并修正措辞。"},"evidence":[],"open_issues":[]}"###;

    fn scripted_executor(provider: Arc<ScriptedProvider>, root: &Path) -> WorkSwarmExecutor {
        let mut executor = WorkSwarmExecutor::new(
            provider as Arc<dyn ModelProvider>,
            "scripted-model",
            root.join("teams"),
        );
        // 本文件脚本化的是完整三角色流水线（producer → critic → leader）：
        // 显式 ForceTeam 保持既有语义（auto 模式的自适应裁剪见 team_strategy_tests）。
        executor.config = WorkSwarmExecutorConfig {
            max_turns_per_worker: 6,
            max_retries_on_failure: 1,
            selection: crate::team_strategy::TeamSelectionMode::ForceTeam,
        };
        executor
    }

    async fn run_observed(
        executor: &WorkSwarmExecutor,
        case: &ProductEvalCase,
        cancel: Arc<AtomicBool>,
        root: &Path,
    ) -> (RawExecOutcome, TeamRunObservation, PathBuf) {
        let sandbox = root.join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let mut ctx = ExecContext::new(case, &sandbox, AgentMode::Multi, 30, 12, cancel);
        let (outcome, observation) = executor.execute_observed(&mut ctx).await.unwrap();
        (outcome, observation, sandbox)
    }

    fn fresh_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ws-eval-unit-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn real_teamrun_yields_workers_artifacts_handoffs_and_final_from_cas() {
        let root = fresh_root("happy");
        let provider =
            ScriptedProvider::new(&[BUILDER_CONTRACT, CRITIC_CONTRACT, LEADER_CONTRACT], 0);
        let case = eval_case("ws-happy", EvalCategory::Document);
        let executor = scripted_executor(provider, &root);

        let (outcome, observation, sandbox) =
            run_observed(&executor, &case, Arc::new(AtomicBool::new(false)), &root).await;

        // 真实 TeamRun：终态 Succeeded；三个角色各有一次模型调用（≥2 个 Worker 记录）。
        assert!(
            outcome.error.is_none(),
            "outcome.error = {:?}",
            outcome.error
        );
        assert_eq!(outcome.model_calls, 3);
        assert_eq!(observation.status, "Succeeded");
        assert_eq!(observation.workers.len(), 3);
        assert!(observation
            .workers
            .iter()
            .all(|worker| worker.model_calls == 1 && worker.attempts == 1));
        assert_eq!(observation.retries_used, 0);
        assert!(observation.failed_steps.is_empty());

        // 结构化 Handoff + 版本化 Artifact。
        assert!(
            observation.handoff_count >= 2,
            "handoff_count = {}",
            observation.handoff_count
        );
        assert!(observation.artifacts.len() >= 3);
        assert!(observation
            .artifacts
            .iter()
            .all(|a| a.version >= 1 && a.content_ref.starts_with("cas://sha256:")));
        let final_id = observation
            .final_artifact_ref
            .clone()
            .expect("必须记录最终 Artifact 引用");
        assert!(final_id.ends_with(":v1"), "final = {final_id}");
        let final_observation = observation
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == final_id)
            .expect("最终 Artifact 必须在产物清单中");
        assert_eq!(final_observation.kind, "final");

        // 最终结果来自 CAS 的版本化 Artifact（内容 = leader 交付文本），而非内存中的最后回复。
        let written = std::fs::read_to_string(sandbox.join("out/report.md")).unwrap();
        assert_eq!(written, LEADER_FINAL);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cancel_before_start_keeps_zero_calls_and_zero_team_activity() {
        let root = fresh_root("pre-cancel");
        let provider =
            ScriptedProvider::new(&[BUILDER_CONTRACT, CRITIC_CONTRACT, LEADER_CONTRACT], 0);
        let case = eval_case("ws-pre-cancel", EvalCategory::Document);
        let executor = scripted_executor(Arc::clone(&provider), &root);
        let cancel = Arc::new(AtomicBool::new(true));

        let (outcome, observation, _sandbox) = run_observed(&executor, &case, cancel, &root).await;

        assert!(outcome.aborted);
        assert_eq!(outcome.model_calls, 0);
        assert_eq!(provider.calls(), 0, "取消后必须零模型调用");
        assert!(observation.cancelled);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn cancel_mid_flight_freezes_provider_calls_and_marks_steps_aborted() {
        let root = fresh_root("mid-cancel");
        let provider =
            ScriptedProvider::new(&[BUILDER_CONTRACT, CRITIC_CONTRACT, LEADER_CONTRACT], 250);
        let case = eval_case("ws-mid-cancel", EvalCategory::Document);
        let executor = scripted_executor(provider.clone(), &root);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_trigger = Arc::clone(&cancel);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel_trigger.store(true, Ordering::Relaxed);
        });

        let (outcome, observation, _sandbox) = run_observed(&executor, &case, cancel, &root).await;

        assert!(outcome.aborted);
        assert!(observation.cancelled);
        assert!(
            observation
                .workers
                .iter()
                .all(|worker| worker.model_calls <= 1),
            "取消后任何 Worker 都不得继续调用模型"
        );
        // 取消返回后计费冻结：等待窗口内零新增调用（零继续计费）。
        let frozen_calls = provider.calls();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            provider.calls(),
            frozen_calls,
            "取消后出现继续计费的模型调用"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn failed_producer_reuses_local_retry_without_rerunning_successful_workers() {
        let root = fresh_root("retry");
        // 第一次产出为空（verify non_empty 失败 → 局部 Retry 只重置 producer 及其下游），
        // 随后 producer 契约信封成功、critic/leader 各一次：全程 4 次模型调用。
        let provider =
            ScriptedProvider::new(&["", BUILDER_CONTRACT, CRITIC_CONTRACT, LEADER_CONTRACT], 0);
        let case = eval_case("ws-retry", EvalCategory::Document);
        let executor = scripted_executor(provider, &root);

        let (outcome, observation, _sandbox) =
            run_observed(&executor, &case, Arc::new(AtomicBool::new(false)), &root).await;

        assert_eq!(
            observation.status, "Succeeded",
            "observation = {observation:?}"
        );
        // 六期一路：空输出 = 契约无效 → 恰好一次定向修复（writer model_calls=2、
        // output_repairs=1、attempts=1）；修复在 worker 内部消化，不触发步骤级 Retry。
        assert_eq!(observation.retries_used, 0, "workers = {observation:?}");
        let writer = observation
            .workers
            .iter()
            .find(|w| w.role == "writer")
            .expect("必须有 producer（writer）记录");
        assert_eq!(writer.output_repairs, 1, "空输出必须触发恰好一次定向修复");
        assert_eq!(writer.attempts, 1, "定向修复不算新尝试");
        assert_eq!(writer.model_calls, 2, "首次空输出 + 一次修复");
        assert_eq!(outcome.model_calls, 4, "成功 Worker 不得重跑（修复+1+1+1）");
        assert!(
            outcome.error.is_none(),
            "outcome.error = {:?}",
            outcome.error
        );
        let final_id = observation
            .final_artifact_ref
            .expect("重试后仍需交付最终 Artifact");
        let final_observation = observation
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == final_id)
            .expect("最终 Artifact 必须在产物清单中");
        assert_eq!(final_observation.kind, "final");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn worker_policy_denies_out_of_scope_and_readonly_writes() {
        let root = fresh_root("policy");
        let workspace = root.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let policy = Policy::read_only(workspace.clone());

        // 越界写入：拒绝。
        let escape = policy.evaluate(
            "write_file",
            &serde_json::json!({"path": "../../escape.md"}),
        );
        assert!(matches!(
            policy.decision(&escape),
            crate::permissions::Decision::Deny
        ));
        // 工作区内写入：只读策略拒绝（Artifact 只能经 RoleWorker/CAS 登记）。
        let inside_write = policy.evaluate("write_file", &serde_json::json!({"path": "out/ok.md"}));
        assert!(matches!(
            policy.decision(&inside_write),
            crate::permissions::Decision::Deny
        ));
        // 工作区内读取：策略放行（不 Deny；Ask 由 AutoApprover 放行）。
        let inside_read =
            policy.evaluate("read_file", &serde_json::json!({"path": "inputs/brief.md"}));
        assert!(!matches!(
            policy.decision(&inside_read),
            crate::permissions::Decision::Deny
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn role_dag_keeps_at_most_three_agent_workers_per_category() {
        for category in [
            EvalCategory::Code,
            EvalCategory::Research,
            EvalCategory::Document,
        ] {
            let case = eval_case("ws-roles", category);
            // 五期自适应组队：显式 team 强制完整流水线（producer → critic → leader）。
            let engine = crate::team_strategy::TeamStrategyEngine::default();
            let plan = engine.decide(
                crate::team_strategy::TeamSelectionMode::ForceTeam,
                &WorkSwarmExecutor::profile_of(&case),
            );
            let roles = WorkSwarmExecutor::roles_from_plan(&case, &plan);
            assert_eq!(roles.len(), 3, "category = {:?}", case.category);
            let mut names: Vec<&str> = roles.iter().map(|role| role.role.as_str()).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), 3, "角色名必须唯一");
            assert!(roles.iter().all(|role| role.assignee == "agent"));
            assert!(roles
                .iter()
                .all(|role| role.verify.as_deref() == Some("non_empty")));
            let critic = roles.iter().find(|role| role.role == "critic").unwrap();
            let leader = roles.iter().find(|role| role.role == "leader").unwrap();
            assert_eq!(critic.depends_on.len(), 1);
            assert_eq!(leader.depends_on, vec!["critic".to_string()]);
        }
    }
}
