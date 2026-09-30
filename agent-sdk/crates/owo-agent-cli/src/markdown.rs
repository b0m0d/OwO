//! 轻量 Markdown → ANSI 渲染（仅 human 模式）。
//!
//! 关键约束：**必须边到边显**。早期实现按整行缓冲，长段落/长句在换行前不显示，
//! 观感上像"思考被隐藏"。现在改为增量渲染：行首只缓冲到能判定块类型为止
//! （标题最多 7 字符、列表/引用 2 字符、围栏 3 字符），其余字符立即输出；
//! 行内样式（粗体/斜体/行内代码/删除线）用状态机即时切换。

use colored::Colorize;

/// 行首块类型判定结果。
enum Block {
    /// 需要更多字符才能判定。
    Pending,
    /// 标题（样式应用到行尾）。
    Heading,
    Bullet,
    Quote,
    /// 围栏代码块开关行。
    Fence,
    /// 普通行：把已缓冲前缀按行内规则输出。
    Plain,
}

/// 增量 Markdown 渲染器：逐 token 喂入，立即输出。
pub(crate) struct MarkdownStream {
    /// 行首块类型判定缓冲（有界：标题 ≤7、列表/引用 ≤2、围栏 ≤3）。
    prefix: String,
    at_line_start: bool,
    /// 围栏开关行/代码行剩余部分丢弃到行尾。
    consume_to_eol: bool,
    /// 等待配对的 `*` / `_` / `~`（区分粗体与斜体）。
    pending: Option<char>,
    bold: bool,
    italic: bool,
    cyan: bool,
    code: bool,
    strike: bool,
    /// 围栏代码块状态。
    in_fence: bool,
}

impl Default for MarkdownStream {
    fn default() -> Self {
        Self {
            prefix: String::new(),
            at_line_start: true,
            consume_to_eol: false,
            pending: None,
            bold: false,
            italic: false,
            cyan: false,
            code: false,
            strike: false,
            in_fence: false,
        }
    }
}

impl MarkdownStream {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 喂入流式增量：立即可见（行首最多缓冲 7 字符）。
    pub(crate) fn push(&mut self, delta: &str) {
        for c in delta.chars() {
            self.push_char(c);
        }
    }

    /// 收尾：输出残留前缀/待配对标记，复位样式与围栏。
    pub(crate) fn finish(&mut self) {
        if self.consume_to_eol {
            self.consume_to_eol = false;
        }
        if !self.prefix.is_empty() {
            let prefix = std::mem::take(&mut self.prefix);
            self.emit_inline_text(&prefix);
        }
        if let Some(marker) = self.pending.take() {
            self.emit_raw(&marker.to_string());
        }
        self.reset_styles();
        self.in_fence = false;
        self.at_line_start = true;
    }

    fn push_char(&mut self, c: char) {
        if self.consume_to_eol {
            if c == '\n' {
                self.consume_to_eol = false;
                self.at_line_start = true;
                println!();
            }
            return;
        }
        if self.at_line_start {
            self.prefix.push(c);
            match self.block_decision() {
                Block::Pending => return,
                Block::Heading => {
                    self.prefix.clear();
                    self.at_line_start = false;
                    self.bold = true;
                    self.cyan = true;
                    self.refresh_styles();
                    return;
                }
                Block::Bullet => {
                    self.prefix.clear();
                    self.at_line_start = false;
                    if Self::colors() {
                        print!("{} ", "•".green());
                    } else {
                        print!("• ");
                    }
                    return;
                }
                Block::Quote => {
                    self.prefix.clear();
                    self.at_line_start = false;
                    if Self::colors() {
                        print!("{} ", "│".dimmed());
                    } else {
                        print!("│ ");
                    }
                    return;
                }
                Block::Fence => {
                    self.prefix.clear();
                    self.at_line_start = false;
                    self.consume_to_eol = true;
                    if self.in_fence {
                        self.in_fence = false;
                        print!("  └─");
                    } else {
                        self.in_fence = true;
                        print!("  ┌─ 代码");
                    }
                    return;
                }
                Block::Plain => {
                    let prefix = std::mem::take(&mut self.prefix);
                    self.at_line_start = false;
                    self.emit_inline_text(&prefix);
                    return;
                }
            }
        }
        if self.in_fence {
            self.emit_raw(&c.to_string());
            if c == '\n' {
                self.at_line_start = true;
            }
            return;
        }
        self.emit_inline(c);
    }

