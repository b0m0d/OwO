// R13:WorkSwarm R2 恢复契约测试（局部重试 + 进程重启中断恢复 + 状态损坏保护）
//! 覆盖（第三路冻结语义）：
//! 1. retry：仅重置目标 Failed/Aborted 步骤及其未完成下游闭包；
//!    已成功步骤执行次数不增加；既有 Artifact 版本/CAS ref、Handoff、DecisionRecord 不动；
//! 2. retry 校验：Succeeded 目标 → Conflict、未知步骤 → NotFound、未运行目标 → Conflict；
//!    全部拒绝路径零写副作用；
//! 3. 运行中 retry → Conflict（409 语义）；
//! 4. 重复发送同一 retry → Conflict 且状态文件字节级不变（无额外副作用）；
//! 5. 进程重启：磁盘 Running + 无活动循环 → 识别为 interrupted（Running 步骤转可恢复），
//!    continue 显式恢复；
//! 6. retry 恢复中断步骤（不触碰已成功兄弟分支）；
//! 7. 状态文件损坏 → 明确失败（CorruptState），原文件字节级保留，扫描不覆盖。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use owo_agent_core::goal::{StepRecord, Worker, WorkerRegistry};
use owo_agent_core::plan::StepStatus;
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use owo_agent_core::workswarm::{
    SteerCommand, TeamCoordinator, TeamTemplateRegistry, WorkSwarmError,
};
use owo_agent_core::{CreateTeamRequest, PhaseOutcome, RoleSpec, RoleWorker};
use owo_agent_protocol::{Artifact, TeamMode, TeamRunStatus};
use serde_json::Value;

const OBJECTIVE: &str = "完成浏览器表单任务并提交报告";

// ---------------------------------------------------------------------------
// 测试内 worker
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

/// 慢速 echo（打开「运行中」观测窗口）。
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

/// 按成员路由：指定成员慢速，其余即时（单 registry 覆盖混合速度场景）。
struct RouterWorker {
    slow_member: String,
    slow_ms: u64,
}

