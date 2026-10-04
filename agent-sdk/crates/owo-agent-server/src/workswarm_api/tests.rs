use super::workers::*;
use super::write_lease::{manager_for_workspace, WriteLease, WriteLeaseManager, WriteScope};
use super::*;
use async_trait::async_trait;
use owo_agent_core::goal::Worker;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[test]
fn team_worker_model_uses_provider_default_unless_explicitly_overridden() {
    assert_eq!(
        super::workers::resolve_agent_model(&json!({}), None),
        owo_agent_core::gateway::MODEL_DEFAULT_SENTINEL
    );
    assert_eq!(
        super::workers::resolve_agent_model(&json!({}), Some(" configured-model ")),
        "configured-model"
    );
    assert_eq!(
        super::workers::resolve_agent_model(
            &json!({ "model": " task-model " }),
            Some("configured-model")
        ),
        "task-model"
    );
    assert_eq!(
        super::workers::resolve_agent_model(
            &json!({ "model": "default" }),
            Some("configured-model")
        ),
        "configured-model"
    );
}

/// 唯一临时目录。
fn unique_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "owo-tracked-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 建一个带初始提交的真实 git 仓库，src/a.rs 处于「执行前已脏」状态。
/// 返回 (root, head_content, pre_agent_content)。
fn git_repo_with_pre_dirty_file(tag: &str) -> (PathBuf, &'static str, &'static str) {
    let root = unique_dir(tag);
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .expect("git 可执行");
        assert!(
            output.status.success(),
            "git {args:?} 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), b"fn a() {} // HEAD\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
    // 用户先手改（执行前已脏）。
    std::fs::write(root.join("src/a.rs"), b"fn a() {} // user dirty\n").unwrap();
    (root, "fn a() {} // HEAD\n", "fn a() {} // user dirty\n")
}

fn tracking_for(root: &Path, dir: &Path) -> workspace_change_tracker::Tracker {
    workspace_change_tracker::Tracker {
        root: root.to_path_buf(),
        run_dir: dir.join("run"),
        team_id: "t1".to_string(),
        role: "implementer".to_string(),
        allowed: Vec::new(),
        cas: owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap(),
        audit: None,
    }
}

/// 模拟 Agent 写文件的测试 worker。
struct WriteWorker(PathBuf, &'static str);

#[async_trait]
impl Worker for WriteWorker {
    fn name(&self) -> &str {
        "agent"
    }
    async fn run(&self, _input: &Value) -> Result<String, String> {
        std::fs::write(&self.0, self.1).unwrap();
        Ok("ok".to_string())
    }
}

fn step_input(step_id: &str) -> Value {
    json!({ "prompt": "p", "_workswarm": {
        "step_id": step_id,
        "attempt_id": format!("t1:{}:attempt-1:epoch-7", step_id)
    } })
}

/// 核心完工要求：执行前已经修改过的文件，Agent 再次修改后必须出现在
/// changed_files；基线哈希 = 执行前内容（reject 恢复到执行前状态，不是 HEAD）；
/// 有真实修改时 diff_ref 非空。
#[tokio::test]
async fn pre_dirty_file_modified_again_enters_changed_files() {
    let dir = unique_dir("dirty");
    let (root, head, pre_agent) = git_repo_with_pre_dirty_file("dirty");
    let tracking = tracking_for(&root, &dir);
    let worker = TrackedRoleWorker {
        inner: Arc::new(WriteWorker(
            root.join("src/a.rs"),
            "fn a() { /* agent fix */ }\n",
        )),
        lease: None,
        lease_waits: None,
        tracking: Some(tracking),
    };
    let output = worker.run(&step_input("s-impl")).await;
    assert!(output.is_ok(), "{output:?}");

    let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
        .await
        .unwrap();
    let record = records.last().expect("应有变更记录");
    assert!(
        record.changed_files.contains(&"src/a.rs".to_string()),
        "执行前已脏 + Agent 再次修改 → 必须在 changed_files：{:?}",
        record.changed_files
    );
    assert!(record.diff_ref.is_some(), "有真实修改时 diff_ref 非空");

    // ChangeSet：changed_files 含该文件；基线 = 执行前内容（≠ HEAD 内容）。
    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    let sets = store.list_for_team("t1").unwrap();
    assert_eq!(sets.len(), 1, "恰好一个 ChangeSet");
    let set = &sets[0];
    assert_eq!(
        set.attempt_id.as_deref(),
        Some("t1:s-impl:attempt-1:epoch-7")
    );
    assert!(set.changed_files.contains(&"src/a.rs".to_string()));
    let base_hash = set
        .base_hashes
        .iter()
        .find(|h| h.path == "src/a.rs")
        .expect("基线含该文件");
    assert_eq!(
        base_hash.sha256.as_deref(),
        Some(owo_agent_core::cas_store::CasStore::hash_of(pre_agent.as_bytes()).as_str()),
        "基线必须是执行前内容，不是 Git HEAD（{head:?}）"
    );
    // 恢复 → 执行前脏内容回来（reject 语义的核心）。
    let report =
        owo_agent_core::change_set::restore_change_set(&root, set, &tracking_for(&root, &dir).cas)
            .await;
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert_eq!(
        std::fs::read(root.join("src/a.rs")).unwrap(),
        pre_agent.as_bytes()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 实际文件改动已发生但变更记录无法落盘时，Worker 必须拒绝成功。
#[tokio::test]
async fn changed_workspace_with_record_persistence_failure_fails_closed() {
    let dir = unique_dir("record-fail-closed");
    let (root, _, _) = git_repo_with_pre_dirty_file("record-fail-closed");
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    // 用同名目录阻止 JSON 记录文件写入，同时不影响 ChangeSet sidecar。
    std::fs::create_dir(run_dir.join("t1-workspace-changes.json")).unwrap();
    let worker = TrackedRoleWorker {
        inner: Arc::new(WriteWorker(
            root.join("src/a.rs"),
            "fn a() { /* tracked */ }\n",
        )),
        lease: None,
        lease_waits: None,
        tracking: Some(tracking_for(&root, &dir)),
    };

    let result = worker.run(&step_input("s-record-fail")).await;
    assert!(result.is_err(), "不可审计的文件变更不能返回成功");
    assert!(
        result.unwrap_err().contains("变更记录未落盘"),
        "应指出失败的留证环节"
    );
    let sets = owo_agent_core::change_set_store::ChangeSetStore::new(&run_dir)
        .list_for_team("t1")
        .unwrap();
    assert_eq!(
        sets.len(),
        1,
        "即使另一条记录失败，也要保留可恢复 ChangeSet"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&root);
}

/// ChangeSet sidecar 无法写入时，已有 workspace change record 也不能掩盖审批闭环缺失。
#[tokio::test]
async fn changed_workspace_with_changeset_persistence_failure_fails_closed() {
    let dir = unique_dir("changeset-fail-closed");
    let (root, _, _) = git_repo_with_pre_dirty_file("changeset-fail-closed");
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    // 阻止 ChangeSetStore 把团队 sidecar 当文件读取/替换；变更记录本身仍可落盘。
    std::fs::create_dir(run_dir.join("t1-change-sets.json")).unwrap();
    let worker = TrackedRoleWorker {
        inner: Arc::new(WriteWorker(
            root.join("src/a.rs"),
            "fn a() { /* changeset failure */ }\n",
        )),
        lease: None,
        lease_waits: None,
        tracking: Some(tracking_for(&root, &dir)),
    };

    let result = worker.run(&step_input("s-changeset-fail")).await;
    assert!(
        result.is_err(),
        "没有审批/恢复 ChangeSet 的文件改动不能返回成功"
    );
    assert!(
        result.unwrap_err().contains("ChangeSet 未落盘"),
        "应指出 ChangeSet 留证失败"
    );
    let records = workspace_change_tracker::load_records(&run_dir, "t1")
        .await
        .unwrap();
    assert!(records
        .iter()
        .any(|record| !record.changed_files.is_empty()));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&root);
}

/// 即使本次没有文件差异，无法持久化执行记录也不能静默报告成功。
#[tokio::test]
async fn no_change_with_record_persistence_failure_fails_closed() {
    let dir = unique_dir("nochange-record-fail-closed");
    let (root, _, _) = git_repo_with_pre_dirty_file("nochange-record-fail-closed");
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::create_dir(run_dir.join("t1-workspace-changes.json")).unwrap();
    let worker = TrackedRoleWorker {
        inner: Arc::new(WriteWorker(
            root.join("src/a.rs"),
            "fn a() {} // user dirty\n",
        )),
        lease: None,
        lease_waits: None,
        tracking: Some(tracking_for(&root, &dir)),
    };

    let result = worker.run(&step_input("s-nochange-record-fail")).await;
    assert!(result.is_err(), "缺失执行审计的步骤不能静默返回成功");
    assert!(result.unwrap_err().contains("变更记录未落盘"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&root);
}

/// 空变更：不创建 ChangeSet（列表为空），记录 changed_files 为空、diff_ref None。
#[tokio::test]
async fn no_change_creates_no_change_set() {
    let dir = unique_dir("nochange");
    let (root, _head, _pre) = git_repo_with_pre_dirty_file("nochange");
    // EchoWorker 无副作用：窗口内没有任何新变更。
    let worker = TrackedRoleWorker {
        inner: Arc::new(EchoWorker) as Arc<dyn Worker>,
        lease: None,
        lease_waits: None,
        tracking: Some(tracking_for(&root, &dir)),
    };
    let output = worker.run(&step_input("s-echo")).await;
    assert!(output.is_ok(), "{output:?}");
    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    assert!(
        store.list_for_team("t1").unwrap().is_empty(),
        "无实际变更不得创建空 ChangeSet"
    );
    let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
        .await
        .unwrap();
    let record = records.last().unwrap();
    assert!(
        record.changed_files.is_empty(),
        "{:?}",
        record.changed_files
    );
    assert!(record.diff_ref.is_none(), "无变更不得产生 diff 引用");
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------------
// 十期 · 四路 R1：ChangeSet 异常闭环——成功/失败/超时/取消都必须完成
// 变更收尾（前快照→执行→后快照→变更登记），任何结局都不得丢变更、留
// 不可解释的修改或孤儿记录。
// -----------------------------------------------------------------------

/// 按 `input.ops` 顺序执行文件操作的测试 worker（写角色落盘的等效模拟）：
/// - {"action":"write","path":…,"content":…} → 写/覆写文件；
/// - {"action":"delete","path":…} → 删除文件；
/// - {"action":"sleep","ms":…} → 延展窗口（扩大与并发方交叉的概率）；
/// - {"action":"fail","message":…} → 执行失败（模拟失败/取消结局）。
struct FileOpsWorker;

#[async_trait]
impl Worker for FileOpsWorker {
    fn name(&self) -> &str {
        "fileops"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let root = input
            .get("root")
            .and_then(Value::as_str)
            .ok_or_else(|| "fileops 缺 root".to_string())?;
        let ops = input
            .get("ops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for op in &ops {
            let action = op.get("action").and_then(Value::as_str).unwrap_or("");
            match action {
                "write" | "delete" => {
                    let path = op
                        .get("path")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "fileops 缺 path".to_string())?;
                    let target = Path::new(root).join(path);
                    if action == "write" {
                        if let Some(parent) = target.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let content = op.get("content").and_then(Value::as_str).unwrap_or("");
                        std::fs::write(&target, content)
                            .map_err(|e| format!("写 {path} 失败：{e}"))?;
                    } else {
                        std::fs::remove_file(&target)
                            .map_err(|e| format!("删 {path} 失败：{e}"))?;
                    }
                }
                "sleep" => {
                    let ms = op.get("ms").and_then(Value::as_u64).unwrap_or(0);
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                }
                "fail" => {
                    return Err(op
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("fileops 注入失败")
                        .to_string());
                }
                other => return Err(format!("未知文件操作：{other:?}")),
            }
        }
        Ok("done".to_string())
    }
}

/// fileops 专用输入（root + ops；root 供 worker 解析相对路径）。
fn fileops_input(root: &Path, ops: Value, step_id: &str) -> Value {
    json!({
        "root": root.to_str().unwrap(),
        "prompt": "p",
        "ops": ops,
        "_workswarm": { "step_id": step_id },
    })
}

/// 场景 1（写入后失败）：Worker 已真实落盘后步骤失败——仍生成 ChangeSet
/// （残留修改可审查、可恢复），记录与审计齐全；不生成空壳 ChangeSet 也不丢记录。
#[tokio::test]
async fn write_then_fail_still_generates_change_set() {
    let dir = unique_dir("wfail");
    let (root, _head, pre_agent) = git_repo_with_pre_dirty_file("wfail");
    let worker = TrackedRoleWorker {
        inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
        lease: None,
        lease_waits: None,
        tracking: Some(tracking_for(&root, &dir)),
    };
    let input = fileops_input(
        &root,
        json!([
            { "action": "write", "path": "src/a.rs",
              "content": "fn a() { /* agent fix */ }\n" },
            { "action": "fail", "message": "写后失败（注入）" },
        ]),
        "s-impl",
    );
    let output = worker.run(&input).await;
    assert!(output.is_err(), "步骤应失败：{output:?}");
    // 变更收尾必须完成：ChangeSet 已生成且基底 = 执行前内容（不是 HEAD）。
    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    let sets = store.list_for_team("t1").unwrap();
    assert_eq!(sets.len(), 1, "写后失败仍应恰好生成一个 ChangeSet");
    let set = &sets[0];
    assert!(
        set.changed_files.contains(&"src/a.rs".to_string()),
        "{:?}",
        set.changed_files
    );
    assert_eq!(
        set.status,
        owo_agent_protocol::ChangeSetStatus::PendingReview
    );
    let base_hash = set
        .base_hashes
        .iter()
        .find(|h| h.path == "src/a.rs")
        .expect("基底含该文件");
    assert_eq!(
        base_hash.sha256.as_deref(),
        Some(owo_agent_core::cas_store::CasStore::hash_of(pre_agent.as_bytes()).as_str()),
        "失败路径基底仍必须是执行前内容，不混入用户已有修改"
    );
    // 变更记录同样落盘（diff 摘要面）。
    let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
        .await
        .unwrap();
    let record = records.last().expect("应有变更记录");
    assert!(record.changed_files.contains(&"src/a.rs".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 2（写入中取消）：Worker 写入后被取消（协作中断返回已取消）——
/// 残留修改同样完成变更收尾：ChangeSet 生成、可恢复。
#[tokio::test]
async fn cancelled_during_write_still_closes_out() {
    let dir = unique_dir("wcancel");
    let (root, _head, _pre_agent) = git_repo_with_pre_dirty_file("wcancel");
    let worker = TrackedRoleWorker {
        inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
        lease: None,
        lease_waits: None,
        tracking: Some(tracking_for(&root, &dir)),
    };
    let input = fileops_input(
        &root,
        json!([
            { "action": "write", "path": "src/cancel.txt", "content": "half\n" },
            { "action": "fail", "message": "已取消" },
        ]),
        "s-impl",
    );
    let output = worker.run(&input).await;
    assert!(output.is_err(), "取消路径应返回原错误：{output:?}");
    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    let sets = store.list_for_team("t1").unwrap();
    assert_eq!(sets.len(), 1, "取消也应完成收尾生成 ChangeSet");
    assert!(
        sets[0]
            .changed_files
            .contains(&"src/cancel.txt".to_string()),
        "{:?}",
        sets[0].changed_files
    );
    // 新建文件基线 = 执行前不存在（恢复即删除）：sha256 为 None 且 content_available。
    let base_hash = sets[0]
        .base_hashes
        .iter()
        .find(|h| h.path == "src/cancel.txt")
        .expect("基底含该文件");
    assert!(
        base_hash.sha256.is_none() && base_hash.content_available,
        "新建文件基线应为「执行前不存在」：{base_hash:?}"
    );
    // 同时确认取消路径的窗口隔离：已有脏文件 a.rs 本轮未触碰 → 不进入变更窗口
    // （窗口 = 执行前后差集，用户已有修改与取消残留区分开）。
    assert!(
        !sets[0].changed_files.iter().any(|p| p == "src/a.rs"),
        "窗口外文件不得进入变更集：{:?}",
        sets[0].changed_files
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 3（非 Git 目录）：内容哈希检测增/改/删——不依赖 git 也能识别
/// 新建、修改、删除三类变更并生成 ChangeSet（diff 走退化摘要）。
#[tokio::test]
async fn non_git_dir_content_hash_detects_new_modify_delete() {
    let dir = unique_dir("nongit");
    let root = dir.join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/mod.txt"), b"v0\n").unwrap();
    std::fs::write(root.join("src/del.txt"), b"x\n").unwrap();
    let tracking = tracking_for(&root, &dir);
    let worker = TrackedRoleWorker {
        inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
        lease: None,
        lease_waits: None,
        tracking: Some(tracking),
    };
    let input = fileops_input(
        &root,
        json!([
            { "action": "write", "path": "src/new.txt", "content": "new\n" },
            { "action": "write", "path": "src/mod.txt", "content": "v1\n" },
            { "action": "delete", "path": "src/del.txt" },
        ]),
        "s-impl",
    );
    let output = worker.run(&input).await;
    assert!(output.is_ok(), "{output:?}");
    let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
        .await
        .unwrap();
    let record = records.last().expect("应有变更记录");
    for expected in ["src/new.txt", "src/mod.txt", "src/del.txt"] {
        assert!(
            record.changed_files.contains(&expected.to_string()),
            "非 git 内容哈希应检出 {expected}（实际 {:#?}）",
            record.changed_files
        );
    }
    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    let sets = store.list_for_team("t1").unwrap();
    assert_eq!(sets.len(), 1);
    for expected in ["src/new.txt", "src/mod.txt", "src/del.txt"] {
        assert!(
            sets[0].changed_files.contains(&expected.to_string()),
            "{:?}",
            sets[0].changed_files
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 场景 4（连续两个写 Worker 共享同一租约）：租约覆盖「前快照→执行→后快照→
/// 变更登记」全程——后一个写步骤的前基线不得混入前一个步骤的变更（各自 ChangeSet
/// 只含自己的文件），且两个步骤的收尾都完整落盘。
#[tokio::test]
async fn two_write_workers_sharing_lease_keep_change_sets_isolated() {
    let dir = unique_dir("wlease");
    let (root, _head, _pre) = git_repo_with_pre_dirty_file("wlease");
    let manager = WriteLeaseManager::new();
    let mk = |role: &str, dir: &Path| -> TrackedRoleWorker {
        TrackedRoleWorker {
            inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
            lease: Some(WriteLease::new(Arc::clone(&manager), WriteScope::global())),
            lease_waits: None,
            tracking: Some(workspace_change_tracker::Tracker {
                root: root.clone(),
                run_dir: dir.join("run"),
                team_id: "t1".to_string(),
                role: role.to_string(),
                allowed: Vec::new(),
                cas: owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap(),
                audit: None,
            }),
        }
    };
    // 并发执行：若租约未覆盖收尾，第二个的前快照会误捕第一个的写入。
    let w1 = mk("w1", &dir);
    let w2 = mk("w2", &dir);
    let input1 = fileops_input(
        &root,
        json!([
            { "action": "write", "path": "src/one.txt", "content": "one\n" },
            { "action": "sleep", "ms": 60 },
        ]),
        "s-one",
    );
    let input2 = fileops_input(
        &root,
        json!([
            { "action": "write", "path": "src/two.txt", "content": "two\n" },
            { "action": "sleep", "ms": 60 },
        ]),
        "s-two",
    );
    let (r1, r2) = tokio::join!(w1.run(&input1), w2.run(&input2));
    assert!(r1.is_ok(), "{r1:?}");
    assert!(r2.is_ok(), "{r2:?}");

    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    let sets = store.list_for_team("t1").unwrap();
    assert_eq!(
        sets.len(),
        2,
        "两个写步骤各应生成自己的 ChangeSet：{sets:#?}"
    );
    let one = sets.iter().find(|s| s.step_id == "s-one").expect("s-one");
    let two = sets.iter().find(|s| s.step_id == "s-two").expect("s-two");
    assert_eq!(
        one.changed_files,
        vec!["src/one.txt".to_string()],
        "{:?}",
        one.changed_files
    );
    assert_eq!(
        two.changed_files,
        vec!["src/two.txt".to_string()],
        "{:?}",
        two.changed_files
    );
    // 变更记录同样两笔、各自窗口独立。
    let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
        .await
        .unwrap();
    assert_eq!(records.len(), 2, "两笔收尾记录：{records:#?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// -----------------------------------------------------------------------
// 十一期 · 二路：范围写租约——声明互不重叠写范围的写者真并发，归属过滤
// 不把并发写者的变更算到本步骤头上。
// -----------------------------------------------------------------------

/// 并发证明 worker：双方都到 barrier 才继续写自己的文件；串行执行会等到超时失败。
struct BarrierWriter {
    barrier: Arc<tokio::sync::Barrier>,
    target: PathBuf,
    content: &'static str,
}

#[async_trait]
impl Worker for BarrierWriter {
    fn name(&self) -> &str {
        "agent"
    }
    async fn run(&self, _input: &Value) -> Result<String, String> {
        if tokio::time::timeout(std::time::Duration::from_secs(5), self.barrier.wait())
            .await
            .is_err()
        {
            return Err("串行执行：并发 barrier 超时（范围租约未放行并发写）".to_string());
        }
        if let Some(parent) = self.target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.target, self.content).map_err(|e| e.to_string())?;
        Ok("ok".to_string())
    }
}

/// 声明互不重叠写范围的两个写 Worker 真并发——barrier 证明同时进入执行；
/// 各自 ChangeSet 只含自己范围内的文件；并发写者范围内的新增文件不误判越界。
#[tokio::test]
async fn disjoint_scoped_writers_run_concurrently_and_keep_change_sets_isolated() {
    let dir = unique_dir("parallel");
    let (root, _head, _pre) = git_repo_with_pre_dirty_file("parallel");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let manager = WriteLeaseManager::new();
    let mk = |role: &str, allowed: Vec<PathBuf>, target: PathBuf, content: &'static str| {
        let mut tracking = tracking_for(&root, &dir);
        tracking.role = role.to_string();
        tracking.allowed = allowed.clone();
        TrackedRoleWorker {
            inner: Arc::new(BarrierWriter {
                barrier: Arc::clone(&barrier),
                target,
                content,
            }) as Arc<dyn Worker>,
            lease: Some(WriteLease::new(
                Arc::clone(&manager),
                WriteScope::from_paths(&allowed),
            )),
            lease_waits: None,
            tracking: Some(tracking),
        }
    };
    let w1 = mk(
        "w1",
        vec![root.join("src/a")],
        root.join("src/a/one.txt"),
        "one\n",
    );
    let w2 = mk(
        "w2",
        vec![root.join("src/b")],
        root.join("src/b/two.txt"),
        "two\n",
    );
    let input1 = step_input("s-one");
    let input2 = step_input("s-two");
    let (r1, r2) = tokio::join!(w1.run(&input1), w2.run(&input2));
    assert!(r1.is_ok(), "{r1:?}");
    assert!(r2.is_ok(), "{r2:?}");

    let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
        .await
        .unwrap();
    assert_eq!(records.len(), 2, "{records:#?}");
    assert!(
        records.iter().all(|record| record.violation.is_none()),
        "并发范围内路径不得误判越界：{records:#?}"
    );
    let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
    let sets = store.list_for_team("t1").unwrap();
    assert_eq!(sets.len(), 2, "{sets:#?}");
    let one = sets.iter().find(|s| s.step_id == "s-one").expect("s-one");
    let two = sets.iter().find(|s| s.step_id == "s-two").expect("s-two");
    assert_eq!(
        one.changed_files,
        vec!["src/a/one.txt".to_string()],
        "w1 只登记自己范围内的变更：{:?}",
        one.changed_files
    );
    assert_eq!(
        two.changed_files,
        vec!["src/b/two.txt".to_string()],
        "w2 只登记自己范围内的变更：{:?}",
        two.changed_files
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 声明范围但与团队绑定无交集时：写白名单哨兵生效（直接写文件被拒为越界）。
#[tokio::test]
async fn scope_without_binding_intersection_denies_writes() {
    let dir = unique_dir("scope-deny");
    let (root, _head, _pre) = git_repo_with_pre_dirty_file("scope-deny");
    let mut tracking = tracking_for(&root, &dir);
    tracking.allowed = vec![root.join(".owo-no-write-scope")];
    let worker = TrackedRoleWorker {
        inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
        // 未持租约（纯归属过滤/白名单路径）也能验证哨兵拒绝。
        lease: None,
        lease_waits: None,
        tracking: Some(tracking),
    };
    let output = worker
        .run(&fileops_input(
            &root,
            json!([{ "action": "write", "path": "src/x.txt", "content": "x\n" }]),
            "s-deny",
        ))
        .await;
    assert!(output.is_err(), "{output:?}");
    assert!(
        output.unwrap_err().contains("scope_violation"),
        "越界必须报 scope_violation"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn delivery_gate_lease_excludes_team_writers_until_validation_commits() {
    let root = unique_dir("delivery-gate-lease");
    let held = acquire_workspace_delivery_lease(&root).await;
    let writer = WriteLease::new(
        manager_for_workspace(&root),
        WriteScope::from_paths(&[root.join("src/main.rs")]),
    );
    let mut waiting = tokio::spawn(async move { writer.acquire().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiting)
            .await
            .is_err(),
        "DeliveryGate lease must serialize against all Team writer scopes"
    );
    drop(held);
    let acquired = tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiting)
        .await
        .expect("writer should proceed after DeliveryGate releases the lease")
        .expect("writer task should not panic");
    drop(acquired);
    let _ = std::fs::remove_dir_all(root);
}
