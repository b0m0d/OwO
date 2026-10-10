//! A8 活跃回合快照 / 桌宠显隐中转 / 跨会话待审批（取优合并自远端 engine）。
//!
//! 路由面：`GET /activity`、`GET/POST /desktop/pet`、`POST /desktop/pet/report`、
//! `GET /approvals/pending`；与 /openapi.json 登记保持一致。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

use owo_agent_server::AppState;

/// 模块内 `poison` 副本（§12 惯例：共享根助手保持各域独立可用）。
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

// ===== A8-2 活跃回合快照（/activity） =====
// 桌宠等外部进度面板的只读数据源：哪个会话在跑、什么阶段、是否有待审批。
// 快照只存最小状态；会话标题等展示信息由 /activity handler 现取。

pub fn begin_activity(state: &AppState, session_id: &str) {
    let Ok(mut activities) = state.activities.lock() else {
        return;
    };
    let now = chrono::Utc::now().to_rfc3339();
    activities.insert(
        session_id.to_string(),
        json!({
            "session_id": session_id,
            "phase": "starting",
            "tool": Value::Null,
            "started_at": now,
            "updated_at": now,
        }),
    );
}

pub fn end_activity(state: &AppState, session_id: &str) {
    if let Ok(mut activities) = state.activities.lock() {
        activities.remove(session_id);
    }
}

pub fn update_activity(state: &AppState, session_id: &str, event: &owo_agent_core::TurnEvent) {
    let Ok(mut activities) = state.activities.lock() else {
        return;
    };
    update_activity_snapshot(&mut activities, session_id, event);
}

fn update_activity_snapshot(
    activities: &mut std::collections::HashMap<String, Value>,
    session_id: &str,
    event: &owo_agent_core::TurnEvent,
) {
    use owo_agent_core::TurnEvent;
    let now = chrono::Utc::now().to_rfc3339();
    let touch = |activities: &mut std::collections::HashMap<String, Value>, phase: &str| {
        let entry = activities
            .entry(session_id.to_string())
            .or_insert_with(|| json!({ "session_id": session_id, "tool": Value::Null }));
        entry["phase"] = json!(phase);
        entry["updated_at"] = json!(now);
        entry["tool"] = Value::Null;
        if let Some(object) = entry.as_object_mut() {
            object.remove("request_id");
            object.remove("reason");
        }
        if entry.get("started_at").is_none() {
            entry["started_at"] = json!(now);
        }
    };
    match event {
        TurnEvent::ModelCall => touch(activities, "thinking"),
        TurnEvent::TokenDelta { .. } | TurnEvent::ReasoningDelta { .. } => {
            touch(activities, "speaking");
        }
        TurnEvent::ToolStart { tool, .. } => {
            touch(activities, "tool");
            if let Some(entry) = activities.get_mut(session_id) {
                entry["tool"] = json!(tool);
            }
        }
        TurnEvent::PermissionRequest(request) => {
            touch(activities, "waiting_approval");
            if let Some(entry) = activities.get_mut(session_id) {
                entry["tool"] = json!(request.tool);
                entry["request_id"] = json!(request.request_id);
                entry["reason"] = json!(request.reason);
            }
        }
        TurnEvent::ToolResult { .. } | TurnEvent::Compaction { .. } => {
            touch(activities, "thinking");
        }
        // Final is model output; persistence/audit and host completion are still running.
        TurnEvent::Final { .. } => touch(activities, "persisting"),
        _ => {}
    }
}

/// `GET /activity`——活跃回合快照（附会话标题/工作区 + 待审批计数）。
pub(super) async fn activity_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let mut list: Vec<Value> = {
        let activities = state.activities.lock().map_err(poison)?;
        activities.values().cloned().collect()
    };
    for item in list.iter_mut() {
        if let Some(sid) = item.get("session_id").and_then(Value::as_str) {
            // 会话标题/工作区现取（内存未见则从存储加载）。
            let session = state
                .sessions
                .lock()
                .ok()
                .and_then(|sessions| sessions.get(sid).cloned())
                .or_else(|| state.store.load(sid).ok());
            if let Some(session) = session {
                item["title"] = json!(session.display_title());
                item["workspace"] = json!(session.workspace.to_string_lossy());
            }
        }
    }
    let pending = state.pending_approvals.lock().map_err(poison)?.len();
    Ok(Json(json!({
        "active": list,
        "pending_approvals": pending,
    })))
}

// ===== A8-3 桌宠显隐中转（/desktop/pet） =====

/// 桌宠状态 JSON（`overlay_online` = 最近 15 秒收到过桌面端心跳）。
fn pet_state_json(state: &AppState) -> Value {
    let pet = match state.pet_state.lock() {
        Ok(pet) => pet,
        Err(poisoned) => poisoned.into_inner(),
    };
    let online = pet
        .actual_seen
        .map(|seen| seen.elapsed() < Duration::from_secs(15))
        .unwrap_or(false);
    json!({
        "desired": pet.desired,
        "desired_at": pet.desired_at,
        "actual": pet.actual,
        "actual_at": pet.actual_at,
        "overlay_online": online,
    })
}

