//! §4.8 + R11 模型配置：**独立配置文件**（codex / opencode 风格）。
//!
//! 为什么改：原先把提供商选择塞进 `provider.json`（只有 provider/base_url/model
//! 三个扁平字段），密钥只能靠环境变量，用户的原话是"一点都不正规"——找不到
//! 一个可以手写、可版本无关、集中放"地址 + 模型名 + API Key"的地方。
//!
//! 现在唯一事实源是 `%LOCALAPPDATA%\OwO\Agent\config.json`（可用
//! `OWO_CONFIG_FILE` 覆盖路径；也可直接在设置页里改）：
//!
//! ```json
//! {
//!   "version": 1,
//!   "model": {
//!     "provider": "bigmodel",              // bigmodel | openai | deepseek | dashscope | ollama | custom | unset
//!     "base_url": "https://open.bigmodel.cn/api/paas/v4",
//!     "name": "glm-5.3-flash",
//!     "api_key": "sk-...",                 // 可留空：留空则读 api_key_env 指向的环境变量
//!     "api_key_env": "OPENAI_API_KEY",
//!     "temperature": 0.7                   // 预留（核心暂未消费，先原样保存不丢字段）
//!   }
//! }
//! ```
//!
//! 凭据优先级（与 UI 显示同口径，**单一实现**）：
//! 1. `model.api_key`（文件里手写或设置页里填的，明文，文件 ACL 收紧为仅当前用户）；
//! 2. `model.api_key_env` 指向的环境变量（缺省 `OPENAI_API_KEY`），再退到
//!    `OPENAI_API_KEY` / `DASHSCOPE_API_KEY`；
//! 3. 都没有 → 未配置（核心仍可 ready，诊断/会话可用，只是模型调用报稳定码）。
//!
//! 密钥纪律：**绝不回传前端、绝不写日志**；壳只回 `keyConfigured`/`keySource`/掩码。
//! 文件位置固定在 `%LOCALAPPDATA%` 下（不是工作区），从设计上避免被提交进仓库。
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 配置文件版本：将来改结构时用它做迁移判断（当前只写不读分支）。
const CONFIG_VERSION: u32 = 1;
/// 默认凭据环境变量名（与核心 `gateway.rs` 读取的变量一致）。
pub const DEFAULT_KEY_ENV: &str = "OPENAI_API_KEY";

/// 提供商选择。`Custom` 与 `Cloud` 的行为差别只在 UI 措辞：两者都是
/// "OpenAI 兼容端点 + 自定义地址/模型名"，统一走同一条注入路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderMode {
    Bigmodel,
    Openai,
    Deepseek,
    Dashscope,
    Ollama,
    Custom,
    Unset,
}

impl ProviderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderMode::Bigmodel => "bigmodel",
            ProviderMode::Openai => "openai",
            ProviderMode::Deepseek => "deepseek",
            ProviderMode::Dashscope => "dashscope",
            ProviderMode::Ollama => "ollama",
            ProviderMode::Custom => "custom",
            ProviderMode::Unset => "unset",
        }
    }

    /// 宽松解析：兼容历史 `provider.json` 里的 `cloud`（= 内置 BigModel 端点）。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "bigmodel" | "zhipu" | "glm" | "cloud" => Some(Self::Bigmodel),
            "openai" => Some(Self::Openai),
            "deepseek" => Some(Self::Deepseek),
            "dashscope" | "qwen" | "aliyun" => Some(Self::Dashscope),
            "ollama" => Some(Self::Ollama),
            "custom" | "self" | "selfhosted" | "openai-compatible" => Some(Self::Custom),
            "unset" | "none" | "" => Some(Self::Unset),
            _ => None,
        }
    }

    /// 本地端点（不需要密钥）。用于状态判定与"未配置凭据"文案的分流。
    pub fn is_local(self) -> bool {
        matches!(self, ProviderMode::Ollama)
    }

    /// 该提供商的默认端点（用户没写 base_url 时用）。
    pub fn default_base_url(self) -> &'static str {
        match self {
            ProviderMode::Bigmodel => "https://open.bigmodel.cn/api/paas/v4",
            ProviderMode::Openai => "https://api.openai.com/v1",
            ProviderMode::Deepseek => "https://api.deepseek.com/v1",
            ProviderMode::Dashscope => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            ProviderMode::Ollama => "http://127.0.0.1:11434/v1",
            ProviderMode::Custom | ProviderMode::Unset => "",
        }
    }

    /// 该提供商的默认模型名。
    pub fn default_model(self) -> &'static str {
        match self {
            ProviderMode::Bigmodel => "glm-5.3-flash",
            ProviderMode::Openai => "gpt-4o-mini",
            ProviderMode::Deepseek => "deepseek-chat",
            ProviderMode::Dashscope => "qwen-plus",
            ProviderMode::Ollama => "local",
            ProviderMode::Custom | ProviderMode::Unset => "",
        }
    }
}

