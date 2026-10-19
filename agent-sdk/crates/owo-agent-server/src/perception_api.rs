//! §12 感知域（perception）API 模块。
//!
//! 提取证明：处理器与结构自 `lib.rs` 第 1877–2255 行逐字迁移
//! （perception_events/capture/layers/tree/template_build/template_get/
//! template_detect/template_build_ocr/template_detect_ocr/elements/ocr/
//! ocr_bytes/ocr_region/window + ocr_status；9 请求结构 + 4 serde 默认助手），
//! 另迁移 `require_perception_layer`（原 lib.rs 2884–2894 行，视觉/桌面域共用，
//! `pub(super)` 供 crate 根与其余域模块调用）。路由路径与 OpenAPI 登记零变化。
//! `poison` 为模块本地副本（签名与 lib 根一致，非引用）。

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

pub(super) fn require_perception_layer(
    state: &owo_agent_server::AppState,
    layer: owo_agent_core::PerceptionLayer,
) -> Result<(), (StatusCode, String)> {
    let perception = state.perception.lock().map_err(poison)?;
    if perception.is_enabled(layer) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, format!("感知层未授权：{layer:?}")))
    }
}

/// perception.subscribe：订阅 L0/L1 事件流（SSE），桌面端感知状态区使用。
pub(super) async fn perception_events(
    State(state): State<Arc<owo_agent_server::AppState>>,
) -> Result<Sse<ReceiverStream<Result<Event, Infallible>>>, (StatusCode, String)> {
    let mut perception = state.perception.lock().map_err(poison)?;
    let _ = perception.refresh_from_platform();
    let mut receiver = perception.subscribe();
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
            if tx
                .send(Ok(Event::default().event("perception").data(data)))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(rx)))
}

/// L2 按需采集：截图 + 本地 OCR 摘要进内存环形缓冲（不落盘）。
pub(super) async fn perception_capture(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<CaptureRequest>,
) -> Result<Json<owo_agent_core::CaptureMeta>, (StatusCode, String)> {
    let mut perception = state.perception.lock().map_err(poison)?;
    let frame = match (request.width, request.height) {
        (Some(width), Some(height)) => perception
            .begin_capture_region(width, height)
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?,
        _ => perception
            .begin_capture_from_screen()
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?,
    };
    Ok(Json(frame))
}

#[derive(serde::Deserialize)]
pub(super) struct CaptureRequest {
    #[serde(default)]
    width: Option<i32>,
    #[serde(default)]
    height: Option<i32>,
}

#[derive(serde::Deserialize)]
pub(super) struct LayersRequest {
    layer: String,
    enabled: bool,
}

/// 感知层级授权开关（L0-L3 逐项授权，可热撤）。
pub(super) async fn perception_layers(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<LayersRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    use owo_agent_core::PerceptionLayer;
    let layer = match request.layer.as_str() {
        "l0_event" => PerceptionLayer::L0Event,
        "l1_ui" => PerceptionLayer::L1Ui,
        "l2_visual" => PerceptionLayer::L2Visual,
        "l3_semantic" => PerceptionLayer::L3Semantic,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("未知感知层：{other}（l0_event/l1_ui/l2_visual/l3_semantic）"),
            ));
        }
    };
    let mut perception = state.perception.lock().map_err(poison)?;
    perception.set_layer_enabled(layer, request.enabled);
    Ok(Json(
        json!({ "layer": request.layer, "enabled": request.enabled }),
    ))
}

#[derive(serde::Deserialize)]
pub(super) struct TreeDumpRequest {
    #[serde(default = "default_tree_depth")]
    max_depth: u32,
    #[serde(default = "default_tree_nodes")]
    max_nodes: usize,
    /// 可选：按窗口句柄抓树（不要求前台），用于窗口模板/后台情景理解。
    #[serde(default)]
    hwnd: Option<i64>,
}

fn default_tree_depth() -> u32 {
    12
}

fn default_tree_nodes() -> usize {
    1000
}

/// 深度 UI 树转储（computer-use 调试：找深层语义锚点，如 QQ 工具栏按钮）。
pub(super) async fn perception_tree(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<TreeDumpRequest>,
) -> Result<Json<Vec<owo_agent_core::UiNode>>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L1Ui)?;
    let tree = match request.hwnd {
        Some(hwnd) => {
            owo_agent_core::ui_tree_for_hwnd(hwnd as isize, request.max_depth, request.max_nodes)
        }
        None => owo_agent_core::foreground_ui_tree(request.max_depth, request.max_nodes),
    };
    tree.map(Json)
        .ok_or((StatusCode::BAD_REQUEST, "无法获取 UI 树".to_string()))
}

