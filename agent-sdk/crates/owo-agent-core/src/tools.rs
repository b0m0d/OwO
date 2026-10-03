//! Agent 工具定义与注册表。
//!
//! 原始句柄查找与直接执行仅供 core 内部运行时使用；下游 crate 必须通过
//! Agent 的受策略控制回合入口，不得自行取出工具句柄运行。
//!
//! ```compile_fail
//! let _ = owo_agent_core::tools::ToolRegistry::get;
//! ```
//!
//! ```compile_fail
//! let _ = owo_agent_core::tools::ToolRegistry::execute;
//! ```

use crate::audit::AuditLog;
use crate::external_tools;
use crate::mcp::{McpClient, McpPrompt, McpResource, McpTool};
use crate::permissions::{Decision, PermissionRequest, Policy};
use crate::session::Session;
use crate::skill::SkillRegistry;
use crate::subagent::{FanOutRunner, SubagentRunner};
use crate::tool_effects::EffectClass;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
// 工具参数取用助手（M1）：归属内核 `tool_args`，本文件多处工具实现共用。
use owo_agent_kernel::required_string;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// §5.1 工具副作用元数据：注册表导出的 spec 携带 effect（唯一事实源）。
    /// 内置工具由 registry 填充；MCP 工具在注册时内嵌（断开随实例消失，
    /// 不再依赖进程级全局名字查询）。None = 未登记（approval 按高风险处理）。
    pub effect: Option<crate::tool_effects::ToolEffect>,
}

impl ToolSpec {
    /// 附带 effect 的构造（MCP 注册与 registry 导出填充用）。
    pub fn with_effect(
        name: impl Into<String>,
        description: String,
        input_schema: Value,
        effect: Option<crate::tool_effects::ToolEffect>,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            input_schema,
            effect,
        }
    }
}

pub struct ToolContext<'a> {
    pub workspace: &'a Path,
    pub policy: &'a Policy,
    pub session: &'a mut Session,
    pub audit: &'a Arc<Mutex<AuditLog>>,
    pub subagent: Option<SubagentRunner<'a>>,
    pub skills: &'a SkillRegistry,
    /// 窗口元素注册表（感知多源融合的稳定元素 ID 空间）。
    pub elements: &'a Arc<Mutex<crate::ElementRegistry>>,
    /// A5-1：fan-out 只读子代理的注入通道（owned；子代理内/无通道场景为 None）。
    pub fanout: Option<FanOutRunner>,
    /// 回合取消标志：工具层桥接（fan-out 停止调度新子任务并 abort 在飞者）。
    pub abort: Option<&'a AtomicBool>,
    /// 用户提问通道（ask_user 工具）：None 表示当前环境没有 UI 通道（CLI/子代理），
    /// 工具会明确报错并提示模型改为书面提问。
    pub questioner: Option<&'a dyn crate::question::Questioner>,
}

/// ToolHost 签发 capability 时必须绑定的运行上下文。
///
/// 该类型只在 core 内部构造；调用方不能只凭工具名/参数伪造一个脱离
/// 当前会话、工作区和回合的执行能力。
#[derive(Debug, Clone)]
pub(crate) struct ToolCapabilityContext {
    pub workspace: String,
    pub session_id: String,
    pub turn_id: String,
    pub scope: String,
    pub max_command_timeout_ms: Option<u64>,
}

/// Policy 放行后的类型化凭证。
///
/// `ToolHostService` 不接受裸 `bool` 作为批准证明；凭证只能由
/// `PermissionRequest + Decision::Allow` 构造，并绑定工具名和参数摘要。
#[derive(Debug, Clone)]
pub(crate) struct ToolApprovalGrant {
    tool: String,
    args_sha256: String,
    request_id: String,
}

impl ToolApprovalGrant {
    pub(crate) fn from_decision(
        request: &PermissionRequest,
        decision: Decision,
    ) -> Result<Self, String> {
        if decision != Decision::Allow {
            return Err(format!("permission not granted: {}", request.tool));
        }
        Ok(Self {
            tool: request.tool.clone(),
            args_sha256: crate::CasStore::hash_of(request.args.to_string().as_bytes()),
            request_id: request.request_id.clone(),
        })
    }
}

impl ToolCapabilityContext {
    pub(crate) fn for_workspace(
        workspace: &Path,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Self {
        Self {
            workspace: workspace.to_string_lossy().to_string(),
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            scope: format!("workspace:{}", workspace.to_string_lossy()),
            max_command_timeout_ms: None,
        }
    }

    pub(crate) fn with_command_timeout(mut self, timeout_ms: Option<u64>) -> Self {
        self.max_command_timeout_ms = timeout_ms;
        if let Some(timeout_ms) = timeout_ms {
            self.scope.push_str(&format!(";command_timeout_ms={timeout_ms}"));
        }
        self
    }
}

/// 受信执行能力：只能由 `ToolHostService::issue` 创建，携带一次性调用参数。
///
/// Agent loop 不再直接从 `ToolRegistry` 取出工具并执行；它必须先经过这个
/// capability 门面。参数摘要进入收据，避免把原始参数写入审计日志。
#[derive(Debug, Clone)]
pub(crate) struct ToolCapability {
    tool: String,
    tool_version: String,
    args: Value,
    args_sha256: String,
    workspace: String,
    session_id: String,
    turn_id: String,
    effect: EffectClass,
    scope: String,
    max_command_timeout_ms: Option<u64>,
    /// 短时效能力：审批结果不能被无限期重放。
    expires_at_unix: u64,
    /// 每次签发的不可预测调用标识，写入收据用于关联但不写原始参数。
    nonce: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolReceipt {
    pub tool: String,
    pub tool_version: String,
    pub session_id: String,
    pub turn_id: String,
    pub approved: bool,
    pub ok: bool,
    pub effect: EffectClass,
    pub scope_sha256: String,
    pub args_sha256: String,
    pub result_sha256: Option<String>,
    pub execution_receipt_id: Option<String>,
    pub changed_files: Vec<String>,
    pub diff_sha256: Option<String>,
    pub duration_ms: u64,
    pub expires_at_unix: u64,
    pub nonce: String,
}

pub(crate) trait ToolReceiptSink: Send + Sync {
    fn record(&self, receipt: ToolReceipt);
}

struct AuditReceiptSink {
    audit: Arc<Mutex<AuditLog>>,
}

impl ToolReceiptSink for AuditReceiptSink {
    fn record(&self, receipt: ToolReceipt) {
        let detail = json!({
            "tool_version": receipt.tool_version,
            "turn_id": receipt.turn_id,
            "approved": receipt.approved,
            "ok": receipt.ok,
            "effect": receipt.effect.label(),
            "scope_sha256": receipt.scope_sha256,
            "args_sha256": receipt.args_sha256,
            "result_sha256": receipt.result_sha256,
            "execution_receipt_id": receipt.execution_receipt_id,
            "changed_files": receipt.changed_files,
            "diff_sha256": receipt.diff_sha256,
            "duration_ms": receipt.duration_ms,
            "expires_at_unix": receipt.expires_at_unix,
            "nonce": receipt.nonce,
        })
        .to_string();
        if let Ok(mut audit) = self.audit.lock() {
            audit.record(
                &receipt.session_id,
                "tool_receipt",
                Some(receipt.tool),
                Some(receipt.approved),
                detail,
            );
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String>;
}

fn tool_spec_fingerprint(spec: &ToolSpec) -> String {
    crate::CasStore::hash_of(
        json!({
            "name": spec.name,
            "input_schema": spec.input_schema,
            "effect": spec.effect,
        })
        .to_string()
        .as_bytes(),
    )
}

/// Agent 工具执行的唯一受信门面。
///
/// `ToolRegistry::get` 仍只在 core crate 内可见，但执行也必须通过这里完成：
/// 先签发带参数摘要的 capability，再由门面查找并运行工具，最后无论成功或
/// 失败都向 receipt sink 写入结构化收据。后续可在不改变 Agent loop 的情况下
/// 将 sink 替换为持久化/变更集收据实现。
#[derive(Clone)]
pub(crate) struct ToolHostService {
    registry: Arc<RwLock<ToolRegistry>>,
    receipt_sink: Arc<dyn ToolReceiptSink>,
}

impl ToolHostService {
    pub(crate) fn new(registry: Arc<RwLock<ToolRegistry>>, audit: Arc<Mutex<AuditLog>>) -> Self {
        Self {
            registry,
            receipt_sink: Arc::new(AuditReceiptSink { audit }),
        }
    }

    /// 只有最终通过审批的调用才能取得执行 capability。
    pub(crate) fn issue(
        &self,
        tool: &str,
        args: Value,
        approval: ToolApprovalGrant,
        context: ToolCapabilityContext,
    ) -> Result<ToolCapability, String> {
        let args_sha256 = crate::CasStore::hash_of(args.to_string().as_bytes());
        if approval.tool != tool {
            return Err(format!("approval tool mismatch: {tool}"));
        }
        if approval.args_sha256 != args_sha256 {
            return Err(format!("approval args mismatch: {tool}"));
        }
        if context.workspace.is_empty()
            || context.session_id.is_empty()
            || context.turn_id.is_empty()
            || context.scope.is_empty()
        {
            return Err(format!("capability context missing: {tool}"));
        }
        let spec = self
            .registry
            .read()
            .map_err(|_| "工具注册表锁中毒".to_string())?
            .get(tool)
            .map(|registered| registered.spec())
            .ok_or_else(|| format!("未知工具：{tool}"))?;
        let tool_version = tool_spec_fingerprint(&spec);
        let effect = spec
            .effect
            .as_ref()
            .map(|metadata| metadata.class)
            .unwrap_or_else(|| crate::tool_effects::effect_class_for(&spec.name));
        let issued_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "系统时钟早于 Unix epoch".to_string())?
            .as_secs();
        Ok(ToolCapability {
            tool: tool.to_string(),
            tool_version,
            args,
            args_sha256,
            workspace: context.workspace,
            session_id: context.session_id,
            turn_id: context.turn_id,
            effect,
            scope: context.scope,
            max_command_timeout_ms: context.max_command_timeout_ms,
            expires_at_unix: issued_at_unix.saturating_add(60),
            nonce: format!("{}:{}", approval.request_id, uuid::Uuid::new_v4()),
        })
    }

    pub(crate) async fn execute(
        &self,
        capability: ToolCapability,
        ctx: &mut ToolContext<'_>,
    ) -> Result<Value, String> {
        let (tool, current_tool_version) = self
            .registry
            .read()
            .map_err(|_| "工具注册表锁中毒".to_string())?
            .get(&capability.tool)
            .map(|registered| {
                let spec = registered.spec();
                (Some(registered), tool_spec_fingerprint(&spec))
            })
            .unwrap_or((None, String::new()));
        let started = std::time::Instant::now();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(u64::MAX);
        let current_workspace = ctx.workspace.to_string_lossy();
        let write_path = (capability.effect == EffectClass::Write)
            .then(|| capability.args.get("path").and_then(Value::as_str))
            .flatten()
            .and_then(|path| resolve_session_path(ctx, path).ok());
        let mut outcome = if capability.session_id != ctx.session.id {
            Err(format!("capability session mismatch: {}", capability.tool))
        } else if capability.workspace != current_workspace {
            Err(format!(
                "capability workspace mismatch: {}",
                capability.tool
            ))
        } else if capability.tool_version != current_tool_version {
            Err(format!(
                "capability tool version mismatch: {}",
                capability.tool
            ))
        } else if now_unix >= capability.expires_at_unix {
            Err(format!("approval expired: {}", capability.tool))
        } else {
            let mut tool_args = capability.args.clone();
            if capability.tool == "run_command" {
                if let (Some(timeout_ms), Some(arguments)) =
                    (capability.max_command_timeout_ms, tool_args.as_object_mut())
                {
                    arguments.insert("_host_timeout_ms".to_string(), json!(timeout_ms));
                }
            }
            match tool {
                Some(tool) => tool.run(ctx, tool_args).await,
                None => Err(format!("未知工具：{}", capability.tool)),
            }
        };
        let mut write_paths = Vec::new();
        if let Some(path) = write_path.as_deref() {
            write_paths.push(path.to_path_buf());
        } else if capability.tool == "apply_patch" {
            if let Some(files) = outcome
                .as_ref()
                .ok()
                .and_then(|value| value.get("files"))
                .and_then(Value::as_array)
            {
                for path in files.iter().filter_map(|file| file.get("path").and_then(Value::as_str)) {
                    if let Ok(path) = resolve_session_path(ctx, path) {
                        write_paths.push(path);
                    }
                }
            }
        }
        let mut execution_receipts = Vec::new();
        if outcome.is_ok() {
            for path in write_paths {
                match ctx
                    .session
                    .record_file_execution(&capability.tool, &capability.turn_id, &path)
                {
                    Ok(Some(receipt)) => execution_receipts.push(receipt),
                    Ok(None) => {}
                    Err(error) => {
                        outcome = Err(format!("写入收据失败：{error}"));
                        execution_receipts.clear();
                        break;
                    }
                }
            }
        }
        if let (Ok(Value::Object(result)), receipts) = (&mut outcome, &execution_receipts) {
            if !receipts.is_empty() {
                result.insert(
                    "execution_receipt_ids".to_string(),
                    Value::Array(
                        receipts
                            .iter()
                            .map(|receipt| Value::String(receipt.receipt_id.clone()))
                            .collect(),
                    ),
                );
                if receipts.len() == 1 {
                    result.insert(
                        "execution_receipt_id".to_string(),
                        Value::String(receipts[0].receipt_id.clone()),
                    );
                }
                let changed_files = receipts
                    .iter()
                    .flat_map(|receipt| receipt.changed_files.iter().cloned())
                    .collect::<Vec<_>>();
                result.insert(
                    "changed_files".to_string(),
                    Value::Array(changed_files.into_iter().map(Value::String).collect()),
                );
                result.insert(
                    "diff_sha256".to_string(),
                    Value::String(crate::CasStore::hash_of(
                        serde_json::to_vec(
                            &receipts.iter().map(|receipt| &receipt.diff_sha256).collect::<Vec<_>>(),
                        )
                        .unwrap_or_default()
                        .as_slice(),
                    )),
                );
            }
        }
        let result_sha256 = outcome
            .as_ref()
            .ok()
            .map(|value| crate::CasStore::hash_of(value.to_string().as_bytes()));
        self.receipt_sink.record(ToolReceipt {
            tool: capability.tool,
            tool_version: capability.tool_version,
            session_id: ctx.session.id.clone(),
            turn_id: capability.turn_id,
            approved: true,
            ok: outcome.is_ok(),
            effect: capability.effect,
            scope_sha256: crate::CasStore::hash_of(capability.scope.as_bytes()),
            args_sha256: capability.args_sha256,
            result_sha256,
            execution_receipt_id: execution_receipts
                .first()
                .map(|receipt| receipt.receipt_id.clone()),
            changed_files: execution_receipts
                .iter()
                .flat_map(|receipt| receipt.changed_files.iter().cloned())
                .collect(),
            diff_sha256: (!execution_receipts.is_empty()).then(|| {
                crate::CasStore::hash_of(
                    serde_json::to_vec(
                        &execution_receipts.iter().map(|receipt| &receipt.diff_sha256).collect::<Vec<_>>(),
                    )
                    .unwrap_or_default()
                    .as_slice(),
                )
            }),
            duration_ms: started.elapsed().as_millis() as u64,
            expires_at_unix: capability.expires_at_unix,
            nonce: capability.nonce,
        });
        outcome
    }
}

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    /// MCP 大 schema 的完整副本（注册时超预算被压缩为骨架；此处保留原始 schema 供按需查询）。
    full_schemas: HashMap<String, Value>,
}

impl ToolRegistry {
    /// 默认最小工具集：文件读写/搜索、受控命令与委派。桌面、视觉、浏览器能力
    /// 必须由工作区设置或显式调用方按场景加入，避免每个 Agent 默认暴露完整工具面。
    pub fn new() -> Self {
        let mut registry = Self::empty();
        // 保留历史基础工具顺序，避免不相关的模型提示变化。
        registry.register(ReadFileTool);
        registry.register(WriteFileTool);
        registry.register(EditFileTool);
        registry.register(MultiEditTool);
        registry.register(ApplyPatchTool);
        registry.register(ListDirTool);
        registry.register(SearchFilesTool);
        registry.register(GrepTool);
        registry.register(crate::git_tools::GitStatusTool);
        registry.register(crate::git_tools::GitDiffTool);
        registry.register(crate::git_tools::GitLogTool);
        registry.register_run_command();
        registry.register(ShellOutputTool);
        registry.register(KillShellTool);
        registry.register(TodoTool);
        registry.register(SingleVerificationPlanTool);
        registry.register(WebFetchTool);
        registry.register(WebSearchTool);
        registry.register(ReadImageTool);
        registry.register_delegation_tools();
        registry
    }

