use super::{
    decode_process_output, resolve_session_path, strip_verbatim_prefix, tool_sandbox_policy, Tool,
    ToolContext, ToolSpec,
};
use crate::external_tools;
use async_trait::async_trait;
use owo_agent_kernel::required_string;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 后台 shell 记录（`run_command background=true` 产生）。
#[derive(Clone)]
struct BackgroundShell {
    handle: crate::sandbox::SandboxHandle,
    log_path: PathBuf,
    done: Arc<std::sync::atomic::AtomicBool>,
    exit_code: Arc<Mutex<Option<i32>>>,
}

fn background_shells() -> &'static Mutex<HashMap<String, BackgroundShell>> {
    static SHELLS: std::sync::OnceLock<Mutex<HashMap<String, BackgroundShell>>> =
        std::sync::OnceLock::new();
    SHELLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `shell_output`：读取后台 shell 的累积输出与状态。
pub(super) struct ShellOutputTool;

#[async_trait]
impl Tool for ShellOutputTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "shell_output".into(),
            description: "查看后台 shell 的输出与状态（shell_id 来自 run_command background=true）"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": { "shell_id": { "type": "string" } },
                "required": ["shell_id"]
            }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let shell_id = required_string(&args, "shell_id")?;
        let shell = background_shells()
            .lock()
            .map_err(|_| "后台 shell 注册表中毒".to_string())?
            .get(&shell_id)
            .cloned()
            .ok_or_else(|| format!("未知 shell_id：{shell_id}"))?;
        let log = std::fs::read(&shell.log_path).unwrap_or_default();
        const TAIL_BYTES: usize = 32 * 1024;
        let slice = if log.len() > TAIL_BYTES {
            &log[log.len() - TAIL_BYTES..]
        } else {
            &log[..]
        };
        let done = shell.done.load(std::sync::atomic::Ordering::Relaxed);
        let exit_code = *shell
            .exit_code
            .lock()
            .map_err(|_| "后台 shell 状态锁中毒".to_string())?;
        Ok(json!({
            "shell_id": shell_id,
            "running": !done,
            "exit_code": exit_code,
            "log_path": shell.log_path.display().to_string(),
            "output": decode_process_output(slice),
        }))
    }
}

/// `kill_shell`：终止后台 shell（仅限本 Agent 启动的 shell）。
pub(super) struct KillShellTool;

#[async_trait]
impl Tool for KillShellTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kill_shell".into(),
            description: "终止后台 shell（仅限 run_command background=true 启动的 shell）".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "shell_id": { "type": "string" } },
                "required": ["shell_id"]
            }),
            effect: None,
        }
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let shell_id = required_string(&args, "shell_id")?;
        let shell = background_shells()
            .lock()
            .map_err(|_| "后台 shell 注册表中毒".to_string())?
            .get(&shell_id)
            .cloned()
            .ok_or_else(|| format!("未知 shell_id：{shell_id}"))?;
        let killed = {
            let manager = crate::sandbox::default_manager();
            let mut manager = manager
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            manager.kill(&shell.handle).is_ok()
        };
        shell.done.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(json!({ "shell_id": shell_id, "killed": killed }))
    }
}

