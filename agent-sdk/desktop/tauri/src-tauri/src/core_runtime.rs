//! §4.2 桌面壳核心运行时：壳必须拥有并监督自己的核心进程。
//!
//! - 动态端口：以 `serve --port 0` 启动，实际端口经 stdout 的 `core_ready` 行取得；
//!   旧核心无该行时回退「壳自选空闲端口 + `--port <n>`」兼容路径。
//! - 有身份的就绪：`/health` 的 `instance_id` 必须等于壳注入值——只认自己启动的
//!   子进程，不再盲复用同端口旧服务（空壳根因）。
//! - 可观测：stdout/stderr 全量捕获到 `%LOCALAPPDATA%\OwO\Agent\logs\`，
//!   写入前对配对密钥脱敏；>5MB 轮转为 .old。
//! - 受控重启：子进程意外退出按 250ms/1s/3s 退避重启至多 3 次，超预算进入
//!   `Failed` 并暴露稳定错误码，禁止静默循环。
//! - 优雅关闭：先带 Bearer（启动时以配对+实例身份引导取得）与实例头调用
//!   `/server/shutdown`，等待 2s，再 `taskkill /T /F` 兜底清进程树。
use serde_json::Value;
use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

use crate::core_supervisor::{self, parse_ready_line};

/// 由 build.rs 从核心服务的 OWO_API_VERSION 单一源码读取。
pub const CORE_API_VERSION: &str = env!("OWO_CORE_API_VERSION");

/// 意外退出后的受控重启退避序列（§4.4：3 次受控重启）。
pub const RESTART_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(1000),
    Duration::from_millis(3000),
];

/// 单次启动的就绪判定预算（core_ready 行等待；随后 /health 实例校验再给 10s）。
const READY_LINE_TIMEOUT: Duration = Duration::from_secs(25);
/// 优雅关闭：graceful 请求后的进程退出宽限。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// §4.3 稳定错误码 + 用户可操作文案（壳的启动诊断页直接消费）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreError {
    BinaryMissing,
    SpawnFailed,
    HandshakeTimeout,
    IdentityMismatch,
    ExitedUnexpectedly,
}

impl CoreError {
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::BinaryMissing => "core/binary_missing",
            CoreError::SpawnFailed => "core/spawn_failed",
            CoreError::HandshakeTimeout => "core/handshake_timeout",
            CoreError::IdentityMismatch => "core/identity_mismatch",
            CoreError::ExitedUnexpectedly => "core/exited",
        }
    }

    pub fn user_message(&self) -> &'static str {
        match self {
            CoreError::BinaryMissing => "核心服务文件缺失，安装可能不完整",
            CoreError::SpawnFailed => "核心服务启动失败，请查看日志",
            CoreError::HandshakeTimeout => "服务已启动但未完成初始化",
            CoreError::IdentityMismatch => "检测到不属于本窗口的旧服务",
            CoreError::ExitedUnexpectedly => "核心服务意外退出，多次重启未恢复",
        }
    }
}

/// 就绪后的核心连接描述符（WebView 经 `get_core_connection` 消费）。
#[derive(Debug, Clone)]
pub struct CoreConnection {
    pub pid: u32,
    pub port: u16,
    pub api_version: String,
    pub build_id: String,
    pub instance_id: String,
}

/// 核心运行状态机（§4.2 CoreState）。
#[derive(Debug, Clone)]
pub enum CoreState {
    Starting {
        attempt: u8,
    },
    Ready(CoreConnection),
    Restarting {
        attempt: u8,
        reason: String,
    },
    Failed {
        code: &'static str,
        message: String,
        log_path: PathBuf,
    },
    Stopped,
}