    /// 显式构造所有内置能力；主要用于兼容性/全能力集成测试，不作为生产默认值。
    pub fn with_all_builtin_capabilities() -> Self {
        let mut registry = Self::new();
        registry.register_desktop_observation_tools();
        registry.register_desktop_control_tools();
        registry.register_browser_tools();
        registry
    }

    /// 空注册表：产品评测等需要最小受控工具集的场景，由调用方自行注册工具。
    pub fn empty() -> Self {
        Self {
            tools: Vec::new(),
            full_schemas: HashMap::new(),
        }
    }

    /// 只读工具表（子代理 explore 使用）：不含写/执行/委派工具。
    pub fn read_only() -> Self {
        let mut registry = Self {
            tools: Vec::new(),
            full_schemas: HashMap::new(),
        };
        registry.register(ReadFileTool);
        registry.register(ListDirTool);
        registry.register(SearchFilesTool);
        registry.register(GrepTool);
        registry.register(crate::git_tools::GitStatusTool);
        registry.register(crate::git_tools::GitDiffTool);
        registry.register(crate::git_tools::GitLogTool);
        registry.register(ReadImageTool);
        registry
    }

    // -------------------------------------------------------------------
    // 按角色分组注册（七期 · 二路）：角色画像按「组」装配工具面，
    // 注册表只含该角色允许的工具——权限在工具面层生效，而非仅靠审批拒绝。
    // -------------------------------------------------------------------

    /// 文件读取组：`read_file` / `list_dir` / `search_files`（只读角色基线）。
    pub fn register_file_read_tools(&mut self) {
        self.register(ReadFileTool);
        self.register(ListDirTool);
        self.register(SearchFilesTool);
    }

    /// 文件写入（无白名单限制；受 `Policy` 审批约束）。
    pub fn register_write_file(&mut self) {
        self.register(WriteFileTool);
    }

    /// 白名单受限写入：写目标必须落在 `allowed` 绝对路径前缀内（见
    /// [`WhitelistWriteFileTool`]）；`allowed` 为空 = 工作区内可写。
    pub fn register_whitelist_write_file(&mut self, allowed: Vec<PathBuf>) {
        self.register(WhitelistWriteFileTool { allowed });
    }

    /// 白名单受限精确补丁：每个补丁目标都必须落入同一授权路径前缀内。
    pub fn register_whitelist_apply_patch(&mut self, allowed: Vec<PathBuf>) {
        self.register(WhitelistApplyPatchTool { allowed });
    }

    /// 受控命令执行：`run_command`（实现族角色专用；沙箱 + 审批约束不变）。
    pub fn register_run_command(&mut self) {
        self.register(RunCommandTool);
    }

    /// 桌面观察/视觉组：截图 OCR、窗口信息与视觉 grounding/verify，不包含输入操作。
    pub fn register_desktop_observation_tools(&mut self) {
        self.register(crate::computer_use::ScreenOcrTool);
        self.register(crate::computer_use::OcrRegionTool);
        self.register(crate::computer_use::DesktopWindowOcrTool);
        self.register(crate::computer_use::DesktopForegroundTool);
        self.register(crate::computer_use::DesktopWindowListTool);
        self.register(crate::computer_use::ScreenVisionTool);
        self.register(crate::computer_use::VisionVerifyTool);
        self.register(crate::computer_use::VisionGroundTool);
    }

    /// 桌面控制组：会激活窗口、注入键鼠或启动程序，默认不注册。
    pub fn register_desktop_control_tools(&mut self) {
        self.register(crate::computer_use::DesktopActivateTool);
        self.register(crate::computer_use::DesktopClickTool);
        self.register(crate::computer_use::DesktopTypeTool);
        self.register(crate::computer_use::DesktopKeyTool);
        self.register(crate::computer_use::DesktopShortcutTool);
        self.register(crate::computer_use::DesktopLaunchTool);
        self.register(crate::computer_use::DesktopScrollTool);
        self.register(crate::computer_use::DesktopWaitTool);
        self.register(crate::computer_use::DesktopWaitUntilTool);
    }

    /// 委派组：`explore` / `subagent` / `use_skill`。
    pub fn register_delegation_tools(&mut self) {
        self.register(ExploreTool);
        self.register(SubagentTool);
        self.register(FanOutSubagentsTool);
        self.register(AskUserTool);
        self.register(UseSkillTool);
    }

    /// 浏览器组：导航/搜索/快照 + 交互与写工作区变体（含 `browser_screenshot` /
    /// `browser_download_image` 写文件、`browser_close`）。只读角色的可见面
    /// 应经 `retain_names` 裁掉写工作区变体。
    pub fn register_browser_tools(&mut self) {
        let browser = crate::computer_use::BrowserTools::new();
        self.register(crate::computer_use::BrowserNavigateTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserSearchTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserSnapshotTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserClickTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserTypeTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserPressTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserScreenshotWriteTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserDownloadImageWriteTool {
            tools: browser.clone(),
        });
        self.register(crate::computer_use::BrowserCloseTool { tools: browser });
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.push(Arc::new(tool));
    }

    /// 注册由宿主构造的共享工具实例；执行仍经 ToolHost 与审批/审计门控。
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec()).collect()
    }

    /// 按前缀撤销工具（插件热卸载：`owo_plugin_<id>_` 前缀）。
    /// 返回被移除的工具数。
    pub fn remove_prefix(&mut self, prefix: &str) -> usize {
        self.remove_prefix_inner(prefix)
    }

    /// 按名单保留工具（角色画像可见工具面，七期 · 二路）：名单为空 = 全部保留
    /// （未声明可见面时按注册顺序全量可用）；返回被移除的工具数。
    /// MCP 完整 schema 副本随名单同步清理。
    pub fn retain_names(&mut self, names: &[String]) -> usize {
        if names.is_empty() {
            return 0;
        }
        let before = self.tools.len();
        self.tools
            .retain(|tool| names.iter().any(|name| tool.spec().name == *name));
        self.full_schemas
            .retain(|name, _| names.iter().any(|keep| keep == name));
        before - self.tools.len()
    }

    /// 取工具句柄（Arc 克隆，锁外可跨 await 执行）。
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.spec().name == name)
            .cloned()
    }

    /// 把 MCP 服务器暴露的工具注册为 Agent 工具（命名：`{server}_{tool}`）。
    ///
    /// 延迟加载（M2）：单工具 schema 超过 `schema_budget_bytes` 时，注册为模型可见的
    /// **压缩骨架**（仅保留 type/required/属性名+属性类型，剔除 description/enum/嵌套细节），
    /// 完整 schema 保留在 `full_schemas` 供 `full_schema()` 按需查询——大 schema 服务不
    /// 显著占用模型上下文；调用工具时仍以完整 schema 校验。
    pub fn register_mcp_tools(
        &mut self,
        server_name: &str,
        client: Arc<tokio::sync::Mutex<McpClient>>,
        tools: Vec<McpTool>,
    ) {
        self.register_mcp_tools_with_health(server_name, client, tools, None);
    }

    /// §10：带 per-server 健康跟踪（熔断/限流/幂等重试）的 MCP 工具注册。
    pub fn register_mcp_tools_with_health(
        &mut self,
        server_name: &str,
        client: Arc<tokio::sync::Mutex<McpClient>>,
        tools: Vec<McpTool>,
        health: Option<Arc<crate::mcp_health::McpHealthTracker>>,
    ) {
        let budget = schema_budget_bytes();
        for tool in tools {
            let full_name = format!(
                "{}_{}",
                sanitize_tool_name(server_name),
                sanitize_tool_name(&tool.name)
            );
            let (input_schema, full_schema) = if schema_bytes(&tool.input_schema) > budget {
                let full = tool.input_schema.clone();
                (compact_schema(&tool.input_schema), Some(full))
            } else {
                (tool.input_schema.clone(), None)
            };
            let mut description = tool.description;
            if full_schema.is_some() {
                description.push_str("（schema 已压缩，完整参数见 /mcp/schema 接口）");
            }
            // §7.2：MCP 工具副作用按 annotations 推导并注册（审批卡/只读判定消费）。
            // §5.1：effect 内嵌进 ToolSpec——断开/卸载时随工具实例消失，不遗留全局条目。
            // §5.2：宿主只读可信声明按「server+tool+schema hash」校验，hash 匹配才
            // 允许 readOnlyHint 降级为 Read；否则按 Execute（deny-by-default）。
            let host_verified_readonly = crate::tool_effects::is_trusted_readonly(
                server_name,
                &tool.name,
                &crate::tool_effects::schema_fingerprint(&tool.input_schema),
            );
            let effect = crate::tool_effects::register_mcp_effect(
                server_name,
                &tool.name,
                tool.annotations.as_ref(),
                host_verified_readonly,
            );
            let spec = ToolSpec {
                name: full_name.clone(),
                description,
                input_schema,
                effect: Some(effect),
            };
            if let Some(full) = full_schema {
                self.full_schemas.insert(full_name.clone(), full);
            }
            self.tools.push(Arc::new(McpToolAdapter {
                full_name,
                server_name: server_name.to_string(),
                tool_name: tool.name,
                spec,
                client: Arc::clone(&client),
                health: health.clone(),
            }));
        }
    }

    /// A2-2：把 MCP 服务器的 resources/prompts 注册为**泛化工具**——
    /// 每服务器至多 2 个（`{server}_read_resource` / `{server}_get_prompt`），
    /// 逐资源/逐模板开工具会撑爆工具表；目录摘要放在描述里供模型选 URI/模板名。
    /// 副作用按「未声明注解的 MCP 工具」登记（Execute，deny-by-default），
    /// 不因名字里带 read 就自动降级。
    pub fn register_mcp_extras(
        &mut self,
        server_name: &str,
        client: Arc<tokio::sync::Mutex<McpClient>>,
        resources: Vec<McpResource>,
        prompts: Vec<McpPrompt>,
    ) {
        let prefix = sanitize_tool_name(server_name);
        if !resources.is_empty() {
            let catalog: Vec<String> = resources
                .iter()
                .take(12)
                .map(|resource| {
                    if resource.name.is_empty() {
                        resource.uri.clone()
                    } else {
                        format!("{}（{}）", resource.uri, resource.name)
                    }
                })
                .collect();
            let full_name = format!("{prefix}_read_resource");
            let effect =
                crate::tool_effects::register_mcp_effect(server_name, "read_resource", None, false);
            self.tools.push(Arc::new(McpResourceAdapter {
                full_name: full_name.clone(),
                server_name: server_name.to_string(),
                spec: ToolSpec {
                    name: full_name,
                    description: format!(
                        "读取 MCP 服务器 {server_name} 的资源内容（resources/read）。可用资源：{}",
                        catalog.join("；")
                    ),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "uri": { "type": "string", "description": "资源 URI（见工具描述里的目录）" }
                        },
                        "required": ["uri"]
                    }),
                    effect: Some(effect),
                },
                client: Arc::clone(&client),
            }));
        }
        if !prompts.is_empty() {
            let catalog: Vec<String> = prompts
                .iter()
                .take(12)
                .map(|prompt| prompt.name.clone())
                .collect();
            let full_name = format!("{prefix}_get_prompt");
            let effect =
                crate::tool_effects::register_mcp_effect(server_name, "get_prompt", None, false);
            self.tools.push(Arc::new(McpPromptAdapter {
                full_name: full_name.clone(),
                server_name: server_name.to_string(),
                spec: ToolSpec {
                    name: full_name,
                    description: format!(
                        "获取 MCP 服务器 {server_name} 的提示模板（prompts/get）。可用模板：{}",
                        catalog.join("；")
                    ),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "模板名" },
                            "arguments": { "type": "object", "description": "模板参数（可选，键值对）" }
                        },
                        "required": ["name"]
                    }),
                    effect: Some(effect),
                },
                client,
            }));
        }
    }

    /// 按需取 MCP 工具的完整 schema（压缩注册时保留；小 schema 工具不重复存储）。
    pub fn full_schema(&self, name: &str) -> Option<Value> {
        self.full_schemas.get(name).cloned()
    }
    /// 移除工具时同步清理完整 schema 副本与副作用注册。
    fn remove_prefix_inner(&mut self, prefix: &str) -> usize {
        let before = self.tools.len();
        self.tools
            .retain(|tool| !tool.spec().name.starts_with(prefix));
        self.full_schemas
            .retain(|name, _| !name.starts_with(prefix));
        crate::tool_effects::remove_prefix(prefix);
        before - self.tools.len()
    }
}

/// 工具名规范化函数已随策略内核下沉到 `owo-agent-policy`（M12）：
/// 它同时是权限/效应判定的输入（`effect_class_for` 按名字查表、MCP 前缀按名字生成），
/// 两条消费链必须共用同一份命名规则，否则会出现"登记名"与"执行名"漂移。
/// 这里保持 `pub(crate)` 可用性不变（原来的可见性就是 `pub(crate)`）。
pub(crate) use owo_agent_policy::tool_names::sanitize_tool_name;

/// MCP 工具注册前缀（`{server}_{tool}` 命名空间）：如 `owo_plugin_owo-translate_`。
pub fn mcp_tool_prefix(server_name: &str) -> String {
    format!("{}_", sanitize_tool_name(server_name))
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn snapshot_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// MCP 工具 schema 延迟加载（M2）：单工具 schema 序列化字节数预算，默认 2048 字节。
pub fn schema_budget_bytes() -> usize {
    std::env::var("OWO_MCP_SCHEMA_BUDGET_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2048)
}

/// 估算 JSON schema 的序列化体积（字节）。
pub fn schema_bytes(schema: &Value) -> usize {
    serde_json::to_string(schema)
        .map(|text| text.len())
        .unwrap_or(usize::MAX)
}

/// 把 JSON Schema 压缩为模型可见的骨架：仅保留 `type`、`required` 与
/// 属性名+属性类型（字符串属性类型；嵌套对象/数组仅保留层级 type）。
/// 剔除 description / enum / pattern / 嵌套细节，体积大幅缩小。
/// 非对象 schema 原样返回（按需加载不适用）。
pub fn compact_schema(schema: &Value) -> Value {
    let Value::Object(map) = schema else {
        return schema.clone();
    };
    let mut compact = serde_json::Map::new();
    if let Some(t) = map.get("type") {
        compact.insert("type".to_string(), t.clone());
    }
    if let Some(required) = map.get("required") {
        compact.insert("required".to_string(), required.clone());
    }
    if let Some(properties) = map.get("properties").and_then(Value::as_object) {
        let mut props = serde_json::Map::new();
        for (name, property) in properties {
            let mut item = serde_json::Map::new();
            if let Some(t) = property.get("type") {
                item.insert("type".to_string(), t.clone());
            }
            props.insert(name.clone(), Value::Object(item));
        }
        compact.insert("properties".to_string(), Value::Object(props));
    }
    Value::Object(compact)
}

/// 以会话工作区为基座解析相对路径，并做策略工作区越界检查。
pub(crate) fn resolve_session_path(ctx: &ToolContext, path: &str) -> Result<PathBuf, String> {
    let base = ctx
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| ctx.workspace.to_path_buf());
    // 绝对路径直接采用（`Path::join` 对绝对路径会整体替换，且 canonicalize 可能不可用）。
    let raw = Path::new(path);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(path)
    };
    let candidate = candidate.canonicalize().unwrap_or(candidate);
    // 越界边界取**本会话的工作区**，不是 `Policy` 的工作区。
    //
    // 两者的来源不同：`ctx.workspace` 来自 `session.workspace`（每个会话都可以在 UI
    // 里选不同目录），而 `Policy` 的工作区来自服务启动参数——**全局唯一**。只要两者
    // 不相等，选过其它目录的会话就会**所有文件类工具一律"路径越界"**：`list_dir` /
    // `read_file` / `search_files` 全废，而 `run_command` 照常能用（命令工具不走这条
    // 判定），表象极像"Agent 整体坏了"。
    //
    // 实测成因：桌面壳在 `workspace.json` 未配置时 `workspacePath()` 回落
    // `process.cwd()`（= electron 安装目录），核心便以那个目录为工作区启动；用户在
    // 会话里选 `D:\OwO-master`，于是每一次读文件都被判越界。
    //
    // 安全口径不变：read_only 档位、deny 命令、审批链都由 `Policy` 独立把关，这里只
    // 决定"文件类工具的活动范围 = 本会话工作区"。
    let boundary = if ctx.workspace.as_os_str().is_empty() {
        ctx.policy.workspace()
    } else {
        ctx.workspace
    };
    let policy_workspace = boundary
        .canonicalize()
        .unwrap_or_else(|_| boundary.to_path_buf());
    // 两侧统一去 Windows verbatim 前缀再比对（`\\?\C:\x` vs `C:\x` 否则恒不匹配）。
    let candidate_cmp = strip_verbatim_prefix(&candidate);
    let workspace_cmp = strip_verbatim_prefix(&policy_workspace);
    let unrestricted = ctx.policy.profile() == crate::permissions::PermissionProfile::Unrestricted;
    if !candidate_cmp.starts_with(&workspace_cmp) && !unrestricted {
        return Err(format!("路径越界：{path}"));
    }
    Ok(candidate)
}

struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "读取工作区内的文本文件（支持 offset/limit 分页与行号，默认 400 行）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "起始行（1-based，默认 1）" },
                    "limit": { "type": "integer", "description": "读取行数（默认 400，上限 2000）" },
                    "number": { "type": "boolean", "description": "是否输出行号（默认 false）" }
                },
                "required": ["path"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(400)
            .clamp(1, 2000) as usize;
        let number = args.get("number").and_then(Value::as_bool).unwrap_or(false);
        let abs = resolve_session_path(ctx, &path)?;
        let raw_bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败：{e}"))?;
        let sha256 = crate::CasStore::hash_of(&raw_bytes);
        let raw = String::from_utf8(raw_bytes)
            .map_err(|e| format!("读取 {path} 失败：文件不是有效 UTF-8：{e}"))?;
        let total_lines = raw.lines().count();
        let start = offset.min(total_lines.saturating_add(1));
        let selected: Vec<&str> = raw.lines().skip(start - 1).take(limit).collect();
        let end_line = start + selected.len().saturating_sub(1);
        let truncated = end_line < total_lines;
        let content = if number {
            selected
                .iter()
                .enumerate()
                .map(|(index, line)| format!("{:>5}\t{line}", start + index))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            selected.join("\n")
        };
        Ok(json!({
            "path": path,
            "content": content,
            "start_line": start,
            "end_line": end_line,
            "total_lines": total_lines,
            "truncated": truncated,
            "bytes": content.len(),
            "sha256": format!("sha256:{sha256}"),
        }))
    }
}

struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "写入工作区内的文件（自动快照，可 diff/revert）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let content = required_string(&args, "content")?;
        let abs = resolve_session_path(ctx, &path)?;
        write_file_body(ctx, &path, &abs, &content).await
    }
}

/// 写入执行体（[`WriteFileTool`] / [`WhitelistWriteFileTool`] 共享）：
/// 首写快照；后续写入先校验上次 Agent 写入哈希，避免覆盖 Agent 运行期间的外部修改。
async fn write_file_body(
    ctx: &mut ToolContext<'_>,
    path: &str,
    abs: &Path,
    content: &str,
) -> Result<Value, String> {
    let key = snapshot_key(abs);
    if let Some(snapshot) = ctx.session.snapshots.get(&key) {
        let current = match tokio::fs::read(abs).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("写入前读取 {path} 失败：{error}")),
        };
        let matches_expected = if let Some(expected) = snapshot.expected_after_sha256.as_deref() {
            current
                .as_deref()
                .is_some_and(|bytes| crate::CasStore::hash_of(bytes) == expected)
        } else {
            let original = match snapshot.original_b64.as_deref() {
                Some(encoded) => Some(
                    BASE64
                        .decode(encoded)
                        .map_err(|error| format!("快照解码失败：{error}"))?,
                ),
                None => None,
            };
            current == original
        };
        if !matches_expected {
            return Err(format!(
                "写入冲突：{path} 在 Agent 上次记录的文件状态后再次变化，已拒绝覆盖"
            ));
        }
    } else {
        let original = match tokio::fs::read(abs).await {
            Ok(bytes) => Some(BASE64.encode(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("写入前快照 {path} 失败：{error}")),
        };
        ctx.session.snapshots.insert(
            key.clone(),
            crate::session::SnapshotEntry {
                original_b64: original,
                expected_after_sha256: None,
                turn: ctx.session.messages.len(),
            },
        );
    }
    if let Some(parent) = abs.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("创建目录失败：{e}"))?;
    }
    tokio::fs::write(abs, content.as_bytes())
        .await
        .map_err(|e| format!("写入 {path} 失败：{e}"))?;
    if let Some(snapshot) = ctx.session.snapshots.get_mut(&key) {
        snapshot.expected_after_sha256 = Some(crate::CasStore::hash_of(content.as_bytes()));
    }
    Ok(json!({
        "path": path,
        "written": true,
        "bytes": content.len(),
    }))
}

/// `edit_file`：精确替换（`old_string` 必须唯一命中，除非 `replace_all`）。
/// 复用 [`write_file_body`] 的快照/冲突校验，可 diff/revert。
struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "精确替换文件片段（old_string → new_string；默认要求唯一命中，replace_all=true 替换全部）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let old_string = required_string(&args, "old_string")?;
        let new_string = required_string(&args, "new_string")?;
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if old_string.is_empty() {
            return Err("old_string 不能为空（新增内容请用 write_file / apply_patch）".to_string());
        }
        let abs = resolve_session_path(ctx, &path)?;
        let original = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败：{e}"))?;
        let occurrences = original.matches(old_string.as_str()).count();
        if occurrences == 0 {
            return Err(format!(
                "未找到 old_string（{path}）：请确认空白/缩进与文件一致"
            ));
        }
        if occurrences > 1 && !replace_all {
            return Err(format!(
                "old_string 命中 {occurrences} 处（不唯一）：请扩大上下文或设置 replace_all=true"
            ));
        }
        let updated = if replace_all {
            original.replace(old_string.as_str(), new_string.as_str())
        } else {
            original.replacen(old_string.as_str(), new_string.as_str(), 1)
        };
        write_file_body(ctx, &path, &abs, &updated).await?;
        Ok(json!({
            "path": path,
            "replaced": if replace_all { occurrences } else { 1 },
            "replace_all": replace_all,
        }))
    }
}

/// 对同一文件做多处精准替换（取优合并自远端 engine）：一次调用替代多次
/// `edit_file`，省回合。原子性：任一处失败则整批不落盘。
struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "multi_edit".into(),
            description: "对同一文件做多处精准替换（一次调用替代多次 edit_file，省回合）。edits 按顺序应用——后面替换的 old_string 要匹配前面替换后的内容。原子性：任一处失败则整批不生效。old_string 必须先 read_file 确认且唯一（或设 replace_all）。自动快照，可 diff/revert。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" },
                    "edits": {
                        "type": "array",
                        "description": "替换列表（按顺序应用，上限 20 个）",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string", "description": "要替换的原文（必须唯一、含缩进）" },
                                "new_string": { "type": "string", "description": "替换后的内容" },
                                "replace_all": { "type": "boolean", "description": "替换该片段的全部出现位置（默认 false）" }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let raw_edits = args
            .get("edits")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if raw_edits.is_empty() {
            return Err("edits 不能为空".to_string());
        }
        if raw_edits.len() > 20 {
            return Err(format!(
                "edits 过多（{} 个 > 20）。请拆分为多次 multi_edit 调用",
                raw_edits.len()
            ));
        }
        // 预解析全部替换项（先整体校验参数，再动文件）。
        let mut planned: Vec<(String, String, bool)> = Vec::with_capacity(raw_edits.len());
        for (index, edit) in raw_edits.iter().enumerate() {
            let old_string = edit
                .get("old_string")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edits[{index}] 缺少 old_string"))?
                .to_string();
            let new_string = edit
                .get("new_string")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edits[{index}] 缺少 new_string"))?
                .to_string();
            if old_string.is_empty() {
                return Err(format!("edits[{index}] 的 old_string 不能为空"));
            }
            let replace_all = edit
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            planned.push((old_string, new_string, replace_all));
        }

        let abs = resolve_session_path(ctx, &path)?;
        if !abs.is_file() {
            return Err(format!("{path} 不存在（新建文件请用 write_file）"));
        }
        let mut content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败（仅支持 UTF-8 文本）：{e}"))?;

        // 内存中顺序应用；任一处失败立即返回，原文件不受影响。
        for (index, (old_string, new_string, replace_all)) in planned.iter().enumerate() {
            let occurrences = content.matches(old_string.as_str()).count();
            if occurrences == 0 {
                return Err(format!(
                    "第 {}/{} 处替换失败：未找到 old_string（前面的替换可能已改变上下文）。整批未应用，请 read_file 后重试",
                    index + 1,
                    planned.len()
                ));
            }
            if occurrences > 1 && !replace_all {
                return Err(format!(
                    "第 {}/{} 处替换失败：old_string 出现 {occurrences} 次。请扩大上下文使其唯一或设 replace_all。整批未应用",
                    index + 1,
                    planned.len()
                ));
            }
            content = if *replace_all {
                content.replace(old_string.as_str(), new_string.as_str())
            } else {
                content.replacen(old_string.as_str(), new_string.as_str(), 1)
            };
        }

        write_file_body(ctx, &path, &abs, &content).await?;
        Ok(json!({
            "path": path,
            "applied": planned.len(),
            "bytes_after": content.len(),
        }))
    }
}

/// 补丁段落：`old_lines`（上下文 + `-`）→ `new_lines`（上下文 + `+`）。
struct PatchHunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
}

/// 单个文件补丁操作。
enum PatchOp {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, hunks: Vec<PatchHunk> },
}

fn patch_op_path(op: &PatchOp) -> &str {
    match op {
        PatchOp::Add { path, .. } | PatchOp::Delete { path } | PatchOp::Update { path, .. } => path,
    }
}

