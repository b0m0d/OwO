//! 统一 Human Inbox（八期 · 第三路：统一 Human Inbox 后端与直接处理）。
//!
//! 把四类「待人工处理」收编为**统一、可领取、可直接处理**的正式待办系统：
//! - `human_result`：人节点任务（Human 结果录入）；
//! - `artifact_review`：产物评审（approve / request_changes / reject）；
//! - `change_set`：ChangeSet 接受/拒绝（八期二路；分派点已预留）；
//! - `step_retry`：失败步骤重试。
//!
//! 路由面（经 [`super::router`] 合并挂载，lib.rs 装配零改动）：
//! - `GET  /human/inbox`：四类待办统一列表（live 扫描 + overlay 协作状态合成）；
//! - `GET  /human/inbox/{id}`：单条详情（协作状态记录）；
//! - `POST /human/inbox/{id}/claim`：领取（同一待办仅一个用户成功；同人幂等）；
//! - `POST /human/inbox/{id}/release`：释放（仅领取者）；
//! - `POST /human/inbox/{id}/resolve`：直接处理——**按类型分派到既有领域能力**
//!   （人节点结果录入 / 评审提交 / steer retry / ChangeSet accept-reject），
//!   不绕过原权限与幂等检查；CAS 占位→分派→终态，同键重放返回缓存结果、
//!   不产生重复 Artifact / 重复写入 / 重复 retry。
//!
//! 数据来源（live 扫描，重启自然恢复；覆盖层只管协作状态）：
//! - 团队扫描：`list_team_runs` + `load_run_state`（人成员未终态步骤 → human_result；
//!   Failed/Aborted 步骤且团队非 succeeded/cancelled → step_retry）；
//! - 项目扫描：`get_project_space().artifacts` 中 `review_state == PendingReview`
//!   → artifact_review；
//! - ChangeSet：八期二路 `change_set` 落地后由其创建路径调用
//!   [`human_inbox_store::HumanInboxStore::ensure_item`] 登记（本模块 resolve
//!   分派点已预留，当前返回结构化 409）。

use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::change_set_store::{ChangeSetStatus, ChangeSetStore};
use owo_agent_core::project_space_store::{
    apply_artifact_review, self_approve_allowed, ArtifactReviewError, ArtifactReviewInput,
};
use owo_agent_core::workswarm::SteerCommand;
use owo_agent_core::{StepStatus, WorkSwarmError};
use owo_agent_protocol::{ArtifactReviewDecision, ReviewState, RuntimeBinding, TeamRunStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use owo_agent_server::AppState;

use super::human_inbox_store::{
    is_valid_kind, open_shared, ClaimError, HumanWorkItem, InboxItemDraft, ResolveLeaseError,
    KIND_ARTIFACT_REVIEW, KIND_CHANGE_SET, KIND_HUMAN_RESULT, KIND_STEP_RETRY,
};

/// 扫描上限（防御异常规模；正常团队/项目数远低于此）。
const MAX_SCAN_TEAMS: usize = 24;
const MAX_SCAN_PROJECTS: usize = 12;

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

/// 路由（`Router<Arc<AppState>>`；由 [`super::router`] 合并后统一 `with_state`）。
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/human/inbox", get(list_inbox))
        .route("/human/inbox/{id}", get(get_inbox_item))
        .route("/human/inbox/{id}/claim", post(claim_item))
        .route("/human/inbox/{id}/release", post(release_item))
        .route("/human/inbox/{id}/resolve", post(resolve_item))
}

fn inbox_store(state: &AppState) -> Arc<super::human_inbox_store::HumanInboxStore> {
    // 与 space.db 同目录：data_root/workswarm/human-inbox.json。
    let path = state.data_root.join("workswarm").join("human-inbox.json");
    open_shared(&path)
}

// ---------------------------------------------------------------------------
// live 扫描（领域对象仍由各自存储持有；此处只产出草稿 + 详情）
// ---------------------------------------------------------------------------

/// 任务视图元素（与 workswarm_api::task_view 同口径的最小子集）。
struct TaskView<'a> {
    step_id: &'a str,
    worker: &'a str,
    role: &'a str,
    status: StepStatus,
    attempts: u32,
    error: Option<&'a str>,
}

