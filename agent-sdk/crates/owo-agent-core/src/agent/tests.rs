use super::*;

#[test]
fn estimate_tokens_counts_chars_and_overhead() {
    let messages = vec![
        ChatMessage::system("规则".to_string()),
        ChatMessage::user("你好，请帮我总结这段代码".to_string()),
        ChatMessage::assistant_text("好的。".to_string()),
    ];
    let total = estimate_tokens(&messages);
    // 每条约 +4 开销：3 条 → 12；正文 ≈ (2 + 12 + 3)/2。
    assert!(
        (15..=25).contains(&total),
        "估算 token {total} 应在合理区间"
    );
}

#[test]
fn empty_messages_cost_zero() {
    assert_eq!(estimate_tokens(&[]), 0);
}

#[test]
fn compact_truncate_keeps_system_and_recent_tail() {
    let mut messages = vec![ChatMessage::system("系统".to_string())];
    for index in 0..10 {
        messages.push(ChatMessage::user(format!("消息{index}")));
    }
    compact_truncate(&mut messages, 4);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, "system");
    assert!(messages
        .iter()
        .any(|message| message.content.as_deref() == Some("消息9")));
    assert!(messages
        .iter()
        .any(|message| message.content.as_deref() == Some("消息7")));
}

#[test]
fn compact_truncate_keeps_tool_call_and_results_together() {
    let mut messages = vec![
        ChatMessage::system("系统".to_string()),
        ChatMessage::user("旧请求".to_string()),
        ChatMessage::assistant_tool_calls(vec![crate::gateway::ToolCall {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "a.txt" }),
        }]),
        ChatMessage::tool("call-1".to_string(), "结果".to_string()),
        ChatMessage::user("继续".to_string()),
        ChatMessage::assistant_text("好的".to_string()),
    ];

    compact_truncate(&mut messages, 4);

    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[1].role, "assistant");
    assert!(messages[1].tool_calls.is_some());
    assert_eq!(messages[2].role, "tool");
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call-1"));
}

#[test]
fn tool_result_is_bounded_without_splitting_unicode() {
    let result = truncate_tool_result(&"中".repeat(10), 3);
    assert!(result.starts_with("中中中"));
    assert!(result.contains("工具输出已截断"));
}

// ---------- §9.1 安全并发只读工具 ----------

use crate::tools::{Tool, ToolSpec};
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

/// §9.1 共享探针状态：活跃数峰值（并发观测）+ 完成名单（顺序/取消观测）。
struct ProbeState {
    active: AtomicUsize,
    peak: Mutex<usize>,
    completed: Mutex<Vec<String>>,
}

impl ProbeState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            peak: Mutex::new(0),
            completed: Mutex::new(Vec::new()),
        })
    }

    fn begin(&self) {
        let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        let mut peak = self.peak.lock().unwrap();
        if current > *peak {
            *peak = current;
        }
    }

    fn end(&self, label: &str) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.completed.lock().unwrap().push(label.to_string());
    }
}

/// effect 可配置的探针工具（验证只读 / 自报只读 / 写），供分组断言。
struct ProbeTool {
    label: &'static str,
    delay_ms: u64,
    class: EffectClass,
    host_verified: bool,
    state: Arc<ProbeState>,
}

#[async_trait::async_trait]
impl Tool for ProbeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            self.label,
            "并发探针".to_string(),
            serde_json::json!({ "type": "object" }),
            Some(crate::tool_effects::ToolEffect {
                tool: self.label.to_string(),
                class: self.class,
                source: "builtin".to_string(),
                risk_note: None,
                annotations: None,
                host_verified_readonly: self.host_verified,
            }),
        )
    }

    async fn run(
        &self,
        _ctx: &mut ToolContext<'_>,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.state.begin();
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        self.state.end(self.label);
        Ok(serde_json::json!({ "tool": self.label, "delay_ms": self.delay_ms }))
    }
}

/// 固定脚本 provider：按序吐出预置输出（工具调用轮 + 文本轮）。
struct ScriptedTestProvider {
    outputs: Mutex<VecDeque<ModelOutput>>,
}

#[async_trait::async_trait]
impl ModelProvider for ScriptedTestProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| "脚本输出已耗尽".to_string())
    }
}

