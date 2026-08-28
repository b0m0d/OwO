// R11:goal 质量收尾完成
//! Goal：目标→计划→并行 worker→验证→仲裁→恢复 的编排层（§12 底座 / 续写 §15）。
//!
//! - [`Goal`]：目标对象（objective / 状态机 / 预算 / 验收条件）。
//! - [`Worker`] + [`WorkerRegistry`]：步骤执行抽象（测试注入 MockWorker；
//!   真实接入 `Agent::run_subagent` 由主控后续做，本模块只读引用 agent 语义）。
//! - [`GoalRunner`]：拓扑 wave 调度 + 并行度上限 + 步内重试 + 验证断言 +
//!   replan（只重建未完成子图）+ 预算熔断 + abort + 持久化恢复（已完成步骤不重跑）+ 全程审计。
//! - A2 统一调度适配层：步骤可经显式绑定定向到
//!   `in_process` / `local_process` / `fleet_node`（接口见 [`crate::execution_target`]）；
//!   显式目标不可用时等待/询问/拒绝，绝不静默切换到权限更高的目标；
//!   三类目标共用同一执行/取消/预算语义。HTTP 与节点注册细节不进入本模块。

use crate::audit::AuditLog;
use crate::blackboard::Blackboard;
use crate::capability::{CapabilityWorkerRegistry, RouteDecision, WorkerRequirement};
use crate::critic::{review_loop, CriticConfig};
use crate::execution_target::{
    dispatch_disposition, select_binding, DispatchCancelRegistry, DispatchChannel,
    DispatchDisposition, FleetDispatchWorker, FleetProbe, LocalProcessProbe, TargetAvailability,
    WorkerBinding,
};
use crate::experience_store::{Attribution, ExperienceStore, Outcome};
use crate::plan::{verify_output, Plan, StepSpec, StepStatus, VerificationSpec};
use crate::worker_pool::{PoolWorker, WorkerPool};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 目标状态机：Pending→Planning→Running→Verifying→Succeeded/Failed/Aborted。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalStatus {
    Pending,
    Planning,
    Running,
    Verifying,
    Succeeded,
    Failed,
    Aborted,
}

impl GoalStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            GoalStatus::Succeeded | GoalStatus::Failed | GoalStatus::Aborted
        )
    }
}

/// 目标预算（熔断阈值）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GoalBudget {
    /// 最大执行步骤数（含重试与 replan 消耗）。
    pub max_steps: u32,
    /// 每步最大重试次数（超出直接失败）。
    pub max_retries_per_step: u32,
    /// 全局重试次数上限（预算熔断）。
    pub max_total_retries: u32,
    /// 最大 replan 次数。
    pub max_replans: u32,
    /// 最大执行时长（秒，0 = 不限）。
    pub max_duration_secs: u64,
}

impl Default for GoalBudget {
    fn default() -> Self {
        Self {
            max_steps: 200,
            max_retries_per_step: 2,
            max_total_retries: 10,
            max_replans: 2,
            max_duration_secs: 0,
        }
    }
}

/// 目标对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    /// 目标描述（objective）。
    pub objective: String,
    pub status: GoalStatus,
    pub budget: GoalBudget,
    /// 目标级验收条件（全部通过才 Succeeded）。
    #[serde(default)]
    pub acceptance: Vec<VerificationSpec>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Goal {
    pub fn new(id: impl Into<String>, objective: impl Into<String>) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            id: id.into(),
            objective: objective.into(),
            status: GoalStatus::Pending,
            budget: GoalBudget::default(),
            acceptance: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            error: None,
        }
    }

    pub fn transition(&mut self, status: GoalStatus) {
        self.status = status;
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

/// Worker：步骤执行抽象。真实接入 `Agent::run_subagent`（主控后续做）。
#[async_trait]
pub trait Worker: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, input: &serde_json::Value) -> Result<String, String>;
}

/// 按名派发的 worker 注册表。
#[derive(Clone, Default)]
pub struct WorkerRegistry {
    workers: Arc<Mutex<HashMap<String, Arc<dyn Worker>>>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, worker: Arc<dyn Worker>) {
        let name = worker.name().to_string();
        if let Ok(mut workers) = self.workers.lock() {
            workers.insert(name, worker);
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Worker>> {
        self.workers.lock().ok().and_then(|w| w.get(name).cloned())
    }
}

/// 单步执行记录（运行状态，持久化恢复的依据）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step_id: String,
    pub status: StepStatus,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次运行的完整状态（可整体持久化：<dir>/<run_id>.json）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRunState {
    pub run_id: String,
    pub goal: Goal,
    pub plan: Plan,
    /// step_id → 执行记录。
    pub records: BTreeMap<String, StepRecord>,
    /// 全局已执行动作数（预算）。
    pub steps_taken: u32,
    /// 全局重试次数。
    pub total_retries: u32,
    /// 已执行 replan 次数。
    pub replan_count: u32,
    pub started_at: String,
    /// 顶层审计事件（文本；可选注入 AuditLog 同步写）。
    pub events: Vec<String>,
    /// 调度器是否已 abort。
    pub aborted: bool,
}

impl GoalRunState {
    pub fn new(goal: Goal, plan: Plan) -> Self {
        let records = plan
            .steps
            .iter()
            .map(|s| {
                (
                    s.id.clone(),
                    StepRecord {
                        step_id: s.id.clone(),
                        status: StepStatus::Pending,
                        attempts: 0,
                        output: None,
                        error: None,
                    },
                )
            })
            .collect();
        Self {
            run_id: format!("run-{}", chrono::Utc::now().timestamp_millis()),
            goal,
            plan,
            records,
            steps_taken: 0,
            total_retries: 0,
            replan_count: 0,
            started_at: chrono::Utc::now().to_rfc3339(),
            events: Vec::new(),
            aborted: false,
        }
    }

    /// 持久化：`<dir>/<run_id>.json`（含目标/计划/步骤记录，重启恢复）。
    pub fn persist(&self, dir: &Path) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建运行目录失败：{e}"))?;
        let path = dir.join(format!("{}.json", self.run_id));
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("运行状态序列化失败：{e}"))?;
        std::fs::write(&path, json).map_err(|e| format!("运行状态写入失败：{e}"))?;
        Ok(path)
    }

    /// 从磁盘恢复运行状态。
    pub fn load(dir: &Path, run_id: &str) -> Result<GoalRunState, String> {
        let path = dir.join(format!("{run_id}.json"));
        let json = std::fs::read_to_string(&path)
            .map_err(|e| format!("运行状态 {run_id} 读取失败：{e}（{path:?}）"))?;
        serde_json::from_str(&json).map_err(|e| format!("运行状态 {run_id} 解析失败：{e}"))
    }
}