fn task_views<'a>(state: &'a owo_agent_core::GoalRunState) -> Vec<TaskView<'a>> {
    state
        .plan
        .steps
        .iter()
        .map(|s| {
            let record = state.records.get(&s.id);
            TaskView {
                step_id: &s.id,
                worker: &s.worker,
                role: s.worker.strip_prefix("m-").unwrap_or(&s.worker),
                status: record.map(|r| r.status).unwrap_or(StepStatus::Pending),
                attempts: record.map(|r| r.attempts).unwrap_or(0),
                error: record.and_then(|r| r.error.as_deref()),
            }
        })
        .collect()
}

/// 团队扫描：人节点待录入（human_result）+ 失败步骤待重试（step_retry）。
async fn scan_team_drafts(
    coordinator: &Arc<owo_agent_core::TeamCoordinator>,
    drafts: &mut Vec<(InboxItemDraft, Value)>,
) {
    let runs = match coordinator.list_team_runs().await {
        Ok(runs) => runs,
        Err(_) => return, // 存储不可用 → 其余来源仍可工作（列表降级为空）
    };
    for team in runs.into_iter().take(MAX_SCAN_TEAMS) {
        let team_id = team.team_id.clone();
        let status: TeamRunStatus = team.status;
        // 重试类：failed 团队保留（正是 retry 的合法场景）；succeeded/cancelled 排除。
        let retry_eligible = !matches!(status, TeamRunStatus::Succeeded | TeamRunStatus::Cancelled);
        // 人节点：任何非终态团队都可能存在未完成人节点步骤。
        if status.is_terminal() && !retry_eligible {
            continue;
        }
        let run_state = match coordinator.load_run_state(&team_id) {
            Ok(s) => s,
            Err(_) => continue, // 状态缺失/损坏 → 跳过该团队（详情页有明确报错）
        };
        // 人成员集合（member_id → role）。
        let mut human_roles: BTreeMap<&str, &str> = BTreeMap::new();
        for member in &team.members {
            if matches!(member.runtime_binding, RuntimeBinding::Human { .. }) {
                human_roles.insert(member.member_id.as_str(), member.role.as_str());
            }
        }
        let interrupted = coordinator.is_interrupted(&team_id);
        for t in task_views(&run_state) {
            let is_human_step = human_roles.contains_key(t.worker);
            if is_human_step
                && !matches!(
                    t.status,
                    StepStatus::Succeeded | StepStatus::Failed | StepStatus::Aborted
                )
            {
                drafts.push((
                    InboxItemDraft {
                        kind: KIND_HUMAN_RESULT.to_string(),
                        team_id: team_id.clone(),
                        project_id: team.project_space_id.clone(),
                        target_id: t.step_id.to_string(),
                        summary: format!(
                            "人节点结果待录入：{}（状态 {}）",
                            t.role,
                            format!("{:?}", t.status).to_lowercase()
                        ),
                    },
                    json!({
                        "step_id": t.step_id,
                        "role": t.role,
                        "step_status": format!("{:?}", t.status),
                        "team_status": format!("{status:?}"),
                    }),
                ));
            }
            if retry_eligible && matches!(t.status, StepStatus::Failed | StepStatus::Aborted) {
                drafts.push((
                    InboxItemDraft {
                        kind: KIND_STEP_RETRY.to_string(),
                        team_id: team_id.clone(),
                        project_id: team.project_space_id.clone(),
                        target_id: t.step_id.to_string(),
                        summary: format!(
                            "失败步骤待重试：{}（第 {} 次尝试{}）",
                            t.role,
                            t.attempts,
                            if interrupted {
                                "，团队已中断"
                            } else {
                                ""
                            }
                        ),
                    },
                    json!({
                        "step_id": t.step_id,
                        "role": t.role,
                        "attempts": t.attempts,
                        "error": t.error,
                        "team_status": format!("{status:?}"),
                        "interrupted": interrupted,
                    }),
                ));
            }
        }
        // 八期（二路）：ChangeSet 待接受/拒绝（pending_review）→ change_set 待办。
        let cs_store = ChangeSetStore::new(coordinator.run_dir());
        if let Ok(change_sets) = cs_store.list_for_team(&team_id) {
            for cs in change_sets
                .iter()
                .filter(|c| c.status == ChangeSetStatus::PendingReview)
            {
                drafts.push((
                    InboxItemDraft {
                        kind: KIND_CHANGE_SET.to_string(),
                        team_id: team_id.clone(),
                        project_id: team.project_space_id.clone(),
                        target_id: cs.change_set_id.clone(),
                        summary: format!(
                            "ChangeSet 待审批：{}（步骤 {}，{} 个文件）",
                            cs.role,
                            cs.step_id,
                            cs.changed_files.len()
                        ),
                    },
                    json!({
                        "change_set_id": cs.change_set_id,
                        "step_id": cs.step_id,
                        "role": cs.role,
                        "changed_files": cs.changed_files,
                        "diff_ref": cs.diff_ref,
                        "status": format!("{:?}", cs.status),
                    }),
                ));
            }
        } // 变更存储不可用 → 该团队 ChangeSet 待办降级为空
    }
}

