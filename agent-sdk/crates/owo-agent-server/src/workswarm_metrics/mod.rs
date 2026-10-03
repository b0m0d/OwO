//! WorkSwarm TeamRun 指标、预算与脱敏（五期 · 第三路）。
//!
//! 组成：
//! - [`MeasuredRoleWorker`]：角色 worker 包装层（在 [`crate::workswarm_api`] 的
//!   `build_run_registry` 包装 `RoleWorker`）。记录每次执行的开始/结束时间、墙钟、
//!   终态与失败原因、尝试序数、输出 Artifact；`model_calls` 经
//!   [`MeasuredProvider`] 按步骤隔离计数，Provider 可用时逐请求记录 request_id/model/usage。
//! - [`MetricsJournal`]：指标以 JSONL 追加落盘到 TeamRun 数据目录
//!   （`<run_dir>/<team_id>-metrics.jsonl`，与 `<team_id>-meta.json` / 运行状态文件同目录），
//!   重启后仍可读取（端点每次从文件聚合，无进程内账本）。
//! - 聚合与预算：[`aggregate_metrics`] 产出团队/角色两级汇总；预算语义在
//!   `TeamRun.budget` JSON 上做 **additive** 扩展（可选 `max_cost_usd` / `max_wall_secs`，
//!   此前未知键被忽略），超限 → 运行循环停止调度下一阶段（见 [`crate::workswarm_api`]）。
//! - 脱敏：[`sanitize_text`] / [`sanitize_value`] 统一脱敏诊断导出
//!   （凭据类键值、bearer/sk- 等令牌、超长文本截断）。
//!
//! 归因口径：每个 Worker span 隔离收集请求元数据；响应包含用量时按 request_id/model
//! 汇总到对应步骤。Provider 未返回逐请求用量时，仅在无重叠时使用共享快照差值；并发
//! 重叠且缺少逐请求用量时 token/费用留空并标记 unknown_concurrent_overlap。

// ---------------------------------------------------------------------------
// 指标记录（JSONL 行结构）
// ---------------------------------------------------------------------------

mod metrics;
mod sanitize;
mod util;

#[cfg(test)]
mod tests;

pub(crate) use metrics::*;
pub(crate) use sanitize::*;
pub(crate) use util::*;
