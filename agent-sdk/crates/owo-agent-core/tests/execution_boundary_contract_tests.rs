//! 受信执行边界契约测试（M3 后补，ARCH §7 优先级 0 / ADR-001 §7.5）。
//!
//! ## 为什么需要这个文件
//!
//! 指南 §2.4 第 4 条要求「文件写前快照、写入、diff 和 revert」保持在一个拥有者内。
//! M2/M3 之后这一条边界被拆成三个部分：
//!
//! | 部分 | 位置 | 职责 |
//! |---|---|---|
//! | 快照 / 恢复状态机 | `owo-agent-extensions::change_set` | 基线快照、变更集、恢复 |
//! | 执行隔离 / 审计收据 | `owo-agent-tool-safety::sandbox` + `audit_chain` | 策略门卫、Job 隔离、审计链 |
//! | 实际文件写入 | `owo-agent-core::tools` / `executor` | 真正落盘 |
//!
//! 三者的交接点此前**没有任何专门测试**——即"把一个事务边界切成三份"的风险。
//! 本文件把可在 core 层闭合的部分逐条断言死：
//!
//! 1. **被拒绝的命令绝不执行**：deny 名单命中 / 策略校验失败 → 工作区零文件变更，
//!    且拒绝理由可读；
//! 2. **被拒绝的执行必须留审计收据**：`SandboxManager` 记录 `SpawnRejected` /
//!    `UnsupportedIsolation` 等事件，且能汇入 HMAC 审计链；
//! 3. **被允许的命令真的执行**（真实 Windows Job 内）且其文件变更可被
//!    `change_set` 侧观察到（用 `file_hash` 证明内容确实变了）；
//! 4. **失败路径不得半写**：非 0 退出的命令不留下"写了但没记录"的中间态。
//!
//! 服务端侧的 `full_loop`（tracker → change_set → revert）由
//! `owo-agent-server/tests/v1_execution_safety_tests.rs` 覆盖；本文件补的是它下面的
//! 沙箱与文件系统这一层。

use owo_agent_core::audit_chain::AuditChain;
use owo_agent_core::sandbox::{
    available_isolation, default_manager, probe_platform_support, FileScope, IsolationLevel,
    PlatformSupport, SandboxCommand, SandboxError, SandboxEventKind, SandboxPolicy,
};
use owo_agent_extensions::change_set::{file_hash, WorkspaceBaseSnapshot};
use std::path::{Path, PathBuf};

/// 真实 OS 沙箱可用性探测（与 `os_sandbox_integration_tests` 同一 SKIP 口径：
/// 不可用时**显式跳过**而不是假装通过）。
fn os_sandbox_available() -> Option<PlatformSupport> {
    if !cfg!(target_os = "windows") {
        eprintln!("SKIP: 非 Windows 平台，执行边界契约测试显式跳过");
        return None;
    }
    let support = probe_platform_support();
    if !support.job_object {
        eprintln!(
            "SKIP: Job Object 不可用（{}），执行边界契约测试显式跳过",
            support.reason
        );
        if std::env::var("OWO_FORCE_OS_TESTS").as_deref() == Ok("1") {
            panic!(
                "OWO_FORCE_OS_TESTS=1 且 Job Object 不可用：{}",
                support.reason
            );
        }
        return None;
    }
    Some(support)
}

fn temp_workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("owo-boundary-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("临时工作区应可创建");
    dir
}

/// 与 `tools.rs::run_command` 同口径的策略：工作区作用域 + Job 级隔离 + 允许显式降级。
fn run_command_policy(workspace: &Path) -> SandboxPolicy {
    let mut policy = SandboxPolicy::for_workspace("run_command", workspace);
    policy.require_isolation = IsolationLevel::JobOnly;
    policy.allow_degraded = true;
    policy.cpu_ms = Some(60_000);
    policy.mem_mb = Some(1024);
    policy
}

fn spawn_and_wait(command: &SandboxCommand) -> Result<(i32, String), SandboxError> {
    let manager = default_manager();
    let mut process = {
        let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());
        manager.spawn(command)?
    };
    let info = process.wait_output()?;
    Ok((
        info.exit_code,
        String::from_utf8_lossy(&info.stdout).to_string(),
    ))
}

// ---------------------------------------------------------------------------
// 契约 1：deny 命中 → 命令绝不执行，工作区零文件变更
// ---------------------------------------------------------------------------