/// 核心运行时。全部字段为锁/原子，线程安全。
pub struct CoreRuntime {
    state: Arc<Mutex<CoreState>>,
    pairing: String,
    instance_id: String,
    generation: Arc<AtomicU64>,
    shutdown_requested: Arc<AtomicBool>,
    log_path: Arc<Mutex<PathBuf>>,
    bearer: Arc<Mutex<Option<String>>>,
    child_pid: Arc<Mutex<Option<u32>>>,
}

impl CoreRuntime {
    pub fn new(pairing: String, instance_id: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(CoreState::Starting { attempt: 0 })),
            pairing,
            instance_id,
            generation: Arc::new(AtomicU64::new(0)),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            log_path: Arc::new(Mutex::new(PathBuf::new())),
            bearer: Arc::new(Mutex::new(None)),
            child_pid: Arc::new(Mutex::new(None)),
        }
    }

    pub fn pairing(&self) -> &str {
        &self.pairing
    }

    pub fn state(&self) -> CoreState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or(CoreState::Stopped)
    }

    pub fn log_path(&self) -> PathBuf {
        self.log_path
            .lock()
            .map(|path| path.clone())
            .unwrap_or_default()
    }

    fn set_state(&self, next: CoreState) {
        if let Ok(mut state) = self.state.lock() {
            *state = next;
        }
    }

    /// 启动监督线程（不阻塞调用方；ready 后状态变为 `Ready`）。
    pub fn start(self: &Arc<Self>) {
        self.shutdown_requested.store(false, Ordering::SeqCst);
        let runtime = Arc::clone(self);
        let generation = self.generation.load(Ordering::SeqCst);
        std::thread::Builder::new()
            .name("owo-core-runtime".into())
            .spawn(move || runtime.supervise(generation))
            .expect("启动核心监督线程失败");
    }

    /// 手动重试（诊断页按钮）：代数递增使旧监督线程失效，立即重新拉起。
    pub fn retry(self: &Arc<Self>) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.start();
    }

    /// 监督主循环：spawn → 就绪判定 → Ready → 等待退出 →（意外退出）退避重启。
    fn supervise(self: Arc<Self>, generation: u64) {
        let mut attempt: u8 = 0;
        loop {
            if self.generation.load(Ordering::SeqCst) != generation
                || self.shutdown_requested.load(Ordering::SeqCst)
            {
                self.set_state(CoreState::Stopped);
                return;
            }
            self.set_state(CoreState::Starting { attempt });
            match self.launch_once() {
                Ok((connection, generation_handle)) => {
                    self.set_state(CoreState::Ready(connection));
                    let reason = wait_for_exit(&generation_handle, generation, &self.generation);
                    if self.shutdown_requested.load(Ordering::SeqCst)
                        || self.generation.load(Ordering::SeqCst) != generation
                    {
                        self.set_state(CoreState::Stopped);
                        return;
                    }
                    if attempt as usize >= RESTART_DELAYS.len() {
                        self.set_state(CoreState::Failed {
                            code: CoreError::ExitedUnexpectedly.code(),
                            message: CoreError::ExitedUnexpectedly.user_message().to_string(),
                            log_path: self.log_path(),
                        });
                        return;
                    }
                    self.set_state(CoreState::Restarting { attempt, reason });
                    std::thread::sleep(RESTART_DELAYS[attempt as usize]);
                    attempt += 1;
                }
                Err(failure) => {
                    if self.generation.load(Ordering::SeqCst) != generation
                        || self.shutdown_requested.load(Ordering::SeqCst)
                    {
                        self.set_state(CoreState::Stopped);
                        return;
                    }
                    self.set_state(CoreState::Failed {
                        code: failure.code(),
                        message: failure.user_message().to_string(),
                        log_path: self.log_path(),
                    });
                    return;
                }
            }
        }
    }

    /// 拉起一次核心并完成就绪判定（ready 行 → /health 实例校验 → 引导 bearer）。
    /// 返回连接描述符与本次子进程的代际句柄（监督循环据此感知意外退出）。
    ///
    /// §4.3/§4.4：ready 消息绑定本次代际（独立 channel + 独立 exit 信号），
    /// 旧代 core 迟到的 stdout/退出事件不会污染本次启动。
    fn launch_once(&self) -> Result<(CoreConnection, ChildGeneration), CoreError> {
        let exe = core_server_path();
        if !exe.exists() {
            return Err(CoreError::BinaryMissing);
        }
        let workspace = core_workspace(&exe);
        let log = open_core_log().map_err(|_| CoreError::SpawnFailed)?;
        *self
            .log_path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = log.clone();

        let (ready_tx, ready_rx) = mpsc::channel::<Value>();
        let mut command = Command::new(&exe);
        command
            .args(["serve", "--port", "0", "--workspace"])
            .arg(&workspace)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        apply_core_env(&mut command, &self.pairing, &self.instance_id);

        let child: Child = command.spawn().map_err(|_| CoreError::SpawnFailed)?;
        let pid = child.id();
        let mut generation = ChildGeneration::spawned(child, pid);
        *self
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(pid);
        let stdout = generation.take_stdout();
        let stderr = generation.take_stderr();
        spawn_log_thread(stdout, &self.pairing, &log, Some(ready_tx));
        spawn_log_thread(stderr, &self.pairing, &log, None);

        // 就绪判定：等本次代际的 core_ready 行取得实际端口（--port 0 由系统分配）。
        let ready = wait_ready_line(&ready_rx, &generation);
        match ready {
            Some(value) => {
                let port = value["port"].as_u64().unwrap_or(0) as u16;
                // §4.3：ready 消息先校验实例身份与端口非 0，再经 /health 二次确认。
                let instance_ok = value["instance_id"]
                    .as_str()
                    .is_some_and(|reported| reported.trim() == self.instance_id);
                if port == 0 || !instance_ok {
                    append_log_line(
                        &log,
                        &format!(
                            "[runtime] ready 行身份校验失败：port={port} instance_ok={instance_ok}（该行可能不属于本次启动）"
                        ),
                    );
                    generation.terminate();
                    return Err(CoreError::IdentityMismatch);
                }
                let health = core_supervisor::wait_for_instance(
                    port,
                    &self.instance_id,
                    CORE_API_VERSION,
                    Duration::from_secs(10),
                )
                .map_err(|error| {
                    append_log_line(&log, &format!("[runtime] 实例校验失败：{error}"));
                    CoreError::IdentityMismatch
                })?;
                self.bootstrap_bearer(port);
                Ok((
                    CoreConnection {
                        pid: health.pid.unwrap_or(pid),
                        port,
                        api_version: health.api_version,
                        build_id: value["build_id"].as_str().unwrap_or("unknown").to_string(),
                        instance_id: self.instance_id.clone(),
                    },
                    generation,
                ))
            }
            None => {
                // §4.4：旧核心无 core_ready 行 → 壳自选空闲端口重启一次；每代独立 exit 信号，
                // 且 kill 旧代后先等待旧 watch 完整结束才创建新代（不串用旧退出事件）。
                append_log_line(&log, "[runtime] 未观察到 core_ready 行，走固定端口兼容回退");
                generation.terminate_and_join();
                let fallback_port = free_port().ok_or(CoreError::HandshakeTimeout)?;
                let mut command = Command::new(&exe);
                command
                    .args(["serve", "--port", &fallback_port.to_string(), "--workspace"])
                    .arg(&workspace)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                apply_core_env(&mut command, &self.pairing, &self.instance_id);
                let fallback_child: Child = command.spawn().map_err(|_| CoreError::SpawnFailed)?;
                let fallback_pid = fallback_child.id();
                let mut fallback_generation =
                    ChildGeneration::spawned(fallback_child, fallback_pid);
                *self
                    .child_pid
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(fallback_pid);
                spawn_log_thread(fallback_generation.take_stdout(), &self.pairing, &log, None);
                spawn_log_thread(fallback_generation.take_stderr(), &self.pairing, &log, None);
                core_supervisor::wait_for_instance(
                    fallback_port,
                    &self.instance_id,
                    CORE_API_VERSION,
                    Duration::from_secs(15),
                )
                .map_err(|error| {
                    append_log_line(&log, &format!("[runtime] 兼容回退实例校验失败：{error}"));
                    CoreError::HandshakeTimeout
                })?;
                self.bootstrap_bearer(fallback_port);
                Ok((
                    CoreConnection {
                        pid: fallback_pid,
                        port: fallback_port,
                        api_version: CORE_API_VERSION.to_string(),
                        build_id: "unknown".to_string(),
                        instance_id: self.instance_id.clone(),
                    },
                    fallback_generation,
                ))
            }
        }
    }

    /// 以配对+实例身份引导 bearer token（供优雅关闭使用；不落盘、不写日志）。
    fn bootstrap_bearer(&self, port: u16) {
        let headers = [
            ("x-owo-desktop-pairing", self.pairing.to_string()),
            ("x-owo-desktop-instance", self.instance_id.to_string()),
        ];
        if let Ok((200, body)) =
            core_supervisor::http_request(port, "GET", "/auth/token", &headers, None)
        {
            if let Ok(value) = serde_json::from_str::<Value>(&body) {
                if let Some(token) = value["token"].as_str() {
                    *self
                        .bearer
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token.to_string());
                    return;
                }
            }
        }
        append_log_line(
            &self.log_path(),
            "[runtime] bearer 引导失败（关闭将走 taskkill 兜底路径）",
        );
    }

    /// 优雅关闭：graceful 请求 → 宽限 → taskkill /T /F 清进程树兜底。
    pub fn shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        if let CoreState::Ready(connection) = self.state() {
            let bearer = self
                .bearer
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            let mut headers: Vec<(&str, String)> =
                vec![("x-owo-desktop-instance", self.instance_id.to_string())];
            if let Some(token) = bearer {
                headers.push(("Authorization", format!("Bearer {token}")));
            }
            let _ = core_supervisor::http_request(
                connection.port,
                "POST",
                "/server/shutdown",
                &headers,
                Some(r#"{"confirm":true}"#),
            );
            std::thread::sleep(SHUTDOWN_GRACE);
        }
        let pid = self
            .child_pid
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pid) = pid {
            kill_process_tree(pid);
        }
    }
}