/// 项目扫描：PendingReview 产物（artifact_review）。
async fn scan_review_drafts(
    coordinator: &Arc<owo_agent_core::TeamCoordinator>,
    drafts: &mut Vec<(InboxItemDraft, Value)>,
) {
    let runs = match coordinator.list_team_runs().await {
        Ok(runs) => runs,
        Err(_) => return,
    };
    let mut project_to_team: BTreeMap<&str, &str> = BTreeMap::new();
    let mut projects: Vec<&str> = Vec::new();
    for team in &runs {
        if let Some(pid) = team.project_space_id.as_deref() {
            if !projects.contains(&pid) {
                projects.push(pid);
                project_to_team.insert(pid, team.team_id.as_str());
            }
        }
    }
    for pid in projects.into_iter().take(MAX_SCAN_PROJECTS) {
        // 产物元数据直接走 store（space.artifacts 是 id 列表；此处需 review_state 等字段）。
        let artifacts = match coordinator.store().list_artifacts_by_project(pid).await {
            Ok(a) => a,
            Err(_) => continue,
        };
        for artifact in &artifacts {
            if artifact.review_state != ReviewState::PendingReview {
                continue;
            }
            let team_id = if artifact.team_id.is_empty() {
                project_to_team
                    .get(pid)
                    .map(|s| (*s).to_string())
                    .unwrap_or_default()
            } else {
                artifact.team_id.clone()
            };
            drafts.push((
                InboxItemDraft {
                    kind: KIND_ARTIFACT_REVIEW.to_string(),
                    team_id,
                    project_id: Some(pid.to_string()),
                    target_id: artifact.artifact_id.clone(),
                    summary: format!("待评审：{} v{}（{}）", artifact.kind, artifact.version, pid),
                },
                json!({
                    "artifact_id": artifact.artifact_id,
                    "version": artifact.version,
                    "kind": artifact.kind,
                    "format": artifact.format,
                    "review_state": format!("{:?}", artifact.review_state),
                }),
            ));
        }
    }
}

/// 全量扫描：四类来源合一（团队任务/失败步骤 + ChangeSet + 产物评审）。
async fn collect_drafts(state: &AppState) -> Vec<(InboxItemDraft, Value)> {
    let mut drafts: Vec<(InboxItemDraft, Value)> = Vec::new();
    if let Ok(coordinator) = state.workswarm.coordinator() {
        scan_team_drafts(&coordinator, &mut drafts).await;
        scan_review_drafts(&coordinator, &mut drafts).await;
    }
    drafts
}

