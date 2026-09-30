use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::Sse;
use axum::response::IntoResponse;
use axum::Json;
use futures_util::stream;
use owo_agent_core::permissions::Decision;
use owo_agent_protocol::{PermissionResponse, SseEvent, TurnRequest};
use serde_json::{json, Value};

use super::approval::*;
use super::queue::*;
use super::wire::*;
use owo_agent_server::AppState;

pub(crate) async fn turn_events(
    State(state): State<Arc<AppState>>,
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<TurnEventsQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    crate::session_api::load_session(&state, &session_id)?;
    let events = state
        .store
        .turn_events_after(
            &session_id,
            Some(&query.turn_id),
            query.after_seq,
            query.limit.unwrap_or(256),
        )
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let active_turn_id = state
        .active_turn_ids
        .lock()
        .map_err(poison)?
        .get(&session_id)
        .cloned();
    let replay_state = events
        .iter()
        .filter_map(|record| match &record.payload {
            SseEvent::Final { .. } => Some(owo_agent_protocol::TurnReplayState::Completed),
            event if is_turn_failed_event(event) => {
                Some(owo_agent_protocol::TurnReplayState::Failed)
            }
            _ => None,
        })
        .next_back()
        .unwrap_or_else(|| {
            if active_turn_id.as_deref() == Some(query.turn_id.as_str()) {
                owo_agent_protocol::TurnReplayState::Active
            } else {
                owo_agent_protocol::TurnReplayState::Interrupted
            }
        });
    let active = replay_state == owo_agent_protocol::TurnReplayState::Active;
    let next_after_seq = events
        .last()
        .map(|record| record.seq)
        .unwrap_or(query.after_seq);
    Ok(Json(json!({
        "events": events,
        "active": active,
        "state": replay_state,
        "next_after_seq": next_after_seq,
    })))
}

