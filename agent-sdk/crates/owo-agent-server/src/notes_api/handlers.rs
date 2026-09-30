use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::store::*;
use super::support::*;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::Json;
use owo_agent_core::{
    add_block, doc_title, doc_to_md, insert_child, md_to_doc, move_block, new_doc, remove_block,
    sanitize_html, Block, BlockKind,
};
use serde::Deserialize;
use serde_json::{json, Value};

// ----------------------------------------------------------------------------
// 请求/响应模型
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateNoteRequest {
    title: String,
    #[serde(default)]
    markdown: Option<String>,
}

#[derive(Deserialize)]
struct ReplaceNoteRequest {
    #[serde(default)]
    title: Option<String>,
    /// 完整块表（BTreeMap 序列化的对象）。缺省仅改标题。
    #[serde(default)]
    blocks: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
struct AddBlockRequest {
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    after: Option<String>,
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Deserialize)]
struct UpdateBlockRequest {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Deserialize)]
struct MoveBlockRequest {
    block_id: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    after: Option<String>,
}

#[derive(Deserialize)]
struct ImportRequest {
    title: String,
    markdown: String,
}

// ----------------------------------------------------------------------------
// 块 kind 解析（text/data → BlockKind）
// ----------------------------------------------------------------------------

pub fn router(state: Arc<owo_agent_server::AppState>) -> axum::Router {
    axum::Router::new()
        .route("/notes", get(list_notes).post(create_note))
        .route(
            "/notes/{id}",
            get(get_note).put(replace_note).delete(delete_note),
        )
        .route("/notes/import", post(import_note))
        .route("/notes/search", get(search_notes_handler))
        .route("/notes/{id}/export/{format}", get(export_note))
        .route("/notes/{id}/blocks", post(add_block_handler))
        .route("/notes/{id}/blocks/move", post(move_block_handler))
        .route(
            "/notes/{id}/blocks/{block_id}",
            patch(update_block).delete(delete_block),
        )
        .route("/notes/{id}/reindex", post(reindex_note))
        .with_state(state)
}

async fn list_notes(
    State(state): State<Arc<owo_agent_server::AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let list = store
        .list()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "count": list.len(), "notes": list })))
}

async fn create_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<CreateNoteRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let title = request.title.trim().to_string();
    if title.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "title 不能为空"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let doc = match &request.markdown {
        Some(md) => md_to_doc(id.clone(), title, md),
        None => {
            let mut doc = new_doc(id.clone(), title);
            let root = doc.root.clone();
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::Paragraph {
                    text: String::new(),
                },
                Default::default(),
            );
            doc
        }
    };
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(
        &state,
        "notes.create",
        &doc.id,
        format!("创建笔记「{}」", doc.title),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "id": doc.id, "title": doc.title })),
    ))
}

async fn get_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let store = store.lock().await;
    let doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    Ok(Json(json!({
        "id": doc.id,
        "title": doc.title,
        "root": doc.root,
        "updated_at": doc.updated_at,
        "blocks": doc.blocks,
    })))
}

async fn replace_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ReplaceNoteRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let mut doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    if let Some(title) = &request.title {
        if title.trim().is_empty() {
            return Err(err(StatusCode::BAD_REQUEST, "title 不能为空"));
        }
        doc_title(&mut doc, title.clone());
    }
    if let Some(blocks) = &request.blocks {
        let blocks_map: Result<BTreeMap<String, Block>, String> = blocks
            .iter()
            .map(|(key, value)| {
                serde_json::from_value::<Block>(value.clone())
                    .map(|b| (key.clone(), b))
                    .map_err(|e| format!("块 {key} 解析失败：{e}"))
            })
            .collect();
        let blocks_map = blocks_map.map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        doc.blocks = blocks_map;
        validate_doc(&doc).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    }
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(&state, "notes.update", &doc.id, "整文档替换");
    Ok(Json(
        json!({ "ok": true, "id": doc.id, "updated_at": doc.updated_at }),
    ))
}

async fn delete_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    store
        .delete(&id)
        .map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    audit(&state, "notes.delete", &id, "删除笔记");
    Ok(Json(json!({ "ok": true, "id": id })))
}

async fn import_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Json(request): Json<ImportRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let id = uuid::Uuid::new_v4().to_string();
    let doc = md_to_doc(id.clone(), request.title, &request.markdown);
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(
        &state,
        "notes.import",
        &doc.id,
        format!("导入 Markdown（{} 块）", doc.blocks.len()),
    );
    Ok(Json(
        json!({ "ok": true, "id": doc.id, "blocks": doc.blocks.len() }),
    ))
}

async fn search_notes_handler(
    State(state): State<Arc<owo_agent_server::AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let q = params.get("q").cloned().unwrap_or_default();
    if q.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "缺少 q 参数"));
    }
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let hits = store
        .search(&q)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let hits_json: Vec<Value> = hits
        .iter()
        .map(|h| json!({ "doc_id": h.doc_id, "block_id": h.block_id, "snippet": h.snippet }))
        .collect();
    Ok(Json(
        json!({ "q": q, "count": hits.len(), "hits": hits_json }),
    ))
}

async fn export_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath((id, format)): AxumPath<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let store = store.lock().await;
    let doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    match format.as_str() {
        "md" => Ok(Json(json!({ "format": "md", "content": doc_to_md(&doc) }))),
        "html" => Ok(Json(
            json!({ "format": "html", "content": block_to_html(&doc) }),
        )),
        other => Err(err(
            StatusCode::BAD_REQUEST,
            format!("未知导出格式：{other}（支持 md|html）"),
        )),
    }
}

