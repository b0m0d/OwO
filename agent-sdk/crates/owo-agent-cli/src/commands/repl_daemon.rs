//! P1 §4.1：Daemon 后端交互 REPL（`owo-agent repl --daemon`）。
//!
//! 与旧 `Repl` 的根本区别：本模块**不**打开 SQLite、不构造 Agent、不连 MCP——
//! 会话/回合/权限/工具/审计全部由 Daemon 持有，CLI 只消费事件流。这是"唯一运行时"
//! 在交互路径上的第一步；`--local` 保留旧实现用于命令迁移期的功能对照。
//!
//! 已迁移命令：help/exit/new/sessions/resume/model/diff/undo/revert/rewind/redo/fork/
//! rename/archive/pin/abort/status/audit/skills/permissions/settings/traces/plan/build/clear。
//! 其余命令在 daemon 模式下给出明确提示（`--local` 使用旧 REPL）。

use crate::support::{ensure_daemon_client, ensure_data_root};
use crate::ui_output::{parse_approval_response, print_sse_event_human};
use colored::Colorize;
use owo_agent_protocol::{PermissionResponse, SseEvent};
use rustyline::error::ReadlineError;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const GRANT_REVOKE_PATH: &str = "/permissions/grants/revoke";

pub(crate) async fn run(args: super::repl::ReplArgs) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = args.workspace.canonicalize()?;
    let root = ensure_data_root(args.data_dir.clone(), &workspace);
    let client = ensure_daemon_client(&root, &workspace).await?;
    let mut repl = DaemonRepl {
        client,
        workspace,
        session: None,
        model: args.model.clone(),
        read_only: args.agent == "plan",
        no_approval: args.no_approval,
        abort: Arc::new(AtomicBool::new(false)),
    };
    println!(
        "{} {}（daemon 模式 · {}）",
        "OwO Agent".bold(),
        env!("CARGO_PKG_VERSION").cyan(),
        repl.workspace.display()
    );
    println!("输入 /help 查看命令；直接输入文字开始任务。");
    if std::io::stdin().is_terminal() {
        repl.run_terminal().await?;
    } else {
        repl.run_piped().await?;
    }
    Ok(())
}

struct DaemonRepl {
    client: owo_agent_client::AgentClient,
    workspace: PathBuf,
    session: Option<String>,
    model: Option<String>,
    read_only: bool,
    no_approval: bool,
    abort: Arc<AtomicBool>,
}

impl DaemonRepl {
    fn prompt(&self) -> String {
        if self.read_only {
            format!("{} ", "plan ❯".yellow())
        } else {
            format!("{} ", "build ❯".green())
        }
    }