/// §4.4 单次子进程代际：每代持有独立退出信号与 watch 线程句柄。
/// kill 旧代后 join 旧 watch 完全结束，再创建新代，杜绝旧退出事件污染新代。
pub struct ChildGeneration {
    pid: u32,
    exit_flag: Arc<AtomicBool>,
    watch: Option<std::thread::JoinHandle<()>>,
    stdout: Option<std::process::ChildStdout>,
    stderr: Option<std::process::ChildStderr>,
}

impl ChildGeneration {
    fn spawned(mut child: Child, pid: u32) -> Self {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let exit_flag = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exit_flag);
        let watch = std::thread::Builder::new()
            .name("owo-core-watch".into())
            .spawn(move || {
                let _ = child.wait();
                flag.store(true, Ordering::SeqCst);
            })
            .ok();
        Self {
            pid,
            exit_flag,
            watch,
            stdout,
            stderr,
        }
    }

    fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.stderr.take()
    }

    /// 终止本代进程树（不等待；调用方如需完全结束应再 join）。
    fn terminate(&self) {
        kill_process_tree(self.pid);
    }

    /// 终止并等待本代 watch 完整结束（兼容回退前使用，杜绝旧代退出事件串线）。
    fn terminate_and_join(&mut self) {
        self.terminate();
        if let Some(handle) = self.watch.take() {
            let _ = handle.join();
        }
    }

    fn has_exited(&self) -> bool {
        self.exit_flag.load(Ordering::SeqCst)
    }
}