/// 扫描并把全部 live 候选登记进覆盖层（list/claim/resolve/release 共用前置）。
/// 单项操作前也执行全量登记：保证客户端未先 GET 列表时 claim/resolve 仍可达。
async fn ensure_all_items(state: &AppState) -> Arc<super::human_inbox_store::HumanInboxStore> {
    let store = inbox_store(state);
    for (draft, _) in collect_drafts(state).await {
        if is_valid_kind(&draft.kind) {
            store.ensure_item(&draft);
        }
    }
    store
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------

/// GET /human/inbox 查询参数（四路冻结：kind/status/team_id/project_id 过滤，缺省全量）。
#[derive(Debug, Default, Deserialize)]
struct InboxQuery {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    team_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

impl InboxQuery {
    fn matches(&self, item: &HumanWorkItem) -> bool {
        if let Some(kind) = self.kind.as_deref() {
            if !kind.is_empty() && item.kind != kind {
                return false;
            }
        }
        if let Some(status) = self.status.as_deref() {
            if !status.is_empty() && wire_status(item) != status {
                return false;
            }
        }
        if let Some(team) = self.team_id.as_deref() {
            if !team.is_empty() && item.team_id != team {
                return false;
            }
        }
        if let Some(project) = self.project_id.as_deref() {
            if !project.is_empty() && item.project_id.as_deref() != Some(project) {
                return false;
            }
        }
        true
    }
}

/// wire 状态（冻结枚举 open|claimed|resolved；瞬态 resolving 对外呈现为 claimed）。
fn wire_status(item: &HumanWorkItem) -> &str {
    if item.status == super::human_inbox_store::STATUS_RESOLVING {
        super::human_inbox_store::STATUS_CLAIMED
    } else {
        &item.status
    }
}

/// GET /human/inbox：四类待办统一列表。
async fn list_inbox(
    State(state): State<Arc<AppState>>,
    AxumQuery(query): AxumQuery<InboxQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = ensure_all_items(&state).await;
    let drafts = collect_drafts(&state).await;
    let mut items = Vec::new();
    let mut counts: BTreeMap<String, u64> = BTreeMap::from([
        (KIND_HUMAN_RESULT.to_string(), 0),
        (KIND_ARTIFACT_REVIEW.to_string(), 0),
        (KIND_CHANGE_SET.to_string(), 0),
        (KIND_STEP_RETRY.to_string(), 0),
    ]);
    for (draft, detail) in drafts {
        if !is_valid_kind(&draft.kind) {
            continue;
        }
        let item_id = draft.item_id();
        let item = match store.get(&item_id) {
            Some(i) => i,
            None => continue,
        };
        if item.status == super::human_inbox_store::STATUS_RESOLVED {
            continue; // 已解决待办不再出现
        }
        if !query.matches(&item) {
            continue;
        }
        *counts.entry(item.kind.clone()).or_insert(0) += 1;
        items.push(item_with_detail(&item, detail));
    }
    Ok(Json(json!({
        "items": items,
        "counts": counts,
    })))
}

fn item_with_detail(item: &HumanWorkItem, detail: Value) -> Value {
    json!({
        "item_id": item.item_id,
        "kind": item.kind,
        "status": wire_status(item),
        "assignee": item.assignee,
        "team_id": item.team_id,
        "project_id": item.project_id,
        "target_id": item.target_id,
        "summary": item.summary,
        "created_at": item.created_at,
        "claimed_at": item.claimed_at,
        "resolved_at": item.resolved_at,
        "detail": detail,
    })
}

/// GET /human/inbox/{id}：单条协作状态记录。
async fn get_inbox_item(
    State(state): State<Arc<AppState>>,
    AxumPath(item_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = ensure_all_items(&state).await;
    let item = store.get(&item_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("待办不存在：{item_id}") })),
        )
    })?;
    Ok(Json(json!({ "item": item })))
}

/// 领取/释放请求体。
#[derive(Debug, Deserialize)]
struct ActorRequest {
    user: String,
}

fn actor_of(body: &ActorRequest) -> Result<String, (StatusCode, Json<Value>)> {
    let user = body.user.trim().to_string();
    if user.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "user 不能为空" })),
        ));
    }
    Ok(user)
}

fn claim_error_response(item_id: &str, e: ClaimError) -> (StatusCode, Json<Value>) {
    match e {
        ClaimError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("待办不存在：{item_id}") })),
        ),
        ClaimError::Conflict { by } => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": match by.as_deref() {
                    Some(user) => format!("待办已被 {user} 领取（同一待办仅允许一个用户处理）"),
                    None => "待办当前状态不允许该操作".to_string(),
                },
                "claimed_by": by,
            })),
        ),
    }
}

/// POST /human/inbox/{id}/claim：领取（同人重复领取幂等返回）。
async fn claim_item(
    State(state): State<Arc<AppState>>,
    AxumPath(item_id): AxumPath<String>,
    Json(body): Json<ActorRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let user = actor_of(&body)?;
    let store = ensure_all_items(&state).await;
    match store.claim(&item_id, &user) {
        Ok(item) => Ok(Json(json!({ "item": item }))),
        Err(e) => Err(claim_error_response(&item_id, e)),
    }
}

/// POST /human/inbox/{id}/release：释放（仅领取者）。
async fn release_item(
    State(state): State<Arc<AppState>>,
    AxumPath(item_id): AxumPath<String>,
    Json(body): Json<ActorRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let user = actor_of(&body)?;
    let store = ensure_all_items(&state).await;
    match store.release(&item_id, &user) {
        Ok(item) => Ok(Json(json!({ "item": item }))),
        Err(e) => Err(claim_error_response(&item_id, e)),
    }
}