    async fn run_terminal(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut editor = rustyline::DefaultEditor::new()?;
        loop {
            match editor.readline(&self.prompt()) {
                Ok(line) => {
                    let line = crate::support::normalize_input_line(&line);
                    if line.is_empty() {
                        continue;
                    }
                    let _ = editor.add_history_entry(line.clone());
                    match self.handle_line(&line).await {
                        Ok(true) => break,
                        Ok(false) => continue,
                        Err(error) => eprintln!("错误：{error}"),
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    println!("{}", "（/exit 退出；已取消当前输入）".dimmed());
                    continue;
                }
                Err(ReadlineError::Eof) => break,
                Err(error) => {
                    eprintln!("输入错误：{error}");
                    break;
                }
            }
        }
        Ok(())
    }

    async fn run_piped(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        let mut line = String::new();
        loop {
            print!("{}", self.prompt());
            let _ = std::io::stdout().flush();
            line.clear();
            if std::io::stdin().read_line(&mut line)? == 0 {
                break;
            }
            let line = crate::support::normalize_input_line(&line);
            if line.is_empty() {
                continue;
            }
            match self.handle_line(&line).await {
                Ok(true) => break,
                Ok(false) => continue,
                Err(error) => eprintln!("错误：{error}"),
            }
        }
        Ok(())
    }

    async fn handle_line(&mut self, line: &str) -> Result<bool, Box<dyn std::error::Error>> {
        if line.starts_with("@explore ") || line.starts_with("@subagent ") {
            println!(
                "{}",
                "子代理命令尚未迁移到 daemon 模式（用 --local）".yellow()
            );
            return Ok(false);
        }
        let Some(command) = line.strip_prefix('/') else {
            self.run_turn(line).await?;
            return Ok(false);
        };
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "help" => print_help(),
            "exit" | "quit" => return Ok(true),
            "new" => self.new_session(parts.next()).await?,
            "sessions" => self.list_sessions().await?,
            "resume" => {
                let id = parts
                    .next()
                    .ok_or("用法：/resume <会话ID>（/sessions 查看）")?;
                self.resume(id).await?;
            }
            "model" => match parts.next() {
                Some(model) => {
                    self.model = Some(model.to_string());
                    if let Some(id) = self.session.clone() {
                        self.client.session_set_model(&id, Some(model)).await?;
                    }
                    println!("{} {}", "模型已切换：".green(), model);
                }
                None => println!("当前模型：{}", self.model.as_deref().unwrap_or("（默认）")),
            },
            "plan" => {
                self.read_only = true;
                println!("{}", "已切换到 plan 模式（只读）".yellow());
            }
            "build" => {
                self.read_only = false;
                println!("{}", "已切换到 build 模式".green());
            }
            "diff" => self.show_diff().await?,
            "undo" | "revert" => self.revert().await?,
            "rewind" => {
                let keep = parts.next().ok_or("用法：/rewind <保留消息数>")?;
                self.rewind(keep).await?;
            }
            "redo" => self.redo().await?,
            "fork" => self.fork(parts.next()).await?,
            "rename" => self.rename(parts.next()).await?,
            "archive" => self.archive(true).await?,
            "pin" => self.pin(true).await?,
            "abort" => {
                self.abort.store(true, Ordering::Relaxed);
                if let Some(id) = self.session.clone() {
                    let _ = self.client.cancel_turn(&id).await;
                }
                println!("已请求中止当前回合");
            }
            "status" => self.show_status(),
            "audit" => self.print_json("审计", "/audit").await,
            "skills" => match parts.next() {
                Some("health") => self.print_json("技能健康", "/skills/health").await,
                Some("reload") => println!(
                    "{}",
                    "技能热重载由 Daemon 持有；请重启 Daemon（--local 为旧行为）".yellow()
                ),
                _ => self.print_json("技能", "/skills").await,
            },
            "permissions" => match parts.next() {
                None | Some("overview") => {
                    self.print_json("权限", "/permissions/overview").await
                }
                Some("revoke") => {
                    let grant_id = parts.next().ok_or("用法：/permissions revoke <授权ID>")?;
                    self.revoke_permission_grant(grant_id).await?;
                }
                Some(other) => println!(
                    "{}",
                    format!("未知权限子命令：{other}（用法：/permissions [overview] 或 /permissions revoke <授权ID>）").yellow()
                ),
            },
            "settings" => self.print_json("设置", "/settings").await,
            "traces" => self.print_json("轨迹", "/traces").await,
            "trace" => {
                let index = parts.next().ok_or("用法：/trace <序号>")?;
                self.print_json("轨迹", &format!("/traces/{index}")).await
            }
            "tree" => {
                let id = self.current_session().await?;
                self.print_json("会话树", &format!("/session/{id}/children"))
                    .await
            }
            "mcp" => match parts.next() {
                Some(sub) => println!(
                    "{}",
                    format!("/mcp {sub} 需 POST（迁移中）；/mcp 列出用 /mcp").yellow()
                ),
                None => self.print_json("MCP", "/mcp").await,
            },
            "plugins" => self.print_json("插件", "/plugins").await,
            "whitelist" => self.print_json("白名单", "/whitelist").await,
            "perception" => self.print_json("情景", "/perception/events").await,
            "learn" => match parts.next().unwrap_or("status") {
                "status" => self.print_json("学习", "/learn/status").await,
                other => println!(
                    "{}",
                    format!("/learn {other} 尚未迁移到 daemon 模式（用 --local）").yellow()
                ),
            },
            "capabilities" => self.print_json("能力", "/capabilities").await,
            "proactive" => println!(
                "{}",
                "/proactive 尚未迁移到 daemon 模式（用 --local）".yellow()
            ),
            "share" | "export" => {
                println!("{}", "/share 尚未迁移到 daemon 模式（用 --local）".yellow())
            }
            "init" => {
                let target = self.workspace.join("AGENTS.md");
                if target.exists() {
                    println!("{} {}", "AGENTS.md 已存在：".yellow(), target.display());
                } else {
                    std::fs::write(&target, crate::support::AGENTS_TEMPLATE)?;
                    println!("{} {}", "已生成".green(), target.display());
                }
            }
            "clear" => print!("\x1b[2J\x1b[1;1H"),
            other => println!(
                "{}",
                format!("命令 /{other} 尚未迁移到 daemon 模式（用 --local 使用旧 REPL）").yellow()
            ),
        }
        Ok(false)
    }

