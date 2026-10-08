//! Pure Team creation policy: strategy resolution, role projection, budgets, and review topology.
use super::task_graph::{is_independent_reviewer_role, is_parallel_writer_name};
use super::*;

const AUTO_REVIEWER_MARKER: &str = "_workswarm_auto_independent_reviewer";

/// Keep the displayed strategy and worker runtime on one budget source.
/// Explicit template budgets remain authoritative; dynamic roles inherit a matching
/// producer, reviewer, or integrator ceiling instead of silently receiving 12 turns.
fn resolve_dynamic_dependency(
    role_name: &str,
    role_specs: &BTreeMap<String, RoleSpec>,
    planned_roles: &std::collections::BTreeSet<String>,
    visiting: &mut std::collections::HashSet<String>,
    resolved: &mut Vec<String>,
) {
    let key = role_name.to_ascii_lowercase();
    if planned_roles.contains(&key) {
        if !resolved
            .iter()
            .any(|role| role.eq_ignore_ascii_case(role_name))
        {
            resolved.push(role_name.to_string());
        }
        return;
    }
    if !visiting.insert(key.clone()) {
        return;
    }
    if let Some(spec) = role_specs.get(&key) {
        for dependency in &spec.depends_on {
            resolve_dynamic_dependency(dependency, role_specs, planned_roles, visiting, resolved);
        }
    }
    visiting.remove(&key);
}

