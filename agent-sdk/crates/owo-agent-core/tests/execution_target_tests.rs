//! A2 统一调度适配层契约测试（第三路 · 定向派发）。
//!
//! 覆盖（§15 A2 完成标准）：
//! - 同一步骤显式选择进程内 / 本地子进程 / 远端节点执行；
//! - 显式目标不可用时等待/询问/拒绝：本地缺失不自动转远端、远端缺失不回退本地高权限 worker；
//! - 三类目标共用一致的 StepResult 映射（成功/失败/取消/预算耗尽）；
//! - correlation ID、绑定预算、输入 CAS 引用完整传递（`_dispatch` 注入 +
//!   TransportTask lineage）；
//! - abort 后不残留 pending 任务或租约。

use async_trait::async_trait;
use owo_agent_core::execution_target::{
    select_binding, BindingBudget, DispatchCancelRegistry, ExecutionTarget, FleetDispatchWorker,
    TARGET_IN_PROCESS,
};
use owo_agent_core::fleet::{RestartPolicy, RestartRule};
use owo_agent_core::fleet_transport::{InMemoryTransport, TransportStatus, TransportTask};
use owo_agent_core::goal::{Goal, GoalRunner, GoalStatus, RunnerConfig, Worker, WorkerRegistry};
use owo_agent_core::lease::LeaseManager;
use owo_agent_core::plan::{Plan, StepSpec};
use owo_agent_core::worker_pool::{child, WorkerPool, WorkerSpec};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------- 进程内捕获 worker ----------

/// 记录型 echo worker：捕获输入、可注入固定失败与每次尝试的睡眠时长。
struct RecordingEcho {
    name: String,
    runs: Arc<AtomicUsize>,
    captured: Arc<Mutex<Vec<Value>>>,
    fail_times: HashMap<String, u32>,
    sleep_ms: u64,
}

impl RecordingEcho {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            runs: Arc::new(AtomicUsize::new(0)),
            captured: Arc::new(Mutex::new(Vec::new())),
            fail_times: HashMap::new(),
            sleep_ms: 0,
        }
    }

    fn fail_text(mut self, text: &str, times: u32) -> Self {
        self.fail_times.insert(text.to_string(), times);
        self
    }

    fn with_sleep(mut self, ms: u64) -> Self {
        self.sleep_ms = ms;
        self
    }

    fn run_count(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    fn inputs(&self) -> Vec<Value> {
        self.captured.lock().unwrap().clone()
    }
}

#[async_trait]
impl Worker for RecordingEcho {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.captured.lock().unwrap().push(input.clone());
        if self.sleep_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
        }
        let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
        // 恒定注入：fail_times 不衰减（始终失败的用例传大次数即可）。
        let verdict = self.fail_times.get(text).copied().unwrap_or(0);
        if verdict > 0 {
            return Err(format!("注入失败：{text}"));
        }
        Ok(format!("out-{text}"))
    }
}

fn registry_of(workers: Vec<Arc<RecordingEcho>>) -> WorkerRegistry {
    let registry = WorkerRegistry::new();
    for w in workers {
        registry.register(w);
    }
    registry
}

fn single_step_plan(goal_id: &str, step_id: &str, text: &str) -> Plan {
    let mut plan = Plan::new("plan-target", goal_id);
    let mut step = StepSpec::new(step_id, "echo");
    step.input = json!({ "text": text });
    plan.add_step(step);
    plan
}

fn config(bindings: Vec<owo_agent_core::execution_target::WorkerBinding>) -> RunnerConfig {
    RunnerConfig {
        bindings,
        ..Default::default()
    }
}

fn binding(
    worker: &str,
    target: ExecutionTarget,
    correlation: Option<&str>,
    cas: Option<&str>,
) -> owo_agent_core::execution_target::WorkerBinding {
    let mut b = owo_agent_core::execution_target::WorkerBinding::new(worker, target);
    b.correlation_id = correlation.map(|c| c.to_string());
    b.input_cas_ref = cas.map(|c| c.to_string());
    b
}

// ---------- 子进程池自举（协议复用 worker_pool::child） ----------

