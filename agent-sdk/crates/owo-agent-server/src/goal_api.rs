//! Goal/Plan 编排 HTTP API（Lane D Part 1）。
//!
//! - 存储：`data_root/goals/<goal_id>/goal.json、plan.json、runs/run-<run_id>.json`
//!   （Goal/GoalRunState/Plan 均为 serde 结构，persist/load 复用 core 能力）。
//! - 内置演示 worker：echo（回显输入文本）、sleep（按参数毫秒睡眠）、fail（按参数失败，演示重试/replan）。
//! - 运行注册表：`OnceLock<Mutex<HashMap<(goal_id, run_id), Arc<tokio::Mutex<GoalRunner>>>>>` 供 abort。
//! - 审计：按 data_root 键控的 `AuditLog`，写操作全部留痕，`GET /goal/{id}/audit` 暴露尾部。
//! - P1 运行模式：`POST /goal/{id}/run` 的 `execution` 字段显式选择执行路径：
//!   `process`（默认，进程内语义不变）/ `worker_pool`（子进程池，必须显式开启并提供受控配置）。
//!   worker_pool 模式安全约束：命令仅限当前可执行文件（canonicalized 比较）；env 白名单
//!   清空宿主环境后注入（凭据类键 → 400）；cwd 显式校验；生命周期事件（启动/预算中止/取消/
//!   停止）经 `WorkerPool::attach_audit` 进入本 API 同一审计链路；运行结束 shutdown 回收子进程。
//!   A1 收口：正式宿主入口已在 `owo-agent` CLI 落地（`--owo-worker-child --handler
//!   <echo|sleep|fail>`，见 `crates/owo-agent-cli/src/worker_child.rs`）。生产部署由 CLI
//!   serve 启动服务，此时 current_exe 即协议宿主二进制——受控命令形如
//!   `command=<owo-agent.exe>, args=["--owo-worker-child","--handler","echo"]`，
//!   不再依赖测试二进制自举；测试环境仍以测试可执行文件自举验证同一机制。
//! - A2 显式执行目标：`execution.targets[]` 按 worker 声明 `in_process` / `local_process`
//!   / `fleet_node`（fleet_node 必须携带明确 node_id）。显式绑定只走对应通道，
//!   不可用即返回等待/询问/拒绝 disposition——禁止静默切换更高权限目标或改派。
//!   兼容约束：旧 `mode:"process"|"worker_pool"` 语义逐位不变（未声明绑定的步骤完全
//!   沿用历史解析顺序）；`local_process` 绑定必须在同一请求的 `execution.workers`
//!   提供受控子进程配置（否则 400）；`in_process` 绑定在 worker_pool 模式下与「内置
//!   进程内 worker 被剥离」矛盾（400）；非法 target 字面量由 serde 反序列化拒绝（422）。
//!   绑定最终映射为核心 `WorkerBinding`（owo-agent-core `execution_target`），correlation
//!   ID 默认派生 `<goal_id>/<run_id>/<worker>`，可按绑定覆盖。
//!
//! 本模块不引用 `crate::`/`super::`（AppState 全限定 `owo_agent_server::AppState`），
//! 可被测试以 `#[path = "../src/goal_api.rs"] mod goal_api;` 独立编译。

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::{Json, Router};
use owo_agent_core::audit::AuditLog;
use owo_agent_core::goal::{
    Goal, GoalBudget, GoalRunState, GoalRunner, GoalStatus, RunnerConfig, Worker, WorkerRegistry,
};
use owo_agent_core::plan::{Plan, StepSpec, VerificationSpec};
use owo_agent_core::worker_pool::{WorkerBudget, WorkerPool, WorkerSpec};
use owo_agent_server::AppState;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

// R5：agent worker 作为本模块子模块编译（lib.rs 无需登记；独立编译、无 crate 引用）。
#[path = "agent_worker.rs"]
pub mod agent_worker;

// ---------- 存储路径 ----------

fn goals_dir(data_root: &Path) -> PathBuf {
    data_root.join("goals")
}

fn goal_dir(data_root: &Path, goal_id: &str) -> PathBuf {
    goals_dir(data_root).join(goal_id)
}

fn goal_file(data_root: &Path, goal_id: &str) -> PathBuf {
    goal_dir(data_root, goal_id).join("goal.json")
}

fn runs_dir(data_root: &Path, goal_id: &str) -> PathBuf {
    goal_dir(data_root, goal_id).join("runs")
}

fn not_found(detail: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::NOT_FOUND, Json(json!({ "error": detail })))
}

fn bad_request(detail: &str) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": detail })))
}

// ---------- 审计（按 data_root 键控） ----------

type AuditMap = Arc<Mutex<HashMap<PathBuf, Arc<Mutex<AuditLog>>>>>;

static AUDITS: OnceLock<AuditMap> = OnceLock::new();

fn audits() -> &'static AuditMap {
    AUDITS.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

fn audit_for(data_root: &Path) -> Arc<Mutex<AuditLog>> {
    let mut map = audits().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(data_root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(AuditLog::default())))
        .clone()
}

fn audit_record(data_root: &Path, event: &str, detail: impl Into<String>) {
    if let Ok(mut log) = audit_for(data_root).lock() {
        log.record("goal-api", event, None, None, detail);
    }
}

// ---------- 运行注册表（abort） ----------

type RunnerHandle = Arc<tokio::sync::Mutex<GoalRunner>>;

