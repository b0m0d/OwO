//! 多 Agent 收益判定与默认组队策略（十期 · 三路）。
//!
//! 目标：让「默认是否组队」由**证据**驱动，而不是由固定三角色链或拍脑袋决定。
//! 本模块消费第二路交付的**配对对照报告**（同一任务集上 single vs multi 的分组
//! 统计），按冻结门槛判定收益是否达标，并把判定结果绑定到
//! （model / template / task_set / strategy_version）四元组——旧模型/旧模板的成绩
//! 不得为新配置背书。
//!
//! 冻结门槛（十期三路，与 `evals/v1/team-policy.json` 默认值一致）：
//! - 成功率提高 ≥ **5 个百分点**（multi.success_rate − single.success_rate ≥ 0.05）；
//! - 或 盲评质量提高 ≥ **10%**（multi.quality ≥ single.quality × 1.10）；
//! - 或 墙钟耗时降低 ≥ **30%**（multi.mean_wall_ms ≤ single.mean_wall_ms × 0.70）。
//!
//! 任一主收益命中只是候选：成功率不退化、质量可比且不退化、token/模型调用/工具调用
//! 遥测完整且开销不超限后，才是 `eligible = true`。
//!
//! 保守性保证：
//! - **样本不足不下结论**：两组样本都 ≥ `min_samples` 才算 `sample_sufficient`，
//!   否则结论仅具方向性（`eligible` 仍如实计算，但 gate 拒绝基于它的 auto 放行）；
//! - **不确定性显式呈现**：成功率附 Wilson 95% 置信区间（小样本稳健）与样本数，
//!   `PolicyGate.reason` 逐条说明；
//! - **记录过期不背书**：`generated_at` 超过 `max_record_age_days` → gate 拒绝；
//! - **绑定不匹配不背书**：报告的 model/template/task_set/strategy_version 与当前
//!   运行不一致 → gate 拒绝（避免拿旧成绩为新配置开绿灯）；
//! - **无证据默认 single**：`allow_auto_team` 仅对「达标且绑定匹配且未过期」的
//!   预选任务组打开；其余一律 single；
//! - **强制独立评审不裁剪**：组策略 `mandatory_review` 强制保留评审角色（策略层
//!   不因省调用量裁剪评审；`team_strategy` 据此设置 `needs_independent_review`）。
//!
//! 设计边界：纯函数 + serde；零 I/O、零模型调用；`TeamPolicy` 可整体从
//! `evals/v1/team-policy.json` 加载（运行时路径），测试用内嵌默认值。

use serde::{Deserialize, Serialize};

/// 收益判定 schema 版本（报告/策略文件自述；后续演进时做显式迁移）。
pub const BENEFIT_SCHEMA_VERSION: u32 = 1;

/// 判定门槛（十期三路冻结值；可经 team-policy.json 覆盖）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BenefitThresholds {
    /// 成功率绝对提升门槛（百分点，如 5.0 = +5pp）。
    pub success_rate_pp: f64,
    /// 盲评质量相对提升门槛（百分比，如 10.0 = +10%）。
    pub quality_pct: f64,
    /// 墙钟耗时相对降低门槛（比率，如 0.30 = −30%）。
    pub wall_rel_save: f64,
    /// Team 的每任务 token 数最多为 Single 的倍数；默认 1.5（+50%）。
    #[serde(default = "default_max_token_ratio")]
    pub max_token_ratio: f64,
    /// 每任务模型请求最多为 Single 的倍数；默认 1.5（+50%）。
    #[serde(default = "default_max_model_call_ratio")]
    pub max_model_call_ratio: f64,
    /// 每任务工具调用最多为 Single 的倍数；默认 2.0（+100%）。
    #[serde(default = "default_max_tool_call_ratio")]
    pub max_tool_call_ratio: f64,
    /// 双侧都有成本估算时的最多倍数；默认 1.5（+50%）。
    #[serde(default = "default_max_cost_ratio")]
    pub max_cost_ratio: f64,
}

fn default_max_token_ratio() -> f64 {
    1.5
}
fn default_max_model_call_ratio() -> f64 {
    1.5
}
fn default_max_tool_call_ratio() -> f64 {
    2.0
}
fn default_max_cost_ratio() -> f64 {
    1.5
}

impl Default for BenefitThresholds {
    fn default() -> Self {
        Self {
            success_rate_pp: 5.0,
            quality_pct: 10.0,
            wall_rel_save: 0.30,
            max_token_ratio: default_max_token_ratio(),
            max_model_call_ratio: default_max_model_call_ratio(),
            max_tool_call_ratio: default_max_tool_call_ratio(),
            max_cost_ratio: default_max_cost_ratio(),
        }
    }
}

/// 单侧统计快照（serde 字段名对齐第二路配对报告/`ModeStats` 的 JSON 形状；
/// quality 为可选新增：盲评质量 0..1，旧报告缺省为 None → 质量门槛跳过并注明）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeStatSnapshot {
    pub mode: String,
    #[serde(default)]
    pub runs_total: usize,
    /// Number of independent task cases; repeated runs of one case count once.
    /// Missing in legacy reports, so auto-Team admission fails closed.
    #[serde(default)]
    pub independent_case_clusters: usize,
    #[serde(default)]
    pub passed: usize,
    /// 0..1。
    #[serde(default)]
    pub success_rate: f64,
    #[serde(default)]
    pub ci95_low: f64,
    #[serde(default)]
    pub ci95_high: f64,
    #[serde(default)]
    pub mean_wall_ms: f64,
    #[serde(default)]
    pub mean_model_calls: Option<f64>,
    /// 每次运行的平均工具调用；缺失视为遥测不完整，不自动放行 Team。
    #[serde(default)]
    pub mean_tool_calls: Option<f64>,
    /// 该组累计 token；需除以 runs_total 后比较每任务开销。
    #[serde(default)]
    pub total_tokens: Option<u64>,
    /// 双侧均有价格配置时的累计估算成本。
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    /// 报告方的提示位；gate 不信任此值，并按 independent_case_clusters 重算。
    #[serde(default)]
    pub sample_sufficient: bool,
    /// 盲评质量（0..1；缺失 = 无质量维度，质量门槛判定为「不满足且说明」）。
    #[serde(default)]
    pub quality: Option<f64>,
}

impl ModeStatSnapshot {
    fn clamp_rate(&mut self) {
        if self.runs_total > 0 && (self.success_rate < 0.0 || self.success_rate > 1.0) {
            self.success_rate = self.success_rate.clamp(0.0, 1.0);
        }
    }
}

/// 收益记录绑定（四元组）：model / template / task_set / strategy_version。
/// 绑定的任意字段与当前运行不一致 → 记录不可用于当前配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenefitBindings {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub task_set: Option<String>,
    #[serde(default)]
    pub strategy_version: String,
}

