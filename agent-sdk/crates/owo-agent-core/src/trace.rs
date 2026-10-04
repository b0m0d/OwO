//! Traces：回合轨迹的结构化记录与持久化（可回放、可审计）。

use crate::agent::{ModelCallRecord, TurnEvent, TurnOutcome};
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
    #[serde(default)]
    pub reached_model_turn_limit: bool,
    pub events: Vec<TurnEvent>,
    #[serde(default)]
    pub usage: TokenUsage,
    /// True only when every recorded provider request returned attributable usage.
    #[serde(default)]
    pub usage_known: bool,
    /// Attributable provider call records; legacy traces deserialize as empty.
    #[serde(default)]
    pub model_calls: Vec<ModelCallRecord>,
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
    /// Durable Single completion decision with host evidence and candidate version identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_record: Option<owo_agent_protocol::TaskCompletionRecordV1>,
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
            reached_model_turn_limit: outcome.reached_model_turn_limit,
            events: outcome.events.clone(),
            usage: outcome.usage,
            usage_known: outcome.usage_known,
            model_calls: outcome.model_calls.clone(),
            phase_timings: outcome.phase_timings.clone(),
            error: None,
            performance_task: configured_performance_task(),
            completion_record: single_completion_record(session, outcome.completion_status),
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
        let model_calls = session.transient_model_calls.clone();
        let usage_known = !model_calls.is_empty()
            && model_calls
                .iter()
                .all(|call| call.metadata.usage.is_some());
        let mut usage = TokenUsage::default();
        for call in &model_calls {
            if let Some(request_usage) = call.metadata.usage {
                usage.add(&request_usage);
            }
        }
        Self {
            session_id: session.id.clone(),
            workspace,
            model: session.model.clone(),
            prompt: prompt.to_string(),
            started_at: started_at.to_string(),
            duration_ms,
            steps: 0,
            final_text: None,
            reached_model_turn_limit: false,
            events: Vec::new(),
            usage,
            usage_known,
            model_calls,
            phase_timings: Vec::new(),
            error: Some(error.to_string()),
            performance_task: configured_performance_task(),
            completion_record: single_completion_record(session, owo_agent_protocol::CompletionStatusV1::Unverified),
        }
    }

    /// Build a cancellation trace with an explicit Aborted completion record.
    pub fn from_aborted(
        session: &Session,
        prompt: &str,
        started_at: &str,
        duration_ms: u64,
        error: &str,
    ) -> Self {
        let mut trace = Self::from_error(session, prompt, started_at, duration_ms, error);
        if let Some(record) = trace.completion_record.as_mut() {
            record.status = crate::completion::decide_completion(
                crate::completion::CompletionEvidence {
                    aborted: true,
                    ..crate::completion::CompletionEvidence::default()
                },
            );
            record.decided_at = chrono::Utc::now().to_rfc3339();
        }
        trace
    }
}