/// resolve 请求体（按 kind 取用对应字段；其余忽略）。
#[derive(Debug, Deserialize)]
struct ResolveRequest {
    /// 处理人（审计/领取校验用；review 类缺省兼作 reviewer）。
    #[serde(default)]
    user: Option<String>,
    /// 幂等键（四路冻结名 `resolution_id`；兼容别名 `idempotency_key`）。
    #[serde(default)]
    resolution_id: Option<String>,
    /// 幂等键别名（兼容早期口径）。
    #[serde(default)]
    idempotency_key: Option<String>,
    /// change_set：accept | reject（四路冻结口径）。
    #[serde(default)]
    action: Option<String>,
    /// human_result：人节点结果文本。
    #[serde(default)]
    result: Option<String>,
    /// artifact_review：approve | request_changes | reject。
    #[serde(default)]
    decision: Option<String>,
    /// artifact_review：评审者（缺省用 user）。
    #[serde(default)]
    reviewer: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    /// artifact_review：乐观并发目标版本。
    #[serde(default)]
    expected_version: Option<u32>,
    /// step_retry / change_set：说明（缺省确定性中文）。
    #[serde(default)]
    note: Option<String>,
}

impl ResolveRequest {
    /// 幂等键：`resolution_id` 优先，其次 `idempotency_key`，缺省由分派参数派生。
    fn explicit_idempotency_key(&self) -> Option<String> {
        self.resolution_id
            .clone()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| {
                self.idempotency_key
                    .clone()
                    .filter(|k| !k.trim().is_empty())
            })
    }
}

/// resolve 分派失败 → HTTP 响应。
fn dispatch_error_response(e: DispatchError) -> (StatusCode, Json<Value>) {
    match e {
        DispatchError::WorkSwarm(err) => match &err {
            WorkSwarmError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": err.to_string() })),
            ),
            WorkSwarmError::Validation(_) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            ),
            WorkSwarmError::Conflict(_) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": err.to_string() })),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            ),
        },
        DispatchError::Review(err) => match &err {
            ArtifactReviewError::ArtifactNotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": err.to_string() })),
            ),
            ArtifactReviewError::Forbidden(_) => (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": err.to_string() })),
            ),
            ArtifactReviewError::VersionConflict { .. }
            | ArtifactReviewError::Superseded(_)
            | ArtifactReviewError::IdempotencyConflict(_) => (
                StatusCode::CONFLICT,
                Json(json!({ "error": err.to_string() })),
            ),
            ArtifactReviewError::Validation(_) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": err.to_string() })),
            ),
            ArtifactReviewError::Store(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            ),
        },
        DispatchError::BadRequest(msg) => (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))),
        DispatchError::Upstream(code, err_json) => (code, Json(err_json)),
    }
}

enum DispatchError {
    WorkSwarm(WorkSwarmError),
    Review(ArtifactReviewError),
    BadRequest(String),
    /// 分派目标端点的原始错误（直通：状态码 + 响应体原样透出）。
    Upstream(StatusCode, Value),
}

impl DispatchError {
    fn message(&self) -> String {
        match self {
            DispatchError::WorkSwarm(err) => err.to_string(),
            DispatchError::Review(err) => err.to_string(),
            DispatchError::BadRequest(msg) => msg.clone(),
            DispatchError::Upstream(_, err_json) => err_json
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("分派目标端点失败")
                .to_string(),
        }
    }
}

