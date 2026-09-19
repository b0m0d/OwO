use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Read,
    Write,
    Execute,
    Inject,
}

impl Level {
    pub fn label(&self) -> &'static str {
        match self {
            Level::Read => "read",
            Level::Write => "write",
            Level::Execute => "execute",
            Level::Inject => "inject",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionProfile {
    /// 只允许宿主验证的只读操作。
    ReadOnly,
    /// 工作区内普通读写自动允许；执行、联网、UI 控制、越界和破坏性操作询问。
    Workspace,
    /// 审批 Agent 可以收紧或代批「可代批」操作，但不能突破 OS 沙箱与 deny 规则。
    AutoReview,
    /// 减少询问，但不绕过审计、秘密脱敏和不可恢复操作确认。
    FullAccess,
    /// 用户按工具、路径、主机和时效组合规则。
    Custom,
}

impl PermissionProfile {
    pub fn label(self) -> &'static str {
        match self {
            PermissionProfile::ReadOnly => "read_only",
            PermissionProfile::Workspace => "workspace",
            PermissionProfile::AutoReview => "auto_review",
            PermissionProfile::FullAccess => "full_access",
            PermissionProfile::Custom => "custom",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read_only" => Some(Self::ReadOnly),
            "workspace" => Some(Self::Workspace),
            "auto_review" => Some(Self::AutoReview),
            "full_access" => Some(Self::FullAccess),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub request_id: String,
    pub tool: String,
    pub args: Value,
    pub level: Level,
    pub reason: String,
    /// §7.2/§7.4：风险说明（审批卡展示）。工具未声明风险信息时给出统一提示；
    /// 已知内置工具无额外说明时省略该字段（保持既有 JSON 契约形状）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_note: Option<String>,
    /// §5.5：脱敏后的参数视图（秘密字段只显示类型与长度），审批卡默认展示；
    /// 原始 args 仅在开发者详情中展开。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted_args: Option<Value>,
}

impl PermissionRequest {
    /// 统一构造入口：业务模块必须经此创建权限请求，禁止直接拼装 request_id。
    /// request_id 由构造函数生成，保证全局唯一，业务调用点无需关心。
    pub fn new(
        tool: impl Into<String>,
        args: Value,
        level: Level,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            request_id: uuid::Uuid::new_v4().to_string(),
            tool: tool.into(),
            args,
            level,
            reason: reason.into(),
            risk_note: None,
            redacted_args: None,
        }
    }

    /// 串联风险说明；`None` 表示省略该字段（保持既有 JSON 契约形状）。
    pub fn with_risk_note(mut self, note: Option<String>) -> Self {
        self.risk_note = note;
        self
    }

    /// §5.5：attach 脱敏参数视图（见 [`redact_args`]）。
    pub fn with_redacted_args(mut self, redacted: Option<Value>) -> Self {
        self.redacted_args = redacted;
        self
    }

    /// 是否可能含有敏感载荷（token/cookie/header/文件内容等；用于拒绝
    /// 「始终允许此只读动作」选项——§5.4 破坏性操作不允许）。
    pub fn is_destructive(&self) -> bool {
        self.level != Level::Read
            || crate::tool_effects::effect_class_for(&self.tool)
                == crate::tool_effects::EffectClass::Inject
    }
}

/// §5.5 秘密字段识别词根：值为 string 且键匹配任一词根 → 只暴露类型与长度。
const SECRET_KEY_HINTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "cookie",
    "authorization",
    "api_key",
    "apikey",
    "x-api",
    "key",
    "credential",
    "header",
];

/// §5.5 脱敏：递归遍历，string 值且键命中秘密词根 → `{"type":"string","length":N}`；
/// 其余原样。命令参数不整段抹掉（审批卡需要看做了什么），但 url/command 之外的
/// 高危键（如 file 内容、data）单独成对处理：data 只保留长度。
pub fn redact_args(args: &Value) -> Value {
    redact_inner(args)
}

/// 递归脱敏：string/array 值且键命中秘密词根 → 只暴露类型与长度；其余原样。
fn redact_inner(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                let hint = SECRET_KEY_HINTS
                    .iter()
                    .any(|hint| key.to_lowercase().contains(hint));
                let value = if hint {
                    match child {
                        Value::String(text) => {
                            let length = text.chars().count();
                            out.insert(
                                key.clone(),
                                json!({ "type": "string", "length": length, "redacted": true }),
                            );
                            continue;
                        }
                        Value::Array(items) if !items.is_empty() => {
                            let total: usize = items
                                .iter()
                                .filter_map(|item| item.as_str())
                                .map(|text| text.chars().count())
                                .sum();
                            out.insert(
                                key.clone(),
                                json!({ "type": "array", "length": total, "redacted": true }),
                            );
                            continue;
                        }
                        _ => redact_inner(child),
                    }
                } else {
                    redact_inner(child)
                };
                out.insert(key.clone(), value);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_inner).collect()),
        other => other.clone(),
    }
}

