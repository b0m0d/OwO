//! 十期 · 四路：执行安全、取消、幂等与崩溃恢复的 HTTP 契约测试
//! （`v1_execution_safety_tests.rs`，四路新建）。
//!
//! 覆盖（全部使用内置 echo/sleep/fail worker，零模型凭据）：
//! - R2 取消链：运行中取消 → 终态 cancelled 且无活动循环/孤儿步骤；
//!   取消幂等（重复取消零副作用）；成功后取消不改状态不丢产物；
//!   取消只影响未完成步骤（已完成步骤产物保留）；取消后 retry 单步恢复。
//! - R4 checkpoint 恢复：磁盘 Running 无活动循环 → interrupted 标记（绝不
//!   静默重放）；continue 显式恢复并清标记；已完成步骤不重跑（attempts 不变）；
//!   重启（新 AppState 同数据目录）产物与状态可读；并发重复 steer 单一执行者。
//! - R3 幂等：ChangeSet 决定同键重放零二次副作用；跨动作 409；重复 continue 空操作。
//! - R5 完成安全：预算超时干净失败无孤儿；终态后指标不再增长；任务视图全终态。
//!
//! 与 ChangeSet 异常闭环（写入后失败/取消中写入/非 Git 内容哈希/连续两写 Worker
//! 租约隔离）对应的 4 个场景在 `workswarm_api.rs` 的单元测试层验证（需要
//! TrackedRoleWorker + 真实落盘 worker，HTTP 面无法注入写 worker）。

use owo_agent_core::change_set::{build_change_set, snapshot_allowed_paths};
use owo_agent_core::change_set_store::ChangeSetStore;
use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::permissions::Policy;
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_protocol::TeamRunStatus;
use owo_agent_server::build_router;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

/// 无外部依赖的最小模型 Provider（本测试只用 echo/sleep/fail worker，模型调用即失败）。
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

/// 轮询 GET /teams/{id} 直到状态进入目标集合（超时返回最后的值）。
async fn poll_team_status(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    team_id: &str,
    want: &[&str],
    timeout: std::time::Duration,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = json!({});
    loop {
        let (status, body) = call(state, app, "GET", &format!("/teams/{team_id}"), None).await;
        assert!(
            status == 200,
            "GET /teams/{team_id} 应 200（实际 {status}：{body}）"
        );
        let st = body["team"]["status"].as_str().unwrap_or("").to_string();
        last = body;
        if want.contains(&st.as_str()) {
            return last;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("等待团队状态 {want:?} 超时，最后状态：{st}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn create_team(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    roles: Value,
) -> String {
    create_team_full(state, app, roles, None, json!(null)).await
}

/// 带可选 budget 的建队。
async fn create_team_full(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    roles: Value,
    workspace: Option<Value>,
    budget: Value,
) -> String {
    let mut obj = json!({
        "objective": "R5 执行安全契约测试",
        "mode": "team",
        "roles": roles,
        "budget": budget,
    });
    if let Some(ws) = workspace {
        obj["workspace"] = ws;
    }
    let body = obj.to_string();
    let (status, created) = call(state, app, "POST", "/teams", Some(&body)).await;
    assert_eq!(status, 202, "POST /teams 应 202：{created}");
    created["team_id"].as_str().unwrap().to_string()
}

fn steer_body(command: &str, extra: Value) -> String {
    let mut obj = json!({ "command": command, "note": "R5 测试" });
    if let (Some(a), Some(b)) = (obj.as_object_mut(), extra.as_object()) {
        for (k, v) in b {
            a.insert(k.clone(), v.clone());
        }
    }
    obj.to_string()
}

fn sleep_role(ms: u64) -> Value {
    json!([{ "role": "runner", "assignee": "agent", "worker": "sleep",
             "extra_input": { "ms": ms }, "verify": "non_empty" }])
}

// ---------------------------------------------------------------------------
// R2 取消链
// ---------------------------------------------------------------------------

/// 运行中取消 → 终态 cancelled；任务视图无 running 步骤；无活动循环。
#[tokio::test]
async fn cancel_mid_run_reaches_cancelled_terminal_no_orphan() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(3000)).await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(status, 200, "cancel 应 200：{res}");

    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(detail["interrupted"], json!(false), "{detail}");
    // 无 running 步骤残留。
    for task in detail["tasks"].as_array().unwrap() {
        let st = task["status"].as_str().unwrap();
        assert!(
            !["Running", "Pending"].contains(&st),
            "取消后不得残留未终态步骤：{task:?}"
        );
    }
    // 再取一次仍稳定（无活动循环/孤儿）。
    let (_, again) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(again["team"]["status"], json!("cancelled"));
    assert_eq!(again["interrupted"], json!(false));
}

/// 重复取消幂等：第二次取消零副作用（状态与审计不再变化）。
#[tokio::test]
async fn cancel_is_idempotent_no_double_side_effect() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(2000)).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (s1, r1) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(s1, 200, "{r1}");
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;

    let (_, before) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    let audit_before = before["audit_tail"].as_array().unwrap().len();

    let (s2, r2) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(s2, 200, "重复取消应可达成：{r2}");
    let (_, after) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(after["team"]["status"], json!("cancelled"));
    assert!(
        after["audit_tail"].as_array().unwrap().len() <= audit_before + 1,
        "重复取消不得产生大量新审计：{before:?} → {after:?}"
    );
}