/// For generated dynamic Team topologies only, make RunMeta match the role plan's
/// producer/reviewer/integrator set and bridge dependencies across removed stages.
/// Explicit roles and versioned templates keep their authored topology unchanged.
pub(super) fn align_dynamic_role_specs(
    specs: &mut Vec<RoleSpec>,
    strategy_roles: &[crate::team_strategy::RolePlan],
    budgets: &mut BTreeMap<String, usize>,
) -> (Vec<crate::team_strategy::SkippedRole>, usize) {
    let Some(planned_producer) = strategy_roles
        .iter()
        .find(|role| !matches!(role.role.as_str(), "critic" | "leader"))
        .map(|role| role.role.clone())
    else {
        return (Vec::new(), 0);
    };
    let producer_index = specs
        .iter()
        .position(|spec| spec.role.eq_ignore_ascii_case(&planned_producer))
        .or_else(|| {
            specs.iter().position(|spec| {
                spec.assignee == "agent"
                    && !spec.is_reviewer()
                    && !matches!(
                        spec.role.to_ascii_lowercase().as_str(),
                        "lead" | "leader" | "finalizer" | "planner" | "task_planner"
                    )
                    && !spec.role.to_ascii_lowercase().contains("integrat")
            })
        });
    let Some(producer_index) = producer_index else {
        return (Vec::new(), 0);
    };
    let old_producer = specs[producer_index].role.clone();
    if !old_producer.eq_ignore_ascii_case(&planned_producer) {
        specs[producer_index].role = planned_producer.clone();
        for spec in specs.iter_mut() {
            for dependency in &mut spec.depends_on {
                if dependency.eq_ignore_ascii_case(&old_producer) {
                    *dependency = planned_producer.clone();
                }
            }
        }
        if let Some(budget) = budgets.remove(&old_producer) {
            budgets.insert(planned_producer.clone(), budget);
        }
    }

    let planned = strategy_roles
        .iter()
        .map(|role| role.role.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    let before = specs
        .iter()
        .map(|spec| (spec.role.to_ascii_lowercase(), spec.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut skipped = Vec::new();
    let mut saved_budget_calls = 0usize;
    for spec in specs
        .iter()
        .filter(|spec| !planned.contains(&spec.role.to_ascii_lowercase()))
    {
        let role = spec.role.clone();
        let declared_budget = budgets.remove(&role);
        let effective_budget = declared_budget.unwrap_or_else(|| {
            if spec.assignee == "agent" {
                crate::worker_profile::DEFAULT_PROFILE_MAX_TURNS
            } else {
                0
            }
        });
        saved_budget_calls = saved_budget_calls.saturating_add(effective_budget);
        skipped.push(crate::team_strategy::SkippedRole {
            role: role.clone(),
            reason: "动态拓扑与策略角色集合对齐，避免未计入预算的串行阶段".to_string(),
        });
    }
    specs.retain(|spec| planned.contains(&spec.role.to_ascii_lowercase()));
    for spec in specs.iter_mut() {
        let original_dependencies = spec.depends_on.clone();
        let mut bridged = Vec::new();
        for dependency in original_dependencies {
            resolve_dynamic_dependency(
                &dependency,
                &before,
                &planned,
                &mut std::collections::HashSet::new(),
                &mut bridged,
            );
        }
        spec.depends_on = bridged;
    }
    (skipped, saved_budget_calls)
}

pub(super) fn bind_missing_strategy_budgets(
    strategy_roles: &[crate::team_strategy::RolePlan],
    specs: &[RoleSpec],
    budgets: &mut BTreeMap<String, usize>,
) {
    let find_role = |name: &str| {
        strategy_roles
            .iter()
            .find(|planned| planned.role.eq_ignore_ascii_case(name))
            .map(|planned| planned.budget_calls)
    };
    let producer_budget = strategy_roles
        .iter()
        .find(|planned| !matches!(planned.role.as_str(), "critic" | "leader"))
        .map(|planned| planned.budget_calls);

    for spec in specs {
        if budgets.contains_key(&spec.role) {
            continue;
        }
        let role = spec.role.to_ascii_lowercase();
        let is_integrator = matches!(
            role.as_str(),
            "lead" | "leader" | "finalizer" | "project_integrator"
        ) || role.contains("integrat");
        let budget = if spec.is_reviewer() {
            find_role("critic")
        } else if is_integrator {
            find_role("leader")
        } else {
            find_role(&spec.role).or(producer_budget)
        };
        if let Some(budget) = budget.filter(|budget| *budget > 0) {
            budgets.insert(spec.role.clone(), budget);
        }
    }
}

pub(super) fn is_auto_reviewer_role(role: &RoleSpec) -> bool {
    role.extra_input
        .get(AUTO_REVIEWER_MARKER)
        .and_then(Value::as_bool)
        == Some(true)
}

pub(super) fn bind_auto_reviewer_strategy_budget(
    plan: &mut crate::team_strategy::TeamPlan,
    budgets: &mut BTreeMap<String, usize>,
    reviewer_role: &str,
) {
    if let Some(planned_reviewer) = plan
        .roles
        .iter_mut()
        .find(|role| role.role.eq_ignore_ascii_case("critic"))
    {
        let budget = planned_reviewer.budget_calls;
        planned_reviewer.role = reviewer_role.to_string();
        planned_reviewer.duty = "只读评审最终候选版本与验收证据".to_string();
        budgets.insert(reviewer_role.to_string(), budget);
        return;
    }

    let budget = 3;
    budgets.insert(reviewer_role.to_string(), budget);
    plan.roles.push(crate::team_strategy::RolePlan {
        role: reviewer_role.to_string(),
        duty: "只读评审最终候选版本与验收证据".to_string(),
        budget_calls: budget,
    });
    plan.budget_calls_total = plan.budget_calls_total.saturating_add(budget);
}

pub(super) fn build_auto_reviewer_role(specs: &[RoleSpec]) -> Option<RoleSpec> {
    if specs.iter().any(is_independent_reviewer_role) {
        return None;
    }
    let mut suffix = 1usize;
    let name = loop {
        let candidate = if suffix == 1 {
            "independent_reviewer".to_string()
        } else {
            format!("independent_{suffix}_reviewer")
        };
        if specs.iter().all(|spec| spec.role != candidate) {
            break candidate;
        }
        suffix = suffix.saturating_add(1);
    };
    let mut role = RoleSpec::agent(name);
    role.depends_on = specs.iter().map(|spec| spec.role.clone()).collect();
    role.handoff_contract = Some("只读评审最终候选代码与验收证据；检查需求覆盖、明显缺陷、回归风险和验证结果，不得修改工作区。发现问题时给出文件/行与修复建议；通过时说明审查的候选版本。".to_string());
    role.verify = Some("non_empty".to_string());
    role.extra_input = serde_json::json!({(AUTO_REVIEWER_MARKER): true});
    Some(role)
}

pub(super) fn build_auto_reviewer_for_model_writers(specs: &[RoleSpec]) -> Option<RoleSpec> {
    let has_model_writer = specs.iter().any(|spec| {
        spec.assignee == "agent"
            && spec.worker.as_deref() == Some("agent")
            && !spec.is_reviewer()
            && crate::worker_profile::WorkerProfile::for_team_role(
                &spec.role,
                &spec.capabilities,
                0,
                !spec.write_paths.is_empty(),
                is_parallel_writer_name(&spec.role),
            )
            .is_writer()
    });
    has_model_writer
        .then(|| build_auto_reviewer_role(specs))
        .flatten()
}

pub(super) fn resolve_strategy_selection(
    mode: TeamMode,
    requested: Option<crate::team_strategy::TeamSelectionMode>,
) -> Result<crate::team_strategy::TeamSelectionMode, String> {
    use crate::team_strategy::TeamSelectionMode;
    if mode == TeamMode::Single {
        return match requested {
            Some(TeamSelectionMode::ForceTeam) => {
                Err("mode=single 与 strategy=team 冲突；请改用 mode=team".to_string())
            }
            _ => Ok(TeamSelectionMode::ForceSingle),
        };
    }
    Ok(requested.unwrap_or_default())
}

pub(super) fn should_trim_to_single(
    mode: TeamMode,
    selection: crate::team_strategy::TeamSelectionMode,
    strategy_is_single: bool,
    role_count: usize,
    has_explicit_roles: bool,
    has_explicit_template: bool,
) -> bool {
    if !strategy_is_single || role_count <= 1 {
        return false;
    }
    match selection {
        crate::team_strategy::TeamSelectionMode::ForceSingle => true,
        crate::team_strategy::TeamSelectionMode::ForceTeam => false,
        crate::team_strategy::TeamSelectionMode::Auto => {
            mode != TeamMode::Swarmflow && !has_explicit_roles && !has_explicit_template
        }
    }
}

pub(super) fn should_add_auto_reviewer(mode: TeamMode, strategy_is_single: bool) -> bool {
    mode != TeamMode::Single && !strategy_is_single
}

pub(super) fn validate_team_budget_config(budget: &serde_json::Value) -> Result<(), String> {
    let Some(fields) = budget.as_object() else {
        return if budget.is_null() {
            Ok(())
        } else {
            Err("budget 必须是 JSON 对象或 null".to_string())
        };
    };

    if let Some(value) = fields
        .get("max_model_calls")
        .filter(|value| !value.is_null())
    {
        if value.as_u64().is_none() {
            return Err("budget.max_model_calls 必须是非负整数".to_string());
        }
    }
    if let Some(value) = fields.get("max_cost_usd").filter(|value| !value.is_null()) {
        match value.as_f64() {
            Some(limit) if limit.is_finite() && limit >= 0.0 => {}
            _ => return Err("budget.max_cost_usd 必须是有限的非负数字".to_string()),
        }
    }
    if let Some(value) = fields.get("max_wall_secs").filter(|value| !value.is_null()) {
        if value.as_u64().is_none() {
            return Err("budget.max_wall_secs 必须是非负整数".to_string());
        }
    }
    Ok(())
}
