use super::*;
use async_trait::async_trait;
use owo_agent_core::gateway::{ChatMessage, ModelOutput, ModelProvider};
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
        outcome: "succeeded".to_string(),
        error: None,
        model_calls: 0,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
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
