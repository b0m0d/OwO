//! One-repetition live full-stack comparison. Ignored by default; all paths are supplied by env.
use owo_agent_product_eval::product_eval::single_agent::SingleAgentExecutor;
use owo_agent_product_eval::product_eval::{
    build_live_provider, load_suite, AgentMode, CaseExecutor, MatrixRunner, RunOptions, RunStatus,
};
use owo_agent_product_eval::workswarm_executor::{WorkSwarmExecutor, WorkSwarmExecutorConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const CASE_ID: &str = "editorial-blog-fullstack";
const MODEL: &str = "glm-5.3-flashx";

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("missing {name}")))
}

fn options(mode: AgentMode, endpoint_sha256: &str) -> RunOptions {
    RunOptions {
        modes: vec![mode],
        reps_override: Some(1),
        only: Some(CASE_ID.to_string()),
        fresh: true,
        batch_label: Some("team-refactor-20261003-r20-bounded-8turns-1retry".into()),
        provider_endpoint_sha256: Some(endpoint_sha256.to_string()),
        ..RunOptions::default()
    }
}

#[tokio::test]
#[ignore = "Bounded one-pair live full-stack comparison; requires isolated paths and configured provider env"]
async fn live_fullstack_single_vs_team_one_rep() {
    let suite_path = required_path("OWO_BLOG_BENCHMARK_SUITE");
    let out_root = required_path("OWO_BLOG_BENCHMARK_OUT");
    let bundle = load_suite(&suite_path).expect("load benchmark suite");
    assert!(
        bundle.cases.iter().any(|case| case.id == CASE_ID),
        "case missing"
    );
    assert!(
        !out_root.exists(),
        "output root must be new; refusing to overwrite it"
    );
    let (provider, model, endpoint_sha256) = build_live_provider(Some(MODEL)).expect("build configured provider");
    println!("paired full-stack run: model={model}, case={CASE_ID}, repetitions=1");
    let cancel = Arc::new(AtomicBool::new(false));

    let single = MatrixRunner::new(bundle.clone(), out_root.join("single"))
        .run(
            Arc::new(SingleAgentExecutor::new(
                Arc::clone(&provider),
                model.clone(),
            )) as Arc<dyn CaseExecutor>,
            "live-single",
            Some(model.clone()),
            &options(AgentMode::Single, &endpoint_sha256),
            Arc::clone(&cancel),
        )
        .await
        .expect("single run");
    for run in &single.runs {
        println!(
            "[single] status={:?} wall_ms={} calls={} tokens={:?} artifacts={} checkers={}/{} error={:?}",
            run.status, run.wall_ms, run.model_calls, run.total_tokens, run.artifact_refs.len(),
            run.checker_passed, run.checker_total, run.error
        );
    }
    // Keep the pair even when one side fails: completion degree and failed wall time
    // are part of a time-boxed speed comparison, not reasons to discard a sample.

    let mut team_executor = WorkSwarmExecutor::new(
        Arc::clone(&provider),
        model.clone(),
        out_root.join("team-runs"),
    );
    team_executor.config = WorkSwarmExecutorConfig {
        max_turns_per_worker: 8,
        max_retries_on_failure: 1,
        selection: owo_agent_core::team_strategy::TeamSelectionMode::ForceTeam,
    };
    let team = MatrixRunner::new(bundle, out_root.join("team"))
        .run(
            Arc::new(team_executor) as Arc<dyn CaseExecutor>,
            "live-team",
            Some(model.clone()),
            &options(AgentMode::Multi, &endpoint_sha256),
            cancel,
        )
        .await
        .expect("team run");

    for (label, report) in [("single", single), ("team", team)] {
        for run in report.runs {
            println!(
                "[{label}] status={:?} wall_ms={} calls={} tokens={:?} artifacts={} checkers={}/{} retries={} error={:?}",
                run.status,
                run.wall_ms,
                run.model_calls,
                run.total_tokens,
                run.artifact_refs.len(),
                run.checker_passed,
                run.checker_total,
                run.retries,
                run.error,
            );
            assert_ne!(
                run.status,
                RunStatus::Error,
                "{label} 未形成有效运行样本：{:?}",
                run.error
            );
        }
    }
}
