// R13:WorkSwarm S0 编排层契约测试（§9.0 S0 完成标准）
//! 覆盖：
//! 1. 接力运行全链路（版本化产物经 ref 传递 + 结构化交接 + 交付清单 + 模板提案只提案不启用）；
//! 2. 人节点：等待/唤醒、录入失败校验；
//! 3. 取消：产物保留；取消后 continue 不重跑已完成步骤；
//! 4. steer：只改未完成节点（DecisionRecord 留痕）；运行中 steer → Conflict；
//! 5. 动态团队 Agent 成员 ≤5；
//! 6. 模板采纳后下次组队复用（模板优先）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use owo_agent_core::builtin_team_templates;
use owo_agent_core::goal::{Worker, WorkerRegistry};
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use owo_agent_core::{
    CreateTeamRequest, PhaseOutcome, RoleSpec, RoleWorker, TeamCoordinator, TeamTemplateRegistry,
};
use owo_agent_protocol::{TeamMode, TeamRunStatus};
use serde_json::Value;

const OBJECTIVE: &str = "完成浏览器表单任务并提交报告";

// ---------------------------------------------------------------------------
// 测试内 worker（内层 worker：echo / 慢速 echo）
// ---------------------------------------------------------------------------

struct EchoWorker;

#[async_trait]
impl Worker for EchoWorker {
    fn name(&self) -> &str {
        "echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }
}

struct SlowEchoWorker {
    ms: u64,
}

#[async_trait]
impl Worker for SlowEchoWorker {
    fn name(&self) -> &str {
        "slow-echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        tokio::time::sleep(Duration::from_millis(self.ms)).await;
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }
}

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

struct Harness {
    dir: PathBuf,
    store: Arc<SqliteProjectSpaceStore>,
    coordinator: Arc<TeamCoordinator>,
    audit: Arc<std::sync::Mutex<owo_agent_core::audit::AuditLog>>,
}

fn harness() -> Harness {
    let dir = std::env::temp_dir().join(format!(
        "owo-ws-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(dir.join("runs")).unwrap();
    let store = Arc::new(SqliteProjectSpaceStore::open(&dir.join("space.db")).unwrap());
    let cas = owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap();
    let templates = Arc::new(TeamTemplateRegistry::new(dir.join("templates")));
    let audit = Arc::new(std::sync::Mutex::new(
        owo_agent_core::audit::AuditLog::default(),
    ));
    let mut coordinator = TeamCoordinator::new(
        Arc::clone(&store)
            as Arc<dyn owo_agent_core::project_space_store::ProjectSpaceStoreBackend>,
        templates,
        cas,
        dir.join("runs"),
    );
    coordinator.attach_audit(Arc::clone(&audit));
    Harness {
        dir,
        store,
        coordinator: Arc::new(coordinator),
        audit,
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 按角色表构建运行 worker 注册表（成员名 → RoleWorker；内层 worker 按引用共享）。
fn build_registry(
    h: &Harness,
    team_id: &str,
    inner: Arc<dyn Worker>,
    roles: &[RoleSpec],
) -> WorkerRegistry {
    let registry = WorkerRegistry::new();
    for r in roles {
        let member_id = format!("m-{}", r.role);
        registry.register(Arc::new(RoleWorker::new(
            Arc::clone(&h.coordinator),
            team_id.to_string(),
            member_id,
            r.role.clone(),
            Arc::clone(&inner),
        )));
    }
    registry
}

/// 驱动到第一个非 MoreReady 结果（MoreReady 内部循环推进）。
async fn drive_next(h: &Harness, team_id: &str, registry: &WorkerRegistry) -> PhaseOutcome {
    let max_rounds = 50;
    let mut rounds = 0;
    loop {
        let outcome = h
            .coordinator
            .run_phase(team_id, registry)
            .await
            .unwrap_or_else(|e| panic!("run_phase({team_id}) 失败：{e}"));
        match outcome {
            PhaseOutcome::MoreReady => {
                rounds += 1;
                assert!(
                    rounds < max_rounds,
                    "MoreReady 循环超过 {max_rounds} 轮（疑似死循环）"
                );
            }
            PhaseOutcome::Done => {
                h.coordinator
                    .finalize_success(team_id)
                    .await
                    .expect("收尾失败");
                return outcome;
            }
            other => return other,
        }
    }
}

/// 接力样例角色（planner → builder → critic → leader），内层 worker 指定。
fn relay_roles(worker: &str) -> Vec<RoleSpec> {
    let mut roles = owo_agent_core::workswarm::default_relay_roles();
    for r in &mut roles {
        r.worker = Some(worker.to_string());
    }
    roles
}

// ---------------------------------------------------------------------------
// 1. 接力全链路（§9.0：产物经 ref 传递 + 交接 + 交付清单 + 模板提案）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn relay_run_completes_with_artifacts_handoffs_and_proposal() {
    let h = harness();
    let roles = relay_roles("echo");
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.clone(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();
    assert!(team.status == TeamRunStatus::Created);

    let registry = build_registry(&h, &team_id, Arc::new(EchoWorker), &roles);
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "接力应成功收尾，实际：{outcome:?}"
    );

    // 团队与项目空间终态。
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status == TeamRunStatus::Succeeded);
    let space = h.store.get_project_space(&project_id).await.unwrap();
    assert!(space.status == owo_agent_protocol::ProjectSpaceStatus::Completed);
    assert!(space.delivery_manifest_ref.is_some(), "交付清单必须落盘");

    // 版本化产物链：4 个角色各 1 个产物；builder 引用 planner 的产物（ref 传递）。
    let artifacts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(artifacts.len(), 4, "四角色接力应有 4 个产物");
    let by_kind: HashMap<&str, &owo_agent_protocol::Artifact> =
        artifacts.iter().map(|a| (a.kind.as_str(), a)).collect();
    let planner = by_kind["plan"];
    let builder = by_kind["document"];
    let critic = by_kind["review"];
    let leader = by_kind["final"];
    assert_eq!(builder.source_refs, vec![planner.artifact_id.clone()]);
    assert_eq!(critic.source_refs, vec![builder.artifact_id.clone()]);
    assert_eq!(leader.source_refs, vec![critic.artifact_id.clone()]);
    // CAS 可解引用；内容中上游链可见（echo 把上下文切片（含上游内容）原样输出）。
    let cas = &h.coordinator;
    let leader_content = cas
        .cas()
        .get_text(leader.content_ref.strip_prefix("cas://sha256:").unwrap())
        .unwrap();
    assert!(leader_content.contains(OBJECTIVE), "上下文切片应含团队目标");
    assert!(
        leader_content.contains("planner"),
        "上游链应可见 planner 产物"
    );
    assert!(leader_content.contains("builder"));
    assert!(leader_content.contains("critic"));

    // 结构化交接记录：每步 → 下游（leader 无下游 → "*"）。
    let handoffs = h.store.list_handoffs_by_project(&project_id).await.unwrap();
    assert_eq!(handoffs.len(), 4);
    let ho: HashMap<&str, &owo_agent_protocol::HandoffRecord> = handoffs
        .iter()
        .map(|x| (x.from_member.as_str(), x))
        .collect();
    assert_eq!(ho["m-builder"].to_member, "m-critic");
    assert_eq!(ho["m-critic"].to_member, "m-leader");
    assert_eq!(ho["m-leader"].to_member, "*");
    assert!(!ho["m-planner"].completed_summary.is_empty());

    // 模板提案：只提案，不自动启用。
    let proposals = h.coordinator.templates().list_proposals();
    assert_eq!(proposals.len(), 1);
    let prop = &proposals[0];
    assert_eq!(
        prop.status,
        owo_agent_protocol::TeamTemplateProposalStatus::Proposed
    );
    assert!(
        h.coordinator
            .templates()
            .get_template(&prop.template.template_id)
            .is_none(),
        "提案未采纳前不得进入模板注册表"
    );
    // 审计留痕（关键动作全部落审计）。
    let entries = h.audit.lock().unwrap().entries.clone();
    let events: Vec<&str> = entries.iter().map(|e| e.event.as_str()).collect();
    assert!(events.contains(&"team.created"));
    assert!(events.contains(&"team.handoff"));
    assert!(events.contains(&"team.template_proposed"));
    assert!(events.contains(&"team.succeeded"));
}

// ---------------------------------------------------------------------------
// 2. 人节点：等待 → 录入 → 唤醒
// ---------------------------------------------------------------------------

fn human_relay_roles() -> Vec<RoleSpec> {
    let mut planner = RoleSpec::agent("planner");
    planner.worker = Some("echo".to_string());
    let mut approver = RoleSpec {
        role: "approver".to_string(),
        assignee: "human".to_string(),
        worker: Some("alice".to_string()),
        depends_on: vec!["planner".to_string()],
        handoff_contract: Some("人工确认方案可执行后放行".to_string()),
        verify: Some("non_empty".to_string()),
        extra_input: Value::Null,
    };
    approver.extra_input = Value::Null;
    let mut leader = RoleSpec::agent("leader");
    leader.depends_on = vec!["approver".to_string()];
    leader.worker = Some("echo".to_string());
    vec![planner, approver, leader]
}

#[tokio::test]
async fn human_node_waits_then_resumes_downstream() {
    let h = harness();
    let roles = human_relay_roles();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.clone(),
        budget: Value::Null,
        human_policy: Some("approve".to_string()),
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();
    let registry = build_registry(&h, &team_id, Arc::new(EchoWorker), &roles);

    // 阶段 1：planner 完成 → 人节点就绪 → AwaitingHuman。
    let outcome = drive_next(&h, &team_id, &registry).await;
    let PhaseOutcome::AwaitingHuman { waits } = outcome else {
        panic!("预期 AwaitingHuman，实际：{outcome:?}");
    };
    assert_eq!(waits.len(), 1);
    assert_eq!(waits[0].step_id, "s-approver");
    assert_eq!(waits[0].user_id, "alice");
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status == TeamRunStatus::AwaitingHuman);

    // 录错节点 / 非人节点 → 校验失败。
    let err = h
        .coordinator
        .record_human_result(&team_id, "s-planner", "x")
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            owo_agent_core::workswarm::WorkSwarmError::Validation(_)
        ),
        "对非人节点录入人结果必须被拒绝：{err:?}"
    );

