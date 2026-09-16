//! 设置与权限档位 HTTP API（§12：从 lib.rs 机械外移的 settings/permissions/grants 域）。
//!
//! 路由面（`GET/POST /settings`、`POST /settings/egress`、`GET/POST /permissions`、
//! `GET /permissions/grants`、`POST /permissions/grants/revoke`）与 /openapi.json
//! 登记保持不变，零行为变化。
//! `auto_approve_enabled` 仍留根（health 与内联测试共用），不在本模块。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::whitelist::Whitelist;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

pub(super) async fn settings_get(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let settings = owo_agent_core::Settings::load(&state.workspace);
    let mut value = serde_json::to_value(&settings)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if let Some(object) = value.as_object_mut() {
        object.insert("runtime".to_string(), effective_runtime_config(&settings));
    }
    Ok(Json(value))
}

/// 只读有效运行配置：前端不得用历史表单默认值冒充当前 provider/model。
/// 凭据只暴露来源，不暴露内容。
pub(super) fn effective_runtime_config(settings: &owo_agent_core::Settings) -> Value {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://open.bigmodel.cn/api/paas/v4".to_string());
    let model = std::env::var("OPENAI_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| settings.model.clone())
        .unwrap_or_else(|| "glm-5.3-flash".to_string());
    let lower = base_url.to_ascii_lowercase();
    let provider = if lower.contains("bigmodel") || lower.contains("zhipu") {
        "bigmodel"
    } else if lower.contains("aliyuncs") || lower.contains("dashscope") {
        "qwen"
    } else if lower.contains("deepseek") {
        "deepseek"
    } else if lower.contains("ollama") || lower.contains("127.0.0.1") || lower.contains("localhost")
    {
        "ollama"
    } else {
        "openai-compatible"
    };
    let local = provider == "ollama";
    let cloud_enabled = std::env::var("OWO_CLOUD_ENABLED")
        .ok()
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(settings.egress.cloud_enabled);
    json!({
        "provider": provider,
        "model": model,
        "endpoint_kind": if local { "local" } else { "cloud" },
        "credential_source": if local { "not_required" } else if std::env::var_os("OPENAI_API_KEY").is_some() { "environment" } else { "missing" },
        "cloud_enabled": cloud_enabled,
        "available_models": [model],
    })
}

#[derive(Deserialize)]
pub(super) struct EgressRequest {
    cloud_enabled: bool,
}

pub(super) async fn settings_egress(
    State(state): State<Arc<AppState>>,
    Json(request): Json<EgressRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut settings = owo_agent_core::Settings::load(&state.workspace);
    settings.egress.cloud_enabled = request.cloud_enabled;
    settings
        .save(&state.workspace)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    std::env::set_var(
        "OWO_CLOUD_ENABLED",
        if request.cloud_enabled {
            "true"
        } else {
            "false"
        },
    );
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "settings",
            "egress",
            None,
            Some(request.cloud_enabled),
            format!("数据出境开关：cloud_enabled={}", request.cloud_enabled),
        );
    }
    Ok(Json(json!({
        "cloud_enabled": request.cloud_enabled,
        "note": "已写入 settings.json 并即时生效",
    })))
}

/// §5.3/§5.4 权限状态：当前档位 + 生效中的授权记忆（脱敏：不含参数原文）。
pub(super) async fn permissions_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let profile = state.agent.permission_profile();
    let grants: Vec<Value> = state
        .grants
        .list()
        .into_iter()
        .map(|grant| {
            json!({
                "grant_id": grant.grant_id,
                "tool_id": grant.tool_id,
                "workspace_id": grant.workspace_id,
                "path_scope": grant.path_scope.map(|path| path.to_string_lossy().into_owned()),
                "host_scope": grant.host_scope,
                "has_fingerprint": grant.argument_fingerprint.is_some(),
                "created_at": grant.created_at.to_rfc3339(),
                "expires_at": grant.expires_at.map(|at| at.to_rfc3339()),
                "remaining_uses": grant.remaining_uses,
            })
        })
        .collect();
    Ok(Json(json!({
        "profile": profile.label(),
        "read_only": state.agent.permission_profile() == owo_agent_core::PermissionProfile::ReadOnly,
        "grants": grants,
        "grant_count": grants.len(),
    })))
}

#[derive(Deserialize)]
pub(super) struct ProfileRequest {
    profile: String,
}

