//! 自适应组队策略引擎测试（R3 第一路）。
//!
//! 只保留策略与路由判定测试（按本轮计划不重跑全量基线）：
//! auto 判定信号（多来源合并/独立评审/风险/历史成功率）、single/team 强制模式、
//! 每角色调用预算、并行度、可展示理由与 JSON 修复标记。

use owo_agent_core::team_strategy::{
    RiskLevel, TaskProfile, TeamSelectionMode, TeamStrategyEngine, TeamStrategyThresholds,
};

fn engine() -> TeamStrategyEngine {
    TeamStrategyEngine::new()
}

fn simple_document() -> TaskProfile {
    TaskProfile {
        category: Some("document".to_string()),
        artifact_count: 1,
        input_count: 1,
        risk: RiskLevel::Low,
        ..TaskProfile::default()
    }
}

fn multi_source_research() -> TaskProfile {
    TaskProfile {
        category: Some("research".to_string()),
        artifact_count: 1,
        input_count: 2,
        risk: RiskLevel::Low,
        ..TaskProfile::default()
    }
}

// ---------------------------------------------------------------------------
// auto 判定
// ---------------------------------------------------------------------------

#[test]
fn auto_simple_document_selects_single_with_reason() {
    let plan = engine().decide(TeamSelectionMode::Auto, &simple_document());
    assert_eq!(plan.mode, "single");
    assert_eq!(plan.requested, "auto");
    assert_eq!(plan.roles.len(), 1);
    assert_eq!(plan.roles[0].role, "writer");
    assert!(!plan.reasons.is_empty(), "auto 模式必须给出可展示理由");
    assert!(
        plan.reasons[0].contains("简单任务"),
        "理由 = {:?}",
        plan.reasons
    );
    assert!(!plan.json_repair);
}

#[test]
fn auto_structured_extract_is_single_plus_json_repair() {
    // 执行器映射：期望 .json → expects_json=true、risk 保持 Low/Normal
    //（格式风险走一次修复机会，而不是触发评审组队——critic 读 JSON 修不了格式）。
    let profile = TaskProfile {
        category: Some("document".to_string()),
        artifact_count: 1,
        input_count: 1,
        risk: RiskLevel::Low,
        expects_json: true,
        ..TaskProfile::default()
    };
    let plan = engine().decide(TeamSelectionMode::Auto, &profile);
    // 结构化提取默认 single（修复机会在执行器层，而不是靠加角色）。
    assert_eq!(plan.mode, "single");
    assert!(plan.json_repair, "JSON 任务必须带一次修复标记");
    assert!(
        plan.reasons.iter().any(|r| r.contains("JSON")),
        "{:?}",
        plan.reasons
    );
}

#[test]
fn auto_multi_source_research_selects_dual_agent_without_critic() {
    let plan = engine().decide(TeamSelectionMode::Auto, &multi_source_research());
    assert_eq!(plan.mode, "team");
    let roles: Vec<&str> = plan.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["researcher", "leader"],
        "多来源合并 = producer + leader 双 Agent"
    );
    assert!(
        plan.reasons.iter().any(|r| r.contains("多来源合并")),
        "理由必须点明多来源合并信号：{:?}",
        plan.reasons
    );
    assert!(
        plan.reasons.iter().any(|r| r.contains("预算")),
        "理由必须含调用预算"
    );
    assert_eq!(plan.parallelism, 1, "链式流水线并行度为 1");
    assert_eq!(plan.budget_calls_total, 4 + 3);
}

#[test]
fn auto_review_signal_adds_critic_only() {
    let profile = TaskProfile {
        needs_independent_review: true,
        ..simple_document()
    };
    let plan = engine().decide(TeamSelectionMode::Auto, &profile);
    assert_eq!(plan.mode, "team");
    let roles: Vec<&str> = plan.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["writer", "critic"],
        "评审信号 = producer + critic 两角色"
    );
    // 无合并信号 → 不启用 leader（普通团队默认两角色）。
    assert!(!roles.contains(&"leader"));
}

#[test]
fn auto_high_risk_adds_critic_and_history_adds_critic() {
    let high_risk = TaskProfile {
        risk: RiskLevel::High,
        ..simple_document()
    };
    let plan = engine().decide(TeamSelectionMode::Auto, &high_risk);
    assert!(plan.roles.iter().any(|r| r.role == "critic"));

    let weak_history = TaskProfile {
        single_agent_success_rate: Some(0.4),
        ..simple_document()
    };
    let plan = engine().decide(TeamSelectionMode::Auto, &weak_history);
    assert!(plan.roles.iter().any(|r| r.role == "critic"));
    assert!(
        plan.reasons.iter().any(|r| r.contains("历史成功率")),
        "{:?}",
        plan.reasons
    );

    // 历史成功率良好（≥0.70）不触发评审。
    let strong_history = TaskProfile {
        single_agent_success_rate: Some(0.9),
        ..simple_document()
    };
    let plan = engine().decide(TeamSelectionMode::Auto, &strong_history);
    assert_eq!(plan.mode, "single");
}

