use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use owo_agent_core::{
    sanitize_html, walk, BlockKind, CanvasBlockData, CanvasNote, CanvasRect, NoteDoc,
};
pub(super) fn parse_block_kind(
    kind: &str,
    text: Option<String>,
    data: &Value,
) -> Result<BlockKind, String> {
    let text = text.unwrap_or_default();
    match kind {
        "paragraph" => Ok(BlockKind::Paragraph { text }),
        "heading" => {
            let level = data
                .get("level")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as u8;
            Ok(BlockKind::Heading { level, text })
        }
        "list" => {
            let ordered = data
                .get("ordered")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(BlockKind::List { ordered })
        }
        "list_item" => Ok(BlockKind::ListItem { text }),
        "code" => {
            let language = data
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(BlockKind::Code { language, text })
        }
        "table" => {
            let rows: Vec<Vec<String>> = data
                .get("rows")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(Value::as_array)
                        .map(|row| {
                            row.iter()
                                .map(|cell| cell.as_str().unwrap_or("").to_string())
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(BlockKind::Table { rows })
        }
        "image" => {
            let src = data
                .get("src")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(BlockKind::Image { src, alt: text })
        }
        "file" => {
            let path = data
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let mime = data
                .get("mime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(BlockKind::File { path, mime })
        }
        "quote" => Ok(BlockKind::Quote { text }),
        "html" => Ok(BlockKind::HtmlEmbed {
            html: sanitize_html(&text),
        }),
        "canvas" => {
            let data = parse_canvas(data);
            Ok(BlockKind::Canvas { data })
        }
        "ai" => {
            let model = data
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let prompt = data
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok(BlockKind::AiGenerated {
                model,
                prompt,
                text,
            })
        }
        other => Err(format!("未知块类型：{other}")),
    }
}

pub(super) fn parse_canvas(data: &Value) -> CanvasBlockData {
    let data = data.get("canvas").unwrap_or(data);
    let rects = data
        .get("rects")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(CanvasRect {
                        id: item.get("id")?.as_str()?.to_string(),
                        x: item.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                        y: item.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                        w: item.get("w").and_then(Value::as_f64).unwrap_or(50.0),
                        h: item.get("h").and_then(Value::as_f64).unwrap_or(30.0),
                        layer: item
                            .get("layer")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let notes = data
        .get("notes")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(CanvasNote {
                        id: item.get("id")?.as_str()?.to_string(),
                        x: item.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                        y: item.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                        text: item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let layers = data
        .get("layers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    CanvasBlockData {
        rects,
        notes,
        layers,
    }
}

/// 校验完整块树：root 存在、children 引用存在、无孤儿块。
pub(super) fn validate_doc(doc: &NoteDoc) -> Result<(), String> {
    if !doc.blocks.contains_key(&doc.root) {
        return Err("缺少根块".to_string());
    }
    for (id, block) in &doc.blocks {
        for child in &block.children {
            if !doc.blocks.contains_key(child) {
                return Err(format!("块 {id} 引用了不存在的子块：{child}"));
            }
        }
    }
    let reachable = walk(doc, &doc.root).len();
    if reachable != doc.blocks.len() {
        return Err(format!(
            "块树不完整：可达 {reachable} 块，实际 {} 块（存在孤儿）",
            doc.blocks.len()
        ));
    }
    Ok(())
}

/// 定位 after 块所在父与下一索引。
pub(super) fn locate_after(doc: &NoteDoc, after: &str) -> Result<(String, usize), String> {
    for block in doc.blocks.values() {
        if let Some(index) = block.children.iter().position(|c| c == after) {
            return Ok((block.id.clone(), index + 1));
        }
    }
    Err(format!("after 块不存在：{after}"))
}

/// 块树 → HTML 渲染（文本转义 + 白名单标签 + sanitize_html 兜底）。
pub(super) fn block_to_html(doc: &NoteDoc) -> String {
    fn esc(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn render(doc: &NoteDoc, id: &str, out: &mut String) {
        let Some(block) = doc.blocks.get(id) else {
            return;
        };
        match &block.kind {
            BlockKind::Paragraph { text } => out.push_str(&format!("<p>{}</p>\n", esc(text))),
            BlockKind::Heading { level, text } => {
                out.push_str(&format!("<h{level}>{}</h{level}>\n", esc(text)))
            }
            BlockKind::List { ordered } => {
                let tag = if *ordered { "ol" } else { "ul" };
                out.push_str(&format!("<{tag}>\n"));
                for child in &block.children {
                    if let Some(item) = doc.blocks.get(child) {
                        if let BlockKind::ListItem { text } = &item.kind {
                            out.push_str(&format!("<li>{}</li>\n", esc(text)));
                        }
                    }
                }
                out.push_str(&format!("</{tag}>\n"));
            }
            BlockKind::ListItem { .. } => {}
            BlockKind::Code { language, text } => {
                out.push_str(&format!(
                    "<pre><code data-lang=\"{}\">{}</code></pre>\n",
                    esc(language),
                    esc(text)
                ));
            }
            BlockKind::Table { rows } => {
                out.push_str("<table>\n");
                for (i, row) in rows.iter().enumerate() {
                    out.push_str("<tr>");
                    for cell in row {
                        if i == 0 {
                            out.push_str(&format!("<th>{}</th>", esc(cell)));
                        } else {
                            out.push_str(&format!("<td>{}</td>", esc(cell)));
                        }
                    }
                    out.push_str("</tr>\n");
                }
                out.push_str("</table>\n");
            }
            BlockKind::Image { src, alt } => {
                out.push_str(&format!(
                    "<img src=\"{}\" alt=\"{}\">\n",
                    esc(src),
                    esc(alt)
                ));
            }
            BlockKind::File { path, .. } => {
                out.push_str(&format!(
                    "<p><a href=\"{}\">📎 {}</a></p>\n",
                    esc(path),
                    esc(path)
                ));
            }
            BlockKind::Quote { text } => {
                out.push_str(&format!("<blockquote>{}</blockquote>\n", esc(text)))
            }
            BlockKind::HtmlEmbed { html } => out.push_str(html),
            BlockKind::Canvas { data } => {
                out.push_str("<div class=\"owo-canvas\">");
                for note in &data.notes {
                    out.push_str(&format!(
                        "<span class=\"owo-canvas-note\">{}</span> ",
                        esc(&note.text)
                    ));
                }
                out.push_str("</div>\n");
            }
            BlockKind::AiGenerated { text, .. } => {
                out.push_str(&format!("<p class=\"owo-ai\">{}</p>\n", esc(text)))
            }
        }
        for child in &block.children {
            render(doc, child, out);
        }
    }
    let mut out = String::new();
    render(doc, &doc.root, &mut out);
    sanitize_html(&out)
}

/// 审计写操作。
pub(super) fn audit(
    state: &owo_agent_server::AppState,
    event: &str,
    id: &str,
    detail: impl Into<String>,
) {
    if let Ok(mut log) = state.agent.audit_log().lock() {
        log.record(
            "notes",
            event,
            Some(id.to_string()),
            Some(true),
            detail.into(),
        );
    }
}

pub(super) fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into() })))
}

// ----------------------------------------------------------------------------
// 路由
// ----------------------------------------------------------------------------