/// 一次运行的句柄：runner（abort 兜底）+ 无锁取消通道（run 任务在飞时立即可达，
/// 不依赖等待 `runner.lock()` —— run 期间锁被 run 任务持有，锁等待会让取消在 run
/// 完成后才生效，子进程无法被及时终止）。
struct RunHandle {
    runner: RunnerHandle,
    cancel: tokio::sync::mpsc::UnboundedSender<()>,
}

type RunnerMap = Arc<Mutex<HashMap<(String, String), RunHandle>>>;

static RUNNERS: OnceLock<RunnerMap> = OnceLock::new();

fn runners() -> &'static RunnerMap {
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

fn builtin_workers(state: Option<&AppState>, worker_pool_mode: bool) -> WorkerRegistry {
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

// ---------- 请求模型 ----------

#[derive(Deserialize)]
struct CreateGoalRequest {
    objective: String,
    #[serde(default)]
    budget: Option<BudgetRequest>,
}

#[derive(Deserialize, Default)]
struct BudgetRequest {
    #[serde(default)]
    max_steps: Option<u32>,
    #[serde(default)]
    max_replans: Option<u32>,
}

#[derive(Deserialize)]
struct PlanStepInput {
    id: String,
    worker: String,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    verify: Option<VerifyInput>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    parallel: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum VerifyInput {
    Text(String),
    Object { kind: String, value: Option<String> },
}

impl VerifyInput {
    fn to_spec(&self) -> VerificationSpec {
        match self {
            VerifyInput::Text(text) => VerificationSpec::OutputContains(text.clone()),
            VerifyInput::Object { kind, value } => match (kind.as_str(), value) {
                ("equals", Some(v)) => VerificationSpec::OutputEquals(v.clone()),
                ("nonempty", _) => VerificationSpec::OutputNonEmpty,
                (_, Some(v)) => VerificationSpec::OutputContains(v.clone()),
                _ => VerificationSpec::OutputNonEmpty,
            },
        }
    }
}

#[derive(Deserialize)]
struct PlanRequest {
    steps: Vec<PlanStepInput>,
}

#[derive(Deserialize, Default)]
struct RunConfigInput {
    #[serde(default)]
    parallelism: Option<usize>,
    #[serde(default)]
    allow_replan: Option<bool>,
    /// P1：显式选择执行路径。缺省 = `process`（进程内语义，与历史一致）；
    /// `worker_pool` 必须显式开启并提供受控 worker 配置（见 [`ExecutionInput`]）。
    #[serde(default)]
    execution: Option<ExecutionInput>,
}

/// P1：Goal/Plan 步骤执行路径选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExecutionMode {
    /// 进程内执行（默认；语义与历史完全一致）。
    Process,
    /// 经 WorkerPool 子进程池执行（必须显式开启并提供受控配置）。
    WorkerPool,
}

/// `execution` 字段载荷：选择运行模式与 worker_pool 受控配置，可叠加按 worker 的显式目标绑定。
#[derive(Deserialize)]
struct ExecutionInput {
    /// 缺省 = `process`（与历史一致）；`worker_pool` 必须显式开启并提供受控配置。
    #[serde(default)]
    mode: Option<ExecutionMode>,
    /// worker_pool 模式下的子进程 worker 列表（该模式必须非空；命令仅限当前可执行文件）。
    #[serde(default)]
    workers: Vec<WorkerPoolWorkerInput>,
    /// A2：按 worker 的显式执行目标绑定（可选；显式绑定步骤只走对应通道，不改派）。
    #[serde(default)]
    targets: Vec<TargetBindingInput>,
}

impl ExecutionInput {
    fn mode(&self) -> ExecutionMode {
        self.mode.unwrap_or(ExecutionMode::Process)
    }
}

// ---------- A2：显式执行目标绑定 ----------

use owo_agent_core::execution_target::{
    BindingBudget as CoreBindingBudget, ExecutionTarget as CoreExecutionTarget,
    PermissionScope as CorePermissionScope, WorkerBinding as CoreWorkerBinding,
};

/// 把校验通过的规范化目标绑定映射为核心 [`CoreWorkerBinding`]（冻结字段一一对应；
/// correlation ID 缺省派生 `<goal_id>/<run_id>/<绑定键>`，显式声明优先）。
fn map_core_bindings(
    targets: &[NormalizedTarget],
    goal_id: &str,
    run_id: &str,
) -> Vec<CoreWorkerBinding> {
    targets
        .iter()
        .map(|t| {
            let target = match t.kind {
                TargetKind::InProcess => CoreExecutionTarget::InProcess,
                TargetKind::LocalProcess => CoreExecutionTarget::LocalProcess,
                TargetKind::FleetNode => CoreExecutionTarget::FleetNode {
                    node_id: t.node_id.clone().unwrap_or_default(),
                },
            };
            let mut binding = CoreWorkerBinding::new(t.worker.clone(), target);
            binding.capabilities = t.capabilities.clone();
            if let Some(scope) = &t.permission_scope {
                binding.permission_scope = CorePermissionScope {
                    allow: scope.allow.clone(),
                    deny: scope.deny.clone(),
                    network_egress: scope.network_egress,
                };
            }
            let mut budget = CoreBindingBudget::default();
            if let Some(v) = t.max_attempts {
                budget.max_attempts = v;
            }
            if let Some(v) = t.max_duration_secs {
                budget.max_duration_secs = v;
            }
            binding.budget = budget;
            binding.input_cas_ref = t.input_cas_ref.clone();
            binding.correlation_id = Some(
                t.correlation_id
                    .clone()
                    .unwrap_or_else(|| format!("{goal_id}/{run_id}/{}", t.worker)),
            );
            binding
        })
        .collect()
}

/// 执行目标种类（serde snake_case；未知字面量反序列化即失败 → 422）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TargetKind {
    InProcess,
    LocalProcess,
    FleetNode,
}

