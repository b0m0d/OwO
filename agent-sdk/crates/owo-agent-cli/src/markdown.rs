//! 轻量 Markdown → ANSI 渲染（仅 human 模式）。
//!
//! 目标：让 CLI 不再直接吐裸 `**加粗**`、反引号、`# 标题`，而是终端里可读的样式。
//! 只覆盖模型高频输出的子集，不追求完整 CommonMark；流式场景按行渲染。

use colored::Colorize;

/// 流式 Markdown 渲染器：逐 token 喂入，按整行渲染；围栏代码块原样缩进展示。
#[derive(Default)]
pub(crate) struct MarkdownStream {
    buffer: String,
    in_code: bool,
}

impl MarkdownStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 喂入一段流式增量，遇到换行即渲染该行。
    pub(crate) fn push(&mut self, delta: &str) {
        self.buffer.push_str(delta);
        while let Some(idx) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=idx).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            self.emit_line(line);
        }
    }

    /// 收尾：渲染尚未换行的残留，并复位围栏状态。
    pub(crate) fn finish(&mut self) {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.emit_line(&line);
        }
        self.in_code = false;
    }

    fn emit_line(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            self.in_code = !self.in_code;
            if self.in_code {
                println!("{}", "  ┌─ 代码".dimmed());
            } else {
                println!("{}", "  └─".dimmed());
            }
            return;
        }
        if self.in_code {
            println!("  {} {}", "│".dimmed(), line.dimmed());
            return;
        }
        println!("{}", render_block_line(line));
    }
}

/// 渲染一行（代码块外）：标题 / 引用 / 列表 / 分隔线 + 行内样式。
pub(crate) fn render_block_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    // 分隔线：--- / *** / ___
    let bare = trimmed.trim_end();
    if bare.len() >= 3
        && (bare.chars().all(|c| c == '-')
            || bare.chars().all(|c| c == '*')
            || bare.chars().all(|c| c == '_'))
    {
        return format!("{indent}{}", "─".repeat(24).dimmed());
    }

    if let Some((level, text)) = split_heading(trimmed) {
        let rendered = render_inline(text);
        return match level {
            1 => format!("{indent}{}", rendered.bold().cyan()),
            2 => format!("{indent}{}", rendered.bold().blue()),
            _ => format!("{indent}{}", rendered.bold()),
        };
    }

    if let Some(rest) = trimmed
        .strip_prefix("> ")
        .or_else(|| trimmed.strip_prefix(">"))
    {
        return format!("{indent}{} {}", "│".dimmed(), render_inline(rest).italic());
    }

    if let Some(rest) = strip_bullet(trimmed) {
        return format!("{indent}{} {}", "•".green(), render_inline(rest));
    }

    format!("{indent}{}", render_inline(trimmed))
}

fn split_heading(s: &str) -> Option<(usize, &str)> {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = s[hashes..].trim_start();
    if rest.is_empty() {
        return None;
    }
    Some((hashes, rest))
}

fn strip_bullet(s: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(marker) {
            return Some(rest);
        }
    }
    None
}

/// 行内样式：`` `code` `` / `**bold**` / `*italic*` / `~~strike~~` / `[text](url)`。
pub(crate) fn render_inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        // 行内代码
        if chars[i] == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                let code: String = chars[i + 1..end].iter().collect();
                out.push_str(&code.cyan().to_string());
                i = end + 1;
                continue;
            }
        }
        // 粗体 ** / __
        if i + 1 < chars.len()
            && ((chars[i] == '*' && chars[i + 1] == '*')
                || (chars[i] == '_' && chars[i + 1] == '_'))
        {
            let marker = chars[i];
            if let Some(end) = find_double(&chars, i + 2, marker) {
                let text: String = chars[i + 2..end].iter().collect();
                out.push_str(&render_inline(&text).bold().to_string());
                i = end + 2;
                continue;
            }
        }
        // 斜体 * / _
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if let Some(end) = find_char(&chars, i + 1, marker) {
                if end > i + 1 {
                    let text: String = chars[i + 1..end].iter().collect();
                    out.push_str(&render_inline(&text).italic().to_string());
                    i = end + 1;
                    continue;
                }
            }
        }
        // 删除线 ~~
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_double(&chars, i + 2, '~') {
                let text: String = chars[i + 2..end].iter().collect();
                out.push_str(&text.strikethrough().to_string());
                i = end + 2;
                continue;
            }
        }
        // 链接 [text](url)
        if chars[i] == '[' {
            if let Some(close) = find_char(&chars, i + 1, ']') {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(paren) = find_char(&chars, close + 2, ')') {
                        let text: String = chars[i + 1..close].iter().collect();
                        let url: String = chars[close + 2..paren].iter().collect();
                        out.push_str(&render_inline(&text).underline().to_string());
                        out.push(' ');
                        out.push_str(&format!("({url})").dimmed().to_string());
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target)
}

fn find_double(chars: &[char], from: usize, marker: char) -> Option<usize> {
    let mut j = from;
    while j + 1 < chars.len() {
        if chars[j] == marker && chars[j + 1] == marker {
            return Some(j);
        }
        j += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in s.chars() {
            if c == '\u{1b}' {
                in_esc = true;
                continue;
            }
            if in_esc {
                if c == 'm' {
                    in_esc = false;
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn inline_strips_markers_but_keeps_text() {
        colored::control::set_override(true);
        assert_eq!(
            plain(&render_inline("**bold** and `code`")),
            "bold and code"
        );
        assert_eq!(
            plain(&render_inline("see [docs](http://x)")),
            "see docs (http://x)"
        );
        assert_eq!(plain(&render_inline("~~old~~")), "old");
        colored::control::unset_override();
    }

    #[test]
    fn headings_and_lists_render() {
        colored::control::set_override(true);
        assert_eq!(plain(&render_block_line("# 标题")), "标题");
        assert_eq!(plain(&render_block_line("- 项目")), "• 项目");
        assert_eq!(plain(&render_block_line("> 引用")), "│ 引用");
        colored::control::unset_override();
    }

    #[test]
    fn code_fence_toggles_state() {
        colored::control::set_override(true);
        let mut md = MarkdownStream::new();
        md.push("```rust\n");
        assert!(md.in_code, "围栏开启后应处于代码态");
        md.push("let x = 1;\n");
        md.push("```\n");
        assert!(!md.in_code, "围栏关闭后应复位");
        colored::control::unset_override();
    }
}
