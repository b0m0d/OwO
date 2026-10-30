//! Authenticated IME task admission; execution rights come from desktop settings.
use crate::ExpressionOriginCapsule;
use serde::{Deserialize, Serialize};

pub const IME_TASK_ORIGIN_PURPOSE: &[u8] = b"OwO/ime-task-origin/v1\0";
pub const IME_TASK_ORIGIN_KEY_ID: &str = "gateway-ime-task-v1";
pub const IME_TASK_ORIGIN_AUDIENCE: &str = "owo-daemon-ime-task";
pub const IME_TASK_ORIGIN_SCOPE: &str = "agent_task";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImeTaskRequest {
    pub prompt: String,
    pub origin_capsule: ExpressionOriginCapsule,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImeTaskReceipt {
    pub session_id: String,
    pub turn_id: String,
    pub status: String,
    pub origin_verified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImeSessionSource {
    pub request_key: String,
    pub prompt_sha256: String,
    pub request_sha256: String,
    pub turn_id: String,
}
