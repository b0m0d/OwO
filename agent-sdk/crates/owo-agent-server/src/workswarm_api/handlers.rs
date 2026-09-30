use super::dto::*;
use super::runtime::*;
use super::workswarm_metrics;
use super::{change_set_api, project_workspace};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use owo_agent_core::workswarm::{
    CreateTeamRequest, HandoffFields, RoleSpec, SteerCommand, TeamCoordinator, WorkSwarmError,
};
use owo_agent_protocol::{TeamMode, TeamRunStatus};
use owo_agent_server::AppState;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateTeamHttpRequest {
    goal_id: Option<String>,
    objective: String,
    mode: Option<String>,
    template_id: Option<String>,
    #[serde(default)]
    roles: Vec<RoleSpec>,
    #[serde(default)]
    budget: Value,
    human_policy: Option<String>,
    /// 五期：组队策略 auto|single|team（缺省 auto；未知值 → 400）。
    #[serde(default)]
    strategy: Option<String>,
    /// 六期（第二路）：可选项目工作区绑定（root 必须已存在；缺省只读）。
    #[serde(default)]
    workspace: Option<project_workspace::WorkspaceSpec>,
}

#[derive(Debug, Deserialize)]
struct SteerHttpRequest {
    /// continue | steer | replace | cancel | retry
    command: String,
    #[serde(default)]
    step_id: Option<String>,
    #[serde(default)]
    new_input: Option<Value>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    new_worker: Option<String>,
    #[serde(default)]
    new_user_id: Option<String>,
}

