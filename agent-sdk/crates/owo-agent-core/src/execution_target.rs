//! A2 统一调度适配层（冻结接口）。
//!
//! 目标（§15 A2）：让一个 Goal 的步骤**显式选择**执行目标——进程内
//! （`in_process`）、本地子进程（`local_process`）、远端节点（`fleet_node`），
//! 且三类目标共用同一执行/取消/预算语义。本模块是 goal.rs 与底层执行通道
//! 之间的隔离层：
//!
//! - HTTP、节点注册、远程控制面细节**不出现在本模块与 goal.rs**
//!   （远端仅经 [`crate::fleet_transport::FleetTransport`] 抽象）。
//! - [`Goal`](crate::goal::GoalRunner) 仍是唯一状态来源；本模块只产出
//!   「派发裁定」与「已装配的通道 worker」，不触碰运行状态。
//! - 安全语义：显式目标不可用时返回 **等待 / 询问 / 拒绝**，
//!   绝不允许静默切换到权限更高的目标（例如本地目标缺失时不转远端，
//!   远端不可达时不回退到高权限进程内 worker）。
//!
//! 冻结类型：
//! - [`ExecutionTarget`]：InProcess / LocalProcess / FleetNode{node_id}。
//! - [`WorkerBinding`]：worker 名称、target、capabilities、permission scope、
//!   budget、input CAS ref、correlation ID。
//! - [`DispatchDisposition`]：Ready / Wait / AskUser / Reject。

use crate::fleet_transport::{FleetTransport, TransportEventKind, TransportStatus, TransportTask};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 执行目标种类常量（HTTP/DSL 字符串形式；由 [`ExecutionTarget::parse`] 解析）。
pub const TARGET_IN_PROCESS: &str = "in_process";
pub const TARGET_LOCAL_PROCESS: &str = "local_process";
pub const TARGET_FLEET_NODE: &str = "fleet_node";

/// 显式执行目标（冻结）。
///
/// - `InProcess`：只能查 [`crate::goal::WorkerRegistry`]。
/// - `LocalProcess`：只能进入 [`crate::worker_pool::WorkerPool`]。
/// - `FleetNode`：只能进入 [`crate::fleet_transport::FleetTransport`]，
///   且必须携带明确 `node_id`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum ExecutionTarget {
    /// 进程内执行（registry 命中才可用）。
    InProcess,
    /// 本地子进程执行（WorkerPool 命中才可用）。
    LocalProcess,
    /// 远端节点执行（transport 已配置且携带明确 node_id 才可用）。
    FleetNode {
        /// 明确的目标节点 ID（空字符串视为非法）。
        node_id: String,
    },
}

impl ExecutionTarget {
    /// 种类字符串（`in_process` / `local_process` / `fleet_node`）。
    pub fn kind(&self) -> &'static str {
        match self {
            ExecutionTarget::InProcess => TARGET_IN_PROCESS,
            ExecutionTarget::LocalProcess => TARGET_LOCAL_PROCESS,
            ExecutionTarget::FleetNode { .. } => TARGET_FLEET_NODE,
        }
    }

    /// 远端节点的 node_id（非 fleet_node 返回 None）。
    pub fn node_id(&self) -> Option<&str> {
        match self {
            ExecutionTarget::FleetNode { node_id } => Some(node_id.as_str()),
            _ => None,
        }
    }

    /// 从字符串形式解析（服务端映射辅助）：
    /// `in_process` / `local_process` 直接返回；`fleet_node` 必须提供非空
    /// `node_id`。未知种类返回错误（调用方应映射为明确的 400/422）。
    pub fn parse(kind: &str, node_id: Option<&str>) -> Result<Self, String> {
        match kind.trim() {
            TARGET_IN_PROCESS => Ok(ExecutionTarget::InProcess),
            TARGET_LOCAL_PROCESS => Ok(ExecutionTarget::LocalProcess),
            TARGET_FLEET_NODE => {
                let id = node_id
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "fleet_node 必须提供明确的 node_id".to_string())?;
                Ok(ExecutionTarget::FleetNode {
                    node_id: id.to_string(),
                })
            }
            other => Err(format!(
                "未知执行目标「{other}」（支持 {TARGET_IN_PROCESS}/{TARGET_LOCAL_PROCESS}/{TARGET_FLEET_NODE}）"
            )),
        }
    }
}

