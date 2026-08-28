//! 真实单 Agent 产品评测执行器（V1-R1 第二天 · 第一路）。
//!
//! 与 dry 参考回放 / live 生成式执行器共用 [`CaseExecutor`] 契约，但不再让模型
//! 直接输出文件块：每次运行在独立沙盒工作区创建独立 [`Session`]，经真实的
//! [`Agent::run_turn()`]（模型循环 + 工具注册表 + 权限策略 + 审批链）完成任务。
//!
//! 权限收口分两层，均按任务声明逐调用强制（不继承任何仓库级权限）：
//! - [`ProductEvalApprover`]：Policy 判为 Ask 的 Write/Execute/Inject 一律经它裁决；
//!   write_file 只放行 `allow_write` 范围，run_command 只放行任务命令白名单，
//!   其余工具一律 Deny（未注册/意外工具调用同样被拒）。
//! - [`ProductEvalScopeTools`]：最小工具集（读/列/搜/写/受控命令），读取在工具内层
//!   复核 `allow_read`，写入复核 `allow_write`——即使未来 Policy 放宽也不越界。
//!
//! 遥测从 [`TurnEvent`] 流采集：模型调用数、工具调用（含真实实参摘要）、审批拒绝、
//! 工具失败、失败步骤、耗时与 token 用量（provider 快照差值）；取消/预算超限经
//! 同一 abort 令牌收口——命中后 run_turn 在下一检查点停止，不再调用模型或工具。

use crate::agent::{Agent, AgentConfig, TurnEvent};
use crate::error::AgentError;
use crate::gateway::{ModelProvider, TokenUsage};
use crate::permissions::{Approver, Decision, PermissionRequest, Policy};
use crate::product_eval::{
    in_scope, sanitize_rel_path, CaseExecutor, EvalCategory, ExecContext, ProductEvalCase,
    RawExecOutcome,
};
use crate::session::Session;
use crate::tools::{required_string, Tool, ToolContext, ToolRegistry, ToolSpec};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// 工具结果文本上限（与 agent.rs 的 MAX_TOOL_RESULT_CHARS 口径一致）。
const TOOL_RESULT_TEXT_CAP: usize = 50_000;

/// 命令链式/重定向元字符检测：白名单按"单命令"收口，禁止通过管道、
/// 顺序执行、后台执行或重定向把放行命令拼接成任意命令。
pub(crate) fn command_has_shell_metachars(command: &str) -> bool {
    command
        .chars()
        .any(|c| matches!(c, '&' | '|' | ';' | '<' | '>' | '`' | '\n' | '\r'))
        || command.contains("$(")
}

// ---------------------------------------------------------------------------
// 任务权限范围
// ---------------------------------------------------------------------------

/// 一次运行的任务级权限范围（从 [`ProductEvalCase`] 提取，逐调用强制）。
#[derive(Debug, Clone, Default)]
pub struct ProductEvalScope {
    pub allow_read: Vec<String>,
    pub allow_write: Vec<String>,
    pub allow_commands: Vec<String>,
}

impl ProductEvalScope {
    pub fn from_case(case: &ProductEvalCase) -> Self {
        Self {
            allow_read: case.allow_read.clone(),
            allow_write: case.allow_write.clone(),
            allow_commands: case.allow_commands.clone(),
        }
    }

    fn read_ok(&self, rel: &str) -> bool {
        in_scope(&self.allow_read, rel)
    }

    /// 目录是否可列：目录本身命中规则，或其下任一文件可能命中（探测子路径）。
    fn dir_readable(&self, rel: &str) -> bool {
        if rel.is_empty() {
            return true; // 根目录仅展示顶层结构，具体文件读取仍受 allow_read 约束
        }
        in_scope(&self.allow_read, rel) || in_scope(&self.allow_read, &format!("{rel}/.probe"))
    }

    fn write_ok(&self, rel: &str) -> bool {
        in_scope(&self.allow_write, rel)
    }

