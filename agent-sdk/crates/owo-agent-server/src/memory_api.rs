//! Memory HTTP API（§12：从 lib.rs 机械外移的记忆查询/清理/技能挖掘域）。
//!
//! 路由面（`/memory/observations`、`/memory/clear`、`/memory/mine-skill`、
//! `/memory/recall`）与 /openapi.json 登记保持不变，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。lib.rs 的共享助手
//! `poison`/`parse_sensitivity` 因被多个域使用而无法随迁，此处放置模块本地副本。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::Sensitivity;
use owo_agent_server::AppState;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// 与 lib.rs 同款：锁中毒统一映射（模块本地副本，保持 #[path] 独立编译）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

/// 与 lib.rs 同款：敏感度解析（模块本地副本，保持 #[path] 独立编译）。
fn parse_sensitivity(value: &str) -> Result<Sensitivity, String> {
    match value {
        "low" => Ok(Sensitivity::Low),
        "medium" => Ok(Sensitivity::Medium),
        "high" => Ok(Sensitivity::High),
        "none" => Ok(Sensitivity::None),
        other => Err(format!("未知敏感度：{other}（low/medium/high/none）")),
    }
}

pub(super) async fn memory_observations(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = params
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100);
    let memory = state.memory.lock().map_err(poison)?;
    let observations = memory.list(limit);
    Ok(Json(json!({
        "count": observations.len(),
        "total": memory.count(),
        "observations": observations,
    })))
}

pub(super) async fn memory_clear(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut memory = state.memory.lock().map_err(poison)?;
    memory
        .clear()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Memory);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct MineSkillRequest {
    name: String,
    target_apps: Vec<String>,
    sensitivity: String,
    description: String,
}

/// 从情景记忆自动挖掘流程技能：观察到的动作序列 → 泛化 → 沉淀技能包。
pub(super) async fn memory_mine_skill(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MineSkillRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sensitivity = parse_sensitivity(&request.sensitivity)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    let actions = {
        let memory = state.memory.lock().map_err(poison)?;
        let observations = memory.list(0);
        owo_agent_core::map_sim_events_to_actions(&observations)
    };
    if actions.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "情景记忆中没有可挖掘的动作（请先运行模拟/真实操作并等待观察器入库）".to_string(),
        ));
    }
    let mut pipeline = state.pipeline.lock().map_err(poison)?;
    pipeline.recorder.start();
    for action in actions {
        pipeline
            .recorder
            .record(action)
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    }
    let package = pipeline
        .sink_skill(
            &request.name,
            request.target_apps,
            sensitivity,
            &request.description,
        )
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "memory",
            "mine-skill",
            Some(package.manifest.name.clone()),
            Some(true),
            format!("从情景记忆挖掘技能包：{}", package.manifest.name),
        );
    }
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Memory);
    Ok(Json(json!({
        "ok": true,
        "name": package.manifest.name,
        "variables": package.manifest.variables,
    })))
}

pub(super) async fn memory_recall(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let q = params
        .get("q")
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "缺少查询参数 q".to_string()))?;
    let top_k = params
        .get("top_k")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5)
        .min(50);
    let memory = state.memory.lock().map_err(poison)?;
    let hits = memory.recall(q, top_k);
    Ok(Json(json!({
        "count": hits.len(),
        "hits": hits,
    })))
}
