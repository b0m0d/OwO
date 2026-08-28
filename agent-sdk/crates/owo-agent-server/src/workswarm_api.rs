//! WorkSwarm S0 HTTP 路由（§8.5 / §9.0 S0）。
//!
//! 路由面：
//! - `POST /teams` 创建 single/team/swarmflow 运行（后台运行循环驱动阶段推进）；
//! - `GET /teams` 团队运行列表（含运行中标志）；
//! - `GET /teams/{id}` 成员、预算、状态（+ 任务视图 + 审计尾迹）；
//! - `GET /teams/{id}/tasks` TeamRun 的任务图（步骤 × 状态）；
//! - `GET /teams/{id}/events` 团队事件流 SSE（审计重放 + 状态轮询；`?format=json` 一次性快照）；
//! - `POST /teams/{id}/steer` continue/steer/replace/cancel/retry（R2：retry 局部重试 + 中断恢复）；
//! - `GET /projects/{id}` Project Space 摘要；
//! - `GET /projects/{id}/artifacts` 版本化共享产物（ref 列表）；
//! - `POST /tasks/{id}/handoff` 结构化接力（team_id 在请求体）；
//! - `POST /tasks/{id}/human-result` Human 节点提交结果（team_id 在请求体）；
//! - `GET /teams/templates` 已采纳模板；
//! - `GET /teams/templates/proposals` 模板提案（只提案，不自动启用）；
//! - `POST /teams/templates/proposals/{proposal_id}/adopt` 采纳提案；
//! - `POST /teams/templates/proposals/{proposal_id}/reject` 拒绝提案（保留记录，可审计）。
//!
//! S0 边界：产物经 CAS ref 传递（大对象不进响应体）；agent 角色经
//! `Agent::run_subagent` 模型驱动；内置 echo/sleep/fail worker 供测试与演示。

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::goal::{Worker, WorkerRegistry};
use owo_agent_core::project_space_store::SqliteProjectSpaceStore;
use owo_agent_core::workswarm::{
    CreateTeamRequest, HandoffFields, RoleSpec, RoleWorker, SteerCommand, TeamCoordinator,
    TeamTemplateRegistry, WorkSwarmError,
};
use owo_agent_protocol::{TeamMode, TeamRunStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

use owo_agent_server::AppState;

// ---------------------------------------------------------------------------
// 状态（进程内单例协调器，懒初始化；目录 = data_root/workswarm）
// ---------------------------------------------------------------------------

/// WorkSwarm 服务端状态：`data_root/workswarm/{space.db, cas, templates, runs}`。
#[derive(Clone)]
pub struct WorkSwarmState {
    inner: Arc<Inner>,
}

struct Inner {
    base_dir: PathBuf,
    coordinator: std::sync::OnceLock<Arc<TeamCoordinator>>,
}

impl WorkSwarmState {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_dir,
                coordinator: std::sync::OnceLock::new(),
            }),
        }
    }

    /// 获取（并懒初始化）协调器。
    pub fn coordinator(&self) -> Result<Arc<TeamCoordinator>, WorkSwarmError> {
        if let Some(c) = self.inner.coordinator.get() {
            return Ok(c.clone());
        }
        let base = self.inner.base_dir.clone();
        std::fs::create_dir_all(&base)
            .map_err(|e| WorkSwarmError::Io(format!("创建 workswarm 目录失败：{e}")))?;
        let store_path = base.join("space.db");
        let store = SqliteProjectSpaceStore::open(&store_path).map_err(WorkSwarmError::Store)?;
        let cas = owo_agent_core::cas_store::CasStore::new(base.join("cas"))
            .map_err(|e| WorkSwarmError::Io(format!("CAS 初始化失败：{e}")))?;
        let templates = Arc::new(TeamTemplateRegistry::new(base.join("templates")));
        let audit = Arc::new(std::sync::Mutex::new(
            owo_agent_core::audit::AuditLog::default(),
        ));
        let mut coordinator = TeamCoordinator::new(
            Arc::new(store)
                as Arc<dyn owo_agent_core::project_space_store::ProjectSpaceStoreBackend>,
            templates,
            cas,
            base.join("runs"),
        );
        coordinator.attach_audit(audit);
        let coordinator = Arc::new(coordinator);
        // 并发首用：set 失败者取先到者（幂等）。
        let _ = self.inner.coordinator.set(coordinator.clone());
        // R2 重启恢复：启动时全量中断扫描（幂等；只落「interrupted」识别标记并
        // 把磁盘遗留的 Running 步骤转为可恢复状态——绝不在启动时静默重放任何写操作；
        // 恢复必须经用户显式 continue / retry 发起）。
        {
            let boot_coordinator = Arc::clone(&coordinator);
            tokio::spawn(async move {
                match boot_coordinator.detect_interrupted().await {
                    Ok(scan) if !scan.interrupted.is_empty() => tracing::info!(
                        teams = ?scan
                            .interrupted
                            .iter()
                            .map(|r| r.team_id.as_str())
                            .collect::<Vec<_>>(),
                        "workswarm 启动扫描：识别到中断运行（等待显式 continue/retry 恢复）"
                    ),
                    Ok(scan) if !scan.unreadable_states.is_empty() => tracing::warn!(
                        teams = ?scan.unreadable_states,
                        "workswarm 启动扫描：存在损坏的运行状态文件（已原样保留，未被覆盖；需人工修复）"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(%e, "workswarm 启动中断扫描失败"),
                }
            });
        }
        self.inner
            .coordinator
            .get()
            .cloned()
            .ok_or_else(|| WorkSwarmError::Run("协调器初始化失败".to_string()))
    }
}

