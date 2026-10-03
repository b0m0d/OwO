use super::write_lease::{WriteLease, WriteScope};
use super::{project_workspace, workspace_change_tracker, workswarm_metrics};
use async_trait::async_trait;
use owo_agent_core::agent::TurnEvent;
use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::goal::Worker;
use owo_agent_core::permissions::AutoApprover;
use owo_agent_core::subagent::SubagentRunner;
use owo_agent_core::tool_effects::{EffectClass, ToolEffect};
use owo_agent_core::tools::{Tool, ToolContext, ToolSpec};
use owo_agent_core::worker_profile::{ProfileSubagentRunner, TurnEventSink, WorkerProfile};
use owo_agent_core::workswarm::TeamCoordinator;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

/// 真实 Agent 子代理 worker（name="agent"）：prompt → 子代理执行。
///
/// 五期（第三路）：与 `Agent::run_subagent` 同口径（顶层 depth=0、
/// 子代理 max_turns 上限 12），但 Provider 支持 [`workswarm_metrics::MeasuredProvider`]
/// 计数装饰器注入——`model_calls` 由 `MeasuredRoleWorker` 逐 span 精确统计
/// （每次模型调用恰好经过 `complete`/`complete_stream` 其一）。
/// 七期（第二路）：角色画像驱动——工具注册表按 `WorkerProfile` 装配（注册表面即
/// 权限边界）、回合上限取模板预算、写面为「角色 ∩ 绑定」交集白名单。
pub struct AgentSubagentWorker {
    pub(super) agent: Arc<owo_agent_core::Agent>,
    pub(super) workspace: PathBuf,
    /// 指标层注入的 per-span 模型调用计数（None = 不计数，行为不变）。
    pub(super) model_calls: Option<Arc<AtomicU64>>,
    pub(super) request_usage: Option<Arc<workswarm_metrics::RequestUsageCollector>>,
    /// 六期（第二路）：项目工作区绑定作用域（None = 全局工作区，行为不变）。
    /// 绑定后：运行目录 = 绑定根；只读绑定强制 read_only；写白名单经审批器强制。
    pub(super) workspace_scope: Option<project_workspace::WorkspaceScope>,
    /// 七期（第二路）：角色画像（工具面/只读/回合上限；None = 防御分支退回通用执行器）。
    pub(super) profile: Option<WorkerProfile>,
    /// 七期（第二路）：critic 角色代理（服务端口径：`role == "critic"` 字面量；
    /// 引擎注入的 read_only 只覆盖 critic，其余内置角色都是 producer，画像另管只读面）。
    pub(super) is_critic: bool,
    /// 七期（第二路）：团队取消桥共享标志（None = 本地标志，行为退化为不可中断）。
    pub(super) cancel_flag: Option<Arc<AtomicBool>>,
    /// 七期（第二路）：最终写白名单（角色 ∩ 绑定交集；空 = 工作区内可写）。
    pub(super) write_allowed: Vec<PathBuf>,
    pub(super) coordinator: Arc<TeamCoordinator>,
    pub(super) session_store: Arc<dyn owo_agent_core::SessionStore>,
    pub(super) parent_session_id: Option<String>,
    pub(super) team_id: String,
    pub(super) role: String,
}

