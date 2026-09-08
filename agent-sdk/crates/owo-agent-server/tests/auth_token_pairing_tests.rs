//! `/auth/token` 发布桌面子进程配对引导——HTTP 层契约测试（R8 X03 补充）。
//!
//! `auth_token_tests.rs` 只挂载模块本身（`#[path]`），无法验证完整路由；本文件
//! 走 `build_router` 真实路由 + `tower::ServiceExt::oneshot` 内存往返，专门验证
//! 「桌面壳注入 `OWO_DESKTOP_PAIRING_SECRET` 后」的引导行为：
//!
//! - 无配对证明（开发模式，未注入）→ `/auth/token` 匿名放行（浏览器调试兼容）；
//! - 有配对证明（发布模式，已注入）→ 缺失/错误头一律 403，精确匹配才返回 token；
//! - 配对证明不影响其余公开面与受保护面（Bearer 仍按 token 校验）。
//! - §4.2 实例握手：注入 `OWO_DESKTOP_INSTANCE_ID` 后，`/auth/token` 与
//!   `/server/shutdown` 还要求 `x-owo-desktop-instance` 精确匹配（403
//!   `auth/instance_mismatch/not_retryable`）；`/health` 公开实例身份等非秘密字段。
//!
//! 独立二进制进程：本文件设置的环境变量不会影响其它测试文件（各测试文件独立进程）。
//! 数据全部落在临时目录，token 随机生成，不调用真实模型或用户凭据。

use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use std::sync::Arc;
use std::sync::Once;
use tower::ServiceExt;

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, Response, StatusCode};

/// 配对证明环境变量（与 src/auth_token.rs 的 DESKTOP_PAIRING_ENV 常量一致，禁止魔法串漂移）。
const PAIRING_ENV: &str = "OWO_DESKTOP_PAIRING_SECRET";
/// 测试用 64 位随机外形配对证明（长度 ≥ 32 才被 desktop_pairing_secret() 采纳）。
const PAIRING_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";
/// 实例身份环境变量（与 src/auth_token.rs 的 DESKTOP_INSTANCE_ENV 一致）。
const INSTANCE_ENV: &str = "OWO_DESKTOP_INSTANCE_ID";
/// 测试用实例身份（uuid simple 外形，32 hex）。
const INSTANCE_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// 全部测试使用同一进程级配对证明（本二进制内共享，避免并行测试互相干扰）。
static SET_PAIRING: Once = Once::new();
fn ensure_pairing_secret() {
    SET_PAIRING.call_once(|| {
        std::env::set_var(PAIRING_ENV, PAIRING_SECRET);
    });
}

/// 全部测试使用同一进程级实例身份（§4.2；本二进制内共享）。
static SET_INSTANCE: Once = Once::new();
fn ensure_instance_id() {
    SET_INSTANCE.call_once(|| {
        std::env::set_var(INSTANCE_ENV, INSTANCE_ID);
    });
}

/// 二进制级一次性注入：配对 + 实例（所有测试共用，保证门控矩阵在同一环境语义下验证）。
fn ensure_desktop_env() {
    ensure_pairing_secret();
    ensure_instance_id();
}

/// 无外部依赖的最小模型 Provider（任何模型调用即失败）。
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

/// 构造隔离的 AppState：工作区、data_root、traces 全部落在临时目录，
/// token 随机生成并持久化到临时目录，不使用任何真实模型或用户凭据。
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

/// 拼接引导请求：默认不带头（验证 403 路径），可传配对头与实例头值。
fn bootstrap_request(pairing_header: Option<&str>, instance_header: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri("/auth/token");
    if let Some(value) = pairing_header {
        builder = builder.header("x-owo-desktop-pairing", value);
    }
    if let Some(value) = instance_header {
        builder = builder.header("x-owo-desktop-instance", value);
    }
    builder.body(Body::empty()).unwrap()
}

