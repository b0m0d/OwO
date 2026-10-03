use super::*;

#[test]
fn goal_status_machine_transitions() {
    let mut goal = Goal::new("g1", "测试目标");
    assert_eq!(goal.status, GoalStatus::Pending);
    goal.transition(GoalStatus::Planning);
    goal.transition(GoalStatus::Running);
    goal.transition(GoalStatus::Verifying);
    goal.transition(GoalStatus::Succeeded);
    assert!(goal.status.is_terminal());
    assert!(!GoalStatus::Running.is_terminal());
}

#[test]
fn run_state_serde_roundtrip() {
    let plan = Plan::new("p1", "g1");
    let state = GoalRunState::new(Goal::new("g1", "目标"), plan);
    let json = serde_json::to_string(&state).unwrap();
    let restored: GoalRunState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.run_id, state.run_id);
    assert_eq!(restored.records.len(), 0);

    let mut legacy =
        serde_json::from_value::<serde_json::Value>(serde_json::to_value(&state).unwrap()).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("validation_receipts");
    legacy["goal"]
        .as_object_mut()
        .unwrap()
        .remove("verification_plan");
    let restored_legacy: GoalRunState = serde_json::from_value(legacy).unwrap();
    assert!(restored_legacy.validation_receipts.is_empty());
    assert!(restored_legacy.goal.verification_plan.is_none());
}

#[test]
fn goal_verification_persists_receipt_bound_to_accepted_output() {
    let mut goal = Goal::new("g-receipt", "验证目标收据");
    goal.verification_plan = Some(crate::plan::VerificationPlanV1 {
        plan_id: "goal-plan-v1".to_string(),
        requirements: vec![crate::verification::requirement_for_spec(
            "goal-ready",
            &crate::plan::VerificationSpec::OutputContains("ready".to_string()),
        )],
    });
    let mut runner = GoalRunner::new(
        goal,
        Plan::new("p-receipt", "g-receipt"),
        RunnerConfig::default(),
    );
    runner.state.records.insert(
        "step-1".to_string(),
        StepRecord {
            step_id: "step-1".to_string(),
            status: StepStatus::Succeeded,
            attempts: 1,
            attempt_id: Some("attempt-1".to_string()),
            output: Some("service ready".to_string()),
            error: None,
            skip_reason: None,
            phase_epoch: Some(3),
            validation_receipts: Vec::new(),
        },
    );

    assert_eq!(runner.verify_goal().unwrap(), GoalStatus::Succeeded);
    let receipt = runner.state.validation_receipts.first().unwrap();
    assert_eq!(receipt.task_id, "g-receipt");
    assert_eq!(receipt.requirement_id, "goal-ready");
    assert_eq!(receipt.validator_id, "artifact-output-contains-v1");
    assert_eq!(receipt.verdict, crate::plan::ValidationVerdictV1::Passed);
    assert_eq!(
        receipt.input_sha256,
        crate::cas_store::CasStore::hash_of(b"service ready")
    );
    assert_eq!(
        receipt.evidence_refs,
        vec!["goal-step:step-1:attempt:attempt-1"]
    );

    let encoded = serde_json::to_vec(&runner.state).unwrap();
    let restored: GoalRunState = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        restored.validation_receipts,
        runner.state.validation_receipts
    );
}

#[test]
fn unsupported_goal_verifier_is_recorded_and_cannot_succeed() {
    let mut goal = Goal::new("g-unsupported", "未知验证器不得通过");
    let mut requirement = crate::verification::requirement_for_spec(
        "goal-custom",
        &crate::plan::VerificationSpec::OutputNonEmpty,
    );
    requirement.validator_id = "custom-behavior-check".to_string();
    goal.verification_plan = Some(crate::plan::VerificationPlanV1 {
        plan_id: "goal-plan-unsupported".to_string(),
        requirements: vec![requirement],
    });
    let mut runner = GoalRunner::new(
        goal,
        Plan::new("p-unsupported", "g-unsupported"),
        RunnerConfig::default(),
    );
    runner.state.records.insert(
        "step-1".to_string(),
        StepRecord {
            step_id: "step-1".to_string(),
            status: StepStatus::Succeeded,
            attempts: 1,
            attempt_id: Some("attempt-1".to_string()),
            output: Some("looks good".to_string()),
            error: None,
            skip_reason: None,
            phase_epoch: Some(1),
            validation_receipts: Vec::new(),
        },
    );

    assert_eq!(runner.verify_goal().unwrap(), GoalStatus::Failed);
    assert_eq!(runner.state.validation_receipts.len(), 1);
    assert_eq!(
        runner.state.validation_receipts[0].verdict,
        crate::plan::ValidationVerdictV1::Unsupported
    );
}