/// 绑定级权限范围（默认 deny：未列出的能力一律不授予；deny 优先于 allow）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionScope {
    /// 允许的工具/能力名。
    #[serde(default)]
    pub allow: Vec<String>,
    /// 显式拒绝（优先于 allow）。
    #[serde(default)]
    pub deny: Vec<String>,
    /// 是否允许网络出网（默认 false）。
    #[serde(default)]
    pub network_egress: bool,
}

impl PermissionScope {
    /// 判定某工具/能力是否被授予（deny 优先；默认 deny）。
    pub fn permits(&self, tool: &str) -> bool {
        if self.deny.iter().any(|d| d == tool) {
            return false;
        }
        self.allow.iter().any(|a| a == tool)
    }

    /// 是否申请了超出默认空授权的权限（供上层决定审批门控；
    /// 本层不做策略判定，只暴露事实）。
    pub fn requests_elevation(&self) -> bool {
        self.network_egress || !self.allow.is_empty()
    }
}

/// 绑定级预算（步骤执行上限；0 = 不限，沿用调度器全局预算）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingBudget {
    /// 该绑定允许的最大尝试次数上限（对 plan 声明的 retries 取 min）。
    #[serde(default)]
    pub max_attempts: u32,
    /// 该绑定允许的最大执行时长（秒，0 = 不限）。
    #[serde(default)]
    pub max_duration_secs: u64,
}

/// 显式执行绑定（冻结字段，只增不改语义）：
/// worker 名称、target、capabilities、permission scope、budget、input CAS ref、correlation ID。
///
/// 匹配规则见 [`select_binding`]：step id 精确匹配优先于 worker 名匹配。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerBinding {
    /// 匹配键：可为 plan 步骤声明的 worker 名，或直接为 step id（优先级更高）。
    pub worker: String,
    /// 显式执行目标。
    pub target: ExecutionTarget,
    /// 声明的能力标签（审计/路由提示；定向派发不做能力匹配决策）。
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 权限范围（默认 deny；映射到通道层的审批/白名单事实）。
    #[serde(default)]
    pub permission_scope: PermissionScope,
    /// 绑定级预算。
    #[serde(default)]
    pub budget: BindingBudget,
    /// 输入内容寻址引用（`cas_store` 哈希；None = 未声明）。
    #[serde(default)]
    pub input_cas_ref: Option<String>,
    /// 关联 ID（审计/血缘贯通；None 时派发期取 `{run_id}:{step_id}` 兜底）。
    #[serde(default)]
    pub correlation_id: Option<String>,
}

impl WorkerBinding {
    /// 最小构造（其余字段取默认）。
    pub fn new(worker: impl Into<String>, target: ExecutionTarget) -> Self {
        Self {
            worker: worker.into(),
            target,
            capabilities: Vec::new(),
            permission_scope: PermissionScope::default(),
            budget: BindingBudget::default(),
            input_cas_ref: None,
            correlation_id: None,
        }
    }

