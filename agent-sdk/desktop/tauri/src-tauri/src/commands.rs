//! §4.3/§4.2 WebView 诊断命令：启动状态、连接描述符、手动重试、日志入口。
//!
//! 字段命名与 Web 侧契约（api-client.js / app.js）逐字一致：
//! - get_core_state → {state, attempt, errorCode?, message?, logPath?, port?, instanceId?}
//! - get_core_connection → ready: {port, instanceId, pairing, apiVersion, pid, buildId, state:"ready"}；
//!   未 ready: {port:0, state, errorCode?, message?}
//! - retry_core_start / open_core_logs → 同 get_core_state 结构（操作后回执）。
use serde_json::{json, Value};
use tauri::State;

use crate::core_runtime::{CoreRuntime, CoreState};
use crate::provider::{ProviderConfig, ProviderMode, ProviderStatus};

fn state_to_value(state: &CoreState, log_path: &std::path::Path) -> Value {
    match state {
        CoreState::Starting { attempt } => json!({
            "state": "starting",
            "attempt": attempt,
            "logPath": log_path.to_string_lossy(),
        }),
        CoreState::Ready(connection) => json!({
            "state": "ready",
            "port": connection.port,
            "instanceId": connection.instance_id,
            "apiVersion": connection.api_version,
            "pid": connection.pid,
            "buildId": connection.build_id,
            "logPath": log_path.to_string_lossy(),
        }),
        CoreState::Restarting { attempt, reason } => json!({
            "state": "restarting",
            "attempt": attempt,
            "message": reason,
            "logPath": log_path.to_string_lossy(),
        }),
        CoreState::Failed {
            code,
            message,
            log_path,
        } => json!({
            "state": "failed",
            "errorCode": code,
            "message": message,
            "logPath": log_path.to_string_lossy(),
        }),
        CoreState::NoWorkspace => json!({
            "state": "no_workspace",
            "message": "尚未选择项目工作区：请选择目录后启用文件工具",
            "logPath": log_path.to_string_lossy(),
        }),
        CoreState::Stopped => json!({ "state": "stopped" }),
    }
}

/// 启动诊断状态（§4.3：错误码 + 用户文案 + 日志入口；独立于 core API 可用）。
#[tauri::command]
pub fn get_core_state(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    state_to_value(&runtime.state(), &runtime.log_path())
}

/// 连接描述符：WebView 据此构造 API base 并随请求携带实例身份头。
/// 秘密（pairing）与短期 bearer token 只经本命令传给当前窗口，不落盘、不写日志。
/// §4：token 注入后，正式桌面冷启动不再需要 GET /auth/token（总请求 ≤5）。
#[tauri::command]
pub fn get_core_connection(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    match runtime.state() {
        CoreState::Ready(connection) => {
            let mut value = json!({
                "port": connection.port,
                "instanceId": connection.instance_id,
                "pairing": runtime.pairing(),
                "apiVersion": connection.api_version,
                "pid": connection.pid,
                "buildId": connection.build_id,
                // §6.1.4：壳编译期期望的 build id（owo-build-info）；WebView 诊断
                // 面板可比对 buildId != expectedBuildId 提示安装包与核心版本错配。
                "expectedBuildId": owo_build_info::COMMIT,
                "state": "ready",
            });
            if let Some(token) = runtime.bearer_token() {
                value["token"] = json!(token);
            }
            value
        }
        other => {
            let mut value = state_to_value(&other, &runtime.log_path());
            value["port"] = json!(0);
            value
        }
    }
}

/// 手动重试（诊断页「重试」按钮）：重置退避并重新拉起核心。
#[tauri::command]
pub fn retry_core_start(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    runtime.retry();
    state_to_value(&runtime.state(), &runtime.log_path())
}

/// 打开日志目录（资源管理器），供用户自助排障。
#[tauri::command]
pub fn open_core_logs(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let path = runtime.log_path();
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("explorer")
            .arg(dir.as_os_str())
            .spawn();
    }
    json!({ "opened": path.to_string_lossy() })
}

// ---- §4.6 工作区 ----

/// 当前项目工作区（与竞态无关的只读查询）：`{ workspace?: string, state }`。
#[tauri::command]
pub fn get_workspace(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let workspace = runtime
        .workspace()
        .map(|path| path.to_string_lossy().to_string());
    json!({
        "workspace": workspace,
        "state": state_to_value(&runtime.state(), &runtime.log_path())["state"].clone(),
    })
}

/// §4.6 选择项目工作区。校验 + 持久化，然后按当前运行状态受控重启：
/// 已在运行 → 代际递增重拉（新工作区生效）；未运行（NoWorkspace/Stopped/Failed）→ 直接启动。
#[tauri::command]
pub fn set_workspace(path: String, runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let result = runtime.set_workspace(std::path::Path::new(&path));
    match result {
        Ok(canonical) => {
            if runtime.is_running() {
                runtime.retry();
            } else {
                runtime.start();
            }
            json!({
                "ok": true,
                "workspace": canonical.to_string_lossy(),
                "state": state_to_value(&runtime.state(), &runtime.log_path())["state"].clone(),
            })
        }
        Err(error) => json!({ "ok": false, "error": error }),
    }
}

// ---- §4.8 提供商 ----

/// 提供商状态（§4.8：展示名称/主机/模型/联网状态，永不返回密钥）：
/// `{ provider, baseUrl, model, keyConfigured, ready }`。
#[tauri::command]
pub fn get_provider_status(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let config = runtime.provider_config();
    let status: ProviderStatus = crate::provider::provider_status(&config);
    json!({
        "provider": status.mode.as_str(),
        "baseUrl": status.base_url,
        "model": status.model,
        "keyConfigured": status.key_configured,
        "ready": status.ready,
    })
}

/// 更新提供商选择（mode: cloud|ollama|unset；baseUrl/model 可选覆盖）。
/// 保存成功后受控重启 core 使新环境生效。
#[tauri::command]
pub fn set_provider(
    mode: String,
    runtime: State<'_, std::sync::Arc<CoreRuntime>>,
    base_url: Option<String>,
    model: Option<String>,
) -> Value {
    let Some(provider_mode) = ProviderMode::parse(&mode) else {
        return json!({ "ok": false, "error": format!("未知提供商模式：{mode}") });
    };
    let mut config = match provider_mode {
        ProviderMode::Cloud => ProviderConfig::cloud(),
        ProviderMode::Ollama => ProviderConfig::ollama(),
        ProviderMode::Unset => ProviderConfig::unset(),
    };
    if let Some(value) = base_url.filter(|value| !value.trim().is_empty()) {
        config.base_url = Some(value);
    }
    if let Some(value) = model.filter(|value| !value.trim().is_empty()) {
        config.model = Some(value);
    }
    match runtime.set_provider(&config) {
        Ok(()) => {
            if runtime.is_running() {
                runtime.retry();
            } else {
                runtime.start();
            }
            let status = crate::provider::provider_status(&runtime.provider_config());
            json!({
                "ok": true,
                "provider": status.mode.as_str(),
                "baseUrl": status.base_url,
                "model": status.model,
                "keyConfigured": status.key_configured,
                "ready": status.ready,
            })
        }
        Err(error) => json!({ "ok": false, "error": error }),
    }
}