impl BenefitBindings {
    /// 自动准入所需的证据范围：模型、任务集和策略版本必填，模板可显式为空。
    pub fn is_complete(&self) -> bool {
        let non_empty = |value: &Option<String>| {
            value
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        };
        non_empty(&self.model)
            && non_empty(&self.task_set)
            && !self.strategy_version.trim().is_empty()
            && self
                .template
                .as_ref()
                .is_none_or(|value| !value.trim().is_empty())
    }

    /// 缺失字段不是通配符，防止旧报告或不完整报告跨配置复用。
    pub fn matches(&self, current: &BenefitBindings) -> bool {
        self.is_complete()
            && current.is_complete()
            && self.model == current.model
            && self.template == current.template
            && self.task_set == current.task_set
            && self.strategy_version == current.strategy_version
    }
    /// 是否完全没有声明绑定。
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.template.is_none()
            && self.task_set.is_none()
            && self.strategy_version.is_empty()
    }
}

/// 一个任务组的配对对照输入（第二路报告的原子单元）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedStats {
    /// 任务组（code / research / document / 具体 case_id 均可；与 team-policy.json 的
    /// `groups` 键对应，匹配精确键或命中任意前缀）。
    pub task_group: String,
    pub single: ModeStatSnapshot,
    pub multi: ModeStatSnapshot,
    /// 本次对照的绑定（必须与当前运行匹配才可背书）。
    #[serde(default)]
    pub bindings: BenefitBindings,
    /// 报告生成时间（RFC3339；过期判定用）。
    #[serde(default)]
    pub generated_at: String,
    /// 该组判定「样本充分」的最小样本（None = 使用默认 30；仅影响本判定单元）。
    #[serde(default)]
    pub min_samples: Option<usize>,
    /// 默认最小样本（无 `min_samples` 时使用；与 team-policy 全局默认对齐）。
    #[serde(default = "default_min_samples")]
    pub default_min_samples: usize,
}

/// 单条门槛判定结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenefitRule {
    pub name: String,
    pub satisfied: bool,
    pub detail: String,
}

/// 收益判定结论（可整体序列化进 UI/报告/策略记录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenefitVerdict {
    pub task_group: String,
    /// 主收益至少一项命中且全部质量/资源护栏通过。
    pub eligible: bool,
    /// 主收益之外的成功率、质量与资源硬护栏。
    #[serde(default)]
    pub guardrails: Vec<BenefitRule>,
    /// 两侧独立 case cluster 都 ≥ min_samples（不达标时结论仅具方向性）。
    pub sample_sufficient: bool,
    /// 运行数（展示用）；重复运行不作为独立样本，gate 使用下面的 case cluster 数。
    #[serde(default)]
    pub single_n: usize,
    #[serde(default)]
    pub multi_n: usize,
    /// Independent case counts used by the auto-admission sample gate.
    #[serde(default)]
    pub single_case_clusters: usize,
    #[serde(default)]
    pub multi_case_clusters: usize,
    pub rules: Vec<BenefitRule>,
    pub bindings: BenefitBindings,
    pub generated_at: String,
}

impl BenefitVerdict {
    /// 绑定是否与当前运行一致（不一致 → 禁止背书给新配置）。
    pub fn bindings_match(&self, current: &BenefitBindings) -> bool {
        self.bindings.matches(current)
    }
}

/// Wilson score 95% 置信区间（小样本稳健；与 product_eval 同一公式）。
/// 返回 (low, high)；无样本时 (0,0)。
pub fn wilson_ci95(passed: usize, total: usize) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    let z = 1.96_f64;
    let n = total as f64;
    let p = passed as f64 / n;
    let denom = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denom;
    let margin = z * ((p * (1.0 - p) + z * z / (4.0 * n)) / n).sqrt() / denom;
    ((center - margin).max(0.0), (center + margin).min(1.0))
}

/// 补全快照的 CI（报告方未填时按样本自行计算；仅用于展示，不改变判定门槛）。
pub fn fill_ci(snapshot: &ModeStatSnapshot) -> ModeStatSnapshot {
    let mut out = snapshot.clone();
    if out.ci95_low == 0.0 && out.ci95_high == 0.0 && out.runs_total > 0 {
        let (low, high) = wilson_ci95(out.passed, out.runs_total);
        out.ci95_low = low;
        out.ci95_high = high;
    }
    out
}

/// 供策略与产品评测共同消费的单拓扑资源指标；数值统一为每个运行样本的均值。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResourceEnvelope {
    pub mean_model_calls: Option<f64>,
    pub mean_tool_calls: Option<f64>,
    pub mean_tokens: Option<f64>,
    pub mean_cost_usd: Option<f64>,
}

fn ratio_guardrail(
    name: &str,
    baseline: Option<f64>,
    candidate: Option<f64>,
    max_ratio: f64,
) -> BenefitRule {
    let (satisfied, detail) = match (baseline, candidate) {
        (Some(base), Some(value))
            if base.is_finite() && value.is_finite() && base >= 0.0 && value >= 0.0 =>
        {
            let ratio = if base == 0.0 {
                if value == 0.0 {
                    1.0
                } else {
                    f64::INFINITY
                }
            } else {
                value / base
            };
            (ratio <= max_ratio, format!("{name}: Single={base:.2}, Team={value:.2}, 倍率={ratio:.2}，上限={max_ratio:.2}"))
        }
        _ => (
            false,
            format!("{name}: Single 或 Team 用量缺失/无效，无法确认资源增幅；默认不自动放行"),
        ),
    };
    BenefitRule {
        name: name.to_string(),
        satisfied,
        detail,
    }
}

/// 共享的 Team 综合资源护栏。Token、模型请求、工具调用必须双侧可计量；成本
/// 估算有价格配置时再作为额外硬护栏，以免无价格的本地/兼容端点被伪报为零成本。
pub fn compare_resource_envelopes(
    single: &ResourceEnvelope,
    team: &ResourceEnvelope,
    thresholds: &BenefitThresholds,
) -> Vec<BenefitRule> {
    let mut checks = vec![
        ratio_guardrail(
            "token_overhead",
            single.mean_tokens,
            team.mean_tokens,
            thresholds.max_token_ratio,
        ),
        ratio_guardrail(
            "model_call_overhead",
            single.mean_model_calls,
            team.mean_model_calls,
            thresholds.max_model_call_ratio,
        ),
        ratio_guardrail(
            "tool_call_overhead",
            single.mean_tool_calls,
            team.mean_tool_calls,
            thresholds.max_tool_call_ratio,
        ),
    ];
    if single.mean_cost_usd.is_some() && team.mean_cost_usd.is_some() {
        checks.push(ratio_guardrail(
            "cost_overhead",
            single.mean_cost_usd,
            team.mean_cost_usd,
            thresholds.max_cost_ratio,
        ));
    }
    checks
}

