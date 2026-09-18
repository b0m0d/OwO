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

/// R3-B（§3.4 动作「测试连接」）：提供商自诊断端点。
///
/// 契约（§2.4 统一错误模型）：响应恒携带稳定码，UI 按 `code` 渲染动作，不匹配中文——
/// - `provider/not_configured`：云端路径缺凭据（**先判凭据，不发起任何网络**）；
/// - `provider/endpoint_reachable` / `provider/endpoint_unreachable`：仅 TCP 层探测
///   （3s 超时），不发真实补全请求、不消耗 token；
/// - 端点展示一律脱敏（scheme+host[:port]，无路径/查询/ userinfo）。
pub(super) async fn settings_provider_test(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let settings = owo_agent_core::Settings::load(&state.workspace);
    let runtime = effective_runtime_config(&settings);
    let base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://open.bigmodel.cn/api/paas/v4".to_string());
    let is_local = runtime["endpoint_kind"].as_str() == Some("local");
    let key_present = std::env::var_os("OPENAI_API_KEY")
        .map(|value| !value.to_string_lossy().trim().is_empty())
        .unwrap_or(false);
    let credential = if is_local {
        "not_required"
    } else if key_present {
        "environment"
    } else {
        "missing"
    };
    let (code, ok, message, latency_ms) = if !is_local && !key_present {
        (
            "provider/not_configured",
            false,
            "模型提供商未配置：请在设置中选择云端（并配置 OPENAI_API_KEY 环境变量）或本地 Ollama"
                .to_string(),
            Value::Null,
        )
    } else if !is_local
        && !std::env::var("OWO_CLOUD_ENABLED")
            .ok()
            .and_then(|value| value.parse::<bool>().ok())
            .unwrap_or(true)
    {
        // 数据出境开关（AGENTS.md 辅助变量 `OWO_CLOUD_ENABLED=false`，与 gateway.rs
        // 的云端放行判定同一语义）：**测试连接本身也是一条出网路径**，必须在发起
        // 连接之前就拒绝。否则用户显式关掉了云，点一下引导页的「测试连接」仍然会
        // 打公网端点——那既是出境泄漏，也让 cargo test 顺带探测公网。
        //
        // 这里返回的是**本地判定结论**，不是网络结论，所以另起稳定码，不复用
        // endpoint_unreachable：把"我们拒绝出网"标成"端点不可达"会把用户推向
        // 查防火墙这条完全错误的路上（§2.5 错误模型：归因必须指向可执行的下一步）。
        (
            "network/cloud_disabled",
            false,
            "已拒绝云端访问（OWO_CLOUD_ENABLED=false）：请改用本地 Ollama，或确认需要出网后显式放开该开关".to_string(),
            Value::Null,
        )
    } else {
        match probe_endpoint(&base_url).await {
            Ok(ms) => (
                "provider/endpoint_reachable",
                true,
                "端点可达（TCP 层；模型答复需真实调用验证）".to_string(),
                json!(ms),
            ),
            Err(error) => (
                "provider/endpoint_unreachable",
                false,
                format!("端点不可达：{error}"),
                Value::Null,
            ),
        }
    };
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "settings",
            "provider_test",
            None,
            Some(ok),
            format!("提供商连接测试：code={code}"),
        );
    }
    Ok(Json(json!({
        "ok": ok,
        "code": code,
        "endpoint": mask_endpoint(&base_url),
        "model": runtime["model"],
        "credential": credential,
        "latency_ms": latency_ms,
        "message": message,
    })))
}

/// 端点脱敏：仅保留 scheme + host[:port]（禁止回显路径/查询/用户信息段）。
fn mask_endpoint(base_url: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("unknown", base_url));
    let authority = rest.split('/').next().unwrap_or("");
    // userinfo（user@host）一律剥离。
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    if host.is_empty() {
        scheme.to_string()
    } else {
        format!("{scheme}://{host}")
    }
}

