//! 诊断域：健康检查、服务状态、build identity。

use crate::error::Result;
use crate::http::AgentClient;
use owo_agent_protocol::HealthResponse;
use serde_json::Value;

impl AgentClient {
    /// `/health`（公开端点；不要求 token）。
    pub async fn health(&self) -> Result<HealthResponse> {
        self.get_json("/health").await
    }

    /// `/server/status`（受保护）。
    pub async fn server_status(&self) -> Result<Value> {
        self.get_json("/server/status").await
    }

    /// 请求优雅关闭（`daemon stop`；需二次确认，服务端校验 `confirm=true`）。
    pub async fn request_shutdown(&self, confirm: bool) -> Result<Value> {
        self.post_json(
            "/server/shutdown",
            &serde_json::json!({ "confirm": confirm }),
        )
        .await
    }
}
