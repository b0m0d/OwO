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
use crate::ui_output::{
    parse_approval_response, print_permission_card, PermissionCard, StreamPrinter,
};
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
        data_root: root,
        session: None,
        model: args.model.clone(),
        read_only: args.agent == "plan",
        no_approval: args.no_approval,
        abort: Arc::new(AtomicBool::new(false)),
        goal: None,
        team_id: None,
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
    data_root: PathBuf,
    session: Option<String>,
    model: Option<String>,
    read_only: bool,
    no_approval: bool,
    abort: Arc<AtomicBool>,
    goal: Option<crate::support::GoalState>,
    /// 当前团队（`/team` 目标；status/steer/diff 缺省作用于它）。
    team_id: Option<String>,
}

impl DaemonRepl {
    fn prompt(&self) -> String {
        // 纯文本提示串：rustyline 的宽度计算不剥离 ANSI，颜色由 ReplHelper::highlight_prompt 渲染。
        crate::support::repl_prompt(self.read_only)
    }

    async fn run_terminal(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut editor = crate::support::new_repl_editor()?;
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
            let prompt = self.with_goal_context(line);
            self.run_turn(&prompt).await?;
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
                Some("set") => {
                    let profile = parts.next().ok_or(
                        "用法：/permissions set <read_only|workspace|auto_review|full_access|unrestricted|custom> [--yes]",
                    )?;
                    let yes = parts.any(|part| part == "--yes");
                    self.set_permission_profile(profile, yes).await?;
                }
                Some("status") => self.print_json("权限", "/permissions").await,
                Some("revoke") => {
                    let grant_id = parts.next().ok_or("用法：/permissions revoke <授权ID>")?;
                    self.revoke_permission_grant(grant_id).await?;
                }
                Some(other) => println!(
                    "{}",
                    format!("未知权限子命令：{other}（用法：/permissions [overview|status] | set <档位> | revoke <授权ID>）").yellow()
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
            "compact" => self.compact().await?,
            "review" => self.review(parts.next()).await?,
            "mention" => crate::support::mention_path(&self.workspace, parts.next()),
            "history" => crate::support::print_history(&self.data_root, parts.next()),
            "editor" => self.editor_input().await?,
            "login" => crate::support::print_login(),
            "logout" => crate::support::print_logout(),
            "debug" => self.show_debug(),
            "goal" => {
                let rest = command.strip_prefix("goal").unwrap_or("").trim();
                self.handle_goal(rest).await?;
            }
            "team" => self.handle_team(parts).await?,
            "todo" => self.show_todos().await?,
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

    /// `/permissions set <profile>`：切换权限档位（高风险档位需确认）。
    async fn set_permission_profile(
        &self,
        profile: &str,
        allow_yes_flag: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !crate::support::confirm_high_risk_profile(profile, allow_yes_flag) {
            println!(
                "{}",
                "已取消（管道模式切换 unrestricted 需显式追加 --yes）".yellow()
            );
            return Ok(());
        }
        let result: serde_json::Value = self
            .client
            .post_json("/permissions", &serde_json::json!({ "profile": profile }))
            .await?;
        let applied = result
            .get("profile")
            .and_then(|value| value.as_str())
            .unwrap_or(profile);
        println!("{} 已切换权限档位：{applied}", "✓".green());
        Ok(())
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

    /// `/compact`：显示上下文占用并触发服务端压缩（对齐 Codex）。
    async fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        if let Ok(value) = self
            .client
            .get_json::<serde_json::Value>(&format!("/session/{id}/context"))
            .await
        {
            let tokens = value
                .get("estimated_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let budget = value
                .get("token_budget")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let over = value
                .get("over_budget")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let suffix = if over {
                "（超预算）".yellow().to_string()
            } else {
                String::new()
            };
            println!("上下文：{tokens} / {budget} tokens{suffix}");
        }
        let result = self
            .client
            .post_empty::<serde_json::Value>(&format!("/session/{id}/compact"))
            .await?;
        let compacted = result
            .get("compacted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if compacted {
            let before = result
                .get("tokens_before")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let after = result
                .get("tokens_after")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            println!("{} 已压缩：{before} → {after} tokens", "✓".green());
            if let Some(summary) = result.get("summary").and_then(|v| v.as_str()) {
                println!("{}", "── 摘要 ──".bold());
                for line in summary.lines() {
                    println!("{}", crate::markdown::render_block_line(line));
                }
            }
        } else {
            println!("{}", "未压缩（历史不足或模型未产出摘要）".yellow());
        }
        Ok(())
    }

    /// `/review [提示]`：发起一次「审查当前改动」的回合。
    async fn review(&mut self, extra: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let prompt = match extra {
            Some(extra) => format!(
                "请审查工作区当前改动（git diff），指出缺陷、风险与改进建议。额外关注：{extra}"
            ),
            None => "请审查工作区当前改动（git diff），指出缺陷、风险与改进建议。".to_string(),
        };
        self.run_turn(&prompt).await
    }

    /// `/editor`：用 `$VISUAL`/`$EDITOR` 编辑多行提示后作为一次输入。
    async fn editor_input(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let editor = std::env::var("VISUAL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("EDITOR")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            });
        let Some(editor) = editor else {
            println!(
                "{}",
                "未设置 $VISUAL/$EDITOR；请先设置外部编辑器（如 set EDITOR=notepad）".yellow()
            );
            return Ok(());
        };
        let path = std::env::temp_dir().join(format!("owo-prompt-{}.md", std::process::id()));
        std::fs::write(&path, "")?;
        let mut cmd = editor.split_whitespace();
        let program = cmd.next().unwrap_or_default().to_string();
        let status = std::process::Command::new(&program)
            .args(cmd)
            .arg(&path)
            .status();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        match status {
            Ok(status) if status.success() && !text.trim().is_empty() => {
                self.run_turn(text.trim()).await?;
            }
            Ok(_) => println!("（空输入，已取消）"),
            Err(error) => println!("{} 启动编辑器失败：{error}", "✘".red()),
        }
        Ok(())
    }

    /// `/debug`：诊断信息。
    fn show_debug(&self) {
        println!("[debug] 版本：{}", env!("CARGO_PKG_VERSION"));
        println!("[debug] 工作区：{}", self.workspace.display());
        println!("[debug] 数据目录：{}", self.data_root.display());
        println!(
            "[debug] 模式：{}",
            if self.read_only {
                "plan（只读）"
            } else {
                "build"
            }
        );
        println!(
            "[debug] 模型：{}",
            self.model.as_deref().unwrap_or("（默认）")
        );
        println!(
            "[debug] 会话：{}",
            self.session.as_deref().unwrap_or("（无）")
        );
        println!("[debug] 后端：daemon {}", self.client.base_url());
        println!(
            "[debug] trace 目录：{}",
            self.data_root.join("traces").display()
        );
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
        self.run_turn_capture(prompt).await.map(|_| ())
    }

    /// 执行一回合并返回最终文本（`/goal` 依赖它判断完成标记）。
    async fn run_turn_capture(
        &mut self,
        prompt: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        self.abort.store(false, Ordering::Relaxed);
        let mut stream = self.client.open_turn(&id, prompt).await?;

        // Ctrl+C 监听随回合结束而退出（旧实现每回合 spawn 一个永不结束的任务：
        // `/goal` 多轮会累积监听器，回合结束后按 Ctrl+C 还会误置 abort）。
        let turn_done = Arc::new(tokio::sync::Notify::new());
        let cancel_client = self.client.clone();
        let cancel_id = id.clone();
        let abort_flag = Arc::clone(&self.abort);
        let done = Arc::clone(&turn_done);
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    abort_flag.store(true, Ordering::Relaxed);
                    let _ = cancel_client.cancel_turn(&cancel_id).await;
                }
                _ = done.notified() => {}
            }
        });

        println!("{} {}", "▶".green(), prompt.dimmed());
        let mut steps = 0usize;
        let mut final_text: Option<String> = None;
        let mut printer = StreamPrinter::new();
        while let Some(event) = stream.next_event().await {
            let event = event?;
            match &event {
                SseEvent::PermissionRequest { request_id, .. } => {
                    printer.print_sse(&event);
                    let response = self.decide_permission(&event)?;
                    let _ = self
                        .client
                        .respond_permission(&id, request_id, &response)
                        .await;
                }
                SseEvent::Final { text } => {
                    final_text = Some(text.clone());
                    printer.print_sse(&event);
                }
                other => {
                    if matches!(other, SseEvent::ToolResult { .. }) {
                        steps += 1;
                    }
                    printer.print_sse(other);
                }
            }
        }
        printer.finish();
        turn_done.notify_waiters();
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
        Ok(final_text)
    }

