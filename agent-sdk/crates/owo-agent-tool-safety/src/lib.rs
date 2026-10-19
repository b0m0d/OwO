//! OwO Agent 受信执行内核（M3）。
//!
//! ## 这个 crate 是什么
//!
//! 指南 §2.2 规定「Policy 与 Executor 必须在同一受信边界，不能让主 Agent 自批自执行」，
//! §2.4 第 3 条要求「权限判定、capability、执行和审计收据」同属一个拥有者，§13 的完成
//! 定义要求「Tool Host 权限不可绕过」。本 crate 是这条边界在当前阶段的落地：
//!
//! | 模块 | 职责 | 指南位置 |
//! |---|---|---|
//! | [`sandbox`] | OS 级隔离与执行：Windows Job Object / 低完整性令牌 / AppContainer、CPU/内存/进程数上限、kill-on-close 防孤儿、egress 与插件拒绝记账 | §2.2 受信执行内核 |
//! | [`audit_chain`] | append-only + 分段 HMAC-SHA256 防篡改审计链，锚点可离线校验；沙箱事件汇入审计链 | §2.4 第 3 条（审计收据） |
//!
//! ## 为什么这两个模块放一起
//!
//! `docs/ARCH-MICROKERNEL.md` §5.1 用 Tarjan 强连通分量分析 core 的 76 模块依赖图，
//! 结论：`audit_chain + sandbox`（2,694 行）是**唯一一个零出边的成规模分量**——它们
//! 不引用 core 的其它任何模块，入边只有 `mcp` 与 `plugin`。因此整体搬迁结构性安全，
//! core 只需保留同名别名 re-export，调用方零改动。
//!
//! 两者在 core 内**互相引用**（`audit_chain` 把 `SandboxAuditLog` 写成审计记录；
//! `sandbox::SandboxManager::drain_into_chain` 调用 `AuditChain::append_sandbox_log`）。
//! 在 core 里这是合法的模块互引；由于两个模块一起搬进本 crate，这条边不再跨越任何
//! crate 边界，**无需接口倒置**（ADR-001 原计划据此已更新）。
//!
//! ## 依赖方向
//!
//! ```text
//! owo-agent-core ──► owo-agent-tool-safety ──► owo-agent-kernel
//! ```
//!
//! 本 crate **不依赖** `owo-agent-core` / `owo-agent-server`，也不依赖 ONNX / Sherpa。
//! `sandbox` 的 Windows 部分是**裸 FFI**（`extern "system"` + `#[link(name = "kernel32"
//! /"advapi32"/"ntdll")]`），因此本 crate 连 `windows` / `windows-sys` 都不需要。
//!
//! ## 不是 Tool Host
//!
//! 指南 §4 的 `services/tool-host` 是**独立 workspace 的进程**，含 policy / grant /
//! approval / executor / mcp-host。本 crate 目前只含「沙箱执行 + 审计收据」两块，
//! 故命名为 tool-safety；§9 A4 抽取 Tool Host 时，本 crate 应成为其 executor 与收据子集。

pub mod audit_chain;
pub mod sandbox;

// 顶层再导出：与拆分前 `owo-agent-core` 的公共面保持 1:1，调用方无需感知迁移。
pub use audit_chain::{
    canonical, export_to_file, hex_encode, hmac_sha256, load_export, verify_export, verify_file,
    Anchor, AuditChain, AuditChainError, AuditCliCommand, AuditCliOutcome, AuditExport,
    AuditRecord, ChainedRecord,
};
pub use sandbox::{
    app_container_network_capabilities, available_isolation, default_manager, evaluate_capability,
    inside_workspace, internet_client_sid, network_requires_app_container, os_struct_layouts_match,
    private_network_client_server_sid, probe_platform_support, validate_app_container_network,
    CapabilityEvaluation, ExecGuard, FileScope, IsolationLevel, JobGuard, MockSandboxExecutor,
    NetworkPolicy, PlatformSupport, SandboxAuditEvent, SandboxAuditLog, SandboxCommand,
    SandboxError, SandboxEventKind, SandboxExecutor, SandboxHandle, SandboxHealth, SandboxManager,
    SandboxPolicy, SandboxProcess, SandboxProcessInner, SandboxProcessStatus, SandboxWaitInfo,
    UnavailableExecutor,
};