/// `config.json` 的 `model` 段（serde 结构即文件契约）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_provider")]
    pub provider: ProviderMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// 模型名。历史字段名 `model` 仍可读（`alias`），写回统一用 `name`。
    #[serde(default, alias = "model", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// 单次请求超时（秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// 上下文窗口（token）。核心据此设压缩预算（`OWO_MODEL_CONTEXT_WINDOW`）。
    /// 这是用户明确要求"不能写死"的一项：不同模型窗口差别巨大（8k → 200k+）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 单次回复最大输出 token（`OWO_MODEL_MAX_OUTPUT_TOKENS`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// 保留最近多少条消息不压缩（`OWO_AGENT_KEEP_RECENT`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent: Option<u64>,
    /// 是否启用上下文压缩（`OWO_AGENT_COMPACTION`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<bool>,
    /// **可选的模型清单**：写在这里的模型名会作为界面下拉/建议项出现。
    ///
    /// 为什么放在配置文件里而不是代码里：用户明确要求"模型名称不能写死"。
    /// 界面的候选来自三处，全部是数据而非代码：
    ///   1. 本字段（用户自己维护的清单，改文件即刻生效）；
    ///   2. `预设`（内置常见服务商的默认模型名，仅作起点）；
    ///   3. 模型名输入框本身可自由输入任意字符串。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

fn default_provider() -> ProviderMode {
    ProviderMode::Unset
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: ProviderMode::Unset,
            base_url: None,
            name: None,
            api_key: None,
            api_key_env: None,
            temperature: None,
            timeout_secs: None,
            context_window: None,
            max_output_tokens: None,
            keep_recent: None,
            compaction: None,
            models: Vec::new(),
        }
    }
}

impl ModelConfig {
    /// 展示/注入用的有效端点。
    pub fn effective_base_url(&self) -> String {
        if let Some(url) = self.base_url.as_ref().filter(|url| !url.trim().is_empty()) {
            return url.trim().to_string();
        }
        self.provider.default_base_url().to_string()
    }

    /// 展示/注入用的有效模型名。
    pub fn effective_model(&self) -> String {
        if let Some(name) = self.name.as_ref().filter(|name| !name.trim().is_empty()) {
            return name.trim().to_string();
        }
        self.provider.default_model().to_string()
    }

    /// 凭据环境变量名（默认 `OPENAI_API_KEY`）。
    pub fn key_env_name(&self) -> String {
        self.api_key_env
            .as_ref()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .unwrap_or(DEFAULT_KEY_ENV)
            .to_string()
    }

    fn file_key(&self) -> Option<&str> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
    }
}

/// 整个配置文件（未知字段保留：用户手写的注释性字段不得被我们悄悄删掉）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub model: ModelConfig,
    /// 未识别字段原样保留（向后兼容：新版本写的字段不该被旧版本抹掉）。
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            model: ModelConfig::default(),
            extra: serde_json::Map::new(),
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

/// 配置文件路径：`OWO_CONFIG_FILE` 优先（便于多套配置与排障），否则数据目录下 `config.json`。
pub fn config_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("OWO_CONFIG_FILE") {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    data_dir().map(|dir| dir.join("config.json"))
}

/// 历史 `provider.json` 路径（仅用于迁移读取，不再写入）。
fn legacy_provider_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("provider.json"))
}