    // 录入人节点结果 → 产物登记 + 步骤完成。
    let artifact = h
        .coordinator
        .record_human_result(&team_id, "s-approver", "批准：方案可以执行")
        .await
        .unwrap();
    assert_eq!(artifact.version, 1);

    // 重复录入 → 冲突。
    let err = h
        .coordinator
        .record_human_result(&team_id, "s-approver", "再次批准")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        owo_agent_core::workswarm::WorkSwarmError::Conflict(_)
    ));

    // 唤醒下游：leader 完成 → 收尾。
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "人节点唤醒后应完成：{outcome:?}"
    );
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status == TeamRunStatus::Succeeded);
    let artifacts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(artifacts.len(), 3);
    // 人节点结果进入产物链（leader 内容可见"批准"）。
    let leader = artifacts
        .iter()
        .find(|a| a.kind == "final")
        .expect("leader 产物缺失");
    let content = h
        .coordinator
        .cas()
        .get_text(leader.content_ref.strip_prefix("cas://sha256:").unwrap())
        .unwrap();
    assert!(content.contains("批准：方案可以执行"));
}

// ---------------------------------------------------------------------------
// 3. 取消：产物保留 + continue 不重跑已完成步骤
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_parked_run_retains_artifacts_and_continue_keeps_progress() {
    let h = harness();
    let roles = human_relay_roles();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.clone(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();
    let registry = build_registry(&h, &team_id, Arc::new(EchoWorker), &roles);
    let _ = drive_next(&h, &team_id, &registry).await; // → AwaitingHuman（planner 已完成）

    // 暂停窗口内取消（steer cancel）。
    let updated = h
        .coordinator
        .apply_steer(&team_id, &owo_agent_core::workswarm::SteerCommand::Cancel)
        .await
        .unwrap();
    assert!(updated.status == TeamRunStatus::Cancelled);
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status.is_terminal());
    // 产物保留：planner 产物仍在。
    let artifacts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(artifacts.len(), 1, "取消后已完成产物必须保留");
    assert_eq!(artifacts[0].kind, "plan");
    // 运行循环收尾：终态 → Finished。
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(matches!(outcome, PhaseOutcome::Finished));

    // continue：重置未完成步骤；planner 不重跑。
    let updated = h
        .coordinator
        .apply_steer(&team_id, &owo_agent_core::workswarm::SteerCommand::Continue)
        .await
        .unwrap();
    assert!(updated.status == TeamRunStatus::Created);
    let _ = h
        .coordinator
        .record_human_result(&team_id, "s-approver", "批准：继续")
        .await
        .unwrap();
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "continue 后应完成：{outcome:?}"
    );
    let artifacts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(artifacts.len(), 3, "planner 不应重跑（产物数 3 而非 4）");
    let plan_artifacts: Vec<_> = artifacts.iter().filter(|a| a.kind == "plan").collect();
    assert_eq!(plan_artifacts.len(), 1, "已完成步骤必须不重跑");
    assert_eq!(plan_artifacts[0].version, 1);
}

