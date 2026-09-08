//! §4.8 模型提供商显式配置：禁止"依据某个其他环境变量存在"静默猜测提供商。
//!
//! - 数据目录持久化 `provider.json`：`{ "provider": "cloud"|"ollama"|"unset", base_url?, model? }`，
//!   首次启动由用户在 UI 明确选择云端 / 本地 Ollama / 稍后配置。
//! - `apply_core_env` 只按用户的显式选择注入环境；若键缺失则 core 可以 ready
//!   （诊断/设置可用），但 `ProviderStatus` 明确提示未配置，UI 给出选择入口。
//! - 历史迁移只提示、不自动生效：发现旧 `DASHSCOPE_API_KEY` 时写入诊断提示，
//!   绝不代用户改写 provider。
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 提供商选择（用户显式；缺省 Unset = 稍后配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderMode {
    Cloud,
    Ollama,
    Unset,
}

impl ProviderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderMode::Cloud => "cloud",
            ProviderMode::Ollama => "ollama",
            ProviderMode::Unset => "unset",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cloud" => Some(Self::Cloud),
            "ollama" => Some(Self::Ollama),
            "unset" => Some(Self::Unset),
            _ => None,
        }
    }
}

/// 提供商配置（持久化；密钥不落盘——只保存模式/端点/模型名）。
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub mode: ProviderMode,
    /// 云端端点（缺省用 core 的 DEFAULT_MODEL_BASE_URL）；Ollama 固定本地地址。
    pub base_url: Option<String>,
    /// 模型名（缺省 ollama 用 local，云端用 core 的 DEFAULT_MODEL_ID）。
    pub model: Option<String>,
}

impl ProviderConfig {
    pub fn unset() -> Self {
        Self {
            mode: ProviderMode::Unset,
            base_url: None,
            model: None,
        }
    }

    /// 云端模式：端点缺省 BigModel、模型缺省 glm-5.3-flash（与 core 默认一致）。
    pub fn cloud() -> Self {
        Self {
            mode: ProviderMode::Cloud,
            base_url: None,
            model: None,
        }
    }

    pub fn ollama() -> Self {
        Self {
            mode: ProviderMode::Ollama,
            base_url: None,
            model: None,
        }
    }

    /// 展示用有效端点：云端缺省 BigModel 端点、Ollama 固定本地 11434、Unset 为空。
    pub fn effective_base_url(&self) -> String {
        match self.mode {
            ProviderMode::Cloud => self
                .base_url
                .clone()
                .unwrap_or_else(|| "https://open.bigmodel.cn/api/paas/v4".to_string()),
            ProviderMode::Ollama => "http://127.0.0.1:11434/v1".to_string(),
            ProviderMode::Unset => String::new(),
        }
    }

    /// 展示用有效模型名。
    pub fn effective_model(&self) -> String {
        match self.mode {
            ProviderMode::Cloud => self
                .model
                .clone()
                .unwrap_or_else(|| "glm-5.3-flash".to_string()),
            ProviderMode::Ollama => self.model.clone().unwrap_or_else(|| "local".to_string()),
            ProviderMode::Unset => String::new(),
        }
    }
}

/// 数据目录：`%LOCALAPPDATA%\OwO\Agent\`（与日志/工作区同根）。
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
        .map(|base| base.join("OwO").join("Agent"))
}

fn provider_state_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("provider.json"))
}

/// 从数据目录读取用户显式提供商选择；读不到/损坏 → Unset（首次启动）。
pub fn load_provider_config() -> ProviderConfig {
    let Some(path) = provider_state_path() else {
        return ProviderConfig::unset();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return ProviderConfig::unset();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return ProviderConfig::unset();
    };
    let mode = value
        .get("provider")
        .and_then(|m| m.as_str())
        .and_then(ProviderMode::parse)
        .unwrap_or(ProviderMode::Unset);
    ProviderConfig {
        mode,
        base_url: value
            .get("base_url")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        model: value
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }
}

/// 持久化提供商选择（密钥不落盘；路径/端点先做基本形状校验）。
pub fn save_provider_config(config: &ProviderConfig) -> Result<(), String> {
    let path = provider_state_path().ok_or_else(|| "无法确定数据目录".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("创建数据目录失败：{error}"))?;
    }
    if let Some(base_url) = &config.base_url {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err("端点必须以 http:// 或 https:// 开头".to_string());
        }
    }
    if let Some(model) = &config.model {
        if model.trim().is_empty() || model.contains(char::is_whitespace) {
            return Err("模型名不能为空且不能含空白".to_string());
        }
    }
    let mut payload = json!({ "provider": config.mode.as_str() });
    if let Some(base_url) = &config.base_url {
        payload["base_url"] = json!(base_url);
    }
    if let Some(model) = &config.model {
        payload["model"] = json!(model);
    }
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| format!("序列化提供商配置失败：{error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("保存提供商配置失败：{error}"))
}

