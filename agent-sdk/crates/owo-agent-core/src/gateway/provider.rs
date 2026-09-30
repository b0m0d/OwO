use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;

use super::config::*;
use super::is_local_endpoint;
use super::message::*;
use super::stream::*;
/// OpenAI-compatible `/chat/completions` 客户端（覆盖 OpenAI、DeepSeek、Ollama、多数代理）。
pub struct OpenAiCompatibleProvider {
    pub(super) client: reqwest::Client,
    pub(super) direct_client: Option<reqwest::Client>,
    config: OpenAiCompatibleConfig,
    usage: std::sync::Mutex<TokenUsage>,
}

impl OpenAiCompatibleProvider {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(180));
        let mut has_proxy = false;
        for name in [
            "OWO_HTTP_PROXY",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "https_proxy",
            "http_proxy",
        ] {
            if let Ok(proxy) = std::env::var(name) {
                if !proxy.trim().is_empty() {
                    let proxy = reqwest::Proxy::all(proxy)
                        .map_err(|e| format!("代理配置无效（{name}）：{e}"))?;
                    builder = builder.proxy(proxy);
                    has_proxy = true;
                    break;
                }
            }
        }
        let client = builder
            .build()
            .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
        let direct_client = if has_proxy {
            Some(
                reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .timeout(std::time::Duration::from_secs(120))
                    .build()
                    .map_err(|e| format!("直连 HTTP 客户端创建失败：{e}"))?,
            )
        } else {
            None
        };
        Ok(Self {
            client,
            direct_client,
            config,
            usage: std::sync::Mutex::new(TokenUsage::default()),
        })
    }

    fn record_usage(&self, usage: &Value) {
        let parsed = parse_usage_value(usage);
        if parsed.total_tokens == 0 && parsed.prompt_tokens == 0 && parsed.completion_tokens == 0 {
            return;
        }
        if let Ok(mut current) = self.usage.lock() {
            current.add(&parsed);
        }
    }

    /// 读取环境变量预算并检查当前累计用量是否超限。
    fn usage_budget_check(&self) -> Option<String> {
        let total_cap = std::env::var("OWO_USAGE_TOKEN_BUDGET")
            .ok()
            .and_then(|value| value.parse::<u64>().ok());
        let cost_cap = std::env::var("OWO_USAGE_COST_BUDGET_USD")
            .ok()
            .and_then(|value| value.parse::<f64>().ok());
        if total_cap.is_none() && cost_cap.is_none() {
            return None;
        }
        let input_price = std::env::var("OWO_MODEL_INPUT_PRICE_PER_MTOK")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
        let output_price = std::env::var("OWO_MODEL_OUTPUT_PRICE_PER_MTOK")
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0);
        let usage = self.usage.lock().map(|usage| *usage).unwrap_or_default();
        budget_violation(&usage, total_cap, cost_cap, input_price, output_price)
    }

    /// 发送请求：优先代理客户端，失败自动切直连重试一次（多轮流式挂起时稳定）。
    async fn post_chat(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, String> {
        let mut last_error = String::new();
        let attempts: Vec<(&str, &reqwest::Client)> = {
            let mut list = vec![("proxy", &self.client)];
            if let Some(direct) = &self.direct_client {
                list.push(("direct", direct));
            }
            list
        };
        for (label, client) in attempts {
            let request = client
                .post(url)
                .json(body)
                .timeout(std::time::Duration::from_secs(120));
            let request = if self.config.api_key.is_empty() {
                request
            } else {
                request.bearer_auth(&self.config.api_key)
            };
            // 联调诊断（RUST_LOG=owo_gateway=debug 可见）：请求画像不落任何凭据。
            tracing::debug!(
                target: "owo_gateway",
                url = %url,
                model = %body
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                tools = body
                    .get("tools")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0),
                stream = body
                    .get("stream")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                channel = label,
                "模型网关请求"
            );
            match request.send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "无响应体".to_string());
                    tracing::debug!(target: "owo_gateway", status = %status, raw_error = %text, "模型网关原始错误");
                    return Err(format!("模型返回 {status}：{text}"));
                }
                Err(error) => {
                    last_error = format!("{label}: {error}");
                }
            }
        }
        Err(format!("模型请求失败：{last_error}"))
    }

    /// 数据出境开关：优先读运行时环境变量（支持设置页即时切换），缺省用启动配置。
    pub(super) fn cloud_enabled(&self) -> bool {
        if is_local_endpoint(&self.config.base_url) {
            return true;
        }
        std::env::var("OWO_CLOUD_ENABLED")
            .ok()
            .and_then(|value| value.parse::<bool>().ok())
            .unwrap_or(self.config.cloud_enabled)
    }

    /// 当前模型：优先读运行时环境变量（支持设置页热切换），缺省用启动配置。
    fn model(&self) -> String {
        std::env::var("OPENAI_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.config.model.clone())
    }

    /// 请求体 `model` 字段取值（M4.2）：请求级覆盖优先；覆盖为空串或 `"default"`
    /// 哨兵时回退 [`model`](Self::model) 解析链——哨兵值绝不泄漏进请求体。
    fn effective_model(&self, model: Option<&str>) -> String {
        match model.map(str::trim) {
            Some(override_model)
                if !override_model.is_empty() && override_model != MODEL_DEFAULT_SENTINEL =>
            {
                override_model.to_string()
            }
            _ => self.model(),
        }
    }

    pub(super) fn request_body(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        stream: bool,
    ) -> Value {
        let tool_payload: Vec<Value> = tools
            .iter()
            .map(|spec| {
                json!({
                    "type": "function",
                    "function": {
                        "name": spec.name,
                        "description": spec.description,
                        "parameters": spec.input_schema,
                    }
                })
            })
            .collect();
        let messages_payload: Vec<Value> = messages
            .iter()
            .map(|message| {
                let mut wire = json!({
                    "role": message.role,
                    "content": message.content,
                });
                // 多模态（取优合并自远端 engine）：user 消息带图片时，content 转成
                // parts 数组（OpenAI 兼容格式：text + image_url）。无图片时保持纯文本。
                if !message.images.is_empty() {
                    let mut parts: Vec<Value> = Vec::new();
                    if let Some(text) = &message.content {
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    for image in &message.images {
                        parts.push(json!({
                            "type": "image_url",
                            "image_url": { "url": image.url },
                        }));
                    }
                    wire["content"] = Value::Array(parts);
                }
                if let Some(tool_call_id) = &message.tool_call_id {
                    wire["tool_call_id"] = Value::String(tool_call_id.clone());
                }
                if let Some(tool_calls) = &message.tool_calls {
                    let wire_calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": serde_json::to_string(&call.arguments)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                }
                            })
                        })
                        .collect();
                    wire["tool_calls"] = Value::Array(wire_calls);
                }
                wire
            })
            .collect();

        let mut body = json!({
            "model": self.effective_model(model),
            "messages": messages_payload,
            "stream": stream,
        });
        if !tool_payload.is_empty() {
            body["tools"] = Value::Array(tool_payload);
        }
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        body
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.complete_with_model(None, messages, tools).await
    }

    async fn complete_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        if !self.cloud_enabled() {
            return Err("云端模型已禁用（数据出境开关关闭）".to_string());
        }
        if let Some(reason) = self.usage_budget_check() {
            return Err(reason);
        }
        let body = self.request_body(model, messages, tools, false);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self.post_chat(&url, &body).await?;

        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("模型响应解析失败：{e}"))?;
        self.record_usage(payload.get("usage").unwrap_or(&Value::Null));
        let message = payload
            .pointer("/choices/0/message")
            .ok_or_else(|| "响应缺少 choices[0].message".to_string())?;
        let content = message
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| {
                        let id = call.get("id")?.as_str()?.to_string();
                        let name = call.pointer("/function/name")?.as_str()?.to_string();
                        let arguments = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .and_then(|raw| serde_json::from_str(raw).ok())
                            .unwrap_or(Value::Null);
                        Some(ToolCall {
                            id,
                            name,
                            arguments,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|calls: &Vec<ToolCall>| !calls.is_empty());

        if let Some(tool_calls) = tool_calls {
            Ok(ModelOutput::ToolCalls(tool_calls))
        } else if let Some(content) = content {
            Ok(ModelOutput::Text(content))
        } else {
            Err("模型响应既无文本也无工具调用".to_string())
        }
    }

    async fn complete_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        self.complete_stream_with_model(None, messages, tools, on_delta)
            .await
    }

    async fn complete_stream_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        if !self.cloud_enabled() {
            return Err("云端模型已禁用（数据出境开关关闭）".to_string());
        }
        if let Some(reason) = self.usage_budget_check() {
            return Err(reason);
        }
        let body = self.request_body(model, messages, tools, true);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let response = self.post_chat(&url, &body).await?;

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut content = String::new();
        let mut accumulators: HashMap<usize, ToolCallAccumulator> = HashMap::new();
        let mut utf8_pending = Vec::new();
        let mut saw_sse = false;

        while let Some(chunk) =
            tokio::time::timeout(std::time::Duration::from_secs(60), stream.next())
                .await
                .map_err(|_| "模型流式输出空闲超时（60s 无数据）".to_string())?
        {
            let chunk = chunk.map_err(|e| format!("流式读取失败：{e}"))?;
            append_utf8_chunk(&mut buffer, &mut utf8_pending, &chunk);
            if let Some(usage) = consume_stream_buffer(
                &mut buffer,
                &mut content,
                &mut accumulators,
                on_delta,
                &mut saw_sse,
            ) {
                self.record_usage(&json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                }));
                // R9：流式路径每块检查预算，超限立即停轮并返回可读错误。
                if let Some(reason) = self.usage_budget_check() {
                    return Err(reason);
                }
            }
        }

        if !utf8_pending.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&utf8_pending));
        }
        if !buffer.trim().is_empty() {
            buffer.push('\n');
            if let Some(usage) = consume_stream_buffer(
                &mut buffer,
                &mut content,
                &mut accumulators,
                on_delta,
                &mut saw_sse,
            ) {
                self.record_usage(&json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                }));
                if let Some(reason) = self.usage_budget_check() {
                    return Err(reason);
                }
            }
        }

        if !saw_sse {
            return Err("模型流式响应为空或不是 SSE 格式".to_string());
        }

        if let Some(tool_calls) = build_tool_calls(&mut accumulators) {
            Ok(ModelOutput::ToolCalls(tool_calls))
        } else {
            Ok(ModelOutput::Text(content))
        }
    }

    fn usage_snapshot(&self) -> TokenUsage {
        self.usage.lock().map(|usage| *usage).unwrap_or_default()
    }
}
