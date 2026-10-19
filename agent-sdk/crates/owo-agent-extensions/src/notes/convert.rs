//! HTML 消毒与 Markdown 导入/导出（文档块树的解析/渲染层）。
//!
//! 从 notes.rs 拆出：只做格式转换，不持有文档状态与索引。

use super::*;
use std::collections::BTreeMap;

const HTML_TAGS: &[&str] = &[
    "p",
    "div",
    "span",
    "strong",
    "em",
    "b",
    "i",
    "u",
    "s",
    "sub",
    "sup",
    "ul",
    "ol",
    "li",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "code",
    "pre",
    "blockquote",
    "a",
    "img",
    "br",
    "hr",
    "mark",
    "small",
    "del",
];

/// 永久移除的标签（含内容）。
const HTML_BLOCKED_TAGS: &[&str] = &[
    "script", "style", "iframe", "object", "embed", "form", "input", "button", "textarea",
    "select", "link", "meta", "base", "svg", "math", "video", "audio", "canvas", "template",
    "noscript",
];

/// 允许的 URL 协议（href/src）。
fn safe_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("vbscript:")
        || lower.contains('<')
        || lower.contains('>')
    {
        return None;
    }
    // 协议白名单：http/https/mailto/tel/# 或相对路径
    if let Some(colon) = lower.find(':') {
        let scheme = &lower[..colon];
        if !matches!(scheme, "http" | "https" | "mailto" | "tel") {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// 允许的属性白名单（按标签泛化）。
fn safe_attrs(tag: &str, attrs: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in attrs {
        let lower = name.to_lowercase();
        if lower.starts_with("on") {
            continue; // 事件属性一律剥离
        }
        match lower.as_str() {
            "href" | "src" => {
                if let Some(url) = safe_url(value) {
                    out.push((name.clone(), url));
                }
            }
            "alt" | "title" | "lang" | "dir" | "colspan" | "rowspan" | "width" | "height"
            | "class" | "id" | "align" | "start" => {
                out.push((name.clone(), value.clone()));
            }
            _ => {} // 其余（style/on* 等）剥离
        }
    }
    let _ = tag;
    out
}

/// 消毒 HTML：剥离危险标签（含内容）与事件属性，URL 白名单校验。
/// 保留原始片段的可见结构（标签/文本/属性中的安全子集）。
pub fn sanitize_html(raw: &str) -> String {
    // 先整体移除被禁标签（含内容，大小写不敏感，非贪婪）。
    let mut text = raw.to_string();
    for tag in HTML_BLOCKED_TAGS {
        let mut lower = text.to_lowercase();
        while let Some(start) = lower.find(&format!("<{tag}")) {
            // 找结束位置：先找闭合标签，找不到则到标签结束（自闭合）或文本末尾
            let open_end = text[start..]
                .find('>')
                .map(|i| start + i + 1)
                .unwrap_or(text.len());
            let close_tag = format!("</{tag}>");
            let close_end = lower[open_end..]
                .find(&close_tag)
                .map(|i| open_end + i + close_tag.len());
            match close_end {
                Some(end) => {
                    text.replace_range(start..end, "");
                }
                None => {
                    // 无闭合：若标签自闭合（/>）则只删标签本身，否则删到末尾
                    if text[open_end.saturating_sub(2)..open_end.min(text.len())].contains("/>") {
                        text.replace_range(start..open_end, "");
                    } else {
                        text.replace_range(start..text.len(), "");
                    }
                }
            }
            lower = text.to_lowercase();
        }
    }
    // 逐标签清洗：白名单外的标签去掉尖括号但保留内容（转义文本），白名单内清洗属性。
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        let Some(gt) = rest.find('>') else {
            // 未闭合的 < → 转义
            out.push_str("&lt;");
            rest = &rest[1..];
            continue;
        };
        let token = &rest[1..gt];
        rest = &rest[gt + 1..];
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (is_close, token) = if let Some(rest) = token.strip_prefix('/') {
            (true, rest.trim())
        } else {
            (false, token)
        };
        let mut parts = token.splitn(2, char::is_whitespace);
        let tag = parts.next().unwrap_or("").to_lowercase();
        let attr_text = parts.next().unwrap_or("");
        if !HTML_TAGS.contains(&tag.as_str()) {
            // 未知标签：保留为转义文本（不吞内容）
            out.push_str("&lt;");
            out.push_str(escape_html(token).as_str());
            out.push_str("&gt;");
            continue;
        }
        // 解析属性
        let attrs = parse_attrs(attr_text);
        let clean = safe_attrs(&tag, &attrs);
        let mut rendered = format!("<{}", if is_close { "/" } else { "" });
        rendered.push_str(&tag);
        for (name, value) in clean {
            rendered.push_str(&format!(" {}=\"{}\"", name, escape_attr(&value)));
        }
        rendered.push('>');
        out.push_str(&rendered);
    }
    out.push_str(rest);
    out
}

fn parse_attrs(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.trim().is_empty() {
        rest = rest.trim_start();
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_string();
        if name.is_empty() {
            break;
        }
        rest = &rest[name_end..];
        let mut value = String::new();
        if let Some(eq) = rest.find('=') {
            if eq == 0 {
                rest = rest[1..].trim_start();
                if let Some(quote) = rest.chars().next() {
                    if quote == '"' || quote == '\'' {
                        rest = &rest[1..];
                        let end = rest.find(quote).unwrap_or(rest.len());
                        value = rest[..end].to_string();
                        rest = &rest[end.min(rest.len())..];
                        if end < rest.len() + 1 && rest.starts_with(quote) {
                            rest = &rest[1..];
                        }
                    } else {
                        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
                        value = rest[..end].to_string();
                        rest = &rest[end..];
                    }
                }
            }
        }
        out.push((name, value));
    }
    out
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(text: &str) -> String {
    escape_html(text).replace('"', "&quot;")
}

// ----------------------------------------------------------------------------
// Markdown 导入/导出（常用元素往返无损）
// ----------------------------------------------------------------------------

/// Markdown 文本 → 块树文档（按 root 下顺序块组织；列表项挂到 List 下）。
pub fn md_to_doc(id: impl Into<String>, title: impl Into<String>, md: &str) -> NoteDoc {
    let mut doc = new_doc(id, title);
    let root = doc.root.clone();
    let mut stack: Vec<BlockId> = vec![root.clone()]; // 栈顶为当前容器（root/List/Quote）
    let mut pending_list: Option<BlockId> = None; // 当前列表容器（连续列表项归组）
    let mut pending_list_ordered = false;

    let lines: Vec<&str> = md.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_end();
        let leading = line.len() - line.trim_start().len();
        let _ = leading;

        // 代码块（``` 围栏）
        if let Some(language) = trimmed.strip_prefix("```") {
            let language = language.trim().to_string();
            let mut code_lines = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                code_lines.push(lines[i]);
                i += 1;
            }
            i += 1; // 跳过闭合围栏
            pending_list = None;
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::Code {
                    language,
                    text: code_lines.join("\n"),
                },
                BTreeMap::new(),
            );
            continue;
        }

        // 标题
        if let Some(level) = heading_level(trimmed) {
            pending_list = None;
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::Heading {
                    level,
                    text: trimmed[level as usize + 1..].trim().to_string(),
                },
                BTreeMap::new(),
            );
            i += 1;
            continue;
        }

        // 引用
        if trimmed.starts_with("> ") || trimmed == ">" {
            pending_list = None;
            let quote = trimmed.trim_start_matches('>').trim().to_string();
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::Quote { text: quote },
                BTreeMap::new(),
            );
            i += 1;
            continue;
        }

        // 列表项
        if let Some((ordered, text)) = list_item(trimmed) {
            if pending_list.is_none()
                || pending_list_ordered != ordered
                || !doc.blocks.get(pending_list.as_ref().unwrap()).is_some_and(
                    |b| matches!(b.kind, BlockKind::List { ordered: o } if o == ordered),
                )
            {
                pending_list = Some(
                    add_block(
                        &mut doc,
                        &root,
                        BlockKind::List { ordered },
                        BTreeMap::new(),
                    )
                    .expect("append list"),
                );
                pending_list_ordered = ordered;
            }
            let list_id = pending_list.clone().unwrap();
            let _ = add_block(
                &mut doc,
                &list_id,
                BlockKind::ListItem { text },
                BTreeMap::new(),
            );
            i += 1;
            continue;
        }
        pending_list = None;

        // 表格（| a | b | 且下一行为分隔行 |---|）
        if trimmed.starts_with('|')
            && i + 1 < lines.len()
            && is_table_separator(lines[i + 1].trim())
        {
            let header: Vec<String> = trimmed
                .trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect();
            let mut rows = vec![header];
            i += 2; // 跳过表头 + 分隔行
            while i < lines.len() && lines[i].trim().starts_with('|') {
                rows.push(
                    lines[i]
                        .trim()
                        .trim_matches('|')
                        .split('|')
                        .map(|c| c.trim().to_string())
                        .collect(),
                );
                i += 1;
            }
            let _ = add_block(&mut doc, &root, BlockKind::Table { rows }, BTreeMap::new());
            continue;
        }

        // 图片
        if let Some((alt, src)) = image_line(trimmed) {
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::Image { src, alt },
                BTreeMap::new(),
            );
            i += 1;
            continue;
        }

        // HTML 嵌入（以 < 开头的行，经消毒入库）
        if trimmed.starts_with('<') {
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::HtmlEmbed {
                    html: sanitize_html(trimmed),
                },
                BTreeMap::new(),
            );
            i += 1;
            continue;
        }

        // 段落（空行忽略）
        if !trimmed.is_empty() {
            let _ = add_block(
                &mut doc,
                &root,
                BlockKind::Paragraph {
                    text: trimmed.to_string(),
                },
                BTreeMap::new(),
            );
        }
        i += 1;
    }
    let _ = &mut stack;
    doc
}

