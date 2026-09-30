// R12:fleet 完成，待主控接线
//! 多 Agent 并行编排内核（P0）：本地 Agent 总线、监督树与并行原语。
//!
//! 设计来源：《多Agent并行体系-生产级设计与跨机扩展-2026-08-16.md》。
//! 范围：L0 单机进程内。消息语义对齐 A2A 任务/消息子集；可靠性机制对齐 OTP 监督树；
//! 并行分解对齐多 GPU 数据并行（fan-out + 聚合）。约束：并行拓扑必须有唯一调度主；
//! 共享状态写单主或 CRDT；任何消息必须带 `correlation_id` 贯通父子。

mod bus;
mod fanout;
mod supervision;
#[cfg(test)]
mod tests;
mod wait;

pub use bus::*;
pub use fanout::*;
pub use supervision::*;
pub use wait::*;
