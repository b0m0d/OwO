//! WM0 契约测试：GuiWorldModel + RuleWorldModel + 候选比较 + 影子预测 + 校准
//! （主文档 §5.12.1/§5.12.2、§9.0 WM0、§11.7）。
//!
//! 覆盖完成标准：
//! - 无模型时安全回退（确定性路径）；
//! - 至少一条预测/真实对照（影子预测）；
//! - 错误预测不改变真实动作决策与任务终态；
//! - 在 S1 任务中展示候选动作的不同预测结果；
//! - 全链路：S1 rollout → TransitionStore → fork 对齐 → DatasetBuilder → RuleWorldModel。

use owo_agent_core::dataset_builder::{build_dataset, DatasetBuilderConfig};
use owo_agent_core::desktop_env::{
    ActionKind, DesktopEnv, GroundedAction, RiskLevel, SimAppKind, SimDesktopEnv, StateDelta,
    TaskSeed,
};
use owo_agent_core::transition::{
    annotate_fork_points, FailureClass, PrivacyScope, TransitionOutcome, TransitionStore,
    TransitionTraceV1, VerifierResult,
};
use owo_agent_core::world_model::{
    advise_candidates, aggregate_calibration, evaluate_prediction, shadow_step, Advice, AdviceMode,
    GuiWorldModel, ModelError, PredictionEvaluation, RuleWorldModel, RunMode, TransitionMeta,
    UnavailableWorldModel, WorldModelContext, WorldPrediction,
};
use serde_json::json;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// 助手
// ---------------------------------------------------------------------------

fn click(id: &str, x: i32, y: i32) -> GroundedAction {
    GroundedAction {
        action_id: id.into(),
        kind: ActionKind::Gui,
        semantic_intent: format!("点击 {id}"),
        target_id: Some(id.into()),
        target_evidence: vec![format!("element:{id}")],
        arguments: json!({ "op": "click", "x": x, "y": y }),
        expected_effects: vec![],
        risk: RiskLevel::Low,
        reversible: true,
        idempotency_key: None,
        click_point: Some((x, y)),
        target_bounds: None,
    }
}

fn wait_action(id: &str) -> GroundedAction {
    GroundedAction {
        action_id: id.into(),
        kind: ActionKind::Wait,
        semantic_intent: "等待".into(),
        target_id: None,
        target_evidence: vec![],
        arguments: json!({ "steps": 1 }),
        expected_effects: vec![],
        risk: RiskLevel::Low,
        reversible: true,
        idempotency_key: None,
        click_point: None,
        target_bounds: None,
    }
}

fn chat_task(seed: u64) -> TaskSeed {
    TaskSeed {
        task_id: "task-chat-send".into(),
        app: SimAppKind::Chat,
        seed,
        assets: json!({ "contacts": ["Alice"] }),
    }
}

fn context_for(env: &SimDesktopEnv) -> WorldModelContext {
    WorldModelContext {
        app: env.env_version().to_string(),
        task_goal: "给 Alice 发送消息".into(),
        history: vec![],
    }
}

/// 固定输出的测试模型：按 action_id 返回预设预测。
struct StubModel {
    predictions: HashMap<String, WorldPrediction>,
    default: Option<WorldPrediction>,
}

impl StubModel {
    fn prediction(p: f32, uncertainty: f32, risk: RiskLevel) -> WorldPrediction {
        let mut delta = StateDelta::default();
        delta.added_elements.push("chat.msg.0".into());
        delta.summary = "新增 1 个元素".into();
        WorldPrediction {
            predicted_delta: delta,
            assertion_probabilities: vec![],
            success_probability: p,
            risk,
            uncertainty,
            model_id: "stub".into(),
            model_version: "0.0.1".into(),
        }
    }
}

impl GuiWorldModel for StubModel {
    fn predict(
        &self,
        _state: &owo_agent_core::desktop_env::WorldStateV1,
        action: &GroundedAction,
        _context: &WorldModelContext,
    ) -> Result<WorldPrediction, ModelError> {
        if let Some(prediction) = self.predictions.get(&action.action_id) {
            return Ok(prediction.clone());
        }
        self.default
            .clone()
            .ok_or_else(|| ModelError::NoRule(action.action_id.clone()))
    }

    fn model_id(&self) -> &str {
        "stub"
    }
    fn model_version(&self) -> &str {
        "0.0.1"
    }
}

