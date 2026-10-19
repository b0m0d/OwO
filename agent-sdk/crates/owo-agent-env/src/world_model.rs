//! GUI 世界模型与影子预测（WM0，对应主开发技术文档 §5.12.1 / §5.12.2）。
//!
//! 职责分离（§5.12.1）：世界模型只预测候选动作的后果与风险，不调用工具、
//! 不把预测写成事实；真实 `observe/judge` 结果才可更新任务状态。
//!
//! 本模块实现：
//! - [`GuiWorldModel`] trait 与 [`WorldPrediction`] DTO（第一版只预测结构化状态差分，
//!   不生成完整下一帧截图）；
//! - [`RuleWorldModel`]：WM-0 规则/频率基线，从 [`TransitionTraceV1`] 语料构建
//!   每「应用 × 动作签名」转换表，输出最频繁差分、成功率、风险与不确定度；
//! - [`UnavailableWorldModel`]：无模型时的显式占位（安全回退测试用）；
//! - [`advise_candidates`]：候选比较。默认深度 1：模型不可用/预测失败 →
//!   确定性路径；全部高不确定 → `reobserve`；前二接近 → `ask_user`；
//! - [`shadow_step`]：`RunMode::Shadow` 单步执行——预测所有候选、执行策略既定动作、
//!   记录预测与真实差分对照；预测不改变真实动作选择与任务终态；
//! - [`PredictionEvaluation`] / [`CalibrationReport`]：命中、校准误差与差分重合度聚合。

use crate::desktop_env::{
    DesktopEnv, GroundedAction, RiskLevel, StateDelta, StepResult, WorldStateV1,
};
use crate::transition::{
    PredictionRef, PrivacyScope, TransitionOutcome, TransitionTraceV1, VerifierResult,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 世界模型错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
pub enum ModelError {
    #[error("模型不可用：{0}")]
    ProviderUnavailable(String),
    #[error("规则表无此动作：{0}")]
    NoRule(String),
    #[error("模型数据损坏：{0}")]
    Corrupted(String),
}

/// 断言概率预测。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionProbability {
    pub assertion: String,
    pub probability: f32,
}

/// 世界模型预测（§5.12.2）。必须标注模型版本、置信度和不确定度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldPrediction {
    pub predicted_delta: StateDelta,
    #[serde(default)]
    pub assertion_probabilities: Vec<AssertionProbability>,
    pub success_probability: f32,
    pub risk: RiskLevel,
    pub uncertainty: f32,
    pub model_id: String,
    pub model_version: String,
}

/// 预测上下文（任务子目标与应用标识）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldModelContext {
    /// 应用标识（S1 中为环境版本，如 `S1-chat-1.0.0`）。
    pub app: String,
    pub task_goal: String,
    #[serde(default)]
    pub history: Vec<String>,
}

/// GUI 世界模型 Provider 接口。
pub trait GuiWorldModel: Send + Sync {
    fn predict(
        &self,
        state: &WorldStateV1,
        action: &GroundedAction,
        context: &WorldModelContext,
    ) -> Result<WorldPrediction, ModelError>;

    fn model_id(&self) -> &str;
    fn model_version(&self) -> &str;
}

/// 无模型占位：永远返回 ProviderUnavailable，触发确定性回退路径。
#[derive(Debug, Clone, Copy, Default)]
pub struct UnavailableWorldModel;

impl GuiWorldModel for UnavailableWorldModel {
    fn predict(
        &self,
        _state: &WorldStateV1,
        _action: &GroundedAction,
        _context: &WorldModelContext,
    ) -> Result<WorldPrediction, ModelError> {
        Err(ModelError::ProviderUnavailable(
            "未加载任何世界模型（规则表为空或模型未部署）".into(),
        ))
    }

    fn model_id(&self) -> &str {
        "unavailable"
    }

    fn model_version(&self) -> &str {
        "0.0.0"
    }
}

// ---------------------------------------------------------------------------
// RuleWorldModel：WM-0 规则/频率基线
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct TransitionStats {
    n: u64,
    successes: u64,
    /// 差分指纹 → (代表差分, 计数)。
    deltas: HashMap<String, (StateDelta, u64)>,
    /// 断言 → (通过次数, 总次数)。
    assertions: HashMap<String, (u64, u64)>,
}

