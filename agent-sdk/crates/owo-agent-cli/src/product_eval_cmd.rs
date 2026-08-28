//! `owo-agent product-eval`（V1-R1）：产品级评测底座 CLI。
//!
//! - `validate`：10 个任务定义 schema/参考输出/权限范围全量校验；
//! - `run`：执行评测矩阵（dry 参考回放 = 本地确定性；live = 网关生成文件块；
//!   agent = 真实单 Agent：独立 Session + 最小受控工具集 + 任务级权限审批），
//!   journal 断点续跑、失败运行永久进报告；
//! - `compare`：对照两份报告（单/多 Agent、不同批次），失败记录不参与任何剔除。

use clap::{Args, Subcommand};
use owo_agent_core::product_eval::{
    build_live_provider, compare_reports, format_report_summary, format_validation, load_report,
    load_suite, resolve_suite_input, AgentMode, EvalCategory, GenerativeExecutor, MatrixRunner,
    ReferenceDryExecutor, RunOptions, SingleAgentExecutor,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Args)]
pub struct ProductEvalArgs {
    #[command(subcommand)]
    pub action: ProductEvalAction,
}

#[derive(Subcommand)]
pub enum ProductEvalAction {
    /// 校验套件与全部任务定义（schema / 参考输出可达 / 权限范围）
    Validate {
        /// suite.json 路径（缺省向上查找 agent-sdk/evals/v1/suite.json）
        #[arg(long)]
        suite: Option<PathBuf>,
    },
    /// 执行评测矩阵（断点续跑；--exec dry 为本地确定性回放）
    Run {
        /// suite.json 路径（缺省向上查找 agent-sdk/evals/v1/suite.json）
        #[arg(long)]
        suite: Option<PathBuf>,
        /// 执行模式：dry（参考回放，零模型调用）| live（网关生成文件块）
        /// | agent（真实单 Agent：独立 Session + 工具 + 权限 + 审批）
        #[arg(long, default_value = "dry")]
        exec: String,
        /// 对照拓扑：single,multi
        #[arg(long, default_value = "single,multi")]
        agents: String,
        /// 覆盖重复次数（缺省用任务/套件定义，V1 目标 20）
        #[arg(long)]
        reps: Option<u32>,
        /// 只跑 id 包含该子串的任务
        #[arg(long)]
        only: Option<String>,
        /// 只跑指定分类：code|research|document
        #[arg(long)]
        category: Option<String>,
        /// 输出目录（缺省 <agent-sdk>/scratch-eval-runs/product-eval）
        #[arg(long)]
        out: Option<PathBuf>,
        /// 模型覆盖（缺省 OPENAI_MODEL / 内置 GLM 默认）
        #[arg(long)]
        model: Option<String>,
        /// 清空输出目录重跑（唯一允许的归零入口）
        #[arg(long)]
        fresh: bool,
    },
    /// 对照两份报告
    Compare {
        report_a: PathBuf,
        report_b: PathBuf,
        /// 输出格式：text|json
        #[arg(long, default_value = "text")]
        format: String,
    },
}

/// 入口（由 main.rs 的 Commands::ProductEval 分发）。
pub async fn run(args: ProductEvalArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.action {
        ProductEvalAction::Validate { suite } => run_validate(suite),
        ProductEvalAction::Run {
            suite,
            exec,
            agents,
            reps,
            only,
            category,
            out,
            model,
            fresh,
        } => run_matrix_cmd(suite, exec, agents, reps, only, category, out, model, fresh).await,
        ProductEvalAction::Compare {
            report_a,
            report_b,
            format,
        } => run_compare(report_a, report_b, format),
    }
}

