//! Trace HTTP API（§12 第二刀：从 lib.rs 机械外移的 traces 查询域）。
//!
//! 路由面（`/traces`、`/traces/{index}`）与 /openapi.json 登记保持不变，
//! 本模块只承载查询处理函数，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

pub(super) async fn traces_list(State(state): State<Arc<AppState>>) -> Json<Value> {
    let traces = owo_agent_core::list_traces(&state.traces_dir);
    let items: Vec<Value> = traces
        .iter()
        .filter_map(|path| {
            let trace = owo_agent_core::load_trace(path).ok()?;
            let preview: String = trace.prompt.chars().take(60).collect();
            Some(json!({
                        "index": {
                            //
            // index 为在倒序列表中的位置（回放用）。
                            "position": traces.iter().position(|p| p == path).unwrap_or(0),
                        },
                        "file": path.file_name().unwrap_or_default().to_string_lossy(),
                        "session_id": trace.session_id,
                        "model": trace.model,
                        "prompt_preview": preview,
                        "prompt": trace.prompt,
                        "started_at": trace.started_at,
                        "duration_ms": trace.duration_ms,
                        "steps": trace.steps,
                        "has_final": trace.final_text.is_some(),
                        "final_text": trace.final_text,
                        "usage": json!({
                            "prompt_tokens": trace.usage.prompt_tokens,
                            "completion_tokens": trace.usage.completion_tokens,
                            "total_tokens": trace.usage.total_tokens,
                        }),
                    }))
        })
        .collect();
    Json(json!({ "count": items.len(), "traces": items }))
}

pub(super) async fn trace_show(
    State(state): State<Arc<AppState>>,
    AxumPath(index): AxumPath<usize>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let traces = owo_agent_core::list_traces(&state.traces_dir);
    let path = traces.get(index).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            format!("trace 序号越界（共 {} 条）", traces.len()),
        )
    })?;
    let trace = owo_agent_core::load_trace(path)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Json(json!({
        "index": index,
        "file": path.file_name().unwrap_or_default().to_string_lossy(),
        "session_id": trace.session_id,
        "workspace": trace.workspace,
        "model": trace.model,
        "prompt": trace.prompt,
        "started_at": trace.started_at,
        "duration_ms": trace.duration_ms,
        "steps": trace.steps,
        "final_text": trace.final_text,
        "usage": trace.usage,
        "events": trace.events,
    })))
}
