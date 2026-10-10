//! MCP 测试服务器：stdio JSON-RPC，提供 echo / add 两个工具。
//!
//! 另供 A2-2 使用：`resources/list` + `resources/read`（file:///readme.md、
//! note://tips）与 `prompts/list` + `prompts/get`（summarize 模板）。

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn resources_response() -> Value {
    json!({
        "resources": [
            {
                "uri": "file:///readme.md",
                "name": "readme",
                "description": "项目说明",
                "mimeType": "text/markdown"
            },
            {
                "uri": "note://tips",
                "name": "tips",
                "description": "使用技巧"
            }
        ]
    })
}

fn read_resource(params: &Value) -> Result<Value, Value> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match uri {
        "file:///readme.md" => Ok(json!({
            "contents": [ { "uri": uri, "mimeType": "text/markdown", "text": "# 欢迎\n这是测试 README" } ]
        })),
        "note://tips" => Ok(json!({
            "contents": [ { "uri": uri, "text": "提示内容" } ]
        })),
        other => Err(json!({ "code": -32602, "message": format!("未知资源：{other}") })),
    }
}

fn prompts_response() -> Value {
    json!({
        "prompts": [
            {
                "name": "summarize",
                "description": "生成摘要模板",
                "arguments": [
                    { "name": "topic", "description": "主题", "required": true }
                ]
            }
        ]
    })
}

fn get_prompt(params: &Value) -> Result<Value, Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name != "summarize" {
        return Err(json!({ "code": -32602, "message": format!("未知模板：{name}") }));
    }
    let topic = params
        .get("arguments")
        .and_then(|args| args.get("topic"))
        .and_then(Value::as_str)
        .unwrap_or("(未指定)");
    Ok(json!({
        "description": "生成摘要模板",
        "messages": [
            { "role": "user", "content": { "type": "text", "text": format!("请总结主题：{topic}") } }
        ]
    }))
}

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
                    "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
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
            "resources/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": resources_response()
            }),
            "resources/read" => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                match read_resource(&params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                }
            }
            "prompts/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": prompts_response()
            }),
            "prompts/get" => {
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                match get_prompt(&params) {
                    Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
                }
            }
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
                    "write_then_hang" => {
                        let path = arguments
                            .get("path")
                            .and_then(Value::as_str)
                            .ok_or("missing counter path")?;
                        let count = std::fs::read_to_string(path)
                            .ok()
                            .and_then(|text| text.parse::<u64>().ok())
                            .unwrap_or(0);
                        std::fs::write(path, (count + 1).to_string())?;
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        json!({ "jsonrpc": "2.0", "id": id, "result": { "content": [{ "type": "text", "text": "written" }] } })
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
