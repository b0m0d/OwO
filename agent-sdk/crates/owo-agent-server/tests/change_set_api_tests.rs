//! ChangeSet 审批、接受与安全撤销 HTTP 集成测试（八期 · 二路）。
//!
//! 覆盖：生命周期（pending_review → accepted/rejected/reverted/conflicted）、
//! accept 后保留修改并解除批准门控、reject 从 CAS 恢复基线内容、幂等键重放零
//! 副作用、已决定后跨动作 409、用户改过文件后 revert 409 + conflicted 不覆盖、
//! 新建文件的删除恢复、多文件恢复只动变更集合内的文件、404/422 边界。
//!
//! 全部走真实路由（bearer + tower oneshot；存储直连 coordinator 的 run_dir/CAS，
//! 不依赖模型凭据）。

use owo_agent_core::change_set::snapshot_allowed_paths;
use owo_agent_core::change_set_store::ChangeSetStore;
use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_protocol::{ChangeSet, ChangeSetStatus};
use owo_agent_server::build_router;
use serde_json::{json, Value};
use std::path::PathBuf;
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
    if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder
            .body(axum::body::Body::from(body.to_string()))
            .unwrap();
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

fn run_dir_of(state: &owo_agent_server::AppState) -> PathBuf {
    state
        .workswarm
        .coordinator()
        .expect("workswarm 协调器初始化")
        .run_dir()
        .to_path_buf()
}

fn cas_of(state: &owo_agent_server::AppState) -> owo_agent_core::cas_store::CasStore {
    state.workswarm.coordinator().unwrap().cas().clone()
}

fn workspace_of(state: &owo_agent_server::AppState) -> PathBuf {
    state.workspace.clone()
}

/// 造一个「Worker 已改写文件」的现场：基线快照（进 coordinator CAS）→ 修改 →
/// 生成并落盘 ChangeSet。返回 change_set_id。
async fn seed_change_set(
    state: &owo_agent_server::AppState,
    team_id: &str,
    relative: &str,
    base_content: &str,
    worker_content: &str,
) -> String {
    let root = workspace_of(state);
    let cas = cas_of(state);
    std::fs::create_dir_all(root.join(relative).parent().unwrap()).unwrap();
    std::fs::write(root.join(relative), base_content).unwrap();
    let base = snapshot_allowed_paths(&root, &[], &cas).await;
    std::fs::write(root.join(relative), worker_content).unwrap();
    let change_set = owo_agent_core::change_set::build_change_set(
        team_id,
        "s-implementer",
        "implementer",
        &base,
        &[relative.to_string()],
        &root,
        None,
    );
    ChangeSetStore::new(&run_dir_of(state))
        .save_upsert(&change_set)
        .unwrap();
    change_set.change_set_id
}

fn sample_change_set(id: &str, team_id: &str) -> ChangeSet {
    ChangeSet {
        change_set_id: id.to_string(),
        team_id: team_id.to_string(),
        step_id: "s1".to_string(),
        role: "implementer".to_string(),
        base_hashes: vec![],
        result_hashes: vec![],
        changed_files: vec![],
        diff_ref: None,
        status: ChangeSetStatus::PendingReview,
        created_at: "2026-01-01T00:00:00+00:00".to_string(),
        decision: None,
        conflicts: vec![],
    }
}

const TEAM: &str = "team-cstest";