/// 成功后取消：状态保持 succeeded、产物不丢。
#[tokio::test]
async fn cancel_after_success_keeps_artifacts_and_state() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "runner", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "done" }, "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(detail["team"]["status"], json!("succeeded"));
    let project_id = detail["team"]["project_space_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, arts0) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    let artifacts0 = arts0["artifacts"].as_array().unwrap().len();

    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(status, 200, "终态取消也应可达成（空操作）：{res}");
    let (_, after) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(
        after["team"]["status"],
        json!("succeeded"),
        "成功后取消不改状态"
    );
    let (_, arts1) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    assert_eq!(
        arts1["artifacts"].as_array().unwrap().len(),
        artifacts0,
        "取消不得删除已完成产物"
    );
}

/// 两步骤接力：取消只影响未完成步骤，已完成步骤产物保留。
#[tokio::test]
async fn cancel_preserves_completed_step_artifact() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "planner", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "plan ok" }, "verify": "non_empty" },
        { "role": "builder", "assignee": "agent", "worker": "sleep",
          "extra_input": { "ms": 4000 }, "depends_on": ["planner"], "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;

    // 等 planner 完成（builder 在睡眠中）。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (_, body) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
        let planner = body["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["role"].as_str() == Some("planner"));
        if let Some(p) = planner {
            if p["status"].as_str() == Some("Succeeded") {
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "planner 未在期限内成功"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // 等 planner 产物已落盘（缓存/异步写盘窗口）。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(status, 200);

    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let project_id = detail["team"]["project_space_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, arts) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    // 已完成步骤（planner）的产物必须保留——取消只影响未完成工作。
    let planner_artifacts = arts["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| {
            a["producer"]
                .as_str()
                .is_some_and(|p| p.contains("planner"))
        })
        .count();
    assert!(planner_artifacts >= 1, "已完成步骤产物必须保留：{arts}");
    // 取消语义：worker 先收到通知再做有界清理——若 sleep 在清理宽限内自然结束，
    // 该步骤算协作完成（Closeout 照常执行）；团队终态必须是 cancelled。
    // 这里只断言终态与已完成步骤产物，不假设在飞步骤的中间结局。
    assert_eq!(detail["team"]["status"], json!("cancelled"), "{detail}");
}

/// 取消后对失败步骤 retry：只重置目标步骤（+未完成下游），已完成步骤产物保留。
#[tokio::test]
async fn retry_after_cancel_resets_only_failed_step() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "planner", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "plan ok" }, "verify": "non_empty" },
        { "role": "builder", "assignee": "agent", "worker": "fail",
          "depends_on": ["planner"], "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(detail["team"]["status"], json!("failed"));

    // replace 内层 worker fail → echo + retry builder。
    let project_id = detail["team"]["project_space_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, arts0) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    let _ = arts0;

    let (s1, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body(
            "replace",
            json!({ "role": "builder", "new_worker": "echo" }),
        )),
    )
    .await;
    assert_eq!(s1, 200);
    let (s2, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("retry", json!({ "step_id": "s-builder" }))),
    )
    .await;
    assert_eq!(s2, 200, "retry 应 200");

    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        done["team"]["status"],
        json!("succeeded"),
        "retry 后团队应成功：{done}"
    );
    // planner（已完成步骤）attempts 应为 1：未被重置重跑。
    let planner = done["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["role"].as_str() == Some("planner"))
        .expect("planner 步骤");
    assert_eq!(
        planner["attempts"],
        json!(1),
        "已完成步骤不得重跑：{planner:?}"
    );
}