    /// 命令是否放行：白名单整条精确或前缀（含后续参数）命中；空白名单 = 全拒。
    pub fn command_ok(&self, command: &str) -> bool {
        let cmd = command.trim();
        if cmd.is_empty() || self.allow_commands.is_empty() {
            return false;
        }
        if command_has_shell_metachars(cmd) {
            return false;
        }
        self.allow_commands.iter().any(|entry| {
            let entry = entry.trim();
            cmd == entry
                || cmd
                    .strip_prefix(entry)
                    .is_some_and(|rest| rest.starts_with(' '))
        })
    }
}

/// 以沙盒为基座归一化相对路径（拒绝绝对路径/越界/空转义）。
fn normalize_rel(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == "./" {
        return Ok(String::new());
    }
    sanitize_rel_path(trimmed)
}

fn cap_text(text: &str) -> String {
    if text.chars().count() <= TOOL_RESULT_TEXT_CAP {
        text.to_string()
    } else {
        let head: String = text.chars().take(TOOL_RESULT_TEXT_CAP).collect();
        format!("{head}\n…（截断：结果超过 {TOOL_RESULT_TEXT_CAP} 字符）")
    }
}

// ---------------------------------------------------------------------------
// 运行日志与遥测（从 TurnEvent 流 + 工具内层采集）
// ---------------------------------------------------------------------------

/// 单次运行的遥测快照（随执行器留存，测试与报告均可取用）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub model_calls: u32,
    pub tool_calls: u32,
    pub approval_requests: u32,
    pub denials: Vec<String>,
    pub tool_errors: Vec<String>,
    /// 真实工具调用轨迹（工具名 + 实参摘要 + 结果），按发生顺序。
    pub tool_log: Vec<String>,
    pub final_text: Option<String>,
    pub duration_ms: u64,
    pub aborted: bool,
    /// 模型调用预算超限触发中止。
    pub budget_exceeded: bool,
}

#[derive(Debug, Clone, Default)]
struct RunState {
    model_calls: u32,
    tool_calls: u32,
    approval_requests: u32,
    denials: Vec<String>,
    tool_errors: Vec<String>,
    final_text: Option<String>,
    budget_exceeded: bool,
}

/// 工具内层共享的调用轨迹（工具名 + 实参摘要 + 结果）。
type ToolLog = Arc<Mutex<Vec<String>>>;

fn log_tool(log: &ToolLog, entry: String) {
    if let Ok(mut guard) = log.lock() {
        guard.push(entry);
    }
}

// ---------------------------------------------------------------------------
// 最小受控工具集（allow_read / allow_write / 命令白名单逐调用强制）
// ---------------------------------------------------------------------------

/// 读取工具：allow_read 逐调用强制（评测专用最小工具集，公开供测试直接构造）。
pub struct ScopeReadFile {
    scope: Arc<ProductEvalScope>,
    log: ToolLog,
}

impl ScopeReadFile {
    pub fn new(scope: Arc<ProductEvalScope>, log: ToolLog) -> Self {
        Self { scope, log }
    }
}

#[async_trait]
impl Tool for ScopeReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "读取工作区内允许读取范围的文件文本（超出 allow_read 直接拒绝）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let raw = required_string(&args, "path")?;
        let rel = normalize_rel(&raw)?;
        if !self.scope.read_ok(&rel) {
            log_tool(
                &self.log,
                format!("read_file {raw} → DENIED（超出 allow_read）"),
            );
            return Err(format!("read_denied:{raw}（超出 allow_read 范围）"));
        }
        let target = ctx.workspace.join(&rel);
        let content =
            std::fs::read_to_string(&target).map_err(|e| format!("读取 {raw} 失败：{e}"))?;
        log_tool(
            &self.log,
            format!("read_file {rel}（{} 字符）", content.chars().count()),
        );
        Ok(json!({ "path": rel, "content": cap_text(&content) }))
    }
}

/// 列目录工具：目录可读性经 allow_read 探测。
pub struct ScopeListDir {
    scope: Arc<ProductEvalScope>,
    log: ToolLog,
}

impl ScopeListDir {
    pub fn new(scope: Arc<ProductEvalScope>, log: ToolLog) -> Self {
        Self { scope, log }
    }
}

