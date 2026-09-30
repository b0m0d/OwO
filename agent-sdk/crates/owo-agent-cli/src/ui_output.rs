//! §11 CLI 统一输出：UiEvent channel + human/plain/jsonl 三种渲染。
//!
//! 完成标准（审计 §11）：`owo-agent turn …` 的 stdout 在 `--output jsonl` 下
//! 只承载稳定 JSONL 协议（重定向/管道可机器解析）；human 模式保持既有观感
//! 不变；plain 模式输出最小稳定文本。tracing 日志走 stderr，永不污染 stdout。

use colored::Colorize;
use owo_agent_core::TurnEvent;
use owo_agent_protocol::{PermissionResponse, SseEvent};
use serde_json::{json, Value};

/// `--output` 输出模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputMode {
    /// 人类可读（默认）：与既有观感一致（状态行走 EventPrinter）。
    Human,
    /// 纯文本：stdout 仅最终结果与改动文件路径；状态行走 stderr。
    Plain,
    /// JSONL：stdout 每行一个 JSON 事件/结果（协议稳定，供重定向/编排）。
    Jsonl,
}

/// `--permissions` 权限档案（§11 顶层权限参数，global）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PermissionsProfile {
    /// 默认：写/执行走审批（与现状一致）。
    Default,
    /// 只读：强制 read-only 策略（注册表无写/执行工具）。
    ReadOnly,
    /// 可信：跳过审批（等价 --no-approval；仅测试/自动化用）。
    Trusted,
}

impl OutputMode {
    /// jsonl 模式判定（UiSink/渲染器共用）。
    #[allow(dead_code)] // repl/tui JSONL 迁移完成前保留判定入口（turn 直接比对枚举）。
    pub fn is_jsonl(self) -> bool {
        matches!(self, Self::Jsonl)
    }
}

// ---------- 协议渲染纯函数（协议稳定性由测试锁定） ----------

/// jsonl：把任意可序列化事件包裹为稳定协议行（turn/repl 共用同一 schema）。
pub fn render_event_jsonl<T: serde::Serialize>(event: &T) -> String {
    json!({ "type": "turn_event", "event": event }).to_string()
}

/// jsonl：最终结果行（字段集即协议契约）。
pub fn render_final_result_jsonl(
    text: Option<&str>,
    steps: usize,
    diff_paths: &[String],
    trace_path: Option<&str>,
    audit_count: usize,
) -> String {
    json!({
        "type": "turn_result",
        "steps": steps,
        "final_text": text,
        "diffs": diff_paths,
        "trace_path": trace_path,
        "audit_count": audit_count,
    })
    .to_string()
}

/// jsonl：错误行。
pub fn render_error_jsonl(message: &str) -> String {
    json!({ "type": "error", "message": message }).to_string()
}

/// plain：最终结果 stdout 行集（最终文本在前，改动文件带 `M ` 前缀）。
pub fn render_final_result_plain(text: Option<&str>, diff_paths: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(text) = text {
        lines.push(text.to_string());
    }
    for diff in diff_paths {
        lines.push(format!("M {diff}"));
    }
    lines
}

/// 把 CLI 审批输入映射到与桌面端相同的四种授权动作。
/// 未识别输入始终拒绝；`s/session` 是旧版“本次会话”的兼容别名，
/// 统一降为任务范围，不再向服务端发送旧 `session` scope。
pub fn parse_approval_response(input: &str) -> PermissionResponse {
    let (allow, scope) = match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "1" | "once" => (true, Some("once")),
        "t" | "task" | "s" | "session" => (true, Some("task")),
        "w" | "workspace" => (true, Some("workspace")),
        "n" | "no" | "d" | "deny" | "" => (false, None),
        _ => (false, None),
    };
    PermissionResponse {
        allow,
        remember: None,
        scope: scope.map(str::to_string),
    }
}

// ---------- 审批卡（本地 Approver / Daemon SSE 两条路径共用同一观感） ----------

/// 审批卡数据：两条路径字段不完全一致，用 Option 收敛；参数优先用脱敏视图。
pub(crate) struct PermissionCard<'a> {
    pub tool: &'a str,
    pub level: Option<&'a str>,
    pub reason: &'a str,
    pub args: Option<&'a Value>,
    pub redacted_args: Option<&'a Value>,
    pub risk_note: Option<&'a str>,
    pub explain: Option<&'a Value>,
}