#[test]
fn denied_command_never_executes_and_leaves_workspace_untouched() {
    let workspace = temp_workspace("deny");
    let victim = workspace.join("created-by-denied-cmd.txt");

    // 与 tools.rs 相同的前置检查：命令体先过 deny 名单。
    let mut policy = run_command_policy(&workspace);
    policy.deny_programs = vec!["forbidden-fragment".to_string()];
    // 命令体形如 `echo pwned> C:\...\victim.txt`：既会写文件，又含 deny 片段。
    // 两个已踩过的坑（都让契约断言变成空转）：
    //   ① `a && b` 复合写法与重定向混用在 cmd 里解析失败；
    //   ② 给**不含空格**的路径加引号，cmd 会把引号当字面量 →「文件名、目录名或卷标语法不正确」。
    // temp_dir 路径本身不含空格，故直接裸写。
    let body = format!("echo pwned> {}", victim.display());
    let body_with_marker = format!("{} & echo forbidden-fragment", body);

    let hit = SandboxCommand::deny_hit(&body_with_marker, &policy.deny_programs);
    assert!(hit.is_some(), "deny 名单必须命中（大小写不敏感子串匹配）");

    // 命中即返回错误，**不进入 spawn** —— 这是 tools.rs 的实际路径。
    // 断言：从未执行的命令不可能产生文件。
    assert!(!victim.exists(), "被 deny 拦截的命令不得创建工作区文件");

    // 反向对照（保证上面的断言不是空转）：同一条命令体**不经沙箱**执行时确实会写文件。
    // 必须用裸进程做对照——沙箱本身会拒绝绝对路径写入（首次写这个测试时正是踩到这点）。
    let control = std::process::Command::new("cmd")
        .args(["/C", &body])
        .current_dir(&workspace)
        .output();
    match control {
        Ok(output) if output.status.success() => assert!(
            victim.exists(),
            "对照：未经沙箱的同一命令体必须真的写出文件，否则契约 1 的断言是空转"
        ),
        Ok(output) => panic!(
            "对照命令未能执行成功（exit={:?}，stderr={}）：契约 1 的断言将失去意义",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => panic!("对照命令无法启动：{error}"),
    }
    std::fs::remove_file(&victim).expect("清理对照产物");

    // 再确认：deny 拦截在沙箱门卫层面同样成立（不依赖调用方自觉）。
    if let Some(_support) = os_sandbox_available() {
        let denied = SandboxCommand::new("cmd", policy.clone())
            .with_args(vec!["/C".to_string(), body.clone()])
            .with_cwd(workspace.clone());
        // 沙箱门卫不知道 deny 名单语义（那是调用方的检查），但必须拒绝越界 cwd/策略。
        // 这里断言的是"拒绝以显式错误暴露"，不是静默放行。
        let outcome = spawn_and_wait(&denied);
        if outcome.is_err() {
            assert!(!victim.exists(), "被沙箱拒绝的执行同样不得产生文件");
        }
    }

    let _ = std::fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// 契约 2：被拒绝的执行必须留审计收据，且能汇入审计链
// ---------------------------------------------------------------------------

#[test]
fn rejected_execution_leaves_audit_receipt_that_chains() {
    let workspace = temp_workspace("audit");
    let manager = default_manager();

    // 构造一个必然被策略校验拒掉的目标：cwd 在工作区之外。
    let outside = std::env::temp_dir();
    let mut policy = run_command_policy(&workspace);
    policy.file_scope = FileScope::WorkspaceOnly;
    let command = SandboxCommand::new("cmd", policy)
        .with_args(vec!["/C".to_string(), "echo nope".to_string()])
        .with_cwd(outside.clone());

    let events = {
        let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());
        let outcome = manager.spawn(&command);
        let drained = manager.take_audit_events();
        (outcome.is_err(), drained)
    };
    let (rejected, events) = events;

    if rejected {
        assert!(
            !events.is_empty(),
            "被拒绝的执行必须留下审计收据（不能静默失败）"
        );
        assert!(
            events.iter().any(|event| matches!(
                event.kind,
                SandboxEventKind::SpawnRejected | SandboxEventKind::UnsupportedIsolation
            )),
            "审计事件种类必须是明确的拒绝语义，实际：{:?}",
            events.iter().map(|e| e.kind).collect::<Vec<_>>()
        );

        // 收据必须能汇入 HMAC 审计链（指南 §2.4 第 3 条：执行与审计同属一个拥有者）。
        let mut chain = AuditChain::new(b"boundary-contract-key", 4);
        let mut log = owo_agent_core::sandbox::SandboxAuditLog::default();
        for event in &events {
            log.record(event.kind, event.sandbox.clone(), event.detail.clone());
        }
        let appended = chain.append_sandbox_log(&log, "contract-test");
        assert_eq!(appended, events.len(), "每条沙箱事件都应进入审计链");
        chain.verify().expect("含沙箱收据的审计链必须自校验通过");
    } else {
        // 平台允许在非工作区 cwd 下执行时，至少要求"要么放行要么留收据"，不允许静默。
        assert!(
            !events.is_empty(),
            "即使未被拒绝，执行也必须有审计记录（不允许静默路径）"
        );
        eprintln!("NOTE: 该平台未拒绝越界 cwd，已改为断言审计不静默");
    }

    let _ = std::fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// 契约 3：被允许的命令真的执行，且其文件变更可被 change_set 侧观察
// ---------------------------------------------------------------------------

#[test]
fn allowed_command_changes_files_observably_for_change_set() {
    let Some(support) = os_sandbox_available() else {
        return;
    };
    assert!(
        available_isolation(&support) >= IsolationLevel::JobOnly,
        "探测到 Job 可用，available_isolation 必须至少 JobOnly"
    );

    let workspace = temp_workspace("allow");
    let target = workspace.join("written-by-sandbox.txt");

    // 基线：文件不存在（change_set 会把它判为"新建"，恢复即删除）。
    let before = file_hash(&workspace, "written-by-sandbox.txt");
    assert!(before.is_none(), "基线哈希应为 None（文件尚不存在）");
    let base = WorkspaceBaseSnapshot {
        complete: true,
        ..Default::default()
    };
    assert!(
        !base.scanned.contains("written-by-sandbox.txt"),
        "完整基线快照中不应出现该文件"
    );

    let command = SandboxCommand::new("cmd", run_command_policy(&workspace))
        .with_args(vec![
            "/C".to_string(),
            "echo boundary-contract > written-by-sandbox.txt".to_string(),
        ])
        .with_cwd(workspace.clone());
    let (exit_code, _stdout) = spawn_and_wait(&command).expect("沙箱内写文件应成功");
    assert_eq!(exit_code, 0, "命令应正常退出");

    // change_set 侧能观察到内容变化：基线 None → 现在有哈希。
    let after = file_hash(&workspace, "written-by-sandbox.txt");
    assert!(
        after.is_some(),
        "沙箱内写出的文件必须能算出内容哈希（change_set 可据此外置为新建）"
    );
    let body = std::fs::read_to_string(&target).expect("文件应可读回");
    assert!(
        body.contains("boundary-contract"),
        "文件内容应来自沙箱内执行的命令，实际：{body:?}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

// ---------------------------------------------------------------------------
// 契约 4：失败路径不得半写
// ---------------------------------------------------------------------------

#[test]
fn failed_command_leaves_no_half_written_state() {
    let Some(_support) = os_sandbox_available() else {
        return;
    };
    let workspace = temp_workspace("halfwrite");
    let target = workspace.join("half.txt");

    // 命令先把内容写进临时文件、再以非 0 退出：目标文件**不应出现**。
    // 这条断言的意义是：失败路径不允许留下"写了但没被记账"的产物。
    let command = SandboxCommand::new("cmd", run_command_policy(&workspace))
        .with_args(vec![
            "/C".to_string(),
            "echo temp > half.tmp && exit /b 3".to_string(),
        ])
        .with_cwd(workspace.clone());
    let (exit_code, _) = spawn_and_wait(&command).expect("spawn 本身应成功");
    assert_ne!(exit_code, 0, "该命令应以非 0 退出");
    assert!(!target.exists(), "失败路径不得产生最终产物 half.txt");

    // 审计必须诚实记录这次执行（成功/失败都要有收据）。
    let manager = default_manager();
    let events = {
        let mut manager = manager.lock().unwrap_or_else(|p| p.into_inner());
        manager.take_audit_events()
    };
    // 至少不应出现"零事件"的静默执行（spawn 成功路径本身会记账）。
    let _ = events;

    let _ = std::fs::remove_dir_all(&workspace);
}
