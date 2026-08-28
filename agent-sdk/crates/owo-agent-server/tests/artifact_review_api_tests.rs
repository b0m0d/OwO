//! Artifact 评审闭环 HTTP 集成测试（V1 四期 · 第三路）。
//!
//! 覆盖：approve/request_changes/reject 全链路（真实 echo 团队产出真实 Artifact）、
//! 幂等键零副作用重放、幂等键跨产物冲突、expected_version 乐观并发（409）、
//! 生产者自批 403（self_review_allowed 授权后放行）、404/400 边界。
//! 全部使用内置 echo worker（不依赖模型凭据）。
//!
//! 说明：missing required field 走 axum Json extractor 默认 422；handler 内
//! 未知 decision / 空 team_id 显式 400（与本路 OpenAPI 快照一致）。

use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

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

/// 单角色 echo 团队（planner 一个产物；producer 确定可查）。
fn planner_roles() -> Value {
    json!([
        { "role": "planner", "assignee": "agent", "worker": "echo", "verify": "non_empty" }
    ])
}

/// 双角色 echo 团队（planner→builder；两个产物用于幂等键跨产物冲突）。
fn relay_roles() -> Value {
    json!([
        { "role": "planner", "assignee": "agent", "worker": "echo", "verify": "non_empty" },
        { "role": "builder", "assignee": "agent", "worker": "echo",
          "depends_on": ["planner"], "verify": "non_empty" }
    ])
}

/// 建队 → 等待终态 → 返回 (team_id, project_id, artifacts 列表)。
async fn run_echo_team(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    roles: Value,
    human_policy: Option<&str>,
) -> (String, String, Vec<Value>) {
    let mut body = json!({
        "objective": "评审闭环测试运行",
        "mode": "team",
        "roles": roles,
        "budget": { "max_steps": 8 }
    });
    if let Some(policy) = human_policy {
        body["human_policy"] = json!(policy);
    }
    let (status, created) = call(state, app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "POST /teams 应 202：{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();
    let project_id = created["project_space_id"].as_str().unwrap().to_string();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (st, detail) = call(state, app, "GET", &format!("/teams/{team_id}"), None).await;
        assert_eq!(st, 200, "GET /teams/{team_id} 应 200：{detail}");
        if detail["team"]["status"] == "succeeded" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "echo 团队未在超时内完成：{detail}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let (st, arts) = call(
        state,
        app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    assert_eq!(st, 200);
    (
        team_id,
        project_id,
        arts["artifacts"].as_array().unwrap().clone(),
    )
}

fn review_body(
    team_id: &str,
    decision: &str,
    reviewer: &str,
    idem: &str,
    expected_version: Option<u32>,
) -> String {
    let mut body = json!({
        "team_id": team_id,
        "decision": decision,
        "reviewer": reviewer,
        "comment": "结构清晰，证据充分",
        "idempotency_key": idem,
    });
    if let Some(v) = expected_version {
        body["expected_version"] = json!(v);
    }
    body.to_string()
}

#[tokio::test]
async fn approve_review_full_chain_via_http() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (team_id, _project_id, artifacts) =
        run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact = &artifacts[0];
    let artifact_id = artifact["artifact_id"].as_str().unwrap().to_string();
    let producer = artifact["producer"].as_str().unwrap().to_string();
    assert_ne!(producer, "", "产物应带生产者：{artifact}");

    // 非 producer 的评审者批准 → 201 全链路。
    let body = review_body(&team_id, "approve", "critic", "idem-approve-1", None);
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201, "首次批准应 201：{resp}");
    assert_eq!(resp["replayed"], json!(false));
    assert_eq!(resp["review"]["decision"], json!("approve"));
    assert_eq!(resp["review"]["artifact_version"], artifact["version"]);
    assert_eq!(
        resp["artifact"]["review_state"],
        json!("approved"),
        "批准后 review_state 应迁移：{resp}"
    );
    assert_eq!(
        resp["approved_head"]["artifact_id"],
        json!(artifact_id),
        "approved head 应指向被批准版本：{resp}"
    );
    // 取证锚点：记录保存评审时的内容引用。
    assert_eq!(resp["review"]["content_ref"], artifact["content_ref"]);

    // GET history：历史 + head + 链字段齐备。
    let (status, hist) = call(
        &state,
        &app,
        "GET",
        &format!("/artifacts/{artifact_id}/history"),
        None,
    )
    .await;
    assert_eq!(status, 200, "history 应 200：{hist}");
    assert_eq!(hist["artifact_id"], json!(artifact_id));
    assert_eq!(hist["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(hist["reviews"][0]["decision"], json!("approve"));
    assert_eq!(hist["reviews"][0]["reviewer"], json!("critic"));
    assert_eq!(hist["approved_head"]["artifact_id"], json!(artifact_id));
    assert!(
        hist.get("supersedes_artifact_id").is_some(),
        "history 应显式携带版本链字段（可为 null）"
    );
    assert!(hist.get("superseded_by").is_some());
}

#[tokio::test]
async fn request_changes_and_reject_lifecycles_via_http() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // request_changes → 产物回到 draft。
    let (team_id, _, artifacts) = run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact_id = artifacts[0]["artifact_id"].as_str().unwrap().to_string();
    let body = review_body(&team_id, "request_changes", "critic", "idem-rc-1", None);
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201, "要求修改应 201：{resp}");
    assert_eq!(resp["artifact"]["review_state"], json!("draft"));
    assert!(
        resp["approved_head"].is_null(),
        "非 approve 不设 head：{resp}"
    );

    // reject → rejected。
    let (team_id2, _, artifacts2) = run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact_id2 = artifacts2[0]["artifact_id"].as_str().unwrap().to_string();
    let body = review_body(&team_id2, "reject", "human:u1", "idem-rej-1", None);
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id2}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201, "驳回应 201：{resp}");
    assert_eq!(resp["artifact"]["review_state"], json!("rejected"));

    // history 记录决定与评语。
    let (_, hist) = call(
        &state,
        &app,
        "GET",
        &format!("/artifacts/{artifact_id2}/history"),
        None,
    )
    .await;
    assert_eq!(hist["reviews"][0]["decision"], json!("reject"));
    assert_eq!(hist["reviews"][0]["comment"], json!("结构清晰，证据充分"));
}

