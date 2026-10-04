//! 角色画像（WorkerProfile，七期 · 二路）：把模板声明的角色权限真正作用到 Worker。
//!
//! - [`WorkerProfile::for_role`]：内置角色 → 工具面 / 只读 / 写白名单 / 回合上限 /
//!   浏览器 / 受控命令的映射（分析·审查·校验·抽取族 = 只读；实现族 = 读写 +
//!   受控命令；研究族 = 只读 + 浏览器；**未知角色默认只读**——权限默认 deny，
//!   显式匹配才有写面）；
//! - [`WorkerProfile::build_registry`]：按画像装配 [`ToolRegistry`]——**注册表面即
//!   权限边界**：读角色的注册表里根本没有写/执行工具，而非注册后靠审批拒绝；
//! - [`intersect_paths`]：角色写白名单 ∩ 团队绑定写白名单（任一侧为空 = 取非空一侧；
//!   两侧都空 = 工作区内可写，仍受审批约束）；
//! - `ProfileSubagentRunner` 与 `ContractSubagentRunner` 是 capability-resolution adapters；
//!   两者均把已解析的策略/工具/预算注入 core `WorkerRuntime`。Runtime 统一 Agent 回合、
//!   任务会话、取消、WorkerOutputV1 修复和逐请求用量，不推断角色或授予工具权限。

use crate::agent::{AgentConfig, TurnEvent};
use crate::gateway::ModelProvider;
use crate::permissions::{Approver, Policy};
use crate::tools::ToolRegistry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 可选 Worker 回合事件回调；事件使用方应只记录安全元数据。
pub type TurnEventSink = Arc<dyn Fn(&TurnEvent) + Send + Sync>;

/// 画像回合上限（与 `SubagentRunner` 子代理口径一致：max_turns 硬上限 16）。
pub const PROFILE_MAX_TURNS_CAP: usize = 16;

/// 模板未声明该角色预算（`budget_calls == 0`）时的缺省回合上限。
pub const DEFAULT_PROFILE_MAX_TURNS: usize = 12;

/// 角色族（工具面装配依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleFamily {
    /// 分析/审查/校验/抽取族：只读文件面（read_file / list_dir / search_files）。
    Read,
    /// 实现族：读写文件 + 搜索 + 受控命令。
    Implementer,
    /// 研究族：只读文件面 + 浏览器（搜索/导航/快照）。
    Researcher,
}