#[async_trait]
impl Tool for ScopeListDir {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "列出工作区内目录的一级条目（根目录可直接列出）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let raw = required_string(&args, "path")?;
        let rel = normalize_rel(&raw)?;
        if !self.scope.dir_readable(&rel) {
            log_tool(
                &self.log,
                format!("list_dir {raw} → DENIED（超出 allow_read）"),
            );
            return Err(format!("read_denied:{raw}（超出 allow_read 范围）"));
        }
        let target = ctx.workspace.join(&rel);
        let mut entries = Vec::new();
        let reader = std::fs::read_dir(&target).map_err(|e| format!("读取目录 {raw} 失败：{e}"))?;
        for entry in reader {
            let entry = entry.map_err(|e| format!("读取目录 {raw} 失败：{e}"))?;
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "is_dir": entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
            }));
        }
        log_tool(
            &self.log,
            format!("list_dir {}（{} 项）", display_rel(&rel), entries.len()),
        );
        Ok(json!({ "path": display_rel(&rel), "entries": entries }))
    }
}

/// 搜索工具：结果仅保留 allow_read 范围内的路径。
pub struct ScopeSearchFiles {
    scope: Arc<ProductEvalScope>,
    log: ToolLog,
}

impl ScopeSearchFiles {
    pub fn new(scope: Arc<ProductEvalScope>, log: ToolLog) -> Self {
        Self { scope, log }
    }
}

#[async_trait]
impl Tool for ScopeSearchFiles {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_files".into(),
            description: "按文件名关键字递归搜索工作区（结果仅保留 allow_read 范围内的路径）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
                "required": ["pattern"]
            }),
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let pattern = required_string(&args, "pattern")?.to_lowercase();
        let mut matches = Vec::new();
        collect_matches(ctx.workspace, ctx.workspace, &pattern, 0, &mut matches);
        let scoped: Vec<String> = matches
            .into_iter()
            .filter(|rel| self.scope.read_ok(rel))
            .collect();
        log_tool(
            &self.log,
            format!(
                "search_files {pattern} → {} 个命中（allow_read 过滤后）",
                scoped.len()
            ),
        );
        Ok(json!({ "pattern": pattern, "matches": scoped }))
    }
}

/// 递归文件名匹配（深度 8 / 数量 200 上限，与内置工具口径一致）。
fn collect_matches(
    root: &Path,
    dir: &Path,
    pattern: &str,
    depth: usize,
    matches: &mut Vec<String>,
) {
    if depth > 8 || matches.len() >= 200 {
        return;
    }
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            collect_matches(root, &path, pattern, depth + 1, matches);
        } else if entry
            .file_name()
            .to_string_lossy()
            .to_lowercase()
            .contains(pattern)
        {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            matches.push(rel);
        }
        if matches.len() >= 200 {
            break;
        }
    }
}

/// 写文件工具：allow_write 逐调用强制（审批链之外的第二道闸）。
pub struct ScopeWriteFile {
    scope: Arc<ProductEvalScope>,
    log: ToolLog,
}

impl ScopeWriteFile {
    pub fn new(scope: Arc<ProductEvalScope>, log: ToolLog) -> Self {
        Self { scope, log }
    }
}

#[async_trait]
impl Tool for ScopeWriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "在工作区内写文件（超出 allow_write 直接拒绝；产物必须落在该范围）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let raw = required_string(&args, "path")?;
        let content = required_string(&args, "content")?;
        let rel = normalize_rel(&raw)?;
        if !self.scope.write_ok(&rel) {
            log_tool(
                &self.log,
                format!("write_file {raw} → DENIED（超出 allow_write）"),
            );
            return Err(format!("write_denied:{raw}（超出 allow_write 范围）"));
        }
        let target = ctx.workspace.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败：{e}"))?;
        }
        std::fs::write(&target, &content).map_err(|e| format!("写入 {raw} 失败：{e}"))?;
        log_tool(
            &self.log,
            format!("write_file {rel}（{} 字节）", content.len()),
        );
        Ok(json!({ "path": rel, "bytes": content.len() }))
    }
}