#[tokio::test]
async fn change_set_lifecycle_accept_replay_and_gate_relief() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let change_set_id =
        seed_change_set(&state, TEAM, "src/lib.rs", "base-content", "worker-content").await;

    // 列表：pending_review + 批准门控生效。
    let (status, body) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{TEAM}/change-sets"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["change_sets"].as_array().unwrap().len(), 1);
    assert_eq!(body["approval_blocked"], json!(true));
    assert!(
        body["approval_block_reason"]
            .as_str()
            .unwrap()
            .contains("未接受 ChangeSet"),
        "门控原因应可展示：{body}"
    );

    // 单查 / 404 / 422 边界。
    let (status, body) = call(
        &state,
        &app,
        "GET",
        &format!("/change-sets/{change_set_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["change_set"]["status"], json!("pending_review"));
    let (status, _) = call(&state, &app, "GET", "/change-sets/cs-unknown-1-2", None).await;
    assert_eq!(status, 404);
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some("{}"),
    )
    .await;
    assert_eq!(status, 422, "缺 idempotency_key 走 extractor 422");

    // accept：保留文件现状（内容不动），门控解除。
    let root = workspace_of(&state);
    let before = std::fs::read(root.join("src/lib.rs")).unwrap();
    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(r#"{ "idempotency_key": "accept-1" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["replayed"], json!(false));
    assert_eq!(body["change_set"]["status"], json!("accepted"));
    assert_eq!(
        std::fs::read(root.join("src/lib.rs")).unwrap(),
        before,
        "accept 不得改动文件"
    );

    // 同动作重放（同键/异键）→ 零副作用。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(r#"{ "idempotency_key": "accept-1" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["replayed"], json!(true));
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(r#"{ "idempotency_key": "accept-2" }"#),
    )
    .await;
    assert_eq!(status, 200);

    // 已接受后跨动作 → 409。
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/reject"),
        Some(r#"{ "idempotency_key": "reject-1" }"#),
    )
    .await;
    assert_eq!(status, 409);

    // 门控解除。
    let (_, body) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{TEAM}/change-sets"),
        None,
    )
    .await;
    assert_eq!(body["approval_blocked"], json!(false));
    assert!(body["approval_block_reason"].is_null());
}