/// §5.5 可解释展示：一行为用户总结「将做什么 / 影响哪里 / 能否撤销」。
/// 返回 { action, target, undoable }（UI 与 CLI 共用；无敏感载荷）。
pub fn describe_request(request: &PermissionRequest) -> serde_json::Value {
    let mut action = request.tool.clone();
    let mut target = String::new();
    let mut undoable = false;
    let args = &request.args;
    match request.tool.as_str() {
        "read_file" | "write_file" => {
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                target = path.to_string();
            }
            undoable = request.tool == "write_file";
            action = if request.tool == "read_file" {
                "读取文件".to_string()
            } else {
                "写入文件".to_string()
            };
        }
        "run_command" => {
            if let Some(command) = args.get("command").and_then(Value::as_str) {
                let parts: Vec<&str> = command.split_whitespace().collect();
                if let Some(program) = parts.first() {
                    action = format!("执行命令：{program}");
                }
                target = parts
                    .iter()
                    .skip(1)
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");
            }
        }
        _ => {
            if let Some(url) = args.get("url").and_then(Value::as_str) {
                let domain = url
                    .strip_prefix("https://")
                    .or_else(|| url.strip_prefix("http://"))
                    .and_then(|rest| rest.split(['/', '?', '#']).next())
                    .unwrap_or(url);
                action = format!("联网访问：{domain}");
                target = url.to_string();
            }
        }
    }
    if !request.reason.starts_with("拒绝") {
        // 文件写与命令执行一般可通过 diff/revert 撤销（会话快照）。注入不可逆。
        undoable = undoable || request.tool == "run_command";
    }
    json!({
        "action": action,
        "target": target,
        "undoable": undoable,
        "risk": request.risk_note.clone().unwrap_or_default(),
    })
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, request: &PermissionRequest) -> Decision;
}

/// 测试/自动化用：统一放行或拒绝。
pub struct AutoApprover {
    pub allow: bool,
}

#[async_trait]
impl Approver for AutoApprover {
    async fn decide(&self, _request: &PermissionRequest) -> Decision {
        if self.allow {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }
}

/// 权限策略：deny 优先，其次 profile 档位与授权记忆（grant），最后 ask。
/// 作用域：所有文件/命令路径必须位于 workspace 内。
pub struct Policy {
    workspace: PathBuf,
    deny_command_fragments: Vec<String>,
    /// 运行时追加的危险命令片段（热生效，与基础列表合并判断）。
    runtime_deny: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    read_only: Arc<AtomicBool>,
    /// §5.3 权限档位（默认 Workspace：工作区内普通读写自动允许，
    /// 执行/联网/UI 控制/越界/破坏性操作询问）。
    profile: std::sync::Arc<std::sync::Mutex<PermissionProfile>>,
    /// §5.4 有作用域、可撤销的授权记忆（用户审批选项生成；命中即放行）。
    grants: std::sync::RwLock<Option<std::sync::Arc<crate::grant_store::GrantStore>>>,
    /// §4.5.3 结构化 profile（权限中心提交）。`None` = 没有显式结构化配置，
    /// 判定完全按档位走（保持既有行为，老调用点零改动）。
    ///
    /// 这个维度层**只收紧不放宽**：`extra_denial` 只会给出 Deny；
    /// 而且优先于 Read 放行与 grant 命中——显式拒绝维度不能被授权记忆绕过。
    spec: std::sync::Arc<std::sync::Mutex<Option<crate::permission_spec::PermissionSpec>>>,
}

impl Policy {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            deny_command_fragments: vec![
                "rm -rf".to_string(),
                "sudo".to_string(),
                "shutdown".to_string(),
                "format c:".to_string(),
                "rd /s".to_string(),
                "remove-item -recurse".to_string(),
                "del /s".to_string(),
                "git push".to_string(),
                "git reset --hard".to_string(),
            ],
            runtime_deny: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            read_only: Arc::new(AtomicBool::new(false)),
            profile: std::sync::Arc::new(std::sync::Mutex::new(PermissionProfile::Workspace)),
            grants: std::sync::RwLock::new(None),
            spec: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 只读策略（Plan 模式）：写/执行/注入一律拒绝（档位=ReadOnly）。
    pub fn read_only(workspace: impl Into<PathBuf>) -> Self {
        let policy = Self::new(workspace);
        policy.read_only.store(true, Ordering::Relaxed);
        if let Ok(mut profile) = policy.profile.lock() {
            *profile = PermissionProfile::ReadOnly;
        }
        policy
    }

