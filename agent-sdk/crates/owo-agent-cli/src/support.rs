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

pub(crate) fn build_agent(
    workspace: &std::path::Path,
    model: &str,
    read_only: bool,
) -> Result<Agent, Box<dyn std::error::Error>> {
    let root = ensure_data_root(None, workspace);
    let settings = Settings::load(workspace);
    let mut skills = SkillRegistry::discover(workspace, &root);
    apply_disabled_skills(&mut skills, &settings);
    build_agent_with_mcp(
        workspace,
        model,
        read_only,
        &[],
        &skills,
        &settings.deny_commands,
    )
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
    let mut agent = Agent::new(provider, ToolRegistry::new(), policy, config);
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

pub(crate) async fn connect_mcp_clients(
    configs: &[McpServerConfig],
) -> Vec<(String, Arc<tokio::sync::Mutex<McpClient>>)> {
    // 服务端必须先进入可用状态；外部 MCP 的不可达/握手卡住不能无限阻塞
    // 本地 HTTP 监听和桌面壳健康检查。失败的可选 MCP 保持降级，后续重启可重试。
    const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let mut clients = Vec::new();
    for config in configs {
        match tokio::time::timeout(CONNECT_TIMEOUT, McpClient::connect(config)).await {
            Ok(Ok(client)) => {
                let tools = client.tools();
                // §5.2：连接成功后先按 config 声明宿主可信只读（server+tool+schema hash），
                // 后续 register 时 hash 匹配的 readOnlyHint 才允许降级为 Read。
                let declared =
                    owo_agent_core::tool_effects::declare_trusted_from_config(config, &tools);
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
            Ok(Err(error)) => println!("{} MCP {} 连接失败：{error}", "✘".red(), config.name),
            Err(_) => println!(
                "{} MCP {} 在 {} 秒内未完成连接，已跳过（不阻塞本地服务启动）",
                "✘".red(),
                config.name,
                CONNECT_TIMEOUT.as_secs()
            ),
        }
    }
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
