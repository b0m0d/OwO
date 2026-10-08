//! 云端任务进度 SSE 集线器（Lane D Part 2）。
//!
//! - [`CloudSseHub`]：按 task_id 一个有界广播 ring + 有界带序号事件历史（支持 Last-Event-ID 续传）。
//! - [`hub()`]：模块内 `OnceLock` 单例。
//! - [`SseHubSink`]：`owo_agent_core::cloud_exec::ProgressSink` 适配器，把
//!   `CloudProgress` 各变体序列化为 JSON 帧发布到 hub。
//! - [`router`]：`GET /cloud/tasks/{id}/events` → text/event-stream（重放历史 + 实时流）。
//!
//! 接线说明（供主控）：在 `lib.rs::cloud_task_submit` 中用 `sse::sink(task_id.clone())`
//! 作为 `run_next` 的 ProgressSink；并把 `sse::router(state)` 合并进 `build_router`。
//! 本模块不引用 `crate::`/`super::`，可被测试以 `#[path] mod` 独立编译。

use axum::extract::Path;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::Router;
use owo_agent_core::cloud_exec::{CloudProgress, ProgressSink};
use owo_agent_server::AppState;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// SSE HTTP 转发队列上限；防止慢客户端让转发任务无限积压。
const SSE_FORWARD_QUEUE_CAPACITY: usize = 64;
/// Per-task live event ring; lagging HTTP consumers are told to resubscribe.
const TASK_LIVE_QUEUE_CAPACITY: usize = 256;

/// Completed-task replay cache bounds retained task histories and sender state.
pub const COMPLETED_TASK_CACHE_CAPACITY: usize = 128;
/// Keep completed task progress available for late subscribers for at most 30 minutes.
const COMPLETED_TASK_RETENTION: Duration = Duration::from_secs(30 * 60);
/// Bound replay history per task by both event count and bytes.
const TASK_HISTORY_CAPACITY: usize = 512;
const TASK_HISTORY_MAX_BYTES: usize = 256 * 1024;
/// A single progress frame must not retain or broadcast an arbitrarily large remote error.
const MAX_PROGRESS_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudSseFrame {
    pub sequence: u64,
    pub payload: String,
}

pub struct CloudEventSubscription {
    pub receiver: broadcast::Receiver<CloudSseFrame>,
    pub history: Vec<CloudSseFrame>,
    pub known_task: bool,
    pub missing_history_events: u64,
    pub completed: bool,
}

struct TaskEventBuffer {
    sender: Option<broadcast::Sender<CloudSseFrame>>,
    history: VecDeque<CloudSseFrame>,
    history_bytes: usize,
    next_sequence: u64,
    completed_at: Option<Instant>,
    completion_order: Option<u64>,
}

fn new_task_event_buffer() -> TaskEventBuffer {
    TaskEventBuffer {
        sender: Some(broadcast::channel(TASK_LIVE_QUEUE_CAPACITY).0),
        history: VecDeque::new(),
        history_bytes: 0,
        next_sequence: 0,
        completed_at: None,
        completion_order: None,
    }
}

#[derive(Default)]
struct CloudSseState {
    tasks: HashMap<String, TaskEventBuffer>,
    next_completion_order: u64,
}

/// SSE 集线器：为活动任务提供实时广播，并仅保留有界的近期终态重放历史。
pub struct CloudSseHub {
    state: Mutex<CloudSseState>,
}

