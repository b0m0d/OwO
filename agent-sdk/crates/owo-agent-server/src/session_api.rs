//! 会话元数据 HTTP API（§12：从 lib.rs 机械外移的 session 元数据域·第一刀）。
//!
//! 路由面（`GET /sessions`、`POST /session`、`GET /session/{id}`、
//! `POST /session/{id}/rename|archive|pin`、`GET /session/{id}/children`）
//! 与 /openapi.json 登记保持不变，零行为变化。
//! 回合执行（turn/diff/revert/…）仍留 lib.rs，经本模块共享
//! `to_session_info`/`load_session`，语义不变。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`（根私有项如
//! `acquire_session_lock` 经 crate 名全限定访问——本模块为其后代可见）。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use owo_agent_core::session::Session;
use owo_agent_protocol::{CreateSessionRequest, SessionInfo};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use owo_agent_server::AppState;

/// 模块内 `poison` 副本（§12 惯例：共享根助手保持各域独立可用）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

pub(super) fn to_session_info(session: &Session) -> SessionInfo {
    SessionInfo {
        id: session.id.clone(),
        workspace: session.workspace.to_string_lossy().into_owned(),
        model: session.model.clone(),
        created_at: session.created_at.clone(),
        updated_at: session.updated_at.clone(),
        title: Some(session.display_title()),
        archived: session.archived,
        pinned: session.pinned,
        parent_id: session.parent_id.clone(),
        fork_point: session.fork_point,
    }
}

pub(super) fn load_session(state: &AppState, id: &str) -> Result<Session, (StatusCode, String)> {
    if let Ok(sessions) = state.sessions.lock() {
        if let Some(session) = sessions.get(id) {
            return Ok(session.clone());
        }
    }
    state.store.load(id).map_err(|error| {
        (
            StatusCode::NOT_FOUND,
            format!("会话不存在：{id}（{error}）"),
        )
    })
}

pub(super) async fn list_sessions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<SessionInfo>>, (StatusCode, String)> {
    let mut sessions = Vec::new();
    for session_id in state.store.list() {
        if let Ok(session) = state.store.load(&session_id) {
            sessions.push(to_session_info(&session));
        }
    }
    sessions.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
    });
    Ok(Json(sessions))
}

pub(super) async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateSessionRequest>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let workspace = PathBuf::from(&request.workspace);
    if !workspace.is_dir() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("工作区不存在：{}", request.workspace),
        ));
    }
    let model = request.model.unwrap_or_else(|| {
        std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| owo_agent_core::gateway::DEFAULT_MODEL_ID.to_string())
    });
    let session = state
        .store
        .create(&workspace, &model, request.system_prompt.as_deref())
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session.clone());
    let title = session.display_title();
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(SessionInfo {
        id: session.id,
        workspace: request.workspace,
        model: session.model,
        created_at: session.created_at,
        updated_at: session.updated_at,
        title: Some(title),
        archived: session.archived,
        pinned: session.pinned,
        parent_id: session.parent_id,
        fork_point: session.fork_point,
    }))
}

pub(super) async fn get_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let session = load_session(&state, &id)?;
    Ok(Json(json!({
        "id": session.id,
        "title": session.display_title(),
        "model": session.model,
        "workspace": session.workspace.to_string_lossy(),
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "archived": session.archived,
        "pinned": session.pinned,
        "parent_id": session.parent_id,
        "fork_point": session.fork_point,
        "messages": session.messages,
    })))
}

#[derive(Deserialize)]
pub(super) struct RenameRequest {
    title: String,
}

#[derive(Deserialize)]
pub(super) struct ArchiveRequest {
    archived: bool,
}

#[derive(Deserialize)]
pub(super) struct PinRequest {
    pinned: bool,
}

pub(super) async fn session_rename(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<RenameRequest>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let mut session = load_session(&state, &id)?;
    session.rename(request.title);
    state
        .store
        .save(&session)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session.clone());
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(to_session_info(&session)))
}

pub(super) async fn session_archive(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ArchiveRequest>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let mut session = load_session(&state, &id)?;
    session.set_archived(request.archived);
    state
        .store
        .save(&session)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session.clone());
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(to_session_info(&session)))
}

pub(super) async fn session_pin(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<PinRequest>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let mut session = load_session(&state, &id)?;
    session.set_pinned(request.pinned);
    state
        .store
        .save(&session)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session.clone());
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(to_session_info(&session)))
}

pub(super) async fn children(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<SessionInfo>>, (StatusCode, String)> {
    let mut result = Vec::new();
    for session_id in state.store.list() {
        if let Ok(session) = state.store.load(&session_id) {
            if session.parent_id.as_deref() == Some(id.as_str()) {
                result.push(to_session_info(&session));
            }
        }
    }
    Ok(Json(result))
}

// ---------- §12 第二刀：附件 / 中止 / diff / 回滚 / fork / rewind / redo / 导出 ----------

pub(super) fn attachment_dir(workspace: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    workspace.join(".owo-attachments").join(session_id)
}

pub(super) fn sanitize_attachment_name(name: &str) -> Option<String> {
    let file_name = std::path::Path::new(name).file_name()?.to_str()?;
    let cleaned: String = file_name
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            )
        })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() || trimmed.len() > 200 {
        None
    } else {
        Some(trimmed)
    }
}

#[derive(Deserialize)]
pub(super) struct AttachmentUploadRequest {
    name: String,
    #[serde(default)]
    mime: Option<String>,
    data_b64: String,
}

