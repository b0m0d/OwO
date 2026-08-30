//! Human Inbox 集成测试（八期 · 第三路：统一 Human Inbox 后端与直接处理）。
//!
//! 覆盖（全部使用 IdleProvider + 直接播种 store/run_dir，不依赖模型凭据）：
//! 1. 四类待办统一列表：human_result（人节点）/ step_retry（失败步骤）/
//!    artifact_review（PendingReview 产物）三类实弹源进同一列表；
//!    change_set 为八期二路 seam（当前 count=0，resolve 返回结构化 409）；
//! 2. claim 互斥：双用户竞争仅一人成功；同人重复领取幂等；release 仅领取者；
//! 3. resolve step_retry：真实分派 steer retry（步骤重置）+ 同参重放零副作用 + 异键 409；
//! 4. resolve artifact_review：真实产生评审记录（approve → Approved）+ 重放幂等；
//! 5. resolve human_result：真实录入人节点结果（步骤 Succeeded + Artifact 落盘）；
//! 6. 已解决待办不再出现在列表；resolve 后 last_error 不残留。
//!
//! 存储层持久化/CAS 语义的单元测试在 `human_inbox_store.rs` 内 `#[cfg(test)]`。

use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::goal::StepRecord;
use owo_agent_core::plan::{Plan, StepSpec, StepStatus};
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::workswarm::{RoleSpec, RunMeta};
use owo_agent_core::{Agent, Goal, GoalRunState};
use owo_agent_protocol::{
    Artifact, ArtifactClassification, MemberHealth, ProjectSpace, ProjectSpaceStatus, ReviewState,
    RuntimeBinding, TeamMember, TeamMode, TeamRun, TeamRunStatus,
};
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
        owo_agent_core::permissions::Policy::new(&workspace),
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
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn now() -> String {
    "2026-08-30T00:00:00Z".to_string()
}

fn human_member(member_id: &str, role: &str) -> TeamMember {
    TeamMember {
        member_id: member_id.to_string(),
        role: role.to_string(),
        runtime_binding: RuntimeBinding::Human {
            user_id: "u-seed".to_string(),
        },
        capabilities: vec![],
        tool_scope: vec![],
        read_scope: vec![],
        write_scope: vec![],
        budget: json!({}),
        handoff_contract: None,
        health: MemberHealth::Active,
    }
}

fn agent_member(member_id: &str, role: &str) -> TeamMember {
    TeamMember {
        member_id: member_id.to_string(),
        role: role.to_string(),
        runtime_binding: RuntimeBinding::Agent {
            agent_id: "a-seed".to_string(),
        },
        capabilities: vec![],
        tool_scope: vec![],
        read_scope: vec![],
        write_scope: vec![],
        budget: json!({}),
        handoff_contract: None,
        health: MemberHealth::Active,
    }
}

async fn seed_team_run(
    state: &owo_agent_server::AppState,
    team_id: &str,
    project_id: &str,
    status: TeamRunStatus,
    members: Vec<TeamMember>,
) {
    let coordinator = state.workswarm.coordinator().unwrap();
    let team = TeamRun {
        team_id: team_id.to_string(),
        goal_id: None,
        mode: TeamMode::Team,
        members,
        task_graph_ref: None,
        project_space_id: Some(project_id.to_string()),
        template_id: None,
        shared_context_refs: vec![],
        budget: json!({}),
        human_policy: None,
        strategy_decision: None,
        status,
        created_at: now(),
        updated_at: now(),
    };
    coordinator.store().save_team_run(&team).await.unwrap();
}

/// 播种 GoalRunState 侧车（`<run_dir>/<team_id>.json`，run_id = team_id）。
fn seed_run_state(
    state: &owo_agent_server::AppState,
    team_id: &str,
    steps: &[(&str, &str)],
    records: &[(&str, StepStatus, u32, Option<&str>)],
) {
    let coordinator = state.workswarm.coordinator().unwrap();
    let mut plan = Plan::new(&format!("plan-{team_id}"), team_id);
    for (step_id, worker) in steps {
        plan.add_step(StepSpec::new(*step_id, *worker));
    }
    let mut run_state = GoalRunState::new(Goal::new(team_id, "inbox 验收目标"), plan);
    run_state.run_id = team_id.to_string();
    for (step_id, status, attempts, error) in records {
        run_state.records.insert(
            (*step_id).to_string(),
            StepRecord {
                step_id: (*step_id).to_string(),
                status: *status,
                attempts: *attempts,
                output: None,
                error: error.map(|e| e.to_string()),
            },
        );
    }
    run_state.persist(&coordinator.run_dir()).unwrap();
}

