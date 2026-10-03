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

struct ObservedGatewayTestProvider;

#[async_trait]
impl ModelProvider for ObservedGatewayTestProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        Ok(ModelOutput::Text("ok".into()))
    }

    async fn complete_with_model_observed(
        &self,
        model: Option<&str>,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ObservedModelOutput, String> {
        Ok(ObservedModelOutput {
            output: ModelOutput::Text("ok".into()),
            metadata: ModelCallMetadata {
                request_id: Some("req-observed".into()),
                model: model.map(str::to_string),
                usage: Some(TokenUsage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    total_tokens: 10,
                }),
                ..Default::default()
            },
        })
    }

    async fn complete_stream_with_reasoning_and_model_observed(
        &self,
        _model: Option<&str>,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
        _on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
    ) -> Result<ObservedModelOutput, String> {
        Ok(ObservedModelOutput {
            output: ModelOutput::Text("ok".into()),
            metadata: ModelCallMetadata {
                request_id: Some("req-observed".into()),
                model: Some("model-observed".into()),
                usage: Some(TokenUsage {
                    prompt_tokens: 7,
                    completion_tokens: 3,
                    total_tokens: 10,
                }),
                ..Default::default()
            },
        })
    }
}

#[tokio::test]
async fn openai_provider_reports_non_stream_usage_request_and_model() {
    let _env_guard = ENV_LOCK.lock().await;
    let previous_proxy = std::env::var("OWO_HTTP_PROXY").ok();
    std::env::set_var("OWO_HTTP_PROXY", "http://127.0.0.1:1");
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 4096];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request)
            .await
            .unwrap();
        let payload = json!({
            "id": "response-id",
            "model": "served-model",
            "choices": [{"message": {"role": "assistant", "content": "fixed"}}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 4, "total_tokens": 13}
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nx-request-id: req-http-123\r\n\r\n{}",
            payload.len(),
            payload
        );
        tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
            .await
            .unwrap();
    });
    let provider_result = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        base_url: format!("http://{address}/v1"),
        api_key: "test-only".to_string(),
        model: "configured-model".to_string(),
        cloud_enabled: false,
    });
    match previous_proxy {
        Some(value) => std::env::set_var("OWO_HTTP_PROXY", value),
        None => std::env::remove_var("OWO_HTTP_PROXY"),
    }
    let provider = provider_result.unwrap();
    let observed = provider
        .complete_with_model_observed(
            Some("requested-model"),
            &[ChatMessage::user("repair output".into())],
            &[],
        )
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(
        observed.metadata.request_id.as_deref(),
        Some("req-http-123")
    );
    assert_eq!(observed.metadata.model.as_deref(), Some("served-model"));
    assert_eq!(observed.metadata.usage.unwrap().total_tokens, 13);
    assert_eq!(provider.usage_snapshot().total_tokens, 13);
}

#[tokio::test]
async fn resilient_provider_preserves_non_stream_request_metadata() {
    let provider = ResilientProvider::new(
        Arc::new(ObservedGatewayTestProvider),
        Vec::new(),
        CircuitBreaker::default(),
        RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        },
    );
    let observed = provider
        .complete_with_model_observed(
            Some("model-observed"),
            &[ChatMessage::user("repair".into())],
            &[],
        )
        .await
        .expect("request should succeed");
    assert_eq!(
        observed.metadata.request_id.as_deref(),
        Some("req-observed")
    );
    assert_eq!(observed.metadata.model.as_deref(), Some("model-observed"));
    assert_eq!(observed.metadata.usage.unwrap().total_tokens, 10);
}

#[tokio::test]
async fn resilient_provider_preserves_per_request_metadata() {
    let provider = ResilientProvider::new(
        Arc::new(ObservedGatewayTestProvider),
        Vec::new(),
        CircuitBreaker::default(),
        RetryPolicy {
            max_retries: 0,
            ..RetryPolicy::default()
        },
    );
    let observed = provider
        .complete_stream_with_reasoning_and_model_observed(
            Some("model-observed"),
            &[ChatMessage::user("hello".into())],
            &[],
            &mut |_| {},
        )
        .await
        .expect("request should succeed");
    assert_eq!(
        observed.metadata.request_id.as_deref(),
        Some("req-observed")
    );
    assert_eq!(observed.metadata.model.as_deref(), Some("model-observed"));
    assert_eq!(observed.metadata.usage.unwrap().total_tokens, 10);
}