/// 单个按 worker 的目标绑定输入（字段与核心 `WorkerBinding` 冻结形状对齐）。
#[derive(Debug, Clone, Deserialize)]
struct TargetBindingInput {
    /// 绑定键：计划步骤 id（优先）或步骤声明的 worker 名。
    #[serde(default)]
    worker: String,
    /// 目标通道种类。
    target: TargetKind,
    /// `fleet_node` 必须提供明确节点 ID。
    #[serde(default)]
    node_id: Option<String>,
    /// 能力标签（透传核心 binding；审计/路由提示）。
    #[serde(default)]
    capabilities: Vec<String>,
    /// 权限范围（默认 deny：未列出的能力一律不授予）。
    #[serde(default)]
    permission_scope: Option<PermissionScopeInput>,
    /// 步骤级预算（max_attempts 对 retries 取 min；max_duration_secs 供派发超时/池预算派生）。
    #[serde(default)]
    budget: Option<StepBudgetInput>,
    /// 输入 CAS 引用（内容寻址产物血缘；透传核心 binding）。
    #[serde(default)]
    input_cas_ref: Option<String>,
    /// 关联 ID（缺省派生 `<goal_id>/<run_id>/<worker>`）。
    #[serde(default)]
    correlation_id: Option<String>,
}

/// 绑定级预算（对齐核心 `BindingBudget`：max_attempts 对 plan retries 取 min；
/// max_duration_secs 供派发超时与池预算派生）。
#[derive(Debug, Clone, Deserialize, Default)]
struct StepBudgetInput {
    #[serde(default)]
    max_attempts: Option<u32>,
    #[serde(default)]
    max_duration_secs: Option<u64>,
}

/// 绑定级权限范围输入（对齐核心 `PermissionScope`；默认 deny，deny 优先于 allow）。
#[derive(Debug, Clone, Deserialize, Default)]
struct PermissionScopeInput {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    network_egress: bool,
}

/// 校验通过的规范化目标绑定（A2 预备层；核心 WorkerBinding 映射在 run 装配处构造）。
/// 仅本模块使用：字段类型同样保持模块私有，避免「私有类型泄漏」告警。
#[derive(Debug, Clone)]
struct NormalizedTarget {
    worker: String,
    kind: TargetKind,
    node_id: Option<String>,
    capabilities: Vec<String>,
    permission_scope: Option<PermissionScopeInput>,
    max_attempts: Option<u32>,
    max_duration_secs: Option<u64>,
    input_cas_ref: Option<String>,
    correlation_id: Option<String>,
}

/// 内置进程内 worker 名（worker_pool 模式下会被剥离注册）。
const INPROCESS_BUILTINS: [&str; 3] = ["echo", "sleep", "fail"];

/// 校验显式目标绑定：只做结构性/矛盾性前置校验（运行期可用性由核心以
/// 等待/询问/拒绝 disposition 表达，绝不静默改派）。任何非法输入返回 Err → 400/422。
/// 绑定键语义与核心 `select_binding` 一致：步骤 id 精确匹配优先，其次按
/// 步骤声明的 worker 名匹配；同一请求内一个键只允许一个显式目标。
fn validate_targets(
    execution: &ExecutionInput,
    plan: &Plan,
) -> Result<Vec<NormalizedTarget>, String> {
    if execution.targets.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let pool_names: HashSet<&str> = execution.workers.iter().map(|w| w.name.trim()).collect();
    let mut normalized = Vec::new();
    for t in &execution.targets {
        let key = t.worker.trim().to_string();
        if key.is_empty() {
            return Err("targets[].worker 不能为空".to_string());
        }
        if !seen.insert(key.clone()) {
            return Err(format!(
                "targets 对同一键重复绑定：{key}（一个 worker/step 只允许一个显式目标）"
            ));
        }
        // 计划中必须存在该键命中的步骤（step id 精确匹配或 worker 名引用），否则绑定无效。
        let bound_step = plan.steps.iter().find(|s| s.id == key || s.worker == key);
        let Some(bound_step) = bound_step else {
            return Err(format!(
                "targets 绑定键「{key}」未命中计划中的任何步骤 id 或 worker 名"
            ));
        };
        // 绑定键可能按 step id 命中：实际执行 worker 以步骤声明为准（agent 判定/内置判定用）。
        let effective_worker = if bound_step.id == key {
            bound_step.worker.as_str()
        } else {
            key.as_str()
        };
        // agent 步骤恒进程内（依赖模型 Provider 与凭据处理），不得下沉到子进程或远端节点。
        if effective_worker == "agent" && t.target != TargetKind::InProcess {
            return Err(format!(
                "worker「agent」只能绑定 in_process（模型凭据不离开本进程），收到：{:?}",
                t.target
            ));
        }
        let node_id = t
            .node_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match t.target {
            TargetKind::FleetNode => {
                let node_id = node_id.ok_or_else(|| {
                    format!(
                        "targets[{key}] target=fleet_node 必须提供明确 node_id（不允许隐式选节点）"
                    )
                })?;
                normalized.push(NormalizedTarget {
                    worker: key.clone(),
                    kind: t.target,
                    node_id: Some(node_id.to_string()),
                    capabilities: t.capabilities.clone(),
                    permission_scope: t.permission_scope.clone(),
                    max_attempts: t.budget.as_ref().and_then(|b| b.max_attempts),
                    max_duration_secs: t.budget.as_ref().and_then(|b| b.max_duration_secs),
                    input_cas_ref: t.input_cas_ref.clone(),
                    correlation_id: t.correlation_id.clone(),
                });
            }
            TargetKind::LocalProcess => {
                // 与 worker 配置矛盾：本地子进程目标最终落到步骤声明的 worker 名上
                //（核心把按 step id 键控的绑定为步骤 worker），故池必须包含该名字。
                let needs = [key.as_str(), effective_worker];
                if pool_names.is_empty() || !needs.iter().any(|n| pool_names.contains(n)) {
                    return Err(format!(
                        "targets[{key}] target=local_process 与 execution.workers 矛盾：\
                         绑定键或其步骤声明的 worker「{effective_worker}」必须在同一请求 \
                         execution.workers 中提供受控配置"
                    ));
                }
                normalized.push(NormalizedTarget {
                    worker: key.clone(),
                    kind: t.target,
                    node_id: None,
                    capabilities: t.capabilities.clone(),
                    permission_scope: t.permission_scope.clone(),
                    max_attempts: t.budget.as_ref().and_then(|b| b.max_attempts),
                    max_duration_secs: t.budget.as_ref().and_then(|b| b.max_duration_secs),
                    input_cas_ref: t.input_cas_ref.clone(),
                    correlation_id: t.correlation_id.clone(),
                });
            }
            TargetKind::InProcess => {
                // worker_pool 模式下内置进程内 worker 已被剥离注册 → 明确矛盾（400）。
                if matches!(execution.mode(), ExecutionMode::WorkerPool)
                    && INPROCESS_BUILTINS.contains(&effective_worker)
                {
                    return Err(format!(
                        "targets[{key}] target=in_process 与 mode=worker_pool 矛盾：\
                         该模式下内置进程内 worker 未注册，仅 agent 可绑定 in_process"
                    ));
                }
                normalized.push(NormalizedTarget {
                    worker: key.clone(),
                    kind: t.target,
                    node_id: None,
                    capabilities: t.capabilities.clone(),
                    permission_scope: t.permission_scope.clone(),
                    max_attempts: t.budget.as_ref().and_then(|b| b.max_attempts),
                    max_duration_secs: t.budget.as_ref().and_then(|b| b.max_duration_secs),
                    input_cas_ref: t.input_cas_ref.clone(),
                    correlation_id: t.correlation_id.clone(),
                });
            }
        }
    }
    Ok(normalized)
}

