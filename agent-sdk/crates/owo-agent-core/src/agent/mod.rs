use crate::audit::AuditLog;
use crate::autoreview::{ReviewVerdict, Reviewer};
use crate::context::{build_system_prompt, load_project_rules};
use crate::deadline::{DeadlineBudget, Phase, PhaseBudgets, PhaseTiming};
use crate::error::AgentError;
use crate::gateway::{
    ChatMessage, ModelCallMetadata, ModelOutput, ModelProvider, StreamChunk, TokenUsage,
};
use crate::injection::sanitize_tool_result;
use crate::permissions::{Approver, Decision, PermissionRequest, Policy};
use crate::session::Session;
use crate::skill::SkillRegistry;
use crate::subagent::SubagentRunner;
use crate::tool_effects::EffectClass;
use crate::tools::{
    ToolApprovalGrant, ToolCapabilityContext, ToolContext, ToolHostService, ToolRegistry,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

mod config;
mod single_review;
mod single_manual_acceptance;

#[cfg(test)]
mod tests;

pub use config::AgentConfig;
pub(crate) use config::DEFAULT_BOUNDED_TOOL_CALL_CAP;
use config::*;

/// 达到最大回合数后的收尾指令：不再调用工具，强制产出可见结论
/// （审查/分析类任务据此给出结构化报告；信息不足时列出需要用户澄清的问题）。
const WRAP_UP_PROMPT: &str = "你已达到本次任务的最大执行步数上限，现在必须停止调用工具，\
     直接用 Markdown 输出最终结论：1) 已完成的工作与关键发现（审查/分析类任务给出结构化报告：结论、证据、风险）；\
     2) 仍未完成或未验证的部分；3) 如果信息不足，列出需要用户澄清的具体问题。不要再请求任何工具。";

/// 空回答的静默重试次数：第一次空响应直接再问一次（不打扰用户），仍为空才走摘要兜底。
const EMPTY_REPLY_RETRIES: usize = 1;

/// 空回答重试时的追加指令：强制产出可见结论，而不是继续思考或调工具。
const EMPTY_REPLY_RETRY_PROMPT: &str = "（系统提示）你上一条回复没有产生任何可见内容。\
     请不要再调用工具，立即用 Markdown 直接输出：1) 当前已完成的工作与结论；\
     2) 仍未完成或不确定的部分。";

/// 兜底摘要中单条工具动作的参数预览长度上限。
const FALLBACK_ACTION_PREVIEW_CHARS: usize = 90;
/// 兜底摘要中最多列出的工具动作条数（去重后）。
const FALLBACK_ACTION_LIMIT: usize = 20;

/// Default safety cap for nested workers. Main user turns are uncapped by default,
/// while delegated subagents always have a finite independent request budget.
fn nested_turn_cap(configured: usize) -> usize {
    if configured == 0 {
        crate::subagent::MAX_SUBAGENT_TURNS
    } else {
        configured.min(crate::subagent::MAX_SUBAGENT_TURNS)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandExecutionReceipt {
    pub command_sha256: String,
    pub exit_code: i32,
    pub result_sha256: String,
    /// ToolHost execution duration; None means this is a legacy receipt without budget evidence.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub workspace_hashes_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator_version: Option<String>,
    #[serde(default)]
    pub workspace_hashes: std::collections::BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnEvent {
    ModelCall,
    TokenDelta {
        delta: String,
    },
    /// 深度思考增量（模型 reasoning；不写入对话历史，仅 UI 展示）。
    ReasoningDelta {
        delta: String,
    },
    /// 任务计划更新（todo 工具整表替换后外发）：前端渲染步骤进度。
    PlanUpdate {
        steps: serde_json::Value,
    },
    Compaction {
        summary: String,
    },
    PermissionRequest(PermissionRequest),
    ToolStart {
        id: String,
        tool: String,
        /// 参数预览（脱敏、截断）：供 CLI/前端实时展示"正在用什么参数调用"。
        /// 序列化向后兼容（缺省为 None）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args_preview: Option<String>,
    },
    ToolResult {
        id: String,
        tool: String,
        ok: bool,
        error: Option<String>,
        /// 结果预览（截断，纯展示）：随事件下发步骤时间线，前端 chip 可展开；
        /// 写回模型上下文的净化仍由 `sanitize_tool_result` 负责。
        /// 序列化向后兼容（缺省为 None）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_receipt: Option<CommandExecutionReceipt>,
    },
    Final {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutcome {
    /// Per-request model identity, provider request id, usage, latency, and outcome.
    #[serde(default)]
    pub model_calls: Vec<ModelCallRecord>,
    pub final_text: Option<String>,
    /// Host assessment: response completion never implies code delivery acceptance.
    #[serde(default)]
    pub completion_status: owo_agent_protocol::CompletionStatusV1,
    /// True when an explicitly configured turn cap forced the wrap-up request.
    #[serde(default)]
    pub reached_model_turn_limit: bool,
    pub steps: usize,
    pub events: Vec<TurnEvent>,
    pub prompt: String,
    pub started_at: String,
    pub duration_ms: u64,
    /// 本回合按请求累加的模型 token 用量。
    #[serde(default)]
    pub usage: TokenUsage,
    /// true 表示回合内每个模型请求都返回了可归属的 usage 元数据。
    #[serde(default)]
    pub usage_known: bool,
    /// §9.3：阶段耗时瀑布（model/approval/tool/persistence，按发生顺序）。
    #[serde(default)]
    pub phase_timings: Vec<crate::deadline::PhaseTiming>,
    /// §9.3：工具面稳定指纹（SHA-256；schema 缓存复用的 key 基础）。
    #[serde(default)]
    pub tools_fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCallRecord {
    pub metadata: ModelCallMetadata,
    pub succeeded: bool,
}

/// Agent 核心：执行循环 + 工具注册表 + 权限策略 + 审计。
///
/// 原始注册表句柄不属于下游公开面：
///
/// ```compile_fail
/// let _ = owo_agent_core::Agent::registry;
/// ```
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    /// 工具注册表（RwLock：MCP 服务器热连接/热卸载时无需重建 Agent）。
    registry: Arc<RwLock<ToolRegistry>>,
    /// 受信工具执行门面：能力签发、执行和 receipt 统一从此处经过。
    tool_host: ToolHostService,
    /// 插件热卸载：已禁用工具前缀（模型不可见、直接调用被拒）。
    disabled_tool_prefixes: Arc<RwLock<HashSet<String>>>,
    /// MCP 客户端进程生命周期注册表（进程级热卸载/退出清理）。
    mcp_clients: Arc<crate::mcp::McpRegistry>,
    /// §10：per-server 健康跟踪（熔断/限流/幂等重试）。
    mcp_health: Arc<crate::mcp_health::McpHealthTracker>,
    /// 独立审批模型（Auto-review）：Ask 先经审查链，Deny 不打扰用户。
    reviewer: Option<Arc<dyn Reviewer>>,
    policy: Policy,
    audit: Arc<Mutex<AuditLog>>,
    config: AgentConfig,
    skills: SkillRegistry,
    elements: Arc<Mutex<crate::ElementRegistry>>,
    /// §9.3：超大工具结果 artifact store（CAS）；None = 维持盲截断旧行为。
    artifact_store: Option<Arc<crate::cas_store::CasStore>>,
    /// A2-1 hooks 生命周期扩展点（settings.json 的 `hooks` 灌入；空 = 无 hook）。
    /// RwLock：服务运行中（Arc<Agent>）也能重灌（settings 保存后热生效）。
    hooks: RwLock<crate::hooks::HookManager>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        registry: ToolRegistry,
        policy: Policy,
        config: AgentConfig,
    ) -> Self {
        let registry = Arc::new(RwLock::new(registry));
        let audit = Arc::new(Mutex::new(AuditLog::default()));
        let tool_host = ToolHostService::new(Arc::clone(&registry), Arc::clone(&audit));
        Self {
            provider,
            registry,
            tool_host,
            disabled_tool_prefixes: Arc::new(RwLock::new(HashSet::new())),
            mcp_clients: Arc::new(crate::mcp::McpRegistry::new()),
            mcp_health: Arc::new(crate::mcp_health::McpHealthTracker::new(
                crate::mcp_health::McpHealthConfig::default(),
            )),
            reviewer: None,
            policy,
            audit,
            config,
            skills: SkillRegistry::default(),
            elements: Arc::new(Mutex::new(crate::ElementRegistry::new())),
            artifact_store: None,
            hooks: RwLock::new(crate::hooks::HookManager::default()),
        }
    }

    /// A2-1：灌入 hooks 配置（settings.json 的 `hooks` 数组；exit 2 = 阻断）。
    /// 快照语义：clone 后释放锁，hook 执行（可达 10s）不阻塞重灌。
    pub fn set_hooks(&self, hooks: crate::hooks::HookManager) {
        if let Ok(mut slot) = self.hooks.write() {
            *slot = hooks;
        }
    }

    /// 当前 hooks 快照（读锁即取即放，避免跨 await 持锁）。
    fn hooks_snapshot(&self) -> crate::hooks::HookManager {
        self.hooks
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// §9.3：挂载 artifact store（CAS）。超大工具结果以「指针+预览」回填模型，
    /// 完整内容按内容寻址落盘；不挂载则维持盲截断旧行为。
    pub fn with_artifact_store(mut self, store: Arc<crate::cas_store::CasStore>) -> Self {
        self.artifact_store = Some(store);
        self
    }

    pub fn set_skills(&mut self, skills: SkillRegistry) {
        self.skills = skills;
    }

    /// 设置独立审批模型（None 表示关闭 Auto-review，恢复纯人工审批）。
    pub fn set_reviewer(&mut self, reviewer: Option<Arc<dyn Reviewer>>) {
        self.reviewer = reviewer;
    }

    /// 当前是否启用 Auto-review。
    pub fn autoreview_enabled(&self) -> bool {
        self.reviewer.is_some()
    }

    /// 注册 MCP 服务器工具（命名空间 `{server}_{tool}`）；热连接，无需重建 Agent。
    pub fn register_mcp_tools(
        &self,
        server_name: &str,
        client: Arc<tokio::sync::Mutex<crate::mcp::McpClient>>,
        tools: Vec<crate::mcp::McpTool>,
    ) {
        self.mcp_clients.insert(server_name, Arc::clone(&client));
        if let Ok(mut registry) = self.registry.write() {
            registry.register_mcp_tools_with_health(
                server_name,
                client,
                tools,
                Some(Arc::clone(&self.mcp_health)),
            );
        }
    }

    /// A2-2：注册 MCP resources/prompts 泛化工具（与 tools 一并热注册）。
    pub fn register_mcp_extras(
        &self,
        server_name: &str,
        client: Arc<tokio::sync::Mutex<crate::mcp::McpClient>>,
        resources: Vec<crate::mcp::McpResource>,
        prompts: Vec<crate::mcp::McpPrompt>,
    ) {
        if let Ok(mut registry) = self.registry.write() {
            registry.register_mcp_extras(server_name, client, resources, prompts);
        }
    }

    /// MCP 客户端进程注册表（进程级热卸载/状态查询）。
    pub fn mcp_clients(&self) -> Arc<crate::mcp::McpRegistry> {
        Arc::clone(&self.mcp_clients)
    }

    /// §10：per-server 健康跟踪器（熔断/限流状态观测接口面）。
    pub fn mcp_health(&self) -> Arc<crate::mcp_health::McpHealthTracker> {
        Arc::clone(&self.mcp_health)
    }

    /// 热连接 MCP 服务器并注册工具（插件启用/热添加）；返回工具数。
    pub async fn connect_mcp_server(
        &self,
        config: &owo_agent_plugins::McpServerConfig,
    ) -> Result<usize, String> {
        let client = crate::mcp::McpClient::connect(config).await?;
        let tools = client.tools();
        let resources = client.resources();
        let prompts = client.prompts();
        // §5.2：先按 config 声明宿主可信只读（server+tool+schema hash），
        // 随后 register 时 hash 匹配的 readOnlyHint 才能降级为 Read。
        crate::tool_effects::declare_trusted_from_config(config, &tools);
        let tool_count = tools.len();
        let client = Arc::new(tokio::sync::Mutex::new(client));
        self.register_mcp_tools(&config.name, Arc::clone(&client), tools);
        self.register_mcp_extras(&config.name, client, resources, prompts);
        Ok(tool_count)
    }

    /// 进程级热卸载 MCP 服务器：kill stdio 子进程 + 撤销工具（前缀移除且禁用）。
    /// 返回 false 表示服务器本未连接（幂等，不报错）。
    pub async fn shutdown_mcp_server(&self, name: &str) -> Result<bool, String> {
        if !self.mcp_clients.names().iter().any(|n| n == name) {
            return Ok(false);
        }
        let prefix = crate::tools::mcp_tool_prefix(name);
        self.set_tool_prefix_enabled(&prefix, false);
        self.remove_tools_prefix(&prefix);
        self.mcp_clients.shutdown(name).await?;
        Ok(true)
    }

    /// 关闭全部 MCP 客户端（服务退出前调用，防止遗留 stdio 子进程）。
    pub async fn shutdown_all_mcp(&self) -> Vec<(String, String)> {
        self.mcp_clients.shutdown_all().await
    }

    /// 按前缀撤销工具（插件热卸载）；返回移除数量。
    pub fn remove_tools_prefix(&self, prefix: &str) -> usize {
        self.registry
            .write()
            .map(|mut registry| registry.remove_prefix(prefix))
            .unwrap_or(0)
    }

    /// 插件工具前缀启停（热卸载：模型不可见 + 直接调用被拒，无需重建 Agent）。
    pub fn set_tool_prefix_enabled(&self, prefix: &str, enabled: bool) {
        if let Ok(mut prefixes) = self.disabled_tool_prefixes.write() {
            if enabled {
                prefixes.remove(prefix);
            } else {
                prefixes.insert(prefix.to_string());
            }
        }
    }

    /// 工具名是否命中任一禁用前缀。
    pub fn tool_disabled(&self, name: &str) -> bool {
        self.disabled_tool_prefixes
            .read()
            .map(|prefixes| prefixes.iter().any(|prefix| name.starts_with(prefix)))
            .unwrap_or(false)
    }

    /// §9.1：调用能否进入并发组——宿主验证只读（`EffectClass::Read` 且
    /// `host_verified_readonly=true`）。MCP 自报 readOnlyHint 未经验证、
    /// 写/执行/注入、effect 缺失与未知工具一律 `false`（保持串行）。
    fn call_is_concurrent_eligible(&self, call: &crate::gateway::ToolCall) -> bool {
        let Ok(registry) = self.registry.read() else {
            return false;
        };
        let Some(tool) = registry.get(&call.name) else {
            return false;
        };
        let spec = tool.spec();
        matches!(
            spec.effect.as_ref(),
            Some(effect) if effect.class == EffectClass::Read && effect.host_verified_readonly
        )
    }

    /// 当前模型可见工具：注册表全量减去禁用前缀。
    pub fn visible_tool_specs(&self) -> Vec<crate::tools::ToolSpec> {
        self.registry
            .read()
            .map(|registry| {
                registry
                    .specs()
                    .into_iter()
                    .filter(|spec| !self.tool_disabled(&spec.name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 设置共享窗口元素注册表（与 HTTP 感知层共用同一 ID 空间）。
    pub fn set_elements(&mut self, elements: Arc<Mutex<crate::ElementRegistry>>) {
        self.elements = elements;
    }

    pub fn elements(&self) -> Arc<Mutex<crate::ElementRegistry>> {
        Arc::clone(&self.elements)
    }

    pub fn skills(&self) -> &SkillRegistry {
        &self.skills
    }

    /// 当前 Agent 配置（只读快照，供诊断/上下文仪表展示）。
    pub fn config(&self) -> AgentConfig {
        self.config.clone()
    }

    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    /// 直呼子代理（CLI `@explore` / `@subagent`）：独立子会话执行，返回最终文本。
    pub async fn run_subagent(
        &self,
        workspace: &std::path::Path,
        model: &str,
        prompt: &str,
        read_only: bool,
    ) -> Result<String, AgentError> {
        let abort = std::sync::atomic::AtomicBool::new(false);
        // 直呼子代理没有可回传到客户端的审批通道（goal/plan 后台 worker、
        // POST /subagent）：改用工作区范围审批器——读恒放行；写/执行仅在
        // **工作区内**放行，越界一律拒绝；只读模式拒绝全部写/执行。
        // 旧实现用 `AutoApprover { allow: read_only }`，导致 producer 角色的
        // 写/执行被一律拒绝（子代理"跑完了但什么都没改"），或阻塞在永远
        // 到不了的审批上直到超时。
        let approver = crate::permissions::WorkspaceApprover {
            workspace: workspace.to_path_buf(),
            allow_writes: !read_only,
        };
        let runner = SubagentRunner {
            provider: Arc::clone(&self.provider),
            approver: &approver,
            abort: &abort,
            depth: self.config.subagent_depth,
            max_turns: nested_turn_cap(self.config.max_turns),
            model: model.to_string(),
            events: None,
        };
        runner
            .run(workspace, prompt, read_only)
            .await
            .map_err(AgentError::Tool)
    }

    pub fn audit_log(&self) -> Arc<Mutex<AuditLog>> {
        Arc::clone(&self.audit)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// 运行时追加危险命令片段（热生效；重启后由 settings.deny_commands 恢复）。
    pub fn add_runtime_deny(&self, fragment: impl Into<String>) {
        self.policy.add_runtime_deny(fragment);
    }

    /// 应用运行时权限设置（设置页保存后立即影响下一次工具调用）。
    pub fn apply_policy_settings(&self, read_only: bool, deny_commands: &[String]) {
        self.policy.set_read_only_runtime(read_only);
        self.policy.replace_runtime_deny(deny_commands);
    }

    /// §5.4 运行时注入授权记忆（server 端与 AppState.grants 共享同一引用）。
    pub fn set_grants(&self, grants: std::sync::Arc<crate::grant_store::GrantStore>) {
        self.policy.set_grants(grants);
    }

    /// §5.3 运行时切换权限档位（UI/CLI/HTTP 统一入口）。
    pub fn set_permission_profile(&self, profile: crate::permissions::PermissionProfile) {
        self.policy.set_profile(profile);
    }

    /// 当前权限档位（诊断/设置页回显）。
    pub fn permission_profile(&self) -> crate::permissions::PermissionProfile {
        self.policy.profile()
    }

    /// 执行一轮任务。审批经 `approver` 独立决策；`abort` 可随时中止。
    pub async fn run_turn(
        &self,
        session: &mut Session,
        prompt: &str,
        approver: &dyn Approver,
        abort: &AtomicBool,
        on_event: &mut (dyn FnMut(&TurnEvent) + Send),
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_with_asker(session, prompt, approver, abort, on_event, None)
            .await
    }

    /// 带用户提问通道的回合（ask_user 工具；取优合并自远端 engine）：
    /// `questioner` 为 None 时语义同 [`Agent::run_turn`]（工具会明确报错，
    /// 模型改为在最终回复里书面提问）。
    pub async fn run_turn_with_asker(
        &self,
        session: &mut Session,
        prompt: &str,
        approver: &dyn Approver,
        abort: &AtomicBool,
        on_event: &mut (dyn FnMut(&TurnEvent) + Send),
        questioner: Option<&dyn crate::question::Questioner>,
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_inner(session, prompt, &[], approver, abort, on_event, questioner)
            .await
    }

    /// 带图片输入的回合（A1-2 多模态；取优合并自远端 engine）：`images` 为空时
    /// 语义同 [`Agent::run_turn_with_asker`]；非空时用户消息携带视觉内容
    /// （provider 层转成 image parts / image block）。
    // 图片/提问/审批/中止/事件回调同为回合执行固有维度，参数数超过 clippy
    // 默认阈值；打包成结构体反而让三处调用点可读性下降，故显式豁免。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn_with_images(
        &self,
        session: &mut Session,
        prompt: &str,
        images: &[crate::gateway::MessageImage],
        approver: &dyn Approver,
        questioner: Option<&dyn crate::question::Questioner>,
        abort: &AtomicBool,
        on_event: &mut (dyn FnMut(&TurnEvent) + Send),
    ) -> Result<TurnOutcome, AgentError> {
        self.run_turn_inner(
            session, prompt, images, approver, abort, on_event, questioner,
        )
        .await
    }

    /// 回合执行主体：`run_turn` / `run_turn_with_asker` / `run_turn_with_images` 共用。
    #[allow(clippy::too_many_arguments)]
    async fn run_turn_inner(
        &self,
        session: &mut Session,
        prompt: &str,
        images: &[crate::gateway::MessageImage],
        approver: &dyn Approver,
        abort: &AtomicBool,
        on_event: &mut (dyn FnMut(&TurnEvent) + Send),
        questioner: Option<&dyn crate::question::Questioner>,
    ) -> Result<TurnOutcome, AgentError> {
        let started_at = Utc::now().to_rfc3339();
        let started = std::time::Instant::now();
        let turn_id = uuid::Uuid::new_v4().to_string();
        let mut usage = TokenUsage::default();
        let mut model_calls = Vec::new();
        session.transient_model_calls.clear();
        session.active_task_context =
            Some(crate::task_context::ResolvedTaskContext::for_single_turn(&turn_id, prompt));
        let mut usage_known = true;
        let mut model_requests = 0usize;
        // §9.2：turn 入口建立统一预算（None = 不限时，仅记账不强制）；
        // §9.3：阶段耗时瀑布按发生顺序累积。
        let mut budget = DeadlineBudget::new(self.config.turn_deadline, PhaseBudgets::default());
        let mut phase_timings: Vec<PhaseTiming> = Vec::new();
        // 新回合代表从当前历史继续发展，旧的 rewind/undo 分支不能再恢复。
        session.redo_stack.clear();
        session.message_redo_stack.clear();
        let rules = load_project_rules(&session.workspace);
        let mut system = build_system_prompt(session.system_prompt.as_deref(), &rules);
        if !self.skills.list_enabled().is_empty() {
            let mut catalog = vec!["可用技能（通过 use_skill 工具按名调用）：".to_string()];
            for skill in self.skills.list_enabled() {
                catalog.push(format!("- {}：{}", skill.name, skill.description));
            }
            system.push_str("\n\n");
            system.push_str(&catalog.join("\n"));
        }
        let mut messages = vec![ChatMessage::system(system)];
        messages.extend(session.messages.iter().cloned());
        if images.is_empty() {
            messages.push(ChatMessage::user(prompt.to_string()));
        } else {
            messages.push(ChatMessage::user_with_images(
                prompt.to_string(),
                images.to_vec(),
            ));
        }
        // 存量历史可能带非法序列（压缩切分、中断半截、外部导入）：发请求前归一。
        sanitize_history(&mut messages);
        // A2-1 UserPromptSubmit hook：exit 2 = 拒绝本回合（敏感词门卫/强制工单号等
        // 确定性控制），stderr 回喂模型与用户。
        let hooks = self.hooks_snapshot();
        if !hooks.is_empty() {
            let outcome = hooks
                .run(
                    crate::hooks::HookEvent::UserPromptSubmit,
                    &serde_json::json!({ "prompt": prompt, "session_id": session.id }),
                )
                .await;
            if let crate::hooks::HookOutcome::Blocked(stderr) = outcome {
                self.audit
                    .lock()
                    .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                    .record(
                        &session.id,
                        "hook_user_prompt_submit",
                        None,
                        Some(false),
                        format!("阻断：{stderr}"),
                    );
                return Err(AgentError::HookBlocked(stderr));
            }
        }
        let tools = self.visible_tool_specs();
        // §9.3：schema 预算——超限时压缩描述/剥离噪声键（不删工具），
        // 并计算稳定指纹（provider schema 缓存复用的 key 基础）。
        let (tools, schema_report) =
            crate::schema_budget::enforce_budget(tools, crate::schema_budget::budget_from_env());

        let mut events = Vec::new();
        let mut final_text = None;
        let mut steps = 0usize;
        // 空回答静默重试计数（见 EMPTY_REPLY_RETRIES）。
        let mut empty_retries = 0usize;
        // 循环保护状态（本回合内）：工具调用总量 + 「同一 name/参数」重复计数。
        let mut tool_calls_seen = 0usize;
        let mut call_signatures: HashMap<String, usize> = HashMap::new();
        // 事件出口共享单元：嵌套子代理（subagent/explore）要能在父回合 await 期间
        // **即时**回传工具进度与审批请求（审批卡必须立刻到达客户端，否则子代理会
        // 一直等一个到不了的决定，直到审批超时——"卡死"的根因）。
        let event_cell: EventCell<'_> = Arc::new(Mutex::new(on_event));
        let nested_sink: crate::subagent::TurnEventSink<'_> = {
            let cell = Arc::clone(&event_cell);
            Arc::new(move |event: &TurnEvent| {
                // 中毒也继续转发（与仓库既有锁处理口径一致）：静默丢弃事件会让
                // 子代理进度/审批卡凭空消失，比"带毒继续"危险得多。
                let mut forward = cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                forward(event);
            })
        };

        let mut model_turns = 0usize;
        let mut reached_model_turn_limit = false;
        let mut turn_completion_status = None;
        let mut validation_feedback_fingerprints = std::collections::BTreeSet::new();
        loop {
            if self.config.max_turns > 0 && model_turns >= self.config.max_turns {
                reached_model_turn_limit = true;
                break;
            }
            model_turns = model_turns.saturating_add(1);
            if abort.load(Ordering::Relaxed) {
                commit_turn_messages(session, &messages);
                return Err(AgentError::Aborted);
            }
            // A2-1 PreCompact hook：通知性质（不阻断——压缩是保护性动作）。
            let hooks = self.hooks_snapshot();
            if !hooks.is_empty() {
                let _ = hooks
                    .run(
                        crate::hooks::HookEvent::PreCompact,
                        &serde_json::json!({
                            "session_id": session.id,
                            "messages": messages.len(),
                            "estimated_tokens": estimate_tokens(&messages),
                        }),
                    )
                    .await;
            }
            let compaction = self.maybe_compact(&mut messages, &session.id, false).await;
            let summary = match compaction {
                Ok(summary) => summary,
                Err(error) => {
                    commit_turn_messages(session, &messages);
                    return Err(error);
                }
            };
            if let Some(summary) = summary {
                // 压缩请求目前未暴露 per-request usage，因此总用量必须标为不完整。
                usage_known = false;
                emit(
                    &mut events,
                    &event_cell,
                    TurnEvent::Compaction {
                        summary: summary.clone(),
                    },
                );
            }
            if messages.len() > self.config.context_limit {
                compact_truncate(&mut messages, self.config.context_limit);
            }

            emit(&mut events, &event_cell, TurnEvent::ModelCall);
            let on_event_reborrow = &event_cell;
            let model_started = std::time::Instant::now();
            // §9.3 瀑布：首个 TokenDelta 到达时刻记为首 token 时延。
            // 哨兵必须与合法值域不相交：0ms 是真实可能（本地/mock 端点同毫秒
            // 首达、时钟粒度），不能用 0 作"未触发"，否则观测数据被静默吞掉。
            const FIRST_TOKEN_UNSET: u64 = u64::MAX;
            // 声明须先于 emit_delta 闭包（闭包捕获引用）。
            let first_token_ms = std::sync::atomic::AtomicU64::new(FIRST_TOKEN_UNSET);
            let mut emit_chunk = |chunk: StreamChunk| {
                // §9.3 瀑布：首个增量到达即记录首 token 时延（compare_exchange 保证只记首次）。
                let _ = first_token_ms.compare_exchange(
                    FIRST_TOKEN_UNSET,
                    model_started.elapsed().as_millis() as u64,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                );
                // 思考通道单独事件外发（不写入对话历史；CLI/前端可折叠展示）。
                let event = match chunk {
                    StreamChunk::Content(delta) => TurnEvent::TokenDelta { delta },
                    StreamChunk::Reasoning(delta) => TurnEvent::ReasoningDelta { delta },
                };
                emit(&mut events, on_event_reborrow, event);
            };
            // §9.2：每次模型调用（即下一回合的 retry 点）前复查剩余预算；
            // 激活时以阶段剩余预算包裹超时，超时即结构化失败。
            // M4.2 会话级路由：显式覆盖（创建会话时指定）进请求体；未覆盖时
            // Provider 自行解析（OPENAI_MODEL 热切换 → 启动配置 → 内置默认）。
            // 克隆出循环体，避免与 `commit_turn_messages(session, …)` 的再借用冲突。
            let wire_model = session.model_override.clone();
            let attempt = async {
                tokio::select! {
                    output = self.provider.complete_stream_with_reasoning_and_model_observed(
                        wire_model.as_deref(),
                        &messages,
                        &tools,
                        &mut emit_chunk,
                    ) => {
                        output.map_err(AgentError::Gateway)
                    }
                    _ = wait_for_abort(abort) => Err(AgentError::Aborted),
                }
            };
            let output = match self.config.turn_deadline {
                Some(_) => {
                    let model_budget = budget.remaining(Phase::Model).map_err(|exceeded| {
                        commit_turn_messages(session, &messages);
                        exceeded.to_agent_error()
                    })?;
                    match tokio::time::timeout(model_budget, attempt).await {
                        Ok(result) => result,
                        Err(_) => {
                            session.transient_model_calls.push(ModelCallRecord {
                                metadata: ModelCallMetadata {
                                    model: wire_model.clone(),
                                    latency_ms: Some(model_started.elapsed().as_millis() as u64),
                                    ..ModelCallMetadata::default()
                                },
                                succeeded: false,
                            });
                            commit_turn_messages(session, &messages);
                            return Err(AgentError::Gateway(format!(
                                "预算耗尽：phase=model elapsed_ms={}（§9.2 DeadlineBudget）",
                                model_started.elapsed().as_millis()
                            )));
                        }
                    }
                }
                None => attempt.await,
            };
            let observed = match output {
                Ok(observed) => observed,
                Err(error) => {
                    session.transient_model_calls.push(ModelCallRecord {
                        metadata: ModelCallMetadata {
                            model: wire_model.clone(),
                            latency_ms: Some(model_started.elapsed().as_millis() as u64),
                            ..ModelCallMetadata::default()
                        },
                        succeeded: false,
                    });
                    commit_turn_messages(session, &messages);
                    return Err(error);
                }
            };
            model_requests = model_requests.saturating_add(1);
            let model_elapsed = model_started.elapsed();
            let mut request_metadata = observed.metadata.clone();
            request_metadata.latency_ms.get_or_insert(model_elapsed.as_millis() as u64);
            let request_record = ModelCallRecord {
                metadata: request_metadata,
                succeeded: true,
            };
            session.transient_model_calls.push(request_record.clone());
            model_calls.push(request_record);
            if let Some(request_usage) = observed.metadata.usage {
                usage.add(&request_usage);
            } else {
                usage_known = false;
            }
            let output = observed.output;
            budget.record(Phase::Model, model_elapsed);
            phase_timings.push(PhaseTiming {
                phase: Phase::Model.as_str().to_string(),
                elapsed_ms: model_elapsed.as_millis() as u64,
                target: String::new(),
                first_token_ms: {
                    let seen = first_token_ms.load(std::sync::atomic::Ordering::SeqCst);
                    (seen != FIRST_TOKEN_UNSET).then_some(seen)
                },
            });

            match output {
                // 空回答（网关截断/模型超载）不再直接当正常完成：先静默重试一次，
                // 仍为空则用「本回合已执行工具动作摘要」兜底，保证用户总有可见回复。
                ModelOutput::Text(text) if text.trim().is_empty() => {
                    if empty_retries < EMPTY_REPLY_RETRIES {
                        empty_retries += 1;
                        messages.push(ChatMessage::user(EMPTY_REPLY_RETRY_PROMPT.to_string()));
                        continue;
                    }
                    let fallback = synthesize_fallback_reply(
                        &messages,
                        steps,
                        "模型连续返回空回答（可能被网关截断或超载）",
                    );
                    messages.push(ChatMessage::assistant_text(fallback.clone()));
                    final_text = Some(fallback.clone());
                    emit(
                        &mut events,
                        &event_cell,
                        TurnEvent::Final { text: fallback },
                    );
                    break;
                }
                ModelOutput::Text(mut text) => {
                    let mut completion_status = assess_single_turn_completion(
                        session,
                        prompt,
                        &turn_id,
                        &events,
                        false,
                        Some(&text),
                    );
                    if completion_status == owo_agent_protocol::CompletionStatusV1::Unverified {
                        let current_plan = session.single_verification_plan.clone().filter(|_| {
                            single_verification_plan_matches_turn(session, prompt, &turn_id)
                        });
                        if let Some(plan) = current_plan.filter(|plan| {
                            plan.requirements.iter().any(|requirement| {
                                requirement.required
                                    && requirement.validator_id
                                        == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
                            })
                        }) {
                            completion_status = single_manual_acceptance::request_single_manual_acceptance(
                                session,
                                &plan,
                                &turn_id,
                                questioner,
                                abort,
                            )
                            .await;
                            if abort.load(Ordering::Relaxed) {
                                messages.push(ChatMessage::assistant_text(text.clone()));
                                commit_turn_messages(session, &messages);
                                return Err(AgentError::Aborted);
                            }
                        }
                    }
                    if completion_status == owo_agent_protocol::CompletionStatusV1::Accepted {
                        let candidate_paths = single_review::accepted_candidate_paths(session, &turn_id);
                        if single_review::is_required(prompt, &candidate_paths) {
                            let model_turn_available =
                                self.config.max_turns == 0 || model_turns < self.config.max_turns;
                            let review_timeout = if self.config.turn_deadline.is_some() {
                                budget.remaining(Phase::Model).ok()
                            } else {
                                None
                            };
                            let allow_review_request =
                                model_turn_available
                                    && (self.config.turn_deadline.is_none()
                                        || review_timeout.is_some());
                            let review = single_review::review_candidate(
                                &self.provider,
                                session.model_override.as_deref(),
                                session,
                                prompt,
                                &turn_id,
                                &crate::CasStore::hash_of(prompt.as_bytes()),
                                &candidate_paths,
                                allow_review_request,
                                abort,
                                review_timeout,
                            )
                            .await;
                            if let Some(request) = review.request {
                                emit(&mut events, &event_cell, TurnEvent::ModelCall);
                                let review_elapsed =
                                    std::time::Duration::from_millis(review.request_duration_ms);
                                budget.record(Phase::Model, review_elapsed);
                                phase_timings.push(PhaseTiming {
                                    phase: Phase::Model.as_str().to_string(),
                                    elapsed_ms: review.request_duration_ms,
                                    target: "single_independent_review".to_string(),
                                    first_token_ms: None,
                                });
                                model_turns = model_turns.saturating_add(1);
                                model_requests = model_requests.saturating_add(1);
                                session.transient_model_calls.push(request.clone());
                                model_calls.push(request);
                                if let Some(request_usage) = review.usage {
                                    usage.add(&request_usage);
                                }
                                usage_known &= review.usage_known;
                            }
                            let review_verdict = review.receipt.verdict;
                            let review_passed =
                                review_verdict == crate::plan::ValidationVerdictV1::Passed;
                            session.validation_receipts.push(review.receipt);
                            if !review_passed {
                                let current_validation_ids = session
                                    .validation_receipts
                                    .iter()
                                    .filter(|receipt| {
                                        receipt.attempt_id == turn_id
                                            && receipt.validator_id != "workspace-independent-review-v1"
                                            && receipt.verdict == crate::plan::ValidationVerdictV1::Passed
                                    })
                                    .map(|receipt| receipt.receipt_id.clone())
                                    .collect::<std::collections::HashSet<_>>();
                                for execution in &mut session.execution_receipts {
                                    if execution.status == "accepted"
                                        && execution.validation_receipt_id.as_ref().is_some_and(|id| {
                                            current_validation_ids.contains(id)
                                        })
                                    {
                                        execution.status = "executed".to_string();
                                        execution.validation_receipt_id = None;
                                    }
                                }
                            }
                            completion_status = crate::completion::apply_required_review(
                                completion_status,
                                review_verdict,
                            );
                            if abort.load(Ordering::Relaxed) {
                                messages.push(ChatMessage::assistant_text(text.clone()));
                                commit_turn_messages(session, &messages);
                                return Err(AgentError::Aborted);
                            }
                        }
                    }
                    if completion_status == owo_agent_protocol::CompletionStatusV1::Accepted {
                        if let Some(notice) =
                            single_manual_acceptance::completion_notice(session, &turn_id)
                        {
                            text.push_str(&notice);
                        }
                    }
                    let can_retry_after_validation =
                        (self.config.max_turns == 0 || model_turns < self.config.max_turns)
                            && (self.config.turn_deadline.is_none()
                                || budget.remaining(Phase::Model).is_ok());
                    let plan_is_current =
                        single_verification_plan_matches_turn(session, prompt, &turn_id);
                    let has_turn_file_candidate = session
                        .execution_receipts
                        .iter()
                        .any(|receipt| {
                            receipt.turn_id == turn_id
                                && receipt.status == "executed"
                                && !receipt.changed_files.is_empty()
                        });
                    let retry_feedback = if can_retry_after_validation
                        && plan_is_current
                        && matches!(
                            completion_status,
                            owo_agent_protocol::CompletionStatusV1::Unverified
                                | owo_agent_protocol::CompletionStatusV1::Blocked
                        )
                    {
                        single_validation_retry_feedback(session, &turn_id)
                    } else if can_retry_after_validation
                        && !plan_is_current
                        && has_turn_file_candidate
                        && matches!(
                            completion_status,
                            owo_agent_protocol::CompletionStatusV1::Candidate
                                | owo_agent_protocol::CompletionStatusV1::Unverified
                        )
                    {
                        single_missing_verification_plan_feedback(session, &turn_id)
                    } else {
                        None
                    };
                    if let Some((fingerprint, feedback)) = retry_feedback {
                        let repeated_failure =
                            !validation_feedback_fingerprints.insert(fingerprint.clone());
                        turn_completion_status = Some(completion_status);
                        messages.push(ChatMessage::assistant_text(text.clone()));
                        if repeated_failure {
                            final_text = Some(text.clone());
                            emit(&mut events, &event_cell, TurnEvent::Final { text });
                            break;
                        }
                        messages.push(ChatMessage::system(feedback));
                        final_text = None;
                        continue;
                    }
                    turn_completion_status = Some(completion_status);
                    messages.push(ChatMessage::assistant_text(text.clone()));
                    final_text = Some(text.clone());
                    emit(&mut events, &event_cell, TurnEvent::Final { text });
                    break;
                }
                ModelOutput::ToolCalls(calls) => {
                    // 循环保护（对标 Codex/OpenCode）：先查总量上限，再逐调用查重复。
                    if self.config.max_tool_calls_per_turn > 0
                        && tool_calls_seen.saturating_add(calls.len())
                            > self.config.max_tool_calls_per_turn
                    {
                        let limit = self.config.max_tool_calls_per_turn;
                        commit_turn_messages(session, &messages);
                        return Err(AgentError::Gateway(format!(
                            "循环保护：单回合工具调用达到上限 {limit}（已请求 {tool_calls_seen} + 本批 {}）。已停止执行以避免失控循环；请缩小任务或分步重试。",
                            calls.len()
                        )));
                    }
                    tool_calls_seen = tool_calls_seen.saturating_add(calls.len());
                    messages.push(ChatMessage::assistant_tool_calls(calls.clone()));
                    // §9.1 阶段一——权限判定保持原始 tool-call 顺序：Ask 的独立审批
                    // 与用户审批仍按原序逐个交互，先得到每个调用的 Allow/Deny。
                    let mut prepared: Vec<PreparedCall> = Vec::with_capacity(calls.len());
                    for call in &calls {
                        if abort.load(Ordering::Relaxed) {
                            commit_turn_messages(session, &messages);
                            return Err(AgentError::Aborted);
                        }
                        // 循环保护：同一 name + 规范化参数重复超过上限 → 拦截，不审批不执行。
                        let signature = tool_call_signature(call);
                        let repeats = call_signatures.entry(signature).or_insert(0);
                        *repeats += 1;
                        if *repeats > self.config.max_repeated_tool_calls {
                            let reason = format!(
                                "循环保护：工具 `{}` 携相同参数已请求 {} 次（上限 {}），本次不再执行。请改变策略或直接给出结论。",
                                call.name, *repeats, self.config.max_repeated_tool_calls
                            );
                            prepared.push(PreparedCall {
                                approval: None,
                                reason: reason.clone(),
                                guard_error: Some(reason),
                            });
                            continue;
                        }
                        // A2-1 PreToolUse hook：exit 2 = 阻断该次调用，stderr 原样作为
                        // 拒绝原因回喂模型（模型可据此换策略），不终止回合。
                        let hooks = self.hooks_snapshot();
                        if !hooks.is_empty() {
                            let outcome = hooks
                                .run(
                                    crate::hooks::HookEvent::PreToolUse,
                                    &serde_json::json!({
                                        "tool": call.name,
                                        "args": call.arguments,
                                        "session_id": session.id,
                                    }),
                                )
                                .await;
                            if let crate::hooks::HookOutcome::Blocked(stderr) = outcome {
                                self.audit
                                    .lock()
                                    .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                                    .record(
                                        &session.id,
                                        "hook_pre_tool_use",
                                        Some(call.name.clone()),
                                        Some(false),
                                        format!("阻断：{stderr}"),
                                    );
                                prepared.push(PreparedCall {
                                    approval: None,
                                    reason: format!("hook 阻断：{stderr}"),
                                    guard_error: Some(format!("hook 阻断：{stderr}")),
                                });
                                continue;
                            }
                        }
                        // §5.1：从注册表取出完整的 ToolSpec（含 effect 唯一事实源），
                        // 交给 Policy 判定，避免全局名字再查询。
                        let call_spec = self
                            .registry
                            .read()
                            .map_err(|_| AgentError::Session("工具注册表锁中毒".into()))?
                            .get(&call.name)
                            .map(|tool| tool.spec());
                        let request = self.policy.evaluate_with_effect(
                            &call.name,
                            call_spec.as_ref().and_then(|spec| spec.effect.as_ref()),
                            &call.arguments,
                        );
                        let decision = match self.policy.decision(&request) {
                            Decision::Ask => {
                                // 独立审批模型先于打扰用户（Auto-review）。
                                let approval_started = std::time::Instant::now();
                                let verdict = if let Some(reviewer) = &self.reviewer {
                                    let context = session
                                        .messages
                                        .last()
                                        .and_then(|message| message.content.clone());
                                    reviewer.review(&request, context.as_deref()).await
                                } else {
                                    ReviewVerdict::Unknown
                                };
                                if self.reviewer.is_some() {
                                    phase_timings.push(PhaseTiming {
                                        phase: Phase::Approval.as_str().to_string(),
                                        elapsed_ms: approval_started.elapsed().as_millis() as u64,
                                        target: format!("{}:review", call.name),
                                        first_token_ms: None,
                                    });
                                }
                                match verdict {
                                    ReviewVerdict::Deny => {
                                        self.audit
                                            .lock()
                                            .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                                            .record(
                                                &session.id,
                                                "auto_review",
                                                Some(call.name.clone()),
                                                Some(false),
                                                format!("独立审批模型拒绝：{}", request.reason),
                                            );
                                        Decision::Deny
                                    }
                                    ReviewVerdict::Allow => {
                                        self.audit
                                            .lock()
                                            .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                                            .record(
                                                &session.id,
                                                "auto_review",
                                                Some(call.name.clone()),
                                                Some(true),
                                                "独立审批模型放行".to_string(),
                                            );
                                        Decision::Allow
                                    }
                                    ReviewVerdict::Unknown => {
                                        emit(
                                            &mut events,
                                            &event_cell,
                                            TurnEvent::PermissionRequest(request.clone()),
                                        );
                                        let decide_started = std::time::Instant::now();
                                        let decided = approver.decide(&request).await;
                                        phase_timings.push(PhaseTiming {
                                            phase: Phase::Approval.as_str().to_string(),
                                            elapsed_ms: decide_started.elapsed().as_millis() as u64,
                                            target: call.name.clone(),
                                            first_token_ms: None,
                                        });
                                        decided
                                    }
                                }
                            }
                            other => other,
                        };
                        let approval = ToolApprovalGrant::from_decision(&request, decision).ok();
                        let approved = approval.is_some();
                        self.audit
                            .lock()
                            .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                            .record(
                                &session.id,
                                "permission",
                                Some(call.name.clone()),
                                Some(approved),
                                request.reason.clone(),
                            );
                        prepared.push(PreparedCall {
                            approval,
                            reason: request.reason.clone(),
                            guard_error: None,
                        });
                    }

                    // §9.1 阶段二——执行：仅「已放行 + 宿主验证只读（EffectClass::Read
                    // 且 host_verified_readonly）+ 未禁用」的连续调用组成有界并发组
                    //（默认 4）；写/执行/注入、MCP 自报只读未验证、未知工具与被拒
                    // 调用一律串行。tool 消息按原 tool-call 顺序回填。
                    let group_events: Arc<Mutex<Vec<TurnEvent>>> = Arc::new(Mutex::new(Vec::new()));
                    let mut results: Vec<Result<serde_json::Value, String>> =
                        Vec::with_capacity(calls.len());
                    let mut index = 0;
                    while index < calls.len() {
                        if abort.load(Ordering::Relaxed) {
                            commit_turn_messages(session, &messages);
                            return Err(AgentError::Aborted);
                        }
                        let eligible_here = prepared[index].approval.is_some()
                            && !self.tool_disabled(&calls[index].name)
                            && self.call_is_concurrent_eligible(&calls[index]);
                        if eligible_here {
                            // 连续可并发段（上限 tool_concurrency）；遇任何不满足条件
                            // 的调用立即断组——写/执行紧邻只读时绝不进同一组。
                            let start = index;
                            let mut end = start + 1;
                            while end < calls.len()
                                && end - start < self.config.tool_concurrency.max(1)
                                && prepared[end].approval.is_some()
                                && !self.tool_disabled(&calls[end].name)
                                && self.call_is_concurrent_eligible(&calls[end])
                            {
                                end += 1;
                            }
                            let mut futures = Vec::with_capacity(end - start);
                            for (offset, call) in calls[start..end].iter().enumerate() {
                                let approval = prepared[start + offset]
                                    .approval
                                    .clone()
                                    .expect("eligible tool call must have approval grant");
                                let workspace = session.workspace.clone();
                                let max_command_timeout_ms = self.config.max_command_timeout_ms;
                                let capability_context = ToolCapabilityContext::for_workspace(
                                    &workspace,
                                    session.id.clone(),
                                    turn_id.clone(),
                                )
                                .with_command_timeout(max_command_timeout_ms);
                                // 并发组内全部为宿主验证只读工具（经审计不改变会话
                                // 状态）；Session 按值克隆以满足 ToolContext 的 &mut
                                // 签名，克隆上的任何变更被有意丢弃（读取语义不变）。
                                let mut session_view = session.clone();
                                let subagent = SubagentRunner {
                                    provider: Arc::clone(&self.provider),
                                    approver,
                                    abort,
                                    depth: self.config.subagent_depth,
                                    max_turns: nested_turn_cap(self.config.max_turns),
                                    model: session.model_override.clone().unwrap_or_default(),
                                    events: Some(Arc::clone(&nested_sink)),
                                };
                                // A5-1：fan-out 通道（owned，'static 闭包约束）。
                                let fanout = crate::subagent::FanOutRunner {
                                    provider: Arc::clone(&self.provider),
                                    workspace: workspace.clone(),
                                    model: session_view.model_override.clone().unwrap_or_default(),
                                    depth: self.config.subagent_depth,
                                    max_turns: nested_turn_cap(self.config.max_turns),
                                };
                                let sink = Arc::clone(&group_events);
                                let call_id = call.id.clone();
                                let tool_name = call.name.clone();
                                let turn_id_for_receipt = turn_id.clone();
                                let arguments = call.arguments.clone();
                                let tool_host = self.tool_host.clone();
                                // 实时状态：ToolStart 立即外发（不再等整组结束），
                                // 让长任务/多工具链在 CLI 上可见"正在跑哪些工具、什么参数"。
                                emit(
                                    &mut events,
                                    &event_cell,
                                    TurnEvent::ToolStart {
                                        id: call_id.clone(),
                                        tool: tool_name.clone(),
                                        args_preview: tool_args_preview(&call.arguments),
                                    },
                                );
                                futures.push(async move {
                                    let mut ctx = ToolContext {
                                        workspace: &workspace,
                                        policy: &self.policy,
                                        session: &mut session_view,
                                        audit: &self.audit,
                                        subagent: Some(subagent),
                                        skills: &self.skills,
                                        elements: &self.elements,
                                        fanout: Some(fanout),
                                        abort: Some(abort),
                                        // 并行只读组不参与提问（无 mut session/UI 通道）。
                                        questioner: None,
                                    };
                                    let outcome = match tool_host.issue(
                                        &tool_name,
                                        arguments,
                                        approval,
                                        capability_context,
                                    ) {
                                        Ok(capability) => {
                                            tool_host.execute(capability, &mut ctx).await
                                        }
                                        Err(error) => Err(error),
                                    };
                                    if let Ok(mut buffer) = sink.lock() {
                                        let command_receipt =
                                            command_execution_receipt(&tool_name, &outcome, ctx.session, &turn_id_for_receipt);
                                        buffer.push(TurnEvent::ToolResult {
                                            id: call_id,
                                            tool: tool_name,
                                            ok: outcome.is_ok(),
                                            error: outcome.as_ref().err().cloned(),
                                            preview: tool_preview(&outcome),
                                            command_receipt,
                                        });
                                    }
                                    outcome
                                });
                            }
                            // 有界并发轮询；abort → 组合 future 被 drop，所有未完成
                            // 工具在 await 点被取消，不残留后台任务（§9.1.6）。
                            let group_started = std::time::Instant::now();
                            let outcomes = if self.config.turn_deadline.is_some() {
                                // §9.2：组级预算包裹——超时 drop 组合 future，
                                // 组内所有未完成工具随 await 点取消。
                                let tool_budget =
                                    budget.remaining(Phase::Tool).map_err(|exceeded| {
                                        commit_turn_messages(session, &messages);
                                        exceeded.to_agent_error()
                                    })?;
                                match tokio::time::timeout(tool_budget, async {
                                    tokio::select! {
                                        outcomes = futures::future::join_all(futures) => Ok(outcomes),
                                        _ = wait_for_abort(abort) => Err(AgentError::Aborted),
                                    }
                                })
                                .await
                                {
                                    Ok(Ok(outcomes)) => outcomes,
                                    Ok(Err(AgentError::Aborted)) => {
                                        commit_turn_messages(session, &messages);
                                        return Err(AgentError::Aborted);
                                    }
                                    Ok(Err(other)) => return Err(other),
                                    Err(_) => {
                                        commit_turn_messages(session, &messages);
                                        return Err(AgentError::Gateway(format!(
                                            "预算耗尽：phase=tool elapsed_ms={}（§9.2 并发组）",
                                            group_started.elapsed().as_millis()
                                        )));
                                    }
                                }
                            } else {
                                // 实时回放：等待期间每 100ms drain 一次组事件缓冲，
                                // 工具一完成即可在 CLI 看到 ToolResult（不再等整组结束）。
                                let join = futures::future::join_all(futures);
                                tokio::pin!(join);
                                loop {
                                    tokio::select! {
                                        outcomes = &mut join => break outcomes,
                                        _ = wait_for_abort(abort) => {
                                            commit_turn_messages(session, &messages);
                                            return Err(AgentError::Aborted);
                                        }
                                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                                            if let Ok(mut buffer) = group_events.lock() {
                                                for event in buffer.drain(..) {
                                                    emit(&mut events, &event_cell, event);
                                                }
                                            }
                                        }
                                    }
                                }
                            };
                            let group_elapsed = group_started.elapsed();
                            budget.record(Phase::Tool, group_elapsed);
                            phase_timings.push(PhaseTiming {
                                phase: Phase::Tool.as_str().to_string(),
                                elapsed_ms: group_elapsed.as_millis() as u64,
                                target: format!("group:{}", end - start),
                                first_token_ms: None,
                            });
                            results.extend(outcomes);
                            // 兜底 drain：把剩余 ToolResult 按插入序回放
                            // （ToolStart 已实时外发；未完成工具的 ToolResult 在此补齐）。
                            if let Ok(mut buffer) = group_events.lock() {
                                for event in buffer.drain(..) {
                                    emit(&mut events, &event_cell, event);
                                }
                            }
                            index = end;
                        } else {
                            // —— 串行执行单个调用（与原实现逐字等价）——
                            let call = &calls[index];
                            let result = if let Some(guard) = prepared[index].guard_error.clone() {
                                // 循环保护拦截：不执行，回灌可读原因（模型据此改策略）。
                                emit(
                                    &mut events,
                                    &event_cell,
                                    TurnEvent::ToolResult {
                                        id: call.id.clone(),
                                        tool: call.name.clone(),
                                        ok: false,
                                        error: Some(guard.clone()),
                                        // 未执行（宿主拦截）没有结果正文可预览。
                                        preview: None,
                                        command_receipt: None,
                                    },
                                );
                                Err(guard)
                            } else if self.tool_disabled(&call.name) {
                                Err(format!("工具已被禁用（插件热卸载）：{}", call.name))
                            } else if prepared[index].approval.is_some() {
                                let workspace = session.workspace.clone();
                                let capability_context = ToolCapabilityContext::for_workspace(
                                    &workspace,
                                    session.id.clone(),
                                    turn_id.clone(),
                                )
                                .with_command_timeout(self.config.max_command_timeout_ms);
                                emit(
                                    &mut events,
                                    &event_cell,
                                    TurnEvent::ToolStart {
                                        id: call.id.clone(),
                                        tool: call.name.clone(),
                                        args_preview: tool_args_preview(&call.arguments),
                                    },
                                );
                                let subagent = SubagentRunner {
                                    provider: Arc::clone(&self.provider),
                                    approver,
                                    abort,
                                    depth: self.config.subagent_depth,
                                    max_turns: nested_turn_cap(self.config.max_turns),
                                    model: session.model_override.clone().unwrap_or_default(),
                                    events: Some(Arc::clone(&nested_sink)),
                                };
                                let fanout = crate::subagent::FanOutRunner {
                                    provider: Arc::clone(&self.provider),
                                    workspace: workspace.clone(),
                                    model: session.model_override.clone().unwrap_or_default(),
                                    depth: self.config.subagent_depth,
                                    max_turns: nested_turn_cap(self.config.max_turns),
                                };
                                // P2-5：计划快照——工具执行后清单变化即发 PlanUpdate
                                //（前端渲染步骤进度；todo 工具为整表替换语义）。
                                let plan_before = session.todos.clone();
                                let mut ctx = ToolContext {
                                    workspace: &workspace,
                                    policy: &self.policy,
                                    session,
                                    audit: &self.audit,
                                    subagent: Some(subagent),
                                    skills: &self.skills,
                                    elements: &self.elements,
                                    fanout: Some(fanout),
                                    abort: Some(abort),
                                    questioner,
                                };
                                let tool_started = std::time::Instant::now();
                                let outcome = match self.tool_host.issue(
                                    &call.name,
                                    call.arguments.clone(),
                                    prepared[index]
                                        .approval
                                        .clone()
                                        .expect("approved tool call must have approval grant"),
                                    capability_context,
                                ) {
                                    Ok(capability) => {
                                        let run = self.tool_host.execute(capability, &mut ctx);
                                        // §9.2：激活预算时以阶段剩余包裹工具执行，
                                        // 超时转工具级错误（回合继续，模型可见）。
                                        if self.config.turn_deadline.is_some() {
                                            match budget.remaining(Phase::Tool) {
                                                Ok(tool_budget) => {
                                                    match tokio::time::timeout(tool_budget, run).await
                                                    {
                                                        Ok(outcome) => outcome,
                                                        Err(_) => Err(format!(
                                                            "工具预算耗尽（§9.2）：{} elapsed_ms={}",
                                                            call.name,
                                                            tool_started.elapsed().as_millis()
                                                        )),
                                                    }
                                                }
                                                Err(exceeded) => Err(format!(
                                                    "工具预算耗尽（§9.2）：{} {exceeded}",
                                                    call.name
                                                )),
                                            }
                                        } else {
                                            run.await
                                        }
                                    }
                                    Err(error) => Err(error),
                                };
                                let tool_elapsed = tool_started.elapsed();
                                budget.record(Phase::Tool, tool_elapsed);
                                phase_timings.push(PhaseTiming {
                                    phase: Phase::Tool.as_str().to_string(),
                                    elapsed_ms: tool_elapsed.as_millis() as u64,
                                    target: call.name.clone(),
                                    first_token_ms: None,
                                });
                                let command_receipt =
                                    command_execution_receipt(&call.name, &outcome, ctx.session, &turn_id);
                                emit(
                                    &mut events,
                                    &event_cell,
                                    TurnEvent::ToolResult {
                                        id: call.id.clone(),
                                        tool: call.name.clone(),
                                        ok: outcome.is_ok(),
                                        error: outcome.as_ref().err().cloned(),
                                        preview: tool_preview(&outcome),
                                        command_receipt,
                                    },
                                );
                                if ctx.session.todos != plan_before {
                                    if let Ok(steps) = serde_json::to_value(&ctx.session.todos) {
                                        emit(
                                            &mut events,
                                            &event_cell,
                                            TurnEvent::PlanUpdate { steps },
                                        );
                                    }
                                }
                                outcome
                            } else {
                                Err(format!("permission denied: {}", prepared[index].reason))
                            };
                            results.push(result);
                            index += 1;
                        }
                    }
                    // —— 回填：按原 tool-call 顺序生成 tool 消息 + 审计 + 计步 ——
                    for (call, result) in calls.iter().zip(results) {
                        let raw_content = match &result {
                            Ok(value) => value.to_string(),
                            Err(error) => format!("工具错误：{error}"),
                        };
                        let truncated = truncate_tool_result(&raw_content, MAX_TOOL_RESULT_CHARS);
                        let content = match (&self.artifact_store, raw_content != truncated) {
                            // §9.3：完整结果入 CAS，模型拿「指针+预览」而非盲截断；
                            // MIME/大小/截断原因/ref 一并携带（hash 即 CAS ref）。
                            (Some(store), true) => match store.put(raw_content.as_bytes()) {
                                Ok(hash) => serde_json::json!({
                                    "artifact": {
                                        "ref": hash,
                                        "mime": "text/plain",
                                        "size_bytes": raw_content.len(),
                                        "truncation_reason": format!(
                                            "工具结果超过 {} 字符上限，完整内容已存 artifact，可按 ref 取回",
                                            MAX_TOOL_RESULT_CHARS
                                        ),
                                        "preview": truncated,
                                    }
                                })
                                .to_string(),
                                Err(_) => sanitize_tool_result(&call.name, &truncated),
                            },
                            _ => sanitize_tool_result(&call.name, &truncated),
                        };
                        messages.push(ChatMessage::tool(call.id.clone(), content.clone()));
                        self.audit
                            .lock()
                            .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                            .record(
                                &session.id,
                                "tool_call",
                                Some(call.name.clone()),
                                None,
                                content.clone(),
                            );
                        steps += 1;
                    }
                }
            }
        }

        if final_text.is_none() {
            reached_model_turn_limit = true;
            // 步数耗尽不能只甩一句「达到最大回合数」：再补一次不带工具的收尾总结，
            // 保证回合一定有可见结论（审查/分析类任务据此产出报告），
            // 而不是让用户看到「思考完就停住」。
            if abort.load(Ordering::Relaxed) {
                commit_turn_messages(session, &messages);
                return Err(AgentError::Aborted);
            }
            emit(&mut events, &event_cell, TurnEvent::ModelCall);
            let mut wrap_messages = messages.clone();
            wrap_messages.push(ChatMessage::user(WRAP_UP_PROMPT.to_string()));
            let wrap_model = session.model_override.clone();
            let mut emit_wrap_delta = |delta: String| {
                emit(&mut events, &event_cell, TurnEvent::TokenDelta { delta });
            };
            let mut wrap_chunks = |chunk: StreamChunk| {
                if let StreamChunk::Content(delta) = chunk {
                    emit_wrap_delta(delta);
                }
            };
            let wrap_up_started = std::time::Instant::now();
            let wrap_up = self
                .provider
                .complete_stream_with_reasoning_and_model_observed(
                    wrap_model.as_deref(),
                    &wrap_messages,
                    &[],
                    &mut wrap_chunks,
                )
                .await;
            model_requests = model_requests.saturating_add(1);
            let wrap_up = match wrap_up {
                Ok(observed) => {
                    let mut request_metadata = observed.metadata.clone();
                    request_metadata
                        .latency_ms
                        .get_or_insert(wrap_up_started.elapsed().as_millis() as u64);
                    let request_record = ModelCallRecord {
                        metadata: request_metadata,
                        succeeded: true,
                    };
                    session.transient_model_calls.push(request_record.clone());
                    model_calls.push(request_record);
                    if let Some(request_usage) = observed.metadata.usage {
                        usage.add(&request_usage);
                    } else {
                        usage_known = false;
                    }
                    Ok(observed.output)
                }
                Err(error) => {
                    usage_known = false;
                    let request_record = ModelCallRecord {
                        metadata: ModelCallMetadata {
                            model: wrap_model.clone(),
                            latency_ms: Some(wrap_up_started.elapsed().as_millis() as u64),
                            ..ModelCallMetadata::default()
                        },
                        succeeded: false,
                    };
                    session.transient_model_calls.push(request_record.clone());
                    model_calls.push(request_record);
                    Err(error)
                }
            };
            // 收尾总结同样不允许「空手而归」：模型没产出内容（或调用失败）时，
            // 用本回合已执行的工具动作摘要兜底——回合必须以可见结论结束。
            let text = match wrap_up {
                Ok(ModelOutput::Text(text)) if !text.trim().is_empty() => text,
                Ok(_) => synthesize_fallback_reply(
                    &messages,
                    steps,
                    &format!(
                        "达到最大回合数（{}）且收尾总结未产出内容",
                        self.config.max_turns
                    ),
                ),
                Err(error) => synthesize_fallback_reply(
                    &messages,
                    steps,
                    &format!(
                        "达到最大回合数（{}）且收尾总结调用失败：{error}",
                        self.config.max_turns
                    ),
                ),
            };
            messages.push(ChatMessage::assistant_text(text.clone()));
            final_text = Some(text.clone());
            emit(&mut events, &event_cell, TurnEvent::Final { text });
        }
        let persist_started = std::time::Instant::now();
        commit_turn_messages(session, &messages);
        // A2-1 Stop hook：回合结束通知（finally 类动作如测试/通知）。
        let hooks = self.hooks_snapshot();
        if !hooks.is_empty() {
            let _ = hooks
                .run(
                    crate::hooks::HookEvent::Stop,
                    &serde_json::json!({
                        "session_id": session.id,
                        "stop_reason": final_text
                            .as_deref()
                            .map(|text| text.chars().take(120).collect::<String>()),
                    }),
                )
                .await;
        }
        let persist_elapsed = persist_started.elapsed();
        budget.record(Phase::Persistence, persist_elapsed);
        phase_timings.push(PhaseTiming {
            phase: Phase::Persistence.as_str().to_string(),
            elapsed_ms: persist_elapsed.as_millis() as u64,
            target: String::new(),
            first_token_ms: None,
        });
        usage_known &= model_requests > 0;
        let completion_status = if reached_model_turn_limit {
            assess_single_turn_completion(
                session,
                prompt,
                &turn_id,
                &events,
                true,
                final_text.as_deref(),
            )
        } else if let Some(status) = turn_completion_status {
            status
        } else {
            assess_single_turn_completion(
                session,
                prompt,
                &turn_id,
                &events,
                false,
                final_text.as_deref(),
            )
        };
        Ok(TurnOutcome {
            model_calls,
            final_text,
            completion_status,
            reached_model_turn_limit,
            steps,
            events,
            prompt: prompt.to_string(),
            started_at,
            duration_ms: started.elapsed().as_millis() as u64,
            usage,
            usage_known,
            phase_timings,
            tools_fingerprint: schema_report.fingerprint,
        })
    }

    /// 当估算 token 超过预算时，用模型把旧历史压缩为摘要（保留最近消息）。
    /// `force=true`（`/compact` 显式触发）时跳过预算判断，仍受 `compaction_enabled` 约束。
    async fn maybe_compact(
        &self,
        messages: &mut Vec<ChatMessage>,
        session_id: &str,
        force: bool,
    ) -> Result<Option<String>, AgentError> {
        if !self.config.compaction_enabled {
            return Ok(None);
        }
        if !force && estimate_tokens(messages) <= self.config.token_budget {
            return Ok(None);
        }
        // 切点必须对齐 tool 群组（A4-2 同源规则），且必须在**生成摘要之前**确定：
        // 摘要是 head、保留段是 tail，两者必须严格互补。若先按 head_end 生成摘要、
        // 再对齐 tail，被对齐让出去的那几条消息就既不在摘要里也不在保留段里——等于
        // 静默丢历史。
        let Some(tail_start) = compaction_split(messages, self.config.keep_recent) else {
            return Ok(None);
        };
        let head = messages[1..tail_start].to_vec();
        let prompt = format!(
            "请把以下 Agent 会话历史压缩成一份简洁的进展摘要（保留：已完成的动作、\
             未完成事项、关键决策、当前上下文；不要编造新信息）：\n\n{}",
            serde_json::to_string(&head).unwrap_or_else(|_| "[]".to_string())
        );
        // 压缩摘要是轻量任务：走 fast 档（OWO_MODEL_FAST 配置时路由到便宜模型，
        // 未配置回退 Provider 解析链；M4.2 任务类型路由）。
        let fast_model = crate::gateway::resolve_tier_model(crate::gateway::ModelTier::Fast);
        let summary = match self
            .provider
            .complete_with_model(fast_model.as_deref(), &[ChatMessage::user(prompt)], &[])
            .await
        {
            Ok(crate::gateway::ModelOutput::Text(text)) => text,
            Ok(_) | Err(_) => {
                // 压缩失败兜底（A4-3）：按 token 硬裁剪历史，保证本回合仍可发送，
                // 而不是带着超预算历史直接撞模型 400。返回 Some 让前端收到可见提示。
                let before = messages.len();
                compact_truncate_to_budget(messages, self.config.token_budget / 2);
                self.audit
                    .lock()
                    .map_err(|_| AgentError::Session("审计锁中毒".into()))?
                    .record(
                        session_id,
                        "compaction_fallback",
                        None,
                        None,
                        format!(
                            "模型压缩失败，按预算硬裁剪历史：{before} → {} 条",
                            messages.len().saturating_sub(1)
                        ),
                    );
                return Ok(Some(
                    "模型压缩失败，已硬裁剪最近历史以保持在上下文预算内".to_string(),
                ));
            }
        };
        let mut compacted = vec![messages[0].clone()];
        compacted.push(ChatMessage::system(format!(
            "历史摘要（已压缩）：\n{summary}"
        )));
        // 保留段起点用对齐后的 tail_start（不是 head_end）：见上方说明，
        // 这条是"压缩后历史仍能发出去"的关键。
        compacted.extend(messages[tail_start..].to_vec());
        // 兜底归一：任何压缩路径（含摘要模型异常、未来新增切分）都不该产出非法序列。
        // 这一步只处理非法配对，正常情况下是空操作。
        sanitize_history(&mut compacted);
        *messages = compacted;
        self.audit
            .lock()
            .map_err(|_| AgentError::Session("审计锁中毒".into()))?
            .record(
                session_id,
                "compaction",
                None,
                None,
                format!("压缩 {} 条历史消息", head.len()),
            );
        Ok(Some(summary))
    }

    /// 显式压缩会话历史（`/compact` 触发）：不依赖 token 预算，直接压缩并写回 `session`。
    /// 返回摘要文本；历史不足或模型未产出摘要时返回 `None`。
    pub async fn compact_session(
        &self,
        session: &mut Session,
    ) -> Result<Option<String>, AgentError> {
        let mut messages = session.messages.clone();
        match self.maybe_compact(&mut messages, &session.id, true).await? {
            Some(summary) => {
                session.messages = messages;
                Ok(Some(summary))
            }
            None => Ok(None),
        }
    }
}

/// 工具参数预览：脱敏（秘密字段只显示类型/长度）+ 紧凑 JSON + 截断，供 `ToolStart`
/// 实时状态展示；空参数返回 `None`。
fn tool_args_preview(args: &serde_json::Value) -> Option<String> {
    let text = crate::permissions::redact_args(args).to_string();
    if text == "null" || text == "{}" {
        return None;
    }
    const MAX_CHARS: usize = 160;
    if text.chars().count() <= MAX_CHARS {
        Some(text)
    } else {
        let truncated: String = text.chars().take(MAX_CHARS).collect();
        Some(format!("{truncated}…"))
    }
}

/// 循环保护签名：`name` + 规范化参数（键序稳定，避免 provider 参数键序不同导致漏判）。
fn tool_call_signature(call: &crate::gateway::ToolCall) -> String {
    format!("{}:{}", call.name, canonical_json(&call.arguments))
}

/// 稳定 JSON 序列化：对象键排序、数组保序，用于「同一调用」判定。
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<String> = map
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_json(value)))
                .collect();
            entries.sort();
            format!("{{{}}}", entries.join(","))
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn commit_turn_messages(session: &mut Session, messages: &[ChatMessage]) {
    session.messages = messages.iter().skip(1).cloned().collect();
    session.updated_at = Utc::now().to_rfc3339();
}

async fn wait_for_abort(abort: &AtomicBool) {
    while !abort.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn truncate_tool_result(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let mut truncated: String = content.chars().take(max_chars).collect();
    truncated.push_str("\n[工具输出已截断]");
    truncated
}

/// 步骤时间线的结果预览上限：够看清这步做了什么，又不会把 SSE 帧撑爆。
const TOOL_PREVIEW_CHARS: usize = 1600;

/// 工具结果预览（随 `ToolResult` 事件下发给前端 chip 展开区）。
/// 纯展示用途：写回模型上下文的净化仍由 `sanitize_tool_result` 负责。
fn command_execution_receipt(
    tool: &str,
    outcome: &Result<serde_json::Value, String>,
    session: &Session,
    _turn_id: &str,
) -> Option<CommandExecutionReceipt> {
    if tool != "run_command" {
        return None;
    }
    let value = outcome.as_ref().ok()?;
    let command = value.get("command")?.as_str()?.trim();
    let exit_code = i32::try_from(value.get("exit_code")?.as_i64()?).ok()?;
    let duration_ms = value.get("duration_ms").and_then(serde_json::Value::as_u64);
    let mut workspace_hashes = std::collections::BTreeMap::new();
    let root = session.workspace.canonicalize().ok()?;
    let mut workspace_hashes_complete = true;
    // Writes and tests commonly span model turns. Snapshot every active host write
    // receipt so a later turn can verify the exact source snapshot before acceptance.
    for receipt in session
        .execution_receipts
        .iter()
        .filter(|receipt| receipt.status != "reverted")
    {
        for relative in &receipt.changed_files {
            let normalized = relative.replace('\\', "/");
            let digest = match workspace_file_hash(&root, &normalized) {
                Some(digest) => digest,
                None => {
                    workspace_hashes_complete = false;
                    None
                }
            };
            workspace_hashes.insert(normalized, digest);
        }
    }
    let registered_behavior_command =
        crate::verification::is_registered_behavior_command(command);
    Some(CommandExecutionReceipt {
        command_sha256: crate::CasStore::hash_of(command.as_bytes()),
        exit_code,
        result_sha256: crate::CasStore::hash_of(value.to_string().as_bytes()),
        duration_ms,
        workspace_hashes_complete,
        validator_id: registered_behavior_command
            .then(|| "workspace-command-success-v1".to_string()),
        validator_version: registered_behavior_command.then(|| "1".to_string()),
        workspace_hashes,
    })
}


fn single_workspace_path_matches(root: &std::path::Path, relative: &str, expected: &str) -> bool {
    let absent_digest = crate::verification::workspace_path_absence_sha256();
    match workspace_file_hash(root, relative) {
        Some(Some(current)) => expected != absent_digest && current == expected,
        Some(None) => expected == absent_digest,
        None => false,
    }
}

/// Read a workspace path without following it outside the session root.
/// Some(None) is a known-absent file; None means the host could not prove its state.
fn workspace_file_hash(root: &std::path::Path, relative: &str) -> Option<Option<String>> {
    let relative_path = std::path::Path::new(relative);
    if relative_path.as_os_str().is_empty()
        || relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let path = root.join(relative_path);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent()?.canonicalize().ok()?;
            parent.starts_with(root).then_some(None)
        }
        Err(_) => None,
        Ok(_) => {
            let canonical = path.canonicalize().ok()?;
            if !canonical.starts_with(root) {
                return None;
            }
            let bytes = std::fs::read(canonical).ok()?;
            Some(Some(crate::CasStore::hash_of(&bytes)))
        }
    }
}

/// Recheck prior Single receipts, consume pending host writes only after a registered
/// behavior command succeeds on their exact final bytes, and keep old evidence stale
/// when workspace files change outside the accepted snapshot.
fn assess_single_turn_completion(
    session: &mut Session,
    prompt: &str,
    turn_id: &str,
    events: &[TurnEvent],
    reached_turn_limit: bool,
    final_text: Option<&str>,
) -> owo_agent_protocol::CompletionStatusV1 {
    use crate::plan::{ValidationReceiptV1, ValidationVerdictV1};

    let decide = |response_finished,
                  reached_turn_limit,
                  has_candidate_changes,
                  required_validation_count,
                  passed_required_validation_count,
                  failed_required_validation_count,
                  stale_evidence| {
        crate::completion::decide_completion(crate::completion::CompletionEvidence {
            response_finished,
            reached_turn_limit,
            has_candidate_changes,
            required_validation_count,
            passed_required_validation_count,
            failed_required_validation_count,
            stale_evidence,
            ..crate::completion::CompletionEvidence::default()
        })
    };

    let root = session.workspace.canonicalize().ok();
    if let Some(root) = root.as_deref() {
        for receipt in &mut session.validation_receipts {
            if receipt.verdict != ValidationVerdictV1::Passed {
                continue;
            }
            let stale = receipt.subject_sha256.iter().any(|(subject, expected)| {
                let Some(relative) = subject.strip_prefix("workspace-path:") else {
                    return true;
                };
                !single_workspace_path_matches(root, relative, expected)
            });
            if stale {
                receipt.verdict = ValidationVerdictV1::Stale;
                receipt.detail = Some("Single 工作区源码已变化，原行为验证收据失效".to_string());
                receipt.completed_at = chrono::Utc::now().to_rfc3339();
                let stale_id = receipt.receipt_id.clone();
                for execution in &mut session.execution_receipts {
                    if execution.validation_receipt_id.as_deref() == Some(stale_id.as_str()) {
                        execution.status = "stale".to_string();
                    }
                }
            }
        }
    }

    if reached_turn_limit || final_text.is_none_or(|text| text.trim().is_empty()) {
        return decide(false, reached_turn_limit, true, 0, 0, 0, false);
    }

    let mut pending_hashes = std::collections::BTreeMap::new();
    let mut latest_receipt_by_path = std::collections::BTreeMap::new();
    let mut stale_candidate = false;
    let mut missing_write_hash = false;
    for (index, execution) in session.execution_receipts.iter().enumerate() {
        // Completion status describes this user turn. A stale receipt from older
        // work must not turn an unrelated answer into an unverified result.
        if execution.status == "stale" && execution.turn_id == turn_id {
            stale_candidate = true;
        }
        if execution.status != "executed" {
            continue;
        }
        for relative in &execution.changed_files {
            let normalized = relative.replace('\\', "/");
            let Some((_, expected)) = execution
                .after_hashes
                .iter()
                .find(|(path, _)| path.replace('\\', "/") == normalized)
            else {
                missing_write_hash = true;
                continue;
            };
            pending_hashes.insert(normalized.clone(), expected.clone());
            latest_receipt_by_path.insert(normalized, (index, execution.receipt_id.clone()));
        }
    }
    let input_sha256 = crate::CasStore::hash_of(prompt.as_bytes());
    let plan = session
        .single_verification_plan
        .clone()
        .filter(|_| {
            session.single_verification_plan_input_sha256.as_deref() == Some(input_sha256.as_str())
                && session.single_verification_plan_turn_id.as_deref() == Some(turn_id)
        });
    let has_current_turn_candidate = session.execution_receipts.iter().any(|execution| {
        execution.turn_id == turn_id
            && execution.status == "executed"
            && !execution.changed_files.is_empty()
    });
    if !has_current_turn_candidate && plan.is_none() {
        // Unaccepted files from prior turns do not turn ordinary conversation into a
        // code candidate. A user can explicitly start a new verification turn by
        // registering a fresh request-bound plan.
        return decide(true, false, false, 0, 0, 0, stale_candidate);
    }
    if missing_write_hash {
        return decide(true, false, true, 1, 0, 0, true);
    }
    if pending_hashes.is_empty() {
        return decide(true, false, false, 0, 0, 0, stale_candidate);
    }
    if root.is_none() {
        return decide(true, false, true, 1, 0, 0, true);
    }
    let root = root.expect("checked above");
    let Some(plan) = plan else {
        // A generic successful command is not enough to claim that a task's declared
        // requirements were covered. The model must register a host-resolvable plan.
        return decide(true, false, true, 0, 0, 0, false);
    };
    execute_single_verification_plan(
        session,
        &plan,
        &input_sha256,
        turn_id,
        events,
        &pending_hashes,
        &root,
    )
}

fn single_verification_plan_matches_turn(session: &Session, prompt: &str, turn_id: &str) -> bool {
    let input_sha256 = crate::CasStore::hash_of(prompt.as_bytes());
    session.single_verification_plan.is_some()
        && session.single_verification_plan_input_sha256.as_deref() == Some(input_sha256.as_str())
        && session.single_verification_plan_turn_id.as_deref() == Some(turn_id)
}

fn single_validation_retry_feedback(session: &Session, turn_id: &str) -> Option<(String, String)> {
    let plan = session.single_verification_plan.as_ref()?;
    let required_ids = plan
        .requirements
        .iter()
        .filter(|requirement| requirement.required)
        .map(|requirement| requirement.requirement_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut latest = std::collections::BTreeMap::new();
    for receipt in session
        .validation_receipts
        .iter()
        .filter(|receipt| receipt.attempt_id.as_str() == turn_id)
    {
        latest.insert(receipt.requirement_id.as_str(), receipt);
    }
    let failures = latest
        .into_iter()
        .filter(|(requirement_id, receipt)| {
            (required_ids.contains(requirement_id)
                || receipt.validator_id == "workspace-independent-review-v1")
                && !matches!(
                    receipt.verdict,
                    crate::plan::ValidationVerdictV1::Passed
                        | crate::plan::ValidationVerdictV1::ManualAccepted
                )
        })
        .map(|(requirement_id, receipt)| {
            serde_json::json!({
                "requirement_id": requirement_id,
                "verdict": format!("{:?}", receipt.verdict),
                "detail": receipt.detail,
                "subject_sha256": receipt.subject_sha256,
                "changeset_sha256": receipt.changeset_sha256,
                "evidence_refs": receipt.evidence_refs,
            })
        })
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return None;
    }
    let fingerprint_failures = failures
        .iter()
        .map(|failure| {
            let mut value = failure.clone();
            if value.get("requirement_id").and_then(serde_json::Value::as_str)
                == Some("host-independent-review")
            {
                if let Some(object) = value.as_object_mut() {
                    object.remove("detail");
                    object.remove("evidence_refs");
                }
            }
            value
        })
        .collect::<Vec<_>>();
    let fingerprint = crate::CasStore::hash_of(
        serde_json::to_vec(&fingerprint_failures).unwrap_or_default().as_slice(),
    );
    let details = failures
        .iter()
        .map(|failure| serde_json::to_string(failure).unwrap_or_else(|_| "{}".to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    Some((
        fingerprint,
        format!(
            "宿主已按本回合冻结的 VerificationPlan 检查最终工作区，但必需验收尚未通过。请依据以下结构化结果修复实现；如果需要行为命令，使用计划登记的命令，并在所有写入之后运行。不要替换或降低计划，也不要声称任务已验证通过。\n{details}"
        ),
    ))
}

fn single_missing_verification_plan_feedback(
    session: &Session,
    turn_id: &str,
) -> Option<(String, String)> {
    let mut changed = std::collections::BTreeMap::<String, Option<String>>::new();
    for receipt in session.execution_receipts.iter().filter(|receipt| {
        receipt.turn_id == turn_id && receipt.status == "executed"
    }) {
        for path in &receipt.changed_files {
            let normalized = path.replace('\\', "/");
            let hash = receipt
                .after_hashes
                .iter()
                .find(|(candidate, _)| candidate.replace('\\', "/") == normalized)
                .map(|(_, hash)| hash.clone())
                .unwrap_or(None);
            changed.insert(normalized, hash);
        }
    }
    if changed.is_empty() {
        return None;
    }

    let fingerprint =
        crate::CasStore::hash_of(serde_json::to_vec(&changed).unwrap_or_default().as_slice());
    let changed_files = changed
        .iter()
        .map(|(path, hash)| {
            format!(
                "- {}  sha256:{}",
                path,
                hash.as_deref().unwrap_or("missing-write-hash")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some((
        fingerprint,
        format!(
            "宿主发现本回合写入了文件，但当前用户请求没有绑定有效的 VerificationPlan，因此这些变更仍是候选结果。请不要结束回合或声称已验证：先调用 verification_plan，为每个相关用户验收点登记当前请求原文中的精确引用、覆盖的变更路径和宿主已登记的检查；源码变更还需要登记并实际运行获准的行为命令。之后根据真实检查结果修复失败并重新验收。不得降低、替换验收要求。\n本回合变更：\n{changed_files}\n若显示 missing-write-hash，宿主无法把该文件绑定到写入后的版本；修复或重写后需取得有效宿主写入收据。"
        ),
    ))
}

fn single_path_is_source_code(path: &str) -> bool {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "go"
            | "java" | "cs" | "cpp" | "c" | "h" | "hpp" | "vue" | "svelte"
            | "php" | "rb" | "swift" | "kt" | "scala" | "sql"
    )
}

fn execute_single_verification_plan(
    session: &mut Session,
    plan: &crate::plan::VerificationPlanV1,
    input_sha256: &str,
    turn_id: &str,
    events: &[TurnEvent],
    pending_hashes: &std::collections::BTreeMap<String, Option<String>>,
    root: &std::path::Path,
) -> owo_agent_protocol::CompletionStatusV1 {
    use crate::plan::{
        ValidationReceiptV1, ValidationVerdictV1, VerificationScopeV1,
    };
    use crate::verification::{execute_workspace_requirement, workspace_validator_arguments_supported};

    let base_evidence = |required, passed, failed, stale| {
        crate::completion::decide_completion(crate::completion::CompletionEvidence {
            response_finished: true,
            has_candidate_changes: true,
            required_validation_count: required,
            passed_required_validation_count: passed,
            failed_required_validation_count: failed,
            stale_evidence: stale,
            ..crate::completion::CompletionEvidence::default()
        })
    };
    if validate_single_verification_plan(plan).is_err() {
        return base_evidence(1, 0, 0, false);
    }

    let normalized_paths = |requirement: &crate::plan::VerificationRequirementV1| {
        match &requirement.scope {
            VerificationScopeV1::WorkspacePaths { relative_paths } => relative_paths
                .iter()
                .map(|path| path.replace('\\', "/"))
                .collect::<std::collections::BTreeSet<_>>(),
            _ => std::collections::BTreeSet::new(),
        }
    };
    let required_requirements = plan
        .requirements
        .iter()
        .filter(|requirement| requirement.required)
        .collect::<Vec<_>>();
    let mut all_covered_paths = std::collections::BTreeSet::new();
    let mut behavior_covered_paths = std::collections::BTreeSet::new();
    let manual_acceptance_covers_candidate = required_requirements.iter().any(|requirement| {
        requirement.validator_id == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
            && matches!(&requirement.scope, VerificationScopeV1::Manual)
    });
    if manual_acceptance_covers_candidate {
        all_covered_paths.extend(pending_hashes.keys().cloned());
        behavior_covered_paths.extend(
            pending_hashes
                .keys()
                .filter(|path| single_path_is_source_code(path))
                .cloned(),
        );
    }
    for requirement in &required_requirements {
        let paths = normalized_paths(requirement);
        all_covered_paths.extend(paths.iter().cloned());
        if requirement.validator_id == "workspace-command-success-v1" {
            behavior_covered_paths.extend(paths);
        }
    }
    let missing_paths = pending_hashes
        .keys()
        .filter(|path| !all_covered_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    let missing_behavior_paths = pending_hashes
        .keys()
        .filter(|path| single_path_is_source_code(path) && !behavior_covered_paths.contains(*path))
        .cloned()
        .collect::<Vec<_>>();
    let coverage_ok = missing_paths.is_empty() && missing_behavior_paths.is_empty();

    let started_at = chrono::Utc::now().to_rfc3339();
    let changeset_bytes = serde_json::to_vec(pending_hashes).unwrap_or_default();
    let changeset_sha256 = crate::CasStore::hash_of(&changeset_bytes);
    let environment_id = crate::CasStore::hash_of(session.workspace.to_string_lossy().as_bytes());
    let validation_epoch = session.validation_receipts.len() as u64 + 1;
    let verification_plan_sha256 = serde_json::to_vec(plan)
        .map(|bytes| crate::CasStore::hash_of(&bytes))
        .unwrap_or_default();
    let verification_plan_evidence_ref =
        format!("verification-plan-sha256:{verification_plan_sha256}");
    let mut required_count = required_requirements.len();
    let mut passed_count = 0usize;
    let mut failed_count = 0usize;
    let mut stale_evidence = false;
    let mut first_passed_receipt_id = None;
    let mut plan_receipts = Vec::with_capacity(plan.requirements.len() + 1);

    for requirement in &plan.requirements {
        let requirement_started = chrono::Utc::now().to_rfc3339();
        let mut subjects = std::collections::HashMap::new();
        let mut evidence_refs = vec![verification_plan_evidence_ref.clone()];
        let (mut verdict, mut detail) = if requirement.validator_id
            == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
            && matches!(&requirement.scope, VerificationScopeV1::Manual)
        {
            for (path, hash) in pending_hashes {
                subjects.insert(
                    format!("workspace-path:{path}"),
                    hash.clone().unwrap_or_else(crate::verification::workspace_path_absence_sha256),
                );
            }
            (
                ValidationVerdictV1::Unverified,
                Some("等待用户对宿主展示的精确候选快照作出验收".to_string()),
            )
        } else if requirement.validator_id == "workspace-command-success-v1" {
            let expected_command = requirement.arguments.get("command").and_then(serde_json::Value::as_str);
            let expected_command_sha = expected_command.map(|command| crate::CasStore::hash_of(command.trim().as_bytes()));
            let command_observation = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match event {
                    TurnEvent::ToolResult { tool, command_receipt: Some(receipt), .. }
                        if tool == "run_command"
                            && Some(receipt.command_sha256.as_str()) == expected_command_sha.as_deref()
                            && receipt.validator_id.as_deref() == Some("workspace-command-success-v1")
                            && receipt.validator_version.as_deref() == Some("1") =>
                    {
                        Some((index, receipt))
                    }
                    _ => None,
                })
                .last();
            match command_observation {
                None => (
                    ValidationVerdictV1::Unverified,
                    Some("本回合没有运行计划登记的宿主行为命令".to_string()),
                ),
                Some((_event_index, receipt)) if receipt.exit_code != 0 => (
                    ValidationVerdictV1::Failed,
                    Some(format!("宿主登记的行为命令失败，exit_code={}", receipt.exit_code)),
                ),
                Some((_, receipt))
                    if receipt.duration_ms.is_none_or(|duration| duration > requirement.resources.timeout_ms) =>
                {
                    (
                        ValidationVerdictV1::Unverified,
                        Some("行为命令缺少宿主耗时证据或超过 VerificationPlan 预算".to_string()),
                    )
                }
                Some((event_index, receipt)) => {
                    let paths = normalized_paths(requirement);
                    let mut snapshot_matches = receipt.workspace_hashes_complete;
                    for relative in &paths {
                        let Some(command_hash) = receipt.workspace_hashes.get(relative) else {
                            snapshot_matches = false;
                            continue;
                        };
                        let Some(current_hash) = workspace_file_hash(root, relative) else {
                            snapshot_matches = false;
                            continue;
                        };
                        let expected_hash = pending_hashes.get(relative);
                        if expected_hash.is_some_and(|expected| expected != command_hash)
                            || command_hash != &current_hash
                        {
                            snapshot_matches = false;
                        }
                        subjects.insert(
                            format!("workspace-path:{relative}"),
                            command_hash.as_ref().cloned().unwrap_or_else(
                                crate::verification::workspace_path_absence_sha256,
                            ),
                        );
                    }
                    let later_mutation = events
                        .iter()
                        .skip(event_index + 1)
                        .any(|event| match event {
                            TurnEvent::ToolResult { tool, ok, .. } if *ok => {
                                crate::tool_effects::effect_class_for(tool)
                                    != crate::tool_effects::EffectClass::Read
                            }
                            _ => false,
                        });
                    evidence_refs.push(format!("command-result:sha256:{}", receipt.result_sha256));
                    if !receipt.workspace_hashes_complete {
                        (
                            ValidationVerdictV1::Unverified,
                            Some("命令执行时宿主文件快照不完整".to_string()),
                        )
                    } else if !snapshot_matches || later_mutation {
                        (
                            ValidationVerdictV1::Stale,
                            Some("行为命令回执与计划路径的最终源码快照不一致".to_string()),
                        )
                    } else {
                        (
                            ValidationVerdictV1::Passed,
                            Some("宿主登记的行为命令成功，覆盖路径与执行及最终源码哈希一致".to_string()),
                        )
                    }
                }
            }
        } else if workspace_validator_arguments_supported(&requirement.validator_id, &requirement.arguments) {
            let (workspace_verdict, workspace_detail, hashes) = execute_workspace_requirement(requirement, root);
            let mut matches_pending = true;
            for (subject, actual_hash) in hashes {
                let relative = subject.strip_prefix("workspace-path:").unwrap_or(&subject);
                let normalized = relative.replace('\\', "/");
                subjects.insert(format!("workspace-path:{normalized}"), actual_hash.clone());
                if pending_hashes.get(&normalized).is_some_and(|expected| expected.as_deref() != Some(actual_hash.as_str())) {
                    matches_pending = false;
                }
                if workspace_file_hash(root, &normalized).and_then(|value| value) != Some(actual_hash) {
                    matches_pending = false;
                }
            }
            if workspace_verdict == ValidationVerdictV1::Passed && !matches_pending {
                (
                    ValidationVerdictV1::Stale,
                    Some("宿主静态验证路径与本次变更或最终文件哈希不一致".to_string()),
                )
            } else {
                (workspace_verdict, workspace_detail)
            }
        } else {
            (
                ValidationVerdictV1::Unsupported,
                Some("VerificationPlan validator 参数未被宿主注册".to_string()),
            )
        };
        if !coverage_ok && requirement.required && verdict == ValidationVerdictV1::Passed {
            verdict = ValidationVerdictV1::Unverified;
            detail = Some(format!(
                "声明的验收项通过，但计划未覆盖所有候选变更；遗漏路径={}，未行为验证源码路径={}",
                missing_paths.join(","),
                missing_behavior_paths.join(",")
            ));
        }
        let receipt_id = format!("single-validation-{}", uuid::Uuid::new_v4());
        if requirement.required {
            match verdict {
                ValidationVerdictV1::Passed | ValidationVerdictV1::ManualAccepted => {
                    passed_count += 1;
                    first_passed_receipt_id.get_or_insert_with(|| receipt_id.clone());
                }
                ValidationVerdictV1::Failed => failed_count += 1,
                ValidationVerdictV1::Stale => stale_evidence = true,
                _ => {}
            }
        }
        plan_receipts.push(ValidationReceiptV1 {
            receipt_id,
            task_id: session.id.clone(),
            attempt_id: turn_id.to_string(),
            epoch: validation_epoch,
            requirement_id: requirement.requirement_id.clone(),
            validator_id: requirement.validator_id.clone(),
            validator_version: requirement.validator_version.clone().unwrap_or_else(|| "unknown".to_string()),
            arguments_sha256: crate::CasStore::hash_of(requirement.arguments.to_string().as_bytes()),
            input_sha256: input_sha256.to_string(),
            environment_id: environment_id.clone(),
            changeset_sha256: Some(changeset_sha256.clone()),
            detail,
            subject_sha256: subjects,
            verdict,
            evidence_refs,
            started_at: requirement_started,
            completed_at: chrono::Utc::now().to_rfc3339(),
        });
    }
    required_count += 1;
    let mut subjects = std::collections::HashMap::new();
    for (path, hash) in pending_hashes {
        subjects.insert(
            format!("workspace-path:{path}"),
            hash.clone().unwrap_or_else(crate::verification::workspace_path_absence_sha256),
        );
    }
    let coverage_detail = if coverage_ok {
        passed_count += 1;
        Some("宿主已确认所有候选变更路径均被必需验证范围覆盖，且所有源码路径均有行为命令覆盖".to_string())
    } else {
        Some(format!(
            "候选变更不在必需验证范围内；遗漏路径={}，未行为验证源码路径={}",
            missing_paths.join(","),
            missing_behavior_paths.join(",")
        ))
    };
    plan_receipts.push(ValidationReceiptV1 {
        receipt_id: format!("single-coverage-{}", uuid::Uuid::new_v4()),
        task_id: session.id.clone(),
        attempt_id: turn_id.to_string(),
        epoch: validation_epoch,
        requirement_id: "host-change-scope-coverage".to_string(),
        validator_id: "host-change-scope-coverage-v1".to_string(),
        validator_version: "1".to_string(),
        arguments_sha256: crate::CasStore::hash_of(plan.plan_id.as_bytes()),
        input_sha256: input_sha256.to_string(),
        environment_id,
        changeset_sha256: Some(changeset_sha256),
        detail: coverage_detail,
        subject_sha256: subjects,
        verdict: if coverage_ok {
            ValidationVerdictV1::Passed
        } else {
            ValidationVerdictV1::Unverified
        },
        evidence_refs: vec![verification_plan_evidence_ref],
        started_at: started_at.clone(),
        completed_at: chrono::Utc::now().to_rfc3339(),
    });
    session.validation_receipts.extend(plan_receipts);

    let status = base_evidence(required_count, passed_count, failed_count, stale_evidence);
    if status == owo_agent_protocol::CompletionStatusV1::Accepted {
        let mut latest_receipt_by_path = std::collections::BTreeMap::new();
        for (index, execution) in session.execution_receipts.iter().enumerate() {
            for path in &execution.changed_files {
                latest_receipt_by_path.insert(path.replace('\\', "/"), index);
            }
        }
        let accepted_receipt_id = first_passed_receipt_id.unwrap_or_default();
        for (index, execution) in session.execution_receipts.iter_mut().enumerate() {
            let paths = execution.changed_files.iter().map(|path| path.replace('\\', "/")).collect::<Vec<_>>();
            if execution.status != "executed" || paths.is_empty() {
                continue;
            }
            let all_paths_are_latest = paths.iter().all(|path| {
                pending_hashes.contains_key(path)
                    && latest_receipt_by_path.get(path) == Some(&index)
            });
            if all_paths_are_latest {
                execution.status = "accepted".to_string();
                execution.validation_receipt_id = Some(accepted_receipt_id.clone());
            } else if paths.iter().any(|path| {
                latest_receipt_by_path.get(path).is_some_and(|latest_index| latest_index != &index)
            }) {
                execution.status = "stale".to_string();
            }
        }
    }
    status
}


#[cfg(test)]
mod single_verification_plan_tests {
    use super::{
        assess_single_turn_completion, CommandExecutionReceipt, TurnEvent,
    };
    use super::single_manual_acceptance::{
        manual_acceptance_answer_verdict, request_single_manual_acceptance,
        SINGLE_MANUAL_ACCEPT_OPTION, SINGLE_MANUAL_REJECT_OPTION,
    };
    use crate::plan::{
        VerificationPlanV1, VerificationRequirementV1, VerificationResourcesV1,
        VerificationScopeV1,
    };
    use crate::session::{ExecutionReceipt, Session};
    use std::collections::HashMap;

    fn plan(validator_id: &str, path: &str, arguments: serde_json::Value) -> VerificationPlanV1 {
        VerificationPlanV1 {
            plan_id: "single-task-plan".to_string(),
            requirements: vec![VerificationRequirementV1 {
                requirement_id: "req-user-visible".to_string(),
                covers_requirement_ids: vec!["user-request:req-user-visible".to_string()],
                validator_id: validator_id.to_string(),
                validator_version: Some("1".to_string()),
                scope: VerificationScopeV1::WorkspacePaths {
                    relative_paths: vec![path.to_string()],
                },
                arguments,
                required: true,
                resources: VerificationResourcesV1 {
                    cpu_slots: 1,
                    memory_mb: 16,
                    exclusive_workspace: false,
                    timeout_ms: 10_000,
                },
            }],
        }
    }

    fn add_write(session: &mut Session, turn_id: &str, relative: &str, hash: &str) {
        session.execution_receipts.push(ExecutionReceipt {
            receipt_id: format!("exec-{turn_id}"),
            tool: "write_file".to_string(),
            turn_id: turn_id.to_string(),
            changed_files: vec![relative.to_string()],
            snapshot_keys: HashMap::new(),
            before_hashes: HashMap::from([(relative.to_string(), None)]),
            after_hashes: HashMap::from([(relative.to_string(), Some(hash.to_string()))]),
            diff_sha256: "diff-hash".to_string(),
            created_at: "2026-10-04T00:00:00Z".to_string(),
            status: "executed".to_string(),
            validation_receipt_id: None,
        });
    }

    #[test]
    fn source_changes_need_a_registered_behavior_check_covering_the_changed_source() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("src").join("lib.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "pub fn ready() -> bool { true }
").unwrap();
        let hash = crate::CasStore::hash_of(&std::fs::read(&source).unwrap());
        let mut session = Session::new(workspace.path(), "mock", None);
        add_write(&mut session, "turn-source", "src/lib.rs", &hash);
        let prompt = "实现 ready 检查";
        session.single_verification_plan = Some(plan(
            "workspace-file-exists-v1",
            "src/lib.rs",
            serde_json::json!({}),
        ));
        session.single_verification_plan_input_sha256 =
            Some(crate::CasStore::hash_of(prompt.as_bytes()));
        session.single_verification_plan_turn_id = Some("turn-source".to_string());

        let status = assess_single_turn_completion(
            &mut session,
            prompt,
            "turn-source",
            &[],
            false,
            Some("已实现并验证。"),
        );
        assert_eq!(status, owo_agent_protocol::CompletionStatusV1::Unverified);
        assert!(session.validation_receipts.iter().any(|receipt| {
            receipt.requirement_id == "host-change-scope-coverage"
                && receipt.verdict == crate::plan::ValidationVerdictV1::Unverified
        }));
    }

    #[test]
    fn generic_command_success_without_a_request_bound_plan_is_only_candidate() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("src").join("lib.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, "pub fn ready() -> bool { true }
").unwrap();
        let hash = crate::CasStore::hash_of(&std::fs::read(&source).unwrap());
        let mut session = Session::new(workspace.path(), "mock", None);
        add_write(&mut session, "turn-unplanned", "src/lib.rs", &hash);
        let command = "cargo test -p owo-agent-core";
        session.single_verification_plan = Some(plan(
            "workspace-command-success-v1",
            "src/lib.rs",
            serde_json::json!({"command":command}),
        ));
        session.single_verification_plan_input_sha256 =
            Some(crate::CasStore::hash_of("实现 ready 检查".as_bytes()));
        session.single_verification_plan_turn_id = Some("older-turn".to_string());
        let events = vec![TurnEvent::ToolResult {
            id: "test-command".to_string(),
            tool: "run_command".to_string(),
            ok: true,
            error: None,
            preview: None,
            command_receipt: Some(CommandExecutionReceipt {
                command_sha256: crate::CasStore::hash_of(command.as_bytes()),
                exit_code: 0,
                result_sha256: "result-hash".to_string(),
                duration_ms: Some(100),
                workspace_hashes_complete: true,
                validator_id: Some("workspace-command-success-v1".to_string()),
                validator_version: Some("1".to_string()),
                workspace_hashes: std::collections::BTreeMap::from([(
                    "src/lib.rs".to_string(),
                    Some(hash),
                )]),
            }),
        }];

        let status = assess_single_turn_completion(
            &mut session,
            "实现 ready 检查",
            "turn-unplanned",
            &events,
            false,
            Some("已实现并运行测试。"),
        );
        assert_eq!(status, owo_agent_protocol::CompletionStatusV1::Candidate);
        assert!(session.validation_receipts.is_empty());
    }

    #[test]
    fn prior_unaccepted_code_does_not_reclassify_a_later_normal_reply() {
        let workspace = tempfile::tempdir().unwrap();
        let mut session = Session::new(workspace.path(), "mock", None);
        add_write(
            &mut session,
            "previous-turn",
            "src/lib.rs",
            "previous-source-hash",
        );

        let status = assess_single_turn_completion(
            &mut session,
            "解释一下所有权",
            "current-chat-turn",
            &[],
            false,
            Some("Rust 所有权用于管理值的生命周期。"),
        );
        assert_eq!(
            status,
            owo_agent_protocol::CompletionStatusV1::ResponseComplete
        );
        assert!(session.validation_receipts.is_empty());
    }

    #[test]
    fn manual_acceptance_requires_the_current_question_and_exact_option() {
        let question_id = "question-current";
        let accepted = crate::question::QuestionAnswer {
            question_id: question_id.to_string(),
            answer: SINGLE_MANUAL_ACCEPT_OPTION.to_string(),
        };
        let declined = crate::question::QuestionAnswer {
            question_id: question_id.to_string(),
            answer: SINGLE_MANUAL_REJECT_OPTION.to_string(),
        };
        let stale_question = crate::question::QuestionAnswer {
            question_id: "question-old".to_string(),
            answer: SINGLE_MANUAL_ACCEPT_OPTION.to_string(),
        };
        assert_eq!(
            manual_acceptance_answer_verdict(Some(&accepted), question_id),
            crate::plan::ValidationVerdictV1::ManualAccepted
        );
        assert_eq!(
            manual_acceptance_answer_verdict(Some(&declined), question_id),
            crate::plan::ValidationVerdictV1::Failed
        );
        assert_eq!(
            manual_acceptance_answer_verdict(Some(&stale_question), question_id),
            crate::plan::ValidationVerdictV1::Unverified
        );
        let free_form = crate::question::QuestionAnswer {
            question_id: question_id.to_string(),
            answer: "yes".to_string(),
        };
        assert_eq!(
            manual_acceptance_answer_verdict(Some(&free_form), question_id),
            crate::plan::ValidationVerdictV1::Unverified
        );
    }

    #[tokio::test]
    async fn manual_acceptance_receipt_binds_the_candidate_snapshot() {
        struct FixedQuestioner {
            answer: String,
            mutate_path: Option<std::path::PathBuf>,
        }

        #[async_trait::async_trait]
        impl crate::question::Questioner for FixedQuestioner {
            async fn ask(
                &self,
                question: &crate::question::UserQuestion,
            ) -> Option<crate::question::QuestionAnswer> {
                if let Some(path) = &self.mutate_path {
                    std::fs::write(path, "changed while waiting").unwrap();
                }
                Some(crate::question::QuestionAnswer {
                    question_id: question.question_id.clone(),
                    answer: self.answer.clone(),
                })
            }
        }

        async fn run_case(mutate: bool) -> (
            owo_agent_protocol::CompletionStatusV1,
            crate::plan::ValidationVerdictV1,
            String,
            String,
            Vec<String>,
            bool,
            bool,
        ) {
            let workspace = tempfile::tempdir().unwrap();
            let source = workspace.path().join("src").join("main.rs");
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(&source, "fn main() {}\n").unwrap();
            let hash = crate::CasStore::hash_of(&std::fs::read(&source).unwrap());
            let mut session = Session::new(workspace.path(), "mock", None);
            add_write(&mut session, "turn-prior", "src/main.rs", &hash);
            let plan = VerificationPlanV1 {
                plan_id: "manual-plan".to_string(),
                requirements: vec![VerificationRequirementV1 {
                    requirement_id: "manual-user-requirement".to_string(),
                    covers_requirement_ids: vec!["user-request:实现可用功能".to_string()],
                    validator_id: crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID.to_string(),
                    validator_version: Some("1".to_string()),
                    scope: VerificationScopeV1::Manual,
                    arguments: serde_json::json!({}),
                    required: true,
                    resources: VerificationResourcesV1::default(),
                }],
            };
            let prompt = "实现可用功能";
            session.single_verification_plan = Some(plan.clone());
            session.single_verification_plan_input_sha256 =
                Some(crate::CasStore::hash_of(prompt.as_bytes()));
            session.single_verification_plan_turn_id = Some("turn-manual".to_string());
            let initial_status = assess_single_turn_completion(
                &mut session,
                prompt,
                "turn-manual",
                &[],
                false,
                Some("候选版本已准备验收。"),
            );
            assert_eq!(initial_status, owo_agent_protocol::CompletionStatusV1::Unverified);
            let questioner = FixedQuestioner {
                answer: SINGLE_MANUAL_ACCEPT_OPTION.to_string(),
                mutate_path: mutate.then(|| source.clone()),
            };
            let abort = std::sync::atomic::AtomicBool::new(false);
            let status = request_single_manual_acceptance(
                &mut session,
                &plan,
                "turn-manual",
                Some(&questioner),
                &abort,
            ).await;
            let review_candidate_present = super::single_review::accepted_candidate_paths(
                &session,
                "turn-manual",
            )
            .contains_key("src/main.rs");
            let notice_is_explicit = super::single_manual_acceptance::completion_notice(
                &session,
                "turn-manual",
            )
            .is_some_and(|notice| notice.contains("不等同于自动行为测试通过"));
            (
                status,
                session.validation_receipts.last().unwrap().verdict,
                session.validation_receipts.last().unwrap().subject_sha256["workspace-path:src/main.rs"].clone(),
                session.execution_receipts[0].status.clone(),
                session.validation_receipts.last().unwrap().evidence_refs.clone(),
                review_candidate_present,
                notice_is_explicit,
            )
        }

        let (
            accepted_status,
            accepted_verdict,
            accepted_hash,
            execution_status,
            evidence_refs,
            review_candidate_present,
            notice_is_explicit,
        ) = run_case(false).await;
        assert_eq!(accepted_status, owo_agent_protocol::CompletionStatusV1::Accepted);
        assert_eq!(accepted_verdict, crate::plan::ValidationVerdictV1::ManualAccepted);
        assert_eq!(accepted_hash, crate::CasStore::hash_of(b"fn main() {}\n"));
        assert_eq!(execution_status, "accepted");
        assert!(review_candidate_present);
        assert!(notice_is_explicit);
        assert!(evidence_refs.iter().any(|reference| reference.starts_with("manual-question:")));
        assert!(evidence_refs.iter().any(|reference| reference.starts_with("user-answer-sha256:")));

        let (stale_status, stale_verdict, _, _, _, _, _) = run_case(true).await;
        assert_eq!(stale_status, owo_agent_protocol::CompletionStatusV1::Unverified);
        assert_eq!(stale_verdict, crate::plan::ValidationVerdictV1::Stale);
    }

    #[test]
    fn missing_current_plan_returns_stable_feedback_for_written_candidate() {
        let workspace = tempfile::tempdir().unwrap();
        let mut session = Session::new(workspace.path(), "mock", None);
        add_write(&mut session, "turn-unplanned", "src/lib.rs", "source-hash");

        let first =
            super::single_missing_verification_plan_feedback(&session, "turn-unplanned");
        let second =
            super::single_missing_verification_plan_feedback(&session, "turn-unplanned");
        assert_eq!(first, second);
        let (fingerprint, feedback) = first.unwrap();
        assert!(!fingerprint.is_empty());
        assert!(feedback.contains("verification_plan"));
        assert!(feedback.contains("src/lib.rs"));
        assert!(feedback.contains("source-hash"));
        assert!(
            super::single_missing_verification_plan_feedback(&session, "other-turn").is_none()
        );
    }

    #[test]
    fn planned_static_check_binds_receipt_to_exact_changed_file_hash() {
        let workspace = tempfile::tempdir().unwrap();
        let doc = workspace.path().join("README.md");
        std::fs::write(&doc, "用户要求：包含 hello
hello
").unwrap();
        let hash = crate::CasStore::hash_of(&std::fs::read(&doc).unwrap());
        let mut session = Session::new(workspace.path(), "mock", None);
        add_write(&mut session, "turn-doc", "README.md", &hash);
        let prompt = "创建说明并包含 hello";
        session.single_verification_plan = Some(plan(
            "workspace-file-contains-v1",
            "README.md",
            serde_json::json!({"text":"hello"}),
        ));
        session.single_verification_plan_input_sha256 =
            Some(crate::CasStore::hash_of(prompt.as_bytes()));
        session.single_verification_plan_turn_id = Some("turn-doc".to_string());

        let status = assess_single_turn_completion(
            &mut session,
            prompt,
            "turn-doc",
            &[],
            false,
            Some("已完成说明。"),
        );
        assert_eq!(status, owo_agent_protocol::CompletionStatusV1::Accepted);
        assert_eq!(session.validation_receipts.len(), 2);
        assert_eq!(session.validation_receipts[0].verdict, crate::plan::ValidationVerdictV1::Passed);
        assert_eq!(session.validation_receipts[0].subject_sha256["workspace-path:README.md"], hash);
        assert_eq!(session.validation_receipts[1].requirement_id, "host-change-scope-coverage");
        assert_eq!(session.validation_receipts[1].verdict, crate::plan::ValidationVerdictV1::Passed);
        assert!(session.validation_receipts[1].evidence_refs[0].starts_with("verification-plan-sha256:"));
        assert_eq!(session.execution_receipts[0].status, "accepted");
    }
}

fn tool_preview(outcome: &Result<serde_json::Value, String>) -> Option<String> {
    let text = match outcome {
        Ok(value) => value.to_string(),
        Err(error) => format!("工具错误：{error}"),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_tool_result(trimmed, TOOL_PREVIEW_CHARS))
}

/// 合成兜底回复：模型在回合结束时没有产出任何内容（空回答/收尾失败）时，
/// 把本回合已执行的工具动作整理成可读摘要，保证用户总能得到明确结论，
/// 而不是只看到一段越来越长的「思考过程」后什么都没有。
fn synthesize_fallback_reply(messages: &[ChatMessage], steps: usize, reason: &str) -> String {
    let mut actions: Vec<String> = Vec::new();
    for message in messages {
        let Some(calls) = &message.tool_calls else {
            continue;
        };
        for call in calls {
            let preview = tool_call_preview(&call.arguments);
            let line = if preview.is_empty() {
                format!("- `{}`", call.name)
            } else {
                format!("- `{}`：{preview}", call.name)
            };
            if !actions.contains(&line) {
                actions.push(line);
            }
        }
    }
    let shown = actions.len().min(FALLBACK_ACTION_LIMIT);
    let mut body = String::new();
    body.push_str("> ⚠️ 本回合模型没有产出正式回答（");
    body.push_str(reason);
    body.push_str("）。以下为系统自动整理的工作摘要，供你确认或让我继续。\n\n");
    if actions.is_empty() {
        body.push_str("**本轮没有执行任何工具操作，也没有产出文本内容。**\n\n");
    } else {
        body.push_str(&format!(
            "**本回合共执行 {steps} 步工具操作（列出前 {shown} 条）：**\n\n"
        ));
        for line in actions.iter().take(shown) {
            body.push_str(line);
            body.push('\n');
        }
        if actions.len() > shown {
            body.push_str(&format!("- …（其余 {} 条已省略）\n", actions.len() - shown));
        }
        body.push('\n');
    }
    body.push_str("你可以直接回复「继续」让我接着完成剩余部分，或指出需要调整的地方。");
    body
}

/// 工具调用参数摘要：优先取路径/命令等最具信息量的字段，截断到预览长度上限。
fn tool_call_preview(arguments: &serde_json::Value) -> String {
    const KEYS: [&str; 6] = ["path", "command", "file", "pattern", "query", "url"];
    let raw = KEYS
        .iter()
        .find_map(|key| arguments.get(key).and_then(serde_json::Value::as_str))
        .or_else(|| {
            arguments
                .as_object()
                .and_then(|map| map.values().find_map(serde_json::Value::as_str))
        })
        .unwrap_or_default()
        .trim();
    if raw.is_empty() {
        return String::new();
    }
    let mut preview: String = raw.chars().take(FALLBACK_ACTION_PREVIEW_CHARS).collect();
    if raw.chars().count() > FALLBACK_ACTION_PREVIEW_CHARS {
        preview.push('…');
    }
    preview.replace('\n', " ")
}

/// 每条消息的固定开销（role / 分隔符等）。
pub const MESSAGE_OVERHEAD: usize = 4;

/// 图片消息的 token 估算（取优合并自远端 engine）：视觉输入 token 随分辨率
/// 浮动（数百到数千），按 1100 中位值计入预算，防止带图消息把 token 估算打穿。
pub const IMAGE_TOKEN_ESTIMATE: usize = 1_100;

/// 单段文本的 token 估算：CJK ≈ 1 token/字，ASCII 按 4 字符/token。
/// （不引入 tiktoken 原生依赖；与真实 cl100k 计数同量级，用于预算与压缩触发。）
fn text_token_estimate(text: &str) -> usize {
    let mut tokens = 0usize;
    let mut ascii_run = 0usize;
    for ch in text.chars() {
        let wide = (ch as u32) >= 0x2E80; // CJK 及全角标点
        if wide {
            tokens += 1;
            ascii_run = 0;
        } else if ch.is_ascii_alphanumeric() || ch == ' ' {
            ascii_run += 1;
            if ascii_run == 4 {
                tokens += 1;
                ascii_run = 0;
            }
        } else {
            tokens += 1;
            ascii_run = 0;
        }
    }
    if ascii_run > 0 {
        tokens += 1;
    }
    tokens
}

/// token 估算（P1-1 口径）：CJK 按 1 token/字，图片按 [`IMAGE_TOKEN_ESTIMATE`]。
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| {
            text_token_estimate(message.content.as_deref().unwrap_or_default())
                + MESSAGE_OVERHEAD
                + message.images.len() * IMAGE_TOKEN_ESTIMATE
        })
        .sum()
}

/// 压缩保留段起点对齐（A4-2）：切点落在 tool 群组内时——
/// 有前置 assistant(tool_calls) 则回退到该消息（调用与结果同进同出）；
/// 否则跳过整个孤儿 tool 群组（脏历史直接发给模型会 400）。
fn align_keep_start(messages: &[ChatMessage], start: usize) -> usize {
    if start >= messages.len() || messages[start].role != "tool" {
        return start;
    }
    let mut group_start = start;
    while group_start > 1 && messages[group_start - 1].role == "tool" {
        group_start -= 1;
    }
    if group_start > 1
        && messages[group_start - 1].role == "assistant"
        && messages[group_start - 1].tool_calls.is_some()
    {
        return group_start - 1;
    }
    let mut after = start;
    while after < messages.len() && messages[after].role == "tool" {
        after += 1;
    }
    after
}

/// 摘要压缩的切分点：返回保留段起点，`None` 表示不该压缩（历史太短或对齐后无内容）。
///
/// 为什么单独抽出来：`keep_recent` 是**按条数**切的，而一个工具回合里
/// `assistant(tool_calls)` 与它的若干 `tool` 结果是多条消息。切点若落进群组中间，
/// 保留段会以**孤立 tool 消息**开头，拼上摘要直接发模型就是 400：
///   "Messages with role 'tool' must be a response to a preceding message with
///    'tool_calls'"
/// 实测症状：跑了 33 个工具的回合，压缩提示刚出现，下一轮请求整体失败。
/// 同文件另两条裁剪路径（[`compact_truncate`] / [`compact_truncate_to_budget`]）
/// 都已对齐，只有摘要压缩这条漏了；抽成纯函数也便于回归测试钉住。
fn compaction_split(messages: &[ChatMessage], keep_recent: usize) -> Option<usize> {
    let head_end = messages.len().saturating_sub(keep_recent);
    if head_end < 4 {
        return None;
    }
    let tail_start = align_keep_start(messages, head_end);
    // 对齐把整段都让出去了（切点后全是孤儿 tool 群组）：不压缩，保持原样。
    if tail_start >= messages.len() {
        return None;
    }
    Some(tail_start)
}

/// 存量脏历史归一（取优合并自远端 engine）：发请求前调用——
/// 丢弃无配对的孤立 tool 消息；给有 tool_calls 但没有结果的 assistant 补占位结果
/// （中断半截提交/外部导入的会话直接发给模型会 400）。
fn sanitize_history(messages: &mut Vec<ChatMessage>) {
    fn flush_pending(pending: &mut Vec<String>, cleaned: &mut Vec<ChatMessage>) {
        for id in pending.drain(..) {
            cleaned.push(ChatMessage::tool(
                id,
                "工具结果缺失（该回合被中断或未完成）".to_string(),
            ));
        }
    }

    let mut cleaned: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut pending: Vec<String> = Vec::new();
    for message in messages.drain(..) {
        match message.role.as_str() {
            "tool" => {
                let id = message.tool_call_id.clone().unwrap_or_default();
                if let Some(position) = pending.iter().position(|call_id| *call_id == id) {
                    pending.remove(position);
                    cleaned.push(message);
                }
                // 无配对（孤立 tool）：丢弃。
            }
            "assistant" => {
                flush_pending(&mut pending, &mut cleaned);
                if let Some(calls) = &message.tool_calls {
                    pending.extend(calls.iter().map(|call| call.id.clone()));
                }
                cleaned.push(message);
            }
            _ => {
                flush_pending(&mut pending, &mut cleaned);
                cleaned.push(message);
            }
        }
    }
    flush_pending(&mut pending, &mut cleaned);
    *messages = cleaned;
}

/// 按 token 预算硬裁剪历史（A4-3 兜底）：从尾部保留尽量多的消息（对齐 tool
/// 群组切点），使 [`estimate_tokens`] 回到 `budget` 内。
///
/// 与条数版 [`compact_truncate`] 的区别：模型上下文是 token 硬约束，压缩模型
/// 调用失败时条数检查挡不住 token 超限。
fn compact_truncate_to_budget(messages: &mut Vec<ChatMessage>, budget: usize) {
    let token_of = |message: &ChatMessage| {
        text_token_estimate(message.content.as_deref().unwrap_or_default())
            + MESSAGE_OVERHEAD
            + message.images.len() * IMAGE_TOKEN_ESTIMATE
    };
    // system（messages[0]）必保留；从尾部往前累计，找出预算内可保留的 tail。
    let mut acc = messages.first().map(&token_of).unwrap_or(0);
    let mut keep = 0usize;
    for message in messages.iter().skip(1).rev() {
        let tokens = token_of(message);
        if acc + tokens > budget && keep > 0 {
            break;
        }
        acc += tokens;
        keep += 1;
    }
    if keep == 0 || keep >= messages.len() {
        return;
    }
    let tail_start = align_keep_start(messages, messages.len() - keep);
    // 对齐可能把 tail 推到群组之后甚至越界：越界时不裁（保守，不破坏序列）。
    if tail_start >= messages.len() {
        return;
    }
    let mut tail = messages[tail_start..].to_vec();
    let system = messages[0].clone();
    tail.insert(0, system);
    *messages = tail;
}

/// 回合事件出口的共享单元：父回合与嵌套子代理（`SubagentRunner.events`）共用同一份，
/// 保证子代理的审批请求/工具进度在父回合 await 期间也能即时外发。
pub(crate) type EventCell<'a> = Arc<Mutex<&'a mut (dyn FnMut(&TurnEvent) + Send + 'a)>>;

/// 回合事件出口：`on_event` 用共享单元传递（子代理/嵌套回合持同一单元即时回传）。
fn emit(events: &mut Vec<TurnEvent>, on_event: &EventCell<'_>, event: TurnEvent) {
    // 中毒也继续转发（见 nested_sink 注释）。
    let mut forward = on_event
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    forward(&event);
    events.push(event);
}

fn compact_truncate(messages: &mut Vec<ChatMessage>, limit: usize) {
    if messages.len() <= limit {
        return;
    }
    let keep = limit.saturating_sub(1);
    let tail_start = align_keep_start(messages, messages.len().saturating_sub(keep));
    if tail_start >= messages.len() {
        return;
    }
    let mut tail = messages[tail_start..].to_vec();
    let system = messages[0].clone();
    tail.insert(0, system);
    *messages = tail;
}
