//! DesktopWorld / WorldModel HTTP 闭环测试（R1 基线 + 第二路持久化/治理扩展，
//! 主文档 §8.5、§5.11、§5.12、§11.7）。
//!
//! 覆盖面（全部走真实 HTTP handler，不经进程内捷径）：
//!
//! R1 基线：
//! 1. create → observe → step → judge → snapshot → restore 全链路；
//! 2. reset/step/restore/inject-fault 写路径租约 fencing（错 token、错 owner、旧 epoch 一律 409）；
//! 3. step 自动写入 Transition，且可按稳定 ID 回查预测/真实差分（影子预测只记录不篡改）；
//! 4. Dataset 构建与 manifest 按 dataset_id 稳定回读。
//!
//! 第二路 · 数据集持久化（§本日目标）：
//! 5. 重启后原数据集 ID 仍可查询；多 manifest 同时恢复；读取以磁盘为权威（清空内存索引不受影响）；
//!    未知 ID / 非法 ID 返回 404；
//! 6. 损坏清单（坏 JSON / 文件名不一致 / content_hash 被篡改 / 目录内杂散文件）
//!    让初始化带具体路径 fail-fast，绝不静默加载；移除损坏文件后可正常恢复其余清单。
//!
//! 第二路 · 模型候选治理（修订 R1 的伪影子样本问题）：
//! 7. 晋升请求校验：无显式确认 → 400、无理由 → 400、未知候选 → 404；
//! 8. metadata_only 候选零影子样本（克隆 active 规则模型不再产生“独立评估”），
//!    即使被塞入伪造样本也无法晋升；
//! 9. 仅声明外部 provider 但未真实接线 → 明确拒绝晋升（当前尚无 WM1 调用面，不伪造已验证状态）；
//! 10. 已接线但样本数低于门槛 → 明确拒绝晋升（旧判据“样本数大于 0”已废弃）；
//! 11. 接线 + 样本达标 + 校准摘要 + 无退化 + ack/理由 → 晋升成功并固化快照；
//!     晋升后旧 active 降级 shadow；
//! 12. 关键指标相对上一任 active 退化超阈值 → 拒绝晋升；
//! 13. 只有真实接线的候选才会经 step 循环获得自然影子样本（端到端管道验证）。
//!
//! 运行态隔离：每个用例以独立 TempDir 数据根构造 [`DesktopWorldHub`]（生产路径是
//! build_router 内的进程级单例），避免跨用例污染 transition 语料与候选登记。

use axum::body::Body;
use axum::http::{header, Method, Request};
use owo_agent_core::desktop_env::{GroundedAction, RiskLevel, StateDelta, WorldStateV1};
use owo_agent_core::world_model::{
    GuiWorldModel, ModelError, PredictionEvaluation, WorldModelContext, WorldPrediction,
};
use owo_agent_server::{router_with_hub, DesktopWorldHub};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// 装配助手
// ---------------------------------------------------------------------------

/// 构造独立运行态：返回 (router, TempDir, hub)。调用方必须持有 TempDir 到用例结束——
/// 它承载 transition/dataset/experience 落盘目录，提前 drop 会让磁盘写路径失效。
/// hub 句柄供治理类用例直接操作登记簿（接线 provider / 注入评估夹具）。
async fn new_app() -> (axum::Router, tempfile::TempDir, Arc<DesktopWorldHub>) {
    let temp = tempfile::tempdir().expect("tempdir");
    let hub = DesktopWorldHub::new(temp.path()).expect("hub");
    let app = router_with_hub(hub.clone());
    (app, temp, hub)
}

fn req(method: &str, path: &str, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path);
    if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder.body(Body::from(b.to_string())).unwrap();
    }
    builder.body(Body::empty()).unwrap()
}

/// 发送请求并断言期望状态码，返回解析后的 JSON。
async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    want: u16,
) -> Value {
    let resp = app
        .clone()
        .oneshot(req(method, path, body))
        .await
        .expect("oneshot");
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        json!(null)
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "响应不是合法 JSON（{method} {path}）：{e}：{}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    assert_eq!(status, want, "{method} {path} 应 {want}：{value}");
    value
}

// ---------------------------------------------------------------------------
// 领域助手（坐标与 SimDesktopEnv 渲染布局对应）
// ---------------------------------------------------------------------------

fn chat_task(seed: u64) -> Value {
    json!({
        "task_id": format!("task-{seed}"),
        "app": "chat",
        "seed": seed,
        "assets": { "contacts": ["Alice", "Bob"] },
    })
}

fn click_action(action_id: &str, target: &str, x: i32, y: i32) -> Value {
    json!({
        "action_id": action_id,
        "kind": "gui",
        "semantic_intent": format!("点击 {target}"),
        "target_id": target,
        "target_evidence": [format!("element:{target}")],
        "arguments": { "op": "click", "x": x, "y": y },
        "risk": "low",
        "reversible": true,
    })
}

fn type_action(action_id: &str, text: &str) -> Value {
    json!({
        "action_id": action_id,
        "kind": "gui",
        "semantic_intent": format!("输入 {text:?}"),
        "arguments": { "op": "type", "text": text },
        "risk": "low",
        "reversible": true,
    })
}

fn sent_assertions(text: &str) -> Value {
    json!({
        "name": "消息已发送",
        "assertions": [
            { "type": "state_contains", "path": "sent_log", "value": { "contact": "Alice", "text": text } },
            { "type": "text_visible", "text": text },
            { "type": "count_at_least", "path": "sent_log", "count": 1 },
        ],
    })
}

/// 从创建响应提取写租约凭证 JSON（LeaseProof 形状）。
fn proof_of(lease: &Value) -> Value {
    json!({
        "owner": lease["owner"],
        "token": lease["token"],
        "epoch": lease["epoch"],
    })
}

