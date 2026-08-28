//! WorkSwarm 长任务响应性契约测试（R3）。
//!
//! 覆盖：
//! 1. 长 Worker 阶段（run_phase 阶段 B 无锁）期间，详情/任务图/进度读路径 ≤200ms；
//! 2. cancel 立即返回（<1s）并终止活动 Worker（阶段代次失效 → 旧结果丢弃）；
//! 3. 过期阶段的产物回传被拒收：只记审计（team.phase.stale_drop），
//!    不创建 Artifact、不改终态；legacy 入口（None）保持兼容；
//! 4. progress seq 单调递增，终态计数正确、current_steps 清空；
//! 5. 并发读不产生死锁、重复 Artifact 或状态回退。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use owo_agent_core::goal::{Worker, WorkerRegistry};
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use owo_agent_core::workswarm::SteerCommand;
use owo_agent_core::{
    CreateTeamRequest, PhaseOutcome, RoleSpec, RoleWorker, TeamCoordinator, TeamTemplateRegistry,
};
use owo_agent_protocol::TeamMode;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 测试内层 worker
// ---------------------------------------------------------------------------

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
// 脚手架（与 workswarm_tests 同构）
// ---------------------------------------------------------------------------

struct Harness {
    dir: PathBuf,
    store: Arc<SqliteProjectSpaceStore>,
    coordinator: Arc<TeamCoordinator>,
    audit: Arc<std::sync::Mutex<owo_agent_core::audit::AuditLog>>,
}

