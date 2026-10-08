//! Team progress persistence and post-commit skip audit effects.
//!
//! Pure state transitions are centralized in `run_state`; this module owns the
//! coordinator lock, durable write and external audit side effects.

use super::run_state::{
    apply_run_execution_event, MergedStepProgress, RunExecutionEffect, RunExecutionEvent,
};
use crate::goal::{GoalRunState, StepProgressUpdate};

/// Compatibility entry for the coordinator transaction; mutation is delegated to the reducer.
pub(super) fn apply_attempt_admission(
    state: &mut GoalRunState,
    phase_epoch: u64,
    step_id: &str,
    attempt_id: &str,
    is_retry: bool,
) -> Result<(), String> {
    match apply_run_execution_event(
        state,
        RunExecutionEvent::AttemptAdmitted {
            phase_epoch,
            step_id,
            attempt_id,
            is_retry,
        },
    )? {
        RunExecutionEffect::AttemptAdmitted => Ok(()),
        _ => unreachable!("attempt admission must produce the matching effect"),
    }
}

/// Compatibility entry for the progress transaction; snapshots do not write run budgets.
pub(super) fn merge_step_progress_updates(
    state: &mut GoalRunState,
    updates: Vec<StepProgressUpdate>,
) -> MergedStepProgress {
    match apply_run_execution_event(state, RunExecutionEvent::ProgressBatch(updates)) {
        Ok(RunExecutionEffect::ProgressMerged(merged)) => merged,
        _ => unreachable!("progress batch must produce the matching effect"),
    }
}

use super::{TeamCoordinator, WorkSwarmResult};
use serde_json::json;

impl TeamCoordinator {
    /// Persist one coalesced notification batch with one Team lock, state load and write.
    /// Model/tool execution remains outside the Team lock.
    pub(super) async fn persist_step_progress_batch(
        &self,
        team_id: &str,
        epoch: u64,
        updates: Vec<StepProgressUpdate>,
        dynamic_skips: &mut Vec<(String, String)>,
    ) -> WorkSwarmResult<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let lock = self.team_lock(team_id);
        let guard = lock.lock().await;
        if self.phase_epoch(team_id) != epoch {
            return Ok(());
        }
        let (_team, _space, mut state) = self.load_bundle(team_id).await?;
        let mut merged = merge_step_progress_updates(&mut state, updates);
        if merged.state_changed {
            self.persist_state(&state)?;
            self.advance_progress(team_id);
        }
        drop(guard);
        dynamic_skips.append(&mut merged.dynamic_skips);