/// 规则/统计世界模型（WM-0）。
///
/// 从 transition 语料构建「应用 × 动作签名」转换表；预测输出：
/// - `success_probability`：Laplace 平滑成功率 `(s+1)/(n+2)`；
/// - `predicted_delta`：该键下出现频率最高的观测差分；
/// - `uncertainty`：`1/(1+n)`（样本越少越不确定）；
/// - `risk`：动作自身风险与历史失败率合成（失败率 ≥ 0.5 且样本 ≥ 3 → 至少 High）。
pub struct RuleWorldModel {
    model_id: String,
    model_version: String,
    table: HashMap<String, TransitionStats>,
}

impl RuleWorldModel {
    pub fn new(model_id: impl Into<String>, model_version: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
            model_version: model_version.into(),
            table: HashMap::new(),
        }
    }

    fn rule_key(app: &str, action: &GroundedAction) -> String {
        format!("{app}|{}", action.signature())
    }

    /// 从 transition 语料构建转换表。
    pub fn from_transitions(
        model_id: impl Into<String>,
        model_version: impl Into<String>,
        traces: &[TransitionTraceV1],
    ) -> Self {
        let mut model = Self::new(model_id, model_version);
        for trace in traces {
            let key = Self::rule_key(&trace.env_version, &trace.action);
            let stats = model.table.entry(key).or_default();
            stats.n += 1;
            if trace.outcome.is_success() {
                stats.successes += 1;
            }
            let fingerprint = trace.observed_delta.fingerprint();
            let entry = stats
                .deltas
                .entry(fingerprint)
                .or_insert_with(|| (trace.observed_delta.clone(), 0));
            entry.1 += 1;
            for verifier in &trace.verifier_results {
                let counter = stats
                    .assertions
                    .entry(verifier.assertion.clone())
                    .or_insert((0, 0));
                counter.1 += 1;
                if verifier.passed {
                    counter.0 += 1;
                }
            }
        }
        model
    }

    /// 语料中覆盖的「应用 × 动作签名」规则数。
    pub fn rule_count(&self) -> usize {
        self.table.len()
    }
}

impl GuiWorldModel for RuleWorldModel {
    fn predict(
        &self,
        _state: &WorldStateV1,
        action: &GroundedAction,
        context: &WorldModelContext,
    ) -> Result<WorldPrediction, ModelError> {
        let key = Self::rule_key(&context.app, action);
        let Some(stats) = self.table.get(&key) else {
            return Err(ModelError::NoRule(format!(
                "应用 {} 动作签名 {} 无历史转换",
                context.app,
                action.signature()
            )));
        };
        let n = stats.n;
        let success_probability = ((stats.successes + 1) as f32) / ((n + 2) as f32);
        let uncertainty = 1.0 / (1.0 + n as f32);
        let failure_rate = if n > 0 {
            (n - stats.successes) as f64 / n as f64
        } else {
            1.0
        };
        let risk = if failure_rate >= 0.5 && n >= 3 {
            std::cmp::max(action.risk, RiskLevel::High)
        } else {
            action.risk
        };
        let predicted_delta = stats
            .deltas
            .values()
            .max_by_key(|(_, count)| *count)
            .map(|(delta, _)| delta.clone())
            .unwrap_or_default();
        let mut assertion_probabilities: Vec<AssertionProbability> = stats
            .assertions
            .iter()
            .map(|(assertion, (passed, total))| AssertionProbability {
                assertion: assertion.clone(),
                probability: ((*passed + 1) as f32) / ((*total + 2) as f32),
            })
            .collect();
        assertion_probabilities.sort_by(|a, b| a.assertion.cmp(&b.assertion));
        Ok(WorldPrediction {
            predicted_delta,
            assertion_probabilities,
            success_probability,
            risk,
            uncertainty,
            model_id: self.model_id.clone(),
            model_version: self.model_version.clone(),
        })
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn model_version(&self) -> &str {
        &self.model_version
    }
}

// ---------------------------------------------------------------------------
// 候选比较（深度 1；不展开树搜索）
// ---------------------------------------------------------------------------

/// 候选建议模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdviceMode {
    /// 模型预测排序选择。
    ModelRanked,
    /// 无模型/预测失败时的确定性路径。
    DeterministicFallback,
}

/// 候选比较结论（§5.12.2：高不确定/前二接近/安全冲突 → reobserve | ask_user | 确定性）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "advice", rename_all = "snake_case")]
pub enum Advice {
    Execute {
        index: usize,
        mode: AdviceMode,
        reason: String,
        /// 完整预测对象较大（含结构化差分），装箱收窄枚举尺寸差异。
        #[serde(default)]
        prediction: Option<Box<WorldPrediction>>,
    },
    Reobserve {
        reason: String,
    },
    AskUser {
        reason: String,
    },
}

