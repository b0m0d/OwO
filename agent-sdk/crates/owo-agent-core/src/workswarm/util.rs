use crate::goal::GoalBudget;
use crate::plan::VerificationSpec;
use serde_json::Value;
use std::path::Path;
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
///   `evals/v1/team-policy.json`（工作区根/当前目录向上探测）；再缺省用
///   [`crate::team_benefit::TeamPolicy::embedded_defaults`]（与 evals/v1 语义一致，
///   测试与无配置文件时使用）。解析失败一律回退内嵌默认并留 warning——绝不因
///   策略文件损坏阻断团队创建。
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
) -> (
    crate::team_benefit::PolicyGate,
    Option<crate::team_benefit::BenefitVerdict>,
    String,
) {
    let policy = load_team_policy_for_runtime(run_dir);
    let task_group = infer_benefit_task_group(template_id, objective);
    let verdict = load_benefit_verdict_for_runtime(&policy, &task_group);
    let current = crate::team_benefit::BenefitBindings {
        model: std::env::var("OPENAI_MODEL").ok(),
        template: template_id.map(str::to_string),
        task_set: None,
        strategy_version: policy.strategy_version.clone(),
    };
    let gate = crate::team_benefit::gate_auto(
        &policy,
        &task_group,
        verdict.as_ref(),
        Some(&current),
        &now_ts(),
    );
    let evidence = if verdict.is_some() {
        "有配对报告证据".to_string()
    } else {
        "无配对报告证据（默认 single，等二路验收报告）".to_string()
    };
    (gate, verdict, evidence)
}

/// 运行时策略加载（见 [`benefit_gate_for_runtime`] 说明）。
pub(super) fn load_team_policy_for_runtime(run_dir: &Path) -> crate::team_benefit::TeamPolicy {
    use crate::team_benefit::TeamPolicy;
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(path) = std::env::var("OWO_TEAM_POLICY") {
        candidates.push(std::path::PathBuf::from(path));
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
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            match TeamPolicy::from_json(&text) {
                Ok(policy) => return policy,
                Err(error) => {
                    tracing::warn!(candidate = %candidate.display(), %error, "team-policy.json 解析失败，回退内嵌默认");
                }
            }
        }
    }
    TeamPolicy::embedded_defaults()
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

pub(super) fn is_critic_role(role: &str) -> bool {
    role == "critic"
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
    }
    b
}