/// 两个探针调用的脚本：第一轮按给定顺序发两个 tool-call，随后文本收尾。
fn two_call_then_text(first: &str, second: &str) -> Mutex<VecDeque<ModelOutput>> {
    Mutex::new(VecDeque::from(vec![
        ModelOutput::ToolCalls(vec![
            crate::gateway::ToolCall {
                id: format!("call-{first}"),
                name: first.to_string(),
                arguments: serde_json::json!({}),
            },
            crate::gateway::ToolCall {
                id: format!("call-{second}"),
                name: second.to_string(),
                arguments: serde_json::json!({}),
            },
        ]),
        ModelOutput::Text("完成".to_string()),
    ]))
}

async fn run_with(
    registry: ToolRegistry,
    outputs: Mutex<VecDeque<ModelOutput>>,
    abort: &AtomicBool,
) -> Result<(TurnOutcome, Session), AgentError> {
    let provider = Arc::new(ScriptedTestProvider { outputs });
    let agent = Agent::new(
        provider,
        registry,
        Policy::new("."),
        AgentConfig {
            max_turns: 4,
            ..Default::default()
        },
    );
    let mut session = Session::new(std::env::temp_dir(), "test-model", None);
    let approver = crate::permissions::AutoApprover { allow: true };
    let outcome = agent
        .run_turn(&mut session, "跑探针", &approver, abort, &mut |_| {})
        .await?;
    Ok((outcome, session))
}

#[tokio::test]
async fn two_verified_read_tools_execute_concurrently() {
    let state = ProbeState::new();
    let mut registry = ToolRegistry::new();
    for label in ["probe_a", "probe_b"] {
        registry.register(ProbeTool {
            label,
            delay_ms: 120,
            class: EffectClass::Read,
            host_verified: true,
            state: Arc::clone(&state),
        });
    }
    let (outcome, _) = run_with(
        registry,
        two_call_then_text("probe_a", "probe_b"),
        &AtomicBool::new(false),
    )
    .await
    .expect("回合应成功");
    assert_eq!(outcome.final_text.as_deref(), Some("完成"));
    assert_eq!(state.completed.lock().unwrap().len(), 2, "两个工具都应完成");
    assert!(
        *state.peak.lock().unwrap() >= 2,
        "两个宿主验证只读工具必须并发（活跃峰值 ≥2）"
    );
}