/// 在聊天环境执行一条「聚焦→输入→发送→等待」轨迹，收集 transitions。
/// `inject_popup_before_send` 复现「注入弹窗导致的失败轨迹」。
async fn collect_chat_episode(
    env_id: &str,
    episode_id: &str,
    seed: u64,
    inject_popup_before_send: bool,
    store: &mut TransitionStore,
) -> (SimDesktopEnv, WorldModelContext) {
    let mut env = SimDesktopEnv::new(env_id);
    env.reset(chat_task(seed)).await.expect("reset");
    let context = WorldModelContext {
        app: env.env_version().to_string(),
        task_goal: "给 Alice 发送消息".into(),
        history: vec![],
    };
    let mut actions = vec![
        click("chat.input", 510, 640),
        {
            let mut a = click("type-anchor", 0, 0);
            a.kind = ActionKind::Gui;
            a.arguments = json!({ "op": "type", "text": "hello" });
            a.action_id = "type-hello".into();
            a.click_point = None;
            a.target_id = None;
            a.target_evidence.clear();
            a
        },
        click("chat.send", 870, 640),
        wait_action("wait-reply"),
    ];
    for (idx, action) in std::mem::take(&mut actions).into_iter().enumerate() {
        if inject_popup_before_send && action.action_id == "chat.send" {
            env.inject_fault(owo_agent_core::desktop_env::FaultSpec::ModalPopup {
                text: "升级提示".into(),
            })
            .await
            .expect("inject popup");
        }
        let before_state = env.observe().await.expect("observe");
        let step = env.step(action).await.expect("step");
        let outcome = if step.verdict.passed() {
            TransitionOutcome::Success
        } else {
            TransitionOutcome::Failure
        };
        let trace = TransitionTraceV1 {
            transition_id: format!("{episode_id}-t{idx}"),
            episode_id: episode_id.into(),
            task_id: "task-chat-send".into(),
            env_id: before_state.env_id.clone(),
            env_version: before_state.env_version.clone(),
            state_before_ref: step.before_state_ref.clone(),
            action: step.action.clone(),
            predicted: None,
            state_after_ref: step.after_state_ref.clone(),
            observed_delta: step.observed_delta.clone(),
            verifier_results: VerifierResult::from_verdict("env.step", &step.verdict),
            reward_parts: step.reward_parts.clone(),
            outcome,
            failure_class: if step.verdict.passed() {
                None
            } else {
                Some(FailureClass::UiChanged)
            },
            fork_point: None,
            policy_version: "policy-1".into(),
            model_versions: vec![],
            privacy_scope: PrivacyScope::S1Sim,
            created_at: "2026-08-22T00:00:00Z".into(),
        };
        store.append(trace).expect("append transition");
    }
    (env, context)
}