#[async_trait]
impl Worker for AgentSubagentWorker {
    fn name(&self) -> &str {
        "agent"
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "agent 步骤缺少 prompt 参数".to_string())?;
        let input_read_only = input
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        // 只读三层叠加（七期 · 二路）：步骤输入 → 团队绑定只读（团队级上限）→
        // 角色画像只读（角色级上限）。任一只读即只读。
        let read_only = input_read_only
            || self
                .workspace_scope
                .as_ref()
                .map(|s| s.read_only)
                .unwrap_or(false)
            || self.profile.as_ref().map(|p| p.read_only).unwrap_or(false);
        let task_write_allowed =
            assigned_task_write_allowlist(input, &self.workspace, &self.write_allowed)?;
        let task_has_no_write_path = task_write_allowed.as_ref().is_some_and(Vec::is_empty);
        let task_has_no_write_capability = task_lacks_file_write_capability(input);
        let task_has_no_write_scope = task_has_no_write_path || task_has_no_write_capability;
        let read_only = read_only || task_has_no_write_scope;
        let effective_write_allowed = task_write_allowed
            .clone()
            .unwrap_or_else(|| self.write_allowed.clone());
        if std::env::var("OPENAI_API_KEY")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(
                "缺少 OPENAI_API_KEY，agent 角色无法调用模型（请配置凭据后重试）".to_string(),
            );
        }
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                std::env::var("OWO_AGENT_MODEL")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "gpt-4.1-mini".to_string());
        // 指标计数注入：MeasuredProvider 包装共享 provider（计数仅对本 span 生效）。
        let provider: Arc<dyn ModelProvider> = match (&self.model_calls, &self.request_usage) {
            (Some(counter), Some(request_usage)) => {
                Arc::new(workswarm_metrics::MeasuredProvider::new_with_request_usage(
                    self.agent.provider(),
                    Arc::clone(counter),
                    Arc::clone(request_usage),
                    workswarm_metrics::request_scope_key(
                        input
                            .get("_workswarm")
                            .and_then(|meta| meta.get("step_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                        input
                            .get("_workswarm")
                            .and_then(|meta| meta.get("phase_epoch"))
                            .and_then(Value::as_u64),
                    ),
                ))
            }
            (Some(counter), None) => Arc::new(workswarm_metrics::MeasuredProvider::new(
                self.agent.provider(),
                Arc::clone(counter),
            )),
            _ => self.agent.provider(),
        };
        // 审批器（七期 · 二路）：绑定作用域 → 白名单审批器，白名单取「角色 ∩ 绑定」
        // 交集（角色白名单空 = 绑定原样）；未绑定保持 AutoApprover 原行为。
        let scope = self.workspace_scope.as_ref();
        let allow_writes = !read_only && !task_has_no_write_scope;
        let workspace_approver;
        let fallback_approver = AutoApprover { allow: read_only };
        let approver: &dyn owo_agent_core::permissions::Approver = match scope {
            Some(s) => {
                let allowed = if let Some(task_allowed) = &task_write_allowed {
                    task_allowed.clone()
                } else if self.write_allowed.is_empty() {
                    s.allowed.clone()
                } else {
                    self.write_allowed.clone()
                };
                workspace_approver = project_workspace::WorkspaceScopeApprover {
                    allow_writes,
                    root: s.root.clone(),
                    allowed,
                };
                &workspace_approver
            }
            None => &fallback_approver,
        };
        // 中断标志（七期 · 二路）：优先团队取消桥共享标志（cancel → 即时置位，
        // run_turn 协作式中断）；None = 本地标志（行为退化为不可中断）。
        let local_abort = AtomicBool::new(false);
        let abort: &AtomicBool = match &self.cancel_flag {
            Some(flag) => flag,
            None => &local_abort,
        };
        // 七期（第二路）：画像执行器——注册表面即权限边界 + 模板预算回合上限；
        // 无画像（防御分支，现网构建路径恒有画像）退回通用执行器保持旧行为。
        let output = match &self.profile {
            Some(profile) => {
                let mut task_profile = profile.clone();
                let is_task_graph_work = input.get("assigned_task_id").is_some();
                if is_task_graph_work {
                    apply_task_capability_scope(&mut task_profile, input, task_has_no_write_scope);
                }
                if task_write_allowed.is_some() {
                    let command_was_assigned = input
                        .get("required_capabilities")
                        .and_then(Value::as_array)
                        .is_some_and(|capabilities| {
                            capabilities.iter().any(|item| item.as_str() == Some("run_command"))
                        });
                    task_profile.can_run_command &= command_was_assigned;
                    if !command_was_assigned {
                        task_profile.visible_tools.retain(|tool| tool != "run_command");
                    }
                    task_profile.write_allowed_paths = effective_write_allowed
                        .iter().map(|path| path.to_string_lossy().to_string()).collect();
                    if task_has_no_write_scope {
                        task_profile.read_only = true;
                    }
                }
                let workswarm = input.get("_workswarm");
                let raw_step_id = workswarm.and_then(|meta| meta.get("step_id"))
                    .and_then(Value::as_str).unwrap_or("unknown").to_string();
                let assigned_task_id = input.get("assigned_task_id")
                    .and_then(Value::as_str).map(str::to_string);
                let refs = ["assigned_read_refs", "assigned_contract_refs"].iter()
                    .filter_map(|key| input.get(*key).and_then(Value::as_array))
                    .flatten().filter_map(Value::as_str).map(str::to_string).collect();
                let context_tool: Arc<dyn Tool> = Arc::new(TeamContextReadTool {
                    coordinator: Arc::clone(&self.coordinator),
                    team_id: self.team_id.clone(),
                    member_id: format!("m-{}", self.role),
                    step_id: raw_step_id.clone(),
                    task_id: assigned_task_id.clone(),
                    refs,
                    workspace_root: self.workspace.clone(),
                });
                let artifact_tool: Arc<dyn Tool> = Arc::new(TeamArtifactReadTool {
                    coordinator: Arc::clone(&self.coordinator),
                    team_id: self.team_id.clone(),
                    member_id: format!("m-{}", self.role),
                    step_id: raw_step_id.clone(),
                });
                let mut extra_tools: Vec<Arc<dyn Tool>> = vec![context_tool, artifact_tool];
                let can_publish_context = input
                    .get("required_capabilities")
                    .and_then(Value::as_array)
                    .map(|capabilities| {
                        capabilities.iter().any(|capability| {
                            capability.as_str() == Some("team_context_publish")
                        })
                    })
                    .unwrap_or(true);
                if !task_profile.read_only && allow_writes && can_publish_context {
                    extra_tools.push(Arc::new(TeamContextPublishTool {
                        coordinator: Arc::clone(&self.coordinator),
                        team_id: self.team_id.clone(),
                        member_id: format!("m-{}", self.role),
                        task_id: assigned_task_id.clone().unwrap_or_else(|| raw_step_id.clone()),
                    }));
                }
                let tool_started_at = Arc::new(Mutex::new(HashMap::<String, Instant>::new()));
                let coordinator = Arc::clone(&self.coordinator);
                let team_id = self.team_id.clone();
                let role = self.role.clone();
                let event_step_id = raw_step_id.clone();
                let event_task_id = assigned_task_id.clone().unwrap_or_else(|| raw_step_id.clone());
                let event_attempt_id = workswarm
                    .and_then(|meta| meta.get("attempt_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let event_sink: TurnEventSink = Arc::new(move |event| {
                    if let TurnEvent::ToolResult {
                        command_receipt: Some(receipt),
                        ..
                    } = event
                    {
                        let detail = serde_json::json!({
                            "step_id": &event_step_id,
                            "task_id": &event_task_id,
                            "attempt_id": &event_attempt_id,
                            "receipt": receipt,
                        })
                        .to_string();
                        coordinator.record_runtime_event(
                            &team_id,
                            "team.command.executed",
                            detail,
                        );
                    }
                    if let Some(detail) = safe_team_model_event(event, &role, &event_step_id) {
                        coordinator.record_runtime_event(&team_id, "team.model.started", detail);
                        return;
                    }
                    let duration_ms = match event {
                        TurnEvent::ToolStart { id, .. } => {
                            if let Ok(mut started) = tool_started_at.lock() {
                                started.insert(id.clone(), Instant::now());
                            }
                            None
                        }
                        TurnEvent::ToolResult { id, .. } => tool_started_at.lock().ok()
                            .and_then(|mut started| started.remove(id))
                            .map(|started| started.elapsed().as_millis() as u64),
                        _ => return,
                    };
                    if let Some((event_name, detail)) =
                        safe_team_tool_event(event, &role, &event_step_id, duration_ms)
                    {
                        coordinator.record_runtime_event(&team_id, event_name, detail);
                    }
                });
                let runner = ProfileSubagentRunner {
                    provider, approver, abort, depth: 0, model,
                    is_critic: self.is_critic,
                    write_allowed: effective_write_allowed,
                    profile: task_profile,
                    agent_config: None,
                    budget_note_override: None,
                    extra_tools,
                    extra_system_prompt: Some(
                         "如需获取执行期间新增的团队事实，请调用 team_context_read。需要上游产物全文时使用 team_artifact_read，它仅允许读取直接依赖产物。具备当前任务写权限时可使用 team_context_publish：先读取 revision，再以 expected_revision 发布；冲突后重读。发布结果始终是 candidate/unverified\n".to_string(),
                    ),
                    event_sink: Some(event_sink),
                    session_store: Some(Arc::clone(&self.session_store)),
                    worker_session_id: Some(worker_session_id(&self.team_id, assigned_task_id.as_deref().unwrap_or(&raw_step_id))),
                    parent_session_id: self.parent_session_id.clone(),
                };
                runner.run(&self.workspace, prompt).await
            }
            None => {
                let runner = SubagentRunner {
                    provider,
                    approver,
                    abort,
                    depth: 0,
                    max_turns: 12,
                    model,
                    events: None,
                };
                runner.run(&self.workspace, prompt, read_only).await
            }
        }
        .map_err(|e| format!("agent 子代理执行失败：{e}"))?;
        Ok(output)
    }
}

fn worker_session_id(team_id: &str, task_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{team_id}\0{task_id}").as_bytes());
    let suffix = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("worker-{suffix}")
}