#[async_trait]
impl Worker for RouterWorker {
    fn name(&self) -> &str {
        "router"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let member = input
            .get("_workswarm")
            .and_then(|w| w.get("member_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if member == self.slow_member {
            tokio::time::sleep(Duration::from_millis(self.slow_ms)).await;
        }
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }
}

/// 注入失败的 echo：`fail_step` 前 `fail_times` 次执行失败；输出带全局执行序号
/// （内容随重跑变化，可区分「重跑过」与「没跑过」）。
struct FlakyEchoWorker {
    fail_step: String,
    remaining: AtomicU32,
    runs: AtomicU32,
}

impl FlakyEchoWorker {
    fn new(fail_step: &str, fail_times: u32) -> Self {
        Self {
            fail_step: fail_step.to_string(),
            remaining: AtomicU32::new(fail_times),
            runs: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Worker for FlakyEchoWorker {
    fn name(&self) -> &str {
        "flaky-echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let step = input
            .get("_workswarm")
            .and_then(|w| w.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let n = self.runs.fetch_add(1, Ordering::SeqCst) + 1;
        let text = input
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if step == self.fail_step && self.remaining.load(Ordering::SeqCst) > 0 {
            self.remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(format!("注入失败（第 {n} 次执行）"));
        }
        Ok(format!("{text}#run{n}"))
    }
}

// ---------------------------------------------------------------------------
// 脚手架
// ---------------------------------------------------------------------------

struct Harness {
    dir: PathBuf,
    store: Arc<SqliteProjectSpaceStore>,
    coordinator: Arc<TeamCoordinator>,
    audit: Arc<std::sync::Mutex<owo_agent_core::audit::AuditLog>>,
}

fn harness() -> Harness {
    let dir = std::env::temp_dir().join(format!(
        "owo-wsrec-{}-{}-{}",
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

/// 在同一数据目录上打开第二个协调器（模拟进程重启：运行标志/循环存活表全空）。
fn reopen(h: &Harness) -> Arc<TeamCoordinator> {
    let store = SqliteProjectSpaceStore::open(&h.dir.join("space.db")).unwrap();
    let cas = owo_agent_core::cas_store::CasStore::new(h.dir.join("cas")).unwrap();
    let templates = Arc::new(TeamTemplateRegistry::new(h.dir.join("templates")));
    let mut coordinator = TeamCoordinator::new(
        Arc::new(store) as Arc<dyn owo_agent_core::project_space_store::ProjectSpaceStoreBackend>,
        templates,
        cas,
        h.dir.join("runs"),
    );
    coordinator.attach_audit(Arc::clone(&h.audit));
    Arc::new(coordinator)
}

/// 在指定协调器上按角色表构建运行 worker 注册表（成员名 → RoleWorker）。
fn build_registry(
    coordinator: &Arc<TeamCoordinator>,
    team_id: &str,
    inner: Arc<dyn Worker>,
    roles: &[RoleSpec],
) -> WorkerRegistry {
    let registry = WorkerRegistry::new();
    for r in roles {
        let member_id = format!("m-{}", r.role);
        registry.register(Arc::new(RoleWorker::new(
            Arc::clone(coordinator),
            team_id.to_string(),
            member_id,
            r.role.clone(),
            Arc::clone(&inner),
        )));
    }
    registry
}

/// 驱动到第一个非 MoreReady 结果；Done 时执行成功收尾。
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
                assert!(rounds < max_rounds, "MoreReady 循环超过 {max_rounds} 轮");
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

/// 在指定协调器上驱动到底（恢复后由新协调器接管）。
async fn drive_next_on(
    coordinator: &Arc<TeamCoordinator>,
    team_id: &str,
    registry: &WorkerRegistry,
) -> PhaseOutcome {
    let max_rounds = 50;
    let mut rounds = 0;
    loop {
        let outcome = coordinator
            .run_phase(team_id, registry)
            .await
            .unwrap_or_else(|e| panic!("run_phase({team_id}) 失败：{e}"));
        match outcome {
            PhaseOutcome::MoreReady => {
                rounds += 1;
                assert!(rounds < max_rounds, "MoreReady 循环超过 {max_rounds} 轮");
            }
            PhaseOutcome::Done => {
                coordinator
                    .finalize_success(team_id)
                    .await
                    .expect("收尾失败");
                return outcome;
            }
            other => return other,
        }
    }
}

/// 四角色接力（planner → builder → critic → leader）。
fn relay_roles(worker: &str) -> Vec<RoleSpec> {
    let mut roles = owo_agent_core::workswarm::default_relay_roles();
    for r in &mut roles {
        r.worker = Some(worker.to_string());
    }
    roles
}

/// 分支任务图：planner 根；builder（下游 reviewer）与 verifier 两条并行支路。
/// retry(builder) 的下游闭包 = {s-reviewer}——用于断言「只重置目标 + 未完成下游」。
fn branch_roles(worker: &str) -> Vec<RoleSpec> {
    let mut planner = RoleSpec::agent("planner");
    planner.worker = Some(worker.to_string());
    planner.verify = Some("non_empty".to_string());
    let mut builder = RoleSpec::agent("builder");
    builder.worker = Some(worker.to_string());
    builder.depends_on = vec!["planner".to_string()];
    builder.verify = Some("non_empty".to_string());
    let mut reviewer = RoleSpec::agent("reviewer");
    reviewer.worker = Some(worker.to_string());
    reviewer.depends_on = vec!["builder".to_string()];
    reviewer.verify = Some("non_empty".to_string());
    let mut verifier = RoleSpec::agent("verifier");
    verifier.worker = Some(worker.to_string());
    verifier.depends_on = vec!["planner".to_string()];
    verifier.verify = Some("non_empty".to_string());
    vec![planner, builder, reviewer, verifier]
}

fn create_req(roles: &[RoleSpec]) -> CreateTeamRequest {
    CreateTeamRequest {
        goal_id: None,
        objective: OBJECTIVE.to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles: roles.to_vec(),
        budget: Value::Null,
        human_policy: None,
    }
}

fn record_of(h: &Harness, team_id: &str, step_id: &str) -> StepRecord {
    h.coordinator
        .load_run_state(team_id)
        .unwrap_or_else(|e| panic!("读取运行状态失败：{e}"))
        .records
        .get(step_id)
        .cloned()
        .unwrap_or_else(|| panic!("步骤 {step_id} 缺少记录"))
}

async fn artifact_of_kind(h: &Harness, project_id: &str, kind: &str) -> Artifact {
    h.store
        .list_artifacts_by_project(project_id)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.kind == kind)
        .unwrap_or_else(|| panic!("缺少 {kind} 产物"))
}

/// 轮询条件成立（超时 panic）。
async fn wait_for(cond: impl Fn() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(std::time::Instant::now() < deadline, "等待超时：{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 直接改写磁盘状态文件：把指定步骤置为 Running（模拟「崩溃时正在执行」的落盘现场），
/// 并把 goal 置为 Running。绕过协调器（等价于崩溃后遗留的磁盘字节）。
fn tamper_step_running(h: &Harness, team_id: &str, step_id: &str) {
    let path = h.dir.join("runs").join(format!("{team_id}.json"));
    let raw = std::fs::read_to_string(&path).expect("状态文件应存在");
    let mut v: Value = serde_json::from_str(&raw).expect("状态文件应为合法 JSON");
    v["records"][step_id]["status"] = Value::String("Running".to_string());
    v["records"][step_id]["attempts"] = serde_json::json!(1);
    v["goal"]["status"] = Value::String("Running".to_string());
    std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

/// 把状态文件整体替换为损坏内容（返回原字节用于最终比对）。
fn corrupt_state_file(h: &Harness, team_id: &str) -> String {
    let path = h.dir.join("runs").join(format!("{team_id}.json"));
    let broken = "{{{corrupted-not-json".to_string();
    std::fs::write(&path, &broken).unwrap();
    broken
}

// ---------------------------------------------------------------------------
// 1. retry：局部重置 + 已成功步骤/产物不动
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_failed_step_resets_only_target_closure() {
    let h = harness();
    let roles = branch_roles("echo");
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();

    // builder 第一次执行失败 → 团队 Failed（planner/verifier 已成功并各自落 1 个产物）。
    let registry = build_registry(
        &h.coordinator,
        &team_id,
        Arc::new(FlakyEchoWorker::new("s-builder", 1)),
        &roles,
    );
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Failed),
        "预期失败：{outcome:?}"
    );

    // 失败现场快照。
    let arts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(
        arts.len(),
        2,
        "失败前应只有 planner/verifier 两个产物：{arts:?}"
    );
    let plan_before = artifact_of_kind(&h, &project_id, "plan").await;
    assert_eq!(plan_before.version, 1);
    let planner_attempts_before = record_of(&h, &team_id, "s-planner").attempts;
    let verifier_attempts_before = record_of(&h, &team_id, "s-verifier").attempts;
    assert_eq!(planner_attempts_before, 1);
    assert_eq!(verifier_attempts_before, 1);

    // retry 目标失败步骤（冻结契约：{"command":"retry","step_id":"builder","note":…}）。
    let updated = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-builder".to_string(),
                note: "修复输入后重试".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(updated.status == TeamRunStatus::Created);

    // 只重置目标 + 未完成下游（reviewer）；已成功步骤原样。
    let state = h.coordinator.load_run_state(&team_id).unwrap();
    assert_eq!(state.records["s-builder"].status, StepStatus::Pending);
    assert_eq!(state.records["s-reviewer"].status, StepStatus::Pending);
    assert_eq!(state.records["s-planner"].status, StepStatus::Succeeded);
    assert_eq!(
        record_of(&h, &team_id, "s-planner").attempts,
        planner_attempts_before
    );
    assert_eq!(
        record_of(&h, &team_id, "s-verifier").attempts,
        verifier_attempts_before
    );

    // DecisionRecord：affected = 目标 + 下游闭包。
    let decisions = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1, "retry 必须留下 DecisionRecord");
    assert!(
        decisions[0].choice.contains("retry"),
        "{:?}",
        decisions[0].choice
    );
    assert!(decisions[0].choice.contains("修复输入后重试"));
    let mut expected_refs = vec!["s-builder".to_string(), "s-reviewer".to_string()];
    expected_refs.sort();
    let mut actual_refs = decisions[0].affected_refs.clone();
    actual_refs.sort();
    assert_eq!(
        actual_refs, expected_refs,
        "affected_refs 应为目标 + 未完成下游闭包"
    );

    // 重跑到底：builder 成功（第 2 次执行），已成功步骤不再执行。
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "retry 后应完成：{outcome:?}"
    );

    // attempts 语义与 steer_continue 一致：重置时清零计数 → 本次窗口内 builder 恰好执行 1 次。
    // 真正的「第二次执行」证明来自 FlakyEcho 输出序号（前三名执行者 planner/builder/verifier
    // 已消耗 #run1..#run3，本次必然是 #run4）。
    let builder_record = record_of(&h, &team_id, "s-builder");
    assert_eq!(builder_record.attempts, 1, "重置后本窗口应恰好执行一次");
    assert!(
        builder_record
            .output
            .as_deref()
            .unwrap_or_default()
            .ends_with("#run4"),
        "builder 应以新执行（#run4）完成，而非复用旧结果：{:?}",
        builder_record.output
    );
    assert_eq!(
        record_of(&h, &team_id, "s-planner").attempts,
        planner_attempts_before,
        "已成功步骤执行次数不得增加"
    );
    assert_eq!(
        record_of(&h, &team_id, "s-verifier").attempts,
        verifier_attempts_before
    );
    assert_eq!(record_of(&h, &team_id, "s-reviewer").attempts, 1);

    // 产物：全部 4 个；planner 产物版本/CAS ref 不变（未重跑）。
    let arts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(
        arts.len(),
        4,
        "四步骤各 1 个产物（retry 不产生 plan/verification 新版本）"
    );
    let plan_after = arts.iter().find(|a| a.kind == "plan").unwrap();
    assert_eq!(plan_after.artifact_id, plan_before.artifact_id);
    assert_eq!(plan_after.version, 1);
    assert_eq!(
        plan_after.content_ref, plan_before.content_ref,
        "已有 Artifact 的 CAS ref 必须保持不变"
    );
    assert_eq!(
        arts.iter().filter(|a| a.kind == "verification").count(),
        1,
        "verifier 未重跑（无新版本产物）"
    );

    // 交接记录：每步 1 条（不因 retry 重复）；决策仍只有 retry 一条。
    let handoffs = h.store.list_handoffs_by_project(&project_id).await.unwrap();
    assert_eq!(handoffs.len(), 4);
    let decisions = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1, "完整重跑不新增 DecisionRecord");
}

// ---------------------------------------------------------------------------
// 2. retry 校验：非法目标全部拒绝且零写副作用
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_target_validation_rejects_without_side_effects() {
    let h = harness();
    let roles = branch_roles("echo");
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();

    // 未知步骤 → NotFound（404 语义）。
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-ghost".to_string(),
                note: "x".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::NotFound(_)),
        "未知步骤必须 NotFound：{err:?}"
    );

    // 从未运行的步骤（Pending）→ Conflict：仅 Failed/Aborted/中断可重试。
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-planner".to_string(),
                note: "x".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::Conflict(_)),
        "Pending 步骤不可 retry：{err:?}"
    );

