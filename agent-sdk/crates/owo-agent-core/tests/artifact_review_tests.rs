//! Artifact 评审闭环核心测试（V1-R2 第三路）。
//!
//! 覆盖：approve/request_changes/reject 全链路、幂等键零副作用回放、
//! 幂等键跨产物冲突、expected_version 乐观并发（旧版本拒绝）、
//! 生产者自批授权门（Forbidden / self_review_allowed 放行）、
//! 被取代版本不可再批准、approved head 只指向真实存在且已批准的版本、
//! 评审历史升序、空历史/空 head 边界、supersedes_artifact_id serde 往返。

use owo_agent_core::project_space_store::{
    apply_artifact_review, self_approve_allowed, ArtifactReviewError, ArtifactReviewInput,
    ProjectSpaceStoreBackend, SqliteProjectSpaceStore,
};
use owo_agent_protocol::{Artifact, ArtifactReviewDecision};
use std::sync::Arc;

fn store() -> Arc<SqliteProjectSpaceStore> {
    // 进程唯一目录（并发测试互不串扰）；与既有 core 测试一致用系统临时目录。
    let dir = std::env::temp_dir().join(format!(
        "owo-artifact-review-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Arc::new(SqliteProjectSpaceStore::open(&dir.join("space.db")).unwrap())
}

fn artifact(
    id: &str,
    kind: &str,
    version: u32,
    producer: &str,
    state: ReviewStateForTest,
) -> Artifact {
    Artifact {
        artifact_id: id.to_string(),
        kind: kind.to_string(),
        version,
        producer: producer.to_string(),
        content_ref: format!("cas://sha256:{id}"),
        schema_ref: None,
        source_refs: vec![],
        classification: owo_agent_protocol::ArtifactClassification::Private,
        review_state: match state {
            ReviewStateForTest::Draft => owo_agent_protocol::ReviewState::Draft,
            ReviewStateForTest::PendingReview => owo_agent_protocol::ReviewState::PendingReview,
            ReviewStateForTest::Approved => owo_agent_protocol::ReviewState::Approved,
            ReviewStateForTest::Rejected => owo_agent_protocol::ReviewState::Rejected,
            ReviewStateForTest::Superseded => owo_agent_protocol::ReviewState::Superseded,
        },
        supersedes_artifact_id: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        // 七期（第三路）交付扩展字段：评审测试不涉及，取缺省值。
        team_id: "test-team".to_string(),
        format: "text".to_string(),
        media_type: "text/plain".to_string(),
        file_name: String::new(),
        sha256: String::new(),
        size_bytes: 0,
        evidence_refs: vec![],
        open_issues: vec![],
        validation: None,
        handoff: None,
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum ReviewStateForTest {
    Draft,
    PendingReview,
    Approved,
    Rejected,
    Superseded,
}

fn input(
    artifact_id: &str,
    decision: ArtifactReviewDecision,
    reviewer: &str,
) -> ArtifactReviewInput {
    ArtifactReviewInput {
        artifact_id: artifact_id.to_string(),
        team_id: "team-1".to_string(),
        decision,
        reviewer: reviewer.to_string(),
        comment: "评审意见".to_string(),
        expected_version: None,
        idempotency_key: format!("idem-{artifact_id}-{reviewer}-{decision:?}"),
        self_approve_authorized: false,
    }
}

#[tokio::test]
async fn approve_full_chain_sets_state_record_and_head() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a1",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();

    let out = apply_artifact_review(
        s.as_ref(),
        &input("a1", ArtifactReviewDecision::Approve, "critic"),
    )
    .await
    .unwrap();
    assert!(!out.replayed);
    assert_eq!(
        out.artifact.review_state,
        owo_agent_protocol::ReviewState::Approved
    );
    assert_eq!(out.review.artifact_version, 1);
    // 取证锚点：记录保存评审时的内容引用。
    assert_eq!(out.review.content_ref, "cas://sha256:a1");
    // approved head 指向本版本。
    let head = s.get_approved_head("proj-1", "document").await.unwrap();
    assert_eq!(head.as_ref().map(|a| a.artifact_id.as_str()), Some("a1"));
    // 历史恰一条。
    let history = s.list_artifact_reviews("a1").await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].decision, ArtifactReviewDecision::Approve);
}

#[tokio::test]
async fn request_changes_returns_to_draft_without_head() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a2",
            "document",
            2,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();

    let out = apply_artifact_review(
        s.as_ref(),
        &input("a2", ArtifactReviewDecision::RequestChanges, "critic"),
    )
    .await
    .unwrap();
    assert_eq!(
        out.artifact.review_state,
        owo_agent_protocol::ReviewState::Draft
    );
    assert!(out.approved_head.is_none());
    assert!(s
        .get_approved_head("proj-1", "document")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn reject_marks_rejected() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a3",
            "code",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();

    let out = apply_artifact_review(
        s.as_ref(),
        &input("a3", ArtifactReviewDecision::Reject, "human:u1"),
    )
    .await
    .unwrap();
    assert_eq!(
        out.artifact.review_state,
        owo_agent_protocol::ReviewState::Rejected
    );
    assert!(s
        .get_approved_head("proj-1", "code")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn idempotent_replay_has_zero_side_effects() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a4",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    let mut first = input("a4", ArtifactReviewDecision::Approve, "critic");
    first.idempotency_key = "idem-same".to_string();
    apply_artifact_review(s.as_ref(), &first).await.unwrap();

    // 同幂等键重放：即便请求体声称不同决定/不同期望版本，也只回放既有记录。
    let mut replay = input("a4", ArtifactReviewDecision::Reject, "critic");
    replay.idempotency_key = "idem-same".to_string();
    replay.expected_version = Some(99);
    let out = apply_artifact_review(s.as_ref(), &replay).await.unwrap();
    assert!(out.replayed);
    assert_eq!(out.review.decision, ArtifactReviewDecision::Approve);
    // 副作用为零：仍只有一条记录，产物状态保持首次决定（Approved）。
    assert_eq!(s.list_artifact_reviews("a4").await.unwrap().len(), 1);
    assert_eq!(
        s.get_artifact("a4").await.unwrap().review_state,
        owo_agent_protocol::ReviewState::Approved
    );
}

#[tokio::test]
async fn idempotency_key_reused_on_other_artifact_conflicts() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a5",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    s.save_artifact(
        &artifact(
            "a6",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    let mut first = input("a5", ArtifactReviewDecision::Approve, "critic");
    first.idempotency_key = "idem-shared".to_string();
    apply_artifact_review(s.as_ref(), &first).await.unwrap();

    let mut second = input("a6", ArtifactReviewDecision::Approve, "critic");
    second.idempotency_key = "idem-shared".to_string();
    let err = apply_artifact_review(s.as_ref(), &second)
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactReviewError::IdempotencyConflict(ref id) if id == "a5"));
}

