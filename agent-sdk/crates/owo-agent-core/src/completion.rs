//! Shared completion-state decision for ordinary Agent turns and Team delivery.
//!
//! Callers collect host-owned evidence; this module only applies the common meaning.

use owo_agent_protocol::CompletionStatusV1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub const SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID: &str = "single-human-acceptance-v1";

pub struct CompletionEvidence {
    pub response_finished: bool,
    pub reached_turn_limit: bool,
    pub has_candidate_changes: bool,
    pub required_validation_count: usize,
    pub passed_required_validation_count: usize,
    pub failed_required_validation_count: usize,
    pub blocking_issue_count: usize,
    pub stale_evidence: bool,
    pub independent_review_required: bool,
    pub independent_review_passed: bool,
}

/// Hash a canonical host snapshot that identifies the candidate version.
/// Callers provide ordered collections or ordered maps; validation evidence stays
/// in receipt IDs instead of being mixed into this identity digest.
pub fn hash_candidate_version<T: serde::Serialize>(
    snapshot: &T,
) -> Result<String, serde_json::Error> {
    serde_json::to_vec(snapshot).map(|bytes| crate::CasStore::hash_of(&bytes))
}

/// Build the durable record after callers have gathered and validated host evidence.
/// Receipt IDs are normalized and sorted so retries do not create order-dependent records.
pub fn build_completion_record(
    task_id: &str,
    attempt_id: &str,
    status: CompletionStatusV1,
    evidence_receipt_ids: impl IntoIterator<Item = String>,
    candidate_version_sha256: Option<String>,
) -> owo_agent_protocol::TaskCompletionRecordV1 {
    let evidence_receipt_ids = evidence_receipt_ids
        .into_iter()
        .filter(|id| !id.trim().is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    owo_agent_protocol::TaskCompletionRecordV1 {
        task_id: task_id.to_string(),
        attempt_id: attempt_id.to_string(),
        status,
        evidence_receipt_ids,
        candidate_version_sha256,
        decided_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Apply the shared completion contract to host-collected evidence.
///
/// Candidate changes cannot become Accepted without at least one required host
/// validation. A failed required check or blocking issue is terminally Blocked;
/// incomplete, stale, or missing evidence remains Unverified.
pub fn decide_completion(evidence: CompletionEvidence) -> CompletionStatusV1 {
    if evidence.blocking_issue_count > 0 || evidence.failed_required_validation_count > 0 {
        return CompletionStatusV1::Blocked;
    }
    if evidence.reached_turn_limit || !evidence.response_finished || evidence.stale_evidence {
        return CompletionStatusV1::Unverified;
    }
    if !evidence.has_candidate_changes {
        return CompletionStatusV1::ResponseComplete;
    }
    if evidence.required_validation_count == 0 {
        return CompletionStatusV1::Candidate;
    }
    if evidence.passed_required_validation_count < evidence.required_validation_count
        || (evidence.independent_review_required && !evidence.independent_review_passed)
    {
        return CompletionStatusV1::Unverified;
    }
    CompletionStatusV1::Accepted
}

/// Fold a required, host-issued independent-review receipt into an already
/// accepted candidate. Review evidence cannot promote an unaccepted candidate.
pub fn apply_required_review(
    validation_status: CompletionStatusV1,
    review_verdict: crate::plan::ValidationVerdictV1,
) -> CompletionStatusV1 {
    if validation_status != CompletionStatusV1::Accepted {
        return validation_status;
    }

    let passed = usize::from(review_verdict == crate::plan::ValidationVerdictV1::Passed);
    let failed = usize::from(review_verdict == crate::plan::ValidationVerdictV1::Failed);
    decide_completion(CompletionEvidence {
        response_finished: true,
        has_candidate_changes: true,
        required_validation_count: 2,
        passed_required_validation_count: 1 + passed,
        failed_required_validation_count: failed,
        stale_evidence: review_verdict == crate::plan::ValidationVerdictV1::Stale,
        independent_review_required: true,
        independent_review_passed: passed == 1,
        ..CompletionEvidence::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_version_hash_binds_the_candidate_snapshot() {
        let first = std::collections::BTreeMap::from([
            ("src/a.rs", "sha-a"),
            ("src/b.rs", "sha-b"),
        ]);
        let second = std::collections::BTreeMap::from([
            ("src/b.rs", "sha-b"),
            ("src/a.rs", "sha-a"),
        ]);
        let changed = std::collections::BTreeMap::from([
            ("src/a.rs", "sha-changed"),
            ("src/b.rs", "sha-b"),
        ]);
        assert_eq!(hash_candidate_version(&first).unwrap(), hash_candidate_version(&second).unwrap());
        assert_ne!(hash_candidate_version(&first).unwrap(), hash_candidate_version(&changed).unwrap());
    }

    #[test]
    fn durable_completion_record_sorts_and_deduplicates_host_receipts() {
        let record = build_completion_record(
            "task-1",
            "attempt-7",
            CompletionStatusV1::Accepted,
            vec![
                "receipt-b".to_string(),
                "".to_string(),
                "receipt-a".to_string(),
                "receipt-b".to_string(),
            ],
            Some("candidate-sha256".to_string()),
        );
        assert_eq!(record.task_id, "task-1");
        assert_eq!(record.attempt_id, "attempt-7");
        assert_eq!(record.status, CompletionStatusV1::Accepted);
        assert_eq!(record.evidence_receipt_ids, vec!["receipt-a".to_string(), "receipt-b".to_string()]);
        assert_eq!(record.candidate_version_sha256.as_deref(), Some("candidate-sha256"));
        assert!(!record.decided_at.is_empty());
    }

    #[test]
    fn no_candidate_is_only_a_completed_response() {
        assert_eq!(
            decide_completion(CompletionEvidence {
                response_finished: true,
                ..CompletionEvidence::default()
            }),
            CompletionStatusV1::ResponseComplete
        );
    }

    #[test]
    fn candidate_without_required_validation_is_not_accepted() {
        assert_eq!(
            decide_completion(CompletionEvidence {
                response_finished: true,
                has_candidate_changes: true,
                ..CompletionEvidence::default()
            }),
            CompletionStatusV1::Candidate
        );
    }

    #[test]
    fn acceptance_requires_all_checks_and_independent_review() {
        let base = CompletionEvidence {
            response_finished: true,
            has_candidate_changes: true,
            required_validation_count: 2,
            passed_required_validation_count: 2,
            independent_review_required: true,
            independent_review_passed: true,
            ..CompletionEvidence::default()
        };
        assert_eq!(decide_completion(base), CompletionStatusV1::Accepted);
        assert_eq!(
            decide_completion(CompletionEvidence {
                independent_review_passed: false,
                ..base
            }),
            CompletionStatusV1::Unverified
        );
        assert_eq!(
            decide_completion(CompletionEvidence {
                passed_required_validation_count: 1,
                ..base
            }),
            CompletionStatusV1::Unverified
        );
    }

    #[test]
    fn required_review_receipt_is_part_of_shared_completion() {
        use crate::plan::ValidationVerdictV1;

        assert_eq!(
            apply_required_review(CompletionStatusV1::Accepted, ValidationVerdictV1::Passed),
            CompletionStatusV1::Accepted
        );
        assert_eq!(
            apply_required_review(CompletionStatusV1::Accepted, ValidationVerdictV1::Failed),
            CompletionStatusV1::Blocked
        );
        assert_eq!(
            apply_required_review(CompletionStatusV1::Accepted, ValidationVerdictV1::Stale),
            CompletionStatusV1::Unverified
        );
        assert_eq!(
            apply_required_review(CompletionStatusV1::Accepted, ValidationVerdictV1::Unverified),
            CompletionStatusV1::Unverified
        );
        assert_eq!(
            apply_required_review(CompletionStatusV1::Candidate, ValidationVerdictV1::Passed),
            CompletionStatusV1::Candidate
        );
    }

    #[test]
    fn failures_staleness_and_limits_cannot_be_accepted() {
        let base = CompletionEvidence {
            response_finished: true,
            has_candidate_changes: true,
            required_validation_count: 1,
            passed_required_validation_count: 1,
            ..CompletionEvidence::default()
        };
        assert_eq!(
            decide_completion(CompletionEvidence {
                failed_required_validation_count: 1,
                ..base
            }),
            CompletionStatusV1::Blocked
        );
        assert_eq!(
            decide_completion(CompletionEvidence {
                stale_evidence: true,
                ..base
            }),
            CompletionStatusV1::Unverified
        );
        assert_eq!(
            decide_completion(CompletionEvidence {
                reached_turn_limit: true,
                ..base
            }),
            CompletionStatusV1::Unverified
        );
        assert_eq!(
            decide_completion(CompletionEvidence {
                blocking_issue_count: 1,
                ..base
            }),
            CompletionStatusV1::Blocked
        );
    }
}
