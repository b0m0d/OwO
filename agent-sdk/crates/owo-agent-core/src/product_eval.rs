//! ProductEval（V1-R1）：固定真实任务集 × 重复次数的产品级评测底座。
//!
//! 与 [`crate::eval`] 的分工：`eval` 面向工程回归（内置 demo 单轮套件），
//! 本模块面向**产品成功率**——每个任务固定输入 fixture、允许读写范围、
//! 预期 Artifact、自动检查器与超时/预算/权限；单 Agent 与多 Agent 在
//! **完全相同的任务、输入、权限、预算与检查器**下对照执行；矩阵支持
//! 中断续跑（不重复已完成 case），失败运行永久进入报告（禁止删除后重算成功率）。
//!
//! 执行模式：
//! - `dry`（参考回放执行器）：零模型调用、本地确定性——把任务内置
//!   `reference_outputs` 写入沙盒后交检查器判定，自检检查器可达性；
//! - `live`（生成式执行器）：经 [`ModelProvider`] 网关真实生成文件块，
//!   受控落盘后过检，产出真实的墙钟/调用次数/token 统计。

use crate::gateway::{ChatMessage, ModelOutput, ModelProvider, OpenAiCompatibleConfig, TokenUsage};
use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 当前产品评测任务 schema 版本。
pub const PRODUCT_EVAL_SCHEMA_VERSION: u32 = 1;

/// 单文件草稿/评审提示词的最大保留字符数（防止超长上下文）。
const PROMPT_TEXT_CAP: usize = 1600;

// ---------------------------------------------------------------------------
// 任务定义类型（suite.json / tasks/*.json 的 schema）
// ---------------------------------------------------------------------------

/// 任务领域分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalCategory {
    Code,
    Research,
    Document,
}

impl EvalCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            EvalCategory::Code => "code",
            EvalCategory::Research => "research",
            EvalCategory::Document => "document",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "code" => Ok(EvalCategory::Code),
            "research" => Ok(EvalCategory::Research),
            "document" => Ok(EvalCategory::Document),
            other => Err(format!(
                "未知分类「{other}」（可选 code/research/document）"
            )),
        }
    }
}

impl fmt::Display for EvalCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 对照拓扑：同一任务定义在两种模式下使用相同的输入/权限/预算/检查器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Single,
    Multi,
}

impl AgentMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Single => "single",
            AgentMode::Multi => "multi",
        }
    }

    /// 矩阵默认拓扑：单/多对照都跑。
    pub fn all() -> [AgentMode; 2] {
        [AgentMode::Single, AgentMode::Multi]
    }

    pub fn parse_list(spec: &str) -> Result<Vec<AgentMode>, String> {
        let mut modes = Vec::new();
        for part in spec.split(',') {
            match part.trim() {
                "single" => modes.push(AgentMode::Single),
                "multi" => modes.push(AgentMode::Multi),
                other => return Err(format!("未知 Agent 拓扑「{other}」（可选 single/multi）")),
            }
        }
        if modes.is_empty() {
            return Err("Agent 拓扑列表为空".to_string());
        }
        Ok(modes)
    }
}

impl fmt::Display for AgentMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 套件级默认值（任务级字段可逐项覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteDefaults {
    /// 默认重复次数（V1 目标：每任务 ×20）。
    #[serde(default = "default_repetitions")]
    pub repetitions: u32,
    /// 默认单次运行超时（秒）。
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// 默认单次运行模型调用预算。
    #[serde(default = "default_max_model_calls")]
    pub max_model_calls: u32,
}

impl Default for SuiteDefaults {
    fn default() -> Self {
        Self {
            repetitions: default_repetitions(),
            timeout_secs: default_timeout_secs(),
            max_model_calls: default_max_model_calls(),
        }
    }
}

fn default_repetitions() -> u32 {
    20
}
fn default_timeout_secs() -> u64 {
    180
}
fn default_max_model_calls() -> u32 {
    6
}

/// 输入 fixture：执行前写入沙盒工作区的文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputFixture {
    pub path: String,
    pub content: String,
}

/// 自动检查器：对一个已完成运行的沙盒产物做判定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArtifactChecker {
    /// 文件必须存在且非空。
    Exists { path: String },
    /// 文件内容必须包含子串。
    Contains { path: String, text: String },
    /// 文件内容不得包含子串。
    NotContains { path: String, text: String },
    /// 文件内容必须匹配正则（Rust regex 语法）。
    Regex { path: String, pattern: String },
    /// 非空行数下限。
    LineCountMin { path: String, min_lines: usize },
    /// JSON 顶层字段等值断言。
    JsonFieldEquals {
        path: String,
        field: String,
        expected: serde_json::Value,
    },
    /// JSON 顶层字段必须**恰好**为该集合（顺序无关）：多余字段或缺失字段均失败。
    /// 用于「只输出一个 JSON 对象，不得包含额外字段」这类硬约束。
    JsonKeysExact { path: String, keys: Vec<String> },
    /// 行为检查：在沙盒内以 `cmd /C <command>` 执行命令（cwd 相对沙盒根），
    /// 断言退出码与 stdout。用于「代码任务检查真实行为和产物」而非只查关键词。
    ///
    /// 执行语义：
    /// - 仅在**真实沙盒目录**评估时执行（内存快照/validate 自检静默跳过，静态校验命令形态）；
    /// - 命令是任务作者声明的固定命令（非 Agent 输入），白名单式无 shell 链式元字符；
    /// - 自带超时与输出截断，失败即该检查器失败。
    CommandCheck {
        command: String,
        /// 相对沙盒根的执行目录（None = 沙盒根）。
        #[serde(default)]
        cwd: Option<String>,
        /// 期望退出码；缺省 Some(0)（None = 不校验退出码）。
        #[serde(default = "default_expect_zero")]
        expect_exit: Option<i32>,
        /// stdout 必须包含的片段。
        #[serde(default)]
        stdout_contains: Vec<String>,
        /// stdout 不得包含的片段。
        #[serde(default)]
        stdout_not_contains: Vec<String>,
        /// 命令超时（秒），缺省 30。
        #[serde(default = "default_command_timeout_secs")]
        timeout_secs: u32,
    },
}

fn default_expect_zero() -> Option<i32> {
    Some(0)
}

fn default_command_timeout_secs() -> u32 {
    30
}

impl ArtifactChecker {
    pub fn describe(&self) -> String {
        match self {
            ArtifactChecker::Exists { path } => format!("exists({path})"),
            ArtifactChecker::Contains { path, text } => {
                format!("contains({path}, {:?})", truncate_for_log(text, 40))
            }
            ArtifactChecker::NotContains { path, text } => {
                format!("not_contains({path}, {:?})", truncate_for_log(text, 40))
            }
            ArtifactChecker::Regex { path, pattern } => {
                format!("regex({path}, {:?})", truncate_for_log(pattern, 60))
            }
            ArtifactChecker::LineCountMin { path, min_lines } => {
                format!("line_count_min({path}, ≥{min_lines})")
            }
            ArtifactChecker::JsonFieldEquals {
                path,
                field,
                expected,
            } => format!("json_field({path}.{field} == {expected})"),
            ArtifactChecker::JsonKeysExact { path, keys } => {
                format!("json_keys_exact({path}, {{{}}})", keys.join(","))
            }
            ArtifactChecker::CommandCheck { command, .. } => {
                format!("command_check({})", truncate_for_log(command, 60))
            }
        }
    }

    /// 静态校验（编译正则、路径合法、数值范围）。
    pub fn static_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let path = match self {
            ArtifactChecker::Exists { path }
            | ArtifactChecker::Contains { path, .. }
            | ArtifactChecker::NotContains { path, .. }
            | ArtifactChecker::Regex { path, .. }
            | ArtifactChecker::LineCountMin { path, .. }
            | ArtifactChecker::JsonFieldEquals { path, .. }
            | ArtifactChecker::JsonKeysExact { path, .. } => Some(path.clone()),
            ArtifactChecker::CommandCheck { .. } => None,
        };
        if let Some(path) = path {
            if let Err(err) = sanitize_rel_path(&path) {
                issues.push(format!("检查器路径非法「{path}」：{err}"));
            }
        }
        match self {
            ArtifactChecker::Exists { .. } => {}
            ArtifactChecker::Contains { text, .. } | ArtifactChecker::NotContains { text, .. } => {
                if text.is_empty() {
                    issues.push("包含断言的 text 为空".to_string());
                }
            }
            ArtifactChecker::Regex { pattern, .. } => {
                if let Err(err) = Regex::new(pattern) {
                    issues.push(format!("正则无效 {pattern:?}：{err}"));
                }
            }
            ArtifactChecker::LineCountMin { min_lines, .. } => {
                if *min_lines == 0 {
                    issues.push("line_count_min 的 min_lines 必须 ≥ 1".to_string());
                }
            }
            ArtifactChecker::JsonFieldEquals { field, .. } => {
                if field.is_empty() {
                    issues.push("json_field 断言缺少 field 名".to_string());
                }
            }
            ArtifactChecker::JsonKeysExact { keys, .. } => {
                if keys.is_empty() {
                    issues.push("json_keys_exact 的 keys 为空：至少声明一个字段".to_string());
                }
                for key in keys {
                    if key.trim().is_empty() {
                        issues.push("json_keys_exact 的 keys 存在空字段名".to_string());
                    }
                }
            }
            ArtifactChecker::CommandCheck {
                command,
                cwd,
                timeout_secs,
                ..
            } => {
                let command = command.trim();
                if command.is_empty() {
                    issues.push("command_check 的 command 为空".to_string());
                } else if command_has_chain_metachars(command) {
                    issues.push(format!(
                        "command_check 的命令「{command}」含链式/重定向元字符（检查命令按单命令收口）"
                    ));
                } else if command.contains("  ") {
                    issues.push(format!(
                        "command_check 的命令「{command}」含连续空格（命令须为单条简单调用）"
                    ));
                }
                if let Some(cwd) = cwd {
                    if let Err(err) = sanitize_rel_path(cwd) {
                        issues.push(format!("command_check 的 cwd 非法：{err}"));
                    }
                }
                if !(1..=120).contains(timeout_secs) {
                    issues.push(format!(
                        "command_check 的 timeout_secs={timeout_secs} 超出 [1,120]"
                    ));
                }
            }
        }
        issues
    }
}

/// 命令链式/重定向元字符检测（与 [`single_agent::command_has_shell_metachars`] 同口径，
/// 供 command_check 静态校验使用：检查命令必须是单条简单调用）。
pub fn command_has_chain_metachars(command: &str) -> bool {
    command
        .chars()
        .any(|c| matches!(c, '&' | '|' | ';' | '<' | '>' | '`' | '\n' | '\r'))
        || command.contains("$(")
}

/// 一个固定产品评测任务（tasks/*.json 的顶层结构）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductEvalCase {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// 全局唯一 ID（kebab-case）。
    pub id: String,
    pub category: EvalCategory,
    pub title: String,
    /// 下发给执行代理的任务说明（live 模式的用户提示词主体）。
    pub instruction: String,
    /// 输入 fixture：执行前写入沙盒。
    #[serde(default)]
    pub inputs: Vec<InputFixture>,
    /// 允许读取范围（相对路径 glob 片段）；inputs 必须全部落在其中。
    #[serde(default)]
    pub allow_read: Vec<String>,
    /// 允许写入范围；expected_artifacts 必须全部落在其中。默认 deny。
    #[serde(default)]
    pub allow_write: Vec<String>,
    /// 预期 Artifact（相对路径），必须在 allow_write 内。
    pub expected_artifacts: Vec<String>,
    /// 自动检查器集合。
    pub checkers: Vec<ArtifactChecker>,
    /// 参考输出：dry 回放模式写入这些内容用于自检检查器可达性。
    #[serde(default)]
    pub reference_outputs: BTreeMap<String, String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub max_model_calls: Option<u32>,
    #[serde(default)]
    pub repetitions: Option<u32>,
    /// 命令白名单（run_command 受控执行）：整条命令需精确或前缀命中其一。
    /// 空列表 = 本任务禁止执行任何命令（默认 deny）。
    #[serde(default)]
    pub allow_commands: Vec<String>,
}

fn default_schema_version() -> u32 {
    PRODUCT_EVAL_SCHEMA_VERSION
}

impl ProductEvalCase {
    /// 生效重复次数：CLI 覆盖 > 任务级 > 套件默认。
    pub fn effective_repetitions(
        &self,
        defaults: &SuiteDefaults,
        cli_override: Option<u32>,
    ) -> u32 {
        cli_override
            .or(self.repetitions)
            .unwrap_or(defaults.repetitions)
            .clamp(1, 100)
    }

    pub fn effective_timeout_secs(&self, defaults: &SuiteDefaults) -> u64 {
        self.timeout_secs.unwrap_or(defaults.timeout_secs).max(1)
    }

    pub fn effective_max_model_calls(&self, defaults: &SuiteDefaults) -> u32 {
        self.max_model_calls
            .unwrap_or(defaults.max_model_calls)
            .clamp(1, 20)
    }
}

/// 套件（suite.json）。tasks 为相对本文件的 JSON 任务路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductEvalSuite {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub defaults: SuiteDefaults,
    pub tasks: Vec<String>,
}

/// 套件与其所在目录（解析 tasks 相对路径的锚点）。
#[derive(Debug, Clone)]
pub struct SuiteBundle {
    pub dir: PathBuf,
    pub suite: ProductEvalSuite,
    pub cases: Vec<ProductEvalCase>,
}