/// 调度器配置。
#[derive(Clone)]
pub struct RunnerConfig {
    /// 并行度上限（wave 内并发执行的步骤数）。
    pub max_parallel: usize,
    /// 持久化目录（Some 时每步执行后落盘；恢复时读取）。
    pub persist_dir: Option<PathBuf>,
    /// 允许 replan（验证/执行失败时重建未完成子图）。
    pub allow_replan: bool,
    /// 是否启用 WorkerPool 子进程执行步骤（feature flag，默认关闭）。
    /// 开启后：registry 未注册的 worker 名若匹配 pool 中的 worker，则经子进程执行；
    /// 关闭时行为与纯进程内完全一致。
    pub use_worker_pool: bool,
    /// WorkerPool（`use_worker_pool=true` 时生效）。
    pub worker_pool: Option<WorkerPool>,
    /// 能力注册表（跨机路由铺路的本地语义）：步骤显式声明 `_cap` 时按能力选 worker。
    pub capability_registry: Option<CapabilityWorkerRegistry>,
    /// 全局能力需求基线（步骤 `_cap` 声明优先级更高；None = 仅按名路由）。
    pub capability_requirement: Option<WorkerRequirement>,
    /// 控制面传输（可选）：registry/pool 均未命中的 worker 经 transport 提交
    /// （失败/恢复语义沿用总线持久化；`bus_store` 重放兜底）。
    pub transport: Option<std::sync::Arc<dyn crate::fleet_transport::FleetTransport>>,
    /// 租约管理器（可选）：步骤持有任务租约，结果写入前 fencing 校验（epoch/token）。
    pub leases: Option<crate::lease::LeaseManager>,
    /// A2 统一调度适配层：显式执行绑定（非空时优先于旧解析链，严格定向派发）。
    ///
    /// - 匹配规则见 [`crate::execution_target::select_binding`]：
    ///   step id 精确匹配优先于步骤声明的 worker 名；按 step id 命中时，
    ///   执行者引用取 plan 步骤声明的 worker 名（target/预算/权限仍来自绑定）。
    /// - 安全语义：显式目标不可用时返回等待/询问/拒绝，
    ///   **绝不静默切换到权限更高的目标**。
    pub bindings: Vec<WorkerBinding>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            persist_dir: None,
            allow_replan: true,
            use_worker_pool: false,
            worker_pool: None,
            capability_registry: None,
            capability_requirement: None,
            transport: None,
            leases: None,
            bindings: Vec::new(),
        }
    }
}

impl std::fmt::Debug for RunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerConfig")
            .field("max_parallel", &self.max_parallel)
            .field("persist_dir", &self.persist_dir)
            .field("allow_replan", &self.allow_replan)
            .field("use_worker_pool", &self.use_worker_pool)
            .field("worker_pool", &self.worker_pool)
            .field("capability_registry", &self.capability_registry)
            .field("capability_requirement", &self.capability_requirement)
            .field(
                "transport",
                &self
                    .transport
                    .as_ref()
                    .map(|t| t.name().to_string())
                    .unwrap_or_else(|| "<none>".to_string()),
            )
            .field("leases", &self.leases)
            .field("bindings", &self.bindings.len())
            .finish()
    }
}

/// Goal/Plan 调度器：wave 拓扑 + 并行限流 + 重试 + 验证 + replan + 恢复 + 审计。
///
/// 可选编排原语（多 Agent P0）：
/// - [`GoalRunner::attach_critic`]：步骤输出经只读 critic 评审，意见回流 worker（步骤 input 声明 `_critic.rounds`）。
/// - [`GoalRunner::attach_blackboard`]：共享工作区状态（单写主 = 本 runner）；步骤 input 声明 `_bb.read/write`。
pub struct GoalRunner {
    pub state: GoalRunState,
    config: RunnerConfig,
    /// 可选审计（与顶层 events 同步写）。
    audit: Option<Arc<Mutex<AuditLog>>>,
    /// 跨并发任务的 abort 标志（随 state.aborted 初始化）。
    aborted_flag: Arc<std::sync::atomic::AtomicBool>,
    /// 可选 critic 评审配置（步骤 input 声明 `_critic.rounds` 时生效）。
    critic: Option<CriticConfig>,
    /// 可选共享黑板（步骤 input 声明 `_bb.read/write` 时生效；写主为本 runner 的 goal id）。
    blackboard: Option<Blackboard>,
    /// 可选经验库（worker/Goal 结果幂等写入；空闲期由主控调 `aggregate` 蒸馏技能元数据）。
    experience: Option<ExperienceStore>,
}

impl GoalRunner {
    pub fn new(goal: Goal, plan: Plan, config: RunnerConfig) -> Self {
        Self {
            state: GoalRunState::new(goal, plan),
            config,
            audit: None,
            aborted_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            critic: None,
            blackboard: None,
            experience: None,
        }
    }

    /// 从持久化状态恢复（崩溃重启；已完成步骤不重跑）。
    pub fn from_state(state: GoalRunState, config: RunnerConfig) -> Self {
        let aborted = state.aborted;
        Self {
            state,
            config,
            audit: None,
            aborted_flag: Arc::new(std::sync::atomic::AtomicBool::new(aborted)),
            critic: None,
            blackboard: None,
            experience: None,
        }
    }

    /// 注入审计日志（每次动作写一条 audit 记录 + 顶层事件）。
    pub fn attach_audit(&mut self, log: Arc<Mutex<AuditLog>>) {
        self.audit = Some(log);
    }

    /// 注入经验库：步骤/Goal 结果以 correlation_id 幂等写入（崩溃恢复/空闲聚合的数据源）。
    pub fn attach_experience(&mut self, store: ExperienceStore) {
        self.experience = Some(store);
    }

    /// 注入 critic 评审配置：步骤声明 `_critic.rounds` 时输出经只读评审后回流 worker。
    pub fn attach_critic(&mut self, config: CriticConfig) {
        self.critic = Some(config);
    }

    /// 注入共享黑板：步骤声明 `_bb.read` 读取共享中间结果、`_bb.write` 写回。
    /// 黑板写主应为该 goal 的 id（`Blackboard::new(goal.id, policy)`）。
    pub async fn attach_blackboard(&mut self, blackboard: Blackboard) {
        self.blackboard = Some(blackboard);
    }

    fn log(&mut self, event: &str, detail: impl Into<String>) {
        let detail = detail.into();
        self.state.events.push(format!("{event}: {detail}"));
        if let Some(log) = &self.audit {
            if let Ok(mut log) = log.lock() {
                log.record(
                    &self.state.goal.id,
                    event,
                    Some(format!("goal/{}", self.state.plan.goal_id)),
                    None,
                    detail,
                );
            }
        }
    }

