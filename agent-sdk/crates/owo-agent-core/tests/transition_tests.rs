//! T0 契约测试：TransitionTraceV1 + 幂等存储 + fork 对齐 + ExperienceStore 接线
//! （主文档 §5.12.3、§9.0 T0）。
//!
//! 覆盖完成标准：一次成功轨迹和一次注入弹窗导致的失败轨迹可以生成结构化差分、
//! 失败类别、fork point 和可重放样本；数据构建器按文档清洗顺序过滤并给出拒绝原因。

use owo_agent_core::desktop_env::{ActionKind, GroundedAction, RewardParts, RiskLevel, StateDelta};
use owo_agent_core::experience_store::{ExperienceKind, ExperienceStore, Outcome};
use owo_agent_core::transition::{
    align_fork_point, annotate_fork_points, record_transition_experience, FailureClass,
    ForkDivergence, PrivacyScope, TransitionOutcome, TransitionStore, TransitionTraceV1,
    VerifierResult,
};
use owo_agent_eval_facade::dataset_builder::{
    build_dataset, load_manifest, save_manifest, DatasetBuilderConfig, RejectReason,
};
use serde_json::json;

/// 随机临时目录（core 无 tempfile 依赖，沿用 std + uuid）。
fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("owo-t0-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

// ---------------------------------------------------------------------------
// 合成轨迹构造助手
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TraceSpec {
    transition_id: String,
    episode_id: String,
    task_id: String,
    env_version: String,
    state_before_ref: String,
    state_after_ref: String,
    action_id: String,
    op: String,
    success: bool,
    verifier_pass: bool,
    scope: PrivacyScope,
    click_point: Option<(i32, i32)>,
    target_bounds: Option<(i32, i32, i32, i32)>,
    target_id: Option<String>,
    target_evidence: Vec<String>,
}

impl Default for TraceSpec {
    fn default() -> Self {
        Self {
            transition_id: "t-1".into(),
            episode_id: "ep-1".into(),
            task_id: "task-1".into(),
            env_version: "S1-chat-1.0.0".into(),
            state_before_ref: "before".into(),
            state_after_ref: "after".into(),
            action_id: "a-1".into(),
            op: "click".into(),
            success: true,
            verifier_pass: true,
            scope: PrivacyScope::S1Sim,
            click_point: None,
            target_bounds: None,
            target_id: None,
            target_evidence: vec![],
        }
    }
}

fn make_trace(spec: &TraceSpec) -> TransitionTraceV1 {
    let action = GroundedAction {
        action_id: spec.action_id.clone(),
        kind: ActionKind::Gui,
        semantic_intent: format!("op {}", spec.op),
        target_id: spec.target_id.clone(),
        target_evidence: spec.target_evidence.clone(),
        arguments: json!({ "op": spec.op }),
        expected_effects: vec![],
        risk: RiskLevel::Low,
        reversible: true,
        idempotency_key: None,
        click_point: spec.click_point,
        target_bounds: spec.target_bounds,
    };
    let mut delta = StateDelta::default();
    if spec.success {
        delta.added_elements.push("form.success".into());
        delta.summary = "新增 1 个元素".into();
    }
    TransitionTraceV1 {
        transition_id: spec.transition_id.clone(),
        episode_id: spec.episode_id.clone(),
        task_id: spec.task_id.clone(),
        env_id: "env-test".into(),
        env_version: spec.env_version.clone(),
        state_before_ref: spec.state_before_ref.clone(),
        action,
        predicted: None,
        state_after_ref: spec.state_after_ref.clone(),
        observed_delta: delta,
        verifier_results: vec![VerifierResult {
            verifier: "env.step".into(),
            assertion: "步骤判定".into(),
            passed: spec.verifier_pass,
            evidence: vec![],
        }],
        reward_parts: RewardParts {
            progress: if spec.success { 1.0 } else { 0.0 },
            efficiency: 1.0,
            safety: 1.0,
        },
        outcome: if spec.success {
            TransitionOutcome::Success
        } else {
            TransitionOutcome::Failure
        },
        failure_class: if spec.success {
            None
        } else {
            Some(FailureClass::UiChanged)
        },
        fork_point: None,
        policy_version: "policy-1".into(),
        model_versions: vec![],
        privacy_scope: spec.scope,
        created_at: "2026-08-22T00:00:00Z".into(),
    }
}

// ---------------------------------------------------------------------------
// TransitionStore：幂等 + 崩溃重放
// ---------------------------------------------------------------------------

#[test]
fn store_is_idempotent_and_replays_from_jsonl() {
    let dir = temp_dir();
    let path = dir.join("transitions.jsonl");

    let trace = make_trace(&TraceSpec::default());
    let mut store = TransitionStore::load(&path).expect("load empty");
    assert!(store.is_empty());

    assert!(
        store.append(trace.clone()).expect("append"),
        "首次写入应为新记录"
    );
    assert!(
        !store.append(trace.clone()).expect("append dup"),
        "重复 transition_id 必须幂等"
    );
    assert_eq!(store.len(), 1);

    // 追加第二条与崩溃重放。
    let spec2 = TraceSpec {
        transition_id: "t-2".into(),
        ..Default::default()
    };
    store.append(make_trace(&spec2)).expect("append 2");

    let reloaded = TransitionStore::load(&path).expect("reload");
    assert_eq!(reloaded.len(), 2, "重放后数量必须一致");
    assert_eq!(reloaded.episodes(), vec!["ep-1".to_string()]);
    assert_eq!(reloaded.by_episode("ep-1").len(), 2);
    assert!(reloaded.get("t-2").is_some());
}

#[test]
fn store_skips_corrupted_lines() {
    let dir = temp_dir();
    let path = dir.join("corrupt.jsonl");
    std::fs::write(&path, concat!("{ this is not json }\n", "\n")).expect("write");
    let store = TransitionStore::load(&path).expect("load");
    assert_eq!(store.len(), 0);
    assert_eq!(store.bad_lines(), 1, "损坏行只计数，不阻塞恢复");
}

// ---------------------------------------------------------------------------
// fork 对齐
// ---------------------------------------------------------------------------

fn episode_trace(
    episode: &str,
    idx: usize,
    before: &str,
    after: &str,
    action_id: &str,
    success: bool,
) -> TransitionTraceV1 {
    episode_trace_target(episode, idx, before, after, action_id, action_id, success)
}

/// 同 [`episode_trace`]，但显式指定 `target`，使动作指纹（签名含 target）可区分。
fn episode_trace_target(
    episode: &str,
    idx: usize,
    before: &str,
    after: &str,
    action_id: &str,
    target: &str,
    success: bool,
) -> TransitionTraceV1 {
    let spec = TraceSpec {
        transition_id: format!("{episode}-t{idx}"),
        episode_id: episode.into(),
        state_before_ref: before.into(),
        state_after_ref: after.into(),
        action_id: action_id.into(),
        op: "click".into(),
        target_id: Some(target.into()),
        target_evidence: vec![format!("element:{target}")],
        success,
        ..Default::default()
    };
    make_trace(&spec)
}

#[test]
fn align_detects_action_choice_divergence() {
    // 相同起始状态；失败轨迹第一步选择了不同目标的动作（指纹不同 → 决策分叉）。
    let success = vec![
        episode_trace_target("ep-s", 0, "S0", "S1", "focus-input", "chat.input", true),
        episode_trace_target("ep-s", 1, "S1", "S2", "send", "chat.send", true),
    ];
    let failure = vec![
        episode_trace_target("ep-f", 0, "S0", "S0", "click-blank", "window.blank", false),
        episode_trace_target(
            "ep-f",
            1,
            "S0",
            "S0",
            "click-blank-again",
            "window.blank",
            false,
        ),
    ];
    let alignment = align_fork_point(&success, &failure).expect("应找到分叉点");
    assert_eq!(alignment.fork_state_ref, "S0");
    assert_eq!(alignment.divergence, ForkDivergence::ActionChoice);
    assert_eq!(alignment.good_action.action_id, "focus-input");
    assert_eq!(alignment.bad_action.action_id, "click-blank");
    assert_eq!(alignment.failure_class, Some(FailureClass::UiChanged));
}

#[test]
fn align_detects_state_divergence_from_popup_fault() {
    // 动作相同，但失败轨迹在第二步起状态被弹窗污染（环境分叉）。
    let success = vec![
        episode_trace("ep-s", 0, "S0", "S1", "focus-input", true),
        episode_trace("ep-s", 1, "S1", "S2", "send", true),
    ];
    let failure = vec![
        episode_trace("ep-f", 0, "S0", "S1-polluted", "focus-input", false),
        episode_trace("ep-f", 1, "S1-polluted", "S1-polluted", "send", false),
    ];
    let alignment = align_fork_point(&success, &failure).expect("应找到分叉点");
    assert_eq!(alignment.divergence, ForkDivergence::StateDivergence);
    assert_eq!(alignment.fork_state_ref, "S0", "分叉点应为最后共同状态");
    assert_eq!(alignment.good_action.action_id, "focus-input");
}

#[test]
fn align_returns_none_without_common_origin() {
    let success = vec![episode_trace("ep-s", 0, "S0", "S1", "a", true)];
    let failure = vec![episode_trace("ep-f", 0, "X0", "X1", "b", false)];
    assert!(align_fork_point(&success, &failure).is_none());
    assert!(align_fork_point(&[], &failure).is_none());
}

#[test]
fn annotate_fork_points_backfills_failure_traces() {
    let mut traces = vec![
        episode_trace("ep-s", 0, "S0", "S1", "focus-input", true),
        episode_trace("ep-s", 1, "S1", "S2", "send", true),
        episode_trace("ep-f", 0, "S0", "S1-polluted", "focus-input", false),
        episode_trace("ep-f", 1, "S1-polluted", "S1-polluted", "send", false),
    ];
    let annotated = annotate_fork_points(&mut traces);
    assert!(
        annotated >= 2,
        "失败轨迹必须回填 fork_point（得到 {annotated}）"
    );
    for trace in &traces {
        if trace.episode_id == "ep-f" {
            assert_eq!(trace.fork_point.as_deref(), Some("S0"));
        } else {
            assert!(trace.fork_point.is_none(), "成功轨迹不回填");
        }
    }
}

// ---------------------------------------------------------------------------
// ExperienceStore 接线
// ---------------------------------------------------------------------------

#[test]
fn transition_experience_is_recorded_idempotently() {
    let store = ExperienceStore::default();
    let trace = make_trace(&TraceSpec::default());
    record_transition_experience(&store, &trace).expect("record");
    record_transition_experience(&store, &trace).expect("record again");

    let events = store.events();
    let transitions: Vec<_> = events
        .iter()
        .filter(|e| e.kind == ExperienceKind::Transition)
        .collect();
    assert_eq!(transitions.len(), 1, "幂等：重复记录不新增");
    assert_eq!(transitions[0].outcome, Outcome::Success);
    assert_eq!(transitions[0].worker, "env:env-test");

    // 失败轨迹带失败摘要。
    let spec = TraceSpec {
        transition_id: "t-fail".into(),
        success: false,
        ..Default::default()
    };
    let failed = make_trace(&spec);
    record_transition_experience(&store, &failed).expect("record fail");
    let failed_event = store
        .events()
        .into_iter()
        .find(|e| e.correlation_id == "transition:t-fail")
        .expect("fail event");
    assert_eq!(failed_event.outcome, Outcome::Failure);
    assert_eq!(
        failed_event.attribution.error.as_deref(),
        Some("ui_changed")
    );
}

// ---------------------------------------------------------------------------
// Dataset Builder：清洗顺序、拒绝原因、清单
// ---------------------------------------------------------------------------

#[test]
fn dataset_builder_applies_documented_filter_order() {
    let mut traces = Vec::new();

    // 合格样本 ×2（一成功一失败，保证平衡不裁剪）。
    let ok_success = TraceSpec {
        transition_id: "ok-success".into(),
        ..Default::default()
    };
    traces.push(make_trace(&ok_success));
    let ok_failure = TraceSpec {
        transition_id: "ok-failure".into(),
        success: false,
        verifier_pass: false,
        state_before_ref: "before-f".into(),
        ..Default::default()
    };
    traces.push(make_trace(&ok_failure));

    // 1) 环境版本不允许。
    let bad_version = TraceSpec {
        transition_id: "bad-version".into(),
        env_version: "S2-vm-0.9".into(),
        state_before_ref: "before-bv".into(),
        ..Default::default()
    };
    traces.push(make_trace(&bad_version));

    // 2) 状态不完整。
    let incomplete = TraceSpec {
        transition_id: "incomplete".into(),
        state_after_ref: "".into(),
        ..Default::default()
    };
    traces.push(make_trace(&incomplete));

    // 3) 目标不在证据中。
    let no_evidence = TraceSpec {
        transition_id: "no-evidence".into(),
        target_id: Some("chat.send".into()),
        target_evidence: vec!["element:other".into()],
        ..Default::default()
    };
    traces.push(make_trace(&no_evidence));

    // 4) 坐标不在目标框。
    let outside = TraceSpec {
        transition_id: "outside-bounds".into(),
        click_point: Some((500, 500)),
        target_bounds: Some((0, 0, 100, 100)),
        ..Default::default()
    };
    traces.push(make_trace(&outside));

    // 5a) 缺少 Verifier（verifier_results 为空在合成器中默认非空，这里直接改）。
    let no_verifier_spec = TraceSpec {
        transition_id: "no-verifier".into(),
        ..Default::default()
    };
    let mut no_verifier = make_trace(&no_verifier_spec);
    no_verifier.verifier_results.clear();
    traces.push(no_verifier);

    // 5b) Verifier 与终态不一致（成功终态但断言失败）。
    let inconsistent_spec = TraceSpec {
        transition_id: "inconsistent".into(),
        verifier_pass: false,
        ..Default::default()
    };
    traces.push(make_trace(&inconsistent_spec));

    // 6) 隐私域默认不可训练。
    let private_spec = TraceSpec {
        transition_id: "private".into(),
        scope: PrivacyScope::S3Real,
        ..Default::default()
    };
    traces.push(make_trace(&private_spec));

    // 7) 重复（与 ok-success 同状态同动作）。
    let duplicate = TraceSpec {
        transition_id: "duplicate".into(),
        ..Default::default()
    };
    traces.push(make_trace(&duplicate));

    let config = DatasetBuilderConfig {
        allowed_env_versions: vec!["S1-chat-1.0.0".into()],
        allow_real_data: false,
        balance_ratio: 3.0,
        dataset_id: Some("dataset-test".into()),
    };
    let result = build_dataset(&traces, &config);

    assert_eq!(result.manifest.dataset_id, "dataset-test");
    assert_eq!(result.manifest.input_count, traces.len());
    assert_eq!(result.manifest.accepted_count, 2);
    assert_eq!(result.manifest.success_count, 1);
    assert_eq!(result.manifest.failure_count, 1);
    assert_eq!(
        result.manifest.env_versions,
        vec!["S1-chat-1.0.0".to_string()]
    );

    // 每类拒绝原因至少命中一次。
    let counts = &result.manifest.rejection_counts;
    for reason in [
        RejectReason::BadEnvVersion,
        RejectReason::IncompleteState,
        RejectReason::TargetNotInEvidence,
        RejectReason::CoordOutsideBounds,
        RejectReason::NoVerifier,
        RejectReason::VerifierInconsistent,
        RejectReason::PrivacyExcluded,
        RejectReason::Duplicate,
    ] {
        assert!(
            counts.get(reason.as_str()).copied().unwrap_or(0) >= 1,
            "拒绝原因 {} 必须被命中（实际：{counts:?}）",
            reason.as_str()
        );
    }

    // 清单落盘往返。
    let dir = temp_dir();
    let manifest_path = dir.join("manifest.json");
    save_manifest(&manifest_path, &result.manifest).expect("save");
    let loaded = load_manifest(&manifest_path).expect("load");
    assert_eq!(loaded, result.manifest);
    assert!(!loaded.content_hash.is_empty());
}

#[test]
fn dataset_builder_balances_success_failure() {
    // 5 成功 + 1 失败，比例上限 1.0 → 成功最多保留 1。
    let mut traces = Vec::new();
    for i in 0..5 {
        let spec = TraceSpec {
            transition_id: format!("s-{i}"),
            state_before_ref: format!("before-s{i}"),
            ..Default::default()
        };
        traces.push(make_trace(&spec));
    }
    let failure = TraceSpec {
        transition_id: "f-0".into(),
        success: false,
        verifier_pass: false,
        state_before_ref: "before-f0".into(),
        ..Default::default()
    };
    traces.push(make_trace(&failure));

    let config = DatasetBuilderConfig {
        balance_ratio: 1.0,
        dataset_id: Some("balanced".into()),
        ..Default::default()
    };
    let result = build_dataset(&traces, &config);
    assert_eq!(result.manifest.failure_count, 1);
    assert!(
        result.manifest.success_count <= 1,
        "多数类必须被裁剪至少数类的 {:.0} 倍以内",
        config.balance_ratio
    );
    assert!(result
        .manifest
        .rejection_counts
        .contains_key(RejectReason::BalanceTrimmed.as_str()));
}

#[test]
fn dataset_builder_allows_real_data_only_with_explicit_flag() {
    let spec = TraceSpec {
        transition_id: "real".into(),
        scope: PrivacyScope::S3Real,
        ..Default::default()
    };
    let traces = vec![make_trace(&spec)];

    let strict = build_dataset(&traces, &DatasetBuilderConfig::default());
    assert_eq!(strict.manifest.accepted_count, 0);

    let permissive = build_dataset(
        &traces,
        &DatasetBuilderConfig {
            allow_real_data: true,
            ..Default::default()
        },
    );
    assert_eq!(
        permissive.manifest.accepted_count, 1,
        "显式放行后 S3 样本可进入"
    );
}