/// POST /human/inbox/{id}/resolve：直接处理（分派到既有领域能力）。
///
/// 三段式幂等：CAS 占位（open/claimed → resolving）→ 分派 → 终态。
/// 分派失败回滚原状态并记录 last_error（可重试）；同键重放返回缓存结果。
async fn resolve_item(
    State(state): State<Arc<AppState>>,
    AxumPath(item_id): AxumPath<String>,
    Json(body): Json<ResolveRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = ensure_all_items(&state).await;
    let item = store.get(&item_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("待办不存在：{item_id}") })),
        )
    })?;
    if !is_valid_kind(&item.kind) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("未知待办类型：{}", item.kind) })),
        ));
    }
    let user = body.user.clone().unwrap_or_default().trim().to_string();
    // 幂等键：resolution_id（冻结）> idempotency_key（别名）> 按 item + 分派参数派生（确定性）。
    let payload_key = match item.kind.as_str() {
        KIND_ARTIFACT_REVIEW => format!(
            "{}|{}|{}",
            body.decision.clone().unwrap_or_default(),
            body.reviewer
                .clone()
                .or(body.user.clone())
                .unwrap_or_default(),
            body.comment.clone().unwrap_or_default()
        ),
        KIND_HUMAN_RESULT => body.result.clone().unwrap_or_default(),
        KIND_STEP_RETRY => body.note.clone().unwrap_or_default(),
        KIND_CHANGE_SET => body.action.clone().unwrap_or_default(),
        _ => String::new(),
    };
    let idempotency_key = body
        .explicit_idempotency_key()
        .unwrap_or_else(|| format!("inbox-resolve:{item_id}:{payload_key}"));

    // CAS 占位（open/claimed → resolving）；重放/冲突在此短路。
    let prev_status = match store.begin_resolve(&item_id, &idempotency_key) {
        Ok(prev) => prev,
        Err(ResolveLeaseError::AlreadyDone { result }) => {
            return Ok(Json(json!({
                "replayed": true,
                "item_id": item_id,
                "result": result,
            })));
        }
        Err(ResolveLeaseError::KeyConflict { existing }) => {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "该待办已按其他幂等键解决（{existing:?}）；同一待办不允许两种解决"
                    ),
                    "existing_idempotency_key": existing,
                })),
            ));
        }
        Err(ResolveLeaseError::InProgress) => {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": "待办正在处理中，请稍后重试" })),
            ));
        }
        Err(ResolveLeaseError::NotFound) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("待办不存在：{item_id}") })),
            ));
        }
    };

    let dispatched = dispatch_item(&state, &item, &body, &user).await;
    match dispatched {
        Ok(result) => {
            store.finish_resolve(&item_id, result.clone());
            Ok(Json(json!({
                "resolved": true,
                "replayed": false,
                "item_id": item_id,
                "kind": item.kind,
                "result": result,
            })))
        }
        Err(e) => {
            let msg = e.message();
            store.abort_resolve(&item_id, &prev_status, &msg);
            Err(dispatch_error_response(e))
        }
    }
}

