//! §11 CLI 统一输出：UiEvent channel + human/plain/jsonl 三种渲染。
//!
//! 完成标准（审计 §11）：`owo-agent turn …` 的 stdout 在 `--output jsonl` 下
//! 只承载稳定 JSONL 协议（重定向/管道可机器解析）；human 模式保持既有观感
//! 不变；plain 模式输出最小稳定文本。tracing 日志走 stderr，永不污染 stdout。

use owo_agent_core::TurnEvent;
use serde_json::json;

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
    pub fn is_jsonl(self) -> bool {
        matches!(self, Self::Jsonl)
    }
}

// ---------- 协议渲染纯函数（协议稳定性由测试锁定；UiSink 委托到这里） ----------

/// jsonl：turn 事件 → 一行 JSON；TokenDelta 不进协议（返回 None）。
pub fn render_turn_event_jsonl(event: &TurnEvent) -> Option<String> {
    if matches!(event, TurnEvent::TokenDelta { .. }) {
        return None;
    }
    Some(json!({ "type": "turn_event", "event": event }).to_string())
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

/// 统一 UI 事件出口（§11 UiEvent channel 的 sink 侧）。
pub struct UiSink {
    mode: OutputMode,
}

impl UiSink {
    pub fn new(mode: OutputMode) -> Self {
        Self { mode }
    }

    /// turn 生命周期事件的统一渲染。
    pub fn emit_turn_event(&self, event: &TurnEvent) {
        match self.mode {
            // human 由既有 EventPrinter 负责（保持观感不变）。
            OutputMode::Human => {}
            OutputMode::Plain => {
                if let TurnEvent::ToolResult {
                    tool, ok, error, ..
                } = event
                {
                    if !ok {
                        eprintln!("tool {tool} 失败：{}", error.as_deref().unwrap_or("-"));
                    }
                }
            }
            OutputMode::Jsonl => {
                self.emit_line(&render_turn_event_jsonl(event).unwrap_or_default());
            }
        }
    }

    /// 输出一行（仅 jsonl 模式；空行忽略）。
    fn emit_line(&self, line: &str) {
        if self.mode.is_jsonl() && !line.is_empty() {
            println!("{line}");
        }
    }

    /// 最终结果（三种模式各自的稳定形态）。
    pub fn final_result(
        &self,
        text: Option<&str>,
        steps: usize,
        diff_paths: &[String],
        trace_path: Option<&str>,
        audit_count: usize,
    ) {
        match self.mode {
            OutputMode::Human => {
                println!("\n[完成] 工具步数：{}，最终文本：{}", steps, text.is_some());
                if !diff_paths.is_empty() {
                    println!("[diff] 本次会话改动文件：");
                    for diff in diff_paths {
                        println!("  - {diff}");
                    }
                }
                println!("[审计] 记录 {audit_count} 条");
                if let Some(path) = trace_path {
                    println!("[trace] {path}");
                }
            }
            OutputMode::Plain => {
                for line in render_final_result_plain(text, diff_paths) {
                    println!("{line}");
                }
                if let Some(path) = trace_path {
                    eprintln!("trace {path}");
                }
            }
            OutputMode::Jsonl => {
                self.emit_line(&render_final_result_jsonl(
                    text,
                    steps,
                    diff_paths,
                    trace_path,
                    audit_count,
                ));
            }
        }
    }

    /// 错误出口（jsonl → stdout 协议行；其余 → stderr）。
    pub fn error(&self, message: &str) {
        match self.mode {
            OutputMode::Jsonl => self.emit_line(&render_error_jsonl(message)),
            _ => eprintln!("错误：{message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_turn_event_wraps_event_and_skips_token_delta() {
        let line = render_turn_event_jsonl(&TurnEvent::ModelCall).expect("普通事件应有协议行");
        let value: serde_json::Value = serde_json::from_str(&line).expect("应为合法 JSON");
        assert_eq!(value["type"], "turn_event");
        assert!(value["event"].is_object(), "应包裹原始事件");
        assert_eq!(
            render_turn_event_jsonl(&TurnEvent::TokenDelta {
                delta: "x".to_string(),
            }),
            None,
            "TokenDelta 不进 JSONL 协议"
        );
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
}