fn envelope_from_mode(mode: &ModeStatSnapshot) -> ResourceEnvelope {
    let per_run = |total: Option<u64>| {
        (mode.runs_total > 0)
            .then_some(total?)
            .map(|value| value as f64 / mode.runs_total as f64)
    };
    ResourceEnvelope {
        mean_model_calls: mode.mean_model_calls,
        mean_tool_calls: mode.mean_tool_calls,
        mean_tokens: per_run(mode.total_tokens),
        mean_cost_usd: mode
            .total_cost_usd
            .filter(|_| mode.runs_total > 0)
            .map(|value| value / mode.runs_total as f64),
    }
}

/// 对一份配对对照做收益判定（纯函数）；样本充分按独立 case cluster 计算。
pub fn evaluate(pair: &PairedStats, thresholds: &BenefitThresholds) -> BenefitVerdict {
    let mut single = pair.single.clone();
    let mut multi = pair.multi.clone();
    single.clamp_rate();
    multi.clamp_rate();

    let mut rules = Vec::new();

    // 规则 1：成功率 +5pp。
    let rate_delta_pp = (multi.success_rate - single.success_rate) * 100.0;
    rules.push(BenefitRule {
        name: "success_rate_pp".to_string(),
        satisfied: rate_delta_pp >= thresholds.success_rate_pp,
        detail: format!(
            "success_rate {:.1}% → {:.1}%（+{:.1}pp；门槛 +{:.1}pp；n_single={} n_multi={}，Wilson95% [{:.1}%, {:.1}%] / [{:.1}%, {:.1}%]）",
            single.success_rate * 100.0,
            multi.success_rate * 100.0,
            rate_delta_pp,
            thresholds.success_rate_pp,
            single.runs_total,
            multi.runs_total,
            single.ci95_low * 100.0,
            single.ci95_high * 100.0,
            multi.ci95_low * 100.0,
            multi.ci95_high * 100.0,
        ),
    });

    // 规则 2：盲评质量 +10%（两侧都有质量分才判定；缺侧 = 不满足并注明）。
    let quality_satisfied = matches!((single.quality, multi.quality),
        (Some(s), Some(m)) if s > 0.0 && m >= s * (1.0 + thresholds.quality_pct / 100.0)
    );
    let quality_detail = match (single.quality, multi.quality) {
        (Some(s), Some(m)) => format!(
            "quality {:.2} → {:.2}（相对 +{:.1}%；门槛 +{:.1}%；n={}/{}）",
            s,
            m,
            (m / s - 1.0) * 100.0,
            thresholds.quality_pct,
            single.runs_total,
            multi.runs_total
        ),
        (Some(_), None) => "multi 无盲评质量（quality=None）：质量门槛不作数".to_string(),
        (None, Some(_)) => "single 无盲评质量（quality=None）：质量门槛不作数".to_string(),
        (None, None) => "双方均无盲评质量：质量门槛不作数".to_string(),
    };
    rules.push(BenefitRule {
        name: "quality_pct".to_string(),
        satisfied: quality_satisfied,
        detail: quality_detail,
    });

    // 规则 3：墙钟耗时 −30%（single 侧需有耗时）。
    let wall_satisfied = single.mean_wall_ms > 0.0
        && multi.mean_wall_ms <= single.mean_wall_ms * (1.0 - thresholds.wall_rel_save);
    rules.push(BenefitRule {
        name: "wall_rel_save".to_string(),
        satisfied: wall_satisfied,
        detail: format!(
            "mean_wall {:.0}ms → {:.0}ms（相对 {:.1}%；门槛 −{:.0}%；n={}/{}）",
            single.mean_wall_ms,
            multi.mean_wall_ms,
            if single.mean_wall_ms > 0.0 {
                (multi.mean_wall_ms / single.mean_wall_ms - 1.0) * 100.0
            } else {
                f64::NAN
            },
            thresholds.wall_rel_save * 100.0,
            single.runs_total,
            multi.runs_total
        ),
    });

    let mut guardrails = vec![BenefitRule {
        name: "success_rate_non_regression".to_string(),
        satisfied: single.success_rate.is_finite()
            && multi.success_rate.is_finite()
            && multi.success_rate >= single.success_rate,
        detail: format!(
            "交付成功率不得退化：Single={:.1}%，Team={:.1}%",
            single.success_rate * 100.0,
            multi.success_rate * 100.0,
        ),
    }];
    match (single.quality, multi.quality) {
        (Some(base), Some(candidate)) => guardrails.push(BenefitRule {
            name: "checker_quality_non_regression".to_string(),
            satisfied: base.is_finite() && candidate.is_finite() && candidate >= base,
            detail: format!("检查器质量不得退化：Single={base:.3}，Team={candidate:.3}"),
        }),
        (None, None) => guardrails.push(BenefitRule {
            name: "checker_quality_non_regression".to_string(),
            satisfied: true,
            detail:
                "双方均无独立检查器质量分；仅以完整交付成功率作为质量下限，不能据此声称质量提升"
                    .to_string(),
        }),
        _ => guardrails.push(BenefitRule {
            name: "checker_quality_non_regression".to_string(),
            satisfied: false,
            detail: "Single/Team 检查器质量覆盖不对称，质量不可比；默认不自动放行".to_string(),
        }),
    }
    guardrails.extend(compare_resource_envelopes(
        &envelope_from_mode(&single),
        &envelope_from_mode(&multi),
        thresholds,
    ));
    let primary_gain = rules.iter().any(|rule| rule.satisfied);
    let guardrails_pass = guardrails.iter().all(|rule| rule.satisfied);
    let eligible = primary_gain && guardrails_pass;
    let single_n = pair.single.runs_total;
    let multi_n = pair.multi.runs_total;
    let single_case_clusters = pair.single.independent_case_clusters;
    let multi_case_clusters = pair.multi.independent_case_clusters;
    BenefitVerdict {
        task_group: pair.task_group.clone(),
        eligible,
        guardrails,
        sample_sufficient: samples_sufficient(
            single_case_clusters,
            multi_case_clusters,
            pair.min_samples.unwrap_or(pair.default_min_samples),
        ),
        single_n,
        multi_n,
        single_case_clusters,
        multi_case_clusters,
        rules,
        bindings: pair.bindings.clone(),
        generated_at: pair.generated_at.clone(),
    }
}

mod policy;

use policy::default_min_samples;
pub use policy::{gate_auto, samples_sufficient, PolicyGate, TaskGroupPolicy, TeamPolicy};

// ---------------------------------------------------------------------------
// 配对对照报告消费层（第二路 `product_eval::build_paired_report_json` 交付形状）
// ---------------------------------------------------------------------------

