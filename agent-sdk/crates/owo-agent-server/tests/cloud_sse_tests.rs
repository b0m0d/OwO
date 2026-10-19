//! Lane D Part 2 契约测试：云端 SSE 进度集线器。
//!
//! 覆盖：hub 发布/订阅（历史重放 + 实时流）、ProgressSink 适配器与 CollectingSink
//! 事件序列一致、SSE 端点（text/event-stream + 首帧 event:/data: 语义）。

#[path = "../src/sse.rs"]
mod sse;

use owo_agent_core::cloud_exec::{CloudProgress, CollectingSink, ProgressSink};
use owo_agent_server::AppState;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

struct IdleProvider;

#[async_trait::async_trait]
impl owo_agent_core::gateway::ModelProvider for IdleProvider {
    async fn complete(
        &self,
        _messages: &[owo_agent_core::ChatMessage],
        _tools: &[owo_agent_core::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        Err("IdleProvider 不应被调用".to_string())
    }
}

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = owo_agent_core::Agent::new(
        Arc::new(IdleProvider),
        owo_agent_core::tools::ToolRegistry::new(),
        owo_agent_core::permissions::Policy::new(&workspace),
        Default::default(),
    );
    let store = owo_agent_core::sqlite_store::SqliteSessionStore::open(&workspace.join("index.db"))
        .unwrap();
    let state = Arc::new(AppState::new(
        agent,
        store,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    (state, temp)
}

fn all_progress_events(task_id: &str) -> Vec<CloudProgress> {
    vec![
        CloudProgress::Snapshotting {
            task_id: task_id.into(),
        },
        CloudProgress::Submitting {
            task_id: task_id.into(),
        },
        CloudProgress::Submitted {
            task_id: task_id.into(),
            remote_id: "remote-1".into(),
        },
        CloudProgress::Executing {
            task_id: task_id.into(),
        },
        CloudProgress::Fetching {
            task_id: task_id.into(),
        },
        CloudProgress::Retrying {
            task_id: task_id.into(),
            retry_count: 2,
        },
        CloudProgress::Succeeded {
            task_id: task_id.into(),
            diff_count: 3,
        },
        CloudProgress::Failed {
            task_id: task_id.into(),
            error: "boom".into(),
        },
        CloudProgress::Canceled {
            task_id: task_id.into(),
        },
    ]
}

#[tokio::test]
async fn hub_publish_replays_history_then_streams() {
    sse::reset_hub_for_test();
    let hub = sse::hub();
    hub.publish("task-hist", "{\"event\":\"submitted\"}".to_string());
    hub.publish("task-hist", "{\"event\":\"executing\"}".to_string());

    // 订阅先重放历史。
    let (mut receiver, history) = hub.subscribe("task-hist");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].payload, "{\"event\":\"submitted\"}");
    assert_eq!((history[0].sequence, history[1].sequence), (1, 2));

    // 再实时收到后续帧。
    hub.publish("task-hist", "{\"event\":\"succeeded\"}".to_string());
    let frame = tokio::time::timeout(std::time::Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.payload, "{\"event\":\"succeeded\"}");
    assert_eq!(frame.sequence, 3);
    assert_eq!(hub.history("task-hist").len(), 3);
}

#[tokio::test]
async fn sink_frames_match_collecting_sink_sequence() {
    sse::reset_hub_for_test();
    let task_id = "task-seq";
    let sink = sse::sink(task_id);
    let collecting = CollectingSink::new();

    let events = all_progress_events(task_id);
    for event in &events {
        collecting.emit(event);
        sink.emit(event);
    }

    let collected: Vec<Value> = collecting
        .all()
        .iter()
        .map(|e| serde_json::from_str(&sse::progress_frame(e)).unwrap())
        .collect();
    let hub_frames: Vec<Value> = sse::hub()
        .history(task_id)
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect();

    assert_eq!(hub_frames.len(), collected.len(), "帧数一致");
    for (hub_frame, collected_frame) in hub_frames.iter().zip(collected.iter()) {
        assert_eq!(hub_frame["kind"], collected_frame["kind"]);
        assert_eq!(hub_frame["task_id"], collected_frame["task_id"]);
    }
    // 序列顺序一致（Submitted 带 remote_id、Retrying 带 retry_count）。
    let kinds: Vec<&str> = hub_frames
        .iter()
        .filter_map(|f| f["kind"].as_str())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "snapshotting",
            "submitting",
            "submitted",
            "executing",
            "fetching",
            "retrying",
            "succeeded",
            "failed",
            "canceled"
        ]
    );
    assert_eq!(hub_frames[2]["remote_id"], "remote-1");
    assert_eq!(hub_frames[5]["retry_count"], 2);
    assert_eq!(hub_frames[7]["error"], "boom");
}

