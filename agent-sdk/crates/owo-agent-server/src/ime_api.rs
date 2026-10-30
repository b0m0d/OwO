//! IME admission reuses the daemon's ordinary sessions, turns, tools and approvals.
use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    Json,
};
use futures_util::StreamExt;
use owo_agent_core::request_scope::RequestPermissionScope;
use owo_agent_core::{
    ime_access::{ImeAccessMode, ImeAccessSettings},
    session::Session,
};
use owo_agent_protocol::{ImeSessionSource, ImeTaskReceipt, ImeTaskRequest, TurnRequest};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};

type ApiError = (StatusCode, String);
struct RuntimeAccess {
    config: ImeAccessSettings,
    scopes: HashMap<String, Weak<RequestPermissionScope>>,
}
pub(crate) struct ImeTaskState {
    runtime: Mutex<RuntimeAccess>,
    pub(crate) origins: Arc<Mutex<crate::expression_origin::OriginVerifierFor<ImeTaskReceipt>>>,
    pub(crate) admission: tokio::sync::Mutex<()>,
}
impl ImeTaskState {
    pub(crate) fn new(workspace: &Path) -> Self {
        Self {
            runtime: Mutex::new(RuntimeAccess {
                config: owo_agent_core::Settings::load(workspace).ime_access,
                scopes: HashMap::new(),
            }),
            origins: Arc::new(Mutex::new(
                crate::expression_origin::OriginVerifierFor::from_env(),
            )),
            admission: tokio::sync::Mutex::new(()),
        }
    }
    pub(crate) fn config(&self) -> Result<ImeAccessSettings, ApiError> {
        Ok(self.runtime.lock().map_err(poison)?.config.clone())
    }
}
fn poison<T>(_: std::sync::PoisonError<T>) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "ime/state_unavailable".into(),
    )
}
fn bad(message: impl ToString) -> ApiError {
    (StatusCode::BAD_REQUEST, message.to_string())
}
fn internal(message: impl ToString) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, message.to_string())
}
fn configured_workspace(
    state: &crate::AppState,
    config: &ImeAccessSettings,
) -> Result<PathBuf, ApiError> {
    let path = config
        .workspace
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| state.workspace.clone());
    let path = path
        .canonicalize()
        .map_err(|_| bad("ime/workspace_unavailable"))?;
    if !path.is_dir() {
        return Err(bad("ime/workspace_unavailable"));
    }
    Ok(path)
}
pub(crate) fn apply_access(
    state: &crate::AppState,
    config: &ImeAccessSettings,
) -> Result<usize, ApiError> {
    let mut runtime = state.ime_tasks.runtime.lock().map_err(poison)?;
    if runtime.config == *config {
        return Ok(0);
    }
    let mut sessions = Vec::new();
    for (session_id, weak) in runtime.scopes.drain() {
        if let Some(scope) = weak.upgrade() {
            scope.revoke();
            sessions.push(session_id);
        }
    }
    runtime.config = config.clone();
    drop(runtime);
    let mut controls = state.turn_controls.lock().map_err(poison)?;
    for id in &sessions {
        let _ = controls.cancel(id, None);
    }
    Ok(sessions.len())
}
/// Called by every ordinary turn on an IME-origin session, including desktop resume.
pub(crate) fn scope_for_session(
    state: &crate::AppState,
    session: &Session,
) -> Result<Option<Arc<RequestPermissionScope>>, ApiError> {
    if session.ime_origin.is_none() {
        return Ok(None);
    }
    let mut runtime = state.ime_tasks.runtime.lock().map_err(poison)?;
    if runtime.config.mode == ImeAccessMode::Disabled {
        return Err((StatusCode::FORBIDDEN, "ime/access_disabled".into()));
    }
    let workspace = configured_workspace(state, &runtime.config)?;
    if session.workspace.canonicalize().map_err(bad)? != workspace {
        return Err((StatusCode::FORBIDDEN, "ime/workspace_changed".into()));
    }
    runtime.scopes.retain(|_, scope| scope.strong_count() > 0);
    if let Some(scope) = runtime.scopes.get(&session.id).and_then(Weak::upgrade) {
        if !scope.is_revoked() {
            return Ok(Some(scope));
        }
    }
    let config = &runtime.config;
    let spec = (config.mode == ImeAccessMode::Custom)
        .then(|| config.permission_spec.clone())
        .flatten();
    let scope = Arc::new(RequestPermissionScope::new(
        config.mode == ImeAccessMode::ReadOnly,
        spec,
        config.allowed_tools.clone(),
    ));
    runtime
        .scopes
        .insert(session.id.clone(), Arc::downgrade(&scope));
    Ok(Some(scope))
}
pub(crate) async fn settings_get(
    State(state): State<Arc<crate::AppState>>,
) -> Result<Json<Value>, ApiError> {
    let tools = state.agent.visible_tool_specs().into_iter().map(|tool| json!({
        "name": tool.name, "description": tool.description.chars().take(256).collect::<String>(),
    })).collect::<Vec<_>>();
    Ok(Json(json!({
        "config": state.ime_tasks.config()?,
        "default_workspace": state.workspace,
        "agent_profile": state.agent.permission_profile().label(),
        "tools": tools,
    })))
}
pub(crate) async fn settings_update(
    State(state): State<Arc<crate::AppState>>,
    Json(config): Json<ImeAccessSettings>,
) -> Result<Json<Value>, ApiError> {
    config.validate().map_err(bad)?;
    configured_workspace(&state, &config)?;
    let _admission = state.ime_tasks.admission.lock().await;
    let mut settings = owo_agent_core::Settings::load(&state.workspace);
    settings.ime_access = config.clone();
    settings.save(&state.workspace).map_err(bad)?;
    let cancelled = apply_access(&state, &config)?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "ime",
            "access_update",
            None,
            Some(true),
            format!(
                "IME mode={:?}, restricted_tools={}, interrupted_sessions={cancelled}",
                config.mode,
                config.allowed_tools.is_some()
            ),
        );
    }
    crate::event_stream::hub().publish_invalidate(crate::event_stream::InvalidateDomain::Settings);
    Ok(Json(
        json!({ "ok": true, "config": config, "interrupted_sessions": cancelled }),
    ))
}
pub(crate) async fn submit(
    State(state): State<Arc<crate::AppState>>,
    Json(request): Json<ImeTaskRequest>,
) -> Result<Json<ImeTaskReceipt>, ApiError> {
    if request.prompt.trim().is_empty() || request.prompt.len() > 64 * 1024 {
        return Err(bad("ime/invalid_prompt"));
    }
    let _admission = state.ime_tasks.admission.lock().await;
    let config = state.ime_tasks.config()?;
    config.validate().map_err(bad)?;
    if config.mode == ImeAccessMode::Disabled {
        return Err((StatusCode::FORBIDDEN, "ime/access_disabled".into()));
    }
    let workspace = configured_workspace(&state, &config)?;
    crate::expression_origin::OriginVerifierFor::start_reaper(&state.ime_tasks.origins);
    let admission = state
        .ime_tasks
        .origins
        .lock()
        .map_err(poison)?
        .begin_task(&request.origin_capsule, &request.prompt)
        .map_err(|e| {
            (
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::FORBIDDEN),
                e.code.replacen("expression/", "ime/", 1),
            )
        })?;
    let lease = match admission {
        crate::expression_origin::OriginAdmission::Cached(receipt) => return Ok(Json(receipt)),
        crate::expression_origin::OriginAdmission::Fresh(lease) => lease,
    };
    let payload = &request.origin_capsule.payload;
    let identity = serde_json::to_vec(&(
        &payload.binding,
        payload.origin.connector_pid,
        &payload.origin.process_created_at_hex,
        &payload.origin.executable_sha256,
    ))
    .map_err(internal)?;
    let digest = Sha256::digest(identity);
    let session_id = uuid::Uuid::from_bytes(digest[..16].try_into().map_err(internal)?).to_string();
    let turn_digest = Sha256::digest(format!("ime-turn:{session_id}").as_bytes());
    let turn_id =
        uuid::Uuid::from_bytes(turn_digest[..16].try_into().map_err(internal)?).to_string();
    let source = ImeSessionSource {
        request_key: format!("{digest:x}"),
        prompt_sha256: payload.prompt_sha256.clone(),
        request_sha256: payload.origin_request_sha256.clone(),
        turn_id: turn_id.clone(),
    };
    let exists = state.store.exists(&session_id).map_err(internal)?;
    if exists {
        let session = crate::session_api::load_session(&state, &session_id)?;
        if session.ime_origin.as_ref() != Some(&source) || session.workspace != workspace {
            return Err((StatusCode::CONFLICT, "ime/request_conflict".into()));
        }
    } else {
        let model = std::env::var("OPENAI_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| owo_agent_core::gateway::DEFAULT_MODEL_ID.into());
        let mut session = Session::new(workspace, model, None);
        session.id = session_id.clone();
        session.title = Some(format!(
            "输入法 · {}",
            request.prompt.chars().take(40).collect::<String>()
        ));
        session.ime_origin = Some(source);
        state.store.save(&session).map_err(internal)?;
        state
            .sessions
            .lock()
            .map_err(poison)?
            .insert(session_id.clone(), session);
        let response = crate::turn_api::turn(
            State(Arc::clone(&state)),
            AxumPath(session_id.clone()),
            Json(TurnRequest {
                model_connection: None,
                prompt: request.prompt,
                attachments: Vec::new(),
                read_only: None,
                turn_id: Some(turn_id.clone()),
            }),
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                state.sessions.lock().map_err(poison)?.remove(&session_id);
                let _ = state.store.remove(&session_id);
                return Err(error);
            }
        };
        // The daemon owns this receiver: closing the IME candidate window does
        // not discard the job. Explicit cancellation still uses the normal API.
        tokio::spawn(async move {
            let mut stream = response.into_body().into_data_stream();
            while let Some(chunk) = stream.next().await {
                if chunk.is_err() {
                    break;
                }
            }
        });
    }
    let receipt = ImeTaskReceipt {
        session_id,
        turn_id,
        status: if exists { "existing" } else { "accepted" }.into(),
        origin_verified: true,
    };
    state
        .ime_tasks
        .origins
        .lock()
        .map_err(poison)?
        .complete(lease, receipt.clone())
        .map_err(|_| internal("ime/receipt_unavailable"))?;
    crate::event_stream::hub().publish_invalidate(crate::event_stream::InvalidateDomain::Sessions);
    Ok(Json(receipt))
}