/// 远端 agent.rs 取优：`ToolResult` 事件带结果预览（截断，供步骤时间线 chip 展开）。
#[tokio::test]
async fn tool_result_events_carry_preview() {
    let state = ProbeState::new();
    let mut registry = ToolRegistry::new();
    for label in ["probe_a", "probe_b"] {
        registry.register(ProbeTool {
            label,
            delay_ms: 0,
            class: EffectClass::Read,
            host_verified: true,
            state: Arc::clone(&state),
        });
    }
    let (outcome, _) = run_with(
        registry,
        two_call_then_text("probe_a", "probe_b"),
        &AtomicBool::new(false),
    )
    .await
    .expect("回合应成功");
    let previews: Vec<(String, String)> = outcome
        .events
        .iter()
        .filter_map(|event| match event {
            TurnEvent::ToolResult {
                tool,
                preview: Some(preview),
                ..
            } => Some((tool.clone(), preview.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(previews.len(), 2, "两个工具结果都应带预览：{previews:?}");
    assert!(
        previews
            .iter()
            .any(|(_, preview)| preview.contains("probe_a")),
        "预览应含工具结果正文：{previews:?}"
    );
}

/// M4.2：`session.model_override` 是请求级路由真相——显式覆盖进 wire，
/// `"default"` 哨兵清除后透传 None（Provider 解析链）；展示模型与 wire 解耦。
#[tokio::test]
async fn session_model_override_reaches_wire_model() {
    struct WireRecorder {
        seen: Mutex<Vec<Option<String>>>,
    }
    #[async_trait::async_trait]
    impl ModelProvider for WireRecorder {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Err("本测试只走流式路径".to_string())
        }
        async fn complete_stream_with_model(
            &self,
            model: Option<&str>,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            on_delta: &mut (dyn FnMut(String) + Send),
        ) -> Result<ModelOutput, String> {
            self.seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(model.map(str::to_string));
            on_delta("完成".to_string());
            Ok(ModelOutput::Text("完成".to_string()))
        }
    }
    let recorder = Arc::new(WireRecorder {
        seen: Mutex::new(Vec::new()),
    });
    let agent = Agent::new(
        recorder.clone() as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
        Policy::new("."),
        AgentConfig {
            max_turns: 4,
            ..Default::default()
        },
    );
    let approver = crate::permissions::AutoApprover { allow: true };
    let mut session = Session::new(std::env::temp_dir(), "display-model", None)
        .with_model_override(Some("wire-x".to_string()));
    agent
        .run_turn(
            &mut session,
            "你好",
            &approver,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .await
        .expect("回合应成功");
    // 展示值不受路由覆盖影响。
    assert_eq!(session.model, "display-model");
    // "default" 哨兵 = 清除覆盖 → 下一回合透传 None（回退 Provider 链）。
    session.set_model_override(Some("default".to_string()));
    agent
        .run_turn(
            &mut session,
            "再来",
            &approver,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .await
        .expect("第二回合应成功");
    let seen = recorder
        .seen
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        seen.as_slice(),
        &[Some("wire-x".to_string()), None],
        "覆盖必须进 wire；清除后必须回退 Provider 解析链"
    );
}

/// §9.3 瀑布：model 阶段记录首 token 时延（流式增量首达时刻 ≤ 总耗时）。
#[tokio::test]
async fn model_phase_timing_carries_first_token_latency() {
    let (outcome, _) = run_with(
        ToolRegistry::new(),
        Mutex::new(VecDeque::from(vec![ModelOutput::Text("完成".to_string())])),
        &AtomicBool::new(false),
    )
    .await
    .expect("回合应成功");
    let model = outcome
        .phase_timings
        .iter()
        .find(|timing| timing.phase == "model")
        .expect("应有 model 阶段瀑布记录");
    assert!(
        model.first_token_ms.is_some(),
        "§9.3：model 阶段应记录首 token 时延（默认流式包装会发首个增量）"
    );
    assert!(
        model.first_token_ms.unwrap() <= model.elapsed_ms,
        "首 token 时延不得超过该阶段总耗时"
    );
}

#[tokio::test]
async fn concurrent_group_results_backfill_in_original_call_order() {
    let mut registry = ToolRegistry::new();
    registry.register(ProbeTool {
        label: "probe_slow",
        delay_ms: 150,
        class: EffectClass::Read,
        host_verified: true,
        state: ProbeState::new(),
    });
    registry.register(ProbeTool {
        label: "probe_fast",
        delay_ms: 10,
        class: EffectClass::Read,
        host_verified: true,
        state: ProbeState::new(),
    });
    let (_, session) = run_with(
        registry,
        two_call_then_text("probe_slow", "probe_fast"),
        &AtomicBool::new(false),
    )
    .await
    .expect("回合应成功");
    // 快工具先完成，但 tool 消息必须仍按原始 tool-call 顺序回填。
    let tool_ids: Vec<&str> = session
        .messages
        .iter()
        .filter(|message| message.role == "tool")
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect();
    assert_eq!(
        tool_ids,
        vec!["call-probe_slow", "call-probe_fast"],
        "结果必须按原 tool-call 顺序回填"
    );
    let slow_content = session
        .messages
        .iter()
        .find(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some("call-probe_slow")
        })
        .and_then(|message| message.content.as_deref())
        .unwrap_or_default();
    assert!(slow_content.contains("probe_slow"), "慢工具结果不得串位");
}

#[test]
fn write_unverified_and_unknown_tools_are_not_concurrent_eligible() {
    let state = ProbeState::new();
    let mut registry = ToolRegistry::new();
    for (label, class, verified) in [
        ("probe_read_verified", EffectClass::Read, true),
        ("probe_read_self_reported", EffectClass::Read, false),
        ("probe_write", EffectClass::Write, true),
    ] {
        registry.register(ProbeTool {
            label,
            delay_ms: 0,
            class,
            host_verified: verified,
            state: Arc::clone(&state),
        });
    }
    let provider = Arc::new(ScriptedTestProvider {
        outputs: Mutex::new(VecDeque::new()),
    });
    let agent = Agent::new(provider, registry, Policy::new("."), AgentConfig::default());
    let call = |name: &str| crate::gateway::ToolCall {
        id: "x".to_string(),
        name: name.to_string(),
        arguments: serde_json::json!({}),
    };
    assert!(agent.call_is_concurrent_eligible(&call("probe_read_verified")));
    // MCP 自报 readOnlyHint 但宿主未验证 → 不得并发（保持串行）。
    assert!(!agent.call_is_concurrent_eligible(&call("probe_read_self_reported")));
    // 写工具绝不可能进并发组（同路径读写不并发由分组排除保证）。
    assert!(!agent.call_is_concurrent_eligible(&call("probe_write")));
    // 未知工具 → 串行错误路径。
    assert!(!agent.call_is_concurrent_eligible(&call("__missing__")));
}

#[tokio::test]
async fn abort_during_concurrent_group_cancels_pending_tools() {
    let state = ProbeState::new();
    let mut registry = ToolRegistry::new();
    for label in ["probe_a", "probe_b"] {
        registry.register(ProbeTool {
            label,
            delay_ms: 400,
            class: EffectClass::Read,
            host_verified: true,
            state: Arc::clone(&state),
        });
    }
    let abort = Arc::new(AtomicBool::new(false));
    {
        let abort = Arc::clone(&abort);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            abort.store(true, Ordering::SeqCst);
        });
    }
    let result = run_with(registry, two_call_then_text("probe_a", "probe_b"), &abort).await;
    assert!(
        matches!(result, Err(AgentError::Aborted)),
        "组执行中途 abort 应返回 Aborted：{result:?}"
    );
    assert!(
        state.completed.lock().unwrap().is_empty(),
        "取消后不得有任何工具完成（无后台残留）"
    );
}

// ---------- §9.3 超大工具结果 artifact 落盘 ----------

/// §9.3：输出超大结果的探针工具（宿主验证只读）。
struct BigOutputTool;

#[async_trait::async_trait]
impl Tool for BigOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            "probe_big_output",
            "超大输出探针".to_string(),
            serde_json::json!({ "type": "object" }),
            Some(crate::tool_effects::ToolEffect {
                tool: "probe_big_output".to_string(),
                class: EffectClass::Read,
                source: "builtin".to_string(),
                risk_note: None,
                annotations: None,
                host_verified_readonly: true,
            }),
        )
    }

    async fn run(
        &self,
        _ctx: &mut ToolContext<'_>,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "blob": "x".repeat(60_000) }))
    }
}