#[tokio::test]
async fn sink_emits_json_with_event_kind_fields() {
    sse::reset_hub_for_test();
    let task_id = "task-frame";
    let sink = sse::sink(task_id);
    sink.emit(&CloudProgress::Succeeded {
        task_id: task_id.into(),
        diff_count: 5,
    });
    let frames = sse::hub().history(task_id);
    assert_eq!(frames.len(), 1);
    let frame: Value = serde_json::from_str(&frames[0]).unwrap();
    assert_eq!(frame["event"], "succeeded");
    assert_eq!(frame["kind"], "succeeded");
    assert_eq!(frame["diff_count"], 5);
}

#[tokio::test]
async fn events_endpoint_returns_event_stream_content_type() {
    let (state, _temp) = test_state().await;
    let app = sse::router(state);
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/cloud/tasks/task-http/events")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.contains("text/event-stream"),
        "Content-Type 应为 text/event-stream：{content_type}"
    );
    assert!(sse::sse_response_ok(&response));
}

#[tokio::test]
async fn events_endpoint_replays_history_in_first_frame() {
    sse::reset_hub_for_test();
    let task_id = "task-replay";
    sse::hub().publish(
        task_id,
        json!({"event": "submitted", "remote_id": "r9"}).to_string(),
    );

    let (state, _temp) = test_state().await;
    let app = sse::router(state);
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(sse::sse_response_ok(&response));

    // SSE 帧格式由 axum Event 编码（event: progress / data: <frame>）；
    // 帧语义在 hub 层验证：历史已被重放、格式含 event:/data: 行。
    let frame_text = sse::sse_frame_text(&sse::hub().history(task_id)[0]);
    assert!(
        frame_text.contains("event: progress"),
        "帧应含 event: 行：{frame_text}"
    );
    assert!(
        frame_text.contains("data:"),
        "帧应含 data: 行：{frame_text}"
    );
    assert!(frame_text.contains("r9"), "历史帧应被重放：{frame_text}");
    assert!(!sse::hub().history(task_id).is_empty());
}

#[tokio::test]
async fn dropping_cloud_event_body_releases_broadcast_receiver() {
    let task_id = format!("task-disconnect-{}", uuid::Uuid::new_v4());
    let (state, _temp) = test_state().await;
    let response = sse::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body();
    drop(body);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if sse::hub().publish(&task_id, "{}".to_string()) == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("断开 HTTP body 后应回收云任务 SSE 广播接收端");
}

#[tokio::test]
async fn hub_history_isolated_by_task_id() {
    // 单例 hub 无法重置（OnceLock）；按 task_id 隔离历史即可并行安全。
    let task_a = format!("task-iso-{}", uuid::Uuid::new_v4());
    let task_b = format!("task-iso-{}", uuid::Uuid::new_v4());
    sse::hub().publish(&task_a, "{}".to_string());
    assert_eq!(sse::hub().history(&task_a).len(), 1);
    assert_eq!(
        sse::hub().history(&task_b).len(),
        0,
        "不同 task_id 互不干扰"
    );
    // 订阅/发布同一 task 前后一致。
    let (_, history) = sse::hub().subscribe(&task_a);
    assert_eq!(history.len(), 1);
}