impl CloudSseHub {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CloudSseState::default()),
        }
    }

    fn prune_completed(state: &mut CloudSseState, now: Instant) {
        state.tasks.retain(|_, task| {
            task.completed_at
                .map(|completed_at| now.duration_since(completed_at) < COMPLETED_TASK_RETENTION)
                .unwrap_or(true)
        });
        let mut completed = state
            .tasks
            .iter()
            .filter_map(|(task_id, task)| {
                task.completion_order.map(|order| (order, task_id.clone()))
            })
            .collect::<Vec<_>>();
        if completed.len() > COMPLETED_TASK_CACHE_CAPACITY {
            completed.sort_unstable_by_key(|(order, _)| *order);
            let evict = completed.len() - COMPLETED_TASK_CACHE_CAPACITY;
            for (_, task_id) in completed.into_iter().take(evict) {
                state.tasks.remove(&task_id);
            }
        }
    }

    /// Register a persisted task before its HTTP submit response exposes the task ID.
    /// This lets clients subscribe while the runner is still being scheduled.
    pub fn prepare_task(&self, task_id: impl Into<String>) {
        let task_id = task_id.into();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_completed(&mut state, Instant::now());
        let task = state
            .tasks
            .entry(task_id)
            .or_insert_with(new_task_event_buffer);
        if task.completed_at.take().is_some() {
            task.completion_order = None;
            task.history.clear();
            task.history_bytes = 0;
            task.sender = Some(broadcast::channel(TASK_LIVE_QUEUE_CAPACITY).0);
        }
    }

    fn mark_completed_locked(state: &mut CloudSseState, task_id: &str, now: Instant) {
        let needs_order = state
            .tasks
            .get(task_id)
            .map(|task| task.completed_at.is_none())
            .unwrap_or(false);
        let completion_order = if needs_order {
            state.next_completion_order = state.next_completion_order.saturating_add(1);
            Some(state.next_completion_order)
        } else {
            None
        };
        if let Some(task) = state.tasks.get_mut(task_id) {
            if let Some(order) = completion_order {
                task.completion_order = Some(order);
                task.completed_at = Some(now);
            }
            // Stop retaining a broadcast sender after completion; active SSE receivers then close.
            task.sender.take();
        }
    }

    /// 订阅时在同一锁内挂接实时流并读取 cursor 之后的历史，消除历史/实时竞态。
    /// 返回 known=false 表示 task_id 未曾发布过事件，避免客户端无限重连错误 ID。
    pub fn subscribe_after(
        &self,
        task_id: &str,
        last_event_id: Option<u64>,
    ) -> CloudEventSubscription {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_completed(&mut state, Instant::now());
        let Some(task) = state.tasks.get_mut(task_id) else {
            let (sender, receiver) = broadcast::channel(1);
            drop(sender);
            return CloudEventSubscription {
                receiver,
                history: Vec::new(),
                known_task: false,
                missing_history_events: 0,
                completed: false,
            };
        };
        let receiver = if let Some(sender) = task.sender.as_ref() {
            sender.subscribe()
        } else {
            let (sender, receiver) = broadcast::channel(1);
            drop(sender);
            receiver
        };
        let history = task
            .history
            .iter()
            .filter(|frame| last_event_id.is_none_or(|cursor| frame.sequence > cursor))
            .cloned()
            .collect::<Vec<_>>();
        let mut expected = last_event_id.unwrap_or(0).saturating_add(1);
        let mut missing_history_events = 0_u64;
        for frame in &history {
            if frame.sequence > expected {
                missing_history_events =
                    missing_history_events.saturating_add(frame.sequence.saturating_sub(expected));
            }
            expected = frame.sequence.saturating_add(1);
        }
        if expected <= task.next_sequence {
            missing_history_events = missing_history_events.saturating_add(
                task.next_sequence
                    .saturating_sub(expected)
                    .saturating_add(1),
            );
        }
        CloudEventSubscription {
            receiver,
            history,
            known_task: true,
            missing_history_events,
            completed: task.completed_at.is_some(),
        }
    }

    /// 订阅完整保留历史；主要供诊断及兼容性测试使用。
    pub fn subscribe(
        &self,
        task_id: &str,
    ) -> (broadcast::Receiver<CloudSseFrame>, Vec<CloudSseFrame>) {
        let subscription = self.subscribe_after(task_id, None);
        (subscription.receiver, subscription.history)
    }

    /// 发布事件帧（历史追加 + 广播）。返回订阅者数；超限帧不进入内存缓存或广播。
    pub fn publish(&self, task_id: &str, payload: String) -> usize {
        if payload.len() > MAX_PROGRESS_FRAME_BYTES {
            return 0;
        }
        let now = Instant::now();
        let terminal = is_terminal_progress_frame(&payload);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_completed(&mut state, now);
        let subscribers = {
            let task = state
                .tasks
                .entry(task_id.to_string())
                .or_insert_with(new_task_event_buffer);

            // A retry on the same task reopens its live stream and resets terminal retention.
            if !terminal {
                if task.completed_at.take().is_some() {
                    task.history.clear();
                    task.history_bytes = 0;
                }
                task.completion_order = None;
                if task.sender.is_none() {
                    task.sender = Some(broadcast::channel(TASK_LIVE_QUEUE_CAPACITY).0);
                }
            }
            task.next_sequence = task.next_sequence.saturating_add(1);
            let frame = CloudSseFrame {
                sequence: task.next_sequence,
                payload,
            };
            if frame.payload.len() <= TASK_HISTORY_MAX_BYTES {
                while task.history.len() >= TASK_HISTORY_CAPACITY
                    || task.history_bytes.saturating_add(frame.payload.len())
                        > TASK_HISTORY_MAX_BYTES
                {
                    let Some(oldest) = task.history.pop_front() else {
                        break;
                    };
                    task.history_bytes = task.history_bytes.saturating_sub(oldest.payload.len());
                }
                task.history_bytes = task.history_bytes.saturating_add(frame.payload.len());
                task.history.push_back(frame.clone());
            }
            let subscribers = task
                .sender
                .as_ref()
                .map(|sender| sender.send(frame).unwrap_or(0))
                .unwrap_or(0);
            if terminal {
                task.sender.take();
            }
            subscribers
        };
        if terminal {
            Self::mark_completed_locked(&mut state, task_id, now);
            Self::prune_completed(&mut state, now);
        }
        subscribers
    }

    /// 将取消或其他无 CloudProgress 终态事件的任务标记为已结束，允许缓存逐步回收。
    pub fn mark_completed(&self, task_id: &str) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::mark_completed_locked(&mut state, task_id, now);
        Self::prune_completed(&mut state, now);
    }

    /// 读取历史（供测试/审计）。
    #[allow(dead_code)] // 仅供 cloud_sse_tests 以 #[path] 独立编译使用；lib 目标内无引用。
    pub fn history(&self, task_id: &str) -> Vec<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_completed(&mut state, Instant::now());
        state
            .tasks
            .get(task_id)
            .map(|task| {
                task.history
                    .iter()
                    .map(|frame| frame.payload.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}
impl Default for CloudSseHub {
    fn default() -> Self {
        Self::new()
    }
}

fn is_terminal_progress_frame(payload: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("kind")
                .or_else(|| value.get("event"))
                .and_then(serde_json::Value::as_str)
                .map(|kind| matches!(kind, "succeeded" | "failed" | "canceled"))
        })
        .unwrap_or(false)
}

