// §12.3 CLI 拆分批次四：turn 子命令域 + 事件输出面（自 main.rs 机械外移，零行为变化）。

use crate::support::*;
use crate::ui_output::{OutputMode, PermissionsProfile, UiSink};
use clap::Args;
use colored::Colorize;
use owo_agent_core::permissions::{Approver, AutoApprover};
use owo_agent_core::session::SessionStore;
use owo_agent_core::TurnEvent;
use owo_agent_core::{save_trace, Session, Settings, SqliteSessionStore, TraceRecord};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Args)]
pub(crate) struct TurnArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[arg(long)]
    prompt: String,
    #[arg(long)]
    model: Option<String>,
    /// 自动允许所有审批（仅测试用）
    #[arg(long)]
    no_approval: bool,
}

pub(crate) async fn run_turn(
    args: TurnArgs,
    output: OutputMode,
    permissions: PermissionsProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    let sink = UiSink::new(output);
    let workspace = args.workspace.canonicalize()?;
    let settings = Settings::load(&workspace);
    apply_egress_setting(&settings);
    settings.apply_usage_env();
    let model = resolve_model(args.model, settings.model.as_deref());
    // §11：--permissions read-only → 强制只读策略。
    let read_only = matches!(permissions, PermissionsProfile::ReadOnly);
    let agent = build_agent(&workspace, &model, read_only)?;
    let mut session = Session::new(workspace.clone(), model, None);
    let abort = Arc::new(AtomicBool::new(false));
    let abort_flag = Arc::clone(&abort);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            abort_flag.store(true, Ordering::Relaxed);
        }
    });

    // §11：--no-approval 标记 deprecated；兼容期显式映射到跳过审批并显示高风险提示。
    if args.no_approval {
        eprintln!(
            "⚠ 已弃用：--no-approval 将在未来版本移除；兼容期等价 --permissions trusted（高风险：全部操作自动批准，含写/执行/联网）"
        );
    }
    // §11：--permissions trusted → 跳过审批（等价 --no-approval）。
    let approver: Arc<dyn Approver> =
        if args.no_approval || matches!(permissions, PermissionsProfile::Trusted) {
            Arc::new(AutoApprover { allow: true })
        } else {
            Arc::new(ConsoleApprover {
                stdin: SharedStdin::new(),
            })
        };

    let mut printer = EventPrinter::new();
    let mut on_event = |event: &TurnEvent| match output {
        OutputMode::Human => printer.print(event),
        OutputMode::Plain | OutputMode::Jsonl => sink.emit_turn_event(event),
    };
    let outcome = agent
        .run_turn(
            &mut session,
            &args.prompt,
            approver.as_ref(),
            &abort,
            &mut on_event,
        )
        .await
        .inspect_err(|error| {
            sink.error(&error.to_string());
        })?;

    let diffs = session.diff();
    let diff_paths: Vec<String> = diffs.iter().map(|diff| diff.path.clone()).collect();
    let root = ensure_data_root(None, &workspace);
    let trace = TraceRecord::from_outcome(&session, &outcome);
    let trace_path = save_trace(&root.join("traces"), &trace)
        .ok()
        .map(|path| display_path(&path));
    let audit_entries = agent
        .audit_log()
        .lock()
        .map(|guard| guard.entries.clone())
        .unwrap_or_default();
    if let Ok(store) = SqliteSessionStore::open(&root.join("index.db")) {
        let _ = store.append_audit(&audit_entries);
    }
    sink.final_result(
        outcome.final_text.as_deref(),
        outcome.steps,
        &diff_paths,
        trace_path.as_deref(),
        audit_entries.len(),
    );
    Ok(())
}

fn print_event(event: &TurnEvent) {
    match event {
        TurnEvent::ModelCall => println!("{}", "  ↻ 调用模型…".cyan()),
        TurnEvent::PermissionRequest(request) => println!(
            "  {} 需要 {} 权限：{}（{}）",
            "审批".yellow(),
            request.level.label(),
            request.tool,
            request.reason
        ),
        TurnEvent::ToolStart { tool, .. } => {
            println!("  {} {tool} …", "▶".blue());
        }
        TurnEvent::ToolResult {
            tool, ok, error, ..
        } => {
            if *ok {
                println!("  {} {tool}", "✔".green());
            } else {
                println!(
                    "  {} {tool}：{}",
                    "✘".red(),
                    error.as_deref().unwrap_or("未知错误")
                );
            }
        }
        TurnEvent::TokenDelta { .. } => {}
        TurnEvent::Compaction { summary } => {
            println!("  {}（上下文已压缩：{}）", "✦".yellow(), summary);
        }
        TurnEvent::Final { .. } => {}
    }
}

/// 事件打印器：把流式增量逐字输出，Final 只收尾不重复打印。
pub(crate) struct EventPrinter {
    streamed: bool,
}

impl EventPrinter {
    pub(crate) fn new() -> Self {
        Self { streamed: false }
    }

    pub(crate) fn print(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::TokenDelta { delta } => {
                use std::io::Write;
                self.streamed = true;
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            TurnEvent::Final { text } => {
                if self.streamed {
                    println!();
                    self.streamed = false;
                } else {
                    println!("\n{}\n{text}", "── 结果 ──".bold());
                }
            }
            other => print_event(other),
        }
    }
}