// ---------------------------------------------------------------------------
// R4 checkpoint 恢复（中断识别 / continue / 重启）
// ---------------------------------------------------------------------------

/// 磁盘 Running 无活动循环 → interrupted 标记；同键重复读取不产生自动重放。
#[tokio::test]
async fn interrupted_leftover_is_surfaced_never_auto_replayed() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "runner", "assignee": "agent", "worker": "sleep",
          "extra_input": { "ms": 120 }, "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;

    // 模拟进程崩溃遗留：磁盘状态改回 Running（无活动循环）。
    let ws_dir = temp.path().join("workswarm");
    let store = SqliteProjectSpaceStore::open(&ws_dir.join("space.db")).unwrap();
    let mut team = store.get_team_run(&team_id).await.unwrap().clone();
    assert!(team.status.is_terminal());
    team.status = TeamRunStatus::Running;
    store.save_team_run(&team).await.unwrap();
    drop(store);

    let (status, body) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["interrupted"], true, "重启遗留应被识别为中断：{body}");
    assert_eq!(
        body["team"]["status"].as_str().unwrap(),
        "running",
        "磁盘状态保持 Running 展示：{body}"
    );

    // 再读一次：绝不静默重放（仍 interrupted，无自动成功/失败转变）。
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (_, body2) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(body2["interrupted"], true, "{body2}");
    assert_eq!(
        body2["team"]["status"].as_str().unwrap(),
        "running",
        "读操作不得自动重放执行：{body2}"
    );

    // 显式 continue 恢复。
    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("continue", json!({}))),
    )
    .await;
    assert_eq!(status, 200, "continue 应 200：{res}");
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed", "cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        done["interrupted"],
        json!(false),
        "continue 后标记清除：{done}"
    );
}

/// 两步骤接力 1 已完成 + 2 中断：continue 只补齐未完成，已完成 attempts 不变。
#[tokio::test]
async fn continue_after_interrupted_reruns_only_incomplete_work() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "planner", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "plan ok" }, "verify": "non_empty" },
        { "role": "builder", "assignee": "agent", "worker": "sleep",
          "extra_input": { "ms": 200 }, "depends_on": ["planner"], "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let planner_attempts_before = detail["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["role"].as_str() == Some("planner"))
        .unwrap()["attempts"]
        .as_u64()
        .unwrap();

    // 模拟崩溃：整体改回 Running（planner 已完成仍在 records 中）。
    let ws_dir = temp.path().join("workswarm");
    let store = SqliteProjectSpaceStore::open(&ws_dir.join("space.db")).unwrap();
    let mut team = store.get_team_run(&team_id).await.unwrap().clone();
    team.status = TeamRunStatus::Running;
    store.save_team_run(&team).await.unwrap();
    drop(store);

    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("continue", json!({}))),
    )
    .await;
    assert_eq!(status, 200);
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed", "cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("succeeded"), "{done}");
    let planner_after = done["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["role"].as_str() == Some("planner"))
        .unwrap()["attempts"]
        .as_u64()
        .unwrap();
    assert_eq!(
        planner_after, planner_attempts_before,
        "已完成步骤 continue 后不得重跑（attempts 不变）"
    );
    // 产物仍在且不重复（已完成的 planner 产物只一份）。
    let project_id = done["team"]["project_space_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, arts) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    let planner_arts = arts["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| {
            a["producer"]
                .as_str()
                .is_some_and(|p| p.contains("planner"))
        })
        .count();
    assert_eq!(planner_arts, 1, "已完成步骤产物不重复：{arts}");
}