/// 全局单例集线器。
static HUB: OnceLock<Arc<CloudSseHub>> = OnceLock::new();

pub fn hub() -> &'static Arc<CloudSseHub> {
    HUB.get_or_init(|| Arc::new(CloudSseHub::new()))
}

/// 把 `CloudProgress` 变体序列化为 JSON 帧（event 名 + 变体字段）。
pub fn progress_frame(event: &CloudProgress) -> String {
    let (kind, payload) = match event {
        CloudProgress::Snapshotting { task_id } => ("snapshotting", json!({ "task_id": task_id })),
        CloudProgress::Submitting { task_id } => ("submitting", json!({ "task_id": task_id })),
        CloudProgress::Submitted { task_id, remote_id } => (
            "submitted",
            json!({ "task_id": task_id, "remote_id": bounded_progress_text(remote_id, 4096) }),
        ),
        CloudProgress::Executing { task_id } => ("executing", json!({ "task_id": task_id })),
        CloudProgress::Fetching { task_id } => ("fetching", json!({ "task_id": task_id })),
        CloudProgress::Retrying {
            task_id,
            retry_count,
        } => (
            "retrying",
            json!({ "task_id": task_id, "retry_count": retry_count }),
        ),
        CloudProgress::Succeeded {
            task_id,
            diff_count,
        } => (
            "succeeded",
            json!({ "task_id": task_id, "diff_count": diff_count }),
        ),
        CloudProgress::Failed { task_id, error } => (
            "failed",
            json!({ "task_id": task_id, "error": bounded_progress_text(error, 4096) }),
        ),
        CloudProgress::Canceled { task_id } => ("canceled", json!({ "task_id": task_id })),
    };
    let mut frame = payload;
    frame["event"] = json!(kind);
    frame["kind"] = json!(kind);
    frame.to_string()
}

fn bounded_progress_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut bounded = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        bounded.push_str("…（内容过长，已截断）");
    }
    bounded
}

/// ProgressSink 适配器：emit → hub.publish（历史 + 广播）。
#[derive(Clone)]
pub struct SseHubSink {
    task_id: String,
    hub: Arc<CloudSseHub>,
}

impl SseHubSink {
    pub fn new(task_id: impl Into<String>) -> Self {
        let task_id = task_id.into();
        let hub = Arc::clone(hub());
        hub.prepare_task(task_id.clone());
        Self { task_id, hub }
    }
}

impl ProgressSink for SseHubSink {
    fn emit(&self, event: &CloudProgress) {
        self.hub.publish(&self.task_id, progress_frame(event));
    }
}

/// 为 task_id 构造 sink（主控在 cloud_task_submit 中使用）。
pub fn sink(task_id: impl Into<String>) -> SseHubSink {
    SseHubSink::new(task_id)
}

/// 无 ID 的 progress 帧文本，用于测试事件名与数据行；实际路由还会输出 sequence `id:`。
#[allow(dead_code)] // 仅供 cloud_sse_tests 以 #[path] 独立编译使用。
pub fn sse_frame_text(frame: &str) -> String {
    format!("event: progress\ndata: {frame}\n\n")
}