async fn create_chat_env(app: &axum::Router, env_id: &str, seed: u64, owner: &str) -> Value {
    let created = call(
        app,
        "POST",
        "/desktop-envs",
        Some(json!({ "env_id": env_id, "task": chat_task(seed), "owner": owner })),
        200,
    )
    .await;
    assert_eq!(created["env_id"], *env_id);
    assert_eq!(
        created["lease"]["owner"], *owner,
        "创建响应必须携带初始写租约"
    );
    created
}

/// 已发送一条消息的环境（聚焦 → 输入 → 发送）。
async fn stepped_env(app: &axum::Router, env_id: &str, seed: u64, text: &str) -> Value {
    let created = create_chat_env(app, env_id, seed, "tester").await;
    let proof = proof_of(&created["lease"]);
    for (i, action) in [
        click_action(&format!("{env_id}-focus"), "chat.input", 510, 640),
        type_action(&format!("{env_id}-type"), text),
        click_action(&format!("{env_id}-send"), "chat.send", 870, 640),
    ]
    .into_iter()
    .enumerate()
    {
        let body = json!({
            "action": action,
            "lease": proof,
            "record": false, // 中间步骤不进语料，保持各用例语料可预测
        });
        let resp = call(
            app,
            "POST",
            &format!("/desktop-envs/{env_id}/step"),
            Some(body),
            200,
        )
        .await;
        assert_eq!(
            resp["verdict"]["verdict"], "pass",
            "步骤 {i} 应回环成功：{resp}"
        );
    }
    created
}

// ---------------------------------------------------------------------------
// 治理助手（候选评估夹具与接线桩）
// ---------------------------------------------------------------------------

/// 构造一条影子评估夹具。governance 门控的数值断言依赖这里的确定性参数。
fn eval_fixture(action_id: &str, p: f32, actual: bool, jaccard: f64) -> PredictionEvaluation {
    PredictionEvaluation {
        action_id: action_id.to_string(),
        success_probability: p,
        actual_success: actual,
        success_hit: (p > 0.5) == actual,
        calibration_error: (p - if actual { 1.0 } else { 0.0 }).abs(),
        delta_jaccard: jaccard,
        uncertainty: 0.1,
    }
}

/// 全部命中的评估序列（实际恒成功、预测概率足够高）。
fn seed_full_hit_evals(hub: &DesktopWorldHub, candidate: &str, n: usize, p: f32, jaccard: f64) {
    for i in 0..n {
        hub.record_evaluation_for(
            candidate.to_string(),
            eval_fixture(&format!("act-{i}"), p, true, jaccard),
        );
    }
}

/// 混合命中评估序列：good 全中、bad 全脱靶（用于构造退化场景）。
fn seed_mixed_hit_evals(
    hub: &DesktopWorldHub,
    candidate: &str,
    good: usize,
    bad: usize,
    jaccard: f64,
) {
    for i in 0..(good + bad) {
        let (p, actual) = if i < good {
            (0.95_f32, true)
        } else {
            (0.40_f32, true)
        };
        hub.record_evaluation_for(
            candidate.to_string(),
            eval_fixture(&format!("act-{i}"), p, actual, jaccard),
        );
    }
}

/// 测试用真实执行器桩：实现 [`GuiWorldModel`]（接线入口要求的真实类型），
/// 给出确定性预测（空差分 + 恒定概率）。用于验证「只有接线的候选才产生影子样本」
/// 以及接线后的并行对照端到端归因。
struct StubExecutor {
    model_id: &'static str,
    success_probability: f32,
}

impl GuiWorldModel for StubExecutor {
    fn predict(
        &self,
        _state: &WorldStateV1,
        _action: &GroundedAction,
        _context: &WorldModelContext,
    ) -> Result<WorldPrediction, ModelError> {
        Ok(WorldPrediction {
            predicted_delta: StateDelta::default(),
            assertion_probabilities: Vec::new(),
            success_probability: self.success_probability,
            risk: RiskLevel::Low,
            uncertainty: 0.1,
            model_id: self.model_id.to_string(),
            model_version: "stub-1".to_string(),
        })
    }

    fn model_id(&self) -> &str {
        self.model_id
    }

    fn model_version(&self) -> &str {
        "stub-1"
    }
}

/// 注册外部 provider 声明的候选，返回 (candidate_id, 响应体)。
async fn register_external_candidate(app: &axum::Router, model_id: &str, locator: &str) -> String {
    let resp = call(
        app,
        "POST",
        "/model-candidates",
        Some(json!({
            "model_id": model_id,
            "model_version": "1.0.0",
            "source": "第二路治理契约测试",
            "provider_ref": { "type": "external", "kind": "wm1-http", "locator": locator },
        })),
        200,
    )
    .await;
    assert_eq!(resp["status"], "shadow", "新候选不得静默成为 active");
    resp["candidate_id"]
        .as_str()
        .expect("candidate_id")
        .to_string()
}

async fn register_metadata_only_candidate(app: &axum::Router, model_id: &str) -> String {
    let resp = call(
        app,
        "POST",
        "/model-candidates",
        Some(json!({ "model_id": model_id, "model_version": "1.0.0" })),
        200,
    )
    .await;
    assert_eq!(resp["provider"]["type"], "metadata_only");
    resp["candidate_id"]
        .as_str()
        .expect("candidate_id")
        .to_string()
}

async fn try_promote(app: &axum::Router, candidate: &str, want: u16) -> Value {
    call(
        app,
        "POST",
        &format!("/model-candidates/{candidate}/promote"),
        Some(json!({ "ack": true, "reason": "第二路治理契约测试" })),
        want,
    )
    .await
}

