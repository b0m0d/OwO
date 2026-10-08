//! Template matching and runtime adaptive-role policy for Team templates.
//! Pure decisions only: host code owns persistence, DAG rewrites, and evidence.

use super::{RiskLevel, TaskProfile};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

// ---------------------------------------------------------------------------
// 自适应角色策略（八期 · 第一路）：模板级 DAG 的角色裁剪
// ---------------------------------------------------------------------------

/// 跳过角色记录（`skipped_roles` / `skip_reason` 指标来源）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedRole {
    pub role: String,
    pub reason: String,
}

/// 自适应角色决策：跳过名单 + 节省的调用预算（`saved_budget_calls`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdaptiveRoleDecision {
    pub skipped: Vec<SkippedRole>,
    pub saved_budget_calls: usize,
    /// 可展示的判定理由（进 strategy_decision / 审计 / UI）。
    pub reasons: Vec<String>,
}

/// 模板级自适应角色策略（八期一路；纯函数）：
///
/// - `code-change-v1` 保留 reviewer 到运行期：无实际变更时跳过，有候选变更时执行版本绑定评审；
/// - `research-brief-v1` 简单任务 → 并行研究（researcher_a/b）后只保留一个
///   汇总角色（跳过 evidence_verifier；来源要求移交 brief_writer 验收段）；
/// - 高风险 / 明确要求独立评审 → 不裁剪（评审是硬需求）；
/// - 其余模板与未知模板 → 不裁剪（结构化抽取的 Schema 校验、文档终稿链是
///   交付语义的一部分）。
///
/// 返回值只描述决策；调用方负责从 DAG 中移除角色并**把指向被跳过角色的依赖
/// 重定向到其上游**（保持 DAG 可拓扑排序）。
pub fn plan_adaptive_roles(
    template_id: Option<&str>,
    role_names: &[String],
    budgets: &BTreeMap<String, usize>,
    profile: &TaskProfile,
) -> AdaptiveRoleDecision {
    let mut decision = AdaptiveRoleDecision::default();
    let Some(template_id) = template_id else {
        return decision;
    };
    // 高风险或明确要求独立评审 → 一律保留评审/核验角色。
    if profile.risk == RiskLevel::High || profile.needs_independent_review {
        return decision;
    }
    let simple = |role: &str| -> Option<SkippedRole> {
        match template_id {
            // Reviewer is retained until the host observes whether a source candidate exists.
            // Runtime skipping handles the no-change case; changed code still gets the same
            // independent review that Single applies to accepted source candidates.
            template
                if crate::builtin_team_templates::is_code_change_template(template)
                    && role == "reviewer" =>
            {
                None
            }
            crate::builtin_team_templates::RESEARCH_BRIEF_V1 if role == "evidence_verifier" => {
                // 并行研究保留（researcher_a/b 都在）才裁核验：汇总前仍有双路证据。
                let parallel_kept = role_names.iter().any(|r| r == "researcher_a")
                    && role_names.iter().any(|r| r == "researcher_b");
                if parallel_kept {
                    Some(SkippedRole {
                        role: role.to_string(),
                        reason: "研究任务并行研究后只保留一个汇总角色：来源要求移交 brief_writer 验收段（每条结论附引用）".to_string(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    for role in role_names {
        if let Some(skip) = simple(role) {
            decision.saved_budget_calls += budgets.get(role).copied().unwrap_or(0);
            decision
                .reasons
                .push(format!("跳过角色 {}：{}", skip.role, skip.reason));
            decision.skipped.push(skip);
        }
    }
    decision
}

/// 运行期 reviewer 跳过判定（八期一路，`code-change-v1` 专用）：
/// 实现步骤未产生任何实际工作区变更时，只读评审没有可评审对象 → 跳过；
/// 有实际变更（或非 reviewer 角色）→ None（正常执行）。
///
/// 「实际变更」由调用方判定：服务端 Git 变更跟踪文件
/// （`<run_dir>/<team_id>-workspace-changes.json`，含 `changed_files` 窗口增量）
/// 或评测执行器的等价信号；无记录视为无变更。
pub fn review_runtime_skip_reason(is_reviewer: bool, has_actual_changes: bool) -> Option<String> {
    if !is_reviewer || has_actual_changes {
        return None;
    }
    Some(
        "上游实现步骤未产生任何实际工作区变更（无可评审对象），按自适应策略跳过；\
         下游完成条件已满足，DAG 提前结束"
            .to_string(),
    )
}

/// Apply the runtime skip policy to host review evidence.
/// Pending review issues always keep the reviewer. Otherwise only an authoritative
/// Some(false) proves there is no candidate to review; missing or malformed tracking
/// data remains review-required.
pub fn review_runtime_skip_for_workspace_status(
    is_reviewer: bool,
    review_issues_pending: bool,
    workspace_has_changes: Option<bool>,
) -> Option<String> {
    if review_issues_pending {
        return None;
    }
    review_runtime_skip_reason(is_reviewer, workspace_has_changes != Some(false))
}

pub fn reviewer_runtime_skip_reason(role: &str, has_actual_changes: bool) -> Option<String> {
    review_runtime_skip_reason(role == "reviewer", has_actual_changes)
}
