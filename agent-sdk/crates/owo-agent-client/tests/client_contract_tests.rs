//! `owo-agent-client` 契约测试：
//!   * SSE 缓冲的半包/UTF-8 边界（离线）；
//!   * 发现文件读取与陈旧 pid 拒绝；
//!   * 对**假 Daemon**（tokio 原生 TCP，无 server 依赖）的端到端往返。

use owo_agent_client::discovery::{descriptor_path, DaemonDiscovery};
use owo_agent_client::sse::{parse_sse_data_line, SseBuffer};
use owo_agent_client::{AgentClient, ClientConfig};
use owo_agent_protocol::{DaemonDescriptor, SseEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// 离线：SSE 半包与 UTF-8 边界
// ---------------------------------------------------------------------------

#[test]
fn sse_parses_event_split_across_chunks() {
    let mut buffer = SseBuffer::new();
    buffer.push(b"event: token_delta\ndata: {\"type\":\"token_delta\",\"de");
    assert!(buffer.next_event().is_none(), "半包不得产出事件");
    buffer.push(b"lta\":\"hi\",\"v\":1}\n\n");
    match buffer.next_event() {
        Some(Ok(SseEvent::TokenDelta { delta })) => assert_eq!(delta, "hi"),
        other => panic!("期望 token_delta，得到 {other:?}"),
    }
    assert!(buffer.next_event().is_none());
}

#[test]
fn sse_preserves_multibyte_utf8_split_across_chunks() {
    // "好" = E5 A5 BD；把它切在两个 chunk 之间。
    let mut buffer = SseBuffer::new();
    buffer.push(b"data: {\"type\":\"token_delta\",\"delta\":\"\xe5\xa5");
    buffer.push(b"\xbd\",\"v\":1}\n");
    match buffer.next_event() {
        Some(Ok(SseEvent::TokenDelta { delta })) => assert_eq!(delta, "好"),
        other => panic!("UTF-8 跨 chunk 被损坏：{other:?}"),
    }
}

#[test]
fn sse_keeps_standard_id_cursor_for_replay() {
    let mut buffer = SseBuffer::new();
    buffer
        .push(b"id: 41\nevent: token_delta\ndata: {\"type\":\"token_delta\",\"delta\":\"a\"}\n\n");
    let frame = buffer.next_frame().expect("complete SSE frame").unwrap();
    assert_eq!(frame.id.as_deref(), Some("41"));
    assert!(matches!(frame.event, SseEvent::TokenDelta { ref delta } if delta == "a"));
}

#[test]
fn sse_ignores_non_data_lines_and_parses_final() {
    assert!(parse_sse_data_line("event: final").is_none());
    assert!(parse_sse_data_line(": keep-alive").is_none());
    let parsed = parse_sse_data_line("data: {\"type\":\"final\",\"text\":\"done\",\"v\":1}");
    match parsed {
        Some(Ok(SseEvent::Final { text })) => assert_eq!(text, "done"),
        other => panic!("期望 final，得到 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 离线：发现文件
// ---------------------------------------------------------------------------

fn descriptor(pid: u32) -> DaemonDescriptor {
    DaemonDescriptor {
        pid,
        port: 4096,
        instance_id: String::new(),
        api_version: "0.7".to_string(),
        build_id: "abc".to_string(),
        started_at: "2026-09-21T00:00:00Z".to_string(),
        data_root: "Agent-deadbeef".to_string(),
    }
}

#[test]
fn discovery_reads_live_descriptor() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime")).unwrap();
    std::fs::write(
        descriptor_path(dir.path()),
        serde_json::to_string(&descriptor(std::process::id())).unwrap(),
    )
    .unwrap();
    let discovery = DaemonDiscovery::read(dir.path()).expect("read");
    assert_eq!(discovery.base_url(), "http://127.0.0.1:4096");
    discovery.validate_api_version("0.7").expect("version ok");
    assert!(discovery.validate_api_version("9.9").is_err());
}

#[test]
fn discovery_rejects_stale_dead_pid() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("runtime")).unwrap();
    std::fs::write(
        descriptor_path(dir.path()),
        serde_json::to_string(&descriptor(0)).unwrap(),
    )
    .unwrap();
    assert!(
        DaemonDiscovery::read(dir.path()).is_err(),
        "pid 0 必须视为陈旧"
    );
}

#[test]
fn discovery_missing_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let error = DaemonDiscovery::read(dir.path()).unwrap_err();
    assert!(matches!(error, owo_agent_client::ClientError::NotFound(_)));
}

// ---------------------------------------------------------------------------
// 端到端：假 Daemon（原生 TCP，客户端不依赖 server）
// ---------------------------------------------------------------------------

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

async fn read_request(socket: &mut TcpStream) -> (String, String) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let read = socket.read(&mut tmp).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..read]);
        if find_subslice(&buf, b"\r\n\r\n").is_some() {
            break;
        }
    }
    let header_end = find_subslice(&buf, b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(buf.len());
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let content_length = lines
        .filter_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_string)
        })
        .next()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let read = socket.read(&mut tmp).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..read]);
    }
    (method, path)
}

