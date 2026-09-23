//! 指南 §2.3：Daemon 发现（`<data_root>/runtime/daemon.json`）的客户端读取端。

use crate::error::{ClientError, Result};
use owo_agent_protocol::DaemonDescriptor;
use std::path::{Path, PathBuf};

/// 发现文件相对 data_root 的路径（与服务端 `discovery::DISCOVERY_RELATIVE_PATH` 一致）。
pub const DISCOVERY_RELATIVE_PATH: &str = "runtime/daemon.json";

/// 解析数据根：`OWO_AGENT_DATA` → `%LOCALAPPDATA%\OwO\Agent` → `data/agent`。
/// 与 CLI `support::data_root` 同口径（客户端不复制第二套业务逻辑，仅复用同一约定）。
pub fn resolve_data_root() -> PathBuf {
    std::env::var("OWO_AGENT_DATA")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("LOCALAPPDATA")
                .ok()
                .map(|dir| PathBuf::from(dir).join("OwO").join("Agent"))
        })
        .unwrap_or_else(|| PathBuf::from("data/agent"))
}

/// 发现文件完整路径。
pub fn descriptor_path(data_root: &Path) -> PathBuf {
    data_root.join("runtime").join("daemon.json")
}

/// 进程存活探测（Windows：OpenProcess；其他：kill(pid, 0)）。
/// 独立实现，保持本 crate 只依赖 protocol。
pub fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        type WinHandle = *mut core::ffi::c_void;
        unsafe extern "system" {
            fn OpenProcess(
                dw_desired_access: u32,
                b_inherit_handle: i32,
                dw_process_id: u32,
            ) -> WinHandle;
            fn GetLastError() -> u32;
            fn SetLastError(dw_err_code: u32);
            fn CloseHandle(h_object: WinHandle) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const ERROR_ACCESS_DENIED: u32 = 5;
        unsafe { SetLastError(0) };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return unsafe { GetLastError() } == ERROR_ACCESS_DENIED;
        }
        unsafe { CloseHandle(handle) };
        true
    }
    #[cfg(not(windows))]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        unsafe { kill(pid as i32, 0) == 0 }
    }
}

/// 一次成功的 Daemon 发现：数据根 + 描述符。
#[derive(Debug, Clone)]
pub struct DaemonDiscovery {
    pub data_root: PathBuf,
    pub descriptor: DaemonDescriptor,
}

impl DaemonDiscovery {
    /// 读取并校验发现文件：缺失/损坏 → 错误；pid 已退出 → `NotFound`（不误连陈旧实例）。
    pub fn read(data_root: &Path) -> Result<Self> {
        let path = descriptor_path(data_root);
        let text = std::fs::read_to_string(&path)
            .map_err(|error| ClientError::NotFound(format!("{}（{error}）", path.display())))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let descriptor: DaemonDescriptor = serde_json::from_str(text)
            .map_err(|error| ClientError::Discovery(format!("{}：{error}", path.display())))?;
        if !process_alive(descriptor.pid) {
            return Err(ClientError::NotFound(format!(
                "发现文件指向已退出进程 pid={}（陈旧发现，需重新启动 Daemon）",
                descriptor.pid
            )));
        }
        Ok(Self {
            data_root: data_root.to_path_buf(),
            descriptor,
        })
    }

    /// 服务基址。
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.descriptor.port)
    }

    /// API 版本兼容校验（不兼容即拒绝连接，禁止静默连到旧实例）。
    pub fn validate_api_version(&self, expected: &str) -> Result<()> {
        if self.descriptor.api_version != expected {
            return Err(ClientError::ApiVersionMismatch {
                expected: expected.to_string(),
                actual: self.descriptor.api_version.clone(),
            });
        }
        Ok(())
    }
}