    /// §5.3 注入授权记忆存储（审批卡选项持久化；重复注入覆盖引用）。
    pub fn with_grants(mut self, grants: std::sync::Arc<crate::grant_store::GrantStore>) -> Self {
        self.grants = std::sync::RwLock::new(Some(grants));
        self
    }

    /// §5.4 运行时注入授权记忆（Agent 已被引用共享时使用；覆盖既有引用）。
    pub fn set_grants(&self, grants: std::sync::Arc<crate::grant_store::GrantStore>) {
        if let Ok(mut current) = self.grants.write() {
            *current = Some(grants);
        }
    }

    /// §11 `/permissions` 视图：读取授权记忆存储（未注入则 None）。
    pub fn grant_store(&self) -> Option<std::sync::Arc<crate::grant_store::GrantStore>> {
        self.grants.read().ok().and_then(|guard| guard.clone())
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.read_only.store(read_only, Ordering::Relaxed);
        if let Ok(mut profile) = self.profile.lock() {
            *profile = if read_only {
                PermissionProfile::ReadOnly
            } else {
                PermissionProfile::Workspace
            };
        }
    }

    /// 运行时切换只读模式（M12 提权为 `pub`：唯一调用方 `agent::set_read_only` 已在
    /// core，`pub(crate)` 跨 crate 后不可达——就是 M8 记档的可见性收缩）。
    pub fn set_read_only_runtime(&self, read_only: bool) {
        self.read_only.store(read_only, Ordering::Relaxed);
        if let Ok(mut profile) = self.profile.lock() {
            *profile = if read_only {
                PermissionProfile::ReadOnly
            } else {
                PermissionProfile::Workspace
            };
        }
    }

    /// §5.3 运行时切换权限档位（UI/CLI/HTTP 统一入口；进程重启后由 settings 恢复）。
    pub fn set_profile(&self, profile: PermissionProfile) {
        if let Ok(mut current) = self.profile.lock() {
            *current = profile;
        }
        self.read_only
            .store(profile == PermissionProfile::ReadOnly, Ordering::Relaxed);
    }

    pub fn profile(&self) -> PermissionProfile {
        self.profile
            .lock()
            .map(|guard| *guard)
            .unwrap_or(PermissionProfile::ReadOnly)
    }

    /// §4.5.3 提交结构化 profile（权限中心唯一写入口）。
    ///
    /// **档位只会被推到只读，永远不会被这次提交放宽**：
    /// - spec 三维全关（等价只读）→ 档位同步为 `ReadOnly`，让 settings 与运行时
    ///   说的是同一件事，不留"档位写着 workspace、实际什么都干不了"的分裂真相；
    /// - 其余情况**不动档位**。收紧由 [`decision`](Self::decision) 里的
    ///   `extra_denial` 层直接生效——它是纯减法，能把 FullAccess 下的命令关掉，
    ///   却不需要（也不应该）把用户的档位旋钮拧松。
    ///
    /// 为什么不做"取最近档位"的自动同步：`AutoReview` 在某些面比 `Workspace` 更严
    /// （工作区内写也要过审批链），任何"按 spec 反推档位"都会把这类更严的档位降级，
    /// 那正好违反"只收紧不放宽"。反推函数 [`crate::permission_spec::PermissionSpec::nearest_profile`]
    /// 因此只用于"是否等价只读"这一个判断。
    pub fn set_spec(&self, spec: crate::permission_spec::PermissionSpec) {
        let forces_read_only = spec.nearest_profile() == PermissionProfile::ReadOnly
            || self.profile() == PermissionProfile::ReadOnly;
        if let Ok(mut current) = self.spec.lock() {
            *current = Some(spec);
        }
        if forces_read_only {
            self.set_profile(PermissionProfile::ReadOnly);
        }
    }

    /// §4.5.3 当前结构化 profile（`None` = 未显式配置，判定只按档位走）。
    pub fn spec(&self) -> Option<crate::permission_spec::PermissionSpec> {
        self.spec.lock().ok().and_then(|guard| guard.clone())
    }

    /// 清除结构化 profile（回到纯档位驱动；权限中心"恢复档位默认"用）。
    pub fn clear_spec(&self) {
        if let Ok(mut current) = self.spec.lock() {
            *current = None;
        }
    }

