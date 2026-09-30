use super::provider::*;
use super::resilience::*;
use super::stream::*;
use super::*;
use crate::tools::ToolSpec;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

/// 环境变量依赖的网关测试串行执行，避免并行设置互相干扰。
static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[test]
fn parses_content_delta() {
    let delta = parse_sse_payload(r#"{"choices":[{"delta":{"content":"你好"}}]}"#).unwrap();
    assert_eq!(delta.content.as_deref(), Some("你好"));
    assert!(delta.tool_call_fragments.is_empty());
}

#[test]
fn parses_tool_call_fragments_and_assembles() {
    let delta = parse_sse_payload(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}"#,
        )
        .unwrap();
    assert_eq!(delta.content, None);
    assert_eq!(delta.tool_call_fragments.len(), 1);

    let mut accumulators = HashMap::new();
    accumulate_tool_fragments(&mut accumulators, &delta.tool_call_fragments);
    let delta2 = parse_sse_payload(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}"#,
        )
        .unwrap();
    accumulate_tool_fragments(&mut accumulators, &delta2.tool_call_fragments);

    let calls = build_tool_calls(&mut accumulators).unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read_file");
    assert_eq!(calls[0].arguments["path"], "a.txt");
}

#[test]
fn ignores_heartbeat_and_done() {
    assert!(parse_sse_payload("").is_none());
    assert!(parse_sse_payload("[DONE]").is_none());
    assert!(parse_sse_payload(": keep-alive").is_none());
}

#[test]
fn parses_usage_value_for_openai_and_ollama_fields() {
    let openai = parse_usage_value(&json!({
        "prompt_tokens": 120,
        "completion_tokens": 30,
        "total_tokens": 150,
    }));
    assert_eq!(openai.prompt_tokens, 120);
    assert_eq!(openai.completion_tokens, 30);
    assert_eq!(openai.total_tokens, 150);

    // Ollama 原生字段名兼容。
    let ollama = parse_usage_value(&json!({
        "prompt_eval_count": 40,
        "eval_count": 12,
    }));
    assert_eq!(ollama.prompt_tokens, 40);
    assert_eq!(ollama.completion_tokens, 12);
    assert_eq!(ollama.total_tokens, 52);

    assert_eq!(parse_usage_value(&Value::Null), TokenUsage::default());
}

#[test]
fn token_usage_arithmetic_and_cost_estimate() {
    let mut usage = TokenUsage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
    };
    usage.add(&TokenUsage {
        prompt_tokens: 200,
        completion_tokens: 30,
        total_tokens: 230,
    });
    assert_eq!(usage.total_tokens, 380);

    let before = TokenUsage {
        prompt_tokens: 300,
        completion_tokens: 80,
        total_tokens: 380,
    };
    let delta = usage.saturating_sub(&before);
    assert_eq!(delta.total_tokens, 0);

    let delta = before.saturating_sub(&TokenUsage::default());
    assert_eq!(delta.prompt_tokens, 300);
    assert!((delta.cost_estimate_usd(2.0, 8.0) - 0.00124).abs() < 1e-9);
}

#[test]
fn budget_violation_blocks_when_caps_exceeded() {
    let usage = TokenUsage {
        prompt_tokens: 900,
        completion_tokens: 200,
        total_tokens: 1100,
    };
    assert!(budget_violation(&usage, None, None, 0.0, 0.0).is_none());
    assert!(budget_violation(&usage, Some(2000), None, 0.0, 0.0).is_none());
    let violation = budget_violation(&usage, Some(1000), None, 0.0, 0.0).expect("token 超限应熔断");
    assert!(violation.contains("用量预算"));
    assert!(violation.contains("1100"));

    let cost = budget_violation(&usage, None, Some(0.001), 2.0, 8.0).expect("成本超限应熔断");
    assert!(cost.contains("成本预算"));

    // 未到成本上限不熔断：0.0006+0.0016=0.0022 < 0.01。
    assert!(budget_violation(&usage, None, Some(0.01), 0.5, 2.0).is_none());
}

#[test]
fn parse_sse_payload_extracts_trailing_usage_block() {
    let payload = r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let delta = parse_sse_payload(payload).expect("usage 块应返回 Some");
    assert_eq!(delta.content, None);
    let usage = delta.usage.expect("usage 应被解析");
    assert_eq!(usage.total_tokens, 15);

    // 无 usage 的空 delta 仍按心跳忽略。
    assert!(parse_sse_payload(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).is_none());
}

