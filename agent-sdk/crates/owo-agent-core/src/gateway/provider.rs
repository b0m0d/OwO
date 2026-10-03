use crate::tools::ToolSpec;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

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
            .connect_timeout(Duration::from_secs(10))
            .timeout(model_request_timeout());
        let mut has_proxy = false;
        // Local model endpoints are isolated from HTTP proxies; proxying loopback
        // breaks local inference and can turn localhost into a remote request.
        if !is_local_endpoint(&config.base_url) {
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
        }
        let client = builder
            .build()
            .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
        let direct_client = if has_proxy {
            Some(
                reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(model_request_timeout())
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
            let mut list = Vec::with_capacity(2);
            let direct_first = std::env::var("OWO_MODEL_DIRECT_FIRST")
                .ok()
                .is_some_and(|value| {
                    matches!(
                        value.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes"
                    )
                });
            if direct_first {
                if let Some(direct) = &self.direct_client {
                    list.push(("direct", direct));
                }
            }
            list.push(("proxy", &self.client));
            if !direct_first {
                if let Some(direct) = &self.direct_client {
                    list.push(("direct", direct));
                }
            }
            list
        };
        for (label, client) in attempts {
            let request = client.post(url).json(body).timeout(model_request_timeout());
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
        if let Some(max_tokens) = max_output_tokens_from_env() {
            body["max_tokens"] = Value::from(max_tokens);
        }
        // 推理档位只在用户显式选择时才下发（默认请求体与旧版完全一致）。
        if let Some(effort) = reasoning_effort_from_env() {
            body["reasoning_effort"] = Value::String(effort);
        }
        body
    }
}

/// 推理档位（`reasoning_effort`，取优合并自远端 engine）：读运行时环境变量
/// （设置页保存后即时生效）。只认 minimal/low/medium/high；未设置或取值非法
/// 则返回 None = 不发送该参数，避免不支持它的 OpenAI 兼容端点因为未知字段 400。
fn reasoning_effort_from_env() -> Option<String> {
    let value = std::env::var("OWO_REASONING_EFFORT")
        .ok()?
        .trim()
        .to_ascii_lowercase();
    if matches!(value.as_str(), "minimal" | "low" | "medium" | "high") {
        Some(value)
    } else {
        None
    }
}

/// OpenAI-compatible output cap. GLM accepts `max_tokens`; invalid/out-of-range values
/// are ignored so a stale local setting cannot turn every request into a provider 400.
fn max_output_tokens_from_env() -> Option<u64> {
    std::env::var("OWO_MODEL_MAX_OUTPUT_TOKENS")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| (1..=32000).contains(value))
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
        self.complete_with_model_observed(model, messages, tools)
            .await
            .map(|observed| observed.output)
    }

    async fn complete_with_model_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ObservedModelOutput, String> {
        let started = std::time::Instant::now();
        if !self.cloud_enabled() {
            return Err("云端模型已禁用（数据出境开关关闭）".to_string());
        }
        if let Some(reason) = self.usage_budget_check() {
            return Err(reason);
        }
        let body = self.request_body(model, messages, tools, false);
        let request_model = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches("/")
        );
        let response = self.post_chat(&url, &body).await?;
        let request_id = ["x-request-id", "request-id", "openai-request-id"]
            .iter()
            .find_map(|name| response.headers().get(*name))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("模型响应解析失败：{e}"))?;
        let usage_value = payload.get("usage");
        self.record_usage(usage_value.unwrap_or(&Value::Null));
        let usage = usage_value
            .filter(|value| {
                let has_prompt = value
                    .get("prompt_tokens")
                    .or_else(|| value.get("prompt_eval_count"))
                    .and_then(Value::as_u64)
                    .is_some();
                let has_completion = value
                    .get("completion_tokens")
                    .or_else(|| value.get("eval_count"))
                    .and_then(Value::as_u64)
                    .is_some();
                has_prompt && has_completion
            })
            .map(parse_usage_value);
        let response_model = payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
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
        let output = if let Some(tool_calls) = tool_calls {
            ModelOutput::ToolCalls(tool_calls)
        } else if let Some(content) = content {
            ModelOutput::Text(content)
        } else {
            return Err("模型响应既无文本也无工具调用".to_string());
        };
        Ok(ObservedModelOutput {
            output,
            metadata: ModelCallMetadata {
                request_id,
                model: response_model.or(request_model),
                usage,
                latency_ms: Some(started.elapsed().as_millis() as u64),
            },
        })
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
        // 兼容入口：只转发正文增量（思考通道经 reasoning 变体消费）。
        let mut forward = |chunk: StreamChunk| {
            if let StreamChunk::Content(text) = chunk {
                on_delta(text);
            }
        };
        self.stream_completion(model, messages, tools, &mut forward)
            .await
    }

    async fn complete_stream_with_reasoning(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.stream_completion(None, messages, tools, on_chunk)
            .await
    }

    async fn complete_stream_with_reasoning_and_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.stream_completion(model, messages, tools, on_chunk)
            .await
    }

    async fn complete_stream_with_reasoning_and_model_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ObservedModelOutput, String> {
        self.stream_completion_observed(model, messages, tools, on_chunk)
            .await
    }

    fn usage_snapshot(&self) -> TokenUsage {
        self.usage.lock().map(|usage| *usage).unwrap_or_default()
    }
}

