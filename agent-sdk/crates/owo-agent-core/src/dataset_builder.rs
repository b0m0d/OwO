//! Dataset Builder（T0，对应主开发技术文档 §5.12.3 数据清洗顺序）。
//!
//! 把 [`crate::transition::TransitionTraceV1`] 清洗为可版本化的训练数据集：
//!
//! ```text
//! 环境/任务版本有效 → 状态完整 → 动作目标在证据中 → 坐标位于目标框（若适用）
//! → Verifier 一致 → 去除敏感值或标记不可训练 → 去重/平衡成功失败
//! ```
//!
//! 清洗只能过滤样本，不能篡改原始轨迹；产物为 [`DatasetManifest`]（统计 +
//! 拒绝原因 + 样本清单 + 内容哈希），与原始轨迹分开保存。

use crate::transition::{TransitionOutcome, TransitionTraceV1};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

/// 拒绝原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// 环境版本不在允许集合 / 任务 id 缺失。
    BadEnvVersion,
    /// 状态引用或必需字段缺失。
    IncompleteState,
    /// GUI 动作声明了 target_id 但不在证据中。
    TargetNotInEvidence,
    /// 点击坐标不在目标框内。
    CoordOutsideBounds,
    /// 缺少 Verifier 结果。
    NoVerifier,
    /// Verifier 结果与终态不一致。
    VerifierInconsistent,
    /// 隐私域默认不可训练（S3/生产）且未显式放行。
    PrivacyExcluded,
    /// 与更早样本重复（同状态 + 同动作指纹）。
    Duplicate,
    /// 成功/失败平衡裁剪。
    BalanceTrimmed,
}

impl RejectReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadEnvVersion => "bad_env_version",
            Self::IncompleteState => "incomplete_state",
            Self::TargetNotInEvidence => "target_not_in_evidence",
            Self::CoordOutsideBounds => "coord_outside_bounds",
            Self::NoVerifier => "no_verifier",
            Self::VerifierInconsistent => "verifier_inconsistent",
            Self::PrivacyExcluded => "privacy_excluded",
            Self::Duplicate => "duplicate",
            Self::BalanceTrimmed => "balance_trimmed",
        }
    }
}

/// 单条拒绝记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    pub transition_id: String,
    pub reason: RejectReason,
    pub detail: String,
}

/// 清洗配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetBuilderConfig {
    /// 允许的环境版本；为空表示允许全部。
    #[serde(default)]
    pub allowed_env_versions: Vec<String>,
    /// 是否显式放行 S3/生产域样本（默认不放行，主文档 §14.7 默认策略）。
    #[serde(default)]
    pub allow_real_data: bool,
    /// 成功/失败多数类对少数类的最大倍数（默认 3.0）。
    #[serde(default = "default_balance_ratio")]
    pub balance_ratio: f64,
    /// 数据集 id（缺省自动生成）。
    #[serde(default)]
    pub dataset_id: Option<String>,
}

fn default_balance_ratio() -> f64 {
    3.0
}

impl Default for DatasetBuilderConfig {
    fn default() -> Self {
        Self {
            allowed_env_versions: Vec::new(),
            allow_real_data: false,
            balance_ratio: default_balance_ratio(),
            dataset_id: None,
        }
    }
}

/// 数据集清单（可落盘、可审计）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetManifest {
    pub dataset_id: String,
    pub created_at: String,
    pub env_versions: Vec<String>,
    pub input_count: usize,
    pub accepted_count: usize,
    pub success_count: usize,
    pub failure_count: usize,
    pub rejection_counts: BTreeMap<String, usize>,
    /// 拒绝明细（上限 200 条，防止清单膨胀）。
    pub rejections: Vec<Rejection>,
    /// 接受样本的 transition_id（写入顺序）。
    pub sample_ids: Vec<String>,
    /// sample_ids 的内容哈希（版本化锚点）。
    pub content_hash: String,
}

/// 构建结果。
#[derive(Debug, Clone)]
pub struct BuildResult {
    pub manifest: DatasetManifest,
    pub accepted: Vec<TransitionTraceV1>,
    pub rejections: Vec<Rejection>,
}