/// 读取配置。
///
/// 契约（2026-09-23 实测缺陷后收紧）：
/// 1. `config.json` 存在且能解析 → **以它为准**，绝不看历史文件；
/// 2. `config.json` 不存在 → 允许从历史 `provider.json` 迁移**一次**，迁移后立刻把
///    历史文件改名为 `provider.json.migrated`，避免它继续以"影子配置"的身份被读；
/// 3. `config.json` 存在但**损坏** → 直接用默认值，**不回落到历史文件**：这时回落
///    等于让用户"明明看到界面写着 ollama，核心却在打 127.0.0.1:9999"（真实故障：
///    旧 provider.json 里的测试端点被持续注入，报错却是 proxy error，完全误导）。
pub fn load_config() -> ConfigFile {
    if let Some(path) = config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<ConfigFile>(&text) {
                Ok(config) => return config,
                Err(_) => {
                    // 损坏文件不猜测、不回落：默认值 + 让上层的 configPath 展示提醒用户。
                    return ConfigFile::default();
                }
            }
        }
    }
    if let Some(path) = legacy_provider_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                let legacy = ConfigFile {
                    version: CONFIG_VERSION,
                    model: ModelConfig {
                        provider: value
                            .get("provider")
                            .and_then(|mode| mode.as_str())
                            .and_then(ProviderMode::parse)
                            .unwrap_or(ProviderMode::Unset),
                        base_url: value
                            .get("base_url")
                            .and_then(|url| url.as_str())
                            .map(str::to_string),
                        name: value
                            .get("model")
                            .and_then(|model| model.as_str())
                            .map(str::to_string),
                        ..ModelConfig::default()
                    },
                    extra: serde_json::Map::new(),
                };
                // 迁移一次：把历史配置写成新文件，然后把历史文件改名归档。
                // 即使写失败也要改名——留着它只会在下次继续冒充"当前配置"。
                let _ = save_config(&legacy);
                let _ = std::fs::rename(&path, path.with_extension("json.migrated"));
                return legacy;
            }
        }
    }
    ConfigFile::default()
}

/// 校验端点/模型名形状（保存前必过，避免把坏配置写进文件后核心起不来）。
pub fn validate(model: &ModelConfig) -> Result<(), String> {
    if let Some(url) = model.base_url.as_ref().filter(|url| !url.trim().is_empty()) {
        let url = url.trim();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("接口地址必须以 http:// 或 https:// 开头".to_string());
        }
    }
    if matches!(model.provider, ProviderMode::Custom) && model.effective_base_url().is_empty() {
        return Err("自定义提供方必须填写接口地址（base_url）".to_string());
    }
    if let Some(name) = model.name.as_ref().filter(|name| !name.trim().is_empty()) {
        if name.contains(char::is_whitespace) {
            return Err("模型名不能包含空格".to_string());
        }
    }
    if let Some(env_name) = model.api_key_env.as_ref().filter(|name| !name.trim().is_empty()) {
        if env_name.contains(char::is_whitespace) || env_name.contains('=') {
            return Err("环境变量名不合法".to_string());
        }
    }
    Ok(())
}

/// Windows：把配置文件 ACL 收紧为"仅当前用户 + SYSTEM 可读写"
/// （密钥可能明文写在里面，不能让同机其他账户读到）。
#[cfg(windows)]
fn harden_file_acl(path: &Path) {
    // icacls 的继承清理等价物：先移除继承，再只授权当前用户。
    use std::process::Command;
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return;
    }
    let mut command = Command::new("icacls");
    command
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:F"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // 不弹控制台
    }
    let _ = command.output();
}

#[cfg(not(windows))]
fn harden_file_acl(_path: &Path) {}

/// 写入配置：原子替换（临时文件 + rename），并收紧 ACL。
/// 只在这里落盘，保证"保存"永远是一个动作、一个口径。
pub fn save_config(config: &ConfigFile) -> Result<PathBuf, String> {
    let path = config_path().ok_or_else(|| "无法确定配置目录".to_string())?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("创建配置目录失败：{error}"))?;
    }
    let mut payload = config.clone();
    payload.version = CONFIG_VERSION;
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| format!("序列化配置失败：{error}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, format!("{text}\n")).map_err(|error| format!("写配置失败：{error}"))?;
    harden_file_acl(&temp);
    std::fs::rename(&temp, &path).map_err(|error| format!("替换配置失败：{error}"))?;
    Ok(path)
}

