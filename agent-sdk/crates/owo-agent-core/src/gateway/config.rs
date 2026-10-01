use crate::tools::ToolSpec;
use async_trait::async_trait;

use super::is_local_endpoint;
use super::message::{ChatMessage, ModelOutput, ModelProvider};
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// 数据出境开关：false 时拒绝一切云端模型调用。
    pub cloud_enabled: bool,
}

/// 默认模型 Provider：GLM（智谱 BigModel，OpenAI 兼容协议）。
///
/// 端点与模型内置为默认回落，凭据仍**只经 `OPENAI_API_KEY` 环境变量注入**
/// （仓库红线：密钥禁止写入代码/配置/提交）；未设 key 时给出明确指引错误。
/// 便于随时一键实测：只需在进程环境提供 key，无需再配 BASE_URL/MODEL。
pub const DEFAULT_MODEL_BASE_URL: &str = "https://open.bigmodel.cn/api/paas/v4";
pub const DEFAULT_MODEL_ID: &str = "glm-5.3-flash";

/// `"default"` 哨兵（M4.2）：请求级模型覆盖等于该值时**不**固定模型，
/// 回退 Provider 自身解析链（OPENAI_MODEL 运行时热切换 → 启动配置 → 内置默认）。
/// 保证哨兵值绝不泄漏进请求体 `model` 字段。
pub const MODEL_DEFAULT_SENTINEL: &str = "default";

/// 模型档位（M4.2 任务类型路由）：main = 会话/回合主模型；fast = 子代理/压缩等
/// 轻量任务；vision = 图像理解任务（视觉通道端点由 `vision` 模块自身配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    Main,
    Fast,
    Vision,
}

impl ModelTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Fast => "fast",
            Self::Vision => "vision",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "main" => Some(Self::Main),
            "fast" => Some(Self::Fast),
            "vision" => Some(Self::Vision),
            _ => None,
        }
    }
}

/// 档位 → 模型解析（env 单一事实源，不落配置文件）：
/// - `Main`：恒 `None`（走 Provider 自身解析链）；
/// - `Fast`：`OWO_MODEL_FAST`；
/// - `Vision`：`OWO_MODEL_VISION`，未配置时兼容视觉通道既有变量 `OWO_VISION_MODEL`。
///
/// 返回 `None` 表示该档位未显式配置，调用方回退主链
/// （OPENAI_MODEL → 启动配置 → `DEFAULT_MODEL_ID`）。空串/纯空白视为未配置。
pub fn resolve_tier_model(tier: ModelTier) -> Option<String> {
    let names: &[&str] = match tier {
        ModelTier::Main => return None,
        ModelTier::Fast => &["OWO_MODEL_FAST"],
        ModelTier::Vision => &["OWO_MODEL_VISION", "OWO_VISION_MODEL"],
    };
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

impl OpenAiCompatibleConfig {
    pub fn from_env() -> Result<Self, String> {
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_MODEL_BASE_URL.to_string());
        let api_key = match std::env::var("OPENAI_API_KEY") {
            Ok(value) => value,
            Err(_) if is_local_endpoint(&base_url) => String::new(),
            Err(_) => {
                return Err(
                    "缺少 OPENAI_API_KEY 环境变量（或设置 OPENAI_BASE_URL 指向本地兼容端点）"
                        .to_string(),
                )
            }
        };
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL_ID.to_string());
        let cloud_enabled = std::env::var("OWO_CLOUD_ENABLED")
            .ok()
            .and_then(|value| value.parse::<bool>().ok())
            .unwrap_or(true);
        Ok(Self {
            base_url,
            api_key,
            model,
            cloud_enabled,
        })
    }
}

/// R3-B（§3.4「provider 未配置」契约）：无凭据时的占位 Provider。
///
/// 桌面 serve 在无 `OPENAI_API_KEY` 时**必须** ready（诊断/设置/会话/工具全部
/// 可用），模型调用一律返回稳定码 `provider/not_configured` + 可操作中文指引。
/// 归因留给"core 早退/握手超时"是历史缺陷 R3-BUG-05：用户看到的是无法修复的
/// 模糊报错。UI 侧据此呈现模型配置引导（§4.8 Unset 语义：core ready，模型调用
/// 在引导后生效）。
/// 模型 HTTP 客户端统一构造（取优合并自远端 engine；A1-4）：
/// 代理按 OWO_HTTP_PROXY/HTTPS_PROXY/HTTP_PROXY 优先，`NO_PROXY` 排除列表
/// （127.0.0.1/localhost 等本地端点必须直连）。返回 (client, has_proxy)。
pub(crate) fn build_model_http_client(
    connect_timeout_secs: u64,
    total_timeout_secs: u64,
) -> Result<(reqwest::Client, bool), String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(total_timeout_secs));
    let mut has_proxy = false;
    for name in [
        "OWO_HTTP_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "https_proxy",
        "http_proxy",
    ] {
        if let Ok(proxy) = std::env::var(name) {
            if !proxy.trim().is_empty() {
                let mut proxy = reqwest::Proxy::all(proxy)
                    .map_err(|e| format!("代理配置无效（{name}）：{e}"))?;
                has_proxy = true;
                let no_proxy = std::env::var("NO_PROXY")
                    .or_else(|_| std::env::var("no_proxy"))
                    .unwrap_or_default();
                if !no_proxy.trim().is_empty() {
                    if let Some(exclusions) = reqwest::NoProxy::from_string(&no_proxy) {
                        proxy = proxy.no_proxy(Some(exclusions));
                    }
                }
                builder = builder.proxy(proxy);
                break;
            }
        }
    }
    let client = builder
        .build()
        .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
    Ok((client, has_proxy))
}

pub struct UnconfiguredModelProvider {
    reason: String,
}

impl UnconfiguredModelProvider {
    /// 统一错误模型（§2.4）的稳定码：layer=provider，name=not_configured。
    pub const CODE: &'static str = "provider/not_configured";

    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// 带稳定码前缀的错误文案（UI 与日志的唯一文案面；UI 按码渲染，不匹配中文）。
    pub fn message(&self) -> String {
        format!(
            "{}：模型提供商未配置（{}）。请在设置中选择云端或本地 Ollama，或经环境变量 OPENAI_API_KEY 配置凭据",
            Self::CODE,
            self.reason
        )
    }
}

#[async_trait]
impl ModelProvider for UnconfiguredModelProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        Err(self.message())
    }

    // complete_stream/complete_with_model 走 trait 默认实现 → 同样落到 complete 的 Err。
}
