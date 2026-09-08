//! §4.1 单实例锁与唤回通道：Windows 命名管道独占 + 第二实例唤回消息。
//!
//! - 主实例：创建命名管道 `\\.\pipe\OwO-Agent-Desktop` 取得独占；内部监听线程
//!   循环接收第二实例的 `show` 请求并执行已注册的唤回回调。
//! - 第二实例：连接管道写入 `show` 即退出（`WokeExisting`，系统始终只有一个壳）。
//! - 错误分类：安全/权限拒绝 → `PermissionDenied`；其余系统错误 → `Unexpected`；
//!   两者都要求调用方 `report_fatal`（原生错误框 + 桌面日志）。
//! - 监听线程在取得锁的瞬间即启动（回调稍后注册），第二实例的写操作永不悬挂。
//! - 非 Windows 构建回退到 loopback 端口锁（仅保证独占，无唤回通道）。

#[cfg(not(windows))]
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 实例独占管道名（本地命名空间，仅本机可连接；`show` 消息无敏感载荷）。
#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\OwO-Agent-Desktop";

/// 唤回消息负载（第二实例 → 主实例）。
#[cfg(windows)]
const SHOW_MESSAGE: &[u8] = b"show";

/// 退出消息负载（InstanceLock::drop → 监听线程）：唤醒阻塞的 ConnectNamedPipe，
/// 使 join 有界可完成，管道名随即释放。
#[cfg(windows)]
const QUIT_MESSAGE: &[u8] = b"quit";

#[derive(Debug)]
pub enum AcquireOutcome {
    /// 本进程成为主实例（持有独占管道）。
    Primary(InstanceLock),
    /// 已有主实例：已发送唤回消息，本进程应立即退出。
    WokeExisting,
    /// 安全软件/权限拒绝（无法建立独占管道）。
    PermissionDenied(String),
    /// 其他系统错误。
    Unexpected(String),
}

/// 命名管道句柄的 Send 包装：HANDLE 由本模块独占管理，跨线程移动安全
/// （监听线程持有副本；drop 关闭原句柄后，阻塞调用以错误返回并退出）。
#[cfg(windows)]
#[derive(Clone, Copy)]
struct PipeHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for PipeHandle {}

/// 唤回回调（第二实例 show 消息到达时执行）；可为空直到 UI 就绪。
#[cfg(windows)]
type WakeCallback = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

/// 持有期间独占的实例锁（Windows：命名管道 + 后台监听线程；drop 即释放）。
pub struct InstanceLock {
    #[cfg(windows)]
    pipe: PipeHandle,
    #[cfg(windows)]
    stop: Arc<AtomicBool>,
    #[cfg(windows)]
    wake: WakeCallback,
    #[cfg(windows)]
    _listener: Option<std::thread::JoinHandle<()>>,
    #[cfg(not(windows))]
    _listener: TcpListener,
}

impl std::fmt::Debug for InstanceLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("InstanceLock(单实例独占锁)")
    }
}

// HANDLE 是 *mut c_void 原生指针：锁本身在不同线程间移动（setup 闭包要求 Send）。
// 句柄生命周期由 InstanceLock 独占管理（drop 关闭），跨线程移动是安全的。
#[cfg(windows)]
unsafe impl Send for InstanceLock {}

#[cfg(windows)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE,
            INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, WriteFile, OPEN_EXISTING};

        // 唤醒监听线程：作为客户端连接管道并写入 quit 消息（监听线程阻塞在
        // ConnectNamedPipe 上时，客户端连接恰好成立；随后读到 quit 即退出循环）。
        // 若监听线程刚好处于断开后、下次连接前的窗口期，CreateFileW 会得到
        // ERROR_PIPE_BUSY，短暂重试。
        let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        for _ in 0..100 {
            let client = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if client == INVALID_HANDLE_VALUE {
                let error = unsafe { GetLastError() };
                if error == ERROR_PIPE_BUSY {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                // 无客户端可连（监听线程尚未进入 ConnectNamedPipe）：
                // 已有 stop 标志，线程会在进入循环时自行退出。
                break;
            }
            let mut written: u32 = 0;
            unsafe {
                WriteFile(
                    client,
                    QUIT_MESSAGE.as_ptr().cast(),
                    QUIT_MESSAGE.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
                CloseHandle(client);
            }
            break;
        }

        self.stop.store(true, Ordering::SeqCst);
        // join 有界：quit 消息已唤醒阻塞的连接，监听线程读完后随即退出。
        if let Some(handle) = self._listener.take() {
            let _ = handle.join();
        }
        unsafe {
            CloseHandle(self.pipe.0);
        }
    }
}

