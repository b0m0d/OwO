//! 自适应组队策略引擎（R3 第一路）：WorkSwarm 降本提效的判定核心。
//!
//! 产品事实（V1-R1 live 基线，`docs/reports/v1-r1-live-baseline.md`）：WorkSwarm
//! 三角色固定组队相对单 Agent 成功率 −30pp、平均耗时 +163%、tokens +101%。
//! 本引擎让"简单任务自动走单 Agent，只有确实适合拆分的任务才创建团队"，并给出
//! **可展示的组队理由**（UI `auto` 模式直接渲染）。
//!
//! 设计边界：纯函数、零 I/O、零模型调用；历史成功率由调用方注入（评测执行器
//! 当前传 `None`，server 侧可接 journal 统计）。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 输入：模式选择 + 任务画像
// ---------------------------------------------------------------------------

/// 组队模式选择（`auto` 为默认值；序列化名为 API/UI 契约：single|team|auto）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamSelectionMode {
    /// 强制单 Agent（一个 producer，零评审零裁决）。
    #[serde(rename = "single")]
    ForceSingle,
    /// 强制多 Agent（完整 producer + critic + leader 流水线）。
    #[serde(rename = "team")]
    ForceTeam,
    /// 系统判定（默认）：按任务画像信号决定 single 或 team。
    #[default]
    #[serde(rename = "auto")]
    Auto,
}

impl TeamSelectionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ForceSingle => "single",
            Self::ForceTeam => "team",
            Self::Auto => "auto",
        }
    }

    /// 解析 CLI/API 字符串（大小写不敏感；未知值报错文案列出合法值）。
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "single" => Ok(Self::ForceSingle),
            "team" => Ok(Self::ForceTeam),
            "auto" => Ok(Self::Auto),
            other => Err(format!(
                "未知组队模式「{other}」（可选 single|team|auto，缺省 auto）"
            )),
        }
    }
}

/// 任务风险档位（影响 critic 是否介入）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    #[default]
    Normal,
    High,
}

/// 任务画像：策略判定的全部输入（由调用方从任务定义/历史提取）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskProfile {
    /// 任务分类标签（code|research|document|…；仅进入理由文本，不单独触发信号）。
    #[serde(default)]
    pub category: Option<String>,
    /// 预期 Artifact 数量。
    #[serde(default)]
    pub artifact_count: usize,
    /// 输入材料数量。
    #[serde(default)]
    pub input_count: usize,
    /// 是否需要独立评审（审批要求/高风险变更/历史检查器频繁失败）。
    #[serde(default)]
    pub needs_independent_review: bool,
    /// 风险与验证复杂度。
    #[serde(default)]
    pub risk: RiskLevel,
    /// 单 Agent 历史成功率（未知 = None；< 0.7 视为"单 Agent 搞不定"信号）。
    #[serde(default)]
    pub single_agent_success_rate: Option<f64>,
    /// 结构化 JSON 任务（期望 .json 产物）→ 判定为 single 时附带一次性格式修复预算。
    #[serde(default)]
    pub expects_json: bool,
}

// ---------------------------------------------------------------------------
// 输出：组队计划
// ---------------------------------------------------------------------------

/// 单角色计划：职责与调用预算。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolePlan {
    pub role: String,
    pub duty: String,
    /// 该角色的模型调用预算（= AgentConfig.max_turns 上限）。
    pub budget_calls: usize,
}

/// 组队决策结果（可整体序列化进 UI/API）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamPlan {
    /// 解析后的执行形态：`single` | `team`。
    pub mode: String,
    /// 用户请求的模式（判定前）。
    pub requested: String,
    /// 角色计划（single 恒 1 个 producer）。
    pub roles: Vec<RolePlan>,
    /// 并行度：DAG 首波可同时启动的角色数（链式流水线为 1）。
    pub parallelism: usize,
    /// 全队模型调用预算（各角色之和）。
    pub budget_calls_total: usize,
    /// 结构化 JSON 任务的一次修复机会（single 判定下与 producer 合并执行）。
    pub json_repair: bool,
    /// 可展示的组队理由（逐条；UI auto 模式直接渲染）。
    pub reasons: Vec<String>,
}