pub(crate) async fn turn(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<TurnRequest>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    // §13 批次六：遥测功能计数（默认关时零开销早退；仅数字，无内容）。
    crate::observability_api::record_telemetry_counter("turn", 1);
    let session = crate::session_api::load_session(&state, &id)?;
    let mut effective_prompt = request.prompt.clone();
    if !request.attachments.is_empty() {
        let dir = crate::session_api::attachment_dir(&session.workspace, &id);
        let mut lines = Vec::new();
        for attachment in &request.attachments {
            let safe = Path::new(attachment)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(attachment);
            let path = dir.join(safe);
            if !path.is_file() {
                return Err((StatusCode::BAD_REQUEST, format!("附件不存在：{safe}")));
            }
            let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            lines.push(format!(
                "- {}（{} 字节，路径 {}）",
                safe,
                size,
                path.display()
            ));
        }
        effective_prompt.push_str("\n\n附件：\n");
        effective_prompt.push_str(&lines.join("\n"));
    }

    let turn_lock = {
        let mut locks = state.turn_locks.lock().map_err(poison)?;
        locks
            .entry(id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let turn_guard = turn_lock
        .try_lock_owned()
        .map_err(|_| (StatusCode::CONFLICT, "该会话已有回合正在运行".to_string()))?;
    // R8：全局并发 turn 上限 + 关闭中拒绝新回合。
    let concurrency_permit = state.shutdown_gate.try_acquire_turn().map_err(|busy| {
        // R10：错误码表接入（domain/reason/retryable 统一前缀，见 error_codes.rs）。
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("[gateway/unavailable/retryable] {busy}"),
        )
    })?;
    // R8/R9：用量预算硬熔断（Agent 4 交付 usage；超限停轮，错误码贯穿，请求用户加额后恢复）。
    if crate::usage::global().check_budget() {
        let reason = crate::usage::global()
            .hard_stop_reason()
            .unwrap_or_else(|| "用量预算超限".to_string());
        let (status, body) = crate::usage::budget_exceeded_response(&reason);
        let detail = body
            .0
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&reason)
            .to_string();
        return Err((
            status,
            format!("[{}] {detail}", crate::usage::BUDGET_ERROR_CODE),
        ));
    }

    let event_queue = Arc::new(TurnEventQueue::new());
    let turn_id = uuid::Uuid::new_v4().to_string();
    let receiver_alive = Arc::new(());
    let receiver_weak = Arc::downgrade(&receiver_alive);
    let abort_flag = {
        let mut aborts = state.aborts.lock().map_err(poison)?;
        aborts
            .entry(id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };
    state
        .active_turn_ids
        .lock()
        .map_err(poison)?
        .insert(id.clone(), turn_id.clone());
    abort_flag.store(false, Ordering::Relaxed);
    let approver = ChannelApprover {
        pending: Arc::clone(&state.pending_approvals),
        pending_sessions: Arc::clone(&state.pending_approval_sessions),
        session_id: id.clone(),
        abort: Arc::clone(&abort_flag),
    };

    let agent = Arc::clone(&state.agent);
    let store = Arc::clone(&state.store);
    let sessions = Arc::clone(&state.sessions);
    let traces_dir = state.traces_dir.clone();
    let state_for_audit = Arc::clone(&state);
    let producer_store = Arc::clone(&store);
    let producer_session_id = session.id.clone();
    let producer_turn_id = turn_id.clone();
    let trace_started = std::time::Instant::now();
    let trace_started_at = chrono::Utc::now().to_rfc3339();
    let trace_prompt = effective_prompt.clone();
    let stream_queue = Arc::clone(&event_queue);
    tokio::spawn(async move {
        let _turn_guard = turn_guard;
        let _concurrency_permit = concurrency_permit;
        let mut current = session;
        let stream_abort = Arc::clone(&abort_flag);
        let mut tool_starts: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
        let producer_queue = Arc::clone(&event_queue);
        let producer_receiver = receiver_weak.clone();
        let producer_store = Arc::clone(&producer_store);
        let producer_session_id = producer_session_id.clone();
        let producer_turn_id = producer_turn_id.clone();
        let mut on_event = |event: &owo_agent_core::TurnEvent| {
            // §13 批次九：工具耗时埋点——ToolStart/ToolResult 以 id 配对，差值进
            // /metrics/runtime 的 tool_durations_ms 样本（超上限丢最旧，见 observability_api）。
            match event {
                owo_agent_core::TurnEvent::ToolStart { id, .. } => {
                    tool_starts.insert(id.clone(), std::time::Instant::now());
                }
                owo_agent_core::TurnEvent::ToolResult { id, .. } => {
                    if let Some(started) = tool_starts.remove(id) {
                        crate::observability_api::record_tool_duration_ms(
                            started.elapsed().as_millis() as u64,
                        );
                    }
                }
                _ => {}
            }
            if let Some(sse) = to_sse(event) {
                if persist_and_queue_event(
                    producer_store.as_ref(),
                    &producer_session_id,
                    &producer_turn_id,
                    &producer_queue,
                    &producer_receiver,
                    sse,
                )
                .is_err()
                {
                    // 客户端断开或有界队列溢出后，尽快停止无主/过载回合。
                    stream_abort.store(true, Ordering::Relaxed);
                }
            }
        };
        match agent
            .run_turn(
                &mut current,
                &effective_prompt,
                &approver,
                &abort_flag,
                &mut on_event,
            )
            .await
        {
            Ok(outcome) => {
                let trace = owo_agent_core::TraceRecord::from_outcome(&current, &outcome);
                let _ = owo_agent_core::save_trace(&traces_dir, &trace);
                // §3.2：trace 落盘后发布 traces 域失效（每个回合至多一次）。
                crate::event_stream::hub()
                    .publish_invalidate(crate::event_stream::InvalidateDomain::Traces);
                if outcome.usage.total_tokens > 0 {
                    let input_price = std::env::var("OWO_MODEL_INPUT_PRICE_PER_MTOK")
                        .ok()
                        .and_then(|value| value.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let output_price = std::env::var("OWO_MODEL_OUTPUT_PRICE_PER_MTOK")
                        .ok()
                        .and_then(|value| value.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let cost = outcome.usage.cost_estimate_usd(input_price, output_price);
                    if let Ok(mut audit) = state_for_audit.agent.audit_log().lock() {
                        audit.record(
                            "model",
                            "usage",
                            Some(current.id.clone()),
                            Some(true),
                            format!(
                                "prompt={} completion={} total={} cost_usd≈{:.6}",
                                outcome.usage.prompt_tokens,
                                outcome.usage.completion_tokens,
                                outcome.usage.total_tokens,
                                cost
                            ),
                        );
                    }
                    // R8：用量与成本归集（Agent 4 交付 usage_router 的会话维度记录）。
                    crate::usage::global().record_tokens(
                        crate::usage::UsageDimension::Session,
                        &current.id,
                        Some(&current.id),
                        outcome.usage.prompt_tokens,
                        outcome.usage.completion_tokens,
                    );
                }
            }
            Err(error) => {
                let error_text = error.to_string();
                crate::logging::error(
                    "agent",
                    None,
                    "回合执行失败",
                    &[("session_id", serde_json::json!(current.id))],
                );
                let trace = owo_agent_core::TraceRecord::from_error(
                    &current,
                    &trace_prompt,
                    &trace_started_at,
                    trace_started.elapsed().as_millis() as u64,
                    &error_text,
                );
                let _ = owo_agent_core::save_trace(&traces_dir, &trace);
                crate::event_stream::hub()
                    .publish_invalidate(crate::event_stream::InvalidateDomain::Traces);
                let _ = persist_and_queue_event(
                    producer_store.as_ref(),
                    &producer_session_id,
                    &producer_turn_id,
                    &producer_queue,
                    &producer_receiver,
                    // 取优合并（远端 engine）：显式 TurnFailed 终态，前端不再停留在
                    // 「执行中」；`is_turn_failed_event` 同时识别旧的 Progress 前缀。
                    SseEvent::TurnFailed {
                        message: error_text.to_string(),
                    },
                );
            }
        }
        if let Ok(mut sessions) = sessions.lock() {
            sessions.insert(current.id.clone(), current.clone());
        }
        if let Err(error) = store.save(&current) {
            let _ = persist_and_queue_event(
                producer_store.as_ref(),
                &producer_session_id,
                &producer_turn_id,
                &producer_queue,
                &producer_receiver,
                SseEvent::Progress {
                    message: format!("session save failed: {error}"),
                },
            );
        }
        if let Ok(mut aborts) = state_for_audit.aborts.lock() {
            if aborts
                .get(&current.id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &abort_flag))
            {
                aborts.remove(&current.id);
            }
        }
        if let Ok(mut active_turn_ids) = state_for_audit.active_turn_ids.lock() {
            if active_turn_ids.get(&current.id) == Some(&producer_turn_id) {
                active_turn_ids.remove(&current.id);
            }
        }
        crate::audit_api::flush_audit(&state_for_audit);
        producer_queue.close();
    });

    let event_stream = stream::unfold(
        (stream_queue, receiver_alive),
        |(queue, receiver_alive)| async move {
            loop {
                match queue.pop() {
                    TurnEventPop::Event(event) => {
                        return Some((
                            to_event(event.seq, event.event),
                            (Arc::clone(&queue), receiver_alive),
                        ));
                    }
                    TurnEventPop::Empty => queue.notify.notified().await,
                    TurnEventPop::Closed => return None,
                }
            }
        },
    );
    let mut response = Sse::new(event_stream).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&turn_id) {
        response.headers_mut().insert("x-owo-turn-id", value);
    }
    Ok(response)
}

pub(crate) async fn respond_permission(
    State(state): State<Arc<AppState>>,
    AxumPath((session_id, request_id)): AxumPath<(String, String)>,
    Json(response): Json<PermissionResponse>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let belongs_to_session = state
        .pending_approval_sessions
        .lock()
        .map_err(poison)?
        .get(&request_id)
        .map(|pending_session| pending_session == &session_id)
        .unwrap_or(false);
    if !belongs_to_session {
        return Err((
            StatusCode::NOT_FOUND,
            format!("审批请求不存在：{request_id}"),
        ));
    }
    let (sender, request) = state
        .pending_approvals
        .lock()
        .map_err(poison)?
        .remove(&request_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("审批请求不存在：{request_id}"),
            )
        })?;
    state
        .pending_approval_sessions
        .lock()
        .map_err(poison)?
        .remove(&request_id);
    let decision = if response.allow {
        // §5.4 审批选项 → 临时授权（Grant）：响应后同工作区同参数不再弹卡。
        // 破坏性/注入请求不允许生成 grant（scope 一律忽略，仅放行本次）。
        if let Some(scope) = response.scope.as_deref() {
            let level_ok = request.level != owo_agent_core::permissions::Level::Inject;
            if level_ok {
                if let Some(scope_enum) = owo_agent_core::grant_store::GrantScope::parse(scope) {
                    if let Some(grant) =
                        state
                            .grants
                            .grant_from_scope(&request, &state.workspace_id(), scope_enum)
                    {
                        state.grants.insert(grant);
                    }
                }
            }
        }
        Decision::Allow
    } else {
        Decision::Deny
    };
    sender
        .send(decision)
        .map_err(|_| (StatusCode::GONE, "审批通道已关闭".to_string()))?;
    Ok(Json(json!({ "ok": true, "granted": response.allow })))
}