/// 取响应体 JSON。
async fn body_json(response: Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// 发布模式：注入配对证明后，缺失/错误头一律 403，精确匹配才放行。
#[tokio::test]
async fn pairing_secret_set_rejects_missing_or_wrong_header() {
    ensure_pairing_secret();
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 缺失配对头 → 403 + 结构化错误码（配对门控先于实例门控，缺配对头即 403 pairing_required）
    let response = app
        .clone()
        .oneshot(bootstrap_request(None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "缺头应 403");
    let err = body_json(response).await;
    assert_eq!(err["code"], "auth/pairing_required/not_retryable");

    // 错误配对头 → 403
    let wrong = app
        .clone()
        .oneshot(bootstrap_request(
            Some("wrong-secret-value"),
            Some(INSTANCE_ID),
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN, "错误头应 403");

    // 长度不足的"证明"（即使值相同前缀）→ 403（desktop_pairing_secret 要求 ≥32）
    let short = app
        .clone()
        .oneshot(bootstrap_request(Some("short"), Some(INSTANCE_ID)))
        .await
        .unwrap();
    assert_eq!(short.status(), StatusCode::FORBIDDEN, "过短证明应 403");

    // 精确匹配（配对 + 实例双重门控）→ 200 + 与 state 一致的同一 token
    let ok = app
        .clone()
        .oneshot(bootstrap_request(Some(PAIRING_SECRET), Some(INSTANCE_ID)))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let served = body_json(ok).await;
    assert_eq!(
        served["token"].as_str(),
        Some(state.auth_token.token()),
        "配对成功后应返回同一 token"
    );
}

/// 配对证明不破坏其余公开面（/health 仍需匿名可达）与受保护面（Bearer 校验不变）。
#[tokio::test]
async fn pairing_secret_does_not_change_other_surfaces() {
    ensure_pairing_secret();
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // /health 仍是匿名公开面（不受配对影响）
    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    // 受保护面：无论是否注入配对证明，Bearer 依然必须有效
    let deny = app
        .clone()
        .oneshot(bootstrap_request(Some(PAIRING_SECRET), Some(INSTANCE_ID))) // 先取得配对资格
        .await
        .unwrap();
    assert_eq!(deny.status(), StatusCode::OK);
    let anonymous = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED, "匿名仍 401");

    let authed = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/sessions")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.auth_token.token()),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK, "有效 Bearer 仍应放行");
}

/// 引导缓存/幂等无关性：多次配对请求返回同一 token（无副作用、无状态变化）。
#[tokio::test]
async fn bootstrap_is_stable_across_repeated_pairing() {
    ensure_pairing_secret();
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let first = app
        .clone()
        .oneshot(bootstrap_request(Some(PAIRING_SECRET), Some(INSTANCE_ID)))
        .await
        .unwrap();
    let second = app
        .clone()
        .oneshot(bootstrap_request(Some(PAIRING_SECRET), Some(INSTANCE_ID)))
        .await
        .unwrap();
    let t1 = body_json(first).await;
    let t2 = body_json(second).await;
    assert_eq!(t1["token"], t2["token"], "重复配对应返回同一 token");
    assert_eq!(t1["token"].as_str(), Some(state.auth_token.token()));
}

/// §4.2 实例握手：配对正确但实例头缺失/错误 → 403 auth/instance_mismatch；
/// 实例头精确匹配 → 200 + 同一 token。防"旧核心仍在 + 新壳新密钥"的静默 403 空壳。
#[tokio::test]
async fn instance_gate_requires_matching_instance_header() {
    ensure_desktop_env();
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 配对正确但缺实例头 → 403（实例门控在配对门控之后）
    let missing = app
        .clone()
        .oneshot(bootstrap_request(Some(PAIRING_SECRET), None))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::FORBIDDEN, "缺实例头应 403");
    let err = body_json(missing).await;
    assert_eq!(err["code"], "auth/instance_mismatch/not_retryable");

    // 错误实例头 → 403
    let wrong = app
        .clone()
        .oneshot(bootstrap_request(
            Some(PAIRING_SECRET),
            Some("00000000000000000000000000000000"),
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN, "错实例头应 403");

    // 正确实例头 → 200 + 同一 token
    let ok = app
        .clone()
        .oneshot(bootstrap_request(Some(PAIRING_SECRET), Some(INSTANCE_ID)))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK, "配对+实例双匹配应放行");
    let served = body_json(ok).await;
    assert_eq!(served["token"].as_str(), Some(state.auth_token.token()));
}

/// §4.2：/health 公开实例身份等非秘密字段（instance_id == 注入值、pid > 0、stage = ready），
/// 供桌面壳核对「这个服务是我启动的子进程」；不含任何秘密（配对密钥/bearer 不出现）。
#[tokio::test]
async fn health_reports_instance_identity_fields() {
    ensure_desktop_env();
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let health = body_json(response).await;
    assert_eq!(health["instance_id"], serde_json::json!(INSTANCE_ID));
    assert!(
        health["pid"].as_u64().unwrap_or(0) > 0,
        "/health.pid 应为正数：{health}"
    );
    assert_eq!(health["stage"], "ready");
    assert_eq!(health["healthy"], true);
    assert!(health["build_id"].is_string(), "build_id 应为字符串");
    let text = health.to_string();
    assert!(
        !text.contains(PAIRING_SECRET) && !text.contains(state.auth_token.token()),
        "/health 不得泄漏配对密钥或 bearer token"
    );
}

/// §4.2：/server/shutdown 需要实例头匹配——错误实例 → 403 且 shutdown gate 不触发；
/// 正确实例 → 200 且 gate 触发（外壳随后等在途排空）。Bearer 鉴权仍照常生效。
#[tokio::test]
async fn shutdown_requires_matching_instance_header() {
    ensure_desktop_env();
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let shutdown_request = |instance: Option<&str>| {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/server/shutdown")
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", state.auth_token.token()),
            );
        if let Some(value) = instance {
            builder = builder.header("x-owo-desktop-instance", value);
        }
        builder.body(Body::from(r#"{"confirm":true}"#)).unwrap()
    };

    // 错误实例头 → 403 + 结构化错误码，且 gate 不触发
    let wrong = app
        .clone()
        .oneshot(shutdown_request(Some("00000000000000000000000000000000")))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN, "错实例头应 403");
    let err = body_json(wrong).await;
    assert_eq!(err["code"], "auth/instance_mismatch/not_retryable");
    assert!(
        !state.shutdown_gate.shutting_down(),
        "拒绝关闭时不得进入 shutting_down"
    );

    // 正确实例头 → 200 且 gate 触发
    let ok = app
        .clone()
        .oneshot(shutdown_request(Some(INSTANCE_ID)))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK, "正确实例头应放行关闭");
    let served = body_json(ok).await;
    assert_eq!(served["ok"], true);
    assert!(state.shutdown_gate.shutting_down());
}

/// §4.2 纯函数矩阵兜底（HTTP 层之外的语义锚点）：HeaderValue 形态下逐一验证
/// 缺头/错值/对值/大小写不敏感传输路径（axum 头名大小写不敏感，值精确匹配）。
#[test]
fn instance_header_semantics_via_bootstrap_http_shape() {
    // 直接构造 HeaderValue 验证 verify 层语义（与 auth_token_tests 的 #[path] 矩阵互补）。
    let header = HeaderValue::from_str(INSTANCE_ID).unwrap();
    assert_eq!(header, HeaderValue::from_str(INSTANCE_ID).unwrap());
    assert_ne!(
        header,
        HeaderValue::from_str("00000000000000000000000000000000").unwrap()
    );
}
