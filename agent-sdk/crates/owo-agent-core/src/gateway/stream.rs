use serde_json::Value;
use std::collections::HashMap;

use super::message::*;
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StreamDelta {
    pub content: Option<String>,
    /// 思考通道增量（`reasoning_content`，GLM/DeepSeek 约定）；不写入对话历史。
    pub reasoning: Option<String>,
    /// 原始 tool_calls 增量片段（JSON 值）。
    pub tool_call_fragments: Vec<Value>,
    /// 末尾 usage 块（OpenAI-compatible 流式响应在最后一条 data 中给出）。
    pub usage: Option<TokenUsage>,
}

/// 解析一条 `data:` 负载。空负载/心跳返回 None。
pub fn parse_sse_payload(payload: &str) -> Option<StreamDelta> {
    let payload = payload.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    let delta = value.pointer("/choices/0/delta")?;
    let content = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // 思考通道（取优合并自远端 engine）：GLM/DeepSeek 的 reasoning_content。
    let reasoning = delta
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let tool_call_fragments = delta
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let usage = value
        .get("usage")
        .map(parse_usage_value)
        .filter(|usage| usage.total_tokens > 0 || usage.prompt_tokens > 0);
    if content.is_none() && reasoning.is_none() && tool_call_fragments.is_empty() && usage.is_none()
    {
        return None;
    }
    Some(StreamDelta {
        content,
        reasoning,
        tool_call_fragments,
        usage,
    })
}

#[derive(Debug, Default)]
pub(super) struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

pub(super) fn accumulate_tool_fragments(
    accumulators: &mut HashMap<usize, ToolCallAccumulator>,
    fragments: &[Value],
) {
    for fragment in fragments {
        let Some(index) = fragment.get("index").and_then(Value::as_u64) else {
            continue;
        };
        let index = index as usize;
        let entry = accumulators.entry(index).or_default();
        if let Some(id) = fragment.get("id").and_then(Value::as_str) {
            entry.id = id.to_string();
        }
        if let Some(name) = fragment.pointer("/function/name").and_then(Value::as_str) {
            entry.name = name.to_string();
        }
        if let Some(arguments) = fragment
            .pointer("/function/arguments")
            .and_then(Value::as_str)
        {
            entry.arguments.push_str(arguments);
        }
    }
}

pub(super) fn build_tool_calls(
    accumulators: &mut HashMap<usize, ToolCallAccumulator>,
) -> Option<Vec<ToolCall>> {
    if accumulators.is_empty() {
        return None;
    }
    let mut calls: Vec<(usize, ToolCall)> = accumulators
        .drain()
        .map(|(index, accum)| {
            (
                index,
                ToolCall {
                    id: if accum.id.is_empty() {
                        format!("call_{index}")
                    } else {
                        accum.id
                    },
                    name: accum.name,
                    arguments: serde_json::from_str(&accum.arguments).unwrap_or(Value::Null),
                },
            )
        })
        .collect();
    calls.sort_by_key(|(index, _)| *index);
    Some(calls.into_iter().map(|(_, call)| call).collect())
}

pub(super) fn append_utf8_chunk(buffer: &mut String, pending: &mut Vec<u8>, chunk: &[u8]) {
    pending.extend_from_slice(chunk);
    match String::from_utf8(std::mem::take(pending)) {
        Ok(text) => buffer.push_str(&text),
        Err(error) => {
            let bytes = error.into_bytes();
            let valid = std::str::from_utf8(&bytes)
                .map(|_| bytes.len())
                .unwrap_or_else(|error| error.valid_up_to());
            buffer.push_str(std::str::from_utf8(&bytes[..valid]).unwrap_or_default());
            pending.extend_from_slice(&bytes[valid..]);
        }
    }
}

pub(super) fn consume_stream_buffer(
    buffer: &mut String,
    content: &mut String,
    accumulators: &mut HashMap<usize, ToolCallAccumulator>,
    on_delta: &mut (dyn FnMut(String) + Send),
    saw_sse: &mut bool,
) -> Option<TokenUsage> {
    let mut usage = None;
    while let Some(newline) = buffer.find('\n') {
        let line = buffer[..newline].trim().to_string();
        buffer.drain(..=newline);
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        *saw_sse = true;
        if payload.trim() == "[DONE]" {
            continue;
        }
        if let Some(delta) = parse_sse_payload(payload) {
            if delta.usage.is_some() {
                usage = delta.usage;
            }
            if let Some(delta_content) = delta.content {
                content.push_str(&delta_content);
                on_delta(delta_content);
            }
            accumulate_tool_fragments(accumulators, &delta.tool_call_fragments);
        }
    }
    usage
}
