//! 本地 API 鉴权（R7 X03）：随机 bearer token + 文件持久化 + 用户级 ACL。
//!
//! - 启动时生成 256 位随机 token（两个 uuid v4 拼接），写入 `<data_root>/auth/token`；
//!   已存在有效 token 时复用（桌面端重启不失效）。
//! - Windows 下用 `icacls` 把文件 ACL 收紧为仅当前用户（`/inheritance:r /grant:r`）；
//!   ACL 应用失败时优雅降级（返回 `acl_warning`），服务仍可运行，由审计记录提示。
//! - 校验为恒定时间比较（长度不匹配同样消耗相同指令量，不提前返回）。
//! - 前端引导：开发/浏览器调试时 `GET /auth/token` 仍可公开读取；发布桌面壳会
//!   注入一次进程配对证明，核心仅接受带正确证明的引导请求。
//!
//! 本模块不引用 `crate::`/`super::`（AppState 全限定），可被测试以
//! `#[path] mod` 独立编译。

// 与 team_api.rs 同款模块级 allow(dead_code)：lib 目标经 build_router 使用
// require_auth/bootstrap；#[path] 独立编译的测试目标内中间件未被调用，
// 避免 clippy -D warnings 在测试目标误报。
#![allow(dead_code)]

use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use owo_agent_server::AppState;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// token 文件名（相对 data_root/auth）。
pub const TOKEN_FILE_NAME: &str = "token";
/// Authorization 头名。
pub const AUTH_HEADER: &str = "authorization";
/// Bearer 前缀。
pub const BEARER_PREFIX: &str = "Bearer ";
/// 桌面壳与其核心子进程之间的一次进程配对证明头。
pub const DESKTOP_PAIRING_HEADER: &str = "x-owo-desktop-pairing";
/// 发布桌面壳传给核心子进程的配对证明环境变量。
pub const DESKTOP_PAIRING_ENV: &str = "OWO_DESKTOP_PAIRING_SECRET";
/// 桌面壳注入的实例身份头（§4.2：壳只认自己启动的核心实例）。
pub const DESKTOP_INSTANCE_HEADER: &str = "x-owo-desktop-instance";
/// 发布桌面壳传给核心子进程的实例身份环境变量。
pub const DESKTOP_INSTANCE_ENV: &str = "OWO_DESKTOP_INSTANCE_ID";

/// 无需鉴权的公开端点（健康/OpenAPI/引导）。
pub fn is_public_path(path: &str) -> bool {
    matches!(path, "/health" | "/openapi.json" | "/auth/token")
}

/// 发布桌面子进程是否要求 `/auth/token` 附带进程配对证明。
///
/// 仅在壳显式注入非空随机证明时收紧：开发时直接运行 `owo-agent serve`
/// 仍可进行浏览器调试，也不会因为旧客户端没有该头而误锁本地服务。
pub fn desktop_pairing_secret() -> Option<String> {
    std::env::var(DESKTOP_PAIRING_ENV)
        .ok()
        .map(|secret| secret.trim().to_string())
        .filter(|secret| secret.len() >= 32)
}

/// 使用恒定时间比较验证桌面壳注入的配对证明。
pub fn verify_desktop_pairing(value: Option<&HeaderValue>, expected: &str) -> bool {
    value
        .and_then(|header| header.to_str().ok())
        .map(|provided| constant_time_eq(expected.as_bytes(), provided.as_bytes()))
        .unwrap_or(false)
}

/// 引导门控判定（纯函数，供测试直接验证全矩阵）：
/// - 无配对证明（开发模式）→ 放行（浏览器调试兼容）；
/// - 有配对证明 → 请求头必须恒定时间精确匹配，否则拒绝。
pub fn pairing_gate_allows(expected_secret: Option<&str>, provided: Option<&HeaderValue>) -> bool {
    match expected_secret {
        None => true,
        Some(secret) => verify_desktop_pairing(provided, secret),
    }
}

