//! 服务器运维 HTTP API（§12：从 lib.rs 机械外移的运行状态与优雅关闭域）。
//!
//! 路由面（`GET /server/status`、`POST /server/shutdown`）与 /openapi.json 登记
//! 保持不变，零行为变化。§4.2 实例握手门禁原样保留。
//!
//! 说明：`auth_token`/`logging` 为根私有模块，本模块作为根的后代模块经
//! crate 名全限定路径访问（本文件无 `#[path]` 独立测试目标，无双目标约束）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// R8：服务运行状态（并发上限/在途/关闭中 + 存储只读降级提示）。
pub(super) async fn server_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "shutdown_gate": {
            "max_concurrent_turns": state.shutdown_gate.max_concurrent(),
            "active_turns": state.shutdown_gate.active_turns(),
            "shutting_down": state.shutdown_gate.shutting_down(),
        },
        "storage": {
            "read_only": state.store.is_read_only(),
            "migration_warning": state.store.migration_warning(),
        },
    }))
}

#[derive(Deserialize)]
pub(super) struct ShutdownRequest {
    confirm: Option<bool>,
}

/// R8：优雅关闭入口（需二次确认；CLI serve 侧接线完成「停止接收→完成在途→flush→退出」）。
/// §4.2 实例握手：注入实例身份时，只有同一桌面实例可关闭服务——
/// 防止旧壳残留或外来进程关掉新实例（对应审计「关闭时先带实例证明调用 /server/shutdown」）。
pub(super) async fn server_shutdown(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(request): Json<ShutdownRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !owo_agent_server::auth_token::instance_gate_allows(
        owo_agent_server::auth_token::desktop_instance_id().as_deref(),
        headers.get(owo_agent_server::auth_token::DESKTOP_INSTANCE_HEADER),
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "桌面实例身份不匹配：该核心服务属于另一个桌面实例",
                "code": "auth/instance_mismatch/not_retryable",
            })),
        ));
    }
    if request.confirm != Some(true) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "需要二次确认：{\"confirm\":true}",
                "code": "validation/invalid_input/not_retryable",
            })),
        ));
    }
    let active = state.shutdown_gate.request_shutdown();
    owo_agent_server::logging::warn(
        "server",
        None,
        "收到优雅关闭请求（需二次确认）",
        &[("active_turns", json!(active))],
    );
    Ok(Json(json!({
        "ok": true,
        "shutting_down": true,
        "active_turns": active,
        "note": "已停止接收新回合；在途回合完成后服务将退出（CLI serve 接线）",
    })))
}