/// 受控命令工具：命令白名单逐调用强制，60 秒超时。
pub struct ScopeRunCommand {
    scope: Arc<ProductEvalScope>,
    log: ToolLog,
}

impl ScopeRunCommand {
    pub fn new(scope: Arc<ProductEvalScope>, log: ToolLog) -> Self {
        Self { scope, log }
    }
}

#[async_trait]
impl Tool for ScopeRunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description:
                "在沙盒内执行白名单内的单条命令（未列入 allow_commands 一律拒绝，60 秒超时）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" }
                },
                "required": ["command"]
            }),
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let command = required_string(&args, "command")?;
        let cwd = match args.get("cwd").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => ctx.workspace.join(normalize_rel(raw)?),
            _ => ctx.workspace.to_path_buf(),
        };
        if !self.scope.command_ok(&command) {
            log_tool(
                &self.log,
                format!("run_command {command} → DENIED（不在任务命令白名单）"),
            );
            return Err(format!(
                "command_denied:{command}（未列入任务命令白名单或含链式元字符）"
            ));
        }
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new("cmd")
                .args(["/C", command.trim()])
                .current_dir(&cwd)
                .output(),
        )
        .await
        .map_err(|_| format!("command_timeout:{command}（超过 60 秒上限）"))?
        .map_err(|e| format!("command_spawn:{command}：{e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        log_tool(
            &self.log,
            format!(
                "run_command {command} → exit {}",
                output.status.code().unwrap_or(-1)
            ),
        );
        Ok(json!({
            "exit_code": output.status.code(),
            "stdout": cap_text(stdout.trim_end()),
            "stderr": cap_text(stderr.trim_end()),
        }))
    }
}

fn display_rel(rel: &str) -> &str {
    if rel.is_empty() {
        "."
    } else {
        rel
    }
}

/// 评测专用最小注册表：仅注册范围受控的五个工具（空注册表起步）。
fn scope_registry(scope: Arc<ProductEvalScope>, log: ToolLog) -> ToolRegistry {
    let mut registry = ToolRegistry::empty();
    registry.register(ScopeReadFile {
        scope: Arc::clone(&scope),
        log: Arc::clone(&log),
    });
    registry.register(ScopeListDir {
        scope: Arc::clone(&scope),
        log: Arc::clone(&log),
    });
    registry.register(ScopeSearchFiles {
        scope: Arc::clone(&scope),
        log: Arc::clone(&log),
    });
    registry.register(ScopeWriteFile {
        scope: Arc::clone(&scope),
        log: Arc::clone(&log),
    });
    registry.register(ScopeRunCommand { scope, log });
    registry
}

// ---------------------------------------------------------------------------
// 审批器：Write/Execute 的 Ask 一律收口为 Allow(范围内) / Deny(越界)
// ---------------------------------------------------------------------------

/// 产品评测审批器：只依据任务声明的 allow_write 与命令白名单裁决，
/// 不继承任何仓库级权限；其余一切 Write/Execute/Inject 工具调用一律 Deny。
pub struct ProductEvalApprover {
    scope: Arc<ProductEvalScope>,
    log: ToolLog,
}

impl ProductEvalApprover {
    pub fn new(scope: Arc<ProductEvalScope>, log: ToolLog) -> Self {
        Self { scope, log }
    }
}

#[async_trait]
impl Approver for ProductEvalApprover {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        let verdict = match request.tool.as_str() {
            "write_file" => args_path_in_scope(request, &self.scope),
            "run_command" => args_command_whitelisted(request, &self.scope),
            _ => false, // 评测期间未声明的工具（桌面/浏览器/注入等）一律拒绝
        };
        if verdict {
            Decision::Allow
        } else {
            log_tool(
                &self.log,
                format!("approver DENY {}（{}）", request.tool, request.reason),
            );
            Decision::Deny
        }
    }
}

fn args_path_in_scope(request: &PermissionRequest, scope: &ProductEvalScope) -> bool {
    let raw = request
        .args
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match normalize_rel(raw) {
        Ok(rel) => scope.write_ok(&rel),
        Err(_) => false,
    }
}

