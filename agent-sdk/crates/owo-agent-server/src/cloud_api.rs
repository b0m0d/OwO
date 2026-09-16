//! 云端执行 HTTP API（§12：从 lib.rs 机械外移的 M4a /cloud/* 域）。
//!
//! 路由面（`POST /cloud/tasks`、`GET /cloud/tasks/{id}`、`GET /cloud/tasks/{id}/result`、
//! `POST /cloud/tasks/{id}/cancel`、`GET /cloud/tasks/{id}/events`）与 /openapi.json
//! 登记一致；其中 events 端点为本模块补齐的原 OpenAPI 已登记但缺失的实现
//! （SSE：历史重放 + 实时帧，帧由 sse::hub 承载，submit 经 SseHubSink 发布）。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// 懒初始化云端任务队列：传输按环境变量选择
/// （OWO_CLOUD_BASE_URL → HttpTransport；缺省 MockRemoteTransport 本地模拟）。
async fn cloud_queue(
    state: &AppState,
) -> Result<tokio::sync::MutexGuard<'_, Option<owo_agent_core::cloud_exec::CloudTaskQueue>>, String>
{
    let mut guard = state.cloud_queue.lock().await;
    if guard.is_none() {
        let dir = state.data_root.join("cloud").join("queue");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建云端队列目录失败：{e}"))?;
        let transport: Box<dyn owo_agent_core::cloud_exec::CloudTransport> =
            match std::env::var("OWO_CLOUD_BASE_URL") {
                Ok(url) if !url.trim().is_empty() => Box::new(
                    owo_agent_core::cloud_exec::HttpTransport::new(url)
                        .map_err(|e| format!("云端传输初始化失败：{e}"))?,
                ),
                _ => Box::new(owo_agent_core::cloud_exec::MockRemoteTransport::new(
                    state.data_root.join("cloud").join("scratch"),
                )),
            };
        *guard = Some(owo_agent_core::cloud_exec::CloudTaskQueue::new(
            dir, transport,
        ));
    }
    Ok(guard)
}

/// 提交云端任务：`POST /cloud/tasks`（body = CloudTaskSpec；入队后立即执行一轮）。
pub(super) async fn cloud_task_submit(
    State(state): State<Arc<AppState>>,
    Json(spec): Json<owo_agent_core::cloud_exec::CloudTaskSpec>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut guard = cloud_queue(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let queue = guard.as_mut().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "云端队列未初始化".to_string(),
    ))?;
    let task_id = queue
        .submit(spec)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    // 第四轮接线：进度经 SSE 集线器发布，前端以同一 task_id 订阅
    // /cloud/tasks/{id}/events（历史重放 + 实时帧）。
    let sink = owo_agent_server::sse::sink(task_id.clone());
    queue
        .run_next(&sink)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let record = queue
        .record(&task_id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, format!("任务 {task_id} 不存在")))?;
    Ok(Json(
        json!({ "ok": true, "task": record, "transport": queue.transport_kind() }),
    ))
}

/// 查询云端任务：`GET /cloud/tasks/{id}`。
pub(super) async fn cloud_task_status(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let guard = cloud_queue(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let queue = guard.as_ref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "云端队列未初始化".to_string(),
    ))?;
    let record = queue
        .record(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("云端任务 {id} 不存在")))?;
    let usage = queue.usage(&id);
    Ok(Json(json!({
        "state": format!("{:?}", record.state),
        "retry_count": record.retry_count,
        "last_error": record.last_error,
        "created_at": record.created_at,
        "duration_ms": record.duration_ms,
        "usage": usage,
    })))
}

/// 获取云端任务结果：`GET /cloud/tasks/{id}/result`。
pub(super) async fn cloud_task_result(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let guard = cloud_queue(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let queue = guard.as_ref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "云端队列未初始化".to_string(),
    ))?;
    let record = queue
        .record(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("云端任务 {id} 不存在")))?;
    let result = record
        .result
        .clone()
        .ok_or((StatusCode::CONFLICT, format!("云端任务 {id} 尚无结果")))?;
    Ok(Json(
        json!({ "ok": true, "result": result, "diff_summary": owo_agent_core::cloud_exec::describe_diff(&result.diff) }),
    ))
}

/// 取消云端任务：`POST /cloud/tasks/{id}/cancel`。
pub(super) async fn cloud_task_cancel(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut guard = cloud_queue(&state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let queue = guard.as_mut().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "云端队列未初始化".to_string(),
    ))?;
    queue
        .cancel(&id)
        .await
        .map(|_| Json(json!({ "ok": true, "task_id": id })))
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
}
// 注：`GET /cloud/tasks/{id}/events` 的处理器由 sse.rs::router 单一提供
//（lib.rs 仅经 merge(sse::router) 注册一处）。本模块曾补齐一份带 404 校验
// 的实现，与 sse::router 重复注册导致 build_router 启动 panic
//（Overlapping method route，运行时冒烟发现）；按 sse.rs 的 Lane D 设计
// 收敛为单一注册点，严格版实现移除（帧管线 sse::hub/sink 仍由本模块消费）。
