//! E0 契约测试：DesktopEnv 协议 + S1 可编程环境（主文档 §5.11、§9.0 E0、§11.7）。
//!
//! 覆盖完成标准：
//! - 同一 seed 可重复执行任务；
//! - 中途恢复快照得到相同状态；
//! - 成功/失败无需 VLM judge 即可判定；
//! - 两个环境可并行运行且互不串状态；
//! - 单写租约（ControllerLease）fencing；
//! - TaskSurface 适配器能力边界显式声明。

use async_trait::async_trait;
use owo_agent_core::computer_use::TaskSurface;
use owo_agent_core::desktop_env::{
    ActionKind, Assertion, DesktopEnv, EnvError, EnvRegistry, FaultSpec, GroundedAction,
    LeaseProof, RiskLevel, SimAppKind, SimDesktopEnv, SuccessSpec, SurfaceEnvAdapter, TaskSeed,
    Verdict,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 动作构造助手（坐标与渲染布局对应）
// ---------------------------------------------------------------------------

fn click(id: &str, x: i32, y: i32) -> GroundedAction {
    GroundedAction {
        action_id: id.into(),
        kind: ActionKind::Gui,
        semantic_intent: format!("点击 {id}"),
        target_id: Some(id.into()),
        target_evidence: vec![format!("element:{id}")],
        arguments: json!({ "op": "click", "x": x, "y": y }),
        expected_effects: vec![],
        risk: RiskLevel::Low,
        reversible: true,
        idempotency_key: None,
        click_point: Some((x, y)),
        target_bounds: None,
    }
}

fn type_action(id: &str, text: &str) -> GroundedAction {
    GroundedAction {
        action_id: id.into(),
        kind: ActionKind::Gui,
        semantic_intent: format!("输入 {text:?}"),
        target_id: None,
        target_evidence: vec![],
        arguments: json!({ "op": "type", "text": text }),
        expected_effects: vec![],
        risk: RiskLevel::Low,
        reversible: true,
        idempotency_key: None,
        click_point: None,
        target_bounds: None,
    }
}

fn wait_action(id: &str, steps: u64) -> GroundedAction {
    GroundedAction {
        action_id: id.into(),
        kind: ActionKind::Wait,
        semantic_intent: "等待".into(),
        target_id: None,
        target_evidence: vec![],
        arguments: json!({ "steps": steps }),
        expected_effects: vec![],
        risk: RiskLevel::Low,
        reversible: true,
        idempotency_key: None,
        click_point: None,
        target_bounds: None,
    }
}

fn chat_task(seed: u64) -> TaskSeed {
    TaskSeed {
        task_id: "task-chat-send".into(),
        app: SimAppKind::Chat,
        seed,
        assets: json!({ "contacts": ["Alice", "Bob"] }),
    }
}

/// 聊天发送闭环：聚焦输入框 → 输入 → 发送 → 等待（触发 seed 决定的自动回复）。
async fn chat_send_flow(env: &mut SimDesktopEnv, text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for action in [
        click("chat.input", 510, 640),
        type_action("type-msg", text),
        click("chat.send", 870, 640),
        wait_action("wait-reply", 1),
    ] {
        let result = env.step(action).await.expect("step 成功");
        refs.push(result.after_state_ref.clone());
    }
    refs
}

fn sent_message_assertion(contact: &str, text: &str) -> SuccessSpec {
    SuccessSpec {
        name: "消息已发送".into(),
        assertions: vec![
            Assertion::StateContains {
                path: "sent_log".into(),
                value: json!({ "contact": contact, "text": text }),
            },
            Assertion::TextVisible { text: text.into() },
            Assertion::CountAtLeast {
                path: "sent_log".into(),
                count: 1,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// 可复现性与快照
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_seed_same_actions_reproducible() {
    let mut env_a = SimDesktopEnv::new("env-a");
    env_a.reset(chat_task(42)).await.expect("reset");
    let mut env_b = SimDesktopEnv::new("env-b");
    env_b.reset(chat_task(42)).await.expect("reset");

    let refs_a = chat_send_flow(&mut env_a, "hello").await;
    let refs_b = chat_send_flow(&mut env_b, "hello").await;
    assert_eq!(refs_a, refs_b, "同一 seed + 同一动作序列必须可复现");
    assert_eq!(
        env_a.hidden_state_json(),
        env_b.hidden_state_json(),
        "终态隐藏状态必须一致"
    );
}

#[tokio::test]
async fn snapshot_restore_returns_identical_state() {
    let mut env = SimDesktopEnv::new("env-snap");
    env.reset(chat_task(7)).await.expect("reset");
    chat_send_flow(&mut env, "first").await;
    let state_at_snapshot = env.hidden_state_json();
    let snap_id = env.snapshot().await.expect("snapshot");

    // 继续推进：再发一条消息。
    env.step(click("chat.input", 510, 640))
        .await
        .expect("focus");
    env.step(type_action("type-second", "second"))
        .await
        .expect("type");
    env.step(click("chat.send", 870, 640)).await.expect("send");
    let verdict = env
        .judge(sent_message_assertion("Alice", "second"))
        .await
        .expect("judge");
    assert!(verdict.passed(), "第二条消息应已发送");

    // 恢复快照：状态回到快照点。
    env.restore(snap_id).await.expect("restore");
    assert_eq!(
        env.hidden_state_json(),
        state_at_snapshot,
        "恢复后状态必须与快照一致"
    );
    let verdict = env
        .judge(sent_message_assertion("Alice", "second"))
        .await
        .expect("judge");
    assert!(!verdict.passed(), "恢复后第二条消息不应存在");
    let verdict = env
        .judge(sent_message_assertion("Alice", "first"))
        .await
        .expect("judge");
    assert!(verdict.passed(), "恢复后第一条消息仍在");

    // 恢复后环境仍可继续执行。
    let result = env
        .step(click("chat.input", 510, 640))
        .await
        .expect("step after restore");
    assert!(result.verdict.passed());
}

#[tokio::test]
async fn restore_unknown_snapshot_rejected() {
    let mut env = SimDesktopEnv::new("env-unknown");
    env.reset(chat_task(1)).await.expect("reset");
    let err = env
        .restore("snap-nope".into())
        .await
        .expect_err("未知快照必须拒绝");
    assert!(matches!(err, EnvError::UnknownSnapshot(_)));
}

// ---------------------------------------------------------------------------
// 程序化判分（无需 VLM）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn judge_pass_and_fail_without_vlm() {
    let mut env = SimDesktopEnv::new("env-judge");
    env.reset(chat_task(3)).await.expect("reset");
    let verdict = env
        .judge(sent_message_assertion("Alice", "hello"))
        .await
        .expect("judge");
    assert!(!verdict.passed(), "未发送前判分必须失败");
    assert!(verdict.reason().is_some());

    chat_send_flow(&mut env, "hello").await;
    let verdict = env
        .judge(sent_message_assertion("Alice", "hello"))
        .await
        .expect("judge");
    assert!(verdict.passed(), "发送后判分必须通过");
    assert!(!verdict.evidence().is_empty());
}

#[tokio::test]
async fn observe_hides_hidden_state() {
    let mut env = SimDesktopEnv::new("env-observe");
    let state = env.reset(chat_task(5)).await.expect("reset");
    assert!(state.structured_app_state.is_none(), "观测不得暴露隐藏状态");
    assert_eq!(state.foreground_app, "owo-sim-chat");
    assert!(state.privacy_labels.contains(&"s1_sim".to_string()));
    let elements = state
        .scene_graph
        .get("elements")
        .and_then(Value::as_array)
        .expect("elements");
    assert!(elements
        .iter()
        .any(|e| e.get("id").and_then(Value::as_str) == Some("chat.send")));
}

// ---------------------------------------------------------------------------
// 故障注入：弹窗阻塞 / 元素漂移 / 迟钝步骤
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modal_popup_blocks_until_closed() {
    let mut env = SimDesktopEnv::new("env-popup");
    env.reset(chat_task(9)).await.expect("reset");
    env.step(click("chat.input", 510, 640))
        .await
        .expect("focus");
    env.step(type_action("type-msg", "blocked"))
        .await
        .expect("type");
    env.inject_fault(FaultSpec::ModalPopup {
        text: "系统更新提示".into(),
    })
    .await
    .expect("inject");

    let result = env.step(click("chat.send", 870, 640)).await.expect("step");
    assert!(!result.verdict.passed(), "弹窗期间发送必须被阻塞");

    let result = env
        .step(click("popup.close", 635, 366))
        .await
        .expect("close popup");
    assert!(result.verdict.passed(), "关闭弹窗必须成功");
    assert!(
        result.observed_delta.window_changed,
        "关闭弹窗的步内应反映窗口栈变化"
    );

    let result = env
        .step(click("chat.send", 870, 640))
        .await
        .expect("send after close");
    assert!(result.verdict.passed(), "关闭弹窗后发送必须成功");
    let verdict = env
        .judge(sent_message_assertion("Alice", "blocked"))
        .await
        .expect("judge");
    assert!(verdict.passed());
}

#[tokio::test]
async fn element_drift_invalidates_old_anchor() {
    let mut env = SimDesktopEnv::new("env-drift");
    env.reset(chat_task(11)).await.expect("reset");
    env.step(click("chat.input", 510, 640))
        .await
        .expect("focus");
    env.step(type_action("type-msg", "drift"))
        .await
        .expect("type");
    env.inject_fault(FaultSpec::ElementDrift {
        element_id: "chat.send".into(),
        dx: 200,
        dy: 0,
    })
    .await
    .expect("inject");

    // 旧锚点坐标失效（按钮已漂移出原位置）。
    let result = env
        .step(click("stale-anchor", 870, 640))
        .await
        .expect("step");
    assert!(!result.verdict.passed(), "漂移后旧坐标不得命中发送按钮");

    // 漂移后的新坐标有效（810+200=1010..1130）。
    let result = env
        .step(click("fresh-anchor", 1070, 640))
        .await
        .expect("step");
    assert!(result.verdict.passed(), "漂移后新坐标必须命中");
    let verdict = env
        .judge(sent_message_assertion("Alice", "drift"))
        .await
        .expect("judge");
    assert!(verdict.passed());
}

#[tokio::test]
async fn sluggish_steps_swallow_input() {
    let mut env = SimDesktopEnv::new("env-sluggish");
    env.reset(chat_task(13)).await.expect("reset");
    env.inject_fault(FaultSpec::SluggishSteps { steps: 1 })
        .await
        .expect("inject");
    let result = env.step(click("chat.input", 510, 640)).await.expect("step");
    assert!(!result.verdict.passed(), "迟钝步骤必须吞掉一次输入");
    let result = env.step(click("chat.input", 510, 640)).await.expect("step");
    assert!(result.verdict.passed(), "迟钝消耗后输入恢复");
}

// ---------------------------------------------------------------------------
// 其余三类应用状态机
// ---------------------------------------------------------------------------

#[tokio::test]
async fn files_rename_flow_with_dialog() {
    let mut env = SimDesktopEnv::new("env-files");
    env.reset(TaskSeed {
        task_id: "task-files-rename".into(),
        app: SimAppKind::Files,
        seed: 1,
        assets: json!({ "files": ["a.txt", "b.txt"] }),
    })
    .await
    .expect("reset");

    env.step(click("files.row.0", 210, 74))
        .await
        .expect("select");
    env.step(click("files.rename", 515, 115))
        .await
        .expect("open dialog");
    env.step(type_action("type-suffix", "_renamed"))
        .await
        .expect("type");
    env.step(click("files.dialog.ok", 260, 325))
        .await
        .expect("confirm");

    let verdict = env
        .judge(SuccessSpec {
            name: "重命名完成".into(),
            assertions: vec![Assertion::StateEquals {
                path: "files.0.name".into(),
                value: json!("a.txt_renamed"),
            }],
        })
        .await
        .expect("judge");
    assert!(verdict.passed(), "文件应已重命名：{verdict:?}");
}

#[tokio::test]
async fn form_validation_gate_then_submit() {
    let mut env = SimDesktopEnv::new("env-form");
    env.reset(TaskSeed {
        task_id: "task-form-submit".into(),
        app: SimAppKind::Form,
        seed: 2,
        assets: json!({ "fields": { "name": "", "email": "" } }),
    })
    .await
    .expect("reset");

    // 填写 name（BTreeMap 顺序：email 行在前，name 行在后）。
    env.step(click("form.input.name", 400, 135))
        .await
        .expect("focus name");
    env.step(type_action("type-name", "OwO"))
        .await
        .expect("type name");
    // email 非法 → 提交被校验阻塞。
    env.step(click("form.input.email", 400, 75))
        .await
        .expect("focus email");
    env.step(type_action("type-email-bad", "not-an-email"))
        .await
        .expect("type email");
    let result = env
        .step(click("form.submit", 260, 198))
        .await
        .expect("submit");
    assert!(!result.verdict.passed(), "非法邮箱必须被校验拦截");

    env.step(type_action("type-email-fix", "@owo.dev"))
        .await
        .expect("fix email");
    let result = env
        .step(click("form.submit", 260, 198))
        .await
        .expect("submit again");
    assert!(result.verdict.passed(), "修复后提交必须成功");

    let verdict = env
        .judge(SuccessSpec {
            name: "表单已提交".into(),
            assertions: vec![
                Assertion::StateEquals {
                    path: "submitted.email".into(),
                    value: json!("not-an-email@owo.dev"),
                },
                Assertion::StateEquals {
                    path: "submitted.name".into(),
                    value: json!("OwO"),
                },
                Assertion::TextVisible {
                    text: "已提交".into(),
                },
            ],
        })
        .await
        .expect("judge");
    assert!(verdict.passed(), "判分应确认提交内容：{verdict:?}");
}

#[tokio::test]
async fn document_type_and_save() {
    let mut env = SimDesktopEnv::new("env-doc");
    env.reset(TaskSeed {
        task_id: "task-doc-save".into(),
        app: SimAppKind::Document,
        seed: 4,
        assets: json!({ "initial_text": "" }),
    })
    .await
    .expect("reset");

    env.step(click("doc.body", 320, 290))
        .await
        .expect("focus doc");
    env.step(type_action("type-doc", "Hello OwO"))
        .await
        .expect("type");
    env.step(click("doc.save", 700, 36)).await.expect("save");

    let verdict = env
        .judge(SuccessSpec {
            name: "文档已保存".into(),
            assertions: vec![
                Assertion::StateEquals {
                    path: "saved_content".into(),
                    value: json!("Hello OwO"),
                },
                Assertion::StateEquals {
                    path: "saves".into(),
                    value: json!(1),
                },
            ],
        })
        .await
        .expect("judge");
    assert!(verdict.passed(), "文档应已保存：{verdict:?}");
}

#[tokio::test]
async fn cli_api_actions_are_unsupported_in_s1() {
    let mut env = SimDesktopEnv::new("env-cli");
    env.reset(chat_task(1)).await.expect("reset");
    let action = GroundedAction {
        action_id: "cli-ls".into(),
        kind: ActionKind::Cli,
        semantic_intent: "执行命令".into(),
        target_id: None,
        target_evidence: vec![],
        arguments: json!({ "command": "ls" }),
        expected_effects: vec![],
        risk: RiskLevel::Medium,
        reversible: false,
        idempotency_key: None,
        click_point: None,
        target_bounds: None,
    };
    let err = env.step(action).await.expect_err("S1 不支持 CLI 动作");
    assert!(matches!(err, EnvError::Unsupported(_)));
}

// ---------------------------------------------------------------------------
// EnvRegistry：并行隔离、克隆、单写租约
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_envs_run_in_parallel_without_state_crosstalk() {
    let registry = EnvRegistry::default();
    registry
        .create("env-pa", chat_task(21))
        .await
        .expect("create a");
    registry
        .create("env-pb", chat_task(21))
        .await
        .expect("create b");

    let lease_a = registry
        .acquire_lease("env-pa", "controller-a")
        .expect("lease a");
    let lease_b = registry
        .acquire_lease("env-pb", "controller-b")
        .expect("lease b");
    let proof_a = LeaseProof {
        owner: lease_a.owner.clone(),
        token: lease_a.token.clone(),
        epoch: lease_a.epoch,
    };
    let proof_b = LeaseProof {
        owner: lease_b.owner.clone(),
        token: lease_b.token.clone(),
        epoch: lease_b.epoch,
    };

    for (proof, env_id, text) in [(&proof_a, "env-pa", "msgA"), (&proof_b, "env-pb", "msgB")] {
        registry
            .step(env_id, proof, click("chat.input", 510, 640))
            .await
            .expect("focus");
        registry
            .step(env_id, proof, type_action("type-msg", text))
            .await
            .expect("type");
        registry
            .step(env_id, proof, click("chat.send", 870, 640))
            .await
            .expect("send");
    }

    let verdict_a = registry
        .judge("env-pa", sent_message_assertion("Alice", "msgA"))
        .await
        .expect("judge a");
    assert!(verdict_a.passed(), "env-a 应包含自己的消息");
    let env_b = registry.get("env-pb").expect("env b");
    let hidden_b = env_b.lock().await.hidden_state_json();
    assert!(
        !hidden_b
            .map(|v| v.to_string())
            .unwrap_or_default()
            .contains("msgA"),
        "env-b 不得串入 env-a 的状态"
    );
}

#[tokio::test]
async fn clone_env_shares_initial_state() {
    let registry = EnvRegistry::default();
    registry
        .create("env-src", chat_task(99))
        .await
        .expect("create");
    registry
        .clone_env("env-src", "env-clone")
        .await
        .expect("clone");
    let src = registry.get("env-src").expect("src");
    let cloned = registry.get("env-clone").expect("clone");
    let (hidden_src, hidden_clone) = {
        (
            src.lock().await.hidden_state_json(),
            cloned.lock().await.hidden_state_json(),
        )
    };
    assert_eq!(hidden_src, hidden_clone, "克隆环境必须与源环境同初始状态");
}

#[tokio::test]
async fn controller_lease_enforces_single_writer() {
    let registry = EnvRegistry::default();
    registry
        .create("env-lease", chat_task(31))
        .await
        .expect("create");

    let lease_a = registry
        .acquire_lease("env-lease", "controller-a")
        .expect("acquire");
    let proof_a = LeaseProof {
        owner: lease_a.owner.clone(),
        token: lease_a.token.clone(),
        epoch: lease_a.epoch,
    };

    // 他人持有期间：第二个控制者获取租约被拒。
    let err = registry
        .acquire_lease("env-lease", "controller-b")
        .expect_err("lease 被占用");
    assert!(matches!(err, EnvError::StaleLease(_)));

    // 他人凭证写入被拒（错误 owner / 错误 epoch / 错误 token）。
    let wrong_owner = LeaseProof {
        owner: "controller-b".into(),
        token: lease_a.token.clone(),
        epoch: lease_a.epoch,
    };
    assert!(matches!(
        registry
            .step("env-lease", &wrong_owner, wait_action("w", 1))
            .await,
        Err(EnvError::StaleLease(_))
    ));
    let wrong_epoch = LeaseProof {
        owner: lease_a.owner.clone(),
        token: lease_a.token.clone(),
        epoch: lease_a.epoch + 1,
    };
    assert!(matches!(
        registry
            .step("env-lease", &wrong_epoch, wait_action("w", 1))
            .await,
        Err(EnvError::StaleLease(_))
    ));
    let wrong_token = LeaseProof {
        owner: lease_a.owner.clone(),
        token: "forged".into(),
        epoch: lease_a.epoch,
    };
    assert!(matches!(
        registry
            .inject_fault(
                "env-lease",
                &wrong_token,
                FaultSpec::SluggishSteps { steps: 1 }
            )
            .await,
        Err(EnvError::StaleLease(_))
    ));

    // 正确凭证可写。
    registry
        .step("env-lease", &proof_a, wait_action("w", 1))
        .await
        .expect("step with valid lease");

    // 续租与释放。
    let renewed = registry.renew_lease("env-lease", &proof_a).expect("renew");
    assert_eq!(renewed.epoch, lease_a.epoch);
    registry
        .release_lease("env-lease", &proof_a)
        .expect("release");
    assert!(
        matches!(
            registry
                .step("env-lease", &proof_a, wait_action("w", 1))
                .await,
            Err(EnvError::StaleLease(_))
        ),
        "释放后旧凭证必须失效"
    );

    // 新控制者接管：epoch 递增（fencing）。
    let lease_b = registry
        .acquire_lease("env-lease", "controller-b")
        .expect("take over");
    assert!(lease_b.epoch > lease_a.epoch, "接管必须提升 epoch");
}

#[tokio::test]
async fn registry_reset_requires_lease_and_reseeds_env() {
    let registry = EnvRegistry::default();
    registry
        .create("env-reseed", chat_task(41))
        .await
        .expect("create");

    // 无活跃租约：复位必须被拒。
    let nobody = LeaseProof {
        owner: "controller-none".into(),
        token: "forged".into(),
        epoch: 0,
    };
    assert!(matches!(
        registry.reset("env-reseed", &nobody, chat_task(42)).await,
        Err(EnvError::StaleLease(_))
    ));

    let lease = registry
        .acquire_lease("env-reseed", "controller-a")
        .expect("acquire");
    let proof = LeaseProof {
        owner: lease.owner.clone(),
        token: lease.token.clone(),
        epoch: lease.epoch,
    };

    // 伪造 token：同样被 fencing 拒绝。
    let forged = LeaseProof {
        owner: lease.owner.clone(),
        token: "forged".into(),
        epoch: lease.epoch,
    };
    assert!(matches!(
        registry.reset("env-reseed", &forged, chat_task(42)).await,
        Err(EnvError::StaleLease(_))
    ));

    // 正确凭证：按新种子复位，旧状态必须清空。
    let state = registry
        .reset("env-reseed", &proof, chat_task(42))
        .await
        .expect("reset");
    assert!(state.snapshot_id.is_none(), "复位后回到初始快照");
    let verdict = registry
        .judge("env-reseed", sent_message_assertion("Alice", "whatever"))
        .await
        .expect("judge");
    assert!(!verdict.passed(), "复位后旧任务状态必须消失");

    // 未知环境：复位被拒（无租约可查）。
    assert!(registry
        .reset("env-ghost", &proof, chat_task(1))
        .await
        .is_err());
}

// ---------------------------------------------------------------------------
// TaskSurface 适配器：能力边界显式声明
// ---------------------------------------------------------------------------

struct MockSurface;

#[async_trait]
impl TaskSurface for MockSurface {
    fn app(&self) -> String {
        "mock-app".into()
    }
    async fn ocr(&mut self) -> Result<Value, String> {
        Ok(json!({ "lines": [
            { "text": "发送", "x": 100, "y": 200, "width": 60, "height": 24, "role_hint": "button" },
            { "text": "你好", "x": 10, "y": 40, "width": 80, "height": 24 }
        ] }))
    }
    async fn click(&mut self, _x: i32, _y: i32) -> Result<(), String> {
        Ok(())
    }
    async fn type_text(&mut self, _text: &str) -> Result<(), String> {
        Ok(())
    }
    async fn key(&mut self, _key: &str) -> Result<(), String> {
        Ok(())
    }
    async fn launch(&mut self, _target: &str) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn surface_adapter_observes_and_acts_but_declares_limits() {
    let mut adapter = SurfaceEnvAdapter::new(MockSurface, "surface-1");
    let state = adapter.observe().await.expect("observe");
    assert_eq!(state.foreground_app, "mock-app");
    let elements = state
        .scene_graph
        .get("elements")
        .and_then(Value::as_array)
        .expect("elements");
    assert!(elements
        .iter()
        .any(|e| e.get("text").and_then(Value::as_str) == Some("发送")));

    let result = adapter
        .step(click("surface-click", 130, 212))
        .await
        .expect("step");
    assert!(result.verdict.passed());

    // 不支持的能力显式声明，不伪造。
    assert!(matches!(
        adapter.reset(chat_task(1)).await,
        Err(EnvError::Unsupported(_))
    ));
    assert!(matches!(
        adapter.snapshot().await,
        Err(EnvError::Unsupported(_))
    ));
    assert!(matches!(
        adapter.judge(sent_message_assertion("Alice", "x")).await,
        Err(EnvError::Unsupported(_))
    ));
}

// ---------------------------------------------------------------------------
// 未 reset 环境保护
// ---------------------------------------------------------------------------

#[tokio::test]
async fn operations_before_reset_rejected() {
    let mut env = SimDesktopEnv::new("env-fresh");
    assert!(matches!(env.observe().await, Err(EnvError::NotReset(_))));
    assert!(matches!(
        env.step(wait_action("w", 1)).await,
        Err(EnvError::NotReset(_))
    ));
    assert!(matches!(env.snapshot().await, Err(EnvError::NotReset(_))));
}

#[tokio::test]
async fn verdict_serialization_is_stable() {
    let pass = Verdict::Pass {
        evidence: vec!["ok".into()],
    };
    let json_text = serde_json::to_string(&pass).expect("serialize");
    assert!(json_text.contains("\"verdict\":\"pass\""));
}