    /// 有效关联 ID：显式声明优先，否则使用调用方给定的兜底值。
    pub fn effective_correlation_id(&self, fallback: &str) -> String {
        self.correlation_id
            .clone()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// 尝试次数上限：绑定限制 > 0 时对基础值取 min。
    pub fn cap_attempts(&self, base: u32) -> u32 {
        if self.budget.max_attempts > 0 {
            base.min(self.budget.max_attempts)
        } else {
            base
        }
    }

    /// 把派发上下文注入步骤输入的 `_dispatch` 键（与 `_cap`/`_critic`/`_bb`
    /// 同族的 `_` 前缀观测约定），三类目标同构传递：
    /// correlation ID、输入 CAS 引用、node_id 对下游全部可见。
    pub fn inject_dispatch_context(&self, input: &mut serde_json::Value, correlation_id: &str) {
        if !input.is_object() {
            *input = serde_json::json!({});
        }
        let Some(obj) = input.as_object_mut() else {
            return;
        };
        let mut ctx = serde_json::Map::new();
        ctx.insert(
            "target".to_string(),
            serde_json::Value::String(self.target.kind().to_string()),
        );
        if let Some(node_id) = self.target.node_id() {
            ctx.insert(
                "node_id".to_string(),
                serde_json::Value::String(node_id.to_string()),
            );
        }
        ctx.insert(
            "capabilities".to_string(),
            serde_json::Value::Array(
                self.capabilities
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        ctx.insert(
            "permission_scope".to_string(),
            serde_json::json!({
                "network_egress": self.permission_scope.network_egress,
                "allow_count": self.permission_scope.allow.len(),
            }),
        );
        if let Some(cas_ref) = &self.input_cas_ref {
            ctx.insert(
                "input_cas_ref".to_string(),
                serde_json::Value::String(cas_ref.clone()),
            );
        }
        ctx.insert(
            "correlation_id".to_string(),
            serde_json::Value::String(correlation_id.to_string()),
        );
        obj.insert("_dispatch".to_string(), serde_json::Value::Object(ctx));
    }
}

// ---------- 派发裁定 ----------

/// 已装配的派发通道（与 `ExecutionTarget` 一一对应；GoalRunner 据此构造执行器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchChannel {
    /// 进程内 WorkerRegistry 的 worker 名。
    Registry(String),
    /// 本地 WorkerPool 的 worker id。
    Pool(String),
    /// 远端节点：明确 node_id + CapabilityCard 注册名。
    Fleet { node_id: String, worker: String },
}

impl DispatchChannel {
    pub fn kind(&self) -> &'static str {
        match self {
            DispatchChannel::Registry(_) => TARGET_IN_PROCESS,
            DispatchChannel::Pool(_) => TARGET_LOCAL_PROCESS,
            DispatchChannel::Fleet { .. } => TARGET_FLEET_NODE,
        }
    }
}

/// Ready 裁定携带的派发描述（含生效绑定快照，供审计与观测）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDispatch {
    pub channel: DispatchChannel,
    pub binding: WorkerBinding,
}

/// 派发裁定（冻结）。绑定命中后按目标探测结果得出：
/// - 配置矛盾 / 目标未启用 → [`DispatchDisposition::Reject`]（重试无意义）；
/// - 目标已启用但实例暂缺（如池内 worker 重启中）→ [`DispatchDisposition::Wait`]；
/// - 需要人决定的不可用状态（如节点探测明确离线）→ [`DispatchDisposition::AskUser`]；
///
/// 其余 → [`DispatchDisposition::Ready`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchDisposition {
    Ready(Box<ResolvedDispatch>),
    Wait { reason: String },
    AskUser { prompt: String },
    Reject { reason: String },
}

impl DispatchDisposition {
    /// 是否为可执行就绪。
    pub fn is_ready(&self) -> bool {
        matches!(self, DispatchDisposition::Ready(_))
    }
}

/// 三类目标的可用性探测结果（由 GoalRunner 用本地注册表/池配置填充；
/// 不含任何远程接口细节）。远端节点当前无独立可达性视图（预留
/// `NodeUnreachable` 给未来的探测器；未知一律交由提交路径失败暴露，
/// 不伪造「已验证可达」）。
#[derive(Debug, Clone, Default)]
pub struct TargetAvailability {
    /// WorkerRegistry 命中该名字。
    pub in_process_ready: bool,
    pub local_process: LocalProcessProbe,
    pub fleet: FleetProbe,
}

/// 本地子进程池探测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalProcessProbe {
    /// use_worker_pool 未启用或未配置 WorkerPool（配置矛盾 → Reject）。
    #[default]
    NotConfigured,
    /// 池已启用但暂无该 worker（可能崩溃重启中 → Wait）。
    WorkerMissing,
    /// 池已启用且包含该 worker。
    Ready,
}

/// 远端传输探测。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FleetProbe {
    /// 未配置控制面传输（→ Reject，绝不回退到本地高权限目标）。
    #[default]
    NotConfigured,
    /// 探测器明确报告目标节点离线（→ AskUser，交人决定是否等待恢复）。
    NodeUnreachable,
    /// 传输已配置且无可否定的反证（提交后失败即失败，不静默改派）。
    Ready,
}

