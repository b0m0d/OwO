// R13:fleet_api 第二阶段（真实远端节点协议闭环），待主控同步 OpenAPI
//! 控制面 HTTP 契约（P2 双节点网格）：节点注册/心跳续租、任务提交/查询/取消/SSE、
//! 审批响应，以及**真实远端节点协议**（领取/进度/证据/结果/取消确认/fencing）。
//!
//! 路由（前缀 /fleet，已在 `lib.rs::build_router` 挂载 `fleet_api::router(state)`）：
//! - `POST /fleet/nodes/register`           节点注册（CapabilityCard + 心跳续租，返回 lease_token/epoch）
//! - `GET  /fleet/nodes`                    节点列表（NodeStatus 快照）
//! - `POST /fleet/nodes/{id}/heartbeat`     节点心跳续租（旧 token 被拒；R13 新增）
//! - `GET  /fleet/nodes/{id}/tasks`         节点可领取/已领取任务（按自身 node_id 匹配；R13 新增）
//! - `POST /fleet/tasks/submit`             任务提交（`Idempotency-Key` 头幂等）
//! - `GET  /fleet/tasks/{id}`               任务状态 + 事件
//! - `POST /fleet/tasks/{id}/claim`         节点领取匹配任务（fencing 校验；R13 新增）
//! - `POST /fleet/tasks/{id}/progress`      节点回传进度 + 结构化证据（R13 新增）
//! - `POST /fleet/tasks/{id}/result`        节点回传成功/失败结果（R13 新增）
//! - `POST /fleet/tasks/{id}/cancel-ack`    节点确认取消（R13 新增）
//! - `POST /fleet/tasks/{id}/cancel`        取消任务
//! - `GET  /fleet/tasks/{id}/events`        SSE（历史重放 + 实时；`?format=json` 拉全量）
//! - `POST /fleet/approvals/{id}/respond`   审批响应（影响预览 + 结构化证据齐备才批准）
//!
//! 运行态：模块内 `OnceLock` 单例 [`FleetHub`]（进程内 [`InMemoryTransport`] 承载任务状态、
//! [`LeaseManager`] 节点租约/fencing、[`AgentBus`]+[`BusStore`] 节点/任务/违规审计持久化、
//! [`CasStore`] 产物、[`ExperienceStore`] 节点状态变迁、`claims` 领取所有权登记）。
//! **R13 起控制面不再在本进程中"伪执行完成"任务**：任务提交后只进入 Running（等待领取），
//! 状态推进仅由显式节点领取（claim）+ 结果回传（result）驱动。
//!
//! 协议约束：本模块不引用 `crate::`/`super::`；`AppState` 全限定名 `owo_agent_server::AppState`；
//! 错误统一 `(StatusCode, Json({error}))`；不给 AppState 加字段（状态在模块内）。
//! 安全：所有节点写操作经 [`LeaseManager::verify_write`] fencing（token + epoch）校验；
//! 越权/过期 epoch/不匹配节点被拒绝并留审计（bus_store + experience）。协议不携带模型凭据。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::bus_store::BusStore;
use owo_agent_core::capability::{CapabilityCard, CapabilityWorkerRegistry};
use owo_agent_core::cas_store::CasStore;
use owo_agent_core::experience_store::{Attribution, ExperienceStore, Outcome};
use owo_agent_core::fleet::{AgentBus, BusMessage, MessageKind, CONTROL_PLANE_AGENT};
use owo_agent_core::fleet_node_protocol::{
    violation_correlation_id, NodeCancelAckBody, NodeClaimBody, NodeHeartbeatBody,
    NodeHeartbeatResponse, NodeProgressBody, NodeProtocolViolation, NodeResultBody,
};
use owo_agent_core::fleet_transport::{
    FleetTransport, InMemoryTransport, TransportEvent, TransportEventKind, TransportStatus,
    TransportTask,
};
use owo_agent_core::lease::{LeaseConfig, LeaseManager};
use owo_agent_core::node_agent::{NodeAgent, NodeStatus};
use owo_agent_core::remote_step::EvidenceItem;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn api_err(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

// ---------- 节点协议校验与违规审计（R13） ----------

/// 协议违规审计：总线落盘 + 经验记录（幂等键 = node:protocol:violation:<node>:<task>）。
fn audit_violation(hub: &FleetHub, node_id: &str, task_id: &str, reason: &str) {
    let violation = NodeProtocolViolation::new(node_id, task_id, reason);
    let correlation_id = violation_correlation_id(node_id, task_id);
    let msg = BusMessage {
        id: 0,
        from: CONTROL_PLANE_AGENT.to_string(),
        to: CONTROL_PLANE_AGENT.to_string(),
        kind: MessageKind::Refusal,
        correlation_id: correlation_id.clone(),
        payload: serde_json::to_value(&violation).unwrap_or_default(),
    };
    let _ = hub.bus_store.persist(&msg);
    let _ = hub.experience.record_worker_outcome(
        correlation_id,
        node_id.to_string(),
        Outcome::Failure,
        Attribution {
            goal_id: None,
            plan_id: None,
            step_id: Some(task_id.to_string()),
            input_keys: Vec::new(),
            error: Some(reason.to_string()),
        },
    );
}

/// 校验节点写操作 fencing：节点已注册 + token 匹配 + 未过期 + 纪元匹配。
/// 越权/过期/旧 token/旧 epoch 一律拒绝（409）并留审计；节点未注册按 404。
fn check_node_lease(
    hub: &FleetHub,
    node_id: &str,
    lease_token: &str,
    epoch: u64,
    task_id: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let registered = {
        let nodes = hub.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.contains_key(node_id)
    };
    if !registered {
        audit_violation(hub, node_id, task_id, "节点未注册");
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("节点未注册：{node_id}"),
        ));
    }
    match hub.leases.verify_write(node_id, lease_token, epoch) {
        Ok(()) => Ok(()),
        Err(e) => {
            audit_violation(hub, node_id, task_id, &format!("fencing 拒绝：{e}"));
            Err(api_err(StatusCode::CONFLICT, format!("租约校验失败：{e}")))
        }
    }
}