/// 渲染审批卡：让用户看清"批准的是什么"（工具/等级/原因/风险/影响/参数）。
pub(crate) fn print_permission_card(card: &PermissionCard<'_>) {
    println!(
        "  {} {} 请求 {} 权限",
        "审批".yellow(),
        card.tool,
        card.level.unwrap_or("unknown")
    );
    if !card.reason.trim().is_empty() {
        println!("    原因：{}", card.reason);
    }
    if let Some(note) = card.risk_note {
        // 内置工具无风险声明时核心会填统一占位串——展示侧抑制，避免每张卡都刷噪声。
        let is_placeholder =
            note.contains("未声明风险信息") || note.contains("未提供 MCP annotations");
        if !note.trim().is_empty() && !is_placeholder {
            println!("    {} {}", "风险：".red(), note);
        }
    }
    if let Some(explain) = card.explain {
        if !explain.is_null() && *explain != json!({}) {
            println!("    影响：{}", summarize_permission_args(explain, 240));
        }
    }
    if let Some(view) = card.redacted_args.or(card.args) {
        let summary = summarize_permission_args(view, 240);
        if !summary.is_empty() {
            println!("    参数：{summary}");
        }
    }
}

/// 审批卡参数摘要：紧凑 JSON，超长按字符截断（秘密字段已由 redacted_args 脱敏）。
pub(crate) fn summarize_permission_args(value: &Value, max_chars: usize) -> String {
    let raw = value.to_string();
    if raw == "null" || raw == "{}" {
        return String::new();
    }
    if raw.chars().count() <= max_chars {
        return raw;
    }
    let truncated: String = raw.chars().take(max_chars).collect();
    format!("{truncated}…")
}

// ---------- human 事件打印（自 turn.rs 迁入；repl 仍复用同一观感） ----------

/// 事件打印器：把流式增量经 Markdown 渲染逐行输出，Final 只收尾不重复打印。
pub struct EventPrinter {
    markdown: crate::markdown::MarkdownStream,
    streamed: bool,
}

impl EventPrinter {
    pub fn new() -> Self {
        Self {
            markdown: crate::markdown::MarkdownStream::new(),
            streamed: false,
        }
    }

    pub fn print(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::TokenDelta { delta } => {
                self.streamed = true;
                self.markdown.push(delta);
            }
            TurnEvent::Final { text } => {
                if self.streamed {
                    self.markdown.finish();
                    self.streamed = false;
                } else {
                    println!("\n{}\n", "── 结果 ──".bold());
                    self.markdown.push(text);
                    self.markdown.finish();
                }
            }
            other => {
                if self.streamed {
                    self.markdown.finish();
                    self.streamed = false;
                }
                print_event(other);
            }
        }
    }
}

impl Default for EventPrinter {
    fn default() -> Self {
        Self::new()
    }
}

pub fn print_event(event: &TurnEvent) {
    match event {
        TurnEvent::ModelCall => println!("{}", "  ↻ 调用模型…".cyan()),
        TurnEvent::PermissionRequest(request) => print_permission_card(&PermissionCard {
            tool: &request.tool,
            level: Some(request.level.label()),
            reason: &request.reason,
            args: Some(&request.args),
            redacted_args: request.redacted_args.as_ref(),
            risk_note: request.risk_note.as_deref(),
            explain: None,
        }),
        TurnEvent::ToolStart { tool, .. } => {
            println!("  {} {tool} …", "▶".blue());
        }
        TurnEvent::ToolResult {
            tool, ok, error, ..
        } => {
            if *ok {
                println!("  {} {tool}", "✔".green());
            } else {
                println!(
                    "  {} {tool}：{}",
                    "✘".red(),
                    error.as_deref().unwrap_or("未知错误")
                );
            }
        }
        TurnEvent::TokenDelta { .. } => {}
        TurnEvent::Compaction { summary } => {
            println!("  {}（上下文已压缩：{}）", "✦".yellow(), summary);
        }
        TurnEvent::Final { .. } => {}
    }
}