const REJECTION_DETAIL_CAP: usize = 200;

/// 单样本静态检查（返回拒绝原因；顺序即文档清洗顺序）。
fn evaluate_trace(
    trace: &TransitionTraceV1,
    config: &DatasetBuilderConfig,
) -> Result<(), Rejection> {
    // 1. 环境/任务版本有效。
    if trace.task_id.trim().is_empty() || trace.env_id.trim().is_empty() {
        return Err(Rejection {
            transition_id: trace.transition_id.clone(),
            reason: RejectReason::BadEnvVersion,
            detail: "task_id 或 env_id 为空".into(),
        });
    }
    if !config.allowed_env_versions.is_empty()
        && !config.allowed_env_versions.contains(&trace.env_version)
    {
        return Err(Rejection {
            transition_id: trace.transition_id.clone(),
            reason: RejectReason::BadEnvVersion,
            detail: format!("环境版本 {} 不在允许集合", trace.env_version),
        });
    }
    // 2. 状态完整。
    if trace.state_before_ref.trim().is_empty()
        || trace.state_after_ref.trim().is_empty()
        || trace.episode_id.trim().is_empty()
    {
        return Err(Rejection {
            transition_id: trace.transition_id.clone(),
            reason: RejectReason::IncompleteState,
            detail: "状态引用或 episode_id 缺失".into(),
        });
    }
    // 3. 动作目标在证据中（声明了 target_id 的 GUI 动作）。
    if matches!(trace.action.kind, crate::desktop_env::ActionKind::Gui) {
        if let Some(target) = &trace.action.target_id {
            if !target.is_empty()
                && !trace
                    .action
                    .target_evidence
                    .iter()
                    .any(|e| e == target || e.contains(target.as_str()))
            {
                return Err(Rejection {
                    transition_id: trace.transition_id.clone(),
                    reason: RejectReason::TargetNotInEvidence,
                    detail: format!("target {target} 不在证据列表"),
                });
            }
        }
        // 4. 坐标位于目标框（若适用）。
        if let (Some((px, py)), Some((bx, by, bw, bh))) =
            (trace.action.click_point, trace.action.target_bounds)
        {
            let inside = px >= bx && px < bx + bw && py >= by && py < by + bh;
            if !inside {
                return Err(Rejection {
                    transition_id: trace.transition_id.clone(),
                    reason: RejectReason::CoordOutsideBounds,
                    detail: format!("点击 ({px},{py}) 不在目标框 ({bx},{by},{bw},{bh}) 内"),
                });
            }
        }
    }
    // 5. Verifier 一致。
    if trace.verifier_results.is_empty() {
        return Err(Rejection {
            transition_id: trace.transition_id.clone(),
            reason: RejectReason::NoVerifier,
            detail: "缺少 Verifier 结果".into(),
        });
    }
    let all_pass = trace.verifier_results.iter().all(|v| v.passed);
    let any_fail = trace.verifier_results.iter().any(|v| !v.passed);
    let consistent = match trace.outcome {
        TransitionOutcome::Success => all_pass,
        TransitionOutcome::Failure | TransitionOutcome::Cancelled => any_fail,
    };
    if !consistent {
        return Err(Rejection {
            transition_id: trace.transition_id.clone(),
            reason: RejectReason::VerifierInconsistent,
            detail: format!("Verifier 结果与终态 {:?} 不一致", trace.outcome),
        });
    }
    // 6. 隐私/敏感域。
    if !trace.privacy_scope.trainable_by_default() && !config.allow_real_data {
        return Err(Rejection {
            transition_id: trace.transition_id.clone(),
            reason: RejectReason::PrivacyExcluded,
            detail: format!("隐私域 {:?} 默认不可训练", trace.privacy_scope),
        });
    }
    Ok(())
}