/// 校验任务领取所有权：task 必须由 `node_id` 领取（防节点 B 回传节点 A 的任务）。
fn check_claim_owner(
    hub: &FleetHub,
    node_id: &str,
    task_id: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let owner = hub
        .claims
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(task_id)
        .cloned();
    match owner {
        Some(owner) if owner == node_id => Ok(()),
        Some(owner) => {
            let reason =
                format!("越权回传：任务 {task_id} 由 {owner} 领取，节点 {node_id} 无权操作");
            audit_violation(hub, node_id, task_id, &reason);
            Err(api_err(StatusCode::FORBIDDEN, reason))
        }
        None => {
            let reason = format!("任务 {task_id} 未被领取，节点 {node_id} 无权回传");
            audit_violation(hub, node_id, task_id, &reason);
            Err(api_err(StatusCode::FORBIDDEN, reason))
        }
    }
}

/// 生成 SSE 帧（历史重放 + 实时订阅共用）。
fn sse_frame(event: &str, payload: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({ "event": event, "payload": payload })
    )
}

// ---------- SSE 集线器（任务事件：历史重放 + 实时） ----------

/// 任务事件 SSE：task_id → 广播通道 + 历史（订阅先重放历史再流式）。
#[derive(Default)]
pub struct FleetSse {
    senders: Mutex<HashMap<String, broadcast::Sender<String>>>,
    history: Mutex<HashMap<String, Vec<String>>>,
}

impl FleetSse {
    pub fn new() -> Self {
        Self::default()
    }

    fn publish(&self, task_id: &str, frame: String) {
        if let Ok(mut history) = self.history.lock() {
            history
                .entry(task_id.to_string())
                .or_default()
                .push(frame.clone());
        }
        if let Ok(senders) = self.senders.lock() {
            if let Some(sender) = senders.get(task_id) {
                let _ = sender.send(frame);
            }
        }
    }

    /// 订阅：返回（广播接收端，历史帧）。
    pub fn subscribe(&self, task_id: &str) -> (broadcast::Receiver<String>, Vec<String>) {
        let sender = {
            let mut senders = self.senders.lock().unwrap_or_else(|e| e.into_inner());
            senders
                .entry(task_id.to_string())
                .or_insert_with(|| broadcast::channel(256).0)
                .clone()
        };
        let history = self
            .history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .cloned()
            .unwrap_or_default();
        (sender.subscribe(), history)
    }
}

// ---------- FleetHub：控制面运行态 ----------

/// 审批记录（影响预览 + 结构化证据齐备才批准）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub approval_id: String,
    pub task_id: String,
    pub step_id: String,
    pub owner_device: String,
    pub summary: String,
    pub impact_preview: String,
    pub evidence: Vec<EvidenceItem>,
    pub decided: bool,
    pub decision: Option<String>,
    pub approved_by: Option<String>,
}

