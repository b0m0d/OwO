//! OwO Agent 微内核（M0 kernel）：agent-sdk 微内核化后的**依赖根**。
//!
//! ## 为什么存在
//!
//! 指南《Agent-SDK-后续任务实施指南》§1.1 指出：`owo-agent-core` 的 91 个模块、
//! 约 6.5 万行全部处在同一个编译闭包内，“Agent、权限、工具、评测、多 Agent、OCR、
//! STT、Windows API、SQLite 全部处于同一编译闭包”。结果是任何一处小改动都要重编
//! 整个 core。本 crate 是拆分的第一步：把**真正共享、且不绑定业务状态机**的稳定
//! 原语下沉为独立 crate，形成 `kernel ← core ← server ← cli` 的单向依赖链。
//!
//! ## 边界（本 crate 只放这些）
//!
//! | 模块 | 归属理由 |
//! |---|---|
//! | [`error`] | 共享错误类型，被会话、审计、存储、平台层共同引用 |
//! | [`platform`] | 前台应用/窗口/截图/剪贴板等 OS 查询，是各运行边界的公共底座 |
//! | [`capability`] | 能力卡与路由判定，属于“能力协商”原语而非 Agent 业务 |
//! | [`audit`]、[`credentials`]、[`storage_crypto`]、[`cas_store`]、[`whitelist`] | 受信执行内核（Tool Host）与 Daemon 都必须共用的持久化/加密/审计原语 |
//! | [`injection`] | 提示注入净化，是跨边界的安全工具函数 |
//! | [`tool_args`] | 工具参数取用助手（原 core 内 `pub(crate)` 死代码的唯一真实使用者是开发工具包） |
//! | [`lease`]、[`deadline`] | 租约与阶段预算，供多 Agent / 可观测性共用 |
//!
//! ## 边界（本 crate 明确不放）
//!
//! * Agent loop、Provider 网关、工具注册表、会话状态机 → `owo-agent-core`（后续按
//!   指南 §9 A2/A4/A5 继续外迁）；
//! * OCR / STT / ONNX / Sherpa / 桌面自动化 → 未来的 Perception Worker（§9 A3）；
//! * 任何 HTTP 路由与 `AppState` → `owo-agent-server`。
//!
//! 本 crate **不依赖** `owo-agent-core` / `owo-agent-server`，也不依赖 ONNX、Sherpa，
//! 因此普通 Agent 功能迭代不会触发本 crate 重编（指南 §9 SLO：普通 Agent 改动触发
//! Rust 编译次数 = 0）。
//!
//! ## 公共 API 兼容性
//!
//! `owo-agent-core` 通过 `pub use owo_agent_kernel::*;` 把本 crate 的全部顶层条目
//! 重新导出，并保留 `pub mod error/platform/capability/...` 别名模块，因此
//! `owo_agent_core::audit::AuditLog`、`crate::platform::capture_screen()`、
//! `owo_agent_core::AgentError` 等既有路径全部继续有效——server、CLI、Tauri 壳与
//! 36 个 core 集成测试无需任何改动（微内核拆分“每拆一步仍能完整运行”的落地手段）。

pub mod audit;
pub mod capability;
pub mod cas_store;
pub mod credentials;
pub mod deadline;
pub mod error;
pub mod injection;
pub mod lease;
pub mod platform;
pub mod storage_crypto;
pub mod tool_args;
pub mod whitelist;

// 顶层再导出：与拆分前 `owo-agent-core` 的 `pub use` 面 1:1 对齐。
// `deadline` / `storage_crypto` 在原 core 中只有 `pub mod`、没有顶层 `pub use`，
// 这里同样只由上面的 `pub mod` 暴露，避免凭空扩大公共 API。
pub use audit::{AuditEntry, AuditLog};
pub use capability::{
    evaluate_capability_match, Arch, CapabilityCard, CapabilityMatch, CapabilityWorkerRegistry,
    EgressMode, Os, RegistrySnapshot, Resources, RouteDecision, RouteStats, TrustLevel,
    WorkerHealth, WorkerRequirement,
};
pub use cas_store::{CasRefsSnapshot, CasStore};
pub use credentials::{
    scan_json_for_secrets, windows_credential_manager, ApiKeyRef, CredentialError,
    CredentialResolver, CredentialStore, MemoryCredentialStore, ProviderConfig, UnavailableStore,
};
pub use error::AgentError;
pub use injection::{sanitize_tool_result, InjectionGuard, InjectionHit, InjectionSeverity};
pub use lease::{Lease, LeaseConfig, LeaseError, LeaseManager};
pub use platform::{capture_screen, clipboard_sequence, poll_foreground_app};
pub use tool_args::required_string;
pub use whitelist::{AppTier, Whitelist, WhitelistEntry};
