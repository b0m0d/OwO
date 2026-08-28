//! 控制面 HTTP 契约测试（P2 双节点网格：第一阶段最小契约 + 第二阶段真实远端节点协议）。
//!
//! `#[path = "../src/fleet_api.rs"] mod fleet_api;` 独立编译；
//! 每个测试独立构造 [`FleetHub`]（tempfile 临时目录），避免跨测试状态污染；
//! 节点执行由测试显式驱动（`hub.transport.complete_task`，模拟节点 agent 产出）。
//!
//! 覆盖：节点注册/列表、任务提交/完成/取消、审批（影响预览 + 结构化证据齐备才批准）、
//! 两节点冒烟链（注册→提交→租约→fencing→远程 step→审批→重放，无孤儿、无重复执行）；
//! R13 真实远端节点协议：注册/心跳续租、按 node_id 领取、越权回传拒绝、进度/证据/结果回传、
//! 取消确认、租约过期/旧 token/旧 epoch 拒绝并留审计（含真实 HTTP 两节点闭环）。

#[path = "../src/fleet_api.rs"]
mod fleet_api;

use axum::body::Body;
use axum::http::{header, Method, Request, Response, StatusCode};
use owo_agent_core::capability::CapabilityCard;
use owo_agent_core::fleet_transport::{FleetTransport, TransportStatus, TransportTask};
use owo_agent_core::lease::LeaseConfig;
use owo_agent_core::remote_step::EvidenceItem;
use owo_agent_core::FleetHttpTransport;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

type Hub = Arc<fleet_api::FleetHub>;

async fn test_hub() -> (Hub, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let hub = fleet_api::FleetHub::new(temp.path()).unwrap();
    (hub, temp)
}

fn request(method: &str, path: &str, body: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path);
    if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder.body(Body::from(b.to_string())).unwrap();
    }
    builder.body(Body::empty()).unwrap()
}

async fn send(hub: Hub, method: &str, path: &str, body: Option<&str>) -> Response<Body> {
    fleet_api::router_with_hub(hub)
        .oneshot(request(method, path, body))
        .await
        .unwrap()
}