/// 控制面运行态。
pub struct FleetHub {
    pub nodes: Mutex<HashMap<String, Arc<NodeAgent>>>,
    pub approvals: Mutex<HashMap<String, ApprovalRecord>>,
    /// R13：task_id → 领取节点 node_id（真实远端节点协议：领取所有权登记；防越权回传）。
    pub claims: Mutex<HashMap<String, String>>,
    pub transport: InMemoryTransport,
    pub leases: LeaseManager,
    pub bus: AgentBus,
    pub bus_store: BusStore,
    pub experience: ExperienceStore,
    /// R12 节点显式驱动阶段尚未消费 CAS（产物写入在 R13 经 HttpTransport 节点进程接线）；
    /// 保留字段以维持控制面"内容寻址产物"契约。
    #[allow(dead_code)]
    pub cas: CasStore,
    pub registry: CapabilityWorkerRegistry,
    pub sse: FleetSse,
}

impl FleetHub {
    /// 新建控制面运行态（持久化目录 data_root/fleet；测试可独立构造，避免跨测试污染）。
    pub fn new(data_root: &std::path::Path) -> Result<Arc<FleetHub>, String> {
        Self::with_lease(
            data_root,
            LeaseConfig {
                ttl_secs: 60,
                renew_interval_secs: 20,
            },
        )
    }

    /// 自定义租约配置构造（测试用短 TTL 验证租约过期/fencing；生产用 [`Self::new`]）。
    pub fn with_lease(
        data_root: &std::path::Path,
        lease: LeaseConfig,
    ) -> Result<Arc<FleetHub>, String> {
        let fleet_dir = data_root.join("fleet");
        let bus = AgentBus::new();
        let bus_store = BusStore::new(Some(fleet_dir.join("bus.jsonl")))?;
        // 运行时挂接总线持久化（关键消息自动落盘；独立任务避免同步初始化阻塞）。
        {
            let bus2 = bus.clone();
            let store2 = bus_store.clone();
            tokio::spawn(async move {
                bus2.attach_store(store2).await;
            });
        }
        let cas = CasStore::new(fleet_dir.join("cas"))?;
        let experience = ExperienceStore::new(Some(fleet_dir.join("experience.jsonl")))?;
        Ok(Arc::new(FleetHub {
            nodes: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            claims: Mutex::new(HashMap::new()),
            transport: InMemoryTransport::with_ttl(Duration::from_secs(120)),
            leases: LeaseManager::with_config(lease),
            bus,
            bus_store,
            experience,
            cas,
            registry: CapabilityWorkerRegistry::new(),
            sse: FleetSse::new(),
        }))
    }
}

/// 进程级控制面运行态（生产：Agent 1 挂载 `fleet_api::router` 时初始化；幂等）。
/// 测试用 [`FleetHub::new`] 独立构造，故本函数在未挂载前标记 dead_code。
#[allow(dead_code)]
pub fn fleet_hub(data_root: &std::path::Path) -> Arc<FleetHub> {
    static HUB: OnceLock<Arc<FleetHub>> = OnceLock::new();
    HUB.get_or_init(|| {
        FleetHub::new(data_root).unwrap_or_else(|e| panic!("fleet hub 初始化失败：{e}"))
    })
    .clone()
}

/// 节点注册请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterNodeBody {
    pub node_id: String,
    pub card: CapabilityCard,
}

/// 任务提交请求（直接内联 TransportTask 字段，便于契约稳定）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitTaskBody {
    pub task_id: String,
    pub worker: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub correlation_id: String,
    #[serde(default)]
    pub lineage: Vec<String>,
    #[serde(default)]
    pub approval_required: bool,
}

/// 审批响应请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRespondBody {
    pub decision: String,
    pub approved_by: String,
}

/// 任务查询结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: String,
    pub worker: String,
    pub correlation_id: String,
    pub status: TransportStatus,
    pub events: Vec<TransportEvent>,
    #[serde(default)]
    pub approval: Option<ApprovalRecord>,
}

// ---------- 路由 ----------

/// 组装 fleet 路由（handler 状态 = 独立 [`FleetHub`]；生产经 [`router`] 用进程级 hub）。
pub fn router_with_hub(hub: Arc<FleetHub>) -> Router {
    Router::new()
        .route("/fleet/nodes/register", post(register_node))
        .route("/fleet/nodes", get(list_nodes))
        .route("/fleet/nodes/{node_id}/heartbeat", post(node_heartbeat))
        .route("/fleet/nodes/{node_id}/tasks", get(node_claimable_tasks))
        .route("/fleet/tasks/submit", post(submit_task))
        .route("/fleet/tasks/{id}", get(get_task))
        .route("/fleet/tasks/{id}/claim", post(claim_task))
        .route("/fleet/tasks/{id}/progress", post(report_progress))
        .route("/fleet/tasks/{id}/result", post(report_result))
        .route("/fleet/tasks/{id}/cancel-ack", post(cancel_ack))
        .route("/fleet/tasks/{id}/cancel", post(cancel_task))
        .route("/fleet/tasks/{id}/events", get(task_events))
        .route("/fleet/approvals/{id}/respond", post(respond_approval))
        .with_state(hub)
}

