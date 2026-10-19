//! Local image loading tool for multimodal Agent turns.

use super::{resolve_session_path, Tool, ToolContext, ToolSpec};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use owo_agent_kernel::required_string;
use serde_json::{json, Value};

/// `read_image`：读取图片并返回元数据 + base64（视觉理解需多模态模型支持）。
pub(super) struct ReadImageTool;

#[async_trait]
impl Tool for ReadImageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_image".into(),
            description: "读取图片文件（返回 mime/尺寸/base64；默认 4MB 上限）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let path = required_string(&args, "path")?;
        let abs = resolve_session_path(ctx, &path)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| format!("读取 {path} 失败：{e}"))?;
        const MAX_BYTES: usize = 4 * 1024 * 1024;
        if bytes.len() > MAX_BYTES {
            return Err(format!(
                "图片过大（{} 字节 > {MAX_BYTES}）：请先压缩",
                bytes.len()
            ));
        }
        let mime = match abs
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            other => return Err(format!("不支持的图片扩展名：{other}")),
        };
        Ok(json!({
            "path": path,
            "mime": mime,
            "bytes": bytes.len(),
            "base64": BASE64.encode(&bytes),
            "note": "多模态视觉理解需要 gateway 支持图像内容；当前返回原始数据供工具/上层使用",
        }))
    }
}
