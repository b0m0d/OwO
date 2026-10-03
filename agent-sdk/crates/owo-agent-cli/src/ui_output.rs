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

fn completion_status_label(status: owo_agent_protocol::CompletionStatusV1) -> String {
    match status {
        owo_agent_protocol::CompletionStatusV1::ResponseComplete => String::new(),
        owo_agent_protocol::CompletionStatusV1::Candidate => " · 代码变更待验收".to_string(),
        owo_agent_protocol::CompletionStatusV1::Accepted => " · 宿主验收通过".to_string(),
        owo_agent_protocol::CompletionStatusV1::Unverified => " · 结果未验证".to_string(),
        owo_agent_protocol::CompletionStatusV1::Blocked => " · 存在阻断问题".to_string(),
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

// ---------- human 流式打印（本地 TurnEvent / Daemon SseEvent 共用） ----------

/// 统一流式打印器：Markdown 增量渲染 + 实时状态（思考中 / 工具参数 / 耗时）。
///
/// - `⏳ 思考中…` 在模型调用开始时显示，首个 token 到达时原地清除（仅 tty）；
/// - `▶ tool <参数预览>` 在工具启动时立即显示（含并发组，核心已改为实时外发）；
/// - `✔/✘ tool（1.2s）` 在工具完成时显示耗时。
pub struct StreamPrinter {
    markdown: crate::markdown::MarkdownStream,
    streamed: bool,
    status_visible: bool,
    tools: std::collections::HashMap<String, std::time::Instant>,
}

impl StreamPrinter {
    pub fn new() -> Self {
        Self {
            markdown: crate::markdown::MarkdownStream::new(),
            streamed: false,
            status_visible: false,
            tools: std::collections::HashMap::new(),
        }
    }

    /// 本地 `TurnEvent`。
    pub fn print_turn(&mut self, event: &TurnEvent) {
        match event {
            TurnEvent::ModelCall => {
                self.clear_status();
                self.show_status("  ⏳ 思考中…");
            }
            TurnEvent::TokenDelta { delta } => {
                self.clear_status();
                self.streamed = true;
                self.markdown.push(delta);
            }
            TurnEvent::ToolStart {
                id,
                tool,
                args_preview,
            } => {
                self.clear_status();
                self.tools.insert(id.clone(), std::time::Instant::now());
                println!(
                    "  {} {tool}{}",
                    "▶".blue(),
                    preview_suffix(args_preview.as_deref())
                );
            }
            TurnEvent::ToolResult {
                id,
                tool,
                ok,
                error,
                // 预览由 TUI 步骤面板消费；行式 REPL 保持单行输出。
                preview: _,
            } => {
                self.clear_status();
                let suffix = self.elapsed_suffix(id);
                if *ok {
                    println!("  {} {tool}{suffix}", "✔".green());
                } else {
                    println!(
                        "  {} {tool}：{}{suffix}",
                        "✘".red(),
                        error.as_deref().unwrap_or("未知错误")
                    );
                }
            }
            TurnEvent::PermissionRequest(request) => {
                self.clear_status();
                print_permission_card(&PermissionCard {
                    tool: &request.tool,
                    level: Some(request.level.label()),
                    reason: &request.reason,
                    args: Some(&request.args),
                    redacted_args: request.redacted_args.as_ref(),
                    risk_note: request.risk_note.as_deref(),
                    explain: None,
                });
            }
            TurnEvent::Compaction { summary } => {
                self.clear_status();
                println!("  {}（上下文已压缩：{}）", "✦".yellow(), summary);
            }
            TurnEvent::ReasoningDelta { delta } => {
                // 思考通道（取优合并自远端 engine）：行式 REPL 以暗色实时输出，
                // 与正文区分；不写入最终回答文本。
                use std::io::Write;
                print!("{}", delta.dimmed());
                let _ = std::io::stdout().flush();
            }
            TurnEvent::PlanUpdate { steps } => {
                // 计划更新：行式 REPL 打印变更后的步骤清单（取优合并自远端 engine）。
                self.clear_status();
                let rendered = steps
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.get("content").and_then(|value| value.as_str()))
                            .collect::<Vec<_>>()
                            .join("；")
                    })
                    .unwrap_or_default();
                if !rendered.is_empty() {
                    println!("  {} 计划更新：{rendered}", "☰".cyan());
                }
            }
            TurnEvent::Final { text } => self.finish_final(text),
        }
    }

    /// Daemon `SseEvent`。
    pub fn print_sse(&mut self, event: &SseEvent) {
        match event {
            SseEvent::Progress { message } => {
                self.clear_status();
                if message.contains("模型调用") {
                    self.show_status("  ⏳ 思考中…");
                } else {
                    println!("{} {message}", "  ↻".cyan());
                }
            }
            SseEvent::TokenDelta { delta } => {
                self.clear_status();
                self.streamed = true;
                self.markdown.push(delta);
            }
            SseEvent::ToolUse { id, tool, args } => {
                self.clear_status();
                self.tools.insert(id.clone(), std::time::Instant::now());
                println!("  {} {tool}{}", "▶".blue(), preview_suffix(args.as_str()));
            }
            SseEvent::ToolResult {
                id,
                tool,
                ok,
                error,
                preview: _,
            } => {
                self.clear_status();
                let suffix = self.elapsed_suffix(id);
                if *ok {
                    println!("  {} {tool}{suffix}", "✔".green());
                } else {
                    println!(
                        "  {} {tool}：{}{suffix}",
                        "✘".red(),
                        error.as_deref().unwrap_or("未知错误")
                    );
                }
            }
            SseEvent::PermissionRequest {
                tool,
                reason,
                level,
                args,
                redacted_args,
                risk_note,
                explain,
                ..
            } => {
                self.clear_status();
                print_permission_card(&PermissionCard {
                    tool,
                    level: level.as_deref(),
                    reason,
                    args: Some(args),
                    redacted_args: redacted_args.as_ref(),
                    risk_note: risk_note.as_deref(),
                    explain: explain.as_ref(),
                });
            }
            SseEvent::PermissionResolved { .. } => {}
            // 回合失败终态（取优合并自远端 engine）：明确打印失败，不留在「执行中」。
            SseEvent::TurnFailed { message } => {
                self.clear_status();
                println!("  {} 回合失败：{message}", "✘".red());
            }
            SseEvent::Compaction { summary } => {
                self.clear_status();
                println!("  {}（上下文已压缩：{}）", "✦".yellow(), summary);
            }
            // 思考通道（取优合并自远端 engine）：暗色流式，不进入最终回答。
            SseEvent::ReasoningDelta { delta } => {
                self.clear_status();
                use std::io::Write;
                print!("{}", delta.dimmed());
                let _ = std::io::stdout().flush();
            }
            SseEvent::PlanUpdate { steps } => {
                self.clear_status();
                let count = steps.as_array().map(Vec::len).unwrap_or(0);
                println!("  {} 计划已更新（{count} 步）", "☰".cyan());
            }
            SseEvent::TurnStats {
                steps,
                duration_ms,
                total_tokens,
                completion_status,
                model_calls,
                ..
            } => {
                self.clear_status();
                println!(
                    "  {} {steps} 步 / {duration_ms} ms / {total_tokens} tokens / {} 次模型请求{}",
                    "⏱".cyan(),
                    model_calls.len(),
                    completion_status_label(*completion_status)
                );
            }
            SseEvent::UserQuestion {
                question_id,
                question,
                options,
            } => {
                self.clear_status();
                println!("  {} 模型提问（{question_id}）：{question}", "?".yellow());
                if !options.is_empty() {
                    println!("     选项：{}", options.join(" / "));
                }
            }
            SseEvent::UserAnswered {
                question_id,
                answer,
                source,
            } => {
                self.clear_status();
                println!(
                    "  {} 已答复（{question_id}/{source}）：{answer}",
                    "✔".green()
                );
            }
            SseEvent::Final { text } => self.finish_final(text),
        }
    }

    /// 收尾：输出 Markdown 残留并清除状态行。
    pub fn finish(&mut self) {
        self.clear_status();
        if self.streamed {
            self.markdown.finish();
            self.streamed = false;
        }
    }

    fn finish_final(&mut self, text: &str) {
        self.clear_status();
        if self.streamed {
            self.markdown.finish();
            self.streamed = false;
        } else {
            println!("\n{}\n", "── 结果 ──".bold());
            self.markdown.push(text);
            self.markdown.finish();
        }
    }

    fn elapsed_suffix(&mut self, id: &str) -> String {
        match self.tools.remove(id) {
            Some(started) => format!("（{:.1}s）", started.elapsed().as_secs_f64()),
            None => String::new(),
        }
    }

    fn show_status(&mut self, text: &str) {
        use std::io::{IsTerminal, Write};
        if std::io::stdout().is_terminal() {
            print!("{text}");
            let _ = std::io::stdout().flush();
            self.status_visible = true;
        } else {
            println!("{text}");
        }
    }

    fn clear_status(&mut self) {
        use std::io::Write;
        if self.status_visible {
            print!("\r\u{1b}[2K");
            let _ = std::io::stdout().flush();
            self.status_visible = false;
        }
    }
}

impl Default for StreamPrinter {
    fn default() -> Self {
        Self::new()
    }
}

fn preview_suffix(preview: Option<&str>) -> String {
    match preview {
        Some(preview) if !preview.is_empty() => format!(" {preview}"),
        _ => String::new(),
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

    #[test]
    fn preview_suffix_formats_tool_args() {
        assert_eq!(
            preview_suffix(Some("{\"path\":\"a\"}")),
            " {\"path\":\"a\"}"
        );
        assert_eq!(preview_suffix(Some("")), "");
        assert_eq!(preview_suffix(None), "");
    }
}
