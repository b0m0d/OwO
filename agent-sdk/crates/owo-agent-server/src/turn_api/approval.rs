use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::permissions::{Approver, Decision, PermissionRequest};
use serde_json::{json, Value};

use owo_agent_server::PendingApproval;
/// `GET /approval/mode`：当前全权限模式状态（界面据此显示开关）。
pub(crate) async fn approval_mode_get() -> Result<Json<Value>, (StatusCode, String)> {
    Ok(Json(json!({
        "auto_approve": auto_approve_enabled(),
        "source": if runtime_auto_approve_path()
            .map(|path| path.is_file())
            .unwrap_or(false)
        {
            "runtime_file"
        } else {
            "environment"
        },
    })))
}

#[derive(serde::Deserialize)]
pub(crate) struct ApprovalModeRequest {
    pub auto_approve: bool,
}

/// `POST /approval/mode`：运行时切换全权限模式。
///
/// 语义（单一事实源 = 开关文件）：
///   `true`  → 所有工具调用**自动放行**，不再弹审批；
///   `false` → 立即恢复逐次审批（无需重启）。
/// 这是用户显式要求的"全权限模式"；页面上必须同时给出可见的风险提示，
/// 并且**只影响审批网关**（密码/支付/验证码锚点熔断与 inject 级策略不受影响）。
pub(crate) async fn approval_mode_set(
    Json(request): Json<ApprovalModeRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let Some(path) = runtime_auto_approve_path() else {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法确定数据目录（OWO_AGENT_DATA 未设置），无法持久化全权限开关".to_string(),
        ));
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("创建数据目录失败：{error}"),
            )
        })?;
    }
    let payload = json!({
        "auto_approve": request.auto_approve,
        "updated_at": owo_agent_server::discovery::now_rfc3339(),
    });
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    std::fs::write(&path, text).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("写入开关文件失败：{error}"),
        )
    })?;
    Ok(Json(json!({
        "ok": true,
        "auto_approve": auto_approve_enabled(),
        "path": path.to_string_lossy(),
    })))
}

/// 全权限模式开关文件（运行时可切换，不需要重启核心）。
///
/// 为什么做成文件而不是只认环境变量：用户要的是"在输入框下面随时改"。环境变量是
/// 进程启动期的快照，改它必须重启核心（会打断正在跑的回合）。这里约定：
///   `<data_root>/approval.json` = `{ "auto_approve": true }`
/// 每次审批请求都重新读一次——审批本身是低频事件，读一个小文件的开销可忽略，
/// 换来的是"界面点一下立刻生效、零重启"。
///
/// 安全边界（如实写在这里，避免以后有人误以为它是万能后门）：
///   * 只影响**审批网关**。密码/支付/验证码类锚点的熔断在 core 的策略层，不经过这里；
///   * `inject`（注入）级别动作仍然走策略层判定，不受本开关影响；
///   * 关闭时立即回到"逐次审批"，无需重启。
pub(crate) fn runtime_auto_approve_path() -> Option<std::path::PathBuf> {
    std::env::var_os("OWO_AGENT_DATA")
        .map(std::path::PathBuf::from)
        .map(|root| root.join("approval.json"))
}

/// 全权限模式是否开启：**开关文件优先**（运行时），其次环境变量（部署级默认）。
pub(crate) fn auto_approve_enabled() -> bool {
    if let Some(path) = runtime_auto_approve_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(flag) = value.get("auto_approve").and_then(|flag| flag.as_bool()) {
                    return flag;
                }
            }
        }
    }
    std::env::var("OWO_AUTO_APPROVE")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub(crate) struct ChannelApprover {
    pub(crate) pending: Arc<Mutex<HashMap<String, PendingApproval>>>,
    pub(crate) pending_sessions: Arc<Mutex<HashMap<String, String>>>,
    pub(crate) session_id: String,
    pub(crate) abort: Arc<AtomicBool>,
}

impl ChannelApprover {
    fn spawn_request(
        &self,
        request: &PermissionRequest,
    ) -> tokio::sync::oneshot::Receiver<Decision> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(request.request_id.clone(), (tx, request.clone()));
        }
        if let Ok(mut sessions) = self.pending_sessions.lock() {
            sessions.insert(request.request_id.clone(), self.session_id.clone());
        }
        rx
    }
}

#[async_trait::async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        if auto_approve_enabled() {
            return Decision::Allow;
        }
        let rx = self.spawn_request(request);
        let mut rx = rx;
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(300));
        tokio::pin!(deadline);
        let decision = loop {
            tokio::select! {
                result = &mut rx => break result.unwrap_or(Decision::Deny),
                _ = &mut deadline => break Decision::Deny,
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                    if self.abort.load(Ordering::Relaxed) {
                        break Decision::Deny;
                    }
                }
            }
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request.request_id);
        }
        if let Ok(mut sessions) = self.pending_sessions.lock() {
            sessions.remove(&request.request_id);
        }
        decision
    }
}
