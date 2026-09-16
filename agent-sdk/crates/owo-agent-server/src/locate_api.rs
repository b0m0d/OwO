//! 元素定位 HTTP API（§12：从 lib.rs 机械外移的 locate 域）。
//!
//! 路由面（`POST /locate/query`）与 /openapi.json 登记保持不变，零行为变化。
//! 场景图更新、命中记录与不确定性评估语义原样保留。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::locate::{locate, AnchorQuery};
use owo_agent_core::scene::{Evidence, EvidenceSource, GraphElement};
use owo_agent_core::SceneElement;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// 模块内 `poison` 副本（§12 惯例：共享根助手保持各域独立可用）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

pub(super) async fn locate_query(
    State(state): State<Arc<AppState>>,
    Json(query): Json<AnchorQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let elements = state.elements.lock().map_err(poison)?;
    let elements: Vec<SceneElement> = if query.app_id.is_some() {
        elements.list(query.app_id.as_deref().unwrap_or_default())
    } else {
        elements.list_all()
    };
    let mut graph_elements: Vec<GraphElement> = elements
        .iter()
        .map(|element| {
            let mut entry = GraphElement::from_element(element.clone());
            let source = if element.sources.contains(&"uia".to_string()) {
                EvidenceSource::Uia
            } else if element.sources.contains(&"ocr".to_string()) {
                EvidenceSource::Ocr
            } else {
                EvidenceSource::Vision
            };
            entry.add_evidence(Evidence::new(source, element, element.confidence));
            entry
        })
        .collect();
    let mut graph = state.scene.lock().map_err(poison)?;
    graph.update(None, None, std::mem::take(&mut graph_elements));

    let result = locate(&graph, &query);
    if let Some(best) = &result.best {
        graph.record_hit(&best.id, &query.signature());
    }
    let candidates: Vec<Value> = result
        .candidates
        .iter()
        .map(|(element, score)| {
            json!({
                "id": element.id,
                "name": element.name,
                "role": element.role_hint,
                "rect": [element.x, element.y, element.width, element.height],
                "score": score,
            })
        })
        .collect();
    Ok(Json(json!({
        "count": candidates.len(),
        "candidates": candidates,
        "best": result.best.as_ref().map(|element| json!({
            "id": element.id,
            "name": element.name,
            "role": element.role_hint,
            "rect": [element.x, element.y, element.width, element.height],
            "confidence": element.confidence,
        })),
        "uncertainty": result.uncertainty,
        "used_source": result.used_source.map(|source| format!("{source:?}").to_lowercase()),
        "reliable": result.is_reliable(),
    })))
}
