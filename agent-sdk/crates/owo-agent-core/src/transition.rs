//! Transition Trace（T0，对应主开发技术文档 §5.12.3）。
//!
//! `ExecutionTraceV1`（trace.rs）记录运行过程；本模块的 [`TransitionTraceV1`]
//! 专门服务模型训练：单步「状态—动作—下一状态」+ 预测对照 + Verifier 判分 +
//! 奖励分量 + 失败类别 + fork point。
//!
//! 组成：
//! - [`TransitionStore`]：JSONL 事件日志 + 按 `transition_id` 幂等的内存索引，
//!   崩溃重放恢复（损坏行跳过并计数），与 ExperienceStore 同一工程模式；
//! - [`align_fork_point`]：成功/失败轨迹对齐，定位最后共同状态与分叉动作，
//!   产出可训练的纠错样本骨架；
//! - [`record_transition_experience`]：把 transition 终态映射进
//!   [`crate::experience_store::ExperienceStore`]（`ExperienceKind::Transition`），
//!   同时保留既有 Goal/Worker Outcome 语义。
//!
//! 数据清洗与版本化在 [`crate::dataset_builder`]；本模块只负责记录与对齐。

use crate::desktop_env::{GroundedAction, RewardParts, StateDelta};
use crate::experience_store::{
    Attribution, ExperienceEvent, ExperienceKind, ExperienceStore, Outcome,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// 失败归因类别（主文档 §5.10.7 FailureAttribution）。
///
/// 低置信归因只能记为 [`FailureClass::UnknownNeedsReview`]，不得伪造确定原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// 锚点漂移：相同文本但 stable id / 几何改变。
    AnchorDrift,
    /// 时序竞争：等待超时的目标随后出现。
    TimingRace,
    /// 断言失配：动作生效但断言不通过。
    AssertionMismatch,
    /// 界面变化：元素整体缺失或结构变化。
    UiChanged,
    /// 权限拒绝：Policy 直接拒绝。
    PermissionDenied,
    /// 工具失败：执行器/工具自身错误。
    ToolFailure,
    /// 模型计划错误：动作选择本身错误。
    ModelPlanError,
    /// 未知，需要人工复核。
    UnknownNeedsReview,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnchorDrift => "anchor_drift",
            Self::TimingRace => "timing_race",
            Self::AssertionMismatch => "assertion_mismatch",
            Self::UiChanged => "ui_changed",
            Self::PermissionDenied => "permission_denied",
            Self::ToolFailure => "tool_failure",
            Self::ModelPlanError => "model_plan_error",
            Self::UnknownNeedsReview => "unknown_needs_review",
        }
    }
}

/// 单步转换的结果语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionOutcome {
    Success,
    Failure,
    Cancelled,
}

impl TransitionOutcome {
    pub fn is_success(self) -> bool {
        self == Self::Success
    }
}

/// 隐私/来源域：决定样本默认可否进入训练集（见 dataset_builder）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyScope {
    /// S1 可编程模拟环境（默认可训练）。
    S1Sim,
    /// S2 Windows VM（默认可训练）。
    S2Vm,
    /// S3 授权真实桌面（默认不可训练，需显式放行与脱敏）。
    S3Real,
    /// 工作区/桌面生产执行记录（默认不可训练）。
    Production,
}

impl PrivacyScope {
    /// 默认是否可进入训练集（主文档 §14.7 默认策略）。
    pub fn trainable_by_default(self) -> bool {
        matches!(self, Self::S1Sim | Self::S2Vm)
    }
}

/// Verifier 单条判分记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifierResult {
    pub verifier: String,
    pub assertion: String,
    pub passed: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl VerifierResult {
    /// 从环境判分结果构造（verifier 名称 + 每条断言证据）。
    pub fn from_verdict(
        verifier: impl Into<String>,
        verdict: &crate::desktop_env::Verdict,
    ) -> Vec<Self> {
        let verifier = verifier.into();
        match verdict {
            crate::desktop_env::Verdict::Pass { evidence } => evidence
                .iter()
                .map(|e| Self {
                    verifier: verifier.clone(),
                    assertion: e.clone(),
                    passed: true,
                    evidence: Vec::new(),
                })
                .collect(),
            crate::desktop_env::Verdict::Fail { reason, evidence } => {
                let mut results = vec![Self {
                    verifier: verifier.clone(),
                    assertion: reason.clone(),
                    passed: false,
                    evidence: evidence.clone(),
                }];
                for passed in evidence {
                    results.push(Self {
                        verifier: verifier.clone(),
                        assertion: passed.clone(),
                        passed: true,
                        evidence: Vec::new(),
                    });
                }
                results
            }
        }
    }
}

/// 世界模型对该步的预测快照（WM0 影子预测写入；无预测时为 None）。
/// 使用 WorldPredictionRef 而不是完整 WorldPrediction，避免 transition 模块
/// 反向依赖 world_model；完整预测对象由 world_model 侧另行留存。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredictionRef {
    pub model_id: String,
    pub model_version: String,
    pub success_probability: f32,
    pub uncertainty: f32,
    pub predicted_delta_fingerprint: String,
}