// ---------------------------------------------------------------------------
// 内层 worker（agent = 模型驱动；echo/sleep/fail = 内置演示/测试）
// ---------------------------------------------------------------------------

/// 真实 Agent 子代理 worker（name="agent"）：prompt → `Agent::run_subagent`。
pub struct AgentSubagentWorker {
    agent: Arc<owo_agent_core::Agent>,
    workspace: PathBuf,
}

#[async_trait]
impl Worker for AgentSubagentWorker {
    fn name(&self) -> &str {
        "agent"
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "agent 步骤缺少 prompt 参数".to_string())?;
        let read_only = input
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if std::env::var("OPENAI_API_KEY")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(
                "缺少 OPENAI_API_KEY，agent 角色无法调用模型（请配置凭据后重试）".to_string(),
            );
        }
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                std::env::var("OWO_AGENT_MODEL")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "gpt-4.1-mini".to_string());
        let output = self
            .agent
            .run_subagent(&self.workspace, &model, prompt, read_only)
            .await
            .map_err(|e| format!("agent 子代理执行失败：{e}"))?;
        Ok(output)
    }
}

struct EchoWorker;

#[async_trait]
impl Worker for EchoWorker {
    fn name(&self) -> &str {
        "echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string()))
    }
}

struct SleepWorker;

#[async_trait]
impl Worker for SleepWorker {
    fn name(&self) -> &str {
        "sleep"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let ms = input
            .get("ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(600_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("slept {ms}ms"))
    }
}

struct FailWorker;

#[async_trait]
impl Worker for FailWorker {
    fn name(&self) -> &str {
        "fail"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Err(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "fail worker 注入失败".to_string()))
    }
}

/// 按角色 worker 名解析内层 worker（"agent"/缺省 = 模型驱动）。
fn inner_worker_for(state: &AppState, worker_name: Option<&str>) -> Option<Arc<dyn Worker>> {
    match worker_name.map(str::trim).filter(|w| !w.is_empty()) {
        Some("echo") => Some(Arc::new(EchoWorker)),
        Some("sleep") => Some(Arc::new(SleepWorker)),
        Some("fail") => Some(Arc::new(FailWorker)),
        _ => Some(Arc::new(AgentSubagentWorker {
            agent: Arc::clone(&state.agent),
            workspace: state.workspace.clone(),
        })),
    }
}

