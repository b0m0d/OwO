//! MCP HTTP 测试服务器：最小 HTTP/1.1 JSON-RPC 端点（无第三方依赖），提供 echo 工具。

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).ok_or("需要端口参数")?.parse()?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    loop {
        let (mut socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            let _ = handle_connection(&mut socket).await;
        });
    }
}

async fn handle_connection(
    socket: &mut tokio::net::TcpStream,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_headers_end(&buffer) {
            let headers = String::from_utf8_lossy(&buffer[..position]).to_string();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let lower = line.to_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buffer.len() >= position + 4 + content_length {
                let body = &buffer[position + 4..position + 4 + content_length];
                let message: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let response = handle_message(message);
                let payload = serde_json::to_string(&response)?;
                let http_response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                socket.write_all(http_response.as_bytes()).await?;
                socket.flush().await?;
                return Ok(());
            }
        }
    }
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn handle_message(message: Value) -> Value {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned();
    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "owo-mcp-http-test", "version": "1.0.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "回显文本",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                }]
            }
        }),
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": arguments.get("text").and_then(Value::as_str).unwrap_or_default()
                    }]
                }
            })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "method not found" }
        }),
    }
}