/// 解析 Codex 风格补丁：`*** Add/Update/Delete File:` + `@@` 段落 + `+`/`-`/空格 行。
fn parse_patch(patch: &str) -> Result<Vec<PatchOp>, String> {
    fn close(
        current: &mut Option<PatchOp>,
        hunk: &mut Option<(Vec<String>, Vec<String>)>,
        ops: &mut Vec<PatchOp>,
    ) -> Result<(), String> {
        if let Some((old, new)) = hunk.take() {
            if old == new {
                return Err("补丁段落没有实际变化（- / + 内容相同）".to_string());
            }
            match current.as_mut() {
                Some(PatchOp::Update { hunks, .. }) => {
                    hunks.push(PatchHunk {
                        old_lines: old,
                        new_lines: new,
                    });
                }
                _ => return Err("@@ 段落只能出现在 Update File 下".to_string()),
            }
        }
        if let Some(op) = current.take() {
            if let PatchOp::Update { hunks, .. } = &op {
                if hunks.is_empty() {
                    return Err("Update File 缺少 @@ 段落".to_string());
                }
            }
            ops.push(op);
        }
        Ok(())
    }

    let mut ops: Vec<PatchOp> = Vec::new();
    let mut current: Option<PatchOp> = None;
    let mut hunk: Option<(Vec<String>, Vec<String>)> = None;
    for line in patch.lines() {
        if line.starts_with("*** Begin Patch") || line.starts_with("*** End Patch") {
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            close(&mut current, &mut hunk, &mut ops)?;
            current = Some(PatchOp::Add {
                path: path.trim().to_string(),
                content: String::new(),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            close(&mut current, &mut hunk, &mut ops)?;
            current = Some(PatchOp::Delete {
                path: path.trim().to_string(),
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            close(&mut current, &mut hunk, &mut ops)?;
            current = Some(PatchOp::Update {
                path: path.trim().to_string(),
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(op) = current.as_mut() else {
            if line.trim().is_empty() {
                continue;
            }
            return Err(format!("补丁格式错误：行不在任何文件操作下：{line}"));
        };
        match op {
            PatchOp::Add { content, .. } => {
                if let Some(rest) = line.strip_prefix('+') {
                    content.push_str(rest);
                    content.push('\n');
                } else if !line.trim().is_empty() {
                    return Err(format!("Add File 内容行必须以 + 开头：{line}"));
                }
            }
            PatchOp::Delete { .. } => {
                if !line.trim().is_empty() {
                    return Err(format!("Delete File 后不应再有内容行：{line}"));
                }
            }
            PatchOp::Update { hunks, .. } => {
                if line.starts_with("@@") {
                    if let Some((old, new)) = hunk.take() {
                        if old == new {
                            return Err("补丁段落没有实际变化（- / + 内容相同）".to_string());
                        }
                        hunks.push(PatchHunk {
                            old_lines: old,
                            new_lines: new,
                        });
                    }
                    hunk = Some((Vec::new(), Vec::new()));
                    continue;
                }
                let Some((old, new)) = hunk.as_mut() else {
                    if line.trim().is_empty() {
                        continue;
                    }
                    return Err(format!("Update File 内容必须位于 @@ 段落内：{line}"));
                };
                if let Some(rest) = line.strip_prefix('-') {
                    old.push(rest.to_string());
                } else if let Some(rest) = line.strip_prefix('+') {
                    new.push(rest.to_string());
                } else if let Some(rest) = line.strip_prefix(' ') {
                    old.push(rest.to_string());
                    new.push(rest.to_string());
                } else if !line.trim().is_empty() {
                    return Err(format!("补丁行必须以 + / - / 空格 开头：{line}"));
                }
            }
        }
    }
    close(&mut current, &mut hunk, &mut ops)?;
    if ops.is_empty() {
        return Err("补丁为空".to_string());
    }
    Ok(ops)
}

/// 逐段应用补丁：每段上下文必须**唯一命中**，否则报错（不改文件）。
fn apply_hunks(original: &str, hunks: &[PatchHunk]) -> Result<String, String> {
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let trailing_newline = original.ends_with('\n');
    for hunk in hunks {
        if hunk.old_lines.is_empty() {
            return Err("补丁段落缺少上下文（无法定位）".to_string());
        }
        let mut found: Option<usize> = None;
        let mut matches = 0usize;
        for index in 0..=lines.len().saturating_sub(hunk.old_lines.len()) {
            if lines[index..index + hunk.old_lines.len()] == hunk.old_lines[..] {
                matches += 1;
                if found.is_none() {
                    found = Some(index);
                }
            }
        }
        let Some(index) = found else {
            return Err(format!(
                "补丁上下文未命中（首个上下文行：{:?}）",
                hunk.old_lines.first()
            ));
        };
        if matches > 1 {
            return Err("补丁上下文命中多处（不唯一）：请扩大上下文".to_string());
        }
        lines.splice(
            index..index + hunk.old_lines.len(),
            hunk.new_lines.iter().cloned(),
        );
    }
    let mut result = lines.join("\n");
    if trailing_newline {
        result.push('\n');
    }
    Ok(result)
}

/// `apply_patch`：先校验整组补丁，再逐文件写入；写入走快照可 diff/revert。
struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description:
                "应用多文件补丁；可传 expected_hashes 做并发修改保护，成功后返回每个文件的哈希收据"
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string" },
                    "expected_hashes": {
                        "type": "object",
                        "description": "可选并发保护；按补丁中的相对路径提供全部文件的基线，Add 用 absent，Update/Delete 用 sha256:<hex>",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["patch"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let patch = required_string(&args, "patch")?;
        let ops = parse_patch(&patch)?;
        let expected_hashes = match args.get("expected_hashes") {
            None => None,
            Some(Value::Object(values)) => Some(values),
            Some(_) => return Err("expected_hashes 必须是对象".to_string()),
        };
        let mut seen = std::collections::HashSet::new();
        let mut prepared: Vec<(String, PathBuf, String, Option<Vec<u8>>, String)> =
            Vec::with_capacity(ops.len());
        for op in &ops {
            let path = patch_op_path(op).to_string();
            let abs = resolve_session_path(ctx, &path)?;
            let key = snapshot_key(&abs);
            if !seen.insert(key) {
                return Err(format!("补丁重复操作同一文件：{path}"));
            }
            let (content, base, kind) = match op {
                PatchOp::Add { content, .. } => {
                    let base = match tokio::fs::read(&abs).await {
                        Ok(_) => return Err(format!("Add File 目标已存在：{path}")),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(format!("Add File 检查 {path} 失败：{error}")),
                    };
                    (content.clone(), base, "add")
                }
                PatchOp::Delete { .. } => {
                    let base = tokio::fs::read(&abs)
                        .await
                        .map_err(|e| format!("Delete File 读取 {path} 失败：{e}"))?;
                    let content = String::from_utf8(base.clone())
                        .map_err(|e| format!("Delete File {path} 不是有效 UTF-8：{e}"))?;
                    (content, Some(base), "delete")
                }
                PatchOp::Update { hunks, .. } => {
                    let base = tokio::fs::read(&abs)
                        .await
                        .map_err(|e| format!("Update File 读取 {path} 失败：{e}"))?;
                    let original = String::from_utf8(base.clone())
                        .map_err(|e| format!("Update File {path} 不是有效 UTF-8：{e}"))?;
                    (apply_hunks(&original, hunks)?, Some(base), "update")
                }
            };
            if let Some(expected) = expected_hashes {
                let expected = expected
                    .get(&path)
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("expected_hashes 缺少文件基线：{path}"))?;
                let actual = base
                    .as_deref()
                    .map(|bytes| format!("sha256:{}", crate::CasStore::hash_of(bytes)))
                    .unwrap_or_else(|| "absent".to_string());
                if expected != actual {
                    return Err(format!(
                        "补丁基线冲突：{path} 当前为 {actual}，期望 {expected}"
                    ));
                }
            }
            prepared.push((path, abs, content, base, kind.to_string()));
        }
        if let Some(expected) = expected_hashes {
            if expected.len() != ops.len()
                || expected
                    .keys()
                    .any(|path| !ops.iter().any(|op| patch_op_path(op) == path))
            {
                return Err("expected_hashes 必须且只能包含补丁涉及的文件路径".to_string());
            }
        }

        let mut applied = Vec::with_capacity(ops.len());
        for (path, abs, content, base, kind) in prepared {
            let current = match tokio::fs::read(&abs).await {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(format!("写入前重新读取 {path} 失败：{error}")),
            };
            if current != base {
                let actual = current
                    .as_deref()
                    .map(|bytes| format!("sha256:{}", crate::CasStore::hash_of(bytes)))
                    .unwrap_or_else(|| "absent".to_string());
                return Err(format!(
                    "补丁基线冲突：{path} 在准备期间已变化，当前为 {actual}"
                ));
            }
            let base_hash = base
                .as_deref()
                .map(|bytes| format!("sha256:{}", crate::CasStore::hash_of(bytes)));
            let result_hash = if kind == "delete" {
                let key = snapshot_key(&abs);
                if let Some(snapshot) = ctx.session.snapshots.get(&key) {
                    let matches_snapshot = if let Some(expected_after) =
                        snapshot.expected_after_sha256.as_deref()
                    {
                        base.as_deref()
                            .is_some_and(|bytes| crate::CasStore::hash_of(bytes) == expected_after)
                    } else {
                        let original = snapshot
                            .original_b64
                            .as_deref()
                            .map(|encoded| BASE64.decode(encoded))
                            .transpose()
                            .map_err(|error| format!("快照解码失败：{error}"))?;
                        original == base
                    };
                    if !matches_snapshot {
                        return Err(format!(
                            "删除冲突：{path} 已在 Agent 记录的文件状态后再次变化"
                        ));
                    }
                } else {
                    ctx.session.snapshots.insert(
                        key,
                        crate::session::SnapshotEntry {
                            original_b64: base.as_deref().map(|bytes| BASE64.encode(bytes)),
                            expected_after_sha256: None,
                            turn: ctx.session.messages.len(),
                        },
                    );
                }
                tokio::fs::remove_file(&abs)
                    .await
                    .map_err(|e| format!("删除 {path} 失败：{e}"))?;
                Value::Null
            } else {
                let key = snapshot_key(&abs);
                ctx.session
                    .snapshots
                    .entry(key)
                    .or_insert_with(|| crate::session::SnapshotEntry {
                        original_b64: base.as_deref().map(|bytes| BASE64.encode(bytes)),
                        expected_after_sha256: None,
                        turn: ctx.session.messages.len(),
                    });
                write_file_body(ctx, &path, &abs, &content).await?;
                json!(format!(
                    "sha256:{}",
                    crate::CasStore::hash_of(content.as_bytes())
                ))
            };
            applied.push(json!({
                "path": path,
                "op": kind,
                "base_sha256": base_hash,
                "result_sha256": result_hash,
            }));
        }
        Ok(json!({ "ok": true, "files": applied }))
    }
}

/// Host-bound acceptance requirements for an ordinary Single task.
struct SingleVerificationPlanTool;

fn validate_single_verification_plan(
    plan: &crate::plan::VerificationPlanV1,
) -> Result<(), String> {
    plan.validate()?;
    if plan.plan_id.len() > 128 || plan.requirements.len() > 32 {
        return Err("VerificationPlan 超过宿主的 plan_id/requirement 数量上限".to_string());
    }
    for requirement in &plan.requirements {
        if requirement.covers_requirement_ids.is_empty() {
            return Err(format!(
                "requirement {} 必须声明覆盖的用户验收点",
                requirement.requirement_id
            ));
        }
        if requirement.validator_version.as_deref() != Some("1")
            || !crate::verification::is_registered_workspace_validator(&requirement.validator_id)
        {
            return Err(format!(
                "requirement {} 使用了未注册的宿主 validator/version",
                requirement.requirement_id
            ));
        }
        let crate::plan::VerificationScopeV1::WorkspacePaths { relative_paths } =
            &requirement.scope
        else {
            return Err(format!(
                "requirement {} 必须绑定 WorkspacePaths",
                requirement.requirement_id
            ));
        };
        if relative_paths.len() > 16
            || !crate::verification::workspace_validator_arguments_supported(
                &requirement.validator_id,
                &requirement.arguments,
            )
        {
            return Err(format!(
                "requirement {} 的路径数量或 validator 参数不符合宿主注册契约",
                requirement.requirement_id
            ));
        }
        let resources = &requirement.resources;
        if resources.cpu_slots != 1
            || !(8..=128).contains(&resources.memory_mb)
            || resources.exclusive_workspace
            || !(1..=30_000).contains(&resources.timeout_ms)
        {
            return Err(format!(
                "requirement {} 的资源声明超出宿主验证器预算",
                requirement.requirement_id
            ));
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for SingleVerificationPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "verification_plan".into(),
            description: "登记本次任务的宿主验收要求。每项都要填写 covers_requirement_ids 对应用户验收点；代码任务的源码路径必须被 workspace-command-success-v1 覆盖，arguments.command 必须与本回合实际 run_command 一致。计划不是通过证据；宿主在回合结束时按绑定源码执行检查并生成回执。resources 四个字段均须显式填写。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "object",
                        "properties": {
                            "plan_id": {"type": "string"},
                            "requirements": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "requirement_id": {"type": "string"},
                                        "covers_requirement_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                                        "validator_id": {"type": "string", "enum": ["workspace-file-exists-v1", "workspace-file-non-empty-v1", "workspace-file-contains-v1", "workspace-json-field-equals-v1", "workspace-command-success-v1"]},
                                        "validator_version": {"type": "string", "enum": ["1"]},
                                        "scope": {
                                            "type": "object",
                                            "properties": {
                                                "kind": {"type": "string", "enum": ["workspace_paths"]},
                                                "relative_paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 16}
                                            },
                                            "required": ["kind", "relative_paths"]
                                        },
                                        "arguments": {"type": "object"},
                                        "required": {"type": "boolean"},
                                        "resources": {
                                            "type": "object",
                                            "properties": {
                                                "cpu_slots": {"type": "integer", "enum": [1]},
                                                "memory_mb": {"type": "integer", "minimum": 8, "maximum": 128, "default": 16},
                                                "exclusive_workspace": {"type": "boolean", "enum": [false]},
                                                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 30000, "default": 30000}
                                            },
                                            "required": ["cpu_slots", "memory_mb", "exclusive_workspace", "timeout_ms"]
                                        }
                                    },
                                    "required": ["requirement_id", "covers_requirement_ids", "validator_id", "validator_version", "scope", "arguments", "required", "resources"]
                                }
                            }
                        },
                        "required": ["plan_id", "requirements"]
                    }
                },
                "required": ["plan"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let plan: crate::plan::VerificationPlanV1 = serde_json::from_value(
            args.get("plan").cloned().ok_or("缺少 plan 对象")?
        ).map_err(|error| format!("VerificationPlan 结构非法：{error}"))?;
        validate_single_verification_plan(&plan)?;
        let input_sha256 = ctx
            .session
            .active_turn_input_sha256
            .clone()
            .ok_or("当前 Agent 回合没有可绑定的用户输入摘要")?;
        let turn_id = ctx
            .session
            .active_turn_id
            .clone()
            .ok_or("当前 Agent 回合没有可绑定的 turn_id")?;
        let has_current_turn_writes = ctx
            .session
            .execution_receipts
            .iter()
            .any(|receipt| receipt.turn_id == turn_id && receipt.status != "reverted");
        if has_current_turn_writes
            && (ctx.session.single_verification_plan_turn_id.as_deref() != Some(turn_id.as_str())
                || ctx.session.single_verification_plan.as_ref() != Some(&plan))
        {
            return Err("VerificationPlan 必须在首次工作区写入前登记，且本回合登记后不可替换".to_string());
        }
        if ctx.session.single_verification_plan_turn_id.as_deref() == Some(turn_id.as_str())
            && ctx.session.single_verification_plan.as_ref().is_some_and(|registered| registered != &plan)
        {
            return Err("本回合 VerificationPlan 已冻结，不能在看到验证结果后降低验收要求".to_string());
        }
        ctx.session.single_verification_plan = Some(plan.clone());
        ctx.session.single_verification_plan_input_sha256 = Some(input_sha256);
        ctx.session.single_verification_plan_turn_id = Some(turn_id);
        Ok(json!({"plan": plan, "status": "registered_pending_host_validation"}))
    }
}

/// `todo`：会话级任务清单（整表替换；CLI `/todo` 渲染）。
struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".into(),
            description: "写入/更新任务清单（整表替换；status: pending|in_progress|completed）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let items = args
            .get("todos")
            .and_then(Value::as_array)
            .ok_or("todos 必须是数组")?;
        let mut todos = Vec::with_capacity(items.len());
        for item in items {
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if content.is_empty() {
                return Err("todo.content 不能为空".to_string());
            }
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            if !matches!(status, "pending" | "in_progress" | "completed") {
                return Err(format!(
                    "todo.status 非法：{status}（pending|in_progress|completed）"
                ));
            }
            todos.push(crate::session::TodoItem {
                content,
                status: status.to_string(),
            });
        }
        ctx.session.todos = todos;
        let rendered = ctx
            .session
            .todos
            .iter()
            .map(|todo| {
                let mark = match todo.status.as_str() {
                    "completed" => "x",
                    "in_progress" => ">",
                    _ => " ",
                };
                format!("[{mark}] {}", todo.content)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(json!({ "todos": ctx.session.todos, "rendered": rendered }))
    }
}

/// 极简 HTML → 文本：去 script/style 与标签，合并空白（web_fetch 用）。
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        let Some(start) = rest.find('<') else {
            text.push_str(rest);
            break;
        };
        text.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            break;
        };
        let tag = rest[start + 1..start + end].to_ascii_lowercase();
        if tag.starts_with("script") || tag.starts_with("style") {
            let close = if tag.starts_with("script") {
                "</script"
            } else {
                "</style"
            };
            match rest[start + end + 1..].find(close) {
                Some(offset) => {
                    let after = start + end + 1 + offset;
                    match rest[after..].find('>') {
                        Some(gt) => {
                            rest = &rest[after + gt + 1..];
                            continue;
                        }
                        None => break,
                    }
                }
                None => break,
            }
        }
        text.push(' ');
        rest = &rest[start + end + 1..];
    }
    let decoded = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 解码来路不明的字节（子进程输出 / 未声明 charset 的响应体等）。
///
/// Windows 控制台程序按**系统代码页**输出（简中为 OEM 936/GBK），而这里曾经一律用
/// `String::from_utf8_lossy` 解码——非 UTF-8 字节会变成替换字符，于是**所有中文命令
/// 输出都乱码**。实测审计原文：
///
/// ```text
/// 命令  : echo 中文输出测试
/// stdout: '�����������'
/// ```
///
/// 顺序：UTF-8 严格 → 系统代码页 → lossy 兜底。UTF-8 优先保证现代工具（git / rg /
/// python / 多数 API）的中文输出不被二次误解；只有严格解析失败才认为它是本机代码页。
pub(crate) fn decode_process_output(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    #[cfg(windows)]
    if let Some(text) = decode_with_system_codepage(bytes) {
        return text;
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 按 Windows 系统 OEM 代码页（`GetOEMCP`）把字节转成 UTF-8。
///
/// 用 OEM 而非 ANSI 代码页：控制台程序（cmd / 批处理 / 多数 CLI）输出走 OEM 代码页，
/// 简中环境下是 936（GBK）。
#[cfg(windows)]
fn decode_with_system_codepage(bytes: &[u8]) -> Option<String> {
    use windows::Win32::Globalization::{
        GetOEMCP, MultiByteToWideChar, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
    };
    if bytes.is_empty() {
        return Some(String::new());
    }
    unsafe {
        let codepage = GetOEMCP();
        let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
        let len = MultiByteToWideChar(codepage, flags, bytes, None);
        if len <= 0 {
            return None;
        }
        let mut wide = vec![0u16; len as usize];
        let written = MultiByteToWideChar(codepage, flags, bytes, Some(&mut wide));
        if written <= 0 {
            return None;
        }
        wide.truncate(written as usize);
        Some(String::from_utf16_lossy(&wide))
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("OwO-Agent/1.0 (+web)");
    // 显式代理优先：仓库把 `OWO_HTTP_PROXY` 定为出网代理开关（AGENTS.md），
    // 但 reqwest 只自动识别 HTTP_PROXY/HTTPS_PROXY/ALL_PROXY，不认这个前缀——
    // 于是"文档说配了代理、实际没走"，在受限网络里表现为所有网络工具静默失败。
    if let Some(proxy) = std::env::var("OWO_HTTP_PROXY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        builder = builder.proxy(
            reqwest::Proxy::all(&proxy)
                .map_err(|error| format!("代理配置无效（OWO_HTTP_PROXY={proxy}）：{error}"))?,
        );
    }
    builder
        .build()
        .map_err(|error| format!("HTTP 客户端构造失败：{error}"))
}

/// `web_fetch`：抓取 URL 并返回纯文本（网络出口，需审批）。
struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "抓取 URL 并返回纯文本（20s 超时、默认 256KB 上限；网络访问需审批）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "max_bytes": { "type": "integer", "description": "响应上限（默认 262144）" }
                },
                "required": ["url"]
            }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let url = required_string(&args, "url")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("url 必须以 http:// 或 https:// 开头".to_string());
        }
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(262_144)
            .clamp(1024, 1_048_576) as usize;
        let client = http_client()?;
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("抓取失败：{error}"))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("读取响应失败：{error}"))?;
        let truncated = bytes.len() > max_bytes;
        // 响应体未必是 UTF-8：国内站点常见 GBK 且不带 charset 声明，用 lossy 解会整页乱码。
        let raw = decode_process_output(&bytes[..bytes.len().min(max_bytes)]);
        let text = if content_type.contains("html") {
            html_to_text(&raw)
        } else {
            raw
        };
        Ok(json!({
            "url": url,
            "status": status,
            "content_type": content_type,
            "truncated": truncated,
            "bytes": bytes.len(),
            "text": text,
        }))
    }
}

/// `web_search`：默认走 DuckDuckGo HTML 端点，可用 `OWO_WEB_SEARCH_URL` 覆盖。
struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "网页搜索（按 DuckDuckGo → Bing → DDG Lite 顺序自动选择可用端点；\
                          可用 OWO_WEB_SEARCH_URL 指定端点、OWO_HTTP_PROXY 指定出网代理）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let query = required_string(&args, "query")?;
        // 显式配置优先；未配置时按候选顺序尝试（见 SEARCH_ENDPOINTS 说明）。
        let configured = std::env::var("OWO_WEB_SEARCH_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let endpoints: Vec<String> = match configured {
            Some(endpoint) => vec![endpoint],
            None => SEARCH_ENDPOINTS
                .iter()
                .map(|item| item.to_string())
                .collect(),
        };
        let client = http_client()?;
        let mut attempts: Vec<String> = Vec::new();
        for endpoint in &endpoints {
            match client
                .get(endpoint)
                .query(&[("q", query.as_str())])
                .send()
                .await
            {
                Err(error) => attempts.push(format!("{endpoint} → 请求失败：{error}")),
                Ok(response) => {
                    let status = response.status().as_u16();
                    if !(200..300).contains(&status) {
                        attempts.push(format!("{endpoint} → HTTP {status}"));
                        continue;
                    }
                    let body = match response.text().await {
                        Ok(body) => body,
                        Err(error) => {
                            attempts.push(format!("{endpoint} → 读取响应失败：{error}"));
                            continue;
                        }
                    };
                    let results = parse_search_results(endpoint, &body);
                    if results.is_empty() {
                        attempts.push(format!("{endpoint} → 无结果（页面结构可能已变）"));
                        continue;
                    }
                    return Ok(json!({
                        "query": query,
                        "engine": endpoint,
                        "status": status,
                        "count": results.len(),
                        "results": results,
                    }));
                }
            }
        }
        // 全部候选都失败：把每一次的原因列出来，并给出可操作的下一步，
        // 而不是丢一句笼统的"搜索请求失败"让调用方猜。
        Err(format!(
            "搜索失败：{}。排查建议：① 若在受限网络，设置 OWO_HTTP_PROXY 指向可用代理；\
             ② 或用 OWO_WEB_SEARCH_URL 指定可达的搜索端点（如 https://cn.bing.com/search）。",
            attempts.join("；")
        ))
    }
}