#[tokio::test]
async fn reject_restores_base_content_from_cas_and_replay_is_noop() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let change_set_id =
        seed_change_set(&state, TEAM, "src/a.rs", "base-content", "worker-content").await;
    let root = workspace_of(&state);
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "worker-content"
    );

    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/reject"),
        Some(r#"{ "idempotency_key": "reject-1", "note": "不符合要求" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["replayed"], json!(false));
    assert_eq!(body["change_set"]["status"], json!("rejected"));
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "base-content",
        "reject 应从 CAS 恢复基线内容"
    );

    // 重复 reject → 零副作用（文件不再被触碰、状态不变）。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/reject"),
        Some(r#"{ "idempotency_key": "reject-2" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["replayed"], json!(true));
    assert_eq!(
        std::fs::read_to_string(root.join("src/a.rs")).unwrap(),
        "base-content"
    );

    // 已拒绝后 accept → 409。
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(r#"{ "idempotency_key": "accept-1" }"#),
    )
    .await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn revert_conflict_returns_409_marks_conflicted_and_never_overwrites() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let change_set_id = seed_change_set(&state, TEAM, "docs/b.md", "base-md", "worker-md").await;
    let root = workspace_of(&state);
    // 用户在撤销前又改了内容。
    std::fs::write(root.join("docs/b.md"), "user-edit").unwrap();

    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/revert"),
        Some(r#"{ "idempotency_key": "revert-1" }"#),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["conflicts"], json!(["docs/b.md"]));
    assert_eq!(body["change_set"]["status"], json!("conflicted"));
    assert_eq!(
        std::fs::read_to_string(root.join("docs/b.md")).unwrap(),
        "user-edit",
        "冲突时不得覆盖用户新内容"
    );

    // conflicted 状态下：用户把文件改回结果状态 → 重试 reject 可恢复到基线。
    std::fs::write(root.join("docs/b.md"), "worker-md").unwrap();
    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/reject"),
        Some(r#"{ "idempotency_key": "reject-after-conflict" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["change_set"]["status"], json!("rejected"));
    assert_eq!(
        std::fs::read_to_string(root.join("docs/b.md")).unwrap(),
        "base-md"
    );
}

#[tokio::test]
async fn revert_deletes_created_file() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    // 基线阶段文件不存在（新建文件：base = (None, true)，恢复即删除）：
    // 先快照，再模拟 Worker 创建文件。
    let root = workspace_of(&state);
    let base = snapshot_allowed_paths(&root, &[], &cas_of(&state)).await;
    std::fs::create_dir_all(root.join("out")).unwrap();
    std::fs::write(root.join("out/new.md"), "created").unwrap();
    assert!(!base.scanned.contains("out/new.md"), "基线阶段文件尚未创建");
    let change_set = owo_agent_core::change_set::build_change_set(
        TEAM,
        "s-implementer",
        "implementer",
        &base,
        &["out/new.md".to_string()],
        &root,
        None,
    );
    ChangeSetStore::new(&run_dir_of(&state))
        .save_upsert(&change_set)
        .unwrap();
    let change_set_id = change_set.change_set_id;

    let (status, body) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/revert"),
        Some(r#"{ "idempotency_key": "revert-created" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["change_set"]["status"], json!("reverted"));
    assert!(!root.join("out/new.md").exists(), "新建文件应被删除恢复");
}

#[tokio::test]
async fn list_unknown_team_returns_empty_and_gate_off() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (status, body) = call(
        &state,
        &app,
        "GET",
        "/teams/team-never-existed/change-sets",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["change_sets"], json!([]));
    assert_eq!(body["approval_blocked"], json!(false));
}

#[tokio::test]
async fn multi_file_change_set_reject_restores_only_its_files() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let root = workspace_of(&state);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/one.rs"), "one-base").unwrap();
    std::fs::write(root.join("src/two.rs"), "two-base").unwrap();
    std::fs::write(root.join("untouched.txt"), "keep").unwrap();
    let base = snapshot_allowed_paths(&root, &[], &cas_of(&state)).await;
    // Worker：改 one.rs、新建 three.md；two.rs/untouched.txt 不动。
    std::fs::write(root.join("src/one.rs"), "one-worker").unwrap();
    std::fs::write(root.join("src/three.md"), "three-worker").unwrap();
    let change_set = owo_agent_core::change_set::build_change_set(
        TEAM,
        "s-implementer",
        "implementer",
        &base,
        &["src/one.rs".to_string(), "src/three.md".to_string()],
        &root,
        None,
    );
    ChangeSetStore::new(&run_dir_of(&state))
        .save_upsert(&change_set)
        .unwrap();
    let change_set_id = change_set.change_set_id;

    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/reject"),
        Some(r#"{ "idempotency_key": "reject-multi" }"#),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        std::fs::read_to_string(root.join("src/one.rs")).unwrap(),
        "one-base",
        "被改文件恢复基线"
    );
    assert!(!root.join("src/three.md").exists(), "新建文件删除恢复");
    assert_eq!(
        std::fs::read_to_string(root.join("untouched.txt")).unwrap(),
        "keep",
        "变更集合之外的文件不受影响"
    );
    // two.rs 未在变更集合内，即使文件存在也不受影响。
    assert_eq!(
        std::fs::read_to_string(root.join("src/two.rs")).unwrap(),
        "two-base"
    );
}

#[tokio::test]
async fn store_isolates_teams_by_sidecar_file() {
    let (state, _temp) = test_state().await;
    let run_dir = run_dir_of(&state);
    let store = ChangeSetStore::new(&run_dir);
    store
        .save_upsert(&sample_change_set("cs-team-x-s1-1", "team-x"))
        .unwrap();
    store
        .save_upsert(&sample_change_set("cs-team-y-s1-2", "team-y"))
        .unwrap();
    assert!(run_dir.join("team-x-change-sets.json").exists());
    assert!(run_dir.join("team-y-change-sets.json").exists());
    assert_eq!(store.list_for_team("team-x").unwrap().len(), 1);
    assert_eq!(store.list_for_team("team-y").unwrap().len(), 1);
}
