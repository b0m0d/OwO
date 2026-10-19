//! 记忆 / 观察 / 学习内核（M11）。
//!
//! 本 crate 承载"感知输入 → 记忆 → 学习/主动建议"这条链上的三个模块：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`memory`] | 语义记忆存储：`MemoryEntry` / `SemanticMemory`，JSONL 持久化 + 剪枝 |
//! | [`observe`] | 桌面观察：`DesktopSnapshot` / `Observation` / `MemoryStore`，把前台应用与剪贴板变化记成可检索的观察 |
//! | [`learn`] | 操作学习与主动建议：录制/泛化/动作图/流程技能包/`ProactiveEngine` |
//! | [`proactive_settings`] | 主动建议的域配置 `ProactiveSettings`（M11 从 core 的 settings.rs 随域搬入） |
//! | [`share_skill`] | 技能包分享与导入：`.owskill` 包的导出/校验/导入（M13 从 core 归位：它唯一的出边就是 `learn`，而 `learn` 已随本 crate 下沉） |
//!
//! 边界（见 `docs/ARCH-MICROKERNEL.md` §13）：
//!
//! * 三个模块之间的边（`memory ↔ observe`、`observe → learn`）**同迁一个 crate**，
//!   不跨 crate 边界，故无需任何倒置；
//! * 跨 crate 的边在前面步骤里已经倒置完毕，本步只把路径改成绝对形式：
//!   `crate::platform::` → [`owo_agent_kernel::platform`]（M0 下沉），
//!   `crate::skill_health::` → [`owo_agent_contracts::skill_health`]（M8 下沉）；
//! * 残留的 `crate::settings::ProactiveSettings` 按 M7 规则（配置类型随域走）
//!   随类型一起搬进 [`proactive_settings`]，core 侧 `pub use` 转出，
//!   `owo_agent_core::settings::ProactiveSettings` 与 `Settings.proactive` 均不变；
//! * **普通依赖里不得出现 `owo-agent-core`**：core 依赖本 crate
//!   （`action_program` / `computer_use` / `executor` / `share_skill` / `workflow`
//!   都引用 `learn`），反向的普通依赖会立刻成环。

pub mod learn;
pub mod memory;
pub mod observe;
pub mod proactive_settings;
pub mod share_skill;

// 迁移期约定（与 M0–M10 一致）：用 glob 再导出，让公共面等价性由编译器证明。
pub use learn::*;
pub use memory::*;
pub use observe::*;
pub use proactive_settings::*;
pub use share_skill::*;