impl TeamPlan {
    /// 按角色名取调用预算（未知角色退回默认 4）。
    pub fn budget_for(&self, role: &str) -> usize {
        self.roles
            .iter()
            .find(|r| r.role == role)
            .map(|r| r.budget_calls)
            .unwrap_or(4)
    }

    pub fn is_single(&self) -> bool {
        self.mode == "single"
    }
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

/// 判定阈值集合（可按产品数据演进调整）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamStrategyThresholds {
    /// 输入材料 ≥ 该值视为"多来源合并"信号。
    pub multi_source_inputs: usize,
    /// 预期 Artifact ≥ 该值视为"多 Artifact 综合"信号。
    pub multi_artifact: usize,
    /// 单 Agent 历史成功率低于该值视为"单 Agent 搞不定"信号。
    pub low_single_success: f64,
    /// producer 缺省调用预算。
    pub producer_budget: usize,
    /// critic 缺省调用预算。
    pub critic_budget: usize,
    /// leader 缺省调用预算。
    pub leader_budget: usize,
}

impl Default for TeamStrategyThresholds {
    fn default() -> Self {
        Self {
            multi_source_inputs: 2,
            multi_artifact: 2,
            low_single_success: 0.70,
            producer_budget: 4,
            critic_budget: 2,
            leader_budget: 3,
        }
    }
}

/// 自适应组队策略引擎（无状态；`decide` 为纯函数）。
#[derive(Debug, Clone, Default)]
pub struct TeamStrategyEngine {
    pub thresholds: TeamStrategyThresholds,
}

