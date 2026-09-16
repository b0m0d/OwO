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
use crate::tools::{ToolContext, ToolRegistry};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

const MAX_TOOL_RESULT_CHARS: usize = 50_000;

/// §9.1 阶段一产物：按原始 tool-call 顺序完成的权限判定（Ask 已归并为 Allow/Deny）。
struct PreparedCall {
    /// 归并后的放行结论。
    approved: bool,
    /// 权限理由（拒绝消息文案）。
    reason: String,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_turns: usize,
    pub context_limit: usize,
    pub subagent_depth: usize,
    pub token_budget: usize,
    pub keep_recent: usize,
    pub compaction_enabled: bool,
    /// §9.1：单个并发组内只读工具的最大并发数（默认 4；下限 1）。
    pub tool_concurrency: usize,
    /// §9.2：turn 级统一截止时间；None = 不限时（保持既有行为，仅记账）。
    pub turn_deadline: Option<std::time::Duration>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 60,
            context_limit: 200,
            subagent_depth: 0,
            token_budget: 60_000,
            keep_recent: 20,
            compaction_enabled: true,
            tool_concurrency: 4,
            turn_deadline: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    },
    ToolResult {
        id: String,
        tool: String,
        ok: bool,
        error: Option<String>,
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
pub struct Agent {
    provider: Arc<dyn ModelProvider>,
    /// 工具注册表（RwLock：MCP 服务器热连接/热卸载时无需重建 Agent）。
    registry: Arc<RwLock<ToolRegistry>>,
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
        Self {
            provider,
            registry: Arc::new(RwLock::new(registry)),
            disabled_tool_prefixes: Arc::new(RwLock::new(HashSet::new())),
            mcp_clients: Arc::new(crate::mcp::McpRegistry::new()),
            mcp_health: Arc::new(crate::mcp_health::McpHealthTracker::new(
                crate::mcp_health::McpHealthConfig::default(),
            )),
            reviewer: None,
            policy,
            audit: Arc::new(Mutex::new(AuditLog::default())),
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
        config: &crate::mcp::McpServerConfig,
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
        // 直呼子代理没有可回传到客户端的审批通道：只读模式可以自动放行，
        // 通用模式必须默认拒绝写入/执行，避免子代理绕过主会话审批。
        let approver = crate::permissions::AutoApprover { allow: read_only };
        let runner = SubagentRunner {
            provider: Arc::clone(&self.provider),
            approver: &approver,
            abort: &abort,
            depth: self.config.subagent_depth,
            max_turns: self.config.max_turns,
            model: model.to_string(),
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

    pub fn registry(&self) -> Arc<RwLock<ToolRegistry>> {
        Arc::clone(&self.registry)
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

        for _index in 0..self.config.max_turns {
            if abort.load(Ordering::Relaxed) {
                commit_turn_messages(session, &messages);
                return Err(AgentError::Aborted);
            }
            let compaction = self.maybe_compact(&mut messages, &session.id).await;
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
                    on_event,
                    TurnEvent::Compaction {
                        summary: summary.clone(),
                    },
                );
            }
            if messages.len() > self.config.context_limit {
                compact_truncate(&mut messages, self.config.context_limit);
            }

            emit(&mut events, on_event, TurnEvent::ModelCall);
            let on_event_reborrow = &mut *on_event;
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
            let attempt = async {
                tokio::select! {
                    output = self.provider.complete_stream(&messages, &tools, &mut emit_delta) => {
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
                ModelOutput::Text(text) => {
                    messages.push(ChatMessage::assistant_text(text.clone()));
                    final_text = Some(text.clone());
                    emit(&mut events, on_event, TurnEvent::Final { text });
                    break;
                }
                ModelOutput::ToolCalls(calls) => {
                    messages.push(ChatMessage::assistant_tool_calls(calls.clone()));
                    // §9.1 阶段一——权限判定保持原始 tool-call 顺序：Ask 的独立审批
                    // 与用户审批仍按原序逐个交互，先得到每个调用的 Allow/Deny。
                    let mut prepared: Vec<PreparedCall> = Vec::with_capacity(calls.len());
                    for call in &calls {
                        if abort.load(Ordering::Relaxed) {
                            commit_turn_messages(session, &messages);
                            return Err(AgentError::Aborted);
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
                                            on_event,
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
                        let approved = decision == Decision::Allow;
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
                            approved,
                            reason: request.reason.clone(),
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
                        let eligible_here = prepared[index].approved
                            && !self.tool_disabled(&calls[index].name)
                            && self.call_is_concurrent_eligible(&calls[index]);
                        if eligible_here {
                            // 连续可并发段（上限 tool_concurrency）；遇任何不满足条件
                            // 的调用立即断组——写/执行紧邻只读时绝不进同一组。
                            let start = index;
                            let mut end = start + 1;
                            while end < calls.len()
                                && end - start < self.config.tool_concurrency.max(1)
                                && prepared[end].approved
                                && !self.tool_disabled(&calls[end].name)
                                && self.call_is_concurrent_eligible(&calls[end])
                            {
                                end += 1;
                            }
                            let mut futures = Vec::with_capacity(end - start);
                            for call in &calls[start..end] {
                                let workspace = session.workspace.clone();
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
                                    model: session.model.clone(),
                                };
                                let tool = self
                                    .registry
                                    .read()
                                    .map_err(|_| AgentError::Session("工具注册表锁中毒".into()))?
                                    .get(&call.name);
                                let sink = Arc::clone(&group_events);
                                let call_id = call.id.clone();
                                let tool_name = call.name.clone();
                                let arguments = call.arguments.clone();
                                if let Ok(mut buffer) = sink.lock() {
                                    buffer.push(TurnEvent::ToolStart {
                                        id: call_id.clone(),
                                        tool: tool_name.clone(),
                                    });
                                }
                                futures.push(async move {
                                    let mut ctx = ToolContext {
                                        workspace: &workspace,
                                        policy: &self.policy,
                                        session: &mut session_view,
                                        audit: &self.audit,
                                        subagent: Some(subagent),
                                        skills: &self.skills,
                                        elements: &self.elements,
                                    };
                                    let outcome = match tool {
                                        Some(tool) => tool.run(&mut ctx, arguments).await,
                                        None => Err(format!("未知工具：{tool_name}")),
                                    };
                                    if let Ok(mut buffer) = sink.lock() {
                                        buffer.push(TurnEvent::ToolResult {
                                            id: call_id,
                                            tool: tool_name,
                                            ok: outcome.is_ok(),
                                            error: outcome.as_ref().err().cloned(),
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
                                tokio::select! {
                                    outcomes = futures::future::join_all(futures) => outcomes,
                                    _ = wait_for_abort(abort) => {
                                        commit_turn_messages(session, &messages);
                                        return Err(AgentError::Aborted);
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
                            // 组事件统一转发：组完成时按 ToolStart…/ToolResult 插入序
                            // 回放（并发工具的 ToolStart 不再实时流式，属预期取舍）。
                            if let Ok(mut buffer) = group_events.lock() {
                                for event in buffer.drain(..) {
                                    emit(&mut events, on_event, event);
                                }
                            }
                            index = end;
                        } else {
                            // —— 串行执行单个调用（与原实现逐字等价）——
                            let call = &calls[index];
                            let result = if self.tool_disabled(&call.name) {
                                Err(format!("工具已被禁用（插件热卸载）：{}", call.name))
                            } else if prepared[index].approved {
                                let workspace = session.workspace.clone();
                                emit(
                                    &mut events,
                                    on_event,
                                    TurnEvent::ToolStart {
                                        id: call.id.clone(),
                                        tool: call.name.clone(),
                                    },
                                );
                                let subagent = SubagentRunner {
                                    provider: Arc::clone(&self.provider),
                                    approver,
                                    abort,
                                    depth: self.config.subagent_depth,
                                    max_turns: self.config.max_turns,
                                    model: session.model.clone(),
                                };
                                let mut ctx = ToolContext {
                                    workspace: &workspace,
                                    policy: &self.policy,
                                    session,
                                    audit: &self.audit,
                                    subagent: Some(subagent),
                                    skills: &self.skills,
                                    elements: &self.elements,
                                };
                                let tool = self
                                    .registry
                                    .read()
                                    .map_err(|_| AgentError::Session("工具注册表锁中毒".into()))?
                                    .get(&call.name);
                                let tool_started = std::time::Instant::now();
                                let outcome = match tool {
                                    Some(tool) => {
                                        let run = tool.run(&mut ctx, call.arguments.clone());
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
                                    None => Err(format!("未知工具：{}", call.name)),
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
                                    on_event,
                                    TurnEvent::ToolResult {
                                        id: call.id.clone(),
                                        tool: call.name.clone(),
                                        ok: outcome.is_ok(),
                                        error: outcome.as_ref().err().cloned(),
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
            commit_turn_messages(session, &messages);
            return Err(AgentError::Gateway(format!(
                "达到最大回合数（{}），任务未正常结束",
                self.config.max_turns
            )));
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
    async fn maybe_compact(
        &self,
        messages: &mut Vec<ChatMessage>,
        session_id: &str,
    ) -> Result<Option<String>, AgentError> {
        if !self.config.compaction_enabled || estimate_tokens(messages) <= self.config.token_budget
        {
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
        let summary = match self
            .provider
            .complete(&[ChatMessage::user(prompt)], &[])
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

fn emit(
    events: &mut Vec<TurnEvent>,
    on_event: &mut (dyn FnMut(&TurnEvent) + Send),
    event: TurnEvent,
) {
    on_event(&event);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_counts_chars_and_overhead() {
        let messages = vec![
            ChatMessage::system("规则".to_string()),
            ChatMessage::user("你好，请帮我总结这段代码".to_string()),
            ChatMessage::assistant_text("好的。".to_string()),
        ];
        let total = estimate_tokens(&messages);
        // 每条约 +4 开销：3 条 → 12；正文 ≈ (2 + 12 + 3)/2。
        assert!(
            (15..=25).contains(&total),
            "估算 token {total} 应在合理区间"
        );
    }

    #[test]
    fn empty_messages_cost_zero() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn compact_truncate_keeps_system_and_recent_tail() {
        let mut messages = vec![ChatMessage::system("系统".to_string())];
        for index in 0..10 {
            messages.push(ChatMessage::user(format!("消息{index}")));
        }
        compact_truncate(&mut messages, 4);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "system");
        assert!(messages
            .iter()
            .any(|message| message.content.as_deref() == Some("消息9")));
        assert!(messages
            .iter()
            .any(|message| message.content.as_deref() == Some("消息7")));
    }

    #[test]
    fn compact_truncate_keeps_tool_call_and_results_together() {
        let mut messages = vec![
            ChatMessage::system("系统".to_string()),
            ChatMessage::user("旧请求".to_string()),
            ChatMessage::assistant_tool_calls(vec![crate::gateway::ToolCall {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            }]),
            ChatMessage::tool("call-1".to_string(), "结果".to_string()),
            ChatMessage::user("继续".to_string()),
            ChatMessage::assistant_text("好的".to_string()),
        ];

        compact_truncate(&mut messages, 4);

        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "assistant");
        assert!(messages[1].tool_calls.is_some());
        assert_eq!(messages[2].role, "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn tool_result_is_bounded_without_splitting_unicode() {
        let result = truncate_tool_result(&"中".repeat(10), 3);
        assert!(result.starts_with("中中中"));
        assert!(result.contains("工具输出已截断"));
    }

    // ---------- §9.1 安全并发只读工具 ----------

    use crate::tools::{Tool, ToolSpec};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    /// §9.1 共享探针状态：活跃数峰值（并发观测）+ 完成名单（顺序/取消观测）。
    struct ProbeState {
        active: AtomicUsize,
        peak: Mutex<usize>,
        completed: Mutex<Vec<String>>,
    }

    impl ProbeState {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                active: AtomicUsize::new(0),
                peak: Mutex::new(0),
                completed: Mutex::new(Vec::new()),
            })
        }

        fn begin(&self) {
            let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let mut peak = self.peak.lock().unwrap();
            if current > *peak {
                *peak = current;
            }
        }

        fn end(&self, label: &str) {
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.completed.lock().unwrap().push(label.to_string());
        }
    }

    /// effect 可配置的探针工具（验证只读 / 自报只读 / 写），供分组断言。
    struct ProbeTool {
        label: &'static str,
        delay_ms: u64,
        class: EffectClass,
        host_verified: bool,
        state: Arc<ProbeState>,
    }

    #[async_trait::async_trait]
    impl Tool for ProbeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::with_effect(
                self.label,
                "并发探针".to_string(),
                serde_json::json!({ "type": "object" }),
                Some(crate::tool_effects::ToolEffect {
                    tool: self.label.to_string(),
                    class: self.class,
                    source: "builtin".to_string(),
                    risk_note: None,
                    annotations: None,
                    host_verified_readonly: self.host_verified,
                }),
            )
        }

        async fn run(
            &self,
            _ctx: &mut ToolContext<'_>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.state.begin();
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            self.state.end(self.label);
            Ok(serde_json::json!({ "tool": self.label, "delay_ms": self.delay_ms }))
        }
    }

    /// 固定脚本 provider：按序吐出预置输出（工具调用轮 + 文本轮）。
    struct ScriptedTestProvider {
        outputs: Mutex<VecDeque<ModelOutput>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for ScriptedTestProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            self.outputs
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| "脚本输出已耗尽".to_string())
        }
    }

    /// 两个探针调用的脚本：第一轮按给定顺序发两个 tool-call，随后文本收尾。
    fn two_call_then_text(first: &str, second: &str) -> Mutex<VecDeque<ModelOutput>> {
        Mutex::new(VecDeque::from(vec![
            ModelOutput::ToolCalls(vec![
                crate::gateway::ToolCall {
                    id: format!("call-{first}"),
                    name: first.to_string(),
                    arguments: serde_json::json!({}),
                },
                crate::gateway::ToolCall {
                    id: format!("call-{second}"),
                    name: second.to_string(),
                    arguments: serde_json::json!({}),
                },
            ]),
            ModelOutput::Text("完成".to_string()),
        ]))
    }

    async fn run_with(
        registry: ToolRegistry,
        outputs: Mutex<VecDeque<ModelOutput>>,
        abort: &AtomicBool,
    ) -> Result<(TurnOutcome, Session), AgentError> {
        let provider = Arc::new(ScriptedTestProvider { outputs });
        let agent = Agent::new(
            provider,
            registry,
            Policy::new("."),
            AgentConfig {
                max_turns: 4,
                ..Default::default()
            },
        );
        let mut session = Session::new(std::env::temp_dir(), "test-model", None);
        let approver = crate::permissions::AutoApprover { allow: true };
        let outcome = agent
            .run_turn(&mut session, "跑探针", &approver, abort, &mut |_| {})
            .await?;
        Ok((outcome, session))
    }

    #[tokio::test]
    async fn two_verified_read_tools_execute_concurrently() {
        let state = ProbeState::new();
        let mut registry = ToolRegistry::new();
        for label in ["probe_a", "probe_b"] {
            registry.register(ProbeTool {
                label,
                delay_ms: 120,
                class: EffectClass::Read,
                host_verified: true,
                state: Arc::clone(&state),
            });
        }
        let (outcome, _) = run_with(
            registry,
            two_call_then_text("probe_a", "probe_b"),
            &AtomicBool::new(false),
        )
        .await
        .expect("回合应成功");
        assert_eq!(outcome.final_text.as_deref(), Some("完成"));
        assert_eq!(state.completed.lock().unwrap().len(), 2, "两个工具都应完成");
        assert!(
            *state.peak.lock().unwrap() >= 2,
            "两个宿主验证只读工具必须并发（活跃峰值 ≥2）"
        );
    }

    /// §9.3 瀑布：model 阶段记录首 token 时延（流式增量首达时刻 ≤ 总耗时）。
    #[tokio::test]
    async fn model_phase_timing_carries_first_token_latency() {
        let (outcome, _) = run_with(
            ToolRegistry::new(),
            Mutex::new(VecDeque::from(vec![ModelOutput::Text("完成".to_string())])),
            &AtomicBool::new(false),
        )
        .await
        .expect("回合应成功");
        let model = outcome
            .phase_timings
            .iter()
            .find(|timing| timing.phase == "model")
            .expect("应有 model 阶段瀑布记录");
        assert!(
            model.first_token_ms.is_some(),
            "§9.3：model 阶段应记录首 token 时延（默认流式包装会发首个增量）"
        );
        assert!(
            model.first_token_ms.unwrap() <= model.elapsed_ms,
            "首 token 时延不得超过该阶段总耗时"
        );
    }

    #[tokio::test]
    async fn concurrent_group_results_backfill_in_original_call_order() {
        let mut registry = ToolRegistry::new();
        registry.register(ProbeTool {
            label: "probe_slow",
            delay_ms: 150,
            class: EffectClass::Read,
            host_verified: true,
            state: ProbeState::new(),
        });
        registry.register(ProbeTool {
            label: "probe_fast",
            delay_ms: 10,
            class: EffectClass::Read,
            host_verified: true,
            state: ProbeState::new(),
        });
        let (_, session) = run_with(
            registry,
            two_call_then_text("probe_slow", "probe_fast"),
            &AtomicBool::new(false),
        )
        .await
        .expect("回合应成功");
        // 快工具先完成，但 tool 消息必须仍按原始 tool-call 顺序回填。
        let tool_ids: Vec<&str> = session
            .messages
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect();
        assert_eq!(
            tool_ids,
            vec!["call-probe_slow", "call-probe_fast"],
            "结果必须按原 tool-call 顺序回填"
        );
        let slow_content = session
            .messages
            .iter()
            .find(|message| {
                message.role == "tool" && message.tool_call_id.as_deref() == Some("call-probe_slow")
            })
            .and_then(|message| message.content.as_deref())
            .unwrap_or_default();
        assert!(slow_content.contains("probe_slow"), "慢工具结果不得串位");
    }

    #[test]
    fn write_unverified_and_unknown_tools_are_not_concurrent_eligible() {
        let state = ProbeState::new();
        let mut registry = ToolRegistry::new();
        for (label, class, verified) in [
            ("probe_read_verified", EffectClass::Read, true),
            ("probe_read_self_reported", EffectClass::Read, false),
            ("probe_write", EffectClass::Write, true),
        ] {
            registry.register(ProbeTool {
                label,
                delay_ms: 0,
                class,
                host_verified: verified,
                state: Arc::clone(&state),
            });
        }
        let provider = Arc::new(ScriptedTestProvider {
            outputs: Mutex::new(VecDeque::new()),
        });
        let agent = Agent::new(provider, registry, Policy::new("."), AgentConfig::default());
        let call = |name: &str| crate::gateway::ToolCall {
            id: "x".to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        };
        assert!(agent.call_is_concurrent_eligible(&call("probe_read_verified")));
        // MCP 自报 readOnlyHint 但宿主未验证 → 不得并发（保持串行）。
        assert!(!agent.call_is_concurrent_eligible(&call("probe_read_self_reported")));
        // 写工具绝不可能进并发组（同路径读写不并发由分组排除保证）。
        assert!(!agent.call_is_concurrent_eligible(&call("probe_write")));
        // 未知工具 → 串行错误路径。
        assert!(!agent.call_is_concurrent_eligible(&call("__missing__")));
    }

    #[tokio::test]
    async fn abort_during_concurrent_group_cancels_pending_tools() {
        let state = ProbeState::new();
        let mut registry = ToolRegistry::new();
        for label in ["probe_a", "probe_b"] {
            registry.register(ProbeTool {
                label,
                delay_ms: 400,
                class: EffectClass::Read,
                host_verified: true,
                state: Arc::clone(&state),
            });
        }
        let abort = Arc::new(AtomicBool::new(false));
        {
            let abort = Arc::clone(&abort);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(80));
                abort.store(true, Ordering::SeqCst);
            });
        }
        let result = run_with(registry, two_call_then_text("probe_a", "probe_b"), &abort).await;
        assert!(
            matches!(result, Err(AgentError::Aborted)),
            "组执行中途 abort 应返回 Aborted：{result:?}"
        );
        assert!(
            state.completed.lock().unwrap().is_empty(),
            "取消后不得有任何工具完成（无后台残留）"
        );
    }

    // ---------- §9.3 超大工具结果 artifact 落盘 ----------

    /// §9.3：输出超大结果的探针工具（宿主验证只读）。
    struct BigOutputTool;

    #[async_trait::async_trait]
    impl Tool for BigOutputTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec::with_effect(
                "probe_big_output",
                "超大输出探针".to_string(),
                serde_json::json!({ "type": "object" }),
                Some(crate::tool_effects::ToolEffect {
                    tool: "probe_big_output".to_string(),
                    class: EffectClass::Read,
                    source: "builtin".to_string(),
                    risk_note: None,
                    annotations: None,
                    host_verified_readonly: true,
                }),
            )
        }

        async fn run(
            &self,
            _ctx: &mut ToolContext<'_>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({ "blob": "x".repeat(60_000) }))
        }
    }

    fn big_output_script() -> Mutex<VecDeque<ModelOutput>> {
        Mutex::new(VecDeque::from(vec![
            ModelOutput::ToolCalls(vec![crate::gateway::ToolCall {
                id: "call-big".to_string(),
                name: "probe_big_output".to_string(),
                arguments: serde_json::json!({}),
            }]),
            ModelOutput::Text("完成".to_string()),
        ]))
    }

    fn first_tool_content(session: &Session) -> String {
        session
            .messages
            .iter()
            .find(|message| message.role == "tool")
            .and_then(|message| message.content.clone())
            .expect("应有 tool 消息")
    }

    #[tokio::test]
    async fn oversized_tool_result_stored_as_artifact_pointer() {
        let mut registry = ToolRegistry::new();
        registry.register(BigOutputTool);
        let provider = Arc::new(ScriptedTestProvider {
            outputs: big_output_script(),
        });
        let artifact_dir =
            std::env::temp_dir().join(format!("owo-artifact-test-{}", uuid::Uuid::new_v4()));
        let store =
            Arc::new(crate::cas_store::CasStore::new(artifact_dir).expect("CAS 初始化应成功"));
        let agent = Agent::new(provider, registry, Policy::new("."), AgentConfig::default())
            .with_artifact_store(Arc::clone(&store));
        let mut session = Session::new(std::env::temp_dir(), "test-model", None);
        let approver = crate::permissions::AutoApprover { allow: true };
        let outcome = agent
            .run_turn(
                &mut session,
                "大输出",
                &approver,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .await
            .expect("回合应成功");
        assert_eq!(outcome.final_text.as_deref(), Some("完成"));

        let tool_message = first_tool_content(&session);
        let envelope: serde_json::Value =
            serde_json::from_str(&tool_message).expect("应为 artifact JSON 信封");
        let artifact = &envelope["artifact"];
        let artifact_ref = artifact["ref"].as_str().expect("应有 ref").to_string();
        assert_eq!(artifact["mime"].as_str(), Some("text/plain"));
        assert!(
            artifact["size_bytes"].as_u64().unwrap_or(0) > 50_000,
            "应记录原始大小"
        );
        assert!(
            artifact["truncation_reason"]
                .as_str()
                .unwrap_or_default()
                .contains("artifact"),
            "应记录截断原因"
        );
        assert!(
            tool_message.chars().count() < 55_000,
            "回填内容应为指针+预览，而非全量原文"
        );
        // CAS 往返：按 ref 可取回完整原文。
        let restored = store.get_text(&artifact_ref).expect("CAS 应含完整结果");
        assert!(restored.len() > 50_000, "完整原文应落盘");
        assert!(restored.contains("xxxxxxxxxx"));
    }

    #[tokio::test]
    async fn without_artifact_store_truncation_keeps_legacy_text() {
        let mut registry = ToolRegistry::new();
        registry.register(BigOutputTool);
        let provider = Arc::new(ScriptedTestProvider {
            outputs: big_output_script(),
        });
        let agent = Agent::new(provider, registry, Policy::new("."), AgentConfig::default());
        let mut session = Session::new(std::env::temp_dir(), "test-model", None);
        let approver = crate::permissions::AutoApprover { allow: true };
        agent
            .run_turn(
                &mut session,
                "大输出",
                &approver,
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .await
            .expect("回合应成功");
        let tool_message = first_tool_content(&session);
        assert!(
            tool_message.contains("工具输出已截断"),
            "未挂载 store 时维持盲截断旧行为"
        );
        assert!(
            !tool_message.contains("\"artifact\""),
            "未挂载 store 时不得产生 artifact 信封"
        );
    }
}