impl SteerHttpRequest {
    fn into_command(self) -> Result<SteerCommand, String> {
        match self.command.as_str() {
            "continue" => Ok(SteerCommand::Continue),
            "cancel" => Ok(SteerCommand::Cancel),
            "steer" => Ok(SteerCommand::Steer {
                step_id: self.step_id,
                new_input: self.new_input,
                note: self.note,
            }),
            // R2 冻结契约：POST /teams/{id}/steer {"command":"retry","step_id":"builder","note":"…"}
            // 缺少/空 step_id → 400（Validation）。
            "retry" => {
                let step_id = self
                    .step_id
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "retry 需要 step_id 字段".to_string())?;
                Ok(SteerCommand::Retry {
                    step_id,
                    note: self.note,
                })
            }
            "replace" => {
                let role = self
                    .role
                    .filter(|r| !r.trim().is_empty())
                    .ok_or_else(|| "replace 需要 role 字段".to_string())?;
                Ok(SteerCommand::Replace {
                    role,
                    new_worker: self.new_worker,
                    new_user_id: self.new_user_id,
                    note: self.note,
                })
            }
            other => Err(format!(
                "未知 steer 指令：{other}（支持 continue/retry/steer/replace/cancel）"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct HandoffHttpRequest {
    team_id: String,
    from_member: String,
    #[serde(default)]
    to_member: Option<String>,
    #[serde(default)]
    completed_summary: String,
    #[serde(default)]
    open_issues: Vec<String>,
    #[serde(default)]
    output_artifact_refs: Vec<String>,
    #[serde(default)]
    evidence_refs: Vec<String>,
    #[serde(default)]
    suggested_next_actions: Vec<String>,
    #[serde(default)]
    known_risks: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HumanResultHttpRequest {
    team_id: String,
    result: String,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/teams", post(create_team).get(list_teams))
        .route("/teams/{id}", get(get_team))
        .route("/teams/{id}/tasks", get(get_team_tasks))
        .route("/teams/{id}/events", get(team_events))
        .route("/teams/{id}/metrics", get(team_metrics))
        .route("/teams/{id}/diagnostic", get(team_diagnostic))
        .route("/teams/{id}/steer", post(steer_team))
        // 八期（第二路）：ChangeSet 审批、接受与安全撤销。
        .route(
            "/teams/{id}/change-sets",
            get(change_set_api::list_team_change_sets),
        )
        .route("/change-sets/{id}", get(change_set_api::get_change_set))
        .route(
            "/change-sets/{id}/accept",
            post(change_set_api::accept_change_set),
        )
        .route(
            "/change-sets/{id}/reject",
            post(change_set_api::reject_change_set),
        )
        .route(
            "/change-sets/{id}/revert",
            post(change_set_api::revert_change_set),
        )
        .route("/projects/{id}", get(get_project_space))
        .route("/projects/{id}/artifacts", get(list_artifacts))
        // 六期（第二路）：项目工作区绑定（真实目录 / 只读 / 写白名单 / 树 / git 状态）。
        .route(
            "/projects/{id}/workspace",
            get(project_workspace::get_workspace).put(project_workspace::put_workspace),
        )
        .route(
            "/projects/{id}/workspace/tree",
            get(project_workspace::get_workspace_tree),
        )
        // §8.1 启动器：团队创建前预览候选工作区目录树（写入路径多选数据源）。
        .route(
            "/workspace/tree",
            get(project_workspace::preview_workspace_tree),
        )
        .route(
            "/projects/{id}/workspace/git-status",
            get(project_workspace::get_workspace_git_status),
        )
        // 七期（第二路）：Worker 代码变更追踪（变更文件 + diff 摘要 + 逐步骤记录）。
        .route(
            "/projects/{id}/workspace/changes",
            get(project_workspace::get_workspace_changes),
        )
        .route("/tasks/{id}/handoff", post(submit_handoff))
        .route("/tasks/{id}/human-result", post(submit_human_result))
        .route("/teams/templates", get(list_templates))
        .route("/teams/templates/proposals", get(list_proposals))
        .route(
            "/teams/templates/proposals/{proposal_id}/adopt",
            post(adopt_proposal),
        )
        .route(
            "/teams/templates/proposals/{proposal_id}/reject",
            post(reject_proposal),
        )
        .with_state(state)
}

// ---------------- handlers ----------------

/// POST /teams：创建运行（202）+ 后台运行循环。
async fn create_team(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTeamHttpRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let mode = match req.mode.as_deref() {
        Some("single") => TeamMode::Single,
        Some("team") | None => TeamMode::Team,
        Some("swarmflow") => TeamMode::Swarmflow,
        Some(other) => {
            return Err(error_response(&WorkSwarmError::Validation(format!(
                "未知 mode：{other}（支持 single/team/swarmflow）"
            ))));
        }
    };
    let strategy = match req.strategy.as_deref() {
        None | Some("") => None,
        Some(raw) => match owo_agent_core::team_strategy::TeamSelectionMode::parse(raw) {
            Ok(s) => Some(s),
            Err(msg) => {
                return Err(error_response(&WorkSwarmError::Validation(msg)));
            }
        },
    };
    // 六期（第二路）：工作区绑定先校验（不依赖团队存在；路径非法 → 400，不建队）。
    let workspace_binding = match &req.workspace {
        Some(spec) => Some(
            project_workspace::validate_workspace_spec(spec)
                .map_err(|msg| (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))))?,
        ),
        None => None,
    };
    let req = CreateTeamRequest {
        goal_id: req.goal_id,
        objective: req.objective,
        mode,
        template_id: req.template_id,
        roles: req.roles,
        budget: req.budget,
        human_policy: req.human_policy,
        strategy,
    };
    let team = coordinator
        .create_team_run(&req)
        .await
        .map_err(|e| error_response(&e))?;
    let team_id = team.team_id.clone();
    // 绑定在运行循环启动前落盘（首迭代 build_run_registry 即生效）。
    let mut bound_workspace = false;
    if let Some(mut binding) = workspace_binding {
        binding.team_id = team_id.clone();
        binding.project_id = team.project_space_id.clone().unwrap_or_default();
        project_workspace::save_binding(coordinator.run_dir(), &binding).map_err(|msg| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": msg })),
            )
        })?;
        bound_workspace = true;
    }
    // 后台运行循环（单写者：阶段边界串行化；人节点等待 = 门闩）。
    tokio::spawn(run_team_loop(state, coordinator, team_id.clone()));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "team_id": team_id,
            "project_space_id": team.project_space_id,
            "mode": format!("{:?}", team.mode),
            "template_id": team.template_id,
            "members": team.members,
            "status": format!("{:?}", team.status),
            "strategy_decision": team.strategy_decision,
            "workspace_bound": bound_workspace,
        })),
    ))
}

/// GET /teams：团队运行列表。
///
/// `active` = 运行循环正在执行阶段（人节点等待窗口 / 终态为 false = 暂停）。
async fn list_teams(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let runs = coordinator
        .list_team_runs()
        .await
        .map_err(|e| error_response(&e))?;
    let mut items = Vec::with_capacity(runs.len());
    for team in runs {
        let mut item = serde_json::to_value(&team).unwrap_or_else(|_| json!({}));
        if let Some(obj) = item.as_object_mut() {
            obj.insert(
                "active".to_string(),
                json!(coordinator.is_run_active(&team.team_id)),
            );
            // R2：中断标记（磁盘 Running 但无活动运行，等待显式恢复）。
            obj.insert(
                "interrupted".to_string(),
                json!(coordinator.is_interrupted(&team.team_id)),
            );
        }
        items.push(item);
    }
    Ok(Json(json!({ "teams": items })))
}