/// R3-B（§3.4）：占位 Provider 的调用面必须携带稳定码 provider/not_configured
/// （UI/矩阵按码断言，不按中文文案）。
#[tokio::test]
async fn unconfigured_provider_returns_stable_code() {
    let provider = UnconfiguredModelProvider::new("缺少 OPENAI_API_KEY 环境变量");
    let error = provider
        .complete(&[ChatMessage::user("hi".to_string())], &[])
        .await
        .expect_err("无凭据调用必须失败，不得静默返回空文本");
    assert!(
        error.starts_with(UnconfiguredModelProvider::CODE),
        "错误必须以稳定码开头：{error}"
    );
    assert!(
        error.contains("OPENAI_API_KEY"),
        "错误必须给出可操作指引：{error}"
    );
}

#[tokio::test]
async fn cloud_disabled_rejects_requests_before_network() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENAI_API_KEY", "test");
    std::env::set_var("OPENAI_BASE_URL", "https://api.example.com/v1");
    std::env::set_var("OPENAI_MODEL", "mock");
    std::env::set_var("OWO_CLOUD_ENABLED", "false");
    let config = OpenAiCompatibleConfig::from_env().unwrap();
    assert!(!config.cloud_enabled);
    let provider = OpenAiCompatibleProvider::new(config).unwrap();
    let error = provider.complete(&[], &[]).await.unwrap_err();
    assert!(error.contains("数据出境"));
    std::env::remove_var("OWO_CLOUD_ENABLED");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("OPENAI_BASE_URL");
    std::env::remove_var("OPENAI_MODEL");
}

#[tokio::test]
async fn cloud_switch_applies_without_reconstruction() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENAI_API_KEY", "test");
    std::env::set_var("OPENAI_BASE_URL", "https://api.example.com/v1");
    std::env::set_var("OPENAI_MODEL", "mock");
    std::env::remove_var("OWO_CLOUD_ENABLED");
    let config = OpenAiCompatibleConfig::from_env().unwrap();
    assert!(config.cloud_enabled);
    let provider = OpenAiCompatibleProvider::new(config).unwrap();
    assert!(provider.cloud_enabled());
    std::env::set_var("OWO_CLOUD_ENABLED", "false");
    let error = provider.complete(&[], &[]).await.unwrap_err();
    assert!(error.contains("数据出境"));
    std::env::remove_var("OWO_CLOUD_ENABLED");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("OPENAI_BASE_URL");
    std::env::remove_var("OPENAI_MODEL");
}

#[tokio::test]
async fn local_endpoint_does_not_require_key_or_cloud_switch() {
    let _guard = ENV_LOCK.lock().await;
    std::env::remove_var("OPENAI_API_KEY");
    std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:11434/v1");
    std::env::set_var("OWO_CLOUD_ENABLED", "false");

    let config = OpenAiCompatibleConfig::from_env().unwrap();
    assert!(config.api_key.is_empty());
    let provider = OpenAiCompatibleProvider::new(config).unwrap();
    assert!(provider.cloud_enabled());

    std::env::remove_var("OWO_CLOUD_ENABLED");
    std::env::remove_var("OPENAI_BASE_URL");
}

#[test]
fn stream_request_includes_usage_option() {
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        base_url: "http://127.0.0.1:11434/v1".to_string(),
        api_key: String::new(),
        model: "local".to_string(),
        cloud_enabled: false,
    })
    .unwrap();
    let body = provider.request_body(None, &[], &[], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
}

#[test]
fn utf8_chunks_are_reassembled_without_replacement_characters() {
    let mut buffer = String::new();
    let mut pending = Vec::new();
    let bytes = "中".as_bytes();
    append_utf8_chunk(&mut buffer, &mut pending, &bytes[..1]);
    append_utf8_chunk(&mut buffer, &mut pending, &bytes[1..]);
    assert_eq!(buffer, "中");
    assert!(pending.is_empty());
}

