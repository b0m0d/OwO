// §12.3 CLI 拆分批次四：共享基建（自 main.rs 机械外移，零行为变化）。
// Agent/MCP/数据目录/模型解析等跨命令域共用设施；经 crate::support 显式引用。

use async_trait::async_trait;
use colored::Colorize;
use owo_agent_core::permissions::{Approver, Decision, PermissionRequest};
use owo_agent_core::{
    Agent, AgentConfig, McpClient, McpServerConfig, OpenAiCompatibleConfig,
    OpenAiCompatibleProvider, PluginManifest, Policy, Settings, SkillRegistry, ToolRegistry,
};
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) const AGENTS_TEMPLATE: &str = r#"# AGENTS.md

<!-- 由 owo-agent /init 生成，按项目实际情况修改。
     该文件会被 Agent 在每次会话开始时注入，作为项目级规则。 -->

## 项目说明

- 一句话描述本项目做什么。

## 开发规则

- 写清楚构建命令、测试命令与代码约定。
- 说明哪些目录/文件禁止修改。
"#;

/// 默认通用文本/推理模型：与核心 gateway 的 `DEFAULT_MODEL_ID` 对齐（GLM / 智谱 BigModel，
/// OpenAI 兼容端点已内置默认）；可在工作台设置或 OPENAI_MODEL 环境变量覆盖。
pub(crate) const DEFAULT_MODEL: &str = owo_agent_core::gateway::DEFAULT_MODEL_ID;

pub(crate) fn apply_egress_setting(settings: &Settings) {
    if !settings.egress.cloud_enabled {
        std::env::set_var("OWO_CLOUD_ENABLED", "false");
    }
}

/// 把 settings.json 的禁用技能列表注入技能注册表（进程内共享集合，Web 切换即时生效）。
pub(crate) fn apply_disabled_skills(skills: &mut SkillRegistry, settings: &Settings) {
    let disabled = Arc::new(Mutex::new(
        settings
            .skills
            .disabled
            .iter()
            .cloned()
            .collect::<HashSet<_>>(),
    ));
    skills.set_disabled(disabled);
}

/// 开发环境下的内置技能包根目录：`<repo>/agent-sdk/skills`。
pub(crate) fn builtin_skills_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OWO_SKILLS_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("skills");
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|parent| parent.parent())
        .map(|root| root.join("skills"))
        .unwrap_or_else(|| PathBuf::from("skills"))
}

pub(crate) fn run_async<F>(future: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: Future<Output = Result<(), Box<dyn std::error::Error>>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(future)
}