    /// 追加额外危险命令片段（deny 优先；写入基础列表，构造时静态）。
    pub fn add_deny_command(&mut self, fragment: impl Into<String>) {
        let fragment = fragment.into().to_lowercase();
        if !self.deny_command_fragments.contains(&fragment) {
            self.deny_command_fragments.push(fragment);
        }
    }

    /// 运行时追加危险命令片段（热生效，不重建 Policy；进程重启后由 settings 恢复）。
    pub fn add_runtime_deny(&self, fragment: impl Into<String>) {
        let fragment = fragment.into().to_lowercase();
        if let Ok(mut runtime) = self.runtime_deny.lock() {
            if !runtime.contains(&fragment) {
                runtime.push(fragment);
            }
        }
    }

    /// 整体替换运行时 deny 片段（同样因调用方在 core 而提权为 `pub`）。
    pub fn replace_runtime_deny(&self, fragments: &[String]) {
        if let Ok(mut runtime) = self.runtime_deny.lock() {
            runtime.clear();
            for fragment in fragments {
                let fragment = fragment.to_lowercase();
                if !fragment.is_empty() && !runtime.contains(&fragment) {
                    runtime.push(fragment);
                }
            }
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed)
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// 全部危险命令片段（基础 + 运行时，去重；诊断/可视化用只读访问器）。
    pub fn deny_fragments(&self) -> Vec<String> {
        let runtime = self
            .runtime_deny
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let mut all = self.deny_command_fragments.clone();
        for fragment in runtime {
            if !all.contains(&fragment) {
                all.push(fragment);
            }
        }
        all
    }

    /// 工具 → 权限级别矩阵（内置映射，诊断面板展示）。
    pub fn tool_levels() -> Vec<(String, Level)> {
        [
            ("read_file", Level::Read),
            ("list_dir", Level::Read),
            ("search_files", Level::Read),
            ("screen_ocr", Level::Read),
            ("desktop_window_ocr", Level::Read),
            ("ocr_region", Level::Read),
            ("desktop_foreground", Level::Read),
            ("desktop_window_list", Level::Read),
            ("desktop_wait", Level::Read),
            ("desktop_wait_until", Level::Read),
            ("browser_snapshot", Level::Read),
            ("screen_vision", Level::Read),
            ("vision_verify", Level::Read),
            ("vision_ground", Level::Read),
            ("write_file", Level::Write),
            ("browser_screenshot", Level::Write),
            ("browser_download_image", Level::Write),
            ("run_command", Level::Execute),
            ("browser_navigate", Level::Execute),
            ("browser_search", Level::Execute),
            ("browser_click", Level::Execute),
            ("browser_type", Level::Execute),
            ("browser_press", Level::Execute),
            ("browser_close", Level::Execute),
            ("text.inject", Level::Inject),
            ("clipboard", Level::Inject),
            ("desktop_click", Level::Inject),
            ("desktop_type", Level::Inject),
            ("desktop_key", Level::Inject),
            ("desktop_shortcut", Level::Inject),
            ("desktop_activate", Level::Inject),
            ("desktop_launch", Level::Inject),
            ("desktop_scroll", Level::Inject),
        ]
        .iter()
        .map(|(tool, level)| (tool.to_string(), *level))
        .collect()
    }

    /// §7.2：工具 → 权限级别由 ToolEffect 元数据驱动（注册表 → 内置矩阵 → Execute 兜底）。
    pub fn level_for(tool: &str) -> Level {
        crate::tool_effects::effect_class_for(tool).into()
    }

    /// 解析并校验路径位于 workspace 内（文件可尚不存在，校验父级）。
    pub fn resolve_within_workspace(&self, path: &str) -> Result<PathBuf, String> {
        resolve_within(&self.workspace, path)
    }

    pub fn evaluate(&self, tool: &str, args: &Value) -> PermissionRequest {
        self.evaluate_with_effect(tool, None, args)
    }

    /// §5.1：接收调用方已从 ToolSpec 解析出的 effect（唯一事实源），
    /// 不再回查进程级全局注册表；`effect=None` 时兜底按未登记高风险处理。
    pub fn evaluate_with_effect(
        &self,
        tool: &str,
        effect: Option<&crate::tool_effects::ToolEffect>,
        args: &Value,
    ) -> PermissionRequest {
        let level: Level = effect
            .map(|effect| effect.class)
            .unwrap_or_else(|| crate::tool_effects::effect_class_for(tool))
            .into();
        let reason = match tool {
            "read_file" | "write_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
                match self.resolve_within_workspace(path) {
                    Ok(_) => format!("{level} 文件操作（工作区内）", level = level.label()),
                    Err(e) => format!("拒绝：{e}"),
                }
            }
            "list_dir" | "search_files" => "目录/搜索操作".to_string(),
            "run_command" => {
                let command = args
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let lower = command.to_lowercase();
                let denied = self
                    .deny_fragments()
                    .iter()
                    .any(|frag| lower.contains(frag));
                if denied {
                    "拒绝：命令命中危险模式".to_string()
                } else {
                    format!("执行命令：{command}")
                }
            }
            _ => format!("工具 {tool} 需要审批"),
        };
        let risk_note = effect
            .and_then(|effect| effect.risk_note.clone())
            .or_else(|| {
                crate::tool_effects::effect_for(tool)
                    .and_then(|effect| effect.risk_note)
                    .or(Some(crate::tool_effects::UNDECLARED_RISK_NOTE.to_string()))
            });
        PermissionRequest::new(tool, args.clone(), level, reason)
            .with_risk_note(risk_note)
            .with_redacted_args(Some(redact_args(args)))
    }