/// 单个受控子进程 worker 的配置。
#[derive(Deserialize)]
struct WorkerPoolWorkerInput {
    /// 与计划步骤 worker 名一致（agent 除外）。
    name: String,
    /// 受控命令：必须 canonicalize 到当前可执行文件（协议宿主）。
    command: String,
    /// 附加参数（字面 argv，不经 shell；拒绝空串/NUL/换行）。
    #[serde(default)]
    args: Vec<String>,
    /// 显式工作目录（必须存在）。
    cwd: String,
    /// 环境白名单：宿主环境清空后仅注入这些变量；凭据类键（含 OPENAI_API_KEY）→ 400。
    #[serde(default)]
    env: HashMap<String, String>,
    /// 预算（可选；0 = 不限）。
    #[serde(default)]
    budget: Option<WorkerBudgetInput>,
    /// 崩溃重启次数上限（默认 3）。
    #[serde(default)]
    max_restarts: Option<u32>,
    /// 重启指数退避基数秒（默认 1）。
    #[serde(default)]
    base_backoff_secs: Option<u64>,
}

/// 子进程 worker 预算（映射 [`WorkerBudget`]）。
#[derive(Deserialize, Default)]
struct WorkerBudgetInput {
    #[serde(default)]
    max_turns: Option<u32>,
    #[serde(default)]
    max_duration_secs: Option<u64>,
    #[serde(default)]
    max_memory_mb: Option<u64>,
    #[serde(default)]
    max_cpu_cores: Option<f32>,
}

// ---------- P1：WorkerPool 运行模式校验 ----------

/// 凭据类环境键：白名单中不允许（不得把凭据传入子进程）。
fn is_credential_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

