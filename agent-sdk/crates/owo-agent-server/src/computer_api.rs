//! Computer-use HTTP API（§12：从 lib.rs 机械外移的任务级审批与闭环执行域）。
//!
//! 路由面（`/computer-use/tasks`、`/computer-use/task`、`/computer-use/task/{id}/{action}`、
//! `/computer-use/task/{id}/check/{action}`、`/computer-use/sensitive-check`、
//! `/computer-use/task/{id}/run`）与 /openapi.json 登记保持不变，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// 任务列表：`GET /computer-use/tasks`。
pub(super) async fn computer_tasks_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let tasks = state.computer_tasks.list();
    Json(json!({ "count": tasks.len(), "tasks": tasks }))
}

#[derive(Deserialize)]
pub(super) struct ComputerTaskCreateRequest {
    target_app: String,
    description: String,
    #[serde(default)]
    allowed_actions: Vec<String>,
    #[serde(default = "default_task_duration_ms")]
    max_duration_ms: u64,
}

fn default_task_duration_ms() -> u64 {
    300_000
}

/// 创建任务：`POST /computer-use/task`（Pending，等待审批）。
pub(super) async fn computer_task_create(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ComputerTaskCreateRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let target_app = request.target_app.trim().to_string();
    if target_app.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 target_app".to_string()));
    }
    let task = owo_agent_core::ComputerTask {
        id: uuid::Uuid::new_v4().to_string(),
        target_app,
        description: request.description,
        allowed_actions: request.allowed_actions,
        max_duration_ms: request.max_duration_ms,
        state: owo_agent_core::TaskState::Pending,
        created_at: chrono::Utc::now().to_rfc3339(),
        fuse_reason: None,
    };
    state
        .computer_tasks
        .create(task.clone())
        .map_err(|error| (StatusCode::CONFLICT, error))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "computer-use",
            "task-create",
            Some(task.id.clone()),
            Some(true),
            format!(
                "创建 computer-use 任务：{}（{}ms，动作 {:?}）",
                task.target_app, task.max_duration_ms, task.allowed_actions
            ),
        );
    }
    // §3.2：任务创建成功后发布 computer 域失效（冲突路径不发布）。
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Computer);
    Ok(Json(json!({ "ok": true, "task": task })))
}

/// 状态迁移：`POST /computer-use/task/{id}/{action}`。
///
/// action ∈ approve/reject/cancel/start/pause/fuse/resume/complete。
pub(super) async fn computer_task_transition(
    State(state): State<Arc<AppState>>,
    AxumPath((id, action)): AxumPath<(String, String)>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let reason = payload
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("人工接管")
        .to_string();
    let next = match action.as_str() {
        "approve" => state.computer_tasks.approve(&id),
        "reject" => state.computer_tasks.reject(&id),
        "cancel" => state.computer_tasks.cancel(&id),
        "start" => state.computer_tasks.start(&id),
        "pause" => state.computer_tasks.pause(&id, &reason),
        "fuse" => state.computer_tasks.fuse(&id, &reason),
        "resume" => state.computer_tasks.resume(&id),
        "complete" => state.computer_tasks.complete(&id),
        other => Err(format!(
            "未知动作：{other}（approve/reject/cancel/start/pause/fuse/resume/complete）"
        )),
    }
    .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "computer-use",
            &format!("task-{action}"),
            Some(id.clone()),
            Some(true),
            format!("computer-use 任务 {id} {action} → {:?}", next),
        );
    }
    // §3.2：状态迁移成功后发布 computer 域失效（非法动作/冲突不发布）。
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Computer);
    Ok(Json(
        json!({ "ok": true, "id": id, "action": action, "state": format!("{next:?}") }),
    ))
}

/// 执行前检查：`GET /computer-use/task/{id}/check/{action}`（状态 + 超时 + 动作白名单）。
pub(super) async fn computer_task_check(
    State(state): State<Arc<AppState>>,
    AxumPath((id, action)): AxumPath<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state
        .computer_tasks
        .check_can_execute(&id)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    state
        .computer_tasks
        .check_action_allowed(&id, &action)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let task = state
        .computer_tasks
        .get(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("任务 {id} 不存在")))?;
    Ok(Json(json!({
        "ok": true,
        "id": id,
        "action": action,
        "state": format!("{:?}", task.state),
        "target_app": task.target_app,
    })))
}

#[derive(Deserialize)]
pub(super) struct SensitiveCheckRequest {
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    ocr_text: String,
}

/// 敏感 UI 检测：`POST /computer-use/sensitive-check`（熔断判断，纯函数）。
pub(super) async fn computer_sensitive_check(
    Json(request): Json<SensitiveCheckRequest>,
) -> Json<Value> {
    match owo_agent_core::sensitive_ui_hit(&request.name, &request.role, &request.ocr_text) {
        Some(reason) => Json(json!({ "sensitive": true, "reason": reason })),
        None => Json(json!({ "sensitive": false })),
    }
}

// ---------- computer-use 审批版闭环执行（M4d，HTTP 接入） ----------

#[derive(Deserialize)]
pub(super) struct ComputerTaskRunRequest {
    /// 闭环步骤（anchor_text/action/value/verify_text）。
    goals: Vec<owo_agent_core::computer_use::TaskGoal>,
}

/// 执行已批准任务：`POST /computer-use/task/{id}/run`。
///
/// 感知→定位→门禁动作→验证 全闭环；模拟面（OWO_SIM_QQ_URL）走 owo-sim-qq，
/// 否则走真实桌面面（RealTaskSurface）。任务未批准/越界应用/敏感熔断等门禁
/// 失败返回 403 并写审计；每步动作均过 `task_gate_check`。
pub(super) async fn computer_task_run(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ComputerTaskRunRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let task = state.computer_tasks.get(&id).ok_or((
        StatusCode::NOT_FOUND,
        format!("computer-use 任务 {id} 不存在"),
    ))?;
    if !task.state.can_execute() {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "computer-use 任务 {id} 状态 {:?} 不可执行（需先 approve/start）",
                task.state
            ),
        ));
    }
    // 用本地 scratch 审计跑闭环（std MutexGuard 不能跨 await），完成后合并回真实审计。
    let mut scratch = owo_agent_core::audit::AuditLog::default();
    let report = if owo_agent_core::computer_use::sim_base_url_configured() {
        owo_agent_core::computer_use::run_approved_task(
            &state.computer_tasks,
            &mut scratch,
            "computer-use",
            &id,
            &request.goals,
        )
        .await
    } else {
        let mut surface = owo_agent_core::computer_use::RealTaskSurface;
        owo_agent_core::computer_use::run_approved_task_on(
            &state.computer_tasks,
            &mut scratch,
            "computer-use",
            &id,
            &request.goals,
            &mut surface,
        )
        .await
    };
    if let Ok(mut log) = state.agent.audit_log().lock() {
        log.entries.extend(scratch.entries);
    }
    // §3.2：任务执行成功后发布 computer 域失效（门禁失败 403 不发布）。
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Computer);
    report
        .map(|r| Json(json!(r)))
        .map_err(|error| (StatusCode::FORBIDDEN, error))
}
