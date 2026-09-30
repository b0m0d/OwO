use crate::bus_store::BusStore;
use crate::fleet_transport::{FleetTransport, TransportTask};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
pub type AgentId = String;
pub type CorrelationId = String;

/// 消息种类（A2A 语义子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageKind {
    Task,
    Result,
    Review,
    Refusal,
    Progress,
}

/// 可合并事件（进度类）允许在背压时静默丢弃。
pub fn is_mergeable(kind: MessageKind) -> bool {
    matches!(kind, MessageKind::Progress)
}

/// 总线消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub id: u64,
    pub from: AgentId,
    pub to: AgentId,
    pub kind: MessageKind,
    pub correlation_id: CorrelationId,
    pub payload: serde_json::Value,
}

/// worker 生命周期事件种类（worker_pool 崩溃/重启/熔断等进入总线与审计）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEventKind {
    Started,
    Crashed,
    Restarted,
    Fused,
    Stopped,
    BudgetAborted,
    Cancelled,
}

impl WorkerEventKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Crashed => "crashed",
            Self::Restarted => "restarted",
            Self::Fused => "fused",
            Self::Stopped => "stopped",
            Self::BudgetAborted => "budget_aborted",
            Self::Cancelled => "cancelled",
        }
    }
}

/// worker 生命周期事件（总线载荷 / 审计条目的统一结构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerEvent {
    pub worker: AgentId,
    pub kind: WorkerEventKind,
    pub detail: String,
    pub correlation_id: CorrelationId,
}

impl WorkerEvent {
    pub fn new(
        worker: impl Into<AgentId>,
        kind: WorkerEventKind,
        detail: impl Into<String>,
        correlation_id: impl Into<CorrelationId>,
    ) -> Self {
        Self {
            worker: worker.into(),
            kind,
            detail: detail.into(),
            correlation_id: correlation_id.into(),
        }
    }
}

/// 邮箱溢出策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// 可合并事件（进度类）丢弃，关键事件（任务/结果/评审/拒绝）保留并报满。
    DropMergeable,
    /// 全部拒绝，调用方应退避或熔断。
    Reject,
}

/// 入队结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    /// 按策略丢弃（未投递）。
    Dropped,
}

/// 总线/邮箱错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    MailboxFull(usize),
    UnknownAgent(AgentId),
    /// 关键消息持久化失败（必须显式报错，不得静默丢消息）。
    Persist(String),
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::MailboxFull(cap) => write!(f, "mailbox full (capacity {cap})"),
            BusError::UnknownAgent(id) => write!(f, "unknown agent `{id}`"),
            BusError::Persist(reason) => write!(f, "总线消息持久化失败：{reason}"),
        }
    }
}

impl Error for BusError {}

/// 有界邮箱：背压语义落在 [`Mailbox::push`] 的返回上。
#[derive(Debug, Clone)]
pub struct Mailbox {
    capacity: usize,
    queue: VecDeque<BusMessage>,
}

impl Mailbox {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            queue: VecDeque::new(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn push(
        &mut self,
        msg: BusMessage,
        policy: OverflowPolicy,
    ) -> Result<PushOutcome, BusError> {
        if self.queue.len() < self.capacity {
            self.queue.push_back(msg);
            return Ok(PushOutcome::Pushed);
        }
        match policy {
            OverflowPolicy::DropMergeable if is_mergeable(msg.kind) => Ok(PushOutcome::Dropped),
            _ => Err(BusError::MailboxFull(self.capacity)),
        }
    }

    pub fn drain(&mut self) -> Vec<BusMessage> {
        self.queue.drain(..).collect()
    }
}

/// 本地 Agent 总线：注册表 + 定向/广播投递 + 可选持久化（断点重放）。
#[derive(Clone, Default, Debug)]
pub struct AgentBus {
    mailboxes: Arc<Mutex<HashMap<AgentId, Mailbox>>>,
    next_id: Arc<AtomicU64>,
    /// 可选总线持久化存储（关键消息落盘；`replay_store` 断点重放）。
    store: Arc<Mutex<Option<BusStore>>>,
}

impl AgentBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// 挂接总线持久化存储：此后 send 的关键消息（Task/Result/Review/Refusal）自动落盘。
    pub async fn attach_store(&self, store: BusStore) {
        *self.store.lock().await = Some(store);
    }

