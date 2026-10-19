use super::*;
use std::collections::HashMap;
use std::path::Path;

#[test]
fn parse_porcelain_paths() {
    assert_eq!(
        parse_porcelain_path(" M src/lib.rs").as_deref(),
        Some("src/lib.rs")
    );
    assert_eq!(
        parse_porcelain_path("?? new.txt").as_deref(),
        Some("new.txt")
    );
    assert_eq!(
        parse_porcelain_path("R  old.txt -> new.txt").as_deref(),
        Some("new.txt")
    );
    assert_eq!(
        parse_porcelain_path("A  \"quoted name.txt\"").as_deref(),
        Some("quoted name.txt")
    );
    assert_eq!(parse_porcelain_path("XY"), None);
    // 四路集成微修：clippy bool_assert_comparison（assert_eq!(.., true) → assert!(..)）。
    assert!(parse_porcelain_path("").is_none());
}

#[test]
fn changed_files_is_window_delta() {
    let before = GitSnapshot {
        git: true,
        status: vec![" M src/a.rs".to_string()],
        diff_stat: String::new(),
        at: 1,
    };
    let post = GitSnapshot {
        git: true,
        status: vec![
            " M src/a.rs".to_string(),
            "?? src/b.rs".to_string(),
            "A  docs/c.md".to_string(),
            "?? out/report.md".to_string(),
        ],
        diff_stat: "2 files changed".to_string(),
        at: 2,
    };
    let changed = post.changed_files(&before);
    assert!(changed.contains(&"src/b.rs".to_string()));
    assert!(changed.contains(&"docs/c.md".to_string()));
    assert!(changed.contains(&"out/report.md".to_string()));
    assert!(
        !changed.contains(&"src/a.rs".to_string()),
        "前快照已有的变更不追溯"
    );
    // 非 git 快照检测不到变更。
    let non_git = GitSnapshot {
        git: false,
        status: vec![" M src/x.rs".to_string()],
        diff_stat: String::new(),
        at: 3,
    };
    assert!(non_git.changed_files(&before).is_empty());
}

#[test]
fn whitelist_check_flags_outside_changes() {
    let root = std::env::temp_dir();
    let allowed = vec![root.join("owo-tracker-test-allowed")];
    // 空变更 / 空白名单 = 放行。
    assert!(check_whitelist(&[], &root, &allowed).is_ok());
    assert!(check_whitelist(&["x.txt".to_string()], &root, &[]).is_ok());
    // 越界 → scope_violation 前缀（失败码口径冻结）。
    let error = check_whitelist(&["outside/secret.txt".to_string()], &root, &allowed).unwrap_err();
    assert!(
        error.starts_with("scope_violation:"),
        "失败码前缀冻结：{error}"
    );
    assert!(error.contains("outside/secret.txt"));
}

#[test]
fn whitelist_check_allows_inside_change_with_verbatim_root() {
    // 回归（七期二路冒烟发现）：绑定 root/allowed 存储为 simplify 后路径，
    // 而 canonicalize 产物带 `\\?\` verbatim 前缀——两侧混用时 `starts_with`
    // 恒 false，白名单内变更被误判越界。白名单内变更必须放行。
    let temp = std::env::temp_dir();
    let inside_dir = temp.join("owo-tracker-test-allowed-inside");
    std::fs::create_dir_all(&inside_dir).unwrap();
    let verbatim_root = temp.canonicalize().unwrap(); // Windows 下带 `\\?\` 前缀
    assert!(check_whitelist(
        &["owo-tracker-test-allowed-inside/ok.txt".to_string()],
        &verbatim_root,
        std::slice::from_ref(&inside_dir),
    )
    .is_ok());
    // 同一口径下越界仍须拦截。
    let error = check_whitelist(
        &["owo-tracker-test-allowed-inside-escape/evil.txt".to_string()],
        &verbatim_root,
        std::slice::from_ref(&inside_dir),
    )
    .unwrap_err();
    assert!(error.starts_with("scope_violation:"));
    let _ = std::fs::remove_dir_all(&inside_dir);
}

#[test]
fn sanitize_step_keeps_safe_filename_fragment() {
    assert_eq!(sanitize_step("s12"), "s12");
    assert_eq!(sanitize_step("step/1 x"), "step_1_x");
}

// ------------------------------------------------------------------
// 九期（一路）：合并变更检测
// ------------------------------------------------------------------

fn snapshot(status: &[&str], at: u64) -> GitSnapshot {
    GitSnapshot {
        git: true,
        status: status.iter().map(|l| l.to_string()).collect(),
        diff_stat: String::new(),
        at,
    }
}