    fn record_mut(&mut self, step_id: &str) -> &mut StepRecord {
        self.state
            .records
            .get_mut(step_id)
            .expect("步骤记录必须存在")
    }

    /// 取消执行：abort 标志置位，未完成步骤标记 Aborted 保留现场。
    pub fn abort(&mut self) {
        self.state.aborted = true;
        self.aborted_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.mark_remaining(StepStatus::Aborted);
        self.log("goal.abort", "调度器收到 abort 请求");
        self.state.goal.transition(GoalStatus::Aborted);
        self.persist_if_needed();
    }

    /// 执行计划（恢复时已完成步骤自动跳过）。返回目标终态。
    /// 结束后把 Goal 结果幂等写入经验库（若有）。
    pub async fn run(&mut self, workers: &WorkerRegistry) -> Result<GoalStatus, String> {
        let status = self.run_inner(workers).await?;
        if let Some(exp) = &self.experience {
            let outcome = match status {
                GoalStatus::Succeeded => Outcome::Success,
                GoalStatus::Aborted => Outcome::Aborted,
                _ => Outcome::Failure,
            };
            let attribution = Attribution {
                goal_id: Some(self.state.goal.id.clone()),
                plan_id: Some(self.state.plan.id.clone()),
                step_id: None,
                input_keys: Vec::new(),
                error: self.state.goal.error.clone(),
            };
            let _ = exp.record_goal_outcome(
                self.state.run_id.clone(),
                self.state.goal.id.clone(),
                outcome,
                attribution,
            );
        }
        Ok(status)
    }

    /// 执行主体（run 的收尾经验写入拆在 [`Self::run`]）。
    async fn run_inner(&mut self, workers: &WorkerRegistry) -> Result<GoalStatus, String> {
        if self.state.goal.status.is_terminal() {
            return Ok(self.state.goal.status);
        }
        if self.state.aborted {
            return Ok(GoalStatus::Aborted);
        }
        self.state.goal.transition(GoalStatus::Running);
        self.log("goal.start", format!("目标 {}", self.state.goal.objective));

        let started = std::time::Instant::now();
        let budget = self.state.goal.budget;
        let max_parallel = self.config.max_parallel.max(1);
        let abort_flag = Arc::clone(&self.aborted_flag);
        let workers = workers.clone();
        let bb_writer = if let Some(bb) = &self.blackboard {
            Some(bb.writer().await)
        } else {
            None
        };
        let rt = StepRuntime {
            budget,
            aborted: abort_flag,
            critic: self.critic.clone(),
            blackboard: self.blackboard.clone(),
            bb_writer,
            use_worker_pool: self.config.use_worker_pool,
            worker_pool: self.config.worker_pool.clone(),
            capabilities: self.config.capability_registry.clone(),
            capability_requirement: self.config.capability_requirement.clone(),
            transport: self.config.transport.clone(),
            leases: self.config.leases.clone(),
            bindings: self.config.bindings.clone(),
            run_id: self.state.run_id.clone(),
            cancels: DispatchCancelRegistry::default(),
        };

        loop {
            if self.state.aborted {
                self.mark_remaining(StepStatus::Aborted);
                self.state.goal.transition(GoalStatus::Aborted);
                self.persist_if_needed();
                return Ok(GoalStatus::Aborted);
            }
            // 时长预算熔断。
            if budget.max_duration_secs > 0
                && started.elapsed().as_secs() >= budget.max_duration_secs
            {
                return self.fail_goal(format!(
                    "预算熔断：执行时长超过 {}s",
                    budget.max_duration_secs
                ));
            }
            // 步骤数预算熔断。
            if self.state.steps_taken >= budget.max_steps {
                return self.fail_goal(format!(
                    "预算熔断：步骤数超过 {}（steps_taken={}）",
                    budget.max_steps, self.state.steps_taken
                ));
            }
            // 全局重试预算熔断。
            if self.state.total_retries >= budget.max_total_retries {
                return self.fail_goal(format!(
                    "预算熔断：全局重试次数超过 {}（total_retries={}）",
                    budget.max_total_retries, self.state.total_retries
                ));
            }

            // 计算当前就绪步骤：依赖全部 Succeeded 且自身未完成。
            let ready: Vec<StepSpec> = self
                .state
                .plan
                .steps
                .iter()
                .filter(|step| {
                    let record = &self.state.records[&step.id];
                    record.status.can_resume() && self.deps_succeeded(step)
                })
                .cloned()
                .collect();
            if ready.is_empty() {
                // 检查是否全部成功 → 目标验收。
                if self
                    .state
                    .plan
                    .steps
                    .iter()
                    .all(|s| self.state.records[&s.id].status == StepStatus::Succeeded)
                {
                    return self.verify_goal();
                }
                if self.state.replan_count >= budget.max_replans {
                    return self.fail_goal("replan 次数超限，未完成步骤无法恢复".to_string());
                }
                return self.fail_goal("死锁：存在未完成步骤但无就绪步骤".to_string());
            }

            // 并行执行就绪步骤（JoinSet + max_parallel 限流）。
            let mut set = tokio::task::JoinSet::new();
            let mut pending: std::collections::VecDeque<StepSpec> = ready.into_iter().collect();
            let mut failed: Vec<StepSpec> = Vec::new();
            let mut stop_error: Option<StepStop> = None;
            while let Some(step) = pending.pop_front() {
                while set.len() >= max_parallel && stop_error.is_none() {
                    if let Err(stop) = self.merge_step_outcome(set.join_next().await, &mut failed) {
                        stop_error = Some(stop);
                    }
                }
                if stop_error.is_some() {
                    break;
                }
                let target_note = select_binding(&self.config.bindings, &step.id, &step.worker)
                    .map(|b| {
                        format!(
                            "，target={} node={}",
                            b.target.kind(),
                            b.target.node_id().unwrap_or("-")
                        )
                    })
                    .unwrap_or_default();
                self.log(
                    "goal.step.start",
                    format!("步骤 {}（worker {}）{}", step.id, step.worker, target_note),
                );
                set.spawn(run_step_attempts(workers.clone(), step, rt.clone()));
            }
            if stop_error.is_none() {
                while let Some(joined) = set.join_next().await {
                    if let Err(stop) = self.merge_step_outcome(Some(joined), &mut failed) {
                        stop_error = Some(stop);
                        break;
                    }
                }
            }
            // 早退路径（abort/预算熔断/目标拒绝）：先终止在飞步骤任务，避免孤儿任务继续运行，
            // 并把在飞的远端派发任务统一 cancel（防 transport 残留 pending）。
            set.abort_all();
            while set.join_next().await.is_some() {}
            rt.cancels.cancel_all().await;
            match stop_error {
                Some(StepStop::Budget(reason)) => {
                    return self.fail_goal(format!("预算熔断：{reason}"));
                }
                Some(StepStop::Fatal(reason)) => {
                    return self.fail_goal(reason);
                }
                None => {}
            }

            if self.state.aborted {
                self.mark_remaining(StepStatus::Aborted);
                self.state.goal.transition(GoalStatus::Aborted);
                self.persist_if_needed();
                return Ok(GoalStatus::Aborted);
            }

            if !failed.is_empty() {
                if !self.config.allow_replan {
                    return self.fail_goal(format!(
                        "步骤失败且 replan 未启用：{:?}",
                        failed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
                    ));
                }
                if self.state.replan_count >= budget.max_replans {
                    return self.fail_goal(format!(
                        "步骤失败且 replan 次数超限（{}）：{:?}",
                        budget.max_replans,
                        failed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
                    ));
                }
                self.replan(&failed);
            }
        }
    }