    /// 断点重放：把已持久化的关键消息按序重新投递到已注册 agent 的邮箱
    /// （接收方以 `dedupe_messages` 幂等去重，保证不重复执行）。返回成功投递数。
    pub async fn replay_store(&self) -> usize {
        let Some(store) = self.store.lock().await.clone() else {
            return 0;
        };
        let msgs = store.replay_messages();
        let mut delivered = 0;
        let mut boxes = self.mailboxes.lock().await;
        for msg in msgs {
            if let Some(mailbox) = boxes.get_mut(&msg.to) {
                if matches!(
                    mailbox.push(msg, OverflowPolicy::Reject),
                    Ok(PushOutcome::Pushed)
                ) {
                    delivered += 1;
                }
            }
        }
        delivered
    }

    /// 已持久化消息数（诊断用）。
    pub async fn store_len(&self) -> usize {
        self.store
            .lock()
            .await
            .as_ref()
            .map(|s| s.len())
            .unwrap_or(0)
    }

    pub async fn register(&self, id: impl Into<AgentId>, capacity: usize) {
        let mut boxes = self.mailboxes.lock().await;
        boxes.insert(id.into(), Mailbox::new(capacity));
    }

    pub async fn unregister(&self, id: &str) -> bool {
        self.mailboxes.lock().await.remove(id).is_some()
    }

    pub async fn contains(&self, id: &str) -> bool {
        self.mailboxes.lock().await.contains_key(id)
    }

    pub async fn agent_count(&self) -> usize {
        self.mailboxes.lock().await.len()
    }

    pub async fn send(
        &self,
        from: impl Into<AgentId>,
        to: impl Into<AgentId>,
        kind: MessageKind,
        correlation_id: impl Into<CorrelationId>,
        payload: serde_json::Value,
        policy: OverflowPolicy,
    ) -> Result<u64, BusError> {
        let from = from.into();
        let to = to.into();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = BusMessage {
            id,
            from,
            to: to.clone(),
            kind,
            correlation_id: correlation_id.into(),
            payload,
        };
        // 关键消息先持久化（崩溃后可重放；幂等去重由 BusStore 保证），进度类按策略。
        if let Some(store) = self.store.lock().await.clone() {
            if store.should_persist(msg.kind) {
                store.persist(&msg).map_err(BusError::Persist)?;
            }
        }
        let mut boxes = self.mailboxes.lock().await;
        let mailbox = boxes
            .get_mut(&to)
            .ok_or_else(|| BusError::UnknownAgent(to.clone()))?;
        mailbox.push(msg, policy)?;
        Ok(id)
    }

    /// 广播到所有已注册 agent，返回成功投递的 agent 列表（溢出/未注册者跳过）。
    /// 持久化：同一逻辑消息（correlation_id+种类+载荷相同）只落盘一次（幂等去重）。
    pub async fn broadcast(
        &self,
        from: impl Into<AgentId>,
        topic: impl Into<AgentId>,
        kind: MessageKind,
        correlation_id: impl Into<CorrelationId>,
        payload: serde_json::Value,
        policy: OverflowPolicy,
    ) -> Vec<AgentId> {
        let from = from.into();
        let topic = topic.into();
        let correlation_id = correlation_id.into();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut boxes = self.mailboxes.lock().await;
        let ids: Vec<AgentId> = boxes.keys().cloned().collect();
        let mut delivered = Vec::with_capacity(ids.len());
        for to in ids {
            let msg = BusMessage {
                id,
                from: from.clone(),
                to: to.clone(),
                kind,
                correlation_id: correlation_id.clone(),
                payload: payload.clone(),
            };
            if let Some(mailbox) = boxes.get_mut(&to) {
                if matches!(mailbox.push(msg, policy), Ok(PushOutcome::Pushed)) {
                    delivered.push(to);
                }
            }
        }
        let _ = topic;
        // 持久化放 mailbox 锁外（仅一次；dedup_key 相同则后续调用幂等跳过）。
        if let Some(store) = self.store.lock().await.clone() {
            if store.should_persist(kind) && !delivered.is_empty() {
                let representative = BusMessage {
                    id,
                    from,
                    to: delivered[0].clone(),
                    kind,
                    correlation_id,
                    payload,
                };
                let _ = store.persist(&representative);
            }
        }
        delivered
    }

    /// 取出某个 agent 的全部待处理消息（拉模型，配合 worker 循环轮询）。
    pub async fn drain(&self, id: &str) -> Vec<BusMessage> {
        self.mailboxes
            .lock()
            .await
            .get_mut(id)
            .map(|m| m.drain())
            .unwrap_or_default()
    }

