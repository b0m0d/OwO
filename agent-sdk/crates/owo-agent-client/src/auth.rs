//! 认证：token 文件发现通道 + `/auth/token` 引导（指南 §2.3 规则 4：token 不进发现文件）。

use crate::error::{ClientError, Result};
use std::path::{Path, PathBuf};

/// token 文件路径：`<data_root>/auth/token`。
pub fn token_path(data_root: &Path) -> PathBuf {
    data_root.join("auth").join("token")
}

/// 读取当前代际 token（文件即发现通道；trim 后为空视为不可用）。
pub fn read_token(data_root: &Path) -> Result<String> {
    let path = token_path(data_root);
    let token = std::fs::read_to_string(&path)
        .map_err(|error| ClientError::Auth(format!("{}（{error}）", path.display())))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(ClientError::Auth(format!("{} 为空", path.display())));
    }
    Ok(token)
}

/// 经 `GET /auth/token` 引导取 token（开发/桌面配对路径）。
///
/// 发布桌面子进程若要求配对证明/实例身份，调用方必须传入对应值；缺失即 403。
pub async fn bootstrap_token(
    base_url: &str,
    pairing_secret: Option<&str>,
    instance_id: Option<&str>,
) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct TokenBody {
        token: String,
    }
    let client = reqwest::Client::new();
    let mut request = client.get(format!("{}/auth/token", base_url.trim_end_matches('/')));
    if let Some(secret) = pairing_secret {
        request = request.header("x-owo-desktop-pairing", secret);
    }
    if let Some(instance) = instance_id {
        request = request.header("x-owo-desktop-instance", instance);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ClientError::Transport(error.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| ClientError::Transport(error.to_string()))?;
    if !status.is_success() {
        return Err(ClientError::Status {
            status: status.as_u16(),
            body: text,
        });
    }
    let body: TokenBody =
        serde_json::from_str(&text).map_err(|error| ClientError::Protocol(error.to_string()))?;
    Ok(body.token)
}