fn safe_team_model_event(event: &TurnEvent, role: &str, step_id: &str) -> Option<String> {
    if !matches!(event, TurnEvent::ModelCall) {
        return None;
    }
    let safe_role = role.chars().take(80).collect::<String>();
    let safe_step = step_id.chars().take(100).collect::<String>();
    Some(format!("role={safe_role} step_id={safe_step}"))
}

fn safe_team_tool_event(
    event: &TurnEvent,
    role: &str,
    step_id: &str,
    duration_ms: Option<u64>,
) -> Option<(&'static str, String)> {
    let (event_name, tool, outcome) = match event {
        TurnEvent::ToolStart { tool, .. } => ("team.tool.started", tool.as_str(), None),
        TurnEvent::ToolResult { tool, ok, .. } => (
            "team.tool.finished",
            tool.as_str(),
            Some(if *ok { "succeeded" } else { "failed" }),
        ),
        _ => return None,
    };
    let safe_tool = tool.chars().take(80).collect::<String>();
    let safe_role = role.chars().take(80).collect::<String>();
    let safe_step = step_id.chars().take(100).collect::<String>();
    let detail = match (outcome, duration_ms) {
        (Some(outcome), Some(ms)) => format!("role={safe_role} step_id={safe_step} tool={safe_tool} outcome={outcome} duration_ms={ms}"),
        (Some(outcome), None) => format!("role={safe_role} step_id={safe_step} tool={safe_tool} outcome={outcome}"),
        (None, _) => format!("role={safe_role} step_id={safe_step} tool={safe_tool}"),
    };
    Some((event_name, detail))
}

fn team_context_fact_is_visible(
    fact: &owo_agent_protocol::SharedContextFact,
    task_id: Option<&str>,
    step_id: &str,
    refs: &[String],
) -> bool {
    fact.task_id.is_none()
        || fact.task_id.as_deref() == task_id
        || fact.task_id.as_deref() == Some(step_id)
        || fact.source_refs.iter().any(|source| refs.contains(source))
}

fn team_context_fact_freshness(
    fact: &owo_agent_protocol::SharedContextFact,
    workspace_root: &std::path::Path,
) -> &'static str {
    let Some(expected) = fact.file_hash.as_deref() else {
        return "untracked";
    };
    if fact.source_refs.len() != 1 {
        return "unverifiable";
    }
    let relative = std::path::Path::new(&fact.source_refs[0]);
    if relative.is_absolute()
        || !relative
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(_)))
        || relative.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return "stale";
    }
    let root = workspace_change_tracker::simplify_path(
        &workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf()),
    );
    let source = canonicalize_task_path(&root.join(relative));
    if !source.starts_with(&root) {
        return "stale";
    }
    let Ok(metadata) = std::fs::metadata(&source) else {
        return "stale";
    };
    if !metadata.is_file() {
        return "stale";
    }
    let Ok(bytes) = std::fs::read(&source) else {
        return "stale";
    };
    let actual = format!("sha256:{}", owo_agent_core::CasStore::hash_of(&bytes));
    if actual.eq_ignore_ascii_case(expected) {
        "current"
    } else {
        "stale"
    }
}

fn take_utf8_bytes(value: &str, max_bytes: usize) -> String {
    value
        .char_indices()
        .take_while(|(offset, ch)| offset + ch.len_utf8() <= max_bytes)
        .map(|(_, ch)| ch)
        .collect()
}

fn team_context_read_effect() -> ToolEffect {
    ToolEffect {
        tool: "team_context_read".to_string(),
        class: EffectClass::Read,
        source: "builtin".to_string(),
        risk_note: None,
        annotations: None,
        host_verified_readonly: true,
    }
}

fn team_context_publish_effect() -> ToolEffect {
    ToolEffect {
        tool: "team_context_publish".into(),
        class: EffectClass::Write,
        source: "builtin".into(),
        risk_note: Some("发布团队共享事实，使用 revision CAS；保持 candidate/unverified".into()),
        annotations: None,
        host_verified_readonly: false,
    }
}
struct TeamContextPublishTool {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    member_id: String,
    task_id: String,
}
#[async_trait]
impl Tool for TeamContextPublishTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            "team_context_publish",
            "使用 expected_revision/CAS 发布当前任务的候选事实，不会提升可信等级。".into(),
            json!({"type":"object","properties":{
                "key":{"type":"string","minLength":1,"maxLength":160},
                "value":{"type":"string","minLength":1,"maxLength":65536},
                "expected_revision":{"type":"integer","minimum":0},
                "source_refs":{"type":"array","items":{"type":"string"},"maxItems":8},
                "file_hash":{"type":"string","pattern":"^sha256:[0-9a-fA-F]{64}$"}},
                "required":["key","value","expected_revision"],"additionalProperties":false}),
            Some(team_context_publish_effect()),
        )
    }
    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| "key 必须是字符串".to_string())?
            .to_string();
        let value = args
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| "value 必须是字符串".to_string())?
            .to_string();
        let expected_revision = args
            .get("expected_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| "expected_revision 必须是非负整数".to_string())?;
        let source_refs = args
            .get("source_refs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "source_refs 只能包含字符串".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let file_hash = args
            .get("file_hash")
            .and_then(Value::as_str)
            .map(str::to_string);
        let fact = self
            .coordinator
            .publish_team_context_fact(
                &self.team_id,
                expected_revision,
                owo_agent_core::workswarm::SharedContextFactDraft {
                    key,
                    value,
                    producer: self.member_id.clone(),
                    task_id: Some(self.task_id.clone()),
                    source_refs,
                    file_hash,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"key":fact.key,"revision":fact.revision,"producer":fact.producer,
            "task_id":fact.task_id,"source_refs":fact.source_refs,"file_hash":fact.file_hash,
            "confidence":fact.confidence,"status":fact.status}),
        )
    }
}
fn team_artifact_read_effect() -> ToolEffect {
    ToolEffect {
        tool: "team_artifact_read".into(),
        class: EffectClass::Read,
        source: "builtin".into(),
        risk_note: None,
        annotations: None,
        host_verified_readonly: true,
    }
}
struct TeamArtifactReadTool {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    member_id: String,
    step_id: String,
}
#[async_trait]
impl Tool for TeamArtifactReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            "team_artifact_read",
            "按 artifact_id 读取当前步骤直接依赖的产物正文，输出有字节上限。".into(),
            json!({"type":"object","properties":{
                "artifact_id":{"type":"string","minLength":1,"maxLength":256},
                "max_bytes":{"type":"integer","minimum":1,"maximum":65536}},
                "required":["artifact_id"],"additionalProperties":false}),
            Some(team_artifact_read_effect()),
        )
    }
    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let id = args
            .get("artifact_id")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "artifact_id 必须是非空字符串".to_string())?;
        let max = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(16 * 1024)
            .clamp(1, 64 * 1024) as usize;
        self.coordinator
            .read_dependency_artifact(&self.team_id, &self.member_id, &self.step_id, id, max)
            .await
            .map_err(|e| e.to_string())
    }
}
struct TeamContextReadTool {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    member_id: String,
    step_id: String,
    task_id: Option<String>,
    refs: Vec<String>,
    workspace_root: PathBuf,
}

