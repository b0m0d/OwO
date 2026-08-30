//! 工作区变更追踪（七期 · 二路）：Worker 执行前后 git 快照、写白名单校验、变更落盘。
//!
//! 数据面（都在 TeamRun 数据目录，重启可读）：
//! - `<run_dir>/<team_id>-workspace-changes.json`：变更记录数组（追加写，逐步骤）；
//! - `<run_dir>/<team_id>-changes/<step>-<ts>.patch`：执行后 `git diff` 全量补丁
//!   （best-effort；非 git 工作区跳过）。
//!
//! 语义：
//! - 快照 = `git status --porcelain`（状态行）+ `git diff --stat`（摘要文本）；
//!   非 git 目录 / 无 git 可执行 → `git=false`：变更不可检测，白名单校验对空变更
//!   自然放行（检测能力以环境为准，能力缺失不判违规，不阻塞任务）；
//! - 白名单校验只对「本次执行窗口新增的变更文件」做（前快照已有的变更不追溯）；
//! - 越界 → `Err("scope_violation: …")`（失败码前缀与 workswarm 失败口径一致），
//!   由包装层转成步骤失败——不登记成功 Artifact；
//! - 追踪是旁路：落盘 IO 失败只告警不阻断任务。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// porcelain 状态行数上限（防超大仓库膨胀快照体）。
const STATUS_LINE_CAP: usize = 500;

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
    /// diff 补丁引用（`<team_id>-changes/<file>.patch`，相对 run_dir；非 git / 无变更 → None）。
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

    /// 落盘：best-effort 保存执行后 diff 补丁 + 追加变更记录。
    /// 返回 Err 仅表示记录未落盘（旁路数据），由调用方告警不阻断。
    pub async fn record(
        &self,
        step: &str,
        post: &GitSnapshot,
        changed: &[String],
        violation: Option<&str>,
    ) -> Result<(), String> {
        // diff 补丁（best-effort）：执行后全量 `git diff`（暂存 + 未暂存）。
        let mut diff_ref = None;
        if post.git {
            if let Some(patch) = git_output(&self.root, &["diff"]).await {
                if !patch.trim().is_empty() {
                    let dir = self.changes_dir();
                    if tokio::fs::create_dir_all(&dir).await.is_ok() {
                        let file_name = format!("{}-{}.patch", sanitize_step(step), post.at);
                        let path = dir.join(&file_name);
                        if tokio::fs::write(&path, patch).await.is_ok() {
                            diff_ref = Some(format!("{}-changes/{}", self.team_id, file_name));
                        }
                    }
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
        self.append_record(record).await
    }

    /// 追加一条记录（读-改-写 JSON 数组；缺失/损坏按空数组重建——记录文件是
    /// 旁路数据，损坏不阻断任务，也不覆盖其他角色已有记录之外的内容）。
    async fn append_record(&self, record: ChangeRecord) -> Result<(), String> {
        let path = self.records_path();
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
}