/// TCP 层可达性探测（3s 超时）。返回连接耗时毫秒。
async fn probe_endpoint(base_url: &str) -> Result<u128, String> {
    let rest = base_url
        .split_once("://")
        .map(|(_, tail)| tail)
        .unwrap_or(base_url);
    let authority = rest.split('/').next().unwrap_or(rest);
    // userinfo 必须先剥掉再解析：否则 `https://token@host/v1` 会把整串
    // `token@host` 当主机名送进 lookup_host —— 凭据因此进入 DNS 查询与解析错误
    // 上下文，正是这个端点最不该发生的泄露面（掩码端点却拿未掩码的值去连网）。
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host_only)| host_only)
        .unwrap_or(authority);
    let default_port: u16 = if base_url.starts_with("http://") {
        80
    } else {
        443
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            // `[::1]:8080` 这类括号 IPv6 字面量只有冒号出现在 ']' 之后才是端口；
            // `[::1]`（省略端口）不能误判成"端口无效"。
            let explicit = !h.starts_with('[') || h.ends_with(']');
            if !explicit {
                (authority.to_string(), default_port)
            } else {
                // 写了 `:xxx` 却不是数字：**必须直说配置错了**。此前这里静默按
                // 443 去连，把"端口打错"报成"端点不可达"，归因方向整个错掉（§2.5）。
                match p.parse::<u16>() {
                    Ok(v) => (h.to_string(), v),
                    Err(_) => return Err(format!("端口无效：{p}")),
                }
            }
        }
        None => (authority.to_string(), default_port),
    };
    if host.is_empty() {
        return Err("端点缺少主机名".to_string());
    }
    let started = std::time::Instant::now();
    let connect = async move {
        let addrs = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| format!("DNS 解析失败：{error}"))?;
        let addrs: Vec<_> = addrs.collect();
        let addr = addrs.first().ok_or("DNS 无解析结果".to_string())?;
        tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|error| format!("连接 {}:{} 失败：{error}", addr.ip(), addr.port()))
    };
    match tokio::time::timeout(std::time::Duration::from_secs(3), connect).await {
        Err(_) => Err("连接超时（3s）".to_string()),
        Ok(Err(error)) => Err(error),
        Ok(Ok(_stream)) => Ok(started.elapsed().as_millis()),
    }
}

/// §3.4「provider 未配置 / 测试连接」契约的 L2 底线：这条端点会被引导页按钮调用，
/// 它的返回值**必然落盘到诊断台账并显示给用户**，所以"只回掩码端点、绝不回显凭据"
/// 不是风格问题，而是秘密泄露面。真机矩阵只能证明界面有码可看，证明不了掩码正确性，
/// 故在此按单元层钉住。
#[cfg(test)]
mod provider_test_contract {
    use super::{mask_endpoint, probe_endpoint};

    #[test]
    fn mask_keeps_host_but_strips_credentials_and_path() {
        // 端点里带 userinfo、带 path、query 里还塞了个 key：三者都不得出现在结果里。
        let masked = mask_endpoint("https://user:pw@api.bigmodel.cn/api/paas/v4?key=sk-SECRET123");
        assert_eq!(masked, "https://api.bigmodel.cn");
        assert!(!masked.contains("SECRET123"), "查询串里的 key 不得回显");
        assert!(!masked.contains("user"), "userinfo 不得回显");
        // 掩码后的"authority 段"必须就是主机名：路径与查询都不得残留
        // （`://` 里的两个斜杠是协议分隔符，不算路径）。
        assert_eq!(
            masked.split_once("://").map(|(_, tail)| tail),
            Some("api.bigmodel.cn"),
            "scheme 之后不得再有路径：{masked}"
        );
    }

    #[test]
    fn mask_keeps_local_port_for_diagnostics() {
        // Ollama 这类本地端点：端口不是秘密，且"127.0.0.1:11434 连不通"正是用户
        // 需要看见的信息，剥掉端口只会让排查回到猜。
        assert_eq!(
            mask_endpoint("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434"
        );
    }

    #[test]
    fn mask_degrades_on_non_url_input_without_panicking() {
        // 配置里出现裸串（用户手输漏了 scheme）时，掩码必须仍可渲染。
        let masked = mask_endpoint("just-a-host");
        assert!(!masked.is_empty());
        assert!(
            masked.starts_with("unknown://"),
            "退化形态应显式标 unknown，实测试探到的正是这一支：{masked}"
        );
    }

    #[tokio::test]
    async fn probe_returns_transport_verdict_without_credentials() {
        // 端口非法：解析层就拒绝，不该发起任何连接。
        let bad = probe_endpoint("https://user:pw@example.invalid:notaport/v1").await;
        assert!(bad.is_err());
        let bad_msg = bad.err().unwrap();
        assert!(
            bad_msg.contains("端口无效"),
            "应给出可操作的归因：{bad_msg}"
        );
        assert!(!bad_msg.contains("pw"), "错误信息不得带出 userinfo");

        // 保留端口但含 userinfo/path 的端点：错误里只可能出现 host:port。
        let refused = probe_endpoint("https://token@127.0.0.1:1/v1").await;
        let msg = match refused {
            Ok(ms) => format!("ok {ms}"),
            Err(e) => e,
        };
        assert!(!msg.contains("token"), "回显里不得出现凭据：{msg}");
        assert!(!msg.contains("/v1"), "回显里不得出现路径：{msg}");
    }
}