/// 校验 `execution` 并生成受控 worker 规格。
/// `process` 模式：返回空（语义与历史一致）。
/// `worker_pool` 模式：命令必须 canonicalize 到当前可执行文件；cwd 必须存在；
/// env 白名单不得含凭据类键；计划中非 agent 步骤的 worker 名必须全部在池中。
/// 任何非法输入返回 Err（由调用方映射为 400）。
fn validate_execution(
    execution: &ExecutionInput,
    plan: &Plan,
) -> Result<(Vec<WorkerSpec>, HashSet<String>), String> {
    if matches!(execution.mode(), ExecutionMode::Process) {
        // process 模式不允许携带受控子进程配置（与显式目标矛盾）。
        if !execution.workers.is_empty() {
            return Err(
                "mode=process 不接受 execution.workers（受控子进程配置仅属于 worker_pool 模式）"
                    .to_string(),
            );
        }
        return Ok((Vec::new(), HashSet::new()));
    }
    if execution.workers.is_empty() {
        return Err("worker_pool 模式必须提供非空 workers 列表".to_string());
    }
    let current_canon = std::fs::canonicalize(
        std::env::current_exe().map_err(|e| format!("无法定位当前可执行文件：{e}"))?,
    )
    .map_err(|e| format!("当前可执行文件 canonicalize 失败：{e}"))?;
    let mut specs = Vec::new();
    let mut names = HashSet::new();
    for w in &execution.workers {
        let name = w.name.trim();
        if name.is_empty() {
            return Err("worker name 不能为空".to_string());
        }
        if !names.insert(name.to_string()) {
            return Err(format!("worker name 重复：{name}"));
        }
        // 受控命令：仅限当前可执行文件（canonicalized 比较）。
        let cmd_canon = std::fs::canonicalize(&w.command)
            .map_err(|_| format!("worker {name} 命令不可解析：{}", w.command))?;
        if cmd_canon != current_canon {
            return Err(format!(
                "worker {name} 命令必须是当前服务可执行文件（受控 worker 协议宿主），收到：{}",
                w.command
            ));
        }
        for arg in &w.args {
            if arg.is_empty() || arg.contains('\0') || arg.contains('\n') {
                return Err(format!("worker {name} 参数非法：{arg:?}"));
            }
        }
        // 显式工作目录：必须存在且为目录。
        let cwd = std::fs::canonicalize(&w.cwd)
            .map_err(|_| format!("worker {name} 工作目录不存在：{}", w.cwd))?;
        if !cwd.is_dir() {
            return Err(format!("worker {name} 工作目录不是目录：{}", w.cwd));
        }
        // 环境白名单：凭据类键拒绝（宿主环境已被 env_clear 清空，仅注入白名单）。
        for key in w.env.keys() {
            if is_credential_key(key) {
                return Err(format!(
                    "worker {name} 环境白名单含凭据类键：{key}（凭据不得传入子进程）"
                ));
            }
        }
        let mut budget = WorkerBudget::default();
        if let Some(b) = &w.budget {
            if let Some(v) = b.max_turns {
                budget.max_turns = v;
            }
            if let Some(v) = b.max_duration_secs {
                budget.max_duration_secs = v;
            }
            if let Some(v) = b.max_memory_mb {
                budget.max_memory_mb = v;
            }
            if let Some(v) = b.max_cpu_cores {
                budget.max_cpu_cores = v;
            }
        }
        let spec = WorkerSpec::new(name.to_string(), &w.command)
            .args(w.args.clone())
            .cwd(&cwd)
            .env_whitelist(w.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .budget(budget)
            .restart_rule(owo_agent_core::fleet::RestartRule {
                max_restarts: w.max_restarts.unwrap_or(3),
                base_backoff_secs: w.base_backoff_secs.unwrap_or(1),
                policy: owo_agent_core::fleet::RestartPolicy::OneForOne,
            });
        specs.push(spec);
    }
    // 计划步骤校验：非 agent 步骤的 worker 名必须全部在池中（否则执行不可达 → 明确 400）。
    for step in &plan.steps {
        if step.worker == "agent" {
            continue;
        }
        if !names.contains(&step.worker) {
            return Err(format!(
                "worker_pool 模式：计划步骤 {} 引用 worker「{}」，但未在 execution.workers 中提供",
                step.id, step.worker
            ));
        }
    }
    Ok((specs, names))
}

// ---------- 持久化辅助 ----------

fn read_goal(data_root: &Path, goal_id: &str) -> Result<Goal, (StatusCode, Json<Value>)> {
    let path = goal_file(data_root, goal_id);
    let raw =
        std::fs::read_to_string(&path).map_err(|_| not_found(&format!("目标 {goal_id} 不存在")))?;
    serde_json::from_str(&raw).map_err(|e| bad_request(&format!("goal.json 解析失败：{e}")))
}

fn write_goal(data_root: &Path, goal: &Goal) -> Result<(), (StatusCode, Json<Value>)> {
    let dir = goal_dir(data_root, &goal.id);
    std::fs::create_dir_all(&dir).map_err(|e| bad_request(&format!("创建目标目录失败：{e}")))?;
    let raw = serde_json::to_string_pretty(goal)
        .map_err(|e| bad_request(&format!("goal 序列化失败：{e}")))?;
    std::fs::write(goal_file(data_root, &goal.id), raw)
        .map_err(|e| bad_request(&format!("goal 写入失败：{e}")))
}

fn read_plan(data_root: &Path, goal_id: &str) -> Result<Plan, (StatusCode, Json<Value>)> {
    Plan::load(&goal_dir(data_root, goal_id), "plan")
        .map_err(|_| not_found(&format!("目标 {goal_id} 尚无计划")))
}

// ---------- 路由 ----------

/// Lane D Part 1 路由：/goal/*（供主控并入 build_router）。
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/goal", axum::routing::get(list_goals).post(create_goal))
        .route("/goal/{id}", axum::routing::get(get_goal))
        .route(
            "/goal/{id}/plan",
            axum::routing::get(get_plan).post(create_plan),
        )
        .route("/goal/{id}/run", axum::routing::post(start_run))
        .route("/goal/{id}/status", axum::routing::get(goal_status))
        .route("/goal/{id}/abort", axum::routing::post(abort_goal))
        .route("/goal/{id}/audit", axum::routing::get(goal_audit))
        .route("/goal/{id}/runs", axum::routing::get(goal_runs))
        .with_state(state)
}

/// 创建目标：`POST /goal {objective, budget?}`。
async fn create_goal(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateGoalRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if request.objective.trim().is_empty() {
        return Err(bad_request("objective 不能为空"));
    }
    let mut goal = Goal::new(uuid::Uuid::new_v4().to_string(), request.objective.trim());
    goal.transition(GoalStatus::Planning);
    if let Some(budget) = request.budget {
        let mut b = GoalBudget::default();
        if let Some(max_steps) = budget.max_steps {
            b.max_steps = max_steps;
        }
        if let Some(max_replans) = budget.max_replans {
            b.max_replans = max_replans;
        }
        goal.budget = b;
    }
    write_goal(&state.data_root, &goal)?;
    audit_record(
        &state.data_root,
        "goal.create",
        format!("创建目标 {}", goal.id),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "goal": goal })),
    ))
}

