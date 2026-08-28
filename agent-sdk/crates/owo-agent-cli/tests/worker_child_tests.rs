//! A1 契约测试：`owo-agent.exe --owo-worker-child` 受控子进程宿主（主文档 §9.1）。
//!
//! 覆盖完成标准（全部使用真实 owo-agent 二进制，`CARGO_BIN_EXE_owo-agent` 定位）：
//! 1. ready → task → result 结构化闭环与 ping/pong 心跳；
//! 2. fail 处理器的显式错误回传；非法 JSON 行不中断协议；
//! 3. sleep 超过任务时长预算时被父进程 kill（budget_aborted / Stopped，无挂起任务）；
//! 4. cancel 为协作信号（不终止子进程），shutdown/EOF 走正常退出路径；
//! 5. `--handler` 非法值被宿主明确拒绝（非零退出 + 指引文案）；
//! 6. 凭据类环境键在注入白名单前被明确拒绝；受控命令零环境继承。
//!
//! 不开放任意 shell；更长的等待/取消最终由父进程超时 kill 与 Goal abort 兜底。

use owo_agent_core::worker_pool::{PoolError, WorkerBudget, WorkerPool, WorkerSpec, WorkerStatus};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 复用 cli 源码模块（纯函数支撑面：只依赖 core/serde，可在测试 crate 内独立编译）。
/// 测试目标只用其中一部分公开函数，其余入口（dispatch/run_child 等）由二进制侧使用。
#[path = "../src/worker_child.rs"]
#[allow(dead_code)]
mod worker_child;

/// 真实宿主二进制（cargo test 注入的绝对路径）。
fn child_exe() -> &'static str {
    env!("CARGO_BIN_EXE_owo-agent")
}

fn protocol_deadline() -> Duration {
    Duration::from_secs(15)
}

type ChildStdin = tokio::process::ChildStdin;

struct RawChild {
    child: tokio::process::Child,
    stdin: ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

/// 直接以协议 flags 启动真实二进制（含 tokio 子进程管道）。
async fn spawn_raw_child(handler: &str) -> RawChild {
    let mut cmd = tokio::process::Command::new(child_exe());
    cmd.args(["--owo-worker-child", "--handler", handler])
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().expect("spawn owo-agent --owo-worker-child");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = BufReader::new(child.stdout.take().expect("stdout"));
    RawChild {
        child,
        stdin,
        stdout,
    }
}

/// 带超时的单行读取（协议必须及时响应）。
async fn read_line(reader: &mut BufReader<tokio::process::ChildStdout>) -> String {
    let mut line = String::new();
    let n = tokio::time::timeout(protocol_deadline(), reader.read_line(&mut line))
        .await
        .expect("读协议行超时")
        .expect("读协议行失败");
    assert!(n > 0, "对端已关闭（EOF），未收到期望的协议行");
    line
}

async fn write_line(stdin: &mut ChildStdin, value: &Value) {
    stdin
        .write_all(format!("{value}\n").as_bytes())
        .await
        .expect("写协议行失败");
    stdin.flush().await.expect("flush 协议行失败");
}

/// 解析协议行为结构化 JSON 并断言 type。
async fn next_type(reader: &mut BufReader<tokio::process::ChildStdout>, want: &str) -> Value {
    let line = read_line(reader).await;
    let value: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("协议行不是合法 JSON：{e}：{line}"));
    assert_eq!(value["type"], want, "应为 {want} 帧：{line}");
    value
}

// ---------------------------------------------------------------------------
// 1+2+4：真实子进程的结构化协议闭环
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_protocol_ready_task_result_and_ping_pong() {
    let mut child = spawn_raw_child("echo").await;

    // ready 握手。
    next_type(&mut child.stdout, "ready").await;

    // task → result(ok)：echo 回显请求文本，task_id 原样带回。
    write_line(
        &mut child.stdin,
        &json!({ "cmd": "task", "task_id": "t1", "input": { "text": "你好 OwO" } }),
    )
    .await;
    let result = next_type(&mut child.stdout, "result").await;
    assert_eq!(result["task_id"], "t1");
    assert_eq!(result["ok"], true);
    assert_eq!(result["output"], "你好 OwO");

    // ping → pong 心跳。
    write_line(&mut child.stdin, &json!({ "cmd": "ping" })).await;
    next_type(&mut child.stdout, "pong").await;

    // cancel 是协作信号：不发 terminate 也不得杀死进程，后续任务照常处理。
    write_line(
        &mut child.stdin,
        &json!({ "cmd": "cancel", "task_id": "none" }),
    )
    .await;
    write_line(
        &mut child.stdin,
        &json!({ "cmd": "task", "task_id": "t2", "input": { "text": "after-cancel" } }),
    )
    .await;
    let result = next_type(&mut child.stdout, "result").await;
    assert_eq!(result["ok"], true, "cancel 后任务仍应可执行");
    assert_eq!(result["output"], "after-cancel");

    // shutdown → 正常退出（exit 0），无残留。
    write_line(&mut child.stdin, &json!({ "cmd": "shutdown" })).await;
    let status = tokio::time::timeout(protocol_deadline(), child.child.wait())
        .await
        .expect("等待退出超时")
        .expect("wait 失败");
    assert!(status.success(), "shutdown 应正常退出：{status}");
}