/// 子进程模式入口（父进程以 `--exact execution_target_child_entry` + 环境标记拉起）。
#[test]
fn execution_target_child_entry() {
    if std::env::var("OWO_WORKER_CHILD").is_err() {
        return;
    }
    child::run_child_protocol(|input| {
        if input.get("crash").and_then(Value::as_bool).unwrap_or(false) {
            std::process::exit(42);
        }
        if let Some(ms) = input.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let text = input.get("text").and_then(Value::as_str).unwrap_or("");
        if input
            .get("fail_always")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(format!("注入失败：{text}"));
        }
        // `_dispatch` 上下文贯通观测：带绑定派发时把 correlation 回显进输出。
        let dispatch_corr = input
            .pointer("/_dispatch/correlation_id")
            .and_then(Value::as_str)
            .map(|c| format!("|corr={c}"))
            .unwrap_or_default();
        Ok(format!("out-{text}{dispatch_corr}"))
    });
}

fn pool_echo_spec(id: &str) -> WorkerSpec {
    WorkerSpec::new(id, std::env::current_exe().unwrap())
        .args(vec![
            "--exact".to_string(),
            "execution_target_child_entry".to_string(),
            "--nocapture".to_string(),
            "--quiet".to_string(),
        ])
        .env_whitelist(vec![("OWO_WORKER_CHILD".to_string(), "1".to_string())])
        .restart_rule(RestartRule {
            max_restarts: 2,
            base_backoff_secs: 0,
            policy: RestartPolicy::OneForOne,
        })
}

fn pool_config(
    pool: &WorkerPool,
    bindings: Vec<owo_agent_core::execution_target::WorkerBinding>,
) -> RunnerConfig {
    RunnerConfig {
        use_worker_pool: true,
        worker_pool: Some(pool.clone()),
        bindings,
        ..Default::default()
    }
}

// ---------- 远端补全器（InMemoryTransport 的节点侧替身） ----------

type SeenTasks = Arc<Mutex<Vec<TransportTask>>>;

