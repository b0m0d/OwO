use crate::audit::AuditLog;
use crate::autoreview::{ReviewVerdict, Reviewer};
use crate::context::{build_system_prompt, load_project_rules};
use crate::deadline::{DeadlineBudget, Phase, PhaseBudgets, PhaseTiming};
use crate::error::AgentError;
use crate::gateway::{ChatMessage, ModelOutput, ModelProvider, TokenUsage};
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

#[cfg(test)]
mod tests;

pub use config::AgentConfig;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TurnEvent {
    ModelCall,
    TokenDelta {
        delta: String,
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
    },
    Final {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutcome {
    pub final_text: Option<String>,
    pub steps: usize,
    pub events: Vec<TurnEvent>,
    pub prompt: String,
    pub started_at: String,
    pub duration_ms: u64,
    /// 本回合模型 token 用量增量（provider 累计快照差值）。
    #[serde(default)]
    pub usage: TokenUsage,
    /// §9.3：阶段耗时瀑布（model/approval/tool/persistence，按发生顺序）。
    #[serde(default)]
    pub phase_timings: Vec<crate::deadline::PhaseTiming>,
    /// §9.3：工具面稳定指纹（SHA-256；schema 缓存复用的 key 基础）。
    #[serde(default)]
    pub tools_fingerprint: String,
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
        }
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
        // §5.2：先按 config 声明宿主可信只读（server+tool+schema hash），
        // 随后 register 时 hash 匹配的 readOnlyHint 才能降级为 Read。
        crate::tool_effects::declare_trusted_from_config(config, &tools);
        let tool_count = tools.len();
        self.register_mcp_tools(
            &config.name,
            Arc::new(tokio::sync::Mutex::new(client)),
            tools,
        );
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
        let abort = AtomicBool::new(false);
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
            max_turns: self.config.max_turns,
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
        let started_at = Utc::now().to_rfc3339();
        let started = std::time::Instant::now();
        let turn_id = uuid::Uuid::new_v4().to_string();
        let usage_before = self.provider.usage_snapshot();
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
        messages.push(ChatMessage::user(prompt.to_string()));
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

        for _index in 0..self.config.max_turns {
            if abort.load(Ordering::Relaxed) {
                commit_turn_messages(session, &messages);
                return Err(AgentError::Aborted);
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
            let mut emit_delta = |delta: String| {
                // §9.3 瀑布：首个增量到达即记录首 token 时延（compare_exchange 保证只记首次）。
                let _ = first_token_ms.compare_exchange(
                    FIRST_TOKEN_UNSET,
                    model_started.elapsed().as_millis() as u64,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                );
                emit(
                    &mut events,
                    on_event_reborrow,
                    TurnEvent::TokenDelta { delta },
                );
            };
            // §9.2：每次模型调用（即下一回合的 retry 点）前复查剩余预算；
            // 激活时以阶段剩余预算包裹超时，超时即结构化失败。
            // M4.2 会话级路由：显式覆盖（创建会话时指定）进请求体；未覆盖时
            // Provider 自行解析（OPENAI_MODEL 热切换 → 启动配置 → 内置默认）。
            // 克隆出循环体，避免与 `commit_turn_messages(session, …)` 的再借用冲突。
            let wire_model = session.model_override.clone();
            let attempt = async {
                tokio::select! {
                    output = self.provider.complete_stream_with_model(
                        wire_model.as_deref(),
                        &messages,
                        &tools,
                        &mut emit_delta,
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
            let output = match output {
                Ok(output) => output,
                Err(error) => {
                    commit_turn_messages(session, &messages);
                    return Err(error);
                }
            };
            let model_elapsed = model_started.elapsed();
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
                ModelOutput::Text(text) => {
                    messages.push(ChatMessage::assistant_text(text.clone()));
                    final_text = Some(text.clone());
                    emit(&mut events, &event_cell, TurnEvent::Final { text });
                    break;
                }
                ModelOutput::ToolCalls(calls) => {
                    // 循环保护（对标 Codex/OpenCode）：先查总量上限，再逐调用查重复。
                    if tool_calls_seen + calls.len() > self.config.max_tool_calls_per_turn {
                        let limit = self.config.max_tool_calls_per_turn;
                        commit_turn_messages(session, &messages);
                        return Err(AgentError::Gateway(format!(
                            "循环保护：单回合工具调用达到上限 {limit}（已请求 {tool_calls_seen} + 本批 {}）。已停止执行以避免失控循环；请缩小任务或分步重试。",
                            calls.len()
                        )));
                    }
                    tool_calls_seen += calls.len();
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
                                let capability_context = ToolCapabilityContext::for_workspace(
                                    &workspace,
                                    session.id.clone(),
                                    turn_id.clone(),
                                );
                                // 并发组内全部为宿主验证只读工具（经审计不改变会话
                                // 状态）；Session 按值克隆以满足 ToolContext 的 &mut
                                // 签名，克隆上的任何变更被有意丢弃（读取语义不变）。
                                let mut session_view = session.clone();
                                let subagent = SubagentRunner {
                                    provider: Arc::clone(&self.provider),
                                    approver,
                                    abort,
                                    depth: self.config.subagent_depth,
                                    max_turns: self.config.max_turns,
                                    model: session.model_override.clone().unwrap_or_default(),
                                    events: Some(Arc::clone(&nested_sink)),
                                };
                                // A5-1：fan-out 通道（owned，'static 闭包约束）。
                                let fanout = crate::subagent::FanOutRunner {
                                    provider: Arc::clone(&self.provider),
                                    workspace: workspace.clone(),
                                    model: session_view.model_override.clone().unwrap_or_default(),
                                    depth: self.config.subagent_depth,
                                    max_turns: self.config.max_turns,
                                };
                                let sink = Arc::clone(&group_events);
                                let call_id = call.id.clone();
                                let tool_name = call.name.clone();
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
                                        buffer.push(TurnEvent::ToolResult {
                                            id: call_id,
                                            tool: tool_name,
                                            ok: outcome.is_ok(),
                                            error: outcome.as_ref().err().cloned(),
                                            preview: tool_preview(&outcome),
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
                                );
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
                                    max_turns: self.config.max_turns,
                                    model: session.model_override.clone().unwrap_or_default(),
                                    events: Some(Arc::clone(&nested_sink)),
                                };
                                let fanout = crate::subagent::FanOutRunner {
                                    provider: Arc::clone(&self.provider),
                                    workspace: workspace.clone(),
                                    model: session.model_override.clone().unwrap_or_default(),
                                    depth: self.config.subagent_depth,
                                    max_turns: self.config.max_turns,
                                };
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
                                emit(
                                    &mut events,
                                    &event_cell,
                                    TurnEvent::ToolResult {
                                        id: call.id.clone(),
                                        tool: call.name.clone(),
                                        ok: outcome.is_ok(),
                                        error: outcome.as_ref().err().cloned(),
                                        preview: tool_preview(&outcome),
                                    },
                                );
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
            let wrap_up = self
                .provider
                .complete_stream_with_model(
                    wrap_model.as_deref(),
                    &wrap_messages,
                    &[],
                    &mut emit_wrap_delta,
                )
                .await;
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
        let persist_elapsed = persist_started.elapsed();
        budget.record(Phase::Persistence, persist_elapsed);
        phase_timings.push(PhaseTiming {
            phase: Phase::Persistence.as_str().to_string(),
            elapsed_ms: persist_elapsed.as_millis() as u64,
            target: String::new(),
            first_token_ms: None,
        });
        let usage = self.provider.usage_snapshot().saturating_sub(&usage_before);
        Ok(TurnOutcome {
            final_text,
            steps,
            events,
            prompt: prompt.to_string(),
            started_at,
            duration_ms: started.elapsed().as_millis() as u64,
            usage,
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
        let head_end = messages.len().saturating_sub(self.config.keep_recent);
        if head_end < 4 {
            return Ok(None);
        }
        let head = messages[1..head_end].to_vec();
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
            Ok(_) | Err(_) => return Ok(None),
        };
        let mut compacted = vec![messages[0].clone()];
        compacted.push(ChatMessage::system(format!(
            "历史摘要（已压缩）：\n{summary}"
        )));
        compacted.extend(messages[head_end..].to_vec());
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

/// 粗略 token 估算：字符数 / 2 + 每条消息固定开销。
pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|message| {
            let chars = message
                .content
                .as_deref()
                .map(str::chars)
                .map(|chars| chars.count())
                .unwrap_or(0);
            chars / 2 + 4
        })
        .sum()
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
    let mut tail_start = messages.len().saturating_sub(keep);
    if tail_start < messages.len() && messages[tail_start].role == "tool" {
        let mut group_start = tail_start;
        while group_start > 1 && messages[group_start - 1].role == "tool" {
            group_start -= 1;
        }
        if group_start > 1
            && messages[group_start - 1].role == "assistant"
            && messages[group_start - 1].tool_calls.is_some()
        {
            tail_start = group_start - 1;
        } else {
            while tail_start < messages.len() && messages[tail_start].role == "tool" {
                tail_start += 1;
            }
        }
    }
    let mut tail = messages[tail_start..].to_vec();
    let system = messages[0].clone();
    tail.insert(0, system);
    *messages = tail;
}
