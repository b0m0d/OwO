//! Shared completion-state decision for ordinary Agent turns and Team delivery.
//!
//! Callers collect host-owned evidence; this module only applies the common meaning.

use owo_agent_protocol::CompletionStatusV1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
