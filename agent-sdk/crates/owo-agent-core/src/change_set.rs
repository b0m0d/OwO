//! ChangeSet（八期 · 二路）：把「Agent 已修改工作区」升级为可审查/可接受/可撤销的
//! 代码变更闭环。
//!
//! - 执行前：[`snapshot_allowed_paths`] 对允许路径（未绑定时 = 工作区根）做内容
//!   基线快照，文件内容进 CAS（按 SHA-256 寻址，天然去重）；`.git`/`node_modules`/
//!   `target` 等目录排除，单文件超过 [`MAX_SNAPSHOT_FILE_BYTES`] 只「扫到」不存内容
//!   （该文件被改后恢复走冲突人工路径）；符号链接跳过，条目总数受
//!   [`MAX_SNAPSHOT_ENTRIES`] 约束；
//! - 执行后：[`build_change_set`] 以「本次执行窗口内实际变更的文件」为界生成
//!   ChangeSet（base_hashes = 执行前、result_hashes = 执行后）。基线三态：
//!   `(Some(hash), true)` = 内容在 CAS 可恢复；`(None, true)` = 执行前不存在
//!   （新建文件，恢复即删除）；`(_, false)` = 基线不可用/未知（恢复按冲突处理，
//!   绝不误删）；
//! - 撤销：[`restore_change_set`] 只恢复该 ChangeSet 修改的文件——先整体分类
//!   （零写入）：当前哈希 == 结果哈希 → 需恢复；== 基线哈希 → 已恢复跳过；其余 =
//!   用户改过 → conflict。任一冲突即整体不落任何写（调用方转 409 + `conflicted`，
//!   不覆盖用户新内容）。

use owo_agent_protocol::{ChangeSet, ChangeSetFileHash, ChangeSetStatus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::cas_store::CasStore;

/// 单文件内容快照上限：超过只「扫到」不存内容（被改后恢复按冲突处理）。
pub const MAX_SNAPSHOT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// 基线快照排除目录（版本库/依赖树/构建产物：写工具进不去，快照无恢复价值）。
pub const SNAPSHOT_EXCLUDED_DIRS: [&str; 5] =
    [".git", "node_modules", "target", "dist", ".owo-agent"];

/// 基线快照条目总数上限（防超大仓库拖慢写角色启动）。
pub const MAX_SNAPSHOT_ENTRIES: usize = 5000;

/// 执行前基线快照。
#[derive(Debug, Clone, Default)]
pub struct WorkspaceBaseSnapshot {
    /// 相对路径 → 执行前内容哈希（内容已进 CAS，≤ [`MAX_SNAPSHOT_FILE_BYTES`]）。
    pub entries: HashMap<String, String>,
    /// 快照扫到的全部相对路径（含超限/读取失败的文件——区分「新建」与「未知」）。
    pub scanned: HashSet<String>,
    /// 快照是否完整（条目预算未耗尽）。不完整时未扫到的文件按「基线未知」处理。
    pub complete: bool,
}

/// 恢复报告（分类结果 + 实际执行的动作）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RestoreReport {
    /// 已写回/删除恢复的文件。
    pub restored: Vec<String>,
    /// 当前已等于基线、无需操作的文件。
    pub already_restored: Vec<String>,
    /// 用户改过（当前哈希既不等于结果也不等于基线）或基线不可用的文件——
    /// 非空时整体未做任何写入（调用方转 409 + conflicted）。
    pub conflicts: Vec<String>,
}

/// 相对路径归一（`\` → `/`；与 git porcelain / 变更追踪口径一致）。
fn relativize(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// 单文件当前内容哈希（不存在 → None）。
pub fn file_hash(root: &Path, relative: &str) -> Option<String> {
    let bytes = std::fs::read(root.join(relative)).ok()?;
    Some(CasStore::hash_of(&bytes))
}

/// 递归收集目录下文件（跳过排除目录与符号链接；预算耗尽即停）。
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if SNAPSHOT_EXCLUDED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk_files(&path, out, budget);
        } else if file_type.is_file() {
            out.push(path);
            *budget -= 1;
        }
    }
}