/// 搜索端点候选：按顺序尝试，第一个返回可解析结果的胜出。
///
/// 为什么必须 fallback：默认的 DuckDuckGo HTML 端点在部分网络环境**完全不可达**
/// （实测 3 次尝试全部 `error sending request`，而同期 `web_fetch` 抓 example.com /
/// crates.io 均 200——是站点级不可达，不是网络故障）。搜索是高频工具，不该因为一个
/// 境外站点就整体不可用；国内可达的 Bing 作为第二档，DDG Lite 作为第三档（结构更简）。
const SEARCH_ENDPOINTS: &[&str] = &[
    "https://html.duckduckgo.com/html/",
    "https://cn.bing.com/search",
    "https://lite.duckduckgo.com/lite/",
];

/// 按端点选择解析器：不同引擎的 HTML 结构完全不同，不能共用一套规则。
fn parse_search_results(endpoint: &str, html: &str) -> Vec<Value> {
    if endpoint.contains("bing.com") {
        parse_bing_results(html)
    } else {
        parse_ddg_results(html)
    }
}

/// 解析 Bing 结果（`<li class="b_algo">` 内的 `<h2><a href=…>`）。
///
/// 候选端点里的 cn.bing.com 是为了在 DuckDuckGo 不可达的网络下仍能搜索；
/// 它的结构是「结果块 → h2 → 锚点」，与 DDG 的 `class="result__a"` 无关。
fn parse_bing_results(html: &str) -> Vec<Value> {
    let mut results = Vec::new();
    let mut rest = html;
    while let Some(position) = rest.find("class=\"b_algo\"") {
        let block = &rest[position..];
        let Some(h2) = block.find("<h2") else {
            break;
        };
        let scoped = &block[h2..];
        let Some(anchor) = scoped.find("<a ") else {
            break;
        };
        let Some(href_offset) = scoped[anchor..].find("href=\"") else {
            break;
        };
        let href_start = anchor + href_offset + 6;
        let Some(href_end) = scoped[href_start..].find('"') else {
            break;
        };
        let url = scoped[href_start..href_start + href_end].to_string();
        let Some(gt) = scoped[href_start..].find('>') else {
            break;
        };
        let title_start = href_start + gt + 1;
        let Some(close) = scoped[title_start..].find("</a>") else {
            break;
        };
        let title = html_to_text(&scoped[title_start..title_start + close]);
        let consumed = position + title_start + close;
        // 只收外链结果：Bing 内嵌了不少站内导航锚点，它们也是 <a>。
        if url.starts_with("http") && !title.is_empty() {
            results.push(json!({ "title": title, "url": url }));
            if results.len() >= 10 {
                break;
            }
        }
        // 无论是否采纳都要前进，否则同一块会被反复解析（死循环）。
        rest = &rest[consumed.min(rest.len())..];
    }
    results
}

/// 解析 DuckDuckGo HTML（`class="result__a"`）与 DDG Lite；最多 10 条。
fn parse_ddg_results(html: &str) -> Vec<Value> {
    let mut results = Vec::new();
    let mut rest = html;
    while let Some(position) = rest.find("class=\"result__a\"") {
        let anchor_start = rest[..position].rfind("<a ").unwrap_or(position);
        let href = rest[anchor_start..position]
            .find("href=\"")
            .map(|offset| {
                let start = anchor_start + offset + 6;
                rest[start..]
                    .find('"')
                    .map(|end| decode_search_href(&rest[start..start + end]))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let Some(gt) = rest[position..].find('>') else {
            break;
        };
        let title_start = position + gt + 1;
        let Some(close) = rest[title_start..].find("</a>") else {
            break;
        };
        let title = html_to_text(&rest[title_start..title_start + close]);
        if !title.is_empty() {
            results.push(json!({ "title": title, "url": href }));
        }
        rest = &rest[title_start + close..];
        if results.len() >= 10 {
            break;
        }
    }
    results
}

/// DDG 跳转链接解码（`uddg=` + percent-encoding）。
fn decode_search_href(href: &str) -> String {
    if let Some(index) = href.find("uddg=") {
        let encoded = &href[index + 5..];
        let encoded = encoded.split('&').next().unwrap_or(encoded);
        return percent_decode(encoded);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `read_image`：读取图片并返回元数据 + base64（视觉理解需多模态模型支持）。
struct ReadImageTool;

#[async_trait]
impl Tool for ReadImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "读取图片文件（返回 mime/尺寸/base64；默认 4MB 上限）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let abs = resolve_session_path(ctx, &path)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败：{e}"))?;
        const MAX_BYTES: usize = 4 * 1024 * 1024;
        if bytes.len() > MAX_BYTES {
            return Err(format!(
                "图片过大（{} 字节 > {MAX_BYTES}）：请先压缩",
                bytes.len()
            ));
        }
        let mime = match abs
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            other => return Err(format!("不支持的图片扩展名：{other}")),
        };
        Ok(json!({
            "path": path,
            "mime": mime,
            "bytes": bytes.len(),
            "base64": BASE64.encode(&bytes),
            "note": "多模态视觉理解需要 gateway 支持图像内容；当前返回原始数据供工具/上层使用",
        }))
    }
}

/// 后台 shell 记录（`run_command background=true` 产生）。
#[derive(Clone)]
struct BackgroundShell {
    handle: crate::sandbox::SandboxHandle,
    log_path: PathBuf,
    done: Arc<std::sync::atomic::AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
}

fn background_shells() -> &'static Mutex<HashMap<String, BackgroundShell>> {
    static SHELLS: std::sync::OnceLock<Mutex<HashMap<String, BackgroundShell>>> =
        std::sync::OnceLock::new();
    SHELLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `shell_output`：读取后台 shell 的累积输出与状态。
struct ShellOutputTool;

#[async_trait]
impl Tool for ShellOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell_output".into(),
            description: "查看后台 shell 的输出与状态（shell_id 来自 run_command background=true）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "shell_id": { "type": "string" } },
                "required": ["shell_id"]
            }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let shell_id = required_string(&args, "shell_id")?;
        let shell = background_shells()
            .lock()
            .map_err(|_| "后台 shell 注册表中毒".to_string())?
            .get(&shell_id)
            .cloned()
            .ok_or_else(|| format!("未知 shell_id：{shell_id}"))?;
        let log = std::fs::read(&shell.log_path).unwrap_or_default();
        const TAIL_BYTES: usize = 32 * 1024;
        let slice = if log.len() > TAIL_BYTES {
            &log[log.len() - TAIL_BYTES..]
        } else {
            &log[..]
        };
        let done = shell.done.load(std::sync::atomic::Ordering::Relaxed);
        let exit_code = *shell
            .exit_code
            .lock()
            .map_err(|_| "后台 shell 状态锁中毒".to_string())?;
        Ok(json!({
            "shell_id": shell_id,
            "running": !done,
            "exit_code": exit_code,
            "log_path": shell.log_path.display().to_string(),
            "output": decode_process_output(slice),
        }))
    }
}

/// `kill_shell`：终止后台 shell（仅限本 Agent 启动的 shell）。
struct KillShellTool;

#[async_trait]
impl Tool for KillShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kill_shell".into(),
            description: "终止后台 shell（仅限 run_command background=true 启动的 shell）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "shell_id": { "type": "string" } },
                "required": ["shell_id"]
            }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let shell_id = required_string(&args, "shell_id")?;
        let shell = background_shells()
            .lock()
            .map_err(|_| "后台 shell 注册表中毒".to_string())?
            .get(&shell_id)
            .cloned()
            .ok_or_else(|| format!("未知 shell_id：{shell_id}"))?;
        let killed = {
            let manager = crate::sandbox::default_manager();
            let mut manager = manager
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            manager.kill(&shell.handle).is_ok()
        };
        shell.done.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({ "shell_id": shell_id, "killed": killed }))
    }
}

/// 白名单受限写入工具（七期 · 二路）：与 [`WriteFileTool`] 同语义（快照可
/// diff/revert），但写目标必须落在 `allowed` 绝对路径前缀内——工具面层强制，
/// 叠加在审批策略之上（权限三道闸：注册表面 → 白名单前缀 → 审批）。
struct WhitelistWriteFileTool {
    /// 允许写入的绝对路径前缀（canonicalize 口径；空 = 仅限工作区根内）。
    allowed: Vec<PathBuf>,
}

/// 白名单前缀判定（纯路径版，便于测试）：candidate 是否落在任一 allowed 前缀内。
/// 空白名单在此返回 false——「空 = 未约束」的放行语义由调用方
/// （[`WhitelistWriteFileTool::resolve_whitelisted`]）短路处理。
fn path_in_whitelist(candidate: &Path, allowed: &[PathBuf]) -> bool {
    allowed.iter().any(|base| candidate.starts_with(base))
}

/// 去掉 Windows verbatim 前缀（`\\?\C:\...` → `C:\...`）：canonicalize 语义不变。
/// 与服务端绑定侧（root/allowed 存储前去前缀）的路径口径对齐——否则
/// `starts_with` 前缀比对在 verbatim × 非 verbatim 混用时恒为 false，
/// 白名单内写入会被误拒。
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped.to_string()),
        None => path.to_path_buf(),
    }
}

impl WhitelistWriteFileTool {
    /// 解析 + 白名单校验：与 [`resolve_session_path`] 同口径解析目标路径
    /// （canonicalize；不存在的目标按「父目录 canonicalize + 文件名」解析），
    /// 比对前去 verbatim 前缀（两侧同口径），不在白名单内 → Err。
    fn resolve_whitelisted(&self, ctx: &ToolContext<'_>, path: &str) -> Result<PathBuf, String> {
        let abs = resolve_session_path(ctx, path)?;
        if self.allowed.is_empty() {
            return Ok(abs);
        }
        let candidate = abs.canonicalize().unwrap_or_else(|_| {
            abs.parent()
                .and_then(|parent| parent.canonicalize().ok())
                .map(|parent| parent.join(abs.file_name().unwrap_or_default()))
                .unwrap_or_else(|| abs.clone())
        });
        let candidate = strip_verbatim_prefix(&candidate);
        let allowed: Vec<PathBuf> = self
            .allowed
            .iter()
            .map(|base| strip_verbatim_prefix(base))
            .collect();
        if path_in_whitelist(&candidate, &allowed) {
            return Ok(abs);
        }
        let list = self
            .allowed
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("写入目标不在写白名单内：{path}（白名单：{list}）"))
    }
}

#[async_trait]
impl Tool for WhitelistWriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "写入工作区内的文件（仅限白名单路径；自动快照，可 diff/revert）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let content = required_string(&args, "content")?;
        let abs = self.resolve_whitelisted(ctx, &path)?;
        write_file_body(ctx, &path, &abs, &content).await
    }
}

/// 白名单受限补丁工具：在交给补丁执行器前校验每个文件目标。
struct WhitelistApplyPatchTool {
    allowed: Vec<PathBuf>,
}

#[async_trait]
impl Tool for WhitelistApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        let mut spec = ApplyPatchTool.spec();
        spec.description =
            "应用多文件精确补丁（仅限白名单路径；支持 expected_hashes 与哈希收据）".into();
        spec
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let patch = required_string(&args, "patch")?;
        let guard = WhitelistWriteFileTool {
            allowed: self.allowed.clone(),
        };
        for op in parse_patch(&patch)? {
            guard.resolve_whitelisted(ctx, patch_op_path(&op))?;
        }
        ApplyPatchTool.run(ctx, args).await
    }
}

/// 工具沙箱策略：默认工作区范围 + Job 隔离（允许显式降级，审计记录）。
/// `unrestricted` 档位显式放开文件/网络范围（`SandboxPolicy::validate` 要求显式开关）。
fn tool_sandbox_policy(ctx: &ToolContext<'_>, name: &str) -> crate::sandbox::SandboxPolicy {
    let mut policy = crate::sandbox::SandboxPolicy::for_workspace(name, ctx.workspace);
    policy.require_isolation = crate::sandbox::IsolationLevel::JobOnly;
    policy.allow_degraded = true;
    if ctx.policy.profile() == crate::permissions::PermissionProfile::Unrestricted {
        policy.file_scope = crate::FileScope::Unrestricted;
        policy.allow_unrestricted_file = true;
        policy.network_policy = crate::NetworkPolicy::Unrestricted;
        policy.allow_unrestricted_network = true;
    }
    policy
}

struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".into(),
            description: "列出工作区内目录条目".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| ".".to_string());
        let abs = resolve_session_path(ctx, &path)?;
        let mut entries = Vec::new();
        let mut reader = tokio::fs::read_dir(&abs)
            .await
            .map_err(|e| format!("读取目录 {path} 失败：{e}"))?;
        while let Some(entry) = reader.next_entry().await.map_err(|e| e.to_string())? {
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "is_dir": entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false),
            }));
        }
        Ok(json!({ "path": path, "entries": entries }))
    }
}

struct SearchFilesTool;

#[async_trait]
impl Tool for SearchFilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "search_files".into(),
            description: "使用随包 ripgrep 按**文件名关键字**递归搜索工作区文件（只读）。\
                          关键字是字面量匹配，不是正则——传 `\\.(txt|md)$` 之类的正则会搜不到东西"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "文件名关键字（字面量，非正则），例如 notes、.md、config"
                    }
                },
                "required": ["pattern"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let pattern = required_string(&args, "pattern")?;
        if pattern.trim().is_empty() {
            return Err("搜索模式不能为空".to_string());
        }
        let rg = external_tools::resolve_ripgrep().ok_or_else(|| {
            "随包 ripgrep 不可用：请重新安装 OwO Agent，或仅在测试时设置 OWO_EXTERNAL_TOOLS_DIR"
                .to_string()
        })?;

        let mut policy = tool_sandbox_policy(ctx, "search_files");
        policy.cpu_ms = Some(30_000);
        policy.mem_mb = Some(512);
        let mut sandbox_command =
            crate::sandbox::SandboxCommand::new(rg.to_string_lossy().into_owned(), policy)
                .with_args(vec![
                    "--files".to_string(),
                    "--hidden".to_string(),
                    "--glob".to_string(),
                    "!.git/**".to_string(),
                    "--glob".to_string(),
                    "!target/**".to_string(),
                    "--glob".to_string(),
                    "!node_modules/**".to_string(),
                    "--iglob".to_string(),
                    format!("*{}*", pattern),
                ])
                .with_cwd(ctx.workspace.to_path_buf());
        if let Some(path) = external_tools::path_with_bundled_tools() {
            sandbox_command.env.push(("PATH".to_string(), path));
        }

        let manager = crate::sandbox::default_manager();
        let process = {
            let mut manager = manager
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            manager
                .spawn(&sandbox_command)
                .map_err(|error| format!("搜索沙箱拒绝执行：{error}"))?
        };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::task::spawn_blocking(move || {
                let mut process = process;
                process.wait_output()
            }),
        )
        .await
        .map_err(|_| "ripgrep 搜索超时（30s，进程仍在受限 Job 内）".to_string())?
        .map_err(|join_error| format!("搜索等待失败：{join_error}"))?
        .map_err(|error| format!("ripgrep 执行失败：{error}"))?;

        if output.exit_code != 0 && output.exit_code != 1 {
            return Err(format!(
                "ripgrep 搜索失败（exit_code={}）：{}",
                output.exit_code,
                decode_process_output(&output.stderr).trim()
            ));
        }
        let matches = decode_process_output(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .take(200)
            .map(|line| line.replace('\\', "/"))
            .collect::<Vec<_>>();
        Ok(json!({
            "pattern": pattern,
            "matches": matches,
            "tool": "ripgrep",
            "tool_version": external_tools::RIPGREP_VERSION,
        }))
    }
}

