//! 通用 JSON SSE 事件流，用于消费 Daemon 的非 turn 事件端点。

use crate::error::Result;
use crate::http::AgentClient;
use crate::sse::JsonEventStream;
use serde_json::Value;

impl AgentClient {
    /// 打开 JSON data 帧组成的 SSE 流。路径必须是本地 Daemon 的 API 相对路径。
    pub async fn open_event_stream(&self, path: &str) -> Result<JsonEventStream> {
        self.open_event_stream_after(path, None).await
    }

    /// Reopen a stream after the last standard SSE id received by this client.
    pub async fn open_event_stream_after(
        &self,
        path: &str,
        last_event_id: Option<&str>,
    ) -> Result<JsonEventStream> {
        let mut request = self.get_stream(path);
        if let Some(last_event_id) = last_event_id {
            request = request.header("Last-Event-ID", last_event_id);
        }
        let response = self.send_stream(request).await?;
        Ok(JsonEventStream::new(response))
    }
}

impl JsonEventStream {
    /// 读取下一条 JSON SSE data 事件；正常 EOF 返回 None。
    pub async fn next_event(&mut self) -> Option<Result<Value>> {
        self.next_value().await
    }
}