    /// 合并一个步骤的并发执行结果到运行状态。失败步骤加入 failed；
    /// 预算熔断返回 [`StepStop::Budget`]，确定性失败返回 [`StepStop::Fatal`]。
    fn merge_step_outcome(
        &mut self,
        joined: Option<Result<StepOutcome, tokio::task::JoinError>>,
        failed: &mut Vec<StepSpec>,
    ) -> Result<(), StepStop> {
        let outcome = match joined {
            Some(Ok(outcome)) => outcome,
            // 既有语义保持：panic/丢失按「预算熔断」表述收尾。
            Some(Err(e)) => return Err(StepStop::Budget(format!("步骤任务 panic：{e}"))),
            None => return Err(StepStop::Budget("步骤任务丢失".to_string())),
        };
        self.state.steps_taken = self.state.steps_taken.saturating_add(outcome.attempts);
        self.state.total_retries = self.state.total_retries.saturating_add(outcome.attempts);
        match outcome.result {
            StepResult::Ok { ref output } => {
                let record = self.record_mut(&outcome.step_id);
                record.status = StepStatus::Succeeded;
                record.attempts = outcome.attempts;
                record.output = Some(output.clone());
                record.error = None;
                self.record_worker_experience(&outcome, true, None);
                self.mark_step_health(&outcome, true);
                self.log(
                    "goal.step.succeeded",
                    format!(
                        "步骤 {} 通过（{} 次尝试）",
                        outcome.step_id, outcome.attempts
                    ),
                );
                self.persist_if_needed();
            }
            StepResult::Retried { ref error } => {
                let record = self.record_mut(&outcome.step_id);
                record.status = StepStatus::Failed;
                record.attempts = outcome.attempts;
                record.error = Some(error.clone());
                self.record_worker_experience(&outcome, false, Some(error));
                self.mark_step_health(&outcome, false);
                self.log(
                    "goal.step.failed",
                    format!("步骤 {} 失败：{error}", outcome.step_id),
                );
                if let Some(step) = self.state.plan.step(&outcome.step_id) {
                    failed.push(step.clone());
                }
            }
            StepResult::Budget { reason } => {
                return Err(StepStop::Budget(reason));
            }
            StepResult::Fatal { ref reason } => {
                // 确定性失败：只记录现场，不写 worker 健康/经验（执行体根本未运行），
                // 不参与 replan；原因直达目标终态。
                let record = self.record_mut(&outcome.step_id);
                record.status = StepStatus::Failed;
                record.attempts = outcome.attempts;
                record.error = Some(reason.clone());
                self.log(
                    "goal.step.rejected",
                    format!("步骤 {} 目标不可用终止：{reason}", outcome.step_id),
                );
                self.persist_if_needed();
                return Err(StepStop::Fatal(reason.clone()));
            }
        }
        Ok(())
    }

    /// 步骤结果反馈能力注册表健康度（worker 生命周期事件接线；路由会跳过失败过多的 worker）。
    fn mark_step_health(&self, outcome: &StepOutcome, ok: bool) {
        let Some(reg) = &self.config.capability_registry else {
            return;
        };
        if let Some(step) = self.state.plan.step(&outcome.step_id) {
            reg.mark_health(&step.worker, ok);
        }
    }

    /// 把单步结果幂等写入经验库（correlation_id = `run_id:step_id`；重放/重跑不重复）。
    fn record_worker_experience(&self, outcome: &StepOutcome, ok: bool, error: Option<&str>) {
        let Some(exp) = &self.experience else {
            return;
        };
        let Some(step) = self.state.plan.step(&outcome.step_id) else {
            return;
        };
        let attribution = Attribution {
            goal_id: Some(self.state.goal.id.clone()),
            plan_id: Some(self.state.plan.id.clone()),
            step_id: Some(step.id.clone()),
            input_keys: step
                .input
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default(),
            error: error.map(|e| e.to_string()),
        };
        let result = if ok {
            Outcome::Success
        } else {
            Outcome::Failure
        };
        let _ = exp.record_worker_outcome(
            format!("{}:{}", self.state.run_id, step.id),
            step.worker.clone(),
            result,
            attribution,
        );
    }

