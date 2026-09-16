//! 主动建议 / 语音转写 / 自动化 HTTP API（§12：从 lib.rs 机械外移的三小域合并模块）。
//!
//! 路由面（`POST /proactive/observe`、`POST /proactive/decide`、`GET /proactive/suggestions`、
//! `POST /stt/transcribe`、`GET/POST /automations`、`POST /automations/{id}/toggle`、
//! `DELETE /automations/{id}`、`GET/POST /automations/reminders`）与 /openapi.json
//! 登记保持不变，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::automation::{AutomationAction, AutomationTask, Schedule};
use owo_agent_core::{ProactiveSuggestion, SuggestionAction};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// 模块内 `poison` 副本（§12 惯例：共享根助手保持各域独立可用）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

// ---------- 主动建议（桌面端"学习/执行一次/忽略/静默"） ----------

#[derive(Deserialize)]
pub(super) struct ProactiveObserveRequest {
    app_id: String,
    actions: Vec<String>,
}

pub(super) async fn proactive_observe(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProactiveObserveRequest>,
) -> Result<Json<Option<ProactiveSuggestion>>, (StatusCode, String)> {
    let mut proactive = state.proactive.lock().map_err(poison)?;
    Ok(Json(proactive.observe(&request.app_id, request.actions)))
}

#[derive(Deserialize)]
pub(super) struct ProactiveDecideRequest {
    suggestion_id: String,
    action: SuggestionAction,
}

pub(super) async fn proactive_decide(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProactiveDecideRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut proactive = state.proactive.lock().map_err(poison)?;
    let suggestion = proactive
        .suggestions()
        .iter()
        .find(|suggestion| suggestion.id == request.suggestion_id)
        .cloned()
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("建议不存在：{}", request.suggestion_id),
            )
        })?;
    proactive
        .decide(&request.suggestion_id, request.action)
        .map_err(|error| (StatusCode::NOT_FOUND, error))?;
    drop(proactive);
    let mut response = json!({ "ok": true });
    if request.action == owo_agent_core::SuggestionAction::Learn {
        // 用户确认"学习"：把建议动作序列沉淀为 active 流程技能包（D24 一键学习）。
        let short_id: String = suggestion.id.chars().take(8).collect();
        let name = format!("proactive-{short_id}");
        let samples = owo_agent_core::recorded_actions_from_sequence(
            &suggestion.app_id,
            &suggestion.sequence,
        );
        let pipeline = state.pipeline.lock().map_err(poison)?;
        let package = pipeline
            .sink_from_actions(
                &name,
                vec![suggestion.app_id.clone()],
                owo_agent_core::Sensitivity::Low,
                &suggestion.summary,
                samples,
            )
            .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
        if let Ok(mut audit) = state.agent.audit_log().lock() {
            audit.record(
                "proactive",
                "learn-confirm",
                Some(package.manifest.name.clone()),
                Some(true),
                format!("主动建议确认沉淀技能包：{}", package.manifest.name),
            );
        }
        response["package"] = json!({
            "name": package.manifest.name,
            "variables": package.manifest.variables,
        });
    }
    Ok(Json(response))
}

/// 主动建议列表（桌面端"学习/执行一次/忽略/静默"四选）。
pub(super) async fn proactive_suggestions(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<ProactiveSuggestion>>, (StatusCode, String)> {
    let proactive = state.proactive.lock().map_err(poison)?;
    Ok(Json(proactive.suggestions().to_vec()))
}

// ---------- 本地离线语音转写 ----------

/// 本地离线转写：请求体为 WAV 字节（16k PCM），返回文本（SenseVoice-Small）。
pub(super) async fn stt_transcribe(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, String)> {
    let wav_path = std::env::temp_dir().join(format!("owo-stt-{}.wav", uuid::Uuid::new_v4()));
    std::fs::write(&wav_path, &body)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    let result = match state.stt.lock() {
        Ok(stt) => stt
            .transcribe_wav(&wav_path)
            .map(|outcome| (outcome, stt.engine().to_string())),
        Err(_) => Err("状态锁中毒".to_string()),
    };
    let _ = std::fs::remove_file(&wav_path);
    let (outcome, engine) = result.map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    Ok(Json(json!({
        "ok": true,
        "text": outcome.text,
        "elapsed_ms": outcome.elapsed_ms,
        "engine": engine,
    })))
}

// ---------- 自动化 ----------

#[derive(Deserialize)]
pub(super) struct CreateAutomationRequest {
    name: String,
    schedule: Schedule,
    reminder: String,
}

pub(super) async fn automations_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<AutomationTask>>, (StatusCode, String)> {
    let automations = state.automations.lock().map_err(poison)?;
    Ok(Json(automations.list()))
}

pub(super) async fn automations_create(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateAutomationRequest>,
) -> Result<Json<AutomationTask>, (StatusCode, String)> {
    let task = AutomationTask::new(
        &request.name,
        request.schedule,
        AutomationAction::Reminder {
            text: request.reminder,
        },
    );
    let mut automations = state.automations.lock().map_err(poison)?;
    automations
        .upsert(task.clone())
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Automations);
    Ok(Json(task))
}

pub(super) async fn automations_toggle(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut automations = state.automations.lock().map_err(poison)?;
    let enabled = automations
        .toggle(&id)
        .map_err(|error| (StatusCode::NOT_FOUND, error))?;
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Automations);
    Ok(Json(json!({ "id": id, "enabled": enabled })))
}

pub(super) async fn automations_delete(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut automations = state.automations.lock().map_err(poison)?;
    automations
        .remove(&id)
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Automations);
    Ok(Json(json!({ "ok": true })))
}

pub(super) async fn automations_reminders(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<String>>, (StatusCode, String)> {
    let automations = state.automations.lock().map_err(poison)?;
    Ok(Json(automations.reminders().to_vec()))
}

pub(super) async fn automations_clear_reminders(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut automations = state.automations.lock().map_err(poison)?;
    automations
        .clear_reminders()
        .map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    Ok(Json(json!({ "ok": true })))
}
