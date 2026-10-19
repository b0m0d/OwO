//! OwO Agent 环境内核（M5）。
//!
//! ## 内容
//!
//! [`desktop_env`]：DesktopEnv 协议 + S1 可编程环境 + 确定性重放 + 租约 fencing +
//! 故障注入 + VLM-free judge，以及把 `TaskSurface` 适配为 DesktopEnv 的兼容层
//! （[`desktop_env::SurfaceEnvAdapter`]）。
//!
//! ## M5 与前几步的差别：第一次真正的依赖倒置
//!
//! 迁移前 `desktop_env` 的唯一出边是 `crate::computer_use::TaskSurface`。按
//! `docs/ARCH-MICROKERNEL.md` §4 的判据：`desktop_env` 与 `computer_use` **不同迁**，
//! 所以这条边必须倒置 —— 否则外迁后 `desktop_env → computer_use` 与 `core → desktop_env`
//! 成环。
//!
//! 倒置方式：把 `TaskSurface` 这个**纯 I/O 契约**下沉到
//! [`owo_agent_kernel::task_surface`]。它只用到 `&str` / `i32` / `serde_json::Value`，
//! 不绑定任何业务类型，因此不会把"某个执行器的数据类型"带进内核（对比 ADR-001 里
//! 被否决的"把 SandboxAuditEvent 下沉内核"）。实现体仍留在 core 的 `computer_use`。
//!
//! 倒置后本 crate 对 `owo-agent-core` 的依赖为**零**（`crate::` 引用实测为空集）。
//!
//! ## 依赖方向
//!
//! ```text
//! owo-agent-core ──► owo-agent-env ──► owo-agent-kernel
//! ```
//!
//! 本 crate 不依赖 core / server / ONNX / Sherpa。

pub mod desktop_env;
pub mod experience_store;
pub mod transition;
pub mod world_model;

// 用 glob 而非手写符号表：首次编译实测手写列表会写错符号名（E0432），
// glob 让迁移后的公共面与拆分前**完全等价**，由编译器验证（与 owo-agent-extensions 同做法）。
pub use experience_store::*;
pub use transition::*;
pub use world_model::*;

// 顶层再导出：与拆分前 `owo-agent-core` 的公共面保持 1:1。
pub use desktop_env::{
    ActionKind, Assertion, DesktopEnv, EnvError, EnvLeaseRecord, EnvRegistry, FaultSpec,
    FieldChange, GroundedAction, LeaseProof, RewardParts, RiskLevel, SimAppKind, SimDesktopEnv,
    SimElement, StateDelta, StepResult, SuccessSpec, SurfaceEnvAdapter, TaskSeed, Verdict,
    WorldStateV1, SIM_ENV_PROTOCOL, SIM_ENV_VERSION,
};
