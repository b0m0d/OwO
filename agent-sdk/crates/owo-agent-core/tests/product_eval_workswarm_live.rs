//! WorkSwarm 真实 GLM 对照驱动（`#[ignore]`；显式带凭据运行）。
//!
//! 对 evals/v1 三个分类各取 1 条任务（code-bug-fix / research-source-compare /
//! document-revise），在**同一 MatrixRunner、同一任务定义、同输入、同权限、
//! 同预算、同检查器**下分别跑：
//! - single：Route 1 的 `SingleAgentExecutor`（真实单 Agent，GLM）；
//! - multi：本适配器 `WorkSwarmExecutor`（真实 TeamRun：writer/builder|researcher
//!   + critic + leader，≤3 个 Agent Worker，中间结果走 Artifact/Handoff）。
//!
//! 运行方式（凭据仅经环境变量，绝不落盘/入库）：
//! ```text
//! $env:OPENAI_API_KEY = "<key>"
//! cargo test -p owo-agent-core --test product_eval_workswarm_live -- --ignored --nocapture --test-threads=1
//! ```
//! 结果落盘于 `agent-sdk/scratch-eval-runs/live-workswarm/`（gitignored）。

use owo_agent_core::product_eval::single_agent::SingleAgentExecutor;
use owo_agent_core::product_eval::{load_suite, AgentMode, CaseExecutor, MatrixRunner, RunOptions};
use owo_agent_core::{WorkSwarmExecutor, WorkSwarmExecutorConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const CASES: [&str; 3] = ["code-bug-fix", "research-source-compare", "document-revise"];

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[tokio::test]
#[ignore = "真实 GLM 对照：需要 OPENAI_API_KEY，显式 --ignored 运行"]
async fn live_single_vs_workswarm_three_categories() {
    if std::env::var("OPENAI_API_KEY")
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        panic!("live 对照需要 OPENAI_API_KEY 环境变量（凭据仅经环境注入）");
    }
    let (provider, model) = owo_agent_core::product_eval::build_live_provider(None).unwrap();
    println!("live 对照模型：{model}");

    let suite_path = repo_path("../../evals/v1/suite.json");
    let bundle = load_suite(&suite_path).unwrap();
    let out_root = repo_path("../../scratch-eval-runs/live-workswarm");
    std::fs::create_dir_all(&out_root).unwrap();

    for case_id in CASES {
        println!("\n================ case {case_id} ================");

        // —— single：Route 1 单 Agent 执行器（同一 runner/任务/检查器）——
        let single_out = out_root.join(format!("out-single-{case_id}"));
        let single_runner = MatrixRunner::new(bundle.clone(), &single_out);
        let single_report = single_runner
            .run(
                Arc::new(SingleAgentExecutor::new(
                    Arc::clone(&provider),
                    model.clone(),
                )) as Arc<dyn CaseExecutor>,
                "live-single",
                Some(model.clone()),
                &RunOptions {
                    modes: vec![AgentMode::Single],
                    reps_override: Some(1),
                    only: Some(case_id.to_string()),
                    fresh: true,
                    ..RunOptions::default()
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();

        // —— multi：WorkSwarm 真实 TeamRun ——
        let multi_out = out_root.join(format!("out-multi-{case_id}"));
        let mut workswarm = WorkSwarmExecutor::new(
            Arc::clone(&provider),
            model.clone(),
            out_root.join(format!("teams-{case_id}")),
        );
        workswarm.config = WorkSwarmExecutorConfig {
            max_turns_per_worker: 6,
            max_retries_on_failure: 1,
            selection: owo_agent_core::team_strategy::TeamSelectionMode::Auto,
        };
        let multi_runner = MatrixRunner::new(bundle.clone(), &multi_out);
        let multi_report = multi_runner
            .run(
                Arc::new(workswarm) as Arc<dyn CaseExecutor>,
                "live-workswarm",
                Some(model.clone()),
                &RunOptions {
                    modes: vec![AgentMode::Multi],
                    reps_override: Some(1),
                    only: Some(case_id.to_string()),
                    fresh: true,
                    ..RunOptions::default()
                },
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();

        let summarize = |label: &str, report: &owo_agent_core::product_eval::ProductEvalReport| {
            for run in &report.runs {
                println!(
                    "[{label}] {} → {:?} wall={}ms model_calls={} retries={} tokens={:?} artifacts={:?} failed={:?} error={:?}",
                    run.key,
                    run.status,
                    run.wall_ms,
                    run.model_calls,
                    run.retries,
                    run.total_tokens,
                    run.artifact_refs,
                    run.failed_steps,
                    run.error
                );
            }
        };
        summarize("single", &single_report);
        summarize("multi", &multi_report);

        // —— multi 观测：各 Worker 独立运行记录 ——
        let teams_dir = out_root.join(format!("teams-{case_id}"));
        if let Ok(entries) = std::fs::read_dir(&teams_dir) {
            for entry in entries.flatten() {
                let observation_path = entry.path().join("observation.json");
                if let Ok(text) = std::fs::read_to_string(&observation_path) {
                    println!("---- observation {case_id} ----\n{text}");
                }
            }
        }
    }
}
