//! Traces：回合轨迹的结构化记录与持久化（可回放、可审计）。

use crate::agent::{TurnEvent, TurnOutcome};
use crate::error::AgentError;
use crate::gateway::TokenUsage;
use crate::session::Session;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
        }
    }
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
        let record = TraceRecord::from_outcome(&session, &outcome);
        let dir = std::env::temp_dir().join(format!("owo-trace-test-{}", uuid::Uuid::new_v4()));
        let path = save_trace(&dir, &record).unwrap();
        let loaded = load_trace(&path).unwrap();
        assert_eq!(loaded.final_text.as_deref(), Some("收到"));
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.usage.total_tokens, 150);
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
        // 旧格式兼容：手写无 phase_timings 字段的 JSON 应可加载（serde default）。
        let legacy: TraceRecord =
            serde_json::from_str(r#"{"session_id":"s","workspace":".","model":"m","prompt":"p","started_at":"t","duration_ms":1,"steps":0,"final_text":null,"events":[],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}}"#)
                .expect("旧 trace 应兼容加载");
        assert!(legacy.phase_timings.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