    /// 目标激活且未完成时，把目标附到每次输入前（目标推进期间用户插话也带目标上下文）。
    fn with_goal_context(&self, line: &str) -> String {
        crate::support::goal_context_prompt(self.goal.as_ref(), line)
    }

    /// `/todo`：查看会话任务清单（`todo` 工具维护）。
    async fn show_todos(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let id = self.current_session().await?;
        let session = self
            .client
            .get_json::<serde_json::Value>(&format!("/session/{id}"))
            .await?;
        let todos = session
            .get("todos")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        if todos.is_empty() {
            println!("（任务清单为空；模型调用 todo 工具后会出现在这里）");
            return Ok(());
        }
        println!("{}", "任务清单：".bold());
        for todo in todos {
            let content = todo
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let status = todo
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("pending");
            let mark = match status {
                "completed" => "✔".green().to_string(),
                "in_progress" => "▶".yellow().to_string(),
                _ => "○".dimmed().to_string(),
            };
            println!("  {mark} {content}");
        }
        Ok(())
    }

    /// `/goal [目标|status|clear]`：目标模式——未完成时持续自动推进，模型标记完成才停。
    async fn handle_goal(&mut self, arg: &str) -> Result<(), Box<dyn std::error::Error>> {
        use crate::support::{
            goal_continue_prompt, goal_first_prompt, goal_max_iterations, GoalState,
        };
        match arg {
            "" | "status" => {
                match &self.goal {
                    Some(goal) => println!(
                        "目标：{}\n  轮次：{}/{}  状态：{}",
                        goal.objective,
                        goal.iterations,
                        goal_max_iterations(),
                        if goal.done { "已完成" } else { "推进中" }
                    ),
                    None => println!("（未设定目标；用法：/goal <目标描述>）"),
                }
                Ok(())
            }
            "clear" | "stop" => {
                self.goal = None;
                println!("{}", "已清除目标".green());
                Ok(())
            }
            objective => {
                let max = goal_max_iterations();
                self.goal = Some(GoalState {
                    objective: objective.to_string(),
                    iterations: 0,
                    done: false,
                });
                println!(
                    "{}（最多 {max} 轮；/goal clear 停止）",
                    format!("目标已设定：{objective}").green()
                );
                loop {
                    if self.abort.load(Ordering::Relaxed) {
                        println!("{}", "（目标推进已中止）".yellow());
                        break;
                    }
                    let Some(goal) = self.goal.as_ref() else {
                        break;
                    };
                    if goal.done {
                        break;
                    }
                    if goal.iterations >= max {
                        println!(
                            "{}",
                            format!("已达最大迭代 {max}，目标未标记完成（/goal status 查看）")
                                .yellow()
                        );
                        break;
                    }
                    let iteration = goal.iterations + 1;
                    let prompt = if iteration == 1 {
                        goal_first_prompt(objective)
                    } else {
                        goal_continue_prompt(objective, iteration)
                    };
                    println!("{}", format!("── 目标推进 {iteration}/{max} ──").bold());
                    let final_text = self.run_turn_capture(&prompt).await?;
                    if let Some(goal) = self.goal.as_mut() {
                        goal.iterations = iteration;
                    }
                    if let Some(text) = final_text {
                        if text.contains(crate::support::GOAL_DONE_MARKER) {
                            if let Some(goal) = self.goal.as_mut() {
                                goal.done = true;
                            }
                            println!("{} 目标完成（第 {iteration} 轮）", "✓".green());
                            break;
                        }
                    }
                }
                Ok(())
            }
        }
    }

