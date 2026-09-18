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
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

use crate::core_supervisor::{self, parse_ready_line};
use crate::provider::{self, ProviderConfig};

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
    /// §4.6：未选择项目工作区（NoWorkspace）。正常由 `start()` 门控，
    /// launch_once 内仅作防御性兜底。
    NoWorkspace,
}

impl CoreError {
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::BinaryMissing => "core/binary_missing",
            CoreError::SpawnFailed => "core/spawn_failed",
            CoreError::HandshakeTimeout => "core/handshake_timeout",
            CoreError::IdentityMismatch => "core/identity_mismatch",
            CoreError::ExitedUnexpectedly => "core/exited",
            CoreError::NoWorkspace => "core/no_workspace",
        }
    }

    pub fn user_message(&self) -> &'static str {
        match self {
            CoreError::BinaryMissing => "核心服务文件缺失，安装可能不完整",
            CoreError::SpawnFailed => "核心服务启动失败，请查看日志",
            CoreError::HandshakeTimeout => "服务已启动但未完成初始化",
            CoreError::IdentityMismatch => "检测到不属于本窗口的旧服务",
            CoreError::ExitedUnexpectedly => "核心服务意外退出，多次重启未恢复",
            CoreError::NoWorkspace => "尚未选择项目工作区",
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

/// 核心运行状态机（§4.2 CoreState；§4.6 增 NoWorkspace）。
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
    /// §4.6：尚未选择项目工作区。刻意不开 core：安装目录只用于找资源、
    /// 数据目录只用于状态持久化、项目工作区三者分离；没有工作区就不启用
    /// 文件写入工具，Web 侧只允许诊断、设置与选择目录。
    NoWorkspace,
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
    /// §4.6：当前项目工作区（数据目录持久化的“最近项目”；None = NoWorkspace）。
    workspace: Arc<Mutex<Option<PathBuf>>>,
    /// §4.8：用户显式选择的模型提供商（数据目录持久化；密钥不落盘）。
    provider_cfg: Arc<Mutex<ProviderConfig>>,
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
            workspace: Arc::new(Mutex::new(load_saved_workspace())),
            provider_cfg: Arc::new(Mutex::new(provider::load_provider_config())),
        }
    }

    /// 测试/注入用构造：显式指定初始工作区（None = NoWorkspace 引导态）。
    #[cfg(test)]
    pub fn new_with_workspace(
        pairing: String,
        instance_id: String,
        workspace: Option<PathBuf>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(CoreState::Starting { attempt: 0 })),
            pairing,
            instance_id,
            generation: Arc::new(AtomicU64::new(0)),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            log_path: Arc::new(Mutex::new(PathBuf::new())),
            bearer: Arc::new(Mutex::new(None)),
            child_pid: Arc::new(Mutex::new(None)),
            workspace: Arc::new(Mutex::new(workspace)),
            provider_cfg: Arc::new(Mutex::new(provider::load_provider_config())),
        }
    }

    pub fn pairing(&self) -> &str {
        &self.pairing
    }

    /// §4 首屏收敛：壳已引导的 core bearer token（供 get_core_connection 注入
    /// 当前 WebView，省去浏览器模式下的 GET /auth/token 冷启动请求）。
    /// 只经 Tauri IPC 传给本窗口，不落盘、不写日志（redact 兜底）。
    pub fn bearer_token(&self) -> Option<String> {
        self.bearer.lock().ok().and_then(|guard| guard.clone())
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

    /// §4.6 当前项目工作区（None = 未选择，处于 NoWorkspace）。
    pub fn workspace(&self) -> Option<PathBuf> {
        self.workspace.lock().ok().and_then(|guard| guard.clone())
    }

    fn set_state(&self, next: CoreState) {
        if let Ok(mut state) = self.state.lock() {
            *state = next;
        }
    }

    /// §4.6 启动监督线程（不阻塞调用方）。
    /// 无工作区时不拉起 core，进入 `NoWorkspace`（文件写入工具不启用）；
    /// 有工作区才监督拉起并做就绪判定（ready 后状态变为 `Ready`）。
    pub fn start(self: &Arc<Self>) {
        self.shutdown_requested.store(false, Ordering::SeqCst);
        if self.workspace().is_none() {
            self.set_state(CoreState::NoWorkspace);
            return;
        }
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

    /// §4.6 设置项目工作区并持久化到数据目录（不在此处重启；命令层持 Arc 调用
    /// `start`/`retry` 完成受控重启，避免 `&self` 无法创建 `&Arc<Self>`）。
    ///
    /// 校验：目录必须真实存在（canonicalize 失败即拒绝），防止把任意字符串
    /// 当工作区传给 core。
    pub fn set_workspace(&self, path: &std::path::Path) -> Result<PathBuf, String> {
        let canonical = path
            .canonicalize()
            .map_err(|error| format!("工作区目录不可用：{error}"))?;
        if !canonical.is_dir() {
            return Err("所选路径不是目录".to_string());
        }
        save_workspace(&canonical)?;
        {
            let mut guard = self
                .workspace
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = Some(canonical.clone());
        }
        Ok(canonical)
    }

    /// §4.6/§4.8 运行时是否已处于启动/就绪（命令层据此选择 start vs retry）。
    pub fn is_running(&self) -> bool {
        matches!(
            self.state(),
            CoreState::Ready(_) | CoreState::Starting { .. } | CoreState::Restarting { .. }
        )
    }

    /// §4.8 当前显式提供商配置（深拷贝供命令层读取）。
    pub fn provider_config(&self) -> ProviderConfig {
        self.provider_cfg
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| ProviderConfig::unset())
    }

    /// §4.8 更新提供商选择并持久化（不在此处重启；命令层持 Arc 触发）。
    pub fn set_provider(&self, config: &ProviderConfig) -> Result<(), String> {
        provider::save_provider_config(config)?;
        {
            let mut guard = self
                .provider_cfg
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = config.clone();
        }
        Ok(())
    }

    /// 监督主循环：spawn → 就绪判定 → Ready → 等待退出 →（意外退出）退避重启。
    fn supervise(self: Arc<Self>, generation: u64) {
        let mut attempt: u8 = 0;
        loop {
            if self.shutdown_requested.load(Ordering::SeqCst) {
                self.set_state(CoreState::Stopped);
                return;
            }
            if self.generation.load(Ordering::SeqCst) != generation {
                // §7：被新代（连点重连）取代的旧监督线程直接退出；状态归新代管，
                // 不得把新代的 Starting/Ready 覆写成 Stopped。
                return;
            }
            self.set_state(CoreState::Starting { attempt });
            match self.launch_once() {
                Ok((connection, generation_handle)) => {
                    self.set_state(CoreState::Ready(connection));
                    let reason = wait_for_exit(&generation_handle, generation, &self.generation);
                    if self.shutdown_requested.load(Ordering::SeqCst) {
                        self.set_state(CoreState::Stopped);
                        return;
                    }
                    if self.generation.load(Ordering::SeqCst) != generation {
                        // §7：被新代取代 → 不覆写状态（新代已接管）。
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
                    if self.shutdown_requested.load(Ordering::SeqCst) {
                        self.set_state(CoreState::Stopped);
                        return;
                    }
                    if self.generation.load(Ordering::SeqCst) != generation {
                        // §7：被新代取代 → 不覆写状态（新代已接管）。
                        return;
                    }
                    if failure == CoreError::NoWorkspace {
                        // §4.6：工作区被并发清除等竞态 → 回到 NoWorkspace 引导，不报 Failed。
                        self.set_state(CoreState::NoWorkspace);
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
        let (exe, rejected) = core_server_path();
        let workspace = self.workspace().ok_or(CoreError::NoWorkspace)?; // start() 已保证存在；防御式返回
        let log = open_core_log().map_err(|_| CoreError::SpawnFailed)?;
        // 先开日志再判定缺失：错误页的「查看日志」按钮必须有可打开的对象，
        // 而"被跳过的历史产物"正是缺失场景下最有价值的排障线索。
        if !rejected.is_empty() {
            let names: Vec<String> = rejected
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect();
            append_log_line(
                &log,
                &format!(
                    "[runtime] 已跳过缺少构建身份的历史 core 产物：{}（请用当前 SDK 源码重新构建或重新安装）",
                    names.join("、")
                ),
            );
        }
        let exe = match exe {
            Some(path) => path,
            None => {
                append_log_line(&log, "[runtime] 无可用 core 产物（候选均缺失或无构建身份）");
                return Err(CoreError::BinaryMissing);
            }
        };
        // R3-A3（§3.3.3）：实际启动二进制的绝对路径必须落日志——验收报告据此
        // 记录"到底启动了哪个 exe"（脚本再对它做 SHA-256），杜绝解析结果不明。
        append_log_line(
            &log,
            &format!(
                "[runtime] launching core: {} (acceptance={})",
                exe.display(),
                acceptance_mode()
            ),
        );
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
        let provider_cfg = self.provider_config();
        apply_core_env(
            &mut command,
            &self.pairing,
            &self.instance_id,
            &provider_cfg,
            &log,
        );

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
                let build_id = value["build_id"].as_str().unwrap_or("unknown").to_string();
                // §6.1.4：core_ready 上报的 build_id 与壳编译期期望（owo-build-info
                // 烧录）不一致时只告警不失败——开发机 dirty 构建常见；发布包错配
                // 会在日志留下可追溯证据。expectedBuildId 经 get_core_connection 下发。
                if build_id != "unknown" && build_id != owo_build_info::COMMIT {
                    append_log_line(
                        &log,
                        &format!(
                            "[runtime] build id 与壳期望不一致：core={build_id} shell={}（版本可能错配）",
                            owo_build_info::COMMIT
                        ),
                    );
                }
                Ok((
                    CoreConnection {
                        pid: health.pid.unwrap_or(pid),
                        port,
                        api_version: health.api_version,
                        build_id,
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
                let provider_cfg = self.provider_config();
                apply_core_env(
                    &mut command,
                    &self.pairing,
                    &self.instance_id,
                    &provider_cfg,
                    &log,
                );
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

/// R3（§8.3）：`--version` 输出是否携带**可用构建身份**。
///
/// R2 起 `owo-agent` 产物一律打印 `commit=<sha> dirty=<bool> built_at=<iso>
/// source=compiled`。缺该行说明它是构建身份链之前的历史产物——这类文件曾被
/// 手工复制进壳的 `target/debug`，而"同目录优先"解析会静默选中它，表现为
/// 40 秒握手超时的"莫名冷启动失败"（实测）。宁可拒绝并给出明确日志，也不要
/// 拉起一个来历不明的核心。
fn version_output_has_build_identity(stdout: &str) -> bool {
    stdout
        .split_whitespace()
        .any(|token| token.starts_with("commit=") && !token.starts_with("commit=unknown"))
        && stdout.contains("built_at=")
}

/// 探测单个候选的构建身份；探测失败（无法启动 / 5s 无输出 / 非零退出 /
/// 无 `commit=`）一律返回 false。
fn candidate_has_build_identity(path: &Path) -> bool {
    let mut child = match Command::new(path)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW：探测不得弹窗
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return false;
    };
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut text = String::new();
        if BufReader::new(stdout).read_to_string(&mut text).is_ok() {
            let _ = tx.send(text);
        }
    });
    let Ok(text) = rx.recv_timeout(Duration::from_secs(5)) else {
        let _ = child.kill();
        return false;
    };
    let Ok(status) = child.wait() else {
        let _ = child.kill();
        return false;
    };
    status.success() && version_output_has_build_identity(&text)
}

/// 在候选列表中挑选第一个"存在且具备构建身份"的 core，并把**存在但被拒绝**的
/// 候选回传给调用方写日志（不静默丢弃：这是排障的关键线索）。
fn pick_current_core(candidates: &[PathBuf]) -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut rejected = Vec::new();
    for candidate in candidates {
        if !candidate.exists() {
            continue;
        }
        if candidate_has_build_identity(candidate) {
            return (Some(candidate.clone()), rejected);
        }
        rejected.push(candidate.clone());
    }
    (None, rejected)
}

/// R3-A3（指南 §3.3.3）：验收模式判定——**仅 debug 构建**且 `OWO_DESKTOP_ACCEPTANCE=1`
/// 生效。release 构建完全忽略该变量（产品路径不存在这条旁路）。
fn acceptance_mode() -> bool {
    cfg!(debug_assertions)
        && std::env::var("OWO_DESKTOP_ACCEPTANCE")
            .map(|value| value == "1")
            .unwrap_or(false)
}

/// 便携/开发双模式的 core 可执行文件定位（R3：解析结果必须带构建身份）。
///
/// 返回 `(选中的 core, 存在但被拒绝的历史产物)`。选中为 `None` 时调用方按
/// `BinaryMissing` 处理——"存在但来历不明"与"缺失"对用户的可操作动作相同：
/// 重新构建/重装，而不是等握手超时。
///
/// 验收模式（§3.3.3）：候选只来自 `OWO_SIDECAR_ROOT`（未设时为壳所在目录），
/// **禁止**回退仓库 target、PATH 或历史安装目录——故障场景因此不再需要改名
/// 真实 debug 产物（R3-BUG-03），"目录里没有 sidecar"即真实的 binary_missing。
pub fn core_server_path() -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if acceptance_mode() {
        let root = std::env::var_os("OWO_SIDECAR_ROOT")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(|dir| dir.to_path_buf()))
            });
        let Some(dir) = root else {
            return (None, Vec::new());
        };
        candidates.push(dir.join("owo-agent-x64.exe"));
        candidates.push(dir.join("owo-agent.exe"));
        return pick_current_core(&candidates);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("owo-agent-x64.exe"));
            candidates.push(dir.join("owo-agent.exe"));
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(
        manifest
            .parent()
            .and_then(|parent| parent.parent())
            .and_then(|parent| parent.parent())
            .map(|root| root.join("target").join("debug").join("owo-agent.exe"))
            .unwrap_or_else(|| PathBuf::from("owo-agent.exe")),
    );
    pick_current_core(&candidates)
}

/// §4.6 工作区持久化：数据目录（`%LOCALAPPDATA%\OwO\Agent\`）下的 `workspace.json`。
/// 安装目录只用于寻找资源，数据目录只用于状态持久化，项目工作区三者分离；
/// 不再从可执行文件所在目录推导工作区（那会让工具权限作用域错绑到程序文件）。
fn workspace_state_path() -> Option<PathBuf> {
    local_appdata().map(|base| base.join("OwO").join("Agent").join("workspace.json"))
}

fn load_saved_workspace() -> Option<PathBuf> {
    let path = workspace_state_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let workspace = value.get("path")?.as_str()?;
    let candidate = PathBuf::from(workspace);
    if candidate.is_dir() {
        Some(candidate)
    } else {
        None
    }
}

fn save_workspace(path: &std::path::Path) -> Result<(), String> {
    let state_path = workspace_state_path().ok_or_else(|| "无法确定数据目录".to_string())?;
    if let Some(dir) = state_path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("创建数据目录失败：{error}"))?;
    }
    let payload = serde_json::json!({ "path": path.to_string_lossy() });
    let text = serde_json::to_string_pretty(&payload)
        .map_err(|error| format!("序列化工作区失败：{error}"))?;
    std::fs::write(&state_path, text).map_err(|error| format!("保存工作区失败：{error}"))
}

