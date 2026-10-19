//! Incremental critical-path prioritization for ready Goal DAG steps.
use crate::plan::StepSpec;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};

/// Event-driven ready frontier ordered by longest root-to-sink path membership.
///
/// The immutable graph and scores are built once per dispatch wave. New work is
/// considered only when one of its direct dependencies completes, avoiding repeated
/// full-plan scans while allowing critical continuations to pass unrelated backlog.
pub(super) struct ReadyFrontier {
    positions: HashMap<String, usize>,
    successors: Vec<Vec<usize>>,
    queued: Vec<bool>,
    criticality: Vec<u64>,
    ready: BinaryHeap<(u64, Reverse<usize>, usize)>,
}

impl ReadyFrontier {
    pub(super) fn new(steps: &[StepSpec]) -> Self {
        let positions: HashMap<String, usize> = steps
            .iter()
            .enumerate()
            .map(|(index, step)| (step.id.clone(), index))
            .collect();
        let mut successors = vec![Vec::<usize>::new(); steps.len()];
        let mut indegree = vec![0_usize; steps.len()];
        for (child, step) in steps.iter().enumerate() {
            for dependency in &step.depends_on {
                if let Some(&parent) = positions.get(dependency) {
                    successors[parent].push(child);
                    indegree[child] = indegree[child].saturating_add(1);
                }
            }
        }

        // Plan::validate rejects cycles before run_inner reaches the scheduler.
        let mut roots = VecDeque::new();
        for (index, degree) in indegree.iter().enumerate() {
            if *degree == 0 {
                roots.push_back(index);
            }
        }
        let mut topological = Vec::with_capacity(steps.len());
        while let Some(parent) = roots.pop_front() {
            topological.push(parent);
            for &child in &successors[parent] {
                indegree[child] = indegree[child].saturating_sub(1);
                if indegree[child] == 0 {
                    roots.push_back(child);
                }
            }
        }

        let efforts: Vec<u64> = steps
            .iter()
            .map(|step| {
                step.input
                    .get("estimated_effort")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 1_000_000)
            })
            .collect();
        let mut longest_prefix = efforts.clone();
        for &parent in &topological {
            for &child in &successors[parent] {
                longest_prefix[child] = longest_prefix[child]
                    .max(longest_prefix[parent].saturating_add(efforts[child]));
            }
        }
        let mut longest_suffix = efforts.clone();
        for &parent in topological.iter().rev() {
            for &child in &successors[parent] {
                longest_suffix[parent] = longest_suffix[parent]
                    .max(efforts[parent].saturating_add(longest_suffix[child]));
            }
        }
        let criticality = longest_prefix
            .iter()
            .zip(longest_suffix)
            .zip(efforts)
            .map(|((prefix, suffix), effort)| prefix.saturating_add(suffix).saturating_sub(effort))
            .collect::<Vec<_>>();

        Self {
            positions,
            successors,
            queued: vec![false; steps.len()],
            criticality,
            ready: BinaryHeap::new(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    pub(super) fn pop(&mut self) -> Option<usize> {
        self.ready.pop().map(|(_, _, index)| index)
    }

    pub(super) fn enqueue_ready_steps(
        &mut self,
        steps: &[StepSpec],
        mut is_ready: impl FnMut(&StepSpec) -> bool,
    ) {
        for (index, step) in steps.iter().enumerate() {
            if is_ready(step) {
                self.push(index);
            }
        }
    }

    pub(super) fn enqueue_ready_successors(
        &mut self,
        completed_step_id: &str,
        steps: &[StepSpec],
        mut is_ready: impl FnMut(&StepSpec) -> bool,
    ) {
        let Some(&parent) = self.positions.get(completed_step_id) else {
            return;
        };
        for offset in 0..self.successors[parent].len() {
            let child = self.successors[parent][offset];
            if is_ready(&steps[child]) {
                self.push(child);
            }
        }
    }

    fn push(&mut self, index: usize) {
        if std::mem::replace(&mut self.queued[index], true) {
            return;
        }
        self.ready
            .push((self.criticality[index], Reverse(index), index));
    }
}

#[cfg(test)]
mod tests {
    use super::ReadyFrontier;
    use crate::plan::StepSpec;

    fn step(id: &str, dependencies: &[&str]) -> StepSpec {
        let mut step = StepSpec::new(id, "worker");
        step.depends_on = dependencies
            .iter()
            .map(|value| (*value).to_string())
            .collect();
        step
    }

    fn with_effort(mut step: StepSpec, effort: u64) -> StepSpec {
        step.input = serde_json::json!({"estimated_effort": effort});
        step
    }

    #[test]
    fn newly_ready_critical_chain_outranks_fifo_backlog_and_ties_stay_stable() {
        let steps = vec![
            step("start", &[]),
            step("queued-independent", &[]),
            step("continuation", &["start"]),
            step("tail", &["continuation"]),
            step("same-rank-a", &[]),
            step("same-rank-b", &[]),
        ];
        let mut frontier = ReadyFrontier::new(&steps);
        frontier.enqueue_ready_steps(&steps, |step| {
            matches!(
                step.id.as_str(),
                "start" | "queued-independent" | "same-rank-a" | "same-rank-b"
            )
        });

        assert_eq!(frontier.pop(), Some(0));
        frontier.enqueue_ready_successors("start", &steps, |step| step.id == "continuation");
        assert_eq!(frontier.pop(), Some(2));
        assert_eq!(frontier.pop(), Some(1));
        assert_eq!(frontier.pop(), Some(4));
        assert_eq!(frontier.pop(), Some(5));
        assert!(frontier.is_empty());
    }

    #[test]
    fn validated_effort_weights_critical_path_ahead_of_unit_step_count() {
        let steps = vec![
            with_effort(step("chain-start", &[]), 1),
            with_effort(step("chain-middle", &["chain-start"]), 1),
            with_effort(step("chain-end", &["chain-middle"]), 1),
            with_effort(step("expensive-independent", &[]), 10),
        ];
        let mut frontier = ReadyFrontier::new(&steps);
        frontier.enqueue_ready_steps(&steps, |step| {
            matches!(step.id.as_str(), "chain-start" | "expensive-independent")
        });

        assert_eq!(frontier.pop(), Some(3));
        assert_eq!(frontier.pop(), Some(0));
    }
}
