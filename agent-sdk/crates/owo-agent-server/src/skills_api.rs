//! 技能注册表 HTTP API（§12：从 lib.rs 机械外移的 skills 域）。
//!
//! 路由面（`GET /skills`、`GET/POST /skills/{name}`、`POST /skills/{name}/enabled`、
//! `GET /skills/health`、`POST /skills/health/{name}/reset`）与 /openapi.json
//! 登记保持不变，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// 模块内 `poison` 副本（§12 惯例：共享根助手保持各域独立可用）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

pub(super) async fn list_skills(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    let registry = state.agent.skills();
    let skills = registry.list();
    Ok(Json(
        skills
            .iter()
            .map(|skill| {
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "path": skill.path.to_string_lossy(),
                    "enabled": registry.is_enabled(&skill.name),
                })
            })
            .collect(),
    ))
}

pub(super) async fn skill_detail(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let registry = state.agent.skills();
    let skill = registry
        .get(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("技能不存在：{name}")))?;
    let content = std::fs::read_to_string(&skill.path)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Json(json!({
        "name": skill.name,
        "description": skill.description,
        "path": skill.path.to_string_lossy(),
        "enabled": registry.is_enabled(&name),
        "content": content,
    })))
}

#[derive(Deserialize)]
pub(super) struct SkillEditRequest {
    content: String,
}

pub(super) async fn skill_edit(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
    Json(request): Json<SkillEditRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let skill = state
        .agent
        .skills()
        .get(&name)
        .ok_or((StatusCode::NOT_FOUND, format!("技能不存在：{name}")))?;
    std::fs::write(&skill.path, &request.content)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "skills",
            "edit",
            Some(name.clone()),
            Some(true),
            "SKILL.md 已更新",
        );
    }
    Ok(Json(json!({
        "ok": true,
        "note": "SKILL.md 已更新（注册表内技能重启核心服务后生效）",
    })))
}

#[derive(Deserialize)]
pub(super) struct SkillEnabledRequest {
    enabled: bool,
}

pub(super) async fn skill_enabled(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
    Json(request): Json<SkillEnabledRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let registry = state.agent.skills();
    if registry.get(&name).is_none() {
        return Err((StatusCode::NOT_FOUND, format!("技能不存在：{name}")));
    }
    let disabled = registry.disabled_set();
    {
        let mut set = disabled.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "禁用集合锁中毒".to_string(),
            )
        })?;
        if request.enabled {
            set.remove(&name);
        } else {
            set.insert(name.clone());
        }
    }
    let mut settings = owo_agent_core::Settings::load(&state.workspace);
    let mut list = {
        let set = disabled.lock().map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "禁用集合锁中毒".to_string(),
            )
        })?;
        let mut list: Vec<String> = set.iter().cloned().collect();
        list.sort();
        list
    };
    settings.skills.disabled = std::mem::take(&mut list);
    settings
        .save(&state.workspace)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "skills",
            "enabled",
            Some(name.clone()),
            Some(request.enabled),
            format!(
                "技能{}：{name}",
                if request.enabled { "启用" } else { "禁用" }
            ),
        );
    }
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Skills);
    Ok(Json(json!({
        "ok": true,
        "enabled": request.enabled,
        "note": "已即时生效",
    })))
}

pub(super) async fn skills_health(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let pipeline = state.pipeline.lock().map_err(poison)?;
    let skills: Vec<Value> = pipeline
        .store
        .list_health()
        .into_iter()
        .map(|(name, health)| {
            json!({
                "name": name,
                "state": health.state,
                "attempts": health.attempts,
                "successes": health.successes,
                "success_rate": health.success_rate(),
                "consecutive_failures": health.consecutive_failures,
                "template_hit_rate": health.template_hit_rate(),
                "recent_failures": health.recent_failures,
            })
        })
        .collect();
    Ok(Json(json!({ "count": skills.len(), "skills": skills })))
}

pub(super) async fn skill_health_reset(
    State(state): State<Arc<AppState>>,
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let pipeline = state.pipeline.lock().map_err(poison)?;
    pipeline
        .store
        .reset_health(&name)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "learn",
            "health-reset",
            Some(name.clone()),
            Some(true),
            format!("重置技能健康度：{name}"),
        );
    }
    Ok(Json(json!({ "ok": true, "name": name })))
}
