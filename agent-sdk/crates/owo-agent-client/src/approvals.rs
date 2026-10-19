//! 审批域：回应 pending 权限请求（拒绝/允许一次/会话/工作区由 scope 决定）。

use crate::error::Result;
use crate::http::AgentClient;
use owo_agent_protocol::PermissionResponse;
use serde_json::Value;

impl AgentClient {
    /// 回应某会话下的审批请求。
    pub async fn respond_permission(
        &self,
        session_id: &str,
        request_id: &str,
        response: &PermissionResponse,
    ) -> Result<Value> {
        self.post_json(
            &format!("/session/{session_id}/permission/{request_id}"),
            response,
        )
        .await
    }
}
