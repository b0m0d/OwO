use super::{McpClient, Tool, ToolContext, ToolSpec};
use async_trait::async_trait;
use owo_agent_kernel::required_string;
use serde_json::Value;
use std::sync::Arc;

pub(super) struct McpToolAdapter {
    pub(super) full_name: String,
    pub(super) server_name: String,
    pub(super) tool_name: String,
    pub(super) spec: ToolSpec,
    pub(super) client: Arc<tokio::sync::Mutex<McpClient>>,
    /// §10：per-server 健康跟踪（None = 无隔离，保持旧行为）。
    pub(super) health: Option<Arc<crate::mcp_health::McpHealthTracker>>,
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
pub(super) struct McpResourceAdapter {
    pub(super) full_name: String,
    pub(super) server_name: String,
    pub(super) spec: ToolSpec,
    pub(super) client: Arc<tokio::sync::Mutex<McpClient>>,
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
pub(super) struct McpPromptAdapter {
    pub(super) full_name: String,
    pub(super) server_name: String,
    pub(super) spec: ToolSpec,
    pub(super) client: Arc<tokio::sync::Mutex<McpClient>>,
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
