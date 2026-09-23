//! §12 回合执行域（turn）API 模块。
//!
//! 提取证明：自 `lib.rs` 逐字迁移 —— turn 编排器（附件注入/会话锁/并发许可/
//! 预算硬熔断/审批通道/SSE 事件流/trace 与用量归集）、respond_permission
//! （§5.4 审批响应 + Grant 临时授权）、ChannelApprover（Approver 实现，
//! 300s 截止 + 中止感知）、auto_approve_enabled、to_sse/to_event（协议 SSE
//! 映射，R10 统一携带 v 字段）。路由路径与 OpenAPI 登记零变化。
//!
//! `acquire_session_lock` 留守 lib.rs（session_api 八处引用的会话域共享基建）。
//! 本模块引用兄弟域（session_api/usage/audit_api/event_stream/logging）经
//! `crate::` 路径，故不可作 #[path] 独立编译目标（仅库内编译）。

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::stream;
use owo_agent_core::permissions::{Approver, Decision, PermissionRequest};
use owo_agent_protocol::{PermissionResponse, SseEvent, TurnRequest};
use serde::Deserialize;
use serde_json::{json, Value};

use owo_agent_server::{AppState, PendingApproval};

const TURN_EVENT_QUEUE_CAPACITY: usize = 128;
const TURN_EVENT_QUEUE_MAX_BYTES: usize = 1024 * 1024;
const TURN_EVENT_COALESCE_AT: usize = TURN_EVENT_QUEUE_CAPACITY / 2;

struct TurnEventQueue {
    state: Mutex<TurnEventQueueState>,
    notify: tokio::sync::Notify,
}

struct TurnEventQueueState {
    events: VecDeque<QueuedTurnEvent>,
    buffered_bytes: usize,
    closed: bool,
    overflowed: bool,
    overflow_notice_sent: bool,
    disconnect_recorded: bool,
}

#[derive(Debug)]
struct QueuedTurnEvent {
    seq: Option<u64>,
    event: SseEvent,
}

#[derive(Debug)]
enum TurnEventPushError {
    ConsumerGone,
    Full,
}

#[derive(Debug)]
enum TurnEventPop {
    Event(Box<QueuedTurnEvent>),
    Empty,
    Closed,
}

