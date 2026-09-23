//! SSE 解析：`data:` 行 → `SseEvent`，跨 chunk 的 UTF-8 与半包安全。
//!
//! 关键点（指南 §4.2 测试要求）：必须缓冲**字节**而不是字符串——多字节字符可能被
//! TCP 分片切断，`from_utf8_lossy` 直接作用在半包上会插入替换字符、损坏文本。
//! 这里只在遇到 `\n`（ASCII 行边界）后才解码整行，因此每行都是完整 UTF-8。

use crate::error::{ClientError, Result};
use crate::http::AgentClient;
use owo_agent_protocol::{SseEvent, TurnEventRecord, TurnEventReplayPage, TurnReplayState};
use std::collections::VecDeque;
use std::time::Duration;

/// 解析一行 SSE：仅 `data:` 行产出事件；其余（`event:`/`id:`/注释/空行）返回 None。
pub fn parse_sse_data_line(line: &str) -> Option<Result<SseEvent>> {
    let line = line.trim_end_matches(['\r', '\n']);
    let data = line.strip_prefix("data:")?;
    let data = data.trim_start();
    if data.is_empty() {
        return None;
    }
    Some(
        serde_json::from_str::<SseEvent>(data)
            .map_err(|error| ClientError::Protocol(format!("SSE 事件解析失败：{error}：{data}"))),
    )
}

/// 与网络无关的 SSE 字节缓冲（可离线单测半包/UTF-8 边界）。
#[derive(Debug, Default)]
pub struct SseBuffer {
    buffer: Vec<u8>,
    finished: bool,
    last_event_id: Option<String>,
}

/// One parsed SSE frame, including the standard EventSource replay cursor.
#[derive(Debug)]
pub struct SseFrame {
    pub id: Option<String>,
    pub event: SseEvent,
}

impl SseBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一段原始字节。
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// 标记流结束（此时残余 buffer 按最后一行处理）。
    pub fn finish(&mut self) {
        self.finished = true;
    }

    /// 取出下一个可解析事件；无完整行且未结束时返回 None。
    pub fn next_event(&mut self) -> Option<Result<SseEvent>> {
        self.next_frame()
            .map(|frame| frame.map(|frame| frame.event))
    }

    /// 取出事件及其最近的 SSE `id:` 游标，供断线恢复使用。
    pub fn next_frame(&mut self) -> Option<Result<SseFrame>> {
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(id) = parse_sse_id_line(&line) {
                self.last_event_id = Some(id);
                continue;
            }
            if let Some(event) = parse_sse_data_line(&line) {
                return Some(event.map(|event| SseFrame {
                    id: self.last_event_id.clone(),
                    event,
                }));
            }
        }
        if self.finished && !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).to_string();
            self.buffer.clear();
            if let Some(id) = parse_sse_id_line(&line) {
                self.last_event_id = Some(id);
                return None;
            }
            return parse_sse_data_line(&line).map(|event| {
                event.map(|event| SseFrame {
                    id: self.last_event_id.clone(),
                    event,
                })
            });
        }
        None
    }
}

fn parse_sse_id_line(line: &str) -> Option<String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let value = line.strip_prefix("id:")?;
    let value = value.strip_prefix(' ').unwrap_or(value);
    if value.contains('\0') {
        return None;
    }
    Some(value.to_string())
}

/// turn 事件流：底层是 reqwest 响应，逐事件拉取。
pub struct TurnStream {
    response: reqwest::Response,
    buffer: SseBuffer,
    client: AgentClient,
    session_id: String,
    turn_id: Option<String>,
    last_seq: u64,
    pending: VecDeque<TurnEventRecord>,
    stream_ended: bool,
    terminal: bool,
    terminal_after_pending: bool,
    replay_state: Option<TurnReplayState>,
    stream_error: Option<String>,
}

