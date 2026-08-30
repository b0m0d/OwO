//! ChangeSet 存储（八期 · 二路）：`<run_dir>/<team_id>-change-sets.json`
//! （JSON 数组，按 change_set_id upsert；重启可读）。
//!
//! 状态机（`apply_decision`）：
//! - `pending_review / conflicted` --accept--> `accepted`（保留文件现状）；
//! - `pending_review / conflicted` --reject/revert--> `rejected / reverted`
//!   （文件恢复由调用方先执行 [`crate::change_set::restore_change_set`]，恢复成功
//!   才落状态）；
//! - 同动作幂等键重放 → `replayed: true` 零副作用；已决定后跨动作 → `Conflict`
//!   （409）；`accepted / rejected / reverted` 为终态。
//!
//! 批准门控：[`ChangeSetStore::approval_block_reason`] —— 存在 `pending_review /
//! conflicted` 的 ChangeSet 时，该团队的代码 Artifact 可评审但不得成为最终
//! approved head（评审/Inbox 分派处接线）。

use owo_agent_protocol::{ChangeSet, ChangeSetDecision};
use std::path::{Path, PathBuf};

// 便捷 re-export：评审/Inbox 分派处直接 `change_set_store::ChangeSetStatus` 引用。
pub use owo_agent_protocol::ChangeSetStatus;

/// ChangeSet 存储错误（路由层映射：NotFound→404、Conflict→409、Storage→500）。
#[derive(Debug, Clone)]
pub enum ChangeSetStoreError {
    NotFound,
    Conflict(String),
    Storage(String),
}

impl std::fmt::Display for ChangeSetStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeSetStoreError::NotFound => write!(formatter, "ChangeSet 不存在"),
            ChangeSetStoreError::Conflict(message) => write!(formatter, "{message}"),
            ChangeSetStoreError::Storage(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for ChangeSetStoreError {}

/// 决定动作（与路由一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeSetAction {
    Accept,
    Reject,
    Revert,
}

impl ChangeSetAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeSetAction::Accept => "accept",
            ChangeSetAction::Reject => "reject",
            ChangeSetAction::Revert => "revert",
        }
    }

    fn status(&self) -> ChangeSetStatus {
        match self {
            ChangeSetAction::Accept => ChangeSetStatus::Accepted,
            ChangeSetAction::Reject => ChangeSetStatus::Rejected,
            ChangeSetAction::Revert => ChangeSetStatus::Reverted,
        }
    }
}

/// 决定结果（`replayed = true` 表示幂等重放，零副作用）。
#[derive(Debug, Clone)]
pub struct DecisionOutcome {
    pub change_set: ChangeSet,
    pub replayed: bool,
}

/// ChangeSet 存储（Clone 共享 run_dir）。
#[derive(Debug, Clone)]
pub struct ChangeSetStore {
    run_dir: PathBuf,
}

impl ChangeSetStore {
    pub fn new(run_dir: &Path) -> Self {
        Self {
            run_dir: run_dir.to_path_buf(),
        }
    }

    fn team_path(&self, team_id: &str) -> PathBuf {
        self.run_dir.join(format!("{team_id}-change-sets.json"))
    }

