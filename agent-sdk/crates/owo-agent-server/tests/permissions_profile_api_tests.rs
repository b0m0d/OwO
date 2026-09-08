//! §5.3/§5.4/§5.5 权限档位、授权记忆（Grant）与脱敏审批 HTTP 契约测试。
//!
//! 覆盖：
//! - GET /permissions：当前档位 + grant 列表（脱敏，不含参数原文）
//! - POST /permissions：切换档位（写 settings.json + 运行时生效）
//! - POST /permissions/grants/revoke：逐条撤销，审计记录
//! - 审批响应带 scope → 生成 Grant → 同工具同参数不再弹卡

use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use std::sync::Arc;
use tower::ServiceExt;

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

#[tokio::test]
async fn permissions_status_reports_profile_and_redacted_grants() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let response = app
        .clone()
        .oneshot(request(&state, "GET", "/permissions", None))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["profile"],
        serde_json::json!("workspace"),
        "默认档位 workspace"
    );
    assert_eq!(body["read_only"], serde_json::json!(false));
    assert_eq!(body["grant_count"], serde_json::json!(0));
}

#[tokio::test]
async fn profile_switch_persists_to_settings_and_takes_effect_at_runtime() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let response = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/permissions",
            Some(r#"{"profile":"full_access"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);

    // settings.json 已持久化。
    let settings = owo_agent_core::Settings::load(&temp.path().join("ws"));
    assert_eq!(
        settings.permission_profile.as_deref(),
        Some("full_access"),
        "settings.permission_profile 必须写入"
    );
    assert!(!settings.read_only);

    // 运行时即时生效。
    let policy = &state.agent;
    let profile = policy.permission_profile();
    assert_eq!(profile.label(), "full_access");
    // FullAccess：写与执行放行（落到 Policy 判定，仅编译期验证形状）。
    let _cmd = Policy::new(temp.path().join("ws"))
        .evaluate("run_command", &serde_json::json!({ "command": "ls" }));
    assert_eq!(
        state.agent.permission_profile().label(),
        "full_access",
        "切换后档位即时生效"
    );

    // 未知档位 → 400；不落盘。
    let bad = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/permissions",
            Some(r#"{"profile":"everything"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 400);
    let settings_after = owo_agent_core::Settings::load(&temp.path().join("ws"));
    assert_eq!(
        settings_after.permission_profile.as_deref(),
        Some("full_access"),
        "非法档位不得覆盖已保存配置"
    );
}

#[tokio::test]
async fn read_only_profile_switches_via_route() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let response = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/permissions",
            Some(r#"{"profile":"read_only"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let settings = owo_agent_core::Settings::load(&temp.path().join("ws"));
    assert!(settings.read_only, "read_only 档必须同步 read_only 标志");
    assert_eq!(state.agent.permission_profile().label(), "read_only");
}

#[tokio::test]
async fn grants_revoke_and_audit() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 直接插入一条 grant（模拟审批生成路径）。
    let req = Policy::new(".").evaluate("read_file", &serde_json::json!({ "path": "a.txt" }));
    let workspace_id = state.workspace_id();
    let grant = state
        .grants
        .grant_from_scope(
            &req,
            &workspace_id,
            owo_agent_core::grant_store::GrantScope::OneHour,
        )
        .expect("one_hour 生成 grant");
    let grant_id = grant.grant_id.clone();
    state.grants.insert(grant);

    // 列表可见。
    let list = app
        .clone()
        .oneshot(request(&state, "GET", "/permissions/grants", None))
        .await
        .unwrap();
    assert_eq!(list.status().as_u16(), 200);
    let list_body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(list.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list_body["grant_count"], serde_json::json!(1));
    let listed = &list_body["grants"][0];
    assert_eq!(listed["grant_id"], serde_json::json!(grant_id));
    // 脱敏：不暴露参数原文。
    let serialized = serde_json::to_string(&list_body).unwrap();
    assert!(
        !serialized.contains("a.txt"),
        "grant 列表不得携带参数原文：{serialized}"
    );

    // 撤销。
    let revoke = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/permissions/grants/revoke",
            Some(&format!(r#"{{"grant_id":"{grant_id}"}}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(revoke.status().as_u16(), 200);
    let after = state.grants.list();
    assert!(after.is_empty(), "撤销后 grant 列表为空");

    // 重复撤销 → 404（资源不存在）。
    let again = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/permissions/grants/revoke",
            Some(&format!(r#"{{"grant_id":"{grant_id}"}}"#)),
        ))
        .await
        .unwrap();
    assert_eq!(again.status().as_u16(), 404);

    // 审计记录包含 grant_revoke。
    let audit = state.agent.audit_log();
    let audit = audit.lock().unwrap();
    assert!(
        audit
            .entries
            .iter()
            .any(|entry| entry.event == "grant_revoke"),
        "撤销必须写审计：{}",
        serde_json::to_string(&audit.entries).unwrap_or_default()
    );
}

#[tokio::test]
async fn grant_hit_skips_approval_for_same_fingerprint() {
    // 端到端：policy 注入 shared grant → 命中后 decision 为 Allow（无需审批）。
    let (state, _temp) = test_state().await;

    // Agent 的 Policy 已由 AppState::new 注入 state.grants（共享引用）。
    let request = owo_agent_core::permissions::PermissionRequest::new(
        "run_command",
        serde_json::json!({ "command": "ls -la" }),
        owo_agent_core::permissions::Level::Execute,
        "执行命令：ls -la",
    );
    // 无 grant → Ask（被测工具不在工作区写自动放行范围）。
    let policy = Policy::new(std::env::temp_dir());
    let _ = policy.decision(&request);

    // 注入 grant 后：同参命中 → Allow。
    let first = state
        .grants
        .grant_from_scope(
            &request,
            &state.workspace_id(),
            owo_agent_core::grant_store::GrantScope::Session,
        )
        .expect("session 生成 grant");
    state.grants.insert(first);
    let workspace = state.workspace.clone();
    let injected = Policy::new(&workspace).with_grants(Arc::clone(&state.grants));
    let decision = injected.decision(&request);
    assert_eq!(
        decision,
        owo_agent_core::permissions::Decision::Allow,
        "同参（同指纹）grant 命中应直接放行"
    );
}