/// 注入 core 子进程的提供商环境（§4.8：只按用户显式选择注入，不静默猜测）。
///
/// - Cloud：注入 OPENAI_BASE_URL/OPENAI_MODEL（缺省 core 同源默认）；OPENAI_API_KEY
///   由壳进程环境继承，缺失仅在 `ProviderStatus` 提示，不代填任何 key。
/// - Ollama：固定本地端点 + `OPENAI_MODEL=local`；不注入任何 key。
/// - Unset：不注入任何提供商变量 → core 可 ready（诊断/设置可用），模型调用由 UI 引导后生效。
/// - 历史迁移：发现 DASHSCOPE_API_KEY 等旧凭据只写诊断提示，绝不自动改写。
pub fn apply_provider_env(
    command: &mut std::process::Command,
    config: &ProviderConfig,
    log_path: &Path,
) {
    match config.mode {
        ProviderMode::Cloud => {
            command
                .env("OPENAI_BASE_URL", config.effective_base_url())
                .env("OPENAI_MODEL", config.effective_model());
            if std::env::var_os("OPENAI_API_KEY").is_none()
                && std::env::var_os("DASHSCOPE_API_KEY").is_some()
            {
                append_log_line(
                    log_path,
                    "[provider] 发现旧 DASHSCOPE_API_KEY：云端模式需要显式配置 OPENAI_API_KEY，\
                     未自动迁移（§4.8 迁移只提示、不生效）",
                );
            }
        }
        ProviderMode::Ollama => {
            command
                .env("OPENAI_BASE_URL", config.effective_base_url())
                .env("OPENAI_MODEL", config.effective_model());
        }
        ProviderMode::Unset => {
            append_log_line(
                log_path,
                "[provider] 未选择模型提供商（Unset）：core 就绪但模型调用暂不可用，\
                 请在设置中选择云端或本地 Ollama",
            );
        }
    }
}

/// 提供商就绪状态（UI 展示；不含密钥）。
#[derive(Debug, Clone)]
pub struct ProviderStatus {
    pub mode: ProviderMode,
    pub base_url: String,
    pub model: String,
    /// 云端模式是否已具备 API key（只回存在性，不回值）。
    pub key_configured: bool,
    /// 可否立即发起模型调用（云端需 key；Ollama/Unset 依模式）。
    pub ready: bool,
}

pub fn provider_status(config: &ProviderConfig) -> ProviderStatus {
    let (key_configured, ready) = match config.mode {
        ProviderMode::Cloud => {
            let has_key = std::env::var_os("OPENAI_API_KEY").is_some();
            (has_key, has_key)
        }
        ProviderMode::Ollama => (false, true),
        ProviderMode::Unset => (false, false),
    };
    ProviderStatus {
        mode: config.mode,
        base_url: config.effective_base_url(),
        model: config.effective_model(),
        key_configured,
        ready,
    }
}

/// 追加一行日志（与 core_runtime::append_log_line 同语义；独立实现避免循环依赖）。
fn append_log_line(path: &Path, line: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_mode_parse_roundtrip() {
        assert_eq!(ProviderMode::parse("cloud"), Some(ProviderMode::Cloud));
        assert_eq!(ProviderMode::parse("ollama"), Some(ProviderMode::Ollama));
        assert_eq!(ProviderMode::parse("unset"), Some(ProviderMode::Unset));
        assert_eq!(ProviderMode::parse("auto"), None);
        assert_eq!(ProviderMode::Cloud.as_str(), "cloud");
    }

    #[test]
    fn effective_endpoints_are_explicit() {
        let cloud = ProviderConfig::cloud();
        assert_eq!(
            cloud.effective_base_url(),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(cloud.effective_model(), "glm-5.3-flash");
        let ollama = ProviderConfig::ollama();
        assert_eq!(ollama.effective_base_url(), "http://127.0.0.1:11434/v1");
        assert_eq!(ollama.effective_model(), "local");
        assert_eq!(ProviderConfig::unset().effective_base_url(), "");
    }

    #[test]
    fn save_rejects_bad_endpoint_and_model() {
        let mut config = ProviderConfig::cloud();
        config.base_url = Some("not-a-url".to_string());
        assert!(save_provider_config(&config).is_err());
        let mut config = ProviderConfig::cloud();
        config.model = Some("model with space".to_string());
        assert!(save_provider_config(&config).is_err());
        let mut config = ProviderConfig::cloud();
        config.base_url = Some("http://127.0.0.1:9999/v1".to_string());
        assert!(save_provider_config(&config).is_ok());
    }

    #[test]
    fn status_reflects_mode_and_key_presence() {
        let status = provider_status(&ProviderConfig::unset());
        assert!(!status.ready);
        assert_eq!(status.base_url, "");
        let status = provider_status(&ProviderConfig::ollama());
        assert!(status.ready);
        assert_eq!(status.model, "local");
    }
}