#[test]
fn subscribe_and_publish_share_one_history_live_boundary() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    for iteration in 0..300 {
        let hub = Arc::new(sse::CloudSseHub::new());
        let gate = Arc::new(Barrier::new(2));
        let task_id = format!("handoff-{iteration}");
        let payload = format!("frame-{iteration}");
        hub.prepare_task(task_id.clone());

        let publish_hub = Arc::clone(&hub);
        let publish_gate = Arc::clone(&gate);
        let publish_task = task_id.clone();
        let publish_payload = payload.clone();
        let publisher = thread::spawn(move || {
            publish_gate.wait();
            publish_hub.publish(&publish_task, publish_payload);
        });

        let subscribe_hub = Arc::clone(&hub);
        let subscribe_gate = Arc::clone(&gate);
        let subscribe_task = task_id.clone();
        let subscriber = thread::spawn(move || {
            subscribe_gate.wait();
            subscribe_hub.subscribe(&subscribe_task)
        });

        publisher.join().unwrap();
        let (mut receiver, history) = subscriber.join().unwrap();
        let in_history = history
            .iter()
            .filter(|frame| frame.payload.as_str() == payload.as_str())
            .count();
        let in_live_stream = usize::from(
            receiver
                .try_recv()
                .ok()
                .is_some_and(|frame| frame.payload == payload),
        );
        assert_eq!(
            in_history + in_live_stream,
            1,
            "a frame must be delivered exactly once across replay/live handoff"
        );
    }
}

#[test]
fn completed_task_cache_and_progress_history_are_bounded() {
    let hub = sse::CloudSseHub::new();
    for index in 0..(sse::COMPLETED_TASK_CACHE_CAPACITY + 8) {
        let task_id = format!("completed-{index}");
        hub.publish(
            &task_id,
            json!({ "event": "succeeded", "kind": "succeeded", "task_id": task_id }).to_string(),
        );
    }
    assert!(
        hub.history("completed-0").is_empty(),
        "oldest completed task must be evicted"
    );
    assert_eq!(
        hub.history("completed-8").len(),
        1,
        "recent replay remains available"
    );

    let history_hub = sse::CloudSseHub::new();
    let payload = format!("\"{}\"", "x".repeat(60_000));
    for _ in 0..5 {
        history_hub.publish("large-history", payload.clone());
    }
    let history = history_hub.history("large-history");
    assert!(
        history.len() <= 4,
        "byte cap evicts older frames even below event-count cap"
    );
    assert!(history.iter().map(String::len).sum::<usize>() <= 256 * 1024);
    assert_eq!(history_hub.publish("oversized", "x".repeat(70 * 1024)), 0);
    assert!(history_hub.history("oversized").is_empty());
}

#[test]
fn reopening_a_completed_task_starts_a_fresh_replay_epoch_without_resetting_ids() {
    let hub = sse::CloudSseHub::new();
    hub.publish(
        "retryable-task",
        json!({"kind":"succeeded","event":"succeeded"}).to_string(),
    );
    hub.prepare_task("retryable-task");
    hub.publish(
        "retryable-task",
        json!({"kind":"submitting","event":"submitting"}).to_string(),
    );
    let subscription = hub.subscribe_after("retryable-task", None);
    assert!(subscription.known_task);
    assert_eq!(subscription.history.len(), 1);
    assert_eq!(subscription.history[0].sequence, 2);
    assert!(subscription.history[0].payload.contains("submitting"));
    assert!(!subscription.history[0].payload.contains("succeeded"));
    assert!(!subscription.completed);
}

#[test]
fn replay_reports_events_evicted_from_the_bounded_history() {
    let hub = sse::CloudSseHub::new();
    for index in 0..520 {
        hub.publish("history-gap", json!({"index":index}).to_string());
    }
    let subscription = hub.subscribe_after("history-gap", None);
    assert!(subscription.known_task);
    assert_eq!(subscription.missing_history_events, 8);
    assert!(!subscription.completed);
    assert_eq!(subscription.history.len(), 512);
    assert_eq!(subscription.history.first().unwrap().sequence, 9);
}

#[tokio::test]
async fn prepared_task_accepts_subscription_before_first_progress_event() {
    sse::reset_hub_for_test();
    let task_id = format!("task-prepared-{}", uuid::Uuid::new_v4());
    sse::hub().prepare_task(task_id.clone());
    let (state, _temp) = test_state().await;
    let response = sse::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // The subscriber is already attached while the task is queued; publishing starts later.
    sse::hub().publish(
        &task_id,
        json!({"kind":"submitting","event":"submitting","task_id":task_id}).to_string(),
    );
    sse::hub().publish(
        &task_id,
        json!({"kind":"succeeded","event":"succeeded","task_id":task_id}).to_string(),
    );
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    .expect("prepared task stream receives later progress and terminates")
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("submitting"));
    assert!(body.contains("succeeded"));
    assert!(!body.contains("task_not_found"));
}