/// 组装 fleet 路由（待主控在 build_router merge；data_root 用于控制面持久化目录）。
/// 测试用 [`router_with_hub`]，故本函数在未挂载前标记 dead_code。
#[allow(dead_code)]
pub fn router(state: Arc<owo_agent_server::AppState>) -> Router {
    router_with_hub(fleet_hub(&state.data_root))
}

/// 状态推进说明：R13 起控制面不再在本进程"伪执行完成"任务——任务提交后只进入
/// Running（等待节点领取）；状态推进仅由显式节点领取（`claim`）+ 结果回传（`result`）驱动。
/// 协议违规（越权/过期 epoch/不匹配节点）经 [`audit_violation`] 留审计。
impl FleetHub {
    /// 生成任务视图（从传输层读状态/事件）。
    fn task_view(&self, task_id: &str) -> Option<TaskView> {
        let task = self.transport.task(task_id);
        let status = self.transport.task_status(task_id)?;
        let events = self.transport.task_events(task_id);
        let approval = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(task_id)
            .cloned();
        let (worker, correlation_id) = match &task {
            Some(t) => (t.worker.clone(), t.correlation_id.clone()),
            None => (String::new(), String::new()),
        };
        Some(TaskView {
            task_id: task_id.to_string(),
            worker,
            correlation_id,
            status,
            events,
            approval,
        })
    }
}

// ---------- 节点 ----------

async fn register_node(
    State(hub): State<Arc<FleetHub>>,
    Json(body): Json<RegisterNodeBody>,
) -> ApiResult<serde_json::Value> {
    let node_id = body.node_id.trim().to_string();
    if node_id.is_empty() {
        return Err(api_err(StatusCode::BAD_REQUEST, "node_id 不能为空"));
    }
    let existing = {
        let nodes = hub.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.get(&node_id).cloned()
    };
    let node = match existing {
        Some(node) => {
            // 幂等重注册 = 心跳续租（复用现有租约 token；token 失效时重新获取）。
            node.heartbeat_and_report_persisted(&hub.registry).await;
            node
        }
        None => {
            let node = Arc::new(NodeAgent::with_timeout(
                node_id.clone(),
                body.card.clone(),
                Duration::from_secs(3),
                owo_agent_core::fleet::RestartRule::default(),
            ));
            node.attach_control_plane(hub.leases.clone(), hub.bus.clone(), hub.experience.clone());
            let lease = node
                .register_with_control_plane(&hub.registry)
                .await
                .map_err(|e| api_err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            let _ = owo_agent_core::bus_store::persist_node_status(
                &hub.bus_store,
                &node_id,
                true,
                "节点注册",
            );
            hub.sse.publish(
                &node_id,
                format!(
                    "data: {}\n\n",
                    serde_json::json!({ "event": "node_registered", "node_id": node_id, "lease_epoch": lease.epoch })
                ),
            );
            if let Ok(mut nodes) = hub.nodes.lock() {
                nodes.insert(node_id.clone(), node.clone());
            }
            node
        }
    };
    // 返回 lease_token：真实远端节点后续心跳/领取/回传需以 token + epoch 做 fencing。
    Ok(Json(serde_json::json!({
        "node_id": node_id,
        "status": node.status(),
        "lease_epoch": node.status().lease_epoch,
        "lease_token": node.lease_token(),
        "renew_interval_secs": hub.leases.lease(&node_id).map(|l| l.ttl.as_secs()).unwrap_or(0),
    })))
}

async fn list_nodes(State(hub): State<Arc<FleetHub>>) -> ApiResult<serde_json::Value> {
    let nodes = hub.nodes.lock().unwrap_or_else(|e| e.into_inner());
    let list: Vec<NodeStatus> = nodes.values().map(|n| n.status()).collect();
    Ok(Json(
        serde_json::json!({ "nodes": list, "count": list.len() }),
    ))
}

/// 节点心跳续租：`POST /fleet/nodes/{node_id}/heartbeat`。
/// `lease_token` 必须匹配当前租约；旧 token / 过期租约被拒（409 + 审计），
/// 节点需重新注册（re-acquire）拿新 token（重连恢复路径）。
async fn node_heartbeat(
    State(hub): State<Arc<FleetHub>>,
    Path(node_id): Path<String>,
    Json(body): Json<NodeHeartbeatBody>,
) -> ApiResult<NodeHeartbeatResponse> {
    let registered = {
        let nodes = hub.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.contains_key(&node_id)
    };
    if !registered {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("节点未注册：{node_id}"),
        ));
    }
    let lease = match hub.leases.renew(&node_id, &body.lease_token) {
        Ok(lease) => lease,
        Err(e) => {
            // 旧 token / 过期：拒绝并留审计，不静默重签（重连须显式重新注册）。
            audit_violation(&hub, &node_id, "", &format!("心跳续租失败：{e}"));
            return Err(api_err(
                StatusCode::CONFLICT,
                format!("心跳续租失败（旧 token 或租约过期）：{e}"),
            ));
        }
    };
    hub.sse.publish(
        &node_id,
        sse_frame(
            "node_heartbeat",
            serde_json::json!({ "node_id": node_id, "lease_epoch": lease.epoch }),
        ),
    );
    Ok(Json(NodeHeartbeatResponse {
        node_id,
        valid: true,
        lease_epoch: lease.epoch,
        lease_token: lease.token,
        renew_interval_secs: lease.ttl.as_secs(),
    }))
}