/// 环境变量依赖的网关测试串行执行，避免并行设置互相干扰。
static ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[test]
fn parses_stream_finish_reason() {
    let delta = parse_sse_payload(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#)
        .expect("终止帧应被解析");
    assert_eq!(delta.finish_reason.as_deref(), Some("length"));
}

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
    let payload = r#"{"id":"req-123","model":"model-real","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let delta = parse_sse_payload(payload).expect("usage 块应返回 Some");
    assert_eq!(delta.content, None);
    assert_eq!(delta.request_id.as_deref(), Some("req-123"));
    assert_eq!(delta.model.as_deref(), Some("model-real"));
    let usage = delta.usage.expect("usage 应被解析");
    assert_eq!(usage.total_tokens, 15);

    // OpenAI-compatible usage-only 帧可带空 choices；usage 仍须进入账本。
    let usage_only = parse_sse_payload(
        r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}"#,
    )
    .expect("空 choices 的 usage 尾帧不能丢失");
    assert_eq!(usage_only.usage.expect("usage").total_tokens, 10);

    // 终止帧没有正文时仍须保留 finish_reason，供网关判断是否被截断。
    let terminal = parse_sse_payload(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
        .expect("终止帧应被保留");
    assert_eq!(terminal.finish_reason.as_deref(), Some("stop"));
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
    assert!(
        provider.direct_client.is_none(),
        "loopback 模型端点应绕过 HTTP 代理"
    );
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

struct AlwaysFailProvider {
    error: String,
    calls: StdMutex<usize>,
}

#[async_trait]
impl ModelProvider for AlwaysFailProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        *self
            .calls
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) += 1;
        Err(self.error.clone())
    }
}

