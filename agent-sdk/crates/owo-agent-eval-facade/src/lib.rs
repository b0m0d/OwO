//! ProductEval 兼容门面。
//!
//! 只做一件事：把 [`owo_agent_product_eval`]（`devtools/product-eval/`，自身是独立
//! workspace 的开发工具 crate）的公共面原样转出，让 server / cli 用稳定路径拿评测面，
//! 同时**不与 `owo-agent-core` 形成 crate 环**。
//!
//! ## 为什么必须存在这一层
//!
//! ```text
//! 期望（错）：core ──► product-eval ──► core      ← Cargo 报 cyclic package dependency
//! 实际（对）：server/cli ──► 本门面 ──► product-eval ──► core
//! ```
//!
//! `devtools/product-eval` 必须依赖 `owo-agent-core`（要真实跑 Agent/Turn/工具/权限）。
//! 若让 core 反向依赖它，两者无论在同一 workspace 还是分开（`exclude`），都会因为 Cargo
// 把 path 依赖解析成同一个 package 实例而报环；workspace 级 `default-features = false`
// 也不解决（feature 是并集，server/cli 一打开就又被点亮）。三条路都实测过。
//!
//! 因此 core 对开发工具的依赖是**零**——这也是正确方向：受信运行时不该依赖开发工具。
//!
//! ## 生命周期
//!
//! 临时构件，但有明确退出条件：指南 §9 计划把 ProductEval 搬到 `devtools/eval/`（Python）。
//! 那时本门面删除，server / cli 改为调用 Python 工具；core 侧不承受任何回退成本。

pub use owo_agent_product_eval::*;