/// 节点可领取/已领取任务：`GET /fleet/nodes/{node_id}/tasks`。
/// 匹配规则：`task.worker == node_id` 且状态 Running（可领取）或已被本节点领取。
async fn node_claimable_tasks(
    State(hub): State<Arc<FleetHub>>,
    Path(node_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    let registered = {
        let nodes = hub.nodes.lock().unwrap_or_else(|e| e.into_inner());
        nodes.contains_key(&node_id)
    };
    if !registered {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("节点未注册：{node_id}"),
        ));
    }
    let claims = hub.claims.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut tasks = Vec::new();
    for task_id in hub.transport.task_ids() {
        let Some(task) = hub.transport.task(&task_id) else {
            continue;
        };
        if task.worker != node_id {
            continue;
        }
        let status = hub
            .transport
            .task_status(&task_id)
            .unwrap_or(TransportStatus::Pending);
        let claimed_by = claims.get(&task_id).cloned();
        tasks.push(serde_json::json!({
            "task_id": task_id,
            "worker": task.worker,
            "status": status,
            "correlation_id": task.correlation_id,
            "claimed_by": claimed_by,
            "claimable": matches!(status, TransportStatus::Running) && claimed_by.is_none(),
        }));
    }
    Ok(Json(
        serde_json::json!({ "node_id": node_id, "tasks": tasks, "count": tasks.len() }),
    ))
}

// ---------- 任务 ----------