fn harness() -> Harness {
    let dir = std::env::temp_dir().join(format!(
        "owo-ws-resp-{}-{}",
        std::process::id(),
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

async fn create_team(h: &Harness, roles: Vec<RoleSpec>) -> String {
    let req = CreateTeamRequest {
        goal_id: None,
        objective: "响应性契约测试目标".to_string(),
        mode: TeamMode::Team,
        template_id: None,
        roles,
        budget: json!(null),
        human_policy: None,
        strategy: None,
    };
    h.coordinator
        .create_team_run(&req)
        .await
        .expect("建队失败")
        .team_id
}

/// 后台驱动循环（镜像 server run_team_loop 的最小形态；终态即返回）。
async fn drive(
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    registry: WorkerRegistry,
) -> PhaseOutcome {
    let mut rounds = 0;
    loop {
        match coordinator.run_phase(&team_id, &registry).await {
            Ok(PhaseOutcome::MoreReady) => {
                rounds += 1;
                assert!(rounds < 200, "MoreReady 循环超限");
            }
            Ok(PhaseOutcome::Done) => {
                let _ = coordinator.finalize_success(&team_id).await;
                return PhaseOutcome::Done;
            }
            Ok(other) => return other,
            Err(e) => panic!("run_phase 失败：{e}"),
        }
    }
}

/// 等待阶段进入执行中（current_steps 非空且 active）。
async fn wait_running(coordinator: &Arc<TeamCoordinator>, team_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let progress = coordinator
            .progress_snapshot(team_id)
            .await
            .expect("进度快照失败");
        if progress.active && !progress.current_steps.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "等待阶段执行中超时：{progress:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn chain_roles(names: &[&str]) -> Vec<RoleSpec> {
    let mut roles: Vec<RoleSpec> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let mut role = RoleSpec::agent(*name);
        if index > 0 {
            role.depends_on = vec![names[index - 1].to_string()];
        }
        roles.push(role);
    }
    roles
}

fn status_ordinal(status: &str) -> u8 {
    match status {
        "Pending" => 0,
        "Ready" => 1,
        "Running" => 2,
        "Succeeded" => 3,
        "Failed" => 4,
        "Aborted" => 5,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// 1. 长 Worker 阶段读路径 ≤200ms + cancel <1s
// ---------------------------------------------------------------------------

#[tokio::test]
async fn detail_reads_stay_fast_during_long_worker_and_cancel_returns_fast() {
    let h = harness();
    let roles = chain_roles(&["builder"]);
    let team_id = create_team(&h, roles.clone()).await;
    let registry = build_registry(&h, &team_id, Arc::new(SlowEchoWorker { ms: 4000 }), &roles);

    let driver = tokio::spawn(drive(Arc::clone(&h.coordinator), team_id.clone(), registry));
    wait_running(&h.coordinator, &team_id).await;

    // 读路径在长 Worker 执行期间逐次测量（详情 / 任务图状态 / 进度快照）。
    for _ in 0..6 {
        let started = Instant::now();
        let team = h
            .coordinator
            .get_team_run(&team_id)
            .await
            .expect("详情失败");
        let _state = h.coordinator.load_run_state(&team_id).expect("任务图失败");
        let progress = h
            .coordinator
            .progress_snapshot(&team_id)
            .await
            .expect("进度失败");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "长 Worker 期间读路径耗时 {elapsed:?}（要求 <200ms）"
        );
        assert!(progress.active, "执行中团队应标记 active");
        assert_eq!(
            progress.current_steps.first().map(|s| s.step_id.as_str()),
            Some("s-builder"),
            "current_steps 应包含执行中步骤：{progress:?}"
        );
        assert_eq!(progress.counts.running, 1);
        assert_eq!(team.status, owo_agent_protocol::TeamRunStatus::Running);
    }

    // cancel 立即返回（<1s）。
    let started = Instant::now();
    h.coordinator
        .apply_steer(&team_id, &SteerCommand::Cancel)
        .await
        .expect("取消失败");
    let cancel_elapsed = started.elapsed();
    assert!(
        cancel_elapsed < Duration::from_secs(1),
        "cancel 耗时 {cancel_elapsed:?}（要求 <1s）"
    );

    // 驱动循环应在 cancel 后很快终止（阶段 B select 立即感知）。
    let outcome = tokio::time::timeout(Duration::from_secs(3), driver)
        .await
        .expect("驱动循环未在取消后退出")
        .expect("驱动任务 panic");
    assert!(
        matches!(outcome, PhaseOutcome::Aborted),
        "应 Aborted：{outcome:?}"
    );
    let team = h.coordinator.get_team_run(&team_id).await.unwrap();
    assert_eq!(team.status, owo_agent_protocol::TeamRunStatus::Cancelled);
    assert!(!h.coordinator.is_run_active(&team_id));
    // 取消阶段未产生任何 Artifact。
    let space_id = team.project_space_id.clone().unwrap();
    let artifacts = h.store.list_artifacts_by_project(&space_id).await.unwrap();
    assert!(
        artifacts.is_empty(),
        "取消阶段不应产生 Artifact：{artifacts:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. 过期阶段回传拒收（只记审计，不创建 Artifact）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_epoch_registration_is_rejected_and_audited() {
    let h = harness();
    let roles = chain_roles(&["builder"]);
    let team_id = create_team(&h, roles.clone()).await;
    let registry = build_registry(&h, &team_id, Arc::new(SlowEchoWorker { ms: 4000 }), &roles);
    let driver = tokio::spawn(drive(Arc::clone(&h.coordinator), team_id.clone(), registry));
    wait_running(&h.coordinator, &team_id).await;

    // cancel：代次立即失效。
    h.coordinator
        .apply_steer(&team_id, &SteerCommand::Cancel)
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(3), driver).await;

    // 旧代次（epoch 0）回传 → Conflict；只记审计；Artifact 集合不变。
    let error = h
        .coordinator
        .register_step_output_checked(
            &team_id,
            "m-builder",
            "builder",
            "s-builder",
            "过期回传内容",
            Some(0),
        )
        .await
        .expect_err("过期回传必须被拒收");
    assert!(error.to_string().contains("阶段已过期"), "{error}");

    let team = h.coordinator.get_team_run(&team_id).await.unwrap();
    let space_id = team.project_space_id.clone().unwrap();
    let artifacts = h.store.list_artifacts_by_project(&space_id).await.unwrap();
    assert!(artifacts.is_empty(), "过期回传不得创建 Artifact");

    // 兼容入口（None = 人节点/诊断路径）不受代次校验影响。
    h.coordinator
        .register_step_output(
            &team_id,
            "m-builder",
            "builder",
            "s-builder",
            "人工补录产物",
        )
        .await
        .expect("legacy 入口应保持兼容");
    let artifacts = h.store.list_artifacts_by_project(&space_id).await.unwrap();
    assert_eq!(artifacts.len(), 1);

    // 阶段 C / 回传校验都应留下 stale_drop 审计。
    let audit = h.audit.lock().unwrap();
    let stale_events = audit
        .entries
        .iter()
        .filter(|e| e.event == "team.phase.stale_drop")
        .count();
    assert!(stale_events >= 1, "应存在 team.phase.stale_drop 审计事件");
}

// ---------------------------------------------------------------------------
// 3. progress seq 单调递增 + 终态计数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_seq_increases_monotonically_until_terminal() {
    let h = harness();
    let roles = chain_roles(&["builder", "critic", "leader"]);
    let team_id = create_team(&h, roles.clone()).await;
    let registry = build_registry(&h, &team_id, Arc::new(SlowEchoWorker { ms: 200 }), &roles);

    let sampler_coordinator = Arc::clone(&h.coordinator);
    let sampler_team = team_id.clone();
    let sampler = tokio::spawn(async move {
        let mut samples: Vec<(u64, u32, usize)> = Vec::new();
        loop {
            let progress = sampler_coordinator
                .progress_snapshot(&sampler_team)
                .await
                .unwrap();
            samples.push((
                progress.seq,
                progress.counts.running,
                progress.current_steps.len(),
            ));
            if progress.status == "Succeeded" || progress.status == "Failed" {
                return samples;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    let outcome = drive(Arc::clone(&h.coordinator), team_id.clone(), registry).await;
    assert!(matches!(outcome, PhaseOutcome::Done), "{outcome:?}");
    let samples = sampler.await.unwrap();

    // seq 单调（采样序列不回退），且存在执行中样本（状态转移发生过）。
    let mut last = 0u64;
    let mut saw_running = false;
    for (index, (seq, running, current)) in samples.iter().enumerate() {
        if index == 0 {
            last = *seq;
        } else {
            assert!(*seq >= last, "progress seq 回退：{samples:?}");
            last = *seq;
        }
        if *running > 0 || *current > 0 {
            saw_running = true;
        }
    }
    assert!(saw_running, "采样应捕获到执行中状态：{samples:?}");

    let progress = h.coordinator.progress_snapshot(&team_id).await.unwrap();
    assert_eq!(progress.status, "Succeeded");
    assert!(!progress.active);
    assert!(progress.current_steps.is_empty());
    assert_eq!(progress.counts.succeeded, 3);
    assert_eq!(progress.counts.pending, 0);
    // 终态产物：3 个版本化 Artifact，无重复。
    let team = h.coordinator.get_team_run(&team_id).await.unwrap();
    let space_id = team.project_space_id.clone().unwrap();
    let artifacts = h.store.list_artifacts_by_project(&space_id).await.unwrap();
    let mut ids: Vec<&str> = artifacts.iter().map(|a| a.artifact_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "产物不得重复：{ids:?}");
}

// ---------------------------------------------------------------------------
// 4. 并发读：无死锁、无状态回退、无重复产物
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_readers_never_deadlock_or_regress_state() {
    let h = harness();
    let roles = chain_roles(&["builder"]);
    let team_id = create_team(&h, roles.clone()).await;
    let registry = build_registry(&h, &team_id, Arc::new(SlowEchoWorker { ms: 1500 }), &roles);

    let reader_coordinator = Arc::clone(&h.coordinator);
    let reader_team = team_id.clone();
    let readers = tokio::spawn(async move {
        let mut series: Vec<u8> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let team = reader_coordinator.get_team_run(&reader_team).await.unwrap();
            let state = reader_coordinator.load_run_state(&reader_team).unwrap();
            let _ = reader_coordinator
                .progress_snapshot(&reader_team)
                .await
                .unwrap();
            let record_status = state
                .records
                .get("s-builder")
                .map(|r| format!("{:?}", r.status))
                .unwrap_or_default();
            let ordinal = status_ordinal(&record_status);
            if let Some(last) = series.last().copied() {
                assert!(ordinal >= last, "状态回退：{series:?} → {ordinal}");
            }
            series.push(ordinal);
            if team.status == owo_agent_protocol::TeamRunStatus::Succeeded
                || team.status == owo_agent_protocol::TeamRunStatus::Failed
                || team.status == owo_agent_protocol::TeamRunStatus::Cancelled
            {
                return series;
            }
            assert!(Instant::now() < deadline, "并发读超时（疑似死锁）");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });

    let outcome = drive(Arc::clone(&h.coordinator), team_id.clone(), registry).await;
    assert!(matches!(outcome, PhaseOutcome::Done), "{outcome:?}");
    let series = tokio::time::timeout(Duration::from_secs(10), readers)
        .await
        .expect("并发读任务未结束（疑似死锁）")
        .unwrap();

    // 全程无回退；至少一次观察到 Running（领取标记即时落盘）。
    assert!(
        series.contains(&2),
        "应有 reader 观察到 Running（领取即时落盘）：{series:?}"
    );

    // 终态产物恰好 1 个（无重复登记）。
    let team = h.coordinator.get_team_run(&team_id).await.unwrap();
    let space_id = team.project_space_id.clone().unwrap();
    let artifacts = h.store.list_artifacts_by_project(&space_id).await.unwrap();
    assert_eq!(artifacts.len(), 1, "不得重复登记产物：{artifacts:?}");
}