/// GET /teams/{id}：成员、预算、状态 + 任务视图 + 审计尾迹。
///
/// R2：响应含 `interrupted` 标记（磁盘 Running 但无活动运行 → 已被识别为中断、
/// 等待显式 continue/retry 恢复）。损坏的状态文件在此明确 500（原文件保留）。
async fn get_team(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    // 请求时先做一次单团队中断识别（幂等；CorruptState 在下方 load_run_state 明确报错）。
    // 响应性（R3）：运行中/循环存活的团队跳过识别——该路径取 team 锁，
    // 长 Worker 阶段绝不等待（运行中的团队按定义不可能是中断残留）。
    if !coordinator.is_run_active(&id) && !coordinator.is_loop_alive(&id) {
        let _ = coordinator.detect_interrupted_for(&id).await;
    }
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let run_state = coordinator
        .load_run_state(&id)
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({
        "team": team,
        "interrupted": coordinator.is_interrupted(&id),
        "tasks": task_view(&run_state),
        "audit_tail": audit_tail(&coordinator, &id),
    })))
}

/// GET /teams/{id}/tasks：任务图（步骤 × 状态）。
async fn get_team_tasks(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let run_state = coordinator
        .load_run_state(&id)
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({
        "team_id": team.team_id,
        "tasks": task_view(&run_state),
    })))
}

/// `GET /teams/{id}/events` 查询参数（`?format=json` 一次性快照，供轮询/契约测试）。
#[derive(Debug, Clone, Default, Deserialize)]
struct TeamEventsQuery {
    #[serde(default)]
    format: Option<String>,
}