#[tokio::test]
async fn stale_expected_version_is_rejected() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a7",
            "document",
            3,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();

    let mut stale = input("a7", ArtifactReviewDecision::Approve, "critic");
    stale.expected_version = Some(2);
    let err = apply_artifact_review(s.as_ref(), &stale).await.unwrap_err();
    assert!(matches!(
        err,
        ArtifactReviewError::VersionConflict {
            current: 3,
            expected: 2
        }
    ));
    // 冲突的提交不产生记录、不改状态。
    assert!(s.list_artifact_reviews("a7").await.unwrap().is_empty());
    assert_eq!(
        s.get_artifact("a7").await.unwrap().review_state,
        owo_agent_protocol::ReviewState::PendingReview
    );

    // 匹配版本可通过。
    let mut fresh = input("a7", ArtifactReviewDecision::Approve, "critic");
    fresh.expected_version = Some(3);
    apply_artifact_review(s.as_ref(), &fresh).await.unwrap();
}

#[tokio::test]
async fn producer_cannot_self_approve_without_human_policy() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a8",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();

    let mut self_review = input("a8", ArtifactReviewDecision::Approve, "builder");
    self_review.self_approve_authorized = false;
    let err = apply_artifact_review(s.as_ref(), &self_review)
        .await
        .unwrap_err();
    assert!(matches!(err, ArtifactReviewError::Forbidden(_)));
    assert!(s.list_artifact_reviews("a8").await.unwrap().is_empty());

    // 显式 self_review_allowed 授权后放行。
    let mut authorized = input("a8", ArtifactReviewDecision::Approve, "builder");
    authorized.self_approve_authorized = true;
    apply_artifact_review(s.as_ref(), &authorized)
        .await
        .unwrap();
    assert_eq!(s.list_artifact_reviews("a8").await.unwrap().len(), 1);
}

#[tokio::test]
async fn producer_can_request_changes_or_reject_own_artifact() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a9",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    // 自批被禁，但要求修改/驳回自己的产物不涉及「自我背书」，允许。
    let mut own = input("a9", ArtifactReviewDecision::RequestChanges, "builder");
    own.self_approve_authorized = false;
    apply_artifact_review(s.as_ref(), &own).await.unwrap();
    assert_eq!(
        s.get_artifact("a9").await.unwrap().review_state,
        owo_agent_protocol::ReviewState::Draft
    );
}

#[tokio::test]
async fn approving_superseded_version_is_rejected() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a10",
            "document",
            1,
            "builder",
            ReviewStateForTest::Superseded,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    let err = apply_artifact_review(
        s.as_ref(),
        &input("a10", ArtifactReviewDecision::Approve, "critic"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ArtifactReviewError::Superseded(_)));
}

