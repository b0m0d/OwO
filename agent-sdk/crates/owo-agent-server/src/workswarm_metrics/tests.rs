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
        task_id: None,
        attempt_id: None,
        model_call_budget: None,
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
        cost_known: cost > 0.0,
        attempt,
        artifact: Some(SpanArtifact {
            artifact_id: format!("team-t:{role}:v{attempt}"),
            kind: "document".to_string(),
            version: attempt,
        }),
    }
}

#[test]
fn host_task_budget_metadata_is_extracted_from_worker_input() {
    let input = json!({
        "assigned_task_id": "task-api",
        "assigned_model_calls_per_attempt": 6,
        "_workswarm": {"attempt_id": "attempt-7"}
    });
    let metadata = task_budget_metadata(&input);
    assert_eq!(metadata.0.as_deref(), Some("task-api"));
    assert_eq!(metadata.1.as_deref(), Some("attempt-7"));
    assert_eq!(metadata.2, Some(6));
}

#[test]
fn task_budget_metrics_roundtrip_and_legacy_records_remain_readable() {
    let mut record = span("builder", "s-task", 0.0, 1, 1);
    record.task_id = Some("task-api".to_string());
    record.attempt_id = Some("attempt-7".to_string());
    record.model_call_budget = Some(6);
    let encoded = serde_json::to_value(&record).unwrap();
    let decoded: WorkerSpanRecord = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.task_id.as_deref(), Some("task-api"));
    assert_eq!(decoded.attempt_id.as_deref(), Some("attempt-7"));
    assert_eq!(decoded.model_call_budget, Some(6));

    let mut legacy = encoded;
    let object = legacy.as_object_mut().unwrap();
    object.remove("task_id");
    object.remove("attempt_id");
    object.remove("model_call_budget");
    object.remove("cost_known");
    let decoded_legacy: WorkerSpanRecord = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded_legacy.task_id, None);
    assert_eq!(decoded_legacy.attempt_id, None);
    assert_eq!(decoded_legacy.model_call_budget, None);
    assert!(!decoded_legacy.cost_known);
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
fn task_model_call_budget_overrun_stops_next_stage() {
    let mut record = span("builder", "s-task-a", 0.0, 1, 100);
    record.task_id = Some("task-a".to_string());
    record.model_call_budget = Some(4);
    record.model_calls = 5;

    let reason = budget_exhaustion_reason(&json!({}), &[record], 200).unwrap();
    assert!(reason.contains("task_id=task-a"));
    assert!(reason.contains("observed=5 limit=4"));
}

#[test]
fn repaired_latest_attempt_clears_prior_attempt_budget_stop() {
    let mut prior = span("builder", "s-task-a", 0.0, 1, 100);
    prior.task_id = Some("task-a".to_string());
    prior.attempt_id = Some("attempt-1".to_string());
    prior.model_call_budget = Some(4);
    prior.model_calls = 5;
    prior.ended_at_ms = 200;

    let mut repaired = span("builder", "s-task-a", 0.0, 2, 300);
    repaired.task_id = Some("task-a".to_string());
    repaired.attempt_id = Some("attempt-2".to_string());
    repaired.model_call_budget = Some(4);
    repaired.model_calls = 4;
    repaired.ended_at_ms = 400;

    assert!(budget_exhaustion_reason(&json!({}), &[prior, repaired], 500).is_none());
}