/// Whether a role name is one of the reserved finite TaskGraph writer slots.
pub fn is_parallel_writer_name(role: &str) -> bool {
    role.strip_prefix('w')
        .map(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

/// 角色权限画像：单角色的实际工具面与执行上限（七期 · 二路）。
///
/// 由模板角色名（`budget_calls_per_role[].role`）派生；服务端在每个 TeamRun 阶段
/// 重建注册表时逐角色实例化，最终写面 = 「团队绑定写白名单 ∩ 角色白名单」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerProfile {
    /// 模型可见工具名（注册表按它裁剪；空 = 不额外过滤）。
    pub visible_tools: Vec<String>,
    /// 只读角色：注册表不含任何写/执行工具，`Policy::read_only` 再兜底一层。
    pub read_only: bool,
    /// 角色级写白名单（相对工作区根；空 = 交由团队绑定白名单决定）。
    pub write_allowed_paths: Vec<String>,
    /// 回合上限（模板 `budget_calls_per_role[].budget_calls`；硬上限 16）。
    pub max_turns: usize,
    /// 允许浏览器（搜索/导航/快照；写工作区变体不在可见面）。
    pub can_use_browser: bool,
    /// 允许受控命令（run_command；仍经沙箱 + 审批策略约束）。
    pub can_run_command: bool,
    /// Host-enforced timeout for the task's registered behavior command.
    #[serde(default)]
    pub verification_timeout_ms: Option<u64>,
}

impl WorkerProfile {
    /// Assemble the base profile for a Team role before any task is claimed.
    /// Parallel writer slots start with a writable tool ceiling so a later
    /// host-validated TaskGraph scope can narrow it; the task input must still
    /// explicitly grant write_file/apply_patch before those tools are exposed.
    pub fn for_team_role(
        role: &str,
        capabilities: &[String],
        budget_calls: usize,
        has_declared_write_scope: bool,
        is_parallel_writer_slot: bool,
    ) -> Self {
        if crate::workswarm::is_review_role(role, capabilities) {
            Self::for_role("reviewer", budget_calls)
        } else if has_declared_write_scope || is_parallel_writer_slot {
            Self::explicit_writer(budget_calls)
        } else {
            Self::for_role_with_capabilities(role, capabilities, budget_calls)
        }
    }

    /// 内置角色 → 画像。`budget_calls` = 模板每角色调用预算（0 = 未声明 → 缺省 12）。
    ///
    /// 映射口径（与四类内置模板对齐）：
    /// - 实现族（名字含 `implementer` / `builder` / `finalizer` / `drafter`，如
    ///   code-change-v1 的 implementer、document-delivery-v1 的 drafter/finalizer）：
    ///   读写文件 + 搜索 + `run_command`；
    /// - 研究族（`researcher*` / 含 `research` / `brief_writer`，如 research-brief-v1
    ///   全角色）：只读文件 + 浏览器三件套（browser_search/navigate/snapshot）；
    /// - 其余（code_analyzer / reviewer / evidence_verifier / schema_validator /
    ///   extractor / artifact_formatter / critic 等分析·审查·校验·抽取族）：
    ///   只读文件面；未知角色同样落这里（默认 deny）。
    pub fn for_role_with_capabilities(
        role: &str,
        capabilities: &[String],
        budget_calls: usize,
    ) -> Self {
        if crate::workswarm::is_review_role(role, capabilities) {
            return Self::for_role("reviewer", budget_calls);
        }
        Self::for_role(role, budget_calls)
    }

    pub fn for_role(role: &str, budget_calls: usize) -> Self {
        let name = role.trim().to_ascii_lowercase();
        // 写角色关键词：实现/交付族。**必须包含通用 producer/writer/leader**——
        // 否则 `producer_role_name` 对未知分类返回的 "producer"（以及 forced team
        // 的 "leader"）会落进只读分支：团队"成功"却什么都没写（"草草了事"的根因）。
        // 注意判定顺序：researcher 分支在前，"brief_writer" 仍是只读研究角色。
        const WRITER_KEYWORDS: [&str; 7] = [
            "implementer",
            "builder",
            "finalizer",
            "drafter",
            "producer",
            "writer",
            "leader",
        ];
        let is_implementer = WRITER_KEYWORDS.iter().any(|k| name.contains(k));
        let is_researcher = name.starts_with("researcher")
            || name.contains("research")
            || name.contains("brief_writer");
        let max_turns = (if budget_calls == 0 {
            DEFAULT_PROFILE_MAX_TURNS
        } else {
            budget_calls
        })
        .clamp(1, PROFILE_MAX_TURNS_CAP);
        if is_researcher {
            // 研究族优先于写角色判定（brief_writer 含 "writer" 但只读）。
            Self {
                visible_tools: [
                    "read_file",
                    "list_dir",
                    "search_files",
                    "browser_search",
                    "browser_navigate",
                    "browser_snapshot",
                ]
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
                read_only: true,
                write_allowed_paths: Vec::new(),
                max_turns,
                can_use_browser: true,
                can_run_command: false,
                verification_timeout_ms: None,
            }
        } else if is_implementer {
            Self {
                // 写角色的工具面：读写 + 搜索 + 执行（白名单写工具由
                // `build_registry` 装配；注册表面即权限边界）。
                visible_tools: [
                    "read_file",
                    "write_file",
                    "apply_patch",
                    "list_dir",
                    "search_files",
                    "run_command",
                ]
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
                read_only: false,
                write_allowed_paths: Vec::new(),
                max_turns,
                can_use_browser: false,
                can_run_command: true,
                verification_timeout_ms: None,
            }
        } else {
            Self {
                visible_tools: ["read_file", "list_dir", "search_files"]
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect(),
                read_only: true,
                write_allowed_paths: Vec::new(),
                max_turns,
                can_use_browser: false,
                can_run_command: false,
                verification_timeout_ms: None,
            }
        }
    }

    /// 角色族。
    pub fn family(&self) -> RoleFamily {
        if !self.read_only {
            RoleFamily::Implementer
        } else if self.can_use_browser {
            RoleFamily::Researcher
        } else {
            RoleFamily::Read
        }
    }

    /// 是否写角色（范围写租约与变更追踪只作用于写角色）。
    pub fn is_writer(&self) -> bool {
        !self.read_only
    }

    /// 显式写角色画像（十一期 · 二路）：角色声明了写范围（`RoleSpec.write_paths`）
    /// 时使用——即使角色名未命中实现族关键词（如自定义 `w1`），也按实现族装配
    /// 工具面（读写 + 搜索 + 受控命令；注册表面即权限边界）；最终写面仍由
    /// 「角色 ∩ 团队绑定」白名单收窄。避免自定义角色名落进只读分支导致团队
    /// "成功"却零产出。
    pub fn explicit_writer(budget_calls: usize) -> Self {
        Self::for_role("implementer", budget_calls)
    }

    /// Limit a TaskGraph attempt to its host-assigned total request budget. The final
    /// request remains available to the single WorkerOutputV1 correction path.
    pub fn with_task_model_call_budget(
        mut self,
        total_calls: usize,
    ) -> Result<Self, String> {
        if !(3..=PROFILE_MAX_TURNS_CAP).contains(&total_calls) {
            return Err("TaskGraph 单次尝试预算必须在 3..=16 次模型请求之间".to_string());
        }
        self.max_turns = self.max_turns.min(total_calls - 1);
        Ok(self)
    }

    /// 移除受控命令能力，但保留白名单文件读写；用于源码实现角色，避免模型
    /// 看到与任务无关的 shell 工具后重复运行测试或探测命令。
    pub fn without_commands(mut self) -> Self {
        self.can_run_command = false;
        self.visible_tools.retain(|tool| tool != "run_command");
        self
    }

    /// 按画像装配工具注册表：注册表面即权限边界。
    ///
    /// - `write_allowed`：最终写白名单（角色 ∩ 团队绑定，见 [`intersect_paths`]；
    ///   空 = 工作区内可写）。只读角色忽略该参数（不注册任何写工具）。
    /// - `visible_tools` 非空时按名单裁剪，保证「实际可见工具 == 模板/画像声明」
    ///   （浏览器组里的写工作区变体也会被裁掉）。
    pub fn build_registry(&self, write_allowed: Vec<PathBuf>) -> ToolRegistry {
        let mut registry = ToolRegistry::empty();
        registry.register_file_read_tools();
        if !self.read_only {
            registry.register_whitelist_write_file(write_allowed.clone());
            registry.register_whitelist_apply_patch(write_allowed);
        }
        if self.can_run_command {
            registry.register_run_command();
        }
        if self.can_use_browser {
            registry.register_browser_tools();
        }
        if !self.visible_tools.is_empty() {
            registry.retain_names(&self.visible_tools);
        }
        registry
    }

    /// 角色 Prompt 的「禁止执行/边界」行（八期 · 一路）：按族与工具面派生，
    /// 供 [`crate::team_prompt::compile_prompt`] 使用——Prompt 声称的能力边界与
    /// `build_registry` 装配的真实工具面一致（注册表面即权限边界的 Prompt 侧投影）。
    pub fn prompt_guard_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if self.visible_tools.is_empty() {
            lines
                .push("工具面未声明：仅可使用缺省只读文件面，禁止任何写入/命令/联网。".to_string());
        } else {
            lines.push(format!(
                "可见工具仅限：{}（工具面之外没有其他执行手段）。",
                self.visible_tools.join(" / ")
            ));
        }
        if self.read_only {
            lines.push(
                "禁止写入工作区文件（你是只读角色）：交付与结论一律通过产物正文/评审声明完成。"
                    .to_string(),
            );
        } else {
            lines.push(
                "只允许在允许写路径内用 write_file 或 apply_patch 落盘最终变更；精确补丁应基于 read_file 返回的 sha256 传 expected_hashes；禁止改写白名单外文件，\
                 禁止把变更只留在说明里而不落盘。"
                    .to_string(),
            );
        }
        if !self.can_run_command {
            lines.push("禁止执行命令（run_command 不在你的工具面）。".to_string());
        }
        if !self.can_use_browser {
            lines.push("禁止联网浏览（浏览器工具不在你的工具面）。".to_string());
        }
        lines
    }
}

