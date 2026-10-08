//! Per-coordinator-epoch execution summaries for WorkSwarm diagnostics.
use super::metrics::WorkerSpanRecord;
use super::util::round6;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const MAX_RETURNED_EPOCHS: usize = 100;

/// Aggregate Worker spans by the host-issued coordinator generation.
/// Wall-window duration is derived from timestamps; summed Worker wall time is
/// kept separately because parallel spans overlap and must not be added as turn time.
pub(crate) fn aggregate_execution_epochs(records: &[WorkerSpanRecord]) -> Value {
    let mut grouped = BTreeMap::<u64, Vec<&WorkerSpanRecord>>::new();
    for record in records {
        if let Some(epoch) = record.phase_epoch {
            grouped.entry(epoch).or_default().push(record);
        }
    }
    let mut epochs = grouped
        .into_iter()
        .map(|(phase_epoch, spans)| {
            let worker_span_count = spans.len() as u64;
            let failed_spans = spans
                .iter()
                .filter(|span| span.outcome != "succeeded")
                .count() as u64;
            let model_calls = spans
                .iter()
                .fold(0u64, |sum, span| sum.saturating_add(span.model_calls));
            let worker_wall_ms_sum = spans
                .iter()
                .fold(0u64, |sum, span| sum.saturating_add(span.wall_ms));
            let provider_wait_ms_sum = spans
                .iter()
                .fold(0u64, |sum, span| sum.saturating_add(span.provider_wait_ms));
            let budget_reservation_wait_ms_sum = spans.iter().fold(0u64, |sum, span| {
                sum.saturating_add(span.budget_reservation_wait_ms)
            });
            let lease_wait_ms_sum = spans
                .iter()
                .fold(0u64, |sum, span| sum.saturating_add(span.lease_wait_ms));
            let prompt_tokens = sum_optional(spans.iter().filter_map(|span| span.prompt_tokens));
            let completion_tokens =
                sum_optional(spans.iter().filter_map(|span| span.completion_tokens));
            let total_tokens = sum_optional(spans.iter().filter_map(|span| span.total_tokens));
            let usage_known = spans
                .iter()
                .all(|span| span.model_calls == 0 || span.total_tokens.is_some());
            let cost_known = spans
                .iter()
                .all(|span| span.model_calls == 0 || span.cost_known);
            let cost_usd = round6(spans.iter().map(|span| span.cost_usd).sum());
            let first_started_at_ms = spans
                .iter()
                .map(|span| span.started_at_ms)
                .min()
                .unwrap_or(0);
            let last_ended_at_ms = spans.iter().map(|span| span.ended_at_ms).max().unwrap_or(0);
            let worker_window_ms = last_ended_at_ms.saturating_sub(first_started_at_ms);
            let parallel_overlap_factor = (worker_window_ms > 0)
                .then(|| round6(worker_wall_ms_sum as f64 / worker_window_ms as f64));
            let task_attempts = spans
                .iter()
                .filter_map(|span| Some((span.task_id.as_ref()?, span.attempt_id.as_ref()?)))
                .collect::<BTreeSet<_>>()
                .len() as u64;
            let slowest_worker = spans
                .iter()
                .max_by_key(|span| (span.wall_ms, span.span_id.as_str()))
                .map(|span| {
                    json!({
                        "span_id": span.span_id,
                        "role": span.role,
                        "step_id": span.step_id,
                        "task_id": span.task_id,
                        "attempt_id": span.attempt_id,
                        "wall_ms": span.wall_ms,
                    })
                })
                .unwrap_or(Value::Null);
            json!({
                "phase_epoch": phase_epoch,
                "worker_span_count": worker_span_count,
                "task_attempt_count": task_attempts,
                "failed_spans": failed_spans,
                "model_calls": model_calls,
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": total_tokens,
                "usage_known": usage_known,
                "cost_usd": cost_usd,
                "cost_known": cost_known,
                "worker_wall_ms_sum": worker_wall_ms_sum,
                "worker_window_ms": worker_window_ms,
                "parallel_overlap_factor": parallel_overlap_factor,
                "provider_wait_ms_sum": provider_wait_ms_sum,
                "budget_reservation_wait_ms_sum": budget_reservation_wait_ms_sum,
                "lease_wait_ms_sum": lease_wait_ms_sum,
                "slowest_worker": slowest_worker,
            })
        })
        .collect::<Vec<_>>();
    let epoch_count = epochs.len();
    let epochs_truncated = epoch_count > MAX_RETURNED_EPOCHS;
    if epochs_truncated {
        epochs.drain(..epoch_count - MAX_RETURNED_EPOCHS);
    }
    json!({
        "epoch_count": epoch_count,
        "returned_epoch_count": epochs.len(),
        "epochs_truncated": epochs_truncated,
        "items": epochs,
    })
}

fn sum_optional(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut found = false;
    let mut sum = 0u64;
    for value in values {
        found = true;
        sum = sum.saturating_add(value);
    }
    found.then_some(sum)
}