#[async_trait]
impl Tool for TeamContextReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            "team_context_read",
            "按需读取当前任务相关的版本化团队事实正文；返回 revision 和来源引用。".to_string(),
            json!({"type":"object","properties":{
                "key":{"type":"string"},
                "limit":{"type":"integer","minimum":1,"maximum":32}
            },"additionalProperties":false}),
            Some(team_context_read_effect()),
        )
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let key_filter = args.get("key").and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(12)
            .clamp(1, 32) as usize;
        let snapshot = self
            .coordinator
            .read_team_context(&self.team_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut latest_keys = HashSet::new();
        let mut facts = Vec::new();
        let mut remaining_bytes = 16 * 1024usize;
        let mut context_revision = snapshot.revision;
        for fact in snapshot.facts.iter().rev() {
            if !latest_keys.insert(fact.key.as_str())
                || (fact.status != "candidate" && fact.status != "confirmed")
                || key_filter.is_some_and(|key| key != fact.key)
            {
                continue;
            }
            if !team_context_fact_is_visible(
                fact,
                self.task_id.as_deref(),
                &self.step_id,
                &self.refs,
            ) {
                continue;
            }
            let freshness = team_context_fact_freshness(fact, &self.workspace_root);
            if freshness == "stale" {
                if let Ok(stale) = self
                    .coordinator
                    .mark_team_context_fact_stale(
                        &self.team_id,
                        &fact.key,
                        fact.revision,
                        context_revision,
                    )
                    .await
                {
                    context_revision = stale.revision;
                } else if let Ok(current) = self.coordinator.read_team_context(&self.team_id).await
                {
                    context_revision = current.revision;
                }
                continue;
            }
            if fact.file_hash.is_some() && freshness != "current" {
                continue;
            }
            if facts.len() >= limit || remaining_bytes == 0 {
                break;
            }
            let hash = fact
                .value_ref
                .strip_prefix("cas://sha256:")
                .ok_or_else(|| "共享事实 CAS 引用格式无效".to_string())?;
            let full = self
                .coordinator
                .cas()
                .get_text(hash)
                .ok_or_else(|| format!("共享事实正文缺失：{}", fact.key))?;
            let value = take_utf8_bytes(&full, remaining_bytes);
            remaining_bytes = remaining_bytes.saturating_sub(value.len());
            facts.push(json!({
                "key":fact.key,"value":value,"revision":fact.revision,"producer":fact.producer,
                "task_id":fact.task_id,"source_refs":fact.source_refs,"file_hash":fact.file_hash,
                "confidence":fact.confidence,"status":fact.status,"freshness":freshness
            }));
        }
        Ok(json!({"team_id":self.team_id,"member_id":self.member_id,
            "step_id":self.step_id,"revision":context_revision,"facts":facts}))
    }
}

fn canonicalize_task_path(path: &std::path::Path) -> PathBuf {
    let mut current = path.to_path_buf();
    let mut suffix = Vec::new();
    while !current.exists() {
        let Some(name) = current.file_name().map(std::ffi::OsString::from) else {
            break;
        };
        suffix.push(name);
        if !current.pop() {
            break;
        }
    }
    let mut canonical = current.canonicalize().unwrap_or(current);
    for part in suffix.iter().rev() {
        canonical.push(part);
    }
    workspace_change_tracker::simplify_path(&canonical)
}

fn task_lacks_file_write_capability(input: &Value) -> bool {
    if input.get("assigned_task_id").is_none() {
        return false;
    }
    let Some(capabilities) = input.get("required_capabilities").and_then(Value::as_array) else {
        return true;
    };
    !capabilities
        .iter()
        .filter_map(Value::as_str)
        .any(|capability| matches!(capability, "write_file" | "apply_patch"))
}

/// Narrow a role profile to the exact capabilities approved on this TaskGraph task.
fn apply_task_capability_scope(
    profile: &mut WorkerProfile,
    input: &Value,
    task_has_no_write_scope: bool,
) {
    let Some(capabilities) = input.get("required_capabilities").and_then(Value::as_array) else {
        profile.read_only = true;
        profile.can_run_command = false;
        profile
            .visible_tools
            .retain(|tool| matches!(tool.as_str(), "read_file" | "list_dir" | "search_files"));
        if profile.visible_tools.is_empty() {
            profile
                .visible_tools
                .push("__owo_no_task_tool__".to_string());
        }
        return;
    };
    let required = capabilities
        .iter()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    let can_write_files = !task_has_no_write_scope
        && (required.contains("write_file") || required.contains("apply_patch"));
    profile
        .visible_tools
        .retain(|tool| required.contains(tool.as_str()));
    profile.can_run_command =
        profile.can_run_command && can_write_files && required.contains("run_command");
    if !can_write_files {
        profile.read_only = true;
        profile
            .visible_tools
            .retain(|tool| !matches!(tool.as_str(), "write_file" | "apply_patch"));
    }
    // WorkerProfile uses an empty list to mean no role-level filter. Keep an
    // impossible registered name so an empty TaskGraph capability set means no
    // workspace tools, rather than restoring the role's broader tool surface.
    if profile.visible_tools.is_empty() {
        profile
            .visible_tools
            .push("__owo_no_task_tool__".to_string());
    }
}

/// Resolve a task-local allowlist and intersect it with the role/workspace allowlist.
fn assigned_task_write_allowlist(
    input: &Value,
    workspace_root: &std::path::Path,
    role_allowed: &[PathBuf],
) -> Result<Option<Vec<PathBuf>>, String> {
    let Some(value) = input.get("assigned_write_paths") else {
        return Ok(None);
    };
    // canonicalize() on Windows returns a verbatim path (\\?\...). Normalize
    // the workspace root exactly like the candidate before prefix checks.
    let workspace_root = workspace_change_tracker::simplify_path(
        &workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf()),
    );
    let paths = value
        .as_array()
        .ok_or_else(|| "任务 assigned_write_paths 必须是数组".to_string())?;
    let mut resolved = Vec::with_capacity(paths.len());
    for value in paths {
        let raw = value
            .as_str()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "任务写范围含空路径或非字符串".to_string())?;
        let relative = std::path::Path::new(raw);
        if relative.is_absolute()
            || !relative
                .components()
                .any(|part| matches!(part, std::path::Component::Normal(_)))
            || relative.components().any(|part| {
                !matches!(
                    part,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return Err(format!("任务写范围路径无效：{raw}"));
        }
        let absolute = workspace_root.join(relative);
        let canonical = canonicalize_task_path(&absolute);
        if !canonical.starts_with(&workspace_root) {
            return Err(format!("任务写范围越出工作区：{raw}"));
        }
        if !role_allowed.is_empty()
            && !role_allowed
                .iter()
                .any(|base| canonical.starts_with(workspace_change_tracker::simplify_path(base)))
        {
            return Err(format!("任务写范围超出角色/团队范围：{raw}"));
        }
        resolved.push(canonical);
    }
    resolved.sort();
    resolved.dedup();
    Ok(Some(resolved))
}

