//! 工作流 / 动作程序内核（M15）。
//!
//! | 模块 | 行数 | 内容 |
//! |---|---:|---|
//! | [`workflow`] | 1,461 | 工作流 DSL：定义/校验/编译、审批、检查点与回滚、触发器、子流程 |
//! | [`action_program`] | 902 | 动作程序运行时：控制流（分支/循环/重试/等待）、变量、断言串联 |
//! | [`assert`] | 525 | 断言：UIA 存在性、OCR 文本/框、窗口标题、状态差异 |
//!
//! 边界（见 `docs/ARCH-MICROKERNEL.md` §19）：
//!
//! * 三个模块互相引用（`workflow → action_program/assert`、`action_program → assert`）
//!   且**同迁一个 crate**，因此这些边不再跨 crate 边界，**零倒置**；
//! * 对外出边全部指向已抽出的 crate：`owo-agent-executor`（动作落地）、
//!   `owo-agent-memory`（`learn` 的动作类型/语义锚点）、
//!   `owo-agent-perception`（OCR/情境快照/场景图）、`owo-agent-kernel`（审计）、
//!   `owo-agent-contracts`（技能健康）；
//! * **普通依赖里不得出现 `owo-agent-core`**。
//!
//! 与 executor 的分工：本 crate 决定"做什么、按什么顺序、失败怎么办"，
//! executor 负责"把单个动作真的落到操作系统上"；权限判定在 `owo-agent-policy`。

pub mod action_program;
pub mod assert;
pub mod workflow;

// 迁移期约定：glob 再导出，让公共面等价性由编译器证明。
pub use action_program::*;
pub use assert::*;
pub use workflow::*;