    fn block_decision(&self) -> Block {
        let p = self.prefix.as_str();
        let Some(first) = p.chars().next() else {
            return Block::Pending;
        };
        match first {
            '#' => {
                let hashes = p.chars().take_while(|c| *c == '#').count();
                if hashes > 6 {
                    return Block::Plain;
                }
                if hashes == p.chars().count() {
                    return Block::Pending;
                }
                if p.chars().nth(hashes) == Some(' ') {
                    return Block::Heading;
                }
                Block::Plain
            }
            '-' | '*' | '+' | '>' => {
                if p.chars().count() == 1 {
                    return Block::Pending;
                }
                if p.starts_with("- ") || p.starts_with("* ") || p.starts_with("+ ") {
                    return Block::Bullet;
                }
                if p.starts_with("> ") || p == ">" {
                    return Block::Quote;
                }
                Block::Plain
            }
            '`' => {
                if p.chars().count() < 3 {
                    return Block::Pending;
                }
                if p == "```" {
                    return Block::Fence;
                }
                Block::Plain
            }
            _ => Block::Plain,
        }
    }

    fn emit_inline_text(&mut self, text: &str) {
        for c in text.chars() {
            self.emit_inline(c);
        }
    }

    fn emit_inline(&mut self, c: char) {
        if c == '\n' {
            if let Some(marker) = self.pending.take() {
                self.emit_raw(&marker.to_string());
            }
            self.reset_styles();
            println!();
            self.at_line_start = true;
            return;
        }
        if self.code {
            self.emit_raw(&c.to_string());
            return;
        }
        if c == '`' {
            self.flush_pending();
            self.toggle(Style::Code);
            return;
        }
        match (self.pending, c) {
            (Some('*'), '*') | (Some('_'), '_') => {
                self.pending = None;
                self.toggle(Style::Bold);
            }
            (Some('~'), '~') => {
                self.pending = None;
                self.toggle(Style::Strike);
            }
            (Some(marker), _) if marker == '*' || marker == '_' => {
                self.pending = None;
                self.toggle(Style::Italic);
                self.emit_inline(c);
            }
            (Some(marker), _) => {
                self.pending = None;
                self.emit_raw(&marker.to_string());
                self.emit_inline(c);
            }
            (None, '*') | (None, '_') | (None, '~') => {
                self.pending = Some(c);
            }
            (None, _) => self.emit_raw(&c.to_string()),
        }
    }

    fn flush_pending(&mut self) {
        if let Some(marker) = self.pending.take() {
            self.emit_raw(&marker.to_string());
        }
    }

    fn emit_raw(&self, text: &str) {
        print!("{text}");
    }

    fn toggle(&mut self, style: Style) {
        match style {
            Style::Bold => self.bold = !self.bold,
            Style::Italic => self.italic = !self.italic,
            Style::Code => self.code = !self.code,
            Style::Strike => self.strike = !self.strike,
        }
        self.refresh_styles();
    }

    fn refresh_styles(&self) {
        if !Self::colors() {
            return;
        }
        let mut codes = String::new();
        if self.bold {
            codes.push_str("\u{1b}[1m");
        }
        if self.italic {
            codes.push_str("\u{1b}[3m");
        }
        if self.cyan {
            codes.push_str("\u{1b}[36m");
        }
        if self.code {
            codes.push_str("\u{1b}[36m");
        }
        if self.strike {
            codes.push_str("\u{1b}[9m");
        }
        print!("\u{1b}[0m{codes}");
    }