impl InstanceLock {
    /// 尝试取得主实例锁，并向可能的已有实例发送唤回消息。
    pub fn acquire() -> AcquireOutcome {
        #[cfg(windows)]
        {
            Self::acquire_windows()
        }
        #[cfg(not(windows))]
        {
            const LOCK_PORT: u16 = 40961;
            match TcpListener::bind(("127.0.0.1", LOCK_PORT)) {
                Ok(listener) => AcquireOutcome::Primary(InstanceLock {
                    _listener: listener,
                }),
                Err(error) => AcquireOutcome::Unexpected(format!("实例锁端口绑定失败：{error}")),
            }
        }
    }

    /// 注册唤回回调（主实例取得锁后、UI 可唤回时调用；可多次调用，后注册者覆盖）。
    /// 监听线程自取得锁起已运行，本方法只替换回调，不存在窗口期丢消息。
    pub fn start_wake_listener(&mut self, on_show: impl Fn() + Send + 'static) {
        #[cfg(windows)]
        {
            let mut guard = self
                .wake
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(Box::new(on_show));
        }
        #[cfg(not(windows))]
        {
            let _ = on_show;
        }
    }

    /// 单实例致命错误出口：写桌面日志 + 弹原生错误框（PermissionDenied/Unexpected）。
    pub fn report_fatal(message: &str) {
        #[cfg(windows)]
        {
            Self::log_error(message);
            show_error_box(message);
        }
        #[cfg(not(windows))]
        {
            eprintln!("[owo-desktop] {message}");
        }
    }

    #[cfg(windows)]
    fn acquire_windows() -> AcquireOutcome {
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY,
            GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, WriteFile, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        };
        use windows_sys::Win32::System::Pipes::{
            CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE, PIPE_WAIT,
        };

        fn wide(value: &str) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt;
            std::ffi::OsStr::new(value)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect()
        }

        let name = wide(PIPE_NAME);

        // 第二实例路径：尝试直接连接已有管道。
        let mut busy_attempts = 0;
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                // 已有主实例：发送唤回消息后释放句柄并退出。
                let mut written: u32 = 0;
                unsafe {
                    WriteFile(
                        handle,
                        SHOW_MESSAGE.as_ptr().cast(),
                        SHOW_MESSAGE.len() as u32,
                        &mut written,
                        std::ptr::null_mut(),
                    );
                    CloseHandle(handle);
                }
                return AcquireOutcome::WokeExisting;
            }
            let error = unsafe { GetLastError() };
            match error {
                ERROR_FILE_NOT_FOUND => break, // 无主实例：本进程尝试成为主实例。
                ERROR_PIPE_BUSY => {
                    busy_attempts += 1;
                    if busy_attempts > 10 {
                        return AcquireOutcome::Unexpected("已有实例管道持续繁忙".to_string());
                    }
                    // WaitNamedPipeW：阻塞等待主实例进入下一个连接窗口。
                    let wait_ok = unsafe { WaitNamedPipeW(name.as_ptr(), 1000) };
                    if wait_ok == 0 {
                        let wait_error = unsafe { GetLastError() };
                        if wait_error == ERROR_FILE_NOT_FOUND {
                            break;
                        }
                    }
                    continue;
                }
                _ => {
                    return AcquireOutcome::Unexpected(format!(
                        "连接实例管道失败（错误码 {error}）"
                    ));
                }
            }
        }

        // 主实例路径：创建独占管道；竞态下第二名创建者收到 ACCESS_DENIED。
        let raw_handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                1,
                512,
                512,
                0,
                std::ptr::null(),
            )
        };
        if raw_handle == INVALID_HANDLE_VALUE {
            let error = unsafe { GetLastError() };
            if error == ERROR_ACCESS_DENIED {
                return AcquireOutcome::WokeExisting;
            }
            return AcquireOutcome::PermissionDenied(format!(
                "创建实例管道被拒绝（错误码 {error}）：可能被安全软件或权限策略拦截"
            ));
        }

        let pipe = PipeHandle(raw_handle);
        // 监听线程只接收句柄的数值副本（usize 天然 Send），避免原生指针
        // 跨线程移动的分析负担；原始 PipeHandle 仍由 InstanceLock 独占持有。
        let handle_value = raw_handle as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let wake: WakeCallback = Arc::new(Mutex::new(None));
        let listener_stop = Arc::clone(&stop);
        let listener_wake = Arc::clone(&wake);
        let listener = std::thread::Builder::new()
            .name("owo-single-instance".into())
            .spawn(move || {
                use windows_sys::Win32::Storage::FileSystem::ReadFile;
                use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, DisconnectNamedPipe};
                let handle = handle_value as windows_sys::Win32::Foundation::HANDLE;
                let mut buffer = [0u8; 64];
                loop {
                    if listener_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    // 阻塞等待第二实例连接；句柄被关闭时立即带错误返回。
                    unsafe {
                        ConnectNamedPipe(handle, std::ptr::null_mut());
                    }
                    if listener_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut bytes_read: u32 = 0;
                    let message = unsafe {
                        let ok = ReadFile(
                            handle,
                            buffer.as_mut_ptr().cast(),
                            buffer.len() as u32,
                            &mut bytes_read,
                            std::ptr::null_mut(),
                        );
                        if ok == 0 {
                            None
                        } else {
                            Some(buffer[..bytes_read as usize].to_vec())
                        }
                    };
                    match message.as_deref() {
                        Some(SHOW_MESSAGE) => {
                            if let Some(callback) = listener_wake
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .as_ref()
                            {
                                callback();
                            }
                        }
                        Some(QUIT_MESSAGE) | None => {
                            // 退出消息或异常断开：结束监听，释放管道。
                            unsafe {
                                DisconnectNamedPipe(handle);
                            }
                            break;
                        }
                        Some(_) => {}
                    }
                    unsafe {
                        DisconnectNamedPipe(handle);
                    }
                }
            })
            .ok();

        AcquireOutcome::Primary(InstanceLock {
            pipe,
            stop,
            wake,
            _listener: Some(listener.unwrap_or_else(|| {
                panic!(
                    "单实例监听线程启动失败（{}）",
                    std::io::Error::last_os_error()
                )
            })),
        })
    }

    #[cfg(windows)]
    fn log_error(message: &str) {
        // 写入桌面日志目录（与核心日志同目录，便于统一排障）。
        let dir = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("TEMP").map(std::path::PathBuf::from))
            .map(|base| base.join("OwO").join("Agent").join("logs"))
            .unwrap_or_default();
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("desktop-error.log");
        use std::io::Write;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
        {
            let _ = writeln!(log, "[single-instance] {message}");
        }
    }
}

