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
        CoreState::Stopped => json!({ "state": "stopped" }),
    }
}

/// 启动诊断状态（§4.3：错误码 + 用户文案 + 日志入口；独立于 core API 可用）。
#[tauri::command]
pub fn get_core_state(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    state_to_value(&runtime.state(), &runtime.log_path())
}

/// 连接描述符：WebView 据此构造 API base 并随请求携带实例身份头。
/// 秘密（pairing）只经本命令传给当前窗口，不落盘、不写日志。
#[tauri::command]
pub fn get_core_connection(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    match runtime.state() {
        CoreState::Ready(connection) => json!({
            "port": connection.port,
            "instanceId": connection.instance_id,
            "pairing": runtime.pairing(),
            "apiVersion": connection.api_version,
            "pid": connection.pid,
            "buildId": connection.build_id,
            "state": "ready",
        }),
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