#[tokio::test]
async fn model_switch_applies_without_reconstruction() {
    let _guard = ENV_LOCK.lock().await;
    std::env::set_var("OPENAI_API_KEY", "test");
    std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:9");
    std::env::set_var("OPENAI_MODEL", "model-a");
    std::env::remove_var("OWO_CLOUD_ENABLED");
    let config = OpenAiCompatibleConfig::from_env().unwrap();
    let provider = OpenAiCompatibleProvider::new(config).unwrap();
    let body = provider.request_body(None, &[], &[], false);
    assert_eq!(body["model"], "model-a");
    std::env::set_var("OPENAI_MODEL", "model-b");
    let body = provider.request_body(None, &[], &[], false);
    assert_eq!(body["model"], "model-b");
    std::env::set_var("OPENAI_MODEL", "");
    let body = provider.request_body(None, &[], &[], false);
    assert_eq!(body["model"], "model-a");
    std::env::remove_var("OWO_CLOUD_ENABLED");
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("OPENAI_BASE_URL");
    std::env::remove_var("OPENAI_MODEL");
}

/// M4.2 会话级路由：显式覆盖进请求体；空串/`"default"` 哨兵回退解析链且
/// 哨兵值绝不泄漏进请求体 `model` 字段。
#[test]
fn request_body_model_override_wins_and_sentinel_never_leaks() {
    let _guard = ENV_LOCK.blocking_lock();
    std::env::set_var("OPENAI_MODEL", "chain-model");
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        base_url: "http://127.0.0.1:11434/v1".to_string(),
        api_key: String::new(),
        model: "config-model".to_string(),
        cloud_enabled: false,
    })
    .unwrap();
    assert_eq!(
        provider.request_body(Some("vision-x"), &[], &[], false)["model"],
        "vision-x"
    );
    assert_eq!(
        provider.request_body(Some("  padded  "), &[], &[], false)["model"],
        "padded"
    );
    assert_eq!(
        provider.request_body(Some(MODEL_DEFAULT_SENTINEL), &[], &[], false)["model"],
        "chain-model"
    );
    assert_eq!(
        provider.request_body(Some("   "), &[], &[], false)["model"],
        "chain-model"
    );
    assert_eq!(
        provider.request_body(None, &[], &[], false)["model"],
        "chain-model"
    );
    std::env::remove_var("OPENAI_MODEL");
}

/// M4.2 档位路由：Fast/Vision 从 env 解析（含 trim 与空白视为未配置）；
/// Vision 兼容视觉通道既有变量 `OWO_VISION_MODEL`；Main 恒走主链。
#[test]
fn tier_resolver_reads_env_with_vision_alias_fallback() {
    let _guard = ENV_LOCK.blocking_lock();
    for name in ["OWO_MODEL_FAST", "OWO_MODEL_VISION", "OWO_VISION_MODEL"] {
        std::env::remove_var(name);
    }
    assert_eq!(resolve_tier_model(ModelTier::Main), None);
    assert_eq!(resolve_tier_model(ModelTier::Fast), None);
    assert_eq!(resolve_tier_model(ModelTier::Vision), None);
    std::env::set_var("OWO_MODEL_FAST", " fast-m ");
    std::env::set_var("OWO_MODEL_VISION", "vision-m");
    std::env::set_var("OWO_VISION_MODEL", "shadowed");
    assert_eq!(
        resolve_tier_model(ModelTier::Fast).as_deref(),
        Some("fast-m")
    );
    assert_eq!(
        resolve_tier_model(ModelTier::Vision).as_deref(),
        Some("vision-m")
    );
    std::env::remove_var("OWO_MODEL_VISION");
    assert_eq!(
        resolve_tier_model(ModelTier::Vision).as_deref(),
        Some("shadowed")
    );
    std::env::set_var("OWO_MODEL_FAST", "   ");
    assert_eq!(resolve_tier_model(ModelTier::Fast), None);
    for name in ["OWO_MODEL_FAST", "OWO_VISION_MODEL"] {
        std::env::remove_var(name);
    }
    assert_eq!(ModelTier::parse(" FAST "), Some(ModelTier::Fast));
    assert_eq!(ModelTier::parse("nope"), None);
    assert_eq!(ModelTier::Vision.as_str(), "vision");
}

/// M4.2：ResilientProvider 主链/failover 全链透传请求级模型覆盖。
#[tokio::test]
async fn resilient_chain_forwards_model_override() {
    struct Recording {
        seen: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl ModelProvider for Recording {
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
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            self.seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(model.map(str::to_string));
            Ok(ModelOutput::Text("ok".to_string()))
        }
    }
    let primary = Arc::new(Recording {
        seen: StdMutex::new(Vec::new()),
    });
    let fallback = Arc::new(Recording {
        seen: StdMutex::new(Vec::new()),
    });
    let resilient = ResilientProvider::new(
        Arc::clone(&primary) as Arc<dyn ModelProvider>,
        vec![Arc::clone(&fallback) as Arc<dyn ModelProvider>],
        CircuitBreaker::from_env(),
        RetryPolicy::from_env(),
    );
    resilient
        .complete_with_model(Some("fast-x"), &[], &[])
        .await
        .expect("主链应成功");
    resilient.complete(&[], &[]).await.expect("无覆盖也应成功");
    let seen = primary
        .seen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(seen.as_slice(), &[Some("fast-x".to_string()), None]);
    // 主链成功时 fallback 不应被调用。
    assert!(fallback.seen.lock().unwrap().is_empty());
}

