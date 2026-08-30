//! 工作区变更追踪（七期 · 二路；九期 · 一路修正检测算法）：Worker 执行前后
//! git 快照、写白名单校验、变更落盘。
//!
//! 数据面（都在 TeamRun 数据目录，重启可读）：
//! - `<run_dir>/<team_id>-workspace-changes.json`：变更记录数组（追加写，逐步骤）；
//! - `<run_dir>/<team_id>-changes/<step>-<ts>.patch`：本次实际变更文件的 git 补丁
//!   （best-effort）；git 不可用/补丁为空时落退化差异摘要
//!   `<step>-<ts>.diff.txt`（执行前 CAS 基线 × 执行后内容，见
//!   `change_set::degraded_diff_summary`）。
//!
//! 语义（九期一路修正）：
//! - 快照 = `git status --porcelain`（状态行）+ `git diff --stat`（摘要文本）；
//!   非 git 目录 / 无 git 可执行 → `git=false`：变更不可检测，白名单校验对空变更
//!   自然放行（检测能力以环境为准，能力缺失不判违规，不阻塞任务）；
//! - **changed_files = 窗口差集 ∪ 重命名 old 侧 ∪ 内容哈希差集**（九期前只做
//!   「前后两次 porcelain 的路径集合差」，漏掉「执行前已脏、执行后仍脏但内容变了」
//!   的文件——Agent 二次修改用户已改文件时 changed_files 为空、ChangeSet 丢失）；
//!   内容哈希差集以「执行前内容哈希 × 执行后内容哈希」为准（执行前哈希来自
//!   ChangeSet 基线快照 + 执行前已脏文件的直接读取），删除（内容消失）同样计入；
//! - 白名单校验对上述合并后的 `changed_files` 做（含执行前已脏但本次被继续修改的
//!   文件——越界写不再因「路径早已脏」而漏检）；
//! - 越界 → `Err("scope_violation: …")`（失败码前缀与 workswarm 失败口径一致），
//!   由包装层转成步骤失败——不登记成功 Artifact；
//! - 追踪是旁路：落盘 IO 失败只告警不阻断任务。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// porcelain 状态行数上限（防超大仓库膨胀快照体）。
const STATUS_LINE_CAP: usize = 500;

/// diff 补丁限定路径数上限：超过则退回全树 `git diff HEAD`（避免命令行超长）。
const DIFF_PATH_ARG_CAP: usize = 100;

/// 工作区 git 快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSnapshot {
    /// 是否取得有效 git 快照（false = 非 git 目录 / 无 git，变更不可检测）。
    pub git: bool,
    /// `git status --porcelain` 原始行（截断 [`STATUS_LINE_CAP`] 行防膨胀）。
    pub status: Vec<String>,
    /// `git diff --stat` 摘要文本（untracked 不入 diff，以 status 行为准）。
    pub diff_stat: String,
    /// 快照时刻（Unix 毫秒）。
    pub at: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 运行一次 git 子命令（current_dir = root）；失败（非 git 目录 / 无 git）→ None。
async fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// porcelain 状态行 → 相对路径（`XY PATH`；重命名/拷贝取 `old -> new` 的 new；
/// 带引号路径去引号）。解析失败 → None（忽略该行）。
pub fn parse_porcelain_path(line: &str) -> Option<String> {
    let rest = line.get(3..)?; // "XY " 前缀：X、Y 各 1 字符 + 1 空格
    let rest = rest.trim();
    let target = match rest.find(" -> ") {
        Some(index) => rest[index + 4..].trim(),
        None => rest,
    };
    let unquoted = target.trim_matches('"');
    if unquoted.is_empty() {
        None
    } else {
        Some(unquoted.to_string())
    }
}

/// porcelain 重命名/拷贝行 → `(old, new)` 路径对（非重命名行 → None）。
///
/// 九期（一路）：重命名的 old 侧在 `parse_porcelain_path` 里被丢弃，恢复时无法
/// 还原被移走的源文件——合并检测需要两侧。
pub fn parse_porcelain_rename(line: &str) -> Option<(String, String)> {
    let rest = line.get(3..)?.trim();
    let index = rest.find(" -> ")?;
    let old = rest[..index].trim().trim_matches('"');
    let new = rest[index + 4..].trim().trim_matches('"');
    if old.is_empty() || new.is_empty() {
        None
    } else {
        Some((old.to_string(), new.to_string()))
    }
}

