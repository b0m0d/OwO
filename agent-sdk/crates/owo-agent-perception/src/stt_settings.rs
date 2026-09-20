//! 语音输入的域配置：`SttSettings`。
//!
//! 该类型原本定义在 `owo-agent-core::settings`，与 `Settings` 聚合放在一起。
//! M14 按 §9.2（M7）定下的规则——**配置类型随域走**——把它移到真正消费它的域：
//! `stt::LocalStt` 直接吃它的每一项（model / language / itn / hotwords / latency_budget），
//! 而 core 的 `Settings` 只是把它作为一个字段聚合进来。
//!
//! 迁移后两条既有路径都继续有效（调用方零改动）：
//!
//! * `owo_agent_core::settings::SttSettings`（core 的 settings.rs 用 `pub use` 转出）；
//! * `Settings { stt: SttSettings, .. }` 的字段类型不变。
//!
//! serde 的 `#[serde(default = "path")]` 里 **path 在定义处解析**，所以这一组默认值
//! 函数必须随类型一起搬（否则本模块里 `default = "default_true"` 找不到符号）。
//! `default_true` / `default_false` / `default_latency_budget` 在 core 的 settings.rs 里
//! 还被别的结构体使用，因此本模块自备等价实现——与 M11 的 `proactive_settings.rs`
//! 同一处置（有意的小重复，换取两侧结构体定义各自内聚）。

use serde::{Deserialize, Serialize};

/// 语音输入配置（v0.4 D20，默认 SenseVoice-Small 本地转写）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SttSettings {
    #[serde(default = "default_stt_model")]
    pub model: String,
    /// SenseVoice 语言（auto / zh / en / ja / ko / yue），可用 OWO_STT_LANGUAGE 覆盖。
    #[serde(default = "default_stt_language")]
    pub language: String,
    /// 是否启用逆文本规范化（ITN），可用 OWO_STT_ITN 覆盖。
    #[serde(default = "default_true")]
    pub itn: bool,
    #[serde(default = "default_false")]
    pub enable_high_accuracy: bool,
    #[serde(default)]
    pub hotwords: Vec<String>,
    #[serde(default = "default_latency_budget")]
    pub latency_budget_ms: u64,
}

impl Default for SttSettings {
    fn default() -> Self {
        Self {
            model: "SenseVoice-Small".to_string(),
            language: "auto".to_string(),
            itn: true,
            enable_high_accuracy: false,
            hotwords: Vec::new(),
            latency_budget_ms: 2000,
        }
    }
}

// `#[serde(default = "...")]` 的路径在**本模块**解析，因此这里必须自备这几条默认值
// （数值与 core::settings 的同名函数一致，来自 v0.4 D20 的冻结默认值）。
fn default_stt_model() -> String {
    "SenseVoice-Small".to_string()
}

fn default_stt_language() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_latency_budget() -> u64 {
    2000
}