async fn add_block_handler(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<AddBlockRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let data = request.data.unwrap_or(Value::Null);
    let kind = parse_block_kind(&request.kind, request.text, &data)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let mut doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    let (parent, index) = match &request.after {
        Some(after) => locate_after(&doc, after).map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
        None => (
            request.parent.unwrap_or_else(|| doc.root.clone()),
            usize::MAX,
        ),
    };
    if !doc.blocks.contains_key(&parent) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("父块不存在：{parent}"),
        ));
    }
    let new_id = if index == usize::MAX {
        add_block(&mut doc, &parent, kind, Default::default())
            .map_err(|e| err(StatusCode::BAD_REQUEST, e))?
    } else {
        insert_child(&mut doc, &parent, index, kind, Default::default())
            .map_err(|e| err(StatusCode::BAD_REQUEST, e))?
    };
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(
        &state,
        "notes.block.add",
        &id,
        format!("添加块 {new_id}（{}）", request.kind),
    );
    Ok(Json(json!({ "ok": true, "id": new_id })))
}

async fn move_block_handler(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<MoveBlockRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let mut doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    if !doc.blocks.contains_key(&request.block_id) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("块不存在：{}", request.block_id),
        ));
    }
    let (parent, index) = match &request.after {
        Some(after) => locate_after(&doc, after).map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
        None => (
            request.parent.unwrap_or_else(|| doc.root.clone()),
            usize::MAX,
        ),
    };
    move_block(
        &mut doc,
        &request.block_id,
        &parent,
        if index == usize::MAX {
            None
        } else {
            Some(index)
        },
    )
    .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(
        &state,
        "notes.block.move",
        &id,
        format!("移动块 {}", request.block_id),
    );
    Ok(Json(json!({ "ok": true, "block_id": request.block_id })))
}

async fn update_block(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath((id, block_id)): AxumPath<(String, String)>,
    Json(request): Json<UpdateBlockRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let mut doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    let Some(block) = doc.blocks.get_mut(&block_id) else {
        return Err(err(StatusCode::NOT_FOUND, format!("块不存在：{block_id}")));
    };
    update_block_kind(block, &request).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(
        &state,
        "notes.block.update",
        &id,
        format!("更新块 {block_id}"),
    );
    Ok(Json(json!({ "ok": true, "block_id": block_id })))
}

fn update_block_kind(block: &mut Block, request: &UpdateBlockRequest) -> Result<(), String> {
    let text = request.text.clone().unwrap_or_default();
    match &mut block.kind {
        BlockKind::Paragraph { text: t } => {
            if request.text.is_some() {
                *t = text;
            }
        }
        BlockKind::Heading { text: t, .. } => {
            if request.text.is_some() {
                *t = text;
            }
        }
        BlockKind::ListItem { text: t } => {
            if request.text.is_some() {
                *t = text;
            }
        }
        BlockKind::Code { text: t, .. } => {
            if request.text.is_some() {
                *t = text;
            }
        }
        BlockKind::Quote { text: t } => {
            if request.text.is_some() {
                *t = text;
            }
        }
        BlockKind::AiGenerated { text: t, .. } => {
            if request.text.is_some() {
                *t = text;
            }
        }
        BlockKind::Table { rows } => {
            if let Some(data) = &request.data {
                if let Some(new_rows) = data.get("rows").and_then(Value::as_array) {
                    *rows = new_rows
                        .iter()
                        .filter_map(Value::as_array)
                        .map(|row| {
                            row.iter()
                                .map(|c| c.as_str().unwrap_or("").to_string())
                                .collect()
                        })
                        .collect();
                }
            }
        }
        BlockKind::Image { src, .. } => {
            if let Some(data) = &request.data {
                if let Some(new_src) = data.get("src").and_then(Value::as_str) {
                    *src = new_src.to_string();
                }
            }
        }
        BlockKind::File { path, .. } => {
            if let Some(data) = &request.data {
                if let Some(new_path) = data.get("path").and_then(Value::as_str) {
                    *path = new_path.to_string();
                }
            }
        }
        BlockKind::HtmlEmbed { html } => {
            if request.text.is_some() {
                *html = sanitize_html(&text);
            }
        }
        BlockKind::Canvas { data } => {
            if let Some(new_data) = &request.data {
                *data = parse_canvas(new_data);
            }
        }
        BlockKind::List { .. } => {}
    }
    Ok(())
}

async fn delete_block(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath((id, block_id)): AxumPath<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let mut doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    if !doc.blocks.contains_key(&block_id) {
        return Err(err(StatusCode::NOT_FOUND, format!("块不存在：{block_id}")));
    }
    let removed = remove_block(&mut doc, &block_id).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    store
        .persist(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    audit(
        &state,
        "notes.block.delete",
        &id,
        format!("删除块 {block_id}（子树 {} 块）", removed.len()),
    );
    Ok(Json(json!({ "ok": true, "removed": removed })))
}

async fn reindex_note(
    State(state): State<Arc<owo_agent_server::AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = store_for(&state.data_root);
    let mut store = store.lock().await;
    let doc = store.load(&id).map_err(|e| err(StatusCode::NOT_FOUND, e))?;
    store
        .reindex(&doc)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "ok": true, "id": id, "reindexed": true })))
}