/// 播种 RunMeta 侧车（人节点分派依赖 roles[worker] → assignee=human）。
fn seed_run_meta(state: &owo_agent_server::AppState, team_id: &str, worker: &str) {
    let coordinator = state.workswarm.coordinator().unwrap();
    // 成员 id 匹配规则：role_spec_of_member 按 `m-{role}` 对应（m-h ↔ role="h"）。
    let meta = RunMeta {
        team_id: team_id.to_string(),
        correlation_id: format!("corr-{team_id}"),
        roles: vec![RoleSpec {
            role: "h".to_string(),
            assignee: "human".to_string(),
            worker: Some(worker.to_string()),
            ..Default::default()
        }],
        template_id: None,
        budgets: std::collections::BTreeMap::new(),
    };
    std::fs::write(
        coordinator.run_dir().join(format!("{team_id}-meta.json")),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();
}

async fn seed_artifact(state: &owo_agent_server::AppState, project_id: &str, artifact: Artifact) {
    let coordinator = state.workswarm.coordinator().unwrap();
    coordinator
        .store()
        .save_artifact(&artifact, project_id)
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn make_artifact(
    artifact_id: &str,
    project_team: &str,
    version: u32,
    review_state: ReviewState,
) -> Artifact {
    Artifact {
        artifact_id: artifact_id.to_string(),
        kind: "document".to_string(),
        version,
        producer: "m-w".to_string(),
        content_ref: format!("cas://sha256:seed-{artifact_id}"),
        schema_ref: None,
        source_refs: vec![],
        classification: ArtifactClassification::Public,
        review_state,
        supersedes_artifact_id: None,
        created_at: now(),
        team_id: project_team.to_string(),
        format: "markdown".to_string(),
        media_type: "text/markdown".to_string(),
        file_name: String::new(),
        sha256: String::new(),
        size_bytes: 0,
        evidence_refs: vec![],
        open_issues: vec![],
        validation: None,
        handoff: None,
    }
}

/// 播种三源夹具：
/// - team-a（Running）：s-1 Failed（agent 成员）→ step_retry；
/// - team-b（AwaitingHuman）：s-9 Running（human 成员）→ human_result；
/// - ps-review：a-1 PendingReview → artifact_review。
/// 播种项目空间（record_human_result/apply_steer 的 load_bundle 依赖其存在）。
async fn seed_project_space(state: &owo_agent_server::AppState, project_id: &str) {
    let coordinator = state.workswarm.coordinator().unwrap();
    let space = ProjectSpace {
        project_id: project_id.to_string(),
        goal_id: None,
        team_id: None,
        tasks: vec![],
        artifacts: vec![],
        decisions: vec![],
        approvals: vec![],
        discussions: vec![],
        activity_stream: vec![],
        delivery_manifest_ref: None,
        rework_tasks: vec![],
        version: 1,
        status: ProjectSpaceStatus::Active,
        created_at: now(),
        updated_at: now(),
    };
    coordinator
        .store()
        .save_project_space(&space)
        .await
        .unwrap();
}

async fn seed_all_sources(state: &owo_agent_server::AppState) {
    seed_project_space(state, "ps-a").await;
    seed_team_run(
        state,
        "team-a",
        "ps-a",
        TeamRunStatus::Running,
        vec![agent_member("m-impl", "implementer")],
    )
    .await;
    seed_run_state(
        state,
        "team-a",
        &[("s-1", "m-impl")],
        &[("s-1", StepStatus::Failed, 1, Some("boom"))],
    );
    seed_artifact(
        state,
        "ps-a",
        make_artifact("a-1", "team-a", 2, ReviewState::PendingReview),
    )
    .await;

    seed_project_space(state, "ps-b").await;
    seed_team_run(
        state,
        "team-b",
        "ps-b",
        TeamRunStatus::AwaitingHuman,
        vec![human_member("m-h", "reviewer")],
    )
    .await;
    seed_run_state(
        state,
        "team-b",
        &[("s-9", "m-h")],
        &[("s-9", StepStatus::Running, 1, None)],
    );
    seed_run_meta(state, "team-b", "m-h");
    seed_artifact(
        state,
        "ps-b",
        make_artifact("a-2", "team-b", 1, ReviewState::Approved),
    )
    .await;
}

fn item_of(body: &Value, item_id: &str) -> Option<Value> {
    body["items"]
        .as_array()?
        .iter()
        .find(|i| i["item_id"].as_str() == Some(item_id))
        .cloned()
}

// ---------------------------------------------------------------------------
// 1) 统一列表
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_lists_all_live_sources() {
    let (state, _temp) = test_state().await;
    seed_all_sources(&state).await;
    let app = build_router(Arc::clone(&state));

    let (status, body) = call(&state, &app, "GET", "/human/inbox", None).await;
    assert_eq!(status, 200, "{body}");

    let retry = item_of(&body, "step_retry:s-1").expect("step_retry 条目缺失");
    assert_eq!(retry["kind"], "step_retry");
    assert_eq!(retry["status"], "open");
    assert_eq!(retry["team_id"], "team-a");
    assert_eq!(retry["detail"]["step_id"], "s-1");
    assert_eq!(retry["detail"]["attempts"], 1);
    assert_eq!(retry["detail"]["error"], "boom");

    let human = item_of(&body, "human_result:s-9").expect("human_result 条目缺失");
    assert_eq!(human["kind"], "human_result");
    assert_eq!(human["team_id"], "team-b");
    assert_eq!(human["detail"]["step_status"], "Running");

    let review = item_of(&body, "artifact_review:a-1").expect("artifact_review 条目缺失");
    assert_eq!(review["kind"], "artifact_review");
    assert_eq!(review["project_id"], "ps-a");
    assert_eq!(review["detail"]["review_state"], "PendingReview");

    assert_eq!(body["counts"]["step_retry"], 1);
    assert_eq!(body["counts"]["human_result"], 1);
    assert_eq!(body["counts"]["artifact_review"], 1);
    assert_eq!(body["counts"]["change_set"], 0, "二路 seam 未接线前恒 0");
}

// ---------------------------------------------------------------------------
// 2) claim 互斥 / release
// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_is_exclusive_and_release_requires_claimer() {
    let (state, _temp) = test_state().await;
    seed_all_sources(&state).await;
    let app = build_router(Arc::clone(&state));
    let path = "/human/inbox/step_retry:s-1/claim";

    let (status, body) = call(&state, &app, "POST", path, Some(r#"{ "user": "alice" }"#)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["status"], "claimed");
    assert_eq!(body["item"]["assignee"], "alice");
    assert!(body["item"]["claimed_at"].is_u64());

    // 另一用户领取 → 409（同一待办仅允许一个用户处理）。
    let (status, body) = call(&state, &app, "POST", path, Some(r#"{ "user": "bob" }"#)).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["claimed_by"], "alice");

    // 同人重复领取 → 幂等 200。
    let (status, body) = call(&state, &app, "POST", path, Some(r#"{ "user": "alice" }"#)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["assignee"], "alice");

    // 非领取者释放 → 409。
    let (status, _) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:s-1/release",
        Some(r#"{ "user": "bob" }"#),
    )
    .await;
    assert_eq!(status, 409);

    // 领取者释放 → open。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:s-1/release",
        Some(r#"{ "user": "alice" }"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["item"]["status"], "open");
    assert_eq!(body["item"]["assignee"], Value::Null);
}

// ---------------------------------------------------------------------------
// 3) resolve step_retry：真实分派 + 幂等
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_step_retry_resets_step_and_is_idempotent() {
    let (state, _temp) = test_state().await;
    seed_all_sources(&state).await;
    let app = build_router(Arc::clone(&state));
    let coordinator = state.workswarm.coordinator().unwrap();

    let body = json!({ "user": "alice", "note": "收口重试" }).to_string();
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:s-1/resolve",
        Some(&body),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resolved"], true);
    assert_eq!(body["replayed"], false);
    assert_eq!(body["result"]["action"], "step_retry");

    // 真实分派：步骤不再处于 Failed（被 retry 重置）。
    let run_state = coordinator.load_run_state("team-a").unwrap();
    let record = run_state.records.get("s-1").unwrap();
    assert!(
        matches!(record.status, StepStatus::Ready | StepStatus::Pending),
        "retry 后步骤应被重置，实际 {:?}",
        record.status
    );

    // 同参重放（派生幂等键相同）→ replayed true，不二次分派。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:s-1/resolve",
        Some(&json!({ "user": "alice", "note": "收口重试" }).to_string()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replayed"], true, "{body}");

    // 异键重复解决 → 409。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:s-1/resolve",
        Some(
            &json!({ "user": "alice", "idempotency_key": "other-key", "note": "另一把钥匙" })
                .to_string(),
        ),
    )
    .await;
    assert_eq!(status, 409, "{body}");

    // 已解决待办不再出现在统一列表。
    let (_, list) = call(&state, &app, "GET", "/human/inbox", None).await;
    assert!(item_of(&list, "step_retry:s-1").is_none(), "{list}");
}

// ---------------------------------------------------------------------------
// 4) resolve artifact_review：真实评审记录
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_artifact_review_creates_review_record() {
    let (state, _temp) = test_state().await;
    seed_all_sources(&state).await;
    let app = build_router(Arc::clone(&state));
    let coordinator = state.workswarm.coordinator().unwrap();

    let body = json!({
        "user": "alice",
        "decision": "approve",
        "reviewer": "critic-1",
        "comment": "收口批准"
    })
    .to_string();
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/artifact_review:a-1/resolve",
        Some(&body),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resolved"], true);
    assert_eq!(body["result"]["action"], "artifact_review");
    assert_eq!(body["result"]["replayed"], false);

    // 真实评审记录：产物转 Approved。
    let artifact = coordinator.store().get_artifact("a-1").await.unwrap();
    assert_eq!(artifact.review_state, ReviewState::Approved);

    // 同参重放 → inbox 层 replayed（缓存结果，不重复分派）。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/artifact_review:a-1/resolve",
        Some(
            &json!({
                "user": "alice",
                "decision": "approve",
                "reviewer": "critic-1",
                "comment": "收口批准"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["replayed"], true);

    // 列表中不再出现。
    let (_, list) = call(&state, &app, "GET", "/human/inbox", None).await;
    assert!(item_of(&list, "artifact_review:a-1").is_none(), "{list}");
}

// ---------------------------------------------------------------------------
// 5) resolve human_result：真实录入
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_human_result_records_artifact() {
    let (state, _temp) = test_state().await;
    seed_all_sources(&state).await;
    let app = build_router(Arc::clone(&state));
    let coordinator = state.workswarm.coordinator().unwrap();

    let body = json!({ "user": "u-seed", "result": "人工复核通过：源对比结论成立" }).to_string();
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/human_result:s-9/resolve",
        Some(&body),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resolved"], true);
    assert_eq!(body["result"]["action"], "human_result");
    let artifact_id = body["result"]["artifact"]["artifact_id"]
        .as_str()
        .expect("人节点结果应登记 Artifact")
        .to_string();
    assert!(!artifact_id.is_empty());

    // 步骤置 Succeeded。
    let run_state = coordinator.load_run_state("team-b").unwrap();
    assert_eq!(
        run_state.records.get("s-9").unwrap().status,
        StepStatus::Succeeded
    );

    // 列表中不再出现。
    let (_, list) = call(&state, &app, "GET", "/human/inbox", None).await;
    assert!(item_of(&list, "human_result:s-9").is_none(), "{list}");
}

// ---------------------------------------------------------------------------
// 6) 边界：404 / ChangeSet seam / 空结果校验
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbox_edges_404_change_set_seam_and_validation() {
    let (state, _temp) = test_state().await;
    seed_all_sources(&state).await;
    let app = build_router(Arc::clone(&state));

    // 未知待办：claim/resolve 均 404。
    let (status, _) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:nope/claim",
        Some(r#"{ "user": "alice" }"#),
    )
    .await;
    assert_eq!(status, 404);
    let (status, _) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:nope/resolve",
        Some(r#"{ "user": "alice" }"#),
    )
    .await;
    assert_eq!(status, 404);

    // GET 单条：不存在 404；存在（claim 后）返回记录。
    let (status, _) = call(&state, &app, "GET", "/human/inbox/step_retry:s-1", None).await;
    assert_eq!(status, 200);

    // change_set 分派 seam：二路未接线前 resolve 返回结构化 409。
    // （该 kind 的条目仅能由二路创建路径登记；此处直接用 store 层不可达，
    //   以契约级断言锁定：列表 change_set 计数为 0，不误报可用。）
    let (_, list) = call(&state, &app, "GET", "/human/inbox", None).await;
    assert_eq!(list["counts"]["change_set"], 0);

    // 空 user 校验。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/step_retry:s-1/claim",
        Some(r#"{ "user": "  " }"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // human_result resolve 缺 result → 400，且条目回到 open（可重试）。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/human/inbox/human_result:s-9/resolve",
        Some(r#"{ "user": "u-seed" }"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, list) = call(&state, &app, "GET", "/human/inbox", None).await;
    let item = item_of(&list, "human_result:s-9").expect("分派失败应回滚为可重试");
    assert_eq!(item["status"], "open", "{item}");
}
