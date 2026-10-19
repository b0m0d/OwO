//! Host-owned source snapshots for behavior commands, shared by Single and Team.
//! Tool execution and authorization stay in ToolHost. Receipts prove that the
//! declared source bytes existed before the command and survived it unchanged.

use crate::plan::{ValidationVerdictV1, VerificationRequirementV1, VerificationScopeV1};
use crate::session::Session;
#[cfg(test)]
use crate::workspace_snapshot::MAX_FILE_BYTES;
use crate::workspace_snapshot::{WorkspaceSnapshotReader, MAX_SNAPSHOT_PATHS};
use crate::CommandExecutionReceipt;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub(crate) struct CommandSnapshot {
    pub hashes: BTreeMap<String, Option<String>>,
    pub complete: bool,
}

/// Known absence is Some(None); unreadable, escaping or oversized paths are None.
pub(crate) fn read_workspace_path(root: &Path, relative: &str) -> Option<Option<String>> {
    WorkspaceSnapshotReader::new(root)?.read(relative)
}

/// Source snapshots belong to registered behavior commands, not shell navigation.
/// Other commands retain execution metadata without pretending to prove source tests.
pub(crate) fn capture_for_tool(
    session: &Session,
    tool: &str,
    arguments: &serde_json::Value,
) -> Option<CommandSnapshot> {
    let command = arguments.get("command")?.as_str()?;
    (tool == "run_command" && crate::verification::is_registered_behavior_command(command))
        .then(|| capture_command_snapshot(session))
}

/// Snapshot unaccepted writes plus the active request's exact verification paths.
/// Accepted history is not repeatedly read unless the current plan references it.
pub(crate) fn capture_command_snapshot(session: &Session) -> CommandSnapshot {
    let mut paths = session
        .execution_receipts
        .iter()
        .filter(|receipt| !matches!(receipt.status.as_str(), "accepted" | "reverted"))
        .flat_map(|receipt| {
            receipt
                .changed_files
                .iter()
                .map(|path| path.replace('\\', "/"))
        })
        .collect::<BTreeSet<_>>();
    if let Some(plan) = &session.single_verification_plan {
        for requirement in &plan.requirements {
            if let VerificationScopeV1::WorkspacePaths { relative_paths } = &requirement.scope {
                paths.extend(relative_paths.iter().map(|path| path.replace('\\', "/")));
            }
        }
    }
    let mut complete = paths.len() <= MAX_SNAPSHOT_PATHS;
    let mut hashes = BTreeMap::new();
    let Some(mut reader) = WorkspaceSnapshotReader::new(&session.workspace) else {
        return CommandSnapshot {
            hashes,
            complete: false,
        };
    };
    for path in paths.into_iter().take(MAX_SNAPSHOT_PATHS) {
        match reader.read(&path) {
            Some(hash) => {
                hashes.insert(path, hash);
            }
            None => {
                complete = false;
            }
        }
    }
    CommandSnapshot { hashes, complete }
}