// ---------------------------------------------------------------------------
// 运行记录 / 报告 / 指标
// ---------------------------------------------------------------------------

/// 单次重复运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Passed,
    Failed,
    Error,
    Timeout,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Passed => "passed",
            RunStatus::Failed => "failed",
            RunStatus::Error => "error",
            RunStatus::Timeout => "timeout",
            RunStatus::Cancelled => "cancelled",
        }
    }
}

/// 矩阵中的一个单元格：`(case_id, agent_mode, repetition)`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MatrixKey {
    pub case_id: String,
    pub agent_mode: AgentMode,
    pub repetition: u32,
}

impl MatrixKey {
    pub fn new(case_id: impl Into<String>, agent_mode: AgentMode, repetition: u32) -> Self {
        Self {
            case_id: case_id.into(),
            agent_mode,
            repetition,
        }
    }

    pub fn slug(&self) -> String {
        format!(
            "{}#{}#{}",
            self.case_id,
            self.agent_mode.as_str(),
            self.repetition
        )
    }
}

impl fmt::Display for MatrixKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.slug())
    }
}

/// 一次运行的完整记录（journal 的最小单元；失败记录同样永久保留）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductEvalRun {
    pub key: MatrixKey,
    pub category: EvalCategory,
    pub status: RunStatus,
    pub wall_ms: u64,
    pub model_calls: u32,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    /// 失败步骤（检查器描述 / 执行器阶段名）。
    pub failed_steps: Vec<String>,
    pub retries: u32,
    pub cancellations: u32,
    /// 最终 Artifact 引用（沙盒内相对路径）。
    pub artifact_refs: Vec<String>,
    /// 真实工具调用轨迹（单 Agent 执行器填写：工具 + 实参摘要 + 结果；旧记录缺省为空）。
    #[serde(default)]
    pub tool_log: Vec<String>,
    /// 通过/总检查器数（质量分 = passed/total；0/0 = 无质量维度）。
    #[serde(default)]
    pub checker_passed: u32,
    #[serde(default)]
    pub checker_total: u32,
    /// 沙盒/失败留档目录相对 out 根的位置（成功运行会即时清理，故为 None）。
    #[serde(default)]
    pub sandbox_rel: Option<String>,
    pub model: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub error: Option<String>,
}

/// 聚合指标。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProductEvalMetrics {
    pub runs_total: usize,
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub timeouts: usize,
    pub cancelled: usize,
    /// 成功率分母为全部已尝试运行（失败/错误/超时一律计入，禁止剔除重算）。
    pub success_rate: f64,
    pub mean_wall_ms: f64,
    pub total_model_calls: u64,
    pub total_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
}

/// 按 (case_id, mode) 分组的细分统计。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseModeMetrics {
    pub case_id: String,
    pub category: EvalCategory,
    pub agent_mode: AgentMode,
    pub runs_total: usize,
    pub passed: usize,
    pub success_rate: f64,
    pub mean_wall_ms: f64,
    pub mean_model_calls: f64,
    pub total_tokens: Option<u64>,
    /// 检查器通过率均值（质量代理；无检查器计数的旧记录缺省为 None）。
    #[serde(default)]
    pub quality: Option<f64>,
}

/// 完整报告：聚合**全部** journal 记录（含失败）+ 未完成单元格清单。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductEvalReport {
    pub schema_version: u32,
    pub suite_name: String,
    pub suite_hash: String,
    pub execution: String,
    pub model: Option<String>,
    /// 批次标签（正式验收批次隔离用，同目录只允许同一批次）。
    #[serde(default)]
    pub batch_label: Option<String>,
    /// 附加标签（CLI --tag 可重复；进报告供溯源）。
    #[serde(default)]
    pub tags: Vec<String>,
    pub generated_at: String,
    pub runs: Vec<ProductEvalRun>,
    /// 计划中但尚未完成的矩阵单元格（中断续跑的目标集）。
    pub pending: Vec<MatrixKey>,
    pub metrics: ProductEvalMetrics,
    pub per_case: Vec<CaseModeMetrics>,
}

/// out 目录元信息（续跑兼容性校验）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunDirMeta {
    schema_version: u32,
    suite_name: String,
    suite_hash: String,
    model: Option<String>,
    #[serde(default)]
    batch_label: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    created_at: String,
}

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// ProductEval 顶层错误（消息已含上下文，直接面向 CLI 展示）。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ProductEvalError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, ProductEvalError> {
    Err(ProductEvalError(msg.into()))
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn truncate_for_log(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out.replace('\n', "\\n")
}

// ---------------------------------------------------------------------------
// 路径工具 / 权限范围
// ---------------------------------------------------------------------------

/// 归一化并校验相对路径：拒绝绝对路径、`..`、空段。
pub fn sanitize_rel_path(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("路径为空".to_string());
    }
    if raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':') {
        return Err(format!("不允许绝对路径「{raw}」"));
    }
    let normalized = raw.replace('\\', "/");
    let mut cleaned: Vec<&str> = Vec::new();
    for seg in normalized.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(format!("路径包含「..」越界：「{raw}」")),
            _ => cleaned.push(seg),
        }
    }
    if cleaned.is_empty() {
        return Err(format!("路径规范化后为空：「{raw}」"));
    }
    Ok(cleaned.join("/"))
}

/// 单段通配匹配：`*` 匹配段内任意字符，`?` 匹配单个字符。
fn segment_matches(pattern: &str, text: &str) -> bool {
    let pc: Vec<char> = pattern.chars().collect();
    let tc: Vec<char> = text.chars().collect();
    segment_match_inner(&pc, &tc)
}

fn segment_match_inner(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' => {
            for skip in 0..=t.len() {
                if segment_match_inner(&p[1..], &t[skip..]) {
                    return true;
                }
            }
            false
        }
        '?' => !t.is_empty() && segment_match_inner(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && segment_match_inner(&p[1..], &t[1..]),
    }
}

/// 判断相对路径是否落在某条 scope 规则内：
/// - 支持段级通配 `*` / `?` 与目录尾缀 `/**`；
/// - 无通配的模式按"目录前缀"语义覆盖其全部子路径（`out` ⊇ `out/a.md`）。
pub fn scope_matches(pattern: &str, rel: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() || rel.is_empty() {
        return false;
    }
    if pattern == "**" || pattern == "**/*" {
        return true;
    }
    let pat_norm = pattern.replace('\\', "/");
    let pat = pat_norm.trim_end_matches('/');
    if rel == pat {
        return true;
    }
    // 目录前缀语义（仅对无通配的纯目录模式）。
    if !pat.contains('*') && !pat.contains('?') && rel.starts_with(&(pat.to_string() + "/")) {
        return true;
    }
    let p_segs: Vec<&str> = pat.split('/').collect();
    let r_segs: Vec<&str> = rel.split('/').collect();
    if p_segs.contains(&"**") {
        return double_star_match(&p_segs, &r_segs);
    }
    if p_segs.len() != r_segs.len() {
        return false;
    }
    p_segs
        .iter()
        .zip(r_segs.iter())
        .all(|(p, r)| segment_matches(p, r))
}

fn double_star_match(p: &[&str], r: &[&str]) -> bool {
    if p.is_empty() {
        return r.is_empty();
    }
    if p[0] == "**" {
        for skip in 0..=r.len() {
            if double_star_match(&p[1..], &r[skip..]) {
                return true;
            }
        }
        return false;
    }
    if r.is_empty() {
        return false;
    }
    segment_matches(p[0], r[0]) && double_star_match(&p[1..], &r[1..])
}

/// 任一规则命中即在范围内。
pub fn in_scope(rules: &[String], rel: &str) -> bool {
    rules.iter().any(|rule| scope_matches(rule, rel))
}

// ---------------------------------------------------------------------------
// 文件来源抽象：让检查器既能跑磁盘也能跑内存快照（validate 自检用）
// ---------------------------------------------------------------------------

pub(crate) trait FileSource {
    fn read_text(&self, rel: &str) -> Option<String>;
    fn exists_nonempty(&self, rel: &str) -> bool;
}

struct FsSource<'a>(&'a Path);

impl FileSource for FsSource<'_> {
    fn read_text(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(rel)).ok()
    }
    fn exists_nonempty(&self, rel: &str) -> bool {
        match std::fs::metadata(self.0.join(rel)) {
            Ok(meta) => meta.is_file() && meta.len() > 0,
            Err(_) => false,
        }
    }
}

struct MapSource<'a>(&'a BTreeMap<String, String>);

impl FileSource for MapSource<'_> {
    fn read_text(&self, rel: &str) -> Option<String> {
        self.0.get(rel).cloned()
    }
    fn exists_nonempty(&self, rel: &str) -> bool {
        self.0
            .get(rel)
            .map(|content| !content.trim().is_empty())
            .unwrap_or(false)
    }
}

