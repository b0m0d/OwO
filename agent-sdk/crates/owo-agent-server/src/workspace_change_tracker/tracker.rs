use super::git::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
pub(crate) fn sanitize_step(step: &str) -> String {
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
    pub(crate) fn records_path(&self) -> PathBuf {
        self.run_dir
            .join(format!("{}-workspace-changes.json", self.team_id))
    }

    pub(crate) fn changes_dir(&self) -> PathBuf {
        self.run_dir.join(format!("{}-changes", self.team_id))
    }

    /// 本次实际变更文件的补丁（十期 · 四路重做基线）：逐文件以**执行前 CAS 基线
    /// 内容**为基线生成 unified diff（`git diff --no-index`，临时基线文件 × 工作区
    /// 当前文件），而不是 `git diff HEAD`——执行前已脏文件的既有用户改动不会混入
    /// 本步骤补丁；新建文件 = 空基线（全量新增）、删除文件 = 空当前（全量删除）。
    ///
    /// 硬性约束（十期 · 四路 R1）：
    /// - `changed` 超过 [`DIFF_PATH_ARG_CAP`] → 返回 None，**绝不回退为全仓
    ///   `git diff HEAD`**；调用方落退化差异摘要（逐文件、CAS 基线、行级截断，
    ///   有界且不混入用户改动）；
    /// - 非 git 工作区（`post.git=false`）→ None（退化摘要同样以 CAS 基线为准）；
    /// - git 不可用 / 任一文件基线不可恢复（超限/不可读）→ None（退化摘要兜底）。
    pub(crate) async fn git_diff_patch(
        &self,
        changed: &[String],
        post_git: bool,
        base: Option<&owo_agent_core::change_set::WorkspaceBaseSnapshot>,
    ) -> Option<String> {
        if !post_git || changed.len() > DIFF_PATH_ARG_CAP {
            return None;
        }
        let default_base = owo_agent_core::change_set::WorkspaceBaseSnapshot::default();
        let base = base.unwrap_or(&default_base);
        // 临时目录：逐文件把「基线内容」物化为文件供 git diff --no-index 使用。
        let tmp_dir = std::env::temp_dir().join(format!(
            "owo-cas-diff-{}-{}",
            self.team_id,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        if std::fs::create_dir_all(&tmp_dir).is_err() {
            return None;
        }
        let result = self.git_diff_patch_inner(changed, base, &tmp_dir).await;
        let _ = std::fs::remove_dir_all(&tmp_dir);
        result
    }

    /// [`Self::git_diff_patch`] 的逐文件主体（临时目录生命周期由外层管理）。
    pub(crate) async fn git_diff_patch_inner(
        &self,
        changed: &[String],
        base: &owo_agent_core::change_set::WorkspaceBaseSnapshot,
        tmp_dir: &Path,
    ) -> Option<String> {
        let base_file = tmp_dir.join("baseline");
        let cur_file = tmp_dir.join("current");
        let mut parts: Vec<String> = Vec::new();
        for relative in changed {
            // 基线内容（新建文件无条目 → 空基线）。
            let baseline_bytes: Vec<u8> = match base.entries.get(relative) {
                Some(hash) => self.cas.get(hash)?, // 基线不可恢复 → 退化摘要兜底
                None => Vec::new(),
            };
            // 当前内容（删除文件读取失败 → 空当前）。
            let current_bytes = std::fs::read(self.root.join(relative)).unwrap_or_default();
            if baseline_bytes == current_bytes {
                continue; // 内容未变（理论上不出现在 changed 中）
            }
            std::fs::write(&base_file, &baseline_bytes).ok()?;
            std::fs::write(&cur_file, &current_bytes).ok()?;
            let label_a = format!("a/{relative}");
            let label_b = format!("b/{relative}");
            let base_str = base_file.to_string_lossy().into_owned();
            let cur_str = cur_file.to_string_lossy().into_owned();
            let output = tokio::process::Command::new("git")
                .args([
                    "diff",
                    "--no-index",
                    "--label",
                    &label_a,
                    "--label",
                    &label_b,
                ])
                .arg(&base_str)
                .arg(&cur_str)
                .output()
                .await
                .ok()?;
            // 退出码 1 = 有差异（正常）；0 = 无差异；>1 = 错误。
            let code = output.status.code().unwrap_or(1);
            if code > 1 {
                return None;
            }
            if code == 1 {
                parts.push(String::from_utf8_lossy(&output.stdout).into_owned());
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
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
                .git_diff_patch(changed, post.git, base)
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
    pub(crate) async fn append_record(&self, record: ChangeRecord) -> Result<(), String> {
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