#[tokio::test]
async fn approved_head_must_point_to_existing_approved_version() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a11",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    apply_artifact_review(
        s.as_ref(),
        &input("a11", ArtifactReviewDecision::Approve, "critic"),
    )
    .await
    .unwrap();
    assert!(s
        .get_approved_head("proj-1", "document")
        .await
        .unwrap()
        .is_some());

    // head 指向的产物被后续流程改回 Draft（如重新返工）→ head 自愈为 None，不悬挂。
    let mut mutated = s.get_artifact("a11").await.unwrap();
    mutated.review_state = owo_agent_protocol::ReviewState::Draft;
    s.save_artifact(&mutated, "proj-1").await.unwrap();
    assert!(s
        .get_approved_head("proj-1", "document")
        .await
        .unwrap()
        .is_none());

    // head 指向的产物被删除 → 同样自愈为 None。
    s.set_approved_head("proj-1", "code", "missing-artifact", "2026-01-01T00:00:00Z")
        .await
        .unwrap();
    assert!(s
        .get_approved_head("proj-1", "code")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn head_moves_when_new_version_approved() {
    let s = store();
    s.save_artifact(
        &artifact(
            "t1:builder:v1",
            "document",
            1,
            "builder",
            ReviewStateForTest::Approved,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    s.save_artifact(
        &artifact(
            "t1:builder:v2",
            "document",
            2,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    // 链：v2 取代 v1。
    let mut v2 = s.get_artifact("t1:builder:v2").await.unwrap();
    v2.supersedes_artifact_id = Some("t1:builder:v1".to_string());
    s.save_artifact(&v2, "proj-1").await.unwrap();

    apply_artifact_review(
        s.as_ref(),
        &input("t1:builder:v2", ArtifactReviewDecision::Approve, "critic"),
    )
    .await
    .unwrap();
    let head = s
        .get_approved_head("proj-1", "document")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.artifact_id, "t1:builder:v2");
    assert_eq!(
        head.supersedes_artifact_id.as_deref(),
        Some("t1:builder:v1")
    );
}

#[tokio::test]
async fn empty_history_and_missing_artifact_boundaries() {
    let s = store();
    // 空历史：返回空列表而非错误。
    assert!(s.list_artifact_reviews("nope").await.unwrap().is_empty());
    // 无 head：None。
    assert!(s
        .get_approved_head("proj-x", "document")
        .await
        .unwrap()
        .is_none());
    // 评审不存在的产物：ArtifactNotFound。
    let err = apply_artifact_review(
        s.as_ref(),
        &input("nope", ArtifactReviewDecision::Approve, "critic"),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ArtifactReviewError::ArtifactNotFound(_)));
    // 空 reviewer / 空幂等键：Validation。
    let mut bad = input("x", ArtifactReviewDecision::Approve, "  ");
    bad.artifact_id = "y".to_string();
    let err = apply_artifact_review(s.as_ref(), &bad).await.unwrap_err();
    assert!(matches!(err, ArtifactReviewError::Validation(_)));
}

#[tokio::test]
async fn history_is_ordered_by_created_at() {
    let s = store();
    s.save_artifact(
        &artifact(
            "a12",
            "document",
            1,
            "builder",
            ReviewStateForTest::PendingReview,
        ),
        "proj-1",
    )
    .await
    .unwrap();
    for (i, decision) in [
        ArtifactReviewDecision::RequestChanges,
        ArtifactReviewDecision::Approve,
    ]
    .into_iter()
    .enumerate()
    {
        let mut inp = input("a12", decision, "critic");
        inp.idempotency_key = format!("idem-hist-{i}");
        apply_artifact_review(s.as_ref(), &inp).await.unwrap();
    }
    let history = s.list_artifact_reviews("a12").await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].decision, ArtifactReviewDecision::RequestChanges);
    assert_eq!(history[1].decision, ArtifactReviewDecision::Approve);
    assert!(history[0].created_at <= history[1].created_at);
}

#[test]
fn supersedes_field_serde_roundtrip_and_default() {
    let mut a = artifact(
        "s1",
        "document",
        2,
        "builder",
        ReviewStateForTest::PendingReview,
    );
    a.supersedes_artifact_id = Some("s0".to_string());
    let json = serde_json::to_string(&a).unwrap();
    assert!(json.contains("supersedes_artifact_id"));
    let back: Artifact = serde_json::from_str(&json).unwrap();
    assert_eq!(back.supersedes_artifact_id.as_deref(), Some("s0"));

    // 旧数据缺字段 → 反序列化为 None（向后兼容）。
    let mut v = serde_json::to_value(artifact(
        "s2",
        "document",
        1,
        "builder",
        ReviewStateForTest::Draft,
    ))
    .unwrap();
    v.as_object_mut().unwrap().remove("supersedes_artifact_id");
    let old: Artifact = serde_json::from_value(v).unwrap();
    assert!(old.supersedes_artifact_id.is_none());
}

#[test]
fn decision_serde_is_snake_case() {
    assert_eq!(
        serde_json::to_string(&ArtifactReviewDecision::Approve).unwrap(),
        "\"approve\""
    );
    assert_eq!(
        serde_json::to_string(&ArtifactReviewDecision::RequestChanges).unwrap(),
        "\"request_changes\""
    );
    assert_eq!(
        serde_json::to_string(&ArtifactReviewDecision::Reject).unwrap(),
        "\"reject\""
    );
}

#[test]
fn self_approve_policy_gate() {
    assert!(!self_approve_allowed(None));
    assert!(!self_approve_allowed(Some("human_approval_required")));
    assert!(!self_approve_allowed(Some("auto_continue")));
    assert!(self_approve_allowed(Some("self_review_allowed")));
}