/// 前二候选预测接近阈值。
pub const CLOSE_CANDIDATE_EPS: f32 = 0.05;
/// 全部候选不确定度高于该值时建议重新感知。
pub const HIGH_UNCERTAINTY: f32 = 0.7;

/// 深度 1 候选比较。
///
/// 规则（顺序执行）：
/// 1. 无候选 → ask_user；
/// 2. 模型缺失或全部预测失败 → 确定性路径（第一个候选）；
/// 3. 预测风险为 Critical 的候选被排除（除非全部 Critical）；
/// 4. 剩余候选全部高不确定 → reobserve；
/// 5. 按 success_probability 排序，前二接近（< ε）→ ask_user；
/// 6. 否则执行最优候选（模型排序）。
pub fn advise_candidates(
    model: Option<&dyn GuiWorldModel>,
    state: &WorldStateV1,
    candidates: &[GroundedAction],
    context: &WorldModelContext,
) -> Advice {
    if candidates.is_empty() {
        return Advice::AskUser {
            reason: "无候选动作".into(),
        };
    }
    let Some(model) = model else {
        return Advice::Execute {
            index: 0,
            mode: AdviceMode::DeterministicFallback,
            reason: "无可用世界模型，使用确定性路径".into(),
            prediction: None,
        };
    };
    let predictions: Vec<Option<WorldPrediction>> = candidates
        .iter()
        .map(|action| model.predict(state, action, context).ok())
        .collect();
    if predictions.iter().all(Option::is_none) {
        return Advice::Execute {
            index: 0,
            mode: AdviceMode::DeterministicFallback,
            reason: "全部候选预测失败，使用确定性路径".into(),
            prediction: None,
        };
    }
    // 安全过滤：剔除预测风险 Critical 的候选。
    let mut eligible: Vec<usize> = (0..candidates.len())
        .filter(|i| {
            predictions[*i]
                .as_ref()
                .map(|p| p.risk < RiskLevel::Critical)
                .unwrap_or(true)
        })
        .collect();
    if eligible.is_empty() {
        eligible = (0..candidates.len()).collect();
    }
    // 全部高不确定 → 重新感知。
    let all_uncertain = eligible.iter().all(|i| {
        predictions[*i]
            .as_ref()
            .map(|p| p.uncertainty > HIGH_UNCERTAINTY)
            .unwrap_or(true)
    });
    if all_uncertain {
        return Advice::Reobserve {
            reason: "全部候选预测不确定度过高，建议重新感知".into(),
        };
    }
    // 排序：成功率降序 → 不确定度升序 → 风险升序。
    eligible.sort_by(|a, b| {
        let pa = predictions[*a].as_ref();
        let pb = predictions[*b].as_ref();
        let sa = pa.map(|p| p.success_probability).unwrap_or(0.0);
        let sb = pb.map(|p| p.success_probability).unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let ua = pa.map(|p| p.uncertainty).unwrap_or(1.0);
                let ub = pb.map(|p| p.uncertainty).unwrap_or(1.0);
                ua.partial_cmp(&ub).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                let ra = pa.map(|p| p.risk).unwrap_or(RiskLevel::Critical);
                let rb = pb.map(|p| p.risk).unwrap_or(RiskLevel::Critical);
                ra.cmp(&rb)
            })
    });
    let best = eligible[0];
    if eligible.len() >= 2 {
        let second = eligible[1];
        if let (Some(pa), Some(pb)) = (predictions[best].as_ref(), predictions[second].as_ref()) {
            if (pa.success_probability - pb.success_probability).abs() < CLOSE_CANDIDATE_EPS {
                return Advice::AskUser {
                    reason: format!(
                        "前二候选预测成功率接近（{} vs {}），请用户选择",
                        pa.success_probability, pb.success_probability
                    ),
                };
            }
        }
    }
    Advice::Execute {
        index: best,
        mode: AdviceMode::ModelRanked,
        reason: "模型排序最优".into(),
        prediction: predictions[best].clone().map(Box::new),
    }
}

// ---------------------------------------------------------------------------
// 影子预测（RunMode::Shadow）
// ---------------------------------------------------------------------------