// ---------------------------------------------------------------------------
// 4. steer：只改未完成节点 + 运行中 steer → Conflict
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steer_changes_only_uncompleted_nodes_with_decision() {
    let h = harness();
    // 两角色接力：planner 先完成；builder 被 steer 改输入后再执行。
    let mut roles = relay_roles("echo");
    roles.truncate(2); // planner + builder
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.clone(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();
    let registry = build_registry(&h, &team_id, Arc::new(EchoWorker), &roles);

    // 创建后立即 steer 未完成节点（运行未开始 = 未执行）：改盘 + DecisionRecord 留痕；
    // 随后完整运行：新输入生效；已完成节点不可再 steer（结论留痕在 DecisionRecord）。
    let _ = h
        .coordinator
        .apply_steer(
            &team_id,
            &owo_agent_core::workswarm::SteerCommand::Steer {
                step_id: Some("s-builder".to_string()),
                new_input: Some(serde_json::json!({ "text": "STEEERED-INPUT" })),
                note: "聚焦验收要点".to_string(),
            },
        )
        .await
        .unwrap();
    // steer 落盘：DecisionRecord 留痕。
    let decisions = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1, "steer 必须留下 DecisionRecord");
    assert!(decisions[0].choice.contains("聚焦验收要点"));
    assert_eq!(decisions[0].affected_refs, vec!["s-builder".to_string()]);

    // 直接运行到底：builder 使用 steer 后的输入；planner 正常。
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "steer 后应能完成：{outcome:?}"
    );
    let artifacts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    let builder = artifacts.iter().find(|a| a.kind == "document").unwrap();
    let content = h
        .coordinator
        .cas()
        .get_text(builder.content_ref.strip_prefix("cas://sha256:").unwrap())
        .unwrap();
    assert_eq!(
        content, "STEEERED-INPUT",
        "steer 的新输入必须生效（echo 原文返回）"
    );

    // 对已完成节点 steer → 冲突（已完成成果不受影响）。
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &owo_agent_core::workswarm::SteerCommand::Steer {
                step_id: Some("s-planner".to_string()),
                new_input: None,
                note: "改已完成节点".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, owo_agent_core::workswarm::WorkSwarmError::Conflict(_)),
        "已完成节点不可 steer：{err:?}"
    );
}

