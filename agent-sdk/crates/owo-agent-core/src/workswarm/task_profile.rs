use super::RoleSpec;
use serde_json::Value;

const MAX_TASK_RISK_EVIDENCE_BYTES: usize = 256 * 1024;
const MAX_TASK_RISK_EVIDENCE_ITEMS: usize = 16 * 1024;

fn consume_task_risk_item(evidence_items: &mut usize, overflow: &mut bool) {
    if *overflow {
        return;
    }
    *evidence_items = (*evidence_items).saturating_add(1);
    if *evidence_items > MAX_TASK_RISK_EVIDENCE_ITEMS {
        // Bound traversal work even when structured input contains only empty/scalar items.
        *overflow = true;
    }
}

fn append_task_risk_text(
    target: &mut String,
    value: &str,
    evidence_bytes: &mut usize,
    evidence_items: &mut usize,
    overflow: &mut bool,
) {
    consume_task_risk_item(evidence_items, overflow);
    if *overflow {
        return;
    }
    let next_bytes = (*evidence_bytes).saturating_add(value.len());
    if next_bytes > MAX_TASK_RISK_EVIDENCE_BYTES {
        // Never truncate declared intent and then infer that the omitted work is safe.
        *overflow = true;
        return;
    }
    *evidence_bytes = next_bytes;
    target.push_str(&value.to_lowercase());
    target.push(' ');
}

