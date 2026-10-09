use crate::tools::ToolSpec;
use async_trait::async_trait;
use std::sync::Arc;

use super::config::*;
use super::is_local_endpoint;
use super::message::*;
use super::provider::*;
/// R9 韧性层：重试策略（指数退避 + jitter）。
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// 重试次数（不含首次请求）。
    pub max_retries: usize,
    /// 首次退避基数（毫秒）。
    pub base_delay_ms: u64,
    /// 退避上限（毫秒）。
    pub max_delay_ms: u64,
    /// 429 是否重试。
    pub retry_429: bool,
    /// 连接/超时/空闲看门狗类失败是否重试。
    pub retry_network: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 500,
            max_delay_ms: 8_000,
            retry_429: true,
            retry_network: true,
        }
    }
}

impl RetryPolicy {
    /// 环境变量：OWO_MODEL_RETRY_MAX / OWO_MODEL_RETRY_BASE_MS / OWO_MODEL_RETRY_MAX_DELAY_MS。
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            max_retries: std::env::var("OWO_MODEL_RETRY_MAX")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default.max_retries),
            base_delay_ms: std::env::var("OWO_MODEL_RETRY_BASE_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default.base_delay_ms),
            max_delay_ms: std::env::var("OWO_MODEL_RETRY_MAX_DELAY_MS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default.max_delay_ms),
            ..default
        }
    }

    /// 第 `attempt` 次重试前延迟：`min(max, base × 2^attempt)` + 0..20% jitter。
    pub fn delay_for(&self, attempt: usize) -> std::time::Duration {
        let exponential = self
            .base_delay_ms
            .saturating_mul(1_u64 << attempt.min(10))
            .min(self.max_delay_ms);
        let jitter = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.subsec_nanos())
                .unwrap_or(0);
            (nanos, attempt).hash(&mut hasher);
            hasher.finish() % 21 // 0..=20
        };
        let delay =
            exponential.saturating_add(exponential.saturating_mul(jitter).saturating_div(100));
        std::time::Duration::from_millis(delay)
    }
}

/// R9 韧性层：熔断器状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// R9 韧性层：连续失败熔断器（Closed → Open → HalfOpen → Closed）。
pub struct CircuitBreaker {
    failure_threshold: usize,
    cooldown: std::time::Duration,
    consecutive_failures: std::sync::atomic::AtomicUsize,
    state: std::sync::Mutex<BreakerState>,
    opened_at: std::sync::Mutex<Option<std::time::Instant>>,
    half_open_probe: std::sync::atomic::AtomicBool,
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(5, std::time::Duration::from_secs(10))
    }
}