/// 配对对照报告（第二路交付第三路的读取契约）。
///
/// 只声明本路消费的字段（`schema_version` / `generated_at` / `bindings` / `pairs`）；
/// 报告中的 `single_report` / `multi_report` 等追溯字段由 serde 忽略。字段名与
/// `product_eval::PAIRED_REPORT_SCHEMA_VERSION`（当前 1）及 `paired_snapshot_json`
/// 的 JSON 形状逐字对齐。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedReport {
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: String,
    /// 报告级绑定（每组 pair 亦各自携带绑定；判定以 pair 自身绑定为准）。
    #[serde(default)]
    pub bindings: BenefitBindings,
    /// Added by product-eval to prevent unaligned reports from enabling Team.
    /// Missing alignment is treated as legacy/unverified and fails closed.
    #[serde(default)]
    pub run_alignment: Option<PairedRunAlignment>,
    #[serde(default)]
    pub pairs: Vec<PairedStats>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PairedRunAlignment {
    #[serde(default)]
    pub configuration_aligned: bool,
    #[serde(default)]
    pub reasons: Vec<String>,
}

impl PairedReport {
    /// 与 `product_eval::PAIRED_REPORT_SCHEMA_VERSION` 对齐（当前 1）。
    pub const SCHEMA_VERSION: u32 = 1;

    /// 从 JSON 文本解析（schema 高版本 → 报错，拒绝静默升级）。
    pub fn from_json(text: &str) -> Result<Self, String> {
        let report: PairedReport =
            serde_json::from_str(text).map_err(|e| format!("配对对照报告解析失败：{e}"))?;
        if report.schema_version != Self::SCHEMA_VERSION {
            return Err(format!(
                "配对对照报告 schema_version={} 不兼容（期望 {}）",
                report.schema_version,
                Self::SCHEMA_VERSION
            ));
        }
        Ok(report)
    }

    /// 从 UTF-8 文件读取。
    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取配对对照报告失败（{}）：{e}", path.display()))?;
        Self::from_json(&text)
    }

    /// 报告任务组 → 策略组键：精确键优先，其次最长前缀命中（v1 case_id 以类别名
    /// 开头，如 `code-bug-fix` → `code`、`research-source-map` → `research`）；
    /// `overall` 与未命中 → `None`（不参与收益判定，保守默认 single）。
    pub fn policy_group_for(&self, policy: &TeamPolicy, task_group: &str) -> Option<String> {
        if policy.groups.contains_key(task_group) {
            return Some(task_group.to_string());
        }
        policy
            .groups
            .keys()
            .filter(|key| task_group.starts_with(key.as_str()))
            .cloned()
            .max_by_key(|key| key.chars().count())
    }
}

/// 单任务组的判定 + gate 汇总（可整体序列化进收益报告 / UI）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupBenefitReport {
    /// 报告里的原始任务组标签（overall / code / 具体 case_id）。
    pub task_group: String,
    /// 解析出的策略组键（`None` = overall/未预选 → 默认 single）。
    pub policy_group: Option<String>,
    pub verdict: BenefitVerdict,
    pub gate: PolicyGate,
}

/// 全量判定：对报告每组做 [`evaluate`] + [`gate_auto`]（纯函数，零 I/O）。
///
/// - 组键经 [`PairedReport::policy_group_for`] 解析后传给 gate（allow_auto_team /
///   min_samples / mandatory_review 按策略组生效）；
/// - `current` 为当前运行的绑定上下文（`None` = 未提供——凡声明了绑定的记录
///   一律不背书，见 [`gate_auto`]）；`now_rfc3339` 用于记录过期判定。
pub fn evaluate_report(
    report: &PairedReport,
    policy: &TeamPolicy,
    current: Option<&BenefitBindings>,
    now_rfc3339: &str,
) -> Vec<GroupBenefitReport> {
    let run_alignment_ok = report
        .run_alignment
        .as_ref()
        .is_some_and(|alignment| alignment.configuration_aligned);
    report
        .pairs
        .iter()
        .map(|pair| {
            let policy_group = report.policy_group_for(policy, &pair.task_group);
            let verdict = evaluate(pair, &policy.thresholds);
            let gate = match (&policy_group, run_alignment_ok) {
                (Some(_), false) => PolicyGate {
                    allow_team: false,
                    reasons: vec![
                        "配对报告缺少有效 run_alignment；该数据不能作为 Team 收益放行依据".to_string(),
                    ],
                    mandatory_review: false,
                },
                (Some(group), true) => {
                    gate_auto(policy, group, Some(&verdict), current, now_rfc3339)
                }
                (None, _) => PolicyGate {
                    allow_team: false,
                    reasons: vec![format!(
                        "任务组「{}」不在 team-policy 预选组内（overall/未知组不参与收益判定）：默认 single",
                        pair.task_group
                    )],
                    mandatory_review: false,
                },
            };
            GroupBenefitReport {
                task_group: pair.task_group.clone(),
                policy_group,
                verdict,
                gate,
            }
        })
        .collect()
}