    // 拒绝路径不产生任何 DecisionRecord。
    let decisions = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap();
    assert!(
        decisions.is_empty(),
        "拒绝的 retry 不得留决策：{decisions:?}"
    );

    // 跑成功后再对已成功步骤 retry → Conflict（重复发送同一 retry 不产生额外副作用）。
    let registry = build_registry(&h.coordinator, &team_id, Arc::new(EchoWorker), &roles);
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(matches!(outcome, PhaseOutcome::Done));
    let arts_before = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-builder".to_string(),
                note: "重复发送".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::Conflict(ref m) if m.contains("不产生额外副作用")),
        "已成功目标必须 Conflict 且说明幂等语义：{err:?}"
    );
    let arts_after = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(arts_before.len(), arts_after.len(), "拒绝路径不得新增产物");
    let decisions = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap();
    assert!(
        decisions.is_empty(),
        "拒绝 retry 也不得新增决策：{decisions:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. 运行中 retry → Conflict（409 语义）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_during_active_run_conflicts() {
    let h = harness();
    // 单步骤团队：一个阶段即 Done，阶段结果断言确定。
    let roles = vec![{
        let mut r = RoleSpec::agent("runner");
        r.worker = Some("slow-echo".to_string());
        r.verify = Some("non_empty".to_string());
        r
    }];
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();

    let registry = build_registry(
        &h.coordinator,
        &team_id,
        Arc::new(SlowEchoWorker { ms: 600 }),
        &roles,
    );
    let coordinator = Arc::clone(&h.coordinator);
    let registry_for_task = registry.clone();
    let task_team = team_id.clone();
    let task =
        tokio::spawn(async move { coordinator.run_phase(&task_team, &registry_for_task).await });

    // 等阶段真正开跑。
    wait_for(|| h.coordinator.is_run_active(&team_id), "run-active 标志").await;

    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-runner".to_string(),
                note: "运行中重试".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::Conflict(ref m) if m.contains("运行")),
        "运行中 retry 必须 Conflict：{err:?}"
    );

    // 阶段自然结束（slow-echo 成功），团队可正常收尾——运行中拒绝不影响后续。
    let outcome = task.await.unwrap().unwrap();
    assert!(
        matches!(outcome, PhaseOutcome::Done),
        "阶段应完成：{outcome:?}"
    );
    h.coordinator.finalize_success(&team_id).await.unwrap();
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status == TeamRunStatus::Succeeded);
}

