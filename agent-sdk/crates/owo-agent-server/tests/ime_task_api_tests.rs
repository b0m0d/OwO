//! IME admission enters the real Agent/tool/approval loop and retains scope on resume.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use hmac::{Hmac, Mac};
use owo_agent_core::{
    permissions::Policy, sqlite_store::SqliteSessionStore, tools::ToolRegistry, Agent,
};
use owo_agent_protocol::*;
use owo_agent_server::{build_router, AppState};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

struct Writer(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl owo_agent_core::gateway::ModelProvider for Writer {
    async fn complete(
        &self,
        _: &[owo_agent_core::ChatMessage],
        _: &[owo_agent_core::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        if self.0.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
            Ok(owo_agent_core::ModelOutput::ToolCalls(vec![
                owo_agent_core::gateway::ToolCall {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "write_file".into(),
                    arguments: json!({"path":"ime-output.txt","content":"completed through the Agent"}),
                },
            ]))
        } else {
            Ok(owo_agent_core::ModelOutput::Text(
                "fixture task complete".into(),
            ))
        }
    }
}
fn req(state: &AppState, method: &str, path: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:4096")
        .header(
            "authorization",
            format!("Bearer {}", state.auth_token.token()),
        )
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
async fn call(state: &Arc<AppState>, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = build_router(state.clone())
        .oneshot(req(state, method, path, body))
        .await
        .unwrap();
    let status = response.status();
    let raw = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&raw)
            .unwrap_or_else(|_| json!({"error":String::from_utf8_lossy(&raw)})),
    )
}
fn capsule(prompt: &str, key: &str) -> ExpressionOriginCapsule {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../contracts/expression-origin/synthetic-golden.json"
    ))
    .unwrap();
    let mut c: ExpressionOriginCapsule =
        serde_json::from_value(fixture["capsule"].clone()).unwrap();
    c.payload.audience = IME_TASK_ORIGIN_AUDIENCE.into();
    c.payload.scope.mode = IME_TASK_ORIGIN_SCOPE.into();
    c.signature.key_id = IME_TASK_ORIGIN_KEY_ID.into();
    c.payload.binding.request_id = key.into();
    c.payload.nonce = format!("{:x}", Sha256::digest(key));
    c.payload.prompt_sha256 = format!("{:x}", Sha256::digest(prompt));
    c.payload.freshness.issued_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    sign(&mut c);
    c
}
fn sign(c: &mut ExpressionOriginCapsule) {
    let mut mac = Hmac::<Sha256>::new_from_slice(&[7; 32]).unwrap();
    mac.update(IME_TASK_ORIGIN_PURPOSE);
    mac.update(&c.payload.canonical_task_bytes().unwrap());
    c.signature.value = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect();
}
async fn replay(state: &Arc<AppState>, receipt: &Value, approve: bool) -> Value {
    let mut after = 0;
    let mut events = Vec::new();
    for _ in 0..300 {
        let path = format!(
            "/session/{}/turn/events?turn_id={}&after_seq={after}",
            receipt["session_id"].as_str().unwrap(),
            receipt["turn_id"].as_str().unwrap()
        );
        let (status, page) = call(state, "GET", &path, Value::Null).await;
        assert_eq!(status, 200, "{page}");
        for record in page["events"].as_array().unwrap() {
            let event = &record["payload"];
            if event["type"] == "permission_request" && approve {
                let path = format!(
                    "/session/{}/permission/{}",
                    receipt["session_id"].as_str().unwrap(),
                    event["request_id"].as_str().unwrap()
                );
                assert_eq!(
                    call(state, "POST", &path, json!({"allow":true,"scope":"once"}))
                        .await
                        .0,
                    200
                );
            }
            events.push(record.clone());
        }
        after = page["next_after_seq"].as_u64().unwrap();
        if page["active"] == false {
            return json!({"events":events,"state":page["state"]});
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("IME turn did not finish: {events:?}");
}
#[tokio::test]
async fn ime_tasks_use_existing_approvals_persist_identity_and_enforce_live_access() {
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Some(v) = &self.0 {
                std::env::set_var("OWO_EXPRESSION_ORIGIN_KEY", v)
            } else {
                std::env::remove_var("OWO_EXPRESSION_ORIGIN_KEY")
            }
        }
    }
    let _restore = Restore(std::env::var_os("OWO_EXPRESSION_ORIGIN_KEY"));
    std::env::set_var("OWO_EXPRESSION_ORIGIN_KEY", "07".repeat(32));
    let temp = tempfile::tempdir().unwrap();
    let ws = temp.path().join("workspace");
    std::fs::create_dir_all(&ws).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let make_state = || {
        let agent = Agent::new(
            Arc::new(Writer(calls.clone())),
            ToolRegistry::new(),
            Policy::new(&ws),
            Default::default(),
        );
        Arc::new(AppState::new(
            agent,
            SqliteSessionStore::open(&temp.path().join("index.db")).unwrap(),
            temp.path().join("traces"),
            temp.path().into(),
            ws.clone(),
        ))
    };
    let state = make_state();
    let c = capsule("write a fixture file", "ime-one");
    let body = json!({"prompt":"write a fixture file","origin_capsule":c});
    // Missing authentication, expression proof, prompt tampering all fail before model use.
    let anonymous = Request::builder()
        .method("POST")
        .uri("/ime/tasks")
        .header("host", "127.0.0.1:4096")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(
        build_router(state.clone())
            .oneshot(anonymous)
            .await
            .unwrap()
            .status(),
        401
    );
    let mut bad = body.clone();
    bad["prompt"] = json!("tampered");
    assert_eq!(call(&state, "POST", "/ime/tasks", bad).await.0, 403);
    let mut expression = c.clone();
    expression.signature.key_id = EXPRESSION_ORIGIN_KEY_ID.into();
    assert_eq!(
        call(
            &state,
            "POST",
            "/ime/tasks",
            json!({"prompt":"write a fixture file","origin_capsule":expression})
        )
        .await
        .0,
        403
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (status, receipt) = call(&state, "POST", "/ime/tasks", body.clone()).await;
    assert_eq!(status, 200, "{receipt}");
    assert!(receipt["origin_verified"].as_bool().unwrap());
    assert!(
        !ws.join("ime-output.txt").exists(),
        "no write before approval"
    );
    let page = replay(&state, &receipt, true).await;
    assert_eq!(page["state"], "completed", "{page}");
    assert_eq!(
        std::fs::read_to_string(ws.join("ime-output.txt")).unwrap(),
        "completed through the Agent"
    );
    assert!(page["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["payload"]["type"] == "permission_request"));
    let count = calls.load(Ordering::SeqCst);
    let duplicate = call(&state, "POST", "/ime/tasks", body.clone()).await;
    assert_eq!(duplicate.1["session_id"], receipt["session_id"]);
    assert_eq!(calls.load(Ordering::SeqCst), count);
    let restored = make_state();
    let duplicate = call(&restored, "POST", "/ime/tasks", body).await;
    assert_eq!(duplicate.0, 200);
    assert_eq!(duplicate.1["status"], "existing");
    assert_eq!(calls.load(Ordering::SeqCst), count);
    // A new signed capsule cannot rebind an accepted connector request to a different prompt.
    let mut conflict = capsule("another instruction", "ime-one");
    conflict.payload.nonce = "06".repeat(32);
    sign(&mut conflict);
    assert_eq!(
        call(
            &state,
            "POST",
            "/ime/tasks",
            json!({"prompt":"another instruction","origin_capsule":conflict})
        )
        .await
        .0,
        409
    );
    for (index,config) in [
        json!({"mode":"read_only"}),
        json!({"mode":"follow_agent","allowed_tools":["read_file"]}),
        json!({"mode":"custom","permission_spec":{"filesystem":"workspace_read","command":"deny","network":"deny","persistence":"once","scopes":[]}}),
        json!({"mode":"follow_agent","allowed_tools":[]}),
    ].into_iter().enumerate() {
        std::fs::remove_file(ws.join("ime-output.txt")).ok();
        assert_eq!(call(&state,"POST","/settings/ime",config).await.0,200);
        let c=capsule("restricted write",&format!("ime-restricted-{index}"));
        let (status,receipt)=call(&state,"POST","/ime/tasks",json!({"prompt":"restricted write","origin_capsule":c})).await;
        assert_eq!(status,200,"{receipt}");
        let page=replay(&state,&receipt,false).await;
        assert!(!ws.join("ime-output.txt").exists(),"{page}");
        assert!(!page["events"].as_array().unwrap().iter().any(|e|e["payload"]["type"]=="permission_request"));
    }
    // Revoke while a normal approval is pending, then attempt to approve the old request.
    assert_eq!(
        call(
            &state,
            "POST",
            "/settings/ime",
            json!({"mode":"follow_agent"})
        )
        .await
        .0,
        200
    );
    let c = capsule("revoke pending write", "ime-revoke");
    let (_, receipt) = call(
        &state,
        "POST",
        "/ime/tasks",
        json!({"prompt":"revoke pending write","origin_capsule":c}),
    )
    .await;
    for _ in 0..100 {
        if calls.load(Ordering::SeqCst) % 2 == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (status, saved) = call(&state, "POST", "/settings/ime", json!({"mode":"disabled"})).await;
    assert_eq!(status, 200);
    assert!(saved["interrupted_sessions"].as_u64().unwrap() >= 1);
    let page = replay(&state, &receipt, false).await;
    assert_eq!(page["state"], "failed", "{page}");
    assert!(!ws.join("ime-output.txt").exists());
    let resume = format!("/session/{}/turn", receipt["session_id"].as_str().unwrap());
    assert_eq!(
        call(
            &state,
            "POST",
            &resume,
            json!({"prompt":"resume","attachments":[]})
        )
        .await
        .0,
        403
    );
    let c = capsule("disabled task", "ime-disabled");
    assert_eq!(
        call(
            &state,
            "POST",
            "/ime/tasks",
            json!({"prompt":"disabled task","origin_capsule":c})
        )
        .await
        .0,
        403
    );
    // Ordinary Desktop sessions remain independently usable.
    let ordinary = state.store.create(&ws, "fixture", None).unwrap();
    let response = build_router(state.clone())
        .oneshot(req(
            &state,
            "POST",
            &format!("/session/{}/turn", ordinary.id),
            json!({"prompt":"normal","read_only":true}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
}
