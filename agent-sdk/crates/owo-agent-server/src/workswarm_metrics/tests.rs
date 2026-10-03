use super::*;
use async_trait::async_trait;
use owo_agent_core::gateway::{
    ChatMessage, ModelCallMetadata, ModelOutput, ModelProvider, ObservedModelOutput, StreamChunk,
    TokenUsage,
};
use owo_agent_core::tools::ToolSpec;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[tokio::test]
async fn measured_provider_counts_each_model_call() {
    struct StubProvider;
    #[async_trait]
    impl ModelProvider for StubProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Ok(ModelOutput::Text("ok".to_string()))
        }
    }
    let calls = Arc::new(AtomicU64::new(0));
    let provider = MeasuredProvider::new(Arc::new(StubProvider), Arc::clone(&calls));
    let msgs: Vec<ChatMessage> = Vec::new();
    let _ = provider.complete(&msgs, &[]).await;
    let _ = provider.complete(&msgs, &[]).await;
    let mut deltas: Vec<String> = Vec::new();
    let _ = provider
        .complete_stream(&msgs, &[], &mut |d: String| deltas.push(d))
        .await;
    assert_eq!(
        calls.load(Ordering::Relaxed),
        3,
        "每次模型调用恰好计一次（complete ×2 + complete_stream ×1）"
    );
    // usage_snapshot 透传（stub 无用量上报 → 零）。
    assert_eq!(provider.usage_snapshot().total_tokens, 0);
}

fn span(role: &str, step: &str, cost: f64, attempt: u32, started_ms: u64) -> WorkerSpanRecord {
    WorkerSpanRecord {
        span_id: format!("span-{role}-{attempt}"),
        team_id: "team-t".to_string(),
        member_id: format!("m-{role}"),
        role: role.to_string(),
        worker_kind: "echo".to_string(),
        step_id: step.to_string(),
        started_at: "2026-08-28T00:00:00+00:00".to_string(),
        ended_at: "2026-08-28T00:00:01+00:00".to_string(),
        started_at_ms: started_ms,
        ended_at_ms: started_ms + 1000,
        wall_ms: 1000,
        provider_wait_ms: 0,
        lease_wait_ms: 0,
        outcome: "succeeded".to_string(),
        error: None,
        model_calls: 0,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        usage_attribution: "not_applicable".to_string(),
        requests: Vec::new(),
        cost_usd: cost,
        attempt,
        artifact: Some(SpanArtifact {
            artifact_id: format!("team-t:{role}:v{attempt}"),
            kind: "document".to_string(),
            version: attempt,
        }),
    }
}