/// `grep`：用随包 ripgrep 做**内容**检索（只读，正则）。
struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "使用随包 ripgrep 做内容检索（正则；只读，返回 path/line/text）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "正则表达式或字面量" },
                    "path": { "type": "string", "description": "可选：检索子路径（默认工作区根）" },
                    "glob": { "type": "string", "description": "可选：文件名过滤，如 *.rs" },
                    "max_results": { "type": "integer", "description": "最多返回条数（默认 100，上限 500）" }
                },
                "required": ["pattern"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let pattern = required_string(&args, "pattern")?;
        if pattern.trim().is_empty() {
            return Err("检索模式不能为空".to_string());
        }
        let rg = external_tools::resolve_ripgrep().ok_or_else(|| {
            "随包 ripgrep 不可用：请重新安装 OwO Agent，或仅在测试时设置 OWO_EXTERNAL_TOOLS_DIR"
                .to_string()
        })?;
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let search_path = match args.get("path").and_then(Value::as_str) {
            Some(path) if !path.trim().is_empty() => resolve_session_path(ctx, path)?,
            _ => ctx.workspace.to_path_buf(),
        };

        let mut rg_args = vec![
            "--json".to_string(),
            "--glob".to_string(),
            "!.git/**".to_string(),
            "--glob".to_string(),
            "!target/**".to_string(),
            "--glob".to_string(),
            "!node_modules/**".to_string(),
        ];
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            if !glob.trim().is_empty() {
                rg_args.push("--glob".to_string());
                rg_args.push(glob.to_string());
            }
        }
        rg_args.push("-e".to_string());
        rg_args.push(pattern.clone());
        rg_args.push(search_path.to_string_lossy().into_owned());

        let mut policy = tool_sandbox_policy(ctx, "grep");
        policy.cpu_ms = Some(30_000);
        policy.mem_mb = Some(512);
        let mut sandbox_command =
            crate::sandbox::SandboxCommand::new(rg.to_string_lossy().into_owned(), policy)
                .with_args(rg_args)
                .with_cwd(ctx.workspace.to_path_buf());
        if let Some(path) = external_tools::path_with_bundled_tools() {
            sandbox_command.env.push(("PATH".to_string(), path));
        }

        let manager = crate::sandbox::default_manager();
        let process = {
            let mut manager = manager
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            manager
                .spawn(&sandbox_command)
                .map_err(|error| format!("检索沙箱拒绝执行：{error}"))?
        };
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::task::spawn_blocking(move || {
                let mut process = process;
                process.wait_output()
            }),
        )
        .await
        .map_err(|_| "ripgrep 检索超时（30s，进程仍在受限 Job 内）".to_string())?
        .map_err(|join_error| format!("检索等待失败：{join_error}"))?
        .map_err(|error| format!("ripgrep 执行失败：{error}"))?;

        if output.exit_code != 0 && output.exit_code != 1 {
            return Err(format!(
                "ripgrep 检索失败（exit_code={}）：{}",
                output.exit_code,
                decode_process_output(&output.stderr).trim()
            ));
        }
        // `--json` 输出逐行解析，避免 Windows 盘符冒号破坏 `path:line:text` 切分。
        let mut matches: Vec<Value> = decode_process_output(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| value.get("type").and_then(Value::as_str) == Some("match"))
            .filter_map(|value| {
                let data = value.get("data")?;
                let path = data.get("path")?.get("text")?.as_str()?.replace('\\', "/");
                let line_no = data.get("line_number")?.as_u64()?;
                let raw = data.get("lines")?.get("text")?.as_str()?.trim_end();
                let text: String = raw.chars().take(300).collect();
                Some(json!({ "path": path, "line": line_no, "text": text }))
            })
            .take(max_results + 1)
            .collect();
        let truncated = matches.len() > max_results;
        matches.truncate(max_results);
        Ok(json!({
            "pattern": pattern,
            "matches": matches,
            "truncated": truncated,
            "tool": "ripgrep",
            "tool_version": external_tools::RIPGREP_VERSION,
        }))
    }
}

struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "在工作区内执行 shell 命令（需审批，60 秒超时）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "background": { "type": "boolean", "description": "后台运行（返回 shell_id，用 shell_output/kill_shell 管理）" }
                },
                "required": ["command"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let command = required_string(&args, "command")?;
        let timeout_ms = args
            .get("_host_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000)
            .clamp(1, 60_000);
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .map(|path| resolve_session_path(ctx, &path))
            .transpose()?
            .unwrap_or_else(|| ctx.workspace.to_path_buf());

        // 沙箱门卫：run_command 统一经 SandboxManager 执行（X01）。
        // 策略：工作区作用域 + 危险片段 deny + Job 级隔离（允许显式降级，审计记录）。
        let mut policy = tool_sandbox_policy(ctx, "run_command");
        policy.cpu_ms = Some(60_000);
        policy.mem_mb = Some(1024);
        // `cmd /C <外部命令>` 至少占 2 个 Job 进程（cmd + 子进程）；默认 limit=1 会
        // 直接报 "Not enough quota"。放宽到 16，仍能兜住进程炸弹。
        policy.active_process_limit = Some(16);
        // 命令文本（cmd /C <command> 的命令体）同样过 deny 检查。
        if let Some(fragment) =
            crate::sandbox::SandboxCommand::deny_hit(&command, &policy.deny_programs)
        {
            return Err(format!("命令命中危险黑名单片段：{fragment}"));
        }
        let mut sandbox_command = crate::sandbox::SandboxCommand::new("cmd", policy.clone())
            .with_args(vec!["/C".to_string(), command.to_string()])
            .with_cwd(cwd.clone());
        if args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // 后台 shell：输出重定向到日志文件，句柄入注册表，watcher 任务等待退出。
            // 日志必须落在**工作区内**：沙箱文件作用域只允许工作区，写到 %TEMP% 会被拒。
            let shell_id = uuid::Uuid::new_v4().to_string();
            // 去 verbatim 前缀：`\\?\C:\…` 在沙箱（受限令牌）下做 cmd 重定向会失败。
            let workspace_plain = strip_verbatim_prefix(ctx.workspace);
            let log_dir = workspace_plain.join(".owo").join("shells");
            tokio::fs::create_dir_all(&log_dir)
                .await
                .map_err(|error| format!("创建后台日志目录失败：{error}"))?;
            let log_path = log_dir.join(format!("{shell_id}.log"));
            // 用包装脚本而不是 `cmd /C "<cmd> > "<log>" 2>&1"`：嵌套引号会被沙箱的
            // 参数引用破坏（cmd 提前截断 → exit 1、日志不生成）。
            let script_path = log_dir.join(format!("{shell_id}.cmd"));
            let script = format!(
                "@echo off\r\n{} > \"{}\" 2>&1\r\nexit /b %ERRORLEVEL%\r\n",
                command,
                log_path.display()
            );
            tokio::fs::write(&script_path, script)
                .await
                .map_err(|error| format!("写入后台脚本失败：{error}"))?;
            let mut background_command = crate::sandbox::SandboxCommand::new("cmd", policy.clone())
                .with_args(vec![
                    "/C".to_string(),
                    script_path.to_string_lossy().into_owned(),
                ])
                .with_cwd(cwd.clone());
            if let Some(path) = external_tools::path_with_bundled_tools() {
                background_command.env.push(("PATH".to_string(), path));
            }
            let manager = crate::sandbox::default_manager();
            let process = {
                let mut manager = manager
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                manager
                    .spawn(&background_command)
                    .map_err(|error| format!("沙箱拒绝执行（{command}）：{error}"))?
            };
            let handle = process.handle.clone();
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
            let done_task = Arc::clone(&done);
            let code_task = Arc::clone(&exit_code);
            tokio::task::spawn_blocking(move || {
                let mut process = process;
                if let Ok(info) = process.wait_output() {
                    if let Ok(mut guard) = code_task.lock() {
                        *guard = Some(info.exit_code);
                    }
                }
                done_task.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            background_shells()
                .lock()
                .map_err(|_| "后台 shell 注册表中毒".to_string())?
                .insert(
                    shell_id.clone(),
                    BackgroundShell {
                        handle,
                        log_path: log_path.clone(),
                        done,
                        exit_code,
                    },
                );
            return Ok(json!({
                "shell_id": shell_id,
                "background": true,
                "log_path": log_path.display().to_string(),
                "hint": "用 shell_output 查看输出，kill_shell 终止",
            }));
        }
        if let Some(path) = external_tools::path_with_bundled_tools() {
            sandbox_command.env.push(("PATH".to_string(), path));
        }

        let manager = crate::sandbox::default_manager();
        let process = {
            let mut manager = manager
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            manager
                .spawn(&sandbox_command)
                .map_err(|error| format!("沙箱拒绝执行（{}）：{error}", command))?
        };

        // 同步等待放在 blocking 线程；超时仅报错，进程仍在 Job 内受限（CPU/内存上限兜底）。
        let process_handle = process.handle.clone();
        let command_started = std::time::Instant::now();
        let mut wait_task = tokio::task::spawn_blocking(move || {
            let mut process = process;
            process.wait_output()
        });
        let output = match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            &mut wait_task,
        )
        .await
        {
            Ok(joined) => joined
                .map_err(|join_error| format!("命令等待失败：{join_error}"))?
                .map_err(|error| format!("沙箱执行失败：{error}"))?,
            Err(_) => {
                let kill_result = {
                    let manager = crate::sandbox::default_manager();
                    let mut manager = manager
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    manager.kill(&process_handle)
                };
                let _ = wait_task.await;
                return Err(match kill_result {
                    Ok(()) => format!(
                        "命令执行超过宿主任务预算 {timeout_ms}ms，沙箱进程已终止"
                    ),
                    Err(error) => format!(
                        "命令执行超过宿主任务预算 {timeout_ms}ms，终止沙箱进程失败：{error}"
                    ),
                });
            }
        };

        Ok(json!({
            "command": command,
            "exit_code": output.exit_code,
            "duration_ms": command_started.elapsed().as_millis() as u64,
            "stdout": decode_process_output(&output.stdout),
            "stderr": decode_process_output(&output.stderr),
        }))
    }
}

struct McpToolAdapter {
    full_name: String,
    server_name: String,
    tool_name: String,
    spec: ToolSpec,
    client: Arc<tokio::sync::Mutex<McpClient>>,
    /// §10：per-server 健康跟踪（None = 无隔离，保持旧行为）。
    health: Option<Arc<crate::mcp_health::McpHealthTracker>>,
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        // §10.2/§10.3：调用前熔断/限流检查——开路即快速失败，不触碰 server 进程。
        if let Some(health) = &self.health {
            health
                .check_call(&self.server_name)
                .map_err(|blocked| blocked.to_string())?;
        }
        let mut client = self.client.lock().await;
        // §10.3：幂等重试仅对只读工具（写/执行/未知 effect 一律 0 次）。
        let retries = self.health.as_ref().map_or(0, |health| {
            health.retries_for(self.spec.effect.as_ref().map(|effect| &effect.class))
        });
        let mut attempt = 0u32;
        loop {
            // §10 服务状态：逐次尝试计时，喂给健康快照 p50/p95。
            let attempt_started = std::time::Instant::now();
            let outcome = client.call_tool(&self.tool_name, args.clone()).await;
            let attempt_elapsed = attempt_started.elapsed();
            match outcome {
                Ok(value) => {
                    if let Some(health) = &self.health {
                        health.record_success_with(&self.server_name, Some(attempt_elapsed));
                    }
                    return Ok(value);
                }
                Err(error) => {
                    let opened = self.health.as_ref().is_some_and(|health| {
                        health.record_failure_with(&self.server_name, Some(attempt_elapsed), &error)
                    });
                    // §10.3：仅瞬态错误值得幂等重试；永久错误（工具缺失/参数
                    // 错误/权限）重试无意义，立即返回。
                    let transient = crate::mcp_health::classify_error(&error)
                        == crate::mcp_health::McpErrorClass::Transient;
                    if attempt < retries && !opened && transient {
                        attempt += 1;
                        // 重试前让出节流窗口（若启用限流）。
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                    return Err(format!(
                        "MCP 工具 {}:{} 失败：{error}",
                        self.server_name, self.tool_name
                    ));
                }
            }
        }
    }
}

impl std::fmt::Debug for McpToolAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpToolAdapter")
            .field("full_name", &self.full_name)
            .finish()
    }
}

/// A2-2：MCP 资源读取的泛化工具（`{server}_read_resource`）。
struct McpResourceAdapter {
    full_name: String,
    server_name: String,
    spec: ToolSpec,
    client: Arc<tokio::sync::Mutex<McpClient>>,
}

#[async_trait]
impl Tool for McpResourceAdapter {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let uri = required_string(&args, "uri")?;
        let mut client = self.client.lock().await;
        client
            .read_resource(&uri)
            .await
            .map_err(|error| format!("MCP 资源读取失败（{}:{}）：{error}", self.server_name, uri))
    }
}

impl std::fmt::Debug for McpResourceAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpResourceAdapter")
            .field("full_name", &self.full_name)
            .finish()
    }
}

/// A2-2：MCP 提示模板获取的泛化工具（`{server}_get_prompt`）。
struct McpPromptAdapter {
    full_name: String,
    server_name: String,
    spec: ToolSpec,
    client: Arc<tokio::sync::Mutex<McpClient>>,
}

impl std::fmt::Debug for McpPromptAdapter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpPromptAdapter")
            .field("full_name", &self.full_name)
            .finish()
    }
}

#[async_trait]
impl Tool for McpPromptAdapter {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let name = required_string(&args, "name")?;
        let arguments = args.get("arguments").cloned();
        let mut client = self.client.lock().await;
        client
            .get_prompt(&name, arguments)
            .await
            .map_err(|error| format!("MCP 模板获取失败（{}:{}）：{error}", self.server_name, name))
    }
}

struct ExploreTool;

#[async_trait]
impl Tool for ExploreTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explore".into(),
            description: "把调查任务交给只读探索子代理（只能读/搜文件），返回其调查汇报；多个独立问题用 queries 数组一次并行调查（比逐个问快数倍）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "单个调查问题（与 queries 二选一）" },
                    "queries": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "多个相互独立的调查问题（并行执行；最多 8 个）"
                    }
                }
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        // 单个 query 与 queries 数组都支持：数组用于"同时调查多个独立问题"，
        // 并行执行（原先只能串行逐个委派，长任务下反馈极慢）。
        let mut queries: Vec<String> = Vec::new();
        if let Some(query) = args.get("query").and_then(Value::as_str) {
            let trimmed = query.trim();
            if !trimmed.is_empty() {
                queries.push(trimmed.to_string());
            }
        }
        if let Some(list) = args.get("queries").and_then(Value::as_array) {
            for item in list {
                if let Some(text) = item.as_str() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        queries.push(trimmed.to_string());
                    }
                }
            }
        }
        if queries.is_empty() {
            return Err("参数缺少字符串字段：query 或 queries".to_string());
        }
        const MAX_PARALLEL_QUERIES: usize = 8;
        let truncated = queries.len() > MAX_PARALLEL_QUERIES;
        queries.truncate(MAX_PARALLEL_QUERIES);
        let runner = ctx.subagent.as_ref().ok_or("子代理运行时不可用")?;
        let workspace = ctx.workspace;
        let results = futures::future::join_all(
            queries
                .iter()
                .map(|query| async move { runner.run(workspace, query, true).await }),
        )
        .await;
        let mut items = Vec::with_capacity(queries.len());
        for (query, result) in queries.into_iter().zip(results) {
            match result {
                Ok(text) => items.push(json!({ "query": query, "ok": true, "text": text })),
                Err(error) => items.push(json!({ "query": query, "ok": false, "error": error })),
            }
        }
        Ok(json!({
            "mode": "explore",
            "parallel": true,
            "truncated": truncated,
            "results": items,
        }))
    }
}

struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "subagent".into(),
            description: "把独立任务委派给通用子代理（完整工具、仍需审批），返回其汇报。任务描述里要写明**验收标准**（做到什么算完成）与需要提交的**证据**（命令输出/文件路径/测试结果）；返回后会自动起只读复核子代理独立核对，未通过则按复核意见返工一次。".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "task": { "type": "string" } },
                "required": ["task"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("参数缺少字符串字段：task")?;
        let runner = ctx.subagent.as_ref().ok_or("子代理运行时不可用")?;
        let workspace = ctx.workspace;
        let text = runner.run(workspace, task, false).await?;
        // 质量门（Step 3）：父侧自动起只读 critic 独立复核；不通过则按复核意见
        // 返工一次（有界，不无限重试）。复核本身失败不阻断交付（best-effort）。
        if !crate::contract_worker::subagent_review_enabled() {
            return Ok(json!({ "mode": "general", "text": text }));
        }
        let review = match runner
            .run(
                workspace,
                &crate::contract_worker::review_prompt(task, &text),
                true,
            )
            .await
        {
            Ok(review) => review,
            Err(error) => {
                return Ok(json!({
                    "mode": "general",
                    "text": text,
                    "review": { "error": error },
                }))
            }
        };
        let approved = crate::contract_worker::critic_approved(&review);
        if approved == Some(false) {
            let rework = runner
                .run(
                    workspace,
                    &crate::contract_worker::rework_prompt(task, &text, &review),
                    false,
                )
                .await?;
            return Ok(json!({
                "mode": "general",
                "text": rework,
                "review": {
                    "approved": false,
                    "reworked": true,
                    "critic": review,
                },
            }));
        }
        Ok(json!({
            "mode": "general",
            "text": text,
            "review": { "approved": approved, "reworked": false, "critic": review },
        }))
    }
}