fn heading_level(line: &str) -> Option<u8> {
    let trimmed = line.trim_start();
    let level = trimmed.bytes().take_while(|b| *b == b'#').count();
    if (1..=6).contains(&level) && trimmed.as_bytes().get(level) == Some(&b' ') {
        Some(level as u8)
    } else {
        None
    }
}

fn list_item(line: &str) -> Option<(bool, String)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return Some((false, rest.trim().to_string()));
    }
    let digits: usize = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 && trimmed.as_bytes().get(digits) == Some(&b'.') {
        let rest = &trimmed[digits + 1..];
        if let Some(text) = rest.strip_prefix(' ') {
            return Some((true, text.trim().to_string()));
        }
    }
    None
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.contains('-') && trimmed.matches('-').count() >= 2
}

fn image_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if !trimmed.starts_with("![") {
        return None;
    }
    let close = trimmed.find("](")?;
    let alt = trimmed[2..close].to_string();
    let rest = &trimmed[close + 2..];
    let end = rest.find(')')?;
    let src = rest[..end].to_string();
    Some((alt, src))
}

/// 块树文档 → Markdown 文本。
pub fn doc_to_md(doc: &NoteDoc) -> String {
    let root = doc.root.clone();
    let mut out = String::new();
    render_children_to_md(doc, &root, &mut out);
    out
}