#[tokio::test]
async fn steer_during_active_run_conflicts_and_cancel_propagates() {
    let h = harness();
    let roles = relay_roles("slow-echo"); // planner 耗时 500ms → 运行窗口可观测
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.clone(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let registry = build_registry(&h, &team_id, Arc::new(SlowEchoWorker { ms: 500 }), &roles);

    // 后台驱动阶段 1（planner 慢速执行中）。
    let coordinator = Arc::clone(&h.coordinator);
    let spawned_registry = registry.clone();
    let spawned_team_id = team_id.clone();
    let run_phase_task = tokio::spawn(async move {
        coordinator
            .run_phase(&spawned_team_id, &spawned_registry)
            .await
    });

    // 等运行窗口开启（run-active 标志）。
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !h.coordinator.is_run_active(&team_id) {
        assert!(std::time::Instant::now() < deadline, "等待 run-active 超时");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 运行中 steer → Conflict（409 语义）。
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &owo_agent_core::workswarm::SteerCommand::Steer {
                step_id: None,
                new_input: None,
                note: "运行中改方向".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, owo_agent_core::workswarm::WorkSwarmError::Conflict(_)),
        "运行中 steer 必须 Conflict：{err:?}"
    );
    // 运行中 cancel → 立即传播，阶段以 Aborted 结束。
    let _ = h
        .coordinator
        .apply_steer(&team_id, &owo_agent_core::workswarm::SteerCommand::Cancel)
        .await
        .unwrap();
    let outcome = run_phase_task.await.unwrap().unwrap();
    assert!(
        matches!(outcome, PhaseOutcome::Aborted),
        "cancel 必须中止运行：{outcome:?}"
    );
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status == TeamRunStatus::Cancelled);
}