/// 重启（同数据目录新建 AppState）：终态、产物、待办决策均可读。
#[tokio::test]
async fn artifacts_and_state_survive_restart_new_app_state() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "runner", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "done" }, "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let project_id = detail["team"]["project_space_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 同数据目录重启：新协调器（无进程内账本）从磁盘读状态。
    let workspace = temp.path().join("ws2");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent2 = Agent::new(
        Arc::new(IdleProvider),
        ToolRegistry::new(),
        Policy::new(&workspace),
        Default::default(),
    );
    let store2 = SqliteSessionStore::open(&workspace.join("index.db")).unwrap();
    let state2 = Arc::new(owo_agent_server::AppState::new(
        agent2,
        store2,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    let app2 = build_router(Arc::clone(&state2));

    let (status, body) = call(&state2, &app2, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(status, 200, "重启后团队应可读：{body}");
    assert_eq!(body["team"]["status"], json!("succeeded"), "{body}");
    assert_eq!(body["interrupted"], json!(false), "{body}");

    let (status, arts) = call(
        &state2,
        &app2,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{arts}");
    assert!(
        !arts["artifacts"].as_array().unwrap().is_empty(),
        "重启后产物必须可读：{arts}"
    );
}

/// 并发重复 retry：单一执行者——一个生效，另一个 409，且无二次副作用。
#[tokio::test]
async fn concurrent_duplicate_steer_retry_single_executor() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "runner", "assignee": "agent", "worker": "fail",
          "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["failed"],
        std::time::Duration::from_secs(30),
    )
    .await;

    // 替换为 echo 后并发两发 retry。
    let (sr, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body(
            "replace",
            json!({ "role": "runner", "new_worker": "echo" }),
        )),
    )
    .await;
    assert_eq!(sr, 200);
    let body = steer_body("retry", json!({ "step_id": "s-runner" }));
    let path = format!("/teams/{team_id}/steer");
    let (r1, r2) = tokio::join!(
        call(&state, &app, "POST", &path, Some(&body)),
        call(&state, &app, "POST", &path, Some(&body))
    );
    let codes = [r1.0, r2.0];
    assert!(
        codes.contains(&200) && codes.contains(&409),
        "并发重复 retry 应一成一拒：{codes:?}（{r1:?} / {r2:?}）"
    );

    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("succeeded"), "{done}");
    let attempt = done["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["role"].as_str() == Some("runner"))
        .unwrap()["attempts"]
        .as_u64()
        .unwrap();
    assert!(
        attempt <= 3,
        "并发重复 retry 不得产生多余执行（attempts={attempt}）：{done}"
    );
}

// ---------------------------------------------------------------------------
// R3 幂等：ChangeSet 决定重放 / 跨动作冲突 / 重复 continue
// ---------------------------------------------------------------------------

fn run_dir_of(state: &owo_agent_server::AppState) -> PathBuf {
    state
        .workswarm
        .coordinator()
        .unwrap()
        .run_dir()
        .to_path_buf()
}

fn workspace_of(state: &owo_agent_server::AppState) -> PathBuf {
    state.workspace.clone()
}

fn cas_of(state: &owo_agent_server::AppState) -> owo_agent_core::cas_store::CasStore {
    state.workswarm.coordinator().unwrap().cas().clone()
}

