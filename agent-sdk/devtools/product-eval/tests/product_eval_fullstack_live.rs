//! Live paired Single/Team suite comparison. Ignored by default; requires isolated paths and provider configuration.
use owo_agent_product_eval::product_eval::single_agent::SingleAgentExecutor;
use owo_agent_product_eval::product_eval::{
    build_live_provider, build_paired_report_json, load_suite, AgentMode, CaseExecutor,
    MatrixRunner, PairedReportOptions, RunOptions, RunStatus,
};
use owo_agent_product_eval::workswarm_executor::{WorkSwarmExecutor, WorkSwarmExecutorConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
    mode: AgentMode,
    endpoint_sha256: &str,
    batch_label: &str,
    repetitions: u32,
    case_filter: Option<String>,
) -> RunOptions {
    RunOptions {
        modes: vec![mode],
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

    let single = MatrixRunner::new(suite.clone(), out_root.join("single"))
        .run(
            Arc::new(SingleAgentExecutor::new(Arc::clone(&provider), model.clone()))
                as Arc<dyn CaseExecutor>,
            "live-single",
            Some(model.clone()),
            &options(
                AgentMode::Single,
                &endpoint_sha256,
                &batch_label,
                repetitions,
                case_filter.clone(),
            ),
            Arc::clone(&cancel),
        )
        .await
        .expect("single run");

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
    let team = MatrixRunner::new(suite.clone(), out_root.join("team"))
        .run(
            Arc::new(team_executor) as Arc<dyn CaseExecutor>,
            "live-workswarm",
            Some(model.clone()),
            &options(
                AgentMode::Multi,
                &endpoint_sha256,
                &batch_label,
                repetitions,
                case_filter,
            ),
            cancel,
        )
        .await
        .expect("team run");

    let pair_options = PairedReportOptions {
        model: Some(model.clone()),
        template: None,
        task_set: Some(suite.suite.name.clone()),
        strategy_version: format!(
            "team-force-v1;worker-turn-cap={team_turn_cap};retries={team_retry_cap}"
        ),
    };
    let mut paired = build_paired_report_json(&single, &team, &pair_options, None);
    if let Some(object) = paired.as_object_mut() {
        object.insert(
            "evaluation_contract".to_string(),
            serde_json::json!({
                "batch_label": batch_label.clone(),
                "repetitions_per_selected_task": repetitions,
                "selected_task_ids": selected.iter().map(|case| case.id.clone()).collect::<Vec<_>>(),
                "suite_hash": single.suite_hash.clone(),
                "run_contract_sha256": single.run_contract_sha256.clone(),
                "provider_endpoint_sha256": endpoint_sha256.clone(),
                "task_diversity": {
                    "selected_task_count": selected.len(),
                    "selected_task_categories": selected.iter().map(|case| case.category.as_str()).collect::<std::collections::BTreeSet<_>>(),
                    "minimum_distinct_tasks_for_broad_claim": 3,
                },
                "execution_order": "all Single cells run before Team cells; order effects and external provider contention are not randomized",
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
    let alignment = paired
        .pointer("/run_alignment/configuration_aligned")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
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
