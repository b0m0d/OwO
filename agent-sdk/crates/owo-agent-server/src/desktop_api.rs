//! §12 桌面/视觉动作域（desktop + vision）API 模块。
//!
//! 提取证明：处理器与结构自 `lib.rs` 第 1896–2309 行逐字迁移
//! （desktop_foreground/windows/activate/click/type/key/shortcut/launch/scroll/wait +
//! vision_status/describe/verify/ground，9 请求结构 + SensitiveProbe），
//! 另迁移共享门禁 `gate_desktop_action` 与模拟面禁用 `ensure_real_desktop`
//! （均 `pub(super)`，供 crate 根与 turn 编排器后续复用）。
//! 感知层授权复用 `perception_api::require_perception_layer`（上轮已外移）。
//! 路由路径与 OpenAPI 登记零变化。`poison` 为模块本地副本（签名与 lib 根一致）。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

fn ensure_real_desktop(tool: &str) -> Result<(), (StatusCode, String)> {
    if std::env::var("OWO_SIM_QQ_URL")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{tool} 在模拟环境下被禁用：请直连模拟服务或通过 Agent 工具执行",),
        ));
    }
    Ok(())
}

/// 可选 task_id 门禁：请求携带 task_id 时，动作执行前先过 computer-use 门禁
/// （状态/超时/允许集/目标应用/敏感熔断/预算），拒绝返回 403 并写审计。
fn gate_desktop_action(
    state: &owo_agent_server::AppState,
    task_id: &Option<String>,
    action: &str,
    probe: &Option<SensitiveProbe>,
) -> Result<(), (StatusCode, String)> {
    let Some(task_id) = task_id else {
        return Ok(());
    };
    let app = owo_agent_core::platform::poll_foreground_app()
        .map(|(app_id, _)| app_id)
        .unwrap_or_default();
    let sensitive = probe
        .as_ref()
        .map(|p| (p.name.as_str(), p.role.as_str(), p.ocr_text.as_str()));
    let audit = state.agent.audit_log();
    let mut log = audit
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "审计锁中毒".to_string()))?;
    owo_agent_core::computer_use::task_gate_check(
        &state.computer_tasks,
        Some(&mut log),
        "computer-use",
        task_id,
        action,
        &app,
        sensitive,
    )
    .map_err(|error| (StatusCode::FORBIDDEN, error))
}

