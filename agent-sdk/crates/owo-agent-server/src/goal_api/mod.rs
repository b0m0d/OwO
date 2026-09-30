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

// R5：agent worker 作为本模块子模块编译（lib.rs 无需登记；独立编译、无 crate 引用）。
#[path = "../agent_worker.rs"]
pub mod agent_worker;

// ---------- 存储路径 ----------

mod handlers;
mod state;

pub use handlers::*;