async fn submit_task(
    State(hub): State<Arc<FleetHub>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SubmitTaskBody>,
) -> ApiResult<serde_json::Value> {
    let task_id = body.task_id.trim().to_string();
    if task_id.is_empty() {
        return Err(api_err(StatusCode::BAD_REQUEST, "task_id 不能为空"));
    }
    // 幂等键：Idempotency-Key 头优先，否则 task_id 本身（transport 拒绝重复提交）。
    let idempotency = headers
        .get("Idempotency-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&task_id)
        .to_string();
    let task = TransportTask {
        task_id: task_id.clone(),
        worker: body.worker.clone(),
        input: body.input.clone(),
        correlation_id: if body.correlation_id.is_empty() {
            format!("fleet:{}", task_id)
        } else {
            body.correlation_id.clone()
        },
        lineage: body.lineage.clone(),
        approval_required: body.approval_required,
    };
    // 审批任务：登记审批记录（影响预览 + 结构化证据由调用方以 input.approval 提供）。
    if body.approval_required {
        let approval = ApprovalRecord {
            approval_id: idempotency.clone(),
            task_id: task_id.clone(),
            step_id: body
                .input
                .pointer("/step_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&task_id)
                .to_string(),
            owner_device: body
                .input
                .pointer("/approval/owner_device")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            summary: body
                .input
                .pointer("/approval/summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            impact_preview: body
                .input
                .pointer("/impact_preview")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            evidence: body
                .input
                .pointer("/evidence")
                .and_then(|v| serde_json::from_value::<Vec<EvidenceItem>>(v.clone()).ok())
                .unwrap_or_default(),
            decided: false,
            decision: None,
            approved_by: None,
        };
        if let Ok(mut approvals) = hub.approvals.lock() {
            approvals.insert(task_id.clone(), approval);
        }
    }
    hub.transport
        .submit(task)
        .await
        .map_err(|e| api_err(StatusCode::CONFLICT, e))?;
    // 总线持久化（关键消息落盘；崩溃重放恢复）。
    let _ = owo_agent_core::bus_store::persist_remote_event(
        &hub.bus_store,
        &owo_agent_core::remote_step::RemoteStepEvent::Submitted {
            step_id: task_id.clone(),
            correlation_id: body.correlation_id.clone(),
            worker: body.worker.clone(),
        },
    );
    let status = hub
        .transport
        .task_status(&task_id)
        .unwrap_or(TransportStatus::Pending);
    Ok(Json(serde_json::json!({
        "task_id": task_id,
        "status": status,
        "idempotency_key": idempotency,
    })))
}

async fn get_task(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
) -> ApiResult<TaskView> {
    hub.task_view(&task_id)
        .map(Json)
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, format!("未知任务：{task_id}")))
}

// ---------- R13 节点协议：领取 / 进度 / 结果 / 取消确认 ----------

/// 节点领取任务：`POST /fleet/tasks/{id}/claim`。
/// 规则：节点已注册 + fencing（token + epoch）通过 + `task.worker == node_id` +
/// 状态 Running + 未被其他节点领取。领取即登记所有权（防越权回传）。
async fn claim_task(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
    Json(body): Json<NodeClaimBody>,
) -> ApiResult<serde_json::Value> {
    check_node_lease(&hub, &body.node_id, &body.lease_token, body.epoch, &task_id)?;
    let Some(task) = hub.transport.task(&task_id) else {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知任务：{task_id}"),
        ));
    };
    // 匹配任务：节点按自身 node_id 领取（worker 必须等于 node_id）。
    if task.worker != body.node_id {
        let reason = format!(
            "节点不匹配：任务 {task_id} 指派给 {}，节点 {} 无权领取",
            task.worker, body.node_id
        );
        audit_violation(&hub, &body.node_id, &task_id, &reason);
        return Err(api_err(StatusCode::FORBIDDEN, reason));
    }
    let status = hub
        .transport
        .task_status(&task_id)
        .unwrap_or(TransportStatus::Pending);
    if !matches!(status, TransportStatus::Running) {
        let reason = format!("任务 {task_id} 不在可领取状态（当前 {status:?}）");
        audit_violation(&hub, &body.node_id, &task_id, &reason);
        return Err(api_err(StatusCode::CONFLICT, reason));
    }
    // 领取所有权：已被其他节点领取 → 拒绝。
    {
        let mut claims = hub.claims.lock().unwrap_or_else(|e| e.into_inner());
        match claims.get(&task_id).cloned() {
            Some(owner) if owner != body.node_id => {
                let reason = format!("任务 {task_id} 已被节点 {owner} 领取");
                audit_violation(&hub, &body.node_id, &task_id, &reason);
                return Err(api_err(StatusCode::CONFLICT, reason));
            }
            // 同节点重复领取 = 幂等（返回现状）。
            Some(_) => {
                return Ok(Json(serde_json::json!({
                    "task_id": task_id,
                    "node_id": body.node_id,
                    "status": status,
                })));
            }
            None => {
                claims.insert(task_id.clone(), body.node_id.clone());
            }
        }
    }
    hub.sse.publish(
        &task_id,
        sse_frame(
            "claimed",
            serde_json::json!({ "task_id": task_id, "node_id": body.node_id }),
        ),
    );
    Ok(Json(serde_json::json!({
        "task_id": task_id,
        "node_id": body.node_id,
        "status": status,
    })))
}

/// 节点回传进度 + 结构化证据：`POST /fleet/tasks/{id}/progress`。
/// 要求：fencing 通过 + 本节点是领取者 + 任务在 Running。
async fn report_progress(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
    Json(body): Json<NodeProgressBody>,
) -> ApiResult<serde_json::Value> {
    check_node_lease(&hub, &body.node_id, &body.lease_token, body.epoch, &task_id)?;
    check_claim_owner(&hub, &body.node_id, &task_id)?;
    let status = hub
        .transport
        .task_status(&task_id)
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, format!("未知任务：{task_id}")))?;
    if !matches!(status, TransportStatus::Running) {
        let reason = format!("任务 {task_id} 不在运行态，无法回传进度（当前 {status:?}）");
        audit_violation(&hub, &body.node_id, &task_id, &reason);
        return Err(api_err(StatusCode::CONFLICT, reason));
    }
    let task = hub.transport.task(&task_id);
    let event = TransportEvent {
        task_id: task_id.clone(),
        kind: TransportEventKind::Progress,
        correlation_id: task
            .as_ref()
            .map(|t| t.correlation_id.clone())
            .unwrap_or_default(),
        payload: serde_json::json!({
            "node_id": body.node_id,
            "text": body.text,
            "evidence": body.evidence,
        }),
        lineage: task.map(|t| t.lineage).unwrap_or_default(),
    };
    if !hub.transport.append_event(&task_id, event) {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知任务：{task_id}"),
        ));
    }
    hub.sse.publish(
        &task_id,
        sse_frame(
            "progress",
            serde_json::json!({ "task_id": task_id, "node_id": body.node_id, "text": body.text }),
        ),
    );
    Ok(Json(serde_json::json!({
        "task_id": task_id,
        "node_id": body.node_id,
        "status": "running",
    })))
}