// ---------------------------------------------------------------------------
// 5. 动态团队上限（§6.1：不超过 5 个 Agent）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dynamic_team_agent_members_capped_at_five() {
    let h = harness();
    let roles: Vec<RoleSpec> = (0..6)
        .map(|i| {
            let mut r = RoleSpec::agent(format!("r{i}"));
            r.worker = Some("echo".to_string());
            if i > 0 {
                r.depends_on = vec![format!("r{}", i - 1)];
            }
            r
        })
        .collect();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles,
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let err = h.coordinator.create_team_run(&req).await.unwrap_err();
    assert!(
        matches!(
            err,
            owo_agent_core::workswarm::WorkSwarmError::Validation(_)
        ),
        "6 个 Agent 成员必须被拒绝：{err:?}"
    );
    // 5 个 → 允许。
    let roles: Vec<RoleSpec> = (0..5)
        .map(|i| {
            let mut r = RoleSpec::agent(format!("r{i}"));
            r.worker = Some("echo".to_string());
            if i > 0 {
                r.depends_on = vec![format!("r{}", i - 1)];
            }
            r
        })
        .collect();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles,
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    assert_eq!(team.members.len(), 5);
}

// ---------------------------------------------------------------------------
// 6. 模板优先：采纳后下次同形态组队复用（§6.1 / §6.7）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adopted_template_is_reused_for_next_dynamic_run() {
    let h = harness();
    let roles = relay_roles("echo");
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.clone(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let registry = build_registry(&h, &team_id, Arc::new(EchoWorker), &roles);
    let _ = drive_next(&h, &team_id, &registry).await; // 成功 → 产生提案

    let proposals = h.coordinator.templates().list_proposals();
    assert_eq!(proposals.len(), 1);
    let proposal_id = proposals[0].proposal_id.clone();
    // 采纳 → 进注册表。
    let template = h
        .coordinator
        .templates()
        .adopt_proposal(&proposal_id)
        .expect("采纳失败");
    assert!(h
        .coordinator
        .templates()
        .get_template(&template.template_id)
        .is_some());

    // 下一次同形态（team + 相同 objective）动态组队 → 命中模板。
    let req2 = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: Vec::new(), // 空 = 走模板优先
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team2 = h.coordinator.create_team_run(&req2).await.unwrap();
    assert_eq!(
        team2.template_id.as_deref(),
        Some(template.template_id.as_str()),
        "同形态组队必须复用已采纳模板"
    );
    assert_eq!(team2.members.len(), 4, "模板角色数 = 4（接力）");
}