    async fn new_session(&mut self, model: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let model = model.map(str::to_string).or_else(|| self.model.clone());
        let workspace = self.workspace.to_string_lossy().to_string();
        let session = self
            .client
            .create_session_with_model(&workspace, model)
            .await?;
        println!("{} {}", "新会话：".green(), session.id);
        self.session = Some(session.id);
        Ok(())
    }

    async fn list_sessions(&self) -> Result<(), Box<dyn std::error::Error>> {
        let sessions = self.client.list_sessions().await?;
        if sessions.is_empty() {
            println!("（无会话）");
        }
        for session in sessions {
            let title = session.title.unwrap_or_default();
            println!(
                "  {}{}  {}  {}",
                session.id,
                if session.pinned { " 📌" } else { "" },
                session.model,
                title
            );
        }
        Ok(())
    }

    async fn resume(&mut self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let session = self.client.get_session(id).await?;
        println!("{} {}", "已恢复会话：".green(), session.id);
        self.session = Some(session.id);
        Ok(())
    }

    async fn current_session(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        if self.session.is_none() {
            self.new_session(None).await?;
        }
        Ok(self.session.clone().expect("session just created"))
    }

    async fn show_diff(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        let diffs = self.client.session_diff(&id).await?;
        if diffs.is_empty() {
            println!("（无改动）");
        }
        for diff in diffs {
            println!("  M {}", diff.path);
        }
        Ok(())
    }

    async fn revert(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        self.client.session_revert(&id).await?;
        println!("{}", "已回滚本次会话全部写操作".green());
        Ok(())
    }

    async fn rewind(&mut self, keep: &str) -> Result<(), Box<dyn std::error::Error>> {
        let keep: usize = keep.parse().map_err(|_| "保留消息数必须是整数")?;
        let id = self.current_session().await?;
        self.client.session_rewind(&id, keep).await?;
        println!("已回退到保留 {keep} 条消息");
        Ok(())
    }

    async fn redo(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        self.client.session_redo(&id).await?;
        println!("已恢复最近一次 rewind");
        Ok(())
    }

    async fn fork(&mut self, index: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let index: usize = index
            .unwrap_or("0")
            .parse()
            .map_err(|_| "消息序号必须是整数")?;
        let id = self.current_session().await?;
        let child = self.client.session_fork(&id, index).await?;
        println!("{} {}", "子会话：".green(), child.id);
        self.session = Some(child.id);
        Ok(())
    }

    async fn rename(&mut self, title: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let title = title.ok_or("用法：/rename <标题>")?;
        let id = self.current_session().await?;
        self.client.session_rename(&id, title).await?;
        println!("已重命名会话");
        Ok(())
    }