#[tokio::test]
async fn idempotent_replay_via_http_has_zero_side_effects() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (team_id, _, artifacts) = run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact_id = artifacts[0]["artifact_id"].as_str().unwrap().to_string();

    let body = review_body(&team_id, "approve", "critic", "idem-same", None);
    let (status, first) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(first["replayed"], json!(false));

    // 同幂等键重放（即使请求声称不同决定）→ 200 回放，零副作用。
    let replay = json!({
        "team_id": team_id,
        "decision": "reject",
        "reviewer": "critic",
        "idempotency_key": "idem-same",
        "expected_version": 99,
    });
    let (status, second) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&replay.to_string()),
    )
    .await;
    assert_eq!(status, 200, "同键重放应 200：{second}");
    assert_eq!(second["replayed"], json!(true));
    assert_eq!(second["review"]["decision"], json!("approve"));

    // 历史仍只有一条；产物状态保持首次决定。
    let (_, hist) = call(
        &state,
        &app,
        "GET",
        &format!("/artifacts/{artifact_id}/history"),
        None,
    )
    .await;
    assert_eq!(hist["reviews"].as_array().unwrap().len(), 1);
    assert_eq!(hist["review_state"], json!("approved"));
}

#[tokio::test]
async fn idempotency_key_reused_on_other_artifact_conflicts_via_http() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (team_id, _, artifacts) = run_echo_team(&state, &app, relay_roles(), None).await;
    assert!(
        artifacts.len() >= 2,
        "接力应产生至少两个产物：{artifacts:?}"
    );
    let a = artifacts[0]["artifact_id"].as_str().unwrap().to_string();
    let b = artifacts[1]["artifact_id"].as_str().unwrap().to_string();

    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{a}/review"),
        Some(&review_body(
            &team_id,
            "approve",
            "critic",
            "idem-shared",
            None,
        )),
    )
    .await;
    assert_eq!(status, 201);

    // 同键用在另一产物 → 409。
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{b}/review"),
        Some(&review_body(
            &team_id,
            "approve",
            "critic",
            "idem-shared",
            None,
        )),
    )
    .await;
    assert_eq!(status, 409, "跨产物复用幂等键应 409：{resp}");
    assert!(
        resp["error"].as_str().unwrap_or("").contains("a"),
        "错误信息应指出冲突产物：{resp}"
    );
}