        for (step_id, role, reason) in merged.skip_events {
            self.note_adaptive_event(
                team_id,
                json!({
                    "kind": "role_skipped",
                    "role": role,
                    "step_id": step_id,
                    "reason": reason,
                    "role_skipped": {"role": role, "reason": reason},
                }),
            )
            .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod progress_batch_merge_tests {
    use super::{apply_attempt_admission, merge_step_progress_updates};
    use crate::goal::{Goal, GoalRunState, StepProgressUpdate};
    use crate::plan::{Plan, StepSpec, StepStatus, ValidationReceiptV1, ValidationVerdictV1};
    use std::collections::HashMap;

    fn receipt() -> ValidationReceiptV1 {
        ValidationReceiptV1 {
            receipt_id: "receipt-step-a".to_string(),
            task_id: "step-a".to_string(),
            attempt_id: "attempt-a".to_string(),
            epoch: 1,
            requirement_id: "behavior".to_string(),
            validator_id: "workspace-command-success-v1".to_string(),
            validator_version: "1".to_string(),
            arguments_sha256: "args".to_string(),
            input_sha256: "input".to_string(),
            environment_id: "workspace".to_string(),
            changeset_sha256: None,
            detail: None,
            review_result: None,
            subject_sha256: HashMap::new(),
            verdict: ValidationVerdictV1::Passed,
            evidence_refs: Vec::new(),
            started_at: "start".to_string(),
            completed_at: "finish".to_string(),
        }
    }

    #[test]
    fn attempt_admissions_persist_before_execution_and_reject_stale_identity() {
        let mut plan = Plan::new("plan-1", "goal-1");
        plan.add_step(StepSpec::new("step-a", "worker-a"));
        let mut state = GoalRunState::new(Goal::new("goal-1", "objective"), plan);
        let record = state.records.get_mut("step-a").unwrap();
        record.status = StepStatus::Running;
        record.attempt_id = Some("attempt-current".to_string());
        record.phase_epoch = Some(7);

        apply_attempt_admission(&mut state, 7, "step-a", "attempt-current", false).unwrap();
        apply_attempt_admission(&mut state, 7, "step-a", "attempt-current", true).unwrap();
        assert_eq!(state.steps_taken, 2);
        assert_eq!(state.total_retries, 1);
        assert_eq!(state.records["step-a"].attempts, 2);

        let stale = apply_attempt_admission(&mut state, 7, "step-a", "attempt-old", true);
        assert!(stale.is_err());
        assert_eq!(state.steps_taken, 2);
        assert_eq!(state.total_retries, 1);
        assert_eq!(state.records["step-a"].attempts, 2);
    }

    #[test]
    fn stale_attempt_progress_cannot_replace_new_claim_or_consume_budget() {
        let mut plan = Plan::new("plan-1", "goal-1");
        plan.add_step(StepSpec::new("step-a", "worker-a"));
        let mut state = GoalRunState::new(Goal::new("goal-1", "objective"), plan);
        let current = state.records.get_mut("step-a").unwrap();
        current.status = StepStatus::Running;
        current.attempts = 2;
        current.attempt_id = Some("attempt-current".to_string());
        current.phase_epoch = Some(7);

        let mut stale = state.records["step-a"].clone();
        stale.status = StepStatus::Failed;
        stale.attempts = 99;
        stale.attempt_id = Some("attempt-stale".to_string());
        stale.output = Some("stale output".to_string());
        stale.skip_reason = Some("stale skip".to_string());
        let merged = merge_step_progress_updates(
            &mut state,
            vec![StepProgressUpdate {
                step_id: "step-a".to_string(),
                worker: "worker-a".to_string(),
                record: stale,
                steps_taken: 99,
                total_retries: 99,
                skip_reason: Some("stale skip".to_string()),
            }],
        );

        assert!(!merged.state_changed);
        assert!(merged.skip_events.is_empty());
        assert!(merged.dynamic_skips.is_empty());
        let current = &state.records["step-a"];
        assert_eq!(current.status, StepStatus::Running);
        assert_eq!(current.attempts, 2);
        assert_eq!(current.attempt_id.as_deref(), Some("attempt-current"));
        assert_eq!(current.output, None);
        assert_eq!(state.steps_taken, 0);
        assert_eq!(state.total_retries, 0);
    }

    #[test]
    fn progress_projection_persists_records_without_writing_durable_counters() {
        let mut plan = Plan::new("plan-1", "goal-1");
        plan.add_step(StepSpec::new("step-a", "worker-a"));
        plan.add_step(StepSpec::new("step-b", "worker-b"));
        let mut state = GoalRunState::new(Goal::new("goal-1", "objective"), plan);
        state.steps_taken = 1;
        state.total_retries = 1;

        let mut receipt_only = state.records["step-a"].clone();
        receipt_only.validation_receipts.push(receipt());
        let mut later_counters = state.records["step-b"].clone();
        later_counters.status = StepStatus::Running;
        later_counters.attempt_id = Some("attempt-b".to_string());
        later_counters.phase_epoch = Some(1);

        let merged = merge_step_progress_updates(
            &mut state,
            vec![
                StepProgressUpdate {
                    step_id: "step-a".to_string(),
                    worker: "worker-a".to_string(),
                    record: receipt_only,
                    steps_taken: 500,
                    total_retries: 300,
                    skip_reason: None,
                },
                StepProgressUpdate {
                    step_id: "step-b".to_string(),
                    worker: "worker-b".to_string(),
                    record: later_counters,
                    steps_taken: 200,
                    total_retries: 100,
                    skip_reason: None,
                },
            ],
        );

        assert!(merged.state_changed);
        assert_eq!(state.records["step-a"].validation_receipts.len(), 1);
        assert_eq!(state.steps_taken, 1);
        assert_eq!(state.total_retries, 1);
    }
}
