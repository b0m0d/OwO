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
    runtime_state_value(&runtime)
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
                // §4.6：ready 时也必须带上代际，否则台账只在故障期拿得到，
                // 日常反而看不到"壳到底重拉过几次"这个权威事实。
                "generation": runtime.generation(),
            });
            if let Some(token) = runtime.bearer_token() {
                value["token"] = json!(token);
            }
            value
        }
        _ => {
            let mut value = runtime_state_value(&runtime);
            value["port"] = json!(0);
            value
        }
    }
}

/// 手动重试（诊断页「重试」按钮）：重置退避并重新拉起核心。
#[tauri::command]
pub fn retry_core_start(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    runtime.retry();
    runtime_state_value(&runtime)
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

/// §4.6 工作区回执的唯一形状来源：手输路径（引导页兼容）与原生选择器
/// **必须返回同一个形状**，否则前端要为两个入口写两套判定（迟早漂移）。
/// `generation` 是壳侧重启口径的权威字段（§4.6 台账），重启之后取值才是新代际。
fn workspace_receipt(runtime: &CoreRuntime, canonical: &std::path::Path) -> Value {
    json!({
        "ok": true,
        "workspace": canonical.to_string_lossy(),
        "state": state_to_value(&runtime.state(), &runtime.log_path())["state"].clone(),
        "generation": runtime.generation(),
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
            workspace_receipt(runtime.inner().as_ref(), &canonical)
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

/// §3.4/§4.7 动作 `choose_data_directory`：存储错误（storage/not_writable）后
/// 用原生目录对话框改选数据根——写指针后受控重启核心。只改目录选择，不触碰凭据。
#[tauri::command]
pub async fn choose_data_directory(
    runtime: State<'_, std::sync::Arc<CoreRuntime>>,
) -> Result<Value, String> {
    // rfd 异步对话框：不阻塞 IPC 线程；用户取消也是合法终态（ok=false）。
    let picked = rfd::AsyncFileDialog::new()
        .set_title("选择新的数据目录")
        .pick_folder()
        .await;
    let Some(folder) = picked else {
        return Ok(json!({ "ok": false, "canceled": true }));
    };
    let path = folder.path().to_path_buf();
    match crate::core_runtime::save_data_root_override(&path) {
        Ok(canonical) => {
            runtime.retry();
            Ok(json!({
                "ok": true,
                "data_root": canonical.to_string_lossy(),
                "state": state_to_value(&runtime.state(), &runtime.log_path())["state"].clone(),
            }))
        }
        Err(error) => Ok(json!({ "ok": false, "error": error })),
    }
}

/// §4.4 表单控件规范：文件夹必须由 **Tauri 原生目录选择器**选，禁止要求用户手输完整路径。
/// 与 `set_workspace` 同一套校验/持久化/受控重启语义（`set_workspace` 保留给
/// 引导页里"已知路径"的程序化设置，不作为日常输入口）。取消是合法终态，不报错。
#[tauri::command]
pub async fn choose_project_directory(
    runtime: State<'_, std::sync::Arc<CoreRuntime>>,
) -> Result<Value, String> {
    let picked = rfd::AsyncFileDialog::new()
        .set_title("选择项目工作区")
        .pick_folder()
        .await;
    let Some(folder) = picked else {
        return Ok(json!({ "ok": false, "canceled": true }));
    };
    let path = folder.path().to_path_buf();
    match runtime.set_workspace(path.as_path()) {
        Ok(canonical) => {
            if runtime.is_running() {
                runtime.retry();
            } else {
                runtime.start();
            }
            Ok(workspace_receipt(runtime.inner().as_ref(), &canonical))
        }
        Err(error) => Ok(json!({ "ok": false, "error": error })),
    }
}

/// R3-B（§3.4 终态可见性）：带"最近一次失败"的完整诊断状态对象。
/// `CoreState::Starting/Restarting` 本身不含稳定码（重试窗口内还没有新事实），
/// 但上一代的故障码必须继续可见——否则 UI 在退避期间只能渲染默认三出口 + 通用文案，
/// 而 §3.4 要求"按 code 渲染动作"。ready 态不注入（不得携带陈旧故障）。
fn runtime_state_value(runtime: &CoreRuntime) -> Value {
    let state = runtime.state();
    let mut value = state_to_value(&state, &runtime.log_path());
    // §4.6 诊断台账的「最近一次 core 重启」需要壳侧权威计数：代际（手动重连/换目录
    // 才 +1）与当代 attempt（崩溃自动重启）是两个不同事实，分开给，UI 不得混称。
    value["generation"] = json!(runtime.generation());
    let state_name = value.get("state").and_then(Value::as_str).unwrap_or("");
    if !matches!(state_name, "ready" | "failed") {
        if let Some((code, message)) = runtime.last_error() {
            value["errorCode"] = json!(code);
            value["message"] = json!(message);
        }
    }
    value
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_runtime::{CoreConnection, CoreState};

    fn fresh_runtime() -> std::sync::Arc<CoreRuntime> {
        std::sync::Arc::new(CoreRuntime::new_with_workspace(
            "pairing-under-test".into(),
            "instance-under-test".into(),
            None,
        ))
    }

    #[test]
    fn runtime_state_value_always_exposes_generation() {
        // §4.6：诊断台账读的是 `payload.generation`。缺这个字段 = UI 只能说
        // "壳未上报"，与接旧壳无异；所以字段存在性本身是契约，不靠肉眼对齐。
        let runtime = fresh_runtime();
        let value = runtime_state_value(&runtime);
        assert_eq!(value["state"].as_str(), Some("starting"));
        assert!(
            value["generation"].is_number(),
            "generation 必须在 payload 里，不得只在部分状态出现：{value}"
        );
        assert_eq!(value["generation"].as_u64(), Some(runtime.generation()));
        runtime.retry();
        assert_eq!(
            runtime_state_value(&runtime)["generation"].as_u64(),
            Some(1),
            "手动重连后 payload 必须跟着换代走"
        );
    }

    #[test]
    fn workspace_receipt_shape_is_shared_by_both_entry_points() {
        // 手输路径（引导页兼容）与原生选择器必须回同一个形状，
        // 否则前端要为两个入口写两套判定（迟早漂移）。
        let runtime = fresh_runtime();
        let path = std::path::Path::new("T:/owo-receipt-check");
        let value = workspace_receipt(&runtime, path);
        assert_eq!(value["ok"].as_bool(), Some(true));
        assert_eq!(value["workspace"].as_str(), Some("T:/owo-receipt-check"));
        assert_eq!(value["state"].as_str(), Some("starting"));
        assert!(value["generation"].is_number(), "回执必须带代际：{value}");
    }

    #[test]
    fn ready_descriptor_carries_identity_and_generation_but_stays_secrets_only_over_ipc() {
        // ready 描述符是 WebView 唯一的连接/身份来源：字段名即前端契约
        // （api-client.js 逐字读取），改名等于断链，故在此钉死形状。
        let runtime = fresh_runtime();
        *runtime.state_for_test().lock().unwrap() = CoreState::Ready(CoreConnection {
            pid: 4242,
            port: 17319,
            api_version: "0.7".into(),
            build_id: "build-under-test".into(),
            instance_id: "instance-under-test".into(),
        });
        let value = state_to_value(&runtime.state(), &runtime.log_path());
        assert_eq!(value["state"].as_str(), Some("ready"));
        assert_eq!(value["port"].as_u64(), Some(17319));
        assert_eq!(value["pid"].as_u64(), Some(4242));
        assert_eq!(value["instanceId"].as_str(), Some("instance-under-test"));
        assert_eq!(value["buildId"].as_str(), Some("build-under-test"));
        // pairing/token 由命令层另行注入，不在状态映射里（防止被日志/台账顺手带走）。
        assert!(value.get("pairing").is_none(), "状态映射不得携带 pairing");
        assert!(
            value.get("token").is_none(),
            "状态映射不得携带 bearer token"
        );
    }

    #[test]
    fn no_workspace_is_a_configuration_state_not_a_failure() {
        // §4.6：没有工作区是**待配置**，UI 据此渲染「选择目录」而非「重试」；
        // 因此它既不能挂错误码，也不能被当成 Failed 进入重试退避。
        let runtime = fresh_runtime();
        runtime.start();
        let value = runtime_state_value(&runtime);
        assert_eq!(value["state"].as_str(), Some("no_workspace"));
        assert!(
            value.get("errorCode").is_none(),
            "待配置态不得凭空出现故障码：{value}"
        );
    }
}