/// 目标列表：`GET /goal`。
async fn list_goals(State(state): State<Arc<AppState>>) -> Json<Value> {
    let mut goals = Vec::new();
    let dir = goals_dir(&state.data_root);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let goal_id = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if let Ok(goal) = read_goal(&state.data_root, &goal_id) {
                goals.push(json!({
                    "id": goal.id,
                    "objective": goal.objective,
                    "status": format!("{:?}", goal.status),
                    "created_at": goal.created_at,
                    "updated_at": goal.updated_at,
                }));
            }
        }
    }
    goals.sort_by(|a, b| {
        a.get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                b.get("created_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    Json(json!({ "goals": goals, "count": goals.len() }))
}

/// 目标详情：`GET /goal/{id}`。
async fn get_goal(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let goal = read_goal(&state.data_root, &goal_id)?;
    Ok(Json(json!(goal)))
}

/// 创建/替换计划：`POST /goal/{id}/plan {steps:[...]}`（环检测 + waves 预览）。
async fn create_plan(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
    Json(request): Json<PlanRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let goal = read_goal(&state.data_root, &goal_id)?;
    if request.steps.is_empty() {
        return Err(bad_request("steps 不能为空"));
    }
    let mut plan = Plan::new("plan".to_string(), goal_id.clone());
    let agent_model = resolve_agent_model(&request.steps);
    for step in request.steps {
        if step.id.is_empty() || step.worker.is_empty() {
            return Err(bad_request("步骤 id/worker 不能为空"));
        }
        // R5：agent 步骤必须在 input 中提供非空 prompt（预校验 → 400）。
        if step.worker == "agent" {
            let input = step.input.clone().unwrap_or(Value::Null);
            if let Err(error) = agent_worker::validate_agent_input(&input) {
                return Err(bad_request(error.as_str()));
            }
        }
        let mut spec = StepSpec::new(step.id.clone(), step.worker.clone());
        spec.depends_on = step.deps;
        spec.parallel = step.parallel;
        spec.input = step.input.unwrap_or(Value::Null);
        spec.verify = step.verify.map(|v| v.to_spec());
        spec.retries = step.max_retries.unwrap_or(0);
        plan.add_step(spec);
    }
    if let Err(error) = plan.validate() {
        return Err(bad_request(&format!("计划非法：{error}")));
    }
    let waves = plan
        .topological_waves()
        .map_err(|e| bad_request(&format!("拓扑排序失败：{e}")))?;
    plan.persist(&goal_dir(&state.data_root, &goal_id))
        .map_err(|e| bad_request(&format!("计划保存失败：{e}")))?;
    audit_record(
        &state.data_root,
        "goal.plan",
        format!("目标 {goal_id} 计划已保存（{} 步）", plan.steps.len()),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "goal_id": goal_id,
            "plan": plan,
            "waves": waves,
            "valid": true,
            "objective": goal.objective,
            "agent_model": agent_model,
        })),
    ))
}

/// R5：计划含 agent 步骤时，返回该步骤将使用的模型名（input.model → OWO_AGENT_MODEL → 缺省）。
fn resolve_agent_model(steps: &[PlanStepInput]) -> Value {
    for step in steps {
        if step.worker == "agent" {
            let input = step.input.clone().unwrap_or(Value::Null);
            return json!(agent_worker::AgentWorker::resolve_model(&input));
        }
    }
    Value::Null
}

/// 计划详情：`GET /goal/{id}/plan`。
async fn get_plan(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let plan = read_plan(&state.data_root, &goal_id)?;
    let waves = plan
        .topological_waves()
        .map_err(|e| bad_request(&format!("拓扑排序失败：{e}")))?;
    Ok(Json(json!({ "plan": plan, "waves": waves, "valid": true })))
}