/// `GET /desktop/pet`——桌宠显隐状态（工作台读开关与在线情况）。
/// 桌面端在线且实际值与期望值不一致时，以实际值为准回写期望（用户可能直接在
/// 桌宠/面板上切换过；避免下次桌面端启动被旧期望值"复活"）。
pub(super) async fn pet_state_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    {
        if let Ok(mut pet) = state.pet_state.lock() {
            let online = pet
                .actual_seen
                .map(|seen| seen.elapsed() < Duration::from_secs(15))
                .unwrap_or(false);
            if online {
                if let Some(actual) = pet.actual {
                    // 只有「实际值比期望值更新」才回写（用户在桌宠/面板上直接改过）；
                    // 期望值更新说明工作台刚写入、桌面端尚未跟进，**不能**回写，
                    // 否则命令会被立即吞掉（已实测开关拨不动）。
                    let actual_at = pet
                        .actual_at
                        .as_deref()
                        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
                    let desired_at = pet
                        .desired_at
                        .as_deref()
                        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok());
                    let actual_is_newer = match (actual_at, desired_at) {
                        (Some(actual_at), Some(desired_at)) => actual_at > desired_at,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    if pet.desired != Some(actual) && actual_is_newer {
                        pet.desired = Some(actual);
                        pet.desired_at = Some(chrono::Utc::now().to_rfc3339());
                    }
                }
            }
        }
    }
    Json(pet_state_json(&state))
}

#[derive(serde::Deserialize)]
pub(super) struct PetVisibleRequest {
    visible: bool,
}

/// `POST /desktop/pet`——工作台开关写入期望显隐；桌面端下一轮心跳应用。
pub(super) async fn pet_state_set(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PetVisibleRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    {
        let mut pet = state.pet_state.lock().map_err(poison)?;
        pet.desired = Some(request.visible);
        pet.desired_at = Some(chrono::Utc::now().to_rfc3339());
    }
    Ok(Json(pet_state_json(&state)))
}

/// `POST /desktop/pet/report`——桌面端心跳：上报实际显隐，回传期望值
/// （桌面端据此做差异化同步：期望 ≠ 实际时立即切换）。
pub(super) async fn pet_state_report(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PetVisibleRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let desired = {
        let mut pet = state.pet_state.lock().map_err(poison)?;
        pet.actual = Some(request.visible);
        pet.actual_at = Some(chrono::Utc::now().to_rfc3339());
        pet.actual_seen = Some(std::time::Instant::now());
        pet.desired
    };
    Ok(Json(json!({ "desired": desired })))
}

// ===== 跨会话待审批（/approvals/pending） =====

/// 跨会话待审批列表：多对话并行时，其他会话的审批卡也要对用户可见可响应
/// （否则会话 A 等审批期间用户停在会话 B，A 只能等 300s 超时被拒）。
pub(super) async fn pending_approvals_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let pending = state.pending_approvals.lock().map_err(poison)?;
    let sessions = state.pending_approval_sessions.lock().map_err(poison)?;
    let items: Vec<Value> = pending
        .iter()
        .map(|(request_id, (_sender, request))| {
            json!({
                "request_id": request_id,
                "session_id": sessions.get(request_id),
                "tool": request.tool,
                "args_summary": request.args.to_string().chars().take(120).collect::<String>(),
                "reason": request.reason,
                "level": request.level.label(),
            })
        })
        .collect();
    Ok(Json(json!({ "count": items.len(), "pending": items })))
}

#[cfg(test)]
mod activity_progress_tests {
    use super::*;
    #[test]
    fn model_final_stays_active_and_clears_stale_tool_details() {
        let mut activities = std::collections::HashMap::new();
        update_activity_snapshot(
            &mut activities,
            "session",
            &owo_agent_core::TurnEvent::ToolStart {
                id: "tool-1".into(),
                tool: "read_file".into(),
                args_preview: None,
            },
        );
        assert_eq!(activities["session"]["phase"], "tool");
        activities.get_mut("session").unwrap()["request_id"] = json!("old-approval");
        activities.get_mut("session").unwrap()["reason"] = json!("old-reason");
        update_activity_snapshot(
            &mut activities,
            "session",
            &owo_agent_core::TurnEvent::Final {
                text: "output".into(),
            },
        );
        assert_eq!(activities["session"]["phase"], "persisting");
        assert!(activities["session"]["tool"].is_null());
        assert!(activities["session"].get("request_id").is_none());
        assert!(activities["session"].get("reason").is_none());
        assert!(activities["session"].get("started_at").is_some());
    }
}