fn parse_timeout_secs(raw: Option<&str>, default_secs: u64) -> u64 {
    raw.and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=600).contains(value))
        .unwrap_or(default_secs)
}

fn duration_from_env(name: &str, default_secs: u64) -> Duration {
    Duration::from_secs(parse_timeout_secs(
        std::env::var(name).ok().as_deref(),
        default_secs,
    ))
}

fn model_request_timeout() -> Duration {
    duration_from_env("OWO_MODEL_REQUEST_TIMEOUT_SECS", 240)
}

fn model_stream_idle_timeout() -> Duration {
    duration_from_env("OWO_MODEL_STREAM_IDLE_TIMEOUT_SECS", 60)
}

impl OpenAiCompatibleProvider {
    /// 流式补全唯一实现：正文与思考通道统一经 `on_chunk` 回调（类型区分）。
    async fn stream_completion(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.stream_completion_observed(model, messages, tools, on_chunk)
            .await
            .map(|observed| observed.output)
    }

    async fn stream_completion_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ObservedModelOutput, String> {
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
        let request_model = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        let response = self.post_chat(&url, &body).await?;
        let request_id = ["x-request-id", "request-id", "openai-request-id"]
            .iter()
            .find_map(|name| response.headers().get(*name))
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        let mut stream = response.bytes_stream();
        let mut request_usage = None;
        let mut state = StreamState {
            request_id,
            ..StreamState::default()
        };

        while let Some(chunk) = tokio::time::timeout(model_stream_idle_timeout(), stream.next())
            .await
            .map_err(|_| {
                format!(
                    "模型流式输出空闲超时（{}s 无数据）",
                    model_stream_idle_timeout().as_secs()
                )
            })?
        {
            let chunk = chunk.map_err(|e| format!("流式读取失败：{e:?}"))?;
            append_utf8_chunk(&mut state.buffer, &mut state.utf8_pending, &chunk);
            if let Some(usage) = consume_stream_buffer(&mut state, on_chunk) {
                request_usage = Some(usage);
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

        if !state.utf8_pending.is_empty() {
            state
                .buffer
                .push_str(&String::from_utf8_lossy(&state.utf8_pending));
        }
        if !state.buffer.trim().is_empty() {
            state.buffer.push('\n');
            if let Some(usage) = consume_stream_buffer(&mut state, on_chunk) {
                request_usage = Some(usage);
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

        if !state.saw_sse {
            return Err("模型流式响应为空或不是 SSE 格式".to_string());
        }
        if state.finish_reason.as_deref() == Some("length") {
            return Err("模型输出达到 max_tokens 上限（finish_reason=length），请提高 OWO_MODEL_MAX_OUTPUT_TOKENS 或缩小单次任务".to_string());
        }
        if !state.saw_done
            && !matches!(
                state.finish_reason.as_deref(),
                Some("stop" | "tool_calls" | "function_call" | "content_filter")
            )
        {
            return Err("模型流式响应未收到结束标记，且缺少正常 finish_reason".to_string());
        }

        let output = if let Some(tool_calls) = build_tool_calls(&mut state.tool_call_accumulators) {
            ModelOutput::ToolCalls(tool_calls)
        } else {
            ModelOutput::Text(state.content)
        };
        Ok(ObservedModelOutput {
            output,
            metadata: ModelCallMetadata {
                request_id: state.request_id,
                model: state.response_model.or(request_model),
                usage: request_usage,
                latency_ms: None,
            },
        })
    }
}

#[cfg(test)]
mod timeout_config_tests {
    use super::parse_timeout_secs;

    #[test]
    fn timeout_values_are_bounded_and_fall_back_safely() {
        assert_eq!(parse_timeout_secs(None, 240), 240);
        assert_eq!(parse_timeout_secs(Some("90"), 240), 90);
        assert_eq!(parse_timeout_secs(Some("0"), 240), 240);
        assert_eq!(parse_timeout_secs(Some("601"), 240), 240);
        assert_eq!(parse_timeout_secs(Some("invalid"), 240), 240);
    }
}