/// GET /teams/{id}/events：团队事件流（SSE：审计重放 + 状态轮询，终态后结束）。
///
/// 团队不存在 → 404（先于开流，保持与 `/teams/{id}` 一致的语义）。
async fn team_events(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<TeamEventsQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    if query.format.as_deref() == Some("json") {
        // 五期（第三路）：快照补 `progress`（seq/counts/current_steps）——轮询降级
        // 不再完全依赖 SSE 帧也能看到当前步骤（快照不可得时为 null，additive 字段）。
        let progress = coordinator
            .progress_snapshot(&id)
            .await
            .ok()
            .and_then(|p| serde_json::to_value(&p).ok());
        return Ok(Json(json!({
            "team_id": team.team_id,
            "status": format!("{:?}", team.status),
            "active": coordinator.is_run_active(&id),
            "interrupted": coordinator.is_interrupted(&id),
            "progress": progress,
            "audit": audit_tail(&coordinator, &id),
        }))
        .into_response());
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Event, Infallible>>();
    let stream_coordinator = Arc::clone(&coordinator);
    let team_id = team.team_id.clone();
    tokio::spawn(async move {
        team_event_stream(stream_coordinator, team_id, tx).await;
    });
    Ok(Sse::new(UnboundedReceiverStream::new(rx)).into_response())
}

/// SSE 流任务：先重放审计尾迹（最近 50 条，旧→新），再 500ms 轮询新增审计 + 状态变化；
/// 团队进入终态（并补发终帧）后结束。客户端断开（发送失败）即退出。
async fn team_event_stream(
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<Result<Event, Infallible>>,
) {
    let emit = |frame: Value| -> bool {
        tx.send(Ok(Event::default().data(frame.to_string())))
            .is_ok()
    };
    if !emit(json!({ "type": "open", "team_id": team_id })) {
        return;
    }

    // 进度帧（R3）：订阅即发当前快照（客户端立即拿到 current_steps/counts/seq），
    // 此后仅当代次变化才发（seq 单调递增；客户端以 seq 去重/断线续传）。
    let mut last_progress_seq: Option<u64> = None;
    if let Ok(progress) = coordinator.progress_snapshot(&team_id).await {
        last_progress_seq = Some(progress.seq);
        if !emit(json!({ "type": "progress", "progress": progress })) {
            return;
        }
    }

    // 该团队当前的审计条目（session_id = team_id）。
    let team_entries = || -> Vec<owo_agent_core::audit::AuditEntry> {
        let Some(log) = coordinator.audit_log() else {
            return Vec::new();
        };
        let Ok(guard) = log.lock() else {
            return Vec::new();
        };
        guard
            .entries
            .iter()
            .filter(|e| e.session_id == team_id)
            .cloned()
            .collect()
    };

    // 历史重放（最近 50 条）。
    let replay = team_entries();
    let mut seen = replay.len();
    for entry in replay.iter().rev().take(50).rev() {
        if !emit(json!({
            "type": "audit",
            "ts": entry.ts,
            "event": entry.event,
            "detail": entry.detail,
        })) {
            return;
        }
    }

    let mut last_status: Option<TeamRunStatus> = None;
    loop {
        let Some(team) = coordinator.get_team_run(&team_id).await.ok() else {
            return;
        };
        // 新增审计条目（重放点之后）。
        let entries = team_entries();
        for entry in entries.iter().skip(seen) {
            if !emit(json!({
                "type": "audit",
                "ts": entry.ts,
                "event": entry.event,
                "detail": entry.detail,
            })) {
                return;
            }
        }
        seen = entries.len();
        // 状态帧（变化即发；首次必发）。
        if last_status != Some(team.status) {
            last_status = Some(team.status);
            if !emit(json!({
                "type": "state",
                "status": format!("{:?}", team.status),
                "active": coordinator.is_run_active(&team_id),
                "interrupted": coordinator.is_interrupted(&team_id),
            })) {
                return;
            }
        }
        // 进度帧（seq 变化即发；步骤开始/完成/失败/取消都会推进 coordinator 侧序号）。
        if let Ok(progress) = coordinator.progress_snapshot(&team_id).await {
            if last_progress_seq != Some(progress.seq) {
                last_progress_seq = Some(progress.seq);
                if !emit(json!({ "type": "progress", "progress": progress })) {
                    return;
                }
            }
        }
        if team.status.is_terminal() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// GET /teams/{id}/metrics：TeamRun 指标汇总（五期 · 第三路）。
///
/// 数据源 = TeamRun 数据目录的 `metrics.jsonl`（`MeasuredRoleWorker` 追加落盘），
/// 每次请求从文件聚合——无进程内账本，重启后仍可读取。响应：
/// `summary`（span 数/成败/返工/总墙钟/调用·token·费用/最慢 Worker/Artifact 版本数）+
/// `roles`（按角色聚合）+ `workers`（span 明细）+ `budget`（预算状态与耗尽原因）。
async fn team_metrics(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    // 团队不存在 → 404（与 /teams/{id} 语义一致）。
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let journal = workswarm_metrics::MetricsJournal::for_team(coordinator.run_dir(), &id);
    let records = journal.read_records();
    let mut payload = workswarm_metrics::aggregate_metrics(&id, &records, &team.budget);
    // 数据源路径（可观测：UI/运维可直接定位 TeamRun 数据目录里的指标文件）。
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "metrics_file".to_string(),
            json!(journal.path().display().to_string()),
        );
    }
    Ok(Json(payload))
}

/// GET /teams/{id}/diagnostic：脱敏诊断导出（五期 · 第三路）。
///
/// 汇集 TeamRun / 任务视图 / 产物（含 CAS 内容预览）/ 评审记录 / 交接 / 指标 /
/// 审计尾迹，统一经脱敏（凭据类键值与令牌 → `[REDACTED]`、超长文本截断），
/// 供「下载诊断信息」。评审记录复用 WorkSwarm `space.db` 独立连接（打开失败
/// 不阻塞其余诊断面，reviews 缺席即其信号）。
async fn team_diagnostic(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let run_state = coordinator
        .load_run_state(&id)
        .map_err(|e| error_response(&e))?;

    // 任务视图（错误信息等自由文本脱敏）。
    let tasks: Vec<Value> = task_view(&run_state)
        .iter()
        .map(workswarm_metrics::sanitize_value)
        .collect();

    // 产物 + 评审记录（评审存储打开失败 → 跳过，不阻塞导出）。
    let review_store =
        SqliteProjectSpaceStore::open(&state.data_root.join("workswarm").join("space.db")).ok();
    let mut artifacts_json: Vec<Value> = Vec::new();
    let mut reviews_json: Vec<Value> = Vec::new();
    if let Some(space_id) = team.project_space_id.as_deref() {
        if let Ok(space) = coordinator.get_project_space(space_id).await {
            if let Ok(artifacts) = coordinator.list_artifacts(&space).await {
                for a in artifacts {
                    let preview = coordinator
                        .cas()
                        .get_text(a.content_ref.strip_prefix("cas://sha256:").unwrap_or(""))
                        .map(|c| {
                            let head: String = c.chars().take(200).collect();
                            workswarm_metrics::sanitize_text(&head)
                        })
                        .unwrap_or_default();
                    artifacts_json.push(json!({
                        "artifact_id": a.artifact_id,
                        "kind": a.kind,
                        "version": a.version,
                        "producer": a.producer,
                        "content_ref": a.content_ref,
                        "review_state": format!("{:?}", a.review_state),
                        "supersedes_artifact_id": a.supersedes_artifact_id,
                        "created_at": a.created_at,
                        "preview": preview,
                    }));
                    if let Some(store) = review_store.as_ref() {
                        if let Ok(records) = store.list_artifact_reviews(&a.artifact_id).await {
                            for r in records {
                                let value = serde_json::to_value(&r).unwrap_or_else(|_| json!({}));
                                reviews_json.push(workswarm_metrics::sanitize_value(&value));
                            }
                        }
                    }
                }
            }
        }
    }

    // 交接记录（completed_summary 等自由文本脱敏）。
    let handoffs_json: Vec<Value> = coordinator
        .list_handoffs(&id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|h| workswarm_metrics::sanitize_value(&serde_json::to_value(h).unwrap_or(json!({}))))
        .collect();

    // 审计尾迹（最近 200 条，detail 脱敏）。
    let audit_tail_json: Vec<Value> = match coordinator.audit_log() {
        Some(log) => match log.lock() {
            Ok(entries) => entries
                .entries
                .iter()
                .filter(|e| e.session_id == id)
                .rev()
                .take(200)
                .map(|e| {
                    json!({
                        "ts": e.ts,
                        "event": e.event,
                        "tool": e.tool,
                        "detail": workswarm_metrics::sanitize_text(&e.detail),
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };

    // 指标（与 /teams/{id}/metrics 同一聚合口径）。
    let journal = workswarm_metrics::MetricsJournal::for_team(coordinator.run_dir(), &id);
    let metrics = workswarm_metrics::aggregate_metrics(&id, &journal.read_records(), &team.budget);
    let metrics_file = journal.path().display().to_string();

    // TeamRun 本体（budget 等自由 JSON 脱敏）+ 运行标志。
    let mut team_value = serde_json::to_value(&team).unwrap_or_else(|_| json!({}));
    team_value = workswarm_metrics::sanitize_value(&team_value);
    if let Some(obj) = team_value.as_object_mut() {
        obj.insert("active".to_string(), json!(coordinator.is_run_active(&id)));
        obj.insert(
            "interrupted".to_string(),
            json!(coordinator.is_interrupted(&id)),
        );
    }

    Ok(Json(json!({
        "team_id": team.team_id,
        "generated_at": workswarm_metrics::rfc3339(),
        "team": team_value,
        "tasks": tasks,
        "artifacts": artifacts_json,
        "reviews": reviews_json,
        "handoffs": handoffs_json,
        "metrics": metrics,
        "metrics_file": metrics_file,
        "audit_tail": audit_tail_json,
        "redaction": {
            "applied": true,
            "note": "凭据类键值与令牌已替换为 [REDACTED]；超长文本截断（*_tokens 为用量计数，不属凭据）"
        },
    })))
}

/// POST /teams/{id}/steer：continue/retry/steer/replace/cancel。
async fn steer_team(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<SteerHttpRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let cmd = req
        .into_command()
        .map_err(|m| (StatusCode::BAD_REQUEST, Json(json!({ "error": m }))))?;
    // continue / retry 成功后都需要重新启动团队运行循环（R2：retry 恢复同样重启循环；
    // 运行循环每轮自行重建 worker 注册表，构建失败会在循环内记录并退出）。
    let restarts_loop = matches!(cmd, SteerCommand::Continue | SteerCommand::Retry { .. });
    let team = coordinator
        .apply_steer(&id, &cmd)
        .await
        .map_err(|e| error_response(&e))?;
    if restarts_loop && !coordinator.is_run_active(&id) {
        let state2 = state.clone();
        let coordinator2 = Arc::clone(&coordinator);
        tokio::spawn(run_team_loop(state2, coordinator2, id.clone()));
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "team_id": team.team_id,
            "status": format!("{:?}", team.status),
            "interrupted": coordinator.is_interrupted(&team.team_id),
        })),
    ))
}

/// GET /projects/{id}：Project Space 摘要。
async fn get_project_space(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let space = coordinator
        .get_project_space(&id)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({ "project": space })))
}