impl TeamStrategyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_thresholds(thresholds: TeamStrategyThresholds) -> Self {
        Self { thresholds }
    }

    /// 判定入口：`auto` 按信号决定；`single`/`team` 为显式强制。
    pub fn decide(&self, selection: TeamSelectionMode, profile: &TaskProfile) -> TeamPlan {
        match selection {
            TeamSelectionMode::ForceSingle => self.single_plan(selection, profile),
            TeamSelectionMode::ForceTeam => self.forced_team_plan(selection, profile),
            TeamSelectionMode::Auto => self.auto_plan(selection, profile),
        }
    }

    // -- 信号提取 -----------------------------------------------------------

    /// 多来源合并 / 多 Artifact 综合信号（leader 的启用依据）。
    fn merge_signal(&self, profile: &TaskProfile) -> Option<String> {
        if profile.input_count >= self.thresholds.multi_source_inputs {
            Some(format!(
                "输入材料 {} 份 ≥ {}：需要多来源合并，启用综合角色",
                profile.input_count, self.thresholds.multi_source_inputs
            ))
        } else if profile.artifact_count >= self.thresholds.multi_artifact {
            Some(format!(
                "预期产物 {} 项 ≥ {}：需要多 Artifact 综合，启用综合角色",
                profile.artifact_count, self.thresholds.multi_artifact
            ))
        } else {
            None
        }
    }

    /// 独立评审信号（critic 的启用依据：高风险 / 需审批 / 单 Agent 历史拉胯）。
    fn review_signal(&self, profile: &TaskProfile) -> Option<String> {
        if profile.risk == RiskLevel::High {
            Some("高风险任务：需要独立评审角色把关".to_string())
        } else if profile.needs_independent_review {
            Some("任务要求独立评审/审批：启用只读评审角色".to_string())
        } else if matches!(profile.single_agent_success_rate, Some(rate) if rate < self.thresholds.low_single_success)
        {
            Some(format!(
                "单 Agent 历史成功率 {:.0}% < {:.0}%：补充评审角色提高一次通过率",
                profile.single_agent_success_rate.unwrap_or(f64::NAN) * 100.0,
                self.thresholds.low_single_success * 100.0
            ))
        } else {
            None
        }
    }

    // -- 计划构造 -----------------------------------------------------------

    fn single_plan(&self, selection: TeamSelectionMode, profile: &TaskProfile) -> TeamPlan {
        let mut reasons = match selection {
            TeamSelectionMode::ForceSingle => {
                vec!["模式强制 single：按用户要求使用单 Agent".to_string()]
            }
            _ => vec![],
        };
        let merge = self.merge_signal(profile);
        let review = self.review_signal(profile);
        if selection == TeamSelectionMode::Auto {
            if merge.is_none() && review.is_none() {
                reasons.push(format!(
                    "简单任务信号：单产物（{}）、输入材料 {} 份、风险 {:?} 且无强制评审要求 → 单 Agent 足够",
                    profile.artifact_count, profile.input_count, profile.risk
                ));
            } else {
                // 信号存在但被历史/画像压过的情况仍如实列出（single 是保守选择时说明代价）。
                if let Some(signal) = merge {
                    reasons.push(format!(
                        "存在合并信号（{signal}），但当前按 single 执行（预算内一次产出）"
                    ));
                }
                if let Some(signal) = review {
                    reasons.push(format!("存在评审信号（{signal}），但当前按 single 执行"));
                }
            }
        }
        if profile.expects_json {
            reasons.push(
                "结构化 JSON 任务：附加确定性格式检查与一次修复机会（json_repair）".to_string(),
            );
        }
        let roles = vec![RolePlan {
            role: producer_role_name(profile),
            duty: "独立完成主交付物并自行核验格式".to_string(),
            budget_calls: self.thresholds.producer_budget,
        }];
        TeamPlan {
            mode: "single".to_string(),
            requested: selection.as_str().to_string(),
            parallelism: 1,
            budget_calls_total: roles[0].budget_calls,
            json_repair: profile.expects_json,
            roles,
            reasons,
        }
    }

    fn forced_team_plan(&self, selection: TeamSelectionMode, profile: &TaskProfile) -> TeamPlan {
        // 显式 team：完整流水线（producer + critic + leader）——用户明确要求评审与裁决。
        let producer = RolePlan {
            role: producer_role_name(profile),
            duty: "产出主交付物草稿".to_string(),
            budget_calls: self.thresholds.producer_budget,
        };
        let critic = RolePlan {
            role: "critic".to_string(),
            duty: "只读评审草稿（完整性/一致性/符合度）".to_string(),
            budget_calls: self.thresholds.critic_budget,
        };
        let leader = RolePlan {
            role: "leader".to_string(),
            duty: "综合草稿与评审意见，裁决并产出最终交付物".to_string(),
            budget_calls: self.thresholds.leader_budget,
        };
        let roles = vec![producer, critic, leader];
        let budget_calls_total = roles.iter().map(|r| r.budget_calls).sum();
        let reasons = vec![
            "模式强制 team：按用户要求启用完整 producer → critic → leader 流水线".to_string(),
            format!(
                "全队调用预算 {budget_calls_total} 次（producer {} + critic {} + leader {}）",
                roles[0].budget_calls, roles[1].budget_calls, roles[2].budget_calls
            ),
        ];
        TeamPlan {
            mode: "team".to_string(),
            requested: selection.as_str().to_string(),
            parallelism: 1,
            budget_calls_total,
            json_repair: profile.expects_json,
            roles,
            reasons,
        }
    }

    fn auto_plan(&self, selection: TeamSelectionMode, profile: &TaskProfile) -> TeamPlan {
        let merge = self.merge_signal(profile);
        let review = self.review_signal(profile);
        if merge.is_none() && review.is_none() {
            // 无任何信号：简单任务，直接单 Agent（降本主路径）。
            return self.single_plan(selection, profile);
        }

        let producer_role = producer_role_name(profile);
        let mut roles = vec![RolePlan {
            role: producer_role.clone(),
            duty: "产出主交付物草稿（吸收全部输入材料）".to_string(),
            budget_calls: self.thresholds.producer_budget,
        }];
        let mut reasons = Vec::new();
        if let Some(signal) = &review {
            reasons.push(signal.clone());
            roles.push(RolePlan {
                role: "critic".to_string(),
                duty: "只读评审草稿（完整性/一致性/符合度）".to_string(),
                budget_calls: self.thresholds.critic_budget,
            });
        }
        if let Some(signal) = &merge {
            reasons.push(signal.clone());
            roles.push(RolePlan {
                role: "leader".to_string(),
                duty: "综合草稿（与评审意见），裁决并产出最终交付物".to_string(),
                budget_calls: self.thresholds.leader_budget,
            });
        }
        reasons.push(format!(
            "普通团队最小化：默认 producer + critic 两角色，仅合并/综合场景追加 leader（当前 {} 角色）",
            roles.len()
        ));
        reasons.push(format!(
            "全队调用预算 {} 次（{}）",
            roles.iter().map(|r| r.budget_calls).sum::<usize>(),
            roles
                .iter()
                .map(|r| format!("{} {}", r.role, r.budget_calls))
                .collect::<Vec<_>>()
                .join(" + ")
        ));
        // 并行度：首波（无依赖）角色数——链式流水线恒 1；如未来拆分并行 producer 自动放大。
        let parallelism = roles
            .iter()
            .filter(|role| role.role == producer_role)
            .count()
            .max(1);
        TeamPlan {
            mode: "team".to_string(),
            requested: selection.as_str().to_string(),
            parallelism,
            budget_calls_total: roles.iter().map(|r| r.budget_calls).sum(),
            json_repair: profile.expects_json,
            roles,
            reasons,
        }
    }
}