/// 造一个「Worker 已改写文件」的 ChangeSet（HTTP 面验幂等用）。
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
    let change_set = build_change_set(
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

/// 同键重放 accept：零二次副作用（文件内容、决策记录数均不变）。
#[tokio::test]
async fn change_set_accept_replay_same_key_no_double_write() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let team_id = "team-r3-replay";
    let change_set_id = seed_change_set(&state, team_id, "src/lib.rs", "base", "worker-v1").await;

    let body = r#"{ "idempotency_key": "accept-1" }"#;
    let (s1, r1) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(body),
    )
    .await;
    assert_eq!(s1, 200, "{r1}");

    // 同键重放：replayed + 状态不变、文件不被重写。
    let root = workspace_of(&state);
    let before = std::fs::read(root.join("src/lib.rs")).unwrap();
    let (s2, r2) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(body),
    )
    .await;
    assert_eq!(s2, 200, "{r2}");
    assert_eq!(r2["replayed"], json!(true), "同键重放应标记 replayed：{r2}");
    let after = std::fs::read(root.join("src/lib.rs")).unwrap();
    assert_eq!(before, after, "重放不得再次落盘/写文件");

    let store = ChangeSetStore::new(&run_dir_of(&state));
    let set = store.find(&change_set_id).unwrap().unwrap();
    assert_eq!(
        set.status,
        owo_agent_protocol::ChangeSetStatus::Accepted,
        "重放后状态仍为 accepted"
    );
    assert_eq!(
        set.decision.as_ref().map(|d| d.action.as_str()),
        Some("accept")
    );
}

/// 已决定后跨动作 → 409，且不改变任何状态。
#[tokio::test]
async fn change_set_cross_action_conflict_409() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let team_id = "team-r3-cross";
    let change_set_id = seed_change_set(&state, team_id, "src/lib.rs", "base", "worker-v1").await;

    let (s1, _) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/accept"),
        Some(r#"{ "idempotency_key": "accept-1" }"#),
    )
    .await;
    assert_eq!(s1, 200);
    let root = workspace_of(&state);
    let before = std::fs::read(root.join("src/lib.rs")).unwrap();

    let (s2, r2) = call(
        &state,
        &app,
        "POST",
        &format!("/change-sets/{change_set_id}/reject"),
        Some(r#"{ "idempotency_key": "reject-1" }"#),
    )
    .await;
    assert_eq!(s2, 409, "已 accept 再 reject 应 409：{r2}");
    let after = std::fs::read(root.join("src/lib.rs")).unwrap();
    assert_eq!(before, after, "409 不得触发任何文件操作");

    let store = ChangeSetStore::new(&run_dir_of(&state));
    let set = store.find(&change_set_id).unwrap().unwrap();
    assert_eq!(
        set.decision.as_ref().map(|d| d.action.as_str()),
        Some("accept")
    );
}

/// 重复 continue：空操作，不产生新执行与新产物。
#[tokio::test]
async fn duplicate_continue_is_noop_no_new_side_effects() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let roles = json!([
        { "role": "runner", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "done" }, "verify": "non_empty" }
    ]);
    let team_id = create_team(&state, &app, roles).await;
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let project_id = detail["team"]["project_space_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, arts0) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    let art_count = arts0["artifacts"].as_array().unwrap().len();

    let (s1, r1) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("continue", json!({}))),
    )
    .await;
    // 已成功团队不可 continue：409（不允许重放已完成运行）——这就是终态幂等护栏。
    assert_eq!(s1, 409, "已成功团队 continue 应 409：{r1}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (s2, r2) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("continue", json!({}))),
    )
    .await;
    assert_eq!(s2, 409, "重复 continue 同样 409：{r2}");

    let (_, after) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(after["team"]["status"], json!("succeeded"));
    let (_, arts1) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    assert_eq!(
        arts1["artifacts"].as_array().unwrap().len(),
        art_count,
        "重复 continue 不得新增产物：{arts1}"
    );
}

// ---------------------------------------------------------------------------
// R5 完成安全：预算超时 / 终态稳定性 / 无孤儿
// ---------------------------------------------------------------------------

/// 预算（max_duration_secs）超时 → 团队干净失败，无活动循环、任务全终态。
#[tokio::test]
async fn budget_duration_timeout_fails_cleanly_no_orphan() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // sleep 1.2s 但时长预算仅 1s：主循环熔断 → Failed。
    let team_id = create_team_full(
        &state,
        &app,
        sleep_role(1200),
        None,
        json!({ "max_duration_secs": 1 }),
    )
    .await;
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["failed", "succeeded", "cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("failed"), "{done}");
    assert_eq!(done["interrupted"], json!(false), "{done}");
    // 全部步骤终态。
    for task in done["tasks"].as_array().unwrap() {
        let st = task["status"].as_str().unwrap();
        assert!(
            !["Running", "Pending"].contains(&st),
            "预算超时后不得残留未终态步骤：{task:?}"
        );
    }
    // 终态稳定：再取一次不再变化、无活动循环。
    let (_, again) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(again["team"]["status"], json!("failed"));
    assert_eq!(again["interrupted"], json!(false));
}