    async fn archive(&mut self, archived: bool) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        self.client.session_archive(&id, archived).await?;
        println!("会话已归档");
        Ok(())
    }

    async fn pin(&mut self, pinned: bool) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        self.client.session_pin(&id, pinned).await?;
        println!("会话已置顶");
        Ok(())
    }

    async fn print_json(&self, label: &str, path: &str) {
        match self.client.get_json::<serde_json::Value>(path).await {
            Ok(value) => println!(
                "[{label}] {}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            ),
            Err(error) => println!("[{label}] 读取失败：{error}"),
        }
    }

    async fn revoke_permission_grant(
        &self,
        grant_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let result: serde_json::Value = self
            .client
            .post_json(GRANT_REVOKE_PATH, &grant_revoke_payload(grant_id))
            .await?;
        println!("{}", format!("已撤销授权：{result}").green());
        self.print_json("权限", "/permissions/overview").await;
        Ok(())
    }

    fn show_status(&self) {
        println!("工作区：{}", self.workspace.display());
        println!("模型：{}", self.model.as_deref().unwrap_or("（默认）"));
        println!(
            "会话：{}",
            self.session.as_deref().unwrap_or("（尚未创建）")
        );
        println!(
            "模式：{}",
            if self.read_only {
                "plan（只读）"
            } else {
                "build"
            }
        );
    }

    async fn run_turn(&mut self, prompt: &str) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        self.abort.store(false, Ordering::Relaxed);
        let mut stream = self.client.open_turn(&id, prompt).await?;

        let cancel_client = self.client.clone();
        let cancel_id = id.clone();
        let abort_flag = Arc::clone(&self.abort);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                abort_flag.store(true, Ordering::Relaxed);
                let _ = cancel_client.cancel_turn(&cancel_id).await;
            }
        });

        println!("{} {}", "▶".green(), prompt.dimmed());
        let mut streamed = false;
        let mut steps = 0usize;
        while let Some(event) = stream.next_event().await {
            let event = event?;
            match &event {
                SseEvent::TokenDelta { delta } => {
                    use std::io::Write;
                    streamed = true;
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                SseEvent::Final { .. } => {}
                SseEvent::PermissionRequest {
                    request_id,
                    tool,
                    reason,
                    level,
                    ..
                } => {
                    let response = self.decide_permission(tool, reason, level.as_deref())?;
                    let _ = self
                        .client
                        .respond_permission(&id, request_id, &response)
                        .await;
                }
                other => {
                    if streamed {
                        println!();
                        streamed = false;
                    }
                    if matches!(other, SseEvent::ToolResult { .. }) {
                        steps += 1;
                    }
                    print_sse_event_human(other);
                }
            }
        }
        if streamed {
            println!();
        }
        let diff_count = self
            .client
            .session_diff(&id)
            .await
            .map(|d| d.len())
            .unwrap_or(0);
        println!(
            "{} 工具步数 {}，改动 {} 个文件（/diff 查看，/undo 回滚）",
            "✓".green(),
            steps,
            diff_count
        );
        Ok(())
    }

    fn decide_permission(
        &self,
        tool: &str,
        reason: &str,
        level: Option<&str>,
    ) -> Result<PermissionResponse, Box<dyn std::error::Error>> {
        if self.no_approval {
            return Ok(PermissionResponse {
                allow: true,
                remember: None,
                scope: Some("once".to_string()),
            });
        }
        eprintln!(
            "{} 需要 {} 权限：{tool}（{reason}）",
            "审批".yellow(),
            level.unwrap_or("unknown")
        );
        eprint!("允许？[y=仅本次 / t=本任务 / w=工作区长期 / n=拒绝] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(parse_approval_response(&line))
    }
}

fn grant_revoke_payload(grant_id: &str) -> serde_json::Value {
    serde_json::json!({ "grant_id": grant_id })
}

fn print_help() {
    println!("{}", "── daemon 模式命令 ──".bold());
    println!("  直接输入文字       向当前会话发起任务");
    println!("  /new [模型]        新建会话");
    println!("  /sessions          列出会话");
    println!("  /resume <id>       恢复会话");
    println!("  /model [名称]      查看/切换模型");
    println!("  /plan | /build     切换只读 / 执行模式");
    println!("  /diff              查看本次会话文件改动");
    println!("  /undo | /revert    回滚本次会话全部写操作");
    println!("  /rewind <n>        回退会话历史到保留 n 条消息");
    println!("  /redo              恢复最近一次 rewind");
    println!("  /fork [序号]       在指定消息处创建子会话");
    println!("  /rename <标题>     重命名当前会话");
    println!("  /archive | /pin    归档 / 置顶当前会话");
    println!("  /abort             中止当前回合");
    println!("  /status            查看工作区/模型/会话状态");
    println!("  /permissions [overview] 查看审批与授权；/permissions revoke <授权ID> 撤销单条授权");
    println!("  /audit /skills /settings /traces  读取服务端状态");
    println!("  /clear             清屏");
    println!("  /exit | /quit      退出");
    println!("  其它命令请用 --local 使用旧 REPL（迁移中）");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_grant_revoke_is_limited_to_one_exact_grant() {
        assert_eq!(GRANT_REVOKE_PATH, "/permissions/grants/revoke");
        assert_eq!(
            grant_revoke_payload("grant-123"),
            serde_json::json!({ "grant_id": "grant-123" })
        );
    }
}
