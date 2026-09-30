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
use crate::mcp::{McpClient, McpTool};
use crate::permissions::{Decision, PermissionRequest, Policy};
use crate::session::Session;
use crate::skill::SkillRegistry;
use crate::subagent::SubagentRunner;
use crate::tool_effects::EffectClass;
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
        }
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
            match tool {
                Some(tool) => tool.run(ctx, capability.args).await,
                None => Err(format!("未知工具：{}", capability.tool)),
            }
        };
        let execution_receipt = if outcome.is_ok() {
            if let Some(path) = write_path.as_deref() {
                match ctx
                    .session
                    .record_file_execution(&capability.tool, &capability.turn_id, path)
                {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        outcome = Err(format!("写入收据失败：{error}"));
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        if let (Ok(Value::Object(result)), Some(receipt)) = (&mut outcome, &execution_receipt) {
            result.insert(
                "execution_receipt_id".to_string(),
                Value::String(receipt.receipt_id.clone()),
            );
            result.insert(
                "changed_files".to_string(),
                Value::Array(
                    receipt
                        .changed_files
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
            result.insert(
                "diff_sha256".to_string(),
                Value::String(receipt.diff_sha256.clone()),
            );
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
            execution_receipt_id: execution_receipt
                .as_ref()
                .map(|receipt| receipt.receipt_id.clone()),
            changed_files: execution_receipt
                .as_ref()
                .map(|receipt| receipt.changed_files.clone())
                .unwrap_or_default(),
            diff_sha256: execution_receipt
                .as_ref()
                .map(|receipt| receipt.diff_sha256.clone()),
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
        registry.register(ApplyPatchTool);
        registry.register(ListDirTool);
        registry.register(SearchFilesTool);
        registry.register(GrepTool);
        registry.register_run_command();
        registry.register(ShellOutputTool);
        registry.register(KillShellTool);
        registry.register(TodoTool);
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
    let policy_workspace = ctx
        .policy
        .workspace()
        .canonicalize()
        .unwrap_or_else(|_| ctx.policy.workspace().to_path_buf());
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
        let raw = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败：{e}"))?;
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

/// `apply_patch`：多文件原子补丁（Add/Update/Delete），写入走快照可 diff/revert。
struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "apply_patch".into(),
            description:
                "应用多文件补丁（*** Add/Update/Delete File: + @@ 段落；写入记录快照，可 diff/revert）"
                    .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "patch": { "type": "string" } },
                "required": ["patch"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let patch = required_string(&args, "patch")?;
        let ops = parse_patch(&patch)?;
        // 先全部解析/校验/计算，再落盘：任一文件失败即整体失败，不留半成品。
        let mut prepared: Vec<(String, PathBuf, String)> = Vec::with_capacity(ops.len());
        for op in &ops {
            match op {
                PatchOp::Add { path, content } => {
                    let abs = resolve_session_path(ctx, path)?;
                    if abs.exists() {
                        return Err(format!("Add File 目标已存在：{path}"));
                    }
                    prepared.push((path.clone(), abs, content.clone()));
                }
                PatchOp::Delete { path } => {
                    let abs = resolve_session_path(ctx, path)?;
                    let original = tokio::fs::read_to_string(&abs)
                        .await
                        .map_err(|e| format!("Delete File 读取 {path} 失败：{e}"))?;
                    prepared.push((path.clone(), abs, original));
                }
                PatchOp::Update { path, hunks } => {
                    let abs = resolve_session_path(ctx, path)?;
                    let original = tokio::fs::read_to_string(&abs)
                        .await
                        .map_err(|e| format!("Update File 读取 {path} 失败：{e}"))?;
                    let updated = apply_hunks(&original, hunks)?;
                    prepared.push((path.clone(), abs, updated));
                }
            }
        }
        let mut applied = Vec::with_capacity(ops.len());
        for (op, (path, abs, content)) in ops.iter().zip(prepared) {
            match op {
                PatchOp::Delete { .. } => {
                    // 删除也落快照（original_b64），revert 可恢复。
                    let key = snapshot_key(&abs);
                    ctx.session.snapshots.entry(key).or_insert_with(|| {
                        crate::session::SnapshotEntry {
                            original_b64: Some(BASE64.encode(content.as_bytes())),
                            expected_after_sha256: None,
                        }
                    });
                    tokio::fs::remove_file(&abs)
                        .await
                        .map_err(|e| format!("删除 {path} 失败：{e}"))?;
                    applied.push(json!({ "path": path, "op": "delete" }));
                }
                PatchOp::Add { .. } | PatchOp::Update { .. } => {
                    write_file_body(ctx, &path, &abs, &content).await?;
                    let kind = if matches!(op, PatchOp::Add { .. }) {
                        "add"
                    } else {
                        "update"
                    };
                    applied.push(json!({ "path": path, "op": kind }));
                }
            }
        }
        Ok(json!({ "ok": true, "files": applied }))
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

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("OwO-Agent/1.0 (+web)")
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
        let raw = String::from_utf8_lossy(&bytes[..bytes.len().min(max_bytes)]).to_string();
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
            description: "网页搜索（默认 DuckDuckGo HTML；可用 OWO_WEB_SEARCH_URL 覆盖端点）"
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
        let endpoint = std::env::var("OWO_WEB_SEARCH_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "https://html.duckduckgo.com/html/".to_string());
        let client = http_client()?;
        let response = client
            .get(&endpoint)
            .query(&[("q", query.as_str())])
            .send()
            .await
            .map_err(|error| format!("搜索请求失败：{error}"))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|error| format!("读取搜索结果失败：{error}"))?;
        let results = parse_search_results(&body);
        Ok(json!({
            "query": query,
            "engine": endpoint,
            "status": status,
            "count": results.len(),
            "results": results,
        }))
    }
}

/// 解析 DuckDuckGo HTML 结果（`class="result__a"`）；最多 10 条。
fn parse_search_results(html: &str) -> Vec<Value> {
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
            "output": String::from_utf8_lossy(slice),
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
            description: "使用随包 ripgrep 按文件名关键字递归搜索工作区文件（只读）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" } },
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
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let matches = String::from_utf8_lossy(&output.stdout)
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
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        // `--json` 输出逐行解析，避免 Windows 盘符冒号破坏 `path:line:text` 切分。
        let mut matches: Vec<Value> = String::from_utf8_lossy(&output.stdout)
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
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::task::spawn_blocking(move || {
                let mut process = process;
                process.wait_output()
            }),
        )
        .await
        .map_err(|_| "命令执行超时（60s，进程仍在受限 Job 内，将被资源上限终止）".to_string())?
        .map_err(|join_error| format!("命令等待失败：{join_error}"))?
        .map_err(|error| format!("沙箱执行失败：{error}"))?;

        Ok(json!({
            "command": command,
            "exit_code": output.exit_code,
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
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
                "apply_patch",
                "list_dir",
                "search_files",
                "grep",
                "run_command",
                "shell_output",
                "kill_shell",
                "todo",
                "web_fetch",
                "web_search",
                "read_image",
                "explore",
                "subagent",
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