// ---------------------------------------------------------------------------
// 1. 全链路闭环：create → observe → step → judge → snapshot → restore
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_loop_create_observe_step_judge_snapshot_restore() {
    let (app, _temp, _hub) = new_app().await;

    // create：返回首帧观测 + 初始租约。
    let created = create_chat_env(&app, "env-loop", 42, "controller").await;
    let proof = proof_of(&created["lease"]);

    // observe：读路径不需要租约。
    let observed = call(&app, "GET", "/desktop-envs/env-loop/observe", None, 200).await;
    assert_eq!(
        observed["foreground_app"], "owo-sim-chat",
        "首帧观测应显示聊天应用前台"
    );
    let elements = observed["scene_graph"]["elements"]
        .as_array()
        .expect("scene graph elements")
        .len();
    assert!(elements > 0);

    // step：写路径必须租约匹配（三步发送一条消息）。
    for action in [
        click_action("a-focus", "chat.input", 510, 640),
        type_action("a-type", "hello"),
        click_action("a-send", "chat.send", 870, 640),
    ] {
        let body = json!({ "action": action, "lease": proof });
        let resp = call(&app, "POST", "/desktop-envs/env-loop/step", Some(body), 200).await;
        assert_eq!(resp["verdict"]["verdict"], "pass", "每一步都应成功：{resp}");
    }

    // judge：程序化判分（无 VLM）。
    let verdict = call(
        &app,
        "POST",
        "/desktop-envs/env-loop/judge",
        Some(json!({ "success": sent_assertions("hello") })),
        200,
    )
    .await;
    assert_eq!(verdict["verdict"], "pass", "发送后判分应通过：{verdict}");

    // snapshot → 继续推进 → restore 回到快照点。
    let snap = call(
        &app,
        "POST",
        "/desktop-envs/env-loop/snapshot",
        Some(json!({})),
        200,
    )
    .await;
    let snap_id = snap["snapshot_id"]
        .as_str()
        .expect("snapshot id")
        .to_string();

    for action in [
        click_action("b-focus", "chat.input", 510, 640),
        type_action("b-type", "second"),
        click_action("b-send", "chat.send", 870, 640),
    ] {
        let body = json!({ "action": action, "lease": proof });
        call(&app, "POST", "/desktop-envs/env-loop/step", Some(body), 200).await;
    }

    let restored = call(
        &app,
        "POST",
        "/desktop-envs/env-loop/restore",
        Some(json!({ "snapshot": snap_id, "lease": proof })),
        200,
    )
    .await;
    assert_eq!(restored["snapshot_id"], snap_id, "恢复后观测应指向快照");

    let verdict = call(
        &app,
        "POST",
        "/desktop-envs/env-loop/judge",
        Some(json!({ "success": sent_assertions("second") })),
        200,
    )
    .await;
    assert_eq!(verdict["verdict"], "fail", "恢复后第二条消息不应存在");
    let verdict = call(
        &app,
        "POST",
        "/desktop-envs/env-loop/judge",
        Some(json!({ "success": sent_assertions("hello") })),
        200,
    )
    .await;
    assert_eq!(verdict["verdict"], "pass", "恢复后第一条消息仍在");
}

// ---------------------------------------------------------------------------
// 2. 写路径租约 fencing：reset/step/restore/inject-fault
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_paths_reject_stale_or_wrong_lease_proof() {
    let (app, _temp, _hub) = new_app().await;
    let created = create_chat_env(&app, "env-lease", 7, "alice").await;
    let alice = proof_of(&created["lease"]);

    // 错误 token 的 step → 409 fencing。
    let bad_token = json!({
        "action": click_action("c1", "chat.input", 510, 640),
        "lease": { "owner": "alice", "token": "forged", "epoch": alice["epoch"] },
    });
    let resp = call(
        &app,
        "POST",
        "/desktop-envs/env-lease/step",
        Some(bad_token),
        409,
    )
    .await;
    assert!(
        resp.to_string().contains("fencing") || resp.to_string().contains("不匹配"),
        "错误 token 应给出 fencing 语义信息：{resp}"
    );

    // 错误 epoch 的 reset → 409（旧 epoch 拒绝）。
    let bad_epoch_reset = json!({
        "task": chat_task(8),
        "lease": { "owner": "alice", "token": alice["token"], "epoch": 999 },
    });
    call(
        &app,
        "POST",
        "/desktop-envs/env-lease/reset",
        Some(bad_epoch_reset),
        409,
    )
    .await;

    // 未知快照的 restore 且凭证错误 → 仍然先撞 409 fencing（凭证优先于资源语义对齐 registry 校验顺序）。
    let bad_restore =
        json!({ "snapshot": "snap-nope", "lease": { "owner": "eve", "token": "t", "epoch": 1 } });
    call(
        &app,
        "POST",
        "/desktop-envs/env-lease/restore",
        Some(bad_restore),
        409,
    )
    .await;

    // 错误 owner 的 inject-fault → 409。
    let bad_fault = json!({
        "fault": { "type": "modal_popup", "text": "升级提示" },
        "lease": { "owner": "eve", "token": alice["token"], "epoch": alice["epoch"] },
    });
    call(
        &app,
        "POST",
        "/desktop-envs/env-lease/inject-fault",
        Some(bad_fault),
        409,
    )
    .await;

    // 释放后他人接管：epoch 单调递增 → 旧持有者的旧 epoch 凭证被拒绝。
    call(
        &app,
        "POST",
        "/desktop-envs/env-lease/lease",
        Some(json!({ "op": "release", "lease": alice })),
        200,
    )
    .await;
    let re_acquired = call(
        &app,
        "POST",
        "/desktop-envs/env-lease/lease",
        Some(json!({ "op": "acquire", "owner": "bob" })),
        200,
    )
    .await;
    let bob = proof_of(&re_acquired["lease"]);
    let old_epoch = alice["epoch"].as_u64().unwrap_or(1);
    let new_epoch = bob["epoch"].as_u64().expect("bob epoch");
    assert!(
        new_epoch > old_epoch,
        "重新接管必须递增 epoch（{old_epoch} → {new_epoch}）"
    );
    let stale_step = json!({
        "action": click_action("c2", "chat.input", 510, 640),
        "lease": alice,
    });
    call(
        &app,
        "POST",
        "/desktop-envs/env-lease/step",
        Some(stale_step),
        409,
    )
    .await;

    // 新 epoch 凭证可正常写入。
    let ok_step = json!({
        "action": click_action("c3", "chat.input", 510, 640),
        "lease": bob,
    });
    let resp = call(
        &app,
        "POST",
        "/desktop-envs/env-lease/step",
        Some(ok_step),
        200,
    )
    .await;
    assert_eq!(resp["verdict"]["verdict"], "pass");
}