/// 对单个检查器求值：Ok(()) 通过，Err(描述) 为失败原因。
/// （公开入口见 [`evaluate_checker_on_map`] / [`evaluate_checker_on_dir`]。）
pub(crate) fn evaluate_checker<S: FileSource>(
    checker: &ArtifactChecker,
    source: &S,
) -> Result<(), String> {
    match checker {
        ArtifactChecker::Exists { path } => {
            if source.exists_nonempty(path) {
                Ok(())
            } else {
                Err(format!("产物缺失或为空：{path}"))
            }
        }
        ArtifactChecker::Contains { path, text } => match source.read_text(path) {
            Some(content) if content.contains(text.as_str()) => Ok(()),
            Some(_) => Err(format!(
                "{path} 缺少期望片段 {:?}",
                truncate_for_log(text, 40)
            )),
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::NotContains { path, text } => match source.read_text(path) {
            Some(content) if !content.contains(text.as_str()) => Ok(()),
            Some(_) => Err(format!(
                "{path} 含禁止片段 {:?}",
                truncate_for_log(text, 40)
            )),
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::Regex { path, pattern } => {
            // 产物是多行文件：`^`/`$` 按行锚定（multi_line），符合"标题必须顶行"类断言的直觉。
            let re = RegexBuilder::new(pattern)
                .multi_line(true)
                .build()
                .map_err(|e| format!("正则无效 {pattern:?}：{e}"))?;
            match source.read_text(path) {
                Some(content) if re.is_match(&content) => Ok(()),
                Some(_) => Err(format!(
                    "{path} 未匹配正则 {:?}",
                    truncate_for_log(pattern, 60)
                )),
                None => Err(format!("产物不存在：{path}")),
            }
        }
        ArtifactChecker::LineCountMin { path, min_lines } => match source.read_text(path) {
            Some(content) => {
                let count = content.lines().filter(|l| !l.trim().is_empty()).count();
                if count >= *min_lines {
                    Ok(())
                } else {
                    Err(format!("{path} 有效行数 {count} < {min_lines}"))
                }
            }
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::JsonFieldEquals {
            path,
            field,
            expected,
        } => match source.read_text(path) {
            Some(content) => {
                let value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| format!("{path} 不是合法 JSON：{e}"))?;
                let actual = value
                    .get(field)
                    .ok_or_else(|| format!("{path} 缺少顶层字段 {field}"))?;
                if actual == expected {
                    Ok(())
                } else {
                    Err(format!("{path}.{field} = {actual}，期望 {expected}"))
                }
            }
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::JsonKeysExact { path, keys } => match source.read_text(path) {
            Some(content) => {
                let value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| format!("{path} 不是合法 JSON：{e}"))?;
                let object = value
                    .as_object()
                    .ok_or_else(|| format!("{path} 顶层必须是 JSON 对象"))?;
                let actual_keys: Vec<&String> = object.keys().collect();
                let mut want: Vec<&String> = keys.iter().collect();
                want.sort();
                let mut actual: Vec<&String> = actual_keys.iter().copied().collect();
                actual.sort();
                if want == actual {
                    Ok(())
                } else {
                    let missing: Vec<&String> = want
                        .iter()
                        .filter(|key| !actual.contains(key))
                        .copied()
                        .collect();
                    let extra: Vec<&String> = actual
                        .iter()
                        .filter(|key| !want.contains(key))
                        .copied()
                        .collect();
                    Err(format!(
                        "{path} 顶层字段集合不符：缺失 {{{}}} 多余 {{{}}}",
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        extra
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            None => Err(format!("产物不存在：{path}")),
        },
        // 行为检查不在文件快照上评估：runner 在真实沙盒上经 [`evaluate_command_check`] 执行。
        ArtifactChecker::CommandCheck { .. } => Ok(()),
    }
}

/// 在真实沙盒目录上执行 command_check（`cmd /C <command>` 单命令收口，自带超时）。
/// 仅对 [`ArtifactChecker::CommandCheck`] 有意义；对其他检查器返回 Err。
pub async fn evaluate_command_check(
    checker: &ArtifactChecker,
    sandbox: &Path,
) -> Result<(), String> {
    let ArtifactChecker::CommandCheck {
        command,
        cwd,
        expect_exit,
        stdout_contains,
        stdout_not_contains,
        timeout_secs,
    } = checker
    else {
        return Err("evaluate_command_check 仅支持 CommandCheck".to_string());
    };
    let base = match cwd {
        Some(raw) => {
            let rel = sanitize_rel_path(raw)?;
            if rel.is_empty() {
                sandbox.to_path_buf()
            } else {
                sandbox.join(rel)
            }
        }
        None => sandbox.to_path_buf(),
    };
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err("command_check 的 command 为空".to_string());
    }
    let command_for_spawn = command.clone();
    let base_for_spawn = base.clone();
    let handle = tokio::task::spawn_blocking(move || {
        std::process::Command::new("cmd")
            .args(["/C", command_for_spawn.as_str()])
            .current_dir(&base_for_spawn)
            .output()
    });
    let output = tokio::time::timeout(Duration::from_secs((*timeout_secs) as u64), handle)
        .await
        .map_err(|_| format!("command_check 超时（{timeout_secs}s）：{command}"))?
        .map_err(|e| format!("command_check 任务失败：{e}"))?
        .map_err(|e| format!("command_check 执行失败（cwd={}）：{e}", base.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let exit_code = output.status.code();
    // 期望退出码：None = 不校验；Some(0) 为缺省。
    if let Some(expected) = expect_exit {
        if exit_code != Some(*expected) {
            return Err(format!(
                "command_check 退出码 {} != 期望 {}（命令：{command}）",
                exit_code.unwrap_or(-1),
                expected
            ));
        }
    }
    for needle in stdout_contains {
        if !stdout.contains(needle.as_str()) {
            return Err(format!(
                "command_check stdout 缺少片段 {:?}（命令：{command}）",
                truncate_for_log(needle, 40)
            ));
        }
    }
    for banned in stdout_not_contains {
        if stdout.contains(banned.as_str()) {
            return Err(format!(
                "command_check stdout 出现禁止片段 {:?}（命令：{command}）",
                truncate_for_log(banned, 40)
            ));
        }
    }
    Ok(())
}

fn evaluate_all<S: FileSource>(checkers: &[ArtifactChecker], source: &S) -> (bool, Vec<String>) {
    let mut failed = Vec::new();
    for checker in checkers {
        if let Err(reason) = evaluate_checker(checker, source) {
            failed.push(format!("{} ⇒ {}", checker.describe(), reason));
        }
    }
    (failed.is_empty(), failed)
}

/// 在真实沙盒上评估全部检查器（command_check 在此真正执行；其余与快照语义一致）。
/// 返回 (通过数, 总数, 失败描述)，供 runner 判定与质量分计算。
pub async fn evaluate_all_on_dir(
    checkers: &[ArtifactChecker],
    sandbox: &Path,
) -> (u32, u32, Vec<String>) {
    let mut failed = Vec::new();
    let mut passed = 0u32;
    let total = checkers.len() as u32;
    for checker in checkers {
        let result = match checker {
            ArtifactChecker::CommandCheck { .. } => evaluate_command_check(checker, sandbox).await,
            _ => evaluate_checker(checker, &FsSource(sandbox)),
        };
        match result {
            Ok(()) => passed += 1,
            Err(reason) => failed.push(format!("{} ⇒ {}", checker.describe(), reason)),
        }
    }
    (passed, total, failed)
}

/// 在内存快照上求值单个检查器（公开入口，供校验/测试复用）。
pub fn evaluate_checker_on_map(
    checker: &ArtifactChecker,
    snapshot: &BTreeMap<String, String>,
) -> Result<(), String> {
    evaluate_checker(checker, &MapSource(snapshot))
}

/// 在磁盘目录上求值单个检查器（公开入口，供校验/测试复用）。
pub fn evaluate_checker_on_dir(checker: &ArtifactChecker, root: &Path) -> Result<(), String> {
    evaluate_checker(checker, &FsSource(root))
}

// ---------------------------------------------------------------------------
// 套件加载 / 校验
// ---------------------------------------------------------------------------

/// 解析 --suite 参数：None 时从 cwd 向上查找 `agent-sdk/evals/v1/suite.json`。
pub fn resolve_suite_input(spec: Option<&Path>) -> Result<PathBuf, ProductEvalError> {
    if let Some(path) = spec {
        return Ok(path.to_path_buf());
    }
    let mut current =
        std::env::current_dir().map_err(|e| ProductEvalError(format!("无法获取当前目录：{e}")))?;
    for _ in 0..8 {
        let candidate = current
            .join("agent-sdk")
            .join("evals")
            .join("v1")
            .join("suite.json");
        if candidate.is_file() {
            return Ok(candidate);
        }
        let nested = current.join("evals").join("v1").join("suite.json");
        if nested.is_file() {
            return Ok(nested);
        }
        if !current.pop() {
            break;
        }
    }
    err("未找到套件：请用 --suite 指定 suite.json（缺省查找 agent-sdk/evals/v1/suite.json）")
}

/// 从 suite.json 加载套件及全部任务定义。
pub fn load_suite(suite_spec: &Path) -> Result<SuiteBundle, ProductEvalError> {
    let suite_file = if suite_spec.is_dir() {
        suite_spec.join("suite.json")
    } else {
        suite_spec.to_path_buf()
    };
    if !suite_file.is_file() {
        return err(format!("suite.json 不存在：{}", suite_file.display()));
    }
    let dir = suite_file
        .parent()
        .ok_or_else(|| ProductEvalError("suite.json 缺少父目录".into()))?
        .to_path_buf();
    let text = std::fs::read_to_string(&suite_file)
        .map_err(|e| ProductEvalError(format!("读取 {} 失败：{e}", suite_file.display())))?;
    let suite: ProductEvalSuite = serde_json::from_str(&text).map_err(|e| {
        ProductEvalError(format!(
            "解析 {} 失败（schema 不符或字段拼写错误）：{e}",
            suite_file.display()
        ))
    })?;
    if suite.schema_version != PRODUCT_EVAL_SCHEMA_VERSION {
        return err(format!(
            "套件 schema 版本不匹配：期望 {PRODUCT_EVAL_SCHEMA_VERSION}，实际 {}",
            suite.schema_version
        ));
    }
    if suite.name.trim().is_empty() {
        return err("套件 name 不能为空");
    }
    if suite.tasks.is_empty() {
        return err("套件 tasks 为空");
    }
    let mut cases = Vec::new();
    for rel in &suite.tasks {
        let task_path = dir.join(rel);
        let task_text = std::fs::read_to_string(&task_path)
            .map_err(|e| ProductEvalError(format!("读取任务 {rel} 失败：{e}")))?;
        let case: ProductEvalCase = serde_json::from_str(&task_text)
            .map_err(|e| ProductEvalError(format!("解析任务 {rel} 失败（schema 不符）：{e}")))?;
        cases.push(case);
    }
    Ok(SuiteBundle { dir, suite, cases })
}

/// 单任务校验；`seen_ids` 用于跨任务查重。
pub fn validate_case(
    case: &ProductEvalCase,
    seen_ids: &mut std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut issues = Vec::new();

    if case.schema_version != PRODUCT_EVAL_SCHEMA_VERSION {
        issues.push(format!(
            "schema_version 期望 {PRODUCT_EVAL_SCHEMA_VERSION}，实际 {}",
            case.schema_version
        ));
    }
    let id_ok = !case.id.is_empty()
        && case
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !id_ok {
        issues.push(format!("id「{}」必须是非空 kebab-case", case.id));
    }
    if !seen_ids.insert(case.id.clone()) {
        issues.push(format!("id「{}」在套件内重复", case.id));
    }
    if case.title.trim().is_empty() {
        issues.push("title 为空".to_string());
    }
    if case.instruction.trim().is_empty() {
        issues.push("instruction 为空".to_string());
    }

    // 产物与写入范围。
    if case.expected_artifacts.is_empty() {
        issues.push("expected_artifacts 为空：至少声明一个预期 Artifact".to_string());
    }
    let mut normalized_artifacts = Vec::new();
    for artifact in &case.expected_artifacts {
        match sanitize_rel_path(artifact) {
            Ok(norm) => {
                if in_scope(&case.allow_write, &norm) {
                    if !normalized_artifacts.contains(&norm) {
                        normalized_artifacts.push(norm);
                    }
                } else {
                    issues.push(format!(
                        "预期 Artifact「{artifact}」不在 allow_write 范围内（权限默认 deny）"
                    ));
                }
            }
            Err(e) => issues.push(format!("预期 Artifact「{artifact}」非法：{e}")),
        }
    }
    if case.allow_write.is_empty() {
        issues.push("allow_write 为空：未授予任何写范围".to_string());
    }

    // 命令白名单：条目必须非空且不得内嵌 shell 链式元字符（执行器同规则逐条强制）。
    for command in &case.allow_commands {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            issues.push("allow_commands 存在空白条目".to_string());
        } else if single_agent::command_has_shell_metachars(trimmed) {
            issues.push(format!(
                "allow_commands 条目「{trimmed}」含链式/重定向元字符（白名单按单命令收口）"
            ));
        }
    }

    // 输入与读取范围。
    for input in &case.inputs {
        match sanitize_rel_path(&input.path) {
            Ok(norm) => {
                if !in_scope(&case.allow_read, &norm) {
                    issues.push(format!(
                        "输入 fixture「{}」不在 allow_read 范围内",
                        input.path
                    ));
                }
            }
            Err(e) => issues.push(format!("输入 fixture「{}」非法：{e}", input.path)),
        }
    }

    // 检查器。
    if case.checkers.is_empty() {
        issues.push("checkers 为空：至少需要一个自动检查器".to_string());
    }
    // 检查器目标可达性：预期产物、输入 fixture 或参考输出均可（行为/内容检查可指向
    // 修复后的源文件——如代码任务验证 src/ 的修改结果）。
    let mut known_files: std::collections::BTreeSet<String> =
        normalized_artifacts.iter().cloned().collect();
    for fixture in &case.inputs {
        if let Ok(norm) = sanitize_rel_path(&fixture.path) {
            known_files.insert(norm);
        }
    }
    for key in case.reference_outputs.keys() {
        if let Ok(norm) = sanitize_rel_path(key) {
            known_files.insert(norm);
        }
    }
    for checker in &case.checkers {
        issues.extend(checker.static_issues());
        let path = match checker {
            ArtifactChecker::Exists { path }
            | ArtifactChecker::Contains { path, .. }
            | ArtifactChecker::NotContains { path, .. }
            | ArtifactChecker::Regex { path, .. }
            | ArtifactChecker::LineCountMin { path, .. }
            | ArtifactChecker::JsonFieldEquals { path, .. }
            | ArtifactChecker::JsonKeysExact { path, .. } => Some(path.clone()),
            ArtifactChecker::CommandCheck { .. } => None,
        };
        if let Some(path) = path {
            if let Ok(norm) = sanitize_rel_path(&path) {
                if !known_files.contains(&norm) {
                    issues.push(format!(
                        "检查器目标「{path}」不在 expected_artifacts / 输入 fixture / 参考输出 中"
                    ));
                }
            }
        }
    }

    // 参考输出覆盖所有预期产物，且参考输出必须通过全部检查器（dry 可达性自检）。
    for artifact in &normalized_artifacts {
        if !case.reference_outputs.contains_key(artifact) {
            issues.push(format!("reference_outputs 缺少预期产物「{artifact}」"));
        }
    }
    let mut refs = case.reference_outputs.clone();
    for fixture in &case.inputs {
        if let Ok(norm) = sanitize_rel_path(&fixture.path) {
            // 参考输出优先（代表"修复后/完成后"状态）；fixture 只在参考未覆盖时补位。
            refs.entry(norm).or_insert_with(|| fixture.content.clone());
        }
    }
    let source = MapSource(&refs);
    let (_, failed) = evaluate_all(&case.checkers, &source);
    for reason in failed {
        issues.push(format!("参考输出未通过检查器：{reason}"));
    }

    // 数值范围。
    if let Some(reps) = case.repetitions {
        if !(1..=100).contains(&reps) {
            issues.push(format!("repetitions={reps} 超出 [1,100]"));
        }
    }
    if let Some(secs) = case.timeout_secs {
        if secs < 5 {
            issues.push(format!("timeout_secs={secs} 过小（≥5）"));
        }
    }
    if let Some(calls) = case.max_model_calls {
        if calls == 0 || calls > 20 {
            issues.push(format!("max_model_calls={calls} 超出 [1,20]"));
        }
    }

    issues
}

/// 全套件校验（含跨任务查重）。
pub fn validate_suite(bundle: &SuiteBundle) -> SuiteValidation {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut tasks = Vec::new();
    for case in &bundle.cases {
        let issues = validate_case(case, &mut seen);
        let ok = issues.is_empty();
        tasks.push(TaskValidation {
            id: case.id.clone(),
            ok,
            issues,
        });
    }
    let ok_count = tasks.iter().filter(|task| task.ok).count();
    SuiteValidation {
        suite_name: bundle.suite.name.clone(),
        task_count: tasks.len(),
        ok_count,
        all_ok: ok_count == tasks.len(),
        tasks,
    }
}

/// 单任务校验结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskValidation {
    pub id: String,
    pub ok: bool,
    pub issues: Vec<String>,
}

/// 整套校验结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuiteValidation {
    pub suite_name: String,
    pub task_count: usize,
    pub ok_count: usize,
    pub all_ok: bool,
    pub tasks: Vec<TaskValidation>,
}

/// 套件指纹：对全部任务定义做规范序列化摘要，作为续跑兼容性凭据。
pub fn suite_hash(bundle: &SuiteBundle) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bundle.suite.name.as_bytes());
    for case in &bundle.cases {
        let canonical = serde_json::to_vec(case).unwrap_or_else(|_| case.id.as_bytes().to_vec());
        hasher.update(&canonical);
    }
    format!("{:x}", hasher.finalize())
}

/// 校验结果的人类可读清单（validate 子命令输出）。
pub fn format_validation(validation: &SuiteValidation) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "套件「{}」任务校验：{}/{} 通过\n",
        validation.suite_name, validation.ok_count, validation.task_count
    ));
    for task in &validation.tasks {
        let mark = if task.ok { "PASS" } else { "FAIL" };
        out.push_str(&format!("  [{mark}] {}\n", task.id));
        for issue in &task.issues {
            out.push_str(&format!("        - {issue}\n"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 执行器契约
// ---------------------------------------------------------------------------

/// 执行器可见的单次运行上下文。写入统一走 [`ExecContext::write_file`]，
/// 强制 allow_write 范围（权限默认 deny），并自动登记 Artifact 引用与失败步骤。
pub struct ExecContext<'ctx> {
    pub case: &'ctx ProductEvalCase,
    pub sandbox: &'ctx Path,
    pub mode: AgentMode,
    pub timeout_secs: u64,
    pub max_model_calls: u32,
    /// 外部取消令牌（Ctrl-C 等）；执行器应在合适时机检查并尽快返回 aborted。
    pub cancel: Arc<AtomicBool>,
    artifact_refs: Vec<String>,
    failed_steps: Vec<String>,
}

impl<'ctx> ExecContext<'ctx> {
    pub(crate) fn new(
        case: &'ctx ProductEvalCase,
        sandbox: &'ctx Path,
        mode: AgentMode,
        timeout_secs: u64,
        max_model_calls: u32,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Self {
            case,
            sandbox,
            mode,
            timeout_secs,
            max_model_calls,
            cancel,
            artifact_refs: Vec::new(),
            failed_steps: Vec::new(),
        }
    }

    pub fn record_failed_step(&mut self, step: impl Into<String>) {
        self.failed_steps.push(step.into());
    }

    /// 直接登记一个已落盘的产物引用（执行器内 Agent 经工具写入磁盘后，
    /// 由执行器清点沙盒并调用本方法补录；与 write_file 的登记口径一致）。
    pub(crate) fn record_artifact(&mut self, rel: impl Into<String>) {
        let rel = rel.into();
        if !self.artifact_refs.contains(&rel) {
            self.artifact_refs.push(rel);
        }
    }

    /// 取消命中检测。
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// 沙盒内受控写入：路径合法性 + allow_write 范围强制。
    /// 失败时自动登记失败步骤并返回 Err。
    pub fn write_file(&mut self, raw_path: &str, content: &str) -> Result<(), String> {
        let rel = sanitize_rel_path(raw_path)?;
        if !in_scope(&self.case.allow_write, &rel) {
            let desc = format!("write_denied:{raw_path}（超出 allow_write 范围）");
            self.failed_steps.push(desc.clone());
            return Err(desc);
        }
        let target = self.sandbox.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
        std::fs::write(&target, content)
            .map_err(|e| format!("写入 {} 失败：{e}", target.display()))?;
        if !self.artifact_refs.contains(&rel) {
            self.artifact_refs.push(rel);
        }
        Ok(())
    }

    fn take_records(&mut self) -> (Vec<String>, Vec<String>) {
        (
            std::mem::take(&mut self.artifact_refs),
            std::mem::take(&mut self.failed_steps),
        )
    }
}

/// 执行器原始产出；状态由 Runner 与检查器共同合成。
#[derive(Debug, Clone, Default)]
pub struct RawExecOutcome {
    /// 执行器观察到取消令牌提前退出。
    pub aborted: bool,
    /// 执行器层面的错误（None = 执行流程完成，成败交由检查器裁决）。
    pub error: Option<String>,
    pub model_calls: u32,
    pub usage: TokenUsage,
    /// Provider 是否回报了可用用量（否则 token 字段落盘为 null）。
    pub usage_known: bool,
    /// 内部重试次数（如多 Agent 复审返工）。
    pub retries: u32,
    /// 真实工具调用轨迹（单 Agent 执行器填写；其余执行器为空）。
    pub tool_log: Vec<String>,
}

/// 用例执行器契约。dry 参考回放 / live 网关生成共用此契约，
/// 因此单/多 Agent 天然共享同一份任务、输入、权限、预算与检查器。
#[async_trait]
pub trait CaseExecutor: Send + Sync {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome;
}

// ---------------------------------------------------------------------------
// dry 执行器：参考输出回放（零模型调用，本地确定性可重复）
// ---------------------------------------------------------------------------

/// 参考输出回放执行器：把任务内置的 reference_outputs 写入沙盒后交检查器判定。
/// 若某个预期产物没有参考内容，判执行器错误——这是 harness 自检信号。
pub struct ReferenceDryExecutor;

#[async_trait]
impl CaseExecutor for ReferenceDryExecutor {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        let mut outcome = RawExecOutcome::default();
        // 回放**全部**参考输出（不只预期产物）：代码任务的"修复后源文件"也在其中，
        // 行为 command_check 才能对参考状态真正执行（dry = 完整 harness 自检）。
        for (path, content) in &ctx.case.reference_outputs {
            if let Err(write_err) = ctx.write_file(path, content) {
                outcome.error = Some(format!("dry 回放写入失败：{write_err}"));
                return outcome;
            }
        }
        // 参考输出必须覆盖全部预期产物（否则检查器无可判定对象）。
        for artifact in &ctx.case.expected_artifacts {
            if !ctx.case.reference_outputs.contains_key(artifact) {
                outcome.error = Some(format!(
                    "dry 回放缺少参考输出：{artifact}（harness 自检失败）"
                ));
                return outcome;
            }
        }
        outcome
    }
}

// ---------------------------------------------------------------------------
// live 执行器：经网关生成的文件块协议
// ---------------------------------------------------------------------------

const OUTPUT_FORMAT_CONTRACT: &str = "=== FILE: 相对路径 ===\n<文件内容，可多行>\n=== END FILE ===";

const SYSTEM_FILE_WRITER: &str = "你是产品评测中的执行代理。你的全部回复只能是若干个文件块，格式严格如下（不要 markdown 围栏、不要解释文字）：\n=== FILE: 相对路径 ===\n<文件内容>\n=== END FILE ===\n可以连续输出多个文件块。只允许写出任务声明的预期产物路径。";

const SYSTEM_PLANNER: &str =
    "你是多 Agent 流水线中的规划者。针对给定任务输出一份不超过 10 条要点的执行计划，不要输出文件内容本身。";

const SYSTEM_REVIEWER: &str = "你是多 Agent 流水线中的评审者。审阅执行者给出的文件草稿：若需返工，回复以 REVISE: 开头并附具体修改意见；若无问题，只回复 APPROVED。不要输出其他内容。";

/// 从 Provider 文本回复中解析 FILE 块；容忍 markdown 围栏包裹。
/// 返回 (blocks, warnings)；warning 描述被丢弃的残块。
pub fn parse_file_blocks(text: &str) -> (Vec<(String, String)>, Vec<String>) {
    let mut blocks = Vec::new();
    let mut warnings = Vec::new();
    let mut current_path: Option<String> = None;
    let mut buffer: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("===") {
            // 完整围栏形如 `=== FILE: 路径 ===` / `=== END FILE ===`：去掉行尾的 "===" 再比对。
            let inner = rest.trim();
            let inner = inner.strip_suffix("===").map(str::trim).unwrap_or(inner);
            if let Some(path_part) = inner.strip_prefix("FILE:") {
                if let Some(open) = &current_path {
                    warnings.push(format!(
                        "嵌套 FILE 开始标记（丢弃残块 {}）",
                        truncate_for_log(open, 40)
                    ));
                }
                current_path = Some(path_part.trim().trim_matches('`').trim().to_string());
                buffer.clear();
                continue;
            }
            if inner == "END FILE" {
                if let Some(path) = current_path.take() {
                    blocks.push((path, buffer.join("\n")));
                    buffer.clear();
                } else {
                    warnings.push("孤立的 END FILE 标记".to_string());
                }
                continue;
            }
        }
        if current_path.is_some() {
            buffer.push(line.to_string());
        }
    }
    if let Some(path) = current_path {
        warnings.push(format!(
            "未闭合的文件块（{}），内容已丢弃",
            truncate_for_log(&path, 40)
        ));
    }
    (blocks, warnings)
}

fn cap_text(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("\n…（截断）");
    }
    out
}

