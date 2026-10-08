//! Artifact 返工与最终交付物 HTTP 路由（V1 五期 · 第二路）。
//!
//! 路由面（经 [`crate::artifact_review_api::router`] 合并挂载）：
//! - `POST /artifacts/{id}/rework`：按 request_changes 评审发起返工
//!   （重置生产步骤及未成功下游 → 注入返工指令 → 重启运行循环 → 产出新版本）；
//! - `GET  /projects/{id}/deliverables`：最终交付物视图
//!   （已批准 head / 待评审 / 被驳回或被取代 / 返工任务）。
//!
//! 契约要点：
//! - 同一 `review_id` 至多一个返工任务；重复请求幂等返回原任务（`replayed: true`）；
//! - 仅 `request_changes` 评审可返工；被驳回/被取代产物不可返工（409）；
//! - 幂等键已被其他评审占用 → 409；
//! - `idempotency_key` 请求字段可选；缺省时用 `rework:{review_id}` 派生。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::project_space_store::{
    list_artifact_rework_tasks, save_artifact_rework_task, ProjectSpaceStoreBackend,
    ProjectSpaceStoreError,
};
use owo_agent_core::workswarm::TeamCoordinator;
use owo_agent_protocol::{ArtifactReworkStatus, ArtifactReworkTask};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use super::store_error;
use crate::workswarm_api::run_team_loop;
use crate::AppState;

/// WorkSwarmError → HTTP 状态（NotFound 404 / Conflict 409 / Validation 400 / 其余 500）。
fn swarm_error(e: owo_agent_core::workswarm::WorkSwarmError) -> (StatusCode, Json<Value>) {
    use owo_agent_core::workswarm::WorkSwarmError;
    let status = match &e {
        WorkSwarmError::NotFound(_) => StatusCode::NOT_FOUND,
        WorkSwarmError::Conflict(_) | WorkSwarmError::DeliveryPending(_) => StatusCode::CONFLICT,
        WorkSwarmError::Validation(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({ "error": format!("{e}") })))
}

/// POST /artifacts/{id}/rework 请求体。
#[derive(Debug, Deserialize)]
pub(crate) struct ArtifactReworkHttpRequest {
    team_id: String,
    /// 触发返工的 request_changes 评审（幂等键）。
    review_id: String,
    /// 返工指令（注入重跑步骤输入 `rework.instruction`）。
    instruction: String,
    /// 可选幂等键；缺省派生为 `rework:{review_id}`。
    #[serde(default)]
    idempotency_key: Option<String>,
}

