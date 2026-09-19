//! 主动建议（proactive）域配置：`ProactiveSettings`。
//!
//! 这个类型原本定义在 `owo-agent-core::settings`，与 `Settings` 聚合放在一起。
//! M11 按 §9.2（M7）定下的规则——**配置类型随域走**——把它移到真正消费它的域：
//! 主动建议引擎 `learn::ProactiveEngine` 的构造与 `apply_settings` 直接吃这个类型，
//! 而 `Settings` 只是把它作为一个字段聚合进来。
//!
//! 迁移后两条既有路径都继续有效（调用方零改动）：
//!
//! * `owo_agent_core::settings::ProactiveSettings`（core 的 settings.rs 用 `pub use` 转出）；
//! * `Settings { proactive: ProactiveSettings, .. }` 的字段类型不变。
//!
//! serde 的 `#[serde(default = "path")]` 里 **path 在定义处解析**，所以这一组默认值
//! 函数必须随类型一起搬（否则新 crate 里 `default = "default_true"` 找不到符号）。
//! core 的 settings.rs 里保留它自己的 `default_true`（仍被另外两个结构体使用）；
//! 本模块另有一份等价的私有实现，注释见下——这是有意的小重复，换来的是两侧
//! 结构体定义各自内聚，不必为一行布尔默认值互相反向依赖。

use serde::{Deserialize, Serialize};

/// 主动建议阈值配置（v0.4 D24，默认仅提示不执行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProactiveSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_weekly_threshold")]
    pub weekly_threshold: u32,
    #[serde(default = "default_daily_threshold")]
    pub daily_threshold: u32,
    #[serde(default = "default_similarity")]
    pub similarity: f64,
    #[serde(default = "default_cooldown_hours")]
    pub cooldown_hours: u32,
    #[serde(default = "default_daily_cap")]
    pub daily_cap: u32,
    #[serde(default = "default_auto_silence_days")]
    pub auto_silence_days: u32,
}

impl Default for ProactiveSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            weekly_threshold: 5,
            daily_threshold: 3,
            similarity: 0.9,
            cooldown_hours: 24,
            daily_cap: 3,
            auto_silence_days: 30,
        }
    }
}

// `#[serde(default = "...")]` 的路径在**本模块**解析，因此这里必须自备这几条
// 默认值（与 core::settings 的同名函数语义一致，数值来自 v0.4 D24 的冻结默认值）。
fn default_true() -> bool {
    true
}

fn default_weekly_threshold() -> u32 {
    5
}

fn default_daily_threshold() -> u32 {
    3
}

fn default_similarity() -> f64 {
    0.9
}

fn default_cooldown_hours() -> u32 {
    24
}

fn default_daily_cap() -> u32 {
    3
}

fn default_auto_silence_days() -> u32 {
    30
}
