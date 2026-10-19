use super::agent_worker;
use async_trait::async_trait;
use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::audit::AuditLog;
use owo_agent_core::goal::{GoalRunner, Worker, WorkerRegistry};
use owo_agent_server::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub(super) fn goals_dir(data_root: &Path) -> PathBuf {
    data_root.join("goals")
}

pub(super) fn goal_dir(data_root: &Path, goal_id: &str) -> PathBuf {
    goals_dir(data_root).join(goal_id)
}

pub(super) fn goal_file(data_root: &Path, goal_id: &str) -> PathBuf {
    goal_dir(data_root, goal_id).join("goal.json")
}

pub(super) fn runs_dir(data_root: &Path, goal_id: &str) -> PathBuf {
    goal_dir(data_root, goal_id).join("runs")
}

pub(super) fn not_found(detail: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": detail })))
}

pub(super) fn bad_request(detail: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": detail })))
}

// ---------- 审计（按 data_root 键控） ----------

type AuditMap = Arc<Mutex<HashMap<PathBuf, Arc<Mutex<AuditLog>>>>>;

static AUDITS: OnceLock<AuditMap> = OnceLock::new();

pub(super) fn audits() -> &'static AuditMap {
    AUDITS.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

pub(super) fn audit_for(data_root: &Path) -> Arc<Mutex<AuditLog>> {
    let mut map = audits().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(data_root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(AuditLog::default())))
        .clone()
}

pub(super) fn audit_record(data_root: &Path, event: &str, detail: impl Into<String>) {
    if let Ok(mut log) = audit_for(data_root).lock() {
        log.record("goal-api", event, None, None, detail);
    }
}

// ---------- 运行注册表（abort） ----------

type RunnerHandle = Arc<tokio::sync::Mutex<GoalRunner>>;

/// 一次运行的句柄：runner（abort 兜底）+ 无锁取消通道（run 任务在飞时立即可达，
/// 不依赖等待 `runner.lock()` —— run 期间锁被 run 任务持有，锁等待会让取消在 run
/// 完成后才生效，子进程无法被及时终止）。
pub(super) struct RunHandle {
    pub(super) runner: RunnerHandle,
    pub(super) cancel: tokio::sync::mpsc::UnboundedSender<()>,
}

type RunnerMap = Arc<Mutex<HashMap<(String, String), RunHandle>>>;

static RUNNERS: OnceLock<RunnerMap> = OnceLock::new();

pub(super) fn runners() -> &'static RunnerMap {
    RUNNERS.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

// ---------- 内置 worker ----------

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
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string()))
    }
}

struct SleepWorker;

#[async_trait]
impl Worker for SleepWorker {
    fn name(&self) -> &str {
        "sleep"
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let ms = input
            .get("ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(60_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("slept {ms}ms"))
    }
}

struct FailWorker;

#[async_trait]
impl Worker for FailWorker {
    fn name(&self) -> &str {
        "fail"
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        Err(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "fail worker 注入失败".to_string()))
    }
}

pub(super) fn builtin_workers(state: Option<&AppState>, worker_pool_mode: bool) -> WorkerRegistry {
    let registry = WorkerRegistry::new();
    // worker_pool 模式下不注册进程内 echo/sleep/fail：
    // 否则 resolve_worker 按 registry 优先派发，子进程池永远不会被选中。
    if !worker_pool_mode {
        registry.register(Arc::new(EchoWorker));
        registry.register(Arc::new(SleepWorker));
        registry.register(Arc::new(FailWorker));
    }
    // R5：真实 Agent worker（name="agent"）始终进程内执行（依赖模型 Provider 与凭据处理）。
    if let Some(state) = state {
        registry.register(Arc::new(agent_worker::AgentWorker::new(
            state.agent.clone(),
            state.workspace.clone(),
        )));
    }
    registry
}