/// 真机门控（M4.1 + M4.2 wire 级）：显式覆盖必须到达真实端点请求体。
///
/// canary 负证法：用不存在的模型名做请求级覆盖，期望端点报错；同时基准链
/// （同端点、无覆盖）必须成功。若实现忽略覆盖，canary 调用会静默走默认模型
/// 并成功 → 本测试必红。凭据只经 `OPENAI_API_KEY` 环境变量注入（红线）。
///
/// ```text
/// $env:OPENAI_API_KEY = (用户级注入)
/// cargo test -p owo-agent-core --lib -- gateway::tests::live_model_override -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "真实端点：需要 OPENAI_API_KEY，显式 --ignored 运行"]
async fn live_model_override_reaches_endpoint() {
    let _guard = ENV_LOCK.lock().await;
    if std::env::var("OPENAI_API_KEY")
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        panic!("live 门控需要 OPENAI_API_KEY 环境变量（凭据仅经环境注入）");
    }
    std::env::remove_var("OPENAI_MODEL");
    std::env::remove_var("OWO_CLOUD_ENABLED");
    let config = OpenAiCompatibleConfig {
        base_url: DEFAULT_MODEL_BASE_URL.to_string(),
        api_key: std::env::var("OPENAI_API_KEY").unwrap(),
        model: DEFAULT_MODEL_ID.to_string(),
        cloud_enabled: true,
    };
    let provider = OpenAiCompatibleProvider::new(config).unwrap();
    // ① 基准：无覆盖走链上默认模型，必须成功（证明端点/凭据可用）。
    let baseline = provider
        .complete(&[ChatMessage::user("只回复两个字：正常".to_string())], &[])
        .await
        .expect("基准链应成功（端点与凭据可用）");
    assert!(
        matches!(baseline, ModelOutput::Text(ref text) if !text.trim().is_empty()),
        "基准链应返回非空文本：{baseline:?}"
    );
    // ② canary：不存在的模型名做请求级覆盖——覆盖若未进请求体，这次调用会
    // 静默落到默认模型并成功（假绿），因此报错即是路由生效的证明。
    let canary = provider
        .complete_with_model(
            Some("owo-routing-canary-not-exist"),
            &[ChatMessage::user("ping".to_string())],
            &[],
        )
        .await;
    assert!(
        canary.is_err(),
        "canary 模型必须因请求体携带它而失败；实际成功＝覆盖未进 wire：{canary:?}"
    );
    // ③ "default" 哨兵：不得作为模型名发出去，必须回退链上默认（成功）。
    let sentinel = provider
        .complete_with_model(
            Some(MODEL_DEFAULT_SENTINEL),
            &[ChatMessage::user("只回复两个字：正常".to_string())],
            &[],
        )
        .await
        .expect("哨兵必须回退默认链，不得把 \"default\" 发给端点");
    assert!(matches!(sentinel, ModelOutput::Text(_)));
}

#[test]
fn provider_creates_direct_client_when_proxy_configured() {
    let _guard = ENV_LOCK.blocking_lock();
    let proxy_envs = [
        "OWO_HTTP_PROXY",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "https_proxy",
        "http_proxy",
    ];
    let previous: Vec<_> = proxy_envs
        .iter()
        .map(|name| (*name, std::env::var(name).ok()))
        .collect();
    for name in proxy_envs {
        std::env::remove_var(name);
    }
    std::env::set_var("OWO_HTTP_PROXY", "http://127.0.0.1:9");
    let config = OpenAiCompatibleConfig {
        base_url: "http://127.0.0.1:9/v1".to_string(),
        api_key: "test".to_string(),
        model: "test".to_string(),
        cloud_enabled: true,
    };
    let provider = OpenAiCompatibleProvider::new(config).expect("客户端创建成功");
    assert!(provider.direct_client.is_some());
    std::env::remove_var("OWO_HTTP_PROXY");
    let config = OpenAiCompatibleConfig {
        base_url: "http://127.0.0.1:9/v1".to_string(),
        api_key: "test".to_string(),
        model: "test".to_string(),
        cloud_enabled: true,
    };
    let provider = OpenAiCompatibleProvider::new(config).expect("客户端创建成功");
    assert!(provider.direct_client.is_none());
    for (name, value) in previous {
        if let Some(value) = value {
            std::env::set_var(name, value);
        }
    }
}