impl CircuitBreaker {
    pub fn new(failure_threshold: usize, cooldown: std::time::Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            cooldown,
            consecutive_failures: std::sync::atomic::AtomicUsize::new(0),
            state: std::sync::Mutex::new(BreakerState::Closed),
            opened_at: std::sync::Mutex::new(None),
            half_open_probe: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 环境变量：OWO_MODEL_CIRCUIT_THRESHOLD（默认 5）/ OWO_MODEL_CIRCUIT_COOLDOWN_SECS（默认 10）。
    pub fn from_env() -> Self {
        let default = Self::default();
        let threshold = std::env::var("OWO_MODEL_CIRCUIT_THRESHOLD")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default.failure_threshold);
        let cooldown = std::env::var("OWO_MODEL_CIRCUIT_COOLDOWN_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(default.cooldown);
        Self::new(threshold, cooldown)
    }

    pub fn state(&self) -> BreakerState {
        match *self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
        {
            BreakerState::Open => {
                let opened = *self
                    .opened_at
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if opened.is_some_and(|at| at.elapsed() >= self.cooldown) {
                    BreakerState::HalfOpen
                } else {
                    BreakerState::Open
                }
            }
            other => other,
        }
    }

    pub fn consecutive_failures(&self) -> usize {
        self.consecutive_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 是否放行请求；HalfOpen 仅放行一个探测请求。
    pub fn allow_request(&self) -> bool {
        match self.state() {
            BreakerState::Closed => true,
            BreakerState::Open => false,
            BreakerState::HalfOpen => self
                .half_open_probe
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok(),
        }
    }

    pub fn record_success(&self) {
        self.consecutive_failures
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.half_open_probe
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = BreakerState::Closed;
        *self
            .opened_at
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }

    pub fn record_failure(&self) {
        let failures = self
            .consecutive_failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if failures >= self.failure_threshold {
            *self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = BreakerState::Open;
            *self
                .opened_at
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(std::time::Instant::now());
            self.half_open_probe
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 强制复位（运维/测试）。
    pub fn reset(&self) {
        self.record_success();
    }
}

/// 已知不可恢复的 Provider 错误，不应通过重试或 fallback 放大请求。
/// BigModel HTTP 429 code 1113 表示余额不足或无可用资源包；其他 429 仍遵循 RetryPolicy。
pub fn is_non_retryable_provider_failure(error: &str) -> bool {
    let Some((_, payload)) = error.split_once("模型返回 429") else {
        return false;
    };
    let Some(json_start) = payload.find('{') else {
        return false;
    };
    let mut parser = serde_json::Deserializer::from_str(&payload[json_start..]);
    let Ok(value) = <serde_json::Value as serde::Deserialize>::deserialize(&mut parser) else {
        return false;
    };
    value
        .get("code")
        .and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| code.as_i64().map(|value| value.to_string()))
        })
        .is_some_and(|code| code == "1113")
}

/// 错误是否可重试：网络/5xx/临时429/空闲看门狗 → 可；资源耗尽/预算/出境/解析/4xx → 不可。
pub(super) fn is_retriable(error: &str, policy: &RetryPolicy) -> bool {
    if error.contains("预算已超限")
        || error.contains("数据出境")
        || is_non_retryable_provider_failure(error)
    {
        return false;
    }
    if error.contains("模型返回 429") {
        return policy.retry_429;
    }
    if error.contains("模型返回 5") {
        return true;
    }
    if error.contains("模型请求失败")
        || error.contains("流式输出空闲超时")
        || error.contains("流式读取失败")
        || error.contains("连接")
        || error.contains("超时")
    {
        return policy.retry_network;
    }
    false
}

/// R9 韧性层：Provider 链（强模型 → 次选云 → 本地），带指数退避重试与熔断器。
/// failover 语义：primary 连续失败 → 熔断打开 → 快速失败；冷却后 HalfOpen 探测。
pub struct ResilientProvider {
    primary: Arc<dyn ModelProvider>,
    fallbacks: Vec<Arc<dyn ModelProvider>>,
    breaker: Arc<CircuitBreaker>,
    retry: RetryPolicy,
}

impl std::fmt::Debug for ResilientProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResilientProvider")
            .field("fallbacks", &self.fallbacks.len())
            .field("breaker", &self.breaker.state())
            .finish()
    }
}

impl ResilientProvider {
    pub fn new(
        primary: Arc<dyn ModelProvider>,
        fallbacks: Vec<Arc<dyn ModelProvider>>,
        breaker: CircuitBreaker,
        retry: RetryPolicy,
    ) -> Self {
        Self {
            primary,
            fallbacks,
            breaker: Arc::new(breaker),
            retry,
        }
    }

    /// 环境变量构造：主 = OPENAI_BASE_URL/OPENAI_API_KEY/OPENAI_MODEL；
    /// `OWO_PROVIDER=anthropic` 且 ANTHROPIC_* 可用时改走 Anthropic 原生通道（A1-1）；
    /// fallback = OWO_MODEL_FALLBACK_BASE_URLS（逗号分隔；本地端点无需 key）。
    pub fn from_env() -> Result<Self, String> {
        if wants_anthropic() {
            let config = crate::anthropic::AnthropicConfig::from_env()?;
            let seed = OpenAiCompatibleConfig {
                base_url: config.base_url.clone(),
                api_key: config.api_key.clone(),
                model: config.model.clone(),
                cloud_enabled: config.cloud_enabled,
            };
            let primary: Arc<dyn ModelProvider> =
                Arc::new(crate::anthropic::AnthropicProvider::new(config)?);
            return Self::from_primary(primary, &seed);
        }
        let config = OpenAiCompatibleConfig::from_env()?;
        Self::from_config(config)
    }

