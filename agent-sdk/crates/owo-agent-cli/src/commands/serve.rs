// §12.3 CLI 拆分批次 4b：serve/init 子命令域（自 main.rs 机械外移，零行为变化）。
// serve：服务生命周期（恢复/遥测/文件日志/路由/实例握手/优雅关闭）；init：项目脚手架。

use crate::support::*;
use clap::Args;
use colored::Colorize;
use owo_agent_core::{install_builtin_packages, Settings, SkillRegistry, SqliteSessionStore};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Args)]
pub(crate) struct ServeArgs {
    /// 监听端口（0 = 由系统分配临时端口；实际端口经 stdout 的 core_ready 行上报）
    #[arg(long, default_value_t = 4096)]
    port: u16,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}

/// R3-B（§3.4 契约）：启动期致命错误以恰好一行 `core_fatal` JSON 上报到 stdout
/// （桌面壳解析并映射为稳定错误码，UI 呈现对应恢复动作），随后非零退出。
/// 消息**必须脱敏**：只带目录路径与 OS 错误文本，绝不携带凭据/环境变量值。
fn emit_core_fatal(code: &str, message: &str) -> ! {
    println!(
        "{}",
        serde_json::json!({ "event": "core_fatal", "code": code, "message": message })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::exit(3);
}

pub(crate) async fn run_serve(args: ServeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = args.workspace.canonicalize()?;
    let settings = Settings::load(&workspace);
    apply_egress_setting(&settings);
    settings.apply_usage_env();
    let model = resolve_model(None, settings.model.as_deref());
    // R3-B（§3.4 `storage/not_writable`）：桌面壳上下文（实例身份由壳注入）绝不允许
    // 静默把数据根迁移到工作区 `.owo-agent`——用户必须看到存储错误并"更换数据目录"。
    // 非桌面 CLI 保持既有回退行为不变。
    let desktop_ctx = std::env::var_os("OWO_DESKTOP_INSTANCE_ID").is_some();
    let root = if desktop_ctx {
        match ensure_data_root_checked(None) {
            Ok(root) => root,
            Err(reason) => emit_core_fatal("storage/not_writable", &reason),
        }
    } else {
        ensure_data_root(None, &workspace)
    };
    let plugin_state = owo_agent_core::PluginStateStore::new(Some(root.join("plugin_state.json")));
    let plugins =
        owo_agent_core::plugin::discover_enabled_plugins(&workspace, &root, &plugin_state);
    let mut mcp_configs = load_mcp_configs(&root);
    merge_plugin_mcp(&plugins, &mut mcp_configs);
    let mcp_clients = connect_mcp_clients(&mcp_configs).await;
    let _ = install_builtin_packages(&builtin_skills_root(), &root);
    let mut skills = SkillRegistry::discover(&workspace, &root);
    apply_disabled_skills(&mut skills, &settings);
    let agent = build_agent_with_mcp_serve(
        &workspace,
        &model,
        settings.read_only,
        &mcp_clients,
        &skills,
        &settings.deny_commands,
    )?;
    let store = match SqliteSessionStore::open(&root.join("index.db")) {
        Ok(store) => store,
        Err(error) => {
            if desktop_ctx {
                emit_core_fatal(
                    "storage/not_writable",
                    &format!("会话库不可打开：{}（{error}）", display_path(&root)),
                )
            }
            return Err(error.into());
        }
    };
    let state = Arc::new(owo_agent_server::AppState::new(
        agent,
        store,
        root.join("traces"),
        root.clone(),
        workspace.clone(),
    ));
    // R8：强杀恢复——陈旧 pid 文件清理；检测到存活实例则显式拒绝双开。
    if let Some(recovery) = owo_agent_server::shutdown::recover_force_kill(&root)? {
        tracing::warn!(
            "检测到强杀残留（pid={:?}），已清理 pid 文件并恢复干净状态",
            recovery.stale_pid
        );
    }
    let _pid_file = owo_agent_server::shutdown::PidFile::create(&root)?;
    // §13 批次三（R10 持久化组接线）：启动恢复用量快照（records/budgets/硬熔断）。
    let restored_usage = owo_agent_server::restore_usage_snapshot(&root);
    if restored_usage > 0 {
        tracing::info!("用量快照已恢复：{restored_usage} 条记录（崩溃后续接）");
    }
    // §13 批次六（遥测接线）：应用 settings.json 遥测开关（默认关）。
    owo_agent_server::apply_telemetry_setting(settings.telemetry_enabled == Some(true));
    // §13 批次八（R10 文件日志接线）：轮转落盘 + 生命周期审计事件。
    owo_agent_server::init_server_file_logging(&root);
    owo_agent_server::logging_lifecycle_audit("server_start", "服务启动完成（恢复/开关应用后）");
    // R8：优雅关闭接线——停止接收 → 完成在途 → flush 审计 → 用量落盘 → 清理 pid → 退出。
    let shutdown_state = Arc::clone(&state);
    let shutdown_root = root.clone();
    tokio::spawn(async move {
        let gate = Arc::clone(&shutdown_state.shutdown_gate);
        gate.wait_shutdown_request().await;
        tracing::info!("收到关闭请求：等待在途回合完成（上限 30s）");
        let remaining = gate.await_drain(std::time::Duration::from_secs(30)).await;
        if remaining > 0 {
            tracing::warn!("仍有 {remaining} 个在途回合超时未完成，强制执行退出");
        }
        owo_agent_server::flush_audit(&shutdown_state);
        if let Some(path) = owo_agent_server::persist_usage_snapshot(&shutdown_root) {
            tracing::info!("用量快照已落盘：{}", path.display());
        }
        owo_agent_server::logging_lifecycle_audit("server_stop", "服务优雅关闭（审计/用量落盘后）");
        owo_agent_server::close_server_file_logging();
        // process::exit 不执行 Drop，显式清理 pid 文件（强杀残留仍由 recover_force_kill 兜底）。
        let _ = std::fs::remove_file(shutdown_root.join("server.pid"));
        tracing::info!("审计已 flush，服务退出");
        std::process::exit(0);
    });
    // 启动时同步插件禁用状态到 Agent 工具前缀（热卸载重启后仍生效）。
    if let Ok(plugin_state) = state.plugin_state.lock() {
        for id in plugin_state.disabled_ids() {
            let prefix = owo_agent_core::tools::mcp_tool_prefix(&id);
            state.agent.set_tool_prefix_enabled(&prefix, false);
        }
    }
    let observer_state = Arc::clone(&state);
    tokio::spawn(async move {
        owo_agent_server::start_observer(observer_state).await;
    });
    let automation_state = Arc::clone(&state);
    tokio::spawn(async move {
        owo_agent_server::start_automation_loop(automation_state).await;
    });
    let memory_state = Arc::clone(&state);
    tokio::spawn(async move {
        owo_agent_server::start_memory_observer(memory_state).await;
    });
    // §13 批次三：用量定时落盘循环（每小时；与 R10 恢复/关闭落盘三面闭环）。
    let usage_persist_state = Arc::clone(&state);
    tokio::spawn(async move {
        owo_agent_server::start_usage_persistence_loop(usage_persist_state).await;
    });
    let app = owo_agent_server::build_router(Arc::clone(&state));
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!("owo-agent server listening on http://{bound}");
    // §4.2 实例握手：路由/数据库/凭据初始化完成且端口已绑定后，向 stdout 打印
    // 恰好一行 core_ready JSON。桌面壳以该行（而非"TCP 能连"）判定服务就绪，
    // 并据此取得实际端口（--port 0 时由系统分配）。stdout 其他内容不受影响。
    println!(
        "{}",
        serde_json::json!({
            "event": "core_ready",
            "pid": std::process::id(),
            "port": bound.port(),
            "api_version": owo_agent_server::OWO_API_VERSION,
            "build_id": resolve_build_id(),
            "instance_id": std::env::var("OWO_DESKTOP_INSTANCE_ID")
                .unwrap_or_default()
                .trim()
                .to_string(),
        })
    );
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let result = axum::serve(listener, app).await;
    // 服务退出：终止全部 MCP stdio 子进程，不留孤儿进程。
    let shutdown_errors = state.agent.shutdown_all_mcp().await;
    for (name, error) in shutdown_errors {
        tracing::warn!("MCP 服务器 {name} 关闭失败：{error}");
    }
    result?;
    Ok(())
}

#[derive(Args)]
pub(crate) struct InitArgs {
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// 已存在时覆盖
    #[arg(long)]
    force: bool,
}

pub(crate) fn run_init(args: InitArgs) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = args.workspace.canonicalize()?;
    let target = workspace.join("AGENTS.md");
    if target.exists() && !args.force {
        println!("{} {}", "AGENTS.md 已存在".yellow(), target.display());
        println!("如需覆盖请使用 --force");
        return Ok(());
    }
    std::fs::write(&target, AGENTS_TEMPLATE)?;
    println!("{} {}", "已生成".green(), target.display());
    Ok(())
}
