use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::fleet::RestartRule;
pub type WorkerId = String;

/// ready 握手超时。
pub(super) const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// 心跳（ping/pong）超时。
pub(super) const PING_TIMEOUT: Duration = Duration::from_secs(3);
/// 状态轮询间隔。
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// 事件日志上限（最近 N 条）。
pub(super) const EVENT_CAP: usize = 200;

/// 隔离模式：本轮 `Process`（进程隔离）；`Sandbox` 为 Agent 3 OS 级沙箱接入点。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// 独立子进程隔离（本轮默认；资源上限为策略字段，OS 强制待沙箱接入）。
    #[default]
    Process,
    /// 经 OS 级沙箱执行（Job Object/AppContainer 等；由 Agent 3 沙箱实现接入）。
    Sandbox,
}

/// worker 预算（策略字段；轮次/时长池侧强制，内存/CPU 本轮仅表达，OS 强制由沙箱实现）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorkerBudget {
    /// 最大任务轮次（0 = 不限）。
    pub max_turns: u32,
    /// 最大运行时长（秒，0 = 不限；超时中止并 kill）。
    pub max_duration_secs: u64,
    /// 内存上限（MB；本轮为策略字段，沙箱接入后 OS 强制）。
    pub max_memory_mb: u64,
    /// CPU 核数上限（本轮为策略字段，沙箱接入后 OS 强制）。
    pub max_cpu_cores: f32,
}

impl Default for WorkerBudget {
    fn default() -> Self {
        Self {
            max_turns: 0,
            max_duration_secs: 0,
            max_memory_mb: 0,
            max_cpu_cores: 0.0,
        }
    }
}

impl WorkerBudget {
    pub fn exceeded(&self, turns: u32, elapsed: Duration) -> bool {
        (self.max_turns > 0 && turns >= self.max_turns)
            || (self.max_duration_secs > 0 && elapsed.as_secs() >= self.max_duration_secs)
    }
}

/// worker 进程规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    /// 注册名（总线/审计中的 worker 标识）。
    pub id: WorkerId,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// 环境变量白名单（仅这些变量传入子进程；其余一律不继承）。
    #[serde(default)]
    pub env_whitelist: Vec<(String, String)>,
    #[serde(default)]
    pub budget: WorkerBudget,
    #[serde(default)]
    pub isolation: IsolationMode,
    /// 崩溃重启规则（指数退避 + 熔断）。
    #[serde(default)]
    pub restart_rule: RestartRule,
}

impl WorkerSpec {
    pub fn new(id: impl Into<WorkerId>, command: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env_whitelist: Vec::new(),
            budget: WorkerBudget::default(),
            isolation: IsolationMode::Process,
            restart_rule: RestartRule::default(),
        }
    }

    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn env_whitelist(mut self, env_whitelist: Vec<(String, String)>) -> Self {
        self.env_whitelist = env_whitelist;
        self
    }

    pub fn budget(mut self, budget: WorkerBudget) -> Self {
        self.budget = budget;
        self
    }

    pub fn isolation(mut self, isolation: IsolationMode) -> Self {
        self.isolation = isolation;
        self
    }

    pub fn restart_rule(mut self, restart_rule: RestartRule) -> Self {
        self.restart_rule = restart_rule;
        self
    }
}

/// worker 运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerStatus {
    Starting,
    Running,
    Restarting { attempts: u32, next_retry_secs: u64 },
    Fused { attempts: u32 },
    Stopped,
}