fn big_output_script() -> Mutex<VecDeque<ModelOutput>> {
    Mutex::new(VecDeque::from(vec![
        ModelOutput::ToolCalls(vec![crate::gateway::ToolCall {
            id: "call-big".to_string(),
            name: "probe_big_output".to_string(),
            arguments: serde_json::json!({}),
        }]),
        ModelOutput::Text("完成".to_string()),
    ]))
}

fn first_tool_content(session: &Session) -> String {
    session
        .messages
        .iter()
        .find(|message| message.role == "tool")
        .and_then(|message| message.content.clone())
        .expect("应有 tool 消息")
}

#[tokio::test]
async fn oversized_tool_result_stored_as_artifact_pointer() {
    let mut registry = ToolRegistry::new();
    registry.register(BigOutputTool);
    let provider = Arc::new(ScriptedTestProvider {
        outputs: big_output_script(),
    });
    let artifact_dir =
        std::env::temp_dir().join(format!("owo-artifact-test-{}", uuid::Uuid::new_v4()));
    let store = Arc::new(crate::cas_store::CasStore::new(artifact_dir).expect("CAS 初始化应成功"));
    let agent = Agent::new(provider, registry, Policy::new("."), AgentConfig::default())
        .with_artifact_store(Arc::clone(&store));
    let mut session = Session::new(std::env::temp_dir(), "test-model", None);
    let approver = crate::permissions::AutoApprover { allow: true };
    let outcome = agent
        .run_turn(
            &mut session,
            "大输出",
            &approver,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .await
        .expect("回合应成功");
    assert_eq!(outcome.final_text.as_deref(), Some("完成"));

    let tool_message = first_tool_content(&session);
    let envelope: serde_json::Value =
        serde_json::from_str(&tool_message).expect("应为 artifact JSON 信封");
    let artifact = &envelope["artifact"];
    let artifact_ref = artifact["ref"].as_str().expect("应有 ref").to_string();
    assert_eq!(artifact["mime"].as_str(), Some("text/plain"));
    assert!(
        artifact["size_bytes"].as_u64().unwrap_or(0) > 50_000,
        "应记录原始大小"
    );
    assert!(
        artifact["truncation_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("artifact"),
        "应记录截断原因"
    );
    assert!(
        tool_message.chars().count() < 55_000,
        "回填内容应为指针+预览，而非全量原文"
    );
    // CAS 往返：按 ref 可取回完整原文。
    let restored = store.get_text(&artifact_ref).expect("CAS 应含完整结果");
    assert!(restored.len() > 50_000, "完整原文应落盘");
    assert!(restored.contains("xxxxxxxxxx"));
}

#[tokio::test]
async fn without_artifact_store_truncation_keeps_legacy_text() {
    let mut registry = ToolRegistry::new();
    registry.register(BigOutputTool);
    let provider = Arc::new(ScriptedTestProvider {
        outputs: big_output_script(),
    });
    let agent = Agent::new(provider, registry, Policy::new("."), AgentConfig::default());
    let mut session = Session::new(std::env::temp_dir(), "test-model", None);
    let approver = crate::permissions::AutoApprover { allow: true };
    agent
        .run_turn(
            &mut session,
            "大输出",
            &approver,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .await
        .expect("回合应成功");
    let tool_message = first_tool_content(&session);
    assert!(
        tool_message.contains("工具输出已截断"),
        "未挂载 store 时维持盲截断旧行为"
    );
    assert!(
        !tool_message.contains("\"artifact\""),
        "未挂载 store 时不得产生 artifact 信封"
    );
}

/// 循环保护契约（对标 Codex/OpenCode）：弱模型反复发起**完全相同**的工具调用时，
/// 宿主必须拦截（不执行），并最终以可读原因结束回合，而不是放大成几百次执行。
#[tokio::test]
async fn repeated_identical_tool_call_is_loop_guarded() {
    struct RepeatingProvider {
        call: crate::gateway::ToolCall,
    }
    #[async_trait::async_trait]
    impl ModelProvider for RepeatingProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Ok(ModelOutput::ToolCalls(vec![self.call.clone()]))
        }
    }

    let state = ProbeState::new();
    let mut registry = ToolRegistry::new();
    registry.register(ProbeTool {
        label: "probe_a",
        delay_ms: 0,
        class: EffectClass::Read,
        host_verified: true,
        state: Arc::clone(&state),
    });
    let provider = Arc::new(RepeatingProvider {
        call: crate::gateway::ToolCall {
            id: "call-fixed".to_string(),
            name: "probe_a".to_string(),
            arguments: serde_json::json!({ "path": "same.txt" }),
        },
    });
    let agent = Agent::new(
        provider,
        registry,
        Policy::new("."),
        AgentConfig {
            max_turns: 30,
            max_repeated_tool_calls: 2,
            max_tool_calls_per_turn: 5,
            ..Default::default()
        },
    );
    let mut session = Session::new(std::env::temp_dir(), "test-model", None);
    let approver = crate::permissions::AutoApprover { allow: true };
    let error = agent
        .run_turn(
            &mut session,
            "死循环",
            &approver,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .await
        .expect_err("应触发循环保护");
    let message = error.to_string();
    assert!(
        message.contains("循环保护"),
        "错误应说明循环保护：{message}"
    );
    assert_eq!(
        state.completed.lock().unwrap().len(),
        2,
        "重复调用只允许执行 max_repeated_tool_calls 次，其余必须被拦截"
    );
}

/// 签名稳定性：参数键序不同但语义相同 → 视为同一调用（否则弱模型换键序即可绕过）。
#[test]
fn tool_call_signature_ignores_key_order() {
    let a = crate::gateway::ToolCall {
        id: "a".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({ "path": "x.txt", "content": "hi" }),
    };
    let b = crate::gateway::ToolCall {
        id: "b".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({ "content": "hi", "path": "x.txt" }),
    };
    assert_eq!(tool_call_signature(&a), tool_call_signature(&b));
}
