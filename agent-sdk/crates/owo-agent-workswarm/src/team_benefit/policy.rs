//! 默认队策略（team-policy.json 形状 + gate 决策，从 team_benefit.rs 拆出）。

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 默认队策略（team-policy.json 形状 + gate 决策）
// ---------------------------------------------------------------------------

/// 任务组默认策略（`groups.<key>`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskGroupPolicy {
    /// 达标且绑定匹配时是否允许 auto 进入团队；false = 该组永不组队（除非用户显式）。
    #[serde(default)]
    pub allow_auto_team: bool,
    /// 该组达到「样本充分」所需的最小样本数（每组；为空用全局默认）。
    #[serde(default)]
    pub min_samples: Option<usize>,
    /// 强制独立评审：策略层不因省调用量裁剪评审角色。
    #[serde(default)]
    pub mandatory_review: bool,
    /// 备注（预选任务类型 / 判定口径说明；进 UI 与审计理由）。
    #[serde(default)]
    pub note: String,
}

/// 默认队策略（十期三路；可从 `evals/v1/team-policy.json` 加载）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamPolicy {
    pub schema_version: u32,
    /// 策略版本：收益记录必须绑定同一版本才可背书。
    pub strategy_version: String,
    /// 全局默认最小样本数（每组；组级 `min_samples` 覆盖它）。
    #[serde(default = "default_min_samples")]
    pub min_samples_default: usize,
    /// 收益记录最长有效期（天；到期视为过期，不再背书）。
    #[serde(default = "default_max_record_age_days")]
    pub max_record_age_days: u64,
    #[serde(default)]
    pub thresholds: BenefitThresholds,
    /// task_group → 策略（键与第二路报告的 `task_group` 对应）。
    #[serde(default)]
    pub groups: BTreeMap<String, TaskGroupPolicy>,
}

pub(super) fn default_min_samples() -> usize {
    30
}
fn default_max_record_age_days() -> u64 {
    30
}

impl Default for TeamPolicy {
    fn default() -> Self {
        Self {
            schema_version: BENEFIT_SCHEMA_VERSION,
            strategy_version: "ten-5-independent-case-samples".to_string(),
            min_samples_default: default_min_samples(),
            max_record_age_days: default_max_record_age_days(),
            thresholds: BenefitThresholds::default(),
            groups: BTreeMap::new(),
        }
    }
}

impl TeamPolicy {
    /// 从 JSON 文本解析（结构问题 → 报错；未知 schema_version 高版本 → 报错拒绝静默升级）。
    pub fn from_json(text: &str) -> Result<Self, String> {
        let policy: TeamPolicy =
            serde_json::from_str(text).map_err(|e| format!("team-policy.json 解析失败：{e}"))?;
        policy.validate()?;
        Ok(policy)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != BENEFIT_SCHEMA_VERSION {
            return Err(format!(
                "team-policy.json schema_version={} 不兼容（期望 {}）",
                self.schema_version, BENEFIT_SCHEMA_VERSION
            ));
        }
        if self.strategy_version.trim().is_empty() {
            return Err("strategy_version 不能为空".to_string());
        }
        let thresholds = &self.thresholds;
        if !thresholds.success_rate_pp.is_finite()
            || !(0.0..=100.0).contains(&thresholds.success_rate_pp)
        {
            return Err("success_rate_pp 必须在 0 到 100 之间".to_string());
        }
        if !thresholds.quality_pct.is_finite() || thresholds.quality_pct < 0.0 {
            return Err("quality_pct 必须是非负有限数".to_string());
        }
        if !thresholds.wall_rel_save.is_finite() || !(0.0..=1.0).contains(&thresholds.wall_rel_save)
        {
            return Err("wall_rel_save 必须在 0 到 1 之间".to_string());
        }
        let limits = [
            thresholds.max_token_ratio,
            thresholds.max_model_call_ratio,
            thresholds.max_tool_call_ratio,
            thresholds.max_cost_ratio,
        ];
        if limits
            .iter()
            .any(|limit| !limit.is_finite() || *limit < 1.0)
        {
            return Err("Team 资源增幅上限必须是有限数且不得低于 Single 基线（1.0）".to_string());
        }
        if self.min_samples_default == 0
            || self
                .groups
                .values()
                .any(|group| group.min_samples == Some(0))
        {
            return Err("默认及任务组 min_samples 必须至少为 1".to_string());
        }
        Ok(())
    }

    /// 默认策略（与 `evals/v1/team-policy.json` 语义等价的构造默认值；
    /// 测试与未提供配置文件时使用）。
    pub fn embedded_defaults() -> Self {
        let mut policy = Self::default();
        let mut push = |group: &str,
                        allow_auto_team: bool,
                        min_samples: Option<usize>,
                        mandatory_review: bool,
                        note: &str| {
            policy.groups.insert(
                group.to_string(),
                TaskGroupPolicy {
                    allow_auto_team,
                    min_samples,
                    mandatory_review,
                    note: note.to_string(),
                },
            );
        };
        push(
            "code",
            true,
            Some(30),
            false,
            "代码任务预选：达标且绑定匹配时允许 auto 组队（默认仍以无证据 single 起步）",
        );
        push(
            "research",
            true,
            Some(30),
            false,
            "研究任务预选：并行研究/材料合并可降耗时、提证据完整度；达标放行 auto",
        );
        push(
            "document",
            false,
            Some(30),
            false,
            "文档任务默认 single：单生产者收口即可，多数场景组队无额外收益",
        );
        policy
    }

    /// 组的生效最小样本数（组级 > 全局默认）。
    pub fn min_samples_for(&self, task_group: &str) -> usize {
        self.groups
            .get(task_group)
            .and_then(|g| g.min_samples)
            .unwrap_or(self.min_samples_default)
    }