impl TurnStream {
    pub(crate) fn new(
        response: reqwest::Response,
        client: AgentClient,
        session_id: String,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            response,
            buffer: SseBuffer::new(),
            client,
            session_id,
            turn_id,
            last_seq: 0,
            pending: VecDeque::new(),
            stream_ended: false,
            terminal: false,
            terminal_after_pending: false,
            replay_state: None,
            stream_error: None,
        }
    }

    /// 下一个事件：断线后按持久 session seq 补拉，`None` = turn 已终止。
    pub async fn next_event(&mut self) -> Option<Result<SseEvent>> {
        loop {
            if self.terminal {
                return None;
            }
            if let Some(record) = self.pending.pop_front() {
                self.last_seq = self.last_seq.max(record.seq);
                if matches!(&record.payload, SseEvent::Final { .. }) {
                    self.terminal = true;
                    return Some(Ok(record.payload));
                }
                if let Some(message) = turn_failure_message(&record.payload) {
                    self.terminal = true;
                    return Some(Err(ClientError::Protocol(message.to_string())));
                }
                return Some(Ok(record.payload));
            }
            if self.terminal_after_pending {
                return self.finish_replay();
            }
            if let Some(frame) = self.buffer.next_frame() {
                return Some(frame.and_then(|frame| {
                    if let Some(id) = frame.id.as_deref().and_then(|id| id.parse::<u64>().ok()) {
                        self.last_seq = self.last_seq.max(id);
                    }
                    if matches!(&frame.event, SseEvent::Final { .. }) {
                        self.terminal = true;
                        return Ok(frame.event);
                    }
                    if let Some(message) = turn_failure_message(&frame.event) {
                        self.terminal = true;
                        return Err(ClientError::Protocol(message.to_string()));
                    }
                    Ok(frame.event)
                }));
            }
            if self.stream_ended {
                return self.next_replayed_event().await;
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self.buffer.push(&bytes),
                Ok(None) => {
                    self.buffer.finish();
                    self.stream_ended = true;
                }
                Err(error) => {
                    self.buffer.finish();
                    self.stream_error = Some(error.to_string());
                    self.stream_ended = true;
                }
            }
        }
    }

    async fn next_replayed_event(&mut self) -> Option<Result<SseEvent>> {
        let Some(turn_id) = self.turn_id.clone() else {
            self.terminal = true;
            return self
                .stream_error
                .take()
                .map(|error| Err(ClientError::Transport(error)));
        };
        loop {
            let path = format!(
                "/session/{}/turn/events?turn_id={}&after_seq={}&limit=256",
                self.session_id, turn_id, self.last_seq
            );
            let page: TurnEventReplayPage = match self.client.get_json(&path).await {
                Ok(page) => page,
                Err(error) => {
                    self.terminal = true;
                    return Some(Err(error));
                }
            };
            for record in page.events {
                if record.turn_id == turn_id && record.seq > self.last_seq {
                    self.pending.push_back(record);
                }
            }
            if !page.active {
                self.terminal_after_pending = true;
                self.replay_state = Some(page.state);
            }
            if let Some(record) = self.pending.pop_front() {
                self.last_seq = self.last_seq.max(record.seq);
                if matches!(&record.payload, SseEvent::Final { .. }) {
                    self.terminal = true;
                    return Some(Ok(record.payload));
                }
                if let Some(message) = turn_failure_message(&record.payload) {
                    self.terminal = true;
                    return Some(Err(ClientError::Protocol(message.to_string())));
                }
                return Some(Ok(record.payload));
            }
            if self.terminal_after_pending {
                return self.finish_replay();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn finish_replay(&mut self) -> Option<Result<SseEvent>> {
        self.terminal = true;
        match self.replay_state.take() {
            Some(TurnReplayState::Completed) => Some(Err(ClientError::Protocol(
                "回合标记为完成，但重放中缺少 final 事件".to_string(),
            ))),
            Some(TurnReplayState::Failed) => Some(Err(ClientError::Protocol(
                "回合执行失败，但重放中缺少失败详情".to_string(),
            ))),
            Some(TurnReplayState::Interrupted) => Some(Err(ClientError::Protocol(
                "回合在写入终态前中断；仅收到已持久化的部分事件".to_string(),
            ))),
            Some(TurnReplayState::Active) | None => Some(Err(ClientError::Protocol(
                "回合重放状态不一致：服务端仍报告活动状态".to_string(),
            ))),
        }
    }

    /// 驱动整轮：逐事件回调，直到流结束。返回错误即中止。
    pub async fn drive<F: FnMut(SseEvent)>(&mut self, mut on_event: F) -> Result<()> {
        while let Some(event) = self.next_event().await {
            on_event(event?);
        }
        Ok(())
    }
}

fn turn_failure_message(event: &SseEvent) -> Option<&str> {
    match event {
        SseEvent::Progress { message }
            if message.starts_with("turn failed:")
                || message.starts_with("session save failed:") =>
        {
            Some(message)
        }
        _ => None,
    }
}
