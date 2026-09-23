//! 指南 §2.3：Daemon 发现文件（`<data_root>/runtime/daemon.json`）的写入端。
//!
//! 为什么需要：在 `core_ready` stdout 之外，客户端（CLI/TUI/桌面壳）需要一条
//! **可发现、可校验、可原子读取**的通道来找到已运行 Daemon 的端口与身份。
//! 只有 `server.pid` 是不够的——它没有端口，导致"已有 Daemon"的客户端只能自己
//! 另起一个实例（F-01 多入口各自建 Agent 的根因之一）。
//!
//! 契约由 `owo-agent-protocol::DaemonDescriptor` 唯一持有；本模块只负责：
//!   * 原子写入（tmp → rename），避免读到半截 JSON；
//!   * RAII 清理（`DiscoveryFile` Drop 时删除，优雅退出不留陈旧发现文件）；
//!   * 脱敏数据根标识（不把绝对路径写进可被随意读取的发现文件）。
//!
//! 本模块不引用 `crate::`/`super::`，可被测试以 `#[path] mod` 独立编译。

use owo_agent_protocol::DaemonDescriptor;
use std::path::{Path, PathBuf};

/// 发现文件相对 data_root 的路径：`runtime/daemon.json`。
pub const DISCOVERY_RELATIVE_PATH: &str = "runtime/daemon.json";

/// 发现文件完整路径。
pub fn descriptor_path(data_root: &Path) -> PathBuf {
    data_root.join("runtime").join("daemon.json")
}

/// 当前时刻 RFC3339（UTC `Z`）。
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// 数据根脱敏标识：`<目录名>-<sha256 前 12 位>`。
/// 只用于诊断区分不同实例，**不暴露绝对路径**（发现文件可被本机进程读取）。
pub fn mask_data_root(data_root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = data_root
        .canonicalize()
        .unwrap_or_else(|_| data_root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let short = digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = canonical
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    format!("{name}-{short}")
}

/// 已写入的发现文件句柄：Drop 时删除（正常退出清理；强杀残留由启动时 stale 恢复兜底）。
#[derive(Debug)]
pub struct DiscoveryFile {
    path: PathBuf,
    removed: bool,
}

impl DiscoveryFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DiscoveryFile {
    fn drop(&mut self) {
        if !self.removed {
            let _ = std::fs::remove_file(&self.path);
            self.removed = true;
        }
    }
}

/// 原子写入发现文件（tmp → rename）并返回 RAII 句柄。
///
/// Windows 上 `rename` 不能覆盖已存在目标，因此先删除旧文件再 rename；中间窗口极短，
/// 且发现文件是"可重建的运行时状态"（读不到即视为无 Daemon，安全降级）。
pub fn write_descriptor(
    data_root: &Path,
    descriptor: &DaemonDescriptor,
) -> Result<DiscoveryFile, String> {
    let path = descriptor_path(data_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建 runtime 目录失败：{error}"))?;
    }
    let json = serde_json::to_string_pretty(descriptor)
        .map_err(|error| format!("序列化发现描述符失败：{error}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|error| format!("写入临时发现文件失败：{error}"))?;
    let _ = std::fs::remove_file(&path);
    std::fs::rename(&tmp, &path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("提交发现文件失败：{error}")
    })?;
    Ok(DiscoveryFile {
        path,
        removed: false,
    })
}

/// 读取并解析发现文件（容忍 UTF-8 BOM）。缺失/损坏 → Err（调用方按"无 Daemon"处理）。
pub fn read_descriptor(data_root: &Path) -> Result<DaemonDescriptor, String> {
    let path = descriptor_path(data_root);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("读取发现文件失败（{}）：{error}", path.display()))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    serde_json::from_str(text).map_err(|error| format!("解析发现文件失败：{error}"))
}

/// 进程存活探测（Windows：OpenProcess；其他：kill(pid, 0)）。
/// 与 `shutdown::process_alive` 同语义；此处独立实现，避免发现模块反向依赖服务内部。
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

/// stale 恢复：发现文件存在但进程已死 → 删除并返回描述符；进程存活 → 返回描述符（复用）。
pub fn recover_stale(data_root: &Path) -> Option<DaemonDescriptor> {
    let descriptor = read_descriptor(data_root).ok()?;
    if !process_alive(descriptor.pid) {
        let _ = std::fs::remove_file(descriptor_path(data_root));
        return None;
    }
    Some(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DaemonDescriptor {
        DaemonDescriptor {
            pid: 4242,
            port: 4096,
            instance_id: "inst-1".to_string(),
            api_version: "0.7".to_string(),
            build_id: "abc123".to_string(),
            started_at: "2026-09-21T00:00:00Z".to_string(),
            data_root: "Agent-deadbeefcafe".to_string(),
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = std::env::temp_dir().join(format!("owo-disc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let handle = write_descriptor(&dir, &sample()).expect("write");
        assert!(handle.path().is_file());
        let read = read_descriptor(&dir).expect("read");
        assert_eq!(read, sample());
        drop(handle);
        assert!(!descriptor_path(&dir).exists(), "Drop 必须清理发现文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_error_not_panic() {
        let dir = std::env::temp_dir().join(format!("owo-disc-none-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read_descriptor(&dir).is_err());
        assert!(recover_stale(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mask_never_exposes_absolute_path() {
        let masked = mask_data_root(Path::new("C:/Users/someone/AppData/Local/OwO/Agent"));
        assert!(!masked.contains("C:"));
        assert!(!masked.contains("someone"));
        assert!(masked.starts_with("Agent-"));
    }

    #[test]
    fn dead_pid_is_recovered_as_stale() {
        let dir = std::env::temp_dir().join(format!("owo-disc-stale-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut descriptor = sample();
        // pid 0 不可能是存活用户进程；recover_stale 应删除文件并返回 None。
        descriptor.pid = 0;
        let _handle = write_descriptor(&dir, &descriptor).unwrap();
        std::mem::forget(_handle); // 模拟强杀：句柄未 Drop，文件留在盘上
        assert!(recover_stale(&dir).is_none());
        assert!(!descriptor_path(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