impl fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Restarting {
                attempts,
                next_retry_secs,
            } => write!(
                f,
                "restarting(attempt={attempts}, backoff={next_retry_secs}s)"
            ),
            Self::Fused { attempts } => write!(f, "fused(attempt={attempts})"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

/// 取消传播的内部标记（区别于真实 worker 错误文本）。
pub(super) const CANCELLED_MARKER: &str = "__owo_cancelled__";

/// 池错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    UnknownWorker(WorkerId),
    Spawn(String),
    NotReady(WorkerId),
    Timeout(String),
    Protocol(String),
    Fused(WorkerId),
    Stopped(WorkerId),
    BudgetDuration { worker: WorkerId, reason: String },
    BudgetTurns { worker: WorkerId, max_turns: u32 },
    WorkerFailed(String),
    Cancelled(WorkerId),
    Io(String),
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownWorker(id) => write!(f, "未知 worker：{id}"),
            Self::Spawn(reason) => write!(f, "spawn 失败：{reason}"),
            Self::NotReady(id) => write!(f, "worker {id} 未就绪"),
            Self::Timeout(reason) => write!(f, "超时：{reason}"),
            Self::Protocol(reason) => write!(f, "协议错误：{reason}"),
            Self::Fused(id) => write!(f, "worker {id} 已熔断"),
            Self::Stopped(id) => write!(f, "worker {id} 已停止"),
            Self::BudgetDuration { worker, reason } => {
                write!(f, "worker {worker} 预算中止：{reason}")
            }
            Self::BudgetTurns { worker, max_turns } => {
                write!(f, "worker {worker} 预算中止：轮次上限 {max_turns}")
            }
            Self::WorkerFailed(reason) => write!(f, "worker 返回错误：{reason}"),
            Self::Cancelled(id) => write!(f, "worker {id} 任务被取消"),
            Self::Io(reason) => write!(f, "IO 错误：{reason}"),
        }
    }
}

impl Error for PoolError {}

/// 子进程回报（reader 任务 → 调度循环）。携带 `gen`（spawn 代数），
/// 调度循环忽略过期代消息（防旧 child 的 Exited/Ready 污染新 child）。
#[derive(Debug)]
pub(super) enum ChildOutcome {
    Ready {
        worker: WorkerId,
        gen: u64,
    },
    Pong {
        worker: WorkerId,
        gen: u64,
    },
    Result {
        worker: WorkerId,
        gen: u64,
        task_id: String,
        result: Result<String, String>,
    },
    Exited {
        worker: WorkerId,
        gen: u64,
    },
    BadLine {
        worker: WorkerId,
        gen: u64,
    },
}

/// 子进程 → 父进程的结构化消息（JSON 行，tag="type"）。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ChildMsg {
    Ready,
    Pong,
    Result {
        task_id: String,
        ok: bool,
        #[serde(default)]
        output: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
}

/// 解析子进程一行输出；非 JSON / 未知类型 → Err（协议强制结构化，禁止自由文本串线）。
pub(super) fn parse_child_line(line: &str) -> Result<ChildMsg, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("空行".to_string());
    }
    serde_json::from_str::<ChildMsg>(trimmed).map_err(|e| format!("非结构化消息：{e}"))
}

pub(super) fn outcome_from_msg(worker: &WorkerId, gen: u64, msg: ChildMsg) -> ChildOutcome {
    match msg {
        ChildMsg::Ready => ChildOutcome::Ready {
            worker: worker.clone(),
            gen,
        },
        ChildMsg::Pong => ChildOutcome::Pong {
            worker: worker.clone(),
            gen,
        },
        ChildMsg::Result {
            task_id,
            ok,
            output,
            error,
        } => {
            let result = if ok {
                Ok(output.unwrap_or_default())
            } else {
                Err(error.unwrap_or_else(|| "未知错误".to_string()))
            };
            ChildOutcome::Result {
                worker: worker.clone(),
                gen,
                task_id,
                result,
            }
        }
    }
}

// ---------- 结构化父→子消息（JSON 行） ----------

/// 结构化消息行：JSON + 行尾换行（子进程 `read_line` 以 `\n` 为消息边界；
/// 无换行会导致子进程阻塞等待，父进程写入永远不被消费）。
pub(super) fn task_line(task_id: &str, correlation_id: &str, input: &serde_json::Value) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "cmd": "task",
            "task_id": task_id,
            "correlation_id": correlation_id,
            "input": input,
        })
    )
}

pub(super) fn ping_line() -> String {
    format!("{}\n", serde_json::json!({ "cmd": "ping" }))
}

pub(super) fn cancel_line(task_id: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({ "cmd": "cancel", "task_id": task_id })
    )
}

pub(super) fn shutdown_line() -> String {
    format!("{}\n", serde_json::json!({ "cmd": "shutdown" }))
}