    fn load_team(&self, team_id: &str) -> Result<Vec<ChangeSet>, ChangeSetStoreError> {
        let path = self.team_path(team_id);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(ChangeSetStoreError::Storage(format!(
                    "读取 ChangeSet 文件失败（{}）：{error}",
                    path.display()
                )))
            }
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            ChangeSetStoreError::Storage(format!(
                "ChangeSet 文件损坏（{}）：{error}",
                path.display()
            ))
        })
    }

    fn save_team(&self, team_id: &str, records: &[ChangeSet]) -> Result<(), ChangeSetStoreError> {
        if let Some(parent) = self.run_dir.parent().or(Some(self.run_dir.as_path())) {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::create_dir_all(&self.run_dir);
        let bytes = serde_json::to_vec_pretty(records).map_err(|error| {
            ChangeSetStoreError::Storage(format!("ChangeSet 序列化失败：{error}"))
        })?;
        std::fs::write(self.team_path(team_id), bytes)
            .map_err(|error| ChangeSetStoreError::Storage(format!("ChangeSet 落盘失败：{error}")))
    }

    /// 按 id upsert（存在同 id → 原位替换；否则追加）。
    pub fn save_upsert(&self, change_set: &ChangeSet) -> Result<(), ChangeSetStoreError> {
        let mut records = self.load_team(&change_set.team_id)?;
        match records
            .iter_mut()
            .find(|record| record.change_set_id == change_set.change_set_id)
        {
            Some(existing) => *existing = change_set.clone(),
            None => records.push(change_set.clone()),
        }
        self.save_team(&change_set.team_id, &records)
    }

    /// 团队全部 ChangeSet（缺失 → 空数组）。
    pub fn list_for_team(&self, team_id: &str) -> Result<Vec<ChangeSet>, ChangeSetStoreError> {
        self.load_team(team_id)
    }

    /// 跨团队全量（扫描 `*-change-sets.json`；规模 = 有变更的团队数，小）。
    pub fn list_all(&self) -> Result<Vec<ChangeSet>, ChangeSetStoreError> {
        let mut all = Vec::new();
        let entries = std::fs::read_dir(&self.run_dir)
            .map_err(|error| ChangeSetStoreError::Storage(format!("扫描 run_dir 失败：{error}")))?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(team_id) = name.strip_suffix("-change-sets.json") else {
                continue;
            };
            all.extend(self.load_team(team_id)?);
        }
        Ok(all)
    }

    /// 按 id 查找。
    pub fn find(&self, change_set_id: &str) -> Result<Option<ChangeSet>, ChangeSetStoreError> {
        Ok(self
            .list_all()?
            .into_iter()
            .find(|record| record.change_set_id == change_set_id))
    }

    /// 标记冲突（revert/reject 检测到用户改动时调用；已决定 → Conflict）。
    pub fn mark_conflicted(
        &self,
        change_set_id: &str,
        conflicts: &[String],
    ) -> Result<ChangeSet, ChangeSetStoreError> {
        let mut all = self.list_all()?;
        let Some(record) = all
            .iter_mut()
            .find(|record| record.change_set_id == change_set_id)
        else {
            return Err(ChangeSetStoreError::NotFound);
        };
        if record.decision.is_some() {
            return Err(ChangeSetStoreError::Conflict(format!(
                "ChangeSet {change_set_id} 已决定（{}），不能再标记冲突",
                record.status_label()
            )));
        }
        record.status = ChangeSetStatus::Conflicted;
        record.conflicts = conflicts.to_vec();
        let updated = record.clone();
        self.save_team(&updated.team_id, &all)?;
        Ok(updated)
    }

    /// 落决定（accept/reject/revert 共用；幂等键 + 终态校验在此内聚）。
    pub fn apply_decision(
        &self,
        change_set_id: &str,
        action: ChangeSetAction,
        idempotency_key: &str,
        note: Option<&str>,
    ) -> Result<DecisionOutcome, ChangeSetStoreError> {
        let mut all = self.list_all()?;
        let Some(record) = all
            .iter_mut()
            .find(|record| record.change_set_id == change_set_id)
        else {
            return Err(ChangeSetStoreError::NotFound);
        };
        // 幂等重放先判：同动作 → 零副作用返回现状。
        if let Some(existing) = &record.decision {
            if existing.action == action.as_str() {
                return Ok(DecisionOutcome {
                    change_set: record.clone(),
                    replayed: true,
                });
            }
            return Err(ChangeSetStoreError::Conflict(format!(
                "ChangeSet {change_set_id} 已 {}，不能再 {}",
                existing.action,
                action.as_str()
            )));
        }
        if !matches!(
            record.status,
            ChangeSetStatus::PendingReview | ChangeSetStatus::Conflicted
        ) {
            return Err(ChangeSetStoreError::Conflict(format!(
                "ChangeSet {change_set_id} 状态为 {}，不接受 {}",
                record.status_label(),
                action.as_str()
            )));
        }
        record.status = action.status();
        record.decision = Some(ChangeSetDecision {
            action: action.as_str().to_string(),
            idempotency_key: idempotency_key.to_string(),
            decided_at: chrono::Utc::now().to_rfc3339(),
            note: note
                .filter(|note| !note.trim().is_empty())
                .map(str::to_string),
        });
        record.conflicts.clear();
        let updated = record.clone();
        self.save_team(&updated.team_id, &all)?;
        Ok(DecisionOutcome {
            change_set: updated,
            replayed: false,
        })
    }

    /// 批准门控（纯函数）：存在 `pending_review / conflicted` 的 ChangeSet → 阻断
    /// 最终 approved head（`accepted / rejected / reverted` 不阻断——前者的修改已
    /// 接受，后两者的修改已撤销、由重试版本的新 ChangeSet 接力）。
    pub fn approval_block_reason(records: &[ChangeSet]) -> Option<String> {
        records
            .iter()
            .find(|record| {
                matches!(
                    record.status,
                    ChangeSetStatus::PendingReview | ChangeSetStatus::Conflicted
                )
            })
            .map(|record| {
                format!(
                    "存在未接受 ChangeSet：{}（{}，步骤 {}，角色 {}）——代码 Artifact 可评审，但不能成为最终 approved head",
                    record.change_set_id,
                    record.status_label(),
                    record.step_id,
                    record.role
                )
            })
    }

    /// 团队级批准门控（评审/Inbox 分派处接线的入口）。
    pub fn approval_block_for_team(
        &self,
        team_id: &str,
    ) -> Result<Option<String>, ChangeSetStoreError> {
        Ok(Self::approval_block_reason(&self.list_for_team(team_id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use owo_agent_protocol::ChangeSetFileHash;

    fn sample_change_set(id: &str, team_id: &str) -> ChangeSet {
        ChangeSet {
            change_set_id: id.to_string(),
            team_id: team_id.to_string(),
            step_id: "s-implementer".to_string(),
            role: "implementer".to_string(),
            base_hashes: vec![ChangeSetFileHash {
                path: "src/a.rs".to_string(),
                sha256: Some("base".to_string()),
                content_available: true,
            }],
            result_hashes: vec![ChangeSetFileHash {
                path: "src/a.rs".to_string(),
                sha256: Some("result".to_string()),
                content_available: false,
            }],
            changed_files: vec!["src/a.rs".to_string()],
            diff_ref: None,
            status: ChangeSetStatus::PendingReview,
            created_at: "2026-01-01T00:00:00+00:00".to_string(),
            decision: None,
            conflicts: Vec::new(),
        }
    }

    fn unique_run_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "owo-change-set-store-{}-{}-{}",
            tag,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_list_find_roundtrip_and_cross_team_isolation() {
        let run_dir = unique_run_dir("roundtrip");
        let store = ChangeSetStore::new(&run_dir);
        let cs_a = sample_change_set("cs-team-a-s1-1", "team-a");
        let cs_b = sample_change_set("cs-team-b-s1-2", "team-b");
        store.save_upsert(&cs_a).unwrap();
        store.save_upsert(&cs_b).unwrap();
        assert_eq!(store.list_for_team("team-a").unwrap().len(), 1);
        assert_eq!(store.list_for_team("team-b").unwrap().len(), 1);
        assert!(store.list_for_team("team-a").unwrap()[0].change_set_id == "cs-team-a-s1-1");
        assert_eq!(
            store.find("cs-team-b-s1-2").unwrap().unwrap().team_id,
            "team-b"
        );
        assert!(store.find("cs-team-x-s9-9").unwrap().is_none());
        // upsert：同 id 原位替换（不重复）。
        let mut updated = cs_a.clone();
        updated.status = ChangeSetStatus::Accepted;
        store.save_upsert(&updated).unwrap();
        assert_eq!(store.list_for_team("team-a").unwrap().len(), 1);
        assert_eq!(
            store.list_for_team("team-a").unwrap()[0].status,
            ChangeSetStatus::Accepted
        );
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn apply_decision_idempotent_replay_and_cross_action_conflict() {
        let run_dir = unique_run_dir("decision");
        let store = ChangeSetStore::new(&run_dir);
        let cs = sample_change_set("cs-team-a-s1-1", "team-a");
        store.save_upsert(&cs).unwrap();

        // 首次 accept → accepted。
        let outcome = store
            .apply_decision("cs-team-a-s1-1", ChangeSetAction::Accept, "k1", None)
            .unwrap();
        assert!(!outcome.replayed);
        assert_eq!(outcome.change_set.status, ChangeSetStatus::Accepted);
        assert_eq!(
            outcome.change_set.decision.as_ref().unwrap().action,
            "accept"
        );

        // 同动作重放（任意键）→ 零副作用。
        let replay = store
            .apply_decision("cs-team-a-s1-1", ChangeSetAction::Accept, "k1", None)
            .unwrap();
        assert!(replay.replayed);
        let replay2 = store
            .apply_decision("cs-team-a-s1-1", ChangeSetAction::Accept, "k2", None)
            .unwrap();
        assert!(replay2.replayed, "同动作不同键也只重放，不产生第二次副作用");

        // 跨动作（reject after accept）→ Conflict。
        assert!(matches!(
            store.apply_decision("cs-team-a-s1-1", ChangeSetAction::Reject, "k3", None),
            Err(ChangeSetStoreError::Conflict(_))
        ));
        assert!(matches!(
            store.apply_decision("cs-team-a-s1-1", ChangeSetAction::Revert, "k4", None),
            Err(ChangeSetStoreError::Conflict(_))
        ));
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn conflicted_can_be_decided_afterwards() {
        let run_dir = unique_run_dir("conflicted");
        let store = ChangeSetStore::new(&run_dir);
        store
            .save_upsert(&sample_change_set("cs-team-a-s1-1", "team-a"))
            .unwrap();
        // revert 撞上用户改动 → conflicted。
        let conflicted = store
            .mark_conflicted("cs-team-a-s1-1", &["src/a.rs".to_string()])
            .unwrap();
        assert_eq!(conflicted.status, ChangeSetStatus::Conflicted);
        assert_eq!(conflicted.conflicts, vec!["src/a.rs".to_string()]);
        // 用户处理后（文件恢复到结果哈希状态）→ 仍可落决定。
        let outcome = store
            .apply_decision(
                "cs-team-a-s1-1",
                ChangeSetAction::Reject,
                "k1",
                Some("用户已处理"),
            )
            .unwrap();
        assert!(!outcome.replayed);
        assert_eq!(outcome.change_set.status, ChangeSetStatus::Rejected);
        assert!(
            outcome.change_set.conflicts.is_empty(),
            "落决定后冲突清单清空"
        );
        let _ = std::fs::remove_dir_all(&run_dir);
    }

    #[test]
    fn approval_block_reason_semantics() {
        let mut pending = sample_change_set("cs-t-s1-1", "t");
        assert!(ChangeSetStore::approval_block_reason(&[pending.clone()]).is_some());
        pending.status = ChangeSetStatus::Accepted;
        assert!(
            ChangeSetStore::approval_block_reason(std::slice::from_ref(&pending)).is_none(),
            "accepted 不阻断"
        );
        let mut rejected = sample_change_set("cs-t-s1-2", "t");
        rejected.status = ChangeSetStatus::Rejected;
        assert!(
            ChangeSetStore::approval_block_reason(&[pending, rejected]).is_none(),
            "rejected/reverted（修改已撤销）不阻断，由重试版本的新 ChangeSet 接力"
        );
        let mut conflicted = sample_change_set("cs-t-s1-3", "t");
        conflicted.status = ChangeSetStatus::Conflicted;
        assert!(ChangeSetStore::approval_block_reason(&[conflicted]).is_some());
    }
}