fn route(method: &str, path: &str) -> (u16, &'static str, Vec<u8>) {
    if method == "GET" && path.starts_with("/health") {
        return (
            200,
            "application/json",
            br#"{"healthy":true,"version":"0.1.0","api_version":"0.7","auto_approve":false,"pid":1,"stage":"ready","build_id":"abc"}"#.to_vec(),
        );
    }
    if method == "GET" && path.starts_with("/sessions") {
        return (200, "application/json", b"[]".to_vec());
    }
    if method == "POST" && path.starts_with("/session/") && path.ends_with("/turn") {
        let body = concat!(
            "event: progress\ndata: {\"type\":\"progress\",\"message\":\"模型调用\",\"v\":1}\n\n",
            "event: token_delta\ndata: {\"type\":\"token_delta\",\"delta\":\"你\",\"v\":1}\n\n",
            "event: token_delta\ndata: {\"type\":\"token_delta\",\"delta\":\"好\",\"v\":1}\n\n",
            "event: final\ndata: {\"type\":\"final\",\"text\":\"你好\",\"v\":1}\n\n"
        );
        return (200, "text/event-stream", body.as_bytes().to_vec());
    }
    if method == "POST" && path.ends_with("/abort") {
        return (200, "application/json", br#"{"ok":true}"#.to_vec());
    }
    if method == "POST" && path == "/session" {
        return (
            200,
            "application/json",
            br#"{"id":"sess-1","workspace":"ws","model":"m","created_at":"2026-01-01T00:00:00Z","updated_at":"","title":null,"archived":false,"pinned":false,"parent_id":null,"fork_point":null}"#.to_vec(),
        );
    }
    (
        404,
        "application/json",
        br#"{"error":"not found"}"#.to_vec(),
    )
}

async fn spawn_fake_daemon() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (method, path) = read_request(&mut socket).await;
                let (status, content_type, body) = route(&method, &path);
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://127.0.0.1:{}", address.port())
}

async fn spawn_replay_daemon(interrupted: bool) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let (method, path) = read_request(&mut socket).await;
                let (content_type, extra_headers, body) = if method == "POST"
                    && path.ends_with("/turn")
                {
                    (
                        "text/event-stream",
                        "X-Owo-Turn-Id: turn-1\r\n",
                        concat!(
                            "id: 1\nevent: progress\ndata: {\"type\":\"progress\",\"message\":\"start\"}\n\n",
                            "id: 2\nevent: token_delta\ndata: {\"type\":\"token_delta\",\"delta\":\"partial\"}\n\n"
                        )
                        .as_bytes()
                        .to_vec(),
                    )
                } else if method == "GET" && path.contains("/turn/events?") {
                    let (events, state, next_after_seq) = if interrupted {
                        (serde_json::json!([]), "interrupted", 2)
                    } else {
                        (
                            serde_json::json!([{
                                "session_id": "sess-1",
                                "turn_id": "turn-1",
                                "seq": 3,
                                "created_at": "2026-09-22T00:00:00Z",
                                "payload": {"type": "final", "text": "resumed"}
                            }]),
                            "completed",
                            3,
                        )
                    };
                    (
                        "application/json",
                        "",
                        serde_json::json!({
                            "events": events,
                            "active": false,
                            "state": state,
                            "next_after_seq": next_after_seq
                        })
                        .to_string()
                        .into_bytes(),
                    )
                } else {
                    ("application/json", "", b"{}".to_vec())
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://127.0.0.1:{}", address.port())
}

#[tokio::test]
async fn client_resumes_turn_from_persisted_events_after_stream_eof() {
    let base_url = spawn_replay_daemon(false).await;
    let client = AgentClient::new(ClientConfig::new(base_url, None)).expect("client");
    let mut stream = client.open_turn("sess-1", "hello").await.expect("turn");
    let mut events = Vec::new();
    stream
        .drive(|event| events.push(event))
        .await
        .expect("drive");
    assert!(matches!(events.first(), Some(SseEvent::Progress { .. })));
    assert!(matches!(events.get(1), Some(SseEvent::TokenDelta { delta }) if delta == "partial"));
    assert!(matches!(events.get(2), Some(SseEvent::Final { text }) if text == "resumed"));
    assert_eq!(events.len(), 3);
}

#[tokio::test]
async fn client_reports_interrupted_turn_after_replaying_partial_output() {
    let base_url = spawn_replay_daemon(true).await;
    let client = AgentClient::new(ClientConfig::new(base_url, None)).expect("client");
    let mut stream = client.open_turn("sess-1", "hello").await.expect("turn");
    let mut events = Vec::new();
    let result = stream.drive(|event| events.push(event)).await;
    assert!(matches!(events.last(), Some(SseEvent::TokenDelta { delta }) if delta == "partial"));
    assert!(
        matches!(result, Err(owo_agent_client::ClientError::Protocol(message)) if message.contains("中断"))
    );
}

#[tokio::test]
async fn client_round_trips_against_fake_daemon() {
    let base_url = spawn_fake_daemon().await;
    let client = AgentClient::new(ClientConfig::new(base_url, Some("test-token".to_string())))
        .expect("client");

    let health = client.health().await.expect("health");
    assert!(health.healthy);
    assert_eq!(health.api_version, "0.7");

    assert!(client.list_sessions().await.expect("list").is_empty());

    let session = client.create_session("ws").await.expect("create");
    assert_eq!(session.id, "sess-1");

    let mut stream = client.open_turn(&session.id, "hi").await.expect("turn");
    let mut deltas = Vec::new();
    let mut final_text = None;
    stream
        .drive(|event| match event {
            SseEvent::TokenDelta { delta } => deltas.push(delta),
            SseEvent::Final { text } => final_text = Some(text),
            _ => {}
        })
        .await
        .expect("drive");
    assert_eq!(deltas, vec!["你".to_string(), "好".to_string()]);
    assert_eq!(final_text.as_deref(), Some("你好"));

    let cancelled = client.cancel_turn(&session.id).await.expect("cancel");
    assert_eq!(cancelled["ok"], true);
}
