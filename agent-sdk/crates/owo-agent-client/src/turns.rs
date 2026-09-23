//! turn 域：开启 SSE 流 + 取消。

use crate::error::Result;
use crate::http::AgentClient;
use crate::sse::TurnStream;
use owo_agent_protocol::TurnRequest;
use serde_json::Value;

impl AgentClient {
    /// 发起一轮对话并返回事件流（尚未读取任何 body；由调用方驱动）。
    pub async fn open_turn(&self, session_id: &str, prompt: &str) -> Result<TurnStream> {
        let request = TurnRequest {
            prompt: prompt.to_string(),
            attachments: Vec::new(),
        };
        let response = self
            .send_stream(self.post_stream(&format!("/session/{session_id}/turn"), &request))
            .await?;
        let turn_id = response
            .headers()
            .get("x-owo-turn-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        Ok(TurnStream::new(
            response,
            self.clone(),
            session_id.to_string(),
            turn_id,
        ))
    }

    /// 取消运行中的回合（幂等；无运行中回合时服务端仍返回 ok）。
    pub async fn cancel_turn(&self, session_id: &str) -> Result<Value> {
        self.post_empty(&format!("/session/{session_id}/abort"))
            .await
    }
}