fn messages_for_generation(
    case: &ProductEvalCase,
    extra_role_context: Option<&str>,
) -> Vec<ChatMessage> {
    let mut user = String::new();
    user.push_str("# 任务\n");
    user.push_str(&format!("[{}] {}\n", case.category.as_str(), case.title));
    user.push_str(case.instruction.trim());
    user.push('\n');
    user.push_str("\n# 必须产出的 Artifact（相对路径）\n");
    for artifact in &case.expected_artifacts {
        user.push_str(&format!("- {artifact}\n"));
    }
    if !case.inputs.is_empty() {
        user.push_str("\n# 输入文件（已就绪，仅供阅读）\n");
        for input in &case.inputs {
            user.push_str(&format!(
                "--- {} ---\n{}\n--- 结束 {} ---\n",
                input.path,
                cap_text(&input.content, PROMPT_TEXT_CAP),
                input.path
            ));
        }
    }
    if let Some(context) = extra_role_context {
        user.push_str("\n# 协作者上下文\n");
        user.push_str(context);
        user.push('\n');
    }
    user.push_str(&format!("\n# 输出格式\n{OUTPUT_FORMAT_CONTRACT}\n"));
    vec![
        ChatMessage::system(SYSTEM_FILE_WRITER.to_string()),
        ChatMessage::user(user),
    ]
}

async fn chat_text(
    provider: &dyn ModelProvider,
    model: &str,
    messages: &[ChatMessage],
) -> Result<String, String> {
    match provider.complete(messages, &[]).await? {
        ModelOutput::Text(text) => {
            if text.trim().is_empty() {
                Err(format!("模型 {model} 返回空文本"))
            } else {
                Ok(text)
            }
        }
        ModelOutput::ToolCalls(_) => Err("评测生成器不接受工具调用回复".to_string()),
    }
}

/// 单次 live 运行的内部状态。
struct LiveState {
    model_calls: u32,
    session_usage: TokenUsage,
    usage_before: TokenUsage,
    plan: String,
    draft: BTreeMap<String, String>,
}

/// live 生成式执行器：经网关产出文件块 → 受控落盘 → 检查器判定。
///
/// - single：一次调用直出全部文件块；
/// - multi：规划者 → 执行者 → 评审者（必要时一轮返工）流水线；
/// - 预算：超过 max_model_calls 即报错终止；
/// - 取消：每次模型调用之间检查取消令牌。
pub struct GenerativeExecutor {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
}

impl GenerativeExecutor {
    async fn call_counted(
        &self,
        state: &mut LiveState,
        budget: u32,
        messages: &[ChatMessage],
    ) -> Result<String, String> {
        if state.model_calls >= budget {
            return Err(format!("模型调用预算耗尽：已达 max_model_calls={budget}"));
        }
        state.model_calls += 1;
        let output = chat_text(self.provider.as_ref(), &self.model, messages).await?;
        let snapshot = self.provider.usage_snapshot();
        let delta = TokenUsage {
            prompt_tokens: snapshot
                .prompt_tokens
                .saturating_sub(state.usage_before.prompt_tokens),
            completion_tokens: snapshot
                .completion_tokens
                .saturating_sub(state.usage_before.completion_tokens),
            total_tokens: snapshot
                .total_tokens
                .saturating_sub(state.usage_before.total_tokens),
        };
        state.session_usage.add(&delta);
        Ok(output)
    }

    async fn generate_files(
        &self,
        ctx: &mut ExecContext<'_>,
        state: &mut LiveState,
        messages: Vec<ChatMessage>,
    ) -> Result<(), String> {
        let text = self
            .call_counted(state, ctx.max_model_calls, &messages)
            .await?;
        let (blocks, warnings) = parse_file_blocks(&text);
        for warning in warnings {
            ctx.record_failed_step(format!("parse_warning:{warning}"));
        }
        if blocks.is_empty() {
            return Err("模型回复不含任何 FILE 块".to_string());
        }
        state.draft.clear();
        for (path, content) in blocks {
            state.draft.insert(path, content);
        }
        Ok(())
    }