fn args_command_whitelisted(request: &PermissionRequest, scope: &ProductEvalScope) -> bool {
    let command = request
        .args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    scope.command_ok(command)
}

// ---------------------------------------------------------------------------
// 提示词
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str =
    "你是产品评测中的单 Agent 执行者，在独立沙盒工作区内完成任务。硬性规则：\n\
1. 只使用提供的工具读写工作区内的文件；不请求网络，不猜测输入中不存在的内容。\n\
2. 只在允许写入范围内写文件；必须生成全部预期产物，内容完整、非空、符合任务格式要求。\n\
3. 命令只能执行任务白名单内的单条命令；未列入白名单的命令一律不要尝试。\n\
4. 逐步执行：先列目录/读输入材料，再完成任务，最后用 write_file 落盘产物。\n\
5. 完成后输出一段简短总结（做了什么、产物在哪），不要输出文件块协议。";

fn build_user_prompt(case: &ProductEvalCase) -> String {
    let mut sections = Vec::new();
    sections.push(format!(
        "任务（{}）：{}",
        case.category.as_str(),
        case.title
    ));
    sections.push(format!("任务要求：\n{}", case.instruction.trim()));
    if case.inputs.is_empty() {
        sections.push("输入材料：无（直接完成任务）。".to_string());
    } else {
        let mut text = String::from("输入材料（工作区内相对路径，先 read_file 阅读）：");
        for input in &case.inputs {
            text.push_str(&format!("\n- {}", input.path));
        }
        sections.push(text);
    }
    let mut text = String::from("预期产物（必须全部用 write_file 生成，路径精确一致，内容非空）：");
    for artifact in &case.expected_artifacts {
        text.push_str(&format!("\n- {artifact}"));
    }
    sections.push(text);

    sections.push(format!(
        "允许读取：{}",
        if case.allow_read.is_empty() {
            "（无，只可列出根目录）".to_string()
        } else {
            case.allow_read.join("、")
        }
    ));
    sections.push(format!(
        "允许写入：{}",
        if case.allow_write.is_empty() {
            "（无）".to_string()
        } else {
            case.allow_write.join("、")
        }
    ));
    sections.push(if case.allow_commands.is_empty() {
        "命令：本任务禁止执行任何命令（不要调用 run_command）。".to_string()
    } else {
        format!(
            "命令白名单（整条或前缀一致才可执行）：{}",
            case.allow_commands.join("；")
        )
    });

    sections.push(match case.category {
        EvalCategory::Code => "执行建议：先 list_dir/read_file 了解输入与结构，再实现，产物需能通过静态检查；白名单内的测试命令可用来自检。".to_string(),
        EvalCategory::Research => "执行建议：完整读取全部输入材料，提炼/结构化后写入产物，引用材料中的事实而非编造。".to_string(),
        EvalCategory::Document => "执行建议：读取输入材料与格式要求，按要求撰写文档并写入产物。".to_string(),
    });
    sections.push("完成后输出简短总结。".to_string());
    sections.join("\n\n")
}

// ---------------------------------------------------------------------------
// 执行器
// ---------------------------------------------------------------------------

/// 真实单 Agent 执行器：Provider 注入（与 live 生成器同一构建入口），
/// 每个单元格独立 Session + 沙盒 + 最小受控工具集 + 任务级审批器。
pub struct SingleAgentExecutor {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    /// 最近一次运行的遥测快照（测试/报告取证用）。
    last_telemetry: Mutex<Option<TelemetrySnapshot>>,
}

impl SingleAgentExecutor {
    pub fn new(provider: Arc<dyn ModelProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
            last_telemetry: Mutex::new(None),
        }
    }

    /// 取走最近一次运行的遥测快照（每次 execute 后刷新）。
    pub fn take_last_telemetry(&self) -> Option<TelemetrySnapshot> {
        self.last_telemetry
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }
}