#[tokio::test]
async fn fail_handler_returns_error_result() {
    let mut child = spawn_raw_child("fail").await;
    next_type(&mut child.stdout, "ready").await;

    write_line(
        &mut child.stdin,
        &json!({ "cmd": "task", "task_id": "f1", "input": { "message": "boom-reason" } }),
    )
    .await;
    let result = next_type(&mut child.stdout, "result").await;
    assert_eq!(result["ok"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("boom-reason"),
        "错误应携带调用方 message：{result}"
    );

    // 错误之后协议仍在服务（可继续 ping/shutdown）。
    write_line(&mut child.stdin, &json!({ "cmd": "ping" })).await;
    next_type(&mut child.stdout, "pong").await;
    write_line(&mut child.stdin, &json!({ "cmd": "shutdown" })).await;
    let _ = tokio::time::timeout(protocol_deadline(), child.child.wait())
        .await
        .expect("等待退出超时")
        .expect("wait 失败");
}

#[tokio::test]
async fn illegal_json_lines_do_not_break_protocol() {
    let mut child = spawn_raw_child("echo").await;
    next_type(&mut child.stdout, "ready").await;

    // 非法 JSON 与未知 cmd 都应被静默忽略（不崩溃、不回包、不退出）。
    for junk in [
        "this is not json",
        "{\"cmd\":\"unknown-thing\"}",
        "{}",
        "[1,2,3]",
    ] {
        child
            .stdin
            .write_all(format!("{junk}\n").as_bytes())
            .await
            .expect("写入垃圾行失败");
    }
    child.stdin.flush().await.unwrap();

    // 垃圾输入之后的合法流量必须继续可用。
    write_line(&mut child.stdin, &json!({ "cmd": "ping" })).await;
    next_type(&mut child.stdout, "pong").await;
    write_line(
        &mut child.stdin,
        &json!({ "cmd": "task", "task_id": "j1", "input": { "text": "still-alive" } }),
    )
    .await;
    let result = next_type(&mut child.stdout, "result").await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["output"], "still-alive");

    // 最后 shutdown 正常退出。
    write_line(&mut child.stdin, &json!({ "cmd": "shutdown" })).await;
    let status = tokio::time::timeout(protocol_deadline(), child.child.wait())
        .await
        .expect("等待退出超时")
        .expect("wait 失败");
    assert!(status.success(), "处理过垃圾行后仍应正常退出：{status}");
}

// ---------------------------------------------------------------------------
// 3+4：父进程预算兜底与无孤儿保证（Goal 同源机制 WorkerPool）
// ---------------------------------------------------------------------------

fn demo_spec(handler: &str, budget_secs: u64) -> WorkerSpec {
    WorkerSpec::new("child-test", child_exe())
        .args(vec![
            "--owo-worker-child".to_string(),
            "--handler".to_string(),
            handler.to_string(),
        ])
        .cwd(std::env::temp_dir().to_string_lossy().to_string())
        .env_whitelist(Vec::new()) // 零环境继承：凭据不外传
        .budget(WorkerBudget {
            max_duration_secs: budget_secs,
            ..Default::default()
        })
}