    /// replan：重置失败步骤及其未完成的后代子图（已 Succeeded 步骤保留），重新调度。
    fn replan(&mut self, failed: &[StepSpec]) {
        self.state.replan_count += 1;
        self.log(
            "goal.replan",
            format!(
                "第 {} 次 replan：重置 {}",
                self.state.replan_count,
                failed
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        // 收集受影响的未完成后代（依赖链中包含 failed 步骤的）。
        let mut to_reset: Vec<String> = Vec::new();
        for step in &self.state.plan.steps {
            if self.state.records[&step.id].status == StepStatus::Succeeded {
                continue;
            }
            if self.depends_on_any(
                step,
                &failed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ) {
                to_reset.push(step.id.clone());
            }
        }
        for step_id in &to_reset {
            let record = self.record_mut(step_id);
            record.status = StepStatus::Pending;
            record.error = None;
            record.attempts = 0;
        }
        // 失败的步骤本身也要重置（含在 to_reset 中，因为 depends_on_any 对自身成立时）——
        // 显式重置避免依赖判断遗漏。
        for step in failed {
            if let Some(record) = self.state.records.get_mut(&step.id) {
                if !to_reset.contains(&step.id) {
                    record.status = StepStatus::Pending;
                    record.error = None;
                    record.attempts = 0;
                }
            }
        }
        self.persist_if_needed();
    }

    fn depends_on_any(&self, step: &StepSpec, targets: &[&str]) -> bool {
        let mut stack: Vec<&str> = step.depends_on.iter().map(|s| s.as_str()).collect();
        let mut visited = std::collections::HashSet::new();
        while let Some(dep) = stack.pop() {
            if targets.contains(&dep) {
                return true;
            }
            if !visited.insert(dep) {
                continue;
            }
            if let Some(dep_step) = self.state.plan.step(dep) {
                stack.extend(dep_step.depends_on.iter().map(|s| s.as_str()));
            }
        }
        false
    }

    fn deps_succeeded(&self, step: &StepSpec) -> bool {
        step.depends_on.iter().all(|dep| {
            self.state
                .records
                .get(dep)
                .map(|r| r.status == StepStatus::Succeeded)
                .unwrap_or(false)
        })
    }

    /// 目标级验收：全部 acceptance 通过才 Succeeded。
    fn verify_goal(&mut self) -> Result<GoalStatus, String> {
        self.state.goal.transition(GoalStatus::Verifying);
        self.log("goal.verifying", "全部步骤成功，进入目标验收");
        for spec in &self.state.goal.acceptance {
            // 验收条件对"最终汇总输出"断言：取全部成功步骤输出拼接。
            let summary = self
                .state
                .records
                .values()
                .filter(|r| r.status == StepStatus::Succeeded)
                .filter_map(|r| r.output.clone())
                .collect::<Vec<_>>()
                .join("\n");
            if let Err(e) = verify_output(spec, &summary) {
                return self.fail_goal(format!("目标验收失败：{e}"));
            }
        }
        self.state.goal.transition(GoalStatus::Succeeded);
        self.log("goal.succeeded", "目标验收通过");
        self.persist_if_needed();
        Ok(GoalStatus::Succeeded)
    }

    fn fail_goal(&mut self, reason: String) -> Result<GoalStatus, String> {
        self.state.goal.error = Some(reason.clone());
        self.state.goal.transition(GoalStatus::Failed);
        self.log("goal.failed", reason);
        self.persist_if_needed();
        Ok(GoalStatus::Failed)
    }

    fn mark_remaining(&mut self, status: StepStatus) {
        for record in self.state.records.values_mut() {
            if !record.status.is_terminal() {
                record.status = status;
            }
        }
    }

    fn persist_if_needed(&mut self) {
        if let Some(dir) = &self.config.persist_dir {
            let _ = self.state.persist(dir);
        }
    }
}

// ---------- 并发执行辅助 ----------

/// 步骤执行结果。
enum StepResult {
    Ok {
        output: String,
    },
    Retried {
        error: String,
    },
    Budget {
        reason: String,
    },
    /// 确定性失败：不重试、不参与 replan，原因直达目标终态。
    /// A2 语义：显式目标被拒绝（Reject）或需用户确认（AskUser）——
    /// 绝不允许静默改派，故直接以原因终止而非消耗重试预算。
    Fatal {
        reason: String,
    },
}

/// wave 内提前终止原因：Budget 沿用既有「预算熔断」终态表述；
/// Fatal 直接以原因作为目标终态错误。
enum StepStop {
    Budget(String),
    Fatal(String),
}

/// 单个步骤的并发执行产出（含尝试次数，供预算合并）。
struct StepOutcome {
    step_id: String,
    attempts: u32,
    result: StepResult,
}

/// 单步运行环境（预算 / abort / 可选 critic / 黑板 / worker pool / 能力路由 / 传输 / 租约，随步骤任务克隆）。
/// A2：另携带显式执行绑定（定向派发）与在飞远端任务取消登记表。
#[derive(Clone)]
struct StepRuntime {
    budget: GoalBudget,
    aborted: Arc<std::sync::atomic::AtomicBool>,
    critic: Option<CriticConfig>,
    blackboard: Option<Blackboard>,
    bb_writer: Option<String>,
    use_worker_pool: bool,
    worker_pool: Option<WorkerPool>,
    capabilities: Option<CapabilityWorkerRegistry>,
    capability_requirement: Option<WorkerRequirement>,
    transport: Option<std::sync::Arc<dyn crate::fleet_transport::FleetTransport>>,
    leases: Option<crate::lease::LeaseManager>,
    /// 显式执行绑定（空 = 未配置，走旧解析链）。
    bindings: Vec<WorkerBinding>,
    /// 运行 id（默认 correlation 兜底 `{run_id}:{step_id}`）。
    run_id: String,
    /// 在飞远端派发任务登记表（abort 收尾统一 cancel）。
    cancels: DispatchCancelRegistry,
}

/// 显式绑定解析结果（A2 定向派发与旧解析链的分界）。
enum WorkerResolution {
    /// 命中显式绑定并装配完成（含生效绑定视图，供 `_dispatch` 注入与预算封顶）。
    Directed(Arc<dyn Worker>, WorkerBinding),
    /// 未配置绑定时走旧解析链命中（行为完全兼容）。
    Legacy(Arc<dyn Worker>),
    /// 绑定命中但目标不可用：等待 / 询问 / 拒绝（绝不改派其他目标）。
    Disposition(DispatchDisposition),
    /// 旧解析链未命中（保持既有可重试错误语义与文案）。
    Unresolved(String),
}

/// A2 统一派发解析：
/// 1) 先解析显式绑定（step id 匹配 > worker 名匹配）；命中即严格定向，
///    目标探测不足时产出 Wait/AskUser/Reject 裁定，不进入任何回退路径；
/// 2) 未配置绑定时保持旧行为兼容（registry → 池 → 能力路由 → transport）。
async fn resolve_execution(
    workers: &WorkerRegistry,
    step: &StepSpec,
    rt: &StepRuntime,
) -> WorkerResolution {
    if let Some(mut binding_view) = select_binding(&rt.bindings, &step.id, &step.worker).cloned() {
        // 按 step id 命中的绑定：执行者引用取 plan 步骤声明的 worker 名，
        // target/能力/权限/预算/关联信息仍来自绑定本身。
        if binding_view.worker == step.id {
            binding_view.worker = step.worker.clone();
        }
        let availability = probe_target_availability(workers, step, rt).await;
        return match dispatch_disposition(&binding_view, &availability) {
            ready @ DispatchDisposition::Ready(_) => WorkerResolution::Directed(
                build_channel_worker(workers, rt, step, ready, &binding_view),
                binding_view,
            ),
            disposition => WorkerResolution::Disposition(disposition),
        };
    }
    match legacy_resolve_worker(workers, step, rt).await {
        Ok(Some(worker)) => WorkerResolution::Legacy(worker),
        Ok(None) => WorkerResolution::Unresolved(format!("worker 未注册：{}", step.worker)),
        Err(reason) => WorkerResolution::Unresolved(reason),
    }
}

/// 三类目标的可用性探测（无远程接口细节；远端仅判断是否配置了传输）。
async fn probe_target_availability(
    workers: &WorkerRegistry,
    step: &StepSpec,
    rt: &StepRuntime,
) -> TargetAvailability {
    TargetAvailability {
        in_process_ready: workers.get(&step.worker).is_some(),
        local_process: match (&rt.use_worker_pool, &rt.worker_pool) {
            (false, _) => LocalProcessProbe::NotConfigured,
            (true, None) => LocalProcessProbe::NotConfigured,
            (true, Some(pool)) => {
                if pool.contains(&step.worker).await {
                    LocalProcessProbe::Ready
                } else {
                    LocalProcessProbe::WorkerMissing
                }
            }
        },
        fleet: match &rt.transport {
            None => FleetProbe::NotConfigured,
            Some(_) => FleetProbe::Ready,
        },
    }
}

/// 把 Ready 裁定装配为可执行的通道 worker。
fn build_channel_worker(
    workers: &WorkerRegistry,
    rt: &StepRuntime,
    step: &StepSpec,
    ready: DispatchDisposition,
    binding: &WorkerBinding,
) -> Arc<dyn Worker> {
    let resolved = match ready {
        DispatchDisposition::Ready(resolved) => resolved,
        other => unreachable!("非 Ready 裁定不应装配通道：{other:?}"),
    };
    let correlation_fallback = format!("{}:{}", rt.run_id, step.id);
    match resolved.channel {
        DispatchChannel::Registry(name) => {
            // 探测已确认命中；unwrap_or_else 仅防御编程错误。
            workers
                .get(&name)
                .unwrap_or_else(|| panic!("定向派发内部错误：registry 探测通过但未找到 {name}"))
        }
        DispatchChannel::Pool(id) => {
            let pool = rt
                .worker_pool
                .as_ref()
                .unwrap_or_else(|| panic!("定向派发内部错误：池探测通过但 WorkerPool 缺失"));
            Arc::new(PoolWorker::new(pool.clone(), id))
        }
        DispatchChannel::Fleet { node_id, .. } => {
            let transport =
                rt.transport.as_ref().map(Arc::clone).unwrap_or_else(|| {
                    panic!("定向派发内部错误：传输探测通过但 FleetTransport 缺失")
                });
            Arc::new(FleetDispatchWorker::from_binding(
                resolved.binding.worker.clone(),
                node_id,
                binding,
                resolved
                    .binding
                    .effective_correlation_id(&correlation_fallback),
                transport,
                rt.cancels.clone(),
            ))
        }
    }
}

/// 旧解析链（保留原文案与顺序）：registry 优先（进程内语义）；feature flag 开启时
/// 回退到 worker pool 子进程；未命中且配置传输时经 transport 提交（跨机铺路）；
/// 步骤显式声明 `_cap` 时按能力路由选 worker。仅在**未配置**显式绑定时启用。
async fn legacy_resolve_worker(
    workers: &WorkerRegistry,
    step: &StepSpec,
    rt: &StepRuntime,
) -> Result<Option<Arc<dyn Worker>>, String> {
    let name = &step.worker;
    if let Some(worker) = workers.get(name) {
        return Ok(Some(worker));
    }
    if rt.use_worker_pool {
        if let Some(pool) = &rt.worker_pool {
            if pool.contains(name).await {
                return Ok(Some(Arc::new(PoolWorker::new(
                    pool.clone(),
                    name.to_string(),
                ))));
            }
        }
    }
    // 能力路由（仅当步骤显式声明能力需求 `_cap`，或 runner 配置了全局需求基线时启用；
    // 此时 worker 名仅为提示，按能力匹配选择执行者）。
    let requirement = step_requirement(step, rt);
    if let Some(req) = requirement {
        if let Some(reg) = &rt.capabilities {
            match reg.route(&req) {
                RouteDecision::Pick(id) => {
                    if let Some(pool) = &rt.worker_pool {
                        if pool.contains(&id).await {
                            return Ok(Some(Arc::new(PoolWorker::new(pool.clone(), id))));
                        }
                    }
                    if let Some(transport) = &rt.transport {
                        return Ok(Some(Arc::new(
                            crate::fleet_transport::TransportWorker::new(transport.clone(), id),
                        )));
                    }
                    return Err(format!("能力路由选中 worker {id}，但池/传输均未注册"));
                }
                RouteDecision::Degrade { worker, missing } => {
                    tracing::warn!(
                        worker = %worker,
                        missing = ?missing,
                        "能力路由降级：缺失 {}",
                        missing.join(", ")
                    );
                    if let Some(pool) = &rt.worker_pool {
                        if pool.contains(&worker).await {
                            return Ok(Some(Arc::new(PoolWorker::new(pool.clone(), worker))));
                        }
                    }
                    if let Some(transport) = &rt.transport {
                        return Ok(Some(Arc::new(
                            crate::fleet_transport::TransportWorker::new(transport.clone(), worker),
                        )));
                    }
                    return Err(format!(
                        "能力路由降级选中 worker {worker}，但池/传输均未注册（缺失：{}）",
                        missing.join(", ")
                    ));
                }
                RouteDecision::Reject { reasons } => {
                    return Err(format!("能力不满足，无可用 worker：{}", reasons.join("；")));
                }
            }
        }
    }
    // 未注册 worker 名且配置了传输：经 transport 提交（失败/恢复沿用总线持久化）。
    if let Some(transport) = &rt.transport {
        return Ok(Some(Arc::new(
            crate::fleet_transport::TransportWorker::new(transport.clone(), name.to_string()),
        )));
    }
    Ok(None)
}

/// 步骤能力需求：步骤 `_cap` 显式声明优先；否则用 runner 全局基线；默认空需求视为未启用。
fn step_requirement(step: &StepSpec, rt: &StepRuntime) -> Option<WorkerRequirement> {
    if let Some(cap) = step.input.get("_cap") {
        if let Ok(req) = serde_json::from_value::<WorkerRequirement>(cap.clone()) {
            return Some(req);
        }
    }
    rt.capability_requirement
        .clone()
        .filter(|r| *r != WorkerRequirement::default())
}

/// 步骤租约 RAII：步骤结束（成功/失败/预算/abort/取消）自动释放租约，
/// 防 `goal:<step_id>` 租约表泄漏（孤儿持有者）。token 匹配才释放。
struct StepLeaseGuard {
    leases: Option<crate::lease::LeaseManager>,
    holder: String,
    token: Option<String>,
}

impl Drop for StepLeaseGuard {
    fn drop(&mut self) {
        if let (Some(leases), Some(token)) = (&self.leases, &self.token) {
            let _ = leases.release(&self.holder, token);
        }
    }
}

/// 定向裁定 → 统一步骤结果映射：
/// Wait 按可重试错误参与既有重试/失败语义；AskUser 与 Reject 为确定性失败
/// （不消耗重试预算、不参与 replan，原因直达目标终态）。
fn disposition_outcome(step: &StepSpec, disposition: DispatchDisposition) -> StepOutcome {
    let result = match disposition {
        DispatchDisposition::Ready(_) => unreachable!("Ready 裁定不会进入失败映射"),
        DispatchDisposition::Wait { reason } => StepResult::Retried {
            error: format!("目标暂不可用（wait）：{reason}"),
        },
        DispatchDisposition::AskUser { prompt } => StepResult::Fatal {
            reason: format!("需用户确认：{prompt}"),
        },
        DispatchDisposition::Reject { reason } => StepResult::Fatal {
            reason: format!("目标拒绝：{reason}"),
        },
    };
    StepOutcome {
        step_id: step.id.clone(),
        attempts: 0,
        result,
    }
}

/// 独立执行一个步骤的尝试循环（worker 调用 + 验证断言 + 可选 critic/黑板；不触碰 runner 状态）。
/// 预算在任务内按 `budget.max_steps` 粗略封顶，全局熔断由 run() 合并后校验。
///
/// 步骤 input 可选约定（多 Agent P0 编排原语，`_` 前缀键）：
/// - `"_critic": { "rounds": N }`：输出经只读 critic 评审，意见回流 worker 重跑，最多 N 轮。
/// - `"_bb": { "read": ["key"], "write": "key" }`：读取黑板中间结果（`{{bb:key}}` 占位符替换）与写回。
async fn run_step_attempts(
    workers: WorkerRegistry,
    step: StepSpec,
    rt: StepRuntime,
) -> StepOutcome {
    // A2：显式绑定优先（严格定向），未配置时走旧解析链。
    let mut directed_binding: Option<WorkerBinding> = None;
    let worker = match resolve_execution(&workers, &step, &rt).await {
        WorkerResolution::Directed(worker, binding) => {
            directed_binding = Some(binding);
            worker
        }
        WorkerResolution::Legacy(worker) => worker,
        WorkerResolution::Unresolved(error) => {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts: 0,
                result: StepResult::Retried { error },
            }
        }
        WorkerResolution::Disposition(disposition) => {
            return disposition_outcome(&step, disposition);
        }
    };
    let prepared_input = match prepare_step_input(&step.input, &rt.blackboard).await {
        Ok(input) => input,
        Err(e) => {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts: 0,
                result: StepResult::Retried { error: e },
            }
        }
    };
    // 定向绑定把派发上下文注入 `_dispatch`（correlation / CAS 引用 / node 对下游可见）。
    let input = match &directed_binding {
        Some(binding) => {
            let correlation =
                binding.effective_correlation_id(&format!("{}:{}", rt.run_id, step.id));
            let mut injected = prepared_input;
            binding.inject_dispatch_context(&mut injected, &correlation);
            injected
        }
        None => prepared_input,
    };
    // 租约：步骤任务持有（fencing 语义；写结果前校验 epoch/token，防分区双写）。
    // RAII guard：任何返回路径自动 release（防租约表孤儿持有者泄漏）。
    let step_lease = match &rt.leases {
        Some(leases) => {
            let holder = format!("goal:{}", step.id);
            match leases.acquire(&holder) {
                Ok(lease) => Some(lease),
                Err(e) => {
                    return StepOutcome {
                        step_id: step.id.clone(),
                        attempts: 0,
                        result: StepResult::Retried {
                            error: format!("步骤租约获取失败：{e}"),
                        },
                    }
                }
            }
        }
        None => None,
    };
    let _step_lease_guard = step_lease.as_ref().map(|lease| StepLeaseGuard {
        leases: rt.leases.clone(),
        holder: lease.holder.clone(),
        token: Some(lease.token.clone()),
    });
    let critic_rounds = step
        .input
        .get("_critic")
        .and_then(|v| v.get("rounds"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    // 尝试上限：绑定预算 > 0 时对 plan 声明的 retries+1 取 min；时长预算 0 = 不限。
    let base_max_attempts = step.retries.saturating_add(1);
    let max_attempts = directed_binding
        .as_ref()
        .map(|b| b.cap_attempts(base_max_attempts))
        .unwrap_or(base_max_attempts);
    let duration_cap_secs = directed_binding
        .as_ref()
        .map(|b| b.budget.max_duration_secs)
        .unwrap_or(0);
    let started_at = std::time::Instant::now();
    let mut attempts = 0u32;
    while attempts < max_attempts {
        if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result: StepResult::Retried {
                    error: "调度器已 abort".to_string(),
                },
            };
        }
        // 绑定级时长预算（三类目标统一语义；超限按可重试错误退出）。
        if duration_cap_secs > 0 && started_at.elapsed().as_secs() >= duration_cap_secs {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result: StepResult::Retried {
                    error: format!("绑定预算耗尽：时长超过 {duration_cap_secs}s"),
                },
            };
        }
        if attempts >= rt.budget.max_steps {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result: StepResult::Budget {
                    reason: format!("步骤数超过 {}", rt.budget.max_steps),
                },
            };
        }
        attempts += 1;
        // 取消传播：在飞步骤任务随 abort 标志即时终止（池路径经 cancel_all 传播到子进程）。
        match run_worker_cancellable(&worker, &input, &rt).await {
            Ok(output) => {
                // fencing 写校验：租约失效（过期/重连/分区）时拒绝写入结果，
                // 按血缘重算（replan 重置）而非重复写。
                if let (Some(leases), Some(lease)) = (&rt.leases, &step_lease) {
                    if let Err(e) = leases.verify_write(&lease.holder, &lease.token, lease.epoch) {
                        return StepOutcome {
                            step_id: step.id.clone(),
                            attempts,
                            result: StepResult::Retried {
                                error: format!("fencing 拒绝写入：{e}"),
                            },
                        };
                    }
                }
                if let Some(spec) = &step.verify {
                    if let Err(e) = verify_output(spec, &output) {
                        if attempts >= max_attempts {
                            return StepOutcome {
                                step_id: step.id.clone(),
                                attempts,
                                result: StepResult::Retried { error: e },
                            };
                        }
                        continue;
                    }
                }
                // 可选 critic 评审：意见回流 worker 重跑。
                if critic_rounds > 0 {
                    let final_output = match run_step_critic(
                        &worker,
                        &input,
                        &output,
                        &step,
                        &rt,
                        critic_rounds,
                        &mut attempts,
                    )
                    .await
                    {
                        Ok(out) => out,
                        Err(e) => {
                            return StepOutcome {
                                step_id: step.id.clone(),
                                attempts,
                                result: StepResult::Retried { error: e },
                            }
                        }
                    };
                    if attempts > rt.budget.max_steps {
                        return StepOutcome {
                            step_id: step.id.clone(),
                            attempts,
                            result: StepResult::Budget {
                                reason: format!("critic 评审后步骤数超过 {}", rt.budget.max_steps),
                            },
                        };
                    }
                    return finish_step(&step, &rt, final_output, attempts).await;
                }
                return finish_step(&step, &rt, output, attempts).await;
            }
            Err(e) => {
                if attempts >= max_attempts {
                    return StepOutcome {
                        step_id: step.id.clone(),
                        attempts,
                        result: StepResult::Retried { error: e },
                    };
                }
            }
        }
    }
    StepOutcome {
        step_id: step.id.clone(),
        attempts,
        result: StepResult::Retried {
            error: "未知错误".to_string(),
        },
    }
}