/// 路径白名单交集（七期 · 二路）：角色写白名单 ∩ 团队绑定写白名单。
///
/// - 任一侧为空 = 取非空一侧（空表示「未额外约束」）；
/// - 两侧都空 = 空（工作区内可写，仍受审批约束）；
/// - 否则逐条收窄：a 的条目落在 b 某前缀内 → 保留该条目（更窄者）；b 某前缀落在
///   a 条目内 → 保留该 b 前缀（窄者胜，交集语义）。
pub fn intersect_paths(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    let mut intersection = Vec::new();
    for path in a {
        if b.iter().any(|base| path.starts_with(base)) {
            intersection.push(path.clone());
        } else if let Some(narrower) = b.iter().find(|base| base.starts_with(path)) {
            intersection.push(narrower.clone());
        }
    }
    intersection
}

/// Compile the shared Worker system prompt used by production and product evaluation.
pub fn compile_worker_system_prompt(
    profile: &WorkerProfile,
    is_review_role: bool,
    budget_note: &str,
    extra_system_prompt: Option<&str>,
) -> String {
    let base_prompt = if is_review_role {
        "你是只读评审子代理：critic 不得提交 artifact；只能读取/搜索工作区文件，禁止写入或执行命令；独立检查交付并简洁汇报发现。\n"
    } else if profile.is_writer() {
        "你是写角色子代理：凡涉及代码/文件变更，必须在允许路径内真实落盘：小范围修改优先用 apply_patch，并传入 read_file 返回的 sha256 作为 expected_hashes；整文件生成或确需重写时使用 write_file（工具面之外没有其他写入手段）；artifact.content 只写变更说明、影响面与验证方式，不要把完整变更只放在 artifact 里而不落盘。回合预算有限：先做必要读取并完成写入；仅当任务验收需要且权限允许时，运行范围明确的定向检查，避免重复读取和全仓构建。最后一个回合只输出契约 JSON，不再调用工具。工具调用仍需审批；无法验证时如实说明。\n"
    } else {
        "你是通用子代理：独立完成委派任务，工具调用仍需审批，完成后汇报结果。\n"
    };
    format!(
        "{}{base_prompt}{budget_note}{}",
        extra_system_prompt.unwrap_or_default(),
        crate::workswarm_output::contract_system_prompt(is_review_role)
    )
}