// ---------------------------------------------------------------------------
// 4. 重复 retry：无额外副作用（状态文件字节级不变）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_retry_produces_no_extra_effects() {
    let h = harness();
    let roles = branch_roles("echo");
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();

    let registry = build_registry(
        &h.coordinator,
        &team_id,
        Arc::new(FlakyEchoWorker::new("s-builder", 1)),
        &roles,
    );
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(matches!(outcome, PhaseOutcome::Failed));

    // 第一次 retry：成功重置。
    let updated = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-builder".to_string(),
                note: "修复输入后重试".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(updated.status == TeamRunStatus::Created);

    // 快照：状态文件字节 + 决策数 + 活动流长度。
    let state_path = h.dir.join("runs").join(format!("{team_id}.json"));
    let bytes_before = std::fs::read(&state_path).unwrap();
    let decisions_before = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap()
        .len();
    let space_before = h.store.get_project_space(&project_id).await.unwrap();
    assert_eq!(decisions_before, 1);

    // 第二次同一 retry → Conflict（目标已重置为 Pending），且零写副作用。
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-builder".to_string(),
                note: "修复输入后重试".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::Conflict(_)),
        "重复 retry 必须 Conflict：{err:?}"
    );
    let bytes_after = std::fs::read(&state_path).unwrap();
    assert_eq!(
        bytes_before, bytes_after,
        "重复 retry 不得改动状态文件（字节级）"
    );
    let decisions_after = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap()
        .len();
    assert_eq!(decisions_before, decisions_after, "重复 retry 不得新增决策");
    let space_after = h.store.get_project_space(&project_id).await.unwrap();
    assert_eq!(
        space_before.activity_stream.len(),
        space_after.activity_stream.len(),
        "重复 retry 不得追加活动流"
    );

    // 跑完后再来第三次 → 仍 Conflict（已成功目标），决策数不变。
    let outcome = drive_next(&h, &team_id, &registry).await;
    assert!(matches!(outcome, PhaseOutcome::Done));
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-builder".to_string(),
                note: "重复发送".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WorkSwarmError::Conflict(_)));
    let decisions_final = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap()
        .len();
    assert_eq!(decisions_final, 1, "全程只应有一次 retry 决策");
}

