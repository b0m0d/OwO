//! Traces：回合轨迹的结构化记录与持久化（可回放、可审计）。

use crate::agent::{TurnEvent, TurnOutcome};
use crate::error::AgentError;
use crate::gateway::TokenUsage;
use crate::session::Session;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PERFORMANCE_TASK_ENV: &str = "OWO_PERF_TASK_ID";
const PERFORMANCE_TASK_IDS: [&str; 8] = [
    "daemon_start_session",
    "short_text_conversation",
    "read_100kb_file",
    "search_1000_files",
    "write_file_and_diff",
    "approval_command",
    "invalid_mcp_startup",
    "disconnect_reconnect_cancel",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub session_id: String,
    pub workspace: String,
    pub model: String,
    pub prompt: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub steps: usize,
    pub final_text: Option<String>,
    pub events: Vec<TurnEvent>,
    #[serde(default)]
    pub usage: TokenUsage,
    /// §9.3 瀑布：同一 trace 内各阶段耗时（按发生顺序；含 model 首 token 时延）。
    #[serde(default)]
    pub phase_timings: Vec<crate::deadline::PhaseTiming>,
    /// Failed/aborted turns retain a trace for diagnosis and performance evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Opt-in fixed-performance-task label. Only allowlisted IDs are persisted;
    /// the environment variable is intended for isolated benchmark daemon processes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub performance_task: Option<String>,
}