    /// 以显式主配置构造（CLI 接线用，主配置的 model/api_key 已确定）；
    /// fallback 仍读 OWO_MODEL_FALLBACK_BASE_URLS（同 model；本地端点无需 key）。
    pub fn from_config(config: OpenAiCompatibleConfig) -> Result<Self, String> {
        let primary: Arc<dyn ModelProvider> =
            Arc::new(OpenAiCompatibleProvider::new(config.clone())?);
        Self::from_primary(primary, &config)
    }

    /// 以给定主 provider 构造（fallback 链共用；Anthropic/OpenAI 只差主通道）。
    pub fn from_primary(
        primary: Arc<dyn ModelProvider>,
        config: &OpenAiCompatibleConfig,
    ) -> Result<Self, String> {
        let mut fallbacks: Vec<Arc<dyn ModelProvider>> = Vec::new();
        if let Ok(urls) = std::env::var("OWO_MODEL_FALLBACK_BASE_URLS") {
            for url in urls.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let local = is_local_endpoint(url);
                let fallback_config = OpenAiCompatibleConfig {
                    base_url: url.to_string(),
                    api_key: if local {
                        String::new()
                    } else {
                        config.api_key.clone()
                    },
                    model: config.model.clone(),
                    cloud_enabled: config.cloud_enabled || local,
                };
                fallbacks.push(Arc::new(OpenAiCompatibleProvider::new(fallback_config)?));
            }
        }
        Ok(Self::new(
            primary,
            fallbacks,
            CircuitBreaker::from_env(),
            RetryPolicy::from_env(),
        ))
    }

    /// 延迟解析入口（取优合并自远端 engine）：主通道 = [`DeferredProvider`]（每次
    /// 调用前重读环境配置并可按指纹热重建），fallback 仍读
    /// `OWO_MODEL_FALLBACK_BASE_URLS`。桌面 serve 用它——未配置时不拒绝启动
    /// （调用点才返回 `provider/not_configured`），配置/换模型后无需重启即生效。
    pub fn from_deferred() -> Self {
        // 配置未就绪时用空种子构造 fallback 链；真正的主 provider 是 DeferredProvider。
        let seed = OpenAiCompatibleConfig::from_env().unwrap_or_else(|_| OpenAiCompatibleConfig {
            base_url: String::new(),
            api_key: String::new(),
            model: std::env::var("OPENAI_MODEL")
                .unwrap_or_else(|_| super::DEFAULT_MODEL_ID.to_string()),
            cloud_enabled: true,
        });
        Self::from_primary(Arc::new(DeferredProvider::new()), &seed)
            .expect("fallback 链构造不应失败")
    }

    pub fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    pub fn retry(&self) -> &RetryPolicy {
        &self.retry
    }

    fn providers(&self) -> Vec<Arc<dyn ModelProvider>> {
        let mut providers = Vec::with_capacity(1 + self.fallbacks.len());
        providers.push(Arc::clone(&self.primary));
        providers.extend(self.fallbacks.iter().cloned());
        providers
    }

    /// 对单个 provider 执行带退避重试的调用；返回 (结果, 是否命中可重试失败)。
    async fn call_with_retry(
        provider: &Arc<dyn ModelProvider>,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        retry: &RetryPolicy,
    ) -> (Result<ModelOutput, String>, bool) {
        let mut attempt = 0;
        loop {
            match provider.complete_with_model(model, messages, tools).await {
                Ok(output) => return (Ok(output), false),
                Err(error) => {
                    let retriable = is_retriable(&error, retry);
                    if !retriable || attempt >= retry.max_retries {
                        return (Err(error), retriable);
                    }
                    tokio::time::sleep(retry.delay_for(attempt)).await;
                    attempt += 1;
                }
            }
        }
    }

    /// 总成本/用量快照：聚合主链与 fallback（各 Provider 自记）。
    fn aggregate_usage(&self) -> TokenUsage {
        let mut total = TokenUsage::default();
        for provider in self.providers() {
            total.add(&provider.usage_snapshot());
        }
        total
    }
}

