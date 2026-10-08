//! Session-local task list tool.

use super::{Tool, ToolContext, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};

/// 会话级任务清单（整表替换；CLI `/todo` 渲染）。
pub(super) struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo".into(),
            description: "写入/更新任务清单（整表替换；status: pending|in_progress|completed）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let items = args
            .get("todos")
            .and_then(Value::as_array)
            .ok_or("todos 必须是数组")?;
        let mut todos = Vec::with_capacity(items.len());
        for item in items {
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if content.is_empty() {
                return Err("todo.content 不能为空".to_string());
            }
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            if !matches!(status, "pending" | "in_progress" | "completed") {
                return Err(format!(
                    "todo.status 非法：{status}（pending|in_progress|completed）"
                ));
            }
            todos.push(crate::session::TodoItem {
                content,
                status: status.to_string(),
            });
        }
        ctx.session.todos = todos;
        let rendered = ctx
            .session
            .todos
            .iter()
            .map(|todo| {
                let mark = match todo.status.as_str() {
                    "completed" => "x",
                    "in_progress" => ">",
                    _ => " ",
                };
                format!("[{mark}] {}", todo.content)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(json!({ "todos": ctx.session.todos, "rendered": rendered }))
    }
}