impl TurnEventQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(TurnEventQueueState {
                events: VecDeque::with_capacity(TURN_EVENT_QUEUE_CAPACITY),
                buffered_bytes: 0,
                closed: false,
                overflowed: false,
                overflow_notice_sent: false,
                disconnect_recorded: false,
            }),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn push(
        &self,
        seq: Option<u64>,
        event: SseEvent,
        receiver_alive: &Weak<()>,
    ) -> Result<(), TurnEventPushError> {
        if receiver_alive.upgrade().is_none() {
            if let Ok(mut state) = self.state.lock() {
                if !state.disconnect_recorded {
                    state.disconnect_recorded = true;
                    crate::observability_api::record_turn_sse_disconnect();
                }
            }
            return Err(TurnEventPushError::ConsumerGone);
        }

        let Ok(mut state) = self.state.lock() else {
            return Err(TurnEventPushError::Full);
        };
        if state.closed || state.overflowed {
            return Err(TurnEventPushError::Full);
        }

        if state.events.len() >= TURN_EVENT_COALESCE_AT {
            if let (
                Some(QueuedTurnEvent {
                    event: SseEvent::TokenDelta { delta: previous },
                    ..
                }),
                SseEvent::TokenDelta { delta },
            ) = (state.events.back(), &event)
            {
                let old_bytes = event_wire_size(&SseEvent::TokenDelta {
                    delta: previous.clone(),
                });
                let mut merged = String::with_capacity(previous.len() + delta.len());
                merged.push_str(previous);
                merged.push_str(delta);
                let merged_event = SseEvent::TokenDelta { delta: merged };
                let new_bytes = event_wire_size(&merged_event);
                let next_bytes = state
                    .buffered_bytes
                    .saturating_sub(old_bytes)
                    .saturating_add(new_bytes);
                if next_bytes > TURN_EVENT_QUEUE_MAX_BYTES {
                    state.overflowed = true;
                    crate::observability_api::record_turn_sse_slow_consumer();
                    drop(state);
                    self.notify.notify_one();
                    return Err(TurnEventPushError::Full);
                }
                if let Some(last) = state.events.back_mut() {
                    last.seq = seq;
                    last.event = merged_event;
                    state.buffered_bytes = next_bytes;
                }
                drop(state);
                self.notify.notify_one();
                return Ok(());
            }
        }

        let event_bytes = event_wire_size(&event);
        if state.events.len() >= TURN_EVENT_QUEUE_CAPACITY
            || state.buffered_bytes.saturating_add(event_bytes) > TURN_EVENT_QUEUE_MAX_BYTES
        {
            state.overflowed = true;
            crate::observability_api::record_turn_sse_slow_consumer();
            drop(state);
            self.notify.notify_one();
            return Err(TurnEventPushError::Full);
        }

        state.buffered_bytes += event_bytes;
        state.events.push_back(QueuedTurnEvent { seq, event });
        drop(state);
        self.notify.notify_one();
        Ok(())
    }

    fn pop(&self) -> TurnEventPop {
        let Ok(mut state) = self.state.lock() else {
            return TurnEventPop::Closed;
        };
        if let Some(event) = state.events.pop_front() {
            state.buffered_bytes = state
                .buffered_bytes
                .saturating_sub(event_wire_size(&event.event));
            return TurnEventPop::Event(Box::new(event));
        }
        if state.closed {
            if state.overflowed && !state.overflow_notice_sent {
                state.overflow_notice_sent = true;
                return TurnEventPop::Event(Box::new(QueuedTurnEvent {
                    seq: None,
                    event: SseEvent::Progress {
                        message:
                        "[turn/sse_slow_consumer] 回合因 SSE 客户端处理过慢而取消；本次流不支持续传，请从会话中重新发起"
                            .to_string(),
                    },
                }));
            }
            return TurnEventPop::Closed;
        }
        TurnEventPop::Empty
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        self.notify.notify_one();
    }
}

fn event_wire_size(event: &SseEvent) -> usize {
    serde_json::to_vec(event)
        .map(|value| value.len())
        .unwrap_or(usize::MAX)
}

fn persist_and_queue_event(
    store: &dyn owo_agent_core::SessionStore,
    session_id: &str,
    turn_id: &str,
    queue: &TurnEventQueue,
    receiver_alive: &Weak<()>,
    event: SseEvent,
) -> Result<(), String> {
    let record = store
        .append_turn_event(session_id, turn_id, &event)
        .map_err(|error| error.to_string())?;
    queue
        .push(Some(record.seq), event, receiver_alive)
        .map_err(|_| "回合 SSE 客户端已断开或队列已满".to_string())
}

fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

/// `GET /approval/mode`：当前全权限模式状态（界面据此显示开关）。
pub(super) async fn approval_mode_get() -> Result<Json<Value>, (StatusCode, String)> {
    Ok(Json(json!({
        "auto_approve": auto_approve_enabled(),
        "source": if runtime_auto_approve_path()
            .map(|path| path.is_file())
            .unwrap_or(false)
        {
            "runtime_file"
        } else {
            "environment"
        },
    })))
}

#[derive(serde::Deserialize)]
pub(super) struct ApprovalModeRequest {
    pub auto_approve: bool,
}

/// `POST /approval/mode`：运行时切换全权限模式。
///
/// 语义（单一事实源 = 开关文件）：
///   `true`  → 所有工具调用**自动放行**，不再弹审批；
///   `false` → 立即恢复逐次审批（无需重启）。
/// 这是用户显式要求的"全权限模式"；页面上必须同时给出可见的风险提示，
/// 并且**只影响审批网关**（密码/支付/验证码锚点熔断与 inject 级策略不受影响）。
pub(super) async fn approval_mode_set(
    Json(request): Json<ApprovalModeRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let Some(path) = runtime_auto_approve_path() else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法确定数据目录（OWO_AGENT_DATA 未设置），无法持久化全权限开关".to_string(),
        ));
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("创建数据目录失败：{error}")))?;
    }
    let payload = json!({
        "auto_approve": request.auto_approve,
        "updated_at": owo_agent_server::discovery::now_rfc3339(),
    });
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    std::fs::write(&path, text)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, format!("写入开关文件失败：{error}")))?;
    Ok(Json(json!({
        "ok": true,
        "auto_approve": auto_approve_enabled(),
        "path": path.to_string_lossy(),
    })))
}

