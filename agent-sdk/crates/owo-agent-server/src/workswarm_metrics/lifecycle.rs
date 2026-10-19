//! Durable Team lifecycle timings for scheduler, registry and delivery orchestration.
use super::util::{now_ms, rfc3339};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TeamLifecycleSpanRecord {
    pub span_id: String,
    pub team_id: String,
    pub sequence: u64,
    /// Coordinator execution epoch for phase-level spans; absent on registry setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_epoch: Option<u64>,
    pub stage: String,
    pub started_at: String,
    pub ended_at: String,
    #[serde(default)]
    pub started_at_ms: u64,
    #[serde(default)]
    pub ended_at_ms: u64,
    pub duration_ms: u64,
    /// succeeded | failed | more_ready | awaiting_human | aborted | finished.
    pub outcome: String,
}

#[derive(Clone)]
pub(crate) struct TeamLifecycleMetricsJournal {
    path: Arc<PathBuf>,
    write_lock: Arc<Mutex<()>>,
}

impl TeamLifecycleMetricsJournal {
    pub(crate) fn for_team(run_dir: &Path, team_id: &str) -> Self {
        Self {
            path: Arc::new(run_dir.join(format!("{team_id}-lifecycle-metrics.jsonl"))),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn append(&self, record: &TeamLifecycleSpanRecord) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        line.push('\n');
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*self.path)?;
        file.write_all(line.as_bytes())
    }

    pub(crate) fn read_records(&self) -> Vec<TeamLifecycleSpanRecord> {
        let Ok(text) = std::fs::read_to_string(&*self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line.trim()).ok())
            .collect()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) struct TeamLifecycleTimer {
    sequence: u64,
    stage: String,
    started_at: String,
    started_at_ms: u64,
    started: Instant,
}

impl TeamLifecycleTimer {
    pub(crate) fn start(sequence: u64, stage: &str) -> Self {
        Self {
            sequence,
            stage: stage.to_string(),
            started_at: rfc3339(),
            started_at_ms: now_ms(),
            started: Instant::now(),
        }
    }

    pub(crate) fn finish(
        self,
        journal: &TeamLifecycleMetricsJournal,
        team_id: &str,
        outcome: &str,
    ) {
        self.finish_with_phase_epoch(journal, team_id, outcome, None);
    }

    pub(crate) fn finish_with_phase_epoch(
        self,
        journal: &TeamLifecycleMetricsJournal,
        team_id: &str,
        outcome: &str,
        phase_epoch: Option<u64>,
    ) {
        let ended_at_ms = now_ms();
        let record = TeamLifecycleSpanRecord {
            span_id: format!("lifecycle-{}", uuid::Uuid::new_v4().simple()),
            team_id: team_id.to_string(),
            sequence: self.sequence,
            phase_epoch,
            stage: self.stage,
            started_at: self.started_at,
            ended_at: rfc3339(),
            started_at_ms: self.started_at_ms,
            ended_at_ms,
            duration_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            outcome: outcome.to_string(),
        };
        if let Err(error) = journal.append(&record) {
            tracing::warn!(team_id, stage = %record.stage, error = %error, "Team 生命周期指标写入失败");
        }
    }
}