/// 清点沙盒产物：allow_write 范围内、非空文件登记为 artifact refs。
fn collect_sandbox_artifacts(sandbox: &Path, scope: &ProductEvalScope) -> Vec<String> {
    let mut artifacts = Vec::new();
    collect_write_scoped_files(sandbox, sandbox, 0, scope, &mut artifacts);
    artifacts.sort();
    artifacts
}

fn collect_write_scoped_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    scope: &ProductEvalScope,
    out: &mut Vec<String>,
) {
    if depth > 8 || out.len() >= 200 {
        return;
    }
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_write_scoped_files(root, &path, depth + 1, scope, out);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let non_empty = std::fs::metadata(&path)
                .map(|m| m.len() > 0)
                .unwrap_or(false);
            if non_empty && scope.write_ok(&rel) && !out.contains(&rel) {
                out.push(rel);
            }
        }
        if out.len() >= 200 {
            break;
        }
    }
}

#[async_trait]
impl CaseExecutor for SingleAgentExecutor {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        let started = std::time::Instant::now();
        // 取消已命中：直接返回，不进入任何模型/工具调用。
        if ctx.cancelled() {
            return RawExecOutcome {
                aborted: true,
                ..RawExecOutcome::default()
            };
        }
        let usage_before = self.provider.usage_snapshot();
        let scope = Arc::new(ProductEvalScope::from_case(ctx.case));
        let tool_log: ToolLog = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(RunState::default()));
        let max_model_calls = ctx.max_model_calls;
        let abort = Arc::clone(&ctx.cancel);

        let registry = scope_registry(Arc::clone(&scope), Arc::clone(&tool_log));
        let policy = Policy::new(ctx.sandbox);
        // 回合数随模型调用预算走：每回合至多一次模型调用（工具回合除外）。
        let config = AgentConfig {
            max_turns: (max_model_calls as usize + 2).clamp(4, 24),
            ..AgentConfig::default()
        };
        let agent = Agent::new(self.provider.clone(), registry, policy, config);
        let approver = ProductEvalApprover::new(Arc::clone(&scope), Arc::clone(&tool_log));

        let mut session = Session::new(
            ctx.sandbox,
            self.model.as_str(),
            Some(SYSTEM_PROMPT.to_string()),
        );
        let prompt = build_user_prompt(ctx.case);

        // TurnEvent 遥测：预算超限即置取消——run_turn 在下一检查点停止（不再调用模型/工具）。
        let state_for_events = Arc::clone(&state);
        let abort_for_events = Arc::clone(&abort);
        let mut on_event = |event: &TurnEvent| {
            let mut state = state_for_events.lock().expect("遥测锁中毒");
            match event {
                TurnEvent::ModelCall => {
                    state.model_calls += 1;
                    if state.model_calls > max_model_calls {
                        state.budget_exceeded = true;
                        abort_for_events.store(true, Ordering::Relaxed);
                    }
                }
                TurnEvent::ToolStart { .. } => state.tool_calls += 1,
                TurnEvent::ToolResult {
                    tool, ok, error, ..
                } => {
                    if !*ok {
                        let error = error.clone().unwrap_or_default();
                        if is_denial(&error) {
                            state.denials.push(format!("{tool}: {error}"));
                        } else {
                            state.tool_errors.push(format!("{tool}: {error}"));
                        }
                    }
                }
                TurnEvent::PermissionRequest(_) => state.approval_requests += 1,
                TurnEvent::Final { text } => state.final_text = Some(text.clone()),
                TurnEvent::TokenDelta { .. } | TurnEvent::Compaction { .. } => {}
            }
        };

        let result = agent
            .run_turn(
                &mut session,
                &prompt,
                &approver,
                abort.as_ref(),
                &mut on_event,
            )
            .await;

        // 遥测 → 失败步骤登记（journal 的 failed_steps 全程保留）。
        // 审批拒绝不产生 ToolResult 事件（agent 的 Deny 分支直接返回错误），
        // 因此拒绝证据取自审批器写入共享日志的 "approver DENY" 条目。
        let (snapshot_state, log_entries) = {
            let state_guard = state.lock().expect("遥测锁中毒");
            let log_entries = tool_log
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            (state_guard.clone(), log_entries)
        };
        let mut denials = snapshot_state.denials.clone();
        for entry in &log_entries {
            if entry.starts_with("approver DENY") && !denials.contains(entry) {
                denials.push(entry.clone());
            }
        }
        for denial in &denials {
            ctx.record_failed_step(format!("denied:{denial}"));
        }
        for tool_error in &snapshot_state.tool_errors {
            ctx.record_failed_step(format!("tool_error:{tool_error}"));
        }
        if snapshot_state.budget_exceeded {
            ctx.record_failed_step(format!(
                "budget:模型调用预算耗尽（max_model_calls={max_model_calls}）"
            ));
        }

        // 沙盒清点：Agent 经工具落盘的文件补录进 artifact refs（与 write_file 口径一致）。
        let scope_ref = &scope;
        for rel in collect_sandbox_artifacts(ctx.sandbox, scope_ref) {
            ctx.record_artifact(rel);
        }

        // 预期产物齐全性闸门（独立于检查器）：缺任一产物即执行器错误。
        let mut missing = Vec::new();
        for artifact in &ctx.case.expected_artifacts {
            let path = match sanitize_rel_path(artifact) {
                Ok(rel) => ctx.sandbox.join(rel),
                Err(_) => continue,
            };
            let present = std::fs::metadata(&path)
                .map(|meta| meta.is_file() && meta.len() > 0)
                .unwrap_or(false);
            if !present {
                missing.push(artifact.clone());
            }
        }
        if !missing.is_empty() {
            let desc = format!("missing_artifact:预期产物缺失：{}", missing.join("、"));
            ctx.record_failed_step(desc.clone());
        }

        let usage_snapshot = self.provider.usage_snapshot();
        let usage = TokenUsage {
            prompt_tokens: usage_snapshot
                .prompt_tokens
                .saturating_sub(usage_before.prompt_tokens),
            completion_tokens: usage_snapshot
                .completion_tokens
                .saturating_sub(usage_before.completion_tokens),
            total_tokens: usage_snapshot
                .total_tokens
                .saturating_sub(usage_before.total_tokens),
        };

        let mut outcome = RawExecOutcome {
            aborted: false,
            error: None,
            model_calls: snapshot_state.model_calls,
            usage,
            usage_known: usage.total_tokens > 0,
            retries: 0,
            tool_log: log_entries.clone(),
        };
        match result {
            Err(AgentError::Aborted) => {
                outcome.aborted = true;
            }
            Err(other) => {
                outcome.error = Some(format!("agent turn 失败：{other}"));
            }
            Ok(_) => {}
        }
        // 预算超限（或取消信号在回合收尾前命中）：本单元格一律按中止计，
        // 不进入检查器判定——即使产物恰好齐备也不能算预算内通过。
        if snapshot_state.budget_exceeded || ctx.cancelled() {
            outcome.aborted = true;
        }
        if !missing.is_empty() && !outcome.aborted {
            outcome.error = Some(format!("预期产物缺失：{}", missing.join("、")));
        }

        let telemetry = TelemetrySnapshot {
            model_calls: snapshot_state.model_calls,
            tool_calls: snapshot_state.tool_calls,
            approval_requests: snapshot_state.approval_requests,
            denials,
            tool_errors: snapshot_state.tool_errors,
            tool_log: log_entries,
            final_text: snapshot_state.final_text,
            duration_ms: started.elapsed().as_millis() as u64,
            aborted: outcome.aborted,
            budget_exceeded: snapshot_state.budget_exceeded,
        };
        if let Ok(mut guard) = self.last_telemetry.lock() {
            *guard = Some(telemetry);
        }
        outcome
    }
}

/// 失败工具结果中的权限拒绝特征（工具内层 + 审批链两层口径）。
fn is_denial(error: &str) -> bool {
    let denied_prefixes = [
        "permission denied",
        "read_denied:",
        "write_denied:",
        "command_denied:",
        "command_timeout:",
        "路径越界",
        "命令命中危险黑名单",
        "工具已被禁用",
    ];
    denied_prefixes
        .iter()
        .any(|prefix| error.starts_with(prefix))
}
