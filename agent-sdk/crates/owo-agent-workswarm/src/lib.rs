//! WorkSwarm 编排契约与状态内核（M9）。
//!
//! 本 crate 承载多 Agent 编排里**只描述数据与状态、不驱动执行**的那一层：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`project_space_store`] | 项目空间持久化：`ProjectSpace` / `Artifact` / `DecisionRecord` / `HandoffRecord` / `TeamRun` 的 SQLite CRUD（trait + 实现） |
//! | [`team_benefit`] | 组队收益判定：配对对照报告 → 冻结门槛 → `TeamPolicy` / `PolicyGate`（纯函数 + serde） |
//! | [`workswarm_output`] | Worker 结构化输出契约（V1）：`WorkerOutputV1` 解析与校验 |
//!
//! 边界（见 `docs/ARCH-MICROKERNEL.md` §11）：
//!
//! * 三个模块的 `crate::` 出边实测为 **0**，因此搬迁不需要任何依赖倒置；
//! * 入边（core 的 `workswarm` / `team_strategy` / `artifact_pipeline` /
//!   `contract_worker` / `worker_profile`，以及 server、集成测试、
//!   `devtools/product-eval`）全部由 core 的同名别名模块 + 顶层 `pub use` 满足，
//!   调用方零改动；
//! * **本 crate 不得依赖 `owo-agent-core`**——只依赖 `owo-agent-protocol`。
//!   否则 core → workswarm 的既有边会变成 crate 环。
//!
//! 为什么先切这一层：指南 §3 把 `workswarm.rs` / `goal.rs` / `workflow.rs` / `team_*`
//! 定档为 `extensions/workswarm/`（默认关闭、可选加载）。在编排代码与 core 同处一个
//! 编译单元时，"可选加载"物理上无法实现（M1 §3.2 实测：feature 关不住一条会成环的边）。
//! 因此先把零出边的契约/状态部分切成独立 crate，作为后续把执行侧（`workswarm.rs` 本体，
//! 4,328 行）变成可选扩展的前置条件。

pub mod project_space_store;
pub mod team_benefit;
pub mod workswarm_output;

// 迁移期约定（与 M0–M8 一致）：用 glob 再导出，保证公共面等价性由编译器证明，
// 而不是靠手抄符号表（M6 实测手写符号表必然漏符号并报 E0432）。
pub use project_space_store::*;
pub use team_benefit::*;
pub use workswarm_output::*;