/// 核心 pure 函数：给定绑定与可用性探测，得出派发裁定。
///
/// 这是「安全语义」的裁决点：任何不可用路径都不产生指向其他目标的 Ready。
pub fn dispatch_disposition(
    binding: &WorkerBinding,
    availability: &TargetAvailability,
) -> DispatchDisposition {
    match &binding.target {
        ExecutionTarget::InProcess => {
            if availability.in_process_ready {
                DispatchDisposition::Ready(Box::new(ResolvedDispatch {
                    channel: DispatchChannel::Registry(binding.worker.clone()),
                    binding: binding.clone(),
                }))
            } else {
                DispatchDisposition::Reject {
                    reason: format!(
                        "显式目标 in_process 下 worker「{}」未在 WorkerRegistry 注册；禁止回退到本地子进程或远端节点",
                        binding.worker
                    ),
                }
            }
        }
        ExecutionTarget::LocalProcess => match availability.local_process {
            LocalProcessProbe::Ready => {
                DispatchDisposition::Ready(Box::new(ResolvedDispatch {
                    channel: DispatchChannel::Pool(binding.worker.clone()),
                    binding: binding.clone(),
                }))
            }
            LocalProcessProbe::WorkerMissing => DispatchDisposition::Wait {
                reason: format!(
                    "显式目标 local_process：本地池暂无 worker「{}」（可能崩溃重启中）；不改派其他目标",
                    binding.worker
                ),
            },
            LocalProcessProbe::NotConfigured => DispatchDisposition::Reject {
                reason: format!(
                    "显式目标 local_process 但 use_worker_pool 未启用或未配置 WorkerPool（worker「{}」）；禁止切换到进程内或远端节点",
                    binding.worker
                ),
            },
        },
        ExecutionTarget::FleetNode { node_id } => {
            if node_id.trim().is_empty() {
                return DispatchDisposition::Reject {
                    reason: "fleet_node 必须携带明确的 node_id".to_string(),
                };
            }
            match availability.fleet {
                FleetProbe::NotConfigured => DispatchDisposition::Reject {
                    reason: format!(
                        "显式目标 fleet_node（节点 {node_id}）但未配置控制面传输；禁止回退到本地高权限 worker"
                    ),
                },
                FleetProbe::NodeUnreachable => DispatchDisposition::AskUser {
                    prompt: format!(
                        "远端节点 {node_id} 当前不可达；是否继续等待该节点恢复？调度器不会自动改派"
                    ),
                },
                FleetProbe::Ready => {
                    DispatchDisposition::Ready(Box::new(ResolvedDispatch {
                        channel: DispatchChannel::Fleet {
                            node_id: node_id.clone(),
                            worker: binding.worker.clone(),
                        },
                        binding: binding.clone(),
                    }))
                }
            }
        }
    }
}

/// 从候选绑定集合选择显式绑定：step id 精确匹配优先，其次按步骤声明的
/// worker 名匹配；同类内部保持声明顺序（先声明者优先），保证确定性。
pub fn select_binding<'a>(
    bindings: &'a [WorkerBinding],
    step_id: &str,
    worker_name: &str,
) -> Option<&'a WorkerBinding> {
    bindings
        .iter()
        .find(|b| b.worker == step_id)
        .or_else(|| bindings.iter().find(|b| b.worker == worker_name))
}

// ---------- 在飞派发取消登记表 ----------

/// 在飞派发任务的取消登记表：abort 时把未完成的 transport 任务显式 cancel，
/// 防「abort 后残留 pending 任务」。进程内/池通道的取消沿用既有机制
/// （JoinSet.abort / pool.cancel_all），本表只覆盖远端通道。
#[derive(Clone, Default)]
pub struct DispatchCancelRegistry(Arc<Mutex<HashMap<String, Arc<dyn FleetTransport>>>>);