/// SSE 事件流端点：`GET /cloud/tasks/{id}/events`。
async fn cloud_task_events(
    Path(task_id): Path<String>,
    headers: HeaderMap,
) -> Sse<ReceiverStream<Result<Event, Infallible>>> {
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    let subscription = hub().subscribe_after(&task_id, last_event_id);
    let receiver = subscription.receiver;
    let history = subscription.history;
    let known = subscription.known_task;
    let missing_history_events = subscription.missing_history_events;
    let completed = subscription.completed;
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(SSE_FORWARD_QUEUE_CAPACITY);

    tokio::spawn(async move {
        if !known {
            let missing = json!({
                "kind": "task_not_found",
                "task_id": task_id,
                "message": "没有找到该任务的进度记录，请核对任务编号。"
            })
            .to_string();
            let _ = tx
                .send(Ok(Event::default().event("progress").data(missing)))
                .await;
            return;
        }
        let mut last_sent_sequence = last_event_id.unwrap_or(0);
        if missing_history_events > 0 {
            let gap = json!({
                "kind": "stream_gap",
                "task_id": task_id,
                "skipped_events": missing_history_events,
                "message": "部分进度已超出服务端历史保留范围；以下续传仅包含当前保留记录，最终状态请核对任务结果。"
            })
            .to_string();
            if tx
                .send(Ok(Event::default()
                    .event("progress")
                    .id(last_sent_sequence.to_string())
                    .data(gap)))
                .await
                .is_err()
            {
                return;
            }
        }
        // 1) 重放 cursor 之后的保留事件；终态帧后直接关闭 SSE。
        for frame in history {
            let CloudSseFrame { sequence, payload } = frame;
            let terminal = is_terminal_progress_frame(&payload);
            if tx
                .send(Ok(Event::default()
                    .event("progress")
                    .id(sequence.to_string())
                    .data(payload)))
                .await
                .is_err()
            {
                return;
            }
            last_sent_sequence = sequence;
            if terminal {
                return;
            }
        }
        if completed {
            let complete = json!({
                "kind": "stream_complete",
                "task_id": task_id,
                "message": "任务已结束；如需确认最终结果，请查询任务状态与结果。"
            })
            .to_string();
            let _ = tx
                .send(Ok(Event::default()
                    .event("progress")
                    .id(last_sent_sequence.to_string())
                    .data(complete)))
                .await;
            return;
        }
        // 2) 实时流。
        let mut receiver = receiver;
        loop {
            let received = tokio::select! {
                _ = tx.closed() => break,
                received = receiver.recv() => received,
            };
            match received {
                Ok(frame) => {
                    let CloudSseFrame { sequence, payload } = frame;
                    let terminal = is_terminal_progress_frame(&payload);
                    if tx
                        .send(Ok(Event::default()
                            .event("progress")
                            .id(sequence.to_string())
                            .data(payload)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    last_sent_sequence = sequence;
                    if terminal {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let gap = json!({
                        "kind": "stream_gap",
                        "task_id": task_id,
                        "skipped_events": skipped,
                        "message": "实时进度因订阅处理过慢发生丢帧，正在按事件序号续传保留的近期历史；最终状态请核对任务结果。"
                    })
                    .to_string();
                    let _ = tx
                        .send(Ok(Event::default()
                            .event("progress")
                            .id(last_sent_sequence.to_string())
                            .data(gap)))
                        .await;
                    // EventSource reconnects with this cursor and the route replays retained events.
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    Sse::new(ReceiverStream::new(rx))
}

/// Lane D Part 2 路由：/cloud/tasks/{id}/events（供主控并入 build_router）。
pub fn router(_state: Arc<AppState>) -> Router {
    Router::new().route(
        "/cloud/tasks/{id}/events",
        axum::routing::get(cloud_task_events),
    )
}

/// 供依赖方测试/调试：把 hub 重置为全新实例（仅测试进程内调用）。
#[allow(dead_code)] // 仅供 cloud_sse_tests 以 #[path] 独立编译使用。
pub fn reset_hub_for_test() {
    let _ = HUB.set(Arc::new(CloudSseHub::new()));
}

/// 健康检查辅助（供测试断言响应形态）。
#[allow(dead_code)] // 仅供 cloud_sse_tests 以 #[path] 独立编译使用。
pub fn sse_response_ok(response: &axum::response::Response) -> bool {
    response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false)
}

// 占位：确保 IntoResponse 路径在独立编译测试中类型完整。
#[allow(dead_code)]
fn _type_probe(response: axum::response::Response) -> impl IntoResponse {
    response
}