#[test]
fn auto_merge_plus_review_yields_trio() {
    let profile = TaskProfile {
        needs_independent_review: true,
        artifact_count: 2,
        ..simple_document()
    };
    let plan = engine().decide(TeamSelectionMode::Auto, &profile);
    let roles: Vec<&str> = plan.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(roles, vec!["writer", "critic", "leader"]);
    assert_eq!(plan.budget_calls_total, 4 + 2 + 3);
}

// ---------------------------------------------------------------------------
// 强制模式
// ---------------------------------------------------------------------------

#[test]
fn force_single_overrides_all_signals() {
    let engine = engine();
    for mut profile in [simple_document(), multi_source_research()] {
        profile.needs_independent_review = true;
        profile.risk = RiskLevel::High;
        let plan = engine.decide(TeamSelectionMode::ForceSingle, &profile);
        assert_eq!(plan.mode, "single");
        assert_eq!(plan.roles.len(), 1);
        assert!(plan.reasons.iter().any(|r| r.contains("强制 single")));
    }
}

#[test]
fn force_team_keeps_full_pipeline() {
    let plan = engine().decide(TeamSelectionMode::ForceTeam, &simple_document());
    assert_eq!(plan.mode, "team");
    let roles: Vec<&str> = plan.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["writer", "critic", "leader"],
        "显式 team = 完整流水线"
    );
    assert!(plan.reasons.iter().any(|r| r.contains("强制 team")));
}

// ---------------------------------------------------------------------------
// 预算与解析
// ---------------------------------------------------------------------------

#[test]
fn role_budgets_and_lookup_follow_plan() {
    let plan = engine().decide(TeamSelectionMode::Auto, &multi_source_research());
    assert_eq!(plan.budget_for("researcher"), 4);
    assert_eq!(plan.budget_for("leader"), 3);
    assert_eq!(plan.budget_for("critic"), 4, "不在计划中的角色退回缺省 4");
    assert_eq!(
        plan.budget_calls_total,
        plan.roles.iter().map(|r| r.budget_calls).sum::<usize>()
    );
}

#[test]
fn thresholds_are_configurable() {
    let engine = TeamStrategyEngine::with_thresholds(TeamStrategyThresholds {
        multi_source_inputs: 3,
        ..TeamStrategyThresholds::default()
    });
    // 2 份输入不再触发合并信号 → single。
    let plan = engine.decide(TeamSelectionMode::Auto, &multi_source_research());
    assert_eq!(plan.mode, "single");
}

#[test]
fn selection_mode_parse_roundtrip() {
    assert_eq!(
        TeamSelectionMode::parse("single").unwrap(),
        TeamSelectionMode::ForceSingle
    );
    assert_eq!(
        TeamSelectionMode::parse("TEAM").unwrap(),
        TeamSelectionMode::ForceTeam
    );
    assert_eq!(
        TeamSelectionMode::parse(" auto ").unwrap(),
        TeamSelectionMode::Auto
    );
    assert!(TeamSelectionMode::parse("swarm").is_err());
    assert_eq!(TeamSelectionMode::default(), TeamSelectionMode::Auto);
    // JSON 序列化为 snake_case（API/UI 契约）。
    assert_eq!(
        serde_json::to_string(&TeamSelectionMode::ForceSingle).unwrap(),
        "\"single\""
    );
}

#[test]
fn producer_role_follows_category() {
    let mut profile = simple_document();
    profile.category = Some("code".to_string());
    let plan = engine().decide(TeamSelectionMode::ForceSingle, &profile);
    assert_eq!(plan.roles[0].role, "builder");
    profile.category = Some("research".to_string());
    let plan = engine().decide(TeamSelectionMode::ForceSingle, &profile);
    assert_eq!(plan.roles[0].role, "researcher");
    profile.category = None;
    let plan = engine().decide(TeamSelectionMode::ForceSingle, &profile);
    assert_eq!(plan.roles[0].role, "producer");
}

#[test]
fn plan_serializes_for_ui() {
    let plan = engine().decide(TeamSelectionMode::Auto, &multi_source_research());
    let json = serde_json::to_value(&plan).unwrap();
    assert_eq!(json["mode"], "team");
    assert_eq!(json["requested"], "auto");
    assert!(json["roles"].as_array().unwrap().len() == 2);
    assert!(json["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.is_string()));
    assert!(json["budget_calls_total"].is_number());
}