impl std::fmt::Debug for DispatchCancelRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.lock().map(|m| m.keys().cloned().collect::<Vec<_>>());
        match inner {
            Ok(keys) => f
                .debug_struct("DispatchCancelRegistry")
                .field("inflight", &keys.len())
                .field("task_ids", &keys)
                .finish(),
            Err(_) => f
                .debug_struct("DispatchCancelRegistry")
                .finish_non_exhaustive(),
        }
    }
}

impl DispatchCancelRegistry {
    /// 登记一个在飞任务（重复 id 以最后一次为准）。
    pub fn register(&self, transport: Arc<dyn FleetTransport>, task_id: &str) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(task_id.to_string(), transport);
        }
    }

    /// 任务结束（成功/失败/超时）后移出登记表。
    pub fn unregister(&self, task_id: &str) {
        if let Ok(mut map) = self.0.lock() {
            map.remove(task_id);
        }
    }

    /// 当前在飞任务数（观测/断言用）。
    pub fn pending_count(&self) -> usize {
        self.0.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// 取消并清空全部在飞任务（abort 收尾路径调用；逐个 await 完成后才返回）。
    pub async fn cancel_all(&self) {
        let drained: Vec<(String, Arc<dyn FleetTransport>)> = match self.0.lock() {
            Ok(mut map) => map.drain().collect(),
            Err(_) => return,
        };
        // 锁已释放；逐个发送取消（失败仅记录，不阻断收尾）。
        for (task_id, transport) in drained {
            if let Err(e) = transport.cancel(&task_id).await {
                tracing::warn!(task_id = %task_id, "abort 取消在飞 fleet 任务失败：{e}");
            }
        }
    }
}

// ---------- 远端派发 worker ----------

/// 远端派发 worker 默认等待终态超时（超时先 cancel 再报错，防孤儿任务挂起）。
pub const DEFAULT_FLEET_DISPATCH_TIMEOUT: Duration = Duration::from_secs(60);

/// 把 fleet 通道包装为 `goal::Worker`：提交带完整关联信息的
/// [`TransportTask`]（correlation_id / lineage=CAS 引用），轮询至终态。
///
/// 与 [`crate::fleet_transport::TransportWorker`] 的区别：本 worker 由
/// 显式绑定驱动——correlation_id 来自绑定（而非随机生成）、输入 CAS 引用
/// 写入 lineage、超时可由绑定预算派生，并在 abort 收尾时经
/// [`DispatchCancelRegistry`] 接受统一取消。
#[derive(Clone)]
pub struct FleetDispatchWorker {
    transport: Arc<dyn FleetTransport>,
    /// CapabilityCard 注册名（TransportTask.worker）。
    worker_name: String,
    node_id: String,
    correlation_id: String,
    input_cas_ref: Option<String>,
    timeout: Option<Duration>,
    cancels: DispatchCancelRegistry,
    approval_required: bool,
}

impl std::fmt::Debug for FleetDispatchWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetDispatchWorker")
            .field("transport", &self.transport.name())
            .field("worker", &self.worker_name)
            .field("node_id", &self.node_id)
            .field("correlation_id", &self.correlation_id)
            .field("input_cas_ref", &self.input_cas_ref)
            .field("timeout", &self.timeout)
            .field("approval_required", &self.approval_required)
            .finish()
    }
}

impl FleetDispatchWorker {
    /// 从生效绑定装配（timeout：绑定预算时长 > 0 时取之，否则用默认值）。
    pub fn from_binding(
        resolved_channel_worker: impl Into<String>,
        node_id: impl Into<String>,
        binding: &WorkerBinding,
        correlation_id: impl Into<String>,
        transport: Arc<dyn FleetTransport>,
        cancels: DispatchCancelRegistry,
    ) -> Self {
        let timeout = if binding.budget.max_duration_secs > 0 {
            Some(Duration::from_secs(binding.budget.max_duration_secs))
        } else {
            Some(DEFAULT_FLEET_DISPATCH_TIMEOUT)
        };
        Self {
            transport,
            worker_name: resolved_channel_worker.into(),
            node_id: node_id.into(),
            correlation_id: correlation_id.into(),
            input_cas_ref: binding.input_cas_ref.clone(),
            timeout,
            cancels,
            approval_required: false,
        }
    }