fn render_children_to_md(doc: &NoteDoc, parent: &BlockId, out: &mut String) {
    let Some(block) = doc.blocks.get(parent) else {
        return;
    };
    for child_id in &block.children {
        let Some(child) = doc.blocks.get(child_id) else {
            continue;
        };
        match &child.kind {
            BlockKind::Paragraph { text } => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            BlockKind::Heading { level, text } => {
                out.push_str(&"#".repeat(*level as usize));
                out.push(' ');
                out.push_str(text);
                out.push_str("\n\n");
            }
            BlockKind::List { ordered } => {
                for (index, item_id) in child.children.iter().enumerate() {
                    if let Some(item) = doc.blocks.get(item_id) {
                        if let BlockKind::ListItem { text } = &item.kind {
                            let marker = if *ordered {
                                format!("{}. ", index + 1)
                            } else {
                                "- ".to_string()
                            };
                            out.push_str(&marker);
                            out.push_str(text);
                            out.push('\n');
                        }
                    }
                }
                out.push('\n');
            }
            BlockKind::ListItem { .. } => {} // 由 List 统一渲染
            BlockKind::Code { language, text } => {
                out.push_str("```");
                out.push_str(language);
                out.push('\n');
                out.push_str(text);
                out.push_str("\n```\n\n");
            }
            BlockKind::Table { rows } => {
                if let Some((first, rest)) = rows.split_first() {
                    out.push_str("| ");
                    out.push_str(&first.join(" | "));
                    out.push_str(" |\n| ");
                    out.push_str(&vec!["---"; first.len()].join(" | "));
                    out.push_str(" |\n");
                    for row in rest {
                        out.push_str("| ");
                        out.push_str(&row.join(" | "));
                        out.push_str(" |\n");
                    }
                    out.push('\n');
                }
            }
            BlockKind::Image { src, alt } => {
                out.push_str(&format!("![{alt}]({src})\n\n"));
            }
            BlockKind::File { path, .. } => {
                out.push_str(&format!("[📎 {path}]({path})\n\n"));
            }
            BlockKind::Quote { text } => {
                for line in text.lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            BlockKind::HtmlEmbed { html } => {
                out.push_str(html);
                out.push_str("\n\n");
            }
            BlockKind::Canvas { .. } => {
                // 画布数据模型不进入 Markdown 渲染（渲染留给前端）；导出为空段落占位。
                out.push_str("\n\n");
            }
            BlockKind::AiGenerated { .. } => {
                // AI 生成块不进入 Markdown（v1：数据保真由 doc.json 承担，MD 为可表达元素渲染器）。
                out.push_str("\n\n");
            }
        }
        render_children_to_md(doc, child_id, out);
    }
}
