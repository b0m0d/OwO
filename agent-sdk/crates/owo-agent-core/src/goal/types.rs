use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::capability::{CapabilityWorkerRegistry, WorkerRequirement};
use crate::execution_target::WorkerBinding;
use crate::plan::{Plan, StepStatus, VerificationPlanV1, VerificationSpec};
use crate::worker_pool::WorkerPool;
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
    /// 并行度上限（十一期 · 团队并行开发：同一 wave 内并发执行的步骤数；
    /// 缺省 4；旧持久化状态缺字段时按缺省反序列化）。
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
}

/// `GoalBudget.max_parallel` 缺省值（与 [`RunnerConfig::default`] 一致）。
pub fn default_max_parallel() -> u32 {
    4
}

impl Default for GoalBudget {
    fn default() -> Self {
        Self {
            max_steps: 200,
            max_retries_per_step: 2,
            max_total_retries: 10,
            max_replans: 2,
            max_duration_secs: 0,
            max_parallel: default_max_parallel(),
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
    /// Typed host verification contract; legacy `acceptance` compiles through the same registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_plan: Option<VerificationPlanV1>,
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
            verification_plan: None,
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
    /// Host-generated unique identity for the active attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Host-authenticated runtime skip disposition; ordinary success must not impersonate a skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// Host epoch that claimed the attempt; stale worker results cannot reuse it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_epoch: Option<u64>,
    /// Host receipts from the current and prior attempts; prior receipts are kept as Stale.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_receipts: Vec<crate::plan::ValidationReceiptV1>,
}

/// Host-owned status for a structured review finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryIssueStatusV1 {
    Open,
    RepairDispatched,
    Resolved,
}

/// Durable review issue bound to the exact reviewed task attempt and its repair closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryIssueV1 {
    pub issue_id: String,
    pub source_review_artifact_id: String,
    pub source_review_sha256: String,
    pub finding_sha256: String,
    pub severity: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement_id: Option<String>,
    pub target_task_id: String,
    pub target_attempt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_artifact_id: Option<String>,
    pub owner_step_id: String,
    pub status: DeliveryIssueStatusV1,
    pub repair_attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_review_artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_review_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_attempt_id: Option<String>,
    pub opened_at: String,
    pub updated_at: String,
}

/// 单步完成状态更新，供上层调度器在任务之间隙持久化。
#[derive(Debug, Clone)]
pub struct StepProgressUpdate {
    pub step_id: String,
    pub worker: String,
    pub record: StepRecord,
    pub steps_taken: u32,
    pub total_retries: u32,
    pub skip_reason: Option<String>,
}

/// 一次运行的完整状态（可整体持久化：<dir>/<run_id>.json）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalRunState {
    pub run_id: String,
    pub goal: Goal,
    pub plan: Plan,
    /// step_id → 执行记录。
    pub records: BTreeMap<String, StepRecord>,
    /// Host-produced goal-level receipts; legacy snapshots load with no receipts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validation_receipts: Vec<crate::plan::ValidationReceiptV1>,
    /// Durable review findings and their owner-repair/re-review closure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivery_issues: Vec<DeliveryIssueV1>,
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
                        attempt_id: None,
                        output: None,
                        error: None,
                        skip_reason: None,
                        phase_epoch: None,
                        validation_receipts: Vec::new(),
                    },
                )
            })
            .collect();
        Self {
            run_id: format!("run-{}", chrono::Utc::now().timestamp_millis()),
            goal,
            plan,
            records,
            validation_receipts: Vec::new(),
            delivery_issues: Vec::new(),
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

/// 取消/早退时的有界清理窗口（十期 · 四路 R2）：置位 abort 标志后**不直接丢弃
/// 在飞 worker Future**——先给协作式 worker（`TrackedRoleWorker` 等）一个回合边界，
/// 让其完成后快照、变更登记等收尾；超过本窗口仍未退出的任务才被强制终止
/// （进程树由沙箱 Job kill-on-close 兜底）。
pub const CANCELLATION_CLEANUP_GRACE: Duration = Duration::from_secs(30);

/// 阶段级取消的整体清理上限（十期 · 四路 R2）：run_phase 收到 cancel 后等待
/// run 在所有在飞步骤的协作清理下自然退出；整体超时才强制丢弃 run Future。
/// 取值为单步清理窗口宽松倍数（并行步骤可同时清理，故无需按步累加）。
pub const PHASE_CANCELLATION_CLEANUP_GRACE: Duration = Duration::from_secs(90);

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