/// 画像驱动子代理执行器（七期 · 二路）：与一路 `ContractSubagentRunner` 同口径
/// （完整回合循环 + `WorkerOutputV1` 输出契约执行 + 至多一次定向修复），区别仅在：
///
/// - 工具注册表由 [`WorkerProfile::build_registry`] 按角色装配（注册表面即权限边界）；
/// - 回合上限取画像值（模板预算，硬上限 16）；
/// - 写面为「角色 ∩ 绑定」交集白名单工具（越界写入在工具层被拒）；
/// - `is_critic` 由服务端按角色名判定（`role == "critic"`；引擎注入的 `read_only`
///   只覆盖 critic，其余内置角色都是 producer，画像另管只读面）。
pub struct ProfileSubagentRunner<'a> {
    pub provider: Arc<dyn ModelProvider>,
    pub approver: &'a dyn Approver,
    /// 中断标志：团队取消桥共享置位，`run_turn` 协作式检查。
    pub abort: &'a AtomicBool,
    pub depth: usize,
    pub model: String,
    /// critic 角色代理（true = 只读探索口径，禁带 artifact；false = producer）。
    pub is_critic: bool,
    /// 最终写白名单（角色 ∩ 绑定交集；空 = 工作区内可写）。
    pub write_allowed: Vec<PathBuf>,
    pub profile: WorkerProfile,
    /// Optional runtime limits supplied by a controlled harness; execution still uses this runner.
    pub agent_config: Option<AgentConfig>,
    /// Optional caller-specific budget wording; role contract/tool assembly stay shared.
    pub budget_note_override: Option<String>,
    /// Team 宿主提供的额外受控工具（仍由 ToolHost 执行）。
    pub extra_tools: Vec<Arc<dyn crate::tools::Tool>>,
    /// 可选的 Team 共享上下文使用说明。
    pub extra_system_prompt: Option<String>,
    /// 可选的脱敏回合事件出口；调用方只应记录安全元数据，不记录参数/结果正文。
    pub event_sink: Option<TurnEventSink>,
    /// Daemon session store, used to resume this team/task history on local rework.
    pub session_store: Option<Arc<dyn crate::session::SessionStore>>,
    pub worker_session_id: Option<String>,
    /// Source user session retained as the worker session parent.
    pub parent_session_id: Option<String>,
}

/// Measured result from the shared Team worker runtime.
#[derive(Debug, Clone)]
pub struct ProfileSubagentRunReport {
    pub output: String,
    pub duration_ms: u64,
    pub steps: usize,
    pub model_calls: u32,
    pub usage: crate::gateway::TokenUsage,
    /// False whenever any request, including contract repair, lacks attributable usage.
    pub usage_known: bool,
    pub output_repairs: u32,
}