    /// `/team ...`：WorkSwarm 多 agent 团队——创建即后台跑，CLI 实时跟踪进度，
    /// 可查看任务图 / 干预（steer/retry/cancel）/ 查看真实变更集（diff）。
    async fn handle_team(
        &mut self,
        parts: std::str::SplitWhitespace<'_>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        const USAGE: &str = "用法：/team [--single|--team|--auto] [--parallel <2..=8> | --role <名>[:依赖1|依赖2]] [--model <模型>] [--model <角色>=<模型>] [--write <角色>=<路径>[;<路径>]] <目标>（缺省自动并行开发；--auto 使用策略选择）| list | use <团队ID> | status | watch | context [publish <key> <内容>] | steer <说明> | retry <步骤ID> | cancel | diff";
        // 前置参数解析（纯函数 `parse_team_args`，便于单测）。声明了自定义角色时
        // 策略缺省强制 team（避免 auto 判定裁剪显式编排）。
        let raw: Vec<String> = parts.map(str::to_string).collect();
        let parsed =
            parse_team_args(&raw).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        let TeamArgs {
            strategy: parsed_strategy,
            roles: parsed_roles,
            rest,
            model: parsed_model,
            parallel,
        } = parsed;
        let (strategy, automatic_parallel) =
            resolve_team_create_intent(parsed_strategy, &parsed_roles, parallel);
        let mut parts = rest.into_iter();
        match parts.next().as_deref() {
            None => println!("{}", USAGE.dimmed()),
            Some("list") => self.team_list().await?,
            Some("use") => {
                let id = parts
                    .next()
                    .ok_or("用法：/team use <团队ID>（/team list 查看）")?;
                self.team_id = Some(id.to_string());
                println!("{} {}", "当前团队：".green(), id);
            }
            Some("status") => {
                let id = self.team_required()?;
                self.team_status(&id).await?;
            }
            Some("watch") => {
                let id = self.team_required()?;
                self.team_watch(&id).await?;
            }
            Some("context") => {
                let id = self.team_required()?;
                match parts.next() {
                    Some(command) if command == "publish" => {
                        let key = parts
                            .next()
                            .ok_or("用法：/team context publish <key> <内容>")?;
                        let value = parts.collect::<Vec<_>>().join(" ");
                        self.team_context_publish(&id, &key, &value).await?;
                    }
                    None => self.team_context(&id).await?,
                    Some(other) => {
                        return Err(format!("未知 context 子命令：{other}（支持 publish）").into())
                    }
                }
            }
            Some("steer") => {
                let id = self.team_required()?;
                let note = parts.collect::<Vec<_>>().join(" ");
                if note.trim().is_empty() {
                    return Err("用法：/team steer <给团队的说明>".into());
                }
                self.team_steer(&id, serde_json::json!({ "command": "steer", "note": note }))
                    .await?;
            }
            Some("retry") => {
                let id = self.team_required()?;
                let step_id = parts
                    .next()
                    .ok_or("用法：/team retry <步骤ID>（/team status 查看步骤）")?;
                self.team_steer(
                    &id,
                    serde_json::json!({ "command": "retry", "step_id": step_id }),
                )
                .await?;
            }
            Some("cancel") => {
                let id = self.team_required()?;
                self.team_steer(&id, serde_json::json!({ "command": "cancel" }))
                    .await?;
            }
            Some("diff") => {
                let id = self.team_required()?;
                self.team_diff(&id).await?;
            }
            Some(first) => {
                // 目标可能含空格：把剩余片段拼回。
                let objective = std::iter::once(first.to_string())
                    .chain(parts)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.team_create_and_watch(
                    &objective,
                    &strategy,
                    &parsed_roles,
                    parsed_model.as_deref(),
                    parallel,
                    automatic_parallel,
                )
                .await?;
            }
        }
        Ok(())
    }

    fn team_required(&self) -> Result<String, Box<dyn std::error::Error>> {
        self.team_id
            .clone()
            .ok_or_else(|| "当前没有团队（先 /team <目标> 创建，或 /team use <团队ID>）".into())
    }