#[tokio::test]
async fn permanent_resource_exhaustion_is_not_retried_but_transient_429_policy_remains() {
    let policy = fast_retry(3);
    assert!(!is_retriable(
        r#"模型返回 429: {"code":"1113","msg":"余额不足"}"#,
        &policy
    ));
    assert!(!is_retriable(
        r#"模型返回 429: {"code":1113,"msg":"no available resource package"}"#,
        &policy
    ));
    assert!(is_retriable(
        r#"模型返回 429: {"code":"1302","msg":"rate limited"}"#,
        &policy
    ));
    assert!(is_retriable("模型返回 429：invalid body", &policy));

    let provider = Arc::new(AlwaysFailProvider {
        error: r#"模型返回 429: {"code":1113}"#.to_string(),
        calls: StdMutex::new(0),
    });
    let fallback = Arc::new(AlwaysFailProvider {
        error: "fallback must not run for permanent resource exhaustion".to_string(),
        calls: StdMutex::new(0),
    });
    let gateway = ResilientProvider::new(
        Arc::clone(&provider) as Arc<dyn ModelProvider>,
        vec![Arc::clone(&fallback) as Arc<dyn ModelProvider>],
        CircuitBreaker::default(),
        policy,
    );
    assert!(gateway.complete(&[], &[]).await.is_err());
    assert_eq!(
        *provider
            .calls
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        1,
        "permanent provider resource exhaustion must issue one request, not retries"
    );
    assert_eq!(
        *fallback
            .calls
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
        0,
        "permanent provider resource exhaustion must not cascade to fallback providers"
    );
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

/// Agent 使用带 reasoning/model override 的流接口，网络失败在首个 chunk 前也要按策略重试。
#[tokio::test]
async fn resilient_reasoning_stream_retries_before_any_chunk() {
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
    let mut chunks = Vec::new();
    resilient
        .complete_stream_with_reasoning_and_model(Some("test-model"), &[], &[], &mut |chunk| {
            chunks.push(chunk)
        })
        .await
        .expect("首个 chunk 前的连接失败应重试成功");
    assert_eq!(chunks, vec![StreamChunk::Content("A".to_string())]);
    assert_eq!(
        *mock.calls.lock().unwrap_or_else(|p| p.into_inner()),
        2,
        "reasoning stream 应使用同一重试策略"
    );
}

/// 多模态（取优合并自远端 engine）：`user_with_images` 构造 + serde 往返 +
/// 老会话记录（缺 images 字段）向前兼容。
#[test]
fn user_with_images_roundtrip_and_legacy_compat() {
    use crate::gateway::{ChatMessage, MessageImage};
    let message = ChatMessage::user_with_images(
        "看看这张图".to_string(),
        vec![MessageImage::from_url("data:image/png;base64,AAAA")],
    );
    assert_eq!(message.images.len(), 1);
    let value = serde_json::to_value(&message).unwrap();
    assert_eq!(value["images"][0]["url"], "data:image/png;base64,AAAA");
    let back: ChatMessage = serde_json::from_value(value).unwrap();
    assert_eq!(back.images.len(), 1);

    // 旧记录没有 images 字段 → 缺省为空（向前兼容）。
    let legacy: ChatMessage =
        serde_json::from_value(serde_json::json!({ "role": "user", "content": "hi" })).unwrap();
    assert!(legacy.images.is_empty());
    // 无图片的消息序列化时不带 images 字段（不污染旧 wire 形状）。
    let plain = ChatMessage::user("hi".to_string());
    let plain_value = serde_json::to_value(&plain).unwrap();
    assert!(plain_value.get("images").is_none(), "{plain_value}");
}

/// 思考通道（取优合并自远端 engine）：`reasoning_content` 增量被解析且不污染正文。
#[test]
fn parse_sse_payload_reads_reasoning_channel() {
    use crate::gateway::stream::parse_sse_payload;
    let reasoning =
        parse_sse_payload(r#"{"choices":[{"delta":{"reasoning_content":"先想一步"}}]}"#)
            .expect("思考增量不应被当作心跳丢弃");
    assert_eq!(reasoning.reasoning.as_deref(), Some("先想一步"));
    assert!(reasoning.content.is_none());

    let content =
        parse_sse_payload(r#"{"choices":[{"delta":{"content":"答案"}}]}"#).expect("正文增量");
    assert_eq!(content.content.as_deref(), Some("答案"));
    assert!(content.reasoning.is_none());

    // 空 reasoning_content 不产生事件（与空正文同口径）。
    assert!(parse_sse_payload(r#"{"choices":[{"delta":{"reasoning_content":""}}]}"#).is_none());
}

/// 回归：**思考通道必须穿过 ResilientProvider**（serve 路径挂的就是这一层）。
///
/// 缺陷形态：`ModelProvider` 的默认 `complete_stream_with_reasoning_and_model`
/// 明确"忽略思考通道"（只把正文包一层转发），而回合主循环调的是思考版方法。
/// ResilientProvider 当时没重写它 → 推理增量在这一层被静默丢弃：
/// 明明挂了推理模型、请求也带上了 reasoning，UI 却永远看不到深度思考。
#[tokio::test]
async fn resilient_chain_forwards_reasoning_chunks() {
    struct ReasoningProvider {
        seen: StdMutex<Vec<Option<String>>>,
    }
    impl ReasoningProvider {
        /// 两个流式入口共用同一套发射逻辑——真实 provider（OpenAiCompatibleProvider /
        /// DeferredProvider）也是两版都实现的。**只实现其中一版是不合格的**：
        /// trait 的默认桥接会把推理增量降级成正文，这就是本缺陷的形态。
        fn emit_all(
            &self,
            model: Option<&str>,
            on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> Result<ModelOutput, String> {
            self.seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(model.map(str::to_string));
            on_chunk(StreamChunk::Reasoning("先想一步。".to_string()));
            on_chunk(StreamChunk::Content("答案".to_string()));
            Ok(ModelOutput::Text("答案".to_string()))
        }
    }
    #[async_trait]
    impl ModelProvider for ReasoningProvider {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            self.complete_stream_with_reasoning_and_model(None, messages, tools, &mut |_| {})
                .await
        }
        async fn complete_stream_with_reasoning(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> Result<ModelOutput, String> {
            self.emit_all(None, on_chunk)
        }
        async fn complete_stream_with_reasoning_and_model(
            &self,
            model: Option<&str>,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            on_chunk: &mut (dyn FnMut(StreamChunk) + Send),
        ) -> Result<ModelOutput, String> {
            self.emit_all(model, on_chunk)
        }
    }
    let primary = Arc::new(ReasoningProvider {
        seen: StdMutex::new(Vec::new()),
    });
    let resilient = ResilientProvider::new(
        Arc::clone(&primary) as Arc<dyn ModelProvider>,
        Vec::new(),
        CircuitBreaker::from_env(),
        RetryPolicy::from_env(),
    );

    let mut chunks: Vec<StreamChunk> = Vec::new();
    let output = resilient
        .complete_stream_with_reasoning_and_model(
            Some("deepseek-reasoner"),
            &[],
            &[],
            &mut |chunk| chunks.push(chunk),
        )
        .await
        .expect("思考版流式应成功");
    assert!(matches!(output, ModelOutput::Text(ref t) if t == "答案"));
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Reasoning(t) if t.contains("先想一步"))),
        "推理增量必须透传给调用方，实际收到：{chunks:?}"
    );
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, StreamChunk::Content(t) if t == "答案")),
        "正文增量必须照常透传，实际收到：{chunks:?}"
    );
    // 请求级模型覆盖同样要穿过这一层（会话切推理模型后必须真的生效）。
    assert_eq!(
        primary
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        &[Some("deepseek-reasoner".to_string())],
        "带覆盖的入口必须把模型覆盖传到内层 provider"
    );

    // 无覆盖入口同样走思考通道（不得退回默认实现丢推理）。
    let mut plain: Vec<StreamChunk> = Vec::new();
    resilient
        .complete_stream_with_reasoning(&[], &[], &mut |chunk| plain.push(chunk))
        .await
        .expect("无覆盖思考流式应成功");
    assert!(
        plain.iter().any(|c| matches!(c, StreamChunk::Reasoning(_))),
        "无覆盖入口也必须透出推理增量"
    );
    assert_eq!(
        primary
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        &[Some("deepseek-reasoner".to_string()), None],
        "无覆盖入口应传 None（由内层 provider 解析默认模型）"
    );
}

