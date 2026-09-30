// P1（指南 §7.2/§8 P1）：`turn` 子命令经**唯一 Daemon 客户端**执行。
//
// 与旧实现的根本区别：本文件不再 `Agent::new` / `SqliteSessionStore::open` /
// `connect_mcp_clients`——会话、模型、权限、工具、审计全部由 Daemon 持有，CLI 只是
// 一个事件流消费者。单实例协议由 `support::ensure_daemon_client` 实现（有则复用，
// 无则启动一个分离的 serve 进程）。此约束由 `tests/turn_path_guard_tests.rs` 静态守卫。

use crate::support::*;
use crate::ui_output::{
    parse_approval_response, render_error_jsonl, render_event_jsonl, render_final_result_jsonl,
    render_final_result_plain, OutputMode, PermissionsProfile, StreamPrinter,
};
use clap::Args;
use colored::Colorize;
use owo_agent_protocol::{PermissionResponse, SseEvent};
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
    /// 自动允许所有审批（仅测试用；等价 --permissions trusted）
    #[arg(long)]
    no_approval: bool,
    /// 数据根（缺省用户级数据目录）；用于发现/启动共享 Daemon。
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

pub(crate) async fn run_turn(
    args: TurnArgs,
    output: OutputMode,
    permissions: PermissionsProfile,
) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = args.workspace.canonicalize()?;
    let workspace_str = workspace.to_string_lossy().to_string();
    let root = ensure_data_root(args.data_dir.clone(), &workspace);
    let client = ensure_daemon_client(&root, &workspace).await?;
    let session = client
        .create_session_with_model(&workspace_str, args.model.clone())
        .await?;

    // Ctrl+C → 取消运行中的回合（贯穿网络与 Daemon 侧 abort 标志）。
    let abort = Arc::new(AtomicBool::new(false));
    let cancel_client = client.clone();
    let cancel_session = session.id.clone();
    let abort_flag = Arc::clone(&abort);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            abort_flag.store(true, Ordering::Relaxed);
            let _ = cancel_client.cancel_turn(&cancel_session).await;
        }
    });

    let trusted = args.no_approval || matches!(permissions, PermissionsProfile::Trusted);
    if args.no_approval {
        eprintln!("⚠ 已弃用：--no-approval 将在未来版本移除；兼容期等价 --permissions trusted（高风险：全部操作自动批准）");
    }

    let mut stream = client.open_turn(&session.id, &args.prompt).await?;
    let mut steps = 0usize;
    let mut final_text: Option<String> = None;
    let mut stream_error: Option<String> = None;
    let mut printer = StreamPrinter::new();

    while let Some(event) = stream.next_event().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                stream_error = Some(error.to_string());
                break;
            }
        };
        let human = matches!(output, OutputMode::Human);
        match &event {
            SseEvent::TokenDelta { .. } => {
                if human {
                    printer.print_sse(&event);
                }
            }
            SseEvent::Final { text } => {
                final_text = Some(text.clone());
                if human {
                    printer.print_sse(&event);
                }
                emit_event(output, &event);
            }
            SseEvent::PermissionRequest { request_id, .. } => {
                if human {
                    printer.print_sse(&event);
                }
                emit_event(output, &event);
                let response = decide_permission(output, trusted, &event)?;
                if let Err(error) = client
                    .respond_permission(&session.id, request_id, &response)
                    .await
                {
                    eprintln!("审批响应失败：{error}");
                }
            }
            SseEvent::ToolResult { .. } => {
                steps += 1;
                emit_event(output, &event);
                if human {
                    printer.print_sse(&event);
                }
            }
            _ => {
                emit_event(output, &event);
                if human {
                    printer.print_sse(&event);
                }
            }
        }
    }

    if let Some(error) = stream_error {
        emit_error(output, &error);
        return Err(error.into());
    }

    let diffs = client.session_diff(&session.id).await.unwrap_or_default();
    let diff_paths: Vec<String> = diffs.iter().map(|diff| diff.path.clone()).collect();
    if matches!(output, OutputMode::Human) {
        printer.finish();
    }
    render_final(output, final_text.as_deref(), steps, &diff_paths);
    if abort.load(Ordering::Relaxed) {
        eprintln!("{}", "（回合已被用户取消）".yellow());
    }
    Ok(())
}

/// 非 human 模式的事件协议出口（jsonl 包裹为 `turn_event`；plain 只保留 stderr 提示）。
fn emit_event(output: OutputMode, event: &SseEvent) {
    match output {
        OutputMode::Human => {}
        OutputMode::Plain => {
            if let SseEvent::ToolResult {
                tool, ok, error, ..
            } = event
            {
                if !ok {
                    eprintln!("tool {tool} 失败：{}", error.as_deref().unwrap_or("-"));
                }
            }
        }
        OutputMode::Jsonl => {
            // token_delta 不进 JSONL 协议（保持既有契约：文本由 final 承载）。
            if !matches!(event, SseEvent::TokenDelta { .. }) {
                println!("{}", render_event_jsonl(event));
            }
        }
    }
}

fn emit_error(output: OutputMode, message: &str) {
    match output {
        OutputMode::Jsonl => println!("{}", render_error_jsonl(message)),
        _ => eprintln!("错误：{message}"),
    }
}

fn render_final(output: OutputMode, text: Option<&str>, steps: usize, diff_paths: &[String]) {
    match output {
        OutputMode::Human => {
            println!(
                "\n{} 工具步数：{}，最终文本：{}",
                "[完成]".green(),
                steps,
                text.is_some()
            );
            if !diff_paths.is_empty() {
                println!("[diff] 本次会话改动文件：");
                for path in diff_paths {
                    println!("  - {path}");
                }
            }
        }
        OutputMode::Plain => {
            for line in render_final_result_plain(text, diff_paths) {
                println!("{line}");
            }
        }
        OutputMode::Jsonl => {
            println!(
                "{}",
                render_final_result_jsonl(text, steps, diff_paths, None, 0)
            );
        }
    }
}

/// 审批决策：trusted 自动允许；human 交互选择；jsonl 默认拒绝（避免编排被阻塞）。
fn decide_permission(
    output: OutputMode,
    trusted: bool,
    event: &SseEvent,
) -> Result<PermissionResponse, Box<dyn std::error::Error>> {
    let SseEvent::PermissionRequest {
        tool,
        reason,
        level,
        ..
    } = event
    else {
        return Ok(parse_approval_response("deny"));
    };
    if trusted {
        return Ok(PermissionResponse {
            allow: true,
            remember: None,
            scope: Some("once".to_string()),
        });
    }
    if matches!(output, OutputMode::Jsonl) {
        eprintln!("需要审批（jsonl 非交互，默认拒绝）：{tool}（{reason}）");
        return Ok(parse_approval_response("deny"));
    }
    if matches!(output, OutputMode::Human) {
        // 审批卡已由 StreamPrinter 输出（含参数/风险/影响），这里只给交互提示。
    } else {
        eprintln!(
            "{} 需要 {} 权限：{tool}（{reason}）",
            "审批".yellow(),
            level.as_deref().unwrap_or("unknown")
        );
    }
    eprint!("允许？[y=仅本次 / t=本任务 / w=工作区长期 / n=拒绝] ");
    use std::io::Write;
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(parse_approval_response(&line))
}