/// 构建团队运行 worker 注册表（成员名 → RoleWorker）。失败返回 None（运行任务记录后退出）。
fn build_run_registry(
    coordinator: &Arc<TeamCoordinator>,
    state: &AppState,
    team_id: &str,
) -> Option<WorkerRegistry> {
    let meta = coordinator.load_run_meta(team_id).ok()?;
    let registry = WorkerRegistry::new();
    for r in &meta.roles {
        let member_id = format!("m-{}", r.role);
        let inner = inner_worker_for(state, r.worker.as_deref())?;
        registry.register(Arc::new(RoleWorker::new(
            Arc::clone(coordinator),
            team_id.to_string(),
            member_id,
            r.role.clone(),
            inner,
        )));
    }
    Some(registry)
}

// ---------------------------------------------------------------------------
// 后台运行循环（阶段驱动 + 人节点门闩）
// ---------------------------------------------------------------------------

/// 运行循环存活守卫（R2）：进程内声明「该团队有活动运行循环」——
/// 中断识别据此区分「循环存活（含批次间隙/人节点门闩）」与「重启遗留」。
/// Drop 兜底撤销（覆盖所有 return 路径与 panic 展开栈）。
struct LoopAliveGuard {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
}

impl LoopAliveGuard {
    fn new(coordinator: &Arc<TeamCoordinator>, team_id: &str) -> Self {
        let guard = Self {
            coordinator: Arc::clone(coordinator),
            team_id: team_id.to_string(),
        };
        guard.coordinator.set_loop_alive(team_id, true);
        guard
    }
}

impl Drop for LoopAliveGuard {
    fn drop(&mut self) {
        self.coordinator.set_loop_alive(&self.team_id, false);
    }
}