// ---------------------------------------------------------------------------
// 7. swarmflow 必须基于版本化模板
// ---------------------------------------------------------------------------

#[tokio::test]
async fn swarmflow_requires_versioned_template() {
    let h = harness();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Swarmflow,
        template_id: None,
        roles: Vec::new(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let err = h.coordinator.create_team_run(&req).await.unwrap_err();
    assert!(
        matches!(
            err,
            owo_agent_core::workswarm::WorkSwarmError::Validation(_)
        ),
        "swarmflow 无模板必须被拒绝：{err:?}"
    );

    // 提供模板 → 允许（显式 template_id）。
    let tpl = owo_agent_protocol::TeamTemplate {
        template_id: "tpl-fixed".into(),
        name: "固定流程".into(),
        mode: TeamMode::Swarmflow,
        roles: vec![
            owo_agent_protocol::TeamTemplateRole {
                role: "planner".into(),
                assignee: "agent".into(),
                worker: Some("echo".into()),
                depends_on: Vec::new(),
                handoff_contract: None,
                verify: None,
            },
            owo_agent_protocol::TeamTemplateRole {
                role: "builder".into(),
                assignee: "agent".into(),
                worker: Some("echo".into()),
                depends_on: vec!["planner".into()],
                handoff_contract: None,
                verify: None,
            },
        ],
        applicability: OBJECTIVE.into(),
        source_team_id: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    h.coordinator.templates().save_template(&tpl).unwrap();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Swarmflow,
        template_id: Some("tpl-fixed".to_string()),
        roles: Vec::new(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    assert_eq!(team.template_id.as_deref(), Some("tpl-fixed"));
    assert_eq!(team.members.len(), 2);
}

// ---------------------------------------------------------------------------
// 8. 单步运行（single 形态）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_mode_runs_one_step_and_skips_template_proposal() {
    let h = harness();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Single,
        template_id: None,
        roles: vec![{
            let mut r = RoleSpec::agent("runner");
            r.worker = Some("echo".to_string());
            r.verify = Some("non_empty".to_string());
            r
        }],
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    let team_id = team.team_id.clone();
    let registry = build_registry(&h, &team_id, Arc::new(EchoWorker), &req.roles);
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "single 应直接完成：{outcome:?}"
    );
    // single 不产生模板提案（无团队经验可沉淀）。
    assert!(h.coordinator.templates().list_proposals().is_empty());
    let space = h
        .store
        .get_project_space(team.project_space_id.as_deref().unwrap())
        .await
        .unwrap();
    assert!(space.status == owo_agent_protocol::ProjectSpaceStatus::Completed);
}

// ---------------------------------------------------------------------------
// 五期（第四路接管集成）：组队策略——auto 判定默认裁剪、强制 single、决策暴露。
// ---------------------------------------------------------------------------
#[tokio::test]
async fn strategy_auto_trims_default_relay_to_single_and_exposes_decision() {
    let h = harness();
    // 无显式角色、无模板命中（全新 objective）→ 默认接力多角色；auto 判定 single → 裁剪为 1。
    let req = CreateTeamRequest {
        goal_id: None,
        objective: "五期策略判定：单产物简单任务".to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: Vec::new(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    assert_eq!(team.members.len(), 1, "auto+简单任务默认裁剪到单角色");
    let decision = team
        .strategy_decision
        .as_ref()
        .expect("创建响应应带 strategy_decision");
    assert_eq!(decision["mode"], "single");
    assert!(
        decision["reasons"]
            .as_array()
            .map(|r| !r.is_empty())
            .unwrap_or(false),
        "auto 判定必须给出可展示理由"
    );
    assert!(
        decision["budget_calls_total"].is_u64(),
        "预算（调用次数）应暴露"
    );
}

#[tokio::test]
async fn strategy_force_single_trims_explicit_multi_roles() {
    let h = harness();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: relay_roles("echo"),
        budget: Value::Null,
        human_policy: None,
        strategy: Some(owo_agent_core::team_strategy::TeamSelectionMode::ForceSingle),
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    assert_eq!(team.members.len(), 1, "强制 single 必须裁剪显式多角色");
    assert_eq!(team.strategy_decision.as_ref().unwrap()["mode"], "single");
}

#[tokio::test]
async fn strategy_force_team_keeps_explicit_roles() {
    let h = harness();
    let req = CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: relay_roles("echo"),
        budget: Value::Null,
        human_policy: None,
        strategy: Some(owo_agent_core::team_strategy::TeamSelectionMode::ForceTeam),
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    assert_eq!(
        team.members.len(),
        relay_roles("echo").len(),
        "强制 team 保留显式角色"
    );
    assert_eq!(team.strategy_decision.as_ref().unwrap()["mode"], "team");
}

// ---------------------------------------------------------------------------
// 8. 八期一路：自适应角色策略（创建期裁剪 + 运行期跳过 + 提前结束）
// ---------------------------------------------------------------------------

/// 安装内置模板到测试注册表并返回其角色（含依赖）。
fn install_template(h: &Harness, template_id: &str) -> Vec<RoleSpec> {
    let d = builtin_team_templates::descriptor(template_id).expect("内置模板应存在");
    h.coordinator
        .templates()
        .save_template(&d.template)
        .expect("模板安装失败");
    d.template
        .roles
        .iter()
        .cloned()
        .map(RoleSpec::from)
        .collect()
}

#[tokio::test]
async fn adaptive_code_template_trims_reviewer_at_creation() {
    let h = harness();
    let template_roles = install_template(&h, builtin_team_templates::CODE_CHANGE_V1);
    // 角色来自模板（req.roles 为空）→ 创建期裁剪生效。
    let req = CreateTeamRequest {
        goal_id: None,
        objective: "修复 src/calc.rs 的减法符号错误".to_string(),
        mode: TeamMode::Team,
        template_id: Some(builtin_team_templates::CODE_CHANGE_V1.to_string()),
        roles: Vec::new(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    // 简单代码任务：analyzer + implementer（reviewer 被自适应裁剪，减少 1 个 Worker）。
    assert_eq!(team.members.len(), template_roles.len() - 1);
    assert!(!team.members.iter().any(|m| m.role == "reviewer"));
    let adaptive = team
        .strategy_decision
        .as_ref()
        .expect("strategy_decision 应存在")["adaptive"]
        .clone();
    assert_eq!(adaptive["saved_budget_calls"], 3, "reviewer 预算 3 次调用");
    let skipped = adaptive["skipped_roles"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0]["role"], "reviewer");
    assert!(
        skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("analyzer + implementer"),
        "skip_reason 应可展示：{}",
        skipped[0]["reason"]
    );
    // 全链路可运行（DAG 合法且收尾成功）。
    let registry = build_registry(&h, &team.team_id, Arc::new(EchoWorker), &template_roles);
    let outcome = drive_next(&h, &team.team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "裁剪后团队应正常收尾：{outcome:?}"
    );
}

#[tokio::test]
async fn adaptive_research_template_rewrites_deps_and_keeps_one_summarizer() {
    let h = harness();
    let template_roles = install_template(&h, builtin_team_templates::RESEARCH_BRIEF_V1);
    let req = CreateTeamRequest {
        goal_id: None,
        objective: "对比两种缓存淘汰策略并产出研究简报".to_string(),
        mode: TeamMode::Team,
        template_id: Some(builtin_team_templates::RESEARCH_BRIEF_V1.to_string()),
        roles: Vec::new(),
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    // 并行研究保留（researcher_a/b），核验被裁 → 只剩一个汇总角色。
    assert_eq!(team.members.len(), template_roles.len() - 1);
    assert!(team.members.iter().any(|m| m.role == "researcher_a"));
    assert!(team.members.iter().any(|m| m.role == "researcher_b"));
    assert!(team.members.iter().any(|m| m.role == "brief_writer"));
    assert!(!team.members.iter().any(|m| m.role == "evidence_verifier"));
    let adaptive = team.strategy_decision.as_ref().unwrap()["adaptive"].clone();
    assert_eq!(adaptive["skipped_roles"][0]["role"], "evidence_verifier");
    assert_eq!(adaptive["saved_budget_calls"], 3);
    // 依赖重定向生效：brief_writer（原依赖 s-evidence_verifier）若未重写到
    // researcher_a/b，create_team_run 内 plan.validate() 会因依赖缺失直接报错；
    // 能走到这里即证明 DAG 合法，再驱动到成功收尾确认无死锁。
    let registry = build_registry(&h, &team.team_id, Arc::new(EchoWorker), &template_roles);
    let outcome = drive_next(&h, &team.team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "重定向后研究团队应正常收尾（无双路依赖死锁）：{outcome:?}"
    );
}

#[tokio::test]
async fn adaptive_runtime_skip_ends_dag_early_when_no_changes() {
    let h = harness();
    let template_roles = install_template(&h, builtin_team_templates::CODE_CHANGE_V1);
    // 用户显式编排（含 reviewer）→ 创建期尊重不裁剪；reviewer 由运行期跳过兜底。
    let mut roles = template_roles.clone();
    for r in &mut roles {
        r.worker = Some("echo".to_string());
    }
    let req = CreateTeamRequest {
        goal_id: None,
        objective: "重构登录模块的错误处理".to_string(),
        mode: TeamMode::Team,
        template_id: Some(builtin_team_templates::CODE_CHANGE_V1.to_string()),
        roles,
        budget: Value::Null,
        human_policy: None,
        strategy: None,
    };
    let team = h.coordinator.create_team_run(&req).await.unwrap();
    assert_eq!(
        team.members.len(),
        template_roles.len(),
        "显式编排保留 reviewer"
    );
    let adaptive_at_creation = team.strategy_decision.as_ref().unwrap()["adaptive"].clone();
    assert!(
        adaptive_at_creation["skipped_roles"]
            .as_array()
            .unwrap()
            .is_empty(),
        "显式编排创建期不裁剪"
    );
    // 运行：echo worker 不产生任何工作区变更 → reviewer 就绪时被运行期跳过，
    // 全部完成条件满足 → DAG 提前结束（Done + early_exit 指标）。
    let registry = build_registry(&h, &team.team_id, Arc::new(EchoWorker), &template_roles);
    let outcome = drive_next(&h, &team.team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "运行期跳过后应提前收尾：{outcome:?}"
    );
    let team = h.store.get_team_run(&team.team_id).await.unwrap();
    assert_eq!(team.status, TeamRunStatus::Succeeded);
    let adaptive = team.strategy_decision.as_ref().unwrap()["adaptive"].clone();
    let runtime_skipped = adaptive["runtime_skipped"].as_array().unwrap();
    assert_eq!(runtime_skipped.len(), 1);
    assert_eq!(runtime_skipped[0]["role"], "reviewer");
    assert!(
        runtime_skipped[0]["reason"]
            .as_str()
            .unwrap()
            .contains("提前结束"),
        "运行期跳过原因应可展示：{}",
        runtime_skipped[0]["reason"]
    );
    // early_exit_reason 平铺字段应存在（四路冻结口径），reason 含提前结束语义。
    assert!(
        adaptive["early_exit_reason"]
            .as_str()
            .unwrap_or("")
            .contains("提前结束"),
        "early_exit_reason 应存在：{}",
        adaptive["early_exit_reason"]
    );
}