/// 面向模型训练的单步状态转换记录（§5.12.3 TransitionTraceV1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionTraceV1 {
    pub transition_id: String,
    pub episode_id: String,
    pub task_id: String,
    pub env_id: String,
    pub env_version: String,
    pub state_before_ref: String,
    pub action: GroundedAction,
    /// 影子预测（若启用）。
    #[serde(default)]
    pub predicted: Option<PredictionRef>,
    pub state_after_ref: String,
    pub observed_delta: StateDelta,
    #[serde(default)]
    pub verifier_results: Vec<VerifierResult>,
    pub reward_parts: RewardParts,
    pub outcome: TransitionOutcome,
    #[serde(default)]
    pub failure_class: Option<FailureClass>,
    /// 成功/失败分叉点状态引用（对齐后回填）。
    #[serde(default)]
    pub fork_point: Option<String>,
    pub policy_version: String,
    #[serde(default)]
    pub model_versions: Vec<String>,
    pub privacy_scope: PrivacyScope,
    pub created_at: String,
}

impl TransitionTraceV1 {
    /// 动作指纹（对齐与去重用；不含随机 action_id）。
    pub fn action_fingerprint(action: &GroundedAction) -> String {
        format!("{}|{}", action.signature(), action.arguments)
    }
}

// ---------------------------------------------------------------------------
// TransitionStore：JSONL 幂等存储
// ---------------------------------------------------------------------------

/// transition 事件的幂等存储（内存索引 + 可选 JSONL 日志）。
#[derive(Debug, Default)]
pub struct TransitionStore {
    path: Option<PathBuf>,
    events: HashMap<String, TransitionTraceV1>,
    order: Vec<String>,
    bad_lines: u64,
}

impl TransitionStore {
    /// 纯内存存储（测试/短生命周期）。
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// 从 JSONL 日志重放加载（不存在则为空；损坏行跳过并计数）。
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut store = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        if !path.exists() {
            return Ok(store);
        }
        let file = File::open(&path)?;
        for line in BufReader::new(file).lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    store.bad_lines += 1;
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TransitionTraceV1>(&line) {
                Ok(trace) => {
                    store.insert(trace);
                }
                Err(_) => store.bad_lines += 1,
            }
        }
        Ok(store)
    }

    fn insert(&mut self, trace: TransitionTraceV1) -> bool {
        if self.events.contains_key(&trace.transition_id) {
            return false;
        }
        self.order.push(trace.transition_id.clone());
        self.events.insert(trace.transition_id.clone(), trace);
        true
    }

    /// 幂等追加：同 `transition_id` 以首次写入为准；返回是否新写入。
    /// 配置了日志路径时同步追加 JSONL 行。
    pub fn append(&mut self, trace: TransitionTraceV1) -> Result<bool, String> {
        let inserted = {
            if !self.insert(trace.clone()) {
                return Ok(false);
            }
            true
        };
        if let Some(path) = &self.path {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("打开转换日志失败：{e}"))?;
            let line = serde_json::to_string(&trace).map_err(|e| format!("序列化失败：{e}"))?;
            writeln!(file, "{line}").map_err(|e| format!("写入转换日志失败：{e}"))?;
        }
        Ok(inserted)
    }

    pub fn get(&self, transition_id: &str) -> Option<&TransitionTraceV1> {
        self.events.get(transition_id)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn bad_lines(&self) -> u64 {
        self.bad_lines
    }

    /// 按写入顺序的轨迹视图。
    pub fn traces(&self) -> Vec<&TransitionTraceV1> {
        self.order
            .iter()
            .filter_map(|id| self.events.get(id))
            .collect()
    }

    /// 全部 episode（按首次出现顺序）。
    pub fn episodes(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for id in &self.order {
            if let Some(trace) = self.events.get(id) {
                if !seen.contains(&trace.episode_id) {
                    seen.push(trace.episode_id.clone());
                }
            }
        }
        seen
    }

    /// 单个 episode 的轨迹（按写入顺序）。
    pub fn by_episode(&self, episode_id: &str) -> Vec<&TransitionTraceV1> {
        self.traces()
            .into_iter()
            .filter(|t| t.episode_id == episode_id)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// ExperienceStore 接线
// ---------------------------------------------------------------------------

/// 把 transition 终态映射为经验事件（幂等键 = `transition:<id>`）。
///
/// worker 记为 `env:<env_id>`；attribution 携带任务/步骤定位与失败摘要，
/// 不携带状态取值（避免敏感内容入经验库）。
pub fn record_transition_experience(
    store: &ExperienceStore,
    trace: &TransitionTraceV1,
) -> Result<(), String> {
    let outcome = match trace.outcome {
        TransitionOutcome::Success => Outcome::Success,
        TransitionOutcome::Failure => Outcome::Failure,
        TransitionOutcome::Cancelled => Outcome::Cancelled,
    };
    let error = trace
        .failure_class
        .map(|c| c.as_str().to_string())
        .or_else(|| {
            trace
                .verifier_results
                .iter()
                .find(|v| !v.passed)
                .map(|v| v.assertion.clone())
        });
    store.record(
        ExperienceEvent {
            correlation_id: format!("transition:{}", trace.transition_id),
            worker: format!("env:{}", trace.env_id),
            kind: ExperienceKind::Transition,
            outcome,
            attribution: Attribution {
                goal_id: None,
                plan_id: Some(trace.episode_id.clone()),
                step_id: Some(trace.transition_id.clone()),
                input_keys: vec![
                    trace.action.kind_name().to_string(),
                    trace.action.signature(),
                    trace.env_version.clone(),
                ],
                error,
            },
            ts: trace.created_at.clone(),
        },
        false,
    )
}

// ---------------------------------------------------------------------------
// 成功/失败轨迹对齐（fork point）
// ---------------------------------------------------------------------------

/// 分叉类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkDivergence {
    /// 相同状态下两者选择了不同动作（决策分叉）。
    ActionChoice,
    /// 相同动作产生了不同状态（环境/故障分叉）。
    StateDivergence,
}

/// 成功/失败轨迹对齐结果（纠错样本骨架）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkAlignment {
    pub episode_success: String,
    pub episode_failure: String,
    /// 最后共同状态引用。
    pub fork_state_ref: String,
    pub good_action: GroundedAction,
    pub bad_action: GroundedAction,
    pub divergence: ForkDivergence,
    pub failure_class: Option<FailureClass>,
}