/// A5-1 取优合并自远端 engine：并行 fan-out 只读子代理（2~6 个独立调研任务同时跑）。
struct FanOutSubagentsTool;

#[async_trait]
impl Tool for FanOutSubagentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fan_out_subagents".into(),
            description: "并行派出 2~6 个只读探索子代理，同时调研多个**相互独立**的问题（多模块分别定位、多关键词并行检索、独立子问题调研），汇总各自结论。子代理只读（不改文件、不执行命令、不联网）；任务间不能有依赖（有依赖请串行 explore/subagent）。单任务失败不影响其余，结果按输入顺序返回。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "子任务列表（每条一个独立问题/检索目标，2~6 条）"
                    },
                    "max_parallel": { "type": "integer", "description": "并发上限（默认 3，最大 4）" },
                    "timeout_secs": { "type": "integer", "description": "单个子任务超时秒数（默认 300，范围 30~900）" }
                },
                "required": ["tasks"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let tasks: Vec<String> = args
            .get("tasks")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if tasks.len() < 2 {
            return Err("tasks 至少 2 条（单个任务请直接用 explore/subagent）".to_string());
        }
        if tasks.len() > 6 {
            return Err(format!(
                "tasks 过多（{} 条 > 6）。请拆成两批分别 fan-out",
                tasks.len()
            ));
        }
        let max_parallel = args
            .get("max_parallel")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 4) as usize;
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(30, 900);
        let fanout = ctx
            .fanout
            .clone()
            .ok_or("当前环境不支持并行子代理（子代理内/CLI/评测环境不可用）")?;
        let abort = ctx.abort;
        // 取消桥：主回合 abort（用户急停/流断开）→ fan-out 取消标志 → 子代理 abort。
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let config = crate::fleet::FanOutConfig {
            max_parallel,
            budget: crate::fleet::Budget {
                max_duration_secs: timeout_secs.saturating_mul(2).max(120),
                ..Default::default()
            },
            per_worker_timeout: Some(std::time::Duration::from_secs(timeout_secs)),
            cancelled: Some(std::sync::Arc::clone(&cancelled)),
            ..Default::default()
        };
        let future = crate::subagent::fan_out_subagents(
            fanout.provider,
            fanout.workspace,
            fanout.model,
            fanout.depth,
            fanout.max_turns,
            tasks.clone(),
            config,
        );
        tokio::pin!(future);
        let report = loop {
            tokio::select! {
                result = &mut future => break result?,
                _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {
                    if let Some(flag) = abort {
                        if flag.load(std::sync::atomic::Ordering::SeqCst) {
                            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }
            }
        };
        let succeeded = report.succeeded().len();
        let failed = report.failed().len();
        let results: Vec<Value> = report
            .outcomes
            .iter()
            .enumerate()
            .map(|(index, outcome)| {
                json!({
                    "index": index + 1,
                    "task": tasks.get(index).cloned().unwrap_or_default(),
                    "ok": outcome.ok,
                    "status": outcome.status,
                    "output": outcome.output,
                    "error": outcome.error,
                })
            })
            .collect();
        Ok(json!({
            "succeeded": succeeded,
            "failed": failed,
            "results": results,
        }))
    }
}

/// 向用户提问并等待回答（信息不足/需求含糊/关键分歧时使用；取优合并自远端 engine）。
/// 回合会挂起直到用户答复或超时；无 UI 通道时明确报错，让模型改为书面提问。
struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "信息不足、需求含糊或存在会显著影响结果的关键分歧时，向用户提问并等待回答（回合暂停直到用户答复）。问题要具体、一次只问最关键的一两个点；能用选项固定答案时给出 options。已经明确的常规操作不要用它确认。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "要向用户提出的问题（简洁明确，一次只问一件事）" },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "可选：2-4 个备选答案，用户可直接点选"
                    }
                },
                "required": ["question"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let question = args
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or("参数缺少字符串字段：question")?
            .to_string();
        let options: Vec<String> = args
            .get("options")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .take(4)
                    .collect()
            })
            .unwrap_or_default();
        let Some(questioner) = ctx.questioner else {
            return Err("当前运行环境没有可用的用户问答通道（无 UI 连接）。请把你的问题直接写进最终回复向用户提出，并给出你建议的默认方案。".to_string());
        };
        let request = crate::question::UserQuestion {
            question_id: uuid::Uuid::new_v4().to_string(),
            question,
            options,
        };
        match questioner.ask(&request).await {
            Some(answer) if !answer.answer.trim().is_empty() => Ok(json!({
                "answered": true,
                "answer": answer.answer,
            })),
            // 超时/中止/空回答：不给模型「卡住」的机会——明确告知并允许继续。
            _ => Ok(json!({
                "answered": false,
                "note": "用户未在时限内回答。请基于已有信息继续执行，并在最终回复里把不确定的部分标注出来。",
            })),
        }
    }
}

struct UseSkillTool;