/// §5.3 切换权限档位：写 settings.json（重启恢复）+ 运行时即时生效。
pub(super) async fn permissions_set_profile(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProfileRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let profile = owo_agent_core::PermissionProfile::parse(&request.profile).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!(
                "未知档位：{}（可选 read_only / workspace / auto_review / full_access / custom）",
                request.profile
            ),
        )
    })?;
    let mut settings = owo_agent_core::Settings::load(&state.workspace);
    if profile == owo_agent_core::PermissionProfile::ReadOnly {
        settings.read_only = true;
    } else {
        settings.read_only = false;
        settings.permission_profile = Some(request.profile.clone());
    }
    settings
        .save(&state.workspace)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    state.agent.set_permission_profile(profile);
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "permissions",
            "profile",
            None,
            Some(true),
            format!("权限档位切换：{}", request.profile),
        );
    }
    Ok(Json(json!({
        "ok": true,
        "profile": profile.label(),
        "note": "已写入 settings.json 并即时生效",
    })))
}

/// §5.4 授权记忆列表（与 permissions_status 同构；独立路由便于前端独立刷新）。
pub(super) async fn grants_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let grants: Vec<Value> = state
        .grants
        .list()
        .into_iter()
        .map(|grant| {
            json!({
                "grant_id": grant.grant_id,
                "tool_id": grant.tool_id,
                "path_scope": grant.path_scope.map(|path| path.to_string_lossy().into_owned()),
                "host_scope": grant.host_scope,
                "expires_at": grant.expires_at.map(|at| at.to_rfc3339()),
                "remaining_uses": grant.remaining_uses,
            })
        })
        .collect();
    Ok(Json(
        json!({ "grants": grants, "grant_count": grants.len() }),
    ))
}

#[derive(Deserialize)]
pub(super) struct GrantRevokeRequest {
    grant_id: String,
}

/// §5.4 逐条撤销授权记忆（审计记录）。
pub(super) async fn grants_revoke(
    State(state): State<Arc<AppState>>,
    Json(request): Json<GrantRevokeRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let removed = state.grants.revoke(&request.grant_id);
    if !removed {
        return Err((
            StatusCode::NOT_FOUND,
            format!("授权记忆不存在：{}", request.grant_id),
        ));
    }
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "permissions",
            "grant_revoke",
            None,
            Some(true),
            format!("撤销授权记忆：{}", request.grant_id),
        );
    }
    Ok(Json(json!({ "ok": true, "revoked": request.grant_id })))
}

/// 通用设置保存：写入 settings.json 并应用运行时设置（数据出境、STT、主动建议、白名单）。
pub(super) async fn settings_update(
    State(state): State<Arc<AppState>>,
    Json(settings): Json<owo_agent_core::Settings>,
) -> Result<Json<Value>, (StatusCode, String)> {
    settings
        .save(&state.workspace)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    settings.apply_usage_env();
    // §13 批次六：遥测开关即时生效（默认关；与 cli serve 启动应用同一出口）。
    owo_agent_server::apply_telemetry_setting(settings.telemetry_enabled == Some(true));
    state
        .agent
        .apply_policy_settings(settings.read_only, &settings.deny_commands);
    // §5.3 档位从 settings 恢复（read_only=true 时以只读档为准）。
    if !settings.read_only {
        if let Some(profile) = settings
            .permission_profile
            .as_deref()
            .and_then(owo_agent_core::PermissionProfile::parse)
        {
            state.agent.set_permission_profile(profile);
        }
    }
    if let Some(model) = &settings.model {
        if !model.trim().is_empty() {
            std::env::set_var("OPENAI_MODEL", model);
        }
    }
    std::env::set_var(
        "OWO_CLOUD_ENABLED",
        settings.egress.cloud_enabled.to_string(),
    );
    if let Ok(mut stt) = state.stt.lock() {
        stt.apply_settings(&settings.stt);
    }
    if let Ok(mut proactive) = state.proactive.lock() {
        proactive.apply_settings(settings.proactive.clone());
    }
    if let Ok(mut whitelist) = state.whitelist.lock() {
        let mut merged = Whitelist::default();
        for entry in settings.whitelist.clone() {
            merged.upsert(entry);
        }
        *whitelist = merged;
    }
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "settings",
            "update",
            None,
            Some(true),
            "设置页保存（settings.json）",
        );
    }
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Settings);
    Ok(Json(json!({
        "ok": true,
        "note": "已写入 settings.json 并应用运行时设置（模型对新回合即时生效）",
        "runtime": effective_runtime_config(&settings),
    })))
}
