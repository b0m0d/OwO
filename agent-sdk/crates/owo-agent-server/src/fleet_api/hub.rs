use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::bus_store::BusStore;
use owo_agent_core::capability::CapabilityWorkerRegistry;
use owo_agent_core::cas_store::CasStore;
use owo_agent_core::experience_store::{Attribution, ExperienceStore, Outcome};
use owo_agent_core::fleet::{AgentBus, BusMessage, MessageKind, CONTROL_PLANE_AGENT};
use owo_agent_core::fleet_node_protocol::{violation_correlation_id, NodeProtocolViolation};
use owo_agent_core::fleet_transport::InMemoryTransport;
use owo_agent_core::lease::{LeaseConfig, LeaseManager};
use owo_agent_core::node_agent::NodeAgent;
use owo_agent_core::remote_step::EvidenceItem;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::broadcast;
pub(super) type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

pub(super) fn api_err(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

// ---------- 节点协议校验与违规审计（R13） ----------

/// 协议违规审计：总线落盘 + 经验记录（幂等键 = node:protocol:violation:<node>:<task>）。
pub(super) fn audit_violation(hub: &FleetHub, node_id: &str, task_id: &str, reason: &str) {
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
pub(super) fn check_node_lease(
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
pub(super) fn check_claim_owner(
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
pub(super) fn sse_frame(event: &str, payload: serde_json::Value) -> String {
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

    pub(super) fn publish(&self, task_id: &str, frame: String) {
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
/// lib.rs build_router 与 goal_api 的 `fleet_node` 目标接线均使用本函数；
/// `#[path]` 独立编译的 fleet_api_tests 目标内无 lib 接线，故保留 allow。
#[allow(dead_code)]
pub fn fleet_hub(data_root: &std::path::Path) -> Arc<FleetHub> {
    static HUB: OnceLock<Arc<FleetHub>> = OnceLock::new();
    HUB.get_or_init(|| {
        FleetHub::new(data_root).unwrap_or_else(|e| panic!("fleet hub 初始化失败：{e}"))
    })
    .clone()
}