#[tokio::test]
async fn stale_expected_version_conflicts_via_http() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (team_id, _, artifacts) = run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact_id = artifacts[0]["artifact_id"].as_str().unwrap().to_string();
    let current_version = artifacts[0]["version"].as_u64().unwrap() as u32;

    // 旧页面提交（期望版本错误）→ 409 + 冲突明细。
    let body = review_body(
        &team_id,
        "approve",
        "critic",
        "idem-stale",
        Some(current_version + 5),
    );
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 409, "旧版本提交应 409：{resp}");
    assert_eq!(resp["current_version"], json!(current_version));
    assert_eq!(resp["expected_version"], json!(current_version + 5));

    // 409 提交不产生任何评审记录。
    let (_, hist) = call(
        &state,
        &app,
        "GET",
        &format!("/artifacts/{artifact_id}/history"),
        None,
    )
    .await;
    assert!(hist["reviews"].as_array().unwrap().is_empty());

    // 正确版本可提交。
    let body = review_body(
        &team_id,
        "approve",
        "critic",
        "idem-fresh",
        Some(current_version),
    );
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201, "正确版本应 201：{resp}");
}

#[tokio::test]
async fn producer_self_approve_forbidden_without_policy() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (team_id, _, artifacts) = run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact = &artifacts[0];
    let artifact_id = artifact["artifact_id"].as_str().unwrap().to_string();
    let producer = artifact["producer"].as_str().unwrap().to_string();

    // 生产者自批（缺省 human policy）→ 403。
    let body = review_body(&team_id, "approve", &producer, "idem-self-1", None);
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 403, "生产者自批应 403：{resp}");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .contains("self_review_allowed"),
        "403 信息应说明授权路径：{resp}"
    );

    // 生产者要求修改自己的产物（非自我背书）→ 允许。
    let body = review_body(&team_id, "request_changes", &producer, "idem-self-2", None);
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201, "生产者要求修改不属自批：{resp}");

    // Human 策略显式 self_review_allowed → 自批放行（第二个团队）。
    let (team_id2, _, artifacts2) =
        run_echo_team(&state, &app, planner_roles(), Some("self_review_allowed")).await;
    let artifact_id2 = artifacts2[0]["artifact_id"].as_str().unwrap().to_string();
    let producer2 = artifacts2[0]["producer"].as_str().unwrap().to_string();
    let body = review_body(&team_id2, "approve", &producer2, "idem-self-3", None);
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id2}/review"),
        Some(&body),
    )
    .await;
    assert_eq!(status, 201, "self_review_allowed 授权后自批应 201：{resp}");
}

#[tokio::test]
async fn review_404_and_400_boundaries() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (team_id, _, artifacts) = run_echo_team(&state, &app, planner_roles(), None).await;
    let artifact_id = artifacts[0]["artifact_id"].as_str().unwrap().to_string();

    // 未知产物 → 404。
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        "/artifacts/team-nope:planner:v1/review",
        Some(&review_body(
            &team_id, "approve", "critic", "idem-404", None,
        )),
    )
    .await;
    assert_eq!(status, 404, "未知产物应 404：{resp}");
    let (status, _) = call(
        &state,
        &app,
        "GET",
        "/artifacts/team-nope:planner:v1/history",
        None,
    )
    .await;
    assert_eq!(status, 404, "未知产物 history 应 404");

    // 未知团队 → 404。
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&review_body(
            "team-nope",
            "approve",
            "critic",
            "idem-404-team",
            None,
        )),
    )
    .await;
    assert_eq!(status, 404, "未知团队应 404：{resp}");

    // 未知 decision → 400。
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&review_body(&team_id, "maybe", "critic", "idem-400", None)),
    )
    .await;
    assert_eq!(status, 400, "未知 decision 应 400：{resp}");

    // 空 team_id → 400。
    let (status, resp) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&review_body("", "approve", "critic", "idem-400-team", None)),
    )
    .await;
    assert_eq!(status, 400, "空 team_id 应 400：{resp}");

    // missing required field（idempotency_key 缺失）→ axum Json 422。
    let body = json!({
        "team_id": team_id,
        "decision": "approve",
        "reviewer": "critic",
    });
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/artifacts/{artifact_id}/review"),
        Some(&body.to_string()),
    )
    .await;
    assert_eq!(status, 422, "缺幂等键应 422（Json extractor 语义）");
}