fn hash_map(entries: &[(&str, &str)]) -> HashMap<String, Option<String>> {
    entries
        .iter()
        .map(|(p, h)| (p.to_string(), Some(h.to_string())))
        .collect()
}

/// 核心修复点：执行前已脏（M）、执行后仍脏（M）但内容变化 → 必须进 changed_files。
/// 内容未变 → 不得进入（不把用户的既有脏文件误记到 Agent 头上）。
#[test]
fn merge_catches_pre_dirty_file_modified_again() {
    let dir = std::env::temp_dir().join(format!(
        "owo-merge-test-{}-{}",
        std::process::id(),
        unique_tag_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // a.rs：执行前内容 "user edit v1"（基线哈希），Agent 改为 "agent content"。
    std::fs::write(src.join("a.rs"), b"agent content").unwrap();
    // b.rs：执行前内容与执行后一致（既有脏文件未被本次触碰）。
    std::fs::write(src.join("b.rs"), b"user dirty").unwrap();
    let hash = |bytes: &[u8]| owo_agent_core::cas_store::CasStore::hash_of(bytes);
    let pre = snapshot(&[" M src/a.rs", " M src/b.rs"], 1);
    let post = snapshot(&[" M src/a.rs", " M src/b.rs"], 2);
    let hashes = hash_map(&[
        ("src/a.rs", hash(b"user edit v1").as_str()),
        ("src/b.rs", hash(b"user dirty").as_str()),
    ]);
    // a.rs 执行前哈希 ≠ 执行后内容哈希 → 计入；b.rs 相同 → 不计入。
    let changed = merge_changed_files(&pre, &post, &hashes, &dir);
    assert!(
        changed.contains(&"src/a.rs".to_string()),
        "执行前已脏、Agent 再次修改的文件必须出现：{changed:?}"
    );
    assert!(
        !changed.contains(&"src/b.rs".to_string()),
        "内容未变的既有脏文件不得误报：{changed:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merge_catches_pre_dirty_file_deleted_and_reports_unchanged_as_absent() {
    // 执行前已脏的文件被 Agent 删除：porcelain 从 pre 有 → post 无，路径差集
    // 漏检（路径本来就在 before 集合里），内容哈希差集必须捕获。
    let dir = std::env::temp_dir().join(format!(
        "owo-merge-del-{}-{}",
        std::process::id(),
        unique_tag_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("kept.rs"), b"kept content").unwrap();
    // gone.rs 已被 Agent 删除（磁盘上不存在）。
    let hash = owo_agent_core::cas_store::CasStore::hash_of(b"kept content");
    let pre = snapshot(&[" M src/gone.rs", " M src/kept.rs"], 1);
    let post = snapshot(&[" M src/kept.rs"], 2);
    let hashes = hash_map(&[
        ("src/gone.rs", "pre-hash-of-gone"),
        ("src/kept.rs", hash.as_str()),
    ]);
    let changed = merge_changed_files(&pre, &post, &hashes, &dir);
    assert!(changed.contains(&"src/gone.rs".to_string()), "{changed:?}");
    assert!(!changed.contains(&"src/kept.rs".to_string()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn merge_includes_rename_old_side_for_recovery() {
    // 窗口内发生暂存重命名：new 侧走窗口差集；old 侧必须并入（恢复才能还原源文件）。
    let pre = snapshot(&["?? src/new-name.rs"], 1);
    let post = snapshot(&["R  src/old-name.rs -> src/new-name.rs"], 2);
    let hashes = HashMap::new();
    let changed = merge_changed_files(&pre, &post, &hashes, Path::new("."));
    assert!(changed.contains(&"src/new-name.rs".to_string()));
    assert!(
        changed.contains(&"src/old-name.rs".to_string()),
        "重命名 old 侧必须进入 changed_files：{changed:?}"
    );
    // pre 里已存在的同一重命名（执行前就发生）→ 不重复计入。
    let pre2 = snapshot(&["R  src/old-name.rs -> src/new-name.rs"], 1);
    let changed2 = merge_changed_files(&pre2, &post, &hashes, Path::new("."));
    assert!(
        !changed2.contains(&"src/old-name.rs".to_string()),
        "{changed2:?}"
    );
}

#[test]
fn merge_is_conservative_without_pre_hash() {
    // 执行前哈希未知（未登记）且文件仍存在 → 只可证明的变更是删除，修改不误报。
    let dir = std::env::temp_dir().join(format!(
        "owo-merge-cons-{}-{}",
        std::process::id(),
        unique_tag_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("x.rs"), b"whatever").unwrap();
    let pre = snapshot(&[" M x.rs"], 1);
    let post = snapshot(&[" M x.rs"], 2);
    let changed = merge_changed_files(&pre, &post, &HashMap::new(), &dir);
    assert!(
        !changed.contains(&"x.rs".to_string()),
        "无基线哈希时不得把文件记为修改：{changed:?}"
    );
    // 删除仍可证明。
    std::fs::remove_file(dir.join("x.rs")).unwrap();
    let changed2 = merge_changed_files(&pre, &post, &HashMap::new(), &dir);
    assert!(changed2.contains(&"x.rs".to_string()), "{changed2:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rename_parse_extracts_both_sides() {
    assert_eq!(
        parse_porcelain_rename("R  old.txt -> new.txt"),
        Some(("old.txt".to_string(), "new.txt".to_string()))
    );
    assert_eq!(parse_porcelain_rename(" M src/a.rs"), None);
    assert_eq!(parse_porcelain_rename("?? x"), None);
}

#[tokio::test]
async fn record_skips_diff_and_uses_degraded_summary() {
    // changed 为空 → 无 diff_ref、不落差异文件；changed 非空 + git 不可用
    //（临时目录不是 git 仓库）→ 退化摘要落盘且引用非空。
    let dir = std::env::temp_dir().join(format!(
        "owo-record-test-{}-{}",
        std::process::id(),
        unique_tag_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let cas = owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap();
    let tracker = Tracker {
        root: root.clone(),
        run_dir: run_dir.clone(),
        team_id: "t1".to_string(),
        role: "implementer".to_string(),
        allowed: Vec::new(),
        cas,
        audit: None,
    };
    let post = GitSnapshot {
        git: false,
        status: Vec::new(),
        diff_stat: String::new(),
        at: 42,
    };
    // 空变更：无 diff_ref。
    let record = tracker.record("s1", &post, &[], None, None).await.unwrap();
    assert!(record.diff_ref.is_none());
    // 非空变更 + git 不可用：退化摘要。
    std::fs::write(root.join("out.md"), b"# report\n").unwrap();
    let base = owo_agent_core::change_set::WorkspaceBaseSnapshot {
        complete: true,
        ..Default::default()
    };
    let record = tracker
        .record("s2", &post, &["out.md".to_string()], None, Some(&base))
        .await
        .unwrap();
    let diff_ref = record.diff_ref.expect("有真实修改时 diff_ref 必须非空");
    assert!(diff_ref.ends_with(".diff.txt"), "{diff_ref}");
    let body = std::fs::read_to_string(run_dir.join(&diff_ref)).unwrap();
    assert!(body.contains("out.md"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// R1 硬性上限：变更文件超过 [`DIFF_PATH_ARG_CAP`]（100）时**绝不退回全仓
/// `git diff`**——`git_diff_patch` 返回 None，`record` 落退化差异摘要
/// （`.diff.txt`，逐文件 CAS 基线），杜绝"超限即拉全仓 diff"的失控路径。
#[tokio::test]
async fn over_cap_files_use_degraded_summary_never_full_repo_diff() {
    let dir = std::env::temp_dir().join(format!(
        "owo-record-cap-{}-{}",
        std::process::id(),
        unique_tag_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let run_dir = dir.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let cas = owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap();
    let tracker = Tracker {
        root: root.clone(),
        run_dir: run_dir.clone(),
        team_id: "t1".to_string(),
        role: "implementer".to_string(),
        allowed: Vec::new(),
        cas,
        audit: None,
    };
    // 伪 git 快照（git=true）：真实仓库场景里超限也必须走退化摘要。
    let post = GitSnapshot {
        git: true,
        status: Vec::new(),
        diff_stat: String::new(),
        at: 7,
    };
    let changed: Vec<String> = (0..DIFF_PATH_ARG_CAP + 1)
        .map(|i| format!("many/f{i}.txt"))
        .collect();
    let record = tracker
        .record("s-over", &post, &changed, None, None)
        .await
        .unwrap();
    let diff_ref = record.diff_ref.expect("有变更必须落差异引用");
    assert!(
        diff_ref.ends_with(".diff.txt"),
        "超限必须走退化差异摘要（.diff.txt），不得生成 git 补丁：{diff_ref}"
    );
    // 摘要内容面：逐文件列出且不出现全仓 diff 特征。
    let body = std::fs::read_to_string(run_dir.join(&diff_ref)).unwrap();
    assert!(body.contains("many/f0.txt"), "{body}");
    assert!(!body.contains("diff --git a/"), "不得有 git 补丁头：{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// 测试辅助：毫秒时间戳（唯一临时目录用）。
fn unique_tag_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