/// Common metadata and before/after identity gate; callers bind the result to their
/// request/attempt, accepted ChangeSet and live final workspace separately.
pub(crate) fn validate_command_receipt(
    requirement: &VerificationRequirementV1,
    receipt: &CommandExecutionReceipt,
) -> Result<BTreeMap<String, String>, (ValidationVerdictV1, String)> {
    let refuse = |verdict, detail: &str| Err((verdict, detail.to_string()));
    let Some(command) = requirement
        .arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
    else {
        return refuse(
            ValidationVerdictV1::Unsupported,
            "行为检查缺少 command 参数",
        );
    };
    if requirement.validator_id != "workspace-command-success-v1"
        || requirement.validator_version.as_deref() != Some("1")
        || !crate::verification::is_registered_behavior_command(command)
    {
        return refuse(
            ValidationVerdictV1::Unsupported,
            "行为验证器或命令未被宿主注册",
        );
    }
    if receipt.validator_id.as_deref() != Some("workspace-command-success-v1")
        || receipt.validator_version.as_deref() != Some("1")
        || receipt.command_sha256 != crate::CasStore::hash_of(command.trim().as_bytes())
    {
        return refuse(
            ValidationVerdictV1::Unverified,
            "宿主命令回执与计划身份不匹配",
        );
    }
    if receipt.exit_code != 0 {
        return Err((
            ValidationVerdictV1::Failed,
            format!("宿主命令退出码为 {}", receipt.exit_code),
        ));
    }
    let Some(duration) = receipt.duration_ms else {
        return refuse(ValidationVerdictV1::Unverified, "命令回执缺少宿主计时数据");
    };
    if duration > requirement.resources.timeout_ms {
        return refuse(ValidationVerdictV1::Failed, "宿主命令耗时超过验证计划预算");
    }
    if !receipt.workspace_hashes_complete {
        return refuse(
            ValidationVerdictV1::Unverified,
            "命令执行时宿主文件快照不完整",
        );
    }
    let VerificationScopeV1::WorkspacePaths { relative_paths } = &requirement.scope else {
        return refuse(
            ValidationVerdictV1::Unsupported,
            "行为检查必须绑定 WorkspacePaths",
        );
    };
    if relative_paths.is_empty() {
        return refuse(ValidationVerdictV1::Unverified, "行为检查没有绑定源码路径");
    }
    let mut subjects = BTreeMap::new();
    for raw in relative_paths {
        let path = raw.replace('\\', "/");
        let Some(before) = receipt.workspace_hashes_before.get(&path) else {
            return refuse(
                ValidationVerdictV1::Unverified,
                "命令回执缺少执行前源码快照",
            );
        };
        let Some(after) = receipt.workspace_hashes.get(&path) else {
            return refuse(
                ValidationVerdictV1::Unverified,
                "命令回执缺少执行后源码快照",
            );
        };
        if before != after {
            return refuse(
                ValidationVerdictV1::Stale,
                "行为命令执行期间源码发生变化，需要重新验证",
            );
        }
        subjects.insert(
            format!("workspace-path:{path}"),
            after
                .clone()
                .unwrap_or_else(crate::verification::workspace_path_absence_sha256),
        );
    }
    Ok(subjects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{VerificationPlanV1, VerificationResourcesV1};
    use crate::session::ExecutionReceipt;

    fn requirement() -> VerificationRequirementV1 {
        VerificationRequirementV1 {
            requirement_id: "behavior".to_string(),
            covers_requirement_ids: vec!["user:behavior".to_string()],
            validator_id: "workspace-command-success-v1".to_string(),
            validator_version: Some("1".to_string()),
            scope: VerificationScopeV1::WorkspacePaths {
                relative_paths: vec!["src/main.rs".to_string()],
            },
            arguments: serde_json::json!({"command":"cargo test"}),
            required: true,
            resources: VerificationResourcesV1 {
                cpu_slots: 1,
                memory_mb: 16,
                exclusive_workspace: false,
                timeout_ms: 1000,
            },
        }
    }

    fn receipt() -> CommandExecutionReceipt {
        let hashes = BTreeMap::from([("src/main.rs".to_string(), Some("source-a".to_string()))]);
        CommandExecutionReceipt {
            command_sha256: crate::CasStore::hash_of(b"cargo test"),
            exit_code: 0,
            result_sha256: "output".to_string(),
            duration_ms: Some(10),
            workspace_hashes_complete: true,
            validator_id: Some("workspace-command-success-v1".to_string()),
            validator_version: Some("1".to_string()),
            workspace_hashes_before: hashes.clone(),
            workspace_hashes: hashes,
        }
    }

    #[test]
    fn stable_command_proof_binds_normalized_source_identity() {
        let mut requirement = requirement();
        requirement.scope = VerificationScopeV1::WorkspacePaths {
            relative_paths: vec![["src", "main.rs"].join(&char::from(92).to_string())],
        };
        let subjects = validate_command_receipt(&requirement, &receipt()).unwrap();
        assert_eq!(
            subjects.get("workspace-path:src/main.rs"),
            Some(&"source-a".to_string())
        );
    }

    #[test]
    fn command_that_rewrites_source_cannot_prove_its_final_bytes_were_tested() {
        let mut receipt = receipt();
        receipt
            .workspace_hashes
            .insert("src/main.rs".to_string(), Some("source-b".to_string()));
        assert_eq!(
            validate_command_receipt(&requirement(), &receipt)
                .unwrap_err()
                .0,
            ValidationVerdictV1::Stale
        );
    }

    #[test]
    fn legacy_post_only_proof_and_incomplete_reads_cannot_pass() {
        let mut post_only = receipt();
        post_only.workspace_hashes_before.clear();
        assert_eq!(
            validate_command_receipt(&requirement(), &post_only)
                .unwrap_err()
                .0,
            ValidationVerdictV1::Unverified
        );
        let serialized = serde_json::to_value(receipt()).unwrap();
        let mut legacy = serialized.as_object().unwrap().clone();
        legacy.remove("workspace_hashes_before");
        let legacy: CommandExecutionReceipt =
            serde_json::from_value(serde_json::Value::Object(legacy)).unwrap();
        assert_eq!(
            validate_command_receipt(&requirement(), &legacy)
                .unwrap_err()
                .0,
            ValidationVerdictV1::Unverified
        );
    }

    #[test]
    fn command_failures_and_timeouts_are_not_accepted() {
        let mut failed = receipt();
        failed.exit_code = 1;
        assert_eq!(
            validate_command_receipt(&requirement(), &failed)
                .unwrap_err()
                .0,
            ValidationVerdictV1::Failed
        );
        let mut timed = receipt();
        timed.duration_ms = Some(1001);
        assert_eq!(
            validate_command_receipt(&requirement(), &timed)
                .unwrap_err()
                .0,
            ValidationVerdictV1::Failed
        );
        timed.duration_ms = None;
        assert_eq!(
            validate_command_receipt(&requirement(), &timed)
                .unwrap_err()
                .0,
            ValidationVerdictV1::Unverified
        );
    }

    #[test]
    fn deleted_source_keeps_a_known_absence_subject() {
        let mut receipt = receipt();
        receipt
            .workspace_hashes_before
            .insert("src/main.rs".to_string(), None);
        receipt
            .workspace_hashes
            .insert("src/main.rs".to_string(), None);
        let subjects = validate_command_receipt(&requirement(), &receipt).unwrap();
        assert_eq!(
            subjects.get("workspace-path:src/main.rs"),
            Some(&crate::verification::workspace_path_absence_sha256())
        );
    }

    fn write_receipt(path: &str, status: &str) -> ExecutionReceipt {
        ExecutionReceipt {
            receipt_id: path.to_string(),
            tool: "write_file".to_string(),
            turn_id: "turn".to_string(),
            changed_files: vec![path.to_string()],
            snapshot_keys: Default::default(),
            before_hashes: Default::default(),
            after_hashes: Default::default(),
            diff_sha256: "diff".to_string(),
            created_at: "now".to_string(),
            status: status.to_string(),
            validation_receipt_id: None,
        }
    }

    #[test]
    fn snapshot_excludes_accepted_history_but_includes_current_plan_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let mut session = Session::new(workspace.path(), "test".to_string(), None);
        session
            .execution_receipts
            .push(write_receipt("old.rs", "accepted"));
        session
            .execution_receipts
            .push(write_receipt("pending.rs", "executed"));
        let snapshot = capture_command_snapshot(&session);
        assert!(snapshot.complete);
        assert!(!snapshot.hashes.contains_key("old.rs"));
        assert_eq!(snapshot.hashes.get("pending.rs"), Some(&None));
        let mut requirement = requirement();
        requirement.scope = VerificationScopeV1::WorkspacePaths {
            relative_paths: vec!["old.rs".to_string()],
        };
        session.single_verification_plan = Some(VerificationPlanV1 {
            plan_id: "current".to_string(),
            requirements: vec![requirement],
        });
        let snapshot = capture_command_snapshot(&session);
        assert!(snapshot.hashes.contains_key("old.rs"));
    }

    #[test]
    fn snapshot_is_bounded_and_refuses_path_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let mut session = Session::new(workspace.path(), "test".to_string(), None);
        for index in 0..=MAX_SNAPSHOT_PATHS {
            session
                .execution_receipts
                .push(write_receipt(&format!("src/{index}.rs"), "executed"));
        }
        let snapshot = capture_command_snapshot(&session);
        assert!(!snapshot.complete);
        assert_eq!(snapshot.hashes.len(), MAX_SNAPSHOT_PATHS);
        assert!(read_workspace_path(workspace.path(), "../outside.rs").is_none());
        let path = workspace.path().join("large.rs");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(read_workspace_path(workspace.path(), "large.rs").is_none());
    }
    #[test]
    fn shell_navigation_does_not_capture_source_but_behavior_commands_do() {
        let workspace = tempfile::tempdir().unwrap();
        let mut session = Session::new(workspace.path(), "test".to_string(), None);
        session
            .execution_receipts
            .push(write_receipt("missing.rs", "executed"));
        assert!(capture_for_tool(
            &session,
            "run_command",
            &serde_json::json!({"command":"git status"})
        )
        .is_none());
        assert!(capture_for_tool(
            &session,
            "run_command",
            &serde_json::json!({"command":"cargo test --no-run"})
        )
        .is_none());
        assert!(capture_for_tool(
            &session,
            "read_file",
            &serde_json::json!({"command":"cargo test"})
        )
        .is_none());
        assert!(
            capture_for_tool(
                &session,
                "run_command",
                &serde_json::json!({"command":"cargo test"})
            )
            .unwrap()
            .complete
        );
    }
}