/// human 模式：单个 SSE 事件行（turn/repl 共用同一观感）。
pub fn print_sse_event_human(event: &SseEvent) {
    match event {
        SseEvent::Progress { message } => println!("{} {message}", "  ↻".cyan()),
        SseEvent::ToolUse { tool, .. } => println!("  {} {tool} …", "▶".blue()),
        SseEvent::ToolResult {
            tool, ok, error, ..
        } => {
            if *ok {
                println!("  {} {tool}", "✔".green());
            } else {
                println!(
                    "  {} {tool}：{}",
                    "✘".red(),
                    error.as_deref().unwrap_or("未知错误")
                );
            }
        }
        SseEvent::PermissionRequest { .. } => {}
        SseEvent::Compaction { summary } => {
            println!("  {}（上下文已压缩：{}）", "✦".yellow(), summary);
        }
        SseEvent::TokenDelta { .. } | SseEvent::Final { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_turn_event_wraps_event() {
        let event = owo_agent_protocol::SseEvent::Progress {
            message: "模型调用".to_string(),
        };
        let line = render_event_jsonl(&event);
        let value: serde_json::Value = serde_json::from_str(&line).expect("应为合法 JSON");
        assert_eq!(value["type"], "turn_event");
        assert!(value["event"].is_object(), "应包裹原始事件");
        assert_eq!(value["event"]["type"], "progress");
    }

    #[test]
    fn jsonl_turn_result_line_fields_are_stable() {
        let line = render_final_result_jsonl(
            Some("完成"),
            3,
            &["src/main.rs".to_string()],
            Some("trace.json"),
            7,
        );
        let value: serde_json::Value = serde_json::from_str(&line).expect("应为合法 JSON");
        assert_eq!(value["type"], "turn_result");
        assert_eq!(value["steps"], 3);
        assert_eq!(value["final_text"], "完成");
        assert_eq!(value["diffs"], serde_json::json!(["src/main.rs"]));
        assert_eq!(value["trace_path"], "trace.json");
        assert_eq!(value["audit_count"], 7);
    }

    #[test]
    fn jsonl_turn_result_line_allows_null_fields() {
        let value: serde_json::Value =
            serde_json::from_str(&render_final_result_jsonl(None, 0, &[], None, 0))
                .expect("应为合法 JSON");
        assert!(value["final_text"].is_null());
        assert!(value["trace_path"].is_null());
        assert_eq!(value["diffs"], serde_json::json!([]));
    }

    #[test]
    fn jsonl_error_line_carries_message() {
        let value: serde_json::Value =
            serde_json::from_str(&render_error_jsonl("boom")).expect("应为合法 JSON");
        assert_eq!(value["type"], "error");
        assert_eq!(value["message"], "boom");
    }

    #[test]
    fn plain_lines_are_final_text_then_diff_markers() {
        let lines =
            render_final_result_plain(Some("答案"), &["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(
            lines,
            vec![
                "答案".to_string(),
                "M a.txt".to_string(),
                "M b.txt".to_string()
            ]
        );
        assert!(render_final_result_plain(None, &[]).is_empty());
    }

    #[test]
    fn approval_choices_match_the_four_desktop_actions() {
        for (input, expected_scope) in [
            ("y", "once"),
            ("once", "once"),
            ("t", "task"),
            ("task", "task"),
            ("w", "workspace"),
            ("workspace", "workspace"),
            ("s", "task"),
            ("session", "task"),
        ] {
            let response = parse_approval_response(input);
            assert!(response.allow, "{input} should allow");
            assert_eq!(response.scope.as_deref(), Some(expected_scope));
            assert!(response.remember.is_none());
        }
    }

    #[test]
    fn approval_rejection_and_unknown_input_fail_closed() {
        for input in ["", "n", "no", "d", "deny", "typo"] {
            let response = parse_approval_response(input);
            assert!(!response.allow, "{input} should deny");
            assert!(response.scope.is_none());
            assert!(response.remember.is_none());
        }
    }

    #[test]
    fn permission_args_summary_skips_empty_and_truncates_long() {
        assert_eq!(summarize_permission_args(&json!(null), 10), "");
        assert_eq!(summarize_permission_args(&json!({}), 10), "");
        let short = summarize_permission_args(&json!({ "path": "a.txt" }), 240);
        assert!(short.contains("a.txt"));
        let long = summarize_permission_args(&json!({ "content": "x".repeat(500) }), 20);
        assert!(long.chars().count() <= 21, "truncated must fit: {long}");
        assert!(long.ends_with('…'));
    }
}