/// POST /artifacts/{id}/rework：按评审发起返工。
pub(crate) async fn submit_rework(
    State(state): State<Arc<AppState>>,
    AxumPath(artifact_id): AxumPath<String>,
    Json(req): Json<ArtifactReworkHttpRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let store: Arc<owo_agent_core::project_space_store::SqliteProjectSpaceStore> =
        state.artifact_review.store()?;
    let team_id = req.team_id.trim().to_string();
    let review_id = req.review_id.trim().to_string();
    let instruction = req.instruction.trim().to_string();
    if team_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "team_id 不能为空" })),
        ));
    }
    if review_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "review_id 不能为空" })),
        ));
    }
    if instruction.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "instruction 不能为空" })),
        ));
    }

    // 目标产物与触发评审校验。
    let artifact = store
        .get_artifact(&artifact_id)
        .await
        .map_err(|e| match e {
            ProjectSpaceStoreError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("产物不存在：{artifact_id}") })),
            ),
            other => store_error(other),
        })?;
    let reviews = store
        .list_artifact_reviews(&artifact_id)
        .await
        .map_err(store_error)?;
    let Some(review) = reviews.iter().find(|r| r.review_id == review_id) else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!(
                "评审 {review_id} 不存在或不属于产物 {artifact_id}"
            ) })),
        ));
    };
    if review.team_id != team_id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!(
                "team_id 与评审记录不符（评审属于团队 {}）",
                review.team_id
            ) })),
        ));
    }
    if review.decision != owo_agent_protocol::ArtifactReviewDecision::RequestChanges {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!(
                "仅 request_changes 评审可发起返工（{review_id} 为 {:?}）",
                review.decision
            ) })),
        ));
    }
    let project_id = store
        .get_artifact_project(&artifact_id)
        .await
        .map_err(store_error)?;

    // 幂等：同一评审至多一个返工任务；重复请求返回原任务（零副作用）。
    let existing = list_artifact_rework_tasks(store.as_ref(), &project_id)
        .await
        .map_err(store_error)?;
    let derived_key = req
        .idempotency_key
        .clone()
        .unwrap_or_else(|| format!("rework:{review_id}"));
    let existing_task = existing
        .iter()
        .find(|task| task.review_id == review_id)
        .cloned();
    let replayed = existing_task.is_some();
    if let Some(task) = &existing_task {
        if task.artifact_id != artifact_id
            || task.team_id != team_id
            || task.idempotency_key != derived_key
            || task.instruction != instruction
        {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": "同一评审的返工请求必须保持产物、幂等键和指令一致" })),
            ));
        }
        if task.status == ArtifactReworkStatus::Failed {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({ "error": "返工任务已失败；请基于当前产物提交新的评审后再返工" })),
            ));
        }
    } else if existing.iter().any(|task| {
        task.team_id == team_id
            && task.artifact_id == artifact_id
            && matches!(
                task.status,
                ArtifactReworkStatus::Dispatching | ArtifactReworkStatus::Requested
            )
    }) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "该产物已有进行中的返工任务" })),
        ));
    }
    // 已完成任务的重放仍须幂等成功，即使原产物已被新版本取代。
    // 只有创建新任务时才按当前 Artifact 状态拒绝返工。
    use owo_agent_protocol::ReviewState;
    if existing_task.is_none()
        && matches!(
            artifact.review_state,
            ReviewState::Rejected | ReviewState::Superseded
        )
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!(
                "产物当前状态 {:?} 不可返工（被驳回产物请重新执行任务；被取代产物请评审最新版本）",
                artifact.review_state
            ) })),
        ));
    }

    if existing
        .iter()
        .any(|t| t.idempotency_key == derived_key && t.review_id != review_id)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!(
                "幂等键冲突：该键已用于其他评审的返工（{derived_key}）"
            ) })),
        ));
    }

    // 定位生产步骤（评审记录 → 计划内 producer 匹配；退化用 s-{role} 约定）。
    let coordinator: Arc<TeamCoordinator> = state.workswarm.coordinator().map_err(swarm_error)?;
    let run_state = coordinator.load_run_state(&team_id).map_err(swarm_error)?;
    let step_id = run_state
        .plan
        .steps
        .iter()
        .find(|s| s.worker == artifact.producer)
        .map(|s| s.id.clone())
        .or_else(|| {
            artifact
                .producer
                .strip_prefix("m-")
                .map(|role| format!("s-{role}"))
        })
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                Json(json!({ "error": format!(
                    "无法定位生产步骤：产物生产者 {} 不在团队计划内",
                    artifact.producer
                ) })),
            )
        })?;
    if existing_task
        .as_ref()
        .is_some_and(|task| task.step_id != step_id)
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "返工记录的生产步骤与当前计划不一致"
            })),
        ));
    }

    // 先持久化 dispatching 意图，再执行协调器副作用。若进程在两边之间崩溃，
    // 同一评审的重放会用 source_id 对 coordinator 操作做幂等恢复。
    let mut task = if let Some(task) = existing_task {
        task
    } else {
        let task = ArtifactReworkTask {
            rework_id: format!("rework-{}", uuid::Uuid::new_v4().simple()),
            artifact_id: artifact.artifact_id.clone(),
            artifact_version: artifact.version,
            review_id: review_id.clone(),
            team_id: team_id.clone(),
            project_id: project_id.clone(),
            step_id: step_id.clone(),
            instruction: instruction.clone(),
            idempotency_key: derived_key,
            status: ArtifactReworkStatus::Dispatching,
            reworked_artifact_id: None,
            error: String::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        save_artifact_rework_task(store.as_ref(), &task)
            .await
            .map_err(store_error)?;
        task
    };

    if task.status == ArtifactReworkStatus::Dispatching {
        if let Err(error) = coordinator
            .rework_step_for_review(&team_id, &step_id, &instruction, &review_id)
            .await
        {
            task.error = error.to_string();
            if let Err(save_error) = save_artifact_rework_task(store.as_ref(), &task).await {
                tracing::error!(
                    rework_id = %task.rework_id,
                    error = %save_error,
                    "返工派发错误诊断无法落盘"
                );
            }
            return Err(swarm_error(error));
        }
        task.status = ArtifactReworkStatus::Requested;
        task.error.clear();
        save_artifact_rework_task(store.as_ref(), &task)
            .await
            .map_err(store_error)?;
    }

    // 重启运行循环（幂等：运行中/已有循环则跳过；循环内部按迭代重建注册表）。
    if task.status != ArtifactReworkStatus::Completed
        && !coordinator.is_run_active(&team_id)
        && !coordinator.is_loop_alive(&team_id)
    {
        tokio::spawn(run_team_loop(Arc::clone(&state), coordinator, team_id));
    }

    Ok((
        if replayed {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(json!({ "replayed": replayed, "rework": task })),
    ))
}

/// GET /projects/{id}/deliverables：最终交付物视图（已批准 head / 待评审 / 取代或驳回）。
pub(crate) async fn project_deliverables(
    State(state): State<Arc<AppState>>,
    AxumPath(project_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = state.artifact_review.store()?;
    let space = store
        .get_project_space(&project_id)
        .await
        .map_err(|e| match e {
            ProjectSpaceStoreError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("项目空间不存在：{project_id}") })),
            ),
            other => store_error(other),
        })?;
    let artifacts = store
        .list_artifacts_by_project(&project_id)
        .await
        .map_err(store_error)?;
    let reworks = list_artifact_rework_tasks(store.as_ref(), &project_id)
        .await
        .map_err(store_error)?;

    use owo_agent_protocol::ReviewState;
    let mut kinds: Vec<&str> = artifacts.iter().map(|a| a.kind.as_str()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut approved: Vec<Value> = Vec::new();
    for kind in &kinds {
        if let Some(head) = store
            .get_approved_head(&project_id, kind)
            .await
            .map_err(store_error)?
        {
            approved.push(json!(head));
        }
    }
    let pending_review: Vec<&owo_agent_protocol::Artifact> = artifacts
        .iter()
        .filter(|a| {
            matches!(
                a.review_state,
                ReviewState::Draft | ReviewState::PendingReview
            )
        })
        .collect();
    let rejected_or_superseded: Vec<&owo_agent_protocol::Artifact> = artifacts
        .iter()
        .filter(|a| {
            matches!(
                a.review_state,
                ReviewState::Rejected | ReviewState::Superseded
            )
        })
        .collect();
    let complete = !approved.is_empty() && pending_review.is_empty();

    Ok(Json(json!({
        "project_id": project_id,
        "complete": complete,
        "delivery_manifest_ref": space.delivery_manifest_ref,
        "approved": approved,
        "pending_review": pending_review,
        "rejected_or_superseded": rejected_or_superseded,
        "rework_tasks": reworks,
    })))
}
