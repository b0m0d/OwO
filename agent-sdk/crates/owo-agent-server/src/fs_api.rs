//! 本机文件系统动作（取优合并自远端 engine）：原生「选择文件夹」对话框与
//! 用外部程序打开工作区文件——浏览器拿不到本地绝对路径/不能调系统程序，
//! 由同机服务代劳。
//!
//! 路由面：`POST /fs/pick-directory`、`POST /fs/open`。
//!
//! 安全边界（`/fs/open` 放行理由）：
//! 1. 路径必须落在当前工作区内（复用 `resolve_within`，越界一律 403）；
//! 2. 打开方式走白名单，不接受任意命令——只把用户点击的路径交给选定程序；
//! 3. 端点位于鉴权保护面（bearer token），非本机配对客户端拿不到。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

// ---------- 选择文件夹 ----------

#[derive(serde::Deserialize)]
pub(super) struct PickDirectoryRequest {
    /// 打开对话框时的起始目录（当前工作区），可为空。
    #[serde(default)]
    initial: Option<String>,
    /// 等待用户选择的秒数上限（默认 180，夹取 5..=600）。
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// 调起系统原生「选择文件夹」对话框，返回绝对路径（取消时 path 为 null）。
/// 浏览器拿不到本地绝对路径，这里由同机的本机服务代劳。
pub(super) async fn fs_pick_directory(
    Json(request): Json<PickDirectoryRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    #[cfg(windows)]
    {
        let initial = request.initial.unwrap_or_default();
        let timeout =
            std::time::Duration::from_secs(request.timeout_secs.unwrap_or(180).clamp(5, 600));
        let selected =
            tokio::task::spawn_blocking(move || pick_directory_windows(&initial, timeout))
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("选择窗口异常：{e}"),
                    )
                })?
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(Json(json!({ "path": selected })))
    }
    #[cfg(not(windows))]
    {
        let _ = request;
        Err((
            StatusCode::NOT_IMPLEMENTED,
            "系统文件夹对话框仅支持 Windows，请手动填写绝对路径".to_string(),
        ))
    }
}

/// 用 Windows PowerShell 调起原生文件夹选择框（STA + TopMost 隐形宿主，
/// 避免对话框被浏览器挡住）；超时后杀掉子进程。
#[cfg(windows)]
fn pick_directory_windows(
    initial: &str,
    timeout: std::time::Duration,
) -> Result<Option<String>, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    // 不弹出 PowerShell 控制台窗口
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = r#"
Add-Type -AssemblyName System.Windows.Forms | Out-Null
Add-Type -AssemblyName System.Drawing | Out-Null
$owner = New-Object System.Windows.Forms.Form
$owner.TopMost = $true
$owner.ShowInTaskbar = $false
$owner.FormBorderStyle = 'None'
$owner.StartPosition = 'Manual'
$owner.Location = New-Object System.Drawing.Point(-2000, -2000)
$owner.Size = New-Object System.Drawing.Size(1, 1)
$owner.Show()
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog
$dialog.Description = '选择 OwO Agent 工作区文件夹'
$dialog.ShowNewFolderButton = $true
$initial = $env:OWO_PICK_INITIAL
if ($initial -and (Test-Path -LiteralPath $initial -PathType Container)) { $dialog.SelectedPath = $initial }
if ($dialog.ShowDialog($owner) -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Out.Write($dialog.SelectedPath) }
"#;
    let mut child = Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", script])
        .env("OWO_PICK_INITIAL", initial)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("无法启动系统选择窗口（powershell）：{e}"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("等待系统选择窗口超时（可能服务不在你的桌面会话中）".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Err(e) => return Err(format!("系统选择窗口状态异常：{e}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("读取选择结果失败：{e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("系统选择窗口失败：{}", stderr.trim()));
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Ok(None); // 用户取消选择
    }
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!("所选路径不是文件夹：{path}"));
    }
    Ok(Some(path))
}

// ---------- 打开工作区文件 ----------

#[derive(serde::Deserialize)]
pub(super) struct OpenPathRequest {
    /// 工作区内的相对路径或绝对路径。
    path: String,
    /// 可选行号：仅对支持定位的编辑器生效（VS Code `-g`）。
    #[serde(default)]
    line: Option<u32>,
    /// 打开方式：system / notepad / vscode（未知取值回落 system）。
    #[serde(default)]
    opener: Option<String>,
}

/// 在工作区内打开文件（工具步骤 chip 与回合汇报卡里点击路径的动作）。
pub(super) async fn fs_open(
    State(state): State<Arc<AppState>>,
    Json(request): Json<OpenPathRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let raw = request.path.trim();
    if raw.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "path 不能为空".to_string()));
    }
    let resolved = owo_agent_core::permissions::resolve_within(&state.workspace, raw)
        .map_err(|error| (StatusCode::FORBIDDEN, error))?;
    let path = resolved
        .canonicalize()
        .map_err(|error| (StatusCode::NOT_FOUND, format!("路径不可访问：{error}")))?;
    let opener = normalize_opener(request.opener.as_deref());
    let line = request.line.filter(|value| *value > 0);
    let opened = tokio::task::spawn_blocking(move || launch_opener(&path, line, &opener)).await;
    let used = opened
        .map_err(|error| internal_error(format!("打开文件异常：{error}")))?
        .map_err(internal_error)?;
    Ok(Json(json!({ "ok": true, "opener": used })))
}

/// 内部错误统一映射为 500（文本原样透出，前端 toast 直接可读）。
fn internal_error(message: String) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// 打开方式白名单归一化：未知取值一律回落系统默认。
fn normalize_opener(raw: Option<&str>) -> String {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("notepad") => "notepad".to_string(),
        Some("vscode") => "vscode".to_string(),
        _ => "system".to_string(),
    }
}

/// 启动外部程序打开路径，返回**实际生效**的打开方式（回落时如实返回 system，不假装成功）。
/// 子进程直接 detach：编辑器/查看器要一直开着，服务端不等待也不回收。
fn launch_opener(
    path: &std::path::Path,
    line: Option<u32>,
    opener: &str,
) -> Result<String, String> {
    let launched = match opener {
        "vscode" => {
            let target = format!("{}:{}", path.display(), line.unwrap_or(1));
            std::process::Command::new("code")
                .arg("-g")
                .arg(&target)
                .spawn()
                .is_ok()
        }
        "notepad" => std::process::Command::new("notepad")
            .arg(path)
            .spawn()
            .is_ok(),
        _ => false,
    };
    if launched {
        return Ok(opener.to_string());
    }
    launch_system_default(path)?;
    Ok("system".to_string())
}

/// 系统默认程序打开。Windows 走 explorer：不经 cmd，路径里的 `& | ^ >` 不会被当成
/// shell 元字符（否则一个名为 `a&calc.txt` 的文件就能变成命令注入）。
#[cfg(windows)]
fn launch_system_default(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("启动系统默认程序失败：{error}"))
}

#[cfg(target_os = "macos")]
fn launch_system_default(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("启动系统默认程序失败：{error}"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launch_system_default(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("启动系统默认程序失败：{error}"))
}
