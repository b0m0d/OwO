//! `owo-agent product-eval`（V1-R1）：产品级评测底座 CLI。
//!
//! - `validate`：10 个任务定义 schema/参考输出/权限范围全量校验；
//! - `preflight`：live 基线环境预检（Provider/凭据存在性/suite/输出目录/磁盘/ORT），
//!   只输出"已配置/未配置"，绝不输出凭据内容；
//! - `run`：执行评测矩阵（dry 参考回放 = 本地确定性；live = 网关生成文件块；
//!   agent = 真实单 Agent：独立 Session + 最小受控工具集 + 任务级权限审批），
//!   journal 断点续跑、失败运行永久进报告；
//! - `compare`：对照两份报告（单/多 Agent、不同批次），失败记录不参与任何剔除。

use clap::{Args, Subcommand};
use owo_agent_core::product_eval::{
    build_live_provider, compare_reports, format_mode_statistics, format_report_summary,
    format_validation, load_report, load_suite, resolve_suite_input, AgentMode, EvalCategory,
    GenerativeExecutor, MatrixRunner, ReferenceDryExecutor, RunOptions, SingleAgentExecutor,
};
use owo_agent_core::product_eval_workswarm::WorkSwarmExecutor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

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
        /// | agent（真实单 Agent）| workswarm（真实 WorkSwarm TeamRun）
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
        /// 组队模式（仅 exec=workswarm 生效）：single|team|auto，缺省 auto
        #[arg(long)]
        team_mode: Option<String>,
        /// 清空输出目录重跑（唯一允许的归零入口）
        #[arg(long)]
        fresh: bool,
    },
    /// live 基线环境预检（不泄露凭据内容；exit 0=就绪，2=存在阻塞项）
    Preflight {
        /// suite.json 路径（缺省向上查找 agent-sdk/evals/v1/suite.json）
        #[arg(long)]
        suite: Option<PathBuf>,
        /// 输出目录（检查可写性；缺省 <agent-sdk>/scratch-eval-runs/product-eval）
        #[arg(long)]
        out: Option<PathBuf>,
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
        ProductEvalAction::Preflight { suite, out } => run_preflight(suite, out),
        ProductEvalAction::Run {
            suite,
            exec,
            agents,
            reps,
            only,
            category,
            out,
            model,
            team_mode,
            fresh,
        } => {
            let selection = match team_mode.as_deref() {
                None => owo_agent_core::team_strategy::TeamSelectionMode::default(),
                Some(raw) => owo_agent_core::team_strategy::TeamSelectionMode::parse(raw)?,
            };
            run_matrix_cmd(
                suite, exec, agents, reps, only, category, out, model, selection, fresh,
            )
            .await
        }
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

// ---------------------------------------------------------------------------
// preflight：live 基线环境预检（只报"已配置/未配置"，绝不输出凭据内容）
// ---------------------------------------------------------------------------

/// 在仓库本地缓存中探测 onnxruntime.lib（ort crate 链接期需要 ORT_LIB_PATH）。
/// 只探测不写入：返回第一个命中目录。
fn probe_ort_lib_dir() -> Option<PathBuf> {
    let sdk_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)?;
    let prebuilt = sdk_root.join("target").join("sherpa-onnx-prebuilt");
    let mut hits: Vec<PathBuf> = Vec::new();
    collect_lib_hits(&prebuilt, 0, &mut hits);
    hits.into_iter().next()
}

fn collect_lib_hits(dir: &Path, depth: usize, hits: &mut Vec<PathBuf>) {
    if depth > 4 || !hits.is_empty() {
        return;
    }
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_lib_hits(&path, depth + 1, hits);
        } else if path
            .file_name()
            .is_some_and(|name| name == "onnxruntime.lib")
        {
            if let Some(parent) = path.parent() {
                hits.push(parent.to_path_buf());
            }
        }
        if !hits.is_empty() {
            break;
        }
    }
}

/// Windows 下查询卷剩余空间（字节）；非 Windows 或失败返回 None。
#[cfg(windows)]
fn disk_free_bytes(path: &Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free: u64 = 0;
    let mut _total: u64 = 0;
    let mut _total_free: u64 = 0;
    // SAFETY：缓冲区与指针均为本函数局部有效分配，系统调用只读路径写入计数。
    unsafe {
        if GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut _total, &mut _total_free) != 0 {
            Some(free)
        } else {
            None
        }
    }
}

#[cfg(not(windows))]
fn disk_free_bytes(_path: &Path) -> Option<u64> {
    None
}

