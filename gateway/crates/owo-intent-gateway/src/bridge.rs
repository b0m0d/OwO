//! IME ↔ agent-server 桥接：turn 异步适配与命令执行（E1.4）。
//!
//! 数据流：管道请求 → [`ImeState::dispatch`]（纯状态决策）→
//! - `Respond`：直接回包；
//! - `StartTurn`：spawn 本模块的 [`run_turn_task`]（HTTP 无工具表达路由），
//!   先把 `thinking` 立即回包；turn 完成后经 `complete_turn` 写入槽位；
//! - `RunCommand`：执行真实动作（回滚 / 打开可信界面 / 任务提交）。
//!
//! Cancellation drops the HTTP request and prevents late output from being published.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::commands;
use crate::http::{BridgeError, OwoHttpClient};
use crate::protocol::{AgentIpcRequest, AgentIpcResponse, Status};
use crate::state::{CommandAction, ImeState, PermissionNotice, StartTurnRequest, StateAction};

/// 最终消息字符上限（协议 `message` maxLength 16384）。
const MESSAGE_LIMIT: usize = 16_000;

/// 桥接器：管道帧处理器 + HTTP 回环客户端 + 会话映射。
pub struct ImeBridge {
    state: Arc<ImeState>,
    client: Arc<OwoHttpClient>,
    workspace: String,
    web_url: String,
    /// IME 会话 → agent-server 会话 ID。
    sessions: Arc<Mutex<HashMap<String, String>>>,
}