/// 对齐两条同任务轨迹，定位分叉点。
///
/// 规则：
/// 1. 优先找「相同状态下动作不同」的最早索引（决策分叉）；
/// 2. 否则找「动作相同但下一状态不同」的最早索引（环境分叉），
///    分叉状态取该步的 before 状态；
/// 3. 完全一致或无共同起点返回 None。
pub fn align_fork_point(
    success: &[TransitionTraceV1],
    failure: &[TransitionTraceV1],
) -> Option<ForkAlignment> {
    if success.is_empty() || failure.is_empty() {
        return None;
    }
    if success[0].state_before_ref != failure[0].state_before_ref {
        return None;
    }
    let n = success.len().min(failure.len());
    for i in 0..n {
        if success[i].state_before_ref == failure[i].state_before_ref
            && TransitionTraceV1::action_fingerprint(&success[i].action)
                != TransitionTraceV1::action_fingerprint(&failure[i].action)
        {
            return Some(ForkAlignment {
                episode_success: success[i].episode_id.clone(),
                episode_failure: failure[i].episode_id.clone(),
                fork_state_ref: success[i].state_before_ref.clone(),
                good_action: success[i].action.clone(),
                bad_action: failure[i].action.clone(),
                divergence: ForkDivergence::ActionChoice,
                failure_class: failure[i].failure_class,
            });
        }
    }
    for i in 1..n {
        if success[i].state_before_ref != failure[i].state_before_ref {
            return Some(ForkAlignment {
                episode_success: success[i].episode_id.clone(),
                episode_failure: failure[i].episode_id.clone(),
                fork_state_ref: success[i - 1].state_before_ref.clone(),
                good_action: success[i - 1].action.clone(),
                bad_action: failure[i - 1].action.clone(),
                divergence: ForkDivergence::StateDivergence,
                failure_class: failure[i].failure_class.or(failure[i - 1].failure_class),
            });
        }
    }
    None
}

/// 批量对齐：按 (task_id) 分组，把每个失败 episode 与同任务最近一个成功 episode 对齐，
/// 并把得到的 fork_state_ref 回填到失败轨迹的 `fork_point` 字段（返回回填数量）。
pub fn annotate_fork_points(traces: &mut [TransitionTraceV1]) -> usize {
    let mut annotated = 0usize;
    let task_ids: Vec<String> = {
        let mut ids: Vec<String> = traces.iter().map(|t| t.task_id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    };
    for task_id in task_ids {
        let success: Vec<TransitionTraceV1> = traces
            .iter()
            .filter(|t| t.task_id == task_id && t.outcome.is_success())
            .cloned()
            .collect();
        if success.is_empty() {
            continue;
        }
        let failure_episodes: Vec<String> = {
            let mut eps: Vec<String> = traces
                .iter()
                .filter(|t| t.task_id == task_id && !t.outcome.is_success())
                .map(|t| t.episode_id.clone())
                .collect();
            eps.sort();
            eps.dedup();
            eps
        };
        for episode in failure_episodes {
            let failure: Vec<TransitionTraceV1> = traces
                .iter()
                .filter(|t| t.task_id == task_id && t.episode_id == episode)
                .cloned()
                .collect();
            let Some(alignment) = align_fork_point(&success, &failure) else {
                continue;
            };
            for trace in traces.iter_mut() {
                if trace.task_id == task_id
                    && trace.episode_id == episode
                    && !trace.outcome.is_success()
                    && trace.fork_point.is_none()
                {
                    trace.fork_point = Some(alignment.fork_state_ref.clone());
                    annotated += 1;
                }
            }
        }
    }
    annotated
}