/// 人类可读摘要：样本数与不确定性显式呈现；无任何组达标 → 明确输出
/// 「多 Agent 收益门槛未通过」（不得以自动回退功能代替收益验收）。
pub fn format_benefit_report(items: &[GroupBenefitReport]) -> String {
    let mut out = String::from(
        "—— 多 Agent 综合收益判定（成功率/盲评质量/墙钟至少一项达标；成功率不退化，token/模型/工具用量在硬上限内）——\n",
    );
    for item in items {
        let mark = if item.gate.allow_team { "✅" } else { "⬜" };
        out.push_str(&format!(
            "  {} {}（策略组 {}）样本 single={} multi={}：{}\n",
            mark,
            item.task_group,
            item.policy_group.as_deref().unwrap_or("—"),
            item.verdict.single_n,
            item.verdict.multi_n,
            if item.gate.allow_team {
                "允许 auto 组队"
            } else {
                "默认 single"
            }
        ));
        for reason in &item.gate.reasons {
            out.push_str(&format!("    {reason}\n"));
        }
        if !item.verdict.sample_sufficient {
            out.push_str("    ⚠️ 样本不足：结论仅具方向性，不作为放行依据\n");
        }
    }
    let any = items.iter().any(|item| item.gate.allow_team);
    out.push_str(if any {
        "  结论：存在达标预选任务组——auto 可对达标组进入 team（其余组仍默认 single）\n"
    } else {
        "  结论：多 Agent 收益门槛未通过——所有任务组默认 single（收益验收不通过，不得以自动回退功能代替）\n"
    });
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(group: &str, s: ModeStatSnapshot, m: ModeStatSnapshot) -> PairedStats {
        PairedStats {
            task_group: group.to_string(),
            single: s,
            multi: m,
            bindings: BenefitBindings::default(),
            generated_at: "2026-08-31T00:00:00+00:00".to_string(),
            min_samples: None,
            default_min_samples: 30,
        }
    }

    fn snap(runs: usize, passed: usize, wall: f64) -> ModeStatSnapshot {
        ModeStatSnapshot {
            mode: "single".to_string(),
            runs_total: runs,
            independent_case_clusters: runs,
            passed,
            success_rate: if runs > 0 {
                passed as f64 / runs as f64
            } else {
                0.0
            },
            ci95_low: 0.0,
            ci95_high: 0.0,
            mean_wall_ms: wall,
            mean_model_calls: Some(3.0),
            mean_tool_calls: Some(8.0),
            total_tokens: Some((runs as u64) * 1000),
            total_cost_usd: None,
            sample_sufficient: runs >= 30,
            quality: None,
        }
    }

    #[test]
    fn resource_envelope_treats_missing_cost_as_unavailable_and_averages_present_cost() {
        let mut snapshot = snap(3, 3, 100.0);
        let missing = envelope_from_mode(&snapshot);
        assert_eq!(missing.mean_cost_usd, None);
        assert_eq!(missing.mean_tokens, Some(1000.0));

        snapshot.total_cost_usd = Some(4.5);
        assert_eq!(envelope_from_mode(&snapshot).mean_cost_usd, Some(1.5));

        snapshot.runs_total = 0;
        assert_eq!(envelope_from_mode(&snapshot).mean_cost_usd, None);
        assert_eq!(envelope_from_mode(&snapshot).mean_tokens, None);
    }

    #[test]
    fn repeated_runs_of_one_case_do_not_open_auto_team_gate() {
        let mut single = snap(30, 30, 1000.0);
        let mut multi = snap(30, 30, 600.0);
        single.independent_case_clusters = 1;
        multi.independent_case_clusters = 2;
        let p = pair("code", single, multi);
        let verdict = evaluate(&p, &BenefitThresholds::default());
        assert!(
            verdict.eligible,
            "point estimate should remain directionally eligible"
        );
        assert!(!verdict.sample_sufficient);
        assert_eq!(verdict.single_n, 30);
        assert_eq!(verdict.single_case_clusters, 1);

        let policy = TeamPolicy::embedded_defaults();
        let gate = gate_auto(
            &policy,
            "code",
            Some(&verdict),
            None,
            "2026-10-05T00:00:00+00:00",
        );
        assert!(!gate.allow_team);
        assert!(gate
            .reasons
            .iter()
            .any(|reason| reason.contains("独立 case 样本不足")));
    }

    #[test]
    fn success_rate_gate_hits_5pp() {
        let p = pair(
            "code",
            snap(30, 24, 1000.0), // 80%
            snap(30, 27, 900.0),  // 90% → +10pp
        );
        let v = evaluate(&p, &BenefitThresholds::default());
        assert!(v.eligible, "v = {v:?}");
        assert!(v.sample_sufficient);
        let rate_rule = v
            .rules
            .iter()
            .find(|r| r.name == "success_rate_pp")
            .unwrap();
        assert!(rate_rule.satisfied, "{rate_rule:?}");
    }

    #[test]
    fn success_rate_below_threshold_not_eligible() {
        let p = pair(
            "code",
            snap(30, 24, 1000.0), // 80%
            snap(30, 25, 900.0),  // 83.3% → +3.3pp
        );
        let v = evaluate(&p, &BenefitThresholds::default());
        assert!(!v.eligible);
    }

    #[test]
    fn quality_gate_hits_10pct() {
        let mut s = snap(30, 20, 1000.0);
        s.quality = Some(0.60);
        let mut m = snap(30, 20, 900.0);
        m.quality = Some(0.70); // +16.7%
        let p = pair("research", s, m);
        let v = evaluate(&p, &BenefitThresholds::default());
        assert!(v.eligible);
        let q = v.rules.iter().find(|r| r.name == "quality_pct").unwrap();
        assert!(q.satisfied, "{q:?}");
    }

    #[test]
    fn quality_missing_one_side_not_eligible_by_quality() {
        let mut s = snap(30, 20, 1000.0);
        s.quality = Some(0.60);
        let m = snap(30, 20, 900.0); // quality None
        let p = pair("document", s, m);
        let v = evaluate(&p, &BenefitThresholds::default());
        let q = v.rules.iter().find(|r| r.name == "quality_pct").unwrap();
        assert!(!q.satisfied);
        assert!(!v.eligible, "成功率/耗时都没有提升 → 不达标");
    }

    #[test]
    fn wall_clock_gate_hits_30pct() {
        let p = pair(
            "research",
            snap(30, 20, 1000.0),
            snap(30, 20, 690.0), // −31%
        );
        let v = evaluate(&p, &BenefitThresholds::default());
        assert!(v.eligible);
        let w = v.rules.iter().find(|r| r.name == "wall_rel_save").unwrap();
        assert!(w.satisfied);
    }

    #[test]
    fn wall_clock_near_miss_not_eligible() {
        let p = pair(
            "research",
            snap(30, 20, 1000.0),
            snap(30, 20, 720.0), // −28% < 30%
        );
        let v = evaluate(&p, &BenefitThresholds::default());
        assert!(!v.eligible);
    }

    #[test]
    fn speed_gain_cannot_offset_excessive_tokens_or_missing_usage() {
        let mut pair = pair("code", snap(30, 24, 1000.0), snap(30, 24, 600.0));
        pair.multi.total_tokens = Some(90_000); // 3x Single: hard ceiling is 1.5x.
        let verdict = evaluate(&pair, &BenefitThresholds::default());
        assert!(verdict
            .rules
            .iter()
            .any(|rule| rule.name == "wall_rel_save" && rule.satisfied));
        assert!(!verdict.eligible);
        assert!(verdict
            .guardrails
            .iter()
            .any(|rule| rule.name == "token_overhead" && !rule.satisfied));

        pair.multi.total_tokens = None;
        let missing = evaluate(&pair, &BenefitThresholds::default());
        assert!(
            !missing.eligible,
            "missing usage must fail closed for auto-team"
        );
        assert!(missing
            .guardrails
            .iter()
            .any(|rule| rule.name == "token_overhead" && !rule.satisfied));
    }

    #[test]
    fn speed_gain_cannot_offset_success_or_checker_quality_regression() {
        let mut pair = pair("code", snap(30, 28, 1000.0), snap(30, 27, 500.0));
        pair.single.quality = Some(0.9);
        pair.multi.quality = Some(0.8);
        let verdict = evaluate(&pair, &BenefitThresholds::default());
        assert!(!verdict.eligible);
        assert!(verdict
            .guardrails
            .iter()
            .any(|rule| rule.name == "success_rate_non_regression" && !rule.satisfied));
        assert!(verdict
            .guardrails
            .iter()
            .any(|rule| rule.name == "checker_quality_non_regression" && !rule.satisfied));
    }

    #[test]
    fn configured_resource_limit_must_be_finite_and_at_least_baseline() {
        let mut policy = TeamPolicy::embedded_defaults();
        policy.thresholds.max_token_ratio = 0.8;
        let mut value = serde_json::to_value(policy).unwrap();
        value["thresholds"]["max_token_ratio"] = serde_json::json!(0.8);
        assert!(TeamPolicy::from_json(&value.to_string()).is_err());
    }

    #[test]
    fn incomplete_or_mismatched_evidence_bindings_never_back_auto_team() {
        let complete = BenefitBindings {
            model: Some("model-a".to_string()),
            template: None,
            task_set: Some("suite-a".to_string()),
            strategy_version: "policy-a".to_string(),
        };
        assert!(complete.matches(&complete));
        let incomplete_model = BenefitBindings {
            model: None,
            ..complete.clone()
        };
        let incomplete_task_set = BenefitBindings {
            task_set: None,
            ..complete.clone()
        };
        let incomplete_strategy = BenefitBindings {
            strategy_version: String::new(),
            ..complete.clone()
        };
        let blank_template = BenefitBindings {
            template: Some(" ".to_string()),
            ..complete.clone()
        };
        for incomplete in [
            incomplete_model,
            incomplete_task_set,
            incomplete_strategy,
            blank_template,
        ] {
            assert!(!incomplete.matches(&complete));
        }
        let other_suite = BenefitBindings {
            task_set: Some("suite-b".to_string()),
            ..complete.clone()
        };
        assert!(!complete.matches(&other_suite));
    }

    #[test]
    fn configured_auto_team_thresholds_and_sample_counts_must_be_sane() {
        let base = serde_json::to_value(TeamPolicy::embedded_defaults()).unwrap();
        for (pointer, invalid) in [
            ("/thresholds/success_rate_pp", serde_json::json!(-1.0)),
            ("/thresholds/success_rate_pp", serde_json::json!(101.0)),
            ("/thresholds/quality_pct", serde_json::json!(-1.0)),
            ("/strategy_version", serde_json::json!("")),
            ("/thresholds/wall_rel_save", serde_json::json!(-0.1)),
            ("/thresholds/wall_rel_save", serde_json::json!(1.1)),
            ("/min_samples_default", serde_json::json!(0)),
            ("/groups/code/min_samples", serde_json::json!(0)),
        ] {
            let mut value = base.clone();
            let target = value
                .pointer_mut(pointer)
                .expect("test pointer exists in embedded policy");
            *target = invalid;
            assert!(
                TeamPolicy::from_json(&value.to_string()).is_err(),
                "invalid policy value must be rejected at {pointer}"
            );
        }
        assert!(TeamPolicy::from_json(&base.to_string()).is_ok());

        let mut invalid = TeamPolicy::embedded_defaults();
        invalid.groups.get_mut("code").unwrap().min_samples = Some(0);
        let gate = gate_auto(&invalid, "code", None, None, "2026-08-31T00:00:00+00:00");
        assert!(!gate.allow_team);
        assert!(gate
            .reasons
            .iter()
            .any(|reason| reason.contains("配置无效")));
    }

    #[test]
    fn wilson_interval_small_sample_reasonable() {
        let (low, high) = wilson_ci95(8, 10);
        assert!(low >= 0.0 && high <= 1.0 && low < high);
        assert_eq!(wilson_ci95(0, 0), (0.0, 0.0));
    }

    #[test]
    fn samples_sufficient_requires_both_sides() {
        assert!(samples_sufficient(30, 30, 30));
        assert!(!samples_sufficient(29, 30, 30));
        assert!(!samples_sufficient(30, 29, 30));
    }

    fn outdated_pair() -> PairedStats {
        let mut p = pair("code", snap(30, 24, 1000.0), snap(30, 28, 700.0));
        p.generated_at = "2026-01-01T00:00:00+00:00".to_string(); // >30 天前
        p
    }

    #[test]
    fn gate_defaults_to_single_without_evidence() {
        let policy = TeamPolicy::embedded_defaults();
        let gate = gate_auto(&policy, "code", None, None, "2026-08-31T00:00:00+00:00");
        assert!(!gate.allow_team);
        assert!(gate.reasons.iter().any(|r| r.contains("无收益证据")));
    }

    #[test]
    fn gate_denies_when_verdict_not_eligible() {
        let policy = TeamPolicy::embedded_defaults();
        let p = pair("code", snap(30, 24, 1000.0), snap(30, 25, 900.0));
        let v = evaluate(&p, &policy.thresholds);
        let gate = gate_auto(&policy, "code", Some(&v), None, "2026-08-31T00:00:00+00:00");
        assert!(!gate.allow_team);
        assert!(gate.reasons.iter().any(|r| r.contains("收益未达标")));
    }

    #[test]
    fn gate_denies_insufficient_samples() {
        let policy = TeamPolicy::embedded_defaults();
        let p = pair(
            "code",
            snap(5, 4, 1000.0), // 80%
            snap(5, 5, 700.0),  // 100% → +20pp，但 n=5 < 30
        );
        let v = evaluate(&p, &policy.thresholds);
        assert!(v.eligible);
        assert!(!v.sample_sufficient);
        let gate = gate_auto(&policy, "code", Some(&v), None, "2026-08-31T00:00:00+00:00");
        assert!(!gate.allow_team, "样本不足不得放行：{gate:?}");
        assert!(gate.reasons.iter().any(|r| r.contains("样本不足")));
    }

    #[test]
    fn gate_denies_expired_record() {
        let policy = TeamPolicy::embedded_defaults();
        let p = outdated_pair(); // 达标但过期
        let v = evaluate(&p, &policy.thresholds);
        assert!(v.eligible);
        let gate = gate_auto(&policy, "code", Some(&v), None, "2026-08-31T00:00:00+00:00");
        assert!(!gate.allow_team);
        assert!(gate.reasons.iter().any(|r| r.contains("过期")));
    }

    #[test]
    fn gate_denies_when_bindings_mismatch() {
        let policy = TeamPolicy::embedded_defaults();
        let mut p = pair("code", snap(30, 24, 1000.0), snap(30, 28, 700.0));
        p.bindings = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: Some("code-change-v1".to_string()),
            task_set: Some("v1-r1-product-suite".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let v = evaluate(&p, &policy.thresholds);
        assert!(v.eligible && v.sample_sufficient);
        // 当前绑定不同模型 → 拒绝背书。
        let current = BenefitBindings {
            model: Some("newer-model".to_string()),
            template: Some("code-change-v1".to_string()),
            task_set: Some("v1-r1-product-suite".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let gate = gate_auto(
            &policy,
            "code",
            Some(&v),
            Some(&current),
            "2026-08-31T00:00:00+00:00",
        );
        assert!(!gate.allow_team);
        assert!(gate.reasons.iter().any(|r| r.contains("绑定")));

        let mut partial = pair("code", snap(30, 24, 1000.0), snap(30, 28, 700.0));
        partial.bindings = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: None,
            task_set: None,
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let partial = evaluate(&partial, &policy.thresholds);
        let active = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: None,
            task_set: Some("v1-r1-product-suite".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let denied = gate_auto(
            &policy,
            "code",
            Some(&partial),
            Some(&active),
            "2026-08-31T00:00:00+00:00",
        );
        assert!(!denied.allow_team);
        assert!(denied.reasons.iter().any(|r| r.contains("绑定")));
    }

    #[test]
    fn gate_rejects_incomplete_bindings_even_without_runtime_context() {
        let policy = TeamPolicy::embedded_defaults();
        let mut p = pair("code", snap(30, 24, 1000.0), snap(30, 28, 700.0));
        p.bindings = BenefitBindings::default();
        let verdict = evaluate(&p, &policy.thresholds);
        let gate = gate_auto(
            &policy,
            "code",
            Some(&verdict),
            None,
            "2026-08-31T00:00:00+00:00",
        );
        assert!(!gate.allow_team);
        assert!(gate.reasons.iter().any(|reason| reason.contains("完整")));
    }

    #[test]
    fn gate_allows_qualified_preselected_group() {
        let policy = TeamPolicy::embedded_defaults();
        let current = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: None,
            task_set: Some("v1".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        // 只有完整模型/任务集/策略版本绑定匹配时，达标证据才可启用预选组。
        let mut bound_pair = pair("code", snap(30, 24, 1000.0), snap(30, 28, 700.0));
        bound_pair.bindings = current.clone();
        let no_evidence = gate_auto(
            &policy,
            "code",
            None,
            Some(&current),
            "2026-08-31T00:00:00+00:00",
        );
        assert!(!no_evidence.allow_team, "无证据不得放行：{no_evidence:?}");
        let v = evaluate(&bound_pair, &policy.thresholds);
        let gate = gate_auto(
            &policy,
            "code",
            Some(&v),
            Some(&current),
            "2026-08-31T00:00:00+00:00",
        );
        assert!(
            gate.allow_team,
            "达标+样本充分+绑定匹配+预选组 → 放行：{gate:?}"
        );
        assert!(!gate.mandatory_review);
    }

    #[test]
    fn non_preselected_group_denied_even_if_qualified() {
        let policy = TeamPolicy::embedded_defaults();
        // document 未开放 allow_auto_team：即便达标也默认 single。
        let mut p = pair("document", snap(30, 24, 1000.0), snap(30, 27, 700.0));
        p.generated_at = "2026-08-30T00:00:00+00:00".to_string();
        let v = evaluate(&p, &policy.thresholds);
        assert!(v.eligible && v.sample_sufficient, "v = {v:?}");
        let gate = gate_auto(
            &policy,
            "document",
            Some(&v),
            None,
            "2026-08-31T00:00:00+00:00",
        );
        assert!(
            !gate.allow_team,
            "非预选组即使达标也不得 auto 组队：{gate:?}"
        );
        assert!(gate.reasons.iter().any(|r| r.contains("不是预选组")));
    }

    #[test]
    fn mandatory_review_surfaces_from_policy() {
        let mut policy = TeamPolicy::embedded_defaults();
        policy.groups.insert(
            "code".to_string(),
            TaskGroupPolicy {
                allow_auto_team: true,
                min_samples: Some(30),
                mandatory_review: true,
                note: "资金相关代码必须独立评审".to_string(),
            },
        );
        let gate = gate_auto(&policy, "code", None, None, "2026-08-31T00:00:00+00:00");
        assert!(
            gate.mandatory_review,
            "强制评审标记必须透出给 team_strategy"
        );
    }

    #[test]
    fn embedded_defaults_parse_like_json() {
        // 内嵌默认与 JSON 形状语义一致（roundtrip 不丢字段）。
        let policy = TeamPolicy::embedded_defaults();
        let json = serde_json::to_value(&policy).unwrap();
        let reparsed: TeamPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed.strategy_version, policy.strategy_version);
        assert_eq!(reparsed.min_samples_default, 30);
        assert_eq!(reparsed.groups.len(), 3);
        assert!(reparsed.groups.get("code").unwrap().allow_auto_team);
        assert!(!reparsed.groups.get("document").unwrap().allow_auto_team);
    }

    #[test]
    fn schema_version_mismatch_rejected() {
        let json = r#"{"schema_version":99,"strategy_version":"x","groups":{}}"#;
        assert!(TeamPolicy::from_json(json).is_err());
    }

    #[test]
    fn evaluate_fills_ci_from_samples() {
        let p = pair("code", snap(30, 24, 1000.0), snap(30, 27, 700.0));
        let v = evaluate(&p, &BenefitThresholds::default());
        assert!(v.eligible);
        // CI 沿用报告侧（fill_ci 只补足缺失侧；本判定不因 CI 改变结论）。
        assert!(v.rules.len() == 3);
    }

    // -- 配对报告消费层（第二路交付形状） -----------------------------------

    /// 与 `product_eval::build_paired_report_json` 相同形状的最小样例
    ///（overall + 分类组；快照字段逐字对齐 `paired_snapshot_json`）。
    fn sample_report_json() -> String {
        let snap = |mode: &str, runs: usize, passed: usize, wall: f64, quality: f64| {
            format!(
                r#"{{"mode":"{mode}","runs_total":{runs},"passed":{passed},"success_rate":{:.4},"ci95_low":0.60,"ci95_high":0.92,"mean_wall_ms":{wall},"mean_model_calls":3.2,"mean_tool_calls":8.0,"total_tokens":{},"total_cost_usd":null,"quality":{quality},"sample_sufficient":{},"independent_case_clusters":{}}}"#,
                passed as f64 / runs as f64,
                runs as u64 * 1000,
                runs >= 30,
                runs,
            )
        };
        let bindings = r#"{"model":"glm-5.3-flash","template":null,"task_set":"v1","strategy_version":"ten-5-independent-case-samples"}"#;
        format!(
            r#"{{
  "schema_version": 1,
  "generated_at": "2026-08-31T00:00:00+00:00",
  "bindings": {bindings},
  "single_report": {{"suite_name":"v1","suite_hash":"abc"}},
  "multi_report": {{"suite_name":"v1","suite_hash":"abc"}},
  "run_alignment": {{"configuration_aligned":true,"reasons":[],"paired_cells":1}},
  "pairs": [
    {{
      "task_group": "overall",
      "single": {},
      "multi": {},
      "bindings": {bindings},
      "generated_at": "2026-08-31T00:00:00+00:00"
    }},
    {{
      "task_group": "code",
      "single": {},
      "multi": {},
      "bindings": {bindings},
      "generated_at": "2026-08-31T00:00:00+00:00"
    }}
  ]
}}"#,
            snap("single", 30, 24, 1000.0, 0.80),
            snap("multi", 30, 24, 1300.0, 0.82),
            snap("single", 30, 24, 1000.0, 0.80),
            snap("multi", 30, 28, 700.0, 0.90),
        )
    }

    #[test]
    fn paired_report_roundtrip_and_group_resolution() {
        let report =
            PairedReport::from_json(&sample_report_json()).expect("二路报告形状必须可解析");
        assert_eq!(report.pairs.len(), 2);
        let policy = TeamPolicy::embedded_defaults();
        assert_eq!(
            report.policy_group_for(&policy, "code").as_deref(),
            Some("code")
        );
        assert_eq!(
            report.policy_group_for(&policy, "code-bug-fix").as_deref(),
            Some("code"),
            "case_id 以类别名为前缀 → 解析到策略组"
        );
        assert_eq!(
            report
                .policy_group_for(&policy, "research-source-map")
                .as_deref(),
            Some("research")
        );
        assert_eq!(
            report.policy_group_for(&policy, "overall"),
            None,
            "overall 不参与收益判定"
        );
        assert_eq!(report.policy_group_for(&policy, "unknown-thing"), None);
    }

    #[test]
    fn paired_report_schema_mismatch_rejected() {
        let bad = sample_report_json().replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert!(PairedReport::from_json(&bad).is_err());
    }

    #[test]
    fn missing_or_failed_run_alignment_cannot_enable_team() {
        let policy = TeamPolicy::embedded_defaults();
        let current = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: None,
            task_set: Some("v1".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        for aligned in [false, true] {
            let mut value: serde_json::Value = serde_json::from_str(&sample_report_json()).unwrap();
            if !aligned {
                value["run_alignment"]["configuration_aligned"] = serde_json::Value::Bool(false);
            } else {
                value.as_object_mut().unwrap().remove("run_alignment");
            }
            let report = PairedReport::from_json(&value.to_string()).unwrap();
            let items = evaluate_report(
                &report,
                &policy,
                Some(&current),
                "2026-08-31T00:00:00+00:00",
            );
            assert!(items.iter().all(|item| !item.gate.allow_team));
            assert!(items
                .iter()
                .filter(|item| item.policy_group.is_some())
                .all(|item| item
                    .gate
                    .reasons
                    .iter()
                    .any(|reason| reason.contains("run_alignment"))));
        }
    }

    #[test]
    fn evaluate_report_flags_qualified_group_and_denies_overall() {
        let report = PairedReport::from_json(&sample_report_json()).unwrap();
        let policy = TeamPolicy::embedded_defaults();
        let current = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: None,
            task_set: Some("v1".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let items = evaluate_report(
            &report,
            &policy,
            Some(&current),
            "2026-08-31T00:00:00+00:00",
        );
        assert_eq!(items.len(), 2);
        let overall = items.iter().find(|i| i.task_group == "overall").unwrap();
        assert!(!overall.gate.allow_team, "overall 必须默认 single");
        let code = items.iter().find(|i| i.task_group == "code").unwrap();
        assert_eq!(code.policy_group.as_deref(), Some("code"));
        assert!(
            code.gate.allow_team,
            "达标（+13.3pp）+ 样本充分 + 绑定匹配 + 预选组 → 放行：{:?}",
            code.gate
        );
    }

    #[test]
    fn evaluate_report_without_current_bindings_denies_declared_records() {
        let report = PairedReport::from_json(&sample_report_json()).unwrap();
        let policy = TeamPolicy::embedded_defaults();
        let items = evaluate_report(&report, &policy, None, "2026-08-31T00:00:00+00:00");
        let code = items.iter().find(|i| i.task_group == "code").unwrap();
        assert!(
            !code.gate.allow_team,
            "报告声明了绑定但调用方未提供绑定上下文 → 不背书"
        );
        assert!(code.gate.reasons.iter().any(|r| r.contains("绑定")));
    }

    #[test]
    fn format_report_states_threshold_not_passed_when_no_group_qualifies() {
        let report = PairedReport::from_json(&sample_report_json()).unwrap();
        let policy = TeamPolicy::embedded_defaults();
        let current = BenefitBindings {
            model: Some("other-model".to_string()),
            template: None,
            task_set: Some("v1".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let items = evaluate_report(
            &report,
            &policy,
            Some(&current),
            "2026-08-31T00:00:00+00:00",
        );
        let text = format_benefit_report(&items);
        assert!(
            text.contains("多 Agent 收益门槛未通过"),
            "无达标组必须明确输出门槛未通过：{text}"
        );
        assert!(
            text.contains("样本 single=30 multi=30"),
            "必须展示样本数：{text}"
        );
    }

    #[test]
    fn format_report_concludes_eligible_when_group_qualifies() {
        let report = PairedReport::from_json(&sample_report_json()).unwrap();
        let policy = TeamPolicy::embedded_defaults();
        let current = BenefitBindings {
            model: Some("glm-5.3-flash".to_string()),
            template: None,
            task_set: Some("v1".to_string()),
            strategy_version: "ten-5-independent-case-samples".to_string(),
        };
        let items = evaluate_report(
            &report,
            &policy,
            Some(&current),
            "2026-08-31T00:00:00+00:00",
        );
        let text = format_benefit_report(&items);
        assert!(text.contains("存在达标预选任务组"), "{text}");
        assert!(
            text.contains("⬜ overall"),
            "overall 组必须显式默认 single：{text}"
        );
    }

    #[test]
    fn policy_file_matches_embedded_defaults() {
        // evals/v1/team-policy.json 与内嵌默认必须语义一致（防止两处漂移）。
        let text = include_str!("../../../../evals/v1/team-policy.json");
        let policy = TeamPolicy::from_json(text).expect("evals/v1/team-policy.json 必须可解析");
        let embedded = TeamPolicy::embedded_defaults();
        assert_eq!(policy.schema_version, embedded.schema_version);
        assert_eq!(policy.strategy_version, embedded.strategy_version);
        assert_eq!(policy.min_samples_default, embedded.min_samples_default);
        assert_eq!(policy.max_record_age_days, embedded.max_record_age_days);
        assert_eq!(policy.groups.len(), embedded.groups.len());
        for (key, embedded_group) in &embedded.groups {
            let file_group = policy
                .groups
                .get(key)
                .unwrap_or_else(|| panic!("team-policy.json 缺少组 {key}"));
            assert_eq!(
                file_group.allow_auto_team, embedded_group.allow_auto_team,
                "组 {key}"
            );
            assert_eq!(
                file_group.mandatory_review, embedded_group.mandatory_review,
                "组 {key}"
            );
            assert_eq!(
                file_group.min_samples, embedded_group.min_samples,
                "组 {key} min_samples"
            );
        }
    }
}