/// GET /projects/{id}/artifacts：版本化共享产物（含 CAS 内容解析）。
async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    // 空间不存在 → 404（区分"空项目"与"不存在"）。
    let space = coordinator
        .get_project_space(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let artifacts = coordinator
        .list_artifacts(&space)
        .await
        .map_err(|e| error_response(&e))?;
    let mut items = Vec::with_capacity(artifacts.len());
    for a in artifacts {
        let content_preview = coordinator
            .cas()
            .get_text(a.content_ref.strip_prefix("cas://sha256:").unwrap_or(""))
            .map(|c| c.chars().take(200).collect::<String>())
            .unwrap_or_default();
        items.push(json!({
            "artifact_id": a.artifact_id,
            "kind": a.kind,
            "version": a.version,
            "producer": a.producer,
            "content_ref": a.content_ref,
            "source_refs": a.source_refs,
            "review_state": format!("{:?}", a.review_state),
            // 五期：版本链链接（返工重跑登记时指向前版）——前端版本时间线/
            // v1v2 差异按此字段合并链（null = 首版/无链接）。
            "supersedes_artifact_id": a.supersedes_artifact_id,
            "created_at": a.created_at,
            "preview": content_preview,
        }));
    }
    Ok(Json(json!({ "project_id": id, "artifacts": items })))
}