/// 构建数据集：过滤 → 去重 → 平衡 → 清单。
pub fn build_dataset(traces: &[TransitionTraceV1], config: &DatasetBuilderConfig) -> BuildResult {
    let mut accepted: Vec<TransitionTraceV1> = Vec::new();
    let mut rejections: Vec<Rejection> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for trace in traces {
        if let Err(rejection) = evaluate_trace(trace, config) {
            rejections.push(rejection);
            continue;
        }
        // 7a. 去重：同环境版本 + 同状态 + 同动作指纹 + 同终态。
        // 终态参与键：同一 (状态, 动作) 的成功/失败样本是不同训练信号，必须保留。
        let dedupe_key = format!(
            "{}|{}|{}|{:?}",
            trace.env_version,
            trace.state_before_ref,
            TransitionTraceV1::action_fingerprint(&trace.action),
            trace.outcome
        );
        if !seen.insert(dedupe_key) {
            rejections.push(Rejection {
                transition_id: trace.transition_id.clone(),
                reason: RejectReason::Duplicate,
                detail: "同状态同动作的更早样本已存在".into(),
            });
            continue;
        }
        accepted.push(trace.clone());
    }

    // 7b. 成功/失败平衡：多数类从尾部裁剪（确定性）。
    let success_total = accepted.iter().filter(|t| t.outcome.is_success()).count();
    let failure_total = accepted.len() - success_total;
    let ratio = config.balance_ratio.max(1.0);
    let mut trimmed: Vec<TransitionTraceV1> = Vec::new();
    if success_total > 0 && failure_total > 0 {
        let cap = |minority: usize| ((minority as f64) * ratio).ceil() as usize;
        let success_cap = cap(failure_total).max(failure_total.min(success_total));
        let failure_cap = cap(success_total).max(success_total.min(failure_total));
        let mut kept_success = 0usize;
        let mut kept_failure = 0usize;
        for trace in accepted {
            if trace.outcome.is_success() {
                if kept_success < success_cap.max(1) {
                    kept_success += 1;
                    trimmed.push(trace);
                } else {
                    rejections.push(Rejection {
                        transition_id: trace.transition_id.clone(),
                        reason: RejectReason::BalanceTrimmed,
                        detail: format!("成功样本超出 {ratio:.1}x 平衡上限"),
                    });
                }
            } else if kept_failure < failure_cap.max(1) {
                kept_failure += 1;
                trimmed.push(trace);
            } else {
                rejections.push(Rejection {
                    transition_id: trace.transition_id.clone(),
                    reason: RejectReason::BalanceTrimmed,
                    detail: format!("失败样本超出 {ratio:.1}x 平衡上限"),
                });
            }
        }
        accepted = trimmed;
    }

    let success_count = accepted.iter().filter(|t| t.outcome.is_success()).count();
    let failure_count = accepted.len() - success_count;
    let mut env_versions: Vec<String> = accepted.iter().map(|t| t.env_version.clone()).collect();
    env_versions.sort();
    env_versions.dedup();
    let mut rejection_counts: BTreeMap<String, usize> = BTreeMap::new();
    for rejection in &rejections {
        *rejection_counts
            .entry(rejection.reason.as_str().to_string())
            .or_insert(0) += 1;
    }
    let sample_ids: Vec<String> = accepted.iter().map(|t| t.transition_id.clone()).collect();
    let content_hash = {
        let mut hasher = Sha256::new();
        for id in &sample_ids {
            hasher.update(id.as_bytes());
            hasher.update(b"\n");
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let dataset_id = config
        .dataset_id
        .clone()
        .unwrap_or_else(|| format!("dataset-{}", uuid::Uuid::new_v4()));
    let manifest = DatasetManifest {
        dataset_id,
        created_at: chrono::Utc::now().to_rfc3339(),
        env_versions,
        input_count: traces.len(),
        accepted_count: accepted.len(),
        success_count,
        failure_count,
        rejection_counts,
        rejections: rejections
            .iter()
            .take(REJECTION_DETAIL_CAP)
            .cloned()
            .collect(),
        sample_ids,
        content_hash,
    };
    BuildResult {
        manifest,
        accepted,
        rejections,
    }
}

/// 清单落盘（JSON）。
pub fn save_manifest(path: impl AsRef<Path>, manifest: &DatasetManifest) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, json)
}

/// 读取清单。
pub fn load_manifest(path: impl AsRef<Path>) -> std::io::Result<DatasetManifest> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