struct FixedOutputWorker(&'static str);

#[async_trait::async_trait]
impl Worker for FixedOutputWorker {
    fn name(&self) -> &str {
        "fixed-output"
    }

    async fn run(&self, _input: &serde_json::Value) -> Result<String, String> {
        Ok(self.0.to_string())
    }
}

#[tokio::test]
async fn typed_step_plan_executes_and_persists_attempt_bound_receipt() {
    let goal = Goal::new("g-step-receipt", "typed step verification");
    let mut plan = Plan::new("p-step-receipt", "g-step-receipt");
    let mut step = crate::plan::StepSpec::new("step-1", "fixed-output");
    step.verification_plan = Some(crate::plan::VerificationPlanV1 {
        plan_id: "step-plan-v1".to_string(),
        requirements: vec![crate::verification::requirement_for_spec(
            "contains-ready",
            &crate::plan::VerificationSpec::OutputContains("ready".to_string()),
        )],
    });
    plan.add_step(step);
    let workers = WorkerRegistry::new();
    workers.register(std::sync::Arc::new(FixedOutputWorker("service ready")));
    let mut runner = GoalRunner::new(goal, plan, RunnerConfig::default());

    assert_eq!(runner.run(&workers).await.unwrap(), GoalStatus::Succeeded);
    let record = runner.state.records.get("step-1").unwrap();
    assert_eq!(record.status, StepStatus::Succeeded);
    assert_eq!(record.output.as_deref(), Some("service ready"));
    assert!(record.attempt_id.is_some());
    assert_eq!(
        record.validation_receipts[0].attempt_id,
        record.attempt_id.as_deref().unwrap()
    );
    assert_eq!(
        Some(record.validation_receipts[0].epoch),
        record.phase_epoch
    );
    assert_eq!(record.validation_receipts.len(), 1);
    assert_eq!(
        record.validation_receipts[0].verdict,
        crate::plan::ValidationVerdictV1::Passed
    );
    assert_eq!(
        record.validation_receipts[0]
            .subject_sha256
            .get("step-output"),
        Some(&crate::cas_store::CasStore::hash_of(b"service ready"))
    );
}

struct FeedbackAwareWorker;

#[async_trait::async_trait]
impl Worker for FeedbackAwareWorker {
    fn name(&self) -> &str {
        "feedback-worker"
    }

    async fn run(&self, input: &serde_json::Value) -> Result<String, String> {
        if input.get("_critic_feedback").is_some() {
            Ok("service ready".to_string())
        } else {
            Ok("service pending".to_string())
        }
    }
}

#[tokio::test]
async fn typed_step_verifier_checks_post_critic_output() {
    let goal = Goal::new("g-critic-receipt", "critic output must be verified");
    let mut plan = Plan::new("p-critic-receipt", "g-critic-receipt");
    let mut step = crate::plan::StepSpec::new("step-critic", "feedback-worker");
    step.input = serde_json::json!({"_critic": {"rounds": 2}});
    step.verification_plan = Some(crate::plan::VerificationPlanV1 {
        plan_id: "critic-output-plan".to_string(),
        requirements: vec![crate::verification::requirement_for_spec(
            "contains-ready",
            &crate::plan::VerificationSpec::OutputContains("ready".to_string()),
        )],
    });
    plan.add_step(step);
    let workers = WorkerRegistry::new();
    workers.register(std::sync::Arc::new(FeedbackAwareWorker));
    let mut runner = GoalRunner::new(goal, plan, RunnerConfig::default());
    runner.attach_critic(crate::critic::CriticConfig::new(
        std::sync::Arc::new(crate::critic::ScriptedCritic::new(vec![
            crate::critic::CriticVerdict::reject(20, vec!["needs repair".to_string()]),
            crate::critic::CriticVerdict::approve(95),
        ])),
        crate::critic::ReadOnlyGate::read_only(),
    ));

    assert_eq!(runner.run(&workers).await.unwrap(), GoalStatus::Succeeded);
    let record = runner.state.records.get("step-critic").unwrap();
    assert_eq!(record.output.as_deref(), Some("service ready"));
    assert_eq!(record.validation_receipts.len(), 1);
    assert_eq!(
        record.validation_receipts[0]
            .subject_sha256
            .get("step-output"),
        Some(&crate::cas_store::CasStore::hash_of(b"service ready"))
    );
}

#[tokio::test]
async fn unsupported_step_verifier_records_rejected_receipt() {
    let goal = Goal::new("g-step-unsupported", "unknown validator must fail closed");
    let mut plan = Plan::new("p-step-unsupported", "g-step-unsupported");
    let mut step = crate::plan::StepSpec::new("step-unsupported", "fixed-output");
    let mut requirement = crate::verification::requirement_for_spec(
        "behavior-check",
        &crate::plan::VerificationSpec::OutputNonEmpty,
    );
    requirement.validator_id = "behavior-check-not-registered".to_string();
    step.verification_plan = Some(crate::plan::VerificationPlanV1 {
        plan_id: "unsupported-step-plan".to_string(),
        requirements: vec![requirement],
    });
    plan.add_step(step);
    let workers = WorkerRegistry::new();
    workers.register(std::sync::Arc::new(FixedOutputWorker("looks good")));
    let mut runner = GoalRunner::new(goal, plan, RunnerConfig::default());

    assert_eq!(runner.run(&workers).await.unwrap(), GoalStatus::Failed);
    let record = runner.state.records.get("step-unsupported").unwrap();
    assert_eq!(record.status, StepStatus::Failed);
    assert!(!record.validation_receipts.is_empty());
    assert!(record
        .validation_receipts
        .iter()
        .all(|receipt| receipt.verdict == crate::plan::ValidationVerdictV1::Unsupported));
}

fn workspace_plan_step(id: &str, expected: &str) -> crate::plan::StepSpec {
    let mut step = crate::plan::StepSpec::new(id, "fixed-output");
    step.verification_plan = Some(crate::plan::VerificationPlanV1 {
        plan_id: format!("workspace-{id}"),
        requirements: vec![crate::plan::VerificationRequirementV1 {
            requirement_id: format!("{id}:file-ready"),
            covers_requirement_ids: Vec::new(),
            validator_id: "workspace-file-contains-v1".to_string(),
            validator_version: Some("1".to_string()),
            scope: crate::plan::VerificationScopeV1::WorkspacePaths {
                relative_paths: vec!["src/lib.rs".to_string()],
            },
            arguments: serde_json::json!({"text": expected}),
            required: true,
            resources: crate::plan::VerificationResourcesV1 {
                cpu_slots: 1,
                memory_mb: 8,
                exclusive_workspace: false,
                timeout_ms: 5_000,
            },
        }],
    });
    step
}

#[tokio::test]
async fn workspace_step_validation_passes_before_success_receipt() {
    let root = std::env::temp_dir().join(format!(
        "owo-goal-workspace-pass-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn ready() {}\n").unwrap();

    let goal = Goal::new("g-workspace-pass", "host workspace validation");
    let mut plan = Plan::new("p-workspace-pass", "g-workspace-pass");
    plan.add_step(workspace_plan_step("step-workspace", "ready"));
    let workers = WorkerRegistry::new();
    workers.register(std::sync::Arc::new(FixedOutputWorker("candidate")));
    let mut runner = GoalRunner::new(goal, plan, RunnerConfig::default());
    runner.attach_workspace_verification_root(root.clone());

    assert_eq!(runner.run(&workers).await.unwrap(), GoalStatus::Succeeded);
    let receipt = &runner.state.records["step-workspace"].validation_receipts[0];
    assert_eq!(receipt.verdict, crate::plan::ValidationVerdictV1::Passed);
    assert_eq!(
        receipt.subject_sha256.get("workspace-path:src/lib.rs"),
        Some(&crate::cas_store::CasStore::hash_of(b"pub fn ready() {}\n"))
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_workspace_validation_keeps_dependent_step_locked() {
    struct CountingWorker {
        name: &'static str,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Worker for CountingWorker {
        fn name(&self) -> &str {
            self.name
        }
        async fn run(&self, _input: &serde_json::Value) -> Result<String, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("candidate".to_string())
        }
    }

    let root = std::env::temp_dir().join(format!(
        "owo-goal-workspace-fail-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn ready() {}\n").unwrap();

    let first_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dependent_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let goal = Goal::new("g-workspace-fail", "failed validation blocks dependents");
    let mut plan = Plan::new("p-workspace-fail", "g-workspace-fail");
    let mut first = workspace_plan_step("step-first", "not-present");
    first.worker = "counting-first".to_string();
    let mut dependent = crate::plan::StepSpec::new("step-dependent", "counting-dependent");
    dependent.depends_on = vec!["step-first".to_string()];
    plan.add_step(first);
    plan.add_step(dependent);
    let workers = WorkerRegistry::new();
    workers.register(std::sync::Arc::new(CountingWorker {
        name: "counting-first",
        calls: first_calls.clone(),
    }));
    workers.register(std::sync::Arc::new(CountingWorker {
        name: "counting-dependent",
        calls: dependent_calls.clone(),
    }));
    let mut runner = GoalRunner::new(goal, plan, RunnerConfig::default());
    runner.attach_workspace_verification_root(root.clone());

    assert_eq!(runner.run(&workers).await.unwrap(), GoalStatus::Failed);
    assert!(first_calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    assert_eq!(dependent_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        runner.state.records["step-first"].status,
        StepStatus::Failed
    );
    assert_eq!(
        runner.state.records["step-dependent"].status,
        StepStatus::Pending
    );
    assert_eq!(
        runner.state.records["step-first"].validation_receipts[0].verdict,
        crate::plan::ValidationVerdictV1::Failed
    );
    std::fs::remove_dir_all(root).unwrap();
}