fn run_validate(suite: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let suite_path = resolve_suite_input(suite.as_deref())?;
    let bundle = load_suite(&suite_path)?;
    println!("套件：{}（{}）", bundle.suite.name, suite_path.display());
    let validation = owo_agent_core::product_eval::validate_suite(&bundle);
    print!("{}", format_validation(&validation));
    if !validation.all_ok {
        std::process::exit(1);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_matrix_cmd(
    suite: Option<PathBuf>,
    exec: String,
    agents: String,
    reps: Option<u32>,
    only: Option<String>,
    category: Option<String>,
    out: Option<PathBuf>,
    model: Option<String>,
    fresh: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let suite_path = resolve_suite_input(suite.as_deref())?;
    let bundle = load_suite(&suite_path)?;
    println!("套件：{}（{}）", bundle.suite.name, suite_path.display());

    let modes = AgentMode::parse_list(&agents)?;
    let category = match category.as_deref() {
        None => None,
        Some(raw) => Some(EvalCategory::parse(raw)?),
    };
    if !matches!(exec.as_str(), "dry" | "live" | "agent") {
        return Err(format!("未知执行模式「{exec}」（可选 dry|live|agent）").into());
    }

    let (executor, execution, model_label): (
        Arc<dyn owo_agent_core::product_eval::CaseExecutor>,
        String,
        Option<String>,
    ) = if exec == "live" {
        let (provider, resolved_model) = build_live_provider(model.as_deref())?;
        (
            Arc::new(GenerativeExecutor {
                provider,
                model: resolved_model.clone(),
            }),
            "live-generative".to_string(),
            Some(resolved_model),
        )
    } else if exec == "agent" {
        let (provider, resolved_model) = build_live_provider(model.as_deref())?;
        (
            Arc::new(SingleAgentExecutor::new(provider, resolved_model.clone())),
            "live-agent".to_string(),
            Some(resolved_model),
        )
    } else {
        (
            Arc::new(ReferenceDryExecutor),
            "dry-reference".to_string(),
            Some(crate::resolve_model(model, None)),
        )
    };

    // 缺省输出目录：<agent-sdk>/scratch-eval-runs/product-eval（scratch-* 已被 gitignore）。
    let out_dir = out.unwrap_or_else(|| {
        bundle
            .dir
            .parent()
            .and_then(|evals| evals.parent())
            .map(|agent_sdk| agent_sdk.join("scratch-eval-runs").join("product-eval"))
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("scratch-eval-runs")
                    .join("product-eval")
            })
    });
    println!("输出目录：{}（{}）", out_dir.display(), execution);

    let runner = MatrixRunner::new(bundle, out_dir.clone());
    let opts = RunOptions {
        modes,
        reps_override: reps,
        only,
        category,
        fresh,
    };
    let cancel = Arc::new(AtomicBool::new(false));

    let run_future = runner.run(
        executor,
        &execution,
        model_label.clone(),
        &opts,
        Arc::clone(&cancel),
    );
    tokio::pin!(run_future);
    let report = tokio::select! {
        result = &mut run_future => result?,
        _ = tokio::signal::ctrl_c() => {
            println!("\n收到 Ctrl-C：停止调度后续单元格，保留已完成记录（重跑同一命令即可续跑）…");
            cancel.store(true, Ordering::Relaxed);
            run_future.await?
        }
    };

    print!("{}", format_report_summary(&report));
    println!("报告：{}", out_dir.join("report.json").display());
    println!("journal：{}", out_dir.join("state.jsonl").display());

    // dry 是校验性运行：未全绿按失败退出，供 CI 门禁使用；live 是测量性运行，恒 0。
    if execution == "dry-reference" && report.metrics.success_rate < 1.0 {
        std::process::exit(1);
    }
    Ok(())
}

fn run_compare(
    report_a: PathBuf,
    report_b: PathBuf,
    format: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let a = load_report(&report_a)?;
    let b = load_report(&report_b)?;
    let as_json = match format.as_str() {
        "json" => true,
        "text" => false,
        other => return Err(format!("未知输出格式「{other}」（可选 text|json）").into()),
    };
    println!("{}", compare_reports(&a, &b, as_json));
    Ok(())
}