/// POST /tasks/{id}/handoff：结构化接力（team_id 在请求体；任务 id 按团队命名空间解析）。
async fn submit_handoff(
    State(state): State<Arc<AppState>>,
    AxumPath(task_id): AxumPath<String>,
    Json(req): Json<HandoffHttpRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let fields = HandoffFields {
        to_member: req.to_member,
        completed_summary: req.completed_summary,
        open_issues: req.open_issues,
        output_artifact_refs: req.output_artifact_refs,
        evidence_refs: req.evidence_refs,
        suggested_next_actions: req.suggested_next_actions,
        known_risks: req.known_risks,
    };
    let handoff = coordinator
        .submit_handoff(&req.team_id, &task_id, &req.from_member, &fields)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({ "handoff": handoff })))
}

/// POST /tasks/{id}/human-result：人节点提交结果（落盘后运行循环自动唤醒下游）。
async fn submit_human_result(
    State(state): State<Arc<AppState>>,
    AxumPath(task_id): AxumPath<String>,
    Json(req): Json<HumanResultHttpRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let artifact = coordinator
        .record_human_result(&req.team_id, &task_id, &req.result)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({ "artifact": artifact })))
}

/// GET /teams/templates：已采纳团队模板。
async fn list_templates(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let templates = coordinator.templates().list_templates();
    Ok(Json(json!({ "templates": templates })))
}

/// GET /teams/templates/proposals：模板提案列表（只提案，不自动启用）。
async fn list_proposals(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let proposals = coordinator.templates().list_proposals();
    Ok(Json(json!({ "proposals": proposals })))
}

/// POST /teams/templates/proposals/{proposal_id}/adopt：采纳 → 进入模板注册表（幂等）。
async fn adopt_proposal(
    State(state): State<Arc<AppState>>,
    AxumPath(proposal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let template = coordinator
        .templates()
        .adopt_proposal(&proposal_id)
        .map_err(|m| {
            let code = if m.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(json!({ "error": m })))
        })?;
    Ok(Json(json!({ "template": template })))
}

/// POST /teams/templates/proposals/{proposal_id}/reject：拒绝提案（保留记录，可审计；已采纳 → 400）。
async fn reject_proposal(
    State(state): State<Arc<AppState>>,
    AxumPath(proposal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    coordinator
        .templates()
        .reject_proposal(&proposal_id)
        .map_err(|m| {
            let code = if m.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(json!({ "error": m })))
        })?;
    Ok(Json(json!({
        "proposal_id": proposal_id,
        "status": "rejected",
    })))
}

// ---------------------------------------------------------------------------
// 九期（一路）：TrackedRoleWorker 合并检测 / 空 ChangeSet 守卫（真实 git 仓库）
// ---------------------------------------------------------------------------
