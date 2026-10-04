//! Live paired Single/Team suite comparison. Ignored by default; requires isolated paths and provider configuration.
use owo_agent_product_eval::product_eval::single_agent::SingleAgentExecutor;
use owo_agent_product_eval::product_eval::{
    build_live_provider, build_paired_report_json, load_suite, split_paired_mode_reports,
    AgentMode, CaseExecutor, ExecContext, MatrixRunner, PairedReportOptions, RawExecOutcome,
    RunOptions, RunStatus,
};
use owo_agent_product_eval::workswarm_executor::{WorkSwarmExecutor, WorkSwarmExecutorConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use sha2::{Digest, Sha256};

struct PairedDispatchExecutor {
    single: Arc<dyn CaseExecutor>,
    team: Arc<dyn CaseExecutor>,
}

#[async_trait::async_trait]
impl CaseExecutor for PairedDispatchExecutor {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        match ctx.mode {
            AgentMode::Single => self.single.execute(ctx).await,
            AgentMode::Multi => self.team.execute(ctx).await,
        }
    }
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}")))
}

fn required_nonempty(name: &str) -> String {
    let value = std::env::var(name)
        .unwrap_or_else(|_| panic!("missing {name}; this value is required for reproducibility"))
        .trim()
        .to_string();
    assert!(!value.is_empty(), "{name} cannot be empty");
    value
}

fn optional_positive_u32(name: &str) -> Option<u32> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<u32>()
                .unwrap_or_else(|_| panic!("{name} must be a positive integer"))
        })
        .map(|value| {
            assert!(value > 0, "{name} must be greater than zero");
            value
        })
}

fn optional_positive_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("{name} must be a positive integer"))
        })
        .map(|value| {
            assert!(value > 0, "{name} must be greater than zero");
            value
        })
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn options(
    modes: Vec<AgentMode>,
    endpoint_sha256: &str,
    batch_label: &str,
    repetitions: u32,
    case_filter: Option<String>,
) -> RunOptions {
    RunOptions {
        modes,
        reps_override: Some(repetitions),
        only: case_filter,
        fresh: true,
        batch_label: Some(batch_label.to_string()),
        provider_endpoint_sha256: Some(endpoint_sha256.to_string()),
        tags: vec![
            "paired-live-v1".to_string(),
            format!("repetitions={repetitions}"),
        ],
        ..RunOptions::default()
    }
}

fn save_json(path: PathBuf, value: &impl serde::Serialize) {
    let contents = serde_json::to_vec_pretty(value).expect("serialize paired evaluation report");
    std::fs::write(&path, contents)
        .unwrap_or_else(|error| panic!("write {} failed: {error}", path.display()));
}

