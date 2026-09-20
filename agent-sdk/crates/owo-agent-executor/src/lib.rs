//! UI 执行内核（M15）——指南 §4 `tool-host/crates/executor` 的代码边界。
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`executor`] | 动作落地：UIA 元素定位（`parse_click_at` / `locate_anchor_point`）、键鼠注入、窗口激活、屏幕捕获取词；结果收敛为 `ExecReport` / `ExecStep` |
//!
//! 边界（见 `docs/ARCH-MICROKERNEL.md` §19）：
//!
//! * `crate::` 出边**全部指向已抽出的 crate**（memory / perception / kernel），
//!   搬迁只改绝对路径（13 处），**零倒置**；
//! * **普通依赖里不得出现 `owo-agent-core`**；
//! * 它**不做权限判定**——权限由 `owo-agent-policy` 判定，本 crate 只执行已裁决的动作；
//!   这条分工是指南 §2.2「受信 Tool Host：权限不可绕过」在 crate 层面的落点。
//!
//! 为什么单独成 crate 而不是并进 workflow：指南 §4 把 `executor` 划给 `tool-host`，
//! 而 workflow 属 `extensions/`。方向是 `workflow → executor`（`action_program` 用它的
//! `UiActionSource`/`ExecReport`），拆开后 Tool Host 侧将来可以只依赖本 crate。

pub mod executor;

// 迁移期约定：glob 再导出，让公共面等价性由编译器证明。
pub use executor::*;