#[derive(serde::Deserialize)]
pub(super) struct SensitiveProbe {
    #[serde(default)]
    name: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    ocr_text: String,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopClickRequest {
    x: i32,
    y: i32,
    /// 可选：computer-use 任务 id，提供时动作先过门禁（未批准/越界应用/敏感熔断拒绝）。
    #[serde(default)]
    task_id: Option<String>,
    /// 可选：敏感 UI 探针（UI 属性/名称/OCR 关键词），门禁内熔断判定。
    #[serde(default)]
    sensitive: Option<SensitiveProbe>,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopTextRequest {
    text: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    sensitive: Option<SensitiveProbe>,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopKeyRequest {
    key: String,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    sensitive: Option<SensitiveProbe>,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopComboRequest {
    combo: String,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopTargetRequest {
    target: String,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopScrollRequest {
    x: i32,
    y: i32,
    delta: i32,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    sensitive: Option<SensitiveProbe>,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopActivateRequest {
    #[serde(default)]
    process: String,
    #[serde(default)]
    title: String,
}

#[derive(serde::Deserialize)]
pub(super) struct DesktopWaitRequest {
    ms: u64,
}

pub(super) async fn desktop_foreground() -> Json<Value> {
    if std::env::var("OWO_SIM_QQ_URL")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return Json(json!({
            "process": "owo-sim-qq",
            "title": "OwO 模拟QQ - 张子豪",
            "rect": [0, 0, 1020, 700],
            "surface": "sim",
        }));
    }
    let (process, title) = owo_agent_core::poll_foreground_app().unwrap_or_default();
    let rect = owo_agent_core::platform::foreground_window_rect();
    Json(json!({ "process": process, "title": title, "rect": rect }))
}

pub(super) async fn desktop_windows() -> Json<Value> {
    if std::env::var("OWO_SIM_QQ_URL")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return Json(json!({
            "windows": [{
                "hwnd": 1,
                "pid": 1,
                "process": "owo-sim-qq",
                "title": "OwO 模拟QQ - 张子豪",
                "rect": [0, 0, 1020, 700],
                "visible": true,
            }],
            "surface": "sim",
        }));
    }
    Json(json!({ "windows": owo_agent_core::platform::window_list() }))
}

pub(super) async fn desktop_activate(
    Json(request): Json<DesktopActivateRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_activate")?;
    owo_agent_core::platform::activate_window(&request.process, &request.title)
        .map(|_| Json(json!({ "ok": true })))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_click(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<DesktopClickRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_click")?;
    gate_desktop_action(
        &state,
        &request.task_id,
        "desktop_click",
        &request.sensitive,
    )?;
    owo_agent_core::computer_use::desktop_click(request.x, request.y)
        .map(|_| Json(json!({ "ok": true, "x": request.x, "y": request.y })))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_type(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<DesktopTextRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_type")?;
    gate_desktop_action(&state, &request.task_id, "desktop_type", &request.sensitive)?;
    owo_agent_core::computer_use::desktop_type(&request.text)
        .map(|_| Json(json!({ "ok": true, "typed_chars": request.text.chars().count() })))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_key(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<DesktopKeyRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_key")?;
    gate_desktop_action(&state, &request.task_id, "desktop_key", &request.sensitive)?;
    owo_agent_core::computer_use::desktop_key(&request.key)
        .map(|_| Json(json!({ "ok": true, "key": request.key })))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_shortcut(
    Json(request): Json<DesktopComboRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_shortcut")?;
    owo_agent_core::computer_use::desktop_shortcut(&request.combo)
        .map(|_| Json(json!({ "ok": true, "combo": request.combo })))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_launch(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<DesktopTargetRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_launch")?;
    gate_desktop_action(&state, &request.task_id, "desktop_launch", &None)?;
    owo_agent_core::computer_use::desktop_launch(&request.target)
        .map(|_| Json(json!({ "ok": true, "target": request.target })))
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_scroll(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<DesktopScrollRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    ensure_real_desktop("desktop_scroll")?;
    gate_desktop_action(
        &state,
        &request.task_id,
        "desktop_scroll",
        &request.sensitive,
    )?;
    owo_agent_core::computer_use::desktop_scroll(request.x, request.y, request.delta)
        .map(|_| {
            Json(json!({ "ok": true, "x": request.x, "y": request.y, "delta": request.delta }))
        })
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn desktop_wait(Json(request): Json<DesktopWaitRequest>) -> Json<Value> {
    let ms = request.ms.min(120_000);
    tokio::time::sleep(Duration::from_millis(ms)).await;
    Json(json!({ "waited_ms": ms }))
}

pub(super) async fn vision_status() -> Json<Value> {
    let config = owo_agent_core::VisionConfig::from_env();
    let models = if config.provider == "ollama" {
        owo_agent_core::ollama_models(&config).await
    } else {
        Vec::new()
    };
    Json(json!({
        "provider": config.provider,
        "model": config.model,
        "ollama_host": config.ollama_host,
        "ollama_models": models,
    }))
}

#[derive(serde::Deserialize)]
pub(super) struct VisionDescribeRequest {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    #[serde(default)]
    scale: Option<u32>,
}

pub(super) async fn vision_describe(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<VisionDescribeRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::perception_api::require_perception_layer(
        &state,
        owo_agent_core::PerceptionLayer::L2Visual,
    )?;
    let (png, surface) = match (request.x, request.y, request.width, request.height) {
        (Some(x), Some(y), Some(width), Some(height)) => owo_agent_core::capture_vision_png_region(
            x,
            y,
            width,
            height,
            request.scale.unwrap_or(3),
        )
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?,
        _ => owo_agent_core::capture_vision_png()
            .await
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?,
    };
    let prompt = request.prompt.unwrap_or_else(|| {
        "请用中文描述这个界面的当前状态：这是什么应用？有哪些关键控件（按钮/输入框/消息）？\
         它们大致在什么位置？最新消息内容是什么？"
            .to_string()
    });
    let description = owo_agent_core::describe_image(&png, &prompt)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let config = owo_agent_core::VisionConfig::from_env();
    Ok(Json(json!({
        "surface": surface,
        "provider": config.provider,
        "model": config.model,
        "description": description,
    })))
}

#[derive(serde::Deserialize)]
pub(super) struct VisionVerifyRequest {
    question: String,
    /// 是否忽略输入框占位文字（默认 true）。
    #[serde(default = "crate::perception_api::default_true")]
    ignore_placeholder: bool,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
    #[serde(default)]
    scale: Option<u32>,
}

/// 视觉完成验证：对当前截图回答 yes/no 问题，返回 answer + confidence。
pub(super) async fn vision_verify(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<VisionVerifyRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::perception_api::require_perception_layer(
        &state,
        owo_agent_core::PerceptionLayer::L2Visual,
    )?;
    let (png, surface) = match (request.x, request.y, request.width, request.height) {
        (Some(x), Some(y), Some(width), Some(height)) => owo_agent_core::capture_vision_png_region(
            x,
            y,
            width,
            height,
            request.scale.unwrap_or(3),
        )
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?,
        _ => owo_agent_core::capture_vision_png()
            .await
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?,
    };
    let prompt = owo_agent_core::verification_prompt(&request.question, request.ignore_placeholder);
    let raw = owo_agent_core::describe_image(&png, &prompt)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let (answer, confidence) = owo_agent_core::parse_verification(&raw);
    let config = owo_agent_core::VisionConfig::from_env();
    Ok(Json(json!({
        "surface": surface,
        "provider": config.provider,
        "model": config.model,
        "question": request.question,
        "answer": answer,
        "confidence": confidence,
        "raw": raw,
    })))
}

#[derive(serde::Deserialize)]
pub(super) struct VisionGroundRequest {
    description: String,
    /// 可选：应用标识，提供时 grounding 结果写入窗口元素注册表并返回 element_id。
    #[serde(default)]
    app_id: Option<String>,
}

/// 视觉 grounding：视觉模型给框 → 与 OCR 文本交叉验证；
/// 无 OCR 文本时仅高置信度（≥0.9）标记 vision_only 允许纯视觉定位。
pub(super) async fn vision_ground(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<VisionGroundRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::perception_api::require_perception_layer(
        &state,
        owo_agent_core::PerceptionLayer::L2Visual,
    )?;
    let mut result = owo_agent_core::ground_element(&request.description)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    if let Some(app_id) = request.app_id {
        if result
            .get("matched")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let grounding = owo_agent_core::computer_use::vision_grounding_from_value(
                &result,
                &request.description,
            )
            .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
            let mut registry = state.elements.lock().map_err(poison)?;
            if let Some(element_id) =
                owo_agent_core::register_vision_grounding(&mut registry, &app_id, grounding)
            {
                result["element_id"] = serde_json::json!(element_id);
                result["app_id"] = serde_json::json!(app_id);
            }
        }
    }
    Ok(Json(result))
}
