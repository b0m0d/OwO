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
//! - ChangeSet 扫描（九期 · 二路拆分）：直读 `ChangeSetStore::list_all()` 跨团队
//!   全量，**不依赖团队运行状态/数量上限/run state 可读性**——succeeded、cancelled、
//!   状态文件损坏的团队，只要有未处理 ChangeSet（pending_review/conflicted）照样
//!   出现在待办中；
//! - 主键（九期）：`kind:team_id:target_id:occurrence`——team 入键修复跨团队
//!   step_id 碰撞；occurrence 为发生版本（step_retry/human_result 首选 attempts，
//!   resolved 占用后自动分配下一空闲版本；artifact_review/change_set 恒 "1"），
//!   修复重试轮次被旧 resolved 记录永久吞掉。

use axum::extract::{Path as AxumPath, Query as AxumQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::project_space_store::{
    apply_artifact_review, self_approve_allowed, ArtifactReviewError, ArtifactReviewInput,
};
use owo_agent_core::workswarm::SteerCommand;
use owo_agent_core::WorkSwarmError;
use owo_agent_protocol::ArtifactReviewDecision;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use owo_agent_server::AppState;

use super::human_inbox_store::{
    is_valid_kind, ClaimError, HumanWorkItem, ResolveLeaseError, KIND_ARTIFACT_REVIEW,
    KIND_CHANGE_SET, KIND_HUMAN_RESULT, KIND_STEP_RETRY,
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

mod drafts;

use drafts::*;
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

/// GET /human/inbox：四类待办统一列表（单次扫描：登记 + 合成）。
async fn list_inbox(
    State(state): State<Arc<AppState>>,
    AxumQuery(query): AxumQuery<InboxQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let drafts = collect_drafts(&state).await;
    let store = inbox_store(&state);
    for (draft, _) in &drafts {
        if is_valid_kind(&draft.kind) {
            store.ensure_item(draft);
        }
    }
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
        // 九期：以 ensure_item 返回的条目为准——首选发生版本被 resolved 占用时，
        // 实际登记键是分配出的新 occurrence（draft.item_id() 只是首选键）。
        let item = store.ensure_item(&draft);
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
            WorkSwarmError::Conflict(_) | WorkSwarmError::DeliveryPending(_) => (
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