pub(super) struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "在工作区内执行 shell 命令（需审批，60 秒超时）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "background": { "type": "boolean", "description": "后台运行（返回 shell_id，用 shell_output/kill_shell 管理）" }
                },
                "required": ["command"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let command = required_string(&args, "command")?;
        let timeout_ms = args
            .get("_host_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(60_000)
            .clamp(1, 60_000);
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string)
            .map(|path| resolve_session_path(ctx, &path))
            .transpose()?
            .unwrap_or_else(|| ctx.workspace.to_path_buf());

        // 沙箱门卫：run_command 统一经 SandboxManager 执行（X01）。
        // 策略：工作区作用域 + 危险片段 deny + Job 级隔离（允许显式降级，审计记录）。
        let mut policy = tool_sandbox_policy(ctx, "run_command");
        policy.cpu_ms = Some(60_000);
        policy.mem_mb = Some(1024);
        // `cmd /C <外部命令>` 至少占 2 个 Job 进程（cmd + 子进程）；默认 limit=1 会
        // 直接报 "Not enough quota"。放宽到 16，仍能兜住进程炸弹。
        policy.active_process_limit = Some(16);
        // 命令文本（cmd /C <command> 的命令体）同样过 deny 检查；用词级检查避免
        // `format(x)`、`--format=json` 这类正常参数被整串子串误杀。
        if let Some(fragment) =
            crate::sandbox::SandboxCommand::deny_hit_in_command(&command, &policy.deny_programs)
        {
            return Err(format!("命令命中危险黑名单片段：{fragment}"));
        }
        let mut sandbox_command = crate::sandbox::SandboxCommand::new("cmd", policy.clone())
            .with_args(vec!["/C".to_string(), command.to_string()])
            .with_cwd(cwd.clone());
        if args
            .get("background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // 后台 shell：输出重定向到日志文件，句柄入注册表，watcher 任务等待退出。
            // 日志必须落在**工作区内**：沙箱文件作用域只允许工作区，写到 %TEMP% 会被拒。
            let shell_id = uuid::Uuid::new_v4().to_string();
            // 去 verbatim 前缀：`\\?\C:\…` 在沙箱（受限令牌）下做 cmd 重定向会失败。
            let workspace_plain = strip_verbatim_prefix(ctx.workspace);
            let log_dir = workspace_plain.join(".owo").join("shells");
            tokio::fs::create_dir_all(&log_dir)
                .await
                .map_err(|error| format!("创建后台日志目录失败：{error}"))?;
            let log_path = log_dir.join(format!("{shell_id}.log"));
            // 用包装脚本而不是 `cmd /C "<cmd> > "<log>" 2>&1"`：嵌套引号会被沙箱的
            // 参数引用破坏（cmd 提前截断 → exit 1、日志不生成）。
            let script_path = log_dir.join(format!("{shell_id}.cmd"));
            let script = format!(
                "@echo off\r\n{} > \"{}\" 2>&1\r\nexit /b %ERRORLEVEL%\r\n",
                command,
                log_path.display()
            );
            tokio::fs::write(&script_path, script)
                .await
                .map_err(|error| format!("写入后台脚本失败：{error}"))?;
            let mut background_command = crate::sandbox::SandboxCommand::new("cmd", policy.clone())
                .with_args(vec![
                    "/C".to_string(),
                    script_path.to_string_lossy().into_owned(),
                ])
                .with_cwd(cwd.clone());
            if let Some(path) = external_tools::path_with_bundled_tools() {
                background_command.env.push(("PATH".to_string(), path));
            }
            let manager = crate::sandbox::default_manager();
            let process = {
                let mut manager = manager
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                manager
                    .spawn(&background_command)
                    .map_err(|error| format!("沙箱拒绝执行（{command}）：{error}"))?
            };
            let handle = process.handle.clone();
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let exit_code: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
            let done_task = Arc::clone(&done);
            let code_task = Arc::clone(&exit_code);
            tokio::task::spawn_blocking(move || {
                let mut process = process;
                if let Ok(info) = process.wait_output() {
                    if let Ok(mut guard) = code_task.lock() {
                        *guard = Some(info.exit_code);
                    }
                }
                done_task.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            background_shells()
                .lock()
                .map_err(|_| "后台 shell 注册表中毒".to_string())?
                .insert(
                    shell_id.clone(),
                    BackgroundShell {
                        handle,
                        log_path: log_path.clone(),
                        done,
                        exit_code,
                    },
                );
            return Ok(json!({
                "shell_id": shell_id,
                "background": true,
                "log_path": log_path.display().to_string(),
                "hint": "用 shell_output 查看输出，kill_shell 终止",
            }));
        }
        if let Some(path) = external_tools::path_with_bundled_tools() {
            sandbox_command.env.push(("PATH".to_string(), path));
        }

        let manager = crate::sandbox::default_manager();
        let process = {
            let mut manager = manager
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            manager
                .spawn(&sandbox_command)
                .map_err(|error| format!("沙箱拒绝执行（{}）：{error}", command))?
        };

        // 同步等待放在 blocking 线程；超时仅报错，进程仍在 Job 内受限（CPU/内存上限兜底）。
        let process_handle = process.handle.clone();
        let command_started = std::time::Instant::now();
        let mut wait_task = tokio::task::spawn_blocking(move || {
            let mut process = process;
            process.wait_output()
        });
        let output = match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            &mut wait_task,
        )
        .await
        {
            Ok(joined) => joined
                .map_err(|join_error| format!("命令等待失败：{join_error}"))?
                .map_err(|error| format!("沙箱执行失败：{error}"))?,
            Err(_) => {
                let kill_result = {
                    let manager = crate::sandbox::default_manager();
                    let mut manager = manager
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    manager.kill(&process_handle)
                };
                let _ = wait_task.await;
                return Err(match kill_result {
                    Ok(()) => format!("命令执行超过宿主任务预算 {timeout_ms}ms，沙箱进程已终止"),
                    Err(error) => format!(
                        "命令执行超过宿主任务预算 {timeout_ms}ms，终止沙箱进程失败：{error}"
                    ),
                });
            }
        };

        Ok(json!({
            "command": command,
            "exit_code": output.exit_code,
            "duration_ms": command_started.elapsed().as_millis() as u64,
            "stdout": decode_process_output(&output.stdout),
            "stderr": decode_process_output(&output.stderr),
        }))
    }
}