    /// 发送 worker 生命周期事件（崩溃/重启/熔断等；关键语义，溢出时拒绝而非丢弃）。
    /// `to` 为监督者 agent；载荷统一为 `WorkerEvent` JSON。
    pub async fn send_worker_event(
        &self,
        from: impl Into<AgentId>,
        to: impl Into<AgentId>,
        event: &WorkerEvent,
    ) -> Result<u64, BusError> {
        // WorkerEvent 全部字段可序列化，to_value 不会失败。
        let payload = serde_json::to_value(event).expect("WorkerEvent 序列化不可失败");
        self.send(
            from,
            to,
            MessageKind::Task,
            event.correlation_id.clone(),
            payload,
            OverflowPolicy::Reject,
        )
        .await
    }

    pub async fn pending(&self, id: &str) -> usize {
        self.mailboxes
            .lock()
            .await
            .get(id)
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

/// 任务预算（对齐设计文档 2.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub max_turns: u32,
    pub max_steps: u32,
    pub max_duration_secs: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_steps: 1000,
            max_duration_secs: 600,
        }
    }
}

impl Budget {
    pub fn exceeded(&self, turns: u32, steps: u32, elapsed: Duration) -> bool {
        turns >= self.max_turns
            || steps >= self.max_steps
            || elapsed.as_secs() >= self.max_duration_secs
    }
}

/// 生成关联 ID（贯通父子 trace）。
pub fn new_correlation_id() -> CorrelationId {
    uuid::Uuid::new_v4().to_string()
}

/// 消息去重键：correlation_id + 消息种类 + payload 摘要（at-least-once 去重）。
/// 同一键重复出现 = 重复消息（可检测、可幂等丢弃）。
pub fn message_dedup_key(msg: &BusMessage) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    msg.correlation_id.hash(&mut hasher);
    (msg.kind as u8).hash(&mut hasher);
    if let Ok(bytes) = serde_json::to_vec(&msg.payload) {
        bytes.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// 保序去重：重复（同 correlation_id + 种类 + 载荷）消息只保留第一条。
pub fn dedupe_messages(msgs: &[BusMessage]) -> Vec<BusMessage> {
    let mut seen = std::collections::HashSet::new();
    msgs.iter()
        .filter(|msg| seen.insert(message_dedup_key(msg)))
        .cloned()
        .collect()
}

/// 把传输任务转为总线消息（Task 种类；bus_store 持久化/重放格式）。
/// 载荷携带 task_id/input/lineage，供断点恢复时按血缘重算。
pub fn transport_task_message(task: &TransportTask) -> BusMessage {
    BusMessage {
        id: 0,
        from: CONTROL_PLANE_AGENT.to_string(),
        to: task.worker.clone(),
        kind: MessageKind::Task,
        correlation_id: task.correlation_id.clone(),
        payload: serde_json::json!({
            "task_id": task.task_id,
            "input": task.input,
            "lineage": task.lineage,
        }),
    }
}

/// 控制面 agent 标识（总线 from 字段）。
pub const CONTROL_PLANE_AGENT: &str = "control-plane";

/// 节点注册消息（总线持久化/重放格式）：负载携带 node_id + CapabilityCard。
pub fn register_node_message(
    node_id: &str,
    card: &crate::capability::CapabilityCard,
) -> BusMessage {
    BusMessage {
        id: 0,
        from: CONTROL_PLANE_AGENT.to_string(),
        to: CONTROL_PLANE_AGENT.to_string(),
        kind: MessageKind::Task,
        correlation_id: format!("node:register:{node_id}"),
        payload: serde_json::json!({
            "node_id": node_id,
            "card": card,
        }),
    }
}

/// 任务经 transport 提交，关键消息先经总线持久化：
/// 失败/恢复语义沿用 BusStore 重放（`replay_store` 重新投递 + 接收方幂等去重）。
pub async fn submit_via_bus_and_transport(
    bus: &AgentBus,
    transport: &Arc<dyn FleetTransport>,
    task: TransportTask,
) -> Result<(), String> {
    let msg = transport_task_message(&task);
    bus.send(
        msg.from.clone(),
        msg.to.clone(),
        msg.kind,
        msg.correlation_id.clone(),
        msg.payload.clone(),
        OverflowPolicy::Reject,
    )
    .await
    .map_err(|e| e.to_string())?;
    transport.submit(task).await
}
