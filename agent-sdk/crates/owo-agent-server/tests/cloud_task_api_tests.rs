//! Cloud task API concurrency contracts: durable submit snapshots and lock-free reads/cancel signal.
#[path = "../src/cloud_api.rs"]
mod cloud_api;

use axum::{
    extract::{Path, State},
    Json,
};
use owo_agent_core::cloud_exec::{CloudTaskQueue, CloudTaskSpec, MockRemoteTransport};
use owo_agent_server::AppState;
use serde_json::Value;
use std::sync::Arc;

struct IdleProvider;

#[async_trait::async_trait]
impl owo_agent_core::gateway::ModelProvider for IdleProvider {
    async fn complete(
        &self,
        _messages: &[owo_agent_core::ChatMessage],
        _tools: &[owo_agent_core::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        Err("IdleProvider should not be called".to_string())
    }
}

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = owo_agent_core::Agent::new(
        Arc::new(IdleProvider),
        owo_agent_core::tools::ToolRegistry::new(),
        owo_agent_core::permissions::Policy::new(&workspace),
        Default::default(),
    );
    let store = owo_agent_core::sqlite_store::SqliteSessionStore::open(&workspace.join("index.db"))
        .unwrap();
    let state = Arc::new(AppState::new(
        agent,
        store,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    (state, temp)
}

async fn persisted_queued_task(state: &AppState, temp: &tempfile::TempDir) -> String {
    let workspace = temp.path().join("task-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut queue = CloudTaskQueue::new(
        state.data_root.join("cloud").join("queue"),
        Box::new(MockRemoteTransport::new(
            state.data_root.join("cloud").join("scratch"),
        )),
    );
    let task_id = queue
        .submit(CloudTaskSpec {
            name: "persisted status read".to_string(),
            workspace_dir: workspace,
            commands: vec!["echo ok".to_string()],
            env_passthrough: Vec::new(),
            timeout_secs: 10,
        })
        .unwrap();
    *state.cloud_queue.lock().await = Some(queue);
    task_id
}

#[tokio::test]
async fn status_and_result_reads_do_not_wait_for_queue_execution_lock() {
    let (state, temp) = test_state().await;
    let task_id = persisted_queued_task(&state, &temp).await;
    let queue_guard = state.cloud_queue.lock().await;

    let status = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        cloud_api::cloud_task_status(State(Arc::clone(&state)), Path(task_id.clone())),
    )
    .await
    .expect("status must read the persisted snapshot without queue lock")
    .unwrap();
    assert_eq!(status.0["state"], "Queued");

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        cloud_api::cloud_task_result(State(Arc::clone(&state)), Path(task_id)),
    )
    .await
    .expect("result must read the persisted snapshot without queue lock")
    .unwrap_err();
    assert_eq!(result.0, axum::http::StatusCode::CONFLICT);
    drop(queue_guard);
}

#[tokio::test]
async fn active_cancel_only_signals_runner_and_does_not_wait_for_queue_lock() {
    let (state, temp) = test_state().await;
    let task_id = persisted_queued_task(&state, &temp).await;
    let (signal, receiver) = tokio::sync::watch::channel(false);
    state
        .cloud_cancel_signals
        .lock()
        .await
        .insert(task_id.clone(), signal);
    let queue_guard = state.cloud_queue.lock().await;

    let response = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        cloud_api::cloud_task_cancel(State(Arc::clone(&state)), Path(task_id.clone())),
    )
    .await
    .expect("active cancellation must not wait for the queue lock")
    .unwrap();
    assert_eq!(response.0["state"], "cancel_requested");
    assert!(*receiver.borrow());
    drop(queue_guard);
}

#[tokio::test]
async fn submit_persists_and_returns_while_execution_queue_lock_is_held() {
    if std::env::var("OWO_CLOUD_BASE_URL").is_ok() {
        // Avoid sending this fixture to a user-configured remote transport.
        return;
    }
    let (state, temp) = test_state().await;
    let workspace = temp.path().join("submit-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let queue_guard = state.cloud_queue.lock().await;

    let response = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        cloud_api::cloud_task_submit(
            State(Arc::clone(&state)),
            Json(CloudTaskSpec {
                name: "fast durable submit".to_string(),
                workspace_dir: workspace,
                commands: vec!["echo ok".to_string()],
                env_passthrough: Vec::new(),
                timeout_secs: 5,
            }),
        ),
    )
    .await
    .expect("submit must not wait for the execution queue lock")
    .unwrap();
    let body: Value = response.0;
    assert_eq!(body["task"]["state"], "Queued");
    let task_id = body["task"]["task_id"].as_str().unwrap().to_string();
    let record_path = temp
        .path()
        .join("cloud")
        .join("queue")
        .join(format!("{task_id}.json"));
    assert!(
        record_path.is_file(),
        "submit responds only after durable enqueue"
    );
    drop(queue_guard);

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let record: Value =
                serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
            if record["state"] == "Succeeded" || record["state"] == "Failed" {
                assert_eq!(record["state"], "Succeeded");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background runner should process the persisted task after lock release");
}