fn percentile(sorted: &[u64], numerator: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = sorted
        .len()
        .saturating_mul(numerator)
        .saturating_add(99)
        .checked_div(100)
        .unwrap_or(1)
        .max(1);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

pub(crate) fn aggregate_lifecycle_metrics(records: &[TeamLifecycleSpanRecord]) -> Value {
    #[derive(Default)]
    struct StageAggregate {
        count: u64,
        failures: u64,
        total_ms: u64,
        durations: Vec<u64>,
    }
    let mut stages = BTreeMap::<String, StageAggregate>::new();
    for record in records {
        let aggregate = stages.entry(record.stage.clone()).or_default();
        aggregate.count = aggregate.count.saturating_add(1);
        aggregate.total_ms = aggregate.total_ms.saturating_add(record.duration_ms);
        aggregate.durations.push(record.duration_ms);
        if record.outcome == "failed" {
            aggregate.failures = aggregate.failures.saturating_add(1);
        }
    }
    let mut epoch_phase = BTreeMap::<u64, (u64, u64, u64)>::new();
    for record in records
        .iter()
        .filter(|record| record.stage == "phase_orchestration")
    {
        if let Some(epoch) = record.phase_epoch {
            let aggregate = epoch_phase.entry(epoch).or_default();
            aggregate.0 = aggregate.0.saturating_add(1);
            aggregate.1 = aggregate.1.saturating_add(record.duration_ms);
            aggregate.2 = aggregate.2.max(record.duration_ms);
        }
    }
    let mut epoch_phase = epoch_phase
        .into_iter()
        .map(
            |(phase_epoch, (span_count, duration_ms_sum, duration_ms_max))| {
                json!({
                    "phase_epoch": phase_epoch,
                    "phase_span_count": span_count,
                    "phase_duration_ms_sum": duration_ms_sum,
                    "phase_duration_ms_max": duration_ms_max,
                })
            },
        )
        .collect::<Vec<_>>();
    const MAX_RETURNED_EPOCHS: usize = 100;
    let epoch_count = epoch_phase.len();
    let epochs_truncated = epoch_count > MAX_RETURNED_EPOCHS;
    if epochs_truncated {
        epoch_phase.drain(..epoch_count - MAX_RETURNED_EPOCHS);
    }
    let stages = stages
        .into_iter()
        .map(|(stage, mut aggregate)| {
            aggregate.durations.sort_unstable();
            json!({
                "stage": stage,
                "count": aggregate.count,
                "failures": aggregate.failures,
                "duration_ms_sum": aggregate.total_ms,
                "duration_ms_p50": percentile(&aggregate.durations, 50),
                "duration_ms_p90": percentile(&aggregate.durations, 90),
                "duration_ms_max": aggregate.durations.last().copied().unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    const MAX_RETURNED_SPANS: usize = 200;
    let mut spans = records.to_vec();
    spans.sort_by_key(|record| {
        (
            record.sequence,
            record.started_at_ms,
            record.span_id.clone(),
        )
    });
    let span_count = spans.len();
    let spans_truncated = span_count > MAX_RETURNED_SPANS;
    if spans_truncated {
        spans.drain(..span_count - MAX_RETURNED_SPANS);
    }
    json!({
        "span_count": span_count,
        "returned_span_count": spans.len(),
        "spans_truncated": spans_truncated,
        "stages": stages,
        "epoch_count": epoch_count,
        "returned_epoch_count": epoch_phase.len(),
        "epochs_truncated": epochs_truncated,
        "epochs": epoch_phase,
        "spans": spans,
    })
}

/// Aggregate durable Worker spans that include context construction time.
pub(crate) fn aggregate_context_assembly_metrics(
    records: &[super::metrics::WorkerSpanRecord],
) -> Value {
    #[derive(Default)]
    struct Aggregate {
        count: u64,
        failures: u64,
        total_ms: u64,
        durations: Vec<u64>,
    }

    let mut total = Aggregate::default();
    let mut by_role = BTreeMap::<String, Aggregate>::new();
    let mut by_epoch = BTreeMap::<u64, Aggregate>::new();
    let mut attribution_missing_count = 0u64;

    for record in records {
        let Some(duration_ms) = record.context_assembly_ms else {
            attribution_missing_count = attribution_missing_count.saturating_add(1);
            continue;
        };
        let failed = record.outcome != "succeeded";
        let append = |aggregate: &mut Aggregate| {
            aggregate.count = aggregate.count.saturating_add(1);
            aggregate.total_ms = aggregate.total_ms.saturating_add(duration_ms);
            aggregate.durations.push(duration_ms);
            if failed {
                aggregate.failures = aggregate.failures.saturating_add(1);
            }
        };
        append(&mut total);
        append(
            by_role
                .entry(record.role.chars().take(80).collect())
                .or_default(),
        );
        if let Some(epoch) = record.phase_epoch {
            append(by_epoch.entry(epoch).or_default());
        }
    }

    let summarize = |mut aggregate: Aggregate| {
        aggregate.durations.sort_unstable();
        json!({
            "sample_count": aggregate.count,
            "failures": aggregate.failures,
            "duration_ms_sum": aggregate.total_ms,
            "duration_ms_p50": percentile(&aggregate.durations, 50),
            "duration_ms_p90": percentile(&aggregate.durations, 90),
            "duration_ms_max": aggregate.durations.last().copied().unwrap_or(0),
        })
    };
    let by_role = by_role
        .into_iter()
        .map(|(role, aggregate)| {
            let mut summary = summarize(aggregate);
            summary["role"] = json!(role);
            summary
        })
        .collect::<Vec<_>>();
    let by_epoch = by_epoch
        .into_iter()
        .map(|(phase_epoch, aggregate)| {
            let mut summary = summarize(aggregate);
            summary["phase_epoch"] = json!(phase_epoch);
            summary
        })
        .collect::<Vec<_>>();

    let mut summary = summarize(total);
    summary["attribution_missing_count"] = json!(attribution_missing_count);
    summary["by_role"] = json!(by_role);
    summary["by_epoch"] = json!(by_epoch);
    summary
}