/// 按类型分派到既有领域能力（不绕过原权限与幂等检查）。
async fn dispatch_item(
    state: &Arc<AppState>,
    item: &HumanWorkItem,
    body: &ResolveRequest,
    user: &str,
) -> std::result::Result<Value, DispatchError> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(DispatchError::WorkSwarm)?;
    match item.kind.as_str() {
        KIND_HUMAN_RESULT => {
            let result = body.result.clone().unwrap_or_default().trim().to_string();
            if result.is_empty() {
                return Err(DispatchError::BadRequest(
                    "人节点结果不能为空（resolve.result）".to_string(),
                ));
            }
            let artifact = coordinator
                .record_human_result(&item.team_id, &item.target_id, &result)
                .await
                .map_err(DispatchError::WorkSwarm)?;
            Ok(json!({
                "action": "human_result",
                "team_id": item.team_id,
                "step_id": item.target_id,
                "artifact": artifact,
            }))
        }
        KIND_ARTIFACT_REVIEW => {
            let decision = match body.decision.as_deref() {
                Some("approve") => ArtifactReviewDecision::Approve,
                Some("request_changes") => ArtifactReviewDecision::RequestChanges,
                Some("reject") => ArtifactReviewDecision::Reject,
                Some(other) => {
                    return Err(DispatchError::BadRequest(format!(
                        "未知 decision：{other}（支持 approve / request_changes / reject）"
                    )))
                }
                None => {
                    return Err(DispatchError::BadRequest(
                        "评审分派需要 decision（approve / request_changes / reject）".to_string(),
                    ))
                }
            };
            let reviewer = body
                .reviewer
                .clone()
                .filter(|r| !r.trim().is_empty())
                .or_else(|| {
                    if user.is_empty() {
                        None
                    } else {
                        Some(user.to_string())
                    }
                })
                .ok_or_else(|| {
                    DispatchError::BadRequest("评审分派需要 reviewer（或 user）".to_string())
                })?;
            let team = coordinator
                .get_team_run(&item.team_id)
                .await
                .map_err(DispatchError::WorkSwarm)?;
            // 八期（二路交接）：approve 前检查 ChangeSet 批准门控——存在
            // pending_review/conflicted 的 ChangeSet 时，代码 Artifact 可评审但
            // 不能成为最终 approved head（拒绝 approve，带阻断原因）。
            if decision == ArtifactReviewDecision::Approve {
                let cs_store =
                    owo_agent_core::change_set_store::ChangeSetStore::new(coordinator.run_dir());
                if let Some(reason) =
                    cs_store
                        .approval_block_for_team(&item.team_id)
                        .map_err(|e| {
                            DispatchError::BadRequest(format!("ChangeSet 门控检查失败：{e}"))
                        })?
                {
                    return Err(DispatchError::BadRequest(reason));
                }
            }
            let store = state.artifact_review.store().map_err(|(_, json)| {
                DispatchError::BadRequest(
                    json.get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("评审存储不可用")
                        .to_string(),
                )
            })?;
            let input = ArtifactReviewInput {
                artifact_id: item.target_id.clone(),
                team_id: item.team_id.clone(),
                decision,
                reviewer,
                comment: body.comment.clone().unwrap_or_default(),
                expected_version: body.expected_version,
                idempotency_key: body
                    .explicit_idempotency_key()
                    .unwrap_or_else(|| format!("inbox-resolve:{}", item.item_id)),
                self_approve_authorized: self_approve_allowed(team.human_policy.as_deref()),
            };
            let outcome = apply_artifact_review(store.as_ref(), &input)
                .await
                .map_err(DispatchError::Review)?;
            Ok(json!({
                "action": "artifact_review",
                "replayed": outcome.replayed,
                "review": outcome.review,
                "artifact": outcome.artifact,
                "approved_head": outcome.approved_head,
            }))
        }
        KIND_STEP_RETRY => {
            let note = body
                .note
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("重试此节点：{}", item.target_id));
            let team = coordinator
                .apply_steer(
                    &item.team_id,
                    &SteerCommand::Retry {
                        step_id: item.target_id.clone(),
                        note,
                    },
                )
                .await
                .map_err(DispatchError::WorkSwarm)?;
            // retry 恢复同样重启运行循环（与 steer_team 同款）。
            if !coordinator.is_run_active(&item.team_id) {
                let state2 = Arc::clone(state);
                let coordinator2 = Arc::clone(&coordinator);
                let team_id2 = item.team_id.clone();
                tokio::spawn(crate::workswarm_api::run_team_loop(
                    state2,
                    coordinator2,
                    team_id2,
                ));
            }
            Ok(json!({
                "action": "step_retry",
                "team_id": item.team_id,
                "step_id": item.target_id,
                "team_status": format!("{:?}", team.status),
            }))
        }
        KIND_CHANGE_SET => {
            // 八期（二路）：转发既有 ChangeSet accept/reject 端点语义
            //（幂等重放、冲突检测、文件恢复、审计全部由二路编排兜底，不绕过）。
            let action = match body.action.as_deref() {
                Some("accept") => "accept",
                Some("reject") => "reject",
                Some(other) => {
                    return Err(DispatchError::BadRequest(format!(
                        "未知 action：{other}（支持 accept / reject）"
                    )))
                }
                None => {
                    return Err(DispatchError::BadRequest(
                        "ChangeSet 分派需要 action（accept / reject）".to_string(),
                    ))
                }
            };
            let decision_payload = json!({
                "idempotency_key": body
                    .explicit_idempotency_key()
                    .unwrap_or_else(|| format!("inbox-resolve:{}", item.item_id)),
                "note": body.note.clone(),
            });
            let request: crate::workswarm_api::change_set_api::ChangeSetDecisionRequest =
                serde_json::from_value(decision_payload).map_err(|e| {
                    DispatchError::BadRequest(format!("ChangeSet 决定请求无效：{e}"))
                })?;
            let outcome = if action == "accept" {
                crate::workswarm_api::change_set_api::accept_change_set(
                    State(Arc::clone(state)),
                    AxumPath(item.target_id.clone()),
                    Json(request),
                )
                .await
            } else {
                crate::workswarm_api::change_set_api::reject_change_set(
                    State(Arc::clone(state)),
                    AxumPath(item.target_id.clone()),
                    Json(request),
                )
                .await
            };
            match outcome {
                Ok(body) => Ok(json!({
                    "action": format!("change_set_{}", action),
                    "change_set_id": item.target_id,
                    "outcome": body.0,
                })),
                Err((code, err_json)) => Err(DispatchError::Upstream(code, err_json.0)),
            }
        }
        other => Err(DispatchError::BadRequest(format!("未知待办类型：{other}"))),
    }
}
