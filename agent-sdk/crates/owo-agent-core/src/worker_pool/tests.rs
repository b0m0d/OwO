use super::protocol::*;
use std::time::Duration;

#[test]
fn parse_child_line_accepts_structured_messages() {
    assert!(matches!(
        parse_child_line("{\"type\":\"ready\"}").unwrap(),
        ChildMsg::Ready
    ));
    assert!(matches!(
        parse_child_line("{\"type\":\"pong\"}").unwrap(),
        ChildMsg::Pong
    ));
    match parse_child_line(
        "{\"type\":\"result\",\"task_id\":\"t1\",\"ok\":true,\"output\":\"out-A\"}",
    )
    .unwrap()
    {
        ChildMsg::Result {
            task_id,
            ok,
            output,
            ..
        } => {
            assert_eq!(task_id, "t1");
            assert!(ok);
            assert_eq!(output.as_deref(), Some("out-A"));
        }
        _ => panic!("应解析为 result"),
    }
}

#[test]
fn parse_child_line_rejects_free_text() {
    assert!(parse_child_line("hello world").is_err());
    assert!(parse_child_line("just some log").is_err());
    assert!(parse_child_line("").is_err());
}

#[test]
fn parent_lines_are_structured_json() {
    let input = serde_json::json!({ "text": "A" });
    let line = task_line("t1", "corr-1", &input);
    let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parsed["cmd"], "task");
    assert_eq!(parsed["task_id"], "t1");
    assert_eq!(parsed["correlation_id"], "corr-1");
    assert_eq!(parsed["input"], input);
    let ping: serde_json::Value = serde_json::from_str(&ping_line()).unwrap();
    assert_eq!(ping["cmd"], "ping");
    let cancel: serde_json::Value = serde_json::from_str(&cancel_line("t9")).unwrap();
    assert_eq!(cancel["cmd"], "cancel");
    assert_eq!(cancel["task_id"], "t9");
    let shutdown: serde_json::Value = serde_json::from_str(&shutdown_line()).unwrap();
    assert_eq!(shutdown["cmd"], "shutdown");
}

#[test]
fn worker_spec_serde_roundtrip() {
    let spec = WorkerSpec::new("w1", "worker.exe")
        .args(vec!["-x".to_string()])
        .env_whitelist(vec![("K".to_string(), "V".to_string())])
        .budget(WorkerBudget {
            max_turns: 5,
            max_duration_secs: 60,
            max_memory_mb: 256,
            max_cpu_cores: 2.0,
        });
    let json = serde_json::to_string(&spec).unwrap();
    let restored: WorkerSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.id, "w1");
    assert_eq!(restored.args, vec!["-x".to_string()]);
    assert_eq!(restored.budget.max_turns, 5);
    assert_eq!(restored.budget.max_cpu_cores, 2.0);
    assert_eq!(restored.isolation, IsolationMode::Process);
}

#[test]
fn worker_budget_exceeded_semantics() {
    let budget = WorkerBudget {
        max_turns: 3,
        max_duration_secs: 10,
        ..Default::default()
    };
    assert!(!budget.exceeded(2, Duration::from_secs(5)));
    assert!(budget.exceeded(3, Duration::from_secs(5)));
    assert!(budget.exceeded(2, Duration::from_secs(10)));
    let unlimited = WorkerBudget::default();
    assert!(!unlimited.exceeded(999, Duration::from_secs(99999)));
}

#[test]
fn isolation_default_is_process() {
    assert_eq!(IsolationMode::default(), IsolationMode::Process);
    let spec = WorkerSpec::new("w", "x.exe");
    assert_eq!(spec.isolation, IsolationMode::Process);
}

#[test]
fn worker_status_display() {
    assert_eq!(WorkerStatus::Running.to_string(), "running");
    assert_eq!(
        WorkerStatus::Restarting {
            attempts: 2,
            next_retry_secs: 4
        }
        .to_string(),
        "restarting(attempt=2, backoff=4s)"
    );
    assert_eq!(
        WorkerStatus::Fused { attempts: 4 }.to_string(),
        "fused(attempt=4)"
    );
    assert_eq!(WorkerStatus::Stopped.to_string(), "stopped");
}