    fn flush_draft_to_sandbox(
        &self,
        ctx: &mut ExecContext<'_>,
        draft: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let mut first_error = None;
        for (path, content) in draft {
            if let Err(write_err) = ctx.write_file(path, content) {
                first_error.get_or_insert(write_err);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn draft_for_prompt(draft: &BTreeMap<String, String>) -> String {
        let mut text = String::new();
        for (path, content) in draft {
            text.push_str(&format!(
                "--- {} ---\n{}\n",
                path,
                cap_text(content, PROMPT_TEXT_CAP)
            ));
        }
        text
    }

    async fn ensure_not_cancelled(ctx: &ExecContext<'_>) -> Result<(), String> {
        if ctx.cancelled() {
            Err("__cancelled__".to_string())
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl CaseExecutor for GenerativeExecutor {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        let usage_before = self.provider.usage_snapshot();
        let mut state = LiveState {
            model_calls: 0,
            session_usage: TokenUsage::default(),
            usage_before,
            plan: String::new(),
            draft: BTreeMap::new(),
        };
        let mut retries = 0u32;

        if ctx.cancelled() {
            return RawExecOutcome {
                aborted: true,
                ..RawExecOutcome::default()
            };
        }

        let pipeline_result: Result<(), String> = async {
            if ctx.mode == AgentMode::Multi {
                // ① 规划者
                Self::ensure_not_cancelled(ctx).await?;
                let mut planner_user = String::new();
                planner_user.push_str(&format!(
                    "# 任务\n[{}] {}\n{}\n",
                    ctx.case.category.as_str(),
                    ctx.case.title,
                    ctx.case.instruction.trim()
                ));
                planner_user.push_str("\n# 必须产出的 Artifact\n");
                for artifact in &ctx.case.expected_artifacts {
                    planner_user.push_str(&format!("- {artifact}\n"));
                }
                let plan = self
                    .call_counted(
                        &mut state,
                        ctx.max_model_calls,
                        &[
                            ChatMessage::system(SYSTEM_PLANNER.to_string()),
                            ChatMessage::user(planner_user),
                        ],
                    )
                    .await?;
                state.plan = plan;
            }

            // ② 执行者
            Self::ensure_not_cancelled(ctx).await?;
            let worker_context = if ctx.mode == AgentMode::Multi {
                Some(format!("规划者建议：\n{}", cap_text(&state.plan, PROMPT_TEXT_CAP)))
            } else {
                None
            };
            self.generate_files(ctx, &mut state, messages_for_generation(ctx.case, worker_context.as_deref()))
                .await?;

            // ③ 评审者（仅 multi）
            if ctx.mode == AgentMode::Multi {
                Self::ensure_not_cancelled(ctx).await?;
                let mut reviewer_user = String::new();
                reviewer_user.push_str(&format!(
                    "# 任务\n[{}] {}\n\n# 执行者草稿\n{}",
                    ctx.case.category.as_str(),
                    ctx.case.title,
                    Self::draft_for_prompt(&state.draft)
                ));
                reviewer_user.push_str("\n# 必须产出的 Artifact\n");
                for artifact in &ctx.case.expected_artifacts {
                    reviewer_user.push_str(&format!("- {artifact}\n"));
                }
                let verdict = self
                    .call_counted(
                        &mut state,
                        ctx.max_model_calls,
                        &[
                            ChatMessage::system(SYSTEM_REVIEWER.to_string()),
                            ChatMessage::user(reviewer_user),
                        ],
                    )
                    .await?;
                if verdict.trim().to_uppercase().starts_with("REVISE") {
                    // ④ 返工一轮
                    retries += 1;
                    Self::ensure_not_cancelled(ctx).await?;
                    let revise_context = format!(
                        "规划者建议：\n{}\n\n你的上一版草稿：\n{}\n\n评审意见：\n{}\n\n请输出修订后的完整文件块（全部产物，不是补丁）。",
                        cap_text(&state.plan, PROMPT_TEXT_CAP),
                        Self::draft_for_prompt(&state.draft),
                        cap_text(&verdict, PROMPT_TEXT_CAP)
                    );
                    self.generate_files(ctx, &mut state, messages_for_generation(ctx.case, Some(&revise_context)))
                        .await?;
                }
            }

            // ⑤ 受控落盘（写入范围强制在 write_file 内）
            let draft = state.draft.clone();
            self.flush_draft_to_sandbox(ctx, &draft)
        }
        .await;

        match pipeline_result {
            Ok(()) => RawExecOutcome {
                aborted: false,
                error: None,
                model_calls: state.model_calls,
                usage: state.session_usage,
                usage_known: state.session_usage.total_tokens > 0,
                retries,
                tool_log: Vec::new(),
            },
            Err(error) if error == "__cancelled__" => RawExecOutcome {
                aborted: true,
                error: None,
                model_calls: state.model_calls,
                usage: state.session_usage,
                usage_known: state.session_usage.total_tokens > 0,
                retries,
                tool_log: Vec::new(),
            },
            Err(error) => RawExecOutcome {
                aborted: false,
                error: Some(error),
                model_calls: state.model_calls,
                usage: state.session_usage,
                usage_known: state.session_usage.total_tokens > 0,
                retries,
                tool_log: Vec::new(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 矩阵执行器（journal + 续跑 + 报告）
// ---------------------------------------------------------------------------

/// 矩阵运行选项。
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// 参与对照的拓扑（默认 single+multi）。
    pub modes: Vec<AgentMode>,
    /// CLI 级重复次数覆盖。
    pub reps_override: Option<u32>,
    /// 只跑 id 包含该子串的任务。
    pub only: Option<String>,
    /// 只跑指定分类。
    pub category: Option<EvalCategory>,
    /// 清空 out 目录重跑（唯一允许"重算"的入口；journal 归零）。
    pub fresh: bool,
    /// 批次标签（写入 meta/报告；同目录批次不一致拒绝续跑）。
    pub batch_label: Option<String>,
    /// 附加标签（溯源用）。
    pub tags: Vec<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            modes: AgentMode::all().to_vec(),
            reps_override: None,
            only: None,
            category: None,
            fresh: false,
            batch_label: None,
            tags: Vec::new(),
        }
    }
}

/// 过滤后的任务列表。
pub fn filter_cases(bundle: &SuiteBundle, opts: &RunOptions) -> Vec<ProductEvalCase> {
    bundle
        .cases
        .iter()
        .filter(|case| {
            if let Some(only) = &opts.only {
                if !case.id.contains(only.as_str()) {
                    return false;
                }
            }
            if let Some(category) = &opts.category {
                if case.category != *category {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// 产品评测矩阵执行器：顺序执行 (case × mode × repetition)，
/// journal 追加落盘 + 断点续跑 + 全量报告（含失败）。
pub struct MatrixRunner {
    pub bundle: SuiteBundle,
    pub out_dir: PathBuf,
}

impl MatrixRunner {
    pub fn new(bundle: SuiteBundle, out_dir: impl Into<PathBuf>) -> Self {
        Self {
            bundle,
            out_dir: out_dir.into(),
        }
    }

    fn journal_path(&self) -> PathBuf {
        self.out_dir.join("state.jsonl")
    }

    fn meta_path(&self) -> PathBuf {
        self.out_dir.join("meta.json")
    }

    fn failures_dir(&self) -> PathBuf {
        self.out_dir.join("failures")
    }

    fn planned_matrix(&self, cases: &[ProductEvalCase], opts: &RunOptions) -> Vec<MatrixKey> {
        let mut keys = Vec::new();
        for case in cases {
            let reps = case.effective_repetitions(&self.bundle.suite.defaults, opts.reps_override);
            for mode in &opts.modes {
                for repetition in 0..reps {
                    keys.push(MatrixKey::new(case.id.clone(), *mode, repetition));
                }
            }
        }
        keys
    }

    fn init_or_verify_out_dir(
        &self,
        model: Option<&str>,
        opts: &RunOptions,
    ) -> Result<(), ProductEvalError> {
        let hash = suite_hash(&self.bundle);
        let fresh = opts.fresh;
        if fresh && self.out_dir.exists() {
            std::fs::remove_dir_all(&self.out_dir).map_err(|e| {
                ProductEvalError(format!("清空 {} 失败：{e}", self.out_dir.display()))
            })?;
        }
        std::fs::create_dir_all(&self.out_dir)
            .map_err(|e| ProductEvalError(format!("创建 {} 失败：{e}", self.out_dir.display())))?;
        std::fs::create_dir_all(self.failures_dir()).map_err(|e| {
            ProductEvalError(format!("创建 {} 失败：{e}", self.failures_dir().display()))
        })?;
        let meta_path = self.meta_path();
        if meta_path.exists() {
            let text = std::fs::read_to_string(&meta_path)
                .map_err(|e| ProductEvalError(format!("读取 meta.json 失败：{e}")))?;
            let meta: RunDirMeta = serde_json::from_str(&text)
                .map_err(|e| ProductEvalError(format!("解析 meta.json 失败：{e}")))?;
            if meta.suite_hash != hash || meta.schema_version != PRODUCT_EVAL_SCHEMA_VERSION {
                return err(format!(
                    "out 目录属于另一套件/版本（suite_hash 不一致）：换 --out 或加 --fresh。目录：{}",
                    self.out_dir.display()
                ));
            }
            // 批次一致性：同一批次的续跑必须沿用同一 batch_label（防跨批次混算）。
            if let Some(label) = &opts.batch_label {
                match &meta.batch_label {
                    Some(existing) if existing != label => {
                        return err(format!(
                            "out 目录已有批次「{existing}」，与本次「{label}」不一致：换 --out 或加 --fresh（修复后重测必须建立新批次，不得覆盖旧失败记录）"
                        ));
                    }
                    _ => {}
                }
            }
        } else {
            if self.journal_path().exists() {
                return err(format!(
                    "out 目录存在 state.jsonl 但缺少 meta.json（来源不明，拒绝续跑）：换 --out 或 --fresh。目录：{}",
                    self.out_dir.display()
                ));
            }
            let meta = RunDirMeta {
                schema_version: PRODUCT_EVAL_SCHEMA_VERSION,
                suite_name: self.bundle.suite.name.clone(),
                suite_hash: hash,
                model: model.map(str::to_string),
                batch_label: opts.batch_label.clone(),
                tags: opts.tags.clone(),
                created_at: now_rfc3339(),
            };
            let text = serde_json::to_string_pretty(&meta)
                .map_err(|e| ProductEvalError(format!("序列化 meta.json 失败：{e}")))?;
            std::fs::write(&meta_path, text)
                .map_err(|e| ProductEvalError(format!("写入 meta.json 失败：{e}")))?;
        }
        Ok(())
    }

    fn load_runs(&self) -> Result<Vec<ProductEvalRun>, ProductEvalError> {
        let path = self.journal_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ProductEvalError(format!("读取 {} 失败：{e}", path.display())))?;
        let mut runs = Vec::new();
        let lines: Vec<&str> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let total = lines.len();
        for (index, line) in lines.iter().enumerate() {
            match serde_json::from_str::<ProductEvalRun>(line) {
                Ok(run) => runs.push(run),
                Err(parse_error) => {
                    let is_torn_tail = index + 1 == total;
                    if is_torn_tail {
                        // 进程中断造成的半行：容忍并忽略（该单元格视为未完成）。
                        tracing::warn!(line = %line, error = %parse_error, "journal 尾行损坏，忽略（对应单元格将重跑）");
                    } else {
                        return err(format!(
                            "state.jsonl 第 {} 行损坏：{parse_error}（拒绝静默丢弃历史记录；如确要重跑请 --fresh）",
                            index + 1
                        ));
                    }
                }
            }
        }
        Ok(runs)
    }

    fn append_run(&self, run: &ProductEvalRun) -> Result<(), ProductEvalError> {
        let line = serde_json::to_string(run)
            .map_err(|e| ProductEvalError(format!("序列化运行记录失败：{e}")))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.journal_path())
            .map_err(|e| ProductEvalError(format!("打开 journal 失败：{e}")))?;
        writeln!(file, "{line}")
            .map_err(|e| ProductEvalError(format!("追加 journal 失败：{e}")))?;
        file.flush()
            .map_err(|e| ProductEvalError(format!("刷新 journal 失败：{e}")))?;
        Ok(())
    }

    /// 执行矩阵（断点续跑）。返回聚合了全部历史记录（含失败）的报告。
    pub async fn run(
        &self,
        executor: Arc<dyn CaseExecutor>,
        execution: &str,
        model: Option<String>,
        opts: &RunOptions,
        cancel: Arc<AtomicBool>,
    ) -> Result<ProductEvalReport, ProductEvalError> {
        let cases = filter_cases(&self.bundle, opts);
        if cases.is_empty() {
            return err("过滤条件下没有可执行的任务");
        }
        self.init_or_verify_out_dir(model.as_deref(), opts)?;
        let mut runs = self.load_runs()?;
        let completed: std::collections::BTreeSet<MatrixKey> =
            runs.iter().map(|run| run.key.clone()).collect();
        let planned = self.planned_matrix(&cases, opts);
        let pending: Vec<MatrixKey> = planned
            .iter()
            .filter(|key| !completed.contains(key))
            .cloned()
            .collect();

        let defaults = self.bundle.suite.defaults.clone();
        let total_planned = planned.len();
        tracing::info!(
            suite = %self.bundle.suite.name,
            planned = total_planned,
            done = runs.len(),
            todo = pending.len(),
            execution = execution,
            "product-eval 矩阵开始"
        );

        for key in &pending {
            if cancel.load(Ordering::Relaxed) {
                tracing::info!("收到取消信号，停止调度后续单元格（已完成记录保留）");
                break;
            }
            let case = cases
                .iter()
                .find(|case| case.id == key.case_id)
                .ok_or_else(|| ProductEvalError(format!("矩阵键引用未知任务 {}", key.case_id)))?;
            let timeout_secs = case.effective_timeout_secs(&defaults);
            let max_model_calls = case.effective_max_model_calls(&defaults);

            // 沙盒：输入 fixture 预写。
            let sandbox = self.out_dir.join("sandboxes").join(format!(
                "{}-{}",
                key.slug().replace('#', "__"),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&sandbox)
                .map_err(|e| ProductEvalError(format!("创建沙盒失败：{e}")))?;
            let mut setup_error = None;
            for input in &case.inputs {
                let target = sandbox.join(sanitize_rel_path(&input.path).unwrap_or_default());
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&target, &input.content) {
                    setup_error = Some(format!("写输入 fixture {} 失败：{e}", input.path));
                    break;
                }
            }

            let started_at = now_rfc3339();
            let cell_started = Instant::now();
            let (status, failed_steps, artifact_refs, outcome, checker_passed, checker_total) =
                if let Some(setup_error) = setup_error {
                    (
                        RunStatus::Error,
                        vec![format!("setup:{setup_error}")],
                        Vec::new(),
                        RawExecOutcome::default(),
                        0,
                        0,
                    )
                } else {
                    let mut ctx = ExecContext::new(
                        case,
                        &sandbox,
                        key.agent_mode,
                        timeout_secs,
                        max_model_calls,
                        Arc::clone(&cancel),
                    );
                    let future = executor.execute(&mut ctx);
                    match tokio::time::timeout(Duration::from_secs(timeout_secs), future).await {
                        Ok(outcome) => {
                            let (artifacts, mut steps) = ctx.take_records();
                            if outcome.aborted {
                                steps.push("cancelled:收到取消信号".to_string());
                            }
                            let (status, checker_steps, checker_passed, checker_total) =
                                if outcome.aborted {
                                    (RunStatus::Cancelled, Vec::new(), 0, 0)
                                } else if let Some(error) = &outcome.error {
                                    steps.push(format!("executor:{error}"));
                                    (RunStatus::Error, Vec::new(), 0, 0)
                                } else {
                                    // 检查器判定：静态检查器 + 行为 command_check 在真实沙盒执行。
                                    let (passed, total, failed) =
                                        evaluate_all_on_dir(&case.checkers, &sandbox).await;
                                    if failed.is_empty() {
                                        (RunStatus::Passed, Vec::new(), passed, total)
                                    } else {
                                        (RunStatus::Failed, failed, passed, total)
                                    }
                                };
                            steps.extend(checker_steps);
                            (
                                status,
                                steps,
                                artifacts,
                                outcome,
                                checker_passed,
                                checker_total,
                            )
                        }
                        Err(_) => {
                            let (artifacts, mut steps) = ctx.take_records();
                            steps.push(format!("timeout:超过 {timeout_secs}s 上限"));
                            (
                                RunStatus::Timeout,
                                steps,
                                artifacts,
                                RawExecOutcome::default(),
                                0,
                                0,
                            )
                        }
                    }
                };
            let wall_ms = cell_started.elapsed().as_millis() as u64;

            // 失败沙盒留档（供事后排查，路径随记录落盘）；成功沙盒即时清理。
            let sandbox_rel = if matches!(status, RunStatus::Passed) {
                let _ = std::fs::remove_dir_all(&sandbox);
                None
            } else {
                let keep = self.failures_dir().join(key.slug().replace('#', "__"));
                let _ = std::fs::remove_dir_all(&keep);
                let _ = std::fs::rename(&sandbox, &keep);
                Some(
                    keep.strip_prefix(&self.out_dir)
                        .unwrap_or(&keep)
                        .to_string_lossy()
                        .replace('\\', "/"),
                )
            };

            let cost_usd = if outcome.usage_known {
                estimate_cost_from_env(&outcome.usage)
            } else {
                None
            };
            let run = ProductEvalRun {
                key: key.clone(),
                category: case.category,
                status,
                wall_ms,
                model_calls: outcome.model_calls,
                prompt_tokens: outcome.usage_known.then_some(outcome.usage.prompt_tokens),
                completion_tokens: outcome
                    .usage_known
                    .then_some(outcome.usage.completion_tokens),
                total_tokens: outcome.usage_known.then_some(outcome.usage.total_tokens),
                cost_usd,
                failed_steps,
                retries: outcome.retries,
                cancellations: if outcome.aborted { 1 } else { 0 },
                artifact_refs,
                tool_log: outcome.tool_log,
                checker_passed,
                checker_total,
                sandbox_rel,
                model: model.clone(),
                started_at,
                finished_at: now_rfc3339(),
                error: outcome.error,
            };
            self.append_run(&run)?;
            runs.push(run);
            let report = self.build_report(&runs, &cases, opts, execution, &model)?;
            write_report(&self.out_dir, &report)?;
            tracing::info!(
                cell = %runs.last().map(|r| r.key.slug()).unwrap_or_default(),
                status = ?runs.last().map(|r| r.status),
                wall_ms,
                "product-eval 单元格完成"
            );
        }

        self.build_report(&runs, &cases, opts, execution, &model)
    }

    fn build_report(
        &self,
        runs: &[ProductEvalRun],
        cases: &[ProductEvalCase],
        opts: &RunOptions,
        execution: &str,
        model: &Option<String>,
    ) -> Result<ProductEvalReport, ProductEvalError> {
        let completed: std::collections::BTreeSet<MatrixKey> =
            runs.iter().map(|run| run.key.clone()).collect();
        let planned = self.planned_matrix(cases, opts);
        let pending: Vec<MatrixKey> = planned
            .iter()
            .filter(|key| !completed.contains(key))
            .cloned()
            .collect();
        Ok(ProductEvalReport {
            schema_version: PRODUCT_EVAL_SCHEMA_VERSION,
            suite_name: self.bundle.suite.name.clone(),
            suite_hash: suite_hash(&self.bundle),
            execution: execution.to_string(),
            model: model.clone(),
            batch_label: opts.batch_label.clone(),
            tags: opts.tags.clone(),
            generated_at: now_rfc3339(),
            runs: runs.to_vec(),
            pending,
            metrics: aggregate_metrics(runs),
            per_case: aggregate_per_case(runs),
        })
    }
}

/// 真实单 Agent 执行器（第一路）：经既有 Agent/工具/权限/Session 全链路执行任务。
pub mod single_agent;

pub use single_agent::{
    ProductEvalApprover, ProductEvalScope, SingleAgentExecutor, TelemetrySnapshot,
};

/// 依据环境变量单价估算成本（未配置则 None）。
fn estimate_cost_from_env(usage: &TokenUsage) -> Option<f64> {
    let input = std::env::var("OWO_EVAL_PRICE_IN_PER_MTOK")
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    let output = std::env::var("OWO_EVAL_PRICE_OUT_PER_MTOK")
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    Some(usage.cost_estimate_usd(input, output))
}

/// 运行检查器质量分（0..1）：通过检查器数 / 总检查器数的均值。
/// 无任何带检查器计数的运行（如旧 journal）时返回 None。
pub fn quality_of(runs: &[ProductEvalRun]) -> Option<f64> {
    let mut sum = 0.0;
    let mut counted = 0usize;
    for run in runs {
        if run.checker_total > 0 {
            sum += run.checker_passed as f64 / run.checker_total as f64;
            counted += 1;
        }
    }
    if counted == 0 {
        None
    } else {
        Some(sum / counted as f64)
    }
}

/// 聚合指标：全部已尝试运行一律计入分母。
pub fn aggregate_metrics(runs: &[ProductEvalRun]) -> ProductEvalMetrics {
    let total = runs.len();
    let passed = runs
        .iter()
        .filter(|r| r.status == RunStatus::Passed)
        .count();
    let failed = runs
        .iter()
        .filter(|r| r.status == RunStatus::Failed)
        .count();
    let errors = runs.iter().filter(|r| r.status == RunStatus::Error).count();
    let timeouts = runs
        .iter()
        .filter(|r| r.status == RunStatus::Timeout)
        .count();
    let cancelled = runs
        .iter()
        .filter(|r| r.status == RunStatus::Cancelled)
        .count();
    let wall_sum: u64 = runs.iter().map(|r| r.wall_ms).sum();
    let model_calls: u64 = runs.iter().map(|r| r.model_calls as u64).sum();
    let tokens_known = runs.iter().any(|r| r.total_tokens.is_some());
    let total_tokens = if tokens_known {
        Some(runs.iter().filter_map(|r| r.total_tokens).sum())
    } else {
        None
    };
    let cost_known = runs.iter().any(|r| r.cost_usd.is_some());
    let estimated_cost_usd = if cost_known {
        Some(runs.iter().filter_map(|r| r.cost_usd).sum())
    } else {
        None
    };
    ProductEvalMetrics {
        runs_total: total,
        passed,
        failed,
        errors,
        timeouts,
        cancelled,
        success_rate: if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64
        },
        mean_wall_ms: if total == 0 {
            0.0
        } else {
            wall_sum as f64 / total as f64
        },
        total_model_calls: model_calls,
        total_tokens,
        estimated_cost_usd,
    }
}

/// 按 (case_id, mode) 分组统计（按 id+mode 排序，输出稳定）。
pub fn aggregate_per_case(runs: &[ProductEvalRun]) -> Vec<CaseModeMetrics> {
    let mut groups: BTreeMap<(String, AgentMode), Vec<&ProductEvalRun>> = BTreeMap::new();
    for run in runs {
        groups
            .entry((run.key.case_id.clone(), run.key.agent_mode))
            .or_default()
            .push(run);
    }
    groups
        .into_iter()
        .map(|((case_id, agent_mode), group)| {
            let owned: Vec<ProductEvalRun> = group.iter().map(|run| (*run).clone()).collect();
            let metrics = aggregate_metrics(&owned);
            let mean_calls = if group.is_empty() {
                0.0
            } else {
                group.iter().map(|r| r.model_calls as f64).sum::<f64>() / group.len() as f64
            };
            CaseModeMetrics {
                case_id,
                category: group
                    .first()
                    .map(|r| r.category)
                    .unwrap_or(EvalCategory::Code),
                agent_mode,
                runs_total: metrics.runs_total,
                passed: metrics.passed,
                success_rate: metrics.success_rate,
                mean_wall_ms: metrics.mean_wall_ms,
                mean_model_calls: mean_calls,
                total_tokens: metrics.total_tokens,
                quality: quality_of(&owned),
            }
        })
        .collect()
}

/// 从磁盘加载报告（compare 用）。
pub fn load_report(path: &Path) -> Result<ProductEvalReport, ProductEvalError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ProductEvalError(format!("读取 {} 失败：{e}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|e| ProductEvalError(format!("解析报告 {} 失败：{e}", path.display())))
}

fn write_report(out_dir: &Path, report: &ProductEvalReport) -> Result<(), ProductEvalError> {
    let text = serde_json::to_string_pretty(report)
        .map_err(|e| ProductEvalError(format!("序列化报告失败：{e}")))?;
    std::fs::write(out_dir.join("report.json"), text)
        .map_err(|e| ProductEvalError(format!("写入报告失败：{e}")))
}

/// 报告的人类可读摘要（run 结束时输出）。
pub fn format_report_summary(report: &ProductEvalReport) -> String {
    let m = &report.metrics;
    let mut out = String::new();
    out.push_str(&format!(
        "套件「{}」（execution={}，model={}）\n",
        report.suite_name,
        report.execution,
        report.model.as_deref().unwrap_or("-")
    ));
    if let Some(label) = &report.batch_label {
        out.push_str(&format!("批次：{label}\n"));
    }
    if !report.tags.is_empty() {
        out.push_str(&format!("标签：{}\n", report.tags.join("、")));
    }
    out.push_str(&format!(
        "运行 {}/{}（passed={} failed={} error={} timeout={} cancelled={}）成功率 {:.1}%\n",
        m.runs_total,
        m.runs_total + report.pending.len(),
        m.passed,
        m.failed,
        m.errors,
        m.timeouts,
        m.cancelled,
        m.success_rate * 100.0
    ));
    out.push_str(&format!(
        "平均墙钟 {:.0}ms；模型调用 {} 次；tokens {:?}；成本 {:?}\n",
        m.mean_wall_ms, m.total_model_calls, m.total_tokens, m.estimated_cost_usd
    ));
    if !report.pending.is_empty() {
        out.push_str(&format!(
            "未完成单元格 {} 个（重跑同一命令即可续跑，不重复已完成 case）\n",
            report.pending.len()
        ));
    }
    for row in &report.per_case {
        out.push_str(&format!(
            "  {:<34} {:<7} {:>2}/{}（{:.0}%）mean_wall={:.0}ms mean_calls={:.1} quality={}\n",
            row.case_id,
            row.agent_mode.as_str(),
            row.passed,
            row.runs_total,
            row.success_rate * 100.0,
            row.mean_wall_ms,
            row.mean_model_calls,
            row.quality
                .map(|q| format!("{:.2}", q))
                .unwrap_or_else(|| "-".to_string())
        ));
    }
    out
}

/// 对照表中一个单元格：A/B 两侧的分组统计（缺侧为 None）。
type CompareRow<'a> = (Option<&'a CaseModeMetrics>, Option<&'a CaseModeMetrics>);

/// 对照两份报告（相同任务集），输出文本表或 JSON；失败运行不参与任何"剔除"。
pub fn compare_reports(a: &ProductEvalReport, b: &ProductEvalReport, as_json: bool) -> String {
    let mut rows: BTreeMap<(String, AgentMode), CompareRow<'_>> = BTreeMap::new();
    for row in &a.per_case {
        rows.insert((row.case_id.clone(), row.agent_mode), (Some(row), None));
    }
    for row in &b.per_case {
        rows.entry((row.case_id.clone(), row.agent_mode))
            .and_modify(|(_, slot_b)| *slot_b = Some(row))
            .or_insert_with(|| (None, Some(row)));
    }
    if as_json {
        let json_rows: Vec<serde_json::Value> = rows
            .iter()
            .map(|((case_id, mode), (ra, rb))| {
                serde_json::json!({
                    "case_id": case_id,
                    "agent_mode": mode.as_str(),
                    "a": ra,
                    "b": rb,
                    "delta_success_rate": match (ra, rb) {
                        (Some(ra), Some(rb)) => Some(rb.success_rate - ra.success_rate),
                        _ => None,
                    },
                })
            })
            .collect();
        return serde_json::to_string_pretty(&serde_json::json!({
            "a": a,
            "b": b,
            "rows": json_rows,
        }))
        .unwrap_or_else(|_| "{}".to_string());
    }
    let mut out = String::new();
    out.push_str(&format!(
        "A：{}（{}，model={}，generated={}）\n",
        a.suite_name,
        a.execution,
        a.model.as_deref().unwrap_or("-"),
        a.generated_at
    ));
    out.push_str(&format!(
        "B：{}（{}，model={}，generated={}）\n\n",
        b.suite_name,
        b.execution,
        b.model.as_deref().unwrap_or("-"),
        b.generated_at
    ));
    out.push_str(&format!(
        "总体成功率：A {:.1}%（{}/{}）→ B {:.1}%（{}/{}）\n",
        a.metrics.success_rate * 100.0,
        a.metrics.passed,
        a.metrics.runs_total,
        b.metrics.success_rate * 100.0,
        b.metrics.passed,
        b.metrics.runs_total
    ));
    out.push_str(&format!(
        "平均墙钟：A {:.0}ms → B {:.0}ms；tokens：A {:?} → B {:?}\n\n",
        a.metrics.mean_wall_ms,
        b.metrics.mean_wall_ms,
        a.metrics.total_tokens,
        b.metrics.total_tokens
    ));
    out.push_str(&format!(
        "{:<32} {:<7} {:>12} {:>12} {:>8}\n",
        "case", "mode", "A", "B", "Δrate"
    ));
    let mut regressions = 0usize;
    let mut improvements = 0usize;
    for ((case_id, mode), (ra, rb)) in &rows {
        let cell_a = ra
            .map(|r| {
                format!(
                    "{}/{}（{:.0}%）",
                    r.passed,
                    r.runs_total,
                    r.success_rate * 100.0
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let cell_b = rb
            .map(|r| {
                format!(
                    "{}/{}（{:.0}%）",
                    r.passed,
                    r.runs_total,
                    r.success_rate * 100.0
                )
            })
            .unwrap_or_else(|| "-".to_string());
        let delta = match (ra, rb) {
            (Some(ra), Some(rb)) => {
                let d = rb.success_rate - ra.success_rate;
                if d < -1e-9 {
                    regressions += 1;
                    format!("{d:+.2}↓")
                } else if d > 1e-9 {
                    improvements += 1;
                    format!("{d:+.2}↑")
                } else {
                    format!("{d:+.2}")
                }
            }
            _ => "-".to_string(),
        };
        out.push_str(&format!(
            "{:<32} {:<7} {:>12} {:>12} {:>8}\n",
            truncate_for_log(case_id, 32),
            mode.as_str(),
            cell_a,
            cell_b,
            delta
        ));
    }
    out.push_str(&format!(
        "\n回归 {regressions} 组，提升 {improvements} 组（Δ 仅作对照，不重算任何历史成功率）\n"
    ));
    if a.execution != b.execution {
        out.push_str("⚠️ 两份报告 execution 不同（如 dry vs live），成功率不具备直接可比性\n");
    }
    if a.suite_hash != b.suite_hash {
        out.push_str("⚠️ 两份报告 suite_hash 不同：任务定义已变更，对照仅具参考意义\n");
    }
    out
}

/// 构建 live 生成式执行器所需的 Provider（GLM/任意 OpenAI 兼容端点）。
/// 模型解析顺序：显式覆盖 > OPENAI_MODEL > 内置默认（GLM）。
pub fn build_live_provider(
    model_override: Option<&str>,
) -> Result<(Arc<dyn ModelProvider>, String), ProductEvalError> {
    let mut config = OpenAiCompatibleConfig::from_env().map_err(ProductEvalError)?;
    if let Some(model) = model_override {
        config.model = model.to_string();
    }
    let model = config.model.clone();
    let provider =
        crate::gateway::ResilientProvider::from_config(config).map_err(ProductEvalError)?;
    Ok((Arc::new(provider), model))
}

// ---------------------------------------------------------------------------
// R1 live 基线统计：Wilson 置信区间 / p50-p95 分位数 / 单多对照与启用条件
// ---------------------------------------------------------------------------

/// 95% 置信区间的 z 值（正态近似）。
pub const CI95_Z: f64 = 1.96;

/// 成功率的 Wilson score 置信区间（小样本稳健；z 传 [`CI95_Z`] 即 95%）。
/// total = 0（无样本）时返回 (0.0, 0.0)。
pub fn wilson_interval(successes: usize, total: usize, z: f64) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let half = (z / denom) * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    (
        (center - half).clamp(0.0, 1.0),
        (center + half).clamp(0.0, 1.0),
    )
}

/// 线性插值分位数（p ∈ [0,100]；空集返回 None；单元素直接返回该值）。
pub fn percentile(values: &[u64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.iter().map(|value| *value as f64).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let rank = (p.clamp(0.0, 100.0) / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let frac = rank - lo as f64;
    Some(sorted[lo] * (1.0 - frac) + sorted[hi.min(sorted.len() - 1)] * frac)
}

/// 判定样本量是否足以支撑统计结论（经验阈值：n ≥ 30）。
pub const SUFFICIENT_SAMPLE_SIZE: usize = 30;

/// 单一拓扑（single/multi）的统计快照（分母一律含失败/错误/超时，不剔除）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeStats {
    pub mode: String,
    pub runs_total: usize,
    pub passed: usize,
    pub success_rate: f64,
    pub ci95_low: f64,
    pub ci95_high: f64,
    pub p50_wall_ms: Option<f64>,
    pub p95_wall_ms: Option<f64>,
    pub mean_wall_ms: f64,
    pub mean_model_calls: f64,
    pub total_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    /// 检查器通过率均值（质量代理；无检查器计数的旧记录缺省为 None）。
    #[serde(default)]
    pub quality: Option<f64>,
    /// 样本量是否达到可判定阈值（n ≥ 30）。
    pub sample_sufficient: bool,
}

/// 计算一个拓扑的统计快照（无样本时 success_rate=0、区间 (0,0)、分位数 None）。
pub fn mode_statistics(runs: &[ProductEvalRun], mode: AgentMode) -> ModeStats {
    let group: Vec<&ProductEvalRun> = runs
        .iter()
        .filter(|run| run.key.agent_mode == mode)
        .collect();
    let total = group.len();
    let passed = group
        .iter()
        .filter(|run| run.status == RunStatus::Passed)
        .count();
    let (low, high) = wilson_interval(passed, total, CI95_Z);
    let walls: Vec<u64> = group.iter().map(|run| run.wall_ms).collect();
    let mean_wall = if total == 0 {
        0.0
    } else {
        walls.iter().sum::<u64>() as f64 / total as f64
    };
    let mean_calls = if total == 0 {
        0.0
    } else {
        group.iter().map(|run| run.model_calls as f64).sum::<f64>() / total as f64
    };
    let tokens_known = group.iter().any(|run| run.total_tokens.is_some());
    let tokens = if tokens_known {
        Some(group.iter().filter_map(|run| run.total_tokens).sum())
    } else {
        None
    };
    let cost_known = group.iter().any(|run| run.cost_usd.is_some());
    let cost = if cost_known {
        Some(group.iter().filter_map(|run| run.cost_usd).sum())
    } else {
        None
    };
    let owned: Vec<ProductEvalRun> = group.iter().map(|run| (*run).clone()).collect();
    let quality = quality_of(&owned);
    ModeStats {
        mode: mode.as_str().to_string(),
        runs_total: total,
        passed,
        success_rate: if total == 0 {
            0.0
        } else {
            passed as f64 / total as f64
        },
        ci95_low: low,
        ci95_high: high,
        p50_wall_ms: percentile(&walls, 50.0),
        p95_wall_ms: percentile(&walls, 95.0),
        mean_wall_ms: mean_wall,
        mean_model_calls: mean_calls,
        total_tokens: tokens,
        total_cost_usd: cost,
        quality,
        sample_sufficient: total >= SUFFICIENT_SAMPLE_SIZE,
    }
}

/// 一条启用条件判定（多 Agent 启用门槛：任一满足即建议启用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnablementRule {
    pub name: String,
    pub satisfied: bool,
    pub detail: String,
}

/// 单/多 Agent 对照差异与启用条件判定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeComparison {
    pub multi_success_rate_diff: f64,
    pub multi_wall_rel_change: Option<f64>,
    pub multi_calls_rel_change: Option<f64>,
    pub multi_tokens_rel_change: Option<f64>,
    pub multi_cost_rel_change: Option<f64>,
    pub rules: Vec<EnablementRule>,
    /// 任一启用条件满足 → true。
    pub enabled: bool,
    /// 样本量是否足以支撑对照结论（两组都 ≥ 30 才算充分）。
    pub sample_sufficient: bool,
}

fn rel_change(baseline: f64, candidate: f64) -> Option<f64> {
    if baseline == 0.0 {
        None
    } else {
        Some((candidate - baseline) / baseline)
    }
}

/// 对照判定（multi 相对 single）。启用条件：
/// ① 成功率差 ≥ +5 个百分点；② 成功率相对提升 ≥ +10%（质量代理：检查器为二元
/// 判定，暂无独立质量指标，以成功率相对改善代替，已在 detail 标注）；③ 平均耗时 −30%。
pub fn compare_mode_statistics(single: &ModeStats, multi: &ModeStats) -> ModeComparison {
    let diff = multi.success_rate - single.success_rate;
    let wall_rel = rel_change(single.mean_wall_ms, multi.mean_wall_ms);
    let calls_rel = rel_change(single.mean_model_calls, multi.mean_model_calls);
    let tokens_rel = match (single.total_tokens, multi.total_tokens) {
        (Some(a), Some(b)) => rel_change(a as f64, b as f64),
        _ => None,
    };
    let cost_rel = match (single.total_cost_usd, multi.total_cost_usd) {
        (Some(a), Some(b)) => rel_change(a, b),
        _ => None,
    };
    let rate_rel = rel_change(single.success_rate, multi.success_rate);
    let mut rules = vec![EnablementRule {
        name: "成功率 +5%".to_string(),
        satisfied: diff >= 0.05,
        detail: format!("multi−single = {:+.1}pp（门槛 ≥ +5.0pp）", diff * 100.0),
    }];
    rules.push(EnablementRule {
        name: "质量 +10%".to_string(),
        satisfied: rate_rel.is_some_and(|value| value >= 0.10),
        detail: format!(
            "质量代理 = 成功率相对提升 {:+.1}%（门槛 ≥ +10%；检查器为二元判定，暂无独立质量指标）",
            rate_rel.map(|value| value * 100.0).unwrap_or(f64::NAN)
        ),
    });
    rules.push(EnablementRule {
        name: "耗时 -30%".to_string(),
        satisfied: wall_rel.is_some_and(|value| value <= -0.30),
        detail: format!(
            "平均墙钟相对变化 {:+.1}%（门槛 ≤ −30.0%）",
            wall_rel.map(|value| value * 100.0).unwrap_or(f64::NAN)
        ),
    });
    ModeComparison {
        multi_success_rate_diff: diff,
        multi_wall_rel_change: wall_rel,
        multi_calls_rel_change: calls_rel,
        multi_tokens_rel_change: tokens_rel,
        multi_cost_rel_change: cost_rel,
        enabled: rules.iter().any(|rule| rule.satisfied),
        rules,
        sample_sufficient: single.sample_sufficient && multi.sample_sufficient,
    }
}

/// 单/多两拓扑的完整统计（报告 JSON 契约形状：供 server/TS 同步）。
pub fn report_statistics(runs: &[ProductEvalRun]) -> serde_json::Value {
    let single = mode_statistics(runs, AgentMode::Single);
    let multi = mode_statistics(runs, AgentMode::Multi);
    let comparison = compare_mode_statistics(&single, &multi);
    serde_json::json!({
        "modes": [serde_json::to_value(&single).unwrap_or_default(),
                  serde_json::to_value(&multi).unwrap_or_default()],
        "comparison": serde_json::to_value(&comparison).unwrap_or_default(),
    })
}

/// 人类可读的统计段落（run 结束摘要追加 / live 基线报告引用）。
pub fn format_mode_statistics(runs: &[ProductEvalRun]) -> String {
    let single = mode_statistics(runs, AgentMode::Single);
    let multi = mode_statistics(runs, AgentMode::Multi);
    let mut out = String::from("—— 统计（95% Wilson 置信区间；分母含失败，不剔除）——\n");
    for stats in [&single, &multi] {
        if stats.runs_total == 0 {
            continue;
        }
        out.push_str(&format!(
            "  {:<7} 成功率 {:.1}% CI95 [{:.1}%, {:.1}%]（{}/{}） p50={:.0}ms p95={:.0}ms mean_calls={:.1} tokens={:?} cost={:?}{}\n",
            stats.mode,
            stats.success_rate * 100.0,
            stats.ci95_low * 100.0,
            stats.ci95_high * 100.0,
            stats.passed,
            stats.runs_total,
            stats.p50_wall_ms.unwrap_or(0.0),
            stats.p95_wall_ms.unwrap_or(0.0),
            stats.mean_model_calls,
            stats.total_tokens,
            stats.total_cost_usd,
            if stats.sample_sufficient { "" } else { "（样本不足 n<30，区间仅供参考）" },
        ));
    }
    if single.runs_total > 0 && multi.runs_total > 0 {
        let comparison = compare_mode_statistics(&single, &multi);
        out.push_str(&format_mode_comparison(&comparison));
    }
    out
}

/// 启用条件判定的人类可读段落。
pub fn format_mode_comparison(comparison: &ModeComparison) -> String {
    let mut out = String::from("  —— 多 Agent 启用条件（任一满足即建议启用）——\n");
    for rule in &comparison.rules {
        let mark = if rule.satisfied { "✅" } else { "⬜" };
        out.push_str(&format!("  {} {}：{}\n", mark, rule.name, rule.detail));
    }
    out.push_str(if comparison.enabled {
        "  结论：建议启用多 Agent（满足至少一条启用条件）\n"
    } else {
        "  结论：暂不建议启用多 Agent（未满足任何启用条件）\n"
    });
    if !comparison.sample_sufficient {
        out.push_str("  ⚠️ 样本不足（n<30）：以上对照结论仅具方向性参考\n");
    }
    out
}

// ---------------------------------------------------------------------------
// 配对对照报告（第二路交付第三路：PairedStats 兼容 JSON）
// ---------------------------------------------------------------------------

/// 配对对照报告 schema 版本（对齐 team_benefit 的读取契约）。
pub const PAIRED_REPORT_SCHEMA_VERSION: u32 = 1;

/// 配对报告绑定参数：四元组（model/template/task_set/strategy_version）。
/// strategy_version 由三路冻结；本路负责如实记录。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PairedReportOptions {
    pub model: Option<String>,
    pub template: Option<String>,
    pub task_set: Option<String>,
    pub strategy_version: String,
}

/// 单侧快照 JSON（字段对齐 `team_benefit::ModeStatSnapshot` + quality）。
fn paired_snapshot_json(mode: AgentMode, runs: &[&ProductEvalRun]) -> serde_json::Value {
    let total = runs.len();
    let passed = runs
        .iter()
        .filter(|run| run.status == RunStatus::Passed)
        .count();
    let (low, high) = wilson_interval(passed, total, CI95_Z);
    let walls: Vec<u64> = runs.iter().map(|run| run.wall_ms).collect();
    let mean_wall = if total == 0 {
        0.0
    } else {
        walls.iter().sum::<u64>() as f64 / total as f64
    };
    let mean_calls = if total == 0 {
        0.0
    } else {
        runs.iter().map(|r| r.model_calls as f64).sum::<f64>() / total as f64
    };
    let tokens_known = runs.iter().any(|r| r.total_tokens.is_some());
    let tokens = if tokens_known {
        Some(runs.iter().filter_map(|r| r.total_tokens).sum::<u64>())
    } else {
        None
    };
    let cost_known = runs.iter().any(|r| r.cost_usd.is_some());
    let cost = if cost_known {
        Some(runs.iter().filter_map(|r| r.cost_usd).sum::<f64>())
    } else {
        None
    };
    let owned: Vec<ProductEvalRun> = runs.iter().map(|r| (*r).clone()).collect();
    serde_json::json!({
        "mode": mode.as_str(),
        "runs_total": total,
        "passed": passed,
        "success_rate": if total == 0 { 0.0 } else { passed as f64 / total as f64 },
        "ci95_low": low,
        "ci95_high": high,
        "mean_wall_ms": mean_wall,
        "mean_model_calls": mean_calls,
        "total_tokens": tokens,
        "total_cost_usd": cost,
        "quality": quality_of(&owned),
        "sample_sufficient": total >= SUFFICIENT_SAMPLE_SIZE,
    })
}

/// 生成三路可直接读取的配对对照报告 JSON（一个包里含全部任务组）。
///
/// 分组：`overall` / 分类 `code|research|document` / 每 `case_id`；
/// 每组含 single/multi 快照（样本数、成功率、质量、耗时）与绑定四元组，
/// 同时保留两侧报告摘要（suite_hash/批次/生成时间）供三路追溯。
pub fn build_paired_report_json(
    single: &ProductEvalReport,
    multi: &ProductEvalReport,
    opts: &PairedReportOptions,
    generated_at: Option<&str>,
) -> serde_json::Value {
    use std::collections::BTreeSet;

    let generated_at = generated_at.unwrap_or(&now_rfc3339()).to_string();
    let bindings = serde_json::json!({
        "model": opts.model.clone(),
        "template": opts.template.clone(),
        "task_set": opts.task_set.clone(),
        "strategy_version": opts.strategy_version,
    });
    let mut pairs: Vec<serde_json::Value> = Vec::new();

    let mut categories: BTreeSet<String> = BTreeSet::new();
    let mut case_ids: BTreeSet<String> = BTreeSet::new();
    for run in single.runs.iter().chain(multi.runs.iter()) {
        categories.insert(run.category.as_str().to_string());
        case_ids.insert(run.key.case_id.clone());
    }

    let mut push_group = |label: &str, filter: &dyn Fn(&ProductEvalRun) -> bool| {
        let single_runs: Vec<&ProductEvalRun> = single
            .runs
            .iter()
            .filter(|r| r.key.agent_mode == AgentMode::Single && filter(r))
            .collect();
        let multi_runs: Vec<&ProductEvalRun> = multi
            .runs
            .iter()
            .filter(|r| r.key.agent_mode == AgentMode::Multi && filter(r))
            .collect();
        if single_runs.is_empty() && multi_runs.is_empty() {
            return;
        }
        pairs.push(serde_json::json!({
            "task_group": label,
            "single": paired_snapshot_json(AgentMode::Single, &single_runs),
            "multi": paired_snapshot_json(AgentMode::Multi, &multi_runs),
            "bindings": bindings.clone(),
            "generated_at": generated_at,
        }));
    };

    push_group("overall", &|_| true);
    for category in &categories {
        let wanted = category.clone();
        push_group(category, &move |r| r.category.as_str() == wanted);
    }
    for case_id in &case_ids {
        let wanted = case_id.clone();
        push_group(case_id, &move |r| r.key.case_id == wanted);
    }

    serde_json::json!({
        "schema_version": PAIRED_REPORT_SCHEMA_VERSION,
        "generated_at": generated_at,
        "bindings": bindings,
        "single_report": {
            "suite_name": single.suite_name,
            "suite_hash": single.suite_hash,
            "execution": single.execution,
            "batch_label": single.batch_label,
            "generated_at": single.generated_at,
            "metrics": single.metrics,
        },
        "multi_report": {
            "suite_name": multi.suite_name,
            "suite_hash": multi.suite_hash,
            "execution": multi.execution,
            "batch_label": multi.batch_label,
            "generated_at": multi.generated_at,
            "metrics": multi.metrics,
        },
        "pairs": pairs,
    })
}

// ---------------------------------------------------------------------------
// freeze.json：任务输入 / 检查器 / 权限 / 预算 / 模型配置 / 版本哈希冻结
// ---------------------------------------------------------------------------

/// freeze.json schema 版本。
pub const FREEZE_SCHEMA_VERSION: u32 = 1;

/// 计算单任务权限哈希：allow_read/allow_write/allow_commands 的规范序列化摘要。
pub fn permissions_hash(case: &ProductEvalCase) -> String {
    let mut hasher = Sha256::new();
    let payload = serde_json::json!({
        "allow_read": case.allow_read,
        "allow_write": case.allow_write,
        "allow_commands": case.allow_commands,
    });
    if let Ok(text) = serde_json::to_vec(&payload) {
        hasher.update(&text);
    }
    format!("{:x}", hasher.finalize())
}

/// 生成 freeze.json 内容（任务文件级 sha256 + 生效预算 + 权限哈希 + 模型/版本）。
/// `task_rel_paths` 为 suite.tasks 的相对路径（与 tasks 文件一一对应）。
pub fn build_freeze_json(
    bundle: &SuiteBundle,
    model: Option<&str>,
    base_url: Option<&str>,
    git_commit: Option<&str>,
    git_dirty: Option<bool>,
    frozen_at: Option<&str>,
) -> Result<serde_json::Value, ProductEvalError> {
    let defaults = &bundle.suite.defaults;
    let mut tasks = Vec::new();
    for (case, rel) in bundle.cases.iter().zip(bundle.suite.tasks.iter()) {
        let file_path = bundle.dir.join(rel);
        let text = std::fs::read_to_string(&file_path)
            .map_err(|e| ProductEvalError(format!("读取任务 {rel} 失败：{e}")))?;
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let file_sha = format!("{:x}", hasher.finalize());
        tasks.push(serde_json::json!({
            "id": case.id,
            "file": rel,
            "sha256": file_sha,
            "category": case.category.as_str(),
            "repetitions": case.effective_repetitions(defaults, None),
            "timeout_secs": case.effective_timeout_secs(defaults),
            "max_model_calls": case.effective_max_model_calls(defaults),
            "permissions_sha256": permissions_hash(case),
        }));
    }
    let total_cells = tasks
        .iter()
        .filter_map(|t| t.get("repetitions").and_then(serde_json::Value::as_u64))
        .sum::<u64>() as usize;
    Ok(serde_json::json!({
        "schema_version": FREEZE_SCHEMA_VERSION,
        "suite": {
            "name": bundle.suite.name,
            "revision_sha256": suite_hash(bundle),
        },
        "defaults": {
            "repetitions": defaults.repetitions,
            "timeout_secs": defaults.timeout_secs,
            "max_model_calls": defaults.max_model_calls,
        },
        "tasks": tasks,
        "permissions": { "policy": "default deny; allow_read/allow_write/allow_commands 逐调用强制，见 tasks[*].permissions_sha256" },
        "budget": {
            "single_cells": total_cells,
            "multi_cells": total_cells,
            "shared": "单/多 Agent 同任务同输入同权限同预算同检查器",
        },
        "model": {
            "id": model,
            "base_url": base_url,
        },
        "version": {
            "git_commit": git_commit,
            "git_dirty": git_dirty,
        },
        "frozen_at": frozen_at.unwrap_or(&now_rfc3339()),
        "frozen_by": "lane2-product-eval",
    }))
}

/// 校验当前套件是否与 freeze.json 一致（输入/检查器/权限/预算/版本哈希）。
/// 返回问题清单；空 = 冻结未被破坏。
pub fn verify_freeze(
    bundle: &SuiteBundle,
    freeze: &serde_json::Value,
    current_model: Option<&str>,
) -> Vec<String> {
    let mut issues = Vec::new();
    let Some(schema) = freeze
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    else {
        issues.push("freeze.json 缺少 schema_version".to_string());
        return issues;
    };
    if schema != FREEZE_SCHEMA_VERSION as u64 {
        issues.push(format!(
            "freeze.json schema_version={schema} 不兼容（期望 {FREEZE_SCHEMA_VERSION}）"
        ));
        return issues;
    }
    let freeze_suite = freeze.get("suite");
    let freeze_name = freeze_suite
        .and_then(|s| s.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if freeze_name != bundle.suite.name {
        issues.push(format!(
            "freeze 套件名「{freeze_name}」与当前「{}」不一致",
            bundle.suite.name
        ));
    }
    let freeze_revision = freeze_suite
        .and_then(|s| s.get("revision_sha256"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let current_revision = suite_hash(bundle);
    if freeze_revision != current_revision {
        issues.push(format!(
            "套件修订哈希不一致：freeze={freeze_revision} 当前={current_revision}（任务输入/检查器/权限/预算已漂移；须重新生成 freeze.json 后建立新批次）"
        ));
    }
    // 任务级文件哈希核对（防同修订下的文件级漂移；正常应被 revision 覆盖）。
    let freeze_tasks = freeze
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let task_by_rel: std::collections::BTreeMap<String, &ProductEvalCase> = bundle
        .cases
        .iter()
        .zip(bundle.suite.tasks.iter())
        .map(|(case, rel)| (rel.clone(), case))
        .collect();
    for entry in &freeze_tasks {
        let Some(rel) = entry.get("file").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let file_path = bundle.dir.join(rel);
        let current_sha = std::fs::read_to_string(&file_path).ok().map(|text| {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            format!("{:x}", hasher.finalize())
        });
        let freeze_sha = entry.get("sha256").and_then(serde_json::Value::as_str);
        if let (Some(freeze_sha), Some(current_sha)) = (freeze_sha, &current_sha) {
            if freeze_sha != current_sha {
                issues.push(format!(
                    "任务文件 {rel} 哈希漂移：freeze={freeze_sha} 当前={current_sha}"
                ));
            }
        } else {
            issues.push(format!("freeze 中任务 {rel} 缺少 sha256 或文件缺失"));
        }
        // 权限哈希。
        if let Some(case) = task_by_rel.get(rel) {
            if let (Some(frozen_perm), Some(id)) = (
                entry
                    .get("permissions_sha256")
                    .and_then(serde_json::Value::as_str),
                entry.get("id").and_then(serde_json::Value::as_str),
            ) {
                let current_perm = permissions_hash(case);
                if frozen_perm != current_perm {
                    issues.push(format!(
                        "任务 {id}（{rel}）权限哈希漂移：freeze={frozen_perm} 当前={current_perm}（allow_read/allow_write/allow_commands 已变更）"
                    ));
                }
            }
        }
    }
    // 数量核对。
    if freeze_tasks.len() != bundle.cases.len() {
        issues.push(format!(
            "freeze 任务数 {} 与当前套件 {} 不一致",
            freeze_tasks.len(),
            bundle.cases.len()
        ));
    }
    // 模型配置冻结核对（环境变量为当前配置来源）。
    if let Some(model_id) = freeze
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(serde_json::Value::as_str)
    {
        match current_model {
            Some(current) if current != model_id => {
                issues.push(format!(
                    "模型配置已漂移：freeze={model_id} 当前={current}（成绩只对冻结模型有效）"
                ));
            }
            Some(_) => {}
            None => issues.push(format!(
                "freeze 冻结模型 {model_id}，但当前未解析出模型（环境配置缺失）"
            )),
        }
    }
    issues
}

/// 从 freeze.json 文本解析（校验 schema 版本）。
pub fn parse_freeze(text: &str) -> Result<serde_json::Value, ProductEvalError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| ProductEvalError(format!("freeze.json 解析失败：{e}")))?;
    Ok(value)
}