    fn reset_styles(&mut self) {
        let had_style = self.bold || self.italic || self.cyan || self.code || self.strike;
        self.bold = false;
        self.italic = false;
        self.cyan = false;
        self.code = false;
        self.strike = false;
        if had_style && Self::colors() {
            print!("\u{1b}[0m");
        }
    }

    fn colors() -> bool {
        colored::control::SHOULD_COLORIZE.should_colorize()
    }
}

#[derive(Clone, Copy)]
enum Style {
    Bold,
    Italic,
    Code,
    Strike,
}

/// `# 标题` → 返回标题正文；否则 None。
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    rest.strip_prefix(' ')
        .map(str::trim_end)
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// 非流式渲染（`/compact` 摘要、测试用）：整行一次渲染。
// ---------------------------------------------------------------------------

/// 渲染一行（代码块外）：标题 / 引用 / 列表 / 分隔线 + 行内样式。
pub(crate) fn render_block_line(line: &str) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    let bare = trimmed.trim_end();
    if bare.len() >= 3
        && (bare.chars().all(|c| c == '-')
            || bare.chars().all(|c| c == '*')
            || bare.chars().all(|c| c == '_'))
    {
        return format!("{indent}{}", "─".repeat(24).dimmed());
    }

    if let Some(text) = heading_text(trimmed) {
        return format!("{indent}{}", render_inline(text).bold().cyan());
    }

    if let Some(rest) = trimmed
        .strip_prefix("> ")
        .or_else(|| trimmed.strip_prefix(">"))
    {
        return format!("{indent}{} {}", "│".dimmed(), render_inline(rest).italic());
    }

    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return format!("{indent}{} {}", "•".green(), render_inline(rest));
    }

    format!("{indent}{}", render_inline(trimmed))
}

/// 行内样式：`` `code` `` / `**bold**` / `*italic*` / `~~strike~~` / `[text](url)`。
pub(crate) fn render_inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                let code: String = chars[i + 1..end].iter().collect();
                out.push_str(&code.cyan().to_string());
                i = end + 1;
                continue;
            }
        }
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
        if i + 1 < chars.len() && chars[i] == '~' && chars[i + 1] == '~' {
            if let Some(end) = find_double(&chars, i + 2, '~') {
                let text: String = chars[i + 2..end].iter().collect();
                out.push_str(&text.strikethrough().to_string());
                i = end + 2;
                continue;
            }
        }
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

    /// 关键回归：流式渲染必须**立即**产生输出，不能整行缓冲。
    #[test]
    fn stream_is_incremental_not_line_buffered() {
        colored::control::set_override(false);
        let mut md = MarkdownStream::new();
        md.push("这是一段没有换行的较长文本，用来确认前缀缓冲不会无限增长。");
        assert!(md.prefix.len() < 8, "行首前缀不应缓冲整行：{:?}", md.prefix);
        assert!(!md.at_line_start, "前缀阶段应已结束，后续字符立即输出");
        colored::control::unset_override();
    }

    #[test]
    fn stream_handles_bold_toggle_and_fence() {
        colored::control::set_override(false);
        let mut md = MarkdownStream::new();
        md.push("- **加粗** 正常\n");
        assert!(!md.bold, "换行后应复位样式");
        md.push("```\ncode line\n```\n");
        assert!(!md.in_fence, "围栏应已闭合");
        colored::control::unset_override();
    }

    #[test]
    fn short_plain_line_is_not_swallowed() {
        colored::control::set_override(false);
        let mut md = MarkdownStream::new();
        md.push("hi\n");
        assert!(md.prefix.is_empty(), "短行必须立即输出，不得滞留缓冲");
        assert!(md.at_line_start);
        colored::control::unset_override();
    }
}
