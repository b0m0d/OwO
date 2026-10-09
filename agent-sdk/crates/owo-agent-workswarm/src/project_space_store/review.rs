//! 评审闭环编排（V1-R2）：乐观并发 + 幂等 + 授权 + approved head（从 project_space_store.rs 拆出）。

use super::*;

// ---------------------------------------------------------------------------
// 评审闭环编排（V1-R2）：乐观并发 + 幂等 + 授权 + approved head
// ---------------------------------------------------------------------------

/// 评审提交（服务端把 HTTP 请求体映射到此；纯存储编排，不含 HTTP 语义）。
#[derive(Debug, Clone)]
pub struct ArtifactReviewInput {
    pub artifact_id: String,
    pub team_id: String,
    pub decision: ArtifactReviewDecision,
    pub reviewer: String,
    pub comment: String,
    /// 乐观并发目标版本；`None` 表示不做版本校验（接受当前版本）。
    pub expected_version: Option<u32>,
    pub idempotency_key: String,
    /// Human 策略是否授权生产者自批（`human_policy == "self_review_allowed"`）。
    pub self_approve_authorized: bool,
}

/// 评审业务错误（服务端映射：NotFound→404、VersionConflict/Superseded/Idempotency→409、
/// Forbidden→403、Validation→400、Store→500）。
#[derive(Debug, thiserror::Error)]
pub enum ArtifactReviewError {
    #[error("产物不存在：{0}")]
    ArtifactNotFound(String),
    #[error("版本冲突：产物当前为 v{current}，提交基于 v{expected}")]
    VersionConflict { current: u32, expected: u32 },
    #[error("无权评审：{0}")]
    Forbidden(String),
    #[error("产物已被新版本取代，不能批准旧版：{0}")]
    Superseded(String),
    #[error("幂等键冲突：该键已用于其他产物（{0}）")]
    IdempotencyConflict(String),
    #[error("评审输入无效：{0}")]
    Validation(String),
    #[error(transparent)]
    Store(#[from] ProjectSpaceStoreError),
}

/// 评审结果：`replayed=true` 表示幂等键命中既有记录（零副作用回放）。
#[derive(Debug, Clone)]
pub struct ArtifactReviewOutcome {
    pub replayed: bool,
    pub review: ArtifactReviewRecord,
    pub artifact: Artifact,
    /// decision=approve 时的新 approved head（其余为 None）。
    pub approved_head: Option<Artifact>,
}

/// Human 策略判定：生产者自批自己的产物需要显式 `self_review_allowed` 授权；
/// 缺省（None / human_approval_required / 其他值）一律禁止自批。
pub fn self_approve_allowed(human_policy: Option<&str>) -> bool {
    human_policy == Some("self_review_allowed")
}

/// 执行一次 Artifact 评审（幂等、乐观并发、授权与 approved head 维护）。
///
/// 语义：
/// 1. 幂等键命中且属于同一产物 → 原样回放既有记录（不追加、不改状态）；
/// 2. 幂等键命中但属于其他产物 → `IdempotencyConflict`；
/// 3. `expected_version` 与当前版本不符 → `VersionConflict`（旧页面提交 409）；
/// 4. 评审者=生产者且 decision=approve 且未获 Human 策略授权 → `Forbidden`；
/// 5. approve 被取代版本（链上已 superseded）→ `Superseded`；
/// 6. 通过后：追加不可变记录、按决定迁移 `review_state`
///    （approve→Approved 并把 (project, kind) head 指向本版；request_changes→Draft；reject→Rejected）。
pub async fn apply_artifact_review(
    store: &dyn ProjectSpaceStoreBackend,
    input: &ArtifactReviewInput,
) -> std::result::Result<ArtifactReviewOutcome, ArtifactReviewError> {
    if input.reviewer.trim().is_empty() {
        return Err(ArtifactReviewError::Validation("reviewer 不能为空".into()));
    }
    if input.idempotency_key.trim().is_empty() {
        return Err(ArtifactReviewError::Validation(
            "idempotency_key 不能为空".into(),
        ));
    }

    // 1. 幂等回放（在任何状态变更之前）。
    if let Some(existing) = store
        .get_artifact_review_by_idempotency_key(&input.idempotency_key)
        .await?
    {
        if existing.artifact_id != input.artifact_id {
            return Err(ArtifactReviewError::IdempotencyConflict(
                existing.artifact_id,
            ));
        }
        let artifact = store.get_artifact(&input.artifact_id).await?;
        let approved_head = if existing.decision == ArtifactReviewDecision::Approve {
            let project_id = store.get_artifact_project(&input.artifact_id).await?;
            store.get_approved_head(&project_id, &artifact.kind).await?
        } else {
            None
        };
        return Ok(ArtifactReviewOutcome {
            replayed: true,
            review: existing,
            artifact,
            approved_head,
        });
    }

    // 2. 目标产物与乐观并发校验。
    let artifact = store
        .get_artifact(&input.artifact_id)
        .await
        .map_err(|e| match e {
            ProjectSpaceStoreError::NotFound(_) => {
                ArtifactReviewError::ArtifactNotFound(input.artifact_id.clone())
            }
            other => ArtifactReviewError::Store(other),
        })?;
    if let Some(expected) = input.expected_version {
        if expected != artifact.version {
            return Err(ArtifactReviewError::VersionConflict {
                current: artifact.version,
                expected,
            });
        }
    }

    // 3. 授权：生产者不得未经 Human 策略授权自批。
    if input.reviewer == artifact.producer
        && input.decision == ArtifactReviewDecision::Approve
        && !input.self_approve_authorized
    {
        return Err(ArtifactReviewError::Forbidden(format!(
            "生产者 {} 不能批准自己的产物（需 Human 策略 self_review_allowed 授权或由他人评审）",
            artifact.producer
        )));
    }

    // 4. 链约束：被新版本取代的旧版不能再被批准为 head。
    if input.decision == ArtifactReviewDecision::Approve
        && artifact.review_state == owo_agent_protocol::ReviewState::Superseded
    {
        return Err(ArtifactReviewError::Superseded(
            artifact.artifact_id.clone(),
        ));
    }

    // 5. 追加不可变记录（含生产步骤关联：WorkSwarm 约定 `m-{role}` → `s-{role}`，
    //    返工据此定位重置目标；评审记录自足，不依赖产物表回查）。
    let record = ArtifactReviewRecord {
        review_id: format!("rev-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        artifact_id: artifact.artifact_id.clone(),
        artifact_version: artifact.version,
        team_id: input.team_id.clone(),
        decision: input.decision,
        reviewer: input.reviewer.trim().to_string(),
        comment: input.comment.clone(),
        idempotency_key: input.idempotency_key.trim().to_string(),
        content_ref: artifact.content_ref.clone(),
        step_id: producer_step_id(&artifact.producer),
        producer_member_id: artifact.producer.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    store.save_artifact_review(&record).await?;

    // 6. 迁移 review_state + approved head。
    let mut updated = artifact.clone();
    updated.review_state = match input.decision {
        ArtifactReviewDecision::Approve => owo_agent_protocol::ReviewState::Approved,
        ArtifactReviewDecision::RequestChanges => owo_agent_protocol::ReviewState::Draft,
        ArtifactReviewDecision::Reject => owo_agent_protocol::ReviewState::Rejected,
    };
    let project_id = store.get_artifact_project(&input.artifact_id).await?;
    store.save_artifact(&updated, &project_id).await?;

    let approved_head = if input.decision == ArtifactReviewDecision::Approve {
        store
            .set_approved_head(
                &project_id,
                &updated.kind,
                &updated.artifact_id,
                &record.created_at,
            )
            .await?;
        // 版本链收口（V1 五期）：同 (project, kind) 的其余活动版本被本版取代
        // （head 只可能有一个；被取代版本保留全部历史记录与评审链）。
        // Rejected 是终态拒绝、Superseded 已被取代——两者保留原状作历史事实。
        let siblings = store.list_artifacts_by_project(&project_id).await?;
        for sibling in siblings {
            if sibling.kind != updated.kind || sibling.artifact_id == updated.artifact_id {
                continue;
            }
            if matches!(
                sibling.review_state,
                owo_agent_protocol::ReviewState::Rejected
                    | owo_agent_protocol::ReviewState::Superseded
            ) {
                continue;
            }
            let mut superseded = sibling;
            superseded.review_state = owo_agent_protocol::ReviewState::Superseded;
            store.save_artifact(&superseded, &project_id).await?;
        }
        store.get_approved_head(&project_id, &updated.kind).await?
    } else {
        None
    };

    Ok(ArtifactReviewOutcome {
        replayed: false,
        review: record,
        artifact: updated,
        approved_head,
    })
}
