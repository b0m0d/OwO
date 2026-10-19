//! 桌面应用白名单 HTTP API（§12：从 lib.rs 机械外移的权限白名单域）。
//!
//! 路由面（`GET /whitelist`、`POST /whitelist/manage`）与 /openapi.json 登记保持
//! 不变，零行为变化。内存白名单与 settings.json 持久层双写语义原样保留。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::whitelist::WhitelistEntry;
use serde::Deserialize;
use std::sync::Arc;

use owo_agent_server::AppState;

/// 模块内 `poison` 副本（§12 惯例：共享根助手保持各域独立可用）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

pub(super) async fn whitelist_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<WhitelistEntry>>, (StatusCode, String)> {
    let whitelist = state.whitelist.lock().map_err(poison)?;
    Ok(Json(whitelist.entries().to_vec()))
}

#[derive(Deserialize)]
pub(super) struct WhitelistManageRequest {
    action: String,
    #[serde(default)]
    entry: Option<WhitelistEntry>,
    #[serde(default)]
    app_id: Option<String>,
}

pub(super) async fn whitelist_manage(
    State(state): State<Arc<AppState>>,
    Json(request): Json<WhitelistManageRequest>,
) -> Result<Json<Vec<WhitelistEntry>>, (StatusCode, String)> {
    let action = request.action.clone();
    let entry = request.entry.clone();
    let app_id = request.app_id.clone();
    let entries = {
        let mut whitelist = state.whitelist.lock().map_err(poison)?;
        match action.as_str() {
            "upsert" => {
                let entry = entry
                    .clone()
                    .ok_or((StatusCode::BAD_REQUEST, "upsert 需要 entry".to_string()))?;
                whitelist.upsert(entry);
            }
            "remove" => {
                let app_id = app_id
                    .clone()
                    .ok_or((StatusCode::BAD_REQUEST, "remove 需要 app_id".to_string()))?;
                whitelist.remove(&app_id);
            }
            other => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("未知操作：{other}（upsert / remove）"),
                ));
            }
        }
        whitelist.entries().to_vec()
    };
    let mut settings = owo_agent_core::Settings::load(&state.workspace);
    match action.as_str() {
        "upsert" => {
            let entry = entry.ok_or((StatusCode::BAD_REQUEST, "upsert 需要 entry".to_string()))?;
            if let Some(existing) = settings
                .whitelist
                .iter_mut()
                .find(|existing| existing.app_id == entry.app_id)
            {
                *existing = entry.clone();
            } else {
                settings.whitelist.push(entry);
            }
        }
        "remove" => {
            let app_id =
                app_id.ok_or((StatusCode::BAD_REQUEST, "remove 需要 app_id".to_string()))?;
            settings
                .whitelist
                .retain(|existing| existing.app_id != app_id);
        }
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("未知操作：{other}（upsert / remove）"),
            ));
        }
    }
    settings
        .save(&state.workspace)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Whitelist);
    Ok(Json(entries))
}