#[tokio::test]
async fn sleep_beyond_budget_is_killed_by_parent_without_orphans() {
    let pool = WorkerPool::new();
    let id = pool.spawn(demo_spec("sleep", 2)).await.expect("spawn");

    let started = std::time::Instant::now();
    let result = pool.submit(&id, &json!({ "secs": 30 })).await;
    let elapsed = started.elapsed();
    match result {
        Err(PoolError::BudgetDuration { .. }) => {}
        other => panic!("超预算 sleep 应产生 BudgetDuration：{other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(1500),
        "父进程应在预算窗口内等待后中止，而不是立刻返回"
    );
    assert_eq!(
        pool.status(&id).await,
        Some(WorkerStatus::Stopped),
        "预算中止后 worker 必须进入已定义终态"
    );
    let labels: Vec<&str> = pool.events().await.iter().map(|e| e.kind.label()).collect();
    assert!(
        labels.contains(&"started") && labels.contains(&"budget_aborted"),
        "审计事件应包含 started/budget_aborted：{labels:?}"
    );

    // 终止后不再有待处理任务：再次提交得到明确拒绝而非挂起。
    let input = json!({ "secs": 1 });
    let outcome = tokio::time::timeout(Duration::from_secs(5), pool.submit(&id, &input))
        .await
        .expect("提交到已停 worker 不应挂起");
    let err = match outcome {
        Err(e) => e,
        Ok(output) => panic!("对 Stopped worker 的再次提交不应成功：{output}"),
    };
    assert!(
        matches!(
            err,
            PoolError::Stopped(_) | PoolError::NotReady(_) | PoolError::UnknownWorker(_)
        ),
        "对 Stopped worker 的再次提交应明确拒绝：{err:?}"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn goal_style_abort_drains_pending_tasks_and_exits_child() {
    let pool = WorkerPool::new();
    let id = pool.spawn(demo_spec("sleep", 60)).await.expect("spawn");

    // 提交长任务（飞行中）→ 模拟 Goal abort 的取消传播。
    let input = json!({ "secs": 30 });
    let submit_fut = pool.submit(&id, &input);
    let cancel_fut = async {
        tokio::time::sleep(Duration::from_millis(500)).await; // 等 submit 进入 pending
        pool.cancel_pending(&id).await
    };
    let (submit_result, cancel_result) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(submit_fut, cancel_fut)
    })
    .await
    .expect("abort 后 submit 必须 promptly 返回");

    assert_eq!(
        cancel_result.expect("cancel_pending"),
        1,
        "恰有一个待处理任务被标记取消"
    );
    match submit_result {
        Err(PoolError::Cancelled(_)) => {}
        other => panic!("abort 应把飞行中任务落为 Cancelled 终态：{other:?}"),
    }

    // abort 后清理：kill 收尾并确认 worker 进入 Stopped、无 pending。
    pool.kill(&id).await.expect("kill");
    assert_eq!(pool.status(&id).await, Some(WorkerStatus::Stopped));
    let again = pool.cancel_pending(&id).await.expect("cancel after kill");
    assert_eq!(again, 0, "abort 后不得残留待处理任务");
    let labels: Vec<&str> = pool.events().await.iter().map(|e| e.kind.label()).collect();
    assert!(labels.contains(&"cancelled"), "{labels:?}");
    assert!(labels.contains(&"stopped"), "{labels:?}");
    pool.shutdown().await;
}

// ---------------------------------------------------------------------------
// 5+6：入口校验与环境防线
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_handler_is_rejected_explicitly() {
    use tokio::process::Command;
    let output = Command::new(child_exe())
        .args(["--owo-worker-child", "--handler", "bogus"])
        .env_clear()
        .output()
        .await
        .expect("run with unknown handler");
    assert!(!output.status.success(), "未知 handler 必须以非零退出拒绝");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("未知") && stderr.to_lowercase().contains("handler"),
        "拒绝信息必须指明 handler 取值问题（并给出可选值）：{stderr}"
    );
    assert!(
        stderr.contains("echo"),
        "拒绝信息应列出可选 handler：{stderr}"
    );
}

#[tokio::test]
async fn credential_env_keys_are_rejected_before_whitelist() {
    let (allowed, rejected) = worker_child::partition_credential_keys(vec![
        ("PATH".to_string(), "C:\\Windows".to_string()),
        ("OPENAI_API_KEY".to_string(), "sk-secret".to_string()),
        ("AUTH_TOKEN".to_string(), "leak-me".to_string()),
        ("MY_PASSWORD".to_string(), "p@ss".to_string()),
        ("client_secret".to_string(), "s".to_string()),
    ]);
    assert_eq!(
        allowed,
        vec![("PATH".to_string(), "C:\\Windows".to_string())],
        "仅非凭据类键允许进入白名单"
    );
    for key in [
        "OPENAI_API_KEY",
        "AUTH_TOKEN",
        "MY_PASSWORD",
        "client_secret",
    ] {
        assert!(
            rejected.iter().any(|r| r == key),
            "凭据类键 {key} 必须出现在明确拒绝清单"
        );
    }

    // 受控命令构造：零环境继承 + 协议参数精确匹配。
    let cmd = worker_child::build_child_command(
        std::path::Path::new(child_exe()),
        worker_child::ChildHandler::Echo,
    );
    assert_eq!(
        cmd.get_envs().count(),
        0,
        "协议宿主不得继承任何环境变量（凭据不可经环境泄露）"
    );
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    assert_eq!(
        args,
        vec![
            worker_child::CHILD_FLAG.to_string(),
            worker_child::HANDLER_FLAG.to_string(),
            "echo".to_string()
        ]
    );
}