impl TraceRecord {
    pub fn from_outcome(session: &Session, outcome: &TurnOutcome) -> Self {
        let workspace = session.workspace.to_string_lossy();
        let workspace = workspace
            .strip_prefix(r"\\?\")
            .unwrap_or(&workspace)
            .to_string();
        Self {
            session_id: session.id.clone(),
            workspace,
            model: session.model.clone(),
            prompt: outcome.prompt.clone(),
            started_at: outcome.started_at.clone(),
            duration_ms: outcome.duration_ms,
            steps: outcome.steps,
            final_text: outcome.final_text.clone(),
            events: outcome.events.clone(),
            usage: outcome.usage,
            phase_timings: outcome.phase_timings.clone(),
            error: None,
            performance_task: configured_performance_task(),
        }
    }

    /// Build a durable trace for a turn that ended before `TurnOutcome` existed.
    pub fn from_error(
        session: &Session,
        prompt: &str,
        started_at: &str,
        duration_ms: u64,
        error: &str,
    ) -> Self {
        let workspace = session.workspace.to_string_lossy();
        let workspace = workspace
            .strip_prefix(r"\\?\")
            .unwrap_or(&workspace)
            .to_string();
        Self {
            session_id: session.id.clone(),
            workspace,
            model: session.model.clone(),
            prompt: prompt.to_string(),
            started_at: started_at.to_string(),
            duration_ms,
            steps: 0,
            final_text: None,
            events: Vec::new(),
            usage: TokenUsage::default(),
            phase_timings: Vec::new(),
            error: Some(error.to_string()),
            performance_task: configured_performance_task(),
        }
    }
}

fn configured_performance_task() -> Option<String> {
    let configured = std::env::var(PERFORMANCE_TASK_ENV).ok()?;
    normalize_performance_task(&configured)
}

fn normalize_performance_task(configured: &str) -> Option<String> {
    let task_id = configured.trim();
    PERFORMANCE_TASK_IDS
        .contains(&task_id)
        .then(|| task_id.to_string())
}

pub fn save_trace(dir: &Path, record: &TraceRecord) -> Result<PathBuf, AgentError> {
    std::fs::create_dir_all(dir)?;
    let stamp = Utc::now().timestamp_millis();
    let path = dir.join(format!("{}-{stamp}.json", record.session_id));
    let content = serde_json::to_vec_pretty(record)?;
    std::fs::write(&path, content)?;
    Ok(path)
}

pub fn load_trace(path: &Path) -> Result<TraceRecord, AgentError> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

pub fn list_traces(dir: &Path) -> Vec<PathBuf> {
    let mut traces = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|ext| ext == "json").unwrap_or(false) {
                traces.push(path);
            }
        }
    }
    traces.sort();
    traces.reverse();
    traces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ChatMessage;

    #[test]
    fn trace_round_trip_and_persistence() {
        let mut session = Session::new(".", "mock", None);
        session.push(ChatMessage::user("你好".to_string()));
        let outcome = TurnOutcome {
            final_text: Some("收到".to_string()),
            steps: 1,
            events: vec![
                TurnEvent::ModelCall,
                TurnEvent::Final {
                    text: "收到".to_string(),
                },
            ],
            prompt: "你好".to_string(),
            started_at: "2026-08-11T00:00:00Z".to_string(),
            duration_ms: 42,
            usage: TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            },
            phase_timings: Vec::new(),
            tools_fingerprint: String::new(),
        };
        let mut record = TraceRecord::from_outcome(&session, &outcome);
        record.performance_task = normalize_performance_task("short_text_conversation");
        let dir = std::env::temp_dir().join(format!("owo-trace-test-{}", uuid::Uuid::new_v4()));
        let path = save_trace(&dir, &record).unwrap();
        let loaded = load_trace(&path).unwrap();
        assert_eq!(loaded.final_text.as_deref(), Some("收到"));
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.usage.total_tokens, 150);
        assert_eq!(
            loaded.performance_task.as_deref(),
            Some("short_text_conversation")
        );
        assert_eq!(list_traces(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §9.3 瀑布持久化：phase_timings（含首 token 时延）随 trace 落盘并可回读；
    /// 旧 trace（无该字段）经 serde default 兼容加载。
    #[test]
    fn trace_persists_phase_timing_waterfall() {
        let mut session = Session::new(".", "mock", None);
        session.push(ChatMessage::user("你好".to_string()));
        let outcome = TurnOutcome {
            final_text: Some("收到".to_string()),
            steps: 1,
            events: vec![],
            prompt: "你好".to_string(),
            started_at: "2026-08-11T00:00:00Z".to_string(),
            duration_ms: 42,
            usage: TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
            phase_timings: vec![crate::deadline::PhaseTiming {
                phase: "model".to_string(),
                elapsed_ms: 120,
                target: String::new(),
                first_token_ms: Some(35),
            }],
            tools_fingerprint: String::new(),
        };
        let record = TraceRecord::from_outcome(&session, &outcome);
        let dir = std::env::temp_dir().join(format!("owo-trace-test-{}", uuid::Uuid::new_v4()));
        let path = save_trace(&dir, &record).unwrap();
        let loaded = load_trace(&path).unwrap();
        assert_eq!(loaded.phase_timings.len(), 1, "瀑布应随 trace 落盘");
        let timing = &loaded.phase_timings[0];
        assert_eq!(timing.phase, "model");
        assert_eq!(timing.elapsed_ms, 120);
        assert_eq!(timing.first_token_ms, Some(35), "首 token 时延应保留");
        assert!(loaded.performance_task.is_none());
        // 旧格式兼容：手写无 phase_timings 字段的 JSON 应可加载（serde default）。
        let legacy: TraceRecord =
            serde_json::from_str(r#"{"session_id":"s","workspace":".","model":"m","prompt":"p","started_at":"t","duration_ms":1,"steps":0,"final_text":null,"events":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#)
                .expect("旧 trace 应兼容加载");
        assert!(legacy.phase_timings.is_empty());
        assert!(legacy.performance_task.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn performance_task_label_is_allowlisted() {
        assert_eq!(
            normalize_performance_task(" short_text_conversation "),
            Some("short_text_conversation".to_string())
        );
        for untrusted in ["../../secrets", "custom-task", "Approval_Command", ""] {
            assert_eq!(normalize_performance_task(untrusted), None);
        }
    }
}