    /// 组是否强制独立评审（策略层不因省调用量裁剪评审）。
    pub fn mandatory_review_for(&self, task_group: &str) -> bool {
        self.groups
            .get(task_group)
            .map(|g| g.mandatory_review)
            .unwrap_or(false)
    }

    /// RFC3339 时间是否已过期（空时间戳视为「未知」→ 按未过期处理但 gate 会以
    /// 「缺少时间戳」拒绝背书，避免无据放行）。
    pub fn record_expired(&self, generated_at: &str, now_rfc3339: &str) -> bool {
        let Ok(generated) = chrono::DateTime::parse_from_rfc3339(generated_at) else {
            return true;
        };
        let Ok(now) = chrono::DateTime::parse_from_rfc3339(now_rfc3339) else {
            return true;
        };
        let age = now.signed_duration_since(generated);
        age.num_days() > self.max_record_age_days as i64
    }
}

/// 策略 gate 输出：默认（auto）模式下是否允许组队 + 可展示理由。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyGate {
    /// true = auto 可进入团队；false = 默认 single。
    pub allow_team: bool,
    /// 逐条理由（UI/审计直接渲染）。
    pub reasons: Vec<String>,
    /// 组是否强制独立评审（不管是否组队，评审都不得裁剪）。
    pub mandatory_review: bool,
}

/// 对某个任务组做默认策略判定（auto 模式的 gate）。
///
/// 判定顺序（任一拒绝即 single）：
/// 1. 无对照数据（`pair=None`）→ 无证据，默认 single；
/// 2. 判定不达标（`eligible=false`）→ 收益不足，默认 single；
/// 3. 样本不足（widths）→ 不下结论，默认 single（保留方向性说明）；
/// 4. 记录过期 → 默认 single；
/// 5. 绑定与当前运行不一致 → 默认 single（旧配置成绩不背书）;
/// 6. 组未开放 `allow_auto_team` → 默认 single（非预选组）。
///
/// `current` 为当前运行的绑定（模型/模板/任务集/策略版本）；None = 未声明绑定
/// （凡是声明了绑定的记录均无法通过匹配 → 保守 single）。
pub fn gate_auto(
    policy: &TeamPolicy,
    task_group: &str,
    verdict: Option<&BenefitVerdict>,
    current: Option<&BenefitBindings>,
    now_rfc3339: &str,
) -> PolicyGate {
    let mut reasons = Vec::new();
    let mandatory_review = policy.mandatory_review_for(task_group);
    if let Err(error) = policy.validate() {
        reasons.push(format!("Team 策略配置无效：{error}；默认 single"));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }

    let Some(verdict) = verdict else {
        reasons.push(format!(
            "任务组「{task_group}」无收益证据记录：默认 single（未达标前不启用多 Agent）"
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    };

    if !verdict.eligible {
        reasons.push(format!(
            "任务组「{task_group}」综合收益未达标：主收益需至少一项成立，且成功率/质量/资源护栏全部通过；默认 single",
        ));
        for rule in verdict.rules.iter().chain(verdict.guardrails.iter()) {
            if !rule.satisfied {
                reasons.push(format!("  ⬜ {}", rule.detail));
            }
        }
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }

    let min_samples = policy.min_samples_for(task_group);
    let sample_sufficient = samples_sufficient(
        verdict.single_case_clusters,
        verdict.multi_case_clusters,
        min_samples,
    );
    if !sample_sufficient {
        reasons.push(format!(
            "任务组「{task_group}」达标但独立 case 样本不足（须 ≥{min_samples} 才有把握；当前 single={} multi={}，重复运行不增加独立样本）：默认 single，结论仅具方向性",
            verdict.single_case_clusters, verdict.multi_case_clusters
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }

    if policy.record_expired(&verdict.generated_at, now_rfc3339) {
        reasons.push(format!(
            "任务组「{task_group}」收益记录已过期（>{} 天）：默认 single",
            policy.max_record_age_days
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }

    // 先应用策略开放范围：未预选组永远不能自动组队，也不应被后续
    // 运行绑定检查遮蔽其真正的拒绝原因。
    if !policy
        .groups
        .get(task_group)
        .map(|g| g.allow_auto_team)
        .unwrap_or(false)
    {
        reasons.push(format!(
            "任务组「{task_group}」不是预选组（team-policy 未开放 allow_auto_team）：默认 single"
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }

    if !verdict.bindings.is_complete() {
        reasons.push(format!(
            "任务组「{task_group}」收益记录缺少完整模型/任务集/策略版本绑定：不背书，默认 single"
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }
    let Some(current) = current else {
        reasons.push(format!(
            "任务组「{task_group}」当前模型或任务集绑定上下文不可确认：不背书，默认 single"
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    };
    if !verdict.bindings_match(current) {
        reasons.push(format!(
            "任务组「{task_group}」收益记录绑定与当前配置不一致（model/template/task_set/strategy_version）：不背书，默认 single"
        ));
        return PolicyGate {
            allow_team: false,
            reasons,
            mandatory_review,
        };
    }

    reasons.push(format!(
        "任务组「{task_group}」收益达标且样本充分、记录有效、绑定匹配：允许 auto 进入团队"
    ));
    for rule in &verdict.rules {
        if rule.satisfied {
            reasons.push(format!("  ✅ {}", rule.detail));
        }
    }
    PolicyGate {
        allow_team: true,
        reasons,
        mandatory_review,
    }
}

/// 抽样「样本充分」判定（供报告方/测试复用：两组都要 ≥ min_samples）。
pub fn samples_sufficient(single_n: usize, multi_n: usize, min_samples: usize) -> bool {
    single_n >= min_samples && multi_n >= min_samples
}
