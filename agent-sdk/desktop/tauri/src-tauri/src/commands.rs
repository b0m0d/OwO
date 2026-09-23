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
use crate::provider::{ProviderMode, ProviderStatus};

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
/// `explorer` 是控制台无关的 GUI 程序，但仍显式加 `CREATE_NO_WINDOW`：
/// 否则从桌面壳（GUI 子系统）里 spawn 会在个别环境下带出一个瞬态控制台黑框。
#[tauri::command]
pub fn open_core_logs(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let path = runtime.log_path();
    if let Some(dir) = path.parent() {
        let mut command = std::process::Command::new("explorer");
        command.arg(dir.as_os_str());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let _ = command.spawn();
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

/// R11：在资源管理器里定位模型配置文件（用户要手改 `config.json` 时少找半天路径）。
/// 用 `explorer /select,` 高亮文件本身；不带文件时退化为打开所在目录。
#[tauri::command]
pub fn reveal_model_config() -> Value {
    let Some(path) = crate::provider::config_path() else {
        return json!({ "ok": false, "error": "无法确定配置路径" });
    };
    if !path.is_file() {
        // 文件还没生成（用户没保存过配置）：至少把目录打开，并说明情况。
        if let Some(dir) = path.parent() {
            let _ = crate::commands::open_path_in_explorer(dir, false);
            return json!({
                "ok": true,
                "created": false,
                "path": path.to_string_lossy(),
                "detail": "配置文件尚未生成：在设置页点一次「保存并重启核心」即可创建",
            });
        }
        return json!({ "ok": false, "error": "配置目录不存在" });
    }
    let opened = crate::commands::open_path_in_explorer(&path, true);
    json!({
        "ok": opened,
        "created": true,
        "path": path.to_string_lossy(),
    })
}

/// 打开目录/定位文件（`create_no_window` 避免 GUI 壳里弹瞬态控制台）。
pub(crate) fn open_path_in_explorer(path: &std::path::Path, select: bool) -> bool {
    let mut command = std::process::Command::new("explorer");
    if select {
        // explorer 的 /select 参数要求 `/select,<path>` 这种单参数形式。
        command.arg(format!("/select,{}", path.display()));
    } else {
        command.arg(path.as_os_str());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    command.spawn().is_ok()
}

// ---- §4.8 / R11 模型配置（独立 config.json） ----

/// 模型配置状态（§4.8：展示提供方/端点/模型名/凭据来源，**永不返回密钥本体**）：
/// `{ provider, baseUrl, model, keyConfigured, keySource, keyMasked, keyEnv, ready, configPath }`。
#[tauri::command]
pub fn get_provider_status(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let model = runtime.model_config();
    let status: ProviderStatus = crate::provider::provider_status(&model);
    json!({
        "provider": status.provider,
        "baseUrl": status.base_url,
        "model": status.model,
        "keyConfigured": status.key_configured,
        // 密钥只回来源与掩码：前端据此显示"已配置（来自配置文件 sk-a…mnop）"，
        // 但拿不到可用凭据（浏览器侧/XSS 都偷不走）。
        "keySource": status.key_source,
        "keyMasked": status.key_masked,
        "keyEnv": status.key_env,
        "ready": status.ready,
        "configPath": status.config_path,
        // 用户维护的模型清单 + 可调参数（界面据此建议/回填；None 表示用核心默认）。
        "models": status.models,
        "contextWindow": status.context_window,
        "maxOutputTokens": status.max_output_tokens,
        "temperature": status.temperature,
        "timeoutSecs": status.timeout_secs,
        "keepRecent": status.keep_recent,
        "compaction": status.compaction,
    })
}

/// R11：保存模型配置到独立配置文件（codex/opencode 风格的 `config.json`）。
///
/// 参数语义（关键：空值 ≠ 清空，避免前端"没传字段"把用户已存的密钥抹掉）：
/// - `mode`：提供方；`base_url` / `model`：地址与模型名（空白视作"用默认"）；
/// - `api_key`：`Some("")` 显式清空；`Some("sk-…")` 覆盖；`None` 保持原样；
/// - `api_key_env`：同上（指向环境变量的名字）。
/// 保存成功后受控重启核心，让新配置经环境变量注入生效。
///
/// 可调参数（context_window / max_output_tokens / temperature / timeout_secs /
/// keep_recent / compaction）全部**可选**：`None` = 保持文件里现有值；`Some("")`
/// 或 `Some(0)` = 清除该字段（回到核心默认）。界面与手改文件因此共用同一份语义。
#[tauri::command]
pub fn set_model_config(
    mode: String,
    runtime: State<'_, std::sync::Arc<CoreRuntime>>,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    context_window: Option<String>,
    max_output_tokens: Option<String>,
    temperature: Option<String>,
    timeout_secs: Option<String>,
    keep_recent: Option<String>,
    compaction: Option<String>,
    models: Option<Vec<String>>,
) -> Value {
    let Some(provider_mode) = ProviderMode::parse(&mode) else {
        return json!({ "ok": false, "error": format!("未知模型提供方：{mode}") });
    };
    // 读改写：保住文件里的未知字段与未提交字段（用户可能手写过注释性字段）。
    let mut config = runtime.model_config();
    config.provider = provider_mode;
    config.base_url = base_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    config.name = model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(key) = api_key {
        let key = key.trim().to_string();
        config.api_key = if key.is_empty() { None } else { Some(key) };
    }
    if let Some(env_name) = api_key_env {
        let env_name = env_name.trim().to_string();
        config.api_key_env = if env_name.is_empty() { None } else { Some(env_name) };
    }
    // 数值型可调参数：空串/0/非法值 = 清除（回到核心默认），而不是写一个会坏事的值。
    fn parse_positive(value: Option<String>) -> Option<u64> {
        value
            .map(|raw| raw.trim().to_string())
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|parsed| *parsed > 0)
    }
    if let Some(raw) = context_window.as_ref() {
        config.context_window = parse_positive(Some(raw.clone()));
    }
    if let Some(raw) = max_output_tokens.as_ref() {
        config.max_output_tokens = parse_positive(Some(raw.clone()));
    }
    if let Some(raw) = timeout_secs.as_ref() {
        config.timeout_secs = parse_positive(Some(raw.clone()));
    }
    if let Some(raw) = keep_recent.as_ref() {
        config.keep_recent = parse_positive(Some(raw.clone()));
    }
    if let Some(raw) = temperature.as_ref() {
        config.temperature = raw
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && (0.0..=2.0).contains(value));
    }
    if let Some(raw) = compaction.as_ref() {
        config.compaction = match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        };
    }
    if let Some(list) = models {
        // 去重保序；空串丢弃（手改文件时常见的尾随逗号/空行产物）。
        let mut cleaned: Vec<String> = Vec::new();
        for item in list {
            let name = item.trim().to_string();
            if !name.is_empty() && !cleaned.contains(&name) {
                cleaned.push(name);
            }
        }
        config.models = cleaned;
    }
    if let Err(error) = crate::provider::validate(&config) {
        return json!({ "ok": false, "error": error });
    }
    match runtime.set_model_config(&config) {
        Ok(()) => {
            if runtime.is_running() {
                runtime.retry();
            } else {
                runtime.start();
            }
            let status = crate::provider::provider_status(&runtime.model_config());
            json!({
                "ok": true,
                "provider": status.provider,
                "baseUrl": status.base_url,
                "model": status.model,
                "keyConfigured": status.key_configured,
                "keySource": status.key_source,
                "keyMasked": status.key_masked,
                "keyEnv": status.key_env,
                "ready": status.ready,
                "configPath": status.config_path,
                "models": status.models,
                "contextWindow": status.context_window,
                "maxOutputTokens": status.max_output_tokens,
                "temperature": status.temperature,
                "timeoutSecs": status.timeout_secs,
                "keepRecent": status.keep_recent,
                "compaction": status.compaction,
            })
        }
        Err(error) => json!({ "ok": false, "error": error }),
    }
}