/// 执行前基线快照：允许路径（空 = 工作区根）内文件内容进 CAS。
///
/// - 可读且 ≤ 上限 → 内容进 CAS，入 `entries`；
/// - 超限/读取失败 → 只入 `scanned`（基线未知，恢复按冲突处理）；
/// - 预算耗尽 → `complete = false`（未扫到的文件按基线未知处理，绝不误删）。
pub async fn snapshot_allowed_paths(
    root: &Path,
    allowed: &[PathBuf],
    cas: &CasStore,
) -> WorkspaceBaseSnapshot {
    let mut snapshot = WorkspaceBaseSnapshot {
        complete: true,
        ..Default::default()
    };
    let mut budget = MAX_SNAPSHOT_ENTRIES;
    let scan_roots: Vec<PathBuf> = if allowed.is_empty() {
        vec![root.to_path_buf()]
    } else {
        allowed.to_vec()
    };
    for scan_root in &scan_roots {
        if !scan_root.exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_files(scan_root, &mut files, &mut budget);
        for path in files {
            let Some(relative) = relativize(root, &path) else {
                continue;
            };
            snapshot.scanned.insert(relative.clone());
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > MAX_SNAPSHOT_FILE_BYTES || snapshot.entries.contains_key(&relative)
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(hash) = cas.put(&bytes) else {
                continue;
            };
            snapshot.entries.insert(relative, hash);
        }
    }
    snapshot.complete = budget > 0;
    snapshot
}