/// 取消后显式 continue：中断的步骤被重置并重跑至完成（取消 → 恢复闭环）。
#[tokio::test]
async fn cancel_then_continue_completes_run() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(3000)).await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(status, 200);
    let cancelled = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(cancelled["team"]["status"], json!("cancelled"));

    // 等待取消收尾彻底结束（运行循环不再存活）后再 continue——
    // R2 语义：先通知停止→有界清理→循环退出；清理窗口内的 steer 会 409。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, list) = call(&state, &app, "GET", "/teams", None).await;
        let entry = list["teams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["team_id"].as_str() == Some(team_id.as_str()))
            .expect("列表含团队");
        if entry["active"] == json!(false) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "取消收尾应在期限内结束：{entry}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 继续：重置未完成步骤 → 重跑至成功（不允许残留取消现场）。
    let (s2, r2) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("continue", json!({}))),
    )
    .await;
    assert_eq!(s2, 200, "cancelled 后可 continue：{r2}");
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("succeeded"), "{done}");
    assert_eq!(done["interrupted"], json!(false), "{done}");
}

/// 并发取消洪峰：全部请求收敛到同一终态（cancelled），无错误、无第二次终态翻转。
#[tokio::test]
async fn concurrent_cancel_flood_single_terminal() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(3000)).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let path = format!("/teams/{team_id}/steer");
    let body = steer_body("cancel", json!({}));
    let mut handles = Vec::new();
    for _ in 0..6 {
        let st = Arc::clone(&state);
        let app = app.clone();
        let path = path.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            call(&st, &app, "POST", &path, Some(&body)).await
        }));
    }
    let mut statuses = Vec::new();
    for h in handles {
        let (s, r) = h.await.unwrap();
        assert!((200..300).contains(&s), "洪峰取消请求应 2xx：{s} {r}");
        statuses.push(s);
    }

    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled", "succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("cancelled"), "{done}");
    // 只有一次「取消落地」：状态已是终态时重复取消不翻转。
    let (_, again) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(again["team"]["status"], json!("cancelled"), "{again}");
}

/// 审计尾部含取消事件，且明确写出 Provider 计费限制（不得宣称「继续计费为 0」）。
#[tokio::test]
async fn audit_records_cancel_with_billing_limitation() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(2000)).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("cancel", json!({}))),
    )
    .await;
    assert_eq!(status, 200);
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;

    let (_, body) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    let tail = body["audit_tail"].as_array().unwrap();
    let cancel_events = tail
        .iter()
        .filter(|e| {
            e["event"].as_str().unwrap_or("").contains("team.cancelled")
                || e["detail"].as_str().unwrap_or("").contains("取消")
        })
        .count();
    assert!(cancel_events >= 1, "审计须记录取消事件：{body}");
    let all_detail = tail.iter().fold(String::new(), |acc, e| {
        acc + e["detail"].as_str().unwrap_or("")
    });
    assert!(
        all_detail.contains("继续计费为 0") && all_detail.contains("无法由客户端证明"),
        "审计须声明 Provider 计费限制（不以「继续计费为 0」宣称）：{body}"
    );
}

