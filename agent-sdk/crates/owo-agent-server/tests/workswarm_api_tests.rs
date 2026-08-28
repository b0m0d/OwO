//! WorkSwarm S0 路由集成测试（§8.5 资源面 + §9.0 S0 完成标准的 HTTP 视角）。
//!
//! 覆盖：建队→接力完成（产物/审计/任务视图）、人节点门闩→提交唤醒、
//! 运行中 steer → 409、cancel、模板提案→采纳→模板复用、swarmflow 无模板 400、
//! 未知资源 404、团队列表 active 标记、事件流（JSON 快照 + SSE 重放/状态帧/终态关闭）、
//! 模板提案拒绝生命周期（拒绝保留记录、拒绝↔采纳互斥）。
//! 全部使用内置 echo/sleep worker（不依赖模型凭据）。

use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

/// 无外部依赖的最小模型 Provider（本测试只用 echo/sleep worker，模型调用即失败）。
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

/// echo 接力 4 角色（planner→builder→critic→leader，全进程内回显，不碰模型）。
fn echo_relay_roles() -> Value {
    json!([
        { "role": "planner", "assignee": "agent", "worker": "echo",
          "handoff_contract": "产出方案大纲", "verify": "non_empty" },
        { "role": "builder", "assignee": "agent", "worker": "echo",
          "depends_on": ["planner"], "handoff_contract": "产出草稿", "verify": "non_empty" },
        { "role": "critic", "assignee": "agent", "worker": "echo",
          "depends_on": ["builder"], "handoff_contract": "只读评审", "verify": "non_empty" },
        { "role": "leader", "assignee": "agent", "worker": "echo",
          "depends_on": ["critic"], "handoff_contract": "最终裁决", "verify": "non_empty" }
    ])
}