/// 步骤 ID → 安全文件名片段（ChangeSet id 组成部分）。
fn sanitize_step(step: &str) -> String {
    step.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 退化差异摘要（九期 · 一路）
// ---------------------------------------------------------------------------

/// 行级差异每侧行数上限（退化摘要不是全量补丁，超长截断）。
const DIFF_LINE_CAP: usize = 40;
/// 单行宽度上限（字符）。
const DIFF_LINE_WIDTH: usize = 200;

fn clip_line(line: &str) -> String {
    if line.chars().count() <= DIFF_LINE_WIDTH {
        line.to_string()
    } else {
        let mut clipped: String = line.chars().take(DIFF_LINE_WIDTH).collect();
        clipped.push('…');
        clipped
    }
}

/// 退化差异摘要：Git diff 不可用/为空时，用**执行前 CAS 基线内容 × 执行后磁盘内容**
/// 生成逐文件差异说明（新建/修改/删除 + 行级增删摘录），作为 diff_ref 落盘内容。
///
/// 不是标准 unified patch（无 hunk 头/@ 行号）——是「退化摘要」：保证有真实修改时
/// diff_ref 非空且可读；行级摘录按「基线有而结果无 = 删除行、结果有而基线无 = 新增行」
/// 的集合差口径（不保序、与真正 diff 相比可能多列上下文重复行）。
pub fn degraded_diff_summary(
    root: &Path,
    base: &WorkspaceBaseSnapshot,
    changed: &[String],
    cas: &CasStore,
) -> String {
    let mut out =
        String::from("# 变更摘要（Git diff 不可用——执行前 CAS 基线 × 执行后内容 退化口径）\n");
    for relative in changed {
        let result = file_hash(root, relative);
        let base_entry = base.entries.get(relative);
        let pre_existing = base_entry.is_some() || base.scanned.contains(relative);
        let status = match (base_entry, &result) {
            (Some(_), None) | (None, None) if pre_existing => "删除",
            (None, Some(_)) if !pre_existing => "新建",
            (Some(base_hash), Some(result_hash)) if base_hash == result_hash => "内容未变",
            _ => "修改",
        };
        out.push_str(&format!("\n## {relative}（{status}）\n"));
        match base_entry {
            Some(hash) => out.push_str(&format!("- 基线: cas://{hash}（可恢复）\n")),
            None if pre_existing => out.push_str("- 基线: 不可用（恢复按冲突处理）\n"),
            None => out.push_str("- 基线: （执行前不存在）\n"),
        }
        match &result {
            Some(hash) => out.push_str(&format!("- 结果: sha256:{hash}\n")),
            None => out.push_str("- 结果: （文件已删除）\n"),
        }
        // 行级摘录（文本可读时）。
        let baseline_text = base_entry.and_then(|hash| cas.get_text(hash));
        let result_text = std::fs::read(root.join(relative))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        match (baseline_text, result_text) {
            (Some(base_body), Some(result_body)) => {
                let base_lines: Vec<&str> = base_body.lines().collect();
                let result_lines: Vec<&str> = result_body.lines().collect();
                let result_set: std::collections::HashSet<&str> =
                    result_lines.iter().copied().collect();
                let base_set: std::collections::HashSet<&str> =
                    base_lines.iter().copied().collect();
                let removed: Vec<&str> = base_lines
                    .iter()
                    .copied()
                    .filter(|line| !result_set.contains(line))
                    .take(DIFF_LINE_CAP)
                    .collect();
                let added: Vec<&str> = result_lines
                    .iter()
                    .copied()
                    .filter(|line| !base_set.contains(line))
                    .take(DIFF_LINE_CAP)
                    .collect();
                if removed.is_empty() && added.is_empty() {
                    continue;
                }
                out.push_str(&format!(
                    "- 行级摘录（退化口径，各截 {} 行）\n",
                    DIFF_LINE_CAP
                ));
                for line in removed {
                    out.push_str(&format!("- {}\n", clip_line(line)));
                }
                for line in added {
                    out.push_str(&format!("+ {}\n", clip_line(line)));
                }
            }
            (None, Some(result_body)) if status == "新建" => {
                out.push_str("- 新文件内容摘录\n");
                for line in result_body.lines().take(DIFF_LINE_CAP) {
                    out.push_str(&format!("+ {}\n", clip_line(line)));
                }
            }
            (Some(base_body), None) if status == "删除" => {
                out.push_str("- 被删除内容摘录\n");
                for line in base_body.lines().take(DIFF_LINE_CAP) {
                    out.push_str(&format!("- {}\n", clip_line(line)));
                }
            }
            _ => {}
        }
    }
    out
}

/// 以「本次执行窗口内实际变更的文件」为界生成 ChangeSet。
///
/// 基线三态（见模块文档）：内容在 CAS / 执行前不存在（新建）/ 基线未知。
/// 结果哈希按当前磁盘内容计算（结果内容不进 CAS——accept 保留磁盘现状）。
pub fn build_change_set(
    team_id: &str,
    step_id: &str,
    role: &str,
    base: &WorkspaceBaseSnapshot,
    changed: &[String],
    root: &Path,
    diff_ref: Option<String>,
) -> ChangeSet {
    let base_hashes: Vec<ChangeSetFileHash> = changed
        .iter()
        .map(|relative| {
            let (sha256, content_available) = if let Some(hash) = base.entries.get(relative) {
                (Some(hash.clone()), true)
            } else if base.scanned.contains(relative) {
                // 扫到但基线不可用（超限/读取失败）→ 恢复按冲突处理，绝不误删。
                (None, false)
            } else if base.complete {
                // 完整快照里没有 = 执行前不存在（新建文件，恢复即删除）。
                (None, true)
            } else {
                // 快照不完整：未扫到的文件按基线未知处理（保守）。
                (None, false)
            };
            ChangeSetFileHash {
                path: relative.clone(),
                sha256,
                content_available,
            }
        })
        .collect();
    let result_hashes: Vec<ChangeSetFileHash> = changed
        .iter()
        .map(|relative| ChangeSetFileHash {
            path: relative.clone(),
            sha256: file_hash(root, relative),
            content_available: false,
        })
        .collect();
    ChangeSet {
        change_set_id: format!(
            "cs-{}-{}-{}",
            team_id,
            sanitize_step(step_id),
            chrono::Utc::now().timestamp_millis().max(0) as u64
        ),
        team_id: team_id.to_string(),
        step_id: step_id.to_string(),
        role: role.to_string(),
        base_hashes,
        result_hashes,
        changed_files: changed.to_vec(),
        diff_ref,
        status: ChangeSetStatus::PendingReview,
        created_at: chrono::Utc::now().to_rfc3339(),
        decision: None,
        conflicts: Vec::new(),
    }
}

/// 恢复 ChangeSet 修改的文件（reject/revert 共用核心）。
///
/// 两阶段：先全量分类（零写入）——任一文件冲突（用户改过 / 基线不可用）即返回
/// `conflicts` 非空且**不落任何写**；全部可恢复才执行写回/删除。
pub async fn restore_change_set(
    root: &Path,
    change_set: &ChangeSet,
    cas: &CasStore,
) -> RestoreReport {
    let base_by_path: HashMap<&str, &ChangeSetFileHash> = change_set
        .base_hashes
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let result_by_path: HashMap<&str, &ChangeSetFileHash> = change_set
        .result_hashes
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    // 阶段一：分类（零写入）。
    enum Action {
        WriteBack(Vec<u8>),
        Delete,
    }
    let mut plan: Vec<(String, Action)> = Vec::new();
    let mut report = RestoreReport::default();
    for relative in &change_set.changed_files {
        let current = file_hash(root, relative);
        let result = result_by_path
            .get(relative.as_str())
            .and_then(|entry| entry.sha256.clone());
        let base = base_by_path.get(relative.as_str()).copied();
        if current == result {
            // 当前 == 结果 → 需要恢复（含 Worker 删除了文件：两侧均 None）；按基线
            // 三态决定动作。
            let Some(base_entry) = base else {
                report.conflicts.push(relative.clone());
                continue;
            };
            match (&base_entry.sha256, base_entry.content_available) {
                (Some(hash), true) => match cas.get(hash) {
                    Some(bytes) => plan.push((relative.clone(), Action::WriteBack(bytes))),
                    None => report.conflicts.push(relative.clone()),
                },
                (None, true) => plan.push((relative.clone(), Action::Delete)),
                _ => report.conflicts.push(relative.clone()),
            }
        } else if current == base.and_then(|entry| entry.sha256.clone()) {
            // 当前已等于基线（含「新建文件已被用户删除」：两侧均 None）→ 无需操作。
            report.already_restored.push(relative.clone());
        } else {
            // 用户改过（或删除了工作产物）→ 冲突，不覆盖。
            report.conflicts.push(relative.clone());
        }
    }
    if !report.conflicts.is_empty() {
        return report;
    }

    // 阶段二：执行恢复。
    for (relative, action) in plan {
        match action {
            Action::WriteBack(bytes) => {
                let target = root.join(&relative);
                if let Some(parent) = target.parent() {
                    if let Err(error) = std::fs::create_dir_all(parent) {
                        tracing::error!(%error, path = %target.display(), "ChangeSet 恢复建目录失败");
                        report.conflicts.push(relative.clone());
                        continue;
                    }
                }
                if let Err(error) = std::fs::write(&target, &bytes) {
                    tracing::error!(%error, path = %target.display(), "ChangeSet 恢复写回失败");
                    report.conflicts.push(relative.clone());
                    continue;
                }
                if let Some(base_hash) = base_by_path
                    .get(relative.as_str())
                    .and_then(|entry| entry.sha256.clone())
                {
                    let verified = file_hash(root, &relative);
                    if verified.as_deref() != Some(base_hash.as_str()) {
                        tracing::warn!(
                            path = %relative,
                            "ChangeSet 恢复后哈希与基线不一致（保留已写回内容）"
                        );
                    }
                }
                report.restored.push(relative);
            }
            Action::Delete => {
                let target = root.join(&relative);
                if target.exists() {
                    if let Err(error) = std::fs::remove_file(&target) {
                        tracing::error!(%error, path = %target.display(), "ChangeSet 恢复删除失败");
                        report.conflicts.push(relative.clone());
                        continue;
                    }
                }
                report.restored.push(relative);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 唯一临时目录（core 无 tempfile dev-dep；测试自清理）。
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "owo-change-set-test-{}-{}-{}",
            tag,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_cas(dir: &Path) -> CasStore {
        CasStore::new(dir.join("cas")).unwrap()
    }

    #[tokio::test]
    async fn snapshot_build_restore_roundtrip_restores_modified_file() {
        let dir = unique_temp_dir("roundtrip");
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let cas = test_cas(&dir);
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let base = snapshot_allowed_paths(&root, &[], &cas).await;
        assert_eq!(base.entries["a.txt"], CasStore::hash_of(b"v1"));

        // 模拟 Worker 修改。
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        let change_set = build_change_set(
            "team-x",
            "s-implementer",
            "implementer",
            &base,
            &["a.txt".to_string()],
            &root,
            None,
        );
        assert_eq!(change_set.status, ChangeSetStatus::PendingReview);
        let base_hash = &change_set.base_hashes[0];
        assert_eq!(
            base_hash.sha256.as_deref(),
            Some(CasStore::hash_of(b"v1").as_str())
        );
        assert!(base_hash.content_available, "基线内容应可恢复");
        assert_eq!(
            change_set.result_hashes[0].sha256.as_deref(),
            Some(CasStore::hash_of(b"v2").as_str())
        );

        // 恢复 → v1 回来。
        let report = restore_change_set(&root, &change_set, &cas).await;
        assert!(report.conflicts.is_empty(), "{report:?}");
        assert_eq!(report.restored, vec!["a.txt".to_string()]);
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_conflicts_on_user_modification_without_writing() {
        let dir = unique_temp_dir("conflict");
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let cas = test_cas(&dir);
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let base = snapshot_allowed_paths(&root, &[], &cas).await;
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        let change_set = build_change_set(
            "team-x",
            "s1",
            "implementer",
            &base,
            &["a.txt".to_string()],
            &root,
            None,
        );
        // 用户在恢复前又改了内容。
        std::fs::write(root.join("a.txt"), b"user-edit").unwrap();
        let report = restore_change_set(&root, &change_set, &cas).await;
        assert_eq!(report.conflicts, vec!["a.txt".to_string()]);
        assert!(report.restored.is_empty(), "冲突时不得落任何写");
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"user-edit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn restore_deletes_created_file_and_restores_deleted_file() {
        let dir = unique_temp_dir("created-deleted");
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let cas = test_cas(&dir);
        std::fs::write(root.join("kept.txt"), b"base").unwrap();
        let base = snapshot_allowed_paths(&root, &[], &cas).await;
        // Worker：新建 created.txt、删除 kept.txt。
        std::fs::write(root.join("created.txt"), b"new").unwrap();
        std::fs::remove_file(root.join("kept.txt")).unwrap();
        let changed = vec!["created.txt".to_string(), "kept.txt".to_string()];
        let change_set =
            build_change_set("team-x", "s1", "implementer", &base, &changed, &root, None);
        let created_base = change_set
            .base_hashes
            .iter()
            .find(|h| h.path == "created.txt")
            .unwrap();
        assert!(
            created_base.sha256.is_none() && created_base.content_available,
            "完整快照下新建文件基线 = (None, true)（恢复即删除）"
        );
        let kept_result = change_set
            .result_hashes
            .iter()
            .find(|h| h.path == "kept.txt")
            .unwrap();
        assert!(kept_result.sha256.is_none(), "被删文件结果应为 None");

        let report = restore_change_set(&root, &change_set, &cas).await;
        assert!(report.conflicts.is_empty(), "{report:?}");
        assert!(!root.join("created.txt").exists(), "新建文件应被删除恢复");
        assert_eq!(
            std::fs::read(root.join("kept.txt")).unwrap(),
            b"base",
            "被删文件应写回基线"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn already_restored_files_are_skipped() {
        let dir = unique_temp_dir("already");
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let cas = test_cas(&dir);
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let base = snapshot_allowed_paths(&root, &[], &cas).await;
        std::fs::write(root.join("a.txt"), b"v2").unwrap();
        let change_set = build_change_set(
            "team-x",
            "s1",
            "implementer",
            &base,
            &["a.txt".to_string()],
            &root,
            None,
        );
        // 用户已手工恢复到基线。
        std::fs::write(root.join("a.txt"), b"v1").unwrap();
        let report = restore_change_set(&root, &change_set, &cas).await;
        assert!(report.conflicts.is_empty());
        assert_eq!(report.already_restored, vec!["a.txt".to_string()]);
        assert!(report.restored.is_empty());
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn base_tristate_mapping_is_conservative() {
        // (Some, true)=可恢复；scanned-but-unstorable=(None,false)=冲突；未扫+不完整=(None,false)。
        let mut base = WorkspaceBaseSnapshot::default();
        base.entries
            .insert("ok.txt".to_string(), "hash-ok".to_string());
        base.scanned.insert("ok.txt".to_string());
        base.scanned.insert("big.txt".to_string());
        base.complete = false; // 模拟预算耗尽：未扫到的文件按未知处理
        let changed = vec![
            "ok.txt".to_string(),
            "big.txt".to_string(),
            "unscanned-new.txt".to_string(),
        ];
        let change_set = build_change_set(
            "team-x",
            "s1",
            "implementer",
            &base,
            &changed,
            Path::new("."),
            None,
        );
        let by_path = |p: &str| {
            change_set
                .base_hashes
                .iter()
                .find(|h| h.path == p)
                .unwrap()
                .clone()
        };
        let ok = by_path("ok.txt");
        assert_eq!(ok.sha256.as_deref(), Some("hash-ok"));
        assert!(ok.content_available);
        let big = by_path("big.txt");
        assert!(
            big.sha256.is_none() && !big.content_available,
            "扫到但不可用 → 冲突语义"
        );
        let unknown = by_path("unscanned-new.txt");
        assert!(
            unknown.sha256.is_none() && !unknown.content_available,
            "不完整快照下未扫到 → 保守按未知（绝不误删）"
        );
    }

    #[tokio::test]
    async fn snapshot_excludes_and_respects_size_cap() {
        let dir = unique_temp_dir("excludes");
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".git").join("HEAD"), b"x").unwrap();
        std::fs::write(root.join("node_modules").join("m.js"), b"x").unwrap();
        std::fs::write(root.join("src").join("main.rs"), b"fn main() {}").unwrap();
        let cas = test_cas(&dir);
        let base = snapshot_allowed_paths(&root, &[], &cas).await;
        assert!(base.entries.contains_key("src/main.rs"));
        assert!(
            !base.entries.contains_key(".git/HEAD"),
            "排除目录不得入基线"
        );
        assert!(!base.scanned.contains(".git/HEAD"));
        assert!(!base.entries.contains_key("node_modules/m.js"));
        // 超限文件：只「扫到」不存内容（恢复走冲突路径）。
        std::fs::write(
            root.join("big.bin"),
            vec![0u8; (MAX_SNAPSHOT_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        let base2 = snapshot_allowed_paths(&root, &[], &cas).await;
        assert!(!base2.entries.contains_key("big.bin"));
        assert!(base2.scanned.contains("big.bin"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn allowed_paths_limit_snapshot_scope() {
        let dir = unique_temp_dir("allowed-scope");
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("src").join("a.rs"), b"a").unwrap();
        std::fs::write(root.join("docs").join("b.md"), b"b").unwrap();
        let cas = test_cas(&dir);
        let base = snapshot_allowed_paths(&root, &[root.join("src")], &cas).await;
        assert!(base.entries.contains_key("src/a.rs"));
        assert!(
            !base.entries.contains_key("docs/b.md"),
            "白名单外不得入基线"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 九期（一路）：执行前已脏的文件被 Agent 再次修改后——
    // ① 必须进入 changed_files（由调用方检测，这里锁 build_change_set/恢复语义）；
    // ② 基线哈希 = 执行前内容（脏内容），reject 恢复到「Agent 执行前状态」，
    //    绝不能恢复到 Git HEAD 内容。
    #[tokio::test]
    async fn pre_dirty_file_recovers_to_pre_agent_content_not_head() {
        let dir = unique_temp_dir("pre-dirty");
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let cas = test_cas(&dir);
        let head_content = b"fn a() {} // HEAD"; // Git HEAD 内容（Agent 未参与）
        let pre_agent_content = b"fn a() {} // user dirty edit"; // 执行前已脏内容
        let agent_content = b"fn a() { /* agent fix */ }"; // Agent 执行后内容
        std::fs::write(root.join("a.rs"), head_content).unwrap();
        // 用户先手改（执行前已脏）→ Agent 拿到基线。
        std::fs::write(root.join("a.rs"), pre_agent_content).unwrap();
        let base = snapshot_allowed_paths(&root, &[], &cas).await;
        assert_eq!(
            base.entries["a.rs"],
            CasStore::hash_of(pre_agent_content),
            "基线必须是执行前内容，不是 HEAD"
        );
        // Agent 修改。
        std::fs::write(root.join("a.rs"), agent_content).unwrap();
        let change_set = build_change_set(
            "team-x",
            "s-impl",
            "implementer",
            &base,
            &["a.rs".to_string()],
            &root,
            None,
        );
        assert_eq!(
            change_set.base_hashes[0].sha256.as_deref(),
            Some(CasStore::hash_of(pre_agent_content).as_str())
        );
        // reject → 回到执行前脏内容（不是 HEAD）。
        let report = restore_change_set(&root, &change_set, &cas).await;
        assert!(report.conflicts.is_empty(), "{report:?}");
        assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), pre_agent_content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn degraded_diff_summary_reports_new_modified_deleted() {
        let dir = unique_temp_dir("degraded");
        let root = dir.join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let cas = test_cas(&dir);
        std::fs::write(root.join("mod.rs"), "line1\nline2\n").unwrap();
        std::fs::write(root.join("del.rs"), "gone\n").unwrap();
        let mut base = WorkspaceBaseSnapshot::default();
        base.complete = true;
        let mod_hash = cas.put(b"line1\nline2\n").unwrap();
        let del_hash = cas.put(b"gone\n").unwrap();
        base.entries.insert("mod.rs".to_string(), mod_hash);
        base.entries.insert("del.rs".to_string(), del_hash);
        // Agent：改 mod.rs、删 del.rs、建 new.rs。
        std::fs::write(root.join("mod.rs"), "line1\nline2-changed\nline3\n").unwrap();
        std::fs::remove_file(root.join("del.rs")).unwrap();
        std::fs::write(root.join("new.rs"), "brand new\n").unwrap();
        let changed = vec![
            "mod.rs".to_string(),
            "del.rs".to_string(),
            "new.rs".to_string(),
        ];
        let summary = degraded_diff_summary(&root, &base, &changed, &cas);
        assert!(summary.contains("mod.rs（修改）"), "{summary}");
        assert!(summary.contains("del.rs（删除）"));
        assert!(summary.contains("new.rs（新建）"));
        assert!(
            summary.contains("+ line2-changed"),
            "修改文件应有行级摘录：{summary}"
        );
        assert!(summary.contains("- gone"), "删除文件应有内容摘录");
        assert!(summary.contains("+ brand new"));
        assert!(summary.contains("cas://"), "基线应带 CAS 引用");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