impl ImeBridge {
    pub fn new(
        state: Arc<ImeState>,
        client: Arc<OwoHttpClient>,
        workspace: impl Into<String>,
        web_url: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            client,
            workspace: workspace.into(),
            web_url: web_url.into(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 处理一帧请求载荷，返回响应载荷（`None` = 不回包直接断开）。
    ///
    /// 非法请求照官方 mock 行为：记录并断连（无法回填 session_id/request_id，不伪造响应）。
    pub async fn handle_frame(&self, payload: Vec<u8>) -> Option<Vec<u8>> {
        self.handle_frame_with_origin(payload, None).await
    }
    async fn handle_frame_with_origin(
        &self,
        payload: Vec<u8>,
        origin: Option<crate::source_capsule::VerifiedOrigin>,
    ) -> Option<Vec<u8>> {
        if origin.as_ref().is_some_and(|e| !e.matches(&payload))
            || (self.client.requires_origin() && origin.is_none())
        {
            return None;
        }
        let request = match AgentIpcRequest::decode(&payload) {
            Ok(request) => request,
            Err(error) => {
                warn!(%error, "IME 请求解码失败，拒绝并断开");
                return None;
            }
        };
        debug!(
            action = ?request.action,
            session = %request.session_id,
            request_id = %request.request_id,
            "收到 IME 请求"
        );

        let response = match self.state.dispatch(&request) {
            StateAction::Respond(response) => response,
            StateAction::StartTurn { immediate, turn } => {
                let capsule = if turn.agent_task {
                    self.client.issue_task_origin(
                        origin.as_ref(),
                        &request,
                        turn.prompt.as_deref().unwrap_or(""),
                    )
                } else {
                    self.client.issue_origin(
                        origin.as_ref(),
                        &request,
                        turn.prompt.as_deref().unwrap_or(""),
                    )
                };
                tokio::spawn(run_turn_task_with_capsule(
                    Arc::clone(&self.state),
                    Arc::clone(&self.client),
                    Arc::clone(&self.sessions),
                    self.workspace.clone(),
                    turn,
                    capsule,
                ));
                immediate
            }
            StateAction::RunCommand { session_id, action } => {
                self.run_command(&request, &session_id, action).await
            }
        };

        match response.encode() {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                error!(%error, "IME 响应编码失败，断连");
                None
            }
        }
    }

    async fn open_session_ui(&self, ime_session: &str, url: &str) {
        if std::env::var("OWO_GATEWAY_DESKTOP_LINK").as_deref() == Ok("1") {
            if let Some(session) = self.sessions.lock().await.get(ime_session) {
                println!(
                    "OWO_DESKTOP_OPEN_SESSION {}",
                    serde_json::json!({"session_id":session})
                );
            }
        } else {
            open_in_browser(url);
        }
    }
    async fn session_url(&self, ime_session: &str) -> String {
        let mut url = match reqwest::Url::parse(&self.web_url) {
            Ok(url) => url,
            Err(_) => return self.web_url.clone(),
        };
        if let Some(session) = self.sessions.lock().await.get(ime_session) {
            url.query_pairs_mut().append_pair("session", session);
        }
        url.to_string()
    }
    /// 执行命令动作（真实 IO）。
    async fn run_command(
        &self,
        request: &AgentIpcRequest,
        ime_session_id: &str,
        action: CommandAction,
    ) -> AgentIpcResponse {
        match action {
            CommandAction::RevertAll => {
                let agent_session = self.sessions.lock().await.get(ime_session_id).cloned();
                let Some(agent_session) = agent_session else {
                    return self.state.complete_command(
                        ime_session_id,
                        request,
                        commands::error_response(request, "session_not_found", "会话不存在"),
                    );
                };
                match self.client.revert(&agent_session).await {
                    Ok(()) => {
                        let mut response = commands::base_response(request, Status::Completed);
                        response.message = "已撤销全部改动".to_string();
                        self.state
                            .complete_command(ime_session_id, request, response)
                    }
                    Err(error) => {
                        warn!(%error, "revert 调用失败");
                        self.state.complete_command(
                            ime_session_id,
                            request,
                            commands::error_response(
                                request,
                                "revert_failed",
                                format!("撤销失败：{error}"),
                            ),
                        )
                    }
                }
            }
            CommandAction::OpenConfirmUi { request_id } => {
                let url = self.session_url(ime_session_id).await;
                info!(%request_id, %url, "打开 Agent 可信界面");
                self.open_session_ui(ime_session_id, &url).await;
                let mut response = commands::base_response(request, Status::WaitingForConfirmation);
                response.message = format!("请在 Agent 可信界面完成确认（已请求打开 {url}）");
                response.can_cancel = true;
                self.state
                    .complete_command(ime_session_id, request, response)
            }
            CommandAction::OpenWorkbench => {
                let url = self.session_url(ime_session_id).await;
                info!(%url, "打开 Agent 工作台");
                self.open_session_ui(ime_session_id, &url).await;
                let mut response = commands::base_response(request, Status::Completed);
                response.message = format!("已打开 Agent 工作台，请在其中继续处理任务（{url}）");
                self.state
                    .complete_command(ime_session_id, request, response)
            }
            CommandAction::CommitTask => {
                // 状态机已转为 StartTurn；此处兜底（理论上不可达）。
                self.state.complete_command(
                    ime_session_id,
                    request,
                    commands::error_response(request, "internal", "任务提交路径异常"),
                )
            }
            CommandAction::InsertReply { text } => {
                // 状态机已内联处理；此处兜底（理论上不可达）。
                let mut response = commands::base_response(request, Status::Completed);
                response.message = text;
                self.state
                    .complete_command(ime_session_id, request, response)
            }
        }
    }
}

/// turn 后台任务：确保会话 → 发 turn → 消费 SSE → 写回结果。
pub async fn run_turn_task(
    state: Arc<ImeState>,
    client: Arc<OwoHttpClient>,
    sessions: Arc<Mutex<HashMap<String, String>>>,
    workspace: String,
    turn: Box<StartTurnRequest>,
) {
    run_turn_task_with_capsule(state, client, sessions, workspace, turn, Ok(None)).await;
}
async fn run_turn_task_with_capsule(
    state: Arc<ImeState>,
    client: Arc<OwoHttpClient>,
    sessions: Arc<Mutex<HashMap<String, String>>>,
    workspace: String,
    turn: Box<StartTurnRequest>,
    capsule: Result<Option<owo_agent_protocol::ExpressionOriginCapsule>, BridgeError>,
) {
    let ime_session_id = turn.session_id.clone();
    let request = turn.request.clone();

    let outcome = match capsule {
        Ok(capsule) => {
            if turn.agent_task {
                execute_agent_task(&client, &sessions, &turn, capsule).await
            } else {
                execute_turn(&client, &sessions, &workspace, &turn, capsule).await
            }
        }
        Err(error) => Err(error),
    };
    let (mut final_response, insert_text) = match outcome {
        Ok(accumulator) => build_final_response(&request, &accumulator),
        Err(error) => {
            warn!(session = %ime_session_id, "IME request failed; no automatic resubmission");
            let (code, message) = match &error {
                BridgeError::Status { status: 403, body }
                    if body.contains("ime/access_disabled") =>
                {
                    (
                        "ime_disabled",
                        "输入法任务调用已关闭，请在 Agent Desktop 的输入法设置中开启",
                    )
                }
                BridgeError::Status { status: 400, body } if body.contains("ime/workspace") => (
                    "ime_workspace_unavailable",
                    "任务工作目录不可用，请在输入法设置中重新选择",
                ),
                BridgeError::Status { status: 403, .. } => (
                    "ime_origin_rejected",
                    "连接验证未通过，请在 Agent Desktop 中重新连接输入法",
                ),
                _ => (
                    "agent_request_failed",
                    "请求失败或连接中断，任务受理状态可能未知；不会自动重发，请在 Agent 会话中检查",
                ),
            };
            (commands::error_response(&request, code, message), None)
        }
    };
    let mut actions = HashMap::new();
    if let Some(text) = insert_text {
        let mut command = commands::insert_reply_command(&text);
        commands::attach_commit_effect(&mut command, &request, &text);
        actions.insert(command.id.clone(), CommandAction::InsertReply { text });
        final_response.commands.push(command);
    }
    if turn.agent_task && sessions.lock().await.contains_key(&ime_session_id) {
        let mut command = commands::command(
            "open-agent",
            "查看 Agent 任务",
            "agent.open",
            crate::protocol::RiskLevel::None,
        );
        command.preview = "查看任务进度、审批与执行结果".into();
        actions.insert(command.id.clone(), CommandAction::OpenWorkbench);
        final_response.commands.push(command);
    }
    let _ = state.complete_expression(&ime_session_id, &turn.slot, final_response, actions);
}

/// Submit once; subsequent network activity only reads durable events.
async fn execute_agent_task(
    client: &OwoHttpClient,
    sessions: &Mutex<HashMap<String, String>>,
    turn: &StartTurnRequest,
    capsule: Option<owo_agent_protocol::ExpressionOriginCapsule>,
) -> Result<TurnAccumulator, BridgeError> {
    use owo_agent_protocol::{SseEvent, TurnReplayState};
    let prompt = turn
        .prompt
        .as_deref()
        .ok_or_else(|| BridgeError::Protocol("missing task prompt".into()))?;
    let capsule = capsule
        .ok_or_else(|| BridgeError::Protocol("authenticated task origin required".into()))?;
    // Do not drop admission on cancellation: the receipt is needed to abort an accepted task.
    let receipt = client.submit_ime_task(prompt, capsule).await?;
    sessions
        .lock()
        .await
        .insert(turn.session_id.clone(), receipt.session_id.clone());
    let mut result = TurnAccumulator::default();
    let mut after = 0;
    loop {
        let page = tokio::select! {
            biased;
            _ = turn.slot.detached() => { result.backgrounded = true; return Ok(result); },
            _ = turn.slot.cancelled() => {
                client.abort_turn(&receipt.session_id, &receipt.turn_id).await?;
                result.cancelled = true;
                return Ok(result);
            },
            page = client.replay_turn(&receipt.session_id, &receipt.turn_id, after) => page?,
        };
        for record in page.events {
            if record.turn_id != receipt.turn_id || record.seq <= after {
                continue;
            }
            after = record.seq;
            match record.payload {
                SseEvent::Final { text } => result.final_text = text,
                SseEvent::ToolUse { .. } => result.tools_used += 1,
                SseEvent::PermissionRequest {
                    request_id,
                    tool,
                    reason,
                    ..
                } => {
                    result.permission = Some(PermissionNotice {
                        request_id,
                        tool,
                        reason,
                    });
                }
                SseEvent::UserQuestion {
                    question_id,
                    question,
                    ..
                } => {
                    result.permission = Some(PermissionNotice {
                        request_id: question_id,
                        tool: "需要补充信息".into(),
                        reason: question,
                    });
                }
                SseEvent::PermissionResolved { request_id, .. } => {
                    if result
                        .permission
                        .as_ref()
                        .is_some_and(|n| n.request_id == request_id)
                    {
                        result.permission = None;
                    }
                }
                SseEvent::UserAnswered { .. } => result.permission = None,
                SseEvent::TurnFailed { message, .. } => result.failed = Some(message),
                _ => {}
            }
        }
        {
            let mut slot = turn.slot.lock();
            slot.tools_used = result.tools_used;
            slot.permission_waiting = result.permission.clone();
        }
        if !page.active {
            if page.state == TurnReplayState::Interrupted {
                result.failed = Some("Agent 已中断，请在桌面查看与恢复任务".into());
            }
            return Ok(result);
        }
        tokio::select! {
            biased;
            _ = turn.slot.detached() => { result.backgrounded = true; return Ok(result); },
            _ = turn.slot.cancelled() => {
                client.abort_turn(&receipt.session_id, &receipt.turn_id).await?;
                result.cancelled = true; return Ok(result);
            },
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
        }
    }
}

/// Execute one stateless expression request; cancellation discards its response.
async fn execute_turn(
    client: &OwoHttpClient,
    _sessions: &Mutex<HashMap<String, String>>,
    _workspace: &str,
    turn: &StartTurnRequest,
    capsule: Option<owo_agent_protocol::ExpressionOriginCapsule>,
) -> Result<TurnAccumulator, BridgeError> {
    let prompt = turn
        .prompt
        .as_deref()
        .ok_or_else(|| BridgeError::Protocol("non-expression input must open Agent UI".into()))?;
    let accumulator = tokio::select! {
        biased;
        _ = turn.slot.cancelled() => TurnAccumulator { cancelled: true, ..Default::default() },
        result = client.expression_with_capsule(prompt,capsule) => TurnAccumulator { final_text: result?, ..Default::default() },
    };
    Ok(accumulator)
}

/// turn 过程累积器。
#[derive(Debug, Default)]
pub struct TurnAccumulator {
    pub final_text: String,
    pub failed: Option<String>,
    pub tools_used: usize,
    pub permission: Option<PermissionNotice>,
    pub cancelled: bool,
    pub backgrounded: bool,
}

/// 由累积器构造最终 IME 响应。
///
/// 返回 `(响应, 可选 insert-reply 文本)`。
fn build_final_response(
    request: &AgentIpcRequest,
    accumulator: &TurnAccumulator,
) -> (AgentIpcResponse, Option<String>) {
    if accumulator.backgrounded {
        let mut response = commands::base_response(request, Status::Completed);
        response.message = "任务已在 Agent 工作台继续执行".into();
        return (response, None);
    }
    if accumulator.cancelled {
        let mut response = commands::base_response(request, Status::Cancelled);
        response.message = "任务已取消".to_string();
        return (response, None);
    }
    if let Some(message) = &accumulator.failed {
        let mut response = commands::error_response(
            request,
            "agent_failed",
            commands::truncate_chars(message, 4096),
        );
        response.retryable = true;
        return (response, None);
    }
    let text = accumulator.final_text.trim();
    let mut response = commands::base_response(request, Status::AgentMode);
    response.message = if text.is_empty() {
        "任务已完成（无文本输出）".to_string()
    } else {
        commands::truncate_chars(text, MESSAGE_LIMIT)
    };
    let insert_text = (!text.is_empty()).then(|| accumulator.final_text.clone());
    (response, insert_text)
}

/// 从 IME 请求构造 agent prompt（正文 + 简短输入法上下文）。
///
/// 隐私红线（协议 agent-ipc-privacy.md）：`application.sensitive_input == true`
/// （密码框等）时**不附带任何上下文、也不推断应用身份**，只发用户输入正文。
pub fn compose_prompt(request: &AgentIpcRequest) -> String {
    let mut prompt = String::new();
    let base = if !request.user_input.is_empty() {
        request.user_input.as_str()
    } else {
        request.input.natural_language.as_str()
    };
    prompt.push_str(base);

    if request.application.sensitive_input {
        return prompt;
    }

    let mut context_lines = Vec::new();
    if !request.input.selected_text.is_empty() {
        context_lines.push(format!(
            "已选文字：{}",
            commands::truncate_chars(&request.input.selected_text, 200)
        ));
    }
    if !request.application.executable.is_empty() {
        context_lines.push(format!("当前应用：{}", request.application.executable));
    }
    if !request.session_context.is_empty() {
        context_lines.push(format!(
            "输入位置上下文：{}",
            commands::truncate_chars(&request.session_context, 800)
        ));
    }
    if !request.input.pending_pinyin.is_empty() {
        context_lines.push(format!("未消耗拼音：{}", request.input.pending_pinyin));
    }
    // context_entries：协议保证按旧到新排列；逐条限长防提示词膨胀。
    for entry in &request.context_entries {
        if entry.text.is_empty() {
            continue;
        }
        let app_hint = if entry.application.executable.is_empty() {
            String::new()
        } else {
            format!("（{}）", entry.application.executable)
        };
        context_lines.push(format!(
            "历史上下文{app_hint}：{}",
            commands::truncate_chars(&entry.text, 200)
        ));
    }
    if !context_lines.is_empty() {
        prompt.push_str("\n\n[输入法上下文]\n");
        prompt.push_str(&context_lines.join("\n"));
    }
    prompt
}

/// 打开默认浏览器（Windows）。
pub fn open_in_browser(url: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(windows)]
#[async_trait::async_trait]
impl crate::pipe::FrameHandler for ImeBridge {
    async fn handle(&self, payload: Vec<u8>) -> Option<Vec<u8>> {
        self.handle_frame(payload).await
    }
    async fn handle_verified(
        &self,
        payload: Vec<u8>,
        origin: crate::source_capsule::VerifiedOrigin,
    ) -> Option<Vec<u8>> {
        self.handle_frame_with_origin(payload, Some(origin)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> AgentIpcRequest {
        AgentIpcRequest {
            schema_version: 3,
            action: crate::protocol::Action::Submit,
            session_id: "0123456789abcdef0123456789abcdef".to_string(),
            request_id: "req-1".to_string(),
            parent_request_id: String::new(),
            idempotency_key: "req-1".to_string(),
            capabilities: vec![],
            protocol_min: 2,
            protocol_max: 3,
            required_features: vec![],
            user_input: "帮我找文件".to_string(),
            input: crate::protocol::InputView {
                selected_text: "选中文字".to_string(),
                ..Default::default()
            },
            application: crate::protocol::ApplicationView {
                executable: "notepad.exe".to_string(),
                ..Default::default()
            },
            session_context: "附近的输入上下文".to_string(),
            context_entries: vec![],
            selection: None,
            command_id: String::new(),
            page: 0,
            task_revision: 0,
            slot_updates: vec![],
        }
    }

    #[test]
    fn prompt_includes_context_by_default() {
        let prompt = compose_prompt(&base_request());
        assert!(prompt.starts_with("帮我找文件"));
        assert!(prompt.contains("notepad.exe"));
        assert!(prompt.contains("附近的输入上下文"));
        assert!(prompt.contains("选中文字"));
    }

    #[test]
    fn sensitive_input_strips_all_context() {
        // 协议隐私红线：密码框等敏感输入不得携带/推断上下文。
        let mut request = base_request();
        request.application.sensitive_input = true;
        request.session_context = "秘密上下文".to_string();
        request.input.selected_text = "敏感选中".to_string();
        request.context_entries = vec![crate::protocol::ContextEntry {
            context_id: "ctx-1".to_string(),
            application: crate::protocol::ApplicationView::default(),
            text: "历史秘密".to_string(),
            sequence: 1,
            context_type: "committed_text".to_string(),
            source: "owo_commit".to_string(),
            created_at_ms: 0,
            relevance_milli: 500,
            is_current: false,
            privacy: crate::protocol::PrivacyFlags::default(),
        }];
        let prompt = compose_prompt(&request);
        assert_eq!(prompt, "帮我找文件", "敏感输入时只允许发送用户正文");
        assert!(!prompt.contains("秘密"));
        assert!(!prompt.contains("notepad.exe"));
    }

    #[test]
    fn context_entries_appended_in_order() {
        let mut request = base_request();
        request.session_context = String::new();
        request.input.selected_text = String::new();
        request.context_entries = vec![
            crate::protocol::ContextEntry {
                context_id: "ctx-1".to_string(),
                application: crate::protocol::ApplicationView {
                    executable: "word.exe".to_string(),
                    ..Default::default()
                },
                text: "第一段".to_string(),
                sequence: 1,
                context_type: "committed_text".to_string(),
                source: "owo_commit".to_string(),
                created_at_ms: 0,
                relevance_milli: 500,
                is_current: false,
                privacy: crate::protocol::PrivacyFlags::default(),
            },
            crate::protocol::ContextEntry {
                context_id: "ctx-2".to_string(),
                application: crate::protocol::ApplicationView::default(),
                text: "第二段".to_string(),
                sequence: 2,
                context_type: "committed_text".to_string(),
                source: "owo_commit".to_string(),
                created_at_ms: 0,
                relevance_milli: 600,
                is_current: false,
                privacy: crate::protocol::PrivacyFlags::default(),
            },
        ];
        let prompt = compose_prompt(&request);
        let first = prompt.find("第一段").expect("第一段必须在");
        let second = prompt.find("第二段").expect("第二段必须在");
        assert!(first < second, "上下文必须按旧到新排列");
        assert!(prompt.contains("word.exe"));
    }

    #[test]
    fn long_context_is_truncated() {
        let mut request = base_request();
        request.session_context = "很".repeat(2000);
        let prompt = compose_prompt(&request);
        // 800 字符上限 + 省略号：总长必须受控。
        assert!(prompt.chars().count() < 1200, "上下文必须限长");
    }
    #[tokio::test]
    async fn agent_task_replays_progress_opens_session_and_cancels_exact_turn() {
        use axum::{
            extract::{Query, State},
            routing::{get, post},
            Json, Router,
        };
        use serde_json::{json, Value};
        use std::sync::atomic::{AtomicUsize, Ordering};
        #[derive(Default)]
        struct Probe {
            submitted: AtomicUsize,
            phase: AtomicUsize,
            aborted: AtomicUsize,
        }
        async fn submit(State(p): State<Arc<Probe>>, Json(body): Json<Value>) -> Json<Value> {
            assert_eq!(body["prompt"], "create a fixture file");
            assert_eq!(
                body["origin_capsule"]["payload"]["scope"]["mode"],
                "agent_task"
            );
            p.submitted.fetch_add(1, Ordering::SeqCst);
            Json(
                json!({"session_id":"11111111-1111-4111-8111-111111111111","turn_id":"22222222-2222-4222-8222-222222222222","origin_verified":true,"status":"accepted"}),
            )
        }
        async fn events(
            State(p): State<Arc<Probe>>,
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            assert_eq!(query["turn_id"], "22222222-2222-4222-8222-222222222222");
            let after = query["after_seq"].parse::<u64>().unwrap();
            let mut events = Vec::new();
            let record = |seq, payload| json!({"session_id":"11111111-1111-4111-8111-111111111111","turn_id":"22222222-2222-4222-8222-222222222222","seq":seq,"created_at":"fixture","payload":payload});
            if after < 1 {
                events.push(record(1,json!({"type":"permission_request","request_id":"fixture-approval","tool":"write_file","args":{},"reason":"write fixture"})));
            }
            let done = p.phase.load(Ordering::SeqCst) > 0;
            if done && after < 2 {
                events.push(record(2,json!({"type":"permission_resolved","request_id":"fixture-approval","source":"user","allowed":true})));
            }
            if done && after < 3 {
                events.push(record(
                    3,
                    json!({"type":"final","text":"fixture completed"}),
                ));
            }
            Json(
                json!({"events":events,"active":!done,"state":if done {"completed"}else{"active"},"next_after_seq":if done {3}else{1}}),
            )
        }
        async fn abort(State(p): State<Arc<Probe>>, Json(body): Json<Value>) -> Json<Value> {
            assert_eq!(body["turn_id"], "22222222-2222-4222-8222-222222222222");
            p.aborted.fetch_add(1, Ordering::SeqCst);
            Json(json!({"ok":true}))
        }
        for cancelled in [false, true] {
            let probe = Arc::new(Probe::default());
            let app = Router::new()
                .route("/ime/tasks", post(submit))
                .route("/session/{id}/turn/events", get(events))
                .route("/session/{id}/abort", post(abort))
                .with_state(probe.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let file =
                std::env::temp_dir().join(format!("owo-task-token-{}", uuid::Uuid::new_v4()));
            std::fs::write(&file, "fixture-token").unwrap();
            let client = Arc::new(OwoHttpClient::new(base, &file).unwrap());
            let state = Arc::new(ImeState::new(std::time::Duration::from_secs(60)));
            let mut request = base_request();
            request.input.selected_text = "create a fixture file".into();
            request.user_input = "create a fixture file".into();
            let draft = match state.dispatch(&request) {
                StateAction::Respond(r) => r,
                _ => panic!("draft"),
            };
            request.action = crate::protocol::Action::Select;
            request.command_id = "task-commit".into();
            request.task_revision = draft.task.revision;
            request.idempotency_key = "task-fixture-commit".into();
            let mut turn = match state.dispatch(&request) {
                StateAction::StartTurn { turn, .. } => turn,
                _ => panic!("commit"),
            };
            turn.prompt = Some("create a fixture file".into());
            assert!(turn.agent_task);
            let pending = turn.slot.clone();
            let sessions = Arc::new(Mutex::new(HashMap::new()));
            let fixture: Value = serde_json::from_str(include_str!(
                "../../../../contracts/expression-origin/synthetic-golden.json"
            ))
            .unwrap();
            let mut capsule: owo_agent_protocol::ExpressionOriginCapsule =
                serde_json::from_value(fixture["capsule"].clone()).unwrap();
            capsule.payload.scope.mode = "agent_task".into();
            let job = tokio::spawn(run_turn_task_with_capsule(
                state.clone(),
                client,
                sessions.clone(),
                String::new(),
                turn,
                Ok(Some(capsule)),
            ));
            for _ in 0..100 {
                if pending.lock().permission_waiting.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                pending.lock().permission_waiting.as_ref().unwrap().tool,
                "write_file"
            );
            let mut poll = request.clone();
            poll.action = crate::protocol::Action::Poll;
            poll.command_id.clear();
            poll.idempotency_key = "poll-fixture".into();
            let waiting = match state.dispatch(&poll) {
                StateAction::Respond(r) => r,
                _ => panic!("poll"),
            };
            assert_eq!(waiting.status, Status::WaitingForConfirmation);
            assert!(waiting
                .commands
                .iter()
                .all(|c| c.category != "permission.allow"));
            if cancelled {
                pending.request_cancel();
            } else {
                probe.phase.store(1, Ordering::SeqCst);
            }
            tokio::time::timeout(std::time::Duration::from_secs(3), job)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(probe.submitted.load(Ordering::SeqCst), 1);
            assert_eq!(probe.aborted.load(Ordering::SeqCst), usize::from(cancelled));
            assert!(pending.lock().done);
            let slot = pending.lock();
            let response = slot.response.as_ref().unwrap();
            if cancelled {
                assert_eq!(response.status, Status::Cancelled);
            } else {
                assert_eq!(response.message, "fixture completed");
                assert!(response.commands.iter().any(|c| c.id == "open-agent"));
            }
            drop(slot);
            server.abort();
            std::fs::remove_file(file).unwrap();
        }
    }
}