/// 注入核心子进程环境（§4.2/§4.7/§4.8）：配对证明 + 实例身份在 debug/release 中
/// 保持一致（生产协议单一）；开发便利经显式 `OWO_DESKTOP_DEV_AUTH=1` 开关启用，
/// 默认关闭——不允许"调试版能用、安装包空壳"的双标准。
///
/// `OWO_DESKTOP_RELEASE=1` 仅用于收紧浏览器 CORS 边界（发布版禁 http://localhost
/// 直连核心），不参与配对证明协议本身。
///
/// §4.8：模型提供商只按用户显式选择注入（provider.rs），禁止依据其他环境变量
/// （如 DASHSCOPE_API_KEY）静默改写端点/模型；历史凭据仅提示不迁移。
fn apply_core_env(
    command: &mut Command,
    pairing: &str,
    instance: &str,
    provider_cfg: &ProviderConfig,
    log_path: &std::path::Path,
) {
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
    crate::provider::apply_provider_env(command, provider_cfg, log_path);
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
    use super::{
        core_server_path, pick_current_core, redact, version_output_has_build_identity,
        workspace_state_path, CoreError, CoreRuntime, CoreState, ProviderConfig, RESTART_DELAYS,
    };
    use std::path::PathBuf;
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
        assert_eq!(CoreError::NoWorkspace.code(), "core/no_workspace");
        for error in [
            CoreError::BinaryMissing,
            CoreError::SpawnFailed,
            CoreError::HandshakeTimeout,
            CoreError::IdentityMismatch,
            CoreError::ExitedUnexpectedly,
            CoreError::NoWorkspace,
        ] {
            assert!(!error.user_message().is_empty(), "{:?} 需要用户文案", error);
        }
    }

    // ---- R3（§8.3）core 产物身份解析 ----

    /// 构建身份判定只接受 R2 起的完整身份行；"来历不明"（无 commit /
    /// `commit=unknown` / 缺 built_at）一律判为不可用。
    #[test]
    fn build_identity_probe_accepts_only_current_identity_line() {
        assert!(version_output_has_build_identity(
            "owo-agent 0.1.0 api=0.7 commit=cec606583d1280af dirty=false built_at=2026-09-17T15:45:56Z source=compiled"
        ));
        // R2 之前的历史产物：只有版本行（实测被手工复制进壳 target 后劫持解析）。
        assert!(!version_output_has_build_identity("owo-agent 0.1.0"));
        // 非 git 环境编译出的身份等同未知。
        assert!(!version_output_has_build_identity(
            "owo-agent 0.1.0 commit=unknown built_at=2026-09-17T15:45:56Z"
        ));
        // 有 commit 但缺 built_at：身份不完整。
        assert!(!version_output_has_build_identity(
            "owo-agent commit=abc123"
        ));
    }

    /// R3-A3（§3.3.3）：验收模式 = `debug_assertions` 且 `OWO_DESKTOP_ACCEPTANCE=1`。
    /// 行为冻结：候选只来自 `OWO_SIDECAR_ROOT`；空目录 → BinaryMissing（**不得**
    /// 回退仓库 target——那正是故障矩阵被迫改名真实 debug core 的根因 R3-BUG-03）；
    /// 场景目录内"存在但无身份"的候选照旧进 rejected 清单留痕。
    #[test]
    fn acceptance_mode_uses_sidecar_root_only_and_never_falls_back() {
        if !cfg!(debug_assertions) {
            // release 构建完全忽略该变量：本用例只在 debug 下断言（产品旁路不存在）。
            return;
        }
        let _serial = data_dir_serial();
        let temp = std::env::temp_dir().join(format!("owo-accept-root-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).expect("场景根目录");
        let prev_accept = std::env::var_os("OWO_DESKTOP_ACCEPTANCE");
        let prev_root = std::env::var_os("OWO_SIDECAR_ROOT");
        std::env::set_var("OWO_DESKTOP_ACCEPTANCE", "1");
        std::env::set_var("OWO_SIDECAR_ROOT", &temp);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (selected, rejected) = core_server_path();
            assert!(
                selected.is_none(),
                "空 sidecar 根不得回退仓库产物找 core：{selected:?}"
            );
            assert!(
                rejected.is_empty(),
                "验收模式不扫描仓库目录，不应出现历史产物拒绝项：{rejected:?}"
            );
            // 场景目录放"无构建身份"的可执行文件：必须被拒并回传留痕（证明扫描范围=根目录）。
            let fake = temp.join("owo-agent.exe");
            std::fs::copy(env!("CARGO"), &fake).expect("复制无身份候选");
            let (selected, rejected) = core_server_path();
            assert!(selected.is_none(), "无身份候选不得被选中：{selected:?}");
            assert_eq!(
                rejected,
                vec![fake.clone()],
                "场景目录内的无身份候选必须回传供日志留痕"
            );
        }));
        match prev_accept {
            Some(value) => std::env::set_var("OWO_DESKTOP_ACCEPTANCE", value),
            None => std::env::remove_var("OWO_DESKTOP_ACCEPTANCE"),
        }
        match prev_root {
            Some(value) => std::env::set_var("OWO_SIDECAR_ROOT", value),
            None => std::env::remove_var("OWO_SIDECAR_ROOT"),
        }
        let _ = std::fs::remove_dir_all(&temp);
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    }

    /// 候选解析：不存在的候选只是未命中（不进拒绝清单），存在但无构建身份的
    /// 候选必须**不被选中**且**可追溯**（调用方据此写日志）。
    #[test]
    fn core_candidate_selection_rejects_identityless_artifact_and_reports_it() {
        let missing = std::env::temp_dir().join("owo-not-exist-core-9f3a.exe");
        assert!(!missing.exists());
        // cargo.exe 是真实可执行文件，但 `--version` 不含 commit= → 必须判为不可用。
        let identityless = PathBuf::from(env!("CARGO"));
        assert!(
            identityless.exists(),
            "测试前置：cargo 可执行文件应存在（{}）",
            identityless.display()
        );
        let (selected, rejected) = pick_current_core(&[missing.clone(), identityless.clone()]);
        assert!(selected.is_none(), "无身份候选不得被选中：{selected:?}");
        assert_eq!(
            rejected,
            vec![identityless.clone()],
            "存在但无身份的候选必须回传供日志留痕"
        );
        assert!(
            !rejected.contains(&missing),
            "不存在的候选不算被拒绝（未命中 ≠ 来历不明）"
        );
    }

    // ---- §4.6 工作区 ----

    /// 数据目录指向临时目录，避免污染真实 `%LOCALAPPDATA%\OwO\Agent`。
    ///
    /// R3 收口：`LOCALAPPDATA` 是**进程级**共享状态，此前靠“调用方尽快完成读写”
    /// 的口头约定 + `--test-threads=1` 才不互相踩（一个用例 `remove_var` 会让并行的
    /// 另一用例落回真实数据目录）。现以全局互斥锁强制串行，`cargo test` 默认并行
    /// 同样稳定——门禁不再依赖手工参数。
    fn data_dir_serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_isolated_data_dir(block: impl FnOnce()) {
        let _serial = data_dir_serial();
        let temp = std::env::temp_dir().join(format!("owo-desktop-test-{}", uuid::Uuid::new_v4()));
        let previous = std::env::var_os("LOCALAPPDATA");
        std::env::set_var("LOCALAPPDATA", &temp);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(block));
        match previous {
            Some(previous) => std::env::set_var("LOCALAPPDATA", previous),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
        let _ = std::fs::remove_dir_all(&temp);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn no_workspace_enters_no_workspace_state_without_core() {
        with_isolated_data_dir(|| {
            let runtime = CoreRuntime::new_with_workspace("pairing".into(), "inst".into(), None);
            let runtime = std::sync::Arc::new(runtime);
            runtime.start();
            assert!(
                matches!(runtime.state(), CoreState::NoWorkspace),
                "无工作区时必须停留在 NoWorkspace，不得拉起 core：{:?}",
                runtime.state()
            );
            assert_eq!(runtime.workspace(), None);
            assert!(!runtime.is_running());
        });
    }

    #[test]
    fn set_workspace_validates_persists_and_updates_memory() {
        with_isolated_data_dir(|| {
            let runtime = std::sync::Arc::new(CoreRuntime::new_with_workspace(
                "pairing".into(),
                "inst".into(),
                None,
            ));
            // 非法：目录不存在。
            let missing = std::env::temp_dir().join("owo-desktop-definitely-missing");
            assert!(runtime.set_workspace(&missing).is_err());
            assert_eq!(runtime.workspace(), None, "失败必须保持原工作区");
            // 非法：文件而非目录。
            let file = std::env::temp_dir().join("owo-desktop-file.txt");
            std::fs::write(&file, "x").unwrap();
            assert!(runtime.set_workspace(&file).is_err());
            let _ = std::fs::remove_file(&file);
            // 合法：真实目录 → 内存更新 + 数据目录落盘可重读。
            let dir = std::env::temp_dir().join("owo-desktop-ws-ok");
            std::fs::create_dir_all(&dir).unwrap();
            let canonical = runtime.set_workspace(&dir).expect("合法目录必须成功");
            assert_eq!(runtime.workspace(), Some(canonical.clone()));
            let state_path = workspace_state_path().expect("数据目录可解析");
            let text = std::fs::read_to_string(state_path).unwrap();
            assert!(
                text.contains("owo-desktop-ws-ok"),
                "工作区必须持久化：{text}"
            );
            // 新实例（模拟重启）从数据目录恢复最近项目。
            let reloaded = CoreRuntime::new("pairing".into(), "inst".into());
            assert_eq!(
                reloaded.workspace(),
                Some(canonical),
                "重启后必须恢复已保存的工作区"
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn provider_choice_persists_and_reloads() {
        with_isolated_data_dir(|| {
            let runtime = std::sync::Arc::new(CoreRuntime::new("pairing".into(), "inst".into()));
            let config = ProviderConfig {
                mode: crate::provider::ProviderMode::Ollama,
                base_url: None,
                model: Some("qwen2.5".into()),
            };
            runtime.set_provider(&config).expect("合法配置可保存");
            let reloaded = CoreRuntime::new("pairing".into(), "inst".into());
            let restored = reloaded.provider_config();
            assert_eq!(restored.mode, config.mode);
            assert_eq!(restored.model.as_deref(), Some("qwen2.5"));
            assert_eq!(restored.effective_base_url(), "http://127.0.0.1:11434/v1");
        });
    }
}