/// 凭据来源（UI 直接展示，便于用户判断密钥到底从哪儿来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// 配置文件里的 `model.api_key`
    ConfigFile,
    /// `model.api_key_env` 指向的环境变量
    ConfigEnv,
    /// 缺省 `OPENAI_API_KEY`
    DefaultEnv,
    /// 历史 `DASHSCOPE_API_KEY`
    LegacyEnv,
    /// 无可用凭据
    None,
}

impl KeySource {
    pub fn as_str(self) -> &'static str {
        match self {
            KeySource::ConfigFile => "config_file",
            KeySource::ConfigEnv => "config_env",
            KeySource::DefaultEnv => "environment",
            KeySource::LegacyEnv => "legacy_env",
            KeySource::None => "none",
        }
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 解析凭据（单一实现）：返回 (密钥, 来源)。
/// 密钥只在本进程内流动：注入核心子进程环境 + 脱敏后展示，绝不回传前端。
pub fn resolve_api_key(model: &ModelConfig) -> (Option<String>, KeySource) {
    if let Some(key) = model.file_key() {
        return (Some(key.to_string()), KeySource::ConfigFile);
    }
    let env_name = model.key_env_name();
    if let Some(key) = env_non_empty(&env_name) {
        let source = if env_name == DEFAULT_KEY_ENV {
            KeySource::DefaultEnv
        } else {
            KeySource::ConfigEnv
        };
        return (Some(key), source);
    }
    if env_name != DEFAULT_KEY_ENV {
        if let Some(key) = env_non_empty(DEFAULT_KEY_ENV) {
            return (Some(key), KeySource::DefaultEnv);
        }
    }
    if model.api_key_env.is_none() {
        if let Some(key) = env_non_empty("DASHSCOPE_API_KEY") {
            return (Some(key), KeySource::LegacyEnv);
        }
    }
    (None, KeySource::None)
}

/// 掩码展示：`sk-1234…cdef`（只给前后各 4 位，长度不足则整体星号）。
pub fn mask_key(key: &str) -> String {
    let key = key.trim();
    if key.len() <= 8 {
        return "*".repeat(key.len().max(4));
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = {
        let chars: Vec<char> = key.chars().collect();
        chars[chars.len().saturating_sub(4)..].iter().collect()
    };
    format!("{head}…{tail}")
}

/// 提供商状态（UI 展示；**不含密钥本体**）。
#[derive(Debug, Clone)]
pub struct ProviderStatus {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub key_configured: bool,
    pub key_source: &'static str,
    /// 掩码后的密钥（未配置时为空），用于让用户确认"填进去的是哪一把"。
    pub key_masked: String,
    /// 凭据环境变量名（UI 提示用）。
    pub key_env: String,
    /// 可否立即发起模型调用：本地端点在、或云端有凭据。
    pub ready: bool,
    /// 配置文件路径（UI 展示"配置写在哪"，用户可手改）。
    pub config_path: String,
    /// 用户维护的模型清单（界面建议项，来自配置文件）。
    pub models: Vec<String>,
    /// 可调参数（原样回传，界面据此回填输入框；None = 用核心默认）。
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub timeout_secs: Option<u64>,
    pub keep_recent: Option<u64>,
    pub compaction: Option<bool>,
}

/// 状态计算（与核心 `EnvProviderProbe` 同口径：显式配置 > 环境凭据）。
pub fn provider_status(model: &ModelConfig) -> ProviderStatus {
    let (key, source) = resolve_api_key(model);
    let ready = match model.provider {
        ProviderMode::Unset => key.is_some(),
        // 本地端点（Ollama）不需要凭据；其余（云端/自建）有 key 才算可用。
        provider if provider.is_local() => true,
        _ => key.is_some(),
    };
    ProviderStatus {
        provider: model.provider.as_str().to_string(),
        base_url: model.effective_base_url(),
        model: model.effective_model(),
        key_configured: key.is_some(),
        key_source: source.as_str(),
        key_masked: key.as_deref().map(mask_key).unwrap_or_default(),
        key_env: model.key_env_name(),
        ready,
        config_path: config_path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
        models: model.models.clone(),
        context_window: model.context_window,
        max_output_tokens: model.max_output_tokens,
        temperature: model.temperature,
        timeout_secs: model.timeout_secs,
        keep_recent: model.keep_recent,
        compaction: model.compaction,
    }
}

/// 注入核心子进程的模型环境变量（§4.8：只按显式配置注入，不静默猜测）。
///
/// - 端点/模型：按配置或提供商默认值注入 `OPENAI_BASE_URL` / `OPENAI_MODEL`；
/// - 凭据：配置文件里的 key 或环境变量解析结果注入 `OPENAI_API_KEY`
///   （核心只认环境变量，密钥不落工作区、不进命令行参数）；
/// - Unset：不注入任何模型变量（核心仍 ready，模型调用给稳定码）。
pub fn apply_provider_env(
    command: &mut std::process::Command,
    model: &ModelConfig,
    log_path: &Path,
) {
    if matches!(model.provider, ProviderMode::Unset) {
        // 未显式选择：不注入端点/模型；若环境里本来就有凭据，核心会按内置端点
        // 兜底工作（gateway 的 EnvProviderProbe 语义），此处只提示不代填。
        let (key, _) = resolve_api_key(model);
        if key.is_none() {
            append_log_line(
                log_path,
                "[provider] 未配置模型提供商与凭据：核心就绪但模型调用不可用，\
                 请在设置页配置（config.json 的 model 段）",
            );
        }
        return;
    }
    command
        .env("OPENAI_BASE_URL", model.effective_base_url())
        .env("OPENAI_MODEL", model.effective_model());
    let (key, source) = resolve_api_key(model);
    if let Some(key) = key {
        command.env("OPENAI_API_KEY", key);
    } else {
        append_log_line(
            log_path,
            &format!(
                "[provider] 未找到模型凭据（config.json 的 model.api_key 为空，环境变量 {} 也未设置）：\
                 核心就绪，模型调用会返回 provider/not_configured",
                model.key_env_name()
            ),
        );
    }
    // 上下文/采样等"可调参数"：配置文件是唯一事实源，核心按同名环境变量消费。
    // 未配置的字段**不注入**——核心保留自己的默认值，绝不在这里编造一个数字。
    for (env_name, value) in [
        ("OWO_MODEL_CONTEXT_WINDOW", model.context_window),
        ("OWO_MODEL_MAX_OUTPUT_TOKENS", model.max_output_tokens),
        ("OWO_AGENT_KEEP_RECENT", model.keep_recent),
        ("OWO_MODEL_TIMEOUT_SECS", model.timeout_secs),
    ] {
        if let Some(value) = value.filter(|value| *value > 0) {
            command.env(env_name, value.to_string());
        }
    }
    if let Some(temperature) = model.temperature.filter(|value| value.is_finite()) {
        command.env("OWO_MODEL_TEMPERATURE", temperature.to_string());
    }
    if let Some(compaction) = model.compaction {
        command.env("OWO_AGENT_COMPACTION", if compaction { "1" } else { "0" });
    }
    append_log_line(
        log_path,
        &format!(
            "[provider] 已注入模型配置：provider={} base_url={} model={} key_source={} context_window={} max_output={} temperature={} keep_recent={}",
            model.provider.as_str(),
            model.effective_base_url(),
            model.effective_model(),
            source.as_str(),
            model
                .context_window
                .map(|value| value.to_string())
                .unwrap_or_else(|| "(核心默认)".to_string()),
            model
                .max_output_tokens
                .map(|value| value.to_string())
                .unwrap_or_else(|| "(核心默认)".to_string()),
            model
                .temperature
                .map(|value| value.to_string())
                .unwrap_or_else(|| "(核心默认)".to_string()),
            model
                .keep_recent
                .map(|value| value.to_string())
                .unwrap_or_else(|| "(核心默认)".to_string()),
        ),
    );
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

/// 把"本次实际注入的模型配置"写进壳日志（**不含密钥本体**）。
///
/// 为什么必须有这条：2026-09-23 的真实故障里，核心报的是
/// `proxy: error sending request for url (http://127.0.0.1:9999/v1/chat/completions)`，
/// 而界面显示的是 ollama —— 用户与开发者都无从判断"到底用了哪份配置、哪个端点"。
/// 一行 `[model] effective ...` 就能把这个问题变成 10 秒定位。
/// 同时把配置来源（config.json / 环境变量）一并写明。
pub fn log_effective_config(log_path: &Path, model: &ModelConfig) {
    let (key, source) = resolve_api_key(model);
    append_log_line(
        log_path,
        &format!(
            "[model] effective provider={} base_url={} model={} key_source={} key={} config={}",
            model.provider.as_str(),
            if model.effective_base_url().is_empty() {
                "(未设置)".to_string()
            } else {
                model.effective_base_url()
            },
            model.effective_model(),
            source.as_str(),
            key.as_deref()
                .map(mask_key)
                .unwrap_or_else(|| "(未配置)".to_string()),
            config_path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "(未知)".to_string()),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn provider_mode_parse_is_lenient_but_explicit() {
        assert_eq!(ProviderMode::parse("bigmodel"), Some(ProviderMode::Bigmodel));
        // 历史 provider.json 写的是 "cloud"：必须仍解析成内置 BigModel 语义，
        // 否则老用户升级后提供商选择会被静默重置成 unset。
        assert_eq!(ProviderMode::parse("cloud"), Some(ProviderMode::Bigmodel));
        assert_eq!(ProviderMode::parse("OpenAI"), Some(ProviderMode::Openai));
        assert_eq!(ProviderMode::parse("selfhosted"), Some(ProviderMode::Custom));
        assert_eq!(ProviderMode::parse("unset"), Some(ProviderMode::Unset));
        assert_eq!(ProviderMode::parse("nonsense"), None);
    }

    #[test]
    fn defaults_fill_endpoint_and_model_per_provider() {
        let mut model = ModelConfig::default();
        model.provider = ProviderMode::Bigmodel;
        assert_eq!(
            model.effective_base_url(),
            "https://open.bigmodel.cn/api/paas/v4"
        );
        assert_eq!(model.effective_model(), "glm-5.3-flash");
        model.provider = ProviderMode::Ollama;
        assert_eq!(model.effective_base_url(), "http://127.0.0.1:11434/v1");
        assert_eq!(model.effective_model(), "local");
    }

    #[test]
    fn explicit_values_win_over_defaults() {
        let model = ModelConfig {
            provider: ProviderMode::Custom,
            base_url: Some("http://127.0.0.1:9999/v1".into()),
            name: Some("my-model".into()),
            ..ModelConfig::default()
        };
        assert_eq!(model.effective_base_url(), "http://127.0.0.1:9999/v1");
        assert_eq!(model.effective_model(), "my-model");
    }

    #[test]
    fn config_file_roundtrip_keeps_unknown_fields_and_reads_legacy_alias() {
        // 手写配置文件里出现我们不认识的字段（或历史 `model` 字段名）时不得报错、
        // 不得丢弃——用户手写的内容被静默抹掉是最难查的一类"配置不生效"。
        let text = r#"{
            "version": 1,
            "model": {
                "provider": "custom",
                "base_url": "https://example.com/v1",
                "model": "legacy-name",
                "api_key_env": "MY_KEY",
                "temperature": 0.3
            },
            "future_section": { "keep": true }
        }"#;
        let parsed: ConfigFile = serde_json::from_str(text).expect("必须能解析");
        assert_eq!(parsed.model.provider, ProviderMode::Custom);
        assert_eq!(parsed.model.effective_model(), "legacy-name");
        assert_eq!(parsed.model.key_env_name(), "MY_KEY");
        assert_eq!(parsed.model.temperature, Some(0.3));
        assert!(parsed.extra.contains_key("future_section"));
        let written = serde_json::to_string(&parsed).unwrap();
        assert!(written.contains("future_section"), "未知字段必须原样保留");
        assert!(written.contains("legacy-name"), "模型名必须能往返");
    }

    #[test]
    fn validate_rejects_bad_endpoint_and_env_name() {
        let mut model = ModelConfig {
            provider: ProviderMode::Custom,
            base_url: Some("not-a-url".into()),
            ..ModelConfig::default()
        };
        assert!(validate(&model).is_err());
        model.base_url = Some("https://ok.example/v1".into());
        assert!(validate(&model).is_ok());
        // 自定义提供方必须有地址，否则保存进去就是"看着配好了、其实连不上"。
        let bare = ModelConfig {
            provider: ProviderMode::Custom,
            ..ModelConfig::default()
        };
        assert!(validate(&bare).is_err());
        let bad_env = ModelConfig {
            provider: ProviderMode::Openai,
            api_key_env: Some("BAD NAME".into()),
            ..ModelConfig::default()
        };
        assert!(validate(&bad_env).is_err());
    }

    #[test]
    fn api_key_precedence_is_file_then_config_env_then_default() {
        let _serial = env_serial();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("OWO_TEST_KEY");
        std::env::remove_var("DASHSCOPE_API_KEY");

        // 文件里的 key 优先。
        let with_file = ModelConfig {
            provider: ProviderMode::Openai,
            api_key: Some("file-key-1234567890".into()),
            ..ModelConfig::default()
        };
        std::env::set_var("OPENAI_API_KEY", "env-key");
        let (key, source) = resolve_api_key(&with_file);
        assert_eq!(key.as_deref(), Some("file-key-1234567890"));
        assert_eq!(source, KeySource::ConfigFile);

        // 文件为空 → 自定义环境变量。
        std::env::set_var("OWO_TEST_KEY", "custom-env-key");
        let mut with_env = with_file.clone();
        with_env.api_key = None;
        with_env.api_key_env = Some("OWO_TEST_KEY".into());
        let (key, source) = resolve_api_key(&with_env);
        assert_eq!(key.as_deref(), Some("custom-env-key"));
        assert_eq!(source, KeySource::ConfigEnv);

        // 自定义环境变量也没值 → 回落 OPENAI_API_KEY。
        std::env::remove_var("OWO_TEST_KEY");
        let (key, source) = resolve_api_key(&with_env);
        assert_eq!(key.as_deref(), Some("env-key"));
        assert_eq!(source, KeySource::DefaultEnv);

        // 全都没有 → None（不得凭空造一个空密钥）。
        std::env::remove_var("OPENAI_API_KEY");
        let (key, source) = resolve_api_key(&with_env);
        assert_eq!(key, None);
        assert_eq!(source, KeySource::None);

        // 未指定 api_key_env 时才看历史 DASHSCOPE_API_KEY（只提示不迁移）。
        std::env::set_var("DASHSCOPE_API_KEY", "legacy-key");
        let (key, source) = resolve_api_key(&ModelConfig::default());
        assert_eq!(key.as_deref(), Some("legacy-key"));
        assert_eq!(source, KeySource::LegacyEnv);
        std::env::remove_var("DASHSCOPE_API_KEY");
    }

    #[test]
    fn status_never_exposes_raw_key_and_reports_readiness() {
        let _serial = env_serial();
        std::env::remove_var("OPENAI_API_KEY");
        let model = ModelConfig {
            provider: ProviderMode::Openai,
            api_key: Some("sk-abcdefghijklmnop".into()),
            ..ModelConfig::default()
        };
        let status = provider_status(&model);
        assert!(status.key_configured);
        assert!(status.ready);
        assert!(!status.key_masked.contains("cdefghij"), "掩码不得泄露中段");
        assert!(status.key_masked.starts_with("sk-a"));
        assert!(status.key_masked.ends_with("mnop"));

        // 本地 Ollama 无凭据也算就绪（本地端点不需要密钥）。
        let local = ModelConfig {
            provider: ProviderMode::Ollama,
            ..ModelConfig::default()
        };
        assert!(provider_status(&local).ready);
        assert!(!provider_status(&local).key_configured);

        // 云端无凭据必须如实报未就绪（不得让引导页把健康主界面顶掉的反向错误）。
        let cloud = ModelConfig {
            provider: ProviderMode::Openai,
            ..ModelConfig::default()
        };
        assert!(!provider_status(&cloud).ready);
    }

    #[test]
    fn mask_key_handles_short_values() {
        assert_eq!(mask_key("abc"), "****");
        assert_eq!(mask_key("12345678"), "********");
        assert!(mask_key("123456789").contains('…'));
    }
}