/// §4.3 等待本次代际的 core_ready 行：绑定本代 channel，不使用全局缓存；
/// 子进程提前退出（未及输出 ready）或超时 → None（调用方决定兼容回退/失败）。
fn wait_ready_line(rx: &mpsc::Receiver<Value>, generation: &ChildGeneration) -> Option<Value> {
    let deadline = std::time::Instant::now() + READY_LINE_TIMEOUT;
    loop {
        if generation.has_exited() {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(value) => return Some(value),
            Err(RecvTimeoutError::Timeout) => {
                if std::time::Instant::now() >= deadline {
                    return None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// 子进程看护线程：持句柄 wait，退出时置位本代退出标志（已迁入 ChildGeneration）。
/// 保持空壳占位：外部历史调用点若仍引用可平滑编译，实际逻辑在 ChildGeneration::spawned。
#[allow(dead_code)]
fn spawn_watch_thread(_child: Child, _exit_flag: Arc<AtomicBool>) {}

/// 等待本次子进程退出（或运行时被新代数取代）；返回原因描述。
fn wait_for_exit(
    generation: &ChildGeneration,
    current_generation: u64,
    current: &AtomicU64,
) -> String {
    loop {
        if current.load(Ordering::SeqCst) != current_generation {
            return "运行时被手动重试取代".to_string();
        }
        if generation.has_exited() {
            return "核心进程退出".to_string();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn kill_process_tree(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output();
}

/// 便携/开发双模式的 core 可执行文件定位。
pub fn core_server_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join("owo-agent-x64.exe");
            if bundled.exists() {
                return bundled;
            }
            let sibling = dir.join("owo-agent.exe");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|parent| parent.parent())
        .and_then(|parent| parent.parent())
        .map(|root| root.join("target").join("debug").join("owo-agent.exe"))
        .unwrap_or_else(|| PathBuf::from("owo-agent.exe"))
}

/// 核心工作区：便携发布用应用目录；开发用 agent-sdk 目录。
fn core_workspace(exe: &std::path::Path) -> PathBuf {
    let portable = exe
        .file_name()
        .map(|name| name.to_string_lossy().contains("-x64"))
        .unwrap_or(false)
        || exe
            .parent()
            .map(|dir| dir.join("owo-agent-desktop.exe").exists())
            .unwrap_or(false);
    if portable {
        exe.parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        exe.parent()
            .and_then(|path| path.parent())
            .and_then(|path| path.parent())
            .map(|path| path.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

/// 注入核心子进程环境（§4.2/§4.7）：配对证明 + 实例身份在 debug/release 中
/// 保持一致（生产协议单一）；开发便利经显式 `OWO_DESKTOP_DEV_AUTH=1` 开关启用，
/// 默认关闭——不允许"调试版能用、安装包空壳"的双标准。
///
/// `OWO_DESKTOP_RELEASE=1` 仅用于收紧浏览器 CORS 边界（发布版禁 http://localhost
/// 直连核心），不参与配对证明协议本身。
fn apply_core_env(command: &mut Command, pairing: &str, instance: &str) {
    let dev_auth = std::env::var("OWO_DESKTOP_DEV_AUTH")
        .map(|value| value == "1")
        .unwrap_or(false);
    if dev_auth {
        // 明确声明开发便利时才跳过配对证明（仅注入实例身份）。
        command.env("OWO_DESKTOP_INSTANCE_ID", instance);
    } else {
        command
            .env("OWO_DESKTOP_PAIRING_SECRET", pairing)
            .env("OWO_DESKTOP_INSTANCE_ID", instance);
    }
    command.env(
        "OWO_DESKTOP_RELEASE",
        if cfg!(debug_assertions) { "0" } else { "1" },
    );
    if let Some(local) = local_appdata() {
        command.env(
            "OWO_AGENT_DATA",
            local.join("OwO").join("Agent").join("data"),
        );
    }
    if std::env::var_os("OPENAI_API_KEY").is_none() && std::env::var_os("OPENAI_BASE_URL").is_none()
    {
        if let Some(token_plan_key) = std::env::var_os("DASHSCOPE_API_KEY") {
            command.env("OPENAI_API_KEY", token_plan_key).env(
                "OPENAI_BASE_URL",
                "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
            );
        } else {
            command
                .env("OPENAI_BASE_URL", "http://127.0.0.1:11434/v1")
                .env("OPENAI_MODEL", "local");
        }
    }
}

fn local_appdata() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
}

/// §4.3 日志线程：把子进程 stdout/stderr 逐行写入当日日志（先脱敏）；
/// stdout 线程若解析到 `core_ready` 行，把该行（原始 Value）发送到本次代际的
/// ready channel——就绪信号与日志落盘完全分离，旧代迟到行不污染新代。
fn spawn_log_thread<R: Read + Send + 'static>(
    pipe: Option<R>,
    pairing: &str,
    log_path: &std::path::Path,
    ready_tx: Option<mpsc::Sender<Value>>,
) {
    let Some(pipe) = pipe else {
        return;
    };
    let pairing = pairing.to_string();
    let log_path = log_path.to_path_buf();
    std::thread::Builder::new()
        .name("owo-core-logger".into())
        .spawn(move || {
            let reader = std::io::BufReader::new(pipe);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        append_log_line(&log_path, &redact(&line, &pairing));
                        if let Some(tx) = &ready_tx {
                            if let Some(value) = parse_ready_line(&line) {
                                let _ = tx.send(value);
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();
}

/// 日志轮转：>5MB 触发，轮转名带时间戳与序号（`desktop-core-YYYYMMDD-HHMMSS.N.log`）；
/// 保留最近 `LOG_KEEP_ROTATED` 份或总计 `LOG_MAX_ROTATED_BYTES`，删除仅作用于已验证日志目录；
/// 重命名/删除失败写入诊断行（不静默忽略）。
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;
const LOG_KEEP_ROTATED: usize = 5;
const LOG_MAX_ROTATED_BYTES: u64 = 25 * 1024 * 1024;

fn open_core_log() -> Result<PathBuf, String> {
    let dir = local_appdata()
        .map(|base| base.join("OwO").join("Agent").join("logs"))
        .ok_or_else(|| "无法确定日志目录".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建日志目录失败：{error}"))?;
    let date = today_compact();
    let path = dir.join(format!("desktop-core-{date}.log"));
    if let Ok(metadata) = std::fs::metadata(&path) {
        if metadata.len() > LOG_ROTATE_BYTES {
            rotate_current_log(&dir, &path, &date);
        }
    }
    Ok(path)
}

/// 轮转当前日志为时间戳+序号命名，并清理过期轮转文件。任何失败都写诊断行。
fn rotate_current_log(dir: &std::path::Path, current: &std::path::Path, date: &str) {
    let stamp = now_hhmmss();
    let mut candidate = dir.join(format!("desktop-core-{date}-{stamp}.1.log"));
    let mut seq = 1u32;
    while candidate.exists() {
        seq += 1;
        candidate = dir.join(format!("desktop-core-{date}-{stamp}.{seq}.log"));
    }
    if let Err(error) = std::fs::rename(current, &candidate) {
        append_log_line(
            current,
            &format!("[runtime] 日志轮转失败（{}）：{error}", candidate.display()),
        );
        return;
    }
    append_log_line(&candidate, "[runtime] 日志轮转：>=5MB，已归档本文件");
    prune_rotated_logs(dir, date);
}

/// 保留最近 N 份轮转文件；总大小超限时删除最旧文件（仅限本日志目录下 desktop-core-*.N.log）。
fn prune_rotated_logs(dir: &std::path::Path, date: &str) {
    let mut rotated: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| {
                            name.starts_with(&format!("desktop-core-{date}-"))
                                && name.ends_with(".log")
                        })
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    rotated.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_default()
    });
    let mut total: u64 = 0;
    let mut keep: Vec<PathBuf> = Vec::new();
    for path in rotated.iter().rev() {
        let size = std::fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let too_many = keep.len() >= LOG_KEEP_ROTATED;
        let too_big = total + size > LOG_MAX_ROTATED_BYTES;
        if too_many || too_big {
            if let Err(error) = std::fs::remove_file(path) {
                append_log_line(
                    path,
                    &format!("[runtime] 轮转日志清理失败（{}）：{error}", path.display()),
                );
            }
        } else {
            total += size;
            keep.push(path.clone());
        }
    }
}

/// 追加一行日志（失败静默——日志不能反过来影响启动）。
fn append_log_line(path: &std::path::Path, line: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

/// 本地日期 YYYYMMDD（仅用于日志文件名，不引 chrono 依赖）。
fn today_compact() -> String {
    let secs = epoch_secs();
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let lengths = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0usize;
    let mut remaining = doy;
    while remaining >= lengths[month] {
        remaining -= lengths[month];
        month += 1;
    }
    format!("{year:04}{:02}{:02}", month + 1, remaining + 1)
}

/// 当日 HHMMSS（轮转文件名的秒级时间戳）。
fn now_hhmmss() -> String {
    let secs = epoch_secs();
    let sod = secs % 86_400;
    format!("{:02}{:02}{:02}", sod / 3600, (sod % 3600) / 60, sod % 60)
}

fn epoch_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 日志脱敏（§4.5）：任何出现配对证明的位置替换为 `[redacted]`；此外对常见的
/// `Bearer <token>` 与 `x-api-key/api-key: <key>` 高熵凭据做掩码（纯函数，可测）。
/// 不匹配任何模式的行原样返回。
pub fn redact(line: &str, secret: &str) -> String {
    let after_secret = if secret.is_empty() {
        line.to_string()
    } else {
        line.split(secret).collect::<Vec<_>>().join("[redacted]")
    };
    let after_bearer = mask_bearer_token(&after_secret);
    mask_api_key(&after_bearer)
}

/// 掩码 `Bearer <token>`（大小写不敏感；token 需 ≥12 位，避免误伤普通词）。
fn mask_bearer_token(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i..]
            .iter()
            .take(7)
            .map(|b| b.to_ascii_lowercase())
            .collect::<Vec<u8>>()
            == b"bearer "
        {
            let mut j = i + 7;
            while j < bytes.len()
                && !bytes[j].is_ascii_whitespace()
                && bytes[j] != b','
                && bytes[j] != b'"'
            {
                j += 1;
            }
            if j - (i + 7) >= 12 {
                out.push_str(&line[i..i + 7]);
                out.push_str("[redacted]");
                i = j;
                continue;
            }
        }
        let ch = line[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&line[i..i + ch]);
        i += ch;
    }
    out
}

/// 掩码 `x-api-key: <key>` / `api-key: <key>` 后的密钥（需 ≥8 位，保留键名后的空白）。
fn mask_api_key(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let rest = &bytes[i..];
        // 前缀精确匹配（不含前导/尾随空白；x-api-key 用 10 字节、api-key 用 8 字节）。
        let colon_pos = if rest
            .iter()
            .take(10)
            .map(|b| b.to_ascii_lowercase())
            .collect::<Vec<u8>>()
            == b"x-api-key:"
        {
            i + 10
        } else if rest
            .iter()
            .take(8)
            .map(|b| b.to_ascii_lowercase())
            .collect::<Vec<u8>>()
            == b"api-key:"
        {
            i + 8
        } else {
            usize::MAX
        };
        if colon_pos != usize::MAX {
            // 键名 + 冒号原样保留；空白原样保留；仅掩码非空白的密钥本体。
            let mut j = colon_pos;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let key_start = j;
            while j < bytes.len()
                && !bytes[j].is_ascii_whitespace()
                && bytes[j] != b','
                && bytes[j] != b'"'
            {
                j += 1;
            }
            if j - key_start >= 8 {
                out.push_str(&line[i..colon_pos]);
                out.push_str(&line[colon_pos..key_start]);
                out.push_str("[redacted]");
                i = j;
                continue;
            }
        }
        let ch = line[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&line[i..i + ch]);
        i += ch;
    }
    out
}

/// 选择一个当前空闲的 loopback 端口（绑定后立即释放）。
fn free_port() -> Option<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
}

#[cfg(test)]
mod tests {
    use super::{redact, CoreError, RESTART_DELAYS};
    use std::time::Duration;

    #[test]
    fn redacts_pairing_secret_anywhere_in_line() {
        assert_eq!(
            redact("start secret-ABC end", "secret-ABC"),
            "start [redacted] end"
        );
        assert_eq!(
            redact("secret-ABCsecret-ABC", "secret-ABC"),
            "[redacted][redacted]"
        );
        assert_eq!(redact("no secret here", "secret-ABC"), "no secret here");
        assert_eq!(redact("anything", ""), "anything", "空密钥原样返回");
    }

    #[test]
    fn redacts_bearer_token_headers() {
        // §4.5：Authorization: Bearer 高熵 token 必须被掩码。
        assert_eq!(
            redact(
                "GET /api -> Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.abc123",
                ""
            ),
            "GET /api -> Authorization: Bearer [redacted]"
        );
        assert_eq!(
            redact(
                "authorization: bearer aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee done",
                ""
            ),
            "authorization: bearer [redacted] done"
        );
        // 非 Bearer 载荷（短词、无前缀）不受影响。
        assert_eq!(
            redact("bearer of the news", ""),
            "bearer of the news",
            "普通词语不应误判"
        );
    }

    #[test]
    fn redacts_api_key_headers() {
        // §4.5：x-api-key / api-key 头后的密钥必须被掩码。
        assert_eq!(
            redact(
                "request x-api-key: sk-proj-abcdef1234567890xyz trace=ok",
                ""
            ),
            "request x-api-key: [redacted] trace=ok"
        );
        assert_eq!(
            redact("api-key: 1234567890abcdef", ""),
            "api-key: [redacted]"
        );
        // 短于阈值（<8 位）不掩码，避免误伤正常文本。
        assert_eq!(redact("api-key: ab", ""), "api-key: ab");
    }

    #[test]
    fn restart_backoff_is_bounded_and_ascending() {
        assert_eq!(RESTART_DELAYS.len(), 3, "至多 3 次受控重启");
        assert_eq!(RESTART_DELAYS[0], Duration::from_millis(250));
        assert_eq!(RESTART_DELAYS[1], Duration::from_millis(1000));
        assert_eq!(RESTART_DELAYS[2], Duration::from_millis(3000));
    }

    #[test]
    fn core_error_codes_and_messages_are_stable() {
        assert_eq!(CoreError::BinaryMissing.code(), "core/binary_missing");
        assert_eq!(CoreError::HandshakeTimeout.code(), "core/handshake_timeout");
        assert_eq!(CoreError::IdentityMismatch.code(), "core/identity_mismatch");
        assert_eq!(CoreError::SpawnFailed.code(), "core/spawn_failed");
        assert_eq!(CoreError::ExitedUnexpectedly.code(), "core/exited");
        for error in [
            CoreError::BinaryMissing,
            CoreError::SpawnFailed,
            CoreError::HandshakeTimeout,
            CoreError::IdentityMismatch,
            CoreError::ExitedUnexpectedly,
        ] {
            assert!(!error.user_message().is_empty(), "{:?} 需要用户文案", error);
        }
    }
}
