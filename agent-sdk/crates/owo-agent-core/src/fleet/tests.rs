use super::bus::*;
use super::fanout::*;
use super::supervision::*;
use super::wait::*;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

fn msg(id: u64, kind: MessageKind) -> BusMessage {
    BusMessage {
        id,
        from: "a".to_string(),
        to: "b".to_string(),
        kind,
        correlation_id: "c1".to_string(),
        payload: serde_json::Value::Null,
    }
}

#[test]
fn mailbox_overflow_drops_mergeable_keeps_critical() {
    let mut mb = Mailbox::new(2);
    assert_eq!(
        mb.push(msg(1, MessageKind::Task), OverflowPolicy::DropMergeable),
        Ok(PushOutcome::Pushed)
    );
    assert_eq!(
        mb.push(msg(2, MessageKind::Progress), OverflowPolicy::DropMergeable),
        Ok(PushOutcome::Pushed)
    );
    assert_eq!(
        mb.push(msg(3, MessageKind::Progress), OverflowPolicy::DropMergeable),
        Ok(PushOutcome::Dropped)
    );
    assert_eq!(
        mb.push(msg(4, MessageKind::Review), OverflowPolicy::DropMergeable),
        Err(BusError::MailboxFull(2))
    );
    assert_eq!(mb.len(), 2);
}

#[test]
fn mailbox_reject_policy_returns_full_error() {
    let mut mb = Mailbox::new(1);
    assert_eq!(
        mb.push(msg(1, MessageKind::Progress), OverflowPolicy::Reject),
        Ok(PushOutcome::Pushed)
    );
    assert_eq!(
        mb.push(msg(2, MessageKind::Progress), OverflowPolicy::Reject),
        Err(BusError::MailboxFull(1))
    );
}

#[tokio::test]
async fn bus_send_drain_and_unregister() {
    let bus = AgentBus::new();
    bus.register("worker-a", 4).await;
    bus.register("worker-b", 4).await;
    let id = bus
        .send(
            "core",
            "worker-a",
            MessageKind::Task,
            "corr-1",
            serde_json::json!({"q":1}),
            OverflowPolicy::Reject,
        )
        .await
        .expect("delivered");
    assert_eq!(bus.pending("worker-a").await, 1);
    let drained = bus.drain("worker-a").await;
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].id, id);
    assert_eq!(drained[0].correlation_id, "corr-1");
    assert!(bus.unregister("worker-a").await);
    assert!(!bus.contains("worker-a").await);
}

#[tokio::test]
async fn bus_send_unknown_agent_errors() {
    let bus = AgentBus::new();
    bus.register("worker-a", 4).await;
    let err = bus
        .send(
            "core",
            "nobody",
            MessageKind::Task,
            "c",
            serde_json::Value::Null,
            OverflowPolicy::Reject,
        )
        .await
        .unwrap_err();
    assert_eq!(err, BusError::UnknownAgent("nobody".to_string()));
}

#[tokio::test]
async fn broadcast_delivers_to_registered_agents() {
    let bus = AgentBus::new();
    bus.register("w1", 4).await;
    bus.register("w2", 4).await;
    let delivered = bus
        .broadcast(
            "core",
            "topic",
            MessageKind::Progress,
            "corr-x",
            serde_json::json!({"n":1}),
            OverflowPolicy::Reject,
        )
        .await;
    assert_eq!(delivered.len(), 2);
    assert!(delivered.contains(&"w1".to_string()));
    assert!(delivered.contains(&"w2".to_string()));
    assert_eq!(bus.pending("w1").await, 1);
    assert_eq!(bus.pending("w2").await, 1);
}

#[test]
fn budget_exceeded_flags() {
    let b = Budget {
        max_turns: 2,
        max_steps: 5,
        max_duration_secs: 10,
    };
    assert!(!b.exceeded(1, 1, Duration::from_secs(1)));
    assert!(b.exceeded(2, 1, Duration::from_secs(1)));
    assert!(b.exceeded(1, 5, Duration::from_secs(1)));
    assert!(b.exceeded(1, 1, Duration::from_secs(10)));
}

