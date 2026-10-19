//! Bounded O(1) handoff for per-attempt context assembly timings.
//!
//! Audit remains the human-readable source; this short-lived index avoids scanning and
//! parsing the shared audit history at every Worker completion. Durable metrics are
//! still written to the existing Worker span journal by the server wrapper.
use super::*;
use std::collections::VecDeque;

const MAX_PENDING_CONTEXT_TIMINGS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContextAssemblyIdentity {
    team_id: String,
    role: String,
    step_id: String,
    task_id: Option<String>,
    attempt_id: Option<String>,
    phase_epoch: Option<u64>,
}

impl ContextAssemblyIdentity {
    fn new(
        team_id: &str,
        role: &str,
        step_id: &str,
        task_id: Option<&str>,
        attempt_id: Option<&str>,
        phase_epoch: Option<u64>,
    ) -> Self {
        Self {
            team_id: team_id.to_string(),
            role: role.to_string(),
            step_id: step_id.to_string(),
            task_id: task_id.map(str::to_string),
            attempt_id: attempt_id.map(str::to_string),
            phase_epoch,
        }
    }
}

#[derive(Default)]
pub(super) struct ContextAssemblyTimingStore {
    sequence: u64,
    pending: HashMap<ContextAssemblyIdentity, (u64, u64)>,
    insertion_order: VecDeque<(ContextAssemblyIdentity, u64)>,
}

impl ContextAssemblyTimingStore {
    fn insert(&mut self, identity: ContextAssemblyIdentity, duration_ms: u64) {
        self.sequence = self.sequence.wrapping_add(1);
        if self.sequence == 0 {
            self.pending.clear();
            self.insertion_order.clear();
            self.sequence = 1;
        }
        let sequence = self.sequence;
        self.pending
            .insert(identity.clone(), (sequence, duration_ms));
        self.insertion_order.push_back((identity, sequence));
        while self.insertion_order.len() > MAX_PENDING_CONTEXT_TIMINGS {
            let Some((expired, expired_sequence)) = self.insertion_order.pop_front() else {
                break;
            };
            if self.pending.get(&expired).map(|value| value.0) == Some(expired_sequence) {
                self.pending.remove(&expired);
            }
        }
    }

    fn take(&mut self, identity: &ContextAssemblyIdentity) -> Option<u64> {
        self.pending
            .remove(identity)
            .map(|(_, duration_ms)| duration_ms)
    }
}

#[allow(clippy::too_many_arguments)] // 事件字段就是协议字段，一一对应避免中间层
pub(super) fn context_assembly_event_detail(
    role: &str,
    step_id: &str,
    task_id: Option<&str>,
    attempt_id: Option<&str>,
    phase_epoch: Option<u64>,
    duration_ms: u64,
    outcome: &str,
    snapshot_cache_hit: bool,
) -> String {
    json!({
        "role": role,
        "step_id": step_id,
        "task_id": task_id,
        "attempt_id": attempt_id,
        "phase_epoch": phase_epoch,
        "duration_ms": duration_ms,
        "outcome": outcome,
        "phase_snapshot_cache_hit": snapshot_cache_hit,
    })
    .to_string()
}

impl TeamCoordinator {
    /// Record a content-free context timing and index it for O(1) Worker attribution.
    #[allow(clippy::too_many_arguments)] // 与 context_assembly_event_detail 字段一一对应
    pub fn record_context_assembly_timing(
        &self,
        team_id: &str,
        role: &str,
        step_id: &str,
        task_id: Option<&str>,
        attempt_id: Option<&str>,
        phase_epoch: Option<u64>,
        duration_ms: u64,
        outcome: &str,
        snapshot_cache_hit: bool,
    ) {
        let identity =
            ContextAssemblyIdentity::new(team_id, role, step_id, task_id, attempt_id, phase_epoch);
        self.context_assembly_timings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(identity, duration_ms);
        self.record_runtime_event(
            team_id,
            "team.context.assembled",
            context_assembly_event_detail(
                role,
                step_id,
                task_id,
                attempt_id,
                phase_epoch,
                duration_ms,
                outcome,
                snapshot_cache_hit,
            ),
        );
    }