/// 执行 worker 且响应 abort 传播：abort 标志置位时立即终止等待，
/// 池路径经 `cancel_all` 把取消传播到子进程（submit 以 Cancelled 立即可见）。
async fn run_worker_cancellable(
    worker: &Arc<dyn Worker>,
    input: &serde_json::Value,
    rt: &StepRuntime,
) -> Result<String, String> {
    let aborted = Arc::clone(&rt.aborted);
    tokio::select! {
        out = worker.run(input) => out,
        _ = wait_aborted(aborted) => {
            if let Some(pool) = &rt.worker_pool {
                let _ = pool.cancel_all().await;
            }
            // A2：abort 即时取消在飞的远端派发任务（防残留 pending）。
            rt.cancels.cancel_all().await;
            Err("调度器已 abort".to_string())
        }
    }
}

/// 轮询 abort 标志（20ms 粒度；预算/取消响应的最低时延）。
async fn wait_aborted(flag: Arc<std::sync::atomic::AtomicBool>) {
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 步骤成功收尾：可选黑板写回 + 组装成功结果。
async fn finish_step(
    step: &StepSpec,
    rt: &StepRuntime,
    output: String,
    attempts: u32,
) -> StepOutcome {
    if let (Some(bb), Some(writer)) = (&rt.blackboard, &rt.bb_writer) {
        if let Some(key) = step
            .input
            .get("_bb")
            .and_then(|v| v.get("write"))
            .and_then(|v| v.as_str())
        {
            if let Err(e) = bb
                .write(writer, key, serde_json::Value::String(output.clone()))
                .await
            {
                return StepOutcome {
                    step_id: step.id.clone(),
                    attempts,
                    result: StepResult::Retried {
                        error: format!("blackboard 写入失败：{e}"),
                    },
                };
            }
        }
    }
    StepOutcome {
        step_id: step.id.clone(),
        attempts,
        result: StepResult::Ok { output },
    }
}

/// 步骤内 critic 评审循环：输出经只读门禁评审，意见回流 worker 重跑。
/// 通过或轮数耗尽返回最终草稿；未通过返回 Err（视为步骤失败）。
async fn run_step_critic(
    worker: &Arc<dyn Worker>,
    input: &serde_json::Value,
    initial_output: &str,
    step: &StepSpec,
    rt: &StepRuntime,
    max_rounds: u32,
    attempts: &mut u32,
) -> Result<String, String> {
    let config = rt
        .critic
        .as_ref()
        .ok_or_else(|| "步骤声明 _critic 但 runner 未 attach critic".to_string())?;
    let context = serde_json::json!({
        "step": step.id,
        "worker": step.worker,
        "correlation_id": step.id,
    });
    let author = {
        let worker = Arc::clone(worker);
        let input = input.clone();
        let rt = rt.clone();
        move |_draft: String, feedback: Vec<String>| {
            let worker = Arc::clone(&worker);
            let input = inject_feedback(&input, &feedback);
            let rt = rt.clone();
            async move {
                if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("调度器已 abort".to_string());
                }
                worker.run(&input).await
            }
        }
    };
    let outcome = review_loop(config, &context, initial_output.to_string(), author).await?;
    *attempts = attempts.saturating_add(outcome.revisions);
    if outcome.approved {
        Ok(outcome.final_draft)
    } else {
        Err(format!(
            "critic 评审未通过（{max_rounds} 轮）：score={}",
            outcome.history.last().map(|r| r.verdict.score).unwrap_or(0)
        ))
    }
}