#[test]
fn aggregate_reports_task_model_call_budget_overruns() {
    let mut within_budget = span("builder", "s-task-a", 0.0, 1, 100);
    within_budget.task_id = Some("task-a".to_string());
    within_budget.model_call_budget = Some(4);
    within_budget.model_calls = 4;

    let mut over_budget = span("builder", "s-task-b", 0.0, 1, 200);
    over_budget.task_id = Some("task-b".to_string());
    over_budget.model_call_budget = Some(5);
    over_budget.model_calls = 6;

    let aggregate = aggregate_metrics("team-t", &[within_budget, over_budget], &json!({}));
    assert_eq!(aggregate["summary"]["task_budgeted_spans"], 2);
    assert_eq!(aggregate["summary"]["task_model_call_budget_overruns"], 1);
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
    let mut cost_span = span("planner", "s1", 12.5, 1, 1_000);
    cost_span.model_calls = 1;
    cost_span.usage_attribution = "request_id_scoped".to_string();
    let records = vec![cost_span];
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
fn cost_budget_fails_closed_when_prices_are_unconfigured() {
    let mut record = span("builder", "step-1", 0.0, 1, 1000);
    record.model_calls = 1;
    record.usage_attribution = "request_id_scoped".to_string();
    record.cost_known = false;
    let reason = budget_exhaustion_reason(&json!({"max_cost_usd": 10.0}), &[record], 2_000)
        .expect("unknown price must not be treated as zero cost");
    assert!(reason.contains("费用预算无法核验"), "{reason}");
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
    record.cost_known = true;
    record
        .requests
        .push(owo_agent_protocol::ModelRequestMetricV1 {
            request_id: Some("req-safe-id".to_string()),
            model: Some("model-test".to_string()),
            usage: Some(owo_agent_protocol::ModelTokenUsageV1 {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
            }),
            latency_ms: Some(25),
            succeeded: true,
        });
    assert_eq!(
        budget_exhaustion_reason(&json!({"max_cost_usd": 10.0}), &[record], 2_000),
        None,
        "逐请求 usage 可用于校验成本预算"
    );
}

#[test]
fn team_request_budget_is_concurrent_and_survives_recreation() {
    let dir = std::env::temp_dir().join(format!(
        "owo-request-budget-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let journal = RequestReservationJournal::for_team(&dir, "team-budget");
    let budget = std::sync::Arc::new(TeamModelRequestBudget::new(4, journal.clone()).unwrap());
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handles = (0..12)
        .map(|_| {
            let budget = std::sync::Arc::clone(&budget);
            let completed = std::sync::Arc::clone(&completed);
            std::thread::spawn(move || {
                if budget.reserve("step#1").is_ok() {
                    completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(journal.reservation_count().unwrap(), 4);

    let resumed = TeamModelRequestBudget::new(8, journal.clone()).unwrap();
    assert_eq!(resumed.used(), 4);
    assert!(resumed.reserve("step-next#2").is_ok());
    assert_eq!(journal.reservation_count().unwrap(), 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn request_budget_status_is_reported_without_claiming_usd_exhaustion() {
    let mut payload = aggregate_metrics("team-budget", &[], &json!({"max_model_calls": 7}));
    attach_request_budget_status(
        &mut payload,
        &json!({"max_model_calls": 7}),
        Ok(5),
    );
    assert_eq!(payload["budget"]["max_model_calls"], 7);
    assert_eq!(payload["budget"]["reserved_model_calls"], 5);
    assert_eq!(payload["budget"]["remaining_model_calls"], 2);
    assert_eq!(payload["budget"]["request_budget_known"], true);
    assert_eq!(payload["budget"]["request_limit_reached"], false);
    assert_eq!(payload["budget"]["exceeded"], false);
}

#[tokio::test]
async fn measured_provider_does_not_call_provider_after_team_budget_is_full() {
    struct CountingProvider(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl ModelProvider for CountingProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ModelOutput::Text("ok".to_string()))
        }
    }
    let dir = std::env::temp_dir().join(format!(
        "owo-request-budget-provider-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let journal = RequestReservationJournal::for_team(&dir, "team-budget");
    let budget = std::sync::Arc::new(TeamModelRequestBudget::new(1, journal).unwrap());
    let inner = std::sync::Arc::new(CountingProvider(std::sync::atomic::AtomicUsize::new(0)));
    let calls = std::sync::Arc::new(AtomicU64::new(0));
    let provider = MeasuredProvider::new_with_request_budget(
        inner.clone(),
        calls.clone(),
        std::sync::Arc::new(RequestUsageCollector::default()),
        "step#1".to_string(),
        Some(budget),
    );

    assert!(provider.complete(&[], &[]).await.is_ok());
    let error = provider.complete(&[], &[]).await.unwrap_err();
    assert!(error.contains("team_model_call_budget_exhausted"));
    assert_eq!(inner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let _ = std::fs::remove_dir_all(&dir);
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
    assert_eq!(records[0].0.request_id.as_deref(), Some("req-42"));
    assert_eq!(records[0].0.model.as_deref(), Some("model-actual"));
    assert_eq!(records[0].0.usage.unwrap().total_tokens, 20);
    assert!(records[0].0.latency_ms.unwrap_or(0) >= 5);
    assert!(records[0].1, "成功的 observed provider call 保留逐请求 outcome");
}


#[tokio::test]
async fn failed_provider_call_is_retained_as_failed_request_metric() {
    struct FailedProvider;
    #[async_trait]
    impl ModelProvider for FailedProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Err("provider unavailable".to_string())
        }
    }

    let calls = Arc::new(AtomicU64::new(0));
    let collector = Arc::new(RequestUsageCollector::default());
    let measured = MeasuredProvider::new_with_request_usage(
        Arc::new(FailedProvider),
        Arc::clone(&calls),
        Arc::clone(&collector),
        "step-failed".to_string(),
    );
    assert!(measured.complete(&[], &[]).await.is_err());
    let records = collector.take("step-failed");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(records.len(), 1);
    assert!(!records[0].1);
    assert!(records[0].0.usage.is_none());
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
    assert_eq!(records_a[0].0.request_id.as_deref(), Some("req-a"));
    assert_eq!(records_b[0].0.request_id.as_deref(), Some("req-b"));
    assert_eq!(records_a[0].0.usage.unwrap().total_tokens, 14);
    assert_eq!(records_b[0].0.usage.unwrap().total_tokens, 36);
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