#[derive(serde::Deserialize)]
pub(super) struct TemplateBuildRequest {
    hwnd: i64,
    app_id: String,
}

pub(super) async fn perception_template_build(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<TemplateBuildRequest>,
) -> Result<Json<owo_agent_core::WindowTemplate>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L1Ui)?;
    let tree = owo_agent_core::ui_tree_for_hwnd(request.hwnd as isize, 14, 10000)
        .ok_or((StatusCode::BAD_REQUEST, "无法获取窗口 UI 树".to_string()))?;
    let template = owo_agent_core::build_template(&request.app_id, &tree);
    owo_agent_core::save_template(&state.data_root, &template)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "template",
            "build",
            Some(request.app_id.clone()),
            Some(true),
            format!(
                "构建窗口模板：{}（{} 个 ROI）",
                request.app_id,
                template.rois.len()
            ),
        );
    }
    Ok(Json(template))
}

pub(super) async fn perception_template_get(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(app_id): AxumPath<String>,
) -> Result<Json<owo_agent_core::WindowTemplate>, (StatusCode, String)> {
    owo_agent_core::load_template(&state.data_root, &app_id)
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, format!("窗口模板不存在：{app_id}")))
}

#[derive(serde::Deserialize)]
pub(super) struct TemplateDetectRequest {
    hwnd: i64,
    app_id: String,
}

pub(super) async fn perception_template_detect(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<TemplateDetectRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L1Ui)?;
    let template = owo_agent_core::load_template(&state.data_root, &request.app_id).ok_or((
        StatusCode::NOT_FOUND,
        format!("窗口模板不存在：{}", request.app_id),
    ))?;
    let tree = owo_agent_core::ui_tree_for_hwnd(request.hwnd as isize, 14, 10000)
        .ok_or((StatusCode::BAD_REQUEST, "无法获取窗口 UI 树".to_string()))?;
    Ok(Json(owo_agent_core::detect_template(&template, &tree)))
}

/// OCR 版模板构建：PrintWindow 抓窗口 → PP-OCRv6 → 按语义文本提取 ROI（后台可用）。
pub(super) async fn perception_template_build_ocr(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<TemplateBuildRequest>,
) -> Result<Json<owo_agent_core::WindowTemplate>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L2Visual)?;
    let (bmp, _rect) = owo_agent_core::platform::capture_window_bmp_deep(request.hwnd as isize)
        .ok_or((StatusCode::BAD_REQUEST, "窗口截图失败".to_string()))?;
    let summary = owo_agent_core::ocr_preferred(&bmp)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let template = owo_agent_core::build_template_from_ocr(&request.app_id, &summary);
    owo_agent_core::save_template(&state.data_root, &template)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(Json(template))
}

/// OCR 版模板检测：当前窗口 OCR 行中心 vs 模板 ROI 命中率。
pub(super) async fn perception_template_detect_ocr(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<TemplateDetectRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L2Visual)?;
    let template = owo_agent_core::load_template(&state.data_root, &request.app_id).ok_or((
        StatusCode::NOT_FOUND,
        format!("窗口模板不存在：{}", request.app_id),
    ))?;
    let (bmp, _rect) = owo_agent_core::platform::capture_window_bmp_deep(request.hwnd as isize)
        .ok_or((StatusCode::BAD_REQUEST, "窗口截图失败".to_string()))?;
    let summary = owo_agent_core::ocr_preferred(&bmp)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    Ok(Json(owo_agent_core::detect_template_ocr(
        &template, &summary,
    )))
}

#[derive(serde::Deserialize)]
pub(super) struct ElementsRequest {
    hwnd: i64,
    app_id: String,
    /// 可选视觉 grounding 结果（vision_ground 的 box + 描述），并入同一注册表。
    #[serde(default)]
    vision: Vec<owo_agent_core::VisionGrounding>,
}

/// 窗口元素注册表：UIA 树 + 窗口 OCR（转屏幕坐标）融合 → 注册表更新 → 返回稳定元素列表。
pub(super) async fn perception_elements(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<ElementsRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L1Ui)?;
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L2Visual)?;
    let tree =
        owo_agent_core::ui_tree_for_hwnd(request.hwnd as isize, 14, 10000).unwrap_or_default();
    let (bmp, rect) = owo_agent_core::platform::capture_window_bmp_deep(request.hwnd as isize)
        .ok_or((StatusCode::BAD_REQUEST, "窗口截图失败".to_string()))?;
    let summary = owo_agent_core::ocr_preferred(&bmp)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let mut lines = owo_agent_core::group_ocr_lines(&summary.boxes);
    for line in &mut lines {
        line.x += rect.0;
        line.y += rect.1;
    }
    let fused = owo_agent_core::fuse_sources_with_vision(&tree, &lines, &request.vision);
    let mut registry = state.elements.lock().map_err(poison)?;
    let elements = registry.update(&request.app_id, fused);
    Ok(Json(json!({
        "app_id": request.app_id,
        "provider": summary.provider,
        "count": elements.len(),
        "elements": elements,
    })))
}