/// 把评审意见注入 input 的 `_critic_feedback` 键（worker 可读取并据此修订）。
fn inject_feedback(input: &serde_json::Value, feedback: &[String]) -> serde_json::Value {
    let mut cloned = input.clone();
    if let Some(obj) = cloned.as_object_mut() {
        obj.insert(
            "_critic_feedback".to_string(),
            serde_json::Value::Array(
                feedback
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    cloned
}

/// 步骤 input 预处理：`_bb.read` 声明的黑板键读取并替换 `{{bb:key}}` 占位符。
async fn prepare_step_input(
    input: &serde_json::Value,
    blackboard: &Option<Blackboard>,
) -> Result<serde_json::Value, String> {
    let Some(read_keys) = input
        .get("_bb")
        .and_then(|v| v.get("read"))
        .and_then(|v| v.as_array())
    else {
        return Ok(input.clone());
    };
    let bb = blackboard
        .as_ref()
        .ok_or_else(|| "步骤声明 _bb.read 但 runner 未 attach blackboard".to_string())?;
    let mut values: Vec<(String, String)> = Vec::new();
    for key in read_keys {
        let key = key
            .as_str()
            .ok_or_else(|| "步骤声明 _bb.read 必须为字符串键数组".to_string())?;
        let value = bb
            .read(key)
            .await
            .map_err(|e| format!("blackboard 读取失败：{e}"))?;
        let text = match value {
            serde_json::Value::String(s) => s,
            other => serde_json::to_string(&other).map_err(|e| format!("黑板值序列化失败：{e}"))?,
        };
        values.push((key.to_string(), text));
    }
    Ok(replace_tokens(input, &values))
}

/// 递归替换字符串叶子中的 `{{bb:key}}` 占位符。
fn replace_tokens(value: &serde_json::Value, values: &[(String, String)]) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let mut out = s.clone();
            for (key, text) in values {
                out = out.replace(&format!("{{{{bb:{key}}}}}"), text);
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|v| replace_tokens(v, values)).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), replace_tokens(v, values)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_status_machine_transitions() {
        let mut goal = Goal::new("g1", "测试目标");
        assert_eq!(goal.status, GoalStatus::Pending);
        goal.transition(GoalStatus::Planning);
        goal.transition(GoalStatus::Running);
        goal.transition(GoalStatus::Verifying);
        goal.transition(GoalStatus::Succeeded);
        assert!(goal.status.is_terminal());
        assert!(!GoalStatus::Running.is_terminal());
    }

    #[test]
    fn run_state_serde_roundtrip() {
        let plan = Plan::new("p1", "g1");
        let state = GoalRunState::new(Goal::new("g1", "目标"), plan);
        let json = serde_json::to_string(&state).unwrap();
        let restored: GoalRunState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.run_id, state.run_id);
        assert_eq!(restored.records.len(), 0);
    }
}