    /// Consume the indexed timing exactly once after the matching Worker attempt ends.
    pub fn take_context_assembly_timing(
        &self,
        team_id: &str,
        role: &str,
        step_id: &str,
        task_id: Option<&str>,
        attempt_id: Option<&str>,
        phase_epoch: Option<u64>,
    ) -> Option<u64> {
        let identity =
            ContextAssemblyIdentity::new(team_id, role, step_id, task_id, attempt_id, phase_epoch);
        self.context_assembly_timings.lock().ok()?.take(&identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(step_id: &str, attempt_id: &str) -> ContextAssemblyIdentity {
        ContextAssemblyIdentity::new(
            "team-a",
            "writer",
            step_id,
            Some("task-a"),
            Some(attempt_id),
            Some(3),
        )
    }

    #[test]
    fn timing_store_matches_full_attempt_identity_and_consumes_once() {
        let mut store = ContextAssemblyTimingStore::default();
        let first = identity("step-a", "attempt-1");
        let other_attempt = identity("step-a", "attempt-2");
        let other_role = ContextAssemblyIdentity::new(
            "team-a",
            "reviewer",
            "step-a",
            Some("task-a"),
            Some("attempt-1"),
            Some(3),
        );
        let other_team = ContextAssemblyIdentity::new(
            "team-b",
            "writer",
            "step-a",
            Some("task-a"),
            Some("attempt-1"),
            Some(3),
        );
        let other_task = ContextAssemblyIdentity::new(
            "team-a",
            "writer",
            "step-a",
            Some("task-b"),
            Some("attempt-1"),
            Some(3),
        );
        store.insert(first.clone(), 21);

        assert_eq!(store.take(&other_attempt), None);
        assert_eq!(store.take(&other_role), None);
        assert_eq!(store.take(&other_team), None);
        assert_eq!(store.take(&other_task), None);
        assert_eq!(store.take(&first), Some(21));
        assert_eq!(store.take(&first), None);
    }

    #[test]
    fn timing_store_evicts_oldest_pending_entry_at_capacity() {
        let mut store = ContextAssemblyTimingStore::default();
        let first = identity("step-0", "attempt-0");
        for index in 0..=MAX_PENDING_CONTEXT_TIMINGS {
            store.insert(
                identity(&format!("step-{index}"), &format!("attempt-{index}")),
                index as u64,
            );
        }

        assert_eq!(store.pending.len(), MAX_PENDING_CONTEXT_TIMINGS);
        assert_eq!(store.take(&first), None);
        assert_eq!(
            store.take(&identity("step-4096", "attempt-4096")),
            Some(4096)
        );
    }

    #[test]
    fn context_event_detail_is_identity_bound_and_contains_no_payload() {
        let detail = context_assembly_event_detail(
            "implementer",
            "s-task-7",
            Some("task-7"),
            Some("attempt-2"),
            Some(9),
            42,
            "succeeded",
            true,
        );
        let value: Value = serde_json::from_str(&detail).unwrap();

        assert_eq!(value["role"], "implementer");
        assert_eq!(value["step_id"], "s-task-7");
        assert_eq!(value["task_id"], "task-7");
        assert_eq!(value["attempt_id"], "attempt-2");
        assert_eq!(value["phase_epoch"], 9);
        assert_eq!(value["duration_ms"], 42);
        assert_eq!(value["outcome"], "succeeded");
        assert_eq!(value["phase_snapshot_cache_hit"], true);
        assert!(value.get("context").is_none());
        assert!(value.get("prompt").is_none());
        assert!(value.get("artifact_content").is_none());
    }
}