// ---------------------------------------------------------------------------
// 3. step 自动写入 Transition + 预测/真实差分可查询
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step_records_transition_with_queryable_prediction_vs_real_diff() {
    let (app, _temp, _hub) = new_app().await;
    let created = create_chat_env(&app, "env-trace", 11, "controller").await;
    let proof = proof_of(&created["lease"]);
    let focus = click_action("t-focus", "chat.input", 510, 640);

    // 第一步：语料为空 → 无世界模型 → 确定性回退（fallback 显式说明），但 transition 照常落盘。
    let first = call(
        &app,
        "POST",
        "/desktop-envs/env-trace/step",
        Some(json!({ "action": focus, "lease": proof })),
        200,
    )
    .await;
    assert_eq!(first["recorded"], true, "默认 record=true 必须落盘");
    assert_eq!(first["prediction"], Value::Null, "语料为空时不应伪造预测");
    assert!(
        first["fallback"].as_str().is_some(),
        "回退必须给显式说明：{first}"
    );
    let first_transition = first["transition_id"]
        .as_str()
        .expect("transition id")
        .to_string();

    // 第二步：同一动作签名已有历史 → 影子预测可用（只记录对照，不改真实结果）。
    let second = call(
        &app,
        "POST",
        "/desktop-envs/env-trace/step",
        Some(json!({ "action": focus, "lease": proof })),
        200,
    )
    .await;
    let pred = second["prediction"]
        .as_object()
        .expect("第二步应有影子预测");
    assert!(
        pred.contains_key("predicted_delta_fingerprint"),
        "预测必须以结构化差分指纹给出：{pred:?}"
    );
    let eval = second["prediction_evaluation"]
        .as_object()
        .expect("应有预测/真实对照评估");
    assert_eq!(eval["actual_success"], true);
    assert_eq!(eval["success_hit"], true, "本例实际成功，p>0.5 判定应命中");

    // 稳定 ID 回查：transition 记录包含预测 ref 与真实观测差分两个字段。
    let trace = call(
        &app,
        "GET",
        &format!("/transitions/{first_transition}"),
        None,
        200,
    )
    .await;
    assert_eq!(trace["transition_id"], first_transition);
    assert_eq!(trace["env_id"], "env-trace");
    assert_eq!(
        trace["outcome"], "success",
        "单步 verdict 通过应记为成功终态"
    );
    assert!(
        trace["observed_delta"].is_object(),
        "真实观测差分必须随 transition 存档：{}",
        trace["observed_delta"]
    );
    assert_eq!(
        trace["action"]["action_id"], "t-focus",
        "transition 记录的动作与请求一致"
    );
    assert_eq!(
        trace["privacy_scope"], "s1_sim",
        "S1 数据默认允许进入训练集"
    );

    // record=false 只执行不落盘。
    let unrecorded = call(
        &app,
        "POST",
        "/desktop-envs/env-trace/step",
        Some(json!({ "action": focus, "lease": proof, "record": false })),
        200,
    )
    .await;
    assert_eq!(unrecorded["recorded"], false);
    assert_eq!(unrecorded["transition_id"], "");
    call(&app, "GET", "/transitions/no-such-transition", None, 404).await;
}

// ---------------------------------------------------------------------------
// 4. Dataset 构建 + manifest 稳定回读
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dataset_build_and_manifest_readback() {
    let (app, _temp, _hub) = new_app().await;
    let created = create_chat_env(&app, "env-data", 21, "controller").await;
    let proof = proof_of(&created["lease"]);
    for action in [
        click_action("d-focus", "chat.input", 510, 640),
        type_action("d-type", "build-me"),
        click_action("d-send", "chat.send", 870, 640),
    ] {
        call(
            &app,
            "POST",
            "/desktop-envs/env-data/step",
            Some(json!({ "action": action, "lease": proof })),
            200,
        )
        .await;
    }

    let built = call(&app, "POST", "/datasets/build", Some(json!({})), 200).await;
    let dataset_id = built["dataset_id"]
        .as_str()
        .expect("dataset_id")
        .to_string();
    assert_eq!(
        built["input_count"], 3,
        "三条已记录 transition 全部入清洗管线"
    );
    assert!(
        built["accepted_count"].as_u64().unwrap_or(0) >= 1,
        "干净成功轨迹至少接受 1 条：{built}"
    );
    assert!(
        !dataset_id.is_empty(),
        "数据集必须有稳定 ID/ref，而不是直接塞原始大对象"
    );

    // 同一 ID 稳定回读：manifest 字段一致。
    let manifest = call(
        &app,
        "GET",
        &format!("/datasets/{dataset_id}/manifest"),
        None,
        200,
    )
    .await;
    assert_eq!(manifest["dataset_id"], dataset_id);
    assert_eq!(manifest["content_hash"], built["content_hash"]);
    assert_eq!(manifest["accepted_count"], built["accepted_count"]);

    // 未知数据集 404；非法字符 id 一律 404（不会被当路径处理）；空语料构建 400。
    call(&app, "GET", "/datasets/no-such-dataset/manifest", None, 404).await;
    call(&app, "GET", "/datasets/bad~tilde-id/manifest", None, 404).await;
    let (empty, _temp_empty, _empty_hub) = new_app().await;
    let err = call(&empty, "POST", "/datasets/build", Some(json!({})), 400).await;
    assert!(
        err.to_string().contains("transition"),
        "空语料错误应指引用户先执行 step：{err}"
    );
}

