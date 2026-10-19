//! 审计查询 HTTP API（§12：从 lib.rs 机械外移的 audit 域）。
//!
//! 路由面（`GET /audit`）与 /openapi.json 登记保持不变，零行为变化。
//! `flush_audit` 为域自有助手（内存审计 → 存储增量落库），一并外移；
//! lib 侧回合编排经 `audit_api::flush_audit` 调用，语义不变。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;

use owo_agent_server::AppState;

/// 把 Agent 内存审计日志中尚未落库的条目追加到存储，返回已 flush 数。
/// 对外契约：经 lib 根部 `pub use` 以 `owo_agent_server::flush_audit` 暴露给 CLI。
pub fn flush_audit(state: &AppState) {
    let mut flushed = match state.audit_flushed.lock() {
        Ok(flushed) => flushed,
        Err(_) => return,
    };
    let log = state.agent.audit_log();
    let audit = match log.lock() {
        Ok(audit) => audit,
        Err(_) => return,
    };
    if audit.entries.len() > *flushed {
        let entries = audit.entries[*flushed..].to_vec();
        let next = audit.entries.len();
        drop(audit);
        if state.store.append_audit(&entries).is_ok() {
            *flushed = next;
        }
    }
}

pub(super) async fn audit_list(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<owo_agent_core::AuditEntry>>, (StatusCode, String)> {
    flush_audit(&state);
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100)
        .min(500);
    let query = owo_agent_core::sqlite_store::AuditQuery {
        limit,
        offset: params
            .get("offset")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0),
        event: params
            .get("event")
            .filter(|value| !value.trim().is_empty())
            .cloned(),
        tool: params
            .get("tool")
            .filter(|value| !value.trim().is_empty())
            .cloned(),
        approved: params.get("approved").and_then(|value| value.parse().ok()),
        q: params
            .get("q")
            .filter(|value| !value.trim().is_empty())
            .cloned(),
    };
    let (entries, _) = state.store.query_audit(&query);
    Ok(Json(entries))
}
