use crate::tools::ToolSpec;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: String) -> Self {
        Self {
            role: "system".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_text(content: String) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool(tool_call_id: String, content: String) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(tool_call_id),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }

    /// 回合增量 = 当前快照 − 回合前快照（saturating）。
    pub fn saturating_sub(&self, other: &TokenUsage) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.saturating_sub(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_sub(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_sub(other.total_tokens),
        }
    }

    /// 成本估算（美元）：价格按每百万 token 计，默认 0（未知价格不估算）。
    pub fn cost_estimate_usd(&self, input_per_mtok: f64, output_per_mtok: f64) -> f64 {
        self.prompt_tokens as f64 / 1_000_000.0 * input_per_mtok
            + self.completion_tokens as f64 / 1_000_000.0 * output_per_mtok
    }
}

/// 用量预算熔断：返回超限原因；未配置预算时返回 None。
///
/// 累计 token 上限（`OWO_USAGE_TOKEN_BUDGET`）与累计成本上限（美元，
/// `OWO_USAGE_COST_BUDGET_USD`，需配合单价环境变量）任一超限即熔断。
pub fn budget_violation(
    usage: &TokenUsage,
    total_tokens_cap: Option<u64>,
    cost_cap_usd: Option<f64>,
    input_price_per_mtok: f64,
    output_price_per_mtok: f64,
) -> Option<String> {
    if let Some(cap) = total_tokens_cap {
        if usage.total_tokens >= cap {
            return Some(format!(
                "模型用量预算已超限：累计 {} tokens ≥ 上限 {}",
                usage.total_tokens, cap
            ));
        }
    }
    if let Some(cap) = cost_cap_usd {
        let cost = usage.cost_estimate_usd(input_price_per_mtok, output_price_per_mtok);
        if cost >= cap {
            return Some(format!(
                "模型成本预算已超限：累计 ${cost:.6} ≥ 上限 ${cap:.6}"
            ));
        }
    }
    None
}

/// 从模型响应 usage 字段提取 token 用量（兼容 OpenAI/DeepSeek 与 Ollama 字段）。
pub fn parse_usage_value(usage: &Value) -> TokenUsage {
    if !usage.is_object() {
        return TokenUsage::default();
    }
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_eval_count").and_then(Value::as_u64))
        .unwrap_or(0);
    let completion = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| usage.get("eval_count").and_then(Value::as_u64))
        .unwrap_or(0);
    let total = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(prompt.saturating_add(completion));
    TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelOutput {
    Text(String),
    ToolCalls(Vec<ToolCall>),
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String>;

    /// 流式补全：文本增量经 `on_delta` 回调；返回最终输出。
    /// 默认实现退化为非流式。
    async fn complete_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        let output = self.complete(messages, tools).await?;
        if let ModelOutput::Text(text) = &output {
            on_delta(text.clone());
        }
        Ok(output)
    }

    /// 按请求覆盖模型（M4.2 会话级/档位路由）：`Some(model)` 时请求体使用该模型，
    /// `None`/空串/`"default"` 哨兵回退 Provider 自身解析链。默认实现忽略覆盖并
    /// 退化为 [`complete`](Self::complete)（测试桩/非 OpenAI Provider 零改动兼容）。
    async fn complete_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        let _ = model;
        self.complete(messages, tools).await
    }

    /// 流式版按请求覆盖（语义同 [`complete_with_model`](Self::complete_with_model)）。
    async fn complete_stream_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        let _ = model;
        self.complete_stream(messages, tools, on_delta).await
    }

    /// 累计 token 用量快照（供回合增量统计；未实现的 Provider 返回零）。
    fn usage_snapshot(&self) -> TokenUsage {
        TokenUsage::default()
    }
}