pub(super) async fn attachment_upload(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<AttachmentUploadRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let session = load_session(&state, &id)?;
    let safe_name = sanitize_attachment_name(&request.name)
        .ok_or((StatusCode::BAD_REQUEST, "附件名非法".to_string()))?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&request.data_b64)
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("附件 base64 解码失败：{error}"),
            )
        })?;
    if bytes.len() > 50 * 1024 * 1024 {
        return Err((StatusCode::BAD_REQUEST, "附件超过 50MB 上限".to_string()));
    }
    let dir = attachment_dir(&session.workspace, &id);
    std::fs::create_dir_all(&dir)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let path = dir.join(&safe_name);
    std::fs::write(&path, &bytes)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            &id,
            "attachment",
            Some(safe_name.clone()),
            Some(true),
            format!("上传附件 {}（{} 字节）", safe_name, bytes.len()),
        );
    }
    Ok(Json(json!({
        "id": safe_name,
        "name": request.name,
        "mime": request.mime,
        "size": bytes.len(),
        "path": path.to_string_lossy(),
    })))
}

pub(super) async fn attachments_list(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<Value>>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let session = load_session(&state, &id)?;
    let dir = attachment_dir(&session.workspace, &id);
    let mut attachments = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            attachments.push(json!({ "id": name, "name": name, "size": size }));
        }
    }
    attachments.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(Json(attachments))
}

pub(super) async fn abort_turn(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(flag) = state.aborts.lock().map_err(poison)?.get(&id).cloned() {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn diff(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<owo_agent_protocol::FileDiff>>, (StatusCode, String)> {
    let session = load_session(&state, &id)?;
    Ok(Json(session.diff()))
}

pub(super) async fn revert(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let mut session = load_session(&state, &id)?;
    let restored = session
        .revert()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("回滚失败：{e}")))?;
    state
        .store
        .save(&session)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session);
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(json!({ "ok": true, "restored": restored })))
}

pub(super) async fn fork_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<owo_agent_protocol::ForkRequest>,
) -> Result<Json<SessionInfo>, (StatusCode, String)> {
    let session = load_session(&state, &id)?;
    let child = session.fork(request.message_index);
    state
        .store
        .save(&child)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(child.id.clone(), child.clone());
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(to_session_info(&child)))
}

pub(super) async fn rewind_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<owo_agent_protocol::RewindRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let mut session = load_session(&state, &id)?;
    if request.keep < session.messages.len() {
        session.revert().await.map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("回滚失败：{error}"),
            )
        })?;
    }
    let removed = session.rewind(request.keep);
    state
        .store
        .save(&session)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session);
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(json!({ "ok": true, "removed": removed.len() })))
}

pub(super) async fn redo_session(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let _session_guard = owo_agent_server::acquire_session_lock(&state, &id).await?;
    let mut session = load_session(&state, &id)?;
    let restored = session.redo().map(|tail| tail.len()).unwrap_or(0);
    state
        .store
        .save(&session)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    state
        .sessions
        .lock()
        .map_err(poison)?
        .insert(session.id.clone(), session);
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Sessions);
    Ok(Json(json!({ "ok": true, "restored": restored })))
}

pub(super) async fn export_session(
    State(state): State<Arc<AppState>>,
    AxumPath((id, format)): AxumPath<(String, String)>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    let session = load_session(&state, &id)?;
    let (body, content_type) = match format.as_str() {
        "md" | "markdown" => (
            owo_agent_core::export_markdown(&session),
            "text/markdown; charset=utf-8",
        ),
        "html" => (
            owo_agent_core::export_html(&session),
            "text/html; charset=utf-8",
        ),
        _ => return Err((StatusCode::BAD_REQUEST, "格式仅支持 md / html".to_string())),
    };
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response())
}

// ---------- 上下文诊断（§12：context_snapshot/session_context 自 lib.rs 机械外移） ----------

/// v0.4：情景快照（感知面 + 剪贴板 + UIA 刷新后返回 snapshot）。
pub(super) async fn context_snapshot(
    State(state): State<Arc<AppState>>,
) -> Result<Json<owo_agent_core::perception::SituationSnapshot>, (StatusCode, String)> {
    let mut perception = state.perception.lock().map_err(poison)?;
    let _ = perception.refresh_from_platform();
    let sequence = owo_agent_core::clipboard_sequence();
    perception.refresh_clipboard(sequence);
    let _ = perception.refresh_from_uia(2, 64);
    Ok(Json(perception.snapshot()))
}

/// 会话上下文诊断：消息数/估算 token/预算占用/规则注入/最近压缩摘要。
pub(super) async fn session_context(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let session = load_session(&state, &id)?;
    let messages: Vec<owo_agent_core::ChatMessage> = session.messages.clone();
    let estimated = owo_agent_core::estimate_tokens(&messages);
    let rules = owo_agent_core::context::load_project_rules(&session.workspace);
    let config = state.agent.config();
    let mut last_compaction: Option<String> = None;
    // 反向找最近的压缩摘要（system 消息以"历史摘要"开头）。
    for message in messages.iter().rev() {
        if message.role == "system"
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.starts_with("历史摘要"))
        {
            last_compaction = message.content.clone();
            break;
        }
    }
    Ok(Json(json!({
        "session_id": id,
        "messages": messages.len(),
        "estimated_tokens": estimated,
        "token_budget": config.token_budget,
        "compaction_enabled": config.compaction_enabled,
        "over_budget": estimated > config.token_budget,
        "rules_injected": !rules.is_empty(),
        "rules_chars": rules.chars().count(),
        "last_compaction": last_compaction,
    })))
}