#[tokio::test]
#[ignore = "Live paired suite comparison; requires explicit repetition/batch, isolated output, and configured provider"]
async fn live_single_vs_team_paired_suite() {
    assert!(
        !truthy_env("OWO_PRODUCT_EVAL_UNBOUNDED_CALLS"),
        "paired evidence requires the shared per-task model-call budget; unset OWO_PRODUCT_EVAL_UNBOUNDED_CALLS"
    );
    let suite_path = required_path("OWO_BLOG_BENCHMARK_SUITE");
    let out_root = required_path("OWO_BLOG_BENCHMARK_OUT");
    let batch_label = required_nonempty("OWO_PAIRED_BATCH_LABEL");
    let repetitions = optional_positive_u32("OWO_PAIRED_REPETITIONS")
        .unwrap_or_else(|| panic!("missing OWO_PAIRED_REPETITIONS; choose the planned sample count"));
    let case_filter = std::env::var("OWO_PAIRED_CASE_FILTER")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let team_turn_cap = optional_positive_usize("OWO_PAIRED_TEAM_TURNS").unwrap_or(8);
    let team_retry_cap = std::env::var("OWO_PAIRED_TEAM_RETRIES")
        .ok()
        .map(|value| value.parse::<u32>().unwrap_or_else(|_| panic!("OWO_PAIRED_TEAM_RETRIES must be an integer")))
        .unwrap_or(1);
    let suite = load_suite(&suite_path).expect("load benchmark suite");
    let selected = owo_agent_product_eval::product_eval::filter_cases(
        &suite,
        &RunOptions {
            only: case_filter.clone(),
            ..RunOptions::default()
        },
    );
    assert!(!selected.is_empty(), "case filter matched no benchmark tasks");
    assert!(
        !out_root.exists(),
        "output root must be new; refusing to overwrite it"
    );

    let model_override = std::env::var("OWO_PAIRED_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let (provider, model, endpoint_sha256) =
        build_live_provider(model_override.as_deref()).expect("build configured provider");
    println!(
        "paired live suite: label={batch_label}, model={model}, tasks={}, repetitions={repetitions}, shared per-task model-call/time budget, Team worker_turn_cap={team_turn_cap}, Team retries={team_retry_cap}",
        selected.len()
    );
    let cancel = Arc::new(AtomicBool::new(false));

    let mut team_executor = WorkSwarmExecutor::new(
        Arc::clone(&provider),
        model.clone(),
        out_root.join("team-runs"),
    );
    team_executor.config = WorkSwarmExecutorConfig {
        max_turns_per_worker: team_turn_cap,
        max_retries_on_failure: team_retry_cap,
        selection: owo_agent_core::team_strategy::TeamSelectionMode::ForceTeam,
    };
    let paired_executor = PairedDispatchExecutor {
        single: Arc::new(SingleAgentExecutor::new(Arc::clone(&provider), model.clone())),
        team: Arc::new(team_executor),
    };
    let matrix_options = options(
        vec![AgentMode::Single, AgentMode::Multi],
        &endpoint_sha256,
        &batch_label,
        repetitions,
        case_filter,
    );
    let matrix = MatrixRunner::new(suite.clone(), out_root.join("matrix"))
        .run(
            Arc::new(paired_executor),
            "live-paired",
            Some(model.clone()),
            &matrix_options,
            cancel,
        )
        .await
        .expect("paired run");
    let (single, team) = split_paired_mode_reports(&matrix).expect("split paired run report");

    let pair_options = PairedReportOptions {
        model: Some(model.clone()),
        template: None,
        task_set: Some(suite.suite.name.clone()),
        strategy_version: format!(
            "team-force-v1;worker-turn-cap={team_turn_cap};retries={team_retry_cap}"
        ),
    };
    let mut paired = build_paired_report_json(&single, &team, &pair_options, None);
    let alignment = paired
        .pointer("/run_alignment/configuration_aligned")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let selected_categories = selected
        .iter()
        .map(|case| case.category.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let paired_cells = paired
        .pointer("/run_alignment/paired_cells")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let coverage_evidence_sufficient =
        alignment && paired_cells >= 30 && selected.len() >= 3 && selected_categories.len() >= 2;
    if let Some(object) = paired.as_object_mut() {
        object.insert(
            "evaluation_contract".to_string(),
            serde_json::json!({
                "batch_label": batch_label.clone(),
                "repetitions_per_selected_task": repetitions,
                "selected_task_ids": selected.iter().map(|case| case.id.clone()).collect::<Vec<_>>(),
                "suite_hash": matrix.suite_hash.clone(),
                "run_contract_sha256": matrix.run_contract_sha256.clone(),
                "provider_endpoint_sha256": endpoint_sha256.clone(),
                "execution_order": "within each task, two-repetition blocks use a batch/task-derived randomized AB/BA order",
                "order_counterbalanced": true,
                "order_randomized": true,
                "order_seed_sha256": format!("{:x}", Sha256::digest(batch_label.as_bytes())),
                "order_assignment": "lowest SHA-256 bit of batch_label + case_id + pair_block selects block orientation; next repetition reverses it",
                "causal_comparison_eligible": false,
                "causal_limit": "external provider contention and model-service drift remain; paired confidence intervals are not yet implemented",
                "sample_assessment": {
                    "paired_cells": paired_cells,
                    "minimum_paired_cells": 30,
                    "selected_task_count": selected.len(),
                    "minimum_distinct_tasks": 3,
                    "selected_task_categories": selected_categories,
                    "minimum_categories": 2,
                    "coverage_evidence_sufficient": coverage_evidence_sufficient,
                },
                "shared_budget": {
                    "model_calls": "case.max_model_calls",
                    "wall_time": "case.timeout_secs",
                    "unbounded_single_override": false,
                },
                "single_profile": {
                    "topology": "single-agent",
                    "model_call_budget": "shared per-task budget",
                },
                "team_profile": {
                    "topology": "forced-team",
                    "global_model_call_budget": "shared per-task budget",
                    "worker_turn_cap": team_turn_cap,
                    "retry_cap": team_retry_cap,
                }
            }),
        );
    }
    save_json(out_root.join("matrix-report.json"), &matrix);
    save_json(out_root.join("single-report.json"), &single);
    save_json(out_root.join("team-report.json"), &team);
    save_json(out_root.join("paired-report.json"), &paired);

    for (label, report) in [("single", &single), ("team", &team)] {
        for run in &report.runs {
            println!(
                "[{label}] case={} repetition={} status={:?} wall_ms={} executor_wall_ms={} validation_wall_ms={} delivery_gate_wall_ms={} calls={} tokens={:?} artifacts={} checkers={}/{} retries={} error={:?}",
                run.key.case_id,
                run.key.repetition,
                run.status,
                run.wall_ms,
                run.executor_wall_ms,
                run.validation_wall_ms,
                run.delivery_gate_wall_ms,
                run.model_calls,
                run.total_tokens,
                run.artifact_refs.len(),
                run.checker_passed,
                run.checker_total,
                run.retries,
                run.error,
            );
        }
    }
    assert!(
        alignment,
        "Single/Team pairing failed alignment; inspect {}",
        out_root.join("paired-report.json").display()
    );
    let error_count = single
        .runs
        .iter()
        .chain(team.runs.iter())
        .filter(|run| run.status == RunStatus::Error)
        .count();
    println!(
        "paired report saved; aligned={alignment}, single_cells={}, team_cells={}, errors={error_count}",
        single.runs.len(),
        team.runs.len()
    );
}