// ---- §4.2 F-02 真流式契约（回归守卫） --------------------------------------------

/// 可编程流式 Provider：可"先吐增量再失败"，用于证明增量是**立即**转发而非整条缓存回放。
struct StreamingMock {
    deltas: Vec<String>,
    /// 前 N 次调用直接失败（不产生任何增量）。
    fail_first: usize,
    /// 产生增量后是否失败（用于验证"已发增量后不再重试"）。
    fail_after_emit: bool,
    calls: StdMutex<usize>,
}

#[async_trait]
impl ModelProvider for StreamingMock {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.complete_with_model(None, messages, tools).await
    }
    async fn complete_with_model(
        &self,
        _model: Option<&str>,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        Ok(ModelOutput::Text("ok".to_string()))
    }
    async fn complete_stream_with_model(
        &self,
        _model: Option<&str>,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
        on_delta: &mut (dyn FnMut(String) + Send),
    ) -> Result<ModelOutput, String> {
        let call = {
            let mut calls = self.calls.lock().unwrap_or_else(|p| p.into_inner());
            *calls += 1;
            *calls
        };
        if call <= self.fail_first {
            // 连接类错误 → 可重试（见 is_retriable）。
            return Err("模型请求失败：连接被重置".to_string());
        }
        for delta in &self.deltas {
            on_delta(delta.clone());
        }
        if self.fail_after_emit {
            return Err("模型请求失败：连接被重置".to_string());
        }
        Ok(ModelOutput::Text(self.deltas.concat()))
    }
}

fn fast_retry(max_retries: usize) -> RetryPolicy {
    RetryPolicy {
        max_retries,
        base_delay_ms: 1,
        max_delay_ms: 1,
        retry_429: true,
        retry_network: true,
    }
}

/// F-02 回归：Provider 吐了增量后失败——增量必须已经**立即**到达调用方，
/// 且因"已输出"而不再重试（否则重试会重复内容）。旧实现（整条缓存成功后回放）
/// 会得到 0 个增量，本测试必红。
#[tokio::test]
async fn resilient_streams_deltas_immediately_and_does_not_retry_after_emit() {
    let mock = Arc::new(StreamingMock {
        deltas: vec!["你".to_string(), "好".to_string()],
        fail_first: 0,
        fail_after_emit: true,
        calls: StdMutex::new(0),
    });
    let resilient = ResilientProvider::new(
        Arc::clone(&mock) as Arc<dyn ModelProvider>,
        Vec::new(),
        CircuitBreaker::default(),
        fast_retry(3),
    );
    let mut got = Vec::new();
    let result = resilient
        .complete_stream(&[], &[], &mut |delta| got.push(delta))
        .await;
    assert!(result.is_err(), "流式中断必须显式返回错误");
    assert_eq!(
        got,
        vec!["你".to_string(), "好".to_string()],
        "增量必须立即转发（真流式），而非整条缓存后回放"
    );
    assert_eq!(
        *mock.calls.lock().unwrap_or_else(|p| p.into_inner()),
        1,
        "已输出增量后不得重试（否则重复内容）"
    );
}

/// 未产生任何增量时仍保留重试：第一次连接失败、第二次成功吐增量。
#[tokio::test]
async fn resilient_still_retries_before_any_delta() {
    let mock = Arc::new(StreamingMock {
        deltas: vec!["A".to_string()],
        fail_first: 1,
        fail_after_emit: false,
        calls: StdMutex::new(0),
    });
    let resilient = ResilientProvider::new(
        Arc::clone(&mock) as Arc<dyn ModelProvider>,
        Vec::new(),
        CircuitBreaker::default(),
        fast_retry(2),
    );
    let mut got = Vec::new();
    resilient
        .complete_stream(&[], &[], &mut |delta| got.push(delta))
        .await
        .expect("第二次应成功");
    assert_eq!(got, vec!["A".to_string()]);
    assert_eq!(
        *mock.calls.lock().unwrap_or_else(|p| p.into_inner()),
        2,
        "未产生增量前应重试一次"
    );
}