/// 启动运行：`POST /goal/{id}/run {config?}` → 202 {run_id}。
async fn start_run(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
    Json(request): Json<RunConfigInput>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let goal = read_goal(&state.data_root, &goal_id)?;
    let plan = read_plan(&state.data_root, &goal_id)?;
    let runs = runs_dir(&state.data_root, &goal_id);
    std::fs::create_dir_all(&runs).map_err(|e| bad_request(&format!("创建 runs 目录失败：{e}")))?;

    let run_id = format!("run-{uuid}", uuid = uuid::Uuid::new_v4());
    // P1：显式选择执行路径；缺省 process（进程内语义不变）。非法输入 → 明确 400。
    let worker_pool_mode = request
        .execution
        .as_ref()
        .map(|e| matches!(e.mode(), ExecutionMode::WorkerPool))
        .unwrap_or(false);
    let (worker_specs, _worker_names) = match &request.execution {
        Some(execution) => validate_execution(execution, &plan).map_err(|e| bad_request(&e))?,
        None => (Vec::new(), HashSet::new()),
    };
    // A2：显式执行目标绑定校验（结构性/矛盾性前置；运行期不可用由核心 disposition 表达，
    // 等待/询问/拒绝——绝不静默改派）。映射为核心 WorkerBinding 后随 RunnerConfig 下发。
    let normalized_targets: Vec<NormalizedTarget> = match &request.execution {
        Some(execution) => validate_targets(execution, &plan).map_err(|e| bad_request(&e))?,
        None => Vec::new(),
    };
    if !normalized_targets.is_empty() {
        audit_record(
            &state.data_root,
            "goal.run.targets",
            format!(
                "目标 {goal_id} 显式执行目标：{}",
                normalized_targets
                    .iter()
                    .map(|t| {
                        let node = t
                            .node_id
                            .as_deref()
                            .map(|n| format!("({n})"))
                            .unwrap_or_default();
                        format!("{}→{:?}{}", t.worker, t.kind, node)
                    })
                    .collect::<Vec<_>>()
                    .join("，")
            ),
        );
    }
    // A2：存在 fleet_node 绑定时才挂载控制面传输（进程级共享，任务可被真实节点领取）；
    // 否则维持 None（不引入隐式远端回退路径）。
    let needs_fleet_transport = normalized_targets
        .iter()
        .any(|t| t.kind == TargetKind::FleetNode);
    // A2 准入校验：fleet_node 绑定必须在控制面存在**已注册**节点，否则请求即明确
    // 400（配置错误）。绝不把注定不可路由的任务提交进队列静默长挂，更不改派本地。
    if needs_fleet_transport {
        let hub = owo_agent_server::fleet_hub(&state.data_root);
        for t in &normalized_targets {
            if t.kind != TargetKind::FleetNode {
                continue;
            }
            let node_id = t.node_id.clone().unwrap_or_default();
            let registered = hub
                .nodes
                .lock()
                .map(|m| m.contains_key(&node_id))
                .unwrap_or(false);
            if !registered {
                return Err(bad_request(&format!(
                    "targets[{}] fleet_node 目标不可路由：节点「{}」未在控制面注册\
                     （不隐式改派到其他目标或本地执行）",
                    t.worker, node_id
                )));
            }
        }
    }
    let target_bindings = map_core_bindings(&normalized_targets, &goal_id, &run_id);
    // P1：worker_pool 模式先建池（spawn 在后台任务执行，避免请求阻塞于 ready 握手）；
    // 池已挂接本 API 同一审计链路，生命周期事件与 goal 写操作同源。
    let pool = if worker_pool_mode {
        let pool = WorkerPool::new();
        pool.attach_audit(audit_for(&state.data_root)).await;
        Some(pool)
    } else {
        None
    };
    let config = RunnerConfig {
        max_parallel: request.parallelism.unwrap_or(2).max(1),
        persist_dir: Some(runs),
        allow_replan: request.allow_replan.unwrap_or(true),
        use_worker_pool: worker_pool_mode,
        worker_pool: pool.clone(),
        capability_registry: None,
        capability_requirement: None,
        // A2：仅当存在 fleet_node 显式绑定才挂载进程级控制面传输（真实节点协议领取）。
        transport: if needs_fleet_transport {
            Some(Arc::new(
                owo_agent_server::fleet_hub(&state.data_root)
                    .transport
                    .clone(),
            )
                as Arc<dyn owo_agent_core::fleet_transport::FleetTransport>)
        } else {
            None
        },
        leases: None,
        // A2：显式执行绑定（核心按 select_binding 定向派发；不可用即等待/询问/拒绝）。
        bindings: target_bindings,
    };
    let mut runner = GoalRunner::new(goal.clone(), plan, config);
    runner.attach_audit(audit_for(&state.data_root));
    runner.state.run_id = run_id.clone();
    let runner = Arc::new(tokio::sync::Mutex::new(runner));
    // P1：无锁取消通道。run 任务用 select 监听；abort 时先经此通道立即传播
    // （worker_pool 终止子进程），再以 runner.abort() 兜底落 Aborted 态。
    let (cancel_tx, mut cancel_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    {
        let mut map = runners().lock().unwrap_or_else(|e| e.into_inner());
        map.insert(
            (goal_id.clone(), run_id.clone()),
            RunHandle {
                runner: Arc::clone(&runner),
                cancel: cancel_tx,
            },
        );
    }

    let workers = builtin_workers(Some(state.as_ref()), worker_pool_mode);
    let data_root = state.data_root.clone();
    let goal_id_clone = goal_id.clone();
    let run_id_clone = run_id.clone();
    tokio::spawn(async move {
        // P1：spawn 受控子进程 worker。启动失败记录审计并继续运行；
        // 缺失 worker 的步骤经 pool 未命中落到已定义 Failed 态（不会出现 Unknown）。
        if let Some(pool) = &pool {
            let mut failed = Vec::new();
            for spec in &worker_specs {
                if let Err(e) = pool.spawn(spec.clone()).await {
                    failed.push(format!("{}: {e}", spec.id));
                }
            }
            if !failed.is_empty() {
                audit_record(
                    &data_root,
                    "goal.worker_pool.spawn_failed",
                    format!(
                        "目标 {goal_id_clone} worker 启动失败：{}",
                        failed.join("；")
                    ),
                );
            }
        }
        let mut guard = runner.lock().await;
        // P1：run 与取消信号竞争。取消在 run 在飞时无锁生效：
        // 先 cancel_all 传播到子进程（pending 立即可见），再 abort() 落 Aborted 终态。
        let mut cancelled = false;
        let run_result = tokio::select! {
            s = guard.run(&workers) => s,
            _ = cancel_rx.recv() => {
                if let Some(pool) = &pool {
                    let _ = pool.cancel_all().await;
                }
                cancelled = true;
                Ok(GoalStatus::Aborted)
            }
        };
        if cancelled {
            guard.abort();
        }
        let status = match run_result {
            Ok(s) => s,
            Err(e) => {
                audit_record(
                    &data_root,
                    "goal.run.error",
                    format!("目标 {goal_id_clone} 运行异常：{e}"),
                );
                GoalStatus::Failed
            }
        };
        drop(guard);
        // P1：运行结束 shutdown 回收全部子进程（kill 兜底，无孤儿）。
        if let Some(pool) = &pool {
            pool.shutdown().await;
        }
        let mut map = runners().lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&(goal_id_clone.clone(), run_id_clone.clone()));
        audit_record(
            &data_root,
            "goal.run.finished",
            format!("目标 {goal_id_clone} 运行 {run_id_clone} → {status:?}"),
        );
    });

    audit_record(
        &state.data_root,
        "goal.run.start",
        format!("目标 {goal_id} 启动运行 {run_id}"),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "ok": true, "goal_id": goal_id, "run_id": run_id })),
    ))
}