#[test]
fn backoff_secs_grows_and_caps() {
    assert_eq!(backoff_secs(1, 0), 1);
    assert_eq!(backoff_secs(1, 2), 4);
    assert_eq!(backoff_secs(2, 3), 16);
    assert_eq!(backoff_secs(1, 100), 60);
}

#[test]
fn supervisor_fuses_after_max_restarts() {
    let rule = RestartRule {
        max_restarts: 2,
        base_backoff_secs: 1,
        policy: RestartPolicy::OneForOne,
    };
    let mut sup = Supervisor::new(rule);
    assert_eq!(
        sup.on_crash(),
        SupervisionState::Restarting {
            attempts: 1,
            next_retry_secs: 1
        }
    );
    assert_eq!(
        sup.on_crash(),
        SupervisionState::Restarting {
            attempts: 2,
            next_retry_secs: 2
        }
    );
    assert_eq!(sup.on_crash(), SupervisionState::Fused { attempts: 3 });
    assert_eq!(sup.attempts(), 3);
}

#[test]
fn supervisor_resets_on_healthy() {
    let mut sup = Supervisor::new(RestartRule::default());
    let _ = sup.on_crash();
    sup.mark_healthy();
    assert_eq!(sup.attempts(), 0);
    assert_eq!(
        sup.on_crash(),
        SupervisionState::Restarting {
            attempts: 1,
            next_retry_secs: 1
        }
    );
}

#[test]
fn detect_cycle_finds_handoff_loop() {
    let edges: Vec<(AgentId, AgentId)> = vec![
        ("a".into(), "b".into()),
        ("b".into(), "c".into()),
        ("c".into(), "a".into()),
    ];
    let cycle = detect_cycle(&edges).expect("cycle found");
    assert!(cycle.len() >= 3);
    assert_eq!(cycle.first(), cycle.last());
}

#[test]
fn detect_cycle_clean_dag_none() {
    let edges: Vec<(AgentId, AgentId)> = vec![
        ("a".into(), "b".into()),
        ("a".into(), "c".into()),
        ("b".into(), "d".into()),
    ];
    assert_eq!(detect_cycle(&edges), None);
}

#[tokio::test]
async fn fan_out_respects_max_parallel() {
    let workers: Vec<AgentId> = vec!["w1".into(), "w2".into(), "w3".into(), "w4".into()];
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let active_clone = Arc::clone(&active);
    let peak_clone = Arc::clone(&peak);
    let outcomes = fan_out(&workers, 2, Budget::default(), move |_id| {
        let active = Arc::clone(&active_clone);
        let peak = Arc::clone(&peak_clone);
        async move {
            let now = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            Ok("done".to_string())
        }
    })
    .await;
    assert!(peak.load(std::sync::atomic::Ordering::SeqCst) <= 2);
    assert!(outcomes.iter().all(|o| o.ok));
    assert_eq!(outcomes.len(), 4);
}

#[tokio::test]
async fn fan_out_partial_failure_and_order() {
    let workers: Vec<AgentId> = vec!["w1".into(), "w2".into(), "w3".into()];
    let outcomes = fan_out(&workers, 2, Budget::default(), |id| async move {
        if id == "w2" {
            Err("boom".to_string())
        } else {
            Ok(format!("ok:{id}"))
        }
    })
    .await;
    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].worker, "w1");
    assert!(outcomes[0].ok);
    assert_eq!(outcomes[0].output.as_deref(), Some("ok:w1"));
    assert_eq!(outcomes[1].worker, "w2");
    assert!(!outcomes[1].ok);
    assert_eq!(outcomes[1].error.as_deref(), Some("boom"));
    assert_eq!(outcomes[2].worker, "w3");
    assert!(outcomes[2].ok);
}

#[tokio::test]
async fn fan_out_zero_budget_aborts_all() {
    let workers: Vec<AgentId> = vec!["w1".into(), "w2".into()];
    let budget = Budget {
        max_turns: 50,
        max_steps: 1000,
        max_duration_secs: 0,
    };
    let outcomes = fan_out(
        &workers,
        2,
        budget,
        |id| async move { Ok(format!("ok:{id}")) },
    )
    .await;
    assert!(outcomes.iter().all(|o| !o.ok));
    assert!(outcomes
        .iter()
        .all(|o| o.error.as_deref() == Some("budget exceeded: task aborted")));
}