/// 原生错误框（Windows）：单实例锁被安全软件/权限策略拒绝时向用户说明。
#[cfg(windows)]
fn show_error_box(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide_title: Vec<u16> = "OwO Agent"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wide_message: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            wide_message.as_ptr(),
            wide_title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// 两个测试共用同一条命名管道（PIPE_NAME），必须串行执行，
    /// 否则并行争用会让其中一个测试拿到 WokeExisting 而非 Primary。
    static PIPE_TESTS_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn acquire_is_exclusive_until_dropped() {
        let _guard = PIPE_TESTS_LOCK.lock().unwrap();
        match InstanceLock::acquire() {
            AcquireOutcome::Primary(first) => {
                // 第二实例不得成为主实例（Windows 下为 WokeExisting）。
                let second = InstanceLock::acquire();
                assert!(
                    matches!(
                        second,
                        AcquireOutcome::WokeExisting
                            | AcquireOutcome::PermissionDenied(_)
                            | AcquireOutcome::Unexpected(_)
                    ),
                    "第二实例不得取得独占锁，实际：{second:?}"
                );
                drop(first);
                let again = InstanceLock::acquire();
                assert!(
                    matches!(again, AcquireOutcome::Primary(_)),
                    "释放后应可重新取得锁，实际：{again:?}"
                );
                if let AcquireOutcome::Primary(lock) = again {
                    drop(lock);
                }
            }
            other => panic!("首次应取得主实例锁，实际：{other:?}"),
        }
    }

    #[test]
    fn wake_message_roundtrip_via_pipe() {
        #[cfg(windows)]
        {
            use std::sync::atomic::AtomicU32;
            let _guard = PIPE_TESTS_LOCK.lock().unwrap();
            let outcome = InstanceLock::acquire();
            let AcquireOutcome::Primary(mut lock) = outcome else {
                panic!("测试需独占管道，实际：{outcome:?}");
            };
            let wakes = Arc::new(AtomicU32::new(0));
            let wakes_for_thread = Arc::clone(&wakes);
            lock.start_wake_listener(move || {
                wakes_for_thread.fetch_add(1, Ordering::SeqCst);
            });

            // 第二实例语义：连接管道写 show，应触发主实例一次唤回回调。
            let second = InstanceLock::acquire();
            assert!(matches!(second, AcquireOutcome::WokeExisting));
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert_eq!(
                wakes.load(Ordering::SeqCst),
                1,
                "主实例应收到并处理一次唤回消息"
            );
            drop(lock);
        }
    }
}
