use crate::project_space_store::ProjectSpaceStoreError;

/// WorkSwarm 编排错误（server 层映射：NotFound→404 / Conflict→409 / Validation→400 / 其余→500）。
#[derive(Debug, thiserror::Error)]
pub enum WorkSwarmError {
    #[error("校验失败：{0}")]
    Validation(String),
    #[error("状态冲突：{0}")]
    Conflict(String),
    #[error("未找到：{0}")]
    NotFound(String),
    #[error("存储错误：{0}")]
    Store(#[from] ProjectSpaceStoreError),
    #[error("运行错误：{0}")]
    Run(String),
    #[error("序列化错误：{0}")]
    Serialization(String),
    #[error("IO 错误：{0}")]
    Io(String),
    /// 运行状态文件损坏（明确失败；原文件一律保留，禁止覆盖成新状态）。
    #[error("运行状态损坏：{0}")]
    CorruptState(String),
}

impl From<serde_json::Error> for WorkSwarmError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

pub type WorkSwarmResult<T> = std::result::Result<T, WorkSwarmError>;