/// 全屏 OCR（含文本框坐标），供 OCR+坐标点击（自绘面板，如 QQ 红包/表情）。
pub(super) async fn perception_ocr(
    State(state): State<Arc<owo_agent_server::AppState>>,
) -> Result<Json<owo_agent_core::OcrSummary>, (StatusCode, String)> {
    if !state
        .perception
        .lock()
        .map_err(poison)?
        .is_enabled(owo_agent_core::PerceptionLayer::L2Visual)
    {
        return Err((StatusCode::BAD_REQUEST, "L2 视觉层未授权".to_string()));
    }
    let bytes = owo_agent_core::capture_screen()
        .ok_or((StatusCode::BAD_REQUEST, "屏幕截图失败".to_string()))?;
    owo_agent_core::ocr_preferred(&bytes)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

pub(super) async fn ocr_status() -> Json<owo_agent_core::OcrEngineStatus> {
    Json(owo_agent_core::ocr_engine_status())
}

#[derive(serde::Deserialize)]
pub(super) struct OcrBytesRequest {
    bmp_b64: String,
}

/// 对 base64 编码的 BMP 做 OCR（模拟窗口帧/附件截图调试用，不依赖屏幕）。
pub(super) async fn perception_ocr_bytes(
    Json(request): Json<OcrBytesRequest>,
) -> Result<Json<owo_agent_core::OcrSummary>, (StatusCode, String)> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&request.bmp_b64)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("base64 解码失败：{e}")))?;
    owo_agent_core::ocr_preferred(&bytes)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

#[derive(serde::Deserialize)]
pub(super) struct OcrRegionRequest {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    #[serde(default = "default_ocr_scale")]
    scale: u32,
}

fn default_ocr_scale() -> u32 {
    2
}

/// serde 默认助手：视觉请求中的可选布尔开关默认开启。
/// 仍被 lib.rs 内联的视觉请求结构引用（`perception_api::default_true`）。
pub(super) fn default_true() -> bool {
    true
}

/// 区域 OCR：裁剪 + 放大后识别（小字验证窗口/自绘面板）。
pub(super) async fn perception_ocr_region(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<OcrRegionRequest>,
) -> Result<Json<owo_agent_core::OcrSummary>, (StatusCode, String)> {
    if !state
        .perception
        .lock()
        .map_err(poison)?
        .is_enabled(owo_agent_core::PerceptionLayer::L2Visual)
    {
        return Err((StatusCode::BAD_REQUEST, "L2 视觉层未授权".to_string()));
    }
    let bytes = owo_agent_core::capture_screen()
        .ok_or((StatusCode::BAD_REQUEST, "屏幕截图失败".to_string()))?;
    let cropped = owo_agent_core::crop_scale_bmp(
        &bytes,
        request.x,
        request.y,
        request.width,
        request.height,
        request.scale,
    )
    .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    owo_agent_core::ocr_preferred(&cropped)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))
}

#[derive(serde::Deserialize)]
pub(super) struct WindowOcrRequest {
    hwnd: i64,
}

/// 窗口级 OCR：PrintWindow 后台只读抓取指定窗口 → PP-OCRv6/Media 识别，返回窗口矩形与文本行。
pub(super) async fn perception_window(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<WindowOcrRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    require_perception_layer(&state, owo_agent_core::PerceptionLayer::L2Visual)?;
    let (bmp, rect) = owo_agent_core::platform::capture_window_bmp_deep(request.hwnd as isize)
        .ok_or((StatusCode::BAD_REQUEST, "窗口截图失败".to_string()))?;
    let summary = owo_agent_core::ocr_preferred(&bmp)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error))?;
    let lines: Vec<Value> = owo_agent_core::group_ocr_lines(&summary.boxes)
        .into_iter()
        .map(|line| {
            json!({
                "text": line.text,
                "x": line.x,
                "y": line.y,
                "width": line.width,
                "height": line.height,
            })
        })
        .collect();
    Ok(Json(json!({
        "window_rect": [rect.0, rect.1, rect.2, rect.3],
        "provider": summary.provider,
        "chars": summary.chars,
        "text": summary.text,
        "lines": lines,
        "boxes": summary.boxes,
    })))
}