impl GitSnapshot {
    /// 采集快照（git 不可用 → `git:false` 空快照，不视为错误）。
    ///
    /// `-uall`：未跟踪目录展开为具体文件（否则 porcelain 只给 `?? dir/`，
    /// 变更文件列表与白名单校验都拿不到文件级路径）。
    pub async fn snapshot(root: &Path) -> Self {
        let at = now_ms();
        match git_output(root, &["status", "--porcelain", "-uall"]).await {
            Some(text) => {
                let status: Vec<String> = text
                    .lines()
                    .map(str::trim_end)
                    .filter(|line| !line.is_empty())
                    .take(STATUS_LINE_CAP)
                    .map(str::to_string)
                    .collect();
                let diff_stat = git_output(root, &["diff", "--stat"])
                    .await
                    .unwrap_or_default();
                Self {
                    git: true,
                    status,
                    diff_stat,
                    at,
                }
            }
            None => Self {
                git: false,
                status: Vec::new(),
                diff_stat: String::new(),
                at,
            },
        }
    }

    /// 本次执行窗口新增的变更文件（porcelain 状态行差集 → 相对路径，去重保序）。
    ///
    /// 注意：这是**纯路径集合差**——执行前已脏的文件不在结果里（九期一路的合并
    /// 检测见 [`merge_changed_files`]；本方法保留为合并算法的第一层）。
    pub fn changed_files(&self, before: &GitSnapshot) -> Vec<String> {
        if !self.git || !before.git {
            return Vec::new();
        }
        let before_paths: HashSet<String> = before
            .status
            .iter()
            .filter_map(|line| parse_porcelain_path(line))
            .collect();
        let mut seen = HashSet::new();
        let mut changed = Vec::new();
        for path in self
            .status
            .iter()
            .filter_map(|line| parse_porcelain_path(line))
        {
            if !before_paths.contains(&path) && seen.insert(path.clone()) {
                changed.push(path);
            }
        }
        changed
    }

    /// 快照中的脏路径（去重保序；`git=false` → 空）。
    pub fn dirty_paths(&self) -> Vec<String> {
        if !self.git {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        self.status
            .iter()
            .filter_map(|line| parse_porcelain_path(line))
            .filter(|path| seen.insert(path.clone()))
            .collect()
    }
}

/// 单文件当前内容哈希（不存在/不可读 → None）。父模块（TrackedRoleWorker 基线
/// 采集）与本模块共用；与 core `change_set::file_hash` 同口径。
pub(crate) fn content_hash(root: &Path, relative: &str) -> Option<String> {
    let bytes = std::fs::read(root.join(relative)).ok()?;
    Some(owo_agent_core::cas_store::CasStore::hash_of(&bytes))
}

/// 九期（一路）：合并变更检测——`changed_files` = **窗口差集 ∪ 重命名 old 侧 ∪
/// 内容哈希差集**。
///
/// - `pre_dirty_hashes`：执行前已脏文件的内容哈希（执行前采集；`Some(Some(hash))` =
///   有基线，`Some(None)` = 执行前不存在/不可读，键缺失 = 未登记——后两者只有
///   「内容消失（删除）」可证明变更，保守不计修改）；
/// - 窗口差集捕获：新建/暂存新增/干净文件被修改或删除/重命名 new 侧；
/// - 内容哈希差集捕获：执行前已脏 → 执行后仍脏但内容变化（核心修复点）、执行前
///   已脏文件被删除、执行前已脏文件被恢复到 HEAD（内容 ≠ 执行前脏内容，同样计入
///   ——ChangeSet 恢复目标是「执行前状态」，回退到 HEAD 对它而言也是一次修改）；
/// - 重命名 old 侧捕获：post 有 `R old -> new` 且 pre 没有同一重命名 → old 被移走，
///   计入 changed（恢复时才能还原源文件）。
pub fn merge_changed_files(
    pre: &GitSnapshot,
    post: &GitSnapshot,
    pre_dirty_hashes: &HashMap<String, Option<String>>,
    root: &Path,
) -> Vec<String> {
    if !pre.git || !post.git {
        return Vec::new();
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut changed: Vec<String> = Vec::new();
    // 1) 窗口差集（新建/删除干净文件/重命名 new 侧/首次修改）。
    for path in post.changed_files(pre) {
        if seen.insert(path.clone()) {
            changed.push(path);
        }
    }
    // 2) 重命名 old 侧（pre 不存在同一重命名 → old 在窗口内被移走）。
    let pre_renames: HashSet<(String, String)> = pre
        .status
        .iter()
        .filter_map(|line| parse_porcelain_rename(line))
        .collect();
    for line in &post.status {
        if let Some((old, new)) = parse_porcelain_rename(line) {
            if !pre_renames.contains(&(old.clone(), new.clone())) && seen.insert(old.clone()) {
                changed.push(old);
            }
        }
    }
    // 3) 内容哈希差集（执行前已脏路径 × 执行后内容）。
    for path in pre.dirty_paths() {
        if seen.contains(&path) {
            continue;
        }
        let current = content_hash(root, &path);
        let changed_now = match pre_dirty_hashes.get(&path) {
            Some(Some(pre_hash)) => current.as_deref() != Some(pre_hash.as_str()),
            // 执行前不存在/不可读/未登记：只有删除可证明变更（保守，不误报修改）。
            _ => current.is_none(),
        };
        if changed_now && seen.insert(path.clone()) {
            changed.push(path);
        }
    }
    changed
}

/// 去掉 Windows verbatim 前缀（`\\?\C:\...` → `C:\...`）：canonicalize 语义不变。
/// 绑定侧 root/allowed 存储前去前缀，而 `canonicalize` 产物带前缀——比对两侧
/// 必须同口径，否则 `starts_with` 恒 false，白名单内变更会被误判越界。
fn simplify_path(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped.to_string()),
        None => path.to_path_buf(),
    }
}