// ---------------------------------------------------------------------------
// 5. 进程重启：磁盘 Running 无活动循环 → interrupted；continue 显式恢复
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_leftover_running_detected_then_continue_recovers() {
    let h = harness();
    let roles = relay_roles("echo");
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();

    // 阶段 1（planner 慢速 800ms，其余即时）在后台执行……
    let registry = build_registry(
        &h.coordinator,
        &team_id,
        Arc::new(RouterWorker {
            slow_member: "m-planner".to_string(),
            slow_ms: 800,
        }),
        &roles,
    );
    let coordinator1 = Arc::clone(&h.coordinator);
    let registry_for_task = registry.clone();
    let task_team = team_id.clone();
    let task =
        tokio::spawn(async move { coordinator1.run_phase(&task_team, &registry_for_task).await });

    // 等「阶段活动」+「磁盘状态 → Running」（R2：阶段开批写盘）。
    wait_for(|| h.coordinator.is_run_active(&team_id), "run-active 标志").await;
    let disk_running = Arc::new(AtomicBool::new(false));
    {
        let store = Arc::clone(&h.store);
        let tid = team_id.clone();
        let flag = Arc::clone(&disk_running);
        tokio::spawn(async move {
            for _ in 0..500 {
                if let Ok(t) = store.get_team_run(&tid).await {
                    if t.status == TeamRunStatus::Running {
                        flag.store(true, Ordering::SeqCst);
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }
    wait_for(|| disk_running.load(Ordering::SeqCst), "磁盘状态 → Running").await;

    // ——模拟崩溃：直接杀掉运行任务（阶段内未 merge，磁盘保留 Running 现场）。
    task.abort();
    let _ = task.await;

    // ——补齐「崩溃时 planner 正在执行」的记录现场（真实崩溃由进程消亡产生；
    //    记录级 Running 落盘需要 mid-merge 场景，这里直写状态文件等价构造）。
    tamper_step_running(&h, &team_id, "s-planner");

    // ——模拟进程重启：全新协调器（运行标志/循环存活表为空）。
    let coordinator2 = reopen(&h);
    assert!(!coordinator2.is_run_active(&team_id));
    assert!(!coordinator2.is_loop_alive(&team_id));

    // 识别为 interrupted。
    let scan = coordinator2.detect_interrupted().await.unwrap();
    assert_eq!(scan.interrupted.len(), 1, "应识别到 1 个中断团队：{scan:?}");
    assert_eq!(scan.interrupted[0].team_id, team_id);
    assert_eq!(
        scan.interrupted[0].interrupted_steps,
        vec!["s-planner".to_string()]
    );
    assert_eq!(scan.unreadable_states.len(), 0);
    assert!(coordinator2.is_interrupted(&team_id), "识别后应有中断标记");

    // 磁盘：Running 步骤转为可恢复（Aborted）+ 中断说明。
    let state = coordinator2.load_run_state(&team_id).unwrap();
    assert_eq!(
        state.records["s-planner"].status,
        StepStatus::Aborted,
        "中断步骤应转为可恢复状态"
    );
    assert!(
        state.records["s-planner"]
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("中断"),
        "中断说明应写入记录：{:?}",
        state.records["s-planner"].error
    );

    // 幂等：再次扫描不重复报告。
    let scan2 = coordinator2.detect_interrupted().await.unwrap();
    assert!(
        scan2.interrupted.is_empty(),
        "已识别团队不得重复报告：{scan2:?}"
    );

    // ——显式 continue 恢复（绝不静默自动重放）。
    let updated = coordinator2
        .apply_steer(&team_id, &SteerCommand::Continue)
        .await
        .unwrap();
    assert!(updated.status == TeamRunStatus::Created);
    assert!(!coordinator2.is_interrupted(&team_id), "恢复后标记应清除");
    let state = coordinator2.load_run_state(&team_id).unwrap();
    assert_eq!(state.records["s-planner"].status, StepStatus::Pending);

    // 新协调器驱动到底：各步骤各执行 1 次、各 1 个产物（无重复执行）。
    let registry2 = build_registry(&coordinator2, &team_id, Arc::new(EchoWorker), &roles);
    let outcome = drive_next_on(&coordinator2, &team_id, &registry2).await;
    assert!(matches!(outcome, PhaseOutcome::Done));
    let team = h.store.get_team_run(&team_id).await.unwrap();
    assert!(team.status == TeamRunStatus::Succeeded);
    let arts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(arts.len(), 4, "恢复后四步骤各 1 个产物（无重复执行）");
}

// ---------------------------------------------------------------------------
// 6. retry 恢复中断步骤（不触碰已成功兄弟分支）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_recovers_interrupted_step_without_touching_siblings() {
    let h = harness();
    let roles = branch_roles("echo");
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();
    let project_id = team.project_space_id.clone().unwrap();

    // 阶段 1：planner（唯一就绪）完成 → MoreReady（planner 产物已落盘）。
    let registry = build_registry(&h.coordinator, &team_id, Arc::new(EchoWorker), &roles);
    let outcome = h.coordinator.run_phase(&team_id, &registry).await.unwrap();
    assert!(matches!(outcome, PhaseOutcome::MoreReady));
    let plan_before = artifact_of_kind(&h, &project_id, "plan").await;
    assert_eq!(
        record_of(&h, &team_id, "s-planner").status,
        StepStatus::Succeeded
    );

    // 模拟崩溃遗留：磁盘 Running + builder（wave2 成员）记录为 Running。
    tamper_step_running(&h, &team_id, "s-builder");

    // 重启识别（新协调器：无活动循环）。
    let coordinator2 = reopen(&h);
    let scan = coordinator2.detect_interrupted().await.unwrap();
    assert_eq!(scan.interrupted.len(), 1);
    assert_eq!(
        scan.interrupted[0].interrupted_steps,
        vec!["s-builder".to_string()]
    );

    // 显式 retry 恢复目标步骤。
    let updated = coordinator2
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-builder".to_string(),
                note: "重启后定向恢复".to_string(),
            },
        )
        .await
        .unwrap();
    assert!(updated.status == TeamRunStatus::Created);
    assert!(!coordinator2.is_interrupted(&team_id));

    // 已成功兄弟（planner）不受影响：状态/次数/产物 ref 全部保持。
    assert_eq!(
        record_of(&h, &team_id, "s-planner").status,
        StepStatus::Succeeded
    );
    assert_eq!(record_of(&h, &team_id, "s-planner").attempts, 1);
    let plan_after = artifact_of_kind(&h, &project_id, "plan").await;
    assert_eq!(plan_after.artifact_id, plan_before.artifact_id);
    assert_eq!(
        plan_after.content_ref, plan_before.content_ref,
        "CAS ref 必须保持不变"
    );

    // 恢复后跑完：builder/reviewer/verifier 各执行 1 次；planner 不再执行。
    let registry2 = build_registry(&coordinator2, &team_id, Arc::new(EchoWorker), &roles);
    let outcome = drive_next_on(&coordinator2, &team_id, &registry2).await;
    assert!(matches!(outcome, PhaseOutcome::Done));
    assert_eq!(
        record_of(&h, &team_id, "s-planner").attempts,
        1,
        "planner 不得重跑"
    );
    assert_eq!(record_of(&h, &team_id, "s-builder").attempts, 1);
    assert_eq!(record_of(&h, &team_id, "s-reviewer").attempts, 1);
    assert_eq!(record_of(&h, &team_id, "s-verifier").attempts, 1);
    let arts = h
        .store
        .list_artifacts_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(arts.len(), 4);
    let decisions = h
        .store
        .list_decisions_by_project(&project_id)
        .await
        .unwrap();
    assert_eq!(decisions.len(), 1, "只应有 retry 这一条决策");
}

// ---------------------------------------------------------------------------
// 7. 状态文件损坏：明确失败 + 原文件保留
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_state_file_fails_explicitly_and_is_preserved() {
    let h = harness();
    let roles = branch_roles("echo");
    let team = h
        .coordinator
        .create_team_run(&create_req(&roles))
        .await
        .unwrap();
    let team_id = team.team_id.clone();

    let planted = corrupt_state_file(&h, &team_id);

    // 单团队操作：明确 CorruptState 失败（非静默重建）。
    let err = h.coordinator.load_run_state(&team_id).unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::CorruptState(ref m) if m.contains("保留")),
        "损坏状态必须明确失败并注明保留：{err:?}"
    );
    let err = h
        .coordinator
        .apply_steer(&team_id, &SteerCommand::Continue)
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::CorruptState(_)),
        "continue 必须拒绝损坏状态：{err:?}"
    );
    let err = h
        .coordinator
        .apply_steer(
            &team_id,
            &SteerCommand::Retry {
                step_id: "s-planner".to_string(),
                note: "x".to_string(),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkSwarmError::CorruptState(_)),
        "retry 必须拒绝损坏状态：{err:?}"
    );

    // 全量扫描：报告不可读团队，不覆盖原文件。
    let scan = h.coordinator.detect_interrupted().await.unwrap();
    assert_eq!(scan.unreadable_states, vec![team_id.clone()]);
    assert!(scan.interrupted.is_empty());

    // 原文件字节级保留。
    let path = h.dir.join("runs").join(format!("{team_id}.json"));
    let raw = std::fs::read_to_string(&path).unwrap();
    assert_eq!(raw, planted, "损坏文件必须原样保留（禁止覆盖成新状态）");
}