async fn body_json(response: Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn register_body(node_id: &str) -> String {
    json!({
        "node_id": node_id,
        "card": {
            "worker": node_id,
            "os": "windows",
            "arch": "x86_64",
            "actions": ["shell".to_string()],
        }
    })
    .to_string()
}

fn submit_body(task_id: &str, worker: &str, approval: Option<Value>) -> String {
    let approval_required = approval.is_some();
    let mut input = json!({ "q": 1 });
    if let Some(a) = approval {
        input = a;
    }
    json!({
        "task_id": task_id,
        "worker": worker,
        "input": input,
        "correlation_id": format!("corr:{task_id}"),
        "approval_required": approval_required,
    })
    .to_string()
}

/// 审批材料齐备的 input。
fn approval_input(task_id: &str, owner: &str) -> Value {
    json!({
        "step_id": task_id,
        "approval": {
            "required": true,
            "owner_device": owner,
            "summary": "远程执行点击",
        },
        "impact_preview": "修改 config.yaml、重启目标服务",
        "evidence": [
            { "kind": "file_diff", "summary": "config.yaml +2 行" },
            { "kind": "command", "summary": "重启 service-a" }
        ]
    })
}

/// 轮询任务直到终态或超时。
async fn wait_status(
    hub: Hub,
    task_id: &str,
    expect: TransportStatus,
    max_wait: Duration,
) -> Value {
    let deadline = std::time::Instant::now() + max_wait;
    loop {
        let resp = send(hub.clone(), "GET", &format!("/fleet/tasks/{task_id}"), None).await;
        assert_eq!(resp.status(), StatusCode::OK, "任务查询应 200");
        let view = body_json(resp).await;
        let status = view["status"].as_str().unwrap_or("");
        let parsed = serde_json::from_value::<TransportStatus>(json!(status)).unwrap();
        if parsed == expect {
            return view;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待任务 {task_id} 到达 {expect:?} 超时（当前 {status}）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

/// 1) 节点注册 + 列表（CapabilityCard 自报 + 租约）。
#[tokio::test]
async fn register_and_list_nodes() {
    let (hub, _temp) = test_hub().await;
    for node in ["node-a", "node-b"] {
        let resp = send(
            hub.clone(),
            "POST",
            "/fleet/nodes/register",
            Some(&register_body(node)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["status"]["registered"], true, "节点应已注册");
        assert!(
            body["lease_epoch"].as_u64().unwrap_or(0) > 0,
            "注册应持租约"
        );
    }
    let resp = send(hub.clone(), "GET", "/fleet/nodes", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["count"], 2);
    let ids: Vec<&str> = body["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"node-a") && ids.contains(&"node-b"));
}

/// 2) 提交 → 节点显式完成 → Succeeded（无孤儿挂起、无重复完成）。
#[tokio::test]
async fn submit_and_complete() {
    let (hub, _temp) = test_hub().await;
    let task_id = new_id("t");
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // 节点执行：显式完成（模拟 node-a 产出）。
    assert!(
        hub.transport
            .complete_task(&task_id, true, json!("out-from-node")),
        "任务应可完成"
    );
    let view = wait_status(
        hub.clone(),
        &task_id,
        TransportStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(view["worker"], "node-a");
    let kinds: Vec<&str> = view["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"result"), "完成事件应落 Result：{kinds:?}");
    // 幂等完成：终态任务不再完成（无重复事件）。
    assert!(
        !hub.transport.complete_task(&task_id, true, json!("again")),
        "终态任务不应重复完成"
    );
}

/// 3) 审批：影响预览 + 结构化证据齐备 → 批准 → 节点执行完成。
#[tokio::test]
async fn approval_with_material_executes() {
    let (hub, _temp) = test_hub().await;
    let task_id = new_id("rs");
    let input = approval_input(&task_id, "phone-1");
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", Some(input))),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // 审批未决。
    let view = wait_status(
        hub.clone(),
        &task_id,
        TransportStatus::AwaitingApproval,
        Duration::from_secs(3),
    )
    .await;
    assert!(view["approval"]["impact_preview"]
        .as_str()
        .unwrap()
        .contains("config.yaml"));
    assert_eq!(view["approval"]["evidence"].as_array().unwrap().len(), 2);
    // 批准放行。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/approvals/{task_id}/respond"),
        Some(&json!({ "decision": "approve", "approved_by": "user-1" }).to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    // 审批放行后节点执行：显式完成。
    assert!(
        hub.transport
            .complete_task(&task_id, true, json!("out-from-node")),
        "审批放行后任务应可完成"
    );
    let view = wait_status(
        hub.clone(),
        &task_id,
        TransportStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(view["approval"]["decision"], "approved");
}

/// 4) 审批材料不齐（缺证据）→ 批准被拒（显式 422，任务取消，不静默执行）。
#[tokio::test]
async fn approval_missing_material_rejected() {
    let (hub, _temp) = test_hub().await;
    let task_id = new_id("rs");
    // 缺 evidence。
    let input = json!({
        "step_id": task_id,
        "approval": { "required": true, "owner_device": "phone-1", "summary": "x" },
        "impact_preview": "只有预览无证据"
    });
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", Some(input))),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/approvals/{task_id}/respond"),
        Some(&json!({ "decision": "approve", "approved_by": "user-1" }).to_string()),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "材料不齐应拒绝批准"
    );
    let view = wait_status(
        hub.clone(),
        &task_id,
        TransportStatus::Cancelled,
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(view["approval"]["decision"], "rejected");
}

/// 5) 审批拒绝决策 → 任务取消。
#[tokio::test]
async fn approval_reject_decision_cancels() {
    let (hub, _temp) = test_hub().await;
    let task_id = new_id("rs");
    let input = approval_input(&task_id, "phone-1");
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", Some(input))),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/approvals/{task_id}/respond"),
        Some(&json!({ "decision": "reject", "approved_by": "user-1" }).to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let view = wait_status(
        hub.clone(),
        &task_id,
        TransportStatus::Cancelled,
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(view["approval"]["decision"], "rejected");
}

/// 6) 取消任务。
#[tokio::test]
async fn cancel_task() {
    let (hub, _temp) = test_hub().await;
    let task_id = new_id("t");
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", None)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/cancel"),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let view =
        body_json(send(hub.clone(), "GET", &format!("/fleet/tasks/{task_id}"), None).await).await;
    let status = serde_json::from_value::<TransportStatus>(json!(view["status"])).unwrap();
    assert!(
        matches!(
            status,
            TransportStatus::Cancelled | TransportStatus::Succeeded
        ),
        "取消后应终态：{status:?}"
    );
}

/// 7) 幂等键：同 task_id 重复提交 → 409（无重复执行）。
#[tokio::test]
async fn duplicate_submit_rejected() {
    let (hub, _temp) = test_hub().await;
    let task_id = new_id("t");
    let body = submit_body(&task_id, "node-a", None);
    let first = send(hub.clone(), "POST", "/fleet/tasks/submit", Some(&body)).await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = send(hub.clone(), "POST", "/fleet/tasks/submit", Some(&body)).await;
    assert_eq!(second.status(), StatusCode::CONFLICT, "重复提交应拒绝");
}

/// 8) 两节点冒烟链：注册→提交→租约→fencing→远程 step→审批→重放（无孤儿、无重复执行）。
#[tokio::test]
async fn two_node_smoke_chain() {
    let (hub, _temp) = test_hub().await;
    // 注册两个节点（持租约）。
    for node in ["node-a", "node-b"] {
        let resp = send(
            hub.clone(),
            "POST",
            "/fleet/nodes/register",
            Some(&register_body(node)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // 租约：node-a 持租约（epoch E1 + token T1）。
    let lease_a = hub.leases.lease("node-a").unwrap();
    let old_token = lease_a.token.clone();
    let old_epoch = lease_a.epoch;
    // 重注册 node-a（幂等心跳续租）：token/epoch 不变（续租不签发新 token）。
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/nodes/register",
        Some(&register_body("node-a")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let renewed = hub.leases.lease("node-a").unwrap();
    assert_eq!(renewed.epoch, old_epoch, "续租不改变纪元");
    assert_eq!(renewed.token, old_token, "幂等重注册（心跳续租）不清 token");
    // fencing：重新 acquire（模拟失联重连）重新签发 token，旧 token 写被拒（防双写）。
    let re_acquire = hub.leases.acquire("node-a").unwrap();
    assert_eq!(re_acquire.epoch, old_epoch, "同 holder 重连 epoch 不变");
    assert_ne!(
        re_acquire.token, old_token,
        "重连重新签发 token（旧 token 作废）"
    );
    assert!(
        matches!(
            hub.leases.verify_write("node-a", &old_token, old_epoch),
            Err(owo_agent_core::lease::LeaseError::BadToken { .. })
        ),
        "旧 token 写应被 fencing 拒绝"
    );
    // 普通任务：node-b 执行（显式完成）。
    let t1 = new_id("t");
    send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&t1, "node-b", None)),
    )
    .await;
    assert!(hub.transport.complete_task(&t1, true, json!("out-b")));
    wait_status(
        hub.clone(),
        &t1,
        TransportStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    // 远程 step（审批）：影响预览 + 结构化证据 → 批准 → 节点执行。
    let rs1 = new_id("rs");
    let input = approval_input(&rs1, "phone-1");
    send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&rs1, "node-a", Some(input))),
    )
    .await;
    wait_status(
        hub.clone(),
        &rs1,
        TransportStatus::AwaitingApproval,
        Duration::from_secs(3),
    )
    .await;
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/approvals/{rs1}/respond"),
        Some(&json!({ "decision": "approve", "approved_by": "owner" }).to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(hub.transport.complete_task(&rs1, true, json!("out-rs")));
    wait_status(
        hub.clone(),
        &rs1,
        TransportStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    // 重放：bus_store 已落盘节点状态/任务提交事件；幂等去重不重复。
    let msgs = hub.bus_store.replay_messages();
    assert!(
        msgs.iter()
            .any(|m| m.correlation_id.starts_with("node:status:")),
        "节点状态事件应落盘：{:?}",
        msgs.iter()
            .map(|m| m.correlation_id.clone())
            .collect::<Vec<_>>()
    );
    let deduped = owo_agent_core::fleet::dedupe_messages(&msgs);
    assert_eq!(deduped.len(), msgs.len(), "重放应无重复消息");
    // 无孤儿：全部任务已终态。
    let ids = hub.transport.task_ids();
    for id in ids {
        let status = hub.transport.task_status(&id).unwrap();
        assert!(
            matches!(
                status,
                TransportStatus::Succeeded | TransportStatus::Failed | TransportStatus::Cancelled
            ),
            "任务 {id} 不应挂起（无孤儿）：{status:?}"
        );
    }
}

// ---------- R13 真实远端节点协议 ----------

fn claim_body(node_id: &str, token: &str, epoch: u64) -> String {
    json!({ "node_id": node_id, "lease_token": token, "epoch": epoch }).to_string()
}

fn result_body(node_id: &str, token: &str, epoch: u64, ok: bool, output: Value) -> String {
    json!({
        "node_id": node_id,
        "lease_token": token,
        "epoch": epoch,
        "ok": ok,
        "output": output,
    })
    .to_string()
}

/// 9) 真实 HTTP 两节点集成闭环：注册 node-a/node-b → node-a 领取 → node-b 越权被拒 →
///    node-a 进度 + 成功结果回传 → 终态幂等拒绝重复回传。
#[tokio::test]
async fn node_protocol_http_closed_loop() {
    let (hub, _temp) = test_hub().await;
    let app = fleet_api::router_with_hub(hub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = FleetHttpTransport::new(format!("http://{addr}"));

    // 注册 node-a / node-b（各自持租约 token + epoch）。
    let card_a = CapabilityCard::new("node-a").actions(vec!["shell".to_string()]);
    let reg_a = client.node_register("node-a", &card_a).await.unwrap();
    let token_a = reg_a["lease_token"].as_str().unwrap().to_string();
    let epoch_a = reg_a["lease_epoch"].as_u64().unwrap();
    let card_b = CapabilityCard::new("node-b").actions(vec!["shell".to_string()]);
    let reg_b = client.node_register("node-b", &card_b).await.unwrap();
    let token_b = reg_b["lease_token"].as_str().unwrap().to_string();
    let epoch_b = reg_b["lease_epoch"].as_u64().unwrap();

    // 提交任务（worker = node-a）。控制面不自动执行：状态保持 Running 等待领取。
    let task_id = new_id("t");
    let task = TransportTask::new(
        task_id.clone(),
        "node-a",
        format!("corr:{task_id}"),
        json!({ "q": 1 }),
    );
    client.submit(task).await.unwrap();
    assert_eq!(
        client.status(&task_id).await.unwrap(),
        TransportStatus::Running
    );

    // 可领取列表：node-a 看到自己的任务；node-b 看不到。
    let claimable_a = client.node_claimable("node-a").await.unwrap();
    assert_eq!(claimable_a["count"], 1, "node-a 应看到匹配任务");
    assert_eq!(claimable_a["tasks"][0]["task_id"], task_id);
    let claimable_b = client.node_claimable("node-b").await.unwrap();
    assert_eq!(claimable_b["count"], 0, "node-b 不应看到 node-a 的任务");

    // node-a 领取成功。
    client
        .node_claim("node-a", &task_id, &token_a, epoch_a)
        .await
        .unwrap();

    // node-b 不能领取该任务（worker 不匹配 → 403）。
    let err = client
        .node_claim("node-b", &task_id, &token_b, epoch_b)
        .await
        .unwrap_err();
    assert!(err.contains("节点不匹配"), "越权领取应被拒：{err}");

    // node-b 不能回传 node-a 的任务（越权回传 → 403 + 审计）。
    let err = client
        .node_report_result(
            "node-b",
            &task_id,
            &token_b,
            epoch_b,
            true,
            Some(json!("b")),
            None,
            vec![],
            None,
        )
        .await
        .unwrap_err();
    assert!(err.contains("越权回传"), "越权回传应被拒：{err}");

    // node-a 回传进度 + 结构化证据。
    client
        .node_report_progress(
            "node-a",
            &task_id,
            &token_a,
            epoch_a,
            "正在执行",
            vec![EvidenceItem::new("command", "生成结果文件")],
        )
        .await
        .unwrap();

    // node-a 回传成功结果（内联输出 + CAS 引用 + 证据）。
    client
        .node_report_result(
            "node-a",
            &task_id,
            &token_a,
            epoch_a,
            true,
            Some(json!("out-from-node-a")),
            Some("sha256:node-a-output".to_string()),
            vec![EvidenceItem::new("shell", "echo done")],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        client.status(&task_id).await.unwrap(),
        TransportStatus::Succeeded,
        "仅显式结果回传推进状态"
    );

    // 事件：进度 + 结果（结构化 payload 保留 CAS 引用与证据）。
    let events = client.events(&task_id).await.unwrap();
    let kinds: Vec<serde_json::Value> = events
        .iter()
        .map(|e| serde_json::to_value(e.kind).unwrap())
        .collect();
    assert!(
        kinds.contains(&json!("progress")),
        "进度事件应落盘：{kinds:?}"
    );
    assert!(
        kinds.contains(&json!("result")),
        "结果事件应落盘：{kinds:?}"
    );
    let result_ev = events
        .iter()
        .find(|e| serde_json::to_value(e.kind).unwrap() == json!("result"))
        .unwrap();
    assert_eq!(result_ev.payload["output_cas"], "sha256:node-a-output");
    assert_eq!(result_ev.payload["evidence"].as_array().unwrap().len(), 1);

    // 终态幂等：重复回传结果被拒绝（无重复执行/重复事件）。
    let err = client
        .node_report_result(
            "node-a",
            &task_id,
            &token_a,
            epoch_a,
            true,
            Some(json!("dup")),
            None,
            vec![],
            None,
        )
        .await
        .unwrap_err();
    assert!(!err.is_empty(), "终态重复回传应被拒绝");

    server.abort();
    // 审计：越权领取 + 越权回传 + 终态重复回传各留一条（协议违规必须可追溯）。
    let msgs = hub.bus_store.replay_messages();
    let violations: Vec<String> = msgs
        .iter()
        .filter(|m| m.correlation_id.starts_with("node:protocol:violation:"))
        .map(|m| m.correlation_id.clone())
        .collect();
    assert_eq!(
        violations.len(),
        3,
        "越权领取 + 越权回传 + 重复回传应各留一条审计：{violations:?}"
    );
    assert!(
        violations
            .iter()
            .filter(|v| { v.starts_with(&format!("node:protocol:violation:node-b:{task_id}")) })
            .count()
            >= 2,
        "node-b 越权应留痕：{violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|v| v.starts_with(&format!("node:protocol:violation:node-a:{task_id}"))),
        "node-a 重复回传应留痕：{violations:?}"
    );
}

/// 10) 租约过期 / 旧 token / 旧 epoch：fencing 拒绝 + 重连恢复 + 审计。
#[tokio::test]
async fn node_protocol_lease_fencing_rejects_old_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let hub = fleet_api::FleetHub::with_lease(
        temp.path(),
        LeaseConfig {
            ttl_secs: 2,
            renew_interval_secs: 1,
        },
    )
    .unwrap();
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/nodes/register",
        Some(&register_body("node-a")),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let reg = body_json(resp).await;
    let token1 = reg["lease_token"].as_str().unwrap().to_string();
    let epoch1 = reg["lease_epoch"].as_u64().unwrap();

    let task_id = new_id("t");
    send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", None)),
    )
    .await;

    // 租约过期：等待超过 TTL。
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/claim"),
        Some(&claim_body("node-a", &token1, epoch1)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "过期租约领取应被拒");

    // 重连 = 重新 acquire（迁移语义：旧租约作废，签发新 token + 新 epoch）。
    let re_acquire = hub.leases.acquire("node-a").unwrap();
    let token2 = re_acquire.token;
    let epoch2 = re_acquire.epoch;
    assert_ne!(token2, token1, "重连应重新签发 token");
    assert!(epoch2 > epoch1, "重连应拿新纪元：{epoch1} → {epoch2}");

    // 旧 token 领取被拒（BadToken）。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/claim"),
        Some(&claim_body("node-a", &token1, epoch1)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "旧 token 应被拒");
    // 旧 epoch（token 新、纪元旧）领取被拒（Fenced）。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/claim"),
        Some(&claim_body("node-a", &token2, epoch1)),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "旧 epoch 应被 fencing 拒绝"
    );
    // 新 token + 新 epoch 领取成功（重连恢复）。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/claim"),
        Some(&claim_body("node-a", &token2, epoch2)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "重连后新租约可领取");

    // 用旧 epoch 回传结果被拒（fencing 拒绝 + 审计）。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/result"),
        Some(&result_body(
            "node-a",
            &token2,
            epoch1,
            true,
            json!("stale"),
        )),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "旧 epoch 回传应被拒");
    // 新 epoch 回传成功 → Succeeded。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/result"),
        Some(&result_body("node-a", &token2, epoch2, true, json!("ok"))),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let view = wait_status(
        hub.clone(),
        &task_id,
        TransportStatus::Succeeded,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(view["status"], "succeeded");

    // 审计：过期/旧 token/旧 epoch 违规均已留痕。
    let msgs = hub.bus_store.replay_messages();
    let violations: Vec<String> = msgs
        .iter()
        .filter(|m| m.correlation_id.starts_with("node:protocol:violation:"))
        .map(|m| m.correlation_id.clone())
        .collect();
    assert!(!violations.is_empty(), "fencing 拒绝应留审计");
}

/// 11) 心跳续租：有效 token 续租成功；旧 token 被拒并审计；越权取消确认被拒。
#[tokio::test]
async fn node_protocol_heartbeat_and_cancel_ack() {
    let (hub, _temp) = test_hub().await;
    for node in ["node-a", "node-b"] {
        let resp = send(
            hub.clone(),
            "POST",
            "/fleet/nodes/register",
            Some(&register_body(node)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let reg_a = body_json(
        send(
            hub.clone(),
            "POST",
            "/fleet/nodes/register",
            Some(&register_body("node-a")),
        )
        .await,
    )
    .await;
    let token_a = reg_a["lease_token"].as_str().unwrap().to_string();
    let epoch_a = reg_a["lease_epoch"].as_u64().unwrap();
    let reg_b = body_json(
        send(
            hub.clone(),
            "POST",
            "/fleet/nodes/register",
            Some(&register_body("node-b")),
        )
        .await,
    )
    .await;
    let token_b = reg_b["lease_token"].as_str().unwrap().to_string();
    let epoch_b = reg_b["lease_epoch"].as_u64().unwrap();

    // 有效 token 心跳续租成功。
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/nodes/node-a/heartbeat",
        Some(&json!({ "lease_token": token_a }).to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let hb = body_json(resp).await;
    assert_eq!(hb["valid"], true);
    assert_eq!(hb["lease_epoch"], epoch_a);

    // 旧 token 心跳被拒（重新注册后旧 token 作废）。
    let resp = send(
        hub.clone(),
        "POST",
        "/fleet/nodes/node-a/heartbeat",
        Some(&json!({ "lease_token": "stale-token" }).to_string()),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "旧 token 心跳应被拒");

    // 提交 + node-a 领取 + 控制面取消 + node-a 确认。
    let task_id = new_id("t");
    send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&task_id, "node-a", None)),
    )
    .await;
    send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/claim"),
        Some(&claim_body("node-a", &token_a, epoch_a)),
    )
    .await;
    send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/cancel"),
        None,
    )
    .await;
    // node-b 越权确认被拒（非领取者 → 403）。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/cancel-ack"),
        Some(
            &json!({
                "node_id": "node-b",
                "lease_token": token_b,
                "epoch": epoch_b,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "非领取者取消确认应被拒"
    );
    // node-a 确认成功 + ack 事件落盘。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{task_id}/cancel-ack"),
        Some(
            &json!({
                "node_id": "node-a",
                "lease_token": token_a,
                "epoch": epoch_a,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ack = body_json(resp).await;
    assert_eq!(ack["acknowledged"], true);
    let view =
        body_json(send(hub.clone(), "GET", &format!("/fleet/tasks/{task_id}"), None).await).await;
    assert!(
        view["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["payload"]["acknowledged"] == true),
        "取消确认事件应落盘"
    );
}

/// 12) 可领取列表：按自身 node_id 匹配；领取后所有权登记；不匹配节点领取被拒。
#[tokio::test]
async fn node_protocol_claimable_matching_and_ownership() {
    let (hub, _temp) = test_hub().await;
    let mut tokens = std::collections::HashMap::new();
    let mut epochs = std::collections::HashMap::new();
    for node in ["node-a", "node-b"] {
        let resp = send(
            hub.clone(),
            "POST",
            "/fleet/nodes/register",
            Some(&register_body(node)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let reg = body_json(resp).await;
        tokens.insert(
            node.to_string(),
            reg["lease_token"].as_str().unwrap().to_string(),
        );
        epochs.insert(node.to_string(), reg["lease_epoch"].as_u64().unwrap());
    }
    let ta = new_id("t");
    let tb = new_id("t");
    send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&ta, "node-a", None)),
    )
    .await;
    send(
        hub.clone(),
        "POST",
        "/fleet/tasks/submit",
        Some(&submit_body(&tb, "node-b", None)),
    )
    .await;

    // node-a 可领取列表只含 ta（自身匹配）。
    let resp = send(hub.clone(), "GET", "/fleet/nodes/node-a/tasks", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    assert_eq!(list["count"], 1, "node-a 只应看到匹配任务");
    assert_eq!(list["tasks"][0]["task_id"], ta);
    assert_eq!(list["tasks"][0]["claimable"], true);

    // node-a 领取 ta 后：claimable=false、claimed_by=node-a。
    send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{ta}/claim"),
        Some(&claim_body("node-a", &tokens["node-a"], epochs["node-a"])),
    )
    .await;
    let resp = send(hub.clone(), "GET", "/fleet/nodes/node-a/tasks", None).await;
    let list = body_json(resp).await;
    assert_eq!(list["tasks"][0]["claimed_by"], "node-a");
    assert_eq!(list["tasks"][0]["claimable"], false);

    // node-b 领取 ta（不匹配任务）→ 403。
    let resp = send(
        hub.clone(),
        "POST",
        &format!("/fleet/tasks/{ta}/claim"),
        Some(&claim_body("node-b", &tokens["node-b"], epochs["node-b"])),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "不匹配节点领取应被拒");
}
