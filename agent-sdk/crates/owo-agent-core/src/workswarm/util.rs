use crate::goal::GoalBudget;
use crate::plan::{VerificationPlanV1, VerificationSpec};
use serde_json::Value;
use std::path::Path;

/// Resolve the one effective model shared by model-driven team roles. Explicit
/// per-role input overrides role and request defaults, matching plan construction.
pub(super) fn resolve_team_model_binding(
    roles: &[super::RoleSpec],
    request_model: Option<&str>,
    provider_model: &str,
) -> Option<String> {
    let request_model = request_model
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let provider_model = provider_model.trim();
    let mut effective = None::<String>;
    for role in roles
        .iter()
        .filter(|role| !role.assignee.eq_ignore_ascii_case("human"))
    {
        let input_model = role
            .extra_input
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let role_model = role
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let model = input_model
            .or(role_model)
            .or(request_model)
            .unwrap_or(provider_model);
        if model.is_empty() {
            return None;
        }
        match effective.as_deref() {
            None => effective = Some(model.to_string()),
            Some(existing) if existing == model => {}
            Some(_) => return None,
        }
    }
    effective
}

pub(super) fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 十期·四路：把三路冻结的收益策略 gate 接入真实运行入口（create_team_run）。
///
/// - 策略加载：`OWO_TEAM_POLICY` 环境变量指向的 JSON；缺省尝试
///   `evals/v1/team-policy.json`（工作区根/当前目录向上探测）；没有配置文件时用
///   [`crate::team_benefit::TeamPolicy::embedded_defaults`]。配置无效会继续找有效替代；
///   若存在无效配置但没有有效替代，则回退内置阈值并关闭 Auto Team，显式 Force Team 不受影响。
/// - 证据加载：`OWO_TEAM_PAIRED_REPORT` 指向二路生成的配对对照报告（可选）。
///   报告解析/组匹配失败 → 视为无证据（保守 single，附可展示理由）。
/// - 判定：`gate_auto(policy, task_group, verdict, bindings, now)`；无证据/不达标/
///   样本不足/过期/绑定不匹配/非预选组 → `allow_team=false`（默认 single）。
/// - 任务组推断：模板 id 优先（code-change-v1→code、research-brief-v1→research、
///   document-delivery-v1→document、structured-extract-v1→document）；无模板时按
///   目标关键词启发式；兜底 "code"（与三路 policy_group_for 前缀解析同口径）。
pub(super) fn benefit_gate_for_runtime(
    template_id: Option<&str>,
    objective: &str,
    run_dir: &Path,
    roles: &[super::RoleSpec],
    request_model: Option<&str>,
) -> (
    crate::team_benefit::PolicyGate,
    Option<crate::team_benefit::BenefitVerdict>,
    String,
) {
    let policy = load_team_policy_for_runtime(run_dir);
    let task_group = infer_benefit_task_group(template_id, objective);
    let verdict = load_benefit_verdict_for_runtime(&policy, &task_group);
    let provider_model = std::env::var("OWO_AGENT_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != crate::gateway::MODEL_DEFAULT_SENTINEL)
        .or_else(|| {
            std::env::var("OPENAI_MODEL")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| crate::gateway::DEFAULT_MODEL_ID.to_string());
    let model_binding = resolve_team_model_binding(roles, request_model, &provider_model);
    let current = model_binding
        .as_ref()
        .map(|model| crate::team_benefit::BenefitBindings {
            model: Some(model.clone()),
            template: template_id.map(str::to_string),
            task_set: std::env::var("OWO_TEAM_TASK_SET")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            strategy_version: policy.strategy_version.clone(),
        });
    let gate = crate::team_benefit::gate_auto(
        &policy,
        &task_group,
        verdict.as_ref(),
        current.as_ref(),
        &now_ts(),
    );
    let evidence = if model_binding.is_none() {
        "无法确认唯一模型，配对报告不背书（默认 single）".to_string()
    } else if gate.allow_team {
        "配对报告与当前模型、模板、任务集和策略版本匹配".to_string()
    } else if verdict.is_some() {
        "存在配对报告，但未通过收益、样本或配置绑定门槛；默认 single".to_string()
    } else {
        "无配对报告证据（默认 single，等二路验收报告）".to_string()
    };
    (gate, verdict, evidence)
}

fn disable_auto_team(policy: &mut crate::team_benefit::TeamPolicy) {
    for group in policy.groups.values_mut() {
        group.allow_auto_team = false;
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)] // 测试模块历史位置靠前；移动会打乱同文件阅读顺序
mod policy_fallback_tests {
    use super::disable_auto_team;
    use crate::team_benefit::TeamPolicy;

    #[test]
    fn invalid_policy_fallback_keeps_manual_team_but_disables_auto_groups() {
        let mut policy = TeamPolicy::embedded_defaults();
        let group_count = policy.groups.len();
        assert!(policy.groups.values().any(|group| group.allow_auto_team));
        disable_auto_team(&mut policy);
        assert_eq!(policy.groups.len(), group_count);
        assert!(policy.groups.values().all(|group| !group.allow_auto_team));
    }
}

/// 运行时策略加载（见 [`benefit_gate_for_runtime`] 说明）。
pub(super) fn load_team_policy_for_runtime(run_dir: &Path) -> crate::team_benefit::TeamPolicy {
    use crate::team_benefit::TeamPolicy;
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    let mut invalid_config_seen = false;
    if let Ok(path) = std::env::var("OWO_TEAM_POLICY") {
        let path = std::path::PathBuf::from(path);
        if path.as_os_str().is_empty() || !path.is_file() {
            tracing::warn!(candidate = %path.display(), "显式 OWO_TEAM_POLICY 不存在，禁用 Auto Team 回退");
            invalid_config_seen = true;
        }
        candidates.push(path);
    }
    // 工作区探测：run_dir 向上找 agent-sdk/evals/v1/team-policy.json；
    // 再加 cwd 相对路径两种写法。
    for probe in [
        "evals/v1/team-policy.json",
        "agent-sdk/evals/v1/team-policy.json",
    ] {
        candidates.push(std::path::PathBuf::from(probe));
        if let Some(ancestor) = run_dir.ancestors().find(|a| a.join(probe).is_file()) {
            candidates.push(ancestor.join(probe));
        }
    }
    for candidate in candidates {
        match std::fs::read_to_string(&candidate) {
            Ok(text) => match TeamPolicy::from_json(&text) {
                Ok(policy) => return policy,
                Err(error) => {
                    tracing::warn!(candidate = %candidate.display(), %error, "team-policy.json 无效；继续查找有效配置");
                    invalid_config_seen = true;
                }
            },
            Err(error) if candidate.is_file() => {
                tracing::warn!(candidate = %candidate.display(), %error, "team-policy.json 不可读取");
                invalid_config_seen = true;
            }
            Err(_) => {}
        }
    }
    let mut policy = TeamPolicy::embedded_defaults();
    if invalid_config_seen {
        disable_auto_team(&mut policy);
    }
    policy
}

/// 运行时证据加载：`OWO_TEAM_PAIRED_REPORT` → 二路配对对照报告 → 命中任务组 →
/// 收益判定（失败/缺文件/组未命中 → None，保守 single）。
pub(super) fn load_benefit_verdict_for_runtime(
    policy: &crate::team_benefit::TeamPolicy,
    task_group: &str,
) -> Option<crate::team_benefit::BenefitVerdict> {
    use crate::team_benefit::PairedReport;
    let path = std::env::var("OWO_TEAM_PAIRED_REPORT").ok()?;
    let report = match PairedReport::from_file(std::path::Path::new(&path)) {
        Ok(report) => report,
        Err(error) => {
            tracing::warn!(path = %path, %error, "配对报告不可用，按无证据处理");
            return None;
        }
    };
    let pair = report.pairs.iter().find(|pair| {
        report.policy_group_for(policy, &pair.task_group) == Some(task_group.to_string())
    })?;
    let verdict = crate::team_benefit::evaluate(pair, &policy.thresholds);
    if verdict.eligible {
        Some(verdict)
    } else {
        None
    }
}

/// 任务组推断（模板 id 优先，见 [`benefit_gate_for_runtime`] 说明）。
pub(super) fn infer_benefit_task_group(template_id: Option<&str>, objective: &str) -> String {
    if let Some(template_id) = template_id {
        if template_id.contains("code") {
            return "code".to_string();
        }
        if template_id.contains("research") {
            return "research".to_string();
        }
        if template_id.contains("document") || template_id.contains("extract") {
            return "document".to_string();
        }
    }
    let lower = objective.to_ascii_lowercase();
    let keywords: &[&str] = &[
        "代码",
        "修复",
        "bug",
        "实现",
        "函数",
        "接口",
        "重构",
        "编译",
        "测试用例",
    ];
    let research_hints: &[&str] = &["研究", "调研", "对比", "综述", "分析", "research", "survey"];
    let document_hints: &[&str] = &["文档", "说明书", "报告", "document", "guide", "手册"];
    for kw in keywords {
        if lower.contains(kw) {
            return "code".to_string();
        }
    }
    for kw in research_hints {
        if lower.contains(kw) {
            return "research".to_string();
        }
    }
    for kw in document_hints {
        if lower.contains(kw) {
            return "document".to_string();
        }
    }
    "code".to_string()
}

/// 文本预览（交接摘要/活动流用）。
pub(super) fn preview(text: &str, max: usize) -> String {
    let t = text.trim();
    let take: String = t.chars().take(max).collect();
    if t.chars().count() > max {
        format!("{take}…")
    } else {
        take.to_string()
    }
}

/// 角色 → 产物分类（§6.6 Artifact.kind 语义）。
pub(super) fn role_kind(role: &str) -> &str {
    match role {
        "planner" => "plan",
        "researcher" => "research",
        "builder" => "document",
        "controller" | "verifier" => "verification",
        "critic" => "review",
        "leader" => "final",
        "coordinator" => "coordination",
        other => other,
    }
}

/// 验证断言字符串 → VerificationSpec（`non_empty` / `contains:x` / `equals:x` / 其他=custom）。
pub(super) fn parse_verify(s: &str) -> VerificationSpec {
    if s == "non_empty" {
        VerificationSpec::OutputNonEmpty
    } else if let Some(x) = s.strip_prefix("contains:") {
        VerificationSpec::OutputContains(x.to_string())
    } else if let Some(x) = s.strip_prefix("equals:") {
        VerificationSpec::OutputEquals(x.to_string())
    } else {
        VerificationSpec::Custom(s.to_string())
    }
}

/// Compile the legacy role assertion into a host-known, bounded verification
/// obligation. Custom validator names stay unknown and will be rejected as
/// unsupported by the execution registry.
pub(super) fn verification_plan_for_step(
    step_id: &str,
    spec: &VerificationSpec,
) -> VerificationPlanV1 {
    VerificationPlanV1 {
        plan_id: format!("verify-{step_id}"),
        requirements: vec![crate::verification::requirement_for_spec(step_id, spec)],
    }
}

/// Capability is authoritative when present; role-name inference remains for legacy records.
pub fn is_review_role(role: &str, capabilities: &[String]) -> bool {
    if capabilities.is_empty() {
        return is_review_role_name(role);
    }
    capabilities
        .iter()
        .any(|capability| capability.eq_ignore_ascii_case("review"))
}

/// Legacy role-name inference used only when an old record has no capability declaration.
pub fn is_review_role_name(role: &str) -> bool {
    let role = role.to_ascii_lowercase();
    matches!(
        role.rsplit(['_', '-']).next(),
        Some("critic" | "reviewer" | "review")
    )
}

pub(super) fn worker_role(worker: &str) -> Option<String> {
    worker.strip_prefix("m-").map(str::to_string)
}

/// 预算 JSON → GoalBudget（缺省用默认值）。
pub(super) fn parse_goal_budget(budget: &Value) -> GoalBudget {
    let mut b = GoalBudget::default();
    if let Some(obj) = budget.as_object() {
        if let Some(v) = obj.get("max_steps").and_then(Value::as_u64) {
            b.max_steps = v as u32;
        }
        if let Some(v) = obj.get("max_retries_per_step").and_then(Value::as_u64) {
            b.max_retries_per_step = v as u32;
        }
        if let Some(v) = obj.get("max_total_retries").and_then(Value::as_u64) {
            b.max_total_retries = v as u32;
        }
        if let Some(v) = obj.get("max_replans").and_then(Value::as_u64) {
            b.max_replans = v as u32;
        }
        if let Some(v) = obj.get("max_duration_secs").and_then(Value::as_u64) {
            b.max_duration_secs = v;
        }
        // 十一期：并行开发度（同一 wave 并发步骤数；1..=8 收敛，防误配爆并发）。
        if let Some(v) = obj.get("max_parallel").and_then(Value::as_u64) {
            b.max_parallel = (v as u32).clamp(1, 8);
        }
    }
    b
}
