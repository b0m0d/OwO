use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("gateway error: {0}")]
    Gateway(String),
    #[error("session error: {0}")]
    Session(String),
    #[error("revert conflict: current files differ from the recorded write: {paths:?}")]
    RevertConflict { paths: Vec<String> },
    #[error("tool error: {0}")]
    Tool(String),
    #[error("agent aborted by user")]
    Aborted,
}
