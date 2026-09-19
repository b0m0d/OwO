//! MCP 宿主内核（M10）。
//!
//! 本 crate 是指南 §4 目标结构里 `tool-host/crates/mcp-host` 的第一段，承载
//! **MCP 接入层**这一整条边界：
//!
//! | 目标 | 内容 |
//! |---|---|
//! | [`mcp`] | MCP 客户端与注册表：stdio / HTTP 传输、工具列表与调用、schema 预算、超时与重连；`McpServerConfig` 经 [`owo_agent_plugins`] 复用 |
//! | `owo-mcp-test-server`（bin） | stdio 假服务器（echo/add 等），测试与示例插件共用 |
//! | `owo-mcp-http-test-server`（bin） | HTTP 假服务器（SSE / POST），覆盖 HTTP 传输路径 |
//! | `tests/mcp_tests.rs` | 13 条 MCP 集成测试（登录、列工具、调用、超时、热注册、官方插件） |
//!
//! 边界（见 `docs/ARCH-MICROKERNEL.md` §12）：
//!
//! * `mcp.rs` 的 `crate::` 出边实测为 **0**（自 M3/M7 起它就走
//!   `owo_agent_tool_safety::` 与 `owo_agent_plugins::` 绝对路径），搬迁不需要任何倒置；
//! * 入边由 core 的同名别名模块 + 顶层 `pub use` 满足，core / server / 集成测试零改动；
//! * **本 crate 的普通依赖里不得出现 `owo-agent-core`**——core 依赖本 crate，
//!   反向的普通依赖会立刻成环。集成测试需要 core 时只能走 `dev-dependencies`。
//!
//! 为什么把两台假服务器和 MCP 集成测试一起搬：`env!("CARGO_BIN_EXE_<name>")` 只在
//! **声明该 bin 的包**的集成测试里可用；而这两台假服务器只服务于 MCP 这一条边界。
//! 三者同迁之后，"改一次 MCP 传输"只重编本 crate，不再牵动核心编译单元。

pub mod mcp;

// 迁移期约定（与 M0–M9 一致）：用 glob 再导出，让公共面等价性由编译器证明。
pub use mcp::*;