    /// 工具执行前的最终判定（拒绝原因通过 request.reason 表达）。
    ///
    /// §5.3/§5.4 判定顺序：deny 优先 → §4.5.3 维度收紧层 → 等级档位（profile）
    /// → 授权记忆（grant）→ ask。
    /// grant 命中放行与 profile 档位叠加，但绝不越过 deny 规则（reason 以"拒绝"开头
    /// 恒为 Deny，false 优先）。
    pub fn decision(&self, request: &PermissionRequest) -> Decision {
        if request.reason.starts_with("拒绝") {
            return Decision::Deny;
        }
        // §4.5.3 维度收紧层：**必须**排在 Read 放行与 grant 命中之前——
        // 用户在权限中心显式关掉某个维度时，"读操作默认放行"和"已授过的权限"
        // 都不能把它绕回去，否则界面关了后端还在跑，就是假合规。
        if let Some(spec) = self.spec() {
            if spec.extra_denial(request).is_some() {
                return Decision::Deny;
            }
        }
        if request.level == Level::Read {
            return Decision::Allow;
        }
        if self.is_read_only() || self.profile() == PermissionProfile::ReadOnly {
            return Decision::Deny;
        }
        // §5.4 授权记忆命中（同一工作区 + 同一参数指纹）即放行。
        if let Some(grants) = self.grants.read().ok().and_then(|guard| guard.clone()) {
            if grants.consume(request, &self.workspace_id()).is_some() {
                return Decision::Allow;
            }
        }
        match self.profile() {
            // Workspace：工作区内普通写自动允许（reason 含"工作区内"），
            // 执行/注入/联网/UI 控制仍询问；越界已在 evaluate 阶段拒绝。
            PermissionProfile::Workspace => {
                if request.level == Level::Write && request.reason.contains("工作区内") {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
            // AutoReview：审批 Agent 可收紧或代批「可代批」操作——工作区内写
            // 不自动放行，一律进入审批链（reviewer 代批 / 人工审批），否则收紧
            // 无从生效；执行/注入照旧询问。
            PermissionProfile::AutoReview => match request.level {
                Level::Write | Level::Execute | Level::Inject => Decision::Ask,
                Level::Read => Decision::Allow,
            },
            // FullAccess：减少询问——Write 与 Execute 放行，Inject 仍询问
            // （注入/UI 控制不可逆，即使全权模式也不绕过确认）。
            PermissionProfile::FullAccess => match request.level {
                Level::Write | Level::Execute => Decision::Allow,
                Level::Inject => Decision::Ask,
                Level::Read => Decision::Allow,
            },
            PermissionProfile::Custom => match request.level {
                Level::Write if request.reason.contains("工作区内") => Decision::Allow,
                Level::Execute | Level::Inject => Decision::Ask,
                Level::Read => Decision::Allow,
                Level::Write => Decision::Ask,
            },
            PermissionProfile::ReadOnly => Decision::Deny, // 上方已拦截，防御性分支
        }
    }

    /// 授权记忆匹配用的工作区标识（canonical 后字符串；不可读取时回退原始路径）。
    pub fn workspace_id(&self) -> String {
        self.workspace
            .canonicalize()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.workspace.to_string_lossy().into_owned())
    }
}

pub fn resolve_within(workspace: &Path, path: &str) -> Result<PathBuf, String> {
    let raw = PathBuf::from(path);
    let candidate = if raw.is_absolute() {
        raw
    } else {
        workspace.join(raw)
    };
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|e| format!("工作区不可访问：{e}"))?;
    let canonical_candidate =
        canonicalize_existing_parent(&candidate).map_err(|e| format!("路径校验失败：{e}"))?;
    if !canonical_candidate.starts_with(&canonical_workspace) {
        return Err(format!("路径位于工作区之外：{path}"));
    }
    Ok(candidate)
}

fn canonicalize_existing_parent(path: &Path) -> std::io::Result<PathBuf> {
    if path.exists() {
        return path.canonicalize();
    }
    let mut current = path;
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if current.exists() {
            let mut base = current.canonicalize()?;
            for part in suffix.iter().rev() {
                base.push(part);
            }
            return Ok(base);
        }
        match current.parent() {
            Some(parent) => {
                if let Some(name) = current.file_name() {
                    suffix.push(name.to_os_string());
                }
                current = parent;
            }
            None => return Ok(path.to_path_buf()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn read_only_policy_denies_writes() {
        let policy = Policy::read_only(".");
        let request = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&request), Decision::Deny);
    }

    #[test]
    fn read_only_policy_allows_reads() {
        let policy = Policy::read_only(".");
        let request = policy.evaluate("read_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&request), Decision::Allow);
    }

    #[test]
    fn default_workspace_profile_auto_allows_workspace_writes() {
        // §5.3 Workspace（默认）：工作区内普通读写自动允许。
        let policy = Policy::new(".");
        let write = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&write), Decision::Allow);
    }