/// 团队运行循环：run_phase 阶段推进；人节点等待窗口内轮询（结果落盘即唤醒；cancel 即终止）。
///
/// 每轮外层迭代重建 worker 注册表（steer/replace 修改角色规格后新阶段生效）。
async fn run_team_loop(state: Arc<AppState>, coordinator: Arc<TeamCoordinator>, team_id: String) {
    let _alive = LoopAliveGuard::new(&coordinator, &team_id);
    let mut backoff = Duration::from_secs(1);
    loop {
        let Some(registry) = build_run_registry(&coordinator, &state, &team_id) else {
            tracing::error!(team_id = %team_id, "workswarm 运行循环：worker 注册表构建失败，运行终止");
            return;
        };
        match coordinator.run_phase(&team_id, &registry).await {
            Err(e) => {
                // 存储/IO 异常：退避重试，避免热循环打爆磁盘。
                tracing::warn!(team_id = %team_id, %e, "run_phase 失败，退避重试");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Ok(outcome) => match outcome {
                owo_agent_core::PhaseOutcome::MoreReady => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
                owo_agent_core::PhaseOutcome::Done => {
                    if let Err(e) = coordinator.finalize_success(&team_id).await {
                        tracing::error!(team_id = %team_id, %e, "workswarm 收尾失败");
                    }
                    return;
                }
                owo_agent_core::PhaseOutcome::Failed
                | owo_agent_core::PhaseOutcome::Aborted
                | owo_agent_core::PhaseOutcome::Finished => return,
                owo_agent_core::PhaseOutcome::AwaitingHuman { waits } => {
                    tracing::info!(team_id = %team_id, ?waits, "团队等待人节点，进入门闩等待");
                    // 门闩：人结果录入（落盘）→ 下一次 run_phase 自动唤醒；
                    // cancel → 立即取消收尾。200ms 轮询（S0 无推送通道，保持最小实现）。
                    let cancel = coordinator.cancel_token(&team_id);
                    loop {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        if cancel.is_cancelled() {
                            if let Err(e) = coordinator
                                .apply_steer(&team_id, &SteerCommand::Cancel)
                                .await
                            {
                                tracing::error!(team_id = %team_id, %e, "取消收尾失败");
                            }
                            return;
                        }
                        match coordinator.run_phase(&team_id, &registry).await {
                            Ok(owo_agent_core::PhaseOutcome::AwaitingHuman { .. }) => continue,
                            Ok(owo_agent_core::PhaseOutcome::Done) => {
                                let _ = coordinator.finalize_success(&team_id).await;
                                return;
                            }
                            Ok(owo_agent_core::PhaseOutcome::MoreReady) => break, // 外层循环重建注册表继续
                            Ok(_) => return,                                      // 终态
                            Err(e) => {
                                tracing::warn!(team_id = %team_id, %e, "门闩等待中 run_phase 失败，退避");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 请求模型
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

// ---------------------------------------------------------------------------
// 响应辅助
// ---------------------------------------------------------------------------

fn error_response(e: &WorkSwarmError) -> (StatusCode, Json<Value>) {
    let (code, msg) = match e {
        WorkSwarmError::Validation(m) => (StatusCode::BAD_REQUEST, m.clone()),
        WorkSwarmError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
        WorkSwarmError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
        // R2：状态文件损坏 → 明确 500 失败；原文件已保留、未被覆盖（消息中注明），
        // 不返回可重试语义——需人工修复后才能继续。
        WorkSwarmError::CorruptState(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    };
    (code, Json(json!({ "error": msg })))
}

fn task_view(state: &owo_agent_core::goal::GoalRunState) -> Vec<Value> {
    state
        .plan
        .steps
        .iter()
        .map(|s| {
            let rec = state.records.get(&s.id);
            json!({
                "task_id": s.id,
                "worker": s.worker,
                "role": s.worker.strip_prefix("m-").unwrap_or(&s.worker),
                "depends_on": s.depends_on,
                "status": format!("{:?}", rec.map(|r| r.status).unwrap_or(
                    owo_agent_core::plan::StepStatus::Pending
                )),
                "attempts": rec.map(|r| r.attempts).unwrap_or(0),
                "error": rec.and_then(|r| r.error.clone()),
            })
        })
        .collect()
}

/// 团队审计尾迹（最近 20 条；S0 可见性：关键动作可回看）。
fn audit_tail(coordinator: &Arc<TeamCoordinator>, team_id: &str) -> Vec<Value> {
    let Some(log) = coordinator.audit_log() else {
        return Vec::new();
    };
    let Ok(entries) = log.lock() else {
        return Vec::new();
    };
    entries
        .entries
        .iter()
        .filter(|e| e.session_id == team_id)
        .rev()
        .take(20)
        .map(|e| json!({ "ts": e.ts, "event": e.event, "tool": e.tool, "detail": e.detail }))
        .collect()
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/teams", post(create_team).get(list_teams))
        .route("/teams/{id}", get(get_team))
        .route("/teams/{id}/tasks", get(get_team_tasks))
        .route("/teams/{id}/events", get(team_events))
        .route("/teams/{id}/steer", post(steer_team))
        .route("/projects/{id}", get(get_project_space))
        .route("/projects/{id}/artifacts", get(list_artifacts))
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
    let req = CreateTeamRequest {
        goal_id: req.goal_id,
        objective: req.objective,
        mode,
        template_id: req.template_id,
        roles: req.roles,
        budget: req.budget,
        human_policy: req.human_policy,
    };
    let team = coordinator
        .create_team_run(&req)
        .await
        .map_err(|e| error_response(&e))?;
    let team_id = team.team_id.clone();
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
        return Ok(Json(json!({
            "team_id": team.team_id,
            "status": format!("{:?}", team.status),
            "active": coordinator.is_run_active(&id),
            "interrupted": coordinator.is_interrupted(&id),
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