/// DeferredProvider（取优合并自远端 engine）：未配置时调用点返回稳定码
/// `provider/not_configured`，且 `provider_ready()` 为 false——core 仍可用。
#[tokio::test]
async fn deferred_provider_reports_not_configured_without_credentials() {
    let _guard = ENV_LOCK.lock().await;
    let saved: Vec<(&str, Option<String>)> = [
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_MODEL",
        "OWO_PROVIDER",
        "ANTHROPIC_API_KEY",
    ]
    .iter()
    .map(|key| (*key, std::env::var(key).ok()))
    .collect();
    std::env::remove_var("OPENAI_API_KEY");
    std::env::remove_var("OPENAI_BASE_URL");
    std::env::remove_var("OPENAI_MODEL");
    std::env::remove_var("OWO_PROVIDER");
    std::env::remove_var("ANTHROPIC_API_KEY");

    assert!(!provider_ready(), "无凭据时应报告未就绪");
    let provider = DeferredProvider::new();
    let error = provider
        .complete(&[ChatMessage::user("hi".to_string())], &[])
        .await
        .expect_err("未配置时调用必须显式报错");
    assert!(
        error.contains(UnconfiguredModelProvider::CODE),
        "错误面必须携带稳定码：{error}"
    );

    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

/// 真实 GLM 流式诊断：验证显式推理档位/输出预算下能收到完整 SSE 终止帧。
#[tokio::test]
#[ignore = "真实 GLM API 请求；显式 --ignored 运行"]
async fn live_glm_stream_finishes_with_bounded_reasoning() {
    let _guard = ENV_LOCK.lock().await;
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .expect("live 门控需要 OPENAI_API_KEY 环境变量");
    let saved_effort = std::env::var("OWO_REASONING_EFFORT").ok();
    let saved_max_tokens = std::env::var("OWO_MODEL_MAX_OUTPUT_TOKENS").ok();
    std::env::set_var("OWO_REASONING_EFFORT", "low");
    std::env::set_var("OWO_MODEL_MAX_OUTPUT_TOKENS", "32000");
    let config = OpenAiCompatibleConfig {
        base_url: std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_MODEL_BASE_URL.to_string()),
        api_key,
        model: DEFAULT_MODEL_ID.to_string(),
        cloud_enabled: true,
    };
    let provider = OpenAiCompatibleProvider::new(config).unwrap();
    let mut chunks = Vec::new();
    let result = provider
        .complete_stream_with_reasoning_and_model(
            Some("glm-5.3-flashx"),
            &[ChatMessage::user("只回复：stream-ok".to_string())],
            &[],
            &mut |chunk| chunks.push(chunk),
        )
        .await;
    if let Some(value) = saved_effort {
        std::env::set_var("OWO_REASONING_EFFORT", value);
    } else {
        std::env::remove_var("OWO_REASONING_EFFORT");
    }
    if let Some(value) = saved_max_tokens {
        std::env::set_var("OWO_MODEL_MAX_OUTPUT_TOKENS", value);
    } else {
        std::env::remove_var("OWO_MODEL_MAX_OUTPUT_TOKENS");
    }
    let output = result.expect("GLM SSE 应正常终止");
    assert!(matches!(output, ModelOutput::Text(ref text) if !text.trim().is_empty()));
    assert!(!chunks.is_empty(), "完整 SSE 应产生正文或推理增量");
}