    #[test]
    fn workspace_profile_asks_for_execute_and_out_of_workspace() {
        let policy = Policy::new(".");
        let command = policy.evaluate("run_command", &json!({ "command": "ls" }));
        assert_eq!(policy.decision(&command), Decision::Ask, "执行始终询问");
        let out_of_scope = policy.evaluate("write_file", &json!({ "path": "../escape.txt" }));
        assert_eq!(
            policy.decision(&out_of_scope),
            Decision::Deny,
            "越界写入在 evaluate 阶段拒绝"
        );
    }

    #[test]
    fn full_access_profile_reduces_questions_but_keeps_inject_ask() {
        // §5.3 FullAccess：减少询问，但不绕过注入/不可逆操作确认。
        let policy = Policy::new(".");
        policy.set_profile(PermissionProfile::FullAccess);
        let write = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&write), Decision::Allow);
        let command = policy.evaluate("run_command", &json!({ "command": "ls" }));
        assert_eq!(policy.decision(&command), Decision::Allow, "执行也减少询问");
        let inject = policy.evaluate("desktop_type", &json!({ "text": "hello" }));
        assert_eq!(
            policy.decision(&inject),
            Decision::Ask,
            "注入/UI 控制即使全权模式也确认"
        );
    }

    #[test]
    fn read_only_profile_via_set_profile_denies_writes() {
        let policy = Policy::new(".");
        policy.set_profile(PermissionProfile::ReadOnly);
        assert!(policy.is_read_only());
        let write = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&write), Decision::Deny);
    }

    #[test]
    fn grant_hit_allows_ask_tool_without_prompting() {
        use crate::grant_store::{GrantScope, GrantStore};
        let store = GrantStore::new();
        let policy = Policy::new(".").with_grants(std::sync::Arc::new(store));
        // 先一次授权（session）再请求：同参数指纹应直接放行。
        let workspace_id = policy.workspace_id();
        let probe = policy.evaluate("run_command", &json!({ "command": "ls -la" }));
        let grants_ref = policy
            .grants
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .expect("grants 已注入");
        let grant = grants_ref
            .grant_from_scope(&probe, &workspace_id, GrantScope::Session)
            .expect("session 生成 grant");
        grants_ref.insert(grant);
        assert_eq!(
            policy.decision(&probe),
            Decision::Allow,
            "grant 命中放行执行类工具"
        );
    }

    #[test]
    fn custom_deny_command_fragment_is_enforced() {
        let mut policy = Policy::new(".");
        policy.add_deny_command("danger-command");
        let request = policy.evaluate(
            "run_command",
            &json!({ "command": "danger-command --force" }),
        );
        assert_eq!(policy.decision(&request), Decision::Deny);
    }

    #[test]
    fn runtime_policy_settings_take_effect_without_rebuilding() {
        let policy = Policy::new(".");
        // 默认 Workspace：工作区内写自动允许。切 ReadOnly 后拒绝，切回恢复。
        let write = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&write), Decision::Allow);

        policy.set_read_only_runtime(true);
        assert_eq!(policy.decision(&write), Decision::Deny);
        policy.set_read_only_runtime(false);
        assert_eq!(policy.decision(&write), Decision::Allow);

        policy.replace_runtime_deny(&["danger-now".to_string()]);
        let denied = policy.evaluate("run_command", &json!({ "command": "danger-now" }));
        assert_eq!(policy.decision(&denied), Decision::Deny);
        policy.replace_runtime_deny(&[]);
        let allowed_to_ask = policy.evaluate("run_command", &json!({ "command": "danger-now" }));
        assert_eq!(policy.decision(&allowed_to_ask), Decision::Ask);
    }

    #[test]
    fn redact_hides_secret_fields_but_keeps_paths() {
        let args = json!({
            "path": "a/b.txt",
            "content": "秘密正文",
            "url": "https://example.com/api",
            "headers": { "Authorization": "Bearer sk-12345", "X-Key": "sec" },
            "command": "run-task --flag"
        });
        let redacted = redact_args(&args);
        assert_eq!(redacted["path"], json!("a/b.txt"), "路径不脱敏");
        assert_eq!(
            redacted["content"],
            json!("秘密正文"),
            "正文按原样保留（UI 看做什么）"
        );
        assert_eq!(
            redacted["url"],
            json!("https://example.com/api"),
            "URL 保留域名便于判断"
        );
        assert_eq!(
            redacted["command"],
            json!("run-task --flag"),
            "命令保留便于判断"
        );
        let header = &redacted["headers"]["Authorization"];
        assert_eq!(header["type"], json!("string"));
        assert_eq!(header["length"], json!(15), "Bearer sk-12345 长度");
        assert_eq!(header["redacted"], json!(true), "秘密字段标记 redacted");
        assert!(
            redacted["headers"]["Authorization"].get("text").is_none(),
            "不携带原文"
        );
    }

    #[test]
    fn describe_write_shows_target_and_undoable() {
        let request = Policy::new(".").evaluate("write_file", &json!({ "path": "a.txt" }));
        let summary = describe_request(&request);
        assert_eq!(summary["action"], json!("写入文件"));
        assert_eq!(summary["target"], json!("a.txt"));
        assert_eq!(summary["undoable"], json!(true));
    }

    #[test]
    fn inject_tool_is_destructive_and_read_file_is_not() {
        let write = Policy::new(".").evaluate("write_file", &json!({ "path": "a.txt" }));
        assert!(write.is_destructive(), "写操作不提供始终允许只读");
        let read = Policy::new(".").evaluate("read_file", &json!({ "path": "a.txt" }));
        assert!(!read.is_destructive(), "只读操作可始终允许");
        let inject = Policy::new(".").evaluate("desktop_type", &json!({ "text": "hi" }));
        assert!(inject.is_destructive(), "注入不可逆，不允许始终允许");
    }

    /// §4.5.3 测试夹具：四个维度独立可设，scopes 留空。
    fn spec(
        filesystem: crate::permission_spec::FilesystemScope,
        command: crate::permission_spec::RuleScope,
        network: crate::permission_spec::RuleScope,
    ) -> crate::permission_spec::PermissionSpec {
        crate::permission_spec::PermissionSpec {
            filesystem,
            command,
            network,
            persistence: crate::permission_spec::PersistenceScope::Once,
            scopes: Vec::new(),
        }
    }

    #[test]
    fn dimension_deny_beats_grant_hit() {
        // 用户在权限中心把「命令执行」关掉之后，先前授出去的 session 授权必须失效。
        // 若判定顺序写错（grant 在前），这条会变成 Allow —— 界面显示"已拒绝"、
        // 后端却继续执行命令，就是假合规。
        use crate::grant_store::{GrantScope, GrantStore};
        use crate::permission_spec::{FilesystemScope, RuleScope};

        let store = std::sync::Arc::new(GrantStore::new());
        let policy = Policy::new(".").with_grants(store.clone());
        let probe = policy.evaluate("run_command", &json!({ "command": "ls -la" }));
        let grant = store
            .grant_from_scope(&probe, &policy.workspace_id(), GrantScope::Session)
            .expect("session 生成 grant");
        store.insert(grant);

        // 对照：白名单档不接管命令 → 仍是 grant 命中放行。
        policy.set_spec(spec(
            FilesystemScope::WorkspaceWrite,
            RuleScope::Allowlisted,
            RuleScope::Deny,
        ));
        assert_eq!(
            policy.decision(&probe),
            Decision::Allow,
            "未显式拒绝该维度时，grant 照常生效（收紧层不得改变既有行为）"
        );

        // 同一策略、同一 grant，只把命令维度改成 Deny → 必须立刻拒绝。
        policy.set_spec(spec(
            FilesystemScope::WorkspaceWrite,
            RuleScope::Deny,
            RuleScope::Deny,
        ));
        assert_eq!(
            policy.decision(&probe),
            Decision::Deny,
            "维度显式拒绝优先于 grant 命中"
        );
        assert_eq!(
            policy.profile(),
            PermissionProfile::Workspace,
            "工作区可写 + 命令拒绝不是全禁，档位不应升到只读"
        );
        assert_eq!(
            store.list().len(),
            1,
            "拒绝不该顺手删掉授权记录（撤销是显式动作）"
        );
    }

    #[test]
    fn filesystem_none_denies_reads_too() {
        // Read 级默认放行是既有语义；filesystem:none 是唯一能把「读」也关掉的面板，
        // 所以收紧层必须排在 Read 放行之前。
        use crate::permission_spec::{FilesystemScope, RuleScope};
        let policy = Policy::new(".");
        let read = policy.evaluate("read_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&read), Decision::Allow);
        policy.set_spec(spec(
            FilesystemScope::None,
            RuleScope::Deny,
            RuleScope::Deny,
        ));
        assert_eq!(
            policy.decision(&read),
            Decision::Deny,
            "文件系统=none 时连读取默认放行也要让位"
        );
        assert_eq!(
            policy.profile(),
            PermissionProfile::ReadOnly,
            "三维全关时档位同步为只读（单一真相，不留分裂）"
        );
    }

    #[test]
    fn absent_spec_leaves_profile_semantics_untouched() {
        // 回归护栏：没有结构化配置时（所有历史调用点与既有部署），判定必须
        // 与引入 spec 之前逐条一致——否则这次改动就不是"只收紧不放宽"。
        let policy = Policy::new(".");
        assert!(policy.spec().is_none(), "默认无结构化配置");
        let write = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        let exec = policy.evaluate("run_command", &json!({ "command": "ls" }));
        let inject = policy.evaluate("desktop_type", &json!({ "text": "hi" }));
        assert_eq!(policy.decision(&write), Decision::Allow);
        assert_eq!(policy.decision(&exec), Decision::Ask);
        assert_eq!(policy.decision(&inject), Decision::Ask);

        policy.clear_spec();
        assert_eq!(policy.decision(&write), Decision::Allow);
        assert_eq!(policy.decision(&exec), Decision::Ask);
        assert_eq!(
            policy.decision(&inject),
            Decision::Ask,
            "clear_spec 不改变档位判定"
        );
    }

    #[test]
    fn read_only_is_an_upper_bound_spec_cannot_lift() {
        // 只读模式必须是上界：提交一张"文件可写 + 命令不限"的结构化配置，
        // 判定仍然是拒绝；档位也不会被抬回可写。这是本次改动唯一的放宽风险面。
        use crate::permission_spec::{
            FilesystemScope, PermissionSpec, PersistenceScope, RuleScope,
        };
        let policy = Policy::read_only(".");
        let write = policy.evaluate("write_file", &json!({ "path": "a.txt" }));
        assert_eq!(policy.decision(&write), Decision::Deny);
        policy.set_spec(PermissionSpec {
            filesystem: FilesystemScope::WorkspaceWrite,
            command: RuleScope::Unrestricted,
            network: RuleScope::Unrestricted,
            persistence: PersistenceScope::Workspace,
            scopes: vec![],
        });
        assert_eq!(
            policy.profile(),
            PermissionProfile::ReadOnly,
            "只读档不能因为 spec 更宽而被同步抬升"
        );
        assert!(policy.is_read_only(), "read_only 开关必须保持为真");
        assert_eq!(
            policy.decision(&write),
            Decision::Deny,
            "更宽的结构化配置不得让写入通过"
        );
    }

    #[test]
    fn spec_tightens_without_moving_the_profile_dial() {
        // 收紧靠 extra_denial，不靠改档位：FullAccess 下关掉命令维度后，
        // 判定立刻拒绝，但档位旋钮仍停留在人亲手选的位置（不会被这次提交拧松）。
        use crate::permission_spec::{
            FilesystemScope, PermissionSpec, PersistenceScope, RuleScope,
        };
        let policy = Policy::new(".");
        policy.set_profile(PermissionProfile::FullAccess);
        let exec = policy.evaluate("run_command", &json!({ "command": "ls" }));
        assert_eq!(policy.decision(&exec), Decision::Allow, "全权档下执行放行");
        policy.set_spec(PermissionSpec {
            filesystem: FilesystemScope::WorkspaceWrite,
            command: RuleScope::Deny,
            network: RuleScope::Unrestricted,
            persistence: PersistenceScope::Task,
            scopes: vec![],
        });
        assert_eq!(
            policy.decision(&exec),
            Decision::Deny,
            "命令维度关掉后必须拒绝"
        );
        assert_eq!(
            policy.profile(),
            PermissionProfile::FullAccess,
            "档位不被这次提交改动（AutoReview 等更严档位同理，不会被降级）"
        );
    }
}