pub(crate) fn resolve_model(option: Option<String>, settings_model: Option<&str>) -> String {
    option
        .or_else(|| std::env::var("OPENAI_MODEL").ok())
        .or_else(|| settings_model.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

pub(crate) fn build_agent_with_mcp(
    workspace: &std::path::Path,
    model: &str,
    read_only: bool,
    mcp_clients: &[(String, Arc<tokio::sync::Mutex<McpClient>>)],
    skills: &SkillRegistry,
    deny_commands: &[String],
) -> Result<Agent, Box<dyn std::error::Error>> {
    let mut config = OpenAiCompatibleConfig::from_env()?;
    config.model = model.to_string();
    // R9：模型网关韧性（重试/退避/熔断/failover 强→次选云→本地）。
    let provider: Arc<dyn owo_agent_core::ModelProvider> = Arc::new(
        owo_agent_core::gateway::ResilientProvider::from_config(config)?,
    );
    assemble_agent(
        provider,
        workspace,
        model,
        read_only,
        mcp_clients,
        skills,
        deny_commands,
    )
}

/// §3.4（R3-B 契约）：桌面 `serve` 专用构建——缺少模型凭据时**不拒绝启动**，
/// 降级为 `UnconfiguredModelProvider`（core ready，诊断/设置/会话/工具全部可用，
/// 模型调用返回稳定码 `provider/not_configured`，UI 据此呈现模型配置引导）。
/// 其余 CLI 命令（chat/turn/repl/tui）走上面的严格路径：缺凭据立刻报错，行为不变。
pub(crate) fn build_agent_with_mcp_serve(
    workspace: &std::path::Path,
    model: &str,
    read_only: bool,
    mcp_clients: &[(String, Arc<tokio::sync::Mutex<McpClient>>)],
    skills: &SkillRegistry,
    deny_commands: &[String],
) -> Result<Agent, Box<dyn std::error::Error>> {
    let provider: Arc<dyn owo_agent_core::ModelProvider> = match OpenAiCompatibleConfig::from_env()
    {
        Ok(mut config) => {
            config.model = model.to_string();
            Arc::new(owo_agent_core::gateway::ResilientProvider::from_config(
                config,
            )?)
        }
        Err(error) => {
            eprintln!("警告：{error}——模型提供商降级为未配置（serve 继续提供诊断/设置/会话）");
            Arc::new(owo_agent_core::UnconfiguredModelProvider::new(error))
        }
    };
    assemble_agent(
        provider,
        workspace,
        model,
        read_only,
        mcp_clients,
        skills,
        deny_commands,
    )
}

/// 共享装配体：策略/预算/CAS/MCP 挂载/技能/审批链（两条构建路径唯一实现）。
fn assemble_agent(
    provider: Arc<dyn owo_agent_core::ModelProvider>,
    workspace: &std::path::Path,
    model: &str,
    read_only: bool,
    mcp_clients: &[(String, Arc<tokio::sync::Mutex<McpClient>>)],
    skills: &SkillRegistry,
    deny_commands: &[String],
) -> Result<Agent, Box<dyn std::error::Error>> {
    let mut policy = if read_only {
        Policy::read_only(workspace.to_path_buf())
    } else {
        Policy::new(workspace.to_path_buf())
    };
    for fragment in deny_commands {
        policy.add_deny_command(fragment.clone());
    }
    let mut config = AgentConfig::default();
    if let Ok(value) = std::env::var("OWO_TOKEN_BUDGET") {
        if let Ok(budget) = value.parse() {
            config.token_budget = budget;
        }
    }
    // 配置文件（桌面壳读 config.json 后注入）优先于上面这些历史变量：
    // 用户在文件里写的上下文窗口/输出上限/温度/超时是显式意图，必须赢。
    config = config.with_env_overrides();
    if let Ok(value) = std::env::var("OWO_KEEP_RECENT") {
        if let Ok(keep) = value.parse() {
            config.keep_recent = keep;
        }
    }
    // §9.2：turn 级统一截止时间（秒；0/未设 = 不限时，仅记账）。
    if let Ok(value) = std::env::var("OWO_TURN_DEADLINE_SECS") {
        if let Ok(secs) = value.parse::<u64>() {
            if secs > 0 {
                config.turn_deadline = Some(std::time::Duration::from_secs(secs));
            }
        }
    }
    let registry = builtin_registry_for_workspace(workspace);
    let mut agent = Agent::new(provider, registry, policy, config);
    // §9.3：超大工具结果落 CAS artifact（workspace/.owo/artifacts）。
    if let Ok(store) =
        owo_agent_core::cas_store::CasStore::new(workspace.join(".owo").join("artifacts"))
    {
        agent = agent.with_artifact_store(Arc::new(store));
    }
    // 统一走 Agent::register_mcp_tools：记录客户端到进程生命周期注册表（进程级热卸载），
    // 同时把工具挂进 Agent 注册表。
    for (server_name, client) in mcp_clients {
        let tools = client
            .try_lock()
            .map_err(|_| format!("MCP 客户端 {server_name} 忙碌"))?
            .tools();
        agent.register_mcp_tools(server_name, Arc::clone(client), tools);
    }
    agent.set_skills(skills.clone());
    attach_auto_review(&mut agent, model);
    Ok(agent)
}

fn builtin_registry_for_workspace(workspace: &std::path::Path) -> ToolRegistry {
    let capabilities = Settings::load(workspace).tool_capabilities;
    let mut registry = ToolRegistry::new();
    if capabilities.desktop_observation {
        registry.register_desktop_observation_tools();
    }
    if capabilities.desktop_control {
        registry.register_desktop_control_tools();
    }
    if capabilities.browser {
        registry.register_browser_tools();
    }
    registry
}

/// 独立审批模型（Auto-review）：
/// - 默认只挂启发式预筛（零模型成本，命中已知注入/高危模式直接 Deny）；
/// - `OWO_AUTO_REVIEW=1` 时追加独立模型复审（`OWO_REVIEW_MODEL` 可选覆盖）。
pub(crate) fn attach_auto_review(agent: &mut Agent, model: &str) {
    if std::env::var("OWO_AUTO_REVIEW").as_deref() == Ok("1") {
        let mut config = match OpenAiCompatibleConfig::from_env() {
            Ok(config) => config,
            Err(error) => {
                eprintln!("警告：Auto-review 模型初始化失败（{error}），仅启用启发式预筛");
                agent.set_reviewer(Some(Arc::new(owo_agent_core::AutoReviewChain::new(None))));
                return;
            }
        };
        config.model = std::env::var("OWO_REVIEW_MODEL").unwrap_or_else(|_| model.to_string());
        let provider = match OpenAiCompatibleProvider::new(config) {
            Ok(provider) => Arc::new(provider),
            Err(error) => {
                eprintln!("警告：Auto-review 模型初始化失败（{error}），仅启用启发式预筛");
                agent.set_reviewer(Some(Arc::new(owo_agent_core::AutoReviewChain::new(None))));
                return;
            }
        };
        agent.set_reviewer(Some(Arc::new(owo_agent_core::AutoReviewChain::from_model(
            provider,
        ))));
    } else {
        agent.set_reviewer(Some(Arc::new(owo_agent_core::AutoReviewChain::new(None))));
    }
}

#[cfg(test)]
mod tool_capability_tests {
    use super::builtin_registry_for_workspace;
    use owo_agent_core::settings::AgentToolCapabilities;
    use owo_agent_core::Settings;

    #[test]
    fn workspace_settings_opt_in_optional_tool_groups() {
        let workspace =
            std::env::temp_dir().join(format!("owo-tool-capabilities-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();

        let defaults = builtin_registry_for_workspace(&workspace);
        let default_names: Vec<String> =
            defaults.specs().into_iter().map(|spec| spec.name).collect();
        assert!(default_names.contains(&"run_command".to_string()));
        assert!(!default_names.contains(&"screen_ocr".to_string()));
        assert!(!default_names.contains(&"desktop_click".to_string()));
        assert!(!default_names.contains(&"browser_navigate".to_string()));

        Settings {
            tool_capabilities: AgentToolCapabilities {
                desktop_observation: true,
                desktop_control: false,
                browser: true,
            },
            ..Settings::default()
        }
        .save(&workspace)
        .unwrap();
        let opted_in = builtin_registry_for_workspace(&workspace);
        let opted_names: Vec<String> = opted_in.specs().into_iter().map(|spec| spec.name).collect();
        assert!(opted_names.contains(&"screen_ocr".to_string()));
        assert!(opted_names.contains(&"browser_navigate".to_string()));
        assert!(!opted_names.contains(&"desktop_click".to_string()));

        let _ = std::fs::remove_dir_all(&workspace);
    }
}

pub(crate) async fn connect_mcp_clients(
    configs: &[McpServerConfig],
) -> Vec<(String, Arc<tokio::sync::Mutex<McpClient>>)> {
    // 服务端必须先进入可用状态；外部 MCP 的不可达/握手卡住不能无限阻塞本地 HTTP
    // 监听与桌面壳健康检查。失败的可选 MCP 保持降级，后续重启可重试。
    //
    // §6.2/P2：改为**并发连接 + 全局 3s 上限**。旧实现逐个串行 3s，N 个坏 MCP 就是
    // 3N 秒——"坏 MCP 不影响 3 秒内 ready"在 N≥2 时直接不成立。并发 + 全局预算后，
    // 无论多少个坏 MCP，就绪延迟都被钉在 3s 内。
    const TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let mut set = tokio::task::JoinSet::new();
    for config in configs {
        let config = config.clone();
        set.spawn(async move {
            let result = McpClient::connect(&config).await;
            (config, result)
        });
    }
    let mut clients = Vec::new();
    let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            println!(
                "{} 仍有 MCP 在 {} 秒内未完成连接，已跳过（不阻塞本地服务启动）",
                "✘".red(),
                TOTAL_TIMEOUT.as_secs()
            );
            break;
        }
        match tokio::time::timeout(remaining, set.join_next()).await {
            Ok(Some(Ok((config, Ok(client))))) => {
                let tools = client.tools();
                // §5.2：连接成功后先按 config 声明宿主可信只读（server+tool+schema hash），
                // 后续 register 时 hash 匹配的 readOnlyHint 才允许降级为 Read。
                let declared =
                    owo_agent_core::tool_effects::declare_trusted_from_config(&config, &tools);
                if declared > 0 {
                    println!(
                        "{} MCP {}：{declared} 个工具获宿主可信只读声明（schema hash 校验）",
                        "✓".green(),
                        config.name
                    );
                }
                println!(
                    "{} MCP {}（工具 {} 个）",
                    "已连接".green(),
                    config.name,
                    tools.len()
                );
                clients.push((
                    config.name.clone(),
                    Arc::new(tokio::sync::Mutex::new(client)),
                ));
            }
            Ok(Some(Ok((config, Err(error))))) => {
                println!("{} MCP {} 连接失败：{error}", "✘".red(), config.name)
            }
            Ok(Some(Err(join_error))) => {
                println!("{} MCP 连接任务异常：{join_error}", "✘".red())
            }
            Ok(None) => break, // 全部完成
            Err(_) => {
                println!(
                    "{} MCP 连接总时长超过 {} 秒，未完成的已跳过（不阻塞本地服务启动）",
                    "✘".red(),
                    TOTAL_TIMEOUT.as_secs()
                );
                break;
            }
        }
    }
    // 超时后终止仍在握手的连接任务（与旧实现丢弃 future 的语义一致，避免后台挂起）。
    set.abort_all();
    clients
}

pub(crate) fn mcp_config_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join("mcp-servers.json")
}

pub(crate) fn load_mcp_configs(root: &std::path::Path) -> Vec<McpServerConfig> {
    std::fs::read_to_string(mcp_config_path(root))
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

pub(crate) fn save_mcp_configs(root: &std::path::Path, configs: &[McpServerConfig]) {
    if let Ok(content) = serde_json::to_string_pretty(configs) {
        let _ = std::fs::write(mcp_config_path(root), content);
    }
}

pub(crate) fn data_root(override_dir: Option<PathBuf>) -> PathBuf {
    override_dir
        .or_else(|| std::env::var("OWO_AGENT_DATA").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var("LOCALAPPDATA")
                .map(|dir| PathBuf::from(dir).join("OwO").join("Agent"))
                .unwrap_or_else(|_| PathBuf::from("data/agent"))
        })
}

/// 优先使用默认数据目录；不可写时回退到工作区 `.owo-agent/`。
pub(crate) fn ensure_data_root(
    override_dir: Option<PathBuf>,
    workspace: &std::path::Path,
) -> PathBuf {
    let preferred = data_root(override_dir);
    if std::fs::create_dir_all(&preferred).is_ok() {
        return preferred;
    }
    let fallback = workspace.join(".owo-agent");
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

/// §2.3/P1：确保有可用 Daemon 并返回**共享**客户端。
///
/// 规则（指南 §2.3）：
///   1. 先读 discovery；存活且 API 兼容 → 直接复用（绝不另起第二个 DB writer）；
///   2. 缺失/陈旧 → 以当前 exe 启动分离的 `serve` 进程，等待 discovery 就绪；
///   3. API 版本不兼容 → 明确报错（禁止静默连到旧实例）。
///
/// 客户端只依赖 protocol；本函数是 CLI 侧"单实例启动协议"的唯一实现，
/// turn/repl/daemon 子命令都经它，不再各自构造 Agent/打开 SQLite。
pub(crate) async fn ensure_daemon_client(
    data_root: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<owo_agent_client::AgentClient, Box<dyn std::error::Error>> {
    let expected = owo_build_info::API_VERSION;
    match owo_agent_client::connect(data_root, Some(expected)).await {
        Ok(client) => return Ok(client),
        Err(owo_agent_client::ClientError::ApiVersionMismatch {
            expected: want,
            actual,
        }) => {
            return Err(format!(
                "已运行 Daemon 的 API 版本为 {actual}，本客户端期望 {want}：请升级或重启 Daemon（禁止静默另起旧实例）"
            )
            .into());
        }
        Err(owo_agent_client::ClientError::NotFound(_))
        | Err(owo_agent_client::ClientError::Discovery(_)) => {
            // 无可用 Daemon：启动一个。
        }
        Err(other) => return Err(other.into()),
    }
    spawn_daemon(data_root, workspace)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut last = String::from("（尚未出现发现文件）");
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        match owo_agent_client::connect(data_root, Some(expected)).await {
            Ok(client) => return Ok(client),
            Err(owo_agent_client::ClientError::ApiVersionMismatch {
                expected: want,
                actual,
            }) => {
                return Err(format!(
                    "新启动 Daemon 的 API 版本为 {actual}，本客户端期望 {want}：拒绝连接"
                )
                .into());
            }
            Err(error) => last = error.to_string(),
        }
    }
    Err(format!("等待 Daemon 就绪超时（60s）：{last}").into())
}

/// 分离启动 `owo-agent serve`（不占用调用方 stdout，退出不随父进程）。
fn spawn_daemon(
    data_root: &std::path::Path,
    workspace: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let logs = data_root.join("logs");
    std::fs::create_dir_all(&logs)?;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let out = std::fs::File::create(logs.join(format!("daemon-{stamp}.out.log")))?;
    let err = out.try_clone()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("serve")
        .arg("--port")
        .arg("0")
        .arg("--output")
        .arg("jsonl")
        .arg("--workspace")
        .arg(workspace)
        .env("OWO_AGENT_DATA", data_root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    command.spawn()?;
    Ok(())
}

/// R3-B（§3.4 `storage/not_writable`）：严格版数据根准备——**不**静默迁移。
/// 首选目录不可写即返回 Err（原因含脱敏路径），由调用方决定是否降级；
/// 桌面壳上下文必须用本函数：悄悄把会话/审计搬进用户项目目录是"看起来成功"
/// 的存储缺陷（工作区 `.owo-agent` 回退仅限非桌面 CLI，保持既有行为）。
pub(crate) fn ensure_data_root_checked(override_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    let preferred = data_root(override_dir);
    std::fs::create_dir_all(&preferred)
        .map(|()| preferred.clone())
        .map_err(|error| format!("数据目录不可写：{}（{error}）", display_path(&preferred)))
}

pub(crate) fn display_path(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
}

/// 归一化一行交互输入：去首尾空白并剥掉可能的前导 BOM。
///
/// 为什么需要：PowerShell 5.1 把管道内容写给原生子进程 stdin 时，首个写入可能带
/// UTF-8 BOM（U+FEFF）。Rust 的 `trim()` 不把 U+FEFF 当空白，于是 `strip_prefix('/')`
/// 失败——`/new` 会被当成普通提示词触发一次模型回合（实测踩到）。
pub(crate) fn normalize_input_line(line: &str) -> String {
    line.trim()
        .trim_start_matches('\u{feff}')
        .trim()
        .to_string()
}

/// §4.2/§5.1/§6.1.2/§7.1：core_ready 行的 build_id 解析——委托
/// `owo_build_info::identity()` 单一链（① OWO_BUILD_INFO 覆写 ② 编译期
/// 烧录 ③ cwd 遗留 build-info.json）。CLI 不再自持第二份回退链；与 server
/// /health.build 严格同源（历史缺陷：CLI 编译期优先、server 覆写优先，
/// 发布链覆写 build-info.json 时两侧会报告不同 build id）。
pub(crate) fn resolve_build_id() -> String {
    owo_build_info::identity().commit
}

pub(crate) fn merge_plugin_mcp(
    plugins: &[(std::path::PathBuf, PluginManifest)],
    configs: &mut Vec<McpServerConfig>,
) {
    for (manifest_path, manifest) in plugins {
        if let Some(config) = owo_agent_core::plugin_mcp_config(manifest_path, manifest) {
            if !configs.iter().any(|existing| existing.name == config.name) {
                configs.push(config);
            }
        }
    }
}

/// 共享 stdin 读取器：REPL 主循环与审批提示共用同一缓冲，避免管道输入被吞行。
#[derive(Clone)]
pub(crate) struct SharedStdin {
    inner: Arc<tokio::sync::Mutex<tokio::io::BufReader<tokio::io::Stdin>>>,
}

impl SharedStdin {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(tokio::sync::Mutex::new(tokio::io::BufReader::new(
                tokio::io::stdin(),
            ))),
        }
    }

    pub(crate) async fn read_line(&self, output: &mut String) -> std::io::Result<usize> {
        use tokio::io::AsyncBufReadExt;
        let mut guard = self.inner.lock().await;
        guard.read_line(output).await
    }
}

pub(crate) struct ConsoleApprover {
    pub(crate) stdin: SharedStdin,
}

#[async_trait]
impl Approver for ConsoleApprover {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        use std::io::Write;
        print!(
            "  {} 允许 {} 执行 {}？[y/N] ",
            "审批".yellow(),
            request.level.label(),
            request.tool
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if self.stdin.read_line(&mut line).await.is_ok() {
            match line.trim().to_lowercase().as_str() {
                "y" | "yes" => return Decision::Allow,
                _ => return Decision::Deny,
            }
        }
        Decision::Deny
    }
}
