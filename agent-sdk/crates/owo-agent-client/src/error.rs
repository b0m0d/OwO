//! 客户端结构化错误（指南 §4.6：UI 按 code/action 决策，不解析中文文本）。
//!
//! 这里先给出客户端侧的分类；服务端错误体的 `code` 已随 `ClientError::Status`
//! 的 body 原样保留，供上层按需提取。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClientError {
    /// 没有可用 Daemon（发现文件缺失 / 进程已退出）。
    #[error("未找到运行中的 Daemon：{0}")]
    NotFound(String),
    /// 发现文件存在但内容不可解析或字段非法。
    #[error("Daemon 发现信息无效：{0}")]
    Discovery(String),
    /// 认证凭据不可用（token 文件缺失/为空）。
    #[error("认证凭据不可用：{0}")]
    Auth(String),
    /// 网络/传输层失败。
    #[error("HTTP 传输失败：{0}")]
    Transport(String),
    /// 服务端返回非 2xx（body 原样保留，含稳定 code）。
    #[error("服务端返回 {status}：{body}")]
    Status { status: u16, body: String },
    /// 协议解析失败（JSON/SSE 帧损坏）。
    #[error("协议解析失败：{0}")]
    Protocol(String),
    /// API 版本不兼容（拒绝静默连接错误实例，§2.3 规则 7）。
    #[error("API 版本不兼容：期望 {expected}，实际 {actual}")]
    ApiVersionMismatch { expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, ClientError>;