/// 写白名单校验：`changed`（相对路径）必须全部落在 `allowed`（绝对前缀）内。
///
/// - `changed` 为空（无窗口新增变更 / 非 git 检测不到）→ 放行；
/// - `allowed` 为空 = 未声明白名单（工作区内可写）→ 放行；
/// - 否则逐条解析为绝对路径（canonicalize，不存在的目标按「父目录 canonicalize +
///   文件名」解析，与绑定侧口径一致；两侧比对前去 verbatim 前缀）；
/// - 越界 → `Err("scope_violation: …")`。
pub fn check_whitelist(changed: &[String], root: &Path, allowed: &[PathBuf]) -> Result<(), String> {
    if changed.is_empty() || allowed.is_empty() {
        return Ok(());
    }
    let base = simplify_path(&root.canonicalize().unwrap_or_else(|_| root.to_path_buf()));
    let allowed: Vec<PathBuf> = allowed.iter().map(|prefix| simplify_path(prefix)).collect();
    let offenders: Vec<String> = changed
        .iter()
        .filter(|relative| {
            let candidate = base.join(relative);
            let candidate = candidate.canonicalize().unwrap_or_else(|_| {
                candidate
                    .parent()
                    .and_then(|parent| parent.canonicalize().ok())
                    .map(|parent| parent.join(candidate.file_name().unwrap_or_default()))
                    .unwrap_or_else(|| candidate.clone())
            });
            let candidate = simplify_path(&candidate);
            !allowed.iter().any(|prefix| candidate.starts_with(prefix))
        })
        .cloned()
        .collect();
    if offenders.is_empty() {
        return Ok(());
    }
    let list = allowed
        .iter()
        .map(|prefix| prefix.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "scope_violation: 白名单外文件变更：{}（允许：{list}）",
        offenders.join(", ")
    ))
}

/// 单次 Worker 执行的变更记录（`workspace-changes.json` 数组元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// 角色。
    pub role: String,
    /// 步骤 ID（`_workswarm.step_id`；未知为 `unknown`）。
    pub step: String,
    /// 记录时刻（Unix 毫秒）。
    pub at: u64,
    /// 是否取得有效 git 快照。
    pub git: bool,
    /// 本次执行窗口新增的变更文件（相对路径）。
    pub changed_files: Vec<String>,
    /// diff 摘要（执行后快照的 `git diff --stat` 文本）。
    pub diff_summary: String,
    /// diff 引用（`<team_id>-changes/<file>.patch|.diff.txt`，相对 run_dir；九期一路：
    /// 仅当有实际变更文件时生成——git 补丁不可用落退化差异摘要，无变更为 None）。
    pub diff_ref: Option<String>,
    /// 白名单越界原因（None = 通过）。
    pub violation: Option<String>,
}

/// 变更追踪器：一个写角色 worker 的追踪配置（由 TrackedRoleWorker 包装层持有）。
#[derive(Debug, Clone)]
pub struct Tracker {
    /// 追踪工作区根（绑定根 / 全局工作区）。
    pub root: PathBuf,
    /// TeamRun 数据目录（落盘基地）。
    pub run_dir: PathBuf,
    pub team_id: String,
    pub role: String,
    /// 最终写白名单（角色 ∩ 绑定；空 = 工作区内可写，校验放行）。
    pub allowed: Vec<PathBuf>,
    /// 八期（二路）：团队 CAS（ChangeSet 基线内容寻址存储；与 Artifact 共用）。
    pub cas: owo_agent_core::cas_store::CasStore,
    /// 八期（二路）：团队审计日志（ChangeSet 生成留痕；None = 不审计）。
    pub audit: Option<Arc<std::sync::Mutex<owo_agent_core::AuditLog>>>,
}

/// 步骤 ID → 补丁文件名安全片段（只留字母数字与 `-_`）。
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

impl Tracker {
    fn records_path(&self) -> PathBuf {
        self.run_dir
            .join(format!("{}-workspace-changes.json", self.team_id))
    }

