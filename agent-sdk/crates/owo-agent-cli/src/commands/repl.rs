// §12.3 CLI 拆分批次五：repl 交互域（自 main.rs 机械外移，零行为变化）。
// Repl 主循环：会话/审批/恢复/子命令；审批者与共享 stdin 经 crate::support 显式引用。

use crate::commands::turn::EventPrinter;
use crate::support::*;
use clap::Args;
use colored::Colorize;
use owo_agent_core::permissions::{Approver, AutoApprover};
use owo_agent_core::session::SessionStore;
use owo_agent_core::{
    discover_plugins, install_builtin_packages, save_trace, Agent, LearnRecorder, McpClient,
    McpServerConfig, PluginManifest, ProactiveEngine, Session, Settings, SituationStore,
    SkillRegistry, SqliteSessionStore, TraceRecord, TurnEvent, Whitelist,
};
use rustyline::error::ReadlineError;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Args)]
pub(crate) struct ReplArgs {
    #[arg(long, default_value = ".")]
    pub(crate) workspace: PathBuf,
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// 初始 agent：build（默认）或 plan（只读）
    #[arg(long, default_value = "build")]
    pub(crate) agent: String,
    /// 自动允许所有审批（仅测试用）
    #[arg(long)]
    pub(crate) no_approval: bool,
    /// 覆盖数据目录（默认 %LOCALAPPDATA%\OwO\Agent 或 OWO_AGENT_DATA）
    #[arg(long)]
    pub(crate) data_dir: Option<PathBuf>,
}

mod handlers;

pub(crate) struct Repl {
    workspace: PathBuf,
    model: String,
    read_only: bool,
    no_approval: bool,
    data_root: PathBuf,
    store: SqliteSessionStore,
    session: Option<Session>,
    agent: Arc<Agent>,
    abort: Arc<AtomicBool>,
    stdin: SharedStdin,
    mcp_configs: Vec<McpServerConfig>,
    mcp_clients: Vec<(String, Arc<tokio::sync::Mutex<McpClient>>)>,
    skills: SkillRegistry,
    settings: Settings,
    plugins: Vec<PluginManifest>,
    audit_flushed: usize,
    perception: SituationStore,
    learn: LearnRecorder,
    whitelist: Whitelist,
    proactive: ProactiveEngine,
}

impl Repl {
    pub(crate) async fn run(args: ReplArgs) -> Result<(), Box<dyn std::error::Error>> {
        let workspace = args.workspace.canonicalize()?;
        let settings = Settings::load(&workspace);
        apply_egress_setting(&settings);
        settings.apply_usage_env();
        let model = resolve_model(args.model, settings.model.as_deref());
        let read_only = args.agent == "plan" || settings.read_only;
        let root = ensure_data_root(args.data_dir, &workspace);
        let store = SqliteSessionStore::open(&root.join("index.db"))?;
        let mut mcp_configs = load_mcp_configs(&root);
        for server in settings.mcp_servers.clone() {
            if !mcp_configs.iter().any(|config| config.name == server.name) {
                mcp_configs.push(server);
            }
        }
        let discovered_plugins = discover_plugins(&workspace, &root);
        let plugin_state =
            owo_agent_core::PluginStateStore::new(Some(root.join("plugin_state.json")));
        let enabled_plugins =
            owo_agent_core::plugin::discover_enabled_plugins(&workspace, &root, &plugin_state);
        merge_plugin_mcp(&enabled_plugins, &mut mcp_configs);
        let plugins: Vec<PluginManifest> = discovered_plugins
            .into_iter()
            .map(|(_, manifest)| manifest)
            .collect();
        let mcp_clients = connect_mcp_clients(&mcp_configs).await;
        let _ = install_builtin_packages(&builtin_skills_root(), &root);
        let mut skills = SkillRegistry::discover(&workspace, &root);
        apply_disabled_skills(&mut skills, &settings);
        let mut whitelist = Whitelist::default();
        for entry in settings.whitelist.clone() {
            whitelist.upsert(entry);
        }
        let proactive = ProactiveEngine::new(settings.proactive.clone());
        let agent = Arc::new(build_agent_with_mcp(
            &workspace,
            &model,
            read_only,
            &mcp_clients,
            &skills,
            &settings.deny_commands,
        )?);
        // §11：--no-approval 弃用告警（兼容期显式映射 + 高风险提示）。
        if args.no_approval {
            eprintln!(
                "⚠ 已弃用：--no-approval 将在未来版本移除；兼容期等价 --permissions trusted（高风险：全部操作自动批准，含写/执行/联网）"
            );
        }
        let mut repl = Repl {
            workspace,
            model,
            read_only,
            no_approval: args.no_approval,
            data_root: root.clone(),
            store,
            session: None,
            agent,
            abort: Arc::new(AtomicBool::new(false)),
            stdin: SharedStdin::new(),
            mcp_configs,
            mcp_clients,
            skills,
            settings,
            plugins,
            audit_flushed: 0,
            perception: SituationStore::new(),
            learn: LearnRecorder::new(),
            whitelist,
            proactive,
        };

        println!(
            "{} {}（{}）",
            "OwO Agent".bold(),
            env!("CARGO_PKG_VERSION").cyan(),
            display_path(&repl.workspace).dimmed()
        );
        println!("输入 /help 查看命令；直接输入文字开始任务。");

        let history_path = root.join("history.txt");
        if std::io::stdin().is_terminal() {
            repl.run_terminal(&history_path).await?;
        } else {
            repl.run_piped().await?;
        }
        if let Some(session) = &repl.session {
            let _ = repl.store.save(session);
        }
        Ok(())
    }

