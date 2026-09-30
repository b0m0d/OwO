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

/// 错误是否可重试：网络/5xx/429/空闲看门狗 → 可；预算/出境/解析/4xx → 不可。
pub(super) fn is_retriable(error: &str, policy: &RetryPolicy) -> bool {
    if error.contains("预算已超限") || error.contains("数据出境") {
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
    /// fallback = OWO_MODEL_FALLBACK_BASE_URLS（逗号分隔；本地端点无需 key）。
    pub fn from_env() -> Result<Self, String> {
        let config = OpenAiCompatibleConfig::from_env()?;
        Self::from_config(config)
    }

    /// 以显式主配置构造（CLI 接线用，主配置的 model/api_key 已确定）；
    /// fallback 仍读 OWO_MODEL_FALLBACK_BASE_URLS（同 model；本地端点无需 key）。
    pub fn from_config(config: OpenAiCompatibleConfig) -> Result<Self, String> {
        let primary = Arc::new(OpenAiCompatibleProvider::new(config.clone())?);
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

    fn usage_snapshot(&self) -> TokenUsage {
        self.aggregate_usage()
    }
}