    /// 设置是否需要审批（上层审批流自行决定；默认不需要）。
    pub fn with_approval_required(mut self, approval_required: bool) -> Self {
        self.approval_required = approval_required;
        self
    }

    /// 自定义等待超时（None = 不超时）。
    pub fn with_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl crate::goal::Worker for FleetDispatchWorker {
    fn name(&self) -> &str {
        &self.worker_name
    }

    async fn run(&self, input: &serde_json::Value) -> Result<String, String> {
        let task_id = format!("t-{}", uuid::Uuid::new_v4());
        let mut task = TransportTask::new(
            task_id.clone(),
            self.worker_name.clone(),
            self.correlation_id.clone(),
            input.clone(),
        );
        if let Some(cas_ref) = &self.input_cas_ref {
            task.lineage.push(format!("cas:{cas_ref}"));
        }
        task.lineage.push(format!("node:{}", self.node_id));
        task.approval_required = self.approval_required;

        self.cancels.register(Arc::clone(&self.transport), &task_id);

        macro_rules! finish_err {
            ($err:expr) => {{
                self.cancels.unregister(&task_id);
                return Err($err);
            }};
        }

        if let Err(e) = self.transport.submit(task).await {
            finish_err!(format!("fleet 任务 {task_id} 提交失败：{e}"));
        }
        let deadline = self.timeout.map(|d| tokio::time::Instant::now() + d);
        loop {
            if let Some(deadline) = deadline {
                if tokio::time::Instant::now() >= deadline {
                    let _ = self.transport.cancel(&task_id).await;
                    finish_err!(format!("fleet 任务 {task_id} 等待超时"));
                }
            }
            let status = match self.transport.status(&task_id).await {
                Ok(status) => status,
                Err(e) => finish_err!(format!("fleet 任务 {task_id} 状态查询失败：{e}")),
            };
            match status {
                TransportStatus::Succeeded => {
                    self.cancels.unregister(&task_id);
                    let events = self.transport.events(&task_id).await?;
                    // Result payload 形状：控制面 report_result 写入对象
                    // {ok,output,output_cas,evidence,error}，其中 output 为任意
                    // JSON 值——字符串按原文提取（与本地/池通道一致），其余标量
                    // 以其 JSON 文本兜底；历史纯字符串 payload 保持兼容。
                    // （第四路应急接线修复 2026-08-27：原先仅 payload.as_str()，
                    //   对象恒取空导致 fleet_node 成功输出与本地目标语义不一致；
                    //   如第三路有更优实现请直接覆盖本段。）
                    let output = events
                        .iter()
                        .find(|e| e.kind == TransportEventKind::Result)
                        .and_then(|e| {
                            e.payload
                                .get("output")
                                .and_then(|v| {
                                    if v.is_null() {
                                        None
                                    } else {
                                        v.as_str().map(str::to_string).or(Some(v.to_string()))
                                    }
                                })
                                .or_else(|| e.payload.as_str().map(str::to_string))
                        })
                        .unwrap_or_default();
                    return Ok(output);
                }
                TransportStatus::Failed | TransportStatus::Cancelled => {
                    finish_err!(format!("fleet 任务 {task_id} 失败/取消"))
                }
                TransportStatus::AwaitingApproval => {
                    finish_err!(format!("fleet 任务 {task_id} 等待审批"))
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }
}

// ---------- 单元测试（冻结接口自身契约） ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_target_serde_roundtrip_with_internal_tag() {
        let in_process = ExecutionTarget::InProcess;
        let json = serde_json::to_value(&in_process).unwrap();
        assert_eq!(json, serde_json::json!({ "target": "in_process" }));
        let restored: ExecutionTarget = serde_json::from_value(json).unwrap();
        assert_eq!(restored, in_process);

        let fleet = ExecutionTarget::FleetNode {
            node_id: "n1".to_string(),
        };
        let json = serde_json::to_value(&fleet).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "target": "fleet_node", "node_id": "n1" })
        );
        assert_eq!(fleet.kind(), TARGET_FLEET_NODE);
        assert_eq!(fleet.node_id(), Some("n1"));
        let restored: ExecutionTarget = serde_json::from_value(json).unwrap();
        assert_eq!(restored, fleet);
    }

    #[test]
    fn execution_target_parse_validates_node_id_and_kind() {
        assert_eq!(
            ExecutionTarget::parse(TARGET_IN_PROCESS, None).unwrap(),
            ExecutionTarget::InProcess
        );
        assert_eq!(
            ExecutionTarget::parse(TARGET_LOCAL_PROCESS, None).unwrap(),
            ExecutionTarget::LocalProcess
        );
        let fleet = ExecutionTarget::parse(TARGET_FLEET_NODE, Some(" node-a ")).unwrap();
        assert_eq!(
            fleet,
            ExecutionTarget::FleetNode {
                node_id: "node-a".to_string()
            }
        );
        // 缺失/空白 node_id → 错误；未知 kind → 错误。
        assert!(ExecutionTarget::parse(TARGET_FLEET_NODE, None).is_err());
        assert!(ExecutionTarget::parse(TARGET_FLEET_NODE, Some("  ")).is_err());
        assert!(ExecutionTarget::parse("remote_machine", None).is_err());
    }

    #[test]
    fn select_binding_prefers_step_id_over_worker_name() {
        let by_step = WorkerBinding::new("s-1", ExecutionTarget::LocalProcess);
        let by_worker = WorkerBinding::new("echo", ExecutionTarget::InProcess);
        let bindings = vec![by_worker.clone(), by_step];
        // step id 精确匹配优先于 worker 名。
        let picked = select_binding(&bindings, "s-1", "echo").unwrap();
        assert_eq!(picked.target, ExecutionTarget::LocalProcess);
        // 无 step id 命中时退回 worker 名。
        let picked = select_binding(&bindings, "other-step", "echo").unwrap();
        assert_eq!(picked.target, ExecutionTarget::InProcess);
        // 都没有 → None。
        assert!(select_binding(&bindings, "x", "y").is_none());
    }

    #[test]
    fn disposition_matrix_rejects_without_cross_target_fallback() {
        // in_process 可用 → Registry Ready。
        let b = WorkerBinding::new("echo", ExecutionTarget::InProcess);
        let avail_in_ok = TargetAvailability {
            in_process_ready: true,
            ..Default::default()
        };
        match dispatch_disposition(&b, &avail_in_ok) {
            DispatchDisposition::Ready(r) => {
                assert_eq!(r.channel, DispatchChannel::Registry("echo".into()))
            }
            other => panic!("应为 Ready(Registry)：{other:?}"),
        }
        // in_process 不可用 → Reject（即使本地池/远端配置齐全也不改派）。
        let avail_everything_else = TargetAvailability {
            in_process_ready: false,
            local_process: LocalProcessProbe::Ready,
            fleet: FleetProbe::Ready,
        };
        match dispatch_disposition(&b, &avail_everything_else) {
            DispatchDisposition::Reject { reason } => {
                assert!(reason.contains("禁止回退"), "{reason}")
            }
            other => panic!("in_process 缺失必须 Reject：{other:?}"),
        }

        // local_process 未配置 → Reject。
        let b_local = WorkerBinding::new("echo", ExecutionTarget::LocalProcess);
        match dispatch_disposition(
            &b_local,
            &TargetAvailability {
                in_process_ready: true,
                ..Default::default()
            },
        ) {
            DispatchDisposition::Reject { reason } => {
                assert!(reason.contains("local_process"), "{reason}")
            }
            other => panic!("local 未配置必须 Reject：{other:?}"),
        }
        // local_process 池内暂缺 → Wait。
        let avail_missing = TargetAvailability {
            local_process: LocalProcessProbe::WorkerMissing,
            ..Default::default()
        };
        match dispatch_disposition(&b_local, &avail_missing) {
            DispatchDisposition::Wait { reason } => {
                assert!(reason.contains("不改派"), "{reason}")
            }
            other => panic!("池内暂缺必须 Wait：{other:?}"),
        }

        // fleet_node 未配置传输 → Reject；节点明确离线 → AskUser；齐全 → Ready(Fleet)。
        let b_fleet = WorkerBinding::new(
            "cap-worker",
            ExecutionTarget::FleetNode {
                node_id: "n1".to_string(),
            },
        );
        match dispatch_disposition(&b_fleet, &TargetAvailability::default()) {
            DispatchDisposition::Reject { reason } => {
                assert!(reason.contains("禁止回退到本地高权限"), "{reason}")
            }
            other => panic!("无传输必须 Reject：{other:?}"),
        }
        match dispatch_disposition(
            &b_fleet,
            &TargetAvailability {
                fleet: FleetProbe::NodeUnreachable,
                ..Default::default()
            },
        ) {
            DispatchDisposition::AskUser { prompt } => {
                assert!(
                    prompt.contains("n1") && prompt.contains("不会自动改派"),
                    "{prompt}"
                )
            }
            other => panic!("节点离线必须 AskUser：{other:?}"),
        }
        match dispatch_disposition(
            &b_fleet,
            &TargetAvailability {
                fleet: FleetProbe::Ready,
                ..Default::default()
            },
        ) {
            DispatchDisposition::Ready(r) => assert_eq!(
                r.channel,
                DispatchChannel::Fleet {
                    node_id: "n1".into(),
                    worker: "cap-worker".into()
                }
            ),
            other => panic!("齐备必须 Ready(Fleet)：{other:?}"),
        }
    }

    #[test]
    fn permission_scope_default_deny_and_deny_wins() {
        let scope = PermissionScope::default();
        assert!(!scope.permits("shell"), "默认 deny");
        let granted = PermissionScope {
            allow: vec!["shell".into()],
            deny: vec!["format_disk".into()],
            network_egress: true,
        };
        assert!(granted.permits("shell"));
        assert!(!granted.permits("format_disk"));
        assert!(!granted.permits("browser"), "未列出的一律 deny");
        assert!(granted.requests_elevation());
        assert!(!PermissionScope::default().requests_elevation());
    }

    #[test]
    fn binding_budget_correlation_and_context_helpers() {
        let mut b = WorkerBinding::new("echo", ExecutionTarget::LocalProcess);
        assert_eq!(b.effective_correlation_id("r1:s1"), "r1:s1");
        b.correlation_id = Some("corr-explicit".into());
        assert_eq!(b.effective_correlation_id("r1:s1"), "corr-explicit");

        assert_eq!(b.cap_attempts(5), 5, "未限 attempts 不封顶");
        b.budget.max_attempts = 2;
        assert_eq!(b.cap_attempts(5), 2, "绑定 attempts 封顶取 min");
        assert_eq!(b.cap_attempts(1), 1);

        let mut input = serde_json::json!({ "text": "T" });
        b.input_cas_ref = Some("sha256:abc".into());
        b.inject_dispatch_context(&mut input, "corr-x");
        let d = input.get("_dispatch").unwrap();
        assert_eq!(d["target"], TARGET_LOCAL_PROCESS);
        assert_eq!(d["correlation_id"], "corr-x");
        assert_eq!(d["input_cas_ref"], "sha256:abc");
        // 非 object 输入也能注入。
        let mut scalar = serde_json::Value::Null;
        b.inject_dispatch_context(&mut scalar, "c2");
        assert_eq!(scalar["_dispatch"]["correlation_id"], "c2");
    }

    #[test]
    fn cancel_registry_tracks_and_cancels_pending_tasks() {
        let reg = DispatchCancelRegistry::default();
        let transport = Arc::new(crate::fleet_transport::InMemoryTransport::new());
        assert_eq!(reg.pending_count(), 0);
        reg.register(Arc::clone(&transport) as Arc<dyn FleetTransport>, "t-1");
        reg.register(Arc::clone(&transport) as Arc<dyn FleetTransport>, "t-2");
        assert_eq!(reg.pending_count(), 2);
        reg.unregister("t-1");
        assert_eq!(reg.pending_count(), 1);
        // cancel_all 清空登记表（InMemoryTransport 无 t-2 任务本身不报错）。
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(reg.cancel_all());
        assert_eq!(reg.pending_count(), 0);
    }
}
