//! A1：WorkerPool 受控子进程宿主入口（主文档 §9.1、§7.1、§14.2）。
//!
//! 职责：把 [`owo_agent_core::worker_pool::child::run_child_protocol`]（JSONL 结构化
//! 协议：ready / task / ping / pong / cancel / result / error / shutdown）接到真实的
//! `owo-agent.exe` 二进制上，使 Goal 的 `worker_pool` 运行模式不再依赖测试自举。
//!
//! 边界（首期，受控最小集）：
//! - 仅提供 `echo`、受上限约束的 `sleep`、显式 `fail` 三个处理器；**不开放任意 shell**；
//! - 子进程零环境继承：启动侧 `env_clear()` + 空白名单；即使父进程误传凭据类变量，
//!   也会被 [`partition_credential_keys`] 显式拒绝并记录；
//! - `cancel` 是协作信号（协议层仅确认），阻塞任务最终由父进程超时 kill/abort 强制回收；
//! - stdout 只承载协议 JSONL，任何人类可读输出一律走 stderr。

use owo_agent_core::worker_pool::child::run_child_protocol;
use serde_json::Value;
use std::time::Duration;

/// 内部子进程入口 flag（clap 隐藏参数）。
pub const CHILD_FLAG: &str = "--owo-worker-child";

/// 处理器选择 flag（与 [`CHILD_FLAG`] 搭配使用）。
pub const HANDLER_FLAG: &str = "--handler";

/// echo/sleep 任务输入上限（拒绝超大负载；echo 同样受限以固定子进程资源面）。
pub const MAX_TEXT_BYTES: usize = 64 * 1024;

/// sleep 单次上限秒数（受上限约束的等待，不做任意时长挂起）。
pub const MAX_SLEEP_SECS: u64 = 30;

/// 未知 handler 时建议的合法取值文案。
pub const HANDLERS_HELP: &str = "echo | sleep | fail";

// ---------------------------------------------------------------------------
// 处理器
// ---------------------------------------------------------------------------

/// 首期受控任务处理器集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildHandler {
    /// 回显文本（结构化回环，用于结果协议验证）。
    Echo,
    /// 有界睡眠（用于预算/超时/取消语义验证）。
    Sleep,
    /// 显式失败（用于错误回传协议验证）。
    Fail,
}

impl ChildHandler {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Echo => "echo",
            Self::Sleep => "sleep",
            Self::Fail => "fail",
        }
    }

    /// 解析 `--handler` 取值；未知值给出明确错误（列出合法选项）。
    pub fn parse_name(name: &str) -> Result<Self, String> {
        match name.trim().to_ascii_lowercase().as_str() {
            "echo" => Ok(Self::Echo),
            "sleep" => Ok(Self::Sleep),
            "fail" => Ok(Self::Fail),
            other => Err(format!(
                "未知 worker 子进程 handler：{other:?}（可选：{HANDLERS_HELP}）"
            )),
        }
    }
}

/// 按 handler 分派一次结构化任务；`Err` 将作为协议 error 回传给父进程。
fn dispatch(handler: ChildHandler, input: &Value) -> Result<String, String> {
    let text_of = |key: &str| -> Option<&str> { input.get(key).and_then(Value::as_str) };
    let oversized = |len: usize| format!("任务负载过大：{len} 字节（上限 {MAX_TEXT_BYTES}）");
    match handler {
        ChildHandler::Echo => {
            let text = text_of("text").ok_or("echo 任务缺少 text 参数")?;
            if text.len() > MAX_TEXT_BYTES {
                return Err(oversized(text.len()));
            }
            Ok(text.to_string())
        }
        ChildHandler::Sleep => {
            let secs = input
                .get("secs")
                .and_then(Value::as_u64)
                .ok_or("sleep 任务缺少 secs 参数（0~30 秒）")?;
            if secs > MAX_SLEEP_SECS {
                return Err(format!(
                    "sleep 秒数超限：{secs}（上限 {MAX_SLEEP_SECS} 秒；更长等待应由调用方拆分或由父进程调度）"
                ));
            }
            std::thread::sleep(Duration::from_secs(secs));
            Ok(format!("slept {secs}s"))
        }
        ChildHandler::Fail => {
            let message = text_of("message")
                .unwrap_or("fail 处理器被显式调用（未提供 message）")
                .to_string();
            Err(message)
        }
    }
}

/// 子进程协议主循环：先发 `ready` 握手，然后循环处理 task/ping/cancel/shutdown。
/// 正常退出路径为 shutdown 或 stdin EOF（内部 `process::exit(0)`），因此本函数不返回。
pub fn run_child(handler: ChildHandler) -> ! {
    run_child_protocol(move |input| dispatch(handler, input))
}

// ---------------------------------------------------------------------------
// 宿主侧环境防线
// ---------------------------------------------------------------------------

/// 凭据类环境键模式（与 goal_api 校验同源）：KEY/TOKEN/SECRET/PASSWORD/PASSWD/CREDENTIAL。
///
/// 说明：子进程宿主不依赖 server crate 的校验函数（避免 lib/server 在 child 启动路径
/// 上产生额外链接面）；模式保持一致并由测试锁定。
fn is_credential_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|needle| upper.contains(needle))
}

/// 把候选环境键值对切分为「允许注入」与「明确拒绝」：
/// 允许注入的部分再经去重；凭据类键绝不进入白名单并逐条列入拒绝清单。
#[allow(dead_code)] // 宿主侧防线由契约测试锁定；正式白名单路径（A2）接手前显式保留
pub fn partition_credential_keys(
    candidates: impl IntoIterator<Item = (String, String)>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut allowed = Vec::new();
    let mut rejected = Vec::new();
    for (key, value) in candidates {
        if is_credential_key(&key) {
            rejected.push(key);
        } else if !allowed.iter().any(|(k, _)| k == &key) {
            allowed.push((key, value));
        }
    }
    (allowed, rejected)
}

/// 构造受控子进程命令：当前可执行文件自身 + 协议 flags + **零环境继承**。
///
/// 宿主进程不需要读取父进程的任何环境（凭据不可经环境泄露给协议 worker）；
/// 后续如需白名单变量，必须在 spawn 前经过 [`partition_credential_keys`]。
/// 说明：宿主演示路径经 WorkerPool 组装命令；本函数为契约测试与 A2 执行目标
/// 适配层保留（语义锁定：零继承 + 固定 flags）。
#[allow(dead_code)]
pub fn build_child_command(exe: &std::path::Path, handler: ChildHandler) -> std::process::Command {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(CHILD_FLAG).arg(HANDLER_FLAG).arg(handler.as_str());
    cmd.env_clear();
    cmd
}
