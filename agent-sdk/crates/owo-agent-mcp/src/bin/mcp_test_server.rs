//! MCP 测试服务器：stdio JSON-RPC，提供 echo / add 两个工具。

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = message.get("id").cloned();
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "owo-mcp-test", "version": "1.0.0" }
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "回显文本",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "text": { "type": "string" } },
                                "required": ["text"]
                            }
                        },
                        {
                            "name": "add",
                            "description": "两数相加",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "a": { "type": "number" },
                                    "b": { "type": "number" }
                                },
                                "required": ["a", "b"]
                            }
                        },
                        {
                            "name": "hang",
                            "description": "按 sleep_ms 挂起后再返回（用于超时测试）",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "sleep_ms": { "type": "number" } },
                                "required": ["sleep_ms"]
                            }
                        }
                    ]
                }
            }),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                match name {
                    "echo" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": arguments.get("text").and_then(Value::as_str).unwrap_or_default()
                            }]
                        }
                    }),
                    "add" => {
                        let a = arguments.get("a").and_then(Value::as_f64).unwrap_or(0.0);
                        let b = arguments.get("b").and_then(Value::as_f64).unwrap_or(0.0);
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": (a + b).to_string() }]
                            }
                        })
                    }
                    "hang" => {
                        let sleep_ms = arguments
                            .get("sleep_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or(1_000);
                        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{ "type": "text", "text": "woke" }]
                            }
                        })
                    }
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": format!("unknown tool: {name}") }
                    }),
                }
            }
            "notifications/initialized" | "exit" => Value::Null,
            _ => Value::Null,
        };
        if response.is_null() {
            if method == "exit" {
                break;
            }
            continue;
        }
        stdout
            .write_all(serde_json::to_string(&response)?.as_bytes())
            .await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }
    Ok(())
}