    async fn run_terminal(
        &mut self,
        history_path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut rl = rustyline::DefaultEditor::new()?;
        if let Ok(content) = std::fs::read_to_string(history_path) {
            for line in content.lines() {
                let _ = rl.add_history_entry(line);
            }
        }
        loop {
            let prompt = if self.read_only {
                format!("{} ", "plan ❯".yellow())
            } else {
                format!("{} ", "build ❯".green())
            };
            match rl.readline(&prompt) {
                Ok(line) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    let _ = rl.add_history_entry(line.clone());
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
        let history: Vec<String> = rl.history().iter().cloned().collect();
        let _ = std::fs::write(history_path, history.join("\n"));
        Ok(())
    }

    async fn run_piped(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut line = String::new();
        loop {
            print!(
                "{} ",
                if self.read_only {
                    "plan ❯".yellow()
                } else {
                    "build ❯".green()
                }
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            line.clear();
            let read = self.stdin.read_line(&mut line).await?;
            if read == 0 {
                break;
            }
            let line = line.trim().to_string();
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
        if let Some(query) = line.strip_prefix("@explore ") {
            self.run_at_subagent(query, true).await?;
            return Ok(false);
        }
        if let Some(task) = line.strip_prefix("@subagent ") {
            self.run_at_subagent(task, false).await?;
            return Ok(false);
        }
        if let Some(command) = line.strip_prefix('/') {
            let mut parts = command.split_whitespace();
            match parts.next().unwrap_or_default() {
                "help" => crate::print_help(),
                "exit" | "quit" => return Ok(true),
                "new" => self.new_session(parts.next()).await?,
                "sessions" => self.list_sessions(),
                "resume" => {
                    let id = parts
                        .next()
                        .ok_or("用法：/resume <会话ID>（/sessions 查看）")?;
                    self.resume(id).await?;
                }
                "model" => match parts.next() {
                    Some(model) => {
                        self.model = model.to_string();
                        self.rebuild_agent()?;
                        println!("{} {}", "模型已切换：".green(), self.model);
                    }
                    None => println!("当前模型：{}", self.model),
                },
                "plan" => {
                    self.set_mode(true)?;
                }
                "build" => {
                    self.set_mode(false)?;
                }
                "agent" => match parts.next() {
                    Some("plan") => self.set_mode(true)?,
                    Some("build") => self.set_mode(false)?,
                    Some(other) => println!("未知 agent：{other}（build / plan）"),
                    None => println!(
                        "当前 agent：{}",
                        if self.read_only {
                            "plan（只读）"
                        } else {
                            "build"
                        }
                    ),
                },
                "diff" => self.show_diff(),
                "undo" | "revert" => self.undo().await?,
                "mcp" => self.handle_mcp(command).await?,
                "skills" => match parts.next() {
                    Some("reload") => {
                        self.reload_skills()?;
                    }
                    _ => self.list_skills(),
                },
                "fork" => self.fork_session(parts.next()).await?,
                "rewind" => {
                    let keep = parts.next().ok_or("用法：/rewind <保留消息数>")?;
                    self.rewind_session(keep).await?;
                }
                "redo" => self.redo_session().await?,
                "undo-msg" => self.undo_message(parts.next()).await?,
                "redo-msg" => self.redo_message().await?,
                "tree" => self.show_tree(),
                "share" => self.share_session(parts.next())?,
                "traces" => self.list_traces(),
                "trace" => self.show_trace(parts.next())?,
                "settings" => self.show_settings(),
                "plugins" => self.list_plugins(),
                "whitelist" => self.show_whitelist(),
                "perception" => self.show_perception(),
                "learn" => self.handle_learn(command)?,
                "proactive" => self.handle_proactive(command)?,
                "status" => self.show_status(),
                "permissions" => self.handle_permissions(command)?,
                "audit" => self.show_audit(),
                "init" => {
                    let target = self.workspace.join("AGENTS.md");
                    if target.exists() {
                        println!("{} {}", "AGENTS.md 已存在：".yellow(), target.display());
                    } else {
                        std::fs::write(&target, AGENTS_TEMPLATE)?;
                        println!("{} {}", "已生成".green(), target.display());
                    }
                }
                "abort" => {
                    self.abort.store(true, Ordering::Relaxed);
                    println!("已请求中止当前回合");
                }
                "clear" => print!("\x1b[2J\x1b[1;1H"),
                other => println!("未知命令：/{other}（/help 查看全部）"),
            }
            return Ok(false);
        }
        self.run_turn(line).await?;
        Ok(false)
    }

    fn set_mode(&mut self, read_only: bool) -> Result<(), Box<dyn std::error::Error>> {
        self.read_only = read_only;
        self.rebuild_agent()?;
        println!(
            "{}",
            if read_only {
                "已切换到 plan 模式（只读，写/执行将被拒绝）".yellow()
            } else {
                "已切换到 build 模式（写/执行需要审批）".green()
            }
        );
        Ok(())
    }

    fn rebuild_agent(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.audit_flushed = 0;
        self.agent = Arc::new(build_agent_with_mcp(
            &self.workspace,
            &self.model,
            self.read_only,
            &self.mcp_clients,
            &self.skills,
            &self.settings.deny_commands,
        )?);
        Ok(())
    }

    async fn run_turn(&mut self, prompt: &str) -> Result<(), Box<dyn std::error::Error>> {
        if self.session.is_none() {
            self.new_session(None).await?;
        }
        let mut session = self.session.take().expect("session just created");
        let prompt = prompt.to_string();
        self.abort.store(false, Ordering::Relaxed);
        let agent = Arc::clone(&self.agent);
        let abort = Arc::clone(&self.abort);
        let approver: Arc<dyn Approver> = if self.no_approval {
            Arc::new(AutoApprover { allow: true })
        } else {
            Arc::new(ConsoleApprover {
                stdin: self.stdin.clone(),
            })
        };

        println!("{} {}", "▶".green(), prompt.dimmed());
        let task = tokio::spawn(async move {
            let mut printer = EventPrinter::new();
            let mut on_event = |event: &TurnEvent| printer.print(event);
            let outcome = agent
                .run_turn(
                    &mut session,
                    &prompt,
                    approver.as_ref(),
                    &abort,
                    &mut on_event,
                )
                .await;
            (outcome, session)
        });

        let abort_flag = Arc::clone(&self.abort);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                abort_flag.store(true, Ordering::Relaxed);
                println!("{}", "（Ctrl+C：正在中止当前回合…）".yellow());
            }
        });

        let (outcome, session) = task
            .await
            .map_err(|error| std::io::Error::other(format!("回合任务失败：{error}")))?;
        self.session = Some(session);
        if let Some(session) = &self.session {
            self.store.save(session)?;
        }
        self.flush_audit();
        let outcome = outcome?;
        let trace =
            TraceRecord::from_outcome(self.session.as_ref().expect("session saved"), &outcome);
        if let Ok(path) = save_trace(&self.data_root.join("traces"), &trace) {
            println!("[trace] {}", display_path(&path));
        }
        println!(
            "{} 工具步数 {}，审计 {} 条，改动 {} 个文件（/diff 查看，/undo 回滚）",
            "✓".green(),
            outcome.steps,
            self.agent
                .audit_log()
                .lock()
                .map(|log| log.entries.len())
                .unwrap_or(0),
            self.session.as_ref().map(|s| s.diff().len()).unwrap_or(0),
        );
        Ok(())
    }

    fn flush_audit(&mut self) {
        let audit_entries = self
            .agent
            .audit_log()
            .lock()
            .map(|guard| guard.entries.clone())
            .unwrap_or_default();
        if audit_entries.len() <= self.audit_flushed {
            return;
        }
        if self
            .store
            .append_audit(&audit_entries[self.audit_flushed..])
            .is_ok()
        {
            self.audit_flushed = audit_entries.len();
        }
    }
}