#[cfg(test)]
mod team_context_scope_tests {
    use super::*;

    #[test]
    fn worker_sessions_are_stable_per_team_and_task() {
        let first = worker_session_id("team-a", "task-a");
        assert_eq!(first, worker_session_id("team-a", "task-a"));
        assert_ne!(first, worker_session_id("team-a", "task-b"));
        assert_ne!(first, worker_session_id("team-b", "task-a"));
        assert!(first.starts_with("worker-"));
        assert_eq!(first.len(), 71);
    }

    #[test]
    fn task_capabilities_narrow_role_tools_and_do_not_grant_write() {
        let mut profile = WorkerProfile::explicit_writer(4);
        apply_task_capability_scope(
            &mut profile,
            &json!({
                "required_capabilities": ["read_file"],
                "assigned_write_paths": ["apps/api"]
            }),
            false,
        );
        assert!(profile.read_only);
        assert!(!profile.can_run_command);
        assert_eq!(profile.visible_tools, vec!["read_file".to_string()]);
        let read_tools = profile
            .build_registry(Vec::new())
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(read_tools, vec!["read_file".to_string()]);

        let mut writer = WorkerProfile::explicit_writer(4);
        apply_task_capability_scope(
            &mut writer,
            &json!({ "required_capabilities": ["write_file"] }),
            false,
        );
        assert!(!writer.read_only);
        assert_eq!(writer.visible_tools, vec!["write_file".to_string()]);
        let write_tools = writer
            .build_registry(Vec::new())
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(write_tools, vec!["write_file".to_string()]);

        let mut scoped_out = WorkerProfile::explicit_writer(4);
        apply_task_capability_scope(
            &mut scoped_out,
            &json!({ "required_capabilities": ["write_file"] }),
            true,
        );
        assert!(scoped_out.read_only);
        assert!(!scoped_out
            .visible_tools
            .iter()
            .any(|tool| tool == "write_file"));

        let mut empty = WorkerProfile::explicit_writer(4);
        apply_task_capability_scope(&mut empty, &json!({ "required_capabilities": [] }), false);
        assert!(empty.read_only);
        assert_eq!(
            empty.visible_tools,
            vec!["__owo_no_task_tool__".to_string()]
        );
        assert!(empty.build_registry(Vec::new()).specs().is_empty());

        assert!(task_lacks_file_write_capability(&json!({
            "assigned_task_id": "read-only",
            "required_capabilities": ["read_file"]
        })));
        assert!(!task_lacks_file_write_capability(&json!({
            "assigned_task_id": "writer",
            "required_capabilities": ["write_file"]
        })));
        assert!(!task_lacks_file_write_capability(&json!({
            "required_capabilities": []
        })));
        assert!(task_lacks_file_write_capability(&json!({
            "assigned_task_id": "missing-capabilities"
        })));
    }

    #[test]
    fn context_tools_have_read_and_explicit_write_effects() {
        let read = team_context_read_effect();
        assert_eq!(read.class, EffectClass::Read);
        assert!(read.host_verified_readonly);
        let publish = team_context_publish_effect();
        assert_eq!(publish.class, EffectClass::Write);
        assert!(!publish.host_verified_readonly);
        let artifact = team_artifact_read_effect();
        assert_eq!(artifact.class, EffectClass::Read);
        assert!(artifact.host_verified_readonly);
    }

    #[test]
    fn context_facts_are_limited_to_global_task_step_or_refs() {
        let fact = |task_id: Option<&str>, refs: &[&str]| owo_agent_protocol::SharedContextFact {
            key: "k".into(),
            value_ref: "cas://sha256:x".into(),
            revision: 1,
            producer: "m-worker".into(),
            task_id: task_id.map(str::to_string),
            source_refs: refs.iter().map(|s| (*s).into()).collect(),
            file_hash: None,
            confidence: "unverified".into(),
            status: "candidate".into(),
            created_at: "now".into(),
        };
        let refs = vec!["src/api.rs".to_string()];
        assert!(team_context_fact_is_visible(
            &fact(None, &[]),
            Some("task"),
            "step",
            &refs
        ));
        assert!(team_context_fact_is_visible(
            &fact(Some("task"), &[]),
            Some("task"),
            "step",
            &refs
        ));
        assert!(team_context_fact_is_visible(
            &fact(Some("step"), &[]),
            Some("task"),
            "step",
            &refs
        ));
        assert!(team_context_fact_is_visible(
            &fact(Some("other"), &["src/api.rs"]),
            Some("task"),
            "step",
            &refs
        ));
        assert!(!team_context_fact_is_visible(
            &fact(Some("other"), &["docs/readme.md"]),
            Some("task"),
            "step",
            &refs
        ));
    }