#[derive(Clone, Copy)]
enum RemoteMode {
    /// 提交即成功并回传 payload 字符串。
    Success(&'static str),
    /// 提交即失败。
    Fail,
}

/// 后台补全任务：轮询 InMemoryTransport，按给定模式完成每个到达的任务，
/// 同时把提交快照记录进 seen（供 correlation/lineage 断言）。返回 JoinHandle 由调用方 abort。
fn spawn_remote_completer(
    tr: &InMemoryTransport,
    seen: SeenTasks,
    mode: RemoteMode,
) -> tokio::task::JoinHandle<()> {
    let tr = tr.clone();
    tokio::spawn(async move {
        loop {
            for id in tr.task_ids() {
                if matches!(
                    tr.task_status(&id),
                    Some(TransportStatus::Running | TransportStatus::Pending)
                ) {
                    if let Some(task) = tr.task(&id) {
                        seen.lock().unwrap().push(task);
                    }
                    match mode {
                        RemoteMode::Success(payload) => {
                            tr.complete_task(&id, true, json!(payload));
                        }
                        RemoteMode::Fail => {
                            tr.complete_task(&id, false, json!("远端执行失败"));
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
}

// ---------- 1. 同一步骤显式选择进程内执行 ----------

#[tokio::test]
async fn explicit_in_process_completes_and_injects_dispatch_context() {
    let worker = Arc::new(RecordingEcho::new("echo"));
    let registry = registry_of(vec![worker.clone()]);
    let bindings = vec![binding(
        "echo",
        ExecutionTarget::InProcess,
        Some("corr-a"),
        Some("sha256:cafebabe"),
    )];
    let mut runner = GoalRunner::new(
        Goal::new("g-inproc", "显式进程内执行"),
        single_step_plan("g-inproc", "s1", "A"),
        config(bindings),
    );
    let status = runner.run(&registry).await.unwrap();
    assert_eq!(status, GoalStatus::Succeeded);
    assert_eq!(runner.state.records["s1"].output.as_deref(), Some("out-A"));
    assert_eq!(worker.run_count(), 1, "进程内 worker 只跑一次");
    // `_dispatch` 注入完整传递。
    let d = &worker.inputs()[0];
    assert_eq!(d["_dispatch"]["target"], TARGET_IN_PROCESS);
    assert_eq!(d["_dispatch"]["correlation_id"], "corr-a");
    assert_eq!(d["_dispatch"]["input_cas_ref"], "sha256:cafebabe");
    assert!(d["_dispatch"].get("node_id").is_none());
}

// ---------- 2. in_process 目标下禁止回退到子进程池 ----------

#[tokio::test]
async fn in_process_binding_rejects_instead_of_falling_back_to_pool() {
    // registry 为空但本地池有同名 worker：显式 in_process 必须 Reject，不得改派。
    let pool = WorkerPool::new();
    pool.spawn(pool_echo_spec("echo")).await.unwrap();
    let registry = WorkerRegistry::new();
    let mut runner = GoalRunner::new(
        Goal::new("g-reject-local", "in_process 禁止回退"),
        single_step_plan("g-reject-local", "s1", "A"),
        pool_config(
            &pool,
            vec![binding("echo", ExecutionTarget::InProcess, None, None)],
        ),
    );
    let status = runner.run(&registry).await.unwrap();
    assert_eq!(status, GoalStatus::Failed);
    let record = &runner.state.records["s1"];
    let error = record.error.as_deref().unwrap();
    assert!(
        error.contains(TARGET_IN_PROCESS) && error.contains("禁止回退"),
        "应明确拒绝而非改派：{error}"
    );
    assert_eq!(record.output, None, "任何通道都不应产出输出");
    pool.shutdown().await;
}

// ---------- 3. 同一步骤显式选择本地子进程 ----------

#[tokio::test]
async fn explicit_local_process_executes_real_child() {
    let pool = WorkerPool::new();
    pool.spawn(pool_echo_spec("echo")).await.unwrap();
    let registry = WorkerRegistry::new(); // 空 registry：只能走子进程池
    let bindings = vec![binding(
        "echo",
        ExecutionTarget::LocalProcess,
        Some("corr-pool"),
        None,
    )];
    let mut runner = GoalRunner::new(
        Goal::new("g-localproc", "显式本地子进程"),
        single_step_plan("g-localproc", "s1", "B"),
        pool_config(&pool, bindings),
    );
    let status = runner.run(&registry).await.unwrap();
    assert_eq!(status, GoalStatus::Succeeded,);
    // 子进程真实回显 + `_dispatch.correlation_id` 贯通到子进程输入。
    assert!(
        runner.state.records["s1"]
            .output
            .as_deref()
            .unwrap()
            .contains("out-B|corr=corr-pool"),
        "子进程应收到绑定上下文：{:?}",
        runner.state.records["s1"].output
    );
    pool.shutdown().await;
}

// ---------- 4. 同一步骤显式选择远端节点 ----------

#[tokio::test]
async fn explicit_fleet_node_executes_over_transport_with_correlation_and_cas() {
    let transport = InMemoryTransport::new();
    let seen: SeenTasks = Arc::new(Mutex::new(Vec::new()));
    let completer = spawn_remote_completer(
        &transport,
        seen.clone(),
        RemoteMode::Success("remote-out-A"),
    );

    // 绑定按步骤声明的 worker 名「echo」匹配；节点绑定携带明确 node_id。
    let bindings = vec![binding(
        "echo",
        ExecutionTarget::FleetNode {
            node_id: "node-x".to_string(),
        },
        Some("corr-fleet"),
        Some("sha256:f001d00d"),
    )];
    let registry = WorkerRegistry::new();
    let mut runner = GoalRunner::new(
        Goal::new("g-fleet", "显式远端节点"),
        single_step_plan("g-fleet", "s1", "A"),
        RunnerConfig {
            transport: Some(Arc::new(transport.clone())),
            bindings,
            ..Default::default()
        },
    );
    let status = runner.run(&registry).await.unwrap();
    completer.abort();
    assert_eq!(status, GoalStatus::Succeeded);
    assert_eq!(
        runner.state.records["s1"].output.as_deref(),
        Some("remote-out-A")
    );
    // 提交的 TransportTask 携带明确关联信息与血缘。
    let tasks = seen.lock().unwrap();
    assert_eq!(tasks.len(), 1, "恰好一次远端提交");
    let task = &tasks[0];
    assert_eq!(task.worker, "echo");
    assert_eq!(task.correlation_id, "corr-fleet");
    assert!(
        task.lineage.contains(&"cas:sha256:f001d00d".to_string()),
        "输入 CAS 引用写入 lineage：{:?}",
        task.lineage
    );
    assert!(
        task.lineage.contains(&"node:node-x".to_string()),
        "节点绑定写入 lineage：{:?}",
        task.lineage
    );
    assert_eq!(task.input["_dispatch"]["node_id"], "node-x");
    assert_eq!(task.input["_dispatch"]["correlation_id"], "corr-fleet");
}

// ---------- 5. 本地目标不可用时绝不转投远端 ----------

#[tokio::test]
async fn local_unavailable_does_not_silently_route_remote() {
    // 远端传输可用且补全器愿意执行任何任务：若发生静默改派，任务会被执行并“成功”。
    let transport = InMemoryTransport::new();
    let seen: SeenTasks = Arc::new(Mutex::new(Vec::new()));
    let completer = spawn_remote_completer(
        &transport,
        seen.clone(),
        RemoteMode::Success("MUST-NOT-RUN"),
    );

    let bindings = vec![binding("echo", ExecutionTarget::LocalProcess, None, None)];
    let registry = WorkerRegistry::new();
    let mut runner = GoalRunner::new(
        Goal::new("g-no-reroute", "本地不可用不改派远端"),
        single_step_plan("g-no-reroute", "s1", "A"),
        RunnerConfig {
            transport: Some(Arc::new(transport.clone())),
            bindings,
            allow_replan: false,
            ..Default::default()
        },
    );
    let status = runner.run(&registry).await.unwrap();
    completer.abort();
    assert_eq!(status, GoalStatus::Failed);
    let error = runner.state.records["s1"].error.as_deref().unwrap();
    assert!(
        error.contains("local_process") && error.contains("禁止切换"),
        "应显式拒绝而非改派：{error}"
    );
    assert_eq!(seen.lock().unwrap().len(), 0, "不允许任何远端提交");
    assert_eq!(transport.task_count(), 0);
}

// ---------- 6. 远端目标不可用时绝不回退到本地高权限 worker ----------

#[tokio::test]
async fn remote_unavailable_does_not_fall_back_to_local_registry_worker() {
    // registry 里存在能力更强的进程内 worker：无传输时必须拒绝，不得静默回退执行。
    let worker = Arc::new(RecordingEcho::new("echo"));
    let registry = registry_of(vec![worker.clone()]);
    let bindings = vec![binding(
        "echo",
        ExecutionTarget::FleetNode {
            node_id: "node-offline".to_string(),
        },
        None,
        None,
    )];
    let mut runner = GoalRunner::new(
        Goal::new("g-no-downgrade", "远端不可用不回退本地"),
        single_step_plan("g-no-downgrade", "s1", "A"),
        RunnerConfig {
            bindings,
            allow_replan: false,
            ..Default::default()
        },
    );
    let status = runner.run(&registry).await.unwrap();
    assert_eq!(status, GoalStatus::Failed);
    let error = runner.state.records["s1"].error.as_deref().unwrap();
    assert!(
        error.contains("fleet_node") && error.contains("禁止回退到本地高权限"),
        "应明确拒绝而非回退：{error}"
    );
    assert_eq!(worker.run_count(), 0, "本地 worker 不应被偷偷执行");
    // AskUser/Reject 属确定性失败：原因原样直达目标终态。
    assert_eq!(
        runner.state.goal.error.as_deref(),
        runner.state.records["s1"].error.as_deref()
    );
}

// ---------- 7. 三类目标映射为一致的 StepResult ----------

/// 公共断言：失败步骤结构一致（Failed 终态 + 非空原因 + 无输出）。
fn assert_failed_shape(runner: &GoalRunner, step_id: &str, tries: u32) {
    let record = &runner.state.records[step_id];
    assert_eq!(
        record.status,
        owo_agent_core::plan::StepStatus::Failed,
        "终态必须一致"
    );
    assert!(
        record.error.as_deref().unwrap_or_default().len() > 4,
        "必须有清晰原因"
    );
    assert_eq!(record.output, None);
    assert_eq!(record.attempts, tries, "尝试次数一致");
    assert_eq!(runner.state.goal.status, GoalStatus::Failed);
}

#[tokio::test]
async fn three_targets_share_identical_failure_and_budget_semantics() {
    // —— 失败：三类通道各自产出一个 attempt 的失败步骤。
    // 进程内：
    let worker = Arc::new(RecordingEcho::new("echo").fail_text("A", u32::MAX));
    let registry = registry_of(vec![worker.clone()]);
    let mut runner = GoalRunner::new(
        Goal::new("g-fail-ip", "统一失败·进程内"),
        single_step_plan("g-fail-ip", "s1", "A"),
        RunnerConfig {
            bindings: vec![binding("echo", ExecutionTarget::InProcess, None, None)],
            allow_replan: false,
            ..Default::default()
        },
    );
    assert_eq!(runner.run(&registry).await.unwrap(), GoalStatus::Failed);
    assert_failed_shape(&runner, "s1", 1);

    // 本地子进程（fail_always 协议响应；默认 retries=0 → 1 次尝试）。
    let pool = WorkerPool::new();
    pool.spawn(pool_echo_spec("echo")).await.unwrap();
    let mut plan = single_step_plan("g-fail-lp", "s1", "A");
    plan.step_mut("s1").unwrap().input = json!({ "text": "A", "fail_always": true });
    let mut runner = GoalRunner::new(
        Goal::new("g-fail-lp", "统一失败·子进程"),
        plan,
        RunnerConfig {
            use_worker_pool: true,
            worker_pool: Some(pool.clone()),
            bindings: vec![binding("echo", ExecutionTarget::LocalProcess, None, None)],
            allow_replan: false,
            ..Default::default()
        },
    );
    assert_eq!(
        runner.run(&WorkerRegistry::new()).await.unwrap(),
        GoalStatus::Failed
    );
    assert_failed_shape(&runner, "s1", 1);

    // 远端（补全器返回失败；定向绑定按 worker 名 echo 匹配）。
    let transport = InMemoryTransport::new();
    let seen: SeenTasks = Arc::new(Mutex::new(Vec::new()));
    let completer = spawn_remote_completer(&transport, seen.clone(), RemoteMode::Fail);
    let mut runner = GoalRunner::new(
        Goal::new("g-fault-fleet", "统一失败·远端"),
        single_step_plan("g-fault-fleet", "s1", "A"),
        RunnerConfig {
            transport: Some(Arc::new(transport.clone())),
            bindings: vec![binding(
                "echo",
                ExecutionTarget::FleetNode {
                    node_id: "n1".to_string(),
                },
                None,
                None,
            )],
            allow_replan: false,
            ..Default::default()
        },
    );
    assert_eq!(
        runner.run(&WorkerRegistry::new()).await.unwrap(),
        GoalStatus::Failed
    );
    completer.abort();
    assert_failed_shape(&runner, "s1", 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn three_targets_share_abort_skip_semantics() {
    // abort 先行：三类通道都不进入执行体，全部步骤落 Aborted —— 取消语义一致。

    // 进程内：
    let worker = Arc::new(RecordingEcho::new("echo").with_sleep(50));
    let registry = registry_of(vec![worker.clone()]);
    let mut runner = GoalRunner::new(
        Goal::new("g-abort-ip", "统一取消·进程内"),
        single_step_plan("g-abort-ip", "s1", "A"),
        RunnerConfig {
            bindings: vec![binding("echo", ExecutionTarget::InProcess, None, None)],
            ..Default::default()
        },
    );
    runner.abort();
    assert_eq!(runner.run(&registry).await.unwrap(), GoalStatus::Aborted);
    assert_eq!(
        runner.state.records["s1"].status,
        owo_agent_core::plan::StepStatus::Aborted
    );
    assert_eq!(worker.run_count(), 0);

    // 子进程：
    let pool = WorkerPool::new();
    pool.spawn(pool_echo_spec("echo")).await.unwrap();
    let mut runner = GoalRunner::new(
        Goal::new("g-abort-lp", "统一取消·子进程"),
        single_step_plan("g-abort-lp", "s1", "A"),
        pool_config(
            &pool,
            vec![binding("echo", ExecutionTarget::LocalProcess, None, None)],
        ),
    );
    runner.abort();
    assert_eq!(
        runner.run(&WorkerRegistry::new()).await.unwrap(),
        GoalStatus::Aborted
    );
    assert_eq!(
        runner.state.records["s1"].status,
        owo_agent_core::plan::StepStatus::Aborted
    );

    // 远端：
    let transport = InMemoryTransport::with_ttl(Duration::from_secs(2));
    let mut runner = GoalRunner::new(
        Goal::new("g-abort-fleet", "统一取消·远端"),
        single_step_plan("g-abort-fleet", "s1", "A"),
        RunnerConfig {
            transport: Some(Arc::new(transport.clone())),
            bindings: vec![binding(
                "echo",
                ExecutionTarget::FleetNode {
                    node_id: "n1".to_string(),
                },
                None,
                None,
            )],
            ..Default::default()
        },
    );
    runner.abort();
    assert_eq!(
        runner.run(&WorkerRegistry::new()).await.unwrap(),
        GoalStatus::Aborted
    );
    assert_eq!(
        runner.state.records["s1"].status,
        owo_agent_core::plan::StepStatus::Aborted
    );
    assert_eq!(transport.task_count(), 0, "abort 前置时不产生在飞任务");
    pool.shutdown().await;
}

#[tokio::test]
async fn binding_budget_caps_attempts_on_every_channel() {
    // 绑定 max_attempts=1 时即使计划允许重试也只跑一次（三通道一致）。
    let mut limited = binding("echo", ExecutionTarget::InProcess, None, None);
    limited.budget = BindingBudget {
        max_attempts: 1,
        max_duration_secs: 0,
    };
    let worker = Arc::new(RecordingEcho::new("echo").fail_text("A", u32::MAX));
    let registry = registry_of(vec![worker.clone()]);
    let mut plan = single_step_plan("g-budget-ip", "s1", "A");
    plan.step_mut("s1").unwrap().retries = 5; // 计划允许重试，绑定封顶为 1
    let mut runner = GoalRunner::new(
        Goal::new("g-budget-ip", "预算封顶·进程内"),
        plan,
        RunnerConfig {
            bindings: vec![limited],
            allow_replan: false,
            ..Default::default()
        },
    );
    assert_eq!(runner.run(&registry).await.unwrap(), GoalStatus::Failed);
    assert_eq!(
        runner.state.records["s1"].attempts, 1,
        "绑定 attempts 封顶生效"
    );

    // 本地子进程同样受 max_attempts 封顶（每次真实子进程往返）。
    let pool = WorkerPool::new();
    pool.spawn(pool_echo_spec("echo")).await.unwrap();
    let mut limited_lp = binding("echo", ExecutionTarget::LocalProcess, None, None);
    limited_lp.budget = BindingBudget {
        max_attempts: 1,
        max_duration_secs: 0,
    };
    let mut plan = single_step_plan("g-budget-lp", "s1", "A");
    plan.step_mut("s1").unwrap().retries = 5;
    plan.step_mut("s1").unwrap().input = json!({ "text": "A", "fail_always": true });
    let mut runner = GoalRunner::new(
        Goal::new("g-budget-lp", "预算封顶·子进程"),
        plan,
        RunnerConfig {
            use_worker_pool: true,
            worker_pool: Some(pool.clone()),
            bindings: vec![limited_lp],
            allow_replan: false,
            ..Default::default()
        },
    );
    assert_eq!(
        runner.run(&WorkerRegistry::new()).await.unwrap(),
        GoalStatus::Failed
    );
    assert_eq!(runner.state.records["s1"].attempts, 1);
    pool.shutdown().await;
}

#[tokio::test]
async fn binding_duration_budget_stops_retrying_sleeping_workers() {
    // 每次尝试睡 300ms 且恒失败：绑定 max_duration_secs=1 应在约 1s 处停止重试
    // （终态失败且错误指明绑定预算），不会耗满计划的全部重试。
    let worker = Arc::new(
        RecordingEcho::new("echo")
            .fail_text("A", u32::MAX)
            .with_sleep(300),
    );
    let registry = registry_of(vec![worker.clone()]);
    let mut dur_limited = binding("echo", ExecutionTarget::InProcess, None, None);
    dur_limited.budget = BindingBudget {
        max_attempts: 0,
        max_duration_secs: 1,
    };
    let mut plan = single_step_plan("g-dur", "s1", "A");
    plan.step_mut("s1").unwrap().retries = 10; // 若无绑定时长预算会烧满 ~3.3s+
    let mut runner = GoalRunner::new(
        Goal::new("g-dur", "绑定时长预算"),
        plan,
        RunnerConfig {
            bindings: vec![dur_limited],
            allow_replan: false,
            ..Default::default()
        },
    );
    let started = std::time::Instant::now();
    assert_eq!(runner.run(&registry).await.unwrap(), GoalStatus::Failed);
    let elapsed = started.elapsed();
    let record = &runner.state.records["s1"];
    assert!(
        record.error.as_deref().unwrap().contains("绑定预算耗尽"),
        "应以绑定预算耗尽收尾：{:?}",
        record.error
    );
    assert!(
        record.attempts >= 2 && record.attempts <= 5,
        "应在预算边界附近停止而不是烧满重试：attempts={}",
        record.attempts
    );
    assert!(
        elapsed < Duration::from_millis(2900),
        "不应烧满全部重试：{elapsed:?}"
    );
}

// ---------- 8. abort 后不残留 pending 任务或租约 ----------

#[tokio::test]
async fn abort_drains_inflight_fleet_tasks_and_release_leases() {
    // 8a：Worker 层直接验证——在飞 transport 任务经 DispatchCancelRegistry 被 abort 收尾取消。
    let transport = InMemoryTransport::new();
    let cancels = DispatchCancelRegistry::default();
    let mut fb = owo_agent_core::execution_target::WorkerBinding::new(
        "cap-worker",
        ExecutionTarget::FleetNode {
            node_id: "n-cancel".to_string(),
        },
    );
    fb.correlation_id = Some("corr-cancel".into());
    let worker = FleetDispatchWorker::from_binding(
        "cap-worker",
        "n-cancel",
        &fb,
        "corr-cancel",
        Arc::new(transport.clone()),
        cancels.clone(),
    )
    .with_timeout(Some(Duration::from_secs(5)));
    let handle = tokio::spawn({
        let worker = worker.clone();
        async move { worker.run(&json!({ "text": "A" })).await }
    });
    // 等待任务注册并起飞。
    let mut registered = false;
    for _ in 0..100 {
        if cancels.pending_count() == 1 {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(registered, "派发 worker 应登记在飞任务");
    assert_eq!(transport.task_count(), 1);
    let inflight_task_id = transport.task_ids()[0].clone();

    // abort 收尾：cancel_all 发出取消 → worker 以错误退出 → 登记表清空。
    cancels.cancel_all().await;
    let err = handle.await.unwrap().expect_err("被取消的任务应返回错误");
    assert!(err.contains("失败/取消"), "应报告 transport 取消：{err}");
    assert_eq!(cancels.pending_count(), 0, "登记表不留残余");
    assert_eq!(
        transport.task_status(&inflight_task_id),
        Some(TransportStatus::Cancelled),
        "transport 任务不应残留 pending"
    );

    // 8b：租约面——前置 abort 场景中租约仍会短暂获取，RAII 必须释放（无孤儿持有者）。
    let leases = Arc::new(LeaseManager::new());
    let worker_ok = Arc::new(RecordingEcho::new("echo"));
    let registry = registry_of(vec![worker_ok.clone()]);
    let mut runner = GoalRunner::new(
        Goal::new("g-abort-lease", "abort 租约清理"),
        single_step_plan("g-abort-lease", "s1", "A"),
        RunnerConfig {
            leases: Some((*leases).clone()),
            bindings: vec![binding("echo", ExecutionTarget::InProcess, None, None)],
            ..Default::default()
        },
    );
    runner.abort();
    assert_eq!(runner.run(&registry).await.unwrap(), GoalStatus::Aborted);
    assert!(
        leases.holders().is_empty(),
        "abort 后租约表不允许孤儿持有者：{:?}",
        leases.holders()
    );

    // 8c：成功路径同样零租约残留。
    let leases2 = Arc::new(LeaseManager::new());
    let worker_ok2 = Arc::new(RecordingEcho::new("echo"));
    let registry2 = registry_of(vec![worker_ok2]);
    let mut runner = GoalRunner::new(
        Goal::new("g-ok-lease", "成功租约清理"),
        single_step_plan("g-ok-lease", "s1", "A"),
        RunnerConfig {
            leases: Some((*leases2).clone()),
            bindings: vec![binding("echo", ExecutionTarget::InProcess, None, None)],
            ..Default::default()
        },
    );
    assert_eq!(runner.run(&registry2).await.unwrap(), GoalStatus::Succeeded);
    assert!(leases2.holders().is_empty(), "成功后租约表不允许孤儿持有者");
}

// ---------- 纯接口层补充：select/direct 装配一致性 ----------

#[tokio::test]
async fn unmatched_bindings_keep_legacy_transport_semantics() {
    // 绑定名不匹配任何 step id / worker 名 ⇒ 不参与定向，步骤走旧解析链
    // （transport 兜底、随机 correlation、无 `_dispatch` 注入）——旧行为冻结。
    let transport = InMemoryTransport::new();
    let seen: SeenTasks = Arc::new(Mutex::new(Vec::new()));
    let completer =
        spawn_remote_completer(&transport, seen.clone(), RemoteMode::Success("legacy-out"));
    let registry = WorkerRegistry::new();
    let mut runner = GoalRunner::new(
        Goal::new("g-legacy", "未命中绑定保持旧路径"),
        single_step_plan("g-legacy", "s1", "A"),
        RunnerConfig {
            transport: Some(Arc::new(transport.clone())),
            bindings: vec![binding(
                "cap-worker",
                ExecutionTarget::FleetNode {
                    node_id: "n1".to_string(),
                },
                Some("never-used"),
                None,
            )],
            ..Default::default()
        },
    );
    assert_eq!(runner.run(&registry).await.unwrap(), GoalStatus::Succeeded);
    completer.abort();
    assert_eq!(
        runner.state.records["s1"].output.as_deref(),
        Some("legacy-out")
    );
    let tasks = seen.lock().unwrap();
    let task = &tasks[0];
    assert_eq!(task.worker, "echo", "旧链按步骤声明的 worker 名提交");
    assert!(task.correlation_id.starts_with("x-"), "旧链沿用随机关联 ID");
    assert!(
        !task.lineage.iter().any(|l| l.contains("node:")),
        "旧链没有节点血缘标记"
    );
}

#[tokio::test]
async fn worker_name_bindings_apply_across_steps_sharing_that_worker() {
    // 按 worker 名声明的绑定约束所有引用该名字的步骤；step-id 级绑定只约束单步
    // （此处借 select_binding 决定性覆盖纯函数层契约，调度层装配已由上面全链路覆盖）。
    let by_name = binding("echo", ExecutionTarget::LocalProcess, None, None);
    let by_step = binding(
        "b-step",
        ExecutionTarget::FleetNode {
            node_id: "n2".to_string(),
        },
        None,
        None,
    );
    let bindings = vec![by_name, by_step];
    assert_eq!(
        select_binding(&bindings, "a-step", "echo")
            .unwrap()
            .target
            .kind(),
        "local_process"
    );
    assert_eq!(
        select_binding(&bindings, "b-step", "echo")
            .unwrap()
            .target
            .node_id()
            .unwrap(),
        "n2"
    );
}
