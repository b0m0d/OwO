// R13:fleet_node_protocol 新增（P2 双节点网格第二阶段：真实远端节点协议）
//! 节点协议：真实远端节点 ↔ 控制面的最小结构化协议（HTTP 契约承载于
//! `owo-agent-server/src/fleet_api.rs` 的 /fleet/* 路由）。
//!
//! 设计来源：《多Agent并行体系-生产级设计与跨机扩展-2026-08-16.md》§4.2/§5.2：
//! - **注册/心跳续租（含 fencing epoch）**：节点注册持租约；心跳续租校验 `lease_token`，
//!   旧 token 被拒（见 [`crate::lease::LeaseManager`]）。
//! - **按自身 node_id 领取匹配任务**：`task.worker == node_id` 才可领取；
//!   越权领取/越权回传一律拒绝。
//! - **回传进度/结构化证据/成功失败结果**：进度事件 + 结果事件携带证据与 CAS 引用。
//! - **取消通知与节点确认**：控制面取消后节点回 `cancel-ack` 确认停止。
//! - **断线/租约过期/重连恢复**：租约过期或旧 epoch 的回传被 fencing 拒绝并留审计；
//!   重连走重新注册（re-acquire）拿新 token，旧 token 作废。
//!
//! 安全边界（§5.2）：
//! - **用户模型 Key 永不出本机**：本协议所有消息均不携带、不持久化模型凭据
//!   （`OPENAI_API_KEY` 等），控制面与节点都不落盘。
//! - **mTLS/配对证书未实现**：仅预留 [`NodeAuthHint`] 字段（不伪造已实现）；
//!   生产接入前必须完成证书签发与校验。
//! - 写操作/审批默认 deny：无影响预览 + 结构化证据不得批准（控制面路由层执行）。

use crate::remote_step::EvidenceItem;
use serde::{Deserialize, Serialize};

/// mTLS/配对证书预留字段（**未实现**，仅占位防契约漂移；生产前必须完成证书校验）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NodeAuthHint {
    /// 一次性配对码（预留；当前未启用校验）。
    #[serde(default)]
    pub pairing_code: Option<String>,
    /// 节点证书指纹（预留；当前未启用校验）。
    #[serde(default)]
    pub cert_fingerprint: Option<String>,
}

/// 节点心跳请求（续租；`lease_token` 必须匹配当前租约，旧 token 被拒）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeHeartbeatBody {
    pub lease_token: String,
}

/// 心跳响应：返回最新租约（token + epoch）与建议续租间隔。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeHeartbeatResponse {
    pub node_id: String,
    pub valid: bool,
    pub lease_epoch: u64,
    pub lease_token: String,
    pub renew_interval_secs: u64,
}

/// 节点领取任务请求（fencing 三元组：node_id + token + epoch）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeClaimBody {
    pub node_id: String,
    pub lease_token: String,
    pub epoch: u64,
}

/// 节点进度回传。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProgressBody {
    pub node_id: String,
    pub lease_token: String,
    pub epoch: u64,
    pub text: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceItem>,
}

/// 节点结果回传（成功/失败；输出经 CAS 引用或内联）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeResultBody {
    pub node_id: String,
    pub lease_token: String,
    pub epoch: u64,
    pub ok: bool,
    /// 成功输出（内联 JSON/文本）。
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    /// 输出 CAS 引用（内容寻址；控制面透传引用，不代取内容——产物仍在节点侧）。
    #[serde(default)]
    pub output_cas: Option<String>,
    #[serde(default)]
    pub evidence: Vec<EvidenceItem>,
    #[serde(default)]
    pub error: Option<String>,
}

/// 取消确认请求（节点确认已停止该任务）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCancelAckBody {
    pub node_id: String,
    pub lease_token: String,
    pub epoch: u64,
}

/// 协议违规事件（越权 / 过期 epoch / 不匹配节点；留审计）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProtocolViolation {
    pub node_id: String,
    pub task_id: String,
    pub reason: String,
    pub ts: String,
}

impl NodeProtocolViolation {
    pub fn new(
        node_id: impl Into<String>,
        task_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            task_id: task_id.into(),
            reason: reason.into(),
            ts: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// 协议违规审计幂等键（总线/经验去重）。
pub fn violation_correlation_id(node_id: &str, task_id: &str) -> String {
    format!("node:protocol:violation:{node_id}:{task_id}")
}