fn append_task_risk_value(
    target: &mut String,
    value: &Value,
    evidence_bytes: &mut usize,
    evidence_items: &mut usize,
    overflow: &mut bool,
) {
    consume_task_risk_item(evidence_items, overflow);
    if *overflow {
        return;
    }
    match value {
        Value::String(text) => {
            append_task_risk_text(target, text, evidence_bytes, evidence_items, overflow)
        }
        Value::Array(items) => {
            for item in items {
                append_task_risk_value(target, item, evidence_bytes, evidence_items, overflow);
                if *overflow {
                    break;
                }
            }
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                append_task_risk_text(target, key, evidence_bytes, evidence_items, overflow);
                append_task_risk_value(target, item, evidence_bytes, evidence_items, overflow);
                if *overflow {
                    break;
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Derive strategy signals from the current request instead of feeding the strategy
/// engine a permanently hard-coded "one artifact, no risk" profile. Risk is a conservative
/// keyword screen over declared text and paths, not proof that an unmatched task is safe.
/// Historical success stays unknown until backed by persisted evaluation evidence.
pub(super) fn derive_task_profile(
    objective: &str,
    roles: &[RoleSpec],
    has_parent_context: bool,
    reviewer_is_explicit: bool,
) -> crate::team_strategy::TaskProfile {
    use crate::team_strategy::{RiskLevel, TaskProfile};

    let objective_lower = objective.to_lowercase();
    let has_any = |needles: &[&str]| {
        needles
            .iter()
            .any(|needle| objective_lower.contains(needle))
    };
    let category = if has_any(&[
        "代码", "源码", "重构", "修复", "bug", "frontend", "backend", "software", "code",
    ]) {
        Some("code")
    } else if has_any(&["研究", "调研", "检索", "文献", "research", "literature"]) {
        Some("research")
    } else if has_any(&[
        "文档", "报告", "简报", "撰写", "document", "report", "brief",
    ]) {
        Some("document")
    } else {
        None
    };
    let planner_roles = [
        "lead",
        "leader",
        "finalizer",
        "integrator",
        "project_integrator",
        "planner",
        "task_planner",
        "project_manager",
    ];
    let artifact_count = roles
        .iter()
        .filter(|role| {
            role.assignee != "human"
                && !role.is_reviewer()
                && !planner_roles
                    .iter()
                    .any(|planner| role.role.eq_ignore_ascii_case(planner))
        })
        .count()
        .max(1);
    // Risk screening includes the user request and the complete declared role work.
    // Byte and node budgets prevent both oversized text and scalar-heavy structured input from
    // making strategy selection unbounded. Exhaustion fails closed into independent review.
    let mut risk_text = String::new();
    let mut risk_evidence_bytes = 0;
    let mut risk_evidence_items = 0;
    let mut risk_evidence_overflow = false;
    append_task_risk_text(
        &mut risk_text,
        objective,
        &mut risk_evidence_bytes,
        &mut risk_evidence_items,
        &mut risk_evidence_overflow,
    );
    for role in roles {
        if risk_evidence_overflow {
            break;
        }
        append_task_risk_text(
            &mut risk_text,
            &role.role,
            &mut risk_evidence_bytes,
            &mut risk_evidence_items,
            &mut risk_evidence_overflow,
        );
        if let Some(contract) = role.handoff_contract.as_deref() {
            append_task_risk_text(
                &mut risk_text,
                contract,
                &mut risk_evidence_bytes,
                &mut risk_evidence_items,
                &mut risk_evidence_overflow,
            );
        }
        if let Some(verification) = role.verify.as_deref() {
            append_task_risk_text(
                &mut risk_text,
                verification,
                &mut risk_evidence_bytes,
                &mut risk_evidence_items,
                &mut risk_evidence_overflow,
            );
        }
        for path in &role.write_paths {
            append_task_risk_text(
                &mut risk_text,
                path,
                &mut risk_evidence_bytes,
                &mut risk_evidence_items,
                &mut risk_evidence_overflow,
            );
        }
        append_task_risk_value(
            &mut risk_text,
            &role.extra_input,
            &mut risk_evidence_bytes,
            &mut risk_evidence_items,
            &mut risk_evidence_overflow,
        );
    }
    let high_risk_markers = [
        "认证",
        "身份验证",
        "权限",
        "支付",
        "凭据",
        "密钥",
        "密码",
        "加密",
        "安全",
        "隐私",
        "个人信息",
        "医疗",
        "病历",
        "金融",
        "客户数据",
        "用户数据",
        "生产数据",
        "生产环境",
        "删库",
        "删除数据",
        "数据库迁移",
        "drop table",
        "truncate table",
        "production data",
        "production database",
        "credential",
        "secret",
        "payment",
        "security",
        "privacy",
        "medical",
        "medical record",
        "patient data",
        "customer data",
        "financial",
        "database migration",
    ];
    let risk = if risk_evidence_overflow
        || high_risk_markers
            .iter()
            .any(|marker| risk_text.contains(marker))
    {
        RiskLevel::High
    } else {
        RiskLevel::Normal
    };
    let needs_independent_review = risk == RiskLevel::High
        || (reviewer_is_explicit && roles.iter().any(RoleSpec::is_reviewer));
    let expects_json = [
        ".json",
        "json格式",
        "json format",
        "输出为 json",
        "return json",
        "emit json",
    ]
    .iter()
    .any(|marker| objective_lower.contains(marker));

    TaskProfile {
        category: category.map(str::to_string),
        artifact_count,
        input_count: if has_parent_context { 1 } else { 0 },
        needs_independent_review,
        risk,
        single_agent_success_rate: None,
        expects_json,
    }
}

#[cfg(test)]
mod task_profile_derivation_tests {
    use super::derive_task_profile;
    use crate::team_strategy::RiskLevel;
    use crate::workswarm::RoleSpec;

    #[test]
    fn profile_uses_declared_deliverers_reviewers_context_and_risk() {
        let mut builder = RoleSpec::agent("builder");
        builder.write_paths = vec!["src/auth.rs".to_string()];
        let mut writer = RoleSpec::agent("docs_writer");
        writer.write_paths = vec!["docs/auth.md".to_string()];
        let reviewer = RoleSpec::agent("reviewer");
        let profile = derive_task_profile(
            "重构认证与权限代码并输出 JSON格式结果",
            &[builder, writer, reviewer],
            true,
            true,
        );

        assert_eq!(profile.category.as_deref(), Some("code"));
        assert_eq!(profile.artifact_count, 2);
        assert_eq!(profile.input_count, 1);
        assert!(profile.needs_independent_review);
        assert_eq!(profile.risk, RiskLevel::High);
        assert!(profile.expects_json);
        assert_eq!(profile.single_agent_success_rate, None);
    }

    #[test]
    fn profile_does_not_invent_history_or_count_planner_and_reviewer_as_deliverables() {
        let mut lead = RoleSpec::agent("lead");
        lead.handoff_contract = Some("拆分任务图".to_string());
        let reviewer = RoleSpec::agent("critic");
        let builder = RoleSpec::agent("builder");
        let integrator = RoleSpec::agent("leader");
        let profile = derive_task_profile(
            "修复一个普通代码缺陷",
            &[lead, reviewer, builder, integrator],
            false,
            true,
        );

        assert_eq!(profile.artifact_count, 1);
        assert_eq!(profile.input_count, 0);
        assert!(profile.needs_independent_review);
        assert_eq!(profile.single_agent_success_rate, None);
    }

    #[test]
    fn generated_dynamic_reviewer_is_not_mistaken_for_a_user_mandated_review_signal() {
        let profile = derive_task_profile(
            "修复一个普通代码缺陷",
            &[RoleSpec::agent("builder"), RoleSpec::agent("critic")],
            false,
            false,
        );
        assert_eq!(profile.artifact_count, 1);
        assert!(!profile.needs_independent_review);
    }

    #[test]
    fn sensitive_paths_raise_review_requirement_even_when_objective_is_generic() {
        let mut writer = RoleSpec::agent("builder");
        writer.write_paths = vec!["config/secrets".to_string()];
        let profile = derive_task_profile("更新配置文件", &[writer], false, false);
        assert_eq!(profile.risk, RiskLevel::High);
        assert!(profile.needs_independent_review);
    }

    #[test]
    fn role_task_contract_and_structured_input_raise_high_risk_review() {
        let mut contract_role = RoleSpec::agent("writer");
        contract_role.handoff_contract =
            Some("Export patient medical records and rotate payment credentials".to_string());
        let contract_profile = derive_task_profile("更新业务配置", &[contract_role], false, false);
        assert_eq!(contract_profile.risk, RiskLevel::High);
        assert!(contract_profile.needs_independent_review);

        let mut input_role = RoleSpec::agent("builder");
        input_role.extra_input = serde_json::json!({
            "assigned_task": "迁移客户数据到新 schema",
            "acceptance": "必须保留生产数据并可回滚"
        });
        let input_profile = derive_task_profile("调整内部导入流程", &[input_role], false, false);
        assert_eq!(input_profile.risk, RiskLevel::High);
        assert!(input_profile.needs_independent_review);
    }

    #[test]
    fn oversized_declared_task_evidence_fails_closed_to_independent_review() {
        let mut role = RoleSpec::agent("builder");
        role.extra_input = serde_json::json!({
            "assigned_task": "x".repeat(super::MAX_TASK_RISK_EVIDENCE_BYTES + 1),
        });
        let profile = derive_task_profile("更新普通配置", &[role], false, false);
        assert_eq!(profile.risk, RiskLevel::High);
        assert!(profile.needs_independent_review);
    }

    #[test]
    fn oversized_structured_item_count_fails_closed_to_independent_review() {
        let mut role = RoleSpec::agent("builder");
        role.extra_input = serde_json::json!({
            "scalar_values": vec![0; super::MAX_TASK_RISK_EVIDENCE_ITEMS + 1],
        });
        let profile = derive_task_profile("更新普通配置", &[role], false, false);
        assert_eq!(profile.risk, RiskLevel::High);
        assert!(profile.needs_independent_review);
    }
}
