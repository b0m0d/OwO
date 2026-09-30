use super::util::*;
use super::*;
use crate::plan::VerificationSpec;
use owo_agent_protocol::TeamTemplate;

#[test]
fn default_relay_roles_form_valid_dag() {
    let roles = default_relay_roles();
    assert_eq!(roles.len(), 4);
    let mut plan = Plan::new("p", "g");
    for r in &roles {
        let step = StepSpec {
            id: format!("s-{}", r.role),
            depends_on: r.depends_on.iter().map(|d| format!("s-{d}")).collect(),
            parallel: true,
            worker: format!("m-{}", r.role),
            input: Value::Null,
            verify: r.verify.as_ref().map(|v| parse_verify(v)),
            retries: 0,
        };
        plan.add_step(step);
    }
    plan.validate().unwrap();
}

#[test]
fn parse_verify_kinds() {
    assert!(matches!(
        parse_verify("non_empty"),
        VerificationSpec::OutputNonEmpty
    ));
    assert!(matches!(
        parse_verify("contains:ok"),
        VerificationSpec::OutputContains(_)
    ));
    assert!(matches!(
        parse_verify("equals:x"),
        VerificationSpec::OutputEquals(_)
    ));
    assert!(matches!(
        parse_verify("whatever"),
        VerificationSpec::Custom(_)
    ));
}

#[test]
fn role_kind_mapping() {
    assert_eq!(role_kind("planner"), "plan");
    assert_eq!(role_kind("builder"), "document");
    assert_eq!(role_kind("critic"), "review");
    assert_eq!(role_kind("leader"), "final");
    assert_eq!(role_kind("custom-role"), "custom-role");
}

#[test]
fn template_adopt_is_idempotent_and_reject_blocks() {
    let dir = std::env::temp_dir().join(format!("owo-workswarm-test-{}", now_ms()));
    let reg = TeamTemplateRegistry::new(dir.clone());
    let template = TeamTemplate {
        template_id: "tpl-x".into(),
        name: "test".into(),
        mode: TeamMode::Team,
        roles: Vec::new(),
        applicability: "浏览器 任务".into(),
        source_team_id: Some("team-x".into()),
        created_at: now_ts(),
    };
    let proposal = TeamTemplateProposal {
        proposal_id: "prop-x".into(),
        template: template.clone(),
        source_team_id: "team-x".into(),
        evidence: vec!["art-1".into()],
        status: TeamTemplateProposalStatus::Proposed,
        created_at: now_ts(),
    };
    reg.save_proposal(&proposal).unwrap();
    // 提案不影响注册表（只提案，不自动启用）。
    assert!(reg.get_template("tpl-x").is_none());
    // 采纳 → 进注册表。
    let adopted = reg.adopt_proposal("prop-x").unwrap();
    assert_eq!(adopted.template_id, "tpl-x");
    assert!(reg.get_template("tpl-x").is_some());
    // 幂等。
    let again = reg.adopt_proposal("prop-x").unwrap();
    assert_eq!(again.template_id, "tpl-x");
    // 已采纳不可拒绝。
    assert!(reg.reject_proposal("prop-x").is_err());
    // 匹配：applicability 关键词命中。
    assert!(reg
        .find_match(TeamMode::Team, "完成浏览器表单任务")
        .is_some());
    assert!(reg
        .find_match(TeamMode::Single, "完成浏览器表单任务")
        .is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enriched_input_agent_and_echo_paths() {
    let ctx = json!({
        "team_id": "t", "objective_text": "O", "role": "critic",
        "handoff_contract": "C", "upstream": [{"role":"builder","artifact_id":"a1","version":1,"content":"BODY"}]
    });
    // agent：注入 prompt + read_only（critic 只读）。
    let v = TeamCoordinator::build_enriched_input(&ctx, &json!({}), "agent");
    assert!(v["prompt"].as_str().unwrap().contains("# 角色：critic"));
    assert_eq!(v["read_only"], true);
    // echo：text 承载上下文切片。
    let v = TeamCoordinator::build_enriched_input(&ctx, &json!({}), "echo");
    assert!(v["text"].as_str().unwrap().contains("BODY"));
}