/// 运行模式（§9.0 WM0：Shadow 只记录，不影响动作决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// 预测参与动作选择。
    Live,
    /// 预测只记录对照，不改变真实动作决策。
    Shadow,
}

/// transition 元数据（由调用方提供）。
#[derive(Debug, Clone)]
pub struct TransitionMeta {
    pub transition_id: String,
    pub episode_id: String,
    pub task_id: String,
    pub policy_version: String,
    pub model_versions: Vec<String>,
    pub privacy_scope: PrivacyScope,
}

/// 预测与真实对照的单步评估。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictionEvaluation {
    pub action_id: String,
    pub success_probability: f32,
    pub actual_success: bool,
    /// `p > 0.5` 与实际成败是否一致。
    pub success_hit: bool,
    /// `|p - actual|`。
    pub calibration_error: f32,
    /// 预测差分与观测差分的集合 Jaccard 重合度（0~1）。
    pub delta_jaccard: f64,
    pub uncertainty: f32,
}

/// 影子单步报告。
#[derive(Debug, Clone)]
pub struct ShadowStepReport {
    pub before_state: WorldStateV1,
    pub advice: Advice,
    pub executed_action_id: String,
    pub step_result: StepResult,
    pub transition: TransitionTraceV1,
    pub predictions: Vec<(String, WorldPrediction)>,
    pub evaluation: Option<PredictionEvaluation>,
}

fn delta_terms(delta: &StateDelta) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for id in &delta.added_elements {
        terms.push(format!("+{id}"));
    }
    for id in &delta.removed_elements {
        terms.push(format!("-{id}"));
    }
    for change in &delta.changed_fields {
        terms.push(format!("~{}", change.path));
    }
    if delta.window_changed {
        terms.push("!window".into());
    }
    terms.sort();
    terms.dedup();
    terms
}