// ---------------------------------------------------------------------------
// 无模型回退（§11.7：无模型回退必须验证）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_model_falls_back_to_deterministic_path() {
    let mut env = SimDesktopEnv::new("env-nomodel");
    let state = env.reset(chat_task(1)).await.expect("reset");
    let candidates = vec![click("chat.input", 510, 640), wait_action("wait")];
    let context = context_for(&env);

    // model = None。
    match advise_candidates(None, &state, &candidates, &context) {
        Advice::Execute { index, mode, .. } => {
            assert_eq!(index, 0);
            assert_eq!(mode, AdviceMode::DeterministicFallback);
        }
        other => panic!("应回退确定性路径，得到 {other:?}"),
    }

    // model = 显式不可用。
    let unavailable = UnavailableWorldModel;
    match advise_candidates(Some(&unavailable), &state, &candidates, &context) {
        Advice::Execute { index, mode, .. } => {
            assert_eq!(index, 0);
            assert_eq!(mode, AdviceMode::DeterministicFallback);
        }
        other => panic!("不可用模型必须回退，得到 {other:?}"),
    }

    // 无候选 → ask_user。
    match advise_candidates(None, &state, &[], &context) {
        Advice::AskUser { .. } => {}
        other => panic!("无候选应 ask_user，得到 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 候选比较策略
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advisor_ranks_close_candidates_and_uncertainty() {
    let mut env = SimDesktopEnv::new("env-advisor");
    let state = env.reset(chat_task(2)).await.expect("reset");
    let context = context_for(&env);
    let candidates = vec![click("c0", 510, 640), click("c1", 870, 640)];

    // 明确排序：c1 成功率显著更高。
    let model = StubModel {
        predictions: HashMap::from([
            (
                "c0".to_string(),
                StubModel::prediction(0.2, 0.1, RiskLevel::Low),
            ),
            (
                "c1".to_string(),
                StubModel::prediction(0.9, 0.1, RiskLevel::Low),
            ),
        ]),
        default: None,
    };
    match advise_candidates(Some(&model), &state, &candidates, &context) {
        Advice::Execute {
            index,
            mode,
            prediction,
            ..
        } => {
            assert_eq!(index, 1, "应选择预测成功率更高的候选");
            assert_eq!(mode, AdviceMode::ModelRanked);
            assert!(prediction.is_some());
        }
        other => panic!("应模型排序执行，得到 {other:?}"),
    }

    // 前二接近 → ask_user。
    let close_model = StubModel {
        predictions: HashMap::from([
            (
                "c0".to_string(),
                StubModel::prediction(0.50, 0.1, RiskLevel::Low),
            ),
            (
                "c1".to_string(),
                StubModel::prediction(0.52, 0.1, RiskLevel::Low),
            ),
        ]),
        default: None,
    };
    assert!(matches!(
        advise_candidates(Some(&close_model), &state, &candidates, &context),
        Advice::AskUser { .. }
    ));

    // 全部高不确定 → reobserve。
    let uncertain_model = StubModel {
        predictions: HashMap::from([
            (
                "c0".to_string(),
                StubModel::prediction(0.9, 0.9, RiskLevel::Low),
            ),
            (
                "c1".to_string(),
                StubModel::prediction(0.2, 0.8, RiskLevel::Low),
            ),
        ]),
        default: None,
    };
    assert!(matches!(
        advise_candidates(Some(&uncertain_model), &state, &candidates, &context),
        Advice::Reobserve { .. }
    ));

    // 安全过滤：Critical 风险候选被剔除（即使成功率更高）。
    let risky_model = StubModel {
        predictions: HashMap::from([
            (
                "c0".to_string(),
                StubModel::prediction(0.6, 0.1, RiskLevel::Low),
            ),
            (
                "c1".to_string(),
                StubModel::prediction(0.95, 0.1, RiskLevel::Critical),
            ),
        ]),
        default: None,
    };
    match advise_candidates(Some(&risky_model), &state, &candidates, &context) {
        Advice::Execute { index, .. } => assert_eq!(index, 0, "Critical 候选必须被剔除"),
        other => panic!("应执行安全候选，得到 {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// RuleWorldModel：从 transition 语料学习
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rule_model_learns_transition_table_from_corpus() {
    let mut store = TransitionStore::in_memory();
    // 三条成功轨迹（同一动作签名）。
    for i in 0..3 {
        collect_chat_episode(
            &format!("env-r{i}"),
            &format!("ep-r{i}"),
            100 + i,
            false,
            &mut store,
        )
        .await;
    }
    let traces: Vec<TransitionTraceV1> = store.traces().into_iter().cloned().collect();
    let model = RuleWorldModel::from_transitions("rule-wm", "0.1.0", &traces);
    assert!(model.rule_count() > 0, "规则表必须非空");

    let mut env = SimDesktopEnv::new("env-predict");
    let state = env.reset(chat_task(5)).await.expect("reset");
    let context = context_for(&env);

    // 已见动作：预测可用，成功率与置信度合理。
    let send = click("chat.send", 870, 640);
    let prediction = model.predict(&state, &send, &context).expect("predict");
    assert!(
        prediction.success_probability > 0.6,
        "全成功语料应预测高成功率"
    );
    assert!(prediction.uncertainty <= 0.25, "样本充足时不确定度应低");
    assert_eq!(prediction.model_id, "rule-wm");
    assert_eq!(prediction.model_version, "0.1.0");
    assert!(
        !prediction.predicted_delta.is_empty(),
        "预测差分应来自语料中最频繁的真实差分"
    );

    // 未见动作：显式 NoRule（由调用方回退，不静默）。
    let unknown = click("nonexistent.element", 1, 1);
    let err = model
        .predict(&state, &unknown, &context)
        .expect_err("未见动作");
    assert!(matches!(err, ModelError::NoRule(_)));
}

// ---------------------------------------------------------------------------
// 影子预测：预测/真实对照，且预测不改变动作决策（§11.7 + §4.2）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shadow_step_records_prediction_vs_reality_without_changing_decision() {
    let mut store = TransitionStore::in_memory();
    // 先积累语料，使规则模型对发送动作有预测能力。
    collect_chat_episode("env-corpus", "ep-corpus", 77, false, &mut store).await;
    let traces: Vec<TransitionTraceV1> = store.traces().into_iter().cloned().collect();
    let model = RuleWorldModel::from_transitions("rule-wm", "0.1.0", &traces);

    let mut env = SimDesktopEnv::new("env-shadow");
    env.reset(chat_task(77)).await.expect("reset");
    let context = context_for(&env);

    env.step(click("chat.input", 510, 640))
        .await
        .expect("focus");
    let mut type_action = click("type-anchor", 0, 0);
    type_action.arguments = json!({ "op": "type", "text": "shadow-hello" });
    type_action.action_id = "type-shadow".into();
    type_action.click_point = None;
    env.step(type_action).await.expect("type");

    // 候选：发送 / 等待。策略既定动作 = 发送（execute_index=0）。
    let candidates = vec![click("chat.send", 870, 640), wait_action("wait-instead")];
    let meta = TransitionMeta {
        transition_id: "shadow-send".into(),
        episode_id: "ep-shadow".into(),
        task_id: "task-chat-send".into(),
        policy_version: "policy-1".into(),
        model_versions: vec!["rule-wm@0.1.0".into()],
        privacy_scope: PrivacyScope::S1Sim,
    };
    let report = shadow_step(
        &mut env,
        RunMode::Shadow,
        Some(&model),
        &candidates,
        0,
        &context,
        meta,
    )
    .await
    .expect("shadow step");

    // 预测被记录（发送动作在语料中有规则）。
    assert!(!report.predictions.is_empty(), "候选预测必须被记录");
    assert!(
        report.transition.predicted.is_some(),
        "执行动作的预测必须写入 transition"
    );
    // 预测/真实对照存在且命中（发送成功）。
    let evaluation = report.evaluation.expect("对照评估必须存在");
    assert!(evaluation.actual_success, "发送应成功");
    assert!(evaluation.success_hit, "成功预测应命中真实成功");
    assert!(report.step_result.verdict.passed());
    // 影子预测不改变执行选择：执行的仍是策略既定的候选 0。
    assert_eq!(report.executed_action_id, candidates[0].action_id);
}

#[tokio::test]
async fn shadow_mode_ignores_model_ranking_for_execution() {
    let mut env = SimDesktopEnv::new("env-shadow-rank");
    env.reset(chat_task(8)).await.expect("reset");
    let context = context_for(&env);
    env.step(click("chat.input", 510, 640))
        .await
        .expect("focus");

    // 模型强烈偏好候选 1（等待），但策略既定动作是候选 0（聚焦联系人）。
    let model = StubModel {
        predictions: HashMap::from([
            (
                "focus-contact".to_string(),
                StubModel::prediction(0.1, 0.1, RiskLevel::Low),
            ),
            (
                "wait-instead".to_string(),
                StubModel::prediction(0.99, 0.05, RiskLevel::Low),
            ),
        ]),
        default: None,
    };
    let candidates = vec![click("focus-contact", 110, 54), wait_action("wait-instead")];
    let meta = TransitionMeta {
        transition_id: "shadow-rank".into(),
        episode_id: "ep-rank".into(),
        task_id: "task-chat-send".into(),
        policy_version: "policy-1".into(),
        model_versions: vec![],
        privacy_scope: PrivacyScope::S1Sim,
    };
    let report = shadow_step(
        &mut env,
        RunMode::Shadow,
        Some(&model),
        &candidates,
        0,
        &context,
        meta,
    )
    .await
    .expect("shadow step");

    // 模型排序确实偏好候选 1……
    match &report.advice {
        Advice::Execute { index, mode, .. } => {
            assert_eq!(*index, 1);
            assert_eq!(*mode, AdviceMode::ModelRanked);
        }
        other => panic!("模型应排序候选 1，得到 {other:?}"),
    }
    // ……但 Shadow 模式执行的仍是策略既定动作（候选 0）：预测不是事实。
    assert_eq!(report.executed_action_id, "focus-contact");
}

// ---------------------------------------------------------------------------
// 校准聚合
// ---------------------------------------------------------------------------

#[test]
fn calibration_report_aggregates_hits_and_buckets() {
    let evaluations = vec![
        PredictionEvaluation {
            action_id: "a1".into(),
            success_probability: 0.9,
            actual_success: true,
            success_hit: true,
            calibration_error: 0.1,
            delta_jaccard: 1.0,
            uncertainty: 0.1,
        },
        PredictionEvaluation {
            action_id: "a2".into(),
            success_probability: 0.8,
            actual_success: false,
            success_hit: false,
            calibration_error: 0.8,
            delta_jaccard: 0.0,
            uncertainty: 0.6,
        },
    ];
    let report = aggregate_calibration(&evaluations);
    assert_eq!(report.samples, 2);
    assert!((report.success_hit_rate - 0.5).abs() < 1e-9);
    // f32 → f64 聚合，放宽容差。
    assert!((report.mean_calibration_error - 0.45).abs() < 1e-6);
    assert!((report.mean_delta_jaccard - 0.5).abs() < 1e-9);
    assert_eq!(report.uncertainty_buckets.len(), 4);
    assert!(report
        .uncertainty_buckets
        .iter()
        .any(|b| b.samples == 1 && b.hit_rate == 1.0));

    let empty = aggregate_calibration(&[]);
    assert_eq!(empty.samples, 0);
}

#[test]
fn evaluate_prediction_compares_delta_sets() {
    let mut predicted = StateDelta::default();
    predicted.added_elements.push("chat.msg.0".into());
    let mut observed = StateDelta::default();
    observed.added_elements.push("chat.msg.0".into());
    observed.added_elements.push("extra".into());
    let prediction = WorldPrediction {
        predicted_delta: predicted,
        assertion_probabilities: vec![],
        success_probability: 0.7,
        risk: RiskLevel::Low,
        uncertainty: 0.2,
        model_id: "m".into(),
        model_version: "v".into(),
    };
    let step = owo_agent_core::desktop_env::StepResult {
        before_state_ref: "b".into(),
        action: wait_action("w"),
        after_state_ref: "a".into(),
        observed_delta: observed,
        verdict: owo_agent_core::desktop_env::Verdict::Pass { evidence: vec![] },
        reward_parts: owo_agent_core::desktop_env::RewardParts {
            progress: 1.0,
            efficiency: 1.0,
            safety: 1.0,
        },
        duration_ms: 0,
        error: None,
        evidence_refs: vec![],
    };
    let evaluation = evaluate_prediction(&prediction, &step);
    assert!(evaluation.success_hit);
    assert!((evaluation.delta_jaccard - (1.0 / 2.0)).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// 全链路：S1 → transitions → fork 对齐 → dataset → 规则模型
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_chain_from_sim_rollout_to_rule_world_model() {
    let mut store = TransitionStore::in_memory();

    // 成功轨迹（seed 相同，保证与失败轨迹同起点）。
    collect_chat_episode("env-chain", "ep-success", 55, false, &mut store).await;
    // 失败轨迹：发送前注入弹窗（对应 T0 完成标准的注入弹窗场景）。
    let (mut env, context) =
        collect_chat_episode("env-chain", "ep-failure", 55, true, &mut store).await;

    // fork 对齐：失败轨迹回填分叉点。
    let mut traces: Vec<TransitionTraceV1> = store.traces().into_iter().cloned().collect();
    let annotated = annotate_fork_points(&mut traces);
    assert!(annotated > 0, "失败轨迹必须回填 fork_point");
    let failure_forks: Vec<_> = traces
        .iter()
        .filter(|t| t.episode_id == "ep-failure" && t.fork_point.is_some())
        .collect();
    assert!(!failure_forks.is_empty());

    // 数据构建：两集前两步 (状态, 动作, 终态) 完全相同 → 去重 2 条；
    // 发送/等待步因状态或终态不同而保留（成功/失败对不被误删）。
    let build = build_dataset(
        &traces,
        &DatasetBuilderConfig {
            allowed_env_versions: vec!["S1-chat-1.0.0".into()],
            dataset_id: Some("dataset-chain".into()),
            ..Default::default()
        },
    );
    // 去重 2（两集前两步完全同键）；平衡裁剪 2：失败集里的 wait 步本身是成功终态，
    // 清洗后成功:失败 = 5:1，超出 3.0x 上限 → 尾部 2 条成功被裁剪。
    assert_eq!(build.manifest.accepted_count, 4);
    assert_eq!(build.manifest.success_count, 3);
    assert_eq!(build.manifest.failure_count, 1);
    assert_eq!(
        build.manifest.rejection_counts.get("duplicate").copied(),
        Some(2)
    );
    assert_eq!(
        build
            .manifest
            .rejection_counts
            .get("balance_trimmed")
            .copied(),
        Some(2)
    );

    // 用接受样本训练规则世界模型。
    let model = RuleWorldModel::from_transitions("rule-wm-chain", "0.1.0", &build.accepted);
    assert!(model.rule_count() > 0);

    // 对「发送」动作的预测应反映 1 成功 + 1 失败的混合历史：
    // success_probability = (1+1)/(2+2) = 0.5（Laplace 平滑）。
    let state = env.observe().await.expect("observe");
    let send = click("chat.send", 870, 640);
    let prediction = model.predict(&state, &send, &context).expect("predict");
    assert!(
        (prediction.success_probability - 0.5).abs() < 1e-6,
        "混合语料应给出平滑后的中间成功率，得到 {}",
        prediction.success_probability
    );
    assert!(prediction.uncertainty > 0.0 && prediction.uncertainty <= 1.0);
}