#[async_trait]
impl ModelProvider for ResilientProvider {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.complete_with_model(None, messages, tools).await
    }

    async fn complete_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        if !self.breaker.allow_request() {
            return Err(format!(
                "模型网关熔断器打开（连续失败 {} 次），请稍后重试",
                self.breaker.consecutive_failures()
            ));
        }
        let mut errors: Vec<String> = Vec::new();
        for provider in self.providers() {
            let (result, retriable) =
                Self::call_with_retry(&provider, model, messages, tools, &self.retry).await;
            match result {
                Ok(output) => {
                    self.breaker.record_success();
                    return Ok(output);
                }
                Err(error) => {
                    errors.push(error);
                    // 不可重试错误（预算/出境/解析）不降级到下一 Provider。
                    if !retriable {
                        break;
                    }
                }
            }
        }
        self.breaker.record_failure();
        Err(format!("模型网关全部失败：{}", errors.join("；")))
    }

    async fn complete_with_model_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ObservedModelOutput, String> {
        if !self.breaker.allow_request() {
            return Err(format!(
                "模型网关熔断器打开（连续失败 {} 次），请稍后重试",
                self.breaker.consecutive_failures()
            ));
        }
        let mut errors = Vec::new();
        for provider in self.providers() {
            let mut attempt = 0;
            loop {
                match provider
                    .complete_with_model_observed(model, messages, tools)
                    .await
                {
                    Ok(observed) => {
                        self.breaker.record_success();
                        return Ok(observed);
                    }
                    Err(error) => {
                        if !is_retriable(&error, &self.retry) || attempt >= self.retry.max_retries {
                            errors.push(error);
                            break;
                        }
                        tokio::time::sleep(self.retry.delay_for(attempt)).await;
                        attempt += 1;
                    }
                }
            }
        }
        self.breaker.record_failure();
        Err(format!("模型网关全部失败：{}", errors.join("；")))
    }

    async fn complete_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        self.complete_stream_with_model(None, messages, tools, on_delta)
            .await
    }

    async fn complete_stream_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        if !self.breaker.allow_request() {
            return Err(format!(
                "模型网关熔断器打开（连续失败 {} 次），请稍后重试",
                self.breaker.consecutive_failures()
            ));
        }
        let mut errors: Vec<String> = Vec::new();
        let mut retriable_seen = false;
        for provider in self.providers() {
            // §4.2 真流式（F-02 修复）：增量**立即**回调给调用方，不再"缓存整条成功后回放"。
            // 约束：一旦已有增量发出，重试或降级都会产生重复内容，因此此时只失败、
            // 不重试也不降级（显式报错，绝不静默重复）。未产生任何增量时保持原有重试/降级。
            let mut attempt = 0;
            let outcome = loop {
                let mut emitted = false;
                let result = {
                    let mut forward = |delta: String| {
                        emitted = true;
                        on_delta(delta);
                    };
                    provider
                        .complete_stream_with_model(model, messages, tools, &mut forward)
                        .await
                };
                match result {
                    Ok(output) => {
                        self.breaker.record_success();
                        return Ok(output);
                    }
                    Err(error) => {
                        if emitted {
                            // 已输出部分内容：不重试、不降级，避免重复。
                            break (error, false, true);
                        }
                        let retriable = is_retriable(&error, &self.retry);
                        if !retriable || attempt >= self.retry.max_retries {
                            break (error, retriable, false);
                        }
                        tokio::time::sleep(self.retry.delay_for(attempt)).await;
                        attempt += 1;
                    }
                }
            };
            let (error, retriable, partial) = outcome;
            if partial {
                errors.push(format!(
                    "{error}（流式中断：已输出部分内容，不再重试以免重复）"
                ));
                break;
            }
            errors.push(error);
            retriable_seen = retriable_seen || retriable;
            if !retriable {
                break;
            }
        }
        let _ = retriable_seen;
        self.breaker.record_failure();
        Err(format!("模型网关全部失败：{}", errors.join("；")))
    }

    /// 思考通道流式（无模型覆盖）：保留请求观测与重试语义。
    async fn complete_stream_with_reasoning(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.complete_stream_with_reasoning_and_model(None, messages, tools, on_chunk)
            .await
    }

    /// 思考通道流式（带模型覆盖）：回合主循环使用的观测入口。
    async fn complete_stream_with_reasoning_and_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.complete_stream_with_reasoning_and_model_observed(model, messages, tools, on_chunk)
            .await
            .map(|observed| observed.output)
    }

    async fn complete_stream_with_reasoning_and_model_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ObservedModelOutput, String> {
        if !self.breaker.allow_request() {
            return Err(format!(
                "模型网关熔断器打开（连续失败 {} 次），请稍后重试",
                self.breaker.consecutive_failures()
            ));
        }
        let mut errors = Vec::new();
        for provider in self.providers() {
            let mut attempt = 0;
            loop {
                // 正文增量会进入用户可见回答，一旦发出就绝不重试（避免重复）；
                // 思考增量只是临时通道（不进最终回答、不持久化），仅思考已发出时
                // 允许重试/降级——真实模型长思考流常在上游 ConnectionReset 后整轮失败，
                // 此前的单一 emitted 布尔把这类可恢复场景也一并判死。
                let mut emitted_content = false;
                let result = {
                    let mut forward = |chunk: StreamChunk| {
                        if matches!(chunk, StreamChunk::Content(_)) {
                            emitted_content = true;
                        }
                        on_chunk(chunk);
                    };
                    provider
                        .complete_stream_with_reasoning_and_model_observed(
                            model,
                            messages,
                            tools,
                            &mut forward,
                        )
                        .await
                };
                match result {
                    Ok(observed) => {
                        self.breaker.record_success();
                        return Ok(observed);
                    }
                    Err(error) => {
                        if emitted_content {
                            self.breaker.record_failure();
                            return Err(format!(
                                "{error}（流式中断：已输出正文内容，不再重试以免重复）"
                            ));
                        }
                        if !is_retriable(&error, &self.retry) || attempt >= self.retry.max_retries {
                            errors.push(error);
                            break;
                        }
                        tokio::time::sleep(self.retry.delay_for(attempt)).await;
                        attempt += 1;
                    }
                }
            }
        }
        self.breaker.record_failure();
        Err(format!("模型网关全部失败：{}", errors.join("；")))
    }

    fn usage_snapshot(&self) -> TokenUsage {
        self.aggregate_usage()
    }
}