/// 运行状态快照：`GET /goal/{id}/status`（最新 run-*.json 的 GoalRunState）。
async fn goal_status(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let runs = runs_dir(&state.data_root, &goal_id);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&runs)
        .map_err(|_| not_found(&format!("目标 {goal_id} 尚无运行")))?
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
        .map(|e| e.path())
        .collect();
    if files.is_empty() {
        return Err(not_found(&format!("目标 {goal_id} 尚无运行")));
    }
    files.sort();
    let latest = files.last().unwrap();
    let run_id = latest
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let state_value = GoalRunState::load(&runs, &run_id)
        .map_err(|e| bad_request(&format!("运行状态读取失败：{e}")))?;
    let plan = Plan::load(&goal_dir(&state.data_root, &goal_id), "plan")
        .unwrap_or_else(|_| Plan::new("plan".to_string(), goal_id.clone()));
    // R5：每步骤输出（截断 2000 字符）+ worker 名 + agent 模型名。
    let steps: Vec<Value> = state_value
        .records
        .iter()
        .map(|(step_id, record)| {
            let worker = plan
                .steps
                .iter()
                .find(|s| &s.id == step_id)
                .map(|s| s.worker.clone())
                .unwrap_or_default();
            let model = if worker == "agent" {
                let input = plan
                    .steps
                    .iter()
                    .find(|s| &s.id == step_id)
                    .map(|s| s.input.clone())
                    .unwrap_or(Value::Null);
                json!(agent_worker::AgentWorker::resolve_model(&input))
            } else {
                Value::Null
            };
            let output = record.output.clone().unwrap_or_default();
            let truncated = output.chars().count() > 2000;
            let output = output.chars().take(2000).collect::<String>();
            json!({
                "step_id": step_id,
                "worker": worker,
                "model": model,
                "status": format!("{:?}", record.status),
                "attempts": record.attempts,
                "output": output,
                "output_truncated": truncated,
                "error": record.error,
            })
        })
        .collect();
    let mut value = serde_json::to_value(&state_value)
        .map_err(|e| bad_request(&format!("运行状态序列化失败：{e}")))?;
    value["run_id"] = json!(run_id);
    value["goal_status"] = json!(format!("{:?}", state_value.goal.status));
    value["steps"] = Json(json!(steps)).0;
    Ok(Json(value))
}

/// 中止运行：`POST /goal/{id}/abort`。
async fn abort_goal(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    read_goal(&state.data_root, &goal_id)?;
    // 先收集运行句柄（std MutexGuard 不能跨 await），再逐个取消。
    let targets: Vec<RunHandle> = {
        let map = runners().lock().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|((gid, _), _)| gid == &goal_id)
            .map(|(_, handle)| RunHandle {
                runner: Arc::clone(&handle.runner),
                cancel: handle.cancel.clone(),
            })
            .collect()
    };
    let mut aborted = 0usize;
    for handle in targets {
        // P1：先经无锁取消通道通知 run 任务（run 在飞时立即可达：
        // worker_pool 取消传播到子进程，run 任务随后 abort() 落 Aborted 终态）；
        // 再 lock().abort() 兜底（run 已结束或通道失效时直接强制 Aborted）。
        let _ = handle.cancel.send(());
        handle.runner.lock().await.abort();
        aborted += 1;
    }
    audit_record(
        &state.data_root,
        "goal.abort",
        format!("目标 {goal_id} 中止 {aborted} 个运行"),
    );
    Ok(Json(
        json!({ "ok": true, "goal_id": goal_id, "aborted": aborted }),
    ))
}

/// 审计尾部：`GET /goal/{id}/audit`。
async fn goal_audit(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    read_goal(&state.data_root, &goal_id)?;
    let entries = {
        let log = audit_for(&state.data_root);
        let guard = log.lock().map_err(|_| bad_request("审计锁中毒"))?;
        guard
            .entries
            .iter()
            .rev()
            .take(50)
            .cloned()
            .collect::<Vec<_>>()
    };
    Ok(Json(json!({ "goal_id": goal_id, "audit": entries })))
}

/// 运行列表：`GET /goal/{id}/runs`。
async fn goal_runs(
    State(state): State<Arc<AppState>>,
    AxumPath(goal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    read_goal(&state.data_root, &goal_id)?;
    let runs = runs_dir(&state.data_root, &goal_id);
    let mut list = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&runs) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|x| x == "json").unwrap_or(false) {
                let run_id = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                if let Ok(state_value) = GoalRunState::load(&runs, &run_id) {
                    list.push(json!({
                        "run_id": run_id,
                        "goal_status": format!("{:?}", state_value.goal.status),
                        "steps_taken": state_value.steps_taken,
                        "replan_count": state_value.replan_count,
                        "started_at": state_value.started_at,
                    }));
                }
            }
        }
    }
    list.sort_by(|a, b| {
        a.get("started_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                b.get("started_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    Ok(Json(
        json!({ "goal_id": goal_id, "runs": list, "count": list.len() }),
    ))
}