// ---------------------------------------------------------------------------
// 5. 数据集持久化：重启恢复 + 磁盘权威
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dataset_manifest_survives_hub_restart_and_disk_is_authoritative() {
    let temp = tempfile::tempdir().expect("tempdir");

    // —— 第一“进程”：产生语料并构建两个稳定 ID 的数据集。
    let hub1 = DesktopWorldHub::new(temp.path()).expect("hub1");
    let app1 = router_with_hub(hub1.clone());
    let created = create_chat_env(&app1, "env-restart", 71, "controller").await;
    let proof = proof_of(&created["lease"]);
    for action in [
        click_action("r-focus", "chat.input", 510, 640),
        type_action("r-type", "persist-me"),
        click_action("r-send", "chat.send", 870, 640),
    ] {
        call(
            &app1,
            "POST",
            "/desktop-envs/env-restart/step",
            Some(json!({ "action": action, "lease": proof })),
            200,
        )
        .await;
    }
    let alpha = call(
        &app1,
        "POST",
        "/datasets/build",
        Some(json!({ "config": { "dataset_id": "ds-alpha" } })),
        200,
    )
    .await;
    let beta = call(
        &app1,
        "POST",
        "/datasets/build",
        Some(json!({ "config": { "dataset_id": "ds-beta" } })),
        200,
    )
    .await;
    let manifest_alpha = alpha["manifest"].clone();
    let manifest_beta = beta["manifest"].clone();
    assert_eq!(alpha["content_hash"], beta["content_hash"]);
    assert!(alpha["manifest_path"].as_str().is_some());

    drop(app1);
    drop(hub1); // 显式模拟第一进程退出；TempDir 仍在测试侧持有。

    // —— 重启：同一数据根新建 hub，索引应从磁盘完整恢复。
    let hub2 = DesktopWorldHub::new(temp.path()).expect("重启后 hub 应可恢复全部数据集索引");
    assert_eq!(
        hub2.datasets.lock().unwrap().len(),
        2,
        "启动扫描应同时恢复两个 manifest"
    );
    assert!(hub2.datasets.lock().unwrap().contains_key("ds-alpha"));
    assert!(hub2.datasets.lock().unwrap().contains_key("ds-beta"));
    let app2 = router_with_hub(hub2.clone());

    let got_alpha = call(&app2, "GET", "/datasets/ds-alpha/manifest", None, 200).await;
    assert_eq!(
        got_alpha, manifest_alpha,
        "重启后 manifest 内容（含 created_at/content_hash）不得改变"
    );
    let got_beta = call(&app2, "GET", "/datasets/ds-beta/manifest", None, 200).await;
    assert_eq!(got_beta, manifest_beta);

    // 未知 ID 与非法 id 一律 404。
    call(
        &app2,
        "GET",
        "/datasets/no-such-dataset/manifest",
        None,
        404,
    )
    .await;
    call(
        &app2,
        "GET",
        "/datasets/.hidden-dot-prefix/manifest",
        None,
        404,
    )
    .await;

    // —— 磁盘权威：即便清空进程内索引，读取依然走磁盘且结果不变。
    hub2.datasets.lock().unwrap().clear();
    let got_alpha_again = call(&app2, "GET", "/datasets/ds-alpha/manifest", None, 200).await;
    assert_eq!(
        got_alpha_again, got_alpha,
        "清空内存缓存后 manifest 读取应以持久化数据为权威"
    );
}

// ---------------------------------------------------------------------------
// 6. 损坏 manifest：初始化带具体路径 fail-fast，不静默加载
// ---------------------------------------------------------------------------