#[async_trait]
impl Tool for UseSkillTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "use_skill".into(),
            description: "读取已加载技能（SKILL.md）的完整指令并按其流程执行；名称可通过 /skills 或技能清单查看".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "task": { "type": "string" }
                },
                "required": ["name"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or("参数缺少字符串字段：name")?;
        let Some(skill) = ctx.skills.get_enabled(name) else {
            let available = ctx
                .skills
                .list_enabled()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "未找到技能或技能已禁用：{name}；可用技能：{available}"
            ));
        };
        let task = args.get("task").and_then(Value::as_str).unwrap_or_default();
        Ok(json!({
            "skill": skill.name,
            "description": skill.description,
            "task": task,
            "instructions": skill.instructions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_single_plan(validator_id: &str, arguments: Value) -> crate::plan::VerificationPlanV1 {
        crate::plan::VerificationPlanV1 {
            plan_id: "single-plan".to_string(),
            requirements: vec![crate::plan::VerificationRequirementV1 {
                requirement_id: "req-user-visible".to_string(),
                covers_requirement_ids: vec!["user-request:req-user-visible".to_string()],
                validator_id: validator_id.to_string(),
                validator_version: Some("1".to_string()),
                scope: crate::plan::VerificationScopeV1::WorkspacePaths {
                    relative_paths: vec!["src/lib.rs".to_string()],
                },
                arguments,
                required: true,
                resources: crate::plan::VerificationResourcesV1 {
                    cpu_slots: 1,
                    memory_mb: 16,
                    exclusive_workspace: false,
                    timeout_ms: 10_000,
                },
            }],
        }
    }

    #[test]
    fn single_verification_plan_accepts_only_registered_scoped_checks() {
        let command_plan = sample_single_plan(
            "workspace-command-success-v1",
            json!({"command":"cargo test -p owo-agent-core"}),
        );
        assert!(validate_single_verification_plan(&command_plan).is_ok());

        let mut unmapped = command_plan.clone();
        unmapped.requirements[0].covers_requirement_ids.clear();
        assert!(validate_single_verification_plan(&unmapped).is_err());

        let unsupported = sample_single_plan("custom-shell-validator", json!({}));
        assert!(validate_single_verification_plan(&unsupported).is_err());

        let bypass = sample_single_plan(
            "workspace-command-success-v1",
            json!({"command":"cargo test -p owo-agent-core --no-run"}),
        );
        assert!(validate_single_verification_plan(&bypass).is_err());
    }

    /// 回归：UTF-8 输出不得被二次误解，OEM 代码页（GBK）输出不得变成替换字符。
    #[test]
    fn decode_process_output_prefers_utf8_then_oem_codepage() {
        // 现代工具（git / rg / python）输出 UTF-8：原样保留。
        assert_eq!(decode_process_output("中文输出".as_bytes()), "中文输出");
        assert_eq!(decode_process_output(b"plain ascii\n"), "plain ascii\n");
        assert_eq!(decode_process_output(b""), "");
        // 「中文」的 GBK 字节（D6D0 CEC4）：严格 UTF-8 解不了，须按系统代码页还原。
        #[cfg(windows)]
        unsafe {
            // GBK 固定字节只适用于 OEM 936；部分 Windows 已把 OEM 页设成 UTF-8。
            // 在这类机器上，此测试仍覆盖 UTF-8 优先分支，不把环境配置差异判成缺陷。
            if windows::Win32::Globalization::GetOEMCP() == 936 {
                assert_eq!(
                    decode_process_output(&[0xD6, 0xD0, 0xCE, 0xC4]),
                    "中文",
                    "GBK 字节未按系统代码页解码（中文命令输出会乱码）"
                );
            }
        }
    }

    #[tokio::test]
    async fn search_files_uses_bundled_ripgrep_with_workspace_scope() {
        let workspace =
            std::env::temp_dir().join(format!("owo-search-ripgrep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(workspace.join("nested")).unwrap();
        std::fs::write(workspace.join("nested").join("AlphaMarker.TXT"), b"ok").unwrap();

        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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

        let result = SearchFilesTool
            .run(&mut context, json!({ "pattern": "alphamarker" }))
            .await
            .unwrap();
        assert_eq!(result["tool"], "ripgrep");
        assert_eq!(result["tool_version"], external_tools::RIPGREP_VERSION);
        assert_eq!(result["matches"][0], "nested/AlphaMarker.TXT");
        drop(context);
        let _ = std::fs::remove_dir_all(workspace);
    }

    /// 回归：文件类工具的越界边界必须是**本会话的工作区**，而不是服务启动时的全局工作区。
    ///
    /// 线上事故：桌面壳在 `workspace.json` 未配置时回落 `process.cwd()`（= electron
    /// 安装目录）来启动核心，用户在会话里选 `D:\OwO-master`，于是 `list_dir` /
    /// `read_file` / `search_files` 全部报"路径越界"；`run_command` 不受影响（命令
    /// 工具不走这条判定），所以整件事看起来像"Agent 只坏了一半"。
    #[tokio::test]
    async fn session_workspace_is_the_boundary_when_it_differs_from_policy_workspace() {
        let root = std::env::temp_dir().join(format!("owo-scope-{}", uuid::Uuid::new_v4()));
        let session_ws = root.join("chosen-by-user");
        let service_ws = root.join("service-launch-cwd");
        std::fs::create_dir_all(session_ws.join("nested")).unwrap();
        std::fs::create_dir_all(&service_ws).unwrap();
        std::fs::write(session_ws.join("nested").join("note.txt"), b"hello").unwrap();

        // Policy 用服务级工作区（模拟 `--workspace` 是壳的 cwd，与会话不同的目录）。
        let policy = crate::Policy::new(&service_ws);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&session_ws, "mock", None);
        let context = ToolContext {
            workspace: &session_ws,
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

        // 1) 会话工作区自身（"."）不得越界——事故现场就是这里恒失败。
        assert!(
            resolve_session_path(&context, ".").is_ok(),
            "会话工作区内的 '.' 被误判越界"
        );
        // 2) 会话工作区内的相对路径可用。
        assert!(resolve_session_path(&context, "nested/note.txt").is_ok());
        // 3) 真正跳出会话工作区的路径仍须拒绝（安全口径不放松）。
        let escaped = resolve_session_path(&context, "../service-launch-cwd");
        if context.policy.profile() != crate::permissions::PermissionProfile::Unrestricted {
            assert!(escaped.is_err(), "跳出会话工作区的路径必须仍被拒绝");
        }

        drop(context);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn repeated_agent_write_refuses_to_overwrite_external_edit() {
        let workspace =
            std::env::temp_dir().join(format!("owo-write-conflict-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("shared.txt");
        std::fs::write(&path, "original").unwrap();
        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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

        write_file_body(&mut context, "shared.txt", &path, "agent version")
            .await
            .unwrap();
        std::fs::write(&path, "user edit").unwrap();
        let error = write_file_body(&mut context, "shared.txt", &path, "agent overwrite")
            .await
            .expect_err("外部编辑必须阻止后续 Agent 写入");

        assert!(error.contains("写入冲突"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edit");
        assert_eq!(
            context.session.snapshots[&snapshot_key(&path)]
                .expected_after_sha256
                .as_deref(),
            Some(crate::CasStore::hash_of(b"agent version").as_str()),
            "被拒绝的写入不得推进快照中的 Agent 版本"
        );
        drop(context);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn read_file_returns_content_hash_and_apply_patch_checks_base_receipt() {
        let workspace =
            std::env::temp_dir().join(format!("owo-patch-receipt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("shared.txt");
        std::fs::write(&path, "old\n").unwrap();
        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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

        let before_hash = format!("sha256:{}", crate::CasStore::hash_of(b"old\n"));
        let read = ReadFileTool
            .run(&mut context, json!({ "path": "shared.txt" }))
            .await
            .unwrap();
        assert_eq!(read["sha256"], before_hash);

        let patch = "*** Begin Patch\n*** Update File: shared.txt\n@@\n-old\n+new\n*** End Patch";
        let stale = ApplyPatchTool
            .run(
                &mut context,
                json!({ "patch": patch, "expected_hashes": { "shared.txt": "sha256:stale" } }),
            )
            .await
            .expect_err("旧基线必须被拒绝");
        assert!(stale.contains("补丁基线冲突"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");

        let applied = ApplyPatchTool
            .run(
                &mut context,
                json!({ "patch": patch, "expected_hashes": { "shared.txt": before_hash } }),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(applied["files"][0]["base_sha256"], before_hash);
        assert_eq!(
            applied["files"][0]["result_sha256"],
            format!("sha256:{}", crate::CasStore::hash_of(b"new\n"))
        );

        let add = ApplyPatchTool
            .run(
                &mut context,
                json!({
                    "patch": "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** End Patch",
                    "expected_hashes": { "created.txt": "absent" }
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(add["files"][0]["base_sha256"], Value::Null);
        let created_hash = format!("sha256:{}", crate::CasStore::hash_of(b"hello\n"));
        assert_eq!(add["files"][0]["result_sha256"], created_hash);

        let delete = ApplyPatchTool
            .run(
                &mut context,
                json!({
                    "patch": "*** Begin Patch\n*** Delete File: created.txt\n*** End Patch",
                    "expected_hashes": { "created.txt": created_hash }
                }),
            )
            .await
            .unwrap();
        assert!(!workspace.join("created.txt").exists());
        assert_eq!(delete["files"][0]["base_sha256"], created_hash);
        assert_eq!(delete["files"][0]["result_sha256"], Value::Null);
        drop(context);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn whitelist_apply_patch_rejects_targets_outside_allowed_paths() {
        let workspace =
            std::env::temp_dir().join(format!("owo-patch-whitelist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(workspace.join("allowed")).unwrap();
        let outside = workspace.join("outside.txt");
        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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
        let tool = WhitelistApplyPatchTool {
            allowed: vec![workspace.join("allowed")],
        };
        let error = tool
            .run(
                &mut context,
                json!({ "patch": "*** Begin Patch\n*** Add File: outside.txt\n+denied\n*** End Patch" }),
            )
            .await
            .expect_err("白名单外补丁必须拒绝");
        assert!(error.contains("写入目标不在写白名单内"));
        assert!(!outside.exists());
        drop(context);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn apply_patch_rejects_duplicate_file_operations_before_writing() {
        let workspace =
            std::env::temp_dir().join(format!("owo-patch-duplicate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("shared.txt");
        std::fs::write(&path, "old\n").unwrap();
        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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
        let patch = "*** Begin Patch\n*** Update File: shared.txt\n@@\n-old\n+new\n*** Update File: shared.txt\n@@\n-old\n+later\n*** End Patch";
        let error = ApplyPatchTool
            .run(&mut context, json!({ "patch": patch }))
            .await
            .expect_err("同一文件重复操作应拒绝");
        assert!(error.contains("重复操作"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        drop(context);
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn sanitizes_tool_names_for_model_api() {
        assert_eq!(
            sanitize_tool_name("owo.plugin.example-hello"),
            "owo_plugin_example-hello"
        );
        assert_eq!(sanitize_tool_name("echo"), "echo");
        assert_eq!(sanitize_tool_name("a b/c"), "a_b_c");
    }

    #[test]
    fn mcp_tool_prefix_sanitizes_plugin_id() {
        assert_eq!(
            mcp_tool_prefix("owo.plugin.translate"),
            "owo_plugin_translate_"
        );
        assert_eq!(
            mcp_tool_prefix("owo-plugin-clipboard"),
            "owo-plugin-clipboard_"
        );
    }

    struct NamedTool {
        name: String,
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                effect: None,
            }
        }

        async fn run(
            &self,
            _ctx: &mut ToolContext<'_>,
            _args: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
    }

    #[tokio::test]
    async fn tool_host_requires_approval_and_emits_receipt() {
        let workspace =
            std::env::temp_dir().join(format!("owo-tool-host-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let registry = Arc::new(RwLock::new(ToolRegistry::empty()));
        registry.write().unwrap().register(NamedTool {
            name: "contract_probe".to_string(),
        });
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let host = ToolHostService::new(Arc::clone(&registry), Arc::clone(&audit));
        let denied_request = PermissionRequest::new(
            "contract_probe",
            json!({"value": 1}),
            crate::permissions::Level::Write,
            "test denial",
        );
        assert!(ToolApprovalGrant::from_decision(&denied_request, Decision::Deny).is_err());
        let approval_for = |args: Value| {
            ToolApprovalGrant::from_decision(
                &PermissionRequest::new(
                    "contract_probe",
                    args,
                    crate::permissions::Level::Write,
                    "test approval",
                ),
                Decision::Allow,
            )
            .unwrap()
        };

        let policy = crate::Policy::new(&workspace);
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let session_id = session.id.clone();
        let capability_context =
            ToolCapabilityContext::for_workspace(&workspace, session.id.clone(), "contract-turn");
        let mut context = ToolContext {
            workspace: &workspace,
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
        let capability = host
            .issue(
                "contract_probe",
                json!({"value": 1}),
                approval_for(json!({"value": 1})),
                capability_context,
            )
            .unwrap();
        assert!(!capability.nonce.is_empty());
        assert!(capability.expires_at_unix > 0);
        assert_eq!(
            host.execute(capability, &mut context).await.unwrap(),
            Value::Null
        );
        let wrong_session = host
            .issue(
                "contract_probe",
                json!({"value": 3}),
                approval_for(json!({"value": 3})),
                ToolCapabilityContext::for_workspace(
                    &workspace,
                    "different-session",
                    "contract-turn-3",
                ),
            )
            .unwrap();
        let error = host.execute(wrong_session, &mut context).await.unwrap_err();
        assert!(error.contains("capability session mismatch"));
        let mut stale_schema = host
            .issue(
                "contract_probe",
                json!({"value": 4}),
                approval_for(json!({"value": 4})),
                ToolCapabilityContext::for_workspace(
                    &workspace,
                    session_id.clone(),
                    "contract-turn-4",
                ),
            )
            .unwrap();
        stale_schema.tool_version = "stale-tool-version".to_string();
        let error = host.execute(stale_schema, &mut context).await.unwrap_err();
        assert!(error.contains("capability tool version mismatch"));
        let mut expired = host
            .issue(
                "contract_probe",
                json!({"value": 2}),
                approval_for(json!({"value": 2})),
                ToolCapabilityContext::for_workspace(&workspace, session_id, "contract-turn-2"),
            )
            .unwrap();
        expired.expires_at_unix = 0;
        let error = host
            .execute(expired, &mut context)
            .await
            .expect_err("过期 capability 不得执行工具");
        assert!(error.contains("approval expired"));
        drop(context);

        let entries = audit.lock().unwrap().entries.clone();
        let receipt = entries
            .iter()
            .find(|entry| entry.event == "tool_receipt")
            .expect("受信执行必须产生 receipt");
        assert_eq!(receipt.tool.as_deref(), Some("contract_probe"));
        assert_eq!(receipt.approved, Some(true));
        assert!(receipt.detail.contains("args_sha256"));
        assert!(receipt.detail.contains("expires_at_unix"));
        assert!(receipt.detail.contains("nonce"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn write_toolhost_emits_execution_receipt_and_scoped_revert() {
        let workspace = std::env::temp_dir().join(format!(
            "owo-tool-host-write-receipt-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let registry = Arc::new(RwLock::new(ToolRegistry::empty()));
        registry.write().unwrap().register_write_file();
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let host = ToolHostService::new(Arc::clone(&registry), Arc::clone(&audit));
        let policy = crate::Policy::new(&workspace);
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let session_id = session.id.clone();
        let args = json!({"path": "receipt.txt", "content": "agent content"});
        let request = PermissionRequest::new(
            "write_file",
            args.clone(),
            crate::permissions::Level::Write,
            "test write",
        );
        let approval = ToolApprovalGrant::from_decision(&request, Decision::Allow).unwrap();
        let context = ToolCapabilityContext::for_workspace(&workspace, session_id, "turn-write");
        let mut tool_context = ToolContext {
            workspace: &workspace,
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
        let capability = host.issue("write_file", args, approval, context).unwrap();
        let result = host.execute(capability, &mut tool_context).await.unwrap();
        let receipt_id = result
            .get("execution_receipt_id")
            .and_then(Value::as_str)
            .expect("写入结果必须返回执行收据 ID")
            .to_string();
        assert_eq!(tool_context.session.execution_receipts.len(), 1);
        assert_eq!(
            tool_context.session.execution_receipts[0].receipt_id,
            receipt_id
        );
        drop(tool_context);
        let restored = session.revert_receipt(Some(&receipt_id)).await.unwrap();
        assert_eq!(restored, vec!["receipt.txt"]);
        assert!(!workspace.join("receipt.txt").exists());
        let audit_guard = audit.lock().unwrap();
        let receipt = audit_guard
            .entries
            .iter()
            .find(|entry| entry.event == "tool_receipt")
            .expect("写入必须产生 tool receipt");
        assert!(receipt.detail.contains("execution_receipt_id"));
        assert!(receipt.detail.contains("diff_sha256"));
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// multi_edit（远端 engine 取优）：按顺序应用多处替换，第 2 处的 old_string
    /// 匹配第 1 处替换后的内容。
    #[tokio::test]
    async fn multi_edit_applies_all_edits_in_order() {
        let workspace =
            std::env::temp_dir().join(format!("owo-multi-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let original = "fn main() {\n    let version = \"1.0\";\n    println!(\"v1.0\");\n}\n";
        std::fs::write(workspace.join("app.rs"), original).unwrap();

        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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

        let result = MultiEditTool
            .run(
                &mut context,
                json!({
                    "path": "app.rs",
                    "edits": [
                        { "old_string": "let version = \"1.0\";", "new_string": "let version = \"2.0\";" },
                        { "old_string": "println!(\"v1.0\");", "new_string": "println!(\"v{version}\");" }
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["applied"], 2);
        let updated = std::fs::read_to_string(workspace.join("app.rs")).unwrap();
        assert!(updated.contains("let version = \"2.0\";"), "{updated}");
        assert!(updated.contains("println!(\"v{version}\");"), "{updated}");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// multi_edit 原子性：任一处失败（未命中/歧义）整批不落盘。
    #[tokio::test]
    async fn multi_edit_is_atomic_on_failure() {
        let workspace =
            std::env::temp_dir().join(format!("owo-multi-edit-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let original = "alpha\nbeta\ngamma\n";
        std::fs::write(workspace.join("data.txt"), original).unwrap();

        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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

        // 第 2 处失败（old_string 不存在）：整批不落盘。
        let error = MultiEditTool
            .run(
                &mut context,
                json!({
                    "path": "data.txt",
                    "edits": [
                        { "old_string": "alpha", "new_string": "ALPHA" },
                        { "old_string": "不存在的片段", "new_string": "x" }
                    ]
                }),
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("2/2") && error.contains("整批未应用"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("data.txt")).unwrap(),
            original,
            "失败时原文件必须保持不变"
        );

        // 多处歧义（未设 replace_all）同样整体失败。
        std::fs::write(workspace.join("data.txt"), "same\nsame\n").unwrap();
        let error = MultiEditTool
            .run(
                &mut context,
                json!({
                    "path": "data.txt",
                    "edits": [{ "old_string": "same", "new_string": "x" }]
                }),
            )
            .await
            .unwrap_err();
        assert!(error.contains("出现 2 次"), "{error}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("data.txt")).unwrap(),
            "same\nsame\n"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// ask_user（取优合并自远端 engine）：无 UI 通道时明确报错，模型改为书面提问。
    #[tokio::test]
    async fn ask_user_without_channel_reports_error() {
        let workspace =
            std::env::temp_dir().join(format!("owo-ask-user-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let mut context = ToolContext {
            workspace: &workspace,
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
        let error = AskUserTool
            .run(&mut context, json!({ "question": "先做 A 还是 B？" }))
            .await
            .unwrap_err();
        assert!(error.contains("没有可用的用户问答通道"), "{error}");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// 回显提问通道：直接返回答案，模拟用户在提问卡上作答。
    struct EchoQuestioner;

    #[async_trait]
    impl crate::question::Questioner for EchoQuestioner {
        async fn ask(
            &self,
            question: &crate::question::UserQuestion,
        ) -> Option<crate::question::QuestionAnswer> {
            Some(crate::question::QuestionAnswer {
                question_id: question.question_id.clone(),
                answer: format!("已收到：{}", question.question),
            })
        }
    }

    /// ask_user：有通道时提问经 Questioner 拿回用户答案（answered=true）。
    #[tokio::test]
    async fn ask_user_returns_answer_through_channel() {
        let workspace =
            std::env::temp_dir().join(format!("owo-ask-user-ok-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let policy = crate::Policy::new(&workspace);
        let audit = Arc::new(Mutex::new(crate::AuditLog::default()));
        let skills = crate::SkillRegistry::default();
        let elements = Arc::new(Mutex::new(crate::ElementRegistry::new()));
        let mut session = Session::new(&workspace, "mock", None);
        let questioner = EchoQuestioner;
        let result = {
            let mut context = ToolContext {
                workspace: &workspace,
                policy: &policy,
                session: &mut session,
                audit: &audit,
                subagent: None,
                skills: &skills,
                elements: &elements,
                fanout: None,
                abort: None,
                questioner: Some(&questioner),
            };
            AskUserTool
                .run(
                    &mut context,
                    json!({ "question": "先做 A 还是 B？", "options": ["A", "B"] }),
                )
                .await
                .unwrap()
        };
        assert_eq!(result["answered"], true);
        assert_eq!(result["answer"], "已收到：先做 A 还是 B？");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn remove_prefix_unregisters_only_matching_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(NamedTool {
            name: "owo_plugin_demo_translate".to_string(),
        });
        registry.register(NamedTool {
            name: "owo_plugin_demo_clipboard".to_string(),
        });
        registry.register(NamedTool {
            name: "builtin_tool".to_string(),
        });
        let removed = registry.remove_prefix("owo_plugin_demo_");
        assert_eq!(removed, 2);
        let names: Vec<String> = registry
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert!(!names
            .iter()
            .any(|name| name.starts_with("owo_plugin_demo_")));
        assert!(names.iter().any(|name| name == "builtin_tool"));
    }

    #[test]
    fn retain_names_keeps_only_listed_tools() {
        // 空名单 = 全部保留。
        let mut registry = ToolRegistry::new();
        assert_eq!(registry.retain_names(&[]), 0);
        // 非空名单 = 只留名单内工具（含 MCP 完整 schema 副本同步清理）。
        let mut registry = ToolRegistry::new();
        let before = registry.specs().len();
        let removed = registry.retain_names(&["read_file".to_string()]);
        assert_eq!(removed, before - 1);
        let names: Vec<String> = registry
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert_eq!(names, vec!["read_file".to_string()]);
    }

    #[test]
    fn grouped_role_constructors_assemble_expected_surface() {
        // 只读组：读三件套（与 read_only() 等价）。
        let mut registry = ToolRegistry::empty();
        registry.register_file_read_tools();
        let names: Vec<String> = registry
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                "read_file".to_string(),
                "list_dir".to_string(),
                "search_files".to_string()
            ]
        );
        // 浏览器组：含读变体与写工作区变体（写变体靠 retain_names 按角色裁剪）。
        let mut registry = ToolRegistry::empty();
        registry.register_browser_tools();
        let names: Vec<String> = registry
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        assert!(names.contains(&"browser_navigate".to_string()));
        assert!(names.contains(&"browser_screenshot".to_string()));
    }

    #[test]
    fn default_registry_is_minimal_and_optional_groups_are_explicit() {
        let mut registry = ToolRegistry::new();
        let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            vec![
                "read_file",
                "write_file",
                "edit_file",
                "multi_edit",
                "apply_patch",
                "list_dir",
                "search_files",
                "grep",
                "git_status",
                "git_diff",
                "git_log",
                "run_command",
                "shell_output",
                "kill_shell",
                "todo",
                "web_fetch",
                "web_search",
                "read_image",
                "explore",
                "subagent",
                "fan_out_subagents",
                "ask_user",
                "use_skill",
            ]
        );
        assert!(!names.iter().any(|name| name.starts_with("desktop_")));
        assert!(!names.iter().any(|name| name.starts_with("browser_")));
        assert!(!names.iter().any(|name| name == "screen_ocr"));

        registry.register_desktop_observation_tools();
        registry.register_desktop_control_tools();
        registry.register_browser_tools();
        let enabled: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
        for optional in ["screen_ocr", "desktop_click", "browser_navigate"] {
            assert!(enabled.iter().any(|name| name == optional));
        }
    }

    #[test]
    fn path_in_whitelist_prefix_match() {
        let base = PathBuf::from("T:/ws/src");
        // 四路集成微修：clippy 冗余 clone（单元素切片用 from_ref 借用即可）。
        assert!(path_in_whitelist(
            &PathBuf::from("T:/ws/src/lib/a.rs"),
            std::slice::from_ref(&base)
        ));
        assert!(!path_in_whitelist(
            &PathBuf::from("T:/ws/docs/b.md"),
            std::slice::from_ref(&base)
        ));
        // 空白名单在此判 false（无前缀可匹配）——「空 = 未约束」由
        // resolve_whitelisted 短路放行，纯函数只做前缀匹配。
        assert!(!path_in_whitelist(
            &PathBuf::from("T:/ws/anything.txt"),
            &[]
        ));
    }

    #[test]
    fn strip_verbatim_prefix_normalizes_windows_canonical_paths() {
        // 回归（七期二路冒烟发现）：canonicalize 产物带 `\\?\` 前缀，与绑定侧
        // simplify 后的 allowed 混用比对会恒 false。
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\ws\src\a.rs")),
            PathBuf::from(r"C:\ws\src\a.rs")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\ws\src\a.rs")),
            PathBuf::from(r"C:\ws\src\a.rs")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new("/home/ws/src/a.rs")),
            PathBuf::from("/home/ws/src/a.rs")
        );
    }

    #[test]
    fn patch_parser_supports_add_update_delete() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** Update File: b.txt\n@@ fn main\n-let x = 1;\n+let x = 2;\n*** Delete File: c.txt\n*** End Patch\n";
        let ops = parse_patch(patch).expect("patch should parse");
        assert_eq!(ops.len(), 3);
        match &ops[0] {
            PatchOp::Add { path, content } => {
                assert_eq!(path, "a.txt");
                assert_eq!(content, "hello\n");
            }
            _ => panic!("first op should be add"),
        }
        match &ops[1] {
            PatchOp::Update { path, hunks } => {
                assert_eq!(path, "b.txt");
                assert_eq!(hunks.len(), 1);
                // `@@` 行是段落标题提示，不计入上下文。
                assert_eq!(hunks[0].old_lines, vec!["let x = 1;"]);
                assert_eq!(hunks[0].new_lines, vec!["let x = 2;"]);
            }
            _ => panic!("second op should be update"),
        }
        assert!(matches!(&ops[2], PatchOp::Delete { path } if path == "c.txt"));
    }

    #[test]
    fn patch_apply_requires_unique_context() {
        let original = "alpha\nbeta\ngamma\n";
        let ok = apply_hunks(
            original,
            &[PatchHunk {
                old_lines: vec!["beta".to_string()],
                new_lines: vec!["BETA".to_string()],
            }],
        )
        .expect("unique context should apply");
        assert_eq!(ok, "alpha\nBETA\ngamma\n");

        let ambiguous = apply_hunks(
            "x\ny\nx\ny\n",
            &[PatchHunk {
                old_lines: vec!["x".to_string(), "y".to_string()],
                new_lines: vec!["z".to_string(), "y".to_string()],
            }],
        );
        assert!(ambiguous.is_err(), "重复上下文必须拒绝");

        let missing = apply_hunks(
            original,
            &[PatchHunk {
                old_lines: vec!["nope".to_string()],
                new_lines: vec!["yes".to_string()],
            }],
        );
        assert!(missing.is_err(), "未命中必须报错");
    }

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = "<html><head><style>body{color:red}</style></head><body><h1>Hello</h1><script>var x=1;</script><p>World&nbsp;!</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World !"), "{text}");
        assert!(!text.contains("var x"), "script 内容应剔除：{text}");
        assert!(!text.contains("color:red"), "style 内容应剔除：{text}");
    }

    #[test]
    fn search_href_decodes_ddg_redirect() {
        assert_eq!(
            decode_search_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&rut=1"),
            "https://example.com/a"
        );
        assert_eq!(
            decode_search_href("https://direct.example"),
            "https://direct.example"
        );
    }
}
