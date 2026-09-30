use crate::tools::ToolApprovalGrant;
pub(super) const MAX_TOOL_RESULT_CHARS: usize = 50_000;

/// §9.1 阶段一产物：按原始 tool-call 顺序完成的权限判定（Ask 已归并为 Allow/Deny）。
pub(super) struct PreparedCall {
    /// Policy/Approver 归并后的类型化放行凭证；拒绝调用没有执行凭证。
    pub(super) approval: Option<ToolApprovalGrant>,
    /// 权限理由（拒绝消息文案）。
    pub(super) reason: String,
    /// 循环保护拦截原因（Some = 本调用被宿主拦截，不执行、不审批）。
    ///
    /// 对标 Codex/OpenCode 的 loop guard：弱模型遇到工具报错时会反复发起**完全相同**
    /// 的调用（例如写文件失败后原样重发），若不加约束，`max_turns` × 并发组会把
    /// 一次任务放大成几百次工具执行。这里对「同一 name + 规范化参数」的重复调用
    /// 计数，超限即拦截并回灌可读原因，让模型改策略而不是空转。
    pub(super) guard_error: Option<String>,
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
    /// 循环保护：单回合内允许请求的工具调用总数上限（默认 64）。
    ///
    /// 与 `max_turns` 的区别：`max_turns` 约束**模型调用轮数**，本项约束**工具执行总量**。
    /// 二者相乘才是真实上界；只设 `max_turns` 时，60 轮 × 每轮多并发会放大成数百次执行。
    pub max_tool_calls_per_turn: usize,
    /// 循环保护：同一 `name + 规范化参数` 的调用在同一回合内允许重复的次数（默认 3）。
    /// 超过即拦截该调用并回灌"改变策略"提示，不再执行。
    pub max_repeated_tool_calls: usize,
    /// §9.2：turn 级统一截止时间；None = 不限时（保持既有行为，仅记账）。
    pub turn_deadline: Option<std::time::Duration>,
    /// 单次回复的最大输出 token（配置文件 `model.max_output_tokens`）。
    pub max_output_tokens: Option<usize>,
    /// 采样温度（配置文件 `model.temperature`）。
    pub temperature: Option<f64>,
    /// 单次模型请求超时秒数（配置文件 `model.timeout_secs`）。
    pub request_timeout_secs: Option<usize>,
}

impl AgentConfig {
    /// 从环境变量覆盖上下文相关预算（**配置文件 → 环境变量 → 代码默认**）。
    ///
    /// 为什么做成环境变量而不是塞进代码：用户明确要求"模型服务地址、模型名称、
    /// 上下文等全都通过文件随时更改，不能写死"。桌面壳读 `config.json` 后把这些值
    /// 注入核心进程环境，核心在这里消费——单一实现、单一来源，改文件即生效。
    ///
    /// 非法值（0、非数字）**忽略并保留默认**，不 panic：用户手写配置文件打错字
    /// 不该让核心起不来，但也不能静默采纳一个会让压缩逻辑失效的 0。
    pub fn with_env_overrides(mut self) -> Self {
        if let Some(value) = env_usize("OWO_MODEL_CONTEXT_WINDOW") {
            self.token_budget = value;
        }
        if let Some(value) = env_usize("OWO_MODEL_MAX_OUTPUT_TOKENS") {
            self.max_output_tokens = Some(value);
        }
        if let Some(value) = env_usize("OWO_AGENT_KEEP_RECENT") {
            self.keep_recent = value;
        }
        if let Some(value) = env_bool("OWO_AGENT_COMPACTION") {
            self.compaction_enabled = value;
        }
        if let Some(value) = env_f64("OWO_MODEL_TEMPERATURE") {
            self.temperature = Some(value);
        }
        if let Some(value) = env_usize("OWO_MODEL_TIMEOUT_SECS") {
            self.request_timeout_secs = Some(value);
        }
        if let Some(value) = env_usize("OWO_AGENT_MAX_TOOL_CALLS") {
            self.max_tool_calls_per_turn = value;
        }
        if let Some(value) = env_usize("OWO_AGENT_MAX_REPEATED_TOOL_CALLS") {
            self.max_repeated_tool_calls = value;
        }
        self
    }
}

/// 读一个正整数环境变量；空串/非法/0 一律当作"未设置"。
fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name)
        .ok()
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
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
            max_tool_calls_per_turn: 64,
            max_repeated_tool_calls: 3,
            turn_deadline: None,
            max_output_tokens: None,
            temperature: None,
            request_timeout_secs: None,
        }
    }
}