    #[test]
    fn context_fact_freshness_detects_changes_and_rejects_escaping_refs() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let source = root.path().join("src/api.rs");
        std::fs::write(&source, b"pub fn api() {}").unwrap();
        let digest = owo_agent_core::CasStore::hash_of(b"pub fn api() {}");
        let fact =
            |refs: Vec<String>, hash: Option<String>| owo_agent_protocol::SharedContextFact {
                key: "api".into(),
                value_ref: "cas://sha256:aa".into(),
                revision: 1,
                producer: "m-w".into(),
                task_id: None,
                source_refs: refs,
                file_hash: hash,
                confidence: "unverified".into(),
                status: "candidate".into(),
                created_at: "now".into(),
            };
        let current = fact(vec!["src/api.rs".into()], Some(format!("sha256:{digest}")));
        assert_eq!(
            team_context_fact_freshness(&current, root.path()),
            "current"
        );
        std::fs::write(&source, b"changed").unwrap();
        assert_eq!(team_context_fact_freshness(&current, root.path()), "stale");
        let escape = fact(
            vec!["../outside.rs".into()],
            Some(format!("sha256:{digest}")),
        );
        assert_eq!(team_context_fact_freshness(&escape, root.path()), "stale");
        let ambiguous = fact(
            vec!["src/api.rs".into(), "api".into()],
            Some(format!("sha256:{digest}")),
        );
        assert_eq!(
            team_context_fact_freshness(&ambiguous, root.path()),
            "unverifiable"
        );
    }

    #[test]
    fn safe_model_events_record_only_bounded_identifiers() {
        let event = TurnEvent::ModelCall;
        let detail = safe_team_model_event(&event, &"r".repeat(100), &"s".repeat(120)).unwrap();
        assert!(detail.starts_with(&format!("role={} step_id=", "r".repeat(80))));
        assert!(detail.chars().count() <= 5 + 80 + 9 + 100);
        assert!(safe_team_model_event(
            &TurnEvent::Final {
                text: "private".into()
            },
            "worker",
            "step"
        )
        .is_none());
        assert!(!detail.contains("private"));
    }

    #[test]
    fn safe_tool_events_exclude_arguments_previews_and_error_text() {
        let start = TurnEvent::ToolStart {
            id: "call-1".into(),
            tool: "read_file".into(),
            args_preview: Some("secret/path".into()),
        };
        let (_, detail) = safe_team_tool_event(&start, "worker", "step-1", None).unwrap();
        assert!(detail.contains("tool=read_file"));
        assert!(!detail.contains("secret/path"));
        let finish = TurnEvent::ToolResult {
            id: "call-1".into(),
            tool: "write_file".into(),
            ok: false,
            error: Some("private contents".into()),
            preview: Some("payload".into()),
            command_receipt: None,
        };
        let (_, detail) = safe_team_tool_event(&finish, "worker", "step-1", Some(12)).unwrap();
        assert!(detail.contains("outcome=failed"));
        assert!(detail.contains("duration_ms=12"));
        assert!(!detail.contains("private contents"));
        assert!(!detail.contains("payload"));
    }
}

pub(super) struct EchoWorker;

#[async_trait]
impl Worker for EchoWorker {
    fn name(&self) -> &str {
        "echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string()))
    }
}

pub(super) struct SleepWorker;

#[async_trait]
impl Worker for SleepWorker {
    fn name(&self) -> &str {
        "sleep"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let ms = input
            .get("ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(600_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("slept {ms}ms"))
    }
}

pub(super) struct FailWorker;

#[async_trait]
impl Worker for FailWorker {
    fn name(&self) -> &str {
        "fail"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Err(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "fail worker 注入失败".to_string()))
    }
}

/// 追踪型 worker 包装（七期 · 二路）：插在内层 worker 与 `RoleWorker` 之间——
/// 1) 写角色先取单写租约（同一工作区同时只允许一个写角色在执行；读角色不参与）；
/// 2) 拿到租约后采集前快照（等待租约的时间不计入变更窗口——否则会把其他写角色
///    正在进行的变更记到本步骤头上）；
/// 3) 执行后采集后快照，登记「变更文件 + diff 摘要 + diff ref」，并对窗口内新增
///    变更做写白名单校验——越界转 `scope_violation` 步骤失败（引擎按失败处理，
///    不登记成功 Artifact）。
///
/// 仅写角色包装追踪（`tracking`/`lease` 均为 None 时纯透传）：读角色没有写工具
/// 不会改文件，而并行读角色的快照窗口会误捕写角色的变更。
///
/// 八期（二路）：执行前对允许路径做内容基线快照（进 CAS）；成功路径自动生成
/// `ChangeSet`（pending_review，等人工 accept/reject/revert；未接受时该团队代码
/// Artifact 不得成为最终 approved head——门控函数见 `change_set_store`）。
pub(super) struct TrackedRoleWorker {
    pub(super) inner: Arc<dyn Worker>,
    /// 范围写租约（None = 只读角色，不参与租约）。
    pub(super) lease: Option<WriteLease>,
    pub(super) lease_waits: Option<Arc<workswarm_metrics::LeaseWaitTracker>>,
    /// 变更追踪配置（None = 不追踪，纯透传）。
    pub(super) tracking: Option<workspace_change_tracker::Tracker>,
}