/// 桌面壳注入的实例身份（trim 后非空才采纳；未注入 = 开发模式）。
pub fn desktop_instance_id() -> Option<String> {
    std::env::var(DESKTOP_INSTANCE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// 使用恒定时间比较验证实例身份头。
pub fn verify_desktop_instance(value: Option<&HeaderValue>, expected: &str) -> bool {
    value
        .and_then(|header| header.to_str().ok())
        .map(|provided| constant_time_eq(expected.as_bytes(), provided.as_bytes()))
        .unwrap_or(false)
}

/// 实例身份门控判定（纯函数，与 pairing_gate_allows 同构）：
/// - 未注入实例身份（开发模式）→ 放行；
/// - 已注入 → 请求头必须恒定时间精确匹配，否则拒绝。
///
/// 用于 `/auth/token` 引导与 `/server/shutdown` 关闭请求：任何不属于当前
/// 桌面实例的调用方都不得引导取 token，更不得关闭服务。
pub fn instance_gate_allows(expected: Option<&str>, provided: Option<&HeaderValue>) -> bool {
    match expected {
        None => true,
        Some(instance) => verify_desktop_instance(provided, instance),
    }
}

/// 仅列出实际存在的只读 SSE 路由；不能用“任意 /events 后缀”放大豁免面。
/// 带鉴权的 fetch-stream 客户端仍会携带 Bearer，公开 EventSource 仅用于兼容旧客户端。
pub fn is_sse_path(path: &str) -> bool {
    path == "/events/stream"
        || (path.starts_with("/cloud/tasks/") && path.ends_with("/events"))
        || (path.starts_with("/workflow/run/") && path.ends_with("/events"))
        || (path.starts_with("/teams/") && path.ends_with("/events"))
        || (path.starts_with("/fleet/tasks/") && path.ends_with("/events"))
}

/// 本地 API bearer token。
#[derive(Debug, Clone)]
pub struct AuthToken {
    token: String,
    /// ACL 应用是否失败（降级运行，审计提示）。
    acl_warning: Option<String>,
}

impl AuthToken {
    /// 生成新随机 token（256 位 hex）。
    pub fn generate() -> Self {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        Self {
            token,
            acl_warning: None,
        }
    }

    /// token 文件路径：`<data_root>/auth/token`。
    pub fn file_path(data_root: &Path) -> PathBuf {
        data_root.join("auth").join(TOKEN_FILE_NAME)
    }

    /// 从文件加载或创建：
    /// - 文件存在且非空 → 复用；
    /// - 否则生成新 token 并持久化 + 收紧 ACL；
    /// - 任何 IO 失败都不 panic：回退到内存 token（`persisted=false`），并带警告。
    pub fn load_or_create(data_root: &Path) -> Self {
        let path = Self::file_path(data_root);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let token = content.trim().to_string();
            if !token.is_empty() && token.len() >= 32 {
                return Self {
                    token,
                    acl_warning: None,
                };
            }
        }
        let fresh = Self::generate();
        match Self::persist(&path, &fresh.token) {
            Ok(()) => fresh,
            Err(warning) => Self {
                token: fresh.token,
                acl_warning: Some(warning),
            },
        }
    }

    /// 写入 token 文件（UTF-8 无 BOM）+ Windows 用户级 ACL。
    fn persist(path: &Path, token: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建 auth 目录失败：{e}"))?;
        }
        std::fs::write(path, token).map_err(|e| format!("写入 token 文件失败：{e}"))?;
        apply_user_only_acl(path).map_err(|e| format!("ACL 收紧失败（降级运行）：{e}"))
    }

    /// 当前 token 值。
    pub fn token(&self) -> &str {
        &self.token
    }

    /// ACL 降级警告（无警告为 None）。
    pub fn acl_warning(&self) -> Option<&str> {
        self.acl_warning.as_deref()
    }

    /// 恒定时间校验 bearer 头（`Authorization: Bearer <token>`）。
    pub fn verify_header(&self, value: Option<&HeaderValue>) -> bool {
        let Some(value) = value else {
            return false;
        };
        let Ok(value) = value.to_str() else {
            return false;
        };
        let Some(provided) = value.strip_prefix(BEARER_PREFIX) else {
            return false;
        };
        self.verify(provided)
    }

    /// 恒定时间校验裸 token。
    pub fn verify(&self, provided: &str) -> bool {
        constant_time_eq(self.token.as_bytes(), provided.as_bytes())
    }
}

/// 恒定时间比较：长度不同也扫描等长窗口，不提前返回。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for i in 0..len {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        diff |= (av ^ bv) as usize;
    }
    diff == 0
}

/// Windows：把 token 文件 ACL 收紧为仅当前用户（继承全部移除）。
/// 非 Windows 平台返回 Ok（占位）。
#[cfg(windows)]
pub fn apply_user_only_acl(path: &Path) -> Result<(), String> {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "".to_string());
    if user.is_empty() {
        return Err("无法确定当前用户名（USERNAME 环境变量缺失）".to_string());
    }
    let output = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{user}:(F)"))
        .output()
        .map_err(|e| format!("icacls 启动失败：{e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "icacls 失败（{}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// 非 Windows 占位：文件权限由宿主系统默认策略管理。
#[cfg(not(windows))]
pub fn apply_user_only_acl(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// 鉴权中间件：无 token 的 API 请求 → 401。
/// 豁免：公开端点、SSE 资源型路径（`/…/events`，EventSource 无法带自定义头）。
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if is_public_path(&path) || is_sse_path(&path) {
        return next.run(request).await;
    }
    let authorized = {
        let auth = &state.auth_token;
        auth.verify_header(request.headers().get(AUTH_HEADER))
    };
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "未授权：缺少或无效的 bearer token（GET /auth/token 获取）",
                "code": "auth/unauthorized/not_retryable",
            })),
        )
            .into_response()
    }
}

/// 引导端点：`GET /auth/token` → `{ token }`。
///
/// 开发服务保持本地浏览器可调试；发布桌面子进程存在配对证明时，只有桌面
/// WebView 通过 Tauri IPC 取得的证明才能读取 bearer token。
pub async fn auth_token_bootstrap(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !pairing_gate_allows(
        desktop_pairing_secret().as_deref(),
        headers.get(DESKTOP_PAIRING_HEADER),
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "桌面进程配对证明缺失或无效",
                "code": "auth/pairing_required/not_retryable",
            })),
        )
            .into_response();
    }
    // §4.2 实例身份门控：注入了实例身份时，只有同一桌面实例的引导请求可取 token，
    // 防"旧核心仍在 + 新壳新密钥"组合下的静默 403 空壳。
    if !instance_gate_allows(
        desktop_instance_id().as_deref(),
        headers.get(DESKTOP_INSTANCE_HEADER),
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "桌面实例身份不匹配：该核心服务属于另一个桌面实例",
                "code": "auth/instance_mismatch/not_retryable",
            })),
        )
            .into_response();
    }
    Json(json!({ "token": state.auth_token.token() })).into_response()
}
