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
        registry.register(ListDirTool);
        registry.register(SearchFilesTool);
        registry.register_run_command();
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
        .map_err(|error| format!("工作区不可访问：{error}"))?;
    let candidate = base.join(path);
    let candidate = candidate.canonicalize().unwrap_or(candidate);
    let policy_workspace = ctx
        .policy
        .workspace()
        .canonicalize()
        .unwrap_or_else(|_| ctx.policy.workspace().to_path_buf());
    if !candidate.starts_with(&policy_workspace) {
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
            description: "读取工作区内的文本文件内容".into(),
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
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败：{e}"))?;
        Ok(json!({
            "path": path,
            "content": content,
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

        let mut policy =
            crate::sandbox::SandboxPolicy::for_workspace("search_files", ctx.workspace);
        policy.require_isolation = crate::sandbox::IsolationLevel::JobOnly;
        policy.allow_degraded = true;
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
                    "cwd": { "type": "string" }
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
        let mut policy = crate::sandbox::SandboxPolicy::for_workspace("run_command", ctx.workspace);
        policy.require_isolation = crate::sandbox::IsolationLevel::JobOnly;
        policy.allow_degraded = true;
        policy.cpu_ms = Some(60_000);
        policy.mem_mb = Some(1024);
        // 命令文本（cmd /C <command> 的命令体）同样过 deny 检查。
        if let Some(fragment) =
            crate::sandbox::SandboxCommand::deny_hit(&command, &policy.deny_programs)
        {
            return Err(format!("命令命中危险黑名单片段：{fragment}"));
        }
        let mut sandbox_command = crate::sandbox::SandboxCommand::new("cmd", policy.clone())
            .with_args(vec!["/C".to_string(), command.to_string()])
            .with_cwd(cwd.clone());
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
            description: "把调查任务交给只读探索子代理（只能读/搜文件），返回其调查汇报".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or("参数缺少字符串字段：query")?;
        let runner = ctx.subagent.as_ref().ok_or("子代理运行时不可用")?;
        let text = runner.run(ctx.workspace, query, true).await?;
        Ok(json!({ "mode": "explore", "text": text }))
    }
}

struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "subagent".into(),
            description: "把独立任务委派给通用子代理（完整工具、仍需审批），返回其汇报".into(),
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
        let text = runner.run(ctx.workspace, task, false).await?;
        Ok(json!({ "mode": "general", "text": text }))
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
                "list_dir",
                "search_files",
                "run_command",
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
}