fn run_preflight(
    suite: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut blocked = 0usize;
    println!("== product-eval preflight（只报已配置/未配置，绝不输出凭据内容）==");

    // 1) Provider 端点与模型（非敏感，可直接展示）。
    let base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| owo_agent_core::gateway::DEFAULT_MODEL_BASE_URL.to_string());
    let model = std::env::var("OPENAI_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| owo_agent_core::gateway::DEFAULT_MODEL_ID.to_string());
    println!("① Provider URL：已配置（{base_url}）");
    println!("② 模型名：已配置（{model}）");

    // 2) 凭据存在性（只看存在与长度）。
    let key_len = std::env::var("OPENAI_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().count());
    match key_len {
        Some(len) => println!("③ OPENAI_API_KEY：已配置（长度 {len}，内容不展示）"),
        None => {
            println!("③ OPENAI_API_KEY：未配置 → live/agent/workswarm 阻塞（dry 不受影响）");
            blocked += 1;
        }
    }

    // 3) suite 与任务定义。
    match resolve_suite_input(suite.as_deref()) {
        Ok(suite_path) => match load_suite(&suite_path) {
            Ok(bundle) => {
                let validation = owo_agent_core::product_eval::validate_suite(&bundle);
                println!(
                    "④ 套件：已加载（{}，{} 任务，校验 {}）",
                    bundle.suite.name,
                    bundle.cases.len(),
                    if validation.all_ok {
                        "全部通过"
                    } else {
                        "存在问题"
                    }
                );
                if !validation.all_ok {
                    blocked += 1;
                }
            }
            Err(error) => {
                println!("④ 套件：加载失败（{error}）");
                blocked += 1;
            }
        },
        Err(error) => {
            println!("④ 套件：定位失败（{error}）");
            blocked += 1;
        }
    }

    // 4) 输出目录可写性 + 磁盘剩余空间。
    let out_dir = out.unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .map(|sdk| sdk.join("scratch-eval-runs").join("product-eval"))
            .unwrap_or_else(|| std::env::temp_dir().join("owo-product-eval"))
    });
    match std::fs::create_dir_all(&out_dir) {
        Ok(()) => {
            let probe = out_dir.join(".preflight-probe");
            let writable = std::fs::write(&probe, b"ok").is_ok();
            let _ = std::fs::remove_file(&probe);
            if writable {
                let free = disk_free_bytes(&out_dir);
                match free {
                    Some(bytes) => println!(
                        "⑤ 输出目录：可写（{}）磁盘剩余 {:.1} GB",
                        out_dir.display(),
                        bytes as f64 / 1024.0 / 1024.0 / 1024.0
                    ),
                    None => println!(
                        "⑤ 输出目录：可写（{}）磁盘剩余空间：查询不可用",
                        out_dir.display()
                    ),
                }
            } else {
                println!("⑤ 输出目录：不可写（{}）", out_dir.display());
                blocked += 1;
            }
        }
        Err(error) => {
            println!("⑤ 输出目录：创建失败（{}）：{error}", out_dir.display());
            blocked += 1;
        }
    }

    // 5) ORT 链接环境（Rust 测试在 ort crate 静态链接时需要 ORT_LIB_PATH）。
    match std::env::var("ORT_LIB_PATH") {
        Ok(value) if !value.trim().is_empty() => {
            let exists = Path::new(value.trim()).join("onnxruntime.lib").exists();
            println!(
                "⑥ ORT_LIB_PATH：已配置（{}）",
                if exists { "目录含 onnxruntime.lib" } else { "⚠️ 目录中未见 onnxruntime.lib" }
            );
        }
        _ => match probe_ort_lib_dir() {
            Some(found) => println!(
                "⑥ ORT_LIB_PATH：未设置（仓库缓存自动探测可用：{}；跑 check-v1-r0.ps1 会自动注入当前进程）",
                found.display()
            ),
            None => {
                println!("⑥ ORT_LIB_PATH：未设置且仓库缓存中未找到 onnxruntime.lib → Rust 测试链接可能失败");
                blocked += 1;
            }
        },
    }

    if blocked == 0 {
        println!("preflight：全部就绪（exit 0）");
        Ok(())
    } else {
        println!("preflight：{blocked} 项阻塞（exit 2）——live 基线在阻塞解除前不会执行，也不会用 reference 结果代替");
        std::process::exit(2);
    }
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
    selection: owo_agent_core::team_strategy::TeamSelectionMode,
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
    if !matches!(exec.as_str(), "dry" | "live" | "agent" | "workswarm") {
        return Err(format!("未知执行模式「{exec}」（可选 dry|live|agent|workswarm）").into());
    }

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
    println!("输出目录：{}（{}）", out_dir.display(), exec);

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
    } else if exec == "workswarm" {
        let (provider, resolved_model) = build_live_provider(model.as_deref())?;
        // 每个单元格独立 TeamRun 工作目录（CAS/sqlite/状态互不串扰）。
        let work_root = out_dir.join("workswarm-teams");
        let mut ws_executor = WorkSwarmExecutor::new(provider, resolved_model.clone(), work_root);
        ws_executor.config.selection = selection;
        println!(
            "组队模式：{}（auto=自适应判定：简单任务单 Agent，多来源合并/评审才组队）",
            selection.as_str()
        );
        (
            Arc::new(ws_executor),
            "live-workswarm".to_string(),
            Some(resolved_model),
        )
    } else {
        (
            Arc::new(ReferenceDryExecutor),
            "dry-reference".to_string(),
            Some(crate::resolve_model(model, None)),
        )
    };

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
    // R1 live 基线统计：95% Wilson 置信区间 / p50-p95 / 单多对照与启用条件。
    print!("{}", format_mode_statistics(&report.runs));
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