/// 全权限模式开关文件（运行时可切换，不需要重启核心）。
///
/// 为什么做成文件而不是只认环境变量：用户要的是"在输入框下面随时改"。环境变量是
/// 进程启动期的快照，改它必须重启核心（会打断正在跑的回合）。这里约定：
///   `<data_root>/approval.json` = `{ "auto_approve": true }`
/// 每次审批请求都重新读一次——审批本身是低频事件，读一个小文件的开销可忽略，
/// 换来的是"界面点一下立刻生效、零重启"。
///
/// 安全边界（如实写在这里，避免以后有人误以为它是万能后门）：
///   * 只影响**审批网关**。密码/支付/验证码类锚点的熔断在 core 的策略层，不经过这里；
///   * `inject`（注入）级别动作仍然走策略层判定，不受本开关影响；
///   * 关闭时立即回到"逐次审批"，无需重启。
fn runtime_auto_approve_path() -> Option<std::path::PathBuf> {
    std::env::var_os("OWO_AGENT_DATA")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("approval.json"))
}

/// 全权限模式是否开启：**开关文件优先**（运行时），其次环境变量（部署级默认）。
pub(super) fn auto_approve_enabled() -> bool {
    if let Some(path) = runtime_auto_approve_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(flag) = value.get("auto_approve").and_then(|flag| flag.as_bool()) {
                    return flag;
                }
            }
        }
    }
    std::env::var("OWO_AUTO_APPROVE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

struct ChannelApprover {
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    pending_sessions: Arc<Mutex<HashMap<String, String>>>,
    session_id: String,
    abort: Arc<AtomicBool>,
}

impl ChannelApprover {
    fn spawn_request(
        &self,
        request: &PermissionRequest,
    ) -> tokio::sync::oneshot::Receiver<Decision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request.request_id.clone(), (tx, request.clone()));
        }
        if let Ok(mut sessions) = self.pending_sessions.lock() {
            sessions.insert(request.request_id.clone(), self.session_id.clone());
        }
        rx
    }
}

#[async_trait::async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        if auto_approve_enabled() {
            return Decision::Allow;
        }
        let rx = self.spawn_request(request);
        let mut rx = rx;
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(300));
        tokio::pin!(deadline);
        let decision = loop {
            tokio::select! {
                result = &mut rx => break result.unwrap_or(Decision::Deny),
                _ = &mut deadline => break Decision::Deny,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if self.abort.load(Ordering::Relaxed) {
                        break Decision::Deny;
                    }
                }
            }
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request.request_id);
        }
        if let Ok(mut sessions) = self.pending_sessions.lock() {
            sessions.remove(&request.request_id);
        }
        decision
    }
}

fn to_sse(event: &owo_agent_core::TurnEvent) -> Option<SseEvent> {
    match event {
        owo_agent_core::TurnEvent::ModelCall => Some(SseEvent::Progress {
            message: "模型调用".to_string(),
        }),
        owo_agent_core::TurnEvent::TokenDelta { delta } => Some(SseEvent::TokenDelta {
            delta: delta.clone(),
        }),
        owo_agent_core::TurnEvent::Compaction { summary } => Some(SseEvent::Compaction {
            summary: summary.clone(),
        }),
        owo_agent_core::TurnEvent::PermissionRequest(request) => {
            let explain = owo_agent_core::permissions::describe_request(request);
            Some(SseEvent::PermissionRequest {
                request_id: request.request_id.clone(),
                tool: request.tool.clone(),
                args: request.args.clone(),
                reason: request.reason.clone(),
                redacted_args: request.redacted_args.clone(),
                level: Some(request.level.label().to_string()),
                risk_note: request.risk_note.clone(),
                explain: Some(explain),
            })
        }
        owo_agent_core::TurnEvent::ToolStart { id, tool } => Some(SseEvent::ToolUse {
            id: id.clone(),
            tool: tool.clone(),
            args: Value::Null,
        }),
        owo_agent_core::TurnEvent::ToolResult {
            id,
            tool,
            ok,
            error,
        } => Some(SseEvent::ToolResult {
            id: id.clone(),
            tool: tool.clone(),
            ok: *ok,
            error: error.clone(),
        }),
        owo_agent_core::TurnEvent::Final { text } => Some(SseEvent::Final { text: text.clone() }),
    }
}

