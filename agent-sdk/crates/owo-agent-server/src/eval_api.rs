//! §12 评测域（eval）API 模块。
//!
//! 提取证明：`run_eval` 自 `lib.rs` 逐字迁移（POST /eval/run，
//! builtin/builtin-demo 套件经 `owo_agent_core::run_suite` 执行）。
//! 路由路径与 OpenAPI 登记零变化。AppState 写全限定名 `owo_agent_server::AppState`。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use owo_agent_protocol::EvalRunRequest;

pub(super) async fn run_eval(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<EvalRunRequest>,
) -> Result<Json<owo_agent_core::EvalReport>, (StatusCode, String)> {
    let suite = match request.suite_id.as_str() {
        "builtin" | "builtin-demo" => owo_agent_core::builtin_suite(),
        _ => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("未知评估套件：{}", request.suite_id),
            ));
        }
    };
    let model = std::env::var("OPENAI_MODEL")
        .unwrap_or_else(|_| owo_agent_core::gateway::DEFAULT_MODEL_ID.to_string());
    let provider = state.agent.provider();
    let report = owo_agent_core::run_suite(provider, &model, &suite).await;
    Ok(Json(report))
}