/// 从磁盘**重新读取** `config.json` 并生效（不重启应用）。
///
/// 用户的核心诉求是"全都通过文件随时更改"：手改文件之后不该被迫重开应用、
/// 更不该重新编译。这条命令把"改文件 → 点一下 → 生效"闭环补齐：
/// 重新解析文件 → 更新壳内内存配置 → 受控重启核心（新环境变量随之注入）。
#[tauri::command]
pub fn reload_model_config(runtime: State<'_, std::sync::Arc<CoreRuntime>>) -> Value {
    let path = crate::provider::config_path();
    let file = crate::provider::load_config();
    if let Some(path) = path.as_ref() {
        if !path.is_file() {
            return json!({
                "ok": false,
                "error": format!("配置文件不存在：{}（先在设置页保存一次即可创建）", path.display()),
                "configPath": path.to_string_lossy(),
            });
        }
    }
    let model = file.model.clone();
    if let Err(error) = runtime.set_model_config(&model) {
        return json!({ "ok": false, "error": error });
    }
    if runtime.is_running() {
        runtime.retry();
    } else {
        runtime.start();
    }
    let status = crate::provider::provider_status(&runtime.model_config());
    json!({
        "ok": true,
        "reloaded": true,
        "configPath": status.config_path,
        "provider": status.provider,
        "baseUrl": status.base_url,
        "model": status.model,
        "keyConfigured": status.key_configured,
        "keySource": status.key_source,
        "keyMasked": status.key_masked,
        "keyEnv": status.key_env,
        "ready": status.ready,
        "models": status.models,
        "contextWindow": status.context_window,
        "maxOutputTokens": status.max_output_tokens,
        "temperature": status.temperature,
        "timeoutSecs": status.timeout_secs,
        "keepRecent": status.keep_recent,
        "compaction": status.compaction,
    })
}

/// 兼容旧调用点（引导页/旧前端）：语义等价于 `set_model_config`，但仅传
/// mode/base_url/model —— 密钥字段一律不动（旧前端没有密钥输入框）。
#[tauri::command]
pub fn set_provider(
    mode: String,
    runtime: State<'_, std::sync::Arc<CoreRuntime>>,
    base_url: Option<String>,
    model: Option<String>,
) -> Value {
    set_model_config(mode, runtime, base_url, model, None, None, None, None, None, None, None, None, None)
}


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
