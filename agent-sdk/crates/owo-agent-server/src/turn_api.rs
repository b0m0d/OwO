//! §12 回合执行域（turn）API 模块。
//!
//! 提取证明：自 `lib.rs` 逐字迁移 —— turn 编排器（附件注入/会话锁/并发许可/
//! 预算硬熔断/审批通道/SSE 事件流/trace 与用量归集）、respond_permission
//! （§5.4 审批响应 + Grant 临时授权）、ChannelApprover（Approver 实现，
//! 300s 截止 + 中止感知）、auto_approve_enabled、to_sse/to_event（协议 SSE
//! 映射，R10 统一携带 v 字段）。路由路径与 OpenAPI 登记零变化。
//!
//! `acquire_session_lock` 留守 lib.rs（session_api 八处引用的会话域共享基建）。
//! 本模块引用兄弟域（session_api/usage/audit_api/event_stream/logging）经
//! `crate::` 路径，故不可作 #[path] 独立编译目标（仅库内编译）。

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::Json;
use owo_agent_core::permissions::{Approver, Decision, PermissionRequest};
use owo_agent_protocol::{PermissionResponse, SseEvent, TurnRequest};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;

use owo_agent_server::{AppState, PendingApproval};

fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

pub(super) fn auto_approve_enabled() -> bool {
    std::env::var("OWO_AUTO_APPROVE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

struct ChannelApprover {
    pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    pending_sessions: Arc<Mutex<HashMap<String, String>>>,
    session_id: String,
    abort: Arc<AtomicBool>,
}

impl ChannelApprover {
    fn spawn_request(
        &self,
        request: &PermissionRequest,
    ) -> tokio::sync::oneshot::Receiver<Decision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request.request_id.clone(), (tx, request.clone()));
        }
        if let Ok(mut sessions) = self.pending_sessions.lock() {
            sessions.insert(request.request_id.clone(), self.session_id.clone());
        }
        rx
    }
}

#[async_trait::async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        if auto_approve_enabled() {
            return Decision::Allow;
        }
        let rx = self.spawn_request(request);
        let mut rx = rx;
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(300));
        tokio::pin!(deadline);
        let decision = loop {
            tokio::select! {
                result = &mut rx => break result.unwrap_or(Decision::Deny),
                _ = &mut deadline => break Decision::Deny,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if self.abort.load(Ordering::Relaxed) {
                        break Decision::Deny;
                    }
                }
            }
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request.request_id);
        }
        if let Ok(mut sessions) = self.pending_sessions.lock() {
            sessions.remove(&request.request_id);
        }
        decision
    }
}

fn to_sse(event: &owo_agent_core::TurnEvent) -> Option<SseEvent> {
    match event {
        owo_agent_core::TurnEvent::ModelCall => Some(SseEvent::Progress {
            message: "模型调用".to_string(),
        }),
        owo_agent_core::TurnEvent::TokenDelta { delta } => Some(SseEvent::TokenDelta {
            delta: delta.clone(),
        }),
        owo_agent_core::TurnEvent::Compaction { summary } => Some(SseEvent::Compaction {
            summary: summary.clone(),
        }),
        owo_agent_core::TurnEvent::PermissionRequest(request) => {
            let explain = owo_agent_core::permissions::describe_request(request);
            Some(SseEvent::PermissionRequest {
                request_id: request.request_id.clone(),
                tool: request.tool.clone(),
                args: request.args.clone(),
                reason: request.reason.clone(),
                redacted_args: request.redacted_args.clone(),
                level: Some(request.level.label().to_string()),
                risk_note: request.risk_note.clone(),
                explain: Some(explain),
            })
        }
        owo_agent_core::TurnEvent::ToolStart { id, tool } => Some(SseEvent::ToolUse {
            id: id.clone(),
            tool: tool.clone(),
            args: Value::Null,
        }),
        owo_agent_core::TurnEvent::ToolResult {
            id,
            tool,
            ok,
            error,
        } => Some(SseEvent::ToolResult {
            id: id.clone(),
            tool: tool.clone(),
            ok: *ok,
            error: error.clone(),
        }),
        owo_agent_core::TurnEvent::Final { text } => Some(SseEvent::Final { text: text.clone() }),
    }
}

fn to_event(sse: SseEvent) -> Result<Event, Infallible> {
    let name = match &sse {
        SseEvent::Progress { .. } => "progress",
        SseEvent::ToolUse { .. } => "tool_use",
        SseEvent::ToolResult { .. } => "tool_result",
        SseEvent::PermissionRequest { .. } => "permission_request",
        SseEvent::Final { .. } => "final",
        SseEvent::TokenDelta { .. } => "token_delta",
        SseEvent::Compaction { .. } => "compaction",
    };
    // R10：SSE 事件统一携带协议版本 v（见 protocol::SSE_PROTOCOL_VERSION）。
    let mut payload = serde_json::to_value(&sse).unwrap_or_else(|_| json!({}));
    if let serde_json::Value::Object(map) = &mut payload {
        map.insert(
            "v".to_string(),
            json!(owo_agent_protocol::SSE_PROTOCOL_VERSION),
        );
    }
    let data = payload.to_string();
    Ok(Event::default().event(name).data(data))
}

pub(super) async fn turn(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<TurnRequest>,
) -> Result<Sse<UnboundedReceiverStream<Result<Event, Infallible>>>, (StatusCode, String)> {
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

    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();
    let abort_flag = {
        let mut aborts = state.aborts.lock().map_err(poison)?;
        aborts
            .entry(id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };
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
    tokio::spawn(async move {
        let _turn_guard = turn_guard;
        let _concurrency_permit = concurrency_permit;
        let mut current = session;
        let stream_abort = Arc::clone(&abort_flag);
        let mut tool_starts: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();
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
                if tx.send(to_event(sse)).is_err() {
                    // 客户端断开后尽快停止后续模型/工具调用，避免无主任务继续消耗资源。
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
                crate::logging::error(
                    "agent",
                    None,
                    "回合执行失败",
                    &[("session_id", serde_json::json!(current.id))],
                );
                let _ = tx.send(to_event(SseEvent::Progress {
                    message: format!("turn failed: {error}"),
                }));
            }
        }
        if let Ok(mut sessions) = sessions.lock() {
            sessions.insert(current.id.clone(), current.clone());
        }
        if let Err(error) = store.save(&current) {
            let _ = tx.send(to_event(SseEvent::Progress {
                message: format!("session save failed: {error}"),
            }));
        }
        if let Ok(mut aborts) = state_for_audit.aborts.lock() {
            if aborts
                .get(&current.id)
                .is_some_and(|registered| Arc::ptr_eq(registered, &abort_flag))
            {
                aborts.remove(&current.id);
            }
        }
        crate::audit_api::flush_audit(&state_for_audit);
    });

    Ok(Sse::new(UnboundedReceiverStream::new(rx)))
}

pub(super) async fn respond_permission(
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
