//! M4.2 模型路由验收契约测试（《Agent-SDK-后续任务实施指南-2026-09-18》§2-M4.2）。
//!
//! 覆盖验收标准中的 HTTP 契约面（wire 层路由真相 = `model_override`）：
//! 1. `session_model_override_is_persisted_and_visible`：创建会话显式指定 model →
//!    覆盖进 `model_override` 并持久化（SQLite 列往返）；GET 暴露路由真相。
//! 2. `session_without_model_stays_auto`：未指定 model → `model_override` 恒 null
//!    （走 Provider 解析链，OPENAI_MODEL 热切换不被创建时刻钉死）。
//! 3. `set_model_route_pins_and_default_clears`：`POST /session/{id}/model` 换绑；
//!    `"default"` 哨兵 = 清除覆盖（哨兵不落库、不进请求体）；未知会话 404。
//!
//! wire 级断言（覆盖值真正进入请求体、哨兵不泄漏）由 core 单测锚定：
//! `agent.rs::session_model_override_reaches_wire_model`、
//! `gateway.rs::request_body_model_override_wins_and_sentinel_never_leaks`。

use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use std::sync::Arc;
use tower::ServiceExt;

/// 无外部依赖的最小模型 Provider（任何模型调用即失败；本文件不触发回合）。
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

async fn build_state() -> (Arc<owo_agent_server::AppState>, tempfile::TempDir) {
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

async fn body_json(
    app: axum::Router,
    req: axum::http::Request<axum::body::Body>,
) -> (u16, serde_json::Value) {
    let response = app.oneshot(req).await.expect("oneshot 不应 panic");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

async fn create_session(
    app: &axum::Router,
    state: &Arc<owo_agent_server::AppState>,
    body: serde_json::Value,
) -> serde_json::Value {
    let (status, value) = body_json(
        app.clone(),
        request(state, "POST", "/session", Some(&body.to_string())),
    )
    .await;
    assert_eq!(status, 200, "POST /session 应 200：{value}");
    value
}

#[tokio::test]
async fn session_model_override_is_persisted_and_visible() {
    let (state, _temp) = build_state().await;
    let app = build_router(Arc::clone(&state));
    let created = create_session(
        &app,
        &state,
        serde_json::json!({"workspace":".","model":"wire-pro"}),
    )
    .await;
    let id = created["id"].as_str().expect("会话 id").to_string();
    assert_eq!(created["model"], "wire-pro", "展示模型 = 显式值");
    // 路由真相经 HTTP 面暴露。
    let (status, got) = body_json(
        app.clone(),
        request(&state, "GET", &format!("/session/{id}"), None),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        got["model_override"], "wire-pro",
        "GET 应暴露 model_override"
    );
    // 持久化真相：直读存储（绕开内存 map），证明 SQLite `model_override` 列往返。
    let persisted = state.store.load(&id).expect("存储应可加载");
    assert_eq!(persisted.model_override.as_deref(), Some("wire-pro"));
}

#[tokio::test]
async fn session_without_model_stays_auto() {
    let (state, _temp) = build_state().await;
    let app = build_router(Arc::clone(&state));
    let created = create_session(&app, &state, serde_json::json!({"workspace":"."})).await;
    let id = created["id"].as_str().expect("会话 id").to_string();
    // 展示值有回落（Provider 链缺省），但路由覆盖必须为空 = 自动。
    assert!(created["model"].as_str().is_some_and(|v| !v.is_empty()));
    let (status, got) = body_json(
        app.clone(),
        request(&state, "GET", &format!("/session/{id}"), None),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        got["model_override"],
        serde_json::Value::Null,
        "未指定 = 自动路由"
    );
    let persisted = state.store.load(&id).expect("存储应可加载");
    assert_eq!(persisted.model_override, None);
    // "default" 哨兵 = 与未指定同义：不落库、不固定、不泄漏为展示值。
    let sentinel = create_session(
        &app,
        &state,
        serde_json::json!({"workspace":".","model":"default"}),
    )
    .await;
    let sid = sentinel["id"].as_str().expect("会话 id").to_string();
    let (status, got) = body_json(
        app.clone(),
        request(&state, "GET", &format!("/session/{sid}"), None),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        got["model_override"],
        serde_json::Value::Null,
        "哨兵必须归一为自动"
    );
    assert_ne!(got["model"], "default", "哨兵值不得泄漏为展示/存储模型");
    assert_eq!(state.store.load(&sid).unwrap().model_override, None);
}

#[tokio::test]
async fn set_model_route_pins_and_default_clears() {
    let (state, _temp) = build_state().await;
    let app = build_router(Arc::clone(&state));
    let created = create_session(
        &app,
        &state,
        serde_json::json!({"workspace":".","model":"pin-a"}),
    )
    .await;
    let id = created["id"].as_str().expect("会话 id").to_string();

    // 换绑：显式值进覆盖并同步展示。
    let (status, value) = body_json(
        app.clone(),
        request(
            &state,
            "POST",
            &format!("/session/{id}/model"),
            Some(r#"{"model":" pin-b "}"#),
        ),
    )
    .await;
    assert_eq!(status, 200, "换绑应 200：{value}");
    assert_eq!(value["model_override"], "pin-b", "覆盖 = trim 后的显式值");
    assert_eq!(value["model"], "pin-b", "固定时展示同步");
    assert_eq!(
        state.store.load(&id).unwrap().model_override.as_deref(),
        Some("pin-b"),
        "换绑必须落库"
    );

    // "default" 哨兵 = 清除覆盖；哨兵不落库，展示保持最后固定值。
    let (status, value) = body_json(
        app.clone(),
        request(
            &state,
            "POST",
            &format!("/session/{id}/model"),
            Some(r#"{"model":"default"}"#),
        ),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        value["model_override"],
        serde_json::Value::Null,
        "哨兵必须清除覆盖"
    );
    assert_eq!(value["model"], "pin-b", "清除不改展示（不猜测缺省值）");
    let persisted = state.store.load(&id).unwrap();
    assert_eq!(persisted.model_override, None, "哨兵值不得落库");

    // null 载荷等价于清除（重复清除幂等）。
    let (status, value) = body_json(
        app.clone(),
        request(
            &state,
            "POST",
            &format!("/session/{id}/model"),
            Some(r#"{"model":null}"#),
        ),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(value["model_override"], serde_json::Value::Null);

    // 未知会话 → 404。
    let (status, _) = body_json(
        app.clone(),
        request(
            &state,
            "POST",
            "/session/__missing__/model",
            Some(r#"{"model":"x"}"#),
        ),
    )
    .await;
    assert_eq!(status, 404, "未知会话必须 404");
}
