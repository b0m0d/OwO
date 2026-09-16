//! 子代理 HTTP API（§12：从 lib.rs 机械外移的 subagent 域）。
//!
//! 路由面（`POST /subagent/run`）与 /openapi.json 登记保持不变，零行为变化。
//! 只读探索（@explore 对齐）与通用子代理（@subagent 对齐）语义原样保留。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

#[derive(Deserialize)]
pub(super) struct SubagentRunRequest {
    prompt: String,
    /// true 为只读探索模式（对齐 CLI `@explore`）；false 为通用子代理（对齐 `@subagent`）。
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    model: Option<String>,
}

pub(super) async fn subagent_run(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SubagentRunRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if request.prompt.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 prompt".to_string()));
    }
    let started = std::time::Instant::now();
    let workspace = state.workspace.clone();
    let model = request
        .model
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            std::env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| owo_agent_core::gateway::DEFAULT_MODEL_ID.to_string())
        });
    let agent = Arc::clone(&state.agent);
    let text = agent
        .run_subagent(&workspace, &model, &request.prompt, request.read_only)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let duration_ms = started.elapsed().as_millis() as u64;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "subagent",
            if request.read_only { "explore" } else { "run" },
            None,
            Some(true),
            format!(
                "{}子代理完成（{}ms）：{}",
                if request.read_only {
                    "只读探索"
                } else {
                    "通用"
                },
                duration_ms,
                request.prompt.chars().take(120).collect::<String>()
            ),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "read_only": request.read_only,
        "model": model,
        "duration_ms": duration_ms,
        "text": text,
    })))
}