fn to_event(seq: Option<u64>, sse: SseEvent) -> Result<Event, Infallible> {
    let name = match &sse {
        SseEvent::Progress { .. } => "progress",
        SseEvent::ToolUse { .. } => "tool_use",
        SseEvent::ToolResult { .. } => "tool_result",
        SseEvent::PermissionRequest { .. } => "permission_request",
        SseEvent::Final { .. } => "final",
        SseEvent::TokenDelta { .. } => "token_delta",
        SseEvent::Compaction { .. } => "compaction",
    };
    // R10：SSE 事件统一携带协议版本 v（见 protocol::SSE_PROTOCOL_VERSION）。
    let mut payload = serde_json::to_value(&sse).unwrap_or_else(|_| json!({}));
    if let serde_json::Value::Object(map) = &mut payload {
        map.insert(
            "v".to_string(),
            json!(owo_agent_protocol::SSE_PROTOCOL_VERSION),
        );
    }
    let data = payload.to_string();
    let event = Event::default().event(name).data(data);
    Ok(match seq {
        Some(seq) => event.id(seq.to_string()),
        None => event,
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct TurnEventsQuery {
    turn_id: String,
    #[serde(default)]
    after_seq: u64,
    limit: Option<usize>,
}

fn is_turn_failed_event(event: &SseEvent) -> bool {
    matches!(event, SseEvent::Progress { message }
        if message.starts_with("turn failed:") || message.starts_with("session save failed:"))
}

/// GET /session/{id}/turn/events：从持久事件表按 session seq 补拉一页。
pub(super) async fn turn_events(
    State(state): State<Arc<AppState>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<TurnEventsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::session_api::load_session(&state, &session_id)?;
    let events = state
        .store
        .turn_events_after(
            &session_id,
            Some(&query.turn_id),
            query.after_seq,
            query.limit.unwrap_or(256),
        )
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let active_turn_id = state
        .active_turn_ids
        .lock()
        .map_err(poison)?
        .get(&session_id)
        .cloned();
    let replay_state = events
        .iter()
        .filter_map(|record| match &record.payload {
            SseEvent::Final { .. } => Some(owo_agent_protocol::TurnReplayState::Completed),
            event if is_turn_failed_event(event) => {
                Some(owo_agent_protocol::TurnReplayState::Failed)
            }
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| {
            if active_turn_id.as_deref() == Some(query.turn_id.as_str()) {
                owo_agent_protocol::TurnReplayState::Active
            } else {
                owo_agent_protocol::TurnReplayState::Interrupted
            }
        });
    let active = replay_state == owo_agent_protocol::TurnReplayState::Active;
    let next_after_seq = events
        .last()
        .map(|record| record.seq)
        .unwrap_or(query.after_seq);
    Ok(Json(json!({
        "events": events,
        "active": active,
        "state": replay_state,
        "next_after_seq": next_after_seq,
    })))
}

pub(super) async fn turn(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<TurnRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // §13 批次六：遥测功能计数（默认关时零开销早退；仅数字，无内容）。
    crate::observability_api::record_telemetry_counter("turn", 1);
    let session = crate::session_api::load_session(&state, &id)?;
    let mut effective_prompt = request.prompt.clone();
    if !request.attachments.is_empty() {
        let dir = crate::session_api::attachment_dir(&session.workspace, &id);
        let mut lines = Vec::new();
        for attachment in &request.attachments {
            let safe = Path::new(attachment)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(attachment);
            let path = dir.join(safe);
            if !path.is_file() {
                return Err((StatusCode::BAD_REQUEST, format!("附件不存在：{safe}")));
            }
            let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            lines.push(format!(
                "- {}（{} 字节，路径 {}）",
                safe,
                size,
                path.display()
            ));
        }
        effective_prompt.push_str("\n\n附件：\n");
        effective_prompt.push_str(&lines.join("\n"));
    }

    let turn_lock = {
        let mut locks = state.turn_locks.lock().map_err(poison)?;
        locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let turn_guard = turn_lock
        .try_lock_owned()
        .map_err(|_| (StatusCode::CONFLICT, "该会话已有回合正在运行".to_string()))?;
    // R8：全局并发 turn 上限 + 关闭中拒绝新回合。
    let concurrency_permit = state.shutdown_gate.try_acquire_turn().map_err(|busy| {
        // R10：错误码表接入（domain/reason/retryable 统一前缀，见 error_codes.rs）。
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("[gateway/unavailable/retryable] {busy}"),
        )
    })?;
    // R8/R9：用量预算硬熔断（Agent 4 交付 usage；超限停轮，错误码贯穿，请求用户加额后恢复）。
    if crate::usage::global().check_budget() {
        let reason = crate::usage::global()
            .hard_stop_reason()
            .unwrap_or_else(|| "用量预算超限".to_string());
        let (status, body) = crate::usage::budget_exceeded_response(&reason);
        let detail = body
            .0
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&reason)
            .to_string();
        return Err((
            status,
            format!("[{}] {detail}", crate::usage::BUDGET_ERROR_CODE),
        ));
    }

    let event_queue = Arc::new(TurnEventQueue::new());
    let turn_id = uuid::Uuid::new_v4().to_string();
    let receiver_alive = Arc::new(());
    let receiver_weak = Arc::downgrade(&receiver_alive);
    let abort_flag = {
        let mut aborts = state.aborts.lock().map_err(poison)?;
        aborts
            .entry(id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };
    state
        .active_turn_ids
        .lock()
        .map_err(poison)?
        .insert(id.clone(), turn_id.clone());
    abort_flag.store(false, Ordering::Relaxed);
    let approver = ChannelApprover {
        pending: Arc::clone(&state.pending_approvals),
        pending_sessions: Arc::clone(&state.pending_approval_sessions),
        session_id: id.clone(),
        abort: Arc::clone(&abort_flag),
    };

    let agent = Arc::clone(&state.agent);
    let store = Arc::clone(&state.store);
    let sessions = Arc::clone(&state.sessions);
    let traces_dir = state.traces_dir.clone();
    let state_for_audit = Arc::clone(&state);
    let producer_store = Arc::clone(&store);
    let producer_session_id = session.id.clone();
    let producer_turn_id = turn_id.clone();
    let trace_started = std::time::Instant::now();
    let trace_started_at = chrono::Utc::now().to_rfc3339();
    let trace_prompt = effective_prompt.clone();
    let stream_queue = Arc::clone(&event_queue);
    tokio::spawn(async move {
        let _turn_guard = turn_guard;
        let _concurrency_permit = concurrency_permit;
        let mut current = session;
        let stream_abort = Arc::clone(&abort_flag);
        let mut tool_starts: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        let producer_queue = Arc::clone(&event_queue);
        let producer_receiver = receiver_weak.clone();
        let producer_store = Arc::clone(&producer_store);
        let producer_session_id = producer_session_id.clone();
        let producer_turn_id = producer_turn_id.clone();
        let mut on_event = |event: &owo_agent_core::TurnEvent| {
            // §13 批次九：工具耗时埋点——ToolStart/ToolResult 以 id 配对，差值进
            // /metrics/runtime 的 tool_durations_ms 样本（超上限丢最旧，见 observability_api）。
            match event {
                owo_agent_core::TurnEvent::ToolStart { id, .. } => {
                    tool_starts.insert(id.clone(), std::time::Instant::now());
                }
                owo_agent_core::TurnEvent::ToolResult { id, .. } => {
                    if let Some(started) = tool_starts.remove(id) {
                        crate::observability_api::record_tool_duration_ms(
                            started.elapsed().as_millis() as u64,
                        );
                    }
                }
                _ => {}
            }
            if let Some(sse) = to_sse(event) {
                if persist_and_queue_event(
                    producer_store.as_ref(),
                    &producer_session_id,
                    &producer_turn_id,
                    &producer_queue,
                    &producer_receiver,
                    sse,
                )
                .is_err()
                {
                    // 客户端断开或有界队列溢出后，尽快停止无主/过载回合。
                    stream_abort.store(true, Ordering::Relaxed);
                }
            }
        };
        match agent
            .run_turn(
                &mut current,
                &effective_prompt,
                &approver,
                &abort_flag,
                &mut on_event,
            )
            .await
        {
            Ok(outcome) => {
                let trace = owo_agent_core::TraceRecord::from_outcome(&current, &outcome);
                let _ = owo_agent_core::save_trace(&traces_dir, &trace);
                // §3.2：trace 落盘后发布 traces 域失效（每个回合至多一次）。
                crate::event_stream::hub()
                    .publish_invalidate(crate::event_stream::InvalidateDomain::Traces);
                if outcome.usage.total_tokens > 0 {
                    let input_price = std::env::var("OWO_MODEL_INPUT_PRICE_PER_MTOK")
                        .ok()
                        .and_then(|value| value.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let output_price = std::env::var("OWO_MODEL_OUTPUT_PRICE_PER_MTOK")
                        .ok()
                        .and_then(|value| value.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let cost = outcome.usage.cost_estimate_usd(input_price, output_price);
                    if let Ok(mut audit) = state_for_audit.agent.audit_log().lock() {
                        audit.record(
                            "model",
                            "usage",
                            Some(current.id.clone()),
                            Some(true),
                            format!(
                                "prompt={} completion={} total={} cost_usd≈{:.6}",
                                outcome.usage.prompt_tokens,
                                outcome.usage.completion_tokens,
                                outcome.usage.total_tokens,
                                cost
                            ),
                        );
                    }
                    // R8：用量与成本归集（Agent 4 交付 usage_router 的会话维度记录）。
                    crate::usage::global().record_tokens(
                        crate::usage::UsageDimension::Session,
                        &current.id,
                        Some(&current.id),
                        outcome.usage.prompt_tokens,
                        outcome.usage.completion_tokens,
                    );
                }
            }
            Err(error) => {
                let error_text = error.to_string();
                crate::logging::error(
                    "agent",
                    None,
                    "回合执行失败",
                    &[("session_id", serde_json::json!(current.id))],
                );
                let trace = owo_agent_core::TraceRecord::from_error(
                    &current,
                    &trace_prompt,
                    &trace_started_at,
                    trace_started.elapsed().as_millis() as u64,
                    &error_text,
                );
                let _ = owo_agent_core::save_trace(&traces_dir, &trace);
                crate::event_stream::hub()
                    .publish_invalidate(crate::event_stream::InvalidateDomain::Traces);
                let _ = persist_and_queue_event(
                    producer_store.as_ref(),
                    &producer_session_id,
                    &producer_turn_id,
                    &producer_queue,
                    &producer_receiver,
                    SseEvent::Progress {
                        message: format!("turn failed: {error_text}"),
                    },
                );
            }
        }
        if let Ok(mut sessions) = sessions.lock() {
            sessions.insert(current.id.clone(), current.clone());
        }
        if let Err(error) = store.save(&current) {
            let _ = persist_and_queue_event(
                producer_store.as_ref(),
                &producer_session_id,
                &producer_turn_id,
                &producer_queue,
                &producer_receiver,
                SseEvent::Progress {
                    message: format!("session save failed: {error}"),
                },
            );
        }
        if let Ok(mut aborts) = state_for_audit.aborts.lock() {
            if aborts
                .get(&current.id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &abort_flag))
            {
                aborts.remove(&current.id);
            }
        }
        if let Ok(mut active_turn_ids) = state_for_audit.active_turn_ids.lock() {
            if active_turn_ids.get(&current.id) == Some(&producer_turn_id) {
                active_turn_ids.remove(&current.id);
            }
        }
        crate::audit_api::flush_audit(&state_for_audit);
        producer_queue.close();
    });

    let event_stream = stream::unfold(
        (stream_queue, receiver_alive),
        |(queue, receiver_alive)| async move {
            loop {
                match queue.pop() {
                    TurnEventPop::Event(event) => {
                        return Some((
                            to_event(event.seq, event.event),
                            (Arc::clone(&queue), receiver_alive),
                        ));
                    }
                    TurnEventPop::Empty => queue.notify.notified().await,
                    TurnEventPop::Closed => return None,
                }
            }
        },
    );
    let mut response = Sse::new(event_stream).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&turn_id) {
        response.headers_mut().insert("x-owo-turn-id", value);
    }
    Ok(response)
}

pub(super) async fn respond_permission(
    State(state): State<Arc<AppState>>,
    AxumPath((session_id, request_id)): AxumPath<(String, String)>,
    Json(response): Json<PermissionResponse>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let belongs_to_session = state
        .pending_approval_sessions
        .lock()
        .map_err(poison)?
        .get(&request_id)
        .map(|pending_session| pending_session == &session_id)
        .unwrap_or(false);
    if !belongs_to_session {
        return Err((
            StatusCode::NOT_FOUND,
            format!("审批请求不存在：{request_id}"),
        ));
    }
    let (sender, request) = state
        .pending_approvals
        .lock()
        .map_err(poison)?
        .remove(&request_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("审批请求不存在：{request_id}"),
            )
        })?;
    state
        .pending_approval_sessions
        .lock()
        .map_err(poison)?
        .remove(&request_id);
    let decision = if response.allow {
        // §5.4 审批选项 → 临时授权（Grant）：响应后同工作区同参数不再弹卡。
        // 破坏性/注入请求不允许生成 grant（scope 一律忽略，仅放行本次）。
        if let Some(scope) = response.scope.as_deref() {
            let level_ok = request.level != owo_agent_core::permissions::Level::Inject;
            if level_ok {
                if let Some(scope_enum) = owo_agent_core::grant_store::GrantScope::parse(scope) {
                    if let Some(grant) =
                        state
                            .grants
                            .grant_from_scope(&request, &state.workspace_id(), scope_enum)
                    {
                        state.grants.insert(grant);
                    }
                }
            }
        }
        Decision::Allow
    } else {
        Decision::Deny
    };
    sender
        .send(decision)
        .map_err(|_| (StatusCode::GONE, "审批通道已关闭".to_string()))?;
    Ok(Json(json!({ "ok": true, "granted": response.allow })))
}

#[cfg(test)]
mod turn_event_queue_tests {
    use super::*;

    fn queue_and_receiver() -> (TurnEventQueue, Arc<()>, Weak<()>) {
        let receiver = Arc::new(());
        let receiver_weak = Arc::downgrade(&receiver);
        (TurnEventQueue::new(), receiver, receiver_weak)
    }

    #[test]
    fn coalesces_adjacent_token_deltas_without_changing_text_order() {
        let (queue, _receiver, receiver_weak) = queue_and_receiver();
        let expected = "x".repeat(TURN_EVENT_COALESCE_AT + 17);
        for seq in 1..=(TURN_EVENT_COALESCE_AT + 17) {
            queue
                .push(
                    Some(seq as u64),
                    SseEvent::TokenDelta {
                        delta: "x".to_string(),
                    },
                    &receiver_weak,
                )
                .expect("queue has room and a live receiver");
        }
        {
            let state = queue.state.lock().expect("queue lock");
            assert_eq!(state.events.len(), TURN_EVENT_COALESCE_AT);
            assert!(state.buffered_bytes <= TURN_EVENT_QUEUE_MAX_BYTES);
        }

        queue.close();
        let mut actual = String::new();
        loop {
            match queue.pop() {
                TurnEventPop::Event(event) => match event.event {
                    SseEvent::TokenDelta { delta } => actual.push_str(&delta),
                    unexpected => panic!("unexpected event: {unexpected:?}"),
                },
                TurnEventPop::Closed => break,
                other => panic!("unexpected queue result: {other:?}"),
            }
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn queue_capacity_is_hard_bounded_and_overflow_is_reported_after_drain() {
        let slow_before = crate::observability_api::turn_sse_counts_for_test().0;
        let (queue, _receiver, receiver_weak) = queue_and_receiver();
        for index in 0..TURN_EVENT_QUEUE_CAPACITY {
            queue
                .push(
                    Some(index as u64 + 1),
                    SseEvent::Progress {
                        message: format!("event-{index}"),
                    },
                    &receiver_weak,
                )
                .expect("the queue accepts up to its configured capacity");
        }
        assert!(matches!(
            queue.push(
                Some(TURN_EVENT_QUEUE_CAPACITY as u64 + 1),
                SseEvent::Progress {
                    message: "overflow".to_string(),
                },
                &receiver_weak,
            ),
            Err(TurnEventPushError::Full)
        ));
        {
            let state = queue.state.lock().expect("queue lock");
            assert_eq!(state.events.len(), TURN_EVENT_QUEUE_CAPACITY);
            assert!(state.buffered_bytes <= TURN_EVENT_QUEUE_MAX_BYTES);
            assert!(state.overflowed);
        }

        queue.close();
        for _ in 0..TURN_EVENT_QUEUE_CAPACITY {
            assert!(matches!(queue.pop(), TurnEventPop::Event(_)));
        }
        match queue.pop() {
            TurnEventPop::Event(event) => match event.event {
                SseEvent::Progress { message } => {
                    assert!(message.starts_with("[turn/sse_slow_consumer]"));
                }
                unexpected => panic!("expected progress event, got {unexpected:?}"),
            },
            other => panic!("expected slow-consumer terminal event, got {other:?}"),
        }
        assert!(matches!(queue.pop(), TurnEventPop::Closed));
        assert!(
            crate::observability_api::turn_sse_counts_for_test().0 > slow_before,
            "bounded queue overflow should increment the slow-consumer counter"
        );
    }

    #[test]
    fn oversized_event_is_rejected_without_exceeding_byte_limit() {
        let (queue, _receiver, receiver_weak) = queue_and_receiver();
        assert!(matches!(
            queue.push(
                Some(1),
                SseEvent::Progress {
                    message: "x".repeat(TURN_EVENT_QUEUE_MAX_BYTES + 1),
                },
                &receiver_weak,
            ),
            Err(TurnEventPushError::Full)
        ));
        let state = queue.state.lock().expect("queue lock");
        assert!(state.events.is_empty());
        assert_eq!(state.buffered_bytes, 0);
        assert!(state.overflowed);
    }

    #[test]
    fn dropped_receiver_stops_accepting_events() {
        let disconnects_before = crate::observability_api::turn_sse_counts_for_test().1;
        let queue = TurnEventQueue::new();
        let receiver = Arc::new(());
        let receiver_weak = Arc::downgrade(&receiver);
        drop(receiver);

        assert!(matches!(
            queue.push(
                Some(1),
                SseEvent::TokenDelta {
                    delta: "not delivered".to_string(),
                },
                &receiver_weak,
            ),
            Err(TurnEventPushError::ConsumerGone)
        ));
        assert!(matches!(queue.pop(), TurnEventPop::Empty));
        assert!(matches!(
            queue.push(
                Some(2),
                SseEvent::TokenDelta {
                    delta: "still gone".to_string(),
                },
                &receiver_weak,
            ),
            Err(TurnEventPushError::ConsumerGone)
        ));
        assert_eq!(
            crate::observability_api::turn_sse_counts_for_test().1,
            disconnects_before + 1,
            "repeated producer callbacks should count one disconnect per turn stream"
        );
    }
}