/// 是否要求 Anthropic 原生通道（`OWO_PROVIDER=anthropic`，大小写不敏感）。
fn wants_anthropic() -> bool {
    std::env::var("OWO_PROVIDER")
        .map(|value| value.trim().eq_ignore_ascii_case("anthropic"))
        .unwrap_or(false)
}

/// 未配置时的稳定错误面（R3-B 契约：`provider/not_configured` + 可操作中文指引；
/// 与 [`super::UnconfiguredModelProvider`] 的用户文案一致）。
fn provider_not_configured(error: String) -> String {
    format!(
        "{}：模型提供商未配置（{error}）。请在设置中选择云端或本地 Ollama，或经环境变量 OPENAI_API_KEY 配置凭据",
        super::UnconfiguredModelProvider::CODE
    )
}

/// 是否已有可用模型配置（诊断/首启门；只读配置，不发起网络）。
///
/// A1-1：`OWO_PROVIDER=anthropic` 时看 `ANTHROPIC_*`（`ANTHROPIC_API_KEY`），
/// 否则看 OpenAI-compatible（`OPENAI_API_KEY` / 本地端点）。
pub fn provider_ready() -> bool {
    if wants_anthropic() {
        return crate::anthropic::AnthropicConfig::from_env().is_ok();
    }
    OpenAiCompatibleConfig::from_env().is_ok()
}