/// DeferredProvider：配置就绪时报 ready，且未发网络请求即完成 provider 构造。
#[tokio::test]
async fn deferred_provider_ready_when_configured_and_reuses_instances() {
    let _guard = ENV_LOCK.lock().await;
    let saved: Vec<(&str, Option<String>)> = ["OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL"]
        .iter()
        .map(|key| (*key, std::env::var(key).ok()))
        .collect();
    std::env::set_var("OPENAI_API_KEY", "test-key");
    std::env::set_var("OPENAI_BASE_URL", "https://api.example.com/v1");
    std::env::set_var("OPENAI_MODEL", "model-a");
    std::env::remove_var("OWO_PROVIDER");
    std::env::remove_var("ANTHROPIC_API_KEY");

    assert!(provider_ready(), "配置齐全时应报告就绪");
    let provider = DeferredProvider::new();
    // 两次 usage_snapshot（内部 resolve）在配置不变时命中同一缓存实例且不 panic。
    assert_eq!(provider.usage_snapshot().total_tokens, 0);
    assert_eq!(provider.usage_snapshot().total_tokens, 0);

    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}

/// 推理档位（取优合并自远端 engine）：只认 minimal/low/medium/high，默认与非法值
/// 都不下发 `reasoning_effort`（避免不支持该字段的端点 400）。
#[tokio::test]
async fn request_body_applies_bounded_output_token_env() {
    let _guard = ENV_LOCK.lock().await;
    let saved = std::env::var("OWO_MODEL_MAX_OUTPUT_TOKENS").ok();
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        base_url: "http://127.0.0.1:11434/v1".to_string(),
        api_key: String::new(),
        model: "local".to_string(),
        cloud_enabled: false,
    })
    .unwrap();

    std::env::remove_var("OWO_MODEL_MAX_OUTPUT_TOKENS");
    assert!(provider
        .request_body(None, &[], &[], false)
        .get("max_tokens")
        .is_none());
    std::env::set_var("OWO_MODEL_MAX_OUTPUT_TOKENS", "32000");
    assert_eq!(
        provider.request_body(None, &[], &[], true)["max_tokens"],
        32000
    );
    std::env::set_var("OWO_MODEL_MAX_OUTPUT_TOKENS", "32001");
    assert!(provider
        .request_body(None, &[], &[], false)
        .get("max_tokens")
        .is_none());

    match saved {
        Some(value) => std::env::set_var("OWO_MODEL_MAX_OUTPUT_TOKENS", value),
        None => std::env::remove_var("OWO_MODEL_MAX_OUTPUT_TOKENS"),
    }
}

#[tokio::test]
async fn request_body_sends_reasoning_effort_only_for_known_levels() {
    let _guard = ENV_LOCK.lock().await;
    let saved = std::env::var("OWO_REASONING_EFFORT").ok();
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        base_url: "http://127.0.0.1:11434/v1".to_string(),
        api_key: String::new(),
        model: "local".to_string(),
        cloud_enabled: false,
    })
    .unwrap();

    // 默认（未选择档位）：请求体与旧版一致，不新增字段。
    std::env::remove_var("OWO_REASONING_EFFORT");
    let body = provider.request_body(None, &[], &[], false);
    assert!(
        body.get("reasoning_effort").is_none(),
        "默认不应下发推理档位"
    );

    std::env::set_var("OWO_REASONING_EFFORT", " HIGH ");
    let body = provider.request_body(None, &[], &[], false);
    assert_eq!(body["reasoning_effort"], "high");

    // 非法取值不下发：宁可回落模型默认，也不让端点因未知字段报错。
    std::env::set_var("OWO_REASONING_EFFORT", "unsupported");
    let body = provider.request_body(None, &[], &[], false);
    assert!(body.get("reasoning_effort").is_none(), "非法取值不应下发");

    match saved {
        Some(value) => std::env::set_var("OWO_REASONING_EFFORT", value),
        None => std::env::remove_var("OWO_REASONING_EFFORT"),
    }
}