#[async_trait]
impl Worker for TrackedRoleWorker {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let Some(mut tracking) = self.tracking.clone() else {
            return self.inner.run(input).await;
        };
        let task_graph = input.get("assigned_task_id").is_some();
        if task_graph {
            let task_paths =
                assigned_task_write_allowlist(input, &tracking.root, &tracking.allowed)?
                    .ok_or_else(|| "TaskGraph 任务缺少 assigned_write_paths".to_string())?;
            if task_lacks_file_write_capability(input) || task_paths.is_empty() {
                return self.inner.run(input).await;
            }
            tracking.allowed = task_paths;
        }
        // TaskGraph 的租约与追踪均缩到当前任务写范围；同槽位的互不重叠任务
        // 可以并行，冲突范围仍由共享工作区仲裁器串行化。
        // 十期·四路 R1：租约覆盖「前快照 → 执行 → 后快照 → 变更登记」全程——
        // 变更登记（record + ChangeSet upsert）完成前不释放：并发写步骤若在
        // 本步骤后快照前插窗口，会把本步骤的变更误算进它的前基线，反之亦然。
        let workswarm = input.get("_workswarm");
        let step_id = workswarm
            .and_then(|meta| meta.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let scope_key = workswarm_metrics::request_scope_key(
            step_id,
            workswarm
                .and_then(|meta| meta.get("phase_epoch"))
                .and_then(Value::as_u64),
        );
        let task_lease = if task_graph {
            self.lease
                .as_ref()
                .map(|lease| lease.for_paths(&tracking.allowed))
                .transpose()?
        } else {
            None
        };
        let lease = task_lease.as_ref().or(self.lease.as_ref());
        let lease_wait_started = Instant::now();
        let _lease = match lease {
            Some(lease) => Some(lease.acquire().await),
            None => None,
        };
        if let Some(waits) = &self.lease_waits {
            waits.record(&scope_key, lease_wait_started.elapsed().as_millis() as u64);
        }
        let pre = workspace_change_tracker::GitSnapshot::snapshot(&tracking.root).await;
        // 八期（二路）：执行前对允许路径做内容基线快照（进 CAS）——ChangeSet
        // reject/revert 的恢复依据；快照尽力而为（超限/读取失败的文件基线记为
        // 「未知」，之后被改只能走 conflicted 人工路径，绝不误删）。
        let base = owo_agent_core::change_set::snapshot_allowed_paths(
            &tracking.root,
            &tracking.allowed,
            &tracking.cas,
        )
        .await;
        // 九期（一路）：执行前已脏文件的内容哈希——合并变更检测的第二基线。
        // 允许路径内的文件直接复用 base.entries（同一份数据，避免重复读盘）；
        // 之外（或基线未覆盖）的执行前脏文件现场读哈希。
        let mut pre_dirty_hashes: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for path in pre.dirty_paths() {
            if let Some(hash) = base.entries.get(&path) {
                pre_dirty_hashes.insert(path, Some(hash.clone()));
            } else {
                let hash = workspace_change_tracker::content_hash(&tracking.root, &path);
                pre_dirty_hashes.insert(path, hash);
            }
        }
        // 执行（成败、超时、取消都以 result 承接——变更收尾对四种结局一视同仁）。
        let result = self.inner.run(input).await;
        // ---- 变更收尾：成功/失败/超时/取消都必须完成 ----
        let post = workspace_change_tracker::GitSnapshot::snapshot(&tracking.root).await;
        // 十期·四路 R1：非 git 目录（含 git 快照不可用）改用内容哈希检测——
        // 执行后对同一组允许路径再采内容快照，与执行前 CAS 基线逐文件比哈希
        // （新增/修改/删除），不依赖 git。
        let (changed, detection) = if pre.git && post.git {
            (
                workspace_change_tracker::merge_changed_files(
                    &pre,
                    &post,
                    &pre_dirty_hashes,
                    &tracking.root,
                ),
                "git 窗口+内容哈希",
            )
        } else {
            let post_base = owo_agent_core::change_set::snapshot_allowed_paths(
                &tracking.root,
                &tracking.allowed,
                &tracking.cas,
            )
            .await;
            (
                owo_agent_core::change_set::merge_content_snapshots(&base, &post_base),
                "内容哈希（非 git）",
            )
        };
        let foreign_scopes: Vec<WriteScope> = _lease
            .as_ref()
            .map(|guard| guard.foreign_scopes_in_window(pre.at, post.at))
            .unwrap_or_default();
        let changed: Vec<String> = if tracking.allowed.is_empty() {
            changed
        } else {
            let pre_dirty: HashSet<String> = pre.dirty_paths().into_iter().collect();
            changed
                .into_iter()
                .filter(|path| {
                    if workspace_change_tracker::path_in_allowed(
                        path,
                        &tracking.root,
                        &tracking.allowed,
                    ) {
                        return true;
                    }
                    if foreign_scopes
                        .iter()
                        .any(|scope| scope.covers(&tracking.root, path))
                    {
                        return false;
                    }
                    !pre_dirty.contains(path)
                })
                .collect()
        };
        let violation =
            workspace_change_tracker::check_whitelist(&changed, &tracking.root, &tracking.allowed)
                .err();
        let step = input
            .get("_workswarm")
            .and_then(|meta| meta.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let (record, record_error) = match tracking
            .record(&step, &post, &changed, violation.as_deref(), Some(&base))
            .await
        {
            Ok(record) => (Some(record), None),
            Err(error) => {
                tracing::error!(
                    team_id = %tracking.team_id,
                    role = %tracking.role,
                    %error,
                    "工作区变更记录落盘失败"
                );
                (None, Some(error))
            }
        };
        if let Some(violation) = violation {
            // 越界：变更已留痕（record 含 violation 字段）但步骤失败——引擎按失败
            // 处理，不登记成功 Artifact；不生成可审批 ChangeSet（越界写入不可审查
            // 为合法工作区改动，恢复基线只会制造更大的不可控面）。
            return Err(violation);
        }
        // 九期（一路）：无实际变更 → 不创建空 pending ChangeSet（此前会生成
        // changed_files 为空的空壳待办，人工审批无法操作），只记审计留痕。
        if changed.is_empty() {
            if let Some(audit) = &tracking.audit {
                if let Ok(mut audit) = audit.lock() {
                    audit.record(
                        &tracking.team_id,
                        "change_set.no_change",
                        Some(format!("workswarm/{}", tracking.team_id)),
                        Some(true),
                        format!(
                            "步骤 {} 无实际工作区变更（检测：{detection}），跳过 ChangeSet 生成",
                            step
                        ),
                    );
                }
            }
            return match (result, record_error) {
                (Ok(_), Some(error)) => Err(format!("工作区变更记录未落盘，步骤拒绝成功：{error}")),
                (Err(original), Some(error)) => {
                    Err(format!("{original}；此外，工作区变更记录未落盘：{error}"))
                }
                (result, None) => result,
            };
        }
        // 十期·四路 R1：无论步骤成败都生成 ChangeSet（pending_review，等人工
        // accept/reject/revert）——「写入后失败」「写入中取消」留下的残留修改
        // 同样可审查、可恢复；未接受时该团队代码 Artifact 不得成为最终 approved
        // head。变更追踪或 ChangeSet 任一持久化失败时，成功输出必须 fail closed，
        // 否则 DeliveryGate 会把无法审计/复原的工作区变更当成交付成功。
        let mut change_set = owo_agent_core::change_set::build_change_set(
            &tracking.team_id,
            &step,
            &tracking.role,
            &base,
            &changed,
            &tracking.root,
            record.as_ref().and_then(|record| record.diff_ref.clone()),
        );
        change_set.attempt_id = workswarm
            .and_then(|meta| meta.get("attempt_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&tracking.run_dir);
        let changeset_error = match store.save_upsert(&change_set) {
            Ok(()) => {
                if let Some(audit) = &tracking.audit {
                    if let Ok(mut audit) = audit.lock() {
                        audit.record(
                            &tracking.team_id,
                            "change_set.created",
                            Some(format!("workswarm/{}", tracking.team_id)),
                            Some(true),
                            format!(
                                "{} 生成（步骤 {}，检测：{detection}，文件：{}）",
                                change_set.change_set_id,
                                step,
                                changed.join(", ")
                            ),
                        );
                    }
                }
                None
            }
            Err(error) => {
                tracing::error!(
                    team_id = %tracking.team_id,
                    role = %tracking.role,
                    error = %error,
                    "ChangeSet 落盘失败（变更审批闭环缺失）"
                );
                Some(error.to_string())
            }
        };
        let persistence_errors = record_error
            .into_iter()
            .map(|error| format!("工作区变更记录未落盘：{error}"))
            .chain(
                changeset_error
                    .into_iter()
                    .map(|error| format!("ChangeSet 未落盘：{error}")),
            )
            .collect::<Vec<_>>();
        // 租约在此作用域末尾释放。只有原执行和全部必要变更收据都持久化成功，
        // 步骤才可返回成功；原执行已失败时保留原错误并附上留证故障。
        if persistence_errors.is_empty() {
            return result;
        }
        let persistence_detail = persistence_errors.join("；");
        match result {
            Ok(_) => Err(format!(
                "工作区已变更，但交付留证不完整，步骤拒绝成功：{persistence_detail}"
            )),
            Err(error) => Err(format!(
                "{error}；此外，工作区变更留证不完整：{persistence_detail}"
            )),
        }
    }
}

#[cfg(test)]
mod worker_context_integration_tests {
    use super::*;

    #[tokio::test]
    async fn worker_context_publish_is_visible_on_next_read_and_prompt_slice() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();
        let store = Arc::new(
            owo_agent_core::SqliteProjectSpaceStore::open(&dir.path().join("space.db")).unwrap(),
        );
        let cas = owo_agent_core::CasStore::new(dir.path().join("cas")).unwrap();
        let templates = Arc::new(owo_agent_core::TeamTemplateRegistry::new(
            dir.path().join("templates"),
        ));
        let coordinator = Arc::new(owo_agent_core::TeamCoordinator::new(
            store, templates, cas, runs,
        ));
        let team = coordinator
            .create_team_run(&owo_agent_core::CreateTeamRequest::new(
                "验证 Worker 共享上下文",
                owo_agent_protocol::TeamMode::Single,
            ))
            .await
            .unwrap();
        let step_id = coordinator
            .load_run_state(&team.team_id)
            .unwrap()
            .plan
            .steps[0]
            .id
            .clone();

        let policy = owo_agent_core::Policy::new(dir.path());
        let mut session = owo_agent_core::Session::new(dir.path(), "test-model", None);
        let audit = Arc::new(std::sync::Mutex::new(
            owo_agent_core::audit::AuditLog::default(),
        ));
        let skills = owo_agent_core::SkillRegistry::default();
        let elements = Arc::new(std::sync::Mutex::new(owo_agent_core::ElementRegistry::new()));
        let mut tool_context = ToolContext {
            workspace: dir.path(),
            policy: &policy,
            session: &mut session,
            audit: &audit,
            subagent: None,
            skills: &skills,
            elements: &elements,
            fanout: None,
            abort: None,
            questioner: None,
        };

        let publisher = TeamContextPublishTool {
            coordinator: Arc::clone(&coordinator),
            team_id: team.team_id.clone(),
            member_id: "m-runner".into(),
            task_id: step_id.clone(),
        };
        let published = publisher
            .run(
                &mut tool_context,
                json!({
                    "key":"api.contract",
                    "value":"GET /items returns ItemList",
                    "expected_revision":0,
                    "source_refs":["src/api.rs"]
                }),
            )
            .await
            .unwrap();
        assert_eq!(published["revision"], 1);
        assert_eq!(published["status"], "candidate");

        let draft = |key: &str| owo_agent_core::workswarm::SharedContextFactDraft {
            key: key.into(),
            value: format!("value for {key}"),
            producer: "m-runner".into(),
            task_id: None,
            source_refs: Vec::new(),
            file_hash: None,
        };
        let left = coordinator.publish_team_context_fact(&team.team_id, 1, draft("race.left"));
        let right = coordinator.publish_team_context_fact(&team.team_id, 1, draft("race.right"));
        let (left, right) = tokio::join!(left, right);
        assert_ne!(
            left.is_ok(),
            right.is_ok(),
            "相同 expected_revision 的并发发布必须恰有一个胜者"
        );
        assert_eq!(
            coordinator
                .read_team_context(&team.team_id)
                .await
                .unwrap()
                .revision,
            2
        );

        let reader = TeamContextReadTool {
            coordinator: Arc::clone(&coordinator),
            team_id: team.team_id.clone(),
            member_id: "m-runner".into(),
            step_id: step_id.clone(),
            task_id: Some(step_id.clone()),
            refs: vec!["src/api.rs".into()],
            workspace_root: dir.path().to_path_buf(),
        };
        let observed = reader
            .run(&mut tool_context, json!({"key":"api.contract"}))
            .await
            .unwrap();
        assert_eq!(observed["revision"], 2);
        assert_eq!(observed["facts"][0]["value"], "GET /items returns ItemList");

        let slice = coordinator
            .assemble_context_slice(&team.team_id, "m-runner", &step_id)
            .await
            .unwrap();
        assert_eq!(slice["shared_context_revision"], 2);
        assert!(slice["shared_facts"]
            .to_string()
            .contains("GET /items returns ItemList"));
    }
}

#[cfg(test)]
mod tracked_worker_parallel_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Barrier;

    struct ConcurrentProbe {
        barrier: Arc<Barrier>,
        entered: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Worker for ConcurrentProbe {
        fn name(&self) -> &str {
            "concurrent-probe"
        }

        async fn run(&self, _input: &Value) -> Result<String, String> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait().await;
            Ok("completed".to_string())
        }
    }

    #[tokio::test]
    async fn same_role_disjoint_task_scopes_enter_worker_concurrently() {
        let temp = tempfile::tempdir().expect("temporary workspace");
        let root = std::fs::canonicalize(temp.path()).expect("canonical workspace");
        let run_dir = root.join("run");
        std::fs::create_dir_all(root.join("src/a")).expect("task a scope");
        std::fs::create_dir_all(root.join("src/b")).expect("task b scope");
        std::fs::create_dir_all(&run_dir).expect("team run directory");

        let allowed = vec![root.join("src/a"), root.join("src/b")];
        let lease = WriteLease::new(
            super::super::write_lease::WriteLeaseManager::new(),
            WriteScope::from_paths(&allowed),
        );
        let tracker = workspace_change_tracker::Tracker {
            root: root.clone(),
            run_dir: run_dir.clone(),
            team_id: "team-parallel".to_string(),
            role: "w1".to_string(),
            allowed,
            cas: owo_agent_core::cas_store::CasStore::new(run_dir.join("cas"))
                .expect("CAS should initialize"),
            audit: None,
        };
        let entered = Arc::new(AtomicUsize::new(0));
        let worker = TrackedRoleWorker {
            inner: Arc::new(ConcurrentProbe {
                barrier: Arc::new(Barrier::new(2)),
                entered: Arc::clone(&entered),
            }),
            lease: Some(lease),
            lease_waits: Some(Arc::new(workswarm_metrics::LeaseWaitTracker::default())),
            tracking: Some(tracker),
        };
        let task_input = |task: &str, path: &str| {
            json!({
                "assigned_task_id": task,
                "required_capabilities": ["write_file"],
                "assigned_write_paths": [path],
                "_workswarm": {"step_id": format!("s-{task}"), "phase_epoch": 1}
            })
        };
        let input_a = task_input("task-a", "src/a");
        let input_b = task_input("task-b", "src/b");
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(worker.run(&input_a), worker.run(&input_b))
        })
        .await
        .expect("disjoint task scopes must reach the worker together");

        assert!(results.0.is_ok(), "task-a wrapper failed: {:?}", results.0);
        assert!(results.1.is_ok(), "task-b wrapper failed: {:?}", results.1);
        assert_eq!(entered.load(Ordering::SeqCst), 2);
    }
}