#[tokio::test]
async fn unknown_task_stream_emits_a_terminal_not_found_notice() {
    let (state, _temp) = test_state().await;
    let response = sse::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!(
                    "/cloud/tasks/missing-{}/events",
                    uuid::Uuid::new_v4()
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    .expect("unknown-task notice closes the stream")
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("task_not_found"));
    assert!(body.contains("任务编号"));
}

#[tokio::test]
async fn cloud_sse_last_event_id_replays_only_later_frames_with_ids() {
    sse::reset_hub_for_test();
    let task_id = format!("task-cursor-{}", uuid::Uuid::new_v4());
    for kind in ["submitting", "executing", "succeeded"] {
        sse::hub().publish(
            &task_id,
            json!({"kind":kind,"event":kind,"task_id":task_id}).to_string(),
        );
    }
    let (state, _temp) = test_state().await;
    let response = sse::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .header("last-event-id", "1")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    .expect("terminal cursor replay must close the stream")
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        !body.contains("submitting"),
        "cursor 1 must exclude seq 1: {body}"
    );
    assert!(body.contains("id: 2"));
    assert!(body.contains("executing"));
    assert!(body.contains("id: 3"));
    assert!(body.contains("succeeded"));

    let (terminal_state, _terminal_temp) = test_state().await;
    let terminal_response = sse::router(terminal_state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .header("last-event-id", "3")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let terminal_body = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        axum::body::to_bytes(terminal_response.into_body(), 64 * 1024),
    )
    .await
    .expect("a cursor after the terminal frame must still close deterministically")
    .unwrap();
    assert!(String::from_utf8_lossy(&terminal_body).contains("stream_complete"));
}

#[tokio::test]
async fn terminal_progress_closes_live_receivers_and_late_replay_streams() {
    sse::reset_hub_for_test();
    let task_id = "terminal-close-http";
    sse::hub().prepare_task(task_id);
    let mut receiver = sse::hub().subscribe(task_id).0;
    sse::hub().publish(
        task_id,
        json!({ "event": "succeeded", "kind": "succeeded", "task_id": task_id }).to_string(),
    );
    let frame = receiver.recv().await.unwrap();
    assert!(frame.payload.contains("succeeded"));
    assert!(
        receiver.recv().await.is_err(),
        "terminal event releases the live channel"
    );

    let (state, _temp) = test_state().await;
    let response = sse::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    .expect("late terminal replay must close instead of holding a connection")
    .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("succeeded"));
}

#[tokio::test]
async fn slow_cloud_sse_consumer_gets_visible_gap_notice_and_stream_closes() {
    sse::reset_hub_for_test();
    let task_id = format!("task-lag-{}", uuid::Uuid::new_v4());
    sse::hub().prepare_task(task_id.clone());
    let (state, _temp) = test_state().await;
    let response = sse::router(state)
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(format!("/cloud/tasks/{task_id}/events"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Do not yield to the forwarding task while publishing: the 256-slot broadcast
    // ring overruns before the HTTP consumer starts draining its bounded channel.
    for index in 0..300 {
        sse::hub().publish(
            &task_id,
            json!({"kind":"executing","task_id":task_id,"index":index}).to_string(),
        );
    }
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    .expect("lag notice must not hold the stream open")
    .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("event: progress"));
    assert!(body.contains("stream_gap"), "gap must be explicit: {body}");
    assert!(body.contains("skipped_events"));
    assert!(body.contains("续传"));
}

#[test]
fn unknown_task_subscription_is_closed_without_a_retained_sender() {
    let hub = sse::CloudSseHub::new();
    let (mut receiver, history) = hub.subscribe("untrusted-unknown-task-id");
    assert!(history.is_empty());
    assert!(
        receiver.try_recv().is_err(),
        "unknown task stream must be closed immediately"
    );
}