/// 直接写入一份通过全部校验的清单文件（与 core build_dataset 同口径计算哈希）。
fn write_valid_manifest(root: &std::path::Path, id: &str) {
    let content_hash: String = {
        use sha2::{Digest, Sha256};
        let ids = ["t-1", "t-2"];
        let mut hasher = Sha256::new();
        for id in ids {
            hasher.update(id.as_bytes());
            hasher.update(b"\n");
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let manifest = json!({
        "dataset_id": id,
        "created_at": "2026-08-27T09:00:00Z",
        "env_versions": ["S1-chat-1.0.0"],
        "input_count": 2,
        "accepted_count": 2,
        "success_count": 2,
        "failure_count": 0,
        "rejection_counts": {},
        "rejections": [],
        "sample_ids": ["t-1", "t-2"],
        "content_hash": content_hash,
    });
    write_raw(
        root,
        &format!("{id}.json"),
        &serde_json::to_string_pretty(&manifest).unwrap(),
    );
}

fn write_raw(root: &std::path::Path, file_name: &str, contents: &str) {
    let dir = root.join("desktop_world").join("datasets");
    std::fs::create_dir_all(&dir).expect("datasets dir");
    std::fs::write(dir.join(file_name), contents).expect("write dataset file");
}

#[test]
fn corrupted_dataset_manifest_fails_init_with_concrete_path() {
    // 子用例 a：纯坏 JSON → 报错点名具体文件。
    let temp = tempfile::tempdir().expect("tempdir");
    write_raw(temp.path(), "ds-garbage.json", "{ this is : not json");
    let err = DesktopWorldHub::new(temp.path()).expect_err("坏 JSON 必须让初始化失败");
    assert!(
        err.contains("ds-garbage.json") && err.contains("JSON 解析失败"),
        "错误必须带具体路径与原因：{err}"
    );

    // 子用例 b：结构合法、文件名一致但 content_hash 与样本列表对不上 → 报错点名文件与哈希原因。
    let temp = tempfile::tempdir().expect("tempdir");
    let stale_hash = "0".repeat(64);
    let tampered = json!({
        "dataset_id": "ds-hashbroke",
        "created_at": "2026-08-27T09:00:00Z",
        "env_versions": ["S1-chat-1.0.0"],
        "input_count": 2,
        "accepted_count": 2,
        "success_count": 2,
        "failure_count": 0,
        "rejection_counts": {},
        "rejections": [],
        "sample_ids": ["t-1", "t-2"],
        "content_hash": stale_hash,
    });
    write_raw(
        temp.path(),
        "ds-hashbroke.json",
        &serde_json::to_string_pretty(&tampered).unwrap(),
    );
    let err = DesktopWorldHub::new(temp.path()).expect_err("哈希不一致的清单必须失败");
    assert!(
        err.contains("ds-hashbroke.json") && err.contains("content_hash"),
        "错误必须点名文件并指出哈希不一致：{err}"
    );

    // 子用例 c：文件名与 dataset_id 不一致 → 报错点名文件。
    let temp = tempfile::tempdir().expect("tempdir");
    write_valid_manifest(temp.path(), "renamed-stem");
    let text =
        std::fs::read_to_string(temp.path().join("desktop_world/datasets/renamed-stem.json"))
            .unwrap();
    std::fs::remove_file(temp.path().join("desktop_world/datasets/renamed-stem.json")).unwrap();
    write_raw(temp.path(), "other-name.json", &text); // 内容合法但名字对不上 dataset_id
    let err = DesktopWorldHub::new(temp.path()).expect_err("文件名与 dataset_id 不一致必须失败");
    assert!(
        err.contains("other-name.json") && err.contains("不一致"),
        "错误必须点名文件并指出名称冲突：{err}"
    );

    // 子用例 d：目录中的杂散非 .json 文件同样 fail-fast（数据目录只收清单）。
    let temp = tempfile::tempdir().expect("tempdir");
    write_raw(temp.path(), "README.md", "# stray file");
    let err = DesktopWorldHub::new(temp.path()).expect_err("杂散文件必须失败而非静默跳过");
    assert!(
        err.contains("README.md"),
        "错误必须点名杂散文件的路径：{err}"
    );

    // 子用例 e：多 manifest 共存 + 移除损坏文件后其余清单可恢复。
    let temp = tempfile::tempdir().expect("tempdir");
    write_valid_manifest(temp.path(), "ds-first");
    write_raw(temp.path(), "ds-second-broken.json", "]]] not json [[[");
    let err = DesktopWorldHub::new(temp.path()).expect_err("存在坏清单时初始化必须失败");
    assert!(
        err.contains("ds-second-broken.json"),
        "错误必须点名坏文件：{err}"
    );
    std::fs::remove_file(
        temp.path()
            .join("desktop_world/datasets/ds-second-broken.json"),
    )
    .unwrap();
    let hub = DesktopWorldHub::new(temp.path()).expect("移除损坏文件后应能恢复其余 manifest");
    assert_eq!(hub.datasets.lock().unwrap().len(), 1);
    assert!(hub.datasets.lock().unwrap().contains_key("ds-first"));
}

// ---------------------------------------------------------------------------
// 7. 晋升请求校验：ack / reason / 未知候选
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promote_request_validation_requires_ack_reason_and_known_candidate() {
    let (app, _temp, _hub) = new_app().await;
    let candidate = register_metadata_only_candidate(&app, "wm-validation-check").await;

    // 未授权（ack=false）→ 400；无理由 → 400；未知候选 → 404。
    call(
        &app,
        "POST",
        &format!("/model-candidates/{candidate}/promote"),
        Some(json!({ "ack": false, "reason": "想转正" })),
        400,
    )
    .await;
    call(
        &app,
        "POST",
        &format!("/model-candidates/{candidate}/promote"),
        Some(json!({ "ack": true, "reason": "  " })),
        400,
    )
    .await;
    call(
        &app,
        "POST",
        "/model-candidates/no-such-candidate/promote",
        Some(json!({ "ack": true, "reason": "不存在" })),
        404,
    )
    .await;
}

// ---------------------------------------------------------------------------
// 8. metadata_only 候选：零影子样本 + 无法晋升（伪造样本也不行）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn metadata_only_candidate_gets_no_shadow_samples_and_cannot_promote() {
    let (app, _temp, hub) = new_app().await;
    let created = create_chat_env(&app, "env-meta", 81, "controller").await;
    let proof = proof_of(&created["lease"]);
    let focus = click_action("m-focus", "chat.input", 510, 640);

    // 两步建立语料：此后每步都会给 active（wm0-rule）记一条真实评估。
    for _ in 0..2 {
        call(
            &app,
            "POST",
            "/desktop-envs/env-meta/step",
            Some(json!({ "action": focus, "lease": proof })),
            200,
        )
        .await;
    }

    // 注册 metadata_only 候选（不带 provider_ref）。
    let candidate = register_metadata_only_candidate(&app, "wm-rule-meta-v1").await;

    // 再执行多步：影子预测循环不再克隆 active 规则模型代跑——
    // 候选必须保持零样本；active 继续按真实链路积累自己的样本。
    for _ in 0..3 {
        call(
            &app,
            "POST",
            "/desktop-envs/env-meta/step",
            Some(json!({ "action": focus, "lease": proof })),
            200,
        )
        .await;
    }
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(providers["active"]["candidate_id"], "wm0-rule");
    let active_samples = providers["samples"]["wm0-rule"].as_u64().unwrap_or(0);
    assert!(
        active_samples >= 1,
        "active 模型应有自然影子样本：{providers}"
    );
    assert!(
        providers["samples"].get(candidate.as_str()).is_none(),
        "metadata_only 候选必须零样本（伪影子样本治理）：{}",
        providers["samples"]
    );

    // 没有 provider 的候选不可晋升。
    let rejection = try_promote(&app, &candidate, 422).await;
    assert!(
        rejection.to_string().contains("metadata_only"),
        "拒绝信息必须点明无可执行 provider：{rejection}"
    );

    // 即便被塞入超过门槛数量的伪造样本（模拟克隆/外灌），provider 门控仍先拒绝。
    seed_full_hit_evals(&hub, &candidate, 24, 0.97, 0.98);
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(
        providers["samples"][candidate.as_str()]
            .as_u64()
            .unwrap_or(0),
        24,
        "夹具样本应已计入（证明后续拒绝是 provider 门控所致而非缺样本）"
    );
    let rejection = try_promote(&app, &candidate, 422).await;
    assert!(
        rejection.to_string().contains("metadata_only"),
        "伪造样本不能换得晋升资格：{rejection}"
    );
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(
        providers["active"]["candidate_id"], "wm0-rule",
        "active 不得被无 provider 候选顶替"
    );
}

// ---------------------------------------------------------------------------
// 9. 声明了外部 provider 但未真实接线 → 明确拒绝晋升
// ---------------------------------------------------------------------------

#[tokio::test]
async fn declared_but_unwired_external_provider_cannot_promote() {
    let (app, _temp, hub) = new_app().await;
    let candidate =
        register_external_candidate(&app, "wm-rule-unwired", "http://127.0.0.1:9/wm1").await;

    // 样本数量堆到门槛之上也无效：声明 ≠ 接线。
    seed_full_hit_evals(&hub, &candidate, 20, 0.99, 0.99);
    let rejection = try_promote(&app, &candidate, 422).await;
    let text = rejection.to_string();
    assert!(
        text.contains("未在本进程真实接线") || text.contains("WM1"),
        "拒绝信息必须点明未接线（当前尚无 WM1 调用面）：{rejection}"
    );

    // 登记状态保持 shadow，绝不静默转为 active。
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(providers["active"]["candidate_id"], "wm0-rule");
    assert_eq!(
        providers["samples"][candidate.as_str()]
            .as_u64()
            .unwrap_or(0),
        20
    );
}

// ---------------------------------------------------------------------------
// 10. 已接线但样本不足 → 明确拒绝晋升
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wired_candidate_below_min_samples_cannot_promote() {
    let (app, _temp, hub) = new_app().await;
    let candidate =
        register_external_candidate(&app, "wm-rule-few-samples", "unit://few-samples").await;

    // 真实接线（治理入口要求外部声明才允许），但只给出少量样本。
    hub.wire_candidate_executor(
        &candidate,
        "wm1-http",
        "unit://few-samples",
        Arc::new(StubExecutor {
            model_id: "wm-rule-few-samples",
            success_probability: 0.9,
        }),
    )
    .expect("外部声明的候选应允许接线");

    seed_full_hit_evals(&hub, &candidate, 5, 0.9, 0.9);
    let rejection = try_promote(&app, &candidate, 422).await;
    let text = rejection.to_string();
    assert!(
        text.contains("5/16"),
        "拒绝信息必须写明当前样本量与最低门槛：{rejection}"
    );
}

// ---------------------------------------------------------------------------
// 11. 全部门槛通过的合法晋升（含快照固化与 active 交接）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wired_candidate_with_clean_gates_promotes_and_snapshots_calibration() {
    let (app, _temp, hub) = new_app().await;
    let candidate =
        register_external_candidate(&app, "wm-rule-clean-pass", "unit://clean-pass").await;
    hub.wire_candidate_executor(
        &candidate,
        "wm1-http",
        "unit://clean-pass",
        Arc::new(StubExecutor {
            model_id: "wm-rule-clean-pass",
            success_probability: 0.95,
        }),
    )
    .expect("接线应成功");

    // 上一任 active 有充分的历史影子样本（质量中等）。
    seed_full_hit_evals(&hub, "wm0-rule", 20, 0.95, 0.80);
    // 新候选样本达标且各指标不低于上一任（部分更优）。
    seed_full_hit_evals(&hub, &candidate, 20, 0.95, 0.95);

    let result = try_promote(&app, &candidate, 200).await;
    assert_eq!(
        result["active"],
        candidate.as_str(),
        "晋升应切换 active：{result}"
    );
    assert_eq!(result["previous_active"], "wm0-rule");
    assert_eq!(result["samples"], 20);
    assert_eq!(result["gates"]["min_shadow_samples"], 16);
    assert_eq!(result["gates"]["provider_wired"]["kind"], "wm1-http");
    assert_eq!(result["gates"]["calibration_summary"]["samples"], 20);
    assert_eq!(result["gates"]["regression_check"]["passed"], true);

    // active 交接与降级、校准摘要快照进入登记。
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(providers["active"]["candidate_id"], candidate.as_str());
    let statuses: Vec<(String, String)> = providers["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|c| {
            (
                c["candidate_id"].as_str().unwrap_or_default().to_string(),
                c["status"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert!(
        statuses
            .iter()
            .any(|(id, st)| *id == "wm0-rule" && *st == "shadow"),
        "被替换的旧 active 应降级为 shadow：{statuses:?}"
    );
    let promoted = providers["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["candidate_id"] == candidate.as_str())
        .cloned()
        .unwrap();
    assert_eq!(promoted["status"], "active");
    assert!(
        promoted["calibration_summary"].is_object(),
        "晋升成功的候选必须携带当时的校准摘要快照：{promoted}"
    );
    assert_eq!(promoted["calibration_summary"]["samples"], 20);
    assert!(
        providers["calibrations"][candidate.as_str()].is_object(),
        "per-candidate 校准摘要应出现在 providers 视图"
    );

    // 审计字段：晋升时间与理由落档。
    assert!(promoted["promoted_at"].is_string());
    assert_eq!(promoted["promote_reason"], "第二路治理契约测试");
}

// ---------------------------------------------------------------------------
// 12. 相对上一任 active 退化超阈值 → 拒绝晋升
// ---------------------------------------------------------------------------

#[tokio::test]
async fn regression_beyond_threshold_blocks_promotion() {
    let (app, _temp, hub) = new_app().await;
    let candidate =
        register_external_candidate(&app, "wm-rule-regressed", "unit://regressed").await;
    hub.wire_candidate_executor(
        &candidate,
        "wm1-http",
        "unit://regressed",
        Arc::new(StubExecutor {
            model_id: "wm-rule-regressed",
            success_probability: 0.4,
        }),
    )
    .expect("接线应成功");

    // 上一任 active 表现良好；新候选命中率明显更低（0.75 vs 1.0 → 差 0.25 ≫ 0.05）。
    seed_full_hit_evals(&hub, "wm0-rule", 20, 0.95, 0.90);
    seed_mixed_hit_evals(&hub, &candidate, 15, 5, 0.90);

    let rejection = try_promote(&app, &candidate, 422).await;
    let text = rejection.to_string();
    assert!(
        text.contains("退化超阈值"),
        "拒绝信息必须指出退化：{rejection}"
    );

    // 登记状态保持不变。
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(providers["active"]["candidate_id"], "wm0-rule");
    let regressed = providers["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["candidate_id"] == candidate.as_str())
        .cloned()
        .unwrap();
    assert_eq!(
        regressed["status"], "shadow",
        "被拒候选必须仍是 shadow：{regressed}"
    );
}

// ---------------------------------------------------------------------------
// 13. 端到端：只有真实接线的候选拿得到 step 循环的自然影子样本
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_wired_candidates_earn_shadow_samples_through_step_loop() {
    let (app, _temp, hub) = new_app().await;
    let created = create_chat_env(&app, "env-wired-e2e", 91, "controller").await;
    let proof = proof_of(&created["lease"]);
    let focus = click_action("w-focus", "chat.input", 510, 640);

    // 无候选时先跑两步：active 累积真实评估。
    for _ in 0..2 {
        call(
            &app,
            "POST",
            "/desktop-envs/env-wired-e2e/step",
            Some(json!({ "action": focus, "lease": proof })),
            200,
        )
        .await;
    }
    let baseline_active_samples = {
        let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
        providers["samples"]["wm0-rule"].as_u64().unwrap_or(0)
    };

    // 一个接线的候选 + 一个未接线的（仅声明）候选。
    let wired = register_external_candidate(&app, "wm-wired-e2e", "unit://wired-e2e").await;
    hub.wire_candidate_executor(
        &wired,
        "wm1-http",
        "unit://wired-e2e",
        Arc::new(StubExecutor {
            model_id: "wm-wired-e2e",
            success_probability: 0.9,
        }),
    )
    .expect("接线应成功");
    let shadow_unwired =
        register_external_candidate(&app, "wm-unwired-e2e", "http://127.0.0.1:9/unwired").await;

    // 接线后跑四步：接线候选拿到恰好 4 个自然样本；未接线候选零样本。
    for _ in 0..4 {
        call(
            &app,
            "POST",
            "/desktop-envs/env-wired-e2e/step",
            Some(json!({ "action": focus, "lease": proof })),
            200,
        )
        .await;
    }
    let providers = call(&app, "GET", "/world-model/providers", None, 200).await;
    assert_eq!(
        providers["samples"][wired.as_str()].as_u64().unwrap_or(0),
        4,
        "接线候选应经 step 循环获得精确数量的自然样本：{}",
        providers["samples"]
    );
    assert!(
        providers["samples"].get(shadow_unwired.as_str()).is_none(),
        "仅声明未接线的候选必须保持零样本"
    );
    assert!(
        providers["samples"]["wm0-rule"].as_u64().unwrap_or(0) > baseline_active_samples,
        "active 自身的样本继续按真实链路增长"
    );
    assert!(
        providers["calibrations"][wired.as_str()].is_object(),
        "接线候选应有独立校准摘要"
    );
}

// ---------------------------------------------------------------------------
// 14. 两环境并发互不串状态（R1 原契约保留）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_envs_run_in_parallel_without_state_bleeding() {
    let (app, _temp, _hub) = new_app().await;
    let a_created = stepped_env(&app, "env-a", 51, "alpha").await;
    let b_created = create_chat_env(&app, "env-b", 51, "controller").await;
    let b_lease = proof_of(&b_created["lease"]);
    let _ = a_created;

    // A 已发送 alpha；B 仍是初始状态。
    let verdict_a = call(
        &app,
        "POST",
        "/desktop-envs/env-a/judge",
        Some(json!({ "success": sent_assertions("alpha") })),
        200,
    )
    .await;
    assert_eq!(verdict_a["verdict"], "pass");
    let verdict_b = call(
        &app,
        "POST",
        "/desktop-envs/env-b/judge",
        Some(json!({ "success": sent_assertions("alpha") })),
        200,
    )
    .await;
    assert_eq!(
        verdict_b["verdict"], "fail",
        "B 的判分不得读到 A 的隐藏状态"
    );

    // 并发各推进一步后仍互不影响（tokio join 模拟并行 HTTP 调用）。
    let (b_step, a_observed) = tokio::join!(
        call(
            &app,
            "POST",
            "/desktop-envs/env-b/step",
            Some(json!({
                "action": click_action("e-b-focus", "chat.input", 510, 640),
                "lease": b_lease,
                "record": false,
            })),
            200,
        ),
        call(&app, "GET", "/desktop-envs/env-a/observe", None, 200),
    );
    assert_eq!(b_step["verdict"]["verdict"], "pass", "B 聚焦输入框应成功");
    assert_eq!(a_observed["env_id"], "env-a");
    let verdict_a = call(
        &app,
        "POST",
        "/desktop-envs/env-a/judge",
        Some(json!({ "success": sent_assertions("alpha") })),
        200,
    )
    .await;
    assert_eq!(verdict_a["verdict"], "pass", "并发后 A 的成果仍在");

    // 快照空间独立：A 的快照不能在 B 上恢复。
    let snap = call(
        &app,
        "POST",
        "/desktop-envs/env-a/snapshot",
        Some(json!({})),
        200,
    )
    .await;
    let snap_id = snap["snapshot_id"].as_str().unwrap_or_default().to_string();
    let result = app
        .clone()
        .oneshot(req(
            "POST",
            "/desktop-envs/env-b/restore",
            Some(json!({ "snapshot": snap_id, "lease": b_lease })),
        ))
        .await
        .unwrap();
    assert_eq!(result.status().as_u16(), 404, "跨环境快照必须不可见");
}