/// 节点回传成功/失败结果：`POST /fleet/tasks/{id}/result`。
/// 要求：fencing 通过 + 本节点是领取者 + 任务在 Running；
/// 成功 → Succeeded + Result 事件（携带 output/output_cas/evidence）；
/// 失败 → Failed + Cancelled 事件（携带 error）。终态任务重复回传被幂等拒绝。
async fn report_result(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
    Json(body): Json<NodeResultBody>,
) -> ApiResult<serde_json::Value> {
    check_node_lease(&hub, &body.node_id, &body.lease_token, body.epoch, &task_id)?;
    check_claim_owner(&hub, &body.node_id, &task_id)?;
    let status = hub
        .transport
        .task_status(&task_id)
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, format!("未知任务：{task_id}")))?;
    if !matches!(status, TransportStatus::Running) {
        let reason = format!("任务 {task_id} 不在运行态，无法回传结果（当前 {status:?}）");
        audit_violation(&hub, &body.node_id, &task_id, &reason);
        return Err(api_err(StatusCode::CONFLICT, reason));
    }
    let payload = serde_json::json!({
        "ok": body.ok,
        "output": body.output,
        "output_cas": body.output_cas,
        "evidence": body.evidence,
        "error": body.error,
    });
    if !hub.transport.complete_task(&task_id, body.ok, payload) {
        let reason = format!("任务 {task_id} 已是终态，重复结果回传被拒绝");
        audit_violation(&hub, &body.node_id, &task_id, &reason);
        return Err(api_err(StatusCode::CONFLICT, reason));
    }
    // 释放领取所有权（结果已入终态，防残留）。
    if let Ok(mut claims) = hub.claims.lock() {
        claims.remove(&task_id);
    }
    hub.sse.publish(
        &task_id,
        sse_frame(
            if body.ok { "succeeded" } else { "failed" },
            serde_json::json!({ "task_id": task_id, "node_id": body.node_id, "ok": body.ok }),
        ),
    );
    let final_status = hub
        .transport
        .task_status(&task_id)
        .unwrap_or(TransportStatus::Failed);
    Ok(Json(serde_json::json!({
        "task_id": task_id,
        "node_id": body.node_id,
        "ok": body.ok,
        "status": final_status,
    })))
}

/// 节点确认取消：`POST /fleet/tasks/{id}/cancel-ack`。
/// 要求：fencing 通过 + 本节点是领取者 + 任务已取消；确认后追加 ack 事件并释放所有权。
async fn cancel_ack(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
    Json(body): Json<NodeCancelAckBody>,
) -> ApiResult<serde_json::Value> {
    check_node_lease(&hub, &body.node_id, &body.lease_token, body.epoch, &task_id)?;
    check_claim_owner(&hub, &body.node_id, &task_id)?;
    let status = hub
        .transport
        .task_status(&task_id)
        .ok_or_else(|| api_err(StatusCode::NOT_FOUND, format!("未知任务：{task_id}")))?;
    if !matches!(status, TransportStatus::Cancelled) {
        let reason = format!("任务 {task_id} 未处于取消态（当前 {status:?}），无需确认");
        audit_violation(&hub, &body.node_id, &task_id, &reason);
        return Err(api_err(StatusCode::CONFLICT, reason));
    }
    let task = hub.transport.task(&task_id);
    let event = TransportEvent {
        task_id: task_id.clone(),
        kind: TransportEventKind::Cancelled,
        correlation_id: task
            .as_ref()
            .map(|t| t.correlation_id.clone())
            .unwrap_or_default(),
        payload: serde_json::json!({ "acknowledged": true, "node_id": body.node_id }),
        lineage: task.map(|t| t.lineage).unwrap_or_default(),
    };
    if !hub.transport.append_event(&task_id, event) {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知任务：{task_id}"),
        ));
    }
    if let Ok(mut claims) = hub.claims.lock() {
        claims.remove(&task_id);
    }
    hub.sse.publish(
        &task_id,
        sse_frame(
            "cancel_acknowledged",
            serde_json::json!({ "task_id": task_id, "node_id": body.node_id }),
        ),
    );
    Ok(Json(serde_json::json!({
        "task_id": task_id,
        "node_id": body.node_id,
        "status": "cancelled",
        "acknowledged": true,
    })))
}

async fn cancel_task(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    hub.transport
        .cancel(&task_id)
        .await
        .map_err(|e| api_err(StatusCode::NOT_FOUND, e))?;
    hub.sse.publish(
        &task_id,
        format!(
            "data: {}\n\n",
            serde_json::json!({ "event": "cancelled", "task_id": task_id })
        ),
    );
    Ok(Json(
        serde_json::json!({ "task_id": task_id, "status": "cancelled" }),
    ))
}