#[tokio::test]
async fn create_team_relay_via_http_completes_with_artifacts() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // POST /teams：echo 接力 4 角色。
    let body = json!({
        "objective": "写一份 API 设计说明",
        "mode": "team",
        "roles": echo_relay_roles(),
        "budget": { "max_steps": 10 }
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "POST /teams 应 202：{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();
    let project_id = created["project_space_id"].as_str().unwrap().to_string();
    assert_eq!(created["members"].as_array().unwrap().len(), 4);

    // 后台运行循环推进 → 4 步全部 Succeeded。
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let st = detail["team"]["status"].as_str().unwrap();
    assert_eq!(st, "succeeded", "echo 接力应成功：{detail}");
    let tasks = detail["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 4);
    for t in tasks {
        assert_eq!(t["status"], "Succeeded", "步骤应全部成功：{tasks:?}");
    }
    // 审计尾迹：关键动作落审计（创建/交接/收尾）。
    assert!(
        detail["audit_tail"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "审计尾迹应非空：{detail}"
    );

    // GET /projects/{id}/artifacts：4 个版本化产物（ref 传递）。
    let (status, arts) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/artifacts"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let list = arts["artifacts"].as_array().unwrap();
    assert_eq!(list.len(), 4, "每步一个产物：{arts}");
    for a in list {
        assert!(
            !a["content_ref"].as_str().unwrap().is_empty(),
            "产物应带 CAS ref：{a}"
        );
    }

    // GET /projects/{id}：Project Space 摘要。
    let (status, space) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(space["project"]["project_id"], project_id);

    // GET /teams/{id}/tasks：任务图。
    let (status, tg) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{team_id}/tasks"),
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(tg["team_id"], team_id);
    assert_eq!(tg["tasks"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn human_node_waits_then_result_wakes_downstream() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let body = json!({
        "objective": "方案评审流程",
        "roles": [
            { "role": "planner", "assignee": "agent", "worker": "echo", "verify": "non_empty" },
            { "role": "approver", "assignee": "human", "worker": "u-1", "depends_on": ["planner"] }
        ]
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();

    // 人节点门闩：planner（echo）完成 → 等 approver。
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["awaiting_human", "succeeded", "failed"],
        std::time::Duration::from_secs(20),
    )
    .await;
    let st = detail["team"]["status"].as_str().unwrap();
    assert_eq!(st, "awaiting_human", "应停在人节点等待：{detail}");

    // POST /tasks/{id}/human-result：提交人结果 → 运行循环自动唤醒下游。
    let (status, res) = call(
        &state,
        &app,
        "POST",
        "/tasks/s-approver/human-result",
        Some(&json!({ "team_id": team_id, "result": "approved: 方案可执行" }).to_string()),
    )
    .await;
    assert_eq!(status, 200, "人结果提交应 200：{res}");

    // 唤醒后全部完成（approver 为最后一步）。
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(20),
    )
    .await;
    assert_eq!(
        detail["team"]["status"].as_str().unwrap(),
        "succeeded",
        "人结果提交后应完成：{detail}"
    );
}

#[tokio::test]
async fn steer_during_active_run_conflicts_then_cancel() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // sleep 4s 的长步骤：打开「运行中」窗口。
    let body = json!({
        "objective": "长任务",
        "roles": [
            { "role": "runner", "assignee": "agent", "worker": "sleep",
              "extra_input": { "ms": 4000 }, "verify": "non_empty" }
        ]
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();

    // 等运行进入活动窗口（阶段执行中）。
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(
            &json!({ "command": "steer", "step_id": "s-runner",
                     "new_input": { "ms": 500 }, "note": "改参数" })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, 409, "运行中 steer 应 409 Conflict：{res}");

    // cancel：阶段边界立即生效（或等待窗口内直接取消）。
    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&json!({ "command": "cancel", "note": "用户取消" }).to_string()),
    )
    .await;
    assert!(status == 200, "cancel 应 200：{res}");
    let detail = poll_team_status(
        &state,
        &app,
        &team_id,
        &["cancelled", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        detail["team"]["status"].as_str().unwrap(),
        "cancelled",
        "应取消：{detail}"
    );
}

#[tokio::test]
async fn template_proposal_adopt_then_template_run() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 先跑一次 echo 接力 → 成功收尾产生 TeamTemplateProposal（只提案，不自动启用）。
    let body = json!({ "objective": "模板来源运行", "roles": echo_relay_roles() });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;

    let (status, props) = call(&state, &app, "GET", "/teams/templates/proposals", None).await;
    assert_eq!(status, 200);
    let proposals = props["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1, "应产生一个模板提案：{props}");
    let proposal_id = proposals[0]["proposal_id"].as_str().unwrap().to_string();
    assert_eq!(proposals[0]["status"], "proposed");

    // 采纳 → 模板进入注册表。
    let (status, adopted) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/templates/proposals/{proposal_id}/adopt"),
        None,
    )
    .await;
    assert_eq!(status, 200, "adopt 应 200：{adopted}");
    let template_id = adopted["template"]["template_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!template_id.is_empty());

    let (status, tpls) = call(&state, &app, "GET", "/teams/templates", None).await;
    assert_eq!(status, 200);
    assert_eq!(tpls["templates"].as_array().unwrap().len(), 1);

    // 模板驱动的新一轮（角色来自模板；echo worker 不碰模型）。
    let body = json!({ "objective": "模板复用的运行", "mode": "team", "template_id": template_id });
    let (status, created2) = call(&state, &app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "模板建队应 202：{created2}");
    assert_eq!(created2["template_id"], template_id);

    // 幂等：重复 adopt 不报错。
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/templates/proposals/{proposal_id}/adopt"),
        None,
    )
    .await;
    assert!(status == 200, "重复 adopt 应幂等");
}

#[tokio::test]
async fn swarmflow_requires_versioned_template() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let (status, res) = call(
        &state,
        &app,
        "POST",
        "/teams",
        Some(&json!({ "objective": "无模板 swarmflow", "mode": "swarmflow" }).to_string()),
    )
    .await;
    assert_eq!(status, 400, "无模板 swarmflow 应 400：{res}");
}

/// 按 id 在提案列表中查找（列表保留全部状态记录，按 created_at 降序）。
fn find_proposal(body: &Value, proposal_id: &str) -> Value {
    body["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["proposal_id"].as_str() == Some(proposal_id))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

#[tokio::test]
async fn list_teams_shows_active_flag_across_lifecycle() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 未建队前：空列表。
    let (status, body) = call(&state, &app, "GET", "/teams", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["teams"].as_array().unwrap().len(),
        0,
        "未建队时列表应为空：{body}"
    );

    // sleep 3s 长步骤：打开「活动」窗口。
    let create = json!({
        "objective": "活动标记",
        "roles": [
            { "role": "runner", "assignee": "agent", "worker": "sleep",
              "extra_input": { "ms": 3000 }, "verify": "non_empty" }
        ]
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();

    // 轮询列表直到 active == true（运行循环已启动，flag 在 agent 批次开批时置位）。
    let mut active_seen = false;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !active_seen && tokio::time::Instant::now() < deadline {
        let (_, list) = call(&state, &app, "GET", "/teams", None).await;
        active_seen = list["teams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["team_id"].as_str() == Some(team_id.as_str()))
            .map(|e| e["active"].as_bool().unwrap_or(false))
            .unwrap_or(false);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(active_seen, "运行中团队在列表里 active 应为 true");

    // 终态后 active 归 false（flag 先于终态写入清除，无竞态）。
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let (status, list) = call(&state, &app, "GET", "/teams", None).await;
    assert_eq!(status, 200);
    let entry = list["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["team_id"].as_str() == Some(team_id.as_str()))
        .expect("终态团队仍在列表中：{list}");
    assert_eq!(entry["active"], false, "终态后 active 应为 false：{list}");
}

#[tokio::test]
async fn team_events_json_snapshot_unknown_and_terminal() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 未知团队：开流前 404。
    let (status, body) = call(
        &state,
        &app,
        "GET",
        "/teams/no-such-team/events?format=json",
        None,
    )
    .await;
    assert_eq!(status, 404, "未知团队事件快照应 404：{body}");

    // echo 接力 → 成功；快照含状态（Debug/PascalCase）与审计尾迹。
    let create = json!({ "objective": "事件快照", "roles": echo_relay_roles() });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;

    let (status, snap) = call(
        &state,
        &app,
        "GET",
        &format!("/teams/{team_id}/events?format=json"),
        None,
    )
    .await;
    assert_eq!(status, 200, "{snap}");
    assert_eq!(snap["team_id"], team_id, "{snap}");
    assert_eq!(
        snap["status"].as_str(),
        Some("Succeeded"),
        "events 快照状态为 Debug 形式（PascalCase）：{snap}"
    );
    assert_eq!(snap["active"], false, "{snap}");
    assert!(
        snap["audit"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "审计尾迹应非空：{snap}"
    );
}

#[tokio::test]
async fn team_events_sse_stream_replays_audit_and_closes_at_terminal() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let create = json!({
        "objective": "SSE 事件流",
        "roles": [
            { "role": "runner", "assignee": "agent", "worker": "sleep",
              "extra_input": { "ms": 800 }, "verify": "non_empty" }
        ]
    });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();

    // 开流（oneshot 在响应头产生时返回）；drain 到终态关闭。
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
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .expect("SSE 响应应带 content-type");
    assert!(
        ct.to_str().unwrap().starts_with("text/event-stream"),
        "content-type 应为 SSE 流：{ct:?}"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("\"type\":\"open\""), "应有 open 帧：{text}");
    assert!(
        text.contains("\"type\":\"audit\""),
        "应有 audit 重放帧（建队审计先于开流落盘）：{text}"
    );
    assert!(
        text.contains("\"type\":\"state\""),
        "应有 state 帧（首次观察必发）：{text}"
    );
    assert!(
        text.contains("\"status\":\"Succeeded\""),
        "终态帧应为 Succeeded（Debug 形式）：{text}"
    );
}

#[tokio::test]
async fn template_proposal_reject_lifecycle() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 未知提案 → 404。
    let (status, body) = call(
        &state,
        &app,
        "POST",
        "/teams/templates/proposals/no-such-proposal/reject",
        None,
    )
    .await;
    assert_eq!(status, 404, "未知提案拒绝应 404：{body}");

    // 运行 1：产生提案 P1（proposed）→ 拒绝 → 列表保留 rejected 记录。
    let create = json!({ "objective": "提案来源一", "roles": echo_relay_roles() });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id = created["team_id"].as_str().unwrap().to_string();
    let _ = poll_team_status(
        &state,
        &app,
        &team_id,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let (status, props) = call(&state, &app, "GET", "/teams/templates/proposals", None).await;
    assert_eq!(status, 200, "{props}");
    assert_eq!(
        props["proposals"].as_array().unwrap().len(),
        1,
        "应产生一个模板提案：{props}"
    );
    let p1 = props["proposals"][0]["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(props["proposals"][0]["status"], "proposed", "{props}");

    // 拒绝 P1 → 200 rejected。
    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/templates/proposals/{p1}/reject"),
        None,
    )
    .await;
    assert_eq!(status, 200, "拒绝 proposed 提案应 200：{res}");
    assert_eq!(res["proposal_id"], p1, "{res}");
    assert_eq!(res["status"], "rejected", "{res}");

    // 列表保留 rejected 记录（可审计）。
    let (_, props) = call(&state, &app, "GET", "/teams/templates/proposals", None).await;
    assert_eq!(
        find_proposal(&props, &p1)["status"],
        "rejected",
        "拒绝记录应保留在列表中：{props}"
    );

    // 已拒绝 → 不能采纳。
    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/templates/proposals/{p1}/adopt"),
        None,
    )
    .await;
    assert_eq!(status, 400, "已拒绝提案采纳应 400：{res}");

    // 运行 2：提案 P2 → 采纳 → 已采纳不能拒绝。
    let create = json!({ "objective": "提案来源二", "roles": echo_relay_roles() });
    let (status, created) = call(&state, &app, "POST", "/teams", Some(&create.to_string())).await;
    assert_eq!(status, 202, "{created}");
    let team_id2 = created["team_id"].as_str().unwrap().to_string();
    let _ = poll_team_status(
        &state,
        &app,
        &team_id2,
        &["succeeded", "failed"],
        std::time::Duration::from_secs(30),
    )
    .await;
    let (status, props) = call(&state, &app, "GET", "/teams/templates/proposals", None).await;
    assert_eq!(status, 200, "{props}");
    let p2 = props["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["status"].as_str() == Some("proposed"))
        .map(|p| p["proposal_id"].as_str().unwrap().to_string())
        .unwrap_or_else(|| panic!("应有一个 proposed 提案：{props}"));
    assert_ne!(p2, p1, "两次运行应产生不同提案：{props}");

    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/templates/proposals/{p2}/adopt"),
        None,
    )
    .await;
    assert_eq!(status, 200, "adopt P2 应 200：{res}");

    let (status, res) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/templates/proposals/{p2}/reject"),
        None,
    )
    .await;
    assert_eq!(status, 400, "已采纳提案拒绝应 400：{res}");
}

#[tokio::test]
async fn unknown_resources_return_404() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    let (status, _) = call(&state, &app, "GET", "/teams/no-such-team", None).await;
    assert_eq!(status, 404, "未知团队应 404");

    let (status, _) = call(&state, &app, "GET", "/teams/no-such-team/tasks", None).await;
    assert_eq!(status, 404, "未知团队任务图应 404");

    let (status, _) = call(&state, &app, "GET", "/projects/no-such-project", None).await;
    assert_eq!(status, 404, "未知项目空间应 404");

    let (status, _) = call(
        &state,
        &app,
        "GET",
        "/projects/no-such-project/artifacts",
        None,
    )
    .await;
    assert_eq!(status, 404, "未知项目产物应 404");

    let (status, _) = call(
        &state,
        &app,
        "POST",
        "/tasks/s-x/handoff",
        Some(
            &json!({ "team_id": "no-such-team", "from_member": "m-x",
                     "completed_summary": "done" })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, 404, "未知团队接力应 404");

    let (status, _) = call(
        &state,
        &app,
        "POST",
        "/tasks/s-x/human-result",
        Some(&json!({ "team_id": "no-such-team", "result": "ok" }).to_string()),
    )
    .await;
    assert_eq!(status, 404, "未知团队人结果应 404");

    let (status, _) = call(
        &state,
        &app,
        "POST",
        "/teams/templates/proposals/no-such-proposal/adopt",
        None,
    )
    .await;
    assert_eq!(status, 404, "未知提案采纳应 404");
}
