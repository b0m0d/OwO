//! TUI 主题、键位解析与差异面板渲染（自 `tui.rs` 机械提取）。
//!
//! 纯渲染/解析逻辑，不接触 Daemon 与会话状态；调用方经 `use keys::*;` 保持原名。
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use owo_agent_protocol::FileDiff;
use ratatui::style::{Color, Modifier, Style};
use std::collections::HashMap;

/// 由 `FileDiff` 列表构造差异面板（纯函数，可离线单测）。
pub(super) fn build_diff_panel(diffs: &[FileDiff]) -> (Vec<(String, Style)>, bool) {
    if diffs.is_empty() {
        return (vec![("当前会话没有未回滚的改动".to_string(), dim())], false);
    }
    let mut lines = Vec::new();
    for diff in diffs {
        lines.push((format!("● {}", diff.path), cyan()));
        match &diff.before {
            Some(before) => {
                for line in before.lines() {
                    lines.push((format!("- {line}"), red()));
                }
            }
            None => lines.push(("(新建文件)".to_string(), green())),
        }
        match &diff.after {
            Some(after) => {
                for line in after.lines() {
                    lines.push((format!("+ {line}"), green()));
                }
            }
            None => lines.push(("(已删除)".to_string(), red())),
        }
    }
    (lines, true)
}

#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) accent: Color,
}

pub(super) fn theme(name: Option<&str>) -> Theme {
    match name {
        Some("light") => Theme {
            accent: Color::Blue,
        },
        _ => Theme {
            accent: Color::Cyan,
        },
    }
}

pub(super) fn parse_keybind(spec: &str) -> Option<KeyEvent> {
    let tokens: Vec<&str> = spec.split('+').map(str::trim).collect();
    let key_token = *tokens.last()?;
    if key_token.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    for token in &tokens[..tokens.len().saturating_sub(1)] {
        match token.to_lowercase().as_str() {
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            _ => {}
        }
    }
    let lower = key_token.to_lowercase();
    let code = match lower.as_str() {
        "tab" => KeyCode::Tab,
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        _ if key_token.len() >= 2 && key_token.starts_with('f') => {
            let number: u8 = key_token[1..].parse().ok()?;
            if (1..=12).contains(&number) {
                KeyCode::F(number)
            } else {
                KeyCode::Char(key_token.chars().next()?)
            }
        }
        _ => KeyCode::Char(key_token.chars().next()?),
    };
    Some(KeyEvent::new(code, modifiers))
}

pub(super) fn build_keybinds(configured: &HashMap<String, String>) -> HashMap<String, KeyEvent> {
    let defaults = [
        ("toggle_mode", "tab"),
        ("abort", "ctrl+c"),
        ("scroll_up", "pageup"),
        ("scroll_down", "pagedown"),
        ("clear", "ctrl+l"),
        ("toggle_diff", "d"),
    ];
    let mut map = HashMap::new();
    for (action, spec) in defaults {
        if let Some(key) = parse_keybind(spec) {
            map.insert(action.to_string(), key);
        }
    }
    for (action, spec) in configured {
        if let Some(key) = parse_keybind(spec) {
            map.insert(action.clone(), key);
        }
    }
    map
}

pub(super) fn format_key(event: &KeyEvent) -> String {
    let mut parts: Vec<String> = Vec::new();
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }
    let key = match event.code {
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::PageUp => "pageup".to_string(),
        KeyCode::PageDown => "pagedown".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Char(character) => character.to_string(),
        other => format!("{other:?}").to_lowercase(),
    };
    parts.push(key);
    parts.join("+")
}

pub(super) fn default() -> Style {
    Style::default()
}
pub(super) fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
pub(super) fn cyan() -> Style {
    Style::default().fg(Color::Cyan)
}
pub(super) fn blue() -> Style {
    Style::default().fg(Color::Blue)
}
pub(super) fn green() -> Style {
    Style::default().fg(Color::Green)
}
pub(super) fn yellow() -> Style {
    Style::default().fg(Color::Yellow)
}
pub(super) fn red() -> Style {
    Style::default().fg(Color::Red)
}
pub(super) fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}