/// 按任务分类给出 producer 角色名（builder/researcher/writer；未知 → producer）。
fn producer_role_name(profile: &TaskProfile) -> String {
    match profile.category.as_deref() {
        Some("code") => "builder".to_string(),
        Some("research") => "researcher".to_string(),
        Some("document") => "writer".to_string(),
        _ => "producer".to_string(),
    }
}

// ---------------------------------------------------------------------------
// 模板适用性匹配共享谓词（六期 · 第三路）
// ---------------------------------------------------------------------------

/// 适用条件关键词切分（`TeamTemplateRegistry::find_match` 同一口径：按空白与
/// 中英标点切段，长度 ≥2）。目录层与策略层共用，保持单一判定语义。
pub fn applicability_tokens(applicability: &str) -> Vec<String> {
    applicability
        .split([' ', '，', ',', '、', '/', '\n', '\t'])
        .map(str::trim)
        .filter(|token| !token.is_empty() && token.chars().count() >= 2)
        .map(str::to_string)
        .collect()
}

/// 目标文本是否命中模板适用条件（大小写不敏感子串，与 find_match 一致）。
///
/// 目录层（`builtin_team_templates` / server `team_template_catalog_api`）用它
/// 预览「哪些目标会自动匹配该模板」；安装状态由调用方保证——**未安装模板不得
/// 参与自动匹配**（注册表只含已安装/已采纳模板，find_match 天然满足）。
pub fn applicability_matches(applicability: &str, objective: &str) -> bool {
    let objective_lower = objective.to_lowercase();
    applicability_tokens(applicability)
        .iter()
        .any(|token| objective_lower.contains(&token.to_lowercase()))
}

#[cfg(test)]
mod applicability_tests {
    use super::*;

    #[test]
    fn tokens_split_on_cjk_and_ascii_separators() {
        let tokens = applicability_tokens("代码 修改，重构、bug/fix\n接口");
        assert_eq!(tokens, vec!["代码", "修改", "重构", "bug", "fix", "接口"]);
    }

    #[test]
    fn matches_objective_case_insensitive_substring() {
        assert!(applicability_matches(
            "代码 重构 修复",
            "重构登录模块的代码"
        ));
        assert!(applicability_matches("文档 报告", "请编写一份 API 报告"));
        assert!(!applicability_matches("代码 重构", "撰写研究简报"));
        // 短于 2 的 token 不参与（避免误命中）。
        assert!(!applicability_matches("a b", "修改代码"));
    }

    #[test]
    fn empty_inputs_never_match() {
        assert!(!applicability_matches("", "任意目标"));
        assert!(!applicability_matches("代码", ""));
    }
}