/// SSE 事件查询参数。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventsQuery {
    /// `json` 时返回一次性 JSON 数组（供 HttpTransport::events 拉取）。
    #[serde(default)]
    pub format: Option<String>,
}

async fn task_events(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if hub.transport.task_status(&task_id).is_none() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知任务：{task_id}"),
        ));
    }
    if query.format.as_deref() == Some("json") {
        let events = hub.transport.task_events(&task_id);
        return Ok(Json(events).into_response());
    }
    // SSE：历史重放 + 实时。
    let (rx, history) = hub.sse.subscribe(&task_id);
    let stream = tokio_stream::wrappers::BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(frame) => Some(Ok::<Event, Infallible>(Event::default().data(frame))),
        Err(_) => None,
    });
    // 历史帧先行。
    let history_stream = tokio_stream::iter(history)
        .map(|frame| Ok::<Event, Infallible>(Event::default().data(frame)));
    let combined = history_stream.chain(stream);
    Ok(Sse::new(combined).into_response())
}

// ---------- 审批 ----------

async fn respond_approval(
    State(hub): State<Arc<FleetHub>>,
    Path(task_id): Path<String>,
    Json(body): Json<ApprovalRespondBody>,
) -> ApiResult<serde_json::Value> {
    let approval = {
        let approvals = hub.approvals.lock().unwrap_or_else(|e| e.into_inner());
        approvals.get(&task_id).cloned()
    };
    let Some(mut approval) = approval else {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("任务 {task_id} 不是审批任务或无审批记录"),
        ));
    };
    if approval.decided {
        return Err(api_err(StatusCode::CONFLICT, "审批已裁决"));
    }
    let status = hub
        .transport
        .task_status(&task_id)
        .unwrap_or(TransportStatus::Pending);
    if !matches!(status, TransportStatus::AwaitingApproval) {
        return Err(api_err(
            StatusCode::CONFLICT,
            format!("任务 {task_id} 不在审批等待态（当前 {status:?}）"),
        ));
    }
    match body.decision.as_str() {
        "approve" => {
            // 影响预览 + 结构化证据齐备才批准（否则拒绝执行）。
            if approval.impact_preview.trim().is_empty() || approval.evidence.is_empty() {
                approval.decided = true;
                approval.decision = Some("rejected".to_string());
                approval.approved_by = Some(body.approved_by.clone());
                let _ = hub
                    .transport
                    .deny_task(&task_id, "审批材料不齐（缺影响预览或证据）");
                if let Ok(mut approvals) = hub.approvals.lock() {
                    approvals.insert(task_id.clone(), approval.clone());
                }
                hub.sse.publish(
                    &task_id,
                    format!(
                        "data: {}\n\n",
                        serde_json::json!({ "event": "approval_rejected", "task_id": task_id })
                    ),
                );
                return Err(api_err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "审批材料不齐：需影响预览 + 结构化证据",
                ));
            }
            if !hub.transport.approve_task(&task_id, &body.approved_by) {
                return Err(api_err(StatusCode::INTERNAL_SERVER_ERROR, "审批放行失败"));
            }
            approval.decided = true;
            approval.decision = Some("approved".to_string());
            approval.approved_by = Some(body.approved_by.clone());
            if let Ok(mut approvals) = hub.approvals.lock() {
                approvals.insert(task_id.clone(), approval.clone());
            }
            hub.sse.publish(
                &task_id,
                format!(
                    "data: {}\n\n",
                    serde_json::json!({ "event": "approval_granted", "task_id": task_id })
                ),
            );
            Ok(Json(serde_json::json!({
                "task_id": task_id,
                "decision": "approved",
                "status": hub.transport.task_status(&task_id),
            })))
        }
        "reject" => {
            if !hub
                .transport
                .deny_task(&task_id, &format!("用户拒绝：{}", body.approved_by))
            {
                return Err(api_err(StatusCode::INTERNAL_SERVER_ERROR, "审批拒绝失败"));
            }
            approval.decided = true;
            approval.decision = Some("rejected".to_string());
            approval.approved_by = Some(body.approved_by.clone());
            if let Ok(mut approvals) = hub.approvals.lock() {
                approvals.insert(task_id.clone(), approval.clone());
            }
            hub.sse.publish(
                &task_id,
                format!(
                    "data: {}\n\n",
                    serde_json::json!({ "event": "approval_rejected", "task_id": task_id })
                ),
            );
            Ok(Json(serde_json::json!({
                "task_id": task_id,
                "decision": "rejected",
                "status": "cancelled",
            })))
        }
        other => Err(api_err(
            StatusCode::BAD_REQUEST,
            format!("decision 必须是 approve/reject，实际 {other}"),
        )),
    }
}