    async fn team_list(&self) -> Result<(), Box<dyn std::error::Error>> {
        let value = self.client.get_json::<serde_json::Value>("/teams").await?;
        let teams = value
            .get("teams")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if teams.is_empty() {
            println!("（没有团队运行；/team <目标> 创建）");
            return Ok(());
        }
        println!("{}", "团队运行：".bold());
        for team in teams {
            let id = team.get("team_id").and_then(|v| v.as_str()).unwrap_or("?");
            let status = team.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let mode = team.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
            let active = team
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let interrupted = team
                .get("interrupted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let members = team
                .get("members")
                .and_then(|v| v.as_array())
                .map(|list| list.len())
                .unwrap_or(0);
            let current = self.team_id.as_deref() == Some(id);
            println!(
                "  {} {}  {}  mode={mode}  {members} 名成员{}{}",
                if current {
                    "→".green().to_string()
                } else {
                    " ".to_string()
                },
                id.dimmed(),
                status,
                if active { " · 运行中" } else { "" },
                if interrupted {
                    " · 已中断（可 /team retry）"
                } else {
                    ""
                },
            );
        }
        Ok(())
    }

    async fn team_context(&self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let context = self
            .client
            .get_json::<serde_json::Value>(&format!("/teams/{id}/context"))
            .await?;
        let revision = context
            .get("revision")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let facts = context
            .get("facts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        println!("{} revision={revision}", "团队共享上下文：".bold());
        if facts.is_empty() {
            println!("（暂无共享事实；用 /team context publish <key> <内容> 发布候选事实）");
            return Ok(());
        }
        let mut seen = std::collections::HashSet::new();
        for fact in facts {
            let key = fact.get("key").and_then(|v| v.as_str()).unwrap_or("?");
            if !seen.insert(key.to_string()) {
                continue;
            }
            let fact_revision = fact.get("revision").and_then(|v| v.as_u64()).unwrap_or(0);
            let producer = fact.get("producer").and_then(|v| v.as_str()).unwrap_or("?");
            let status = fact.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let value = fact.get("value").and_then(|v| v.as_str()).unwrap_or("");
            let preview = value.chars().take(240).collect::<String>();
            println!("  r{fact_revision} [{status}] {key} · {producer}\n    {preview}");
        }
        Ok(())
    }

    async fn team_context_publish(
        &self,
        id: &str,
        key: &str,
        value: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if key.trim().is_empty() || value.trim().is_empty() {
            return Err("用法：/team context publish <key> <内容>".into());
        }
        let current = self
            .client
            .get_json::<serde_json::Value>(&format!("/teams/{id}/context"))
            .await?;
        let expected_revision = current
            .get("revision")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let body = serde_json::json!({
            "expected_revision": expected_revision,
            "key": key,
            "value": value,
            "producer": "user",
            "source_refs": []
        });
        let result: serde_json::Value = self
            .client
            .post_json(&format!("/teams/{id}/context"), &body)
            .await?;
        let revision = result
            .pointer("/fact/revision")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| expected_revision.saturating_add(1));
        println!(
            "{} {key}（revision {revision}，候选事实）",
            "已共享：".green()
        );
        Ok(())
    }

    async fn team_status(&self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let detail = self
            .client
            .get_json::<serde_json::Value>(&format!("/teams/{id}"))
            .await?;
        print_team_detail(&detail);
        Ok(())
    }

    /// 创建团队（后台跑）+ 实时跟踪到终态（Ctrl+C 只停止跟踪，团队继续跑）。
    ///
    /// - `roles` 非空 = 自定义/并行角色编排；
    /// - `model`：显式 `--model` 才随请求下发；缺省由服务端读
    ///   `<workspace>/settings.json` 的 `team.model`（→ `model` → 环境变量/内置缺省）；
    /// - `parallel` = 显式并行开发：请求带 `parallel=true` +
    ///   `max_agent_members=N+2` + `budget.max_parallel=N`；缺省由服务端按
    ///   `team.parallel` 决定；lead 拆解出的 `subtasks`（子任务 + 写范围）由服务端
    ///   运行期动态应用到 w1..wN。
    async fn team_create_and_watch(
        &mut self,
        objective: &str,
        strategy: &str,
        roles: &[serde_json::Value],
        model: Option<&str>,
        parallel: Option<usize>,
        automatic_parallel: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let model = model.map(str::to_string);
        let mut body = serde_json::json!({
            "objective": objective,
            "mode": "team",
            "strategy": strategy,
            "workspace": {
                "root": self.workspace.to_string_lossy(),
                "read_only": false,
            },
        });
        if let Some(model) = &model {
            body["model"] = serde_json::json!(model);
        }
        if let Some(session_id) = self.session.as_deref() {
            body["parent_session_id"] = serde_json::json!(session_id);
        }
        apply_team_execution_intent(&mut body, parallel, automatic_parallel);
        if automatic_parallel {
            println!(
                "{} 默认自动并行开发；并发容量按工作区 team 配置（缺省 4）",
                "编排：".green()
            );
        }
        if !roles.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "roles".to_string(),
                    serde_json::Value::Array(roles.to_vec()),
                );
            }
            if let Some(writers) = parallel {
                println!(
                    "{} 并行 {writers} 路：lead 拆解 → w1..w{writers} 并行 → leader 汇总；\
                     写范围由 lead 拆解动态分配（也可 --write w1=路径 预声明）",
                    "编排：".green()
                );
            } else {
                // 只读提示：自定义角色既未声明写范围、角色名也不属写角色族时，服务端
                // 按权限默认 deny 只读执行（不会产出文件）——提前告知避免"成功零产出"。
                for role in roles {
                    let name = role.get("role").and_then(|v| v.as_str()).unwrap_or("?");
                    let has_write = role
                        .get("write_paths")
                        .and_then(|v| v.as_array())
                        .is_some_and(|paths| !paths.is_empty());
                    if !has_write
                        && !owo_agent_core::worker_profile::WorkerProfile::for_role(name, 0)
                            .is_writer()
                    {
                        println!(
                            "{} 角色 {name} 未声明 --write 且不在写角色族，将按只读执行（不产出文件）",
                            "提示：".yellow()
                        );
                    }
                }
            }
        }
        let created: serde_json::Value = self.client.post_json("/teams", &body).await?;
        let team_id = created
            .get("team_id")
            .and_then(|v| v.as_str())
            .ok_or("创建团队响应缺少 team_id")?
            .to_string();
        let mode = created
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("Team");
        let template = created
            .get("template_id")
            .and_then(|v| v.as_str())
            .unwrap_or("（动态组队）");
        // 成员展示：role 名 +（角色级模型覆盖时的）标注；统一模型在标题行展示。
        let members = created
            .get("members")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|member| {
                        let role = member.get("role").and_then(|v| v.as_str())?;
                        let model = roles
                            .iter()
                            .find(|r| r.get("role").and_then(|v| v.as_str()) == Some(role))
                            .and_then(|r| r.get("model"))
                            .and_then(|v| v.as_str());
                        Some(match model {
                            Some(model) => format!("{role}({model})"),
                            None => role.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        self.team_id = Some(team_id.clone());
        println!(
            "{} 团队已创建：{}（{mode} · 模板 {template} · 模型 {}）",
            "✓".green(),
            team_id,
            model
                .as_deref()
                .unwrap_or("settings.json team.model / 服务端缺省")
        );
        if !members.is_empty() {
            println!("  成员：{members}");
        }
        println!("  团队在 Daemon 侧后台运行；下面实时跟踪（Ctrl+C 停止跟踪，团队继续跑）");
        self.team_watch(&team_id).await
    }

    /// 轮询任务图并打印状态转移，直到团队进入终态。
    async fn team_watch(&self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        use std::collections::{HashMap, HashSet};
        use std::time::{Duration, Instant};

        let deadline = Instant::now() + Duration::from_secs(600);
        let mut last = HashMap::<String, String>::new();
        let mut seen_audit = HashSet::<u64>::new();
        let mut last_event_id: Option<String> = None;
        let mut detail = self
            .client
            .get_json::<serde_json::Value>(&format!("/teams/{id}"))
            .await?;
        print_team_detail(&detail);
        seed_team_step_states(&detail, &mut last);
        let mut status = team_status(&detail);
        if team_status_is_terminal(&status) {
            print_team_watch_terminal(&status, &detail);
            return Ok(());
        }

        let mut retry_delay = Duration::from_millis(250);
        loop {
            if Instant::now() >= deadline {
                println!(
                    "{}",
                    "（跟踪已到 10 分钟上限；团队继续在后台运行，/team watch 继续跟踪）".yellow()
                );
                return Ok(());
            }

            let stream_path = format!("/teams/{id}/events");
            match self
                .client
                .open_event_stream_after(&stream_path, last_event_id.as_deref())
                .await
            {
                Ok(mut stream) => loop {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            println!("{}", "（已停止跟踪；团队继续在后台运行，/team status 查看）".yellow());
                            return Ok(());
                        }
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                            println!("{}", "（跟踪已到 10 分钟上限；团队继续在后台运行，/team watch 继续跟踪）".yellow());
                            return Ok(());
                        }
                        event = stream.next_event() => match event {
                            Some(Ok(event)) => {
                                if let Some(cursor) = stream.last_event_id() {
                                    last_event_id = Some(cursor.to_string());
                                }
                                match event.get("type").and_then(|v| v.as_str()) {
                                    Some("progress") => {
                                        if let Some(steps) = event.pointer("/progress/current_steps").and_then(|v| v.as_array()) {
                                            print_team_step_updates(steps, &mut last);
                                        }
                                    }
                                    Some("audit") => {
                                        if let Some(signature) = team_audit_signature(&event) {
                                            if seen_audit.insert(signature) {
                                                if seen_audit.len() > 2048 {
                                                    seen_audit.clear();
                                                    seen_audit.insert(signature);
                                                }
                                                if let Some(summary) = team_audit_summary(&event) {
                                                    println!("  · {summary}");
                                                }
                                            }
                                        }
                                    }
                                    Some("state") => {
                                        status = event.get("status").and_then(|v| v.as_str()).unwrap_or("running").to_string();
                                        if team_status_is_terminal(&status) {
                                            detail = self.client.get_json(&format!("/teams/{id}")).await?;
                                            print_team_watch_terminal(&status, &detail);
                                            return Ok(());
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(Err(error)) => {
                                if let Some(cursor) = stream.last_event_id() {
                                    last_event_id = Some(cursor.to_string());
                                }
                                eprintln!("{} {error}", "团队事件流中断，正在刷新状态并重连：".yellow());
                                break;
                            }
                            None => {
                                if let Some(cursor) = stream.last_event_id() {
                                    last_event_id = Some(cursor.to_string());
                                }
                                break;
                            }
                        }
                    }
                },
                Err(error) => {
                    eprintln!(
                        "{} {error}",
                        "团队事件流暂不可用，正在刷新状态并重连：".yellow()
                    );
                }
            }

            // SSE 断开时先用快照补齐状态，再重连；不会让断流丢失进度。
            detail = self
                .client
                .get_json::<serde_json::Value>(&format!("/teams/{id}"))
                .await?;
            status = team_status(&detail);
            if let Some(steps) = detail
                .get("progress")
                .and_then(|progress| progress.get("current_steps"))
                .and_then(|steps| steps.as_array())
            {
                print_team_step_updates(steps, &mut last);
            }
            if let Some(tasks) = detail.get("tasks").and_then(|tasks| tasks.as_array()) {
                print_team_step_updates(tasks, &mut last);
            }
            if team_status_is_terminal(&status) {
                print_team_watch_terminal(&status, &detail);
                return Ok(());
            }

            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    println!("{}", "（已停止跟踪；团队继续在后台运行，/team status 查看）".yellow());
                    return Ok(());
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    println!("{}", "（跟踪已到 10 分钟上限；团队继续在后台运行，/team watch 继续跟踪）".yellow());
                    return Ok(());
                }
                _ = tokio::time::sleep(retry_delay) => {}
            }
            retry_delay = (retry_delay * 2).min(Duration::from_secs(5));
        }
    }

    async fn team_steer(
        &self,
        id: &str,
        payload: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let value: serde_json::Value = self
            .client
            .post_json(&format!("/teams/{id}/steer"), &payload)
            .await?;
        println!("{} {}", "已提交团队指令：".green(), value);
        Ok(())
    }

    async fn team_diff(&self, id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let value = self
            .client
            .get_json::<serde_json::Value>(&format!("/teams/{id}/change-sets"))
            .await?;
        let change_sets = value
            .get("change_sets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if change_sets.is_empty() {
            println!("（团队还没有变更集；写角色产出真实文件改动后会出现）");
            return Ok(());
        }
        println!("{}", "团队变更集：".bold());
        if let Some(reason) = value.get("approval_block_reason").and_then(|v| v.as_str()) {
            println!("  {} {reason}", "批准门：".yellow());
        }
        for change_set in change_sets {
            let cs_id = change_set
                .get("change_set_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let role = change_set
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let state = change_set
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let files = change_set
                .get("changed_files")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            println!(
                "  {} role={role} status={state} 文件 {} 个",
                cs_id.dimmed(),
                files.len()
            );
            for file in files.iter().take(10) {
                if let Some(path) = file.as_str() {
                    println!("      {path}");
                }
            }
            if let Some(diff_ref) = change_set.get("diff_ref").and_then(|v| v.as_str()) {
                println!("      diff: {diff_ref}");
            }
        }
        println!("  （接受/拒绝/回滚：POST /change-sets/{id}/accept|reject|revert）");
        Ok(())
    }
    fn decide_permission(
        &self,
        event: &SseEvent,
    ) -> Result<PermissionResponse, Box<dyn std::error::Error>> {
        let SseEvent::PermissionRequest {
            tool,
            reason,
            level,
            args,
            redacted_args,
            risk_note,
            explain,
            ..
        } = event
        else {
            return Ok(parse_approval_response("deny"));
        };
        if self.no_approval {
            return Ok(PermissionResponse {
                allow: true,
                remember: None,
                scope: Some("once".to_string()),
            });
        }
        print_permission_card(&PermissionCard {
            tool,
            level: level.as_deref(),
            reason,
            args: Some(args),
            redacted_args: redacted_args.as_ref(),
            risk_note: risk_note.as_deref(),
            explain: explain.as_ref(),
        });
        eprint!("允许？[y=仅本次 / t=本任务 / w=工作区长期 / n=拒绝] ");
        use std::io::Write;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(parse_approval_response(&line))
    }
}

fn team_status(detail: &serde_json::Value) -> String {
    detail
        .get("team")
        .and_then(|team| team.get("status"))
        .and_then(|value| value.as_str())
        .unwrap_or("running")
        .to_string()
}

fn team_status_is_terminal(status: &str) -> bool {
    ["succeeded", "failed", "cancelled", "canceled"]
        .iter()
        .any(|terminal| status.eq_ignore_ascii_case(terminal))
}

fn seed_team_step_states(
    detail: &serde_json::Value,
    last: &mut std::collections::HashMap<String, String>,
) {
    if let Some(tasks) = detail.get("tasks").and_then(|value| value.as_array()) {
        for task in tasks {
            if let (Some(id), Some(status)) = (
                task.get("task_id").and_then(|value| value.as_str()),
                task.get("status").and_then(|value| value.as_str()),
            ) {
                last.insert(id.to_string(), status.to_string());
            }
        }
    }
}

fn print_team_step_updates(
    steps: &[serde_json::Value],
    last: &mut std::collections::HashMap<String, String>,
) {
    for step in steps {
        let Some(id) = step
            .get("step_id")
            .or_else(|| step.get("task_id"))
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        let role = step
            .get("worker")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        let state = step
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("Pending");
        if last.get(id).is_some_and(|previous| previous == state) {
            continue;
        }
        let line = match state.to_ascii_lowercase().as_str() {
            "claimed" => format!("  {} {role} 已领取，等待执行", "◷".yellow()),
            "running" => format!("  {} {role} 开始执行", "▶".blue()),
            "succeeded" => format!("  {} {role} 完成", "✔".green()),
            "failed" | "aborted" => format!(
                "  {} {role} 失败：{}",
                "✘".red(),
                step.get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("查看 /team status")
            ),
            "skipped" => format!("  {} {role} 跳过", "○".dimmed()),
            _ => format!("  {} {role} {state}", "○".dimmed()),
        };
        println!("{line}");
        last.insert(id.to_string(), state.to_string());
    }
}

fn team_audit_signature(event: &serde_json::Value) -> Option<u64> {
    use std::hash::{Hash, Hasher};

    let event_name = event.get("event")?.as_str()?;
    let detail = event
        .get("detail")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let timestamp = event
        .get("ts")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    timestamp.hash(&mut hasher);
    event_name.hash(&mut hasher);
    detail.hash(&mut hasher);
    Some(hasher.finish())
}

fn team_audit_field(detail: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    detail
        .split_whitespace()
        .find_map(|field| field.strip_prefix(&prefix))
        .map(|value| {
            value
                .chars()
                .filter(|character| !character.is_control())
                .take(80)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

fn team_audit_summary(event: &serde_json::Value) -> Option<String> {
    let name = event.get("event")?.as_str()?;
    let detail = event
        .get("detail")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let field = |key| team_audit_field(detail, key).unwrap_or_else(|| "?".to_string());
    match name {
        "team.worker.started" => Some(format!(
            "{} 开始执行（{}）",
            field("role"),
            field("step_id")
        )),
        "team.tool.started" => Some(format!(
            "{} 正在使用 {} · {}",
            field("role"),
            field("tool"),
            field("step_id")
        )),
        "team.tool.finished" => Some(format!(
            "{} 使用 {}：{} · {} ms · {}",
            field("role"),
            field("tool"),
            field("outcome"),
            field("duration_ms"),
            field("step_id")
        )),
        "team.model.started" => Some(format!(
            "{} 发起模型请求 · {}",
            field("role"),
            field("step_id")
        )),
        "team.model.request_completed" => Some(format!(
            "模型请求：{} · {} · {} ms · {} tokens",
            field("role"),
            field("model"),
            field("latency_ms"),
            field("usage_tokens")
        )),
        "team.lease.wait_completed" => Some(format!(
            "{} 等待写入租约 {} ms",
            field("role"),
            field("wait_ms")
        )),
        "team.worker.finished" => Some(format!(
            "{} 执行结束：{} · {} ms",
            field("role"),
            field("outcome"),
            field("wall_ms")
        )),
        "team.context.fact_published" => Some(format!(
            "共享事实更新：{} · revision {}",
            field("key"),
            field("revision")
        )),
        "team.artifact.validation_started" => Some(format!(
            "产物校验开始（{}，{}）",
            field("member"),
            field("format")
        )),
        "team.artifact.validation_passed" => Some(format!(
            "产物校验通过（{}，{} · {} ms）",
            field("member"),
            field("format"),
            field("duration_ms")
        )),
        "team.artifact.validation_rejected" => Some(format!(
            "产物校验未通过（{} · {} ms）",
            team_audit_field(detail, "step").unwrap_or_else(|| "查看 /team status".to_string()),
            field("duration_ms")
        )),
        _ => None,
    }
}

fn print_team_watch_terminal(status: &str, detail: &serde_json::Value) {
    if let Some(tasks) = detail.get("tasks").and_then(|value| value.as_array()) {
        for task in tasks.iter().filter(|task| {
            task.get("status")
                .and_then(|value| value.as_str())
                .is_some_and(|state| {
                    matches!(state.to_ascii_lowercase().as_str(), "failed" | "aborted")
                })
        }) {
            let role = task
                .get("role")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let error = task
                .get("error")
                .and_then(|value| value.as_str())
                .unwrap_or("未知");
            println!("  {} {role} 失败：{error}", "✘".red());
        }
    }
    let mark = if status.eq_ignore_ascii_case("succeeded") {
        "✓".green().to_string()
    } else {
        "✘".red().to_string()
    };
    println!("{mark} 团队终态：{status}（/team diff 看真实变更集，/team status 看详情）");
}

/// 团队详情渲染（状态 / 成员 / 任务图 / 审计尾迹）——`/team status` 与跟踪共用。
fn print_team_detail(detail: &serde_json::Value) {
    let team = detail.get("team").cloned().unwrap_or_default();
    let id = team.get("team_id").and_then(|v| v.as_str()).unwrap_or("?");
    let status = team.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let mode = team.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
    let template = team
        .get("template_id")
        .and_then(|v| v.as_str())
        .unwrap_or("（动态组队）");
    let interrupted = detail
        .get("interrupted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    println!(
        "{} {id}  {status}  mode={mode}  模板={template}{}",
        "团队：".bold(),
        if interrupted { "  · 已中断" } else { "" }
    );
    if let Some(members) = team.get("members").and_then(|v| v.as_array()) {
        let names = members
            .iter()
            .filter_map(|member| member.get("user_id").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        if !names.is_empty() {
            println!("  成员：{names}");
        }
    }
    if let Some(tasks) = detail.get("tasks").and_then(|v| v.as_array()) {
        println!("  任务图：");
        for task in tasks {
            let task_id = task.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
            let role = task.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            let state = task
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("Pending");
            let mark = match state {
                "Succeeded" => "✔".green().to_string(),
                "Running" => "▶".blue().to_string(),
                "Failed" => "✘".red().to_string(),
                _ => "○".dimmed().to_string(),
            };
            let error = task
                .get("error")
                .and_then(|v| v.as_str())
                .map(|error| format!("：{error}"))
                .unwrap_or_default();
            println!("    {mark} {task_id}  {role}  {state}{error}");
        }
    }
    if let Some(tail) = detail.get("audit_tail").and_then(|v| v.as_array()) {
        if !tail.is_empty() {
            println!("  审计尾迹（最近 {} 条）：", tail.len());
            for entry in tail.iter().take(5) {
                let event = entry.get("event").and_then(|v| v.as_str()).unwrap_or("?");
                let text = entry.get("detail").and_then(|v| v.as_str()).unwrap_or("");
                println!("    {event}  {text}");
            }
        }
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
    println!(
        "  /permissions [status] 查看档位与授权；set <档位> 切换；revoke <授权ID> 撤销单条授权"
    );
    println!("  /team [--parallel N] [--model <模型>] [--write <角色>=<路径>] <目标>");
    println!(
        "                    并行 N 路：lead 拆解 → w1..wN 并行 → leader 汇总；模型/并行/角色缺省读 <workspace>/settings.json 的 team 段（list/status/steer/retry/cancel/diff）"
    );
    println!("                    context 查看团队共享事实；context publish <key> <内容> 发布带 revision 的候选事实");
    println!("  /audit /skills /settings /traces  读取服务端状态");
    println!("  /clear             清屏");
    println!("  /exit | /quit      退出");
    println!("  其它命令请用 --local 使用旧 REPL（迁移中）");
}

/// `/team` 前置参数解析结果（纯逻辑，便于单测）。
#[derive(Debug, Default, PartialEq)]
struct TeamArgs {
    /// 显式策略（`--single|--team|--auto`；None = 按是否声明角色/并行取缺省）。
    strategy: Option<String>,
    /// 自定义角色（RoleSpec JSON：role/assignee/depends_on[/model/write_paths]）。
    roles: Vec<serde_json::Value>,
    /// 非前置参数（子命令或目标片段）；空 = 仅打印用法。
    rest: Vec<String>,
    /// 团队统一模型（`--model <模型>`，全队 agent 共用；None = 用默认常量）。
    model: Option<String>,
    /// 并行路数（`--parallel N`：lead 拆解 → w1..wN 并行 → leader 汇总）。
    parallel: Option<usize>,
}

/// 解析 `/team` 前置参数：
/// - `--single|--team|--auto`：策略开关；
/// - `--parallel <N>`（2..=8）：**并行开发**——生成 `lead`（只读拆解）→ `w1..wN`
///   （依赖 lead，彼此无依赖 = 同 wave 真并行）→ `leader`（汇总）；运行期 lead
///   产出的 `subtasks`（子任务 + 写范围）动态应用到对应 writer；
/// - `--role <名[:依赖1|依赖2]>`（可重复）：自定义角色；与 `--parallel` 互斥；
/// - `--model <模型>`：团队统一模型（全队共用）；`--model <角色>=<模型>[,…]`：
///   角色级覆盖（高级用法）；不传则读 `<workspace>/settings.json` 的 `team.model`；
/// - `--write <角色>=<路径>[;<路径>]`：角色写范围（互不重叠 → 并发落盘）。
///
/// 错误：缺参、角色重复/为空、`--parallel` 与 `--role` 同用、并行路数越界、
/// `--model`/`--write` 指向未声明角色、映射格式非法。
fn apply_team_execution_intent(
    body: &mut serde_json::Value,
    parallel: Option<usize>,
    automatic_parallel: bool,
) {
    if let Some(writers) = parallel {
        body["parallel"] = serde_json::json!(true);
        body["max_agent_members"] = serde_json::json!(writers + 2);
        body["budget"] = serde_json::json!({ "max_parallel": writers });
    } else if automatic_parallel {
        // Capacity remains unset so the server can honor workspace settings.
        body["parallel"] = serde_json::json!(true);
    }
}

fn resolve_team_create_intent(
    strategy: Option<String>,
    roles: &[serde_json::Value],
    parallel: Option<usize>,
) -> (String, bool) {
    let automatic_parallel = strategy.is_none() && roles.is_empty() && parallel.is_none();
    let strategy = strategy.unwrap_or_else(|| {
        if automatic_parallel || !roles.is_empty() || parallel.is_some() {
            "team".to_string()
        } else {
            "auto".to_string()
        }
    });
    (strategy, automatic_parallel)
}

fn parse_team_args(args: &[String]) -> Result<TeamArgs, String> {
    let mut parsed = TeamArgs::default();
    let mut role_specs: Vec<String> = Vec::new();
    let mut model_specs: Vec<String> = Vec::new();
    let mut write_specs: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--single" => {
                parsed.strategy = Some("single".to_string());
                index += 1;
            }
            "--team" => {
                parsed.strategy = Some("team".to_string());
                index += 1;
            }
            "--auto" => {
                parsed.strategy = Some("auto".to_string());
                index += 1;
            }
            "--parallel" => {
                let value = args
                    .get(index + 1)
                    .map(String::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| "--parallel 需要参数（2..=8）".to_string())?;
                let writers: usize = value
                    .parse()
                    .map_err(|_| format!("--parallel 需要 2..=8 的数字：{value}"))?;
                if !(2..=8).contains(&writers) {
                    return Err(format!("--parallel 需要在 2..=8 之间：{writers}"));
                }
                parsed.parallel = Some(writers);
                index += 2;
            }
            flag @ ("--role" | "--model" | "--write") => {
                let value = args
                    .get(index + 1)
                    .map(String::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| format!("{flag} 需要参数"))?
                    .to_string();
                index += 2;
                match flag {
                    "--role" => role_specs.push(value),
                    "--model" => model_specs.push(value),
                    _ => write_specs.push(value),
                }
            }
            _ => break,
        }
    }
    parsed.rest = args[index..].to_vec();

    if parsed.parallel.is_some() && !role_specs.is_empty() {
        return Err(
            "--parallel 与 --role 互斥（并行模式已内置 lead/w1..wN/leader 角色）".to_string(),
        );
    }

    if let Some(writers) = parsed.parallel {
        // 十一期：并行开发角色组由核心提供（契约与运行期分配口径同源）。
        parsed.roles = owo_agent_core::workswarm::parallel_roles(writers)
            .into_iter()
            .map(|role| serde_json::to_value(role).unwrap_or_default())
            .collect();
    } else {
        // `名[:依赖1|依赖2]` → RoleSpec JSON（assignee=agent，缺省模型/写范围由服务端解析）。
        for spec in &role_specs {
            let (name, deps) = match spec.split_once(':') {
                Some((name, deps)) => (
                    name.trim(),
                    deps.split(['|', ','])
                        .map(str::trim)
                        .filter(|d| !d.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                ),
                None => (spec.trim(), Vec::new()),
            };
            if name.is_empty() {
                return Err("--role 角色名不能为空".to_string());
            }
            if parsed
                .roles
                .iter()
                .any(|r| r.get("role").and_then(|v| v.as_str()) == Some(name))
            {
                return Err(format!("--role 角色重复：{name}"));
            }
            parsed.roles.push(serde_json::json!({
                "role": name,
                "assignee": "agent",
                "depends_on": deps,
            }));
        }
    }
    let role_names: Vec<String> = parsed
        .roles
        .iter()
        .filter_map(|r| r.get("role").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let find_role = |roles: &mut [serde_json::Value], role: &str| -> Result<(), String> {
        roles
            .iter_mut()
            .find(|r| r.get("role").and_then(|v| v.as_str()) == Some(role))
            .map(|_| ())
            .ok_or_else(|| {
                format!(
                    "角色 {role} 不在 --role 列表（现有：{}）",
                    role_names.join(", ")
                )
            })
    };
    for spec in &model_specs {
        let spec = spec.trim();
        // 无 `=`：团队统一模型（`--model glm-5.3-flashx` 全队共用，最后一条生效）。
        if !spec.contains('=') {
            if spec.contains(',') {
                return Err(format!("--model 统一模型不能含逗号：{spec}"));
            }
            parsed.model = Some(spec.to_string());
            continue;
        }
        for pair in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (role, model) = pair
                .split_once('=')
                .ok_or_else(|| format!("--model 需要 <角色>=<模型>：{pair}"))?;
            let (role, model) = (role.trim(), model.trim());
            if role.is_empty() || model.is_empty() {
                return Err(format!("--model 需要 <角色>=<模型>：{pair}"));
            }
            find_role(&mut parsed.roles, role)?;
            let target = parsed
                .roles
                .iter_mut()
                .find(|r| r.get("role").and_then(|v| v.as_str()) == Some(role))
                .expect("find_role 已校验存在");
            target["model"] = serde_json::json!(model);
        }
    }
    for spec in &write_specs {
        let (role, paths) = spec
            .split_once('=')
            .ok_or_else(|| format!("--write 需要 <角色>=<路径[;路径]>：{spec}"))?;
        let role = role.trim();
        let paths: Vec<String> = paths
            .split(';')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        if role.is_empty() || paths.is_empty() {
            return Err(format!("--write 需要 <角色>=<路径[;路径]>：{spec}"));
        }
        find_role(&mut parsed.roles, role)?;
        let target = parsed
            .roles
            .iter_mut()
            .find(|r| r.get("role").and_then(|v| v.as_str()) == Some(role))
            .expect("find_role 已校验存在");
        target["write_paths"] = serde_json::json!(paths);
    }
    Ok(parsed)
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

    fn team_args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_team_args_builds_parallel_roles_with_models_and_scopes() {
        let parsed = parse_team_args(&team_args(&[
            "--role",
            "w1",
            "--role",
            "w2:reviewer",
            "--model",
            "w1=glm-5.3-flash,w2=glm-4.6",
            "--write",
            "w1=src/a;src/common",
            "--write",
            "w2=src/b",
            "实现",
            "A 与 B",
        ]))
        .unwrap();
        assert_eq!(parsed.strategy, None, "缺省策略按是否声明角色推导");
        assert_eq!(parsed.rest, vec!["实现".to_string(), "A 与 B".to_string()]);
        assert_eq!(parsed.roles.len(), 2);
        assert_eq!(parsed.roles[0]["role"], "w1");
        assert_eq!(parsed.roles[0]["assignee"], "agent");
        assert_eq!(parsed.roles[0]["model"], "glm-5.3-flash");
        assert_eq!(
            parsed.roles[0]["write_paths"],
            serde_json::json!(["src/a", "src/common"])
        );
        assert_eq!(parsed.roles[1]["role"], "w2");
        assert_eq!(
            parsed.roles[1]["depends_on"],
            serde_json::json!(["reviewer"])
        );
        assert_eq!(parsed.roles[1]["model"], "glm-4.6");
        assert_eq!(parsed.roles[1]["write_paths"], serde_json::json!(["src/b"]));
    }

    #[test]
    fn automatic_parallel_request_leaves_capacity_to_workspace_settings() {
        let mut body = serde_json::json!({"objective":"goal"});
        apply_team_execution_intent(&mut body, None, true);
        assert_eq!(body["parallel"], true);
        assert!(body.get("budget").is_none());
        assert!(body.get("max_agent_members").is_none());

        let mut explicit = serde_json::json!({"objective":"goal"});
        apply_team_execution_intent(&mut explicit, Some(3), false);
        assert_eq!(explicit["parallel"], true);
        assert_eq!(explicit["max_agent_members"], 5);
        assert_eq!(explicit["budget"]["max_parallel"], 3);
    }

    #[test]
    fn default_team_goal_selects_parallel_development_without_overriding_explicit_modes() {
        let (strategy, parallel) = resolve_team_create_intent(None, &[], None);
        assert_eq!(strategy, "team");
        assert!(parallel);

        let (strategy, parallel) = resolve_team_create_intent(Some("auto".to_string()), &[], None);
        assert_eq!(strategy, "auto");
        assert!(!parallel);

        let roles = vec![serde_json::json!({"role":"reviewer"})];
        let (strategy, parallel) = resolve_team_create_intent(None, &roles, None);
        assert_eq!(strategy, "team");
        assert!(!parallel);

        let (strategy, parallel) =
            resolve_team_create_intent(Some("single".to_string()), &[], None);
        assert_eq!(strategy, "single");
        assert!(!parallel);
    }

    #[test]
    fn parse_team_args_keeps_defaults_and_rejects_bad_mappings() {
        // 无前置参数：rest 原样，无角色；模型不写死（缺省读 settings.json team 段）。
        let parsed = parse_team_args(&team_args(&["--team", "目标"])).unwrap();
        assert_eq!(parsed.strategy.as_deref(), Some("team"));
        assert!(parsed.roles.is_empty());
        assert!(parsed.model.is_none(), "未显式 --model 时不得内置模型常量");
        assert!(parsed.parallel.is_none());
        assert_eq!(parsed.rest, vec!["目标".to_string()]);
        // 未声明角色 → 模型/写范围映射报错；缺参数报错。
        assert!(parse_team_args(&team_args(&["--model", "w1=x"])).is_err());
        assert!(parse_team_args(&team_args(&["--role", "w1", "--write", "w2=src"])).is_err());
        assert!(parse_team_args(&team_args(&["--write"])).is_err());
    }

    #[test]
    fn parse_team_args_parallel_generates_lead_writers_and_leader() {
        let parsed = parse_team_args(&team_args(&[
            "--parallel",
            "3",
            "--model",
            "glm-5.3-flashx",
            "--write",
            "w2=src/b",
            "开发",
            "功能",
        ]))
        .unwrap();
        assert_eq!(parsed.parallel, Some(3));
        assert_eq!(parsed.model.as_deref(), Some("glm-5.3-flashx"));
        let roles: Vec<&str> = parsed
            .roles
            .iter()
            .map(|r| r["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["lead", "w1", "w2", "w3", "leader"]);
        assert_eq!(parsed.roles[1]["depends_on"], serde_json::json!(["lead"]));
        assert_eq!(
            parsed.roles[4]["depends_on"],
            serde_json::json!(["w1", "w2", "w3"])
        );
        assert_eq!(parsed.roles[2]["write_paths"], serde_json::json!(["src/b"]));
        assert_eq!(parsed.rest, vec!["开发".to_string(), "功能".to_string()]);
    }

    #[test]
    fn team_audit_summary_exposes_runtime_measurements_without_control_characters() {
        let event = serde_json::json!({
            "ts": "2026-10-02T05:00:00Z",
            "event": "team.model.request_completed",
            "detail": "role=w1 model=glm-5 latency_ms=123 usage_tokens=42 request_id=secret"
        });
        let summary = team_audit_summary(&event).expect("known runtime event");
        assert!(summary.contains("w1"));
        assert!(summary.contains("glm-5"));
        assert!(summary.contains("123 ms"));
        assert!(summary.contains("42 tokens"));
        assert!(!summary.contains("secret"));

        let model_started = serde_json::json!({
            "event": "team.model.started",
            "detail": "role=w1 step_id=step-1"
        });
        let model_summary = team_audit_summary(&model_started).expect("model start is visible");
        assert!(model_summary.contains("w1"));
        assert!(model_summary.contains("step-1"));

        let tool_event = serde_json::json!({
            "event": "team.tool.finished",
            "detail": "role=implementer step_id=step-2 tool=apply_patch outcome=succeeded duration_ms=13",
        });
        let tool_summary = team_audit_summary(&tool_event).expect("tool event is visible");
        assert!(tool_summary.contains("apply_patch"));
        assert!(tool_summary.contains("succeeded"));
        assert!(tool_summary.contains("13 ms"));

        let validation_event = serde_json::json!({
            "event": "team.artifact.validation_passed",
            "detail": "member=m-w1 step=step-2 format=markdown duration_ms=4",
        });
        let validation_summary =
            team_audit_summary(&validation_event).expect("validation timing is visible");
        assert!(validation_summary.contains("4 ms"));

        let unsafe_field = serde_json::json!({
            "event": "team.worker.started",
            "detail": "role=w1\u{001b}[31m step_id=step-1"
        });
        let summary = team_audit_summary(&unsafe_field).expect("worker start");
        assert!(!summary.contains('\u{1b}'));
        assert!(team_audit_summary(&serde_json::json!({"event":"unknown.event"})).is_none());
    }

    #[test]
    fn parse_team_args_parallel_rejects_bad_or_conflicting_flags() {
        assert!(parse_team_args(&team_args(&["--parallel", "1"])).is_err());
        assert!(parse_team_args(&team_args(&["--parallel", "9"])).is_err());
        assert!(parse_team_args(&team_args(&["--parallel", "2", "--role", "w1"])).is_err());
        assert!(parse_team_args(&team_args(&["--parallel"])).is_err());
    }
}