/// 延迟解析模型 provider（取优合并自远端 engine）：每次调用前重读环境配置，
/// 以「provider 种类 + 端点/密钥/模型」指纹缓存——配置任一变化即重建实例。
///
/// 存在的理由有两条：
/// 1. **首启门不能把服务卡死**——未配置时服务仍要能起来，让设置页可访问、可填写；
///    调用点才返回稳定码 `provider/not_configured`（R3-B：core ready，模型调用给引导）。
/// 2. **保存后即时生效**——设置页写入 `OPENAI_MODEL` 等环境配置后，下一个回合就用新
///    模型，无需重启（本仓库红线：凭据只来自环境变量，不在设置存储里落盘密钥）。
pub struct DeferredProvider {
    cached: std::sync::Mutex<Option<(String, Arc<dyn ModelProvider>)>>,
}

impl Default for DeferredProvider {
    fn default() -> Self {
        Self {
            cached: std::sync::Mutex::new(None),
        }
    }
}

impl DeferredProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取当前配置对应的 provider；配置指纹变化则重建（设置保存后自动换新）。
    fn resolve(&self) -> Result<Arc<dyn ModelProvider>, String> {
        // 指纹 = provider 种类 + 配置摘要：种类或端点/密钥/模型任一变化即重建。
        let (fingerprint, provider): (String, Arc<dyn ModelProvider>) = if wants_anthropic() {
            let config =
                crate::anthropic::AnthropicConfig::from_env().map_err(provider_not_configured)?;
            let fingerprint = format!(
                "anthropic|{}|{}|{}|{}",
                config.base_url, config.api_key, config.model, config.cloud_enabled
            );
            let provider: Arc<dyn ModelProvider> =
                Arc::new(crate::anthropic::AnthropicProvider::new(config)?);
            (fingerprint, provider)
        } else {
            let config = OpenAiCompatibleConfig::from_env().map_err(provider_not_configured)?;
            let fingerprint = format!(
                "openai|{}|{}|{}|{}",
                config.base_url, config.api_key, config.model, config.cloud_enabled
            );
            let provider: Arc<dyn ModelProvider> = Arc::new(OpenAiCompatibleProvider::new(config)?);
            (fingerprint, provider)
        };
        let mut slot = self
            .cached
            .lock()
            .map_err(|_| "provider 缓存锁中毒".to_string())?;
        if let Some((cached_fingerprint, provider)) = slot.as_ref() {
            if *cached_fingerprint == fingerprint {
                return Ok(Arc::clone(provider));
            }
        }
        *slot = Some((fingerprint, Arc::clone(&provider)));
        Ok(provider)
    }
}

#[async_trait]
impl ModelProvider for DeferredProvider {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.resolve()?.complete(messages, tools).await
    }

    async fn complete_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.resolve()?
            .complete_with_model(model, messages, tools)
            .await
    }

    async fn complete_with_model_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ObservedModelOutput, String> {
        self.resolve()?
            .complete_with_model_observed(model, messages, tools)
            .await
    }

    async fn complete_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        self.resolve()?
            .complete_stream(messages, tools, on_delta)
            .await
    }

    async fn complete_stream_with_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        self.resolve()?
            .complete_stream_with_model(model, messages, tools, on_delta)
            .await
    }

    async fn complete_stream_with_reasoning(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.resolve()?
            .complete_stream_with_reasoning(messages, tools, on_chunk)
            .await
    }

    async fn complete_stream_with_reasoning_and_model(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ModelOutput, String> {
        self.resolve()?
            .complete_stream_with_reasoning_and_model(model, messages, tools, on_chunk)
            .await
    }

    async fn complete_stream_with_reasoning_and_model_observed(
        &self,
        model: Option<&str>,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
        on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ObservedModelOutput, String> {
        self.resolve()?
            .complete_stream_with_reasoning_and_model_observed(model, messages, tools, on_chunk)
            .await
    }

    /// 转发真实 provider 的用量累计（否则外层 ResilientProvider 聚合到零值，
    /// 回合汇报卡的 token 消耗会一直缺失）。
    fn usage_snapshot(&self) -> TokenUsage {
        self.resolve()
            .map(|provider| provider.usage_snapshot())
            .unwrap_or_default()
    }
}