fn single_completion_record(
    session: &Session,
    status: owo_agent_protocol::CompletionStatusV1,
) -> Option<owo_agent_protocol::TaskCompletionRecordV1> {
    let context = session.active_task_context.as_ref()?;
    let task_id = context.task_id.as_deref()?;
    let attempt_id = context.attempt_id.as_deref()?;
    let mut evidence_ids = std::collections::BTreeSet::new();
    let current_attempt_validation_ids = session
        .validation_receipts
        .iter()
        .filter(|receipt| {
            receipt.attempt_id == attempt_id
                && (status != owo_agent_protocol::CompletionStatusV1::Accepted
                    || matches!(
                        receipt.verdict,
                        crate::plan::ValidationVerdictV1::Passed
                            | crate::plan::ValidationVerdictV1::ManualAccepted
                    ))
        })
        .map(|receipt| receipt.receipt_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut changed_paths = std::collections::BTreeMap::new();
    for receipt in session
        .execution_receipts
        .iter()
        .filter(|receipt| {
            let accepted_by_current_attempt = receipt.status == "accepted"
                && receipt.validation_receipt_id.as_deref().is_some_and(|id| {
                    current_attempt_validation_ids.contains(id)
                });
            if status == owo_agent_protocol::CompletionStatusV1::Accepted {
                accepted_by_current_attempt
            } else {
                (receipt.turn_id == attempt_id || accepted_by_current_attempt)
                    && receipt.status != "reverted"
                    && receipt.status != "stale"
            }
        })
    {
        evidence_ids.insert(receipt.receipt_id.clone());
        for path in &receipt.changed_files {
            let normalized = path.replace('\\', "/");
            let after_hash = receipt
                .after_hashes
                .iter()
                .find(|(candidate, _)| candidate.replace('\\', "/") == normalized)
                .map(|(_, hash)| hash.clone())
                .unwrap_or(None);
            changed_paths.insert(normalized, after_hash);
        }
    }
    for receipt in session
        .validation_receipts
        .iter()
        .rev()
        .filter(|receipt| {
            receipt.attempt_id == attempt_id
                && (status != owo_agent_protocol::CompletionStatusV1::Accepted
                    || matches!(
                        receipt.verdict,
                        crate::plan::ValidationVerdictV1::Passed
                            | crate::plan::ValidationVerdictV1::ManualAccepted
                    ))
        })
    {
        evidence_ids.insert(receipt.receipt_id.clone());
        for (subject, hash) in &receipt.subject_sha256 {
            let Some(path) = subject.strip_prefix("workspace-path:") else {
                continue;
            };
            changed_paths.entry(path.replace('\\', "/")).or_insert_with(|| {
                if hash == &crate::verification::workspace_path_absence_sha256() {
                    None
                } else {
                    Some(hash.clone())
                }
            });
        }
    }
    let candidate_version_sha256 = if changed_paths.is_empty() {
        None
    } else {
        Some(crate::completion::hash_candidate_version(&changed_paths).ok()?)
    };
    Some(crate::completion::build_completion_record(
        task_id,
        attempt_id,
        status,
        evidence_ids,
        candidate_version_sha256,
    ))
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
    fn error_trace_preserves_successful_and_failed_provider_requests() {
        let mut session = Session::new(".", "mock", None);
        session.transient_model_calls = vec![
            crate::agent::ModelCallRecord {
                metadata: crate::gateway::ModelCallMetadata {
                    request_id: Some("req-before-error".to_string()),
                    model: Some("served-model".to_string()),
                    usage: Some(TokenUsage {
                        prompt_tokens: 30,
                        completion_tokens: 5,
                        total_tokens: 35,
                    }),
                    latency_ms: Some(120),
                },
                succeeded: true,
            },
            crate::agent::ModelCallRecord {
                metadata: crate::gateway::ModelCallMetadata {
                    model: Some("served-model".to_string()),
                    latency_ms: Some(5000),
                    ..crate::gateway::ModelCallMetadata::default()
                },
                succeeded: false,
            },
        ];
        let trace = TraceRecord::from_error(
            &session,
            "implement feature",
            "2026-10-04T00:00:00Z",
            5120,
            "provider timeout",
        );
        assert_eq!(trace.model_calls.len(), 2);
        assert!(trace.model_calls[0].succeeded);
        assert!(!trace.model_calls[1].succeeded);
        assert_eq!(trace.model_calls[0].metadata.usage.unwrap().total_tokens, 35);
        assert_eq!(trace.usage.total_tokens, 35);
        assert!(!trace.usage_known, "失败请求 usage 未知时不得将部分合计标为完整");
        assert_eq!(trace.model_calls[1].metadata.latency_ms, Some(5000));
        let restored: TraceRecord = serde_json::from_value(serde_json::to_value(trace).unwrap()).unwrap();
        assert_eq!(restored.model_calls.len(), 2);
        assert!(!restored.model_calls[1].succeeded);
    }

    #[test]
    fn aborted_trace_persists_an_explicit_aborted_completion_status() {
        let mut session = Session::new(".", "mock", None);
        session.active_task_context = Some(
            crate::task_context::ResolvedTaskContext::for_single_turn(
                "turn-cancelled",
                "完成这项代码任务",
            ),
        );
        let trace = TraceRecord::from_aborted(
            &session,
            "完成这项代码任务",
            "2026-10-04T00:00:00Z",
            100,
            "agent aborted by user",
        );
        assert_eq!(
            trace.completion_record.unwrap().status,
            owo_agent_protocol::CompletionStatusV1::Aborted
        );
    }

    #[test]
    fn completion_record_includes_prior_writes_accepted_by_this_attempt() {
        let mut session = Session::new(".", "mock", None);
        session.active_task_context = Some(
            crate::task_context::ResolvedTaskContext::for_single_turn(
                "turn-current",
                "请验收已有候选",
            ),
        );
        let hash = crate::CasStore::hash_of(b"fn main() {}\n");
        session.execution_receipts.push(crate::session::ExecutionReceipt {
            receipt_id: "exec-prior".to_string(),
            tool: "write_file".to_string(),
            turn_id: "turn-prior".to_string(),
            changed_files: vec!["src/main.rs".to_string()],
            snapshot_keys: Default::default(),
            before_hashes: std::collections::HashMap::from([("src/main.rs".to_string(), None)]),
            after_hashes: std::collections::HashMap::from([(
                "src/main.rs".to_string(),
                Some(hash.clone()),
            )]),
            diff_sha256: "diff".to_string(),
            created_at: "2026-10-04T00:00:00Z".to_string(),
            status: "accepted".to_string(),
            validation_receipt_id: Some("manual-current".to_string()),
        });
        session.validation_receipts.push(crate::plan::ValidationReceiptV1 {
            receipt_id: "manual-current".to_string(),
            task_id: "session-1".to_string(),
            attempt_id: "turn-current".to_string(),
            epoch: 1,
            requirement_id: "manual-acceptance".to_string(),
            validator_id: crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID.to_string(),
            validator_version: "1".to_string(),
            arguments_sha256: crate::CasStore::hash_of(b"{}"),
            input_sha256: crate::CasStore::hash_of("请验收已有候选".as_bytes()),
            environment_id: "workspace".to_string(),
            changeset_sha256: Some("changeset".to_string()),
            detail: Some("accepted".to_string()),
            subject_sha256: std::collections::HashMap::from([(
                "workspace-path:src/main.rs".to_string(),
                hash.clone(),
            )]),
            verdict: crate::plan::ValidationVerdictV1::ManualAccepted,
            evidence_refs: vec!["manual-question:q1".to_string()],
            review_result: None,
            started_at: "2026-10-04T00:00:00Z".to_string(),
            completed_at: "2026-10-04T00:00:01Z".to_string(),
        });
        let mut failed_receipt = session.validation_receipts.last().unwrap().clone();
        failed_receipt.receipt_id = "failed-current".to_string();
        failed_receipt.requirement_id = "failed-requirement".to_string();
        failed_receipt.validator_id = "failed-validator".to_string();
        failed_receipt.verdict = crate::plan::ValidationVerdictV1::Failed;
        session.validation_receipts.push(failed_receipt);

        let record = single_completion_record(
            &session,
            owo_agent_protocol::CompletionStatusV1::Accepted,
        )
        .unwrap();
        let expected = crate::completion::hash_candidate_version(
            &std::collections::BTreeMap::from([(
                "src/main.rs".to_string(),
                Some(hash),
            )]),
        )
        .unwrap();
        assert_eq!(record.candidate_version_sha256.as_deref(), Some(expected.as_str()));
        assert!(record.evidence_receipt_ids.contains(&"exec-prior".to_string()));
        assert!(record.evidence_receipt_ids.contains(&"manual-current".to_string()));
        assert!(!record.evidence_receipt_ids.contains(&"failed-current".to_string()));
        let diagnostic_record = single_completion_record(
            &session,
            owo_agent_protocol::CompletionStatusV1::Unverified,
        )
        .unwrap();
        assert!(diagnostic_record
            .evidence_receipt_ids
            .contains(&"failed-current".to_string()));
    }

    #[test]
    fn trace_round_trip_and_persistence() {
        let mut session = Session::new(".", "mock", None);
        session.push(ChatMessage::user("你好".to_string()));
        session.active_task_context = Some(
            crate::task_context::ResolvedTaskContext::for_single_turn("turn-trace-1", "你好"),
        );
        let outcome = TurnOutcome {
            model_calls: vec![crate::agent::ModelCallRecord {
                metadata: crate::gateway::ModelCallMetadata {
                    request_id: Some("req-1".to_string()),
                    model: Some("served-model".to_string()),
                    usage: Some(TokenUsage {
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        total_tokens: 150,
                    }),
                    latency_ms: Some(25),
                },
                succeeded: true,
            }],
            final_text: Some("收到".to_string()),
            completion_status: owo_agent_protocol::CompletionStatusV1::ResponseComplete,
            reached_model_turn_limit: false,
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
            usage_known: true,
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
        let completion = loaded.completion_record.as_ref().unwrap();
        assert_eq!(completion.task_id, "single-turn:turn-trace-1");
        assert_eq!(completion.attempt_id, "turn-trace-1");
        assert_eq!(completion.status, owo_agent_protocol::CompletionStatusV1::ResponseComplete);
        assert_eq!(loaded.final_text.as_deref(), Some("收到"));
        assert_eq!(loaded.events.len(), 2);
        assert_eq!(loaded.usage.total_tokens, 150);
        assert_eq!(loaded.model_calls.len(), 1);
        assert_eq!(
            loaded.model_calls[0].metadata.request_id.as_deref(),
            Some("req-1")
        );
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
            model_calls: Vec::new(),
            final_text: Some("收到".to_string()),
            completion_status: owo_agent_protocol::CompletionStatusV1::ResponseComplete,
            reached_model_turn_limit: false,
            steps: 1,
            events: vec![],
            prompt: "你好".to_string(),
            started_at: "2026-08-11T00:00:00Z".to_string(),
            duration_ms: 42,
            usage_known: true,
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
        assert!(legacy.completion_record.is_none());
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