/// 差分集合 Jaccard 重合度。
pub fn delta_overlap(predicted: &StateDelta, observed: &StateDelta) -> f64 {
    let a = delta_terms(predicted);
    let b = delta_terms(observed);
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.iter().filter(|t| b.contains(t)).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

/// 评估一次预测与实际结果。
pub fn evaluate_prediction(
    prediction: &WorldPrediction,
    step: &StepResult,
) -> PredictionEvaluation {
    let actual_success = step.verdict.passed();
    let success_hit = (prediction.success_probability > 0.5) == actual_success;
    let calibration_error =
        (prediction.success_probability - if actual_success { 1.0 } else { 0.0 }).abs();
    PredictionEvaluation {
        action_id: step.action.action_id.clone(),
        success_probability: prediction.success_probability,
        actual_success,
        success_hit,
        calibration_error,
        delta_jaccard: delta_overlap(&prediction.predicted_delta, &step.observed_delta),
        uncertainty: prediction.uncertainty,
    }
}

/// 影子单步：预测全部候选 → 执行策略既定动作（`execute_index`）→ 记录对照。
///
/// 关键保证（§4.2 预测不是事实）：
/// - 执行的动作由调用方的 `execute_index` 决定，预测不参与、不覆盖；
/// - 预测结果只写入 transition 与报告，不改变环境状态或任务终态。
pub async fn shadow_step<E: DesktopEnv + ?Sized>(
    env: &mut E,
    mode: RunMode,
    model: Option<&dyn GuiWorldModel>,
    candidates: &[GroundedAction],
    execute_index: usize,
    context: &WorldModelContext,
    meta: TransitionMeta,
) -> Result<ShadowStepReport, String> {
    if candidates.is_empty() {
        return Err("shadow_step 需要至少一个候选动作".into());
    }
    if execute_index >= candidates.len() {
        return Err(format!(
            "execute_index {execute_index} 超出候选数量 {}",
            candidates.len()
        ));
    }
    let before_state = env
        .observe()
        .await
        .map_err(|e| format!("observe 失败：{e}"))?;
    let advice = advise_candidates(model, &before_state, candidates, context);
    let mut predictions: Vec<(String, WorldPrediction)> = Vec::new();
    if let Some(model) = model {
        for candidate in candidates {
            if let Ok(prediction) = model.predict(&before_state, candidate, context) {
                predictions.push((candidate.action_id.clone(), prediction));
            }
        }
    }
    // Shadow 模式：预测不改变执行选择；Live 模式同样以调用方 index 为准
    // （接线到执行器前，Live 与 Shadow 的执行路径一致，只影响记录策略）。
    let _ = mode;
    let action = candidates[execute_index].clone();
    let step = env
        .step(action)
        .await
        .map_err(|e| format!("step 失败：{e}"))?;
    let executed_prediction = predictions
        .iter()
        .find(|(id, _)| *id == step.action.action_id)
        .map(|(_, p)| p.clone());
    let evaluation = executed_prediction
        .as_ref()
        .map(|p| evaluate_prediction(p, &step));
    let outcome = if step.verdict.passed() {
        TransitionOutcome::Success
    } else {
        TransitionOutcome::Failure
    };
    let transition = TransitionTraceV1 {
        transition_id: meta.transition_id,
        episode_id: meta.episode_id,
        task_id: meta.task_id,
        env_id: before_state.env_id.clone(),
        env_version: before_state.env_version.clone(),
        state_before_ref: step.before_state_ref.clone(),
        action: step.action.clone(),
        predicted: executed_prediction.as_ref().map(|p| PredictionRef {
            model_id: p.model_id.clone(),
            model_version: p.model_version.clone(),
            success_probability: p.success_probability,
            uncertainty: p.uncertainty,
            predicted_delta_fingerprint: p.predicted_delta.fingerprint(),
        }),
        state_after_ref: step.after_state_ref.clone(),
        observed_delta: step.observed_delta.clone(),
        verifier_results: VerifierResult::from_verdict("env.step", &step.verdict),
        reward_parts: step.reward_parts.clone(),
        outcome,
        failure_class: None,
        fork_point: None,
        policy_version: meta.policy_version,
        model_versions: meta.model_versions,
        privacy_scope: meta.privacy_scope,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    Ok(ShadowStepReport {
        before_state,
        advice,
        executed_action_id: step.action.action_id.clone(),
        step_result: step,
        transition,
        predictions,
        evaluation,
    })
}

// ---------------------------------------------------------------------------
// 校准聚合
// ---------------------------------------------------------------------------

/// 预测校准报告（WM0 完成标准：聚合命中、错误类型与置信区间，作为后续神经模型训练目标）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReport {
    pub samples: u64,
    pub success_hit_rate: f64,
    pub mean_calibration_error: f64,
    pub mean_delta_jaccard: f64,
    /// 按不确定度分桶（0.0-0.25 / 0.25-0.5 / 0.5-0.75 / 0.75-1.0）的样本数与命中率。
    pub uncertainty_buckets: Vec<UncertaintyBucket>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UncertaintyBucket {
    pub label: String,
    pub samples: u64,
    pub hit_rate: f64,
}

pub fn aggregate_calibration(evaluations: &[PredictionEvaluation]) -> CalibrationReport {
    let samples = evaluations.len() as u64;
    if samples == 0 {
        return CalibrationReport {
            samples: 0,
            success_hit_rate: 0.0,
            mean_calibration_error: 0.0,
            mean_delta_jaccard: 0.0,
            uncertainty_buckets: Vec::new(),
        };
    }
    let hits = evaluations.iter().filter(|e| e.success_hit).count();
    let mean_calibration_error = evaluations
        .iter()
        .map(|e| e.calibration_error as f64)
        .sum::<f64>()
        / samples as f64;
    let mean_delta_jaccard =
        evaluations.iter().map(|e| e.delta_jaccard).sum::<f64>() / samples as f64;
    let bucket_labels = ["0.00-0.25", "0.25-0.50", "0.50-0.75", "0.75-1.00"];
    let mut uncertainty_buckets = Vec::new();
    for (i, label) in bucket_labels.iter().enumerate() {
        let lower = i as f32 * 0.25;
        let upper = lower + 0.25;
        let in_bucket: Vec<&PredictionEvaluation> = evaluations
            .iter()
            .filter(|e| e.uncertainty >= lower && (e.uncertainty < upper || i == 3))
            .collect();
        let bucket_samples = in_bucket.len() as u64;
        let bucket_hits = in_bucket.iter().filter(|e| e.success_hit).count();
        uncertainty_buckets.push(UncertaintyBucket {
            label: (*label).to_string(),
            samples: bucket_samples,
            hit_rate: if bucket_samples == 0 {
                0.0
            } else {
                bucket_hits as f64 / bucket_samples as f64
            },
        });
    }
    CalibrationReport {
        samples,
        success_hit_rate: hits as f64 / samples as f64,
        mean_calibration_error,
        mean_delta_jaccard,
        uncertainty_buckets,
    }
}