/// 列表面同样暴露 interrupted 标记；continue 后标记清除且不再出现在运行中。
#[tokio::test]
async fn interrupted_surfaces_in_list_then_clears_after_continue() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(120)).await;
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;

    // 模拟崩溃遗留。
    let ws_dir = temp.path().join("workswarm");
    let store = SqliteProjectSpaceStore::open(&ws_dir.join("space.db")).unwrap();
    let mut team = store.get_team_run(&team_id).await.unwrap().clone();
    team.status = TeamRunStatus::Running;
    store.save_team_run(&team).await.unwrap();
    drop(store);

    // 详情面先触发一次中断识别（幂等），列表随后才带出 interrupted 标记。
    let (_, detail) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(detail["interrupted"], true, "{detail}");

    let (_, list) = call(&state, &app, "GET", "/teams", None).await;
    let entry = list["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["team_id"].as_str() == Some(team_id.as_str()))
        .expect("列表含团队");
    assert_eq!(entry["interrupted"], true, "{list}");

    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body("continue", json!({}))),
    )
    .await;
    assert_eq!(status, 200);
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed", "cancelled"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let (_, list2) = call(&state, &app, "GET", "/teams", None).await;
    let entry2 = list2["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["team_id"].as_str() == Some(team_id.as_str()))
        .expect("列表仍含团队");
    assert_eq!(entry2["interrupted"], false, "恢复后列表标记清除：{list2}");
}

/// 终态后指标不再增长（无后台孤儿 Worker 继续记账）。
#[tokio::test]
async fn terminal_metrics_stable_no_growth_after_done() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(150)).await;
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("succeeded"));

    let (s1, m1) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{team_id}/metrics"),
        None,
    )
    .await;
    assert_eq!(s1, 200, "{m1}");
    let spans1 = m1["summary"]["span_count"].as_u64().unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let (_, m2) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{team_id}/metrics"),
        None,
    )
    .await;
    let spans2 = m2["summary"]["span_count"].as_u64().unwrap();
    assert_eq!(
        spans1, spans2,
        "终态后不得有孤儿 Worker 继续产生 span：{m1} → {m2}"
    );
}

/// 终态后 apply_steer 不可再触发运行（继续/重试在终态上是空操作或明确 409）。
#[tokio::test]
async fn steer_after_terminal_does_not_restart_execution() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let team_id = create_team(&state, &app, sleep_role(120)).await;
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("succeeded"));

    let (s1, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&steer_body(
            "steer",
            json!({ "step_id": "s-runner", "note": "改输入" }),
        )),
    )
    .await;
    // 成功团队的已成功步骤不可 steer（409）；团队仍成功、不得重跑。
    assert_eq!(s1, 409, "已成功步骤 steer 应 409");
    let (_, after) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
    assert_eq!(after["team"]["status"], json!("succeeded"));
    let attempts = after["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["role"].as_str() == Some("runner"))
        .unwrap()["attempts"]
        .as_u64()
        .unwrap();
    assert_eq!(attempts, 1, "终态 steer 不得重跑：{after}");
}

// ---------------------------------------------------------------------------
// 非 Git 绑定工作区（R1 收尾不依赖 Git）：内容哈希路径走通且不误报
// ---------------------------------------------------------------------------

/// 绑定非 Git 工作区 + 写角色：运行成功、无 git 也能收尾（无变更则不生成
/// 空 ChangeSet；这是对 R1「diff 退回内容哈希」HTTP 面的冒烟）。
#[tokio::test]
async fn non_git_bound_workspace_run_succeeds_cleanly() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 非 Git 工作区（无 .git）。
    let root = temp.path().join("non-git-ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), b"fn a() {}\n").unwrap();

    let roles = json!([
        { "role": "implementer", "assignee": "agent", "worker": "echo",
          "extra_input": { "text": "no real write" }, "verify": "non_empty" }
    ]);
    let ws_spec = json!({
        "root": root.to_str().unwrap(),
        "read_only": false,
        "write_allowed_paths": ["src"],
    });
    let team_id = create_team_full(&state, &app, roles, Some(ws_spec), json!(null)).await;
    let done = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(done["team"]["status"], json!("succeeded"), "{done}");
    // 无实际变更 → 不生成空 ChangeSet。
    let (status, sets) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{team_id}/change-sets"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{sets}");
    assert_eq!(
        sets["change_sets"].as_array().unwrap().len(),
        0,
        "无实际变更不得生成空 ChangeSet：{sets}"
    );
    // 工作区文件未被破坏。
    assert_eq!(
        std::fs::read(root.join("src/a.rs")).unwrap(),
        b"fn a() {}\n"
    );
}