/// Failure telemetry is retained so eval and production diagnostics do not hide
/// the cost of a rejected worker submission or its contract-repair request.
#[derive(Debug, Clone)]
pub struct ProfileSubagentRunError {
    pub message: String,
    pub duration_ms: u64,
    pub steps: usize,
    pub model_calls: u32,
    pub usage: crate::gateway::TokenUsage,
    pub usage_known: bool,
    pub output_repairs: u32,
}

impl From<String> for ProfileSubagentRunError {
    fn from(message: String) -> Self {
        Self {
            message,
            duration_ms: 0,
            steps: 0,
            model_calls: 0,
            usage: crate::gateway::TokenUsage::default(),
            usage_known: false,
            output_repairs: 0,
        }
    }
}

impl ProfileSubagentRunner<'_> {
    /// Compatibility entry point for production call sites.
    pub async fn run(&self, workspace: &Path, prompt: &str) -> Result<String, String> {
        self.run_report(workspace, prompt)
            .await
            .map(|report| report.output)
            .map_err(|error| error.message)
    }

    /// Resolve the capability profile here, then delegate all worker execution to WorkerRuntime.
    pub async fn run_report(
        &self,
        workspace: &Path,
        prompt: &str,
    ) -> Result<ProfileSubagentRunReport, ProfileSubagentRunError> {
        let policy = if self.profile.read_only {
            Policy::read_only(workspace.to_path_buf())
        } else {
            Policy::new(workspace.to_path_buf())
        };
        let mut registry = self.profile.build_registry(self.write_allowed.clone());
        for tool in &self.extra_tools {
            registry.register_arc(Arc::clone(tool));
        }
        let mut config = self.agent_config.clone().unwrap_or_else(|| AgentConfig {
            max_turns: self.profile.max_turns.min(PROFILE_MAX_TURNS_CAP),
            max_tool_calls_per_turn: crate::agent::DEFAULT_BOUNDED_TOOL_CALL_CAP,
            subagent_depth: self.depth + 1,
            ..Default::default()
        });
        let profile_turn_cap = self.profile.max_turns.min(PROFILE_MAX_TURNS_CAP);
        config.max_turns = if config.max_turns == 0 {
            profile_turn_cap
        } else {
            config.max_turns.min(profile_turn_cap)
        };
        if config.max_tool_calls_per_turn == 0 {
            config.max_tool_calls_per_turn = crate::agent::DEFAULT_BOUNDED_TOOL_CALL_CAP;
        }
        if let Some(task_timeout_ms) = self.profile.verification_timeout_ms {
            config.max_command_timeout_ms = Some(
                config
                    .max_command_timeout_ms
                    .map_or(task_timeout_ms, |configured| configured.min(task_timeout_ms)),
            );
        }
        config.subagent_depth = self.depth + 1;
        let budget_note = self.budget_note_override.clone().unwrap_or_else(|| {
            format!(
                "你的回合预算为 {} 回合：前 {} 回合完成必要的读取、写入和任务要求的定向验证；最后一个回合必须直接输出最终 JSON（不要再调用任何工具）。尽量少花回合。\n",
                self.profile.max_turns,
                self.profile.max_turns.saturating_sub(1)
            )
        });
        let system_prompt = compile_worker_system_prompt(
            &self.profile,
            self.is_critic,
            &budget_note,
            self.extra_system_prompt.as_deref(),
        );
        let runtime = crate::worker_runtime::WorkerRuntime {
            provider: Arc::clone(&self.provider),
            approver: self.approver,
            abort: self.abort,
            depth: self.depth,
            model: self.model.clone(),
            workspace: workspace.to_path_buf(),
            registry,
            policy,
            config,
            system_prompt: Some(system_prompt),
            is_critic: self.is_critic,
            event_sink: self.event_sink.clone(),
            session_store: self.session_store.clone(),
            worker_session_id: self.worker_session_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
        };
        runtime
            .run_report(prompt)
            .await
            .map(|report| ProfileSubagentRunReport {
                output: report.output,
                duration_ms: report.duration_ms,
                steps: report.steps,
                model_calls: report.model_calls,
                usage: report.usage,
                usage_known: report.usage_known,
                output_repairs: report.output_repairs,
            })
            .map_err(|error| ProfileSubagentRunError {
                message: error.message,
                duration_ms: error.duration_ms,
                steps: error.steps,
                model_calls: error.model_calls,
                usage: error.usage,
                usage_known: error.usage_known,
                output_repairs: error.output_repairs,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect()
    }

    #[test]
    fn implementer_gets_write_and_command_surface() {
        let profile = WorkerProfile::for_role("implementer", 5);
        assert_eq!(profile.family(), RoleFamily::Implementer);
        assert!(profile.is_writer());
        assert!(profile.can_run_command);
        assert_eq!(profile.max_turns, 5);
        let names = tool_names(&profile.build_registry(Vec::new()));
        for expected in [
            "read_file",
            "write_file",
            "apply_patch",
            "list_dir",
            "search_files",
            "run_command",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "implementer 缺工具 {expected}：{names:?}"
            );
        }
        assert_eq!(names.len(), 6, "implementer 不应有多余工具：{names:?}");
    }

    #[test]
    fn team_parallel_writer_slots_start_writable_but_review_roles_stay_read_only() {
        assert!(is_parallel_writer_name("w1"));
        assert!(is_parallel_writer_name("w12"));
        assert!(!is_parallel_writer_name("writer1"));
        assert!(!is_parallel_writer_name("w1x"));

        let writer = WorkerProfile::for_team_role("w1", &[], 4, false, true);
        assert!(!writer.read_only);
        assert!(writer.visible_tools.iter().any(|tool| tool == "write_file"));

        let ordinary = WorkerProfile::for_team_role("w1", &[], 4, false, false);
        assert!(ordinary.read_only);
        assert!(!ordinary
            .visible_tools
            .iter()
            .any(|tool| tool == "write_file"));

        let scoped_custom = WorkerProfile::for_team_role("frontend_engineer", &[], 4, true, false);
        assert!(!scoped_custom.read_only);
        assert!(scoped_custom
            .visible_tools
            .iter()
            .any(|tool| tool == "write_file"));

        let reviewer = WorkerProfile::for_team_role("w1", &["review".to_string()], 4, false, true);
        assert!(reviewer.read_only);
        assert!(!reviewer
            .visible_tools
            .iter()
            .any(|tool| tool == "write_file"));
    }

    #[test]
    fn generic_producer_writer_leader_roles_are_writers() {
        // 回归：`producer_role_name` 对未知分类返回 "producer"，forced team 用
        // "leader"，文档族用 "writer"——这些都必须能落盘，否则团队"成功"却没产出。
        for role in ["producer", "writer", "leader", "builder", "implementer"] {
            let profile = WorkerProfile::for_role(role, 6);
            assert!(profile.is_writer(), "{role} 应为写角色");
            assert!(profile.can_run_command, "{role} 应可执行命令");
            let names = tool_names(&profile.build_registry(Vec::new()));
            assert!(
                names.iter().any(|name| name == "write_file"),
                "{role} 注册表缺 write_file：{names:?}"
            );
        }
        // brief_writer 仍走研究族（只读），不被 "writer" 关键词误判为写角色。
        let brief = WorkerProfile::for_role("brief_writer", 6);
        assert!(brief.read_only, "brief_writer 必须保持只读");
        assert!(!brief.is_writer(), "brief_writer 不应是写角色");
    }

    #[test]
    fn source_writer_profile_removes_command_tool_but_keeps_scoped_file_writes() {
        let profile = WorkerProfile::explicit_writer(8).without_commands();
        assert!(!profile.can_run_command);
        assert!(!profile
            .visible_tools
            .iter()
            .any(|tool| tool == "run_command"));
        assert!(profile
            .visible_tools
            .iter()
            .any(|tool| tool == "write_file"));
        assert!(profile
            .visible_tools
            .iter()
            .any(|tool| tool == "apply_patch"));
        assert!(profile.is_writer());
    }

    #[test]
    fn explicit_writer_profile_overrides_read_only_role_names() {
        // 十一期：声明了写范围的自定义角色（w1/w2…）不能落进只读分支。
        for role in ["w1", "module-b", "some_future_role"] {
            assert!(
                !WorkerProfile::for_role(role, 0).is_writer(),
                "{role} 无名命中 → 缺省只读（权限默认 deny）"
            );
            let profile = WorkerProfile::explicit_writer(4);
            assert!(profile.is_writer(), "{role} 显式写角色应为写面");
            assert_eq!(profile.max_turns, 4, "预算应透传");
            assert!(profile.can_run_command, "写角色应可执行命令");
            let names = tool_names(&profile.build_registry(Vec::new()));
            assert!(
                names.iter().any(|name| name == "write_file"),
                "{role} 显式写角色注册表缺 write_file：{names:?}"
            );
        }
    }

    #[test]
    fn explicit_review_capability_keeps_custom_role_read_only() {
        let profile =
            WorkerProfile::for_role_with_capabilities("quality_gate", &["review".to_string()], 4);
        assert!(profile.read_only);
        assert!(!profile.is_writer());
        assert_eq!(profile.max_turns, 4);
        let names = tool_names(&profile.build_registry(Vec::new()));
        assert!(!names.iter().any(|name| name == "write_file"));
        assert!(!names.iter().any(|name| name == "run_command"));
    }

    #[test]
    fn writer_prompt_recommends_hash_guarded_patch_for_small_changes() {
        let profile = WorkerProfile::for_role("implementer", 5);
        let lines = profile.prompt_guard_lines().join("\n");
        assert!(lines.contains("apply_patch"));
        assert!(lines.contains("expected_hashes"));
        assert!(lines.contains("read_file 返回的 sha256"));

        let read_only = WorkerProfile::for_role("reviewer", 5);
        let read_only_lines = read_only.prompt_guard_lines().join("\n");
        assert!(!read_only_lines.contains("apply_patch"));
        assert!(read_only_lines.contains("禁止写入工作区文件"));
    }

    #[test]
    fn implementer_write_tool_is_whitelist_wrapped_but_names_unchanged() {
        // 白名单包装不改变工具名（模型可见面不变），仅在执行时做前缀校验。
        let profile = WorkerProfile::for_role("implementer", 5);
        let allowed = vec![PathBuf::from("T:/ws/src")];
        let names = tool_names(&profile.build_registry(allowed));
        assert!(names.iter().any(|name| name == "write_file"));
        assert!(names.iter().any(|name| name == "apply_patch"));
    }

    #[test]
    fn read_roles_cannot_write_or_run_commands() {
        for role in [
            "code_analyzer",
            "reviewer",
            "evidence_verifier",
            "schema_validator",
            "extractor",
            "artifact_formatter",
            "content_reviewer",
            // 未知角色默认只读（权限默认 deny）。
            "some_future_role",
        ] {
            let profile = WorkerProfile::for_role(role, 3);
            assert_eq!(profile.family(), RoleFamily::Read, "{role} 应为只读读面");
            assert!(!profile.is_writer(), "{role} 不应是写角色");
            assert!(!profile.can_run_command, "{role} 不应允许 run_command");
            assert!(!profile.can_use_browser, "{role} 不应允许浏览器");
            let names = tool_names(&profile.build_registry(Vec::new()));
            assert!(
                !names.iter().any(|name| name == "write_file"),
                "{role} 注册表含 write_file：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "run_command"),
                "{role} 注册表含 run_command：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name.starts_with("browser_")),
                "{role} 注册表含浏览器工具：{names:?}"
            );
            assert_eq!(names.len(), 3, "{role} 工具面应恰为读三件套：{names:?}");
        }
    }

    #[test]
    fn researcher_family_gets_browser_without_write() {
        for role in ["researcher_a", "researcher_b", "brief_writer"] {
            let profile = WorkerProfile::for_role(role, 4);
            assert_eq!(profile.family(), RoleFamily::Researcher, "{role}");
            assert!(profile.read_only);
            assert!(profile.can_use_browser);
            let names = tool_names(&profile.build_registry(Vec::new()));
            assert!(
                !names.iter().any(|name| name == "write_file"),
                "{role} 注册表含 write_file：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "run_command"),
                "{role} 注册表含 run_command：{names:?}"
            );
            for expected in ["browser_search", "browser_navigate", "browser_snapshot"] {
                assert!(
                    names.iter().any(|name| name == expected),
                    "{role} 缺 {expected}：{names:?}"
                );
            }
            // 浏览器组里的写工作区变体必须在可见面裁剪后消失。
            assert!(
                !names.iter().any(|name| name == "browser_screenshot"),
                "{role} 不应含写工作区变体 browser_screenshot：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "browser_download_image"),
                "{role} 不应含写工作区变体 browser_download_image：{names:?}"
            );
        }
    }

    #[test]
    fn compiled_worker_system_prompt_uses_shared_role_contract() {
        let writer = WorkerProfile::explicit_writer(4);
        let writer_prompt = compile_worker_system_prompt(&writer, false, "budget", Some("host"));
        assert!(writer_prompt.starts_with("host"));
        assert!(writer_prompt.contains("apply_patch"));
        assert!(writer_prompt.contains("write_file"));
        assert!(writer_prompt.contains("无法验证时如实说明"));

        let reviewer = WorkerProfile::for_role("reviewer", 4);
        let review_prompt = compile_worker_system_prompt(&reviewer, true, "budget", None);
        assert!(review_prompt.contains("只读评审子代理"));
        assert!(review_prompt.contains("critic 不得提交 artifact"));
        assert!(!review_prompt.contains("必须在允许路径内真实落盘"));
    }

    #[test]
    fn task_total_call_budget_reserves_one_output_repair_request() {
        let profile = WorkerProfile::for_role("implementer", 12)
            .with_task_model_call_budget(5)
            .unwrap();
        assert_eq!(profile.max_turns, 4);
        assert!(WorkerProfile::for_role("implementer", 12)
            .with_task_model_call_budget(2)
            .is_err());
    }

    #[test]
    fn budget_maps_to_turn_cap() {
        assert_eq!(
            WorkerProfile::for_role("implementer", 0).max_turns,
            DEFAULT_PROFILE_MAX_TURNS,
            "未声明预算 → 缺省"
        );
        assert_eq!(WorkerProfile::for_role("implementer", 5).max_turns, 5);
        assert_eq!(
            WorkerProfile::for_role("implementer", 99).max_turns,
            PROFILE_MAX_TURNS_CAP,
            "预算超硬上限 → 截到 16"
        );
    }

    #[test]
    fn prompt_guard_lines_match_tool_surface() {
        // 写角色：写面 + 命令提示，无只读禁令。
        let writer = WorkerProfile::for_role("implementer", 5);
        let writer_lines = writer.prompt_guard_lines().join("\n");
        assert!(writer_lines.contains("write_file"));
        assert!(writer_lines.contains("apply_patch"));
        assert!(writer_lines.contains("sha256"));
        assert!(writer_lines.contains("允许写路径"));
        assert!(!writer_lines.contains("禁止写入工作区文件"));
        assert!(writer_lines.contains("禁止联网浏览"));
        // 读角色：只读禁令 + 无命令/浏览器。
        let reader = WorkerProfile::for_role("reviewer", 3);
        let reader_lines = reader.prompt_guard_lines().join("\n");
        assert!(reader_lines.contains("禁止写入工作区文件"));
        assert!(reader_lines.contains("禁止执行命令"));
        assert!(reader_lines.contains("禁止联网浏览"));
        assert!(reader_lines.contains("read_file"));
        // 研究族：浏览器放开，仍只读。
        let researcher = WorkerProfile::for_role("researcher_a", 4);
        let researcher_lines = researcher.prompt_guard_lines().join("\n");
        assert!(researcher_lines.contains("browser_search"));
        assert!(!researcher_lines.contains("禁止联网浏览"));
        assert!(researcher_lines.contains("禁止写入工作区文件"));
        // 未声明工具面（缺省画像不会出现，防御性分支仍如实告知）。
        let empty = WorkerProfile {
            visible_tools: Vec::new(),
            read_only: true,
            write_allowed_paths: Vec::new(),
            max_turns: 3,
            can_use_browser: false,
            can_run_command: false,
            verification_timeout_ms: None,
        };
        assert!(empty.prompt_guard_lines()[0].contains("未声明"));
    }

    #[test]
    fn intersect_paths_semantics() {
        let src = PathBuf::from("T:/ws/src");
        let lib = PathBuf::from("T:/ws/src/lib");
        let docs = PathBuf::from("T:/ws/docs");
        // 两侧都空 = 空（工作区内可写，仍受审批约束）。
        assert!(intersect_paths(&[], &[]).is_empty());
        // 任一侧空 = 取非空一侧。（四路集成微修：clippy 冗余 clone → std::slice::from_ref 借用）
        assert_eq!(
            intersect_paths(std::slice::from_ref(&src), &[]),
            vec![src.clone()]
        );
        assert_eq!(
            intersect_paths(&[], std::slice::from_ref(&docs)),
            vec![docs.clone()]
        );
        // 窄者胜：profile(src) ∩ scope(src/lib) = src/lib。
        assert_eq!(
            intersect_paths(std::slice::from_ref(&src), std::slice::from_ref(&lib)),
            vec![lib.clone()]
        );
        // 不相交 = 空。
        assert!(
            intersect_paths(std::slice::from_ref(&docs), std::slice::from_ref(&src)).is_empty()
        );
        // 多条目：相交的留下，不相交的丢弃。
        let got = intersect_paths(&[src.clone(), docs.clone()], &[lib.clone(), docs.clone()]);
        assert_eq!(got, vec![lib.clone(), docs.clone()]);
    }
}
