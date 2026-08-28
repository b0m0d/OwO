//! WorkSwarm 实时进度 API 响应性测试（R3）。
//!
//! 覆盖（HTTP 视角，内置 sleep worker，不依赖模型凭据）：
//! 1. 90 秒级长 Worker 执行期间，GET 详情请求 ≤200ms（不等待 team 锁）；
//! 2. steer cancel 在 1 秒内返回并停止活动 Worker（终态 Cancelled）；
//! 3. SSE 流含 `progress` 事件：`seq` 单调递增，运行中含 current_steps/counts。

use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

/// 无外部依赖的最小模型 Provider（本测试只用 sleep worker，模型调用即失败）。
struct IdleProvider;

#[async_trait::async_trait]
impl ModelProvider for IdleProvider {
    async fn complete(
        &self,
        _messages: &[owo_agent_core::ChatMessage],
        _tools: &[owo_agent_core::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        Err("IdleProvider 不应被调用".to_string())
    }
}

async fn test_state() -> (Arc<owo_agent_server::AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = Agent::new(
        Arc::new(IdleProvider),
        ToolRegistry::new(),
        Policy::new(&workspace),
        Default::default(),
    );
    let store = SqliteSessionStore::open(&workspace.join("index.db")).unwrap();
    let state = Arc::new(owo_agent_server::AppState::new(
        agent,
        store,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    (state, temp)
}

fn request(
    state: &Arc<owo_agent_server::AppState>,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> axum::http::Request<axum::body::Body> {
    use axum::http::{header, Method, Request};
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", state.auth_token.token()),
        );
    if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder.body(axum::body::Body::from(b.to_string())).unwrap();
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

/// 发请求并解析 JSON 响应体。
async fn call(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, Value) {
    let resp = app
        .clone()
        .oneshot(request(state, method, path, body))
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        json!(null)
    } else {
        serde_json::from_slice(&bytes).unwrap_or(json!(null))
    };
    (status, value)
}

/// 轮询 GET /teams/{id} 直到状态进入目标集合。
async fn poll_team_status(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    team_id: &str,
    want: &[&str],
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let (status, body) = call(state, app, "GET", &format!("/teams/{team_id}"), None).await;
        assert!(
            status == 200,
            "GET /teams/{team_id} 应 200：{status} {body}"
        );
        let st = body["team"]["status"].as_str().unwrap_or("").to_string();
        if want.contains(&st.as_str()) {
            return body;
        }
        if Instant::now() > deadline {
            panic!("等待团队状态 {want:?} 超时，最后：{st}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// 1+2. 长 Worker 期间 GET 详情 ≤200ms；cancel <1s 返回并停止 Worker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detail_latency_under_200ms_during_90s_worker_and_cancel_stops_fast() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 90 秒 sleep Worker（server 侧 cap 已放宽到 600s）。
    let create = json!({
        "objective": "长任务响应性验收",
        "roles": [
            { "role": "builder", "assignee": "agent", "worker": "sleep",
              "extra_input": { "ms": 90000 }, "verify": "non_empty" }
        ]
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();

    // 等待进入 running（任务视图步骤 Running / progress active）。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut running = false;
    while Instant::now() < deadline {
        let (_, body) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
        let tasks = body["tasks"].as_array().cloned().unwrap_or_default();
        if tasks
            .iter()
            .any(|t| t["status"].as_str() == Some("Running"))
        {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(running, "团队应进入 running（步骤执行中）");

    // 长 Worker 执行期间：连续 5 次 GET 详情，逐次 ≤200ms。
    for _ in 0..5 {
        let started = Instant::now();
        let (status, body) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
        let elapsed = started.elapsed();
        assert_eq!(status, 200);
        assert!(
            elapsed < Duration::from_millis(200),
            "长 Worker 期间 GET 详情耗时 {elapsed:?}（要求 <200ms）：{body}"
        );
        assert_eq!(body["team"]["status"].as_str(), Some("running"));
        assert_eq!(body["interrupted"].as_bool(), Some(false));
    }

    // cancel：<1s 返回，并停止活动 Worker。
    let started = Instant::now();
    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&json!({ "command": "cancel", "note": "验收取消" }).to_string()),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "{body}");
    assert!(
        elapsed < Duration::from_secs(1),
        "steer cancel 耗时 {elapsed:?}（要求 <1s）"
    );

    let final_state = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled"],
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(final_state["interrupted"].as_bool(), Some(false));
    // Worker 已停止：任务视图步骤 Aborted（而非跑完 Succeeded）。
    let tasks = final_state["tasks"].as_array().cloned().unwrap_or_default();
    assert!(
        tasks
            .iter()
            .all(|t| t["status"].as_str() == Some("Aborted")),
        "取消后任务视图应 Aborted：{tasks:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. SSE progress 事件：seq 单调；运行中含 current_steps
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sse_stream_emits_monotonic_progress_events() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 慢 sleep：留出窗口在流打开后观察到 running 的 current_steps，再取消收尾。
    let create = json!({
        "objective": "SSE progress 验收",
        "roles": [
            { "role": "builder", "assignee": "agent", "worker": "sleep",
              "extra_input": { "ms": 30000 }, "verify": "non_empty" }
        ]
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();

    // 等待 running 再开流（保证首帧 progress 即含 current_steps）。
    poll_team_status(
        &state,
        &app,
        &team_id,
        &["running"],
        Duration::from_secs(10),
    )
    .await;

    // 开流（流在终态关闭；本测试稍后取消使其闭合）。
    let resp = app
        .clone()
        .oneshot(request(
            &state,
            "GET",
            &format!("/teams/{team_id}/events"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);

    // 流读取期间发起 cancel（另路），让流自然闭合。
    let cancel_state = Arc::clone(&state);
    let cancel_app = app.clone();
    let cancel_team = team_id.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        let started = Instant::now();
        let (status, body) = call(
            &cancel_state,
            &cancel_app,
            "POST",
            &format!("/teams/{cancel_team}/steer"),
            Some(&json!({ "command": "cancel" }).to_string()),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "cancel 耗时 {elapsed:?}（要求 <1s）"
        );
    });

    // drain 到终态关闭（cancel 使其 <3s 闭合）。
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024),
    )
    .await
    .expect("SSE 流未在取消后闭合")
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    canceller.await.expect("取消任务不应 panic");

    assert!(text.contains("\"type\":\"open\""), "应有 open 帧：{text}");
    // progress 帧：含 seq / current_steps / counts。
    let progress_frames: Vec<&str> = text
        .split("\n\n")
        .filter(|frame| frame.contains("\"type\":\"progress\""))
        .collect();
    assert!(!progress_frames.is_empty(), "应有 progress 帧：{text}");
    let first = progress_frames.first().unwrap();
    assert!(
        first.contains("\"seq\":") && first.contains("\"current_steps\":"),
        "progress 帧应含 seq/current_steps：{first}"
    );
    assert!(
        first.contains("\"running\""),
        "progress 帧应含 running 计数：{first}"
    );

    // seq 单调递增（按出现顺序解析）。
    let mut last: i64 = -1;
    for frame in &progress_frames {
        let data = frame
            .split("data: ")
            .nth(1)
            .unwrap_or("")
            .trim()
            .to_string();
        let value: Value = serde_json::from_str(&data)
            .unwrap_or_else(|e| panic!("progress 帧非法：{data}（{e}）"));
        let seq = value["progress"]["seq"].as_i64().unwrap_or(-1);
        assert!(seq > last, "progress seq 应递增：{last} → {seq}");
        last = seq;
    }
    assert!(
        progress_frames.len() >= 2,
        "取消转移应至少产生两个 progress 帧：{progress_frames:?}"
    );
}