    fn changes_dir(&self) -> PathBuf {
        self.run_dir.join(format!("{}-changes", self.team_id))
    }

    /// 本次实际变更文件的 git 补丁（staged + unstaged 相对 HEAD；限定到 changed
    /// 路径——不再落全仓 diff，执行前已有的无关脏文件不会混进本步骤补丁）。
    async fn git_diff_patch(&self, changed: &[String]) -> Option<String> {
        if changed.len() > DIFF_PATH_ARG_CAP {
            return git_output(&self.root, &["diff", "HEAD"]).await;
        }
        let mut args: Vec<&str> = vec!["diff", "HEAD", "--"];
        args.extend(changed.iter().map(String::as_str));
        git_output(&self.root, &args).await
    }

    /// 落盘：best-effort 保存本次实际变更的差异 + 追加变更记录。
    /// 返回落盘的记录（调用方据此生成 ChangeSet：diff_ref 等由记录携带）；
    /// 返回 Err 仅表示记录未落盘（旁路数据），由调用方告警不阻断。
    ///
    /// 九期（一路）diff_ref 口径：仅当 `changed` 非空时生成——git 可用且补丁非空
    /// 落 `.patch`（限定变更文件）；git 不可用/补丁为空（如全部是未跟踪新文件）
    /// 落 `.diff.txt` 退化摘要（执行前 CAS 基线 × 执行后内容）。
    /// `changed` 为空 → `diff_ref = None`（也不再产生空补丁文件）。
    pub async fn record(
        &self,
        step: &str,
        post: &GitSnapshot,
        changed: &[String],
        violation: Option<&str>,
        base: Option<&owo_agent_core::change_set::WorkspaceBaseSnapshot>,
    ) -> Result<ChangeRecord, String> {
        let mut diff_ref = None;
        if !changed.is_empty() {
            let git_patch = self
                .git_diff_patch(changed)
                .await
                .filter(|patch| !patch.trim().is_empty());
            let (content, extension) = match git_patch {
                Some(patch) => (patch, "patch"),
                None => {
                    let default_base = owo_agent_core::change_set::WorkspaceBaseSnapshot::default();
                    let fallback = base.unwrap_or(&default_base);
                    (
                        owo_agent_core::change_set::degraded_diff_summary(
                            &self.root, fallback, changed, &self.cas,
                        ),
                        "diff.txt",
                    )
                }
            };
            let dir = self.changes_dir();
            if tokio::fs::create_dir_all(&dir).await.is_ok() {
                let file_name = format!("{}-{}.{}", sanitize_step(step), post.at, extension);
                let path = dir.join(&file_name);
                if tokio::fs::write(&path, content).await.is_ok() {
                    diff_ref = Some(format!("{}-changes/{}", self.team_id, file_name));
                }
            }
        }
        let record = ChangeRecord {
            role: self.role.clone(),
            step: step.to_string(),
            at: now_ms(),
            git: post.git,
            changed_files: changed.to_vec(),
            diff_summary: post.diff_stat.clone(),
            diff_ref,
            violation: violation.map(str::to_string),
        };
        self.append_record(record.clone()).await?;
        Ok(record)
    }

    /// 追加一条记录（读-改-写 JSON 数组；缺失/损坏按空数组重建——记录文件是
    /// 旁路数据，损坏不阻断任务，也不覆盖其他角色已有记录之外的内容）。
    /// run_dir 缺失时先建目录（无变更路径不会创建 changes 子目录，记录仍须落盘）。
    async fn append_record(&self, record: ChangeRecord) -> Result<(), String> {
        let path = self.records_path();
        if let Some(parent) = path.parent() {
            if tokio::fs::create_dir_all(parent).await.is_err() {
                return Err("变更记录目录创建失败".to_string());
            }
        }
        let mut records: Vec<ChangeRecord> = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        records.push(record);
        let bytes = serde_json::to_vec_pretty(&records)
            .map_err(|error| format!("变更记录序列化失败：{error}"))?;
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|error| format!("变更记录落盘失败：{error}"))
    }
}

/// 读取变更记录（HTTP 读取面；缺失 → 空数组；损坏 → Err，由路由转 500）。
pub async fn load_records(run_dir: &Path, team_id: &str) -> Result<Vec<ChangeRecord>, String> {
    let path = run_dir.join(format!("{team_id}-workspace-changes.json"));
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("变更记录文件损坏（{}）：{error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("读取变更记录失败：{error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let error =
            check_whitelist(&["outside/secret.txt".to_string()], &root, &allowed).unwrap_err();
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
        let mut base = owo_agent_core::change_set::WorkspaceBaseSnapshot::default();
        base.complete = true;
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

    /// 测试辅助：毫秒时间戳（唯一临时目录用）。
    fn unique_tag_ms() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }
}
