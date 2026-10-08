use super::*;

/// Execution identity is host-owned. The persisted high-water mark survives
/// cancellation and resetting every step, unlike deriving it from live records.
impl TeamCoordinator {
    pub(crate) fn durable_execution_epoch(state: &GoalRunState) -> u64 {
        state
            .records
            .values()
            .filter_map(|record| record.phase_epoch)
            .fold(state.execution_epoch, u64::max)
    }

    pub(crate) fn phase_epoch(&self, team_id: &str) -> u64 {
        self.phase_epochs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(team_id)
            .copied()
            .unwrap_or(0)
    }

    /// Current host-issued execution generation for correlating runtime diagnostics.
    /// This value is observational; callers cannot change the fencing generation.
    pub fn current_execution_epoch(&self, team_id: &str) -> u64 {
        self.phase_epoch(team_id)
    }

    /// A fresh coordinator must fence the previous process before it dispatches.
    /// Repeated claims in one uninterrupted generation retain the same epoch.
    pub(crate) fn claim_execution_epoch(
        &self,
        team_id: &str,
        state: &GoalRunState,
    ) -> WorkSwarmResult<u64> {
        let durable = Self::durable_execution_epoch(state);
        let mut epochs = self
            .phase_epochs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let epoch = match epochs.get(team_id).copied() {
            Some(current) if current > 0 && current >= durable => current,
            _ => next_execution_epoch(durable)?,
        };
        epochs.insert(team_id.to_string(), epoch);
        Ok(epoch)
    }

    /// Invalidate in-flight results immediately. The caller holds the per-team
    /// writer lock (except Cancel, which first cancels execution), then persists
    /// the new high-water mark with the state transition.
    pub(crate) fn bump_phase_epoch(&self, team_id: &str) -> WorkSwarmResult<u64> {
        // Cancel must fence promptly even while another thread writes a checkpoint.
        // A cached generation was already reconciled with disk before dispatch.
        let cached = self
            .phase_epochs
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(team_id)
            .copied();
        let durable = match cached {
            Some(epoch) => epoch,
            None => Self::durable_execution_epoch(&self.load_goal_state(team_id)?),
        };
        let mut epochs = self
            .phase_epochs
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let floor = epochs.get(team_id).copied().unwrap_or(0).max(durable);
        let epoch = next_execution_epoch(floor)?;
        epochs.insert(team_id.to_string(), epoch);
        Ok(epoch)
    }
}

fn next_execution_epoch(current: u64) -> WorkSwarmResult<u64> {
    current.checked_add(1).ok_or_else(|| {
        WorkSwarmError::Validation(
            "execution epoch exhausted; refusing to reuse a fencing generation".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn coordinator(dir: &Path) -> TeamCoordinator {
        let store =
            crate::project_space_store::SqliteProjectSpaceStore::open(&dir.join("space.db"))
                .unwrap();
        TeamCoordinator::new(
            Arc::new(store),
            Arc::new(TeamTemplateRegistry::new(dir.join("templates"))),
            CasStore::new(dir.join("cas")).unwrap(),
            dir.join("runs"),
        )
    }
    fn state() -> GoalRunState {
        let mut plan = Plan::new("plan", "goal");
        plan.add_step(StepSpec::new("step", "worker"));
        GoalRunState::new(Goal::new("goal", "identity test"), plan)
    }
    #[test]
    fn first_claim_is_nonzero_and_uninterrupted_claims_are_stable() {
        let dir = tempfile::tempdir().unwrap();
        let host = coordinator(dir.path());
        let state = state();
        assert_eq!(
            host.claim_execution_epoch(&state.run_id, &state).unwrap(),
            1
        );
        assert_eq!(
            host.claim_execution_epoch(&state.run_id, &state).unwrap(),
            1
        );
        assert_eq!(host.phase_epoch(&state.run_id), 1);
        assert_eq!(host.current_execution_epoch(&state.run_id), 1);
    }
    #[test]
    fn restart_claim_exceeds_persisted_records_and_new_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let host = coordinator(dir.path());
        let mut state = state();
        state.records.get_mut("step").unwrap().phase_epoch = Some(7);
        assert_eq!(
            host.claim_execution_epoch(&state.run_id, &state).unwrap(),
            8
        );
        host.persist_state(&state).unwrap();
        let persisted = host.load_run_state(&state.run_id).unwrap();
        assert_eq!(persisted.execution_epoch, 8);
        let restarted = coordinator(dir.path());
        assert_eq!(
            restarted
                .claim_execution_epoch(&state.run_id, &persisted)
                .unwrap(),
            9
        );
    }
    #[test]
    fn reset_all_steps_does_not_erase_cancel_generation() {
        let dir = tempfile::tempdir().unwrap();
        let host = coordinator(dir.path());
        let mut state = state();
        host.claim_execution_epoch(&state.run_id, &state).unwrap();
        host.persist_state(&state).unwrap();
        assert_eq!(host.bump_phase_epoch(&state.run_id).unwrap(), 2);
        state.records.get_mut("step").unwrap().phase_epoch = None;
        host.persist_state(&state).unwrap();
        let checkpoint = host.load_run_state(&state.run_id).unwrap();
        assert_eq!(checkpoint.execution_epoch, 2);
        let restarted = coordinator(dir.path());
        assert_eq!(
            restarted
                .claim_execution_epoch(&state.run_id, &checkpoint)
                .unwrap(),
            3
        );
    }
    #[test]
    fn exhausted_generation_fails_closed_instead_of_wrapping() {
        assert!(next_execution_epoch(u64::MAX).is_err());
        let dir = tempfile::tempdir().unwrap();
        let host = coordinator(dir.path());
        let mut state = state();
        state.execution_epoch = u64::MAX;
        assert!(host.claim_execution_epoch(&state.run_id, &state).is_err());
        assert_eq!(host.phase_epoch(&state.run_id), 0);
    }
    #[test]
    fn legacy_snapshot_without_checkpoint_recovers_record_high_water_mark() {
        let mut state = state();
        state.records.get_mut("step").unwrap().phase_epoch = Some(11);
        let mut json = serde_json::to_value(&state).unwrap();
        json.as_object_mut().unwrap().remove("execution_epoch");
        let loaded: GoalRunState = serde_json::from_value(json).unwrap();
        assert_eq!(loaded.execution_epoch, 0);
        assert_eq!(TeamCoordinator::durable_execution_epoch(&loaded), 11);
    }
    #[tokio::test]
    async fn progress_watch_wakes_subscribers_and_starts_at_latest_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let host = coordinator(dir.path());
        let mut receiver = host.subscribe_progress("team-1");

        assert_eq!(host.advance_progress("team-1"), 1);
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), 1);

        let late_subscriber = host.subscribe_progress("team-1");
        assert_eq!(*late_subscriber.borrow(), 1);
    }
}