#[test]
fn journal_roundtrip_and_corrupt_line_skip() {
    let dir = std::env::temp_dir().join(format!(
        "owo-metrics-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let journal = MetricsJournal::for_team(&dir, "team-j");
    assert!(journal.read_records().is_empty(), "无文件 = 空记录");
    journal.append(&span("planner", "s1", 0.0, 1, 1)).unwrap();
    journal.append(&span("builder", "s2", 0.5, 1, 2)).unwrap();
    // 追加半行/坏行（崩溃中断写安全）。
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(journal.path())
            .unwrap();
        writeln!(f, "{{\"broken json").unwrap();
    }
    let records = journal.read_records();
    assert_eq!(records.len(), 2, "坏行应被跳过：{records:?}");
    assert_eq!(records[1].role, "builder");
    assert_eq!(journal.count_step_spans("s2"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn aggregate_reports_roles_slowest_and_rework() {
    let records = vec![
        span("planner", "s1", 0.0, 1, 100),
        span("builder", "s2", 0.25, 1, 200),
        span("builder", "s2", 0.25, 2, 300),
    ];
    let agg = aggregate_metrics("team-t", &records, &json!({}));
    let summary = &agg["summary"];
    assert_eq!(summary["span_count"], 3);
    assert_eq!(summary["succeeded_spans"], 3);
    assert_eq!(summary["failed_spans"], 0);
    assert_eq!(summary["rework_count"], 1, "attempt>1 记返工");
    assert_eq!(summary["artifact_versions"], 3);
    assert_eq!(summary["model_calls"], 0);
    assert_eq!(summary["cost_usd"], 0.5);
    assert_eq!(summary["slowest_worker"]["role"], "planner");
    let roles = agg["roles"].as_array().unwrap();
    assert_eq!(roles.len(), 2);
    let builder = roles.iter().find(|r| r["role"] == "builder").unwrap();
    assert_eq!(builder["spans"], 2);
    assert_eq!(builder["rework"], 1);
    assert_eq!(builder["artifact_versions"], 2);
    assert_eq!(agg["workers"].as_array().unwrap().len(), 3);
    assert_eq!(agg["budget"]["exceeded"], false);
    assert_eq!(agg["team_id"], "team-t");
}

#[test]
fn aggregate_empty_is_well_formed() {
    let agg = aggregate_metrics("team-empty", &[], &json!({}));
    assert_eq!(agg["summary"]["span_count"], 0);
    assert!(agg["roles"].as_array().unwrap().is_empty());
    assert_eq!(agg["summary"]["slowest_worker"], Value::Null);
    assert_eq!(agg["summary"]["total_tokens"], Value::Null);
}

#[test]
fn budget_exhaustion_cost_and_wall() {
    let records = vec![span("planner", "s1", 12.5, 1, 1_000)];
    // 费用超限（严格大于）。
    let reason = budget_exhaustion_reason(&json!({"max_cost_usd": 10.0}), &records, 2_000).unwrap();
    assert!(reason.contains("费用预算耗尽"), "{reason}");
    // 未超限。
    assert!(budget_exhaustion_reason(&json!({"max_cost_usd": 12.5}), &records, 2_000).is_none());
    // 墙钟超限（窗口 1999ms > 1s）。
    let reason = budget_exhaustion_reason(&json!({"max_wall_secs": 1}), &records, 3_000).unwrap();
    assert!(reason.contains("墙钟预算耗尽"), "{reason}");
    // 未配置 → 不启用。
    assert!(budget_exhaustion_reason(&json!({}), &records, 9_000).is_none());
    // 空记录 → 永不耗尽。
    assert!(budget_exhaustion_reason(&json!({"max_cost_usd": 0.0}), &[], 9_000).is_none());
}

#[test]
fn sensitive_keys_exclude_usage_token_counts() {
    assert!(is_sensitive_key("api_key"));
    assert!(is_sensitive_key("OPENAI_API_KEY"));
    assert!(is_sensitive_key("access_token"));
    assert!(is_sensitive_key("authorization"));
    assert!(is_sensitive_key("password"));
    assert!(is_sensitive_key("client_secret"));
    // 用量计数复数不是凭据。
    assert!(!is_sensitive_key("prompt_tokens"));
    assert!(!is_sensitive_key("completion_tokens"));
    assert!(!is_sensitive_key("total_tokens"));
    assert!(!is_sensitive_key("artifact_versions"));
}

#[test]
fn sanitize_text_redacts_credentials_but_keeps_usage_counts() {
    let text = "失败原因：api_key=sk-abcdef123456 password: hunter2 Bearer abc.def.ghi";
    let out = sanitize_text(text);
    assert!(!out.contains("sk-abcdef"), "{out}");
    assert!(!out.contains("hunter2"), "{out}");
    assert!(!out.contains("abc.def.ghi"), "{out}");
    assert!(out.contains("[REDACTED]"), "{out}");
    // 用量计数保留。
    let usage = sanitize_text("total_tokens: 12345 completion_tokens=678");
    assert!(usage.contains("12345"), "{usage}");
    assert!(usage.contains("678"), "{usage}");
    // CAS ref 保留。
    let cas = sanitize_text("content_ref cas://sha256:0123456789abcdef0123456789abcdef");
    assert!(cas.contains("cas://sha256:"), "{cas}");
}

#[test]
fn sanitize_value_redacts_nested_sensitive_keys_and_truncates() {
    let value = json!({
        "budget": { "api_key": "sk-secret-0001", "max_cost_usd": 1.0 },
        "note": "x".repeat(1000),
        "items": [ { "password": "hunter2", "role": "builder" } ]
    });
    let out = sanitize_value(&value);
    assert_eq!(out["budget"]["api_key"], "[REDACTED]");
    assert_eq!(out["budget"]["max_cost_usd"], 1.0);
    assert_eq!(out["items"][0]["password"], "[REDACTED]");
    assert_eq!(out["items"][0]["role"], "builder");
    let note = out["note"].as_str().unwrap();
    assert!(note.len() < 500, "长文本应截断：{}", note.len());
    assert!(note.contains("[截断"), "{note}");
}

#[test]
fn cost_budget_fails_closed_when_concurrent_usage_is_unknown() {
    let mut record = span("builder", "step-1", 0.0, 1, 1000);
    record.model_calls = 1;
    record.usage_attribution = "unknown_concurrent_overlap".to_string();
    let reason = budget_exhaustion_reason(&json!({"max_cost_usd": 10.0}), &[record], 2_000)
        .expect("未知并发用量时不能继续使用成本预算门");
    assert!(reason.contains("费用预算无法核验"), "{reason}");
}

#[test]
fn concurrent_spans_are_marked_for_unknown_usage_attribution() {
    let tracker = UsageAttributionTracker::default();
    let first = tracker.begin();
    assert!(!first.load(Ordering::Relaxed));
    let second = tracker.begin();
    assert!(first.load(Ordering::Relaxed));
    assert!(second.load(Ordering::Relaxed));
    tracker.finish(&second);
    let third = tracker.begin();
    assert!(third.load(Ordering::Relaxed));
    tracker.finish(&third);
    tracker.finish(&first);
}

#[test]
fn request_scoped_usage_is_accepted_by_cost_budget_gate() {
    let mut record = span("builder", "step-1", 0.25, 1, 1000);
    record.model_calls = 1;
    record.prompt_tokens = Some(100);
    record.completion_tokens = Some(50);
    record.total_tokens = Some(150);
    record.usage_attribution = "request_id_scoped".to_string();
    record
        .requests
        .push(owo_agent_core::gateway::ModelCallMetadata {
            request_id: Some("req-safe-id".to_string()),
            model: Some("model-test".to_string()),
            usage: Some(owo_agent_core::gateway::TokenUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            }),
            latency_ms: Some(25),
        });
    assert_eq!(
        budget_exhaustion_reason(&json!({"max_cost_usd": 10.0}), &[record], 2_000),
        None,
        "逐请求 usage 可用于校验成本预算"
    );
}

#[tokio::test]
async fn measured_provider_preserves_request_metadata_once() {
    struct ObservedStub;
    #[async_trait]
    impl ModelProvider for ObservedStub {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Ok(ModelOutput::Text("ok".to_string()))
        }

        async fn complete_stream_with_reasoning_and_model_observed(
            &self,
            model: Option<&str>,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> Result<ObservedModelOutput, String> {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok(ObservedModelOutput {
                output: ModelOutput::Text("ok".to_string()),
                metadata: ModelCallMetadata {
                    request_id: Some("req-42".to_string()),
                    model: model.map(str::to_string),
                    usage: Some(TokenUsage {
                        prompt_tokens: 12,
                        completion_tokens: 8,
                        total_tokens: 20,
                    }),
                    latency_ms: None,
                },
            })
        }
    }

    let calls = Arc::new(AtomicU64::new(0));
    let collector = Arc::new(RequestUsageCollector::default());
    let measured = MeasuredProvider::new_with_request_usage(
        Arc::new(ObservedStub),
        Arc::clone(&calls),
        Arc::clone(&collector),
        "step-a".to_string(),
    );
    let mut chunks = Vec::new();
    let observed = measured
        .complete_stream_with_reasoning_and_model_observed(
            Some("model-actual"),
            &[],
            &[],
            &mut |chunk| chunks.push(chunk),
        )
        .await
        .unwrap();
    let records = collector.take("step-a");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(observed.output, ModelOutput::Text("ok".to_string()));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request_id.as_deref(), Some("req-42"));
    assert_eq!(records[0].model.as_deref(), Some("model-actual"));
    assert_eq!(records[0].usage.unwrap().total_tokens, 20);
    assert!(records[0].latency_ms.unwrap_or(0) >= 5);
}

#[tokio::test]
async fn concurrent_measured_providers_keep_request_usage_scoped_to_each_step() {
    struct ScopedProvider {
        request_id: &'static str,
        usage: TokenUsage,
    }

    #[async_trait]
    impl ModelProvider for ScopedProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Ok(ModelOutput::Text("ok".to_string()))
        }

        async fn complete_stream_with_reasoning_and_model_observed(
            &self,
            model: Option<&str>,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> Result<ObservedModelOutput, String> {
            tokio::task::yield_now().await;
            Ok(ObservedModelOutput {
                output: ModelOutput::Text("ok".to_string()),
                metadata: ModelCallMetadata {
                    request_id: Some(self.request_id.to_string()),
                    model: model.map(str::to_string),
                    usage: Some(self.usage),
                    latency_ms: None,
                },
            })
        }
    }

    let collector = Arc::new(RequestUsageCollector::default());
    let calls_a = Arc::new(AtomicU64::new(0));
    let calls_b = Arc::new(AtomicU64::new(0));
    let provider_a = MeasuredProvider::new_with_request_usage(
        Arc::new(ScopedProvider {
            request_id: "req-a",
            usage: TokenUsage {
                prompt_tokens: 11,
                completion_tokens: 3,
                total_tokens: 14,
            },
        }),
        Arc::clone(&calls_a),
        Arc::clone(&collector),
        request_scope_key("step-a", Some(4)),
    );
    let provider_b = MeasuredProvider::new_with_request_usage(
        Arc::new(ScopedProvider {
            request_id: "req-b",
            usage: TokenUsage {
                prompt_tokens: 29,
                completion_tokens: 7,
                total_tokens: 36,
            },
        }),
        Arc::clone(&calls_b),
        Arc::clone(&collector),
        request_scope_key("step-b", Some(4)),
    );
    let mut on_chunk_a = |_chunk: StreamChunk| {};
    let mut on_chunk_b = |_chunk: StreamChunk| {};
    let (result_a, result_b) = tokio::join!(
        provider_a.complete_stream_with_reasoning_and_model_observed(
            Some("model-a"),
            &[],
            &[],
            &mut on_chunk_a,
        ),
        provider_b.complete_stream_with_reasoning_and_model_observed(
            Some("model-b"),
            &[],
            &[],
            &mut on_chunk_b,
        ),
    );
    result_a.unwrap();
    result_b.unwrap();

    let records_a = collector.take(&request_scope_key("step-a", Some(4)));
    let records_b = collector.take(&request_scope_key("step-b", Some(4)));
    assert_eq!(calls_a.load(Ordering::Relaxed), 1);
    assert_eq!(calls_b.load(Ordering::Relaxed), 1);
    assert_eq!(records_a.len(), 1);
    assert_eq!(records_b.len(), 1);
    assert_eq!(records_a[0].request_id.as_deref(), Some("req-a"));
    assert_eq!(records_b[0].request_id.as_deref(), Some("req-b"));
    assert_eq!(records_a[0].usage.unwrap().total_tokens, 14);
    assert_eq!(records_b[0].usage.unwrap().total_tokens, 36);
}

#[test]
fn lease_wait_measurements_are_isolated_by_step() {
    let waits = LeaseWaitTracker::default();
    waits.record(&request_scope_key("step-a", Some(3)), 23);
    waits.record(&request_scope_key("step-a", Some(4)), 41);
    assert_eq!(waits.take(&request_scope_key("step-a", Some(3))), 23);
    assert_eq!(waits.take(&request_scope_key("step-a", Some(4))), 41);
    assert_eq!(waits.take(&request_scope_key("step-a", Some(3))), 0);
}
