// R10:worker_pool 完成（租约/fencing 挂接），待主控接线
//! worker 子进程池（多 Agent P1）：把 worker 从进程内扩展到独立子进程。
//!
//! 设计来源：《多Agent并行体系-生产级设计与跨机扩展-2026-08-16.md》§2 任务模型 与 §4 可靠性：
//! - **隔离**：每个 worker 独立子进程；`IsolationMode::Sandbox` 为 OS 级沙箱接入点（Agent 3 沙箱实现）。
//! - **崩溃自愈**：心跳检测 + 指数退避重启（复用 `fleet::backoff_secs`，封顶 60s）+ 连续失败熔断
//!   （复用 `fleet::Supervisor`；健康任务完成后复位计数）。
//! - **预算**：轮次/时长在池侧强制（策略字段）；内存/CPU 上限本轮仅表达，OS 强制由沙箱实现。
//! - **协议**：受限 stdin/stdout + JSON 行结构化消息，禁止自由文本串线；stderr 供人读诊断。
//! - **清理**：`kill`/`shutdown`/`Drop` 均终止子进程；`Drop` 是安全网（同步 start_kill）。
//! - **事件**：崩溃/重启/熔断/预算中止/取消经 `fleet::WorkerEvent` 进入总线与审计。
//!
//! 生命周期：`spawn`（ready 握手）→ `submit`（结构化任务）→ `check_health`（心跳自愈）→
//! `cancel_pending`/`cancel_all`（取消传播）→ `kill`/`shutdown`（终止与清理）。

mod pool;
mod protocol;

#[cfg(test)]
mod tests;

pub use pool::*;
pub use protocol::*;
