//! Verification batch regressions. Source declarations only until Rust tests are run.
use super::*;
use crate::plan::{VerificationResourcesV1, VerificationScopeV1};
use crate::workspace_snapshot::WorkspaceSnapshotBatch;
use serde_json::json;

fn requirement(id: &str, paths: &[&str], arguments: Value) -> VerificationRequirementV1 {
    VerificationRequirementV1 {
        requirement_id: format!("req-{id}"),
        covers_requirement_ids: Vec::new(),
        validator_id: id.into(),
        validator_version: Some("1".into()),
        scope: VerificationScopeV1::WorkspacePaths {
            relative_paths: paths.iter().map(|p| p.to_string()).collect(),
        },
        arguments,
        required: true,
        resources: VerificationResourcesV1 {
            cpu_slots: 1,
            memory_mb: 16,
            exclusive_workspace: false,
            timeout_ms: 5000,
        },
    }
}

#[test]
fn requirements_share_exact_file_bytes_but_fresh_fence_rejects_mutation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("result.json"), br#"{"status":"ready"}"#).unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    let contains = requirement(
        "workspace-file-contains-v1",
        &["result.json"],
        json!({"text":"ready"}),
    );
    let (_, _, hashes) = batch.execute_workspace(&contains);
    let initial = batch.file("result.json").unwrap();
    std::fs::write(dir.path().join("result.json"), br#"{"status":"broken"}"#).unwrap();
    let equals = requirement(
        "workspace-json-field-equals-v1",
        &["./result.json"],
        json!({"field":"status","expected":"ready"}),
    );
    assert_eq!(
        batch.execute_workspace(&equals).0,
        ValidationVerdictV1::Passed
    );
    assert!(Arc::ptr_eq(&initial, &batch.file("./result.json").unwrap()));
    assert_eq!(batch.files.len(), 1);
    let subjects = hashes.into_iter().collect();
    assert!(!WorkspaceSnapshotBatch::new(Some(dir.path())).subjects_match(&subjects));
    let mut fresh = WorkspaceValidationBatch::new(Some(dir.path()));
    assert_eq!(
        fresh.execute_workspace(&equals).0,
        ValidationVerdictV1::Failed
    );
}

#[test]
fn repeated_reads_do_not_consume_aggregate_budget_twice() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"123456").unwrap();
    std::fs::write(dir.path().join("b.txt"), b"abcdef").unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    batch.remaining_bytes = 10;
    let exists = requirement("workspace-file-exists-v1", &["a.txt"], json!({}));
    assert_eq!(
        batch.execute_workspace(&exists).0,
        ValidationVerdictV1::Passed
    );
    let nonempty = requirement("workspace-file-non-empty-v1", &["./a.txt"], json!({}));
    assert_eq!(
        batch.execute_workspace(&nonempty).0,
        ValidationVerdictV1::Passed
    );
    assert_eq!(batch.remaining_bytes, 4);
    let second = requirement("workspace-file-exists-v1", &["b.txt"], json!({}));
    assert_eq!(
        batch.execute_workspace(&second).0,
        ValidationVerdictV1::Unverified
    );
    assert_eq!(batch.remaining_bytes, 4);
}

#[test]
fn invalid_scope_version_resources_and_arguments_cannot_read_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    let mut req = requirement("workspace-file-exists-v1", &["../outside"], json!({}));
    assert_eq!(
        batch.execute_workspace(&req).0,
        ValidationVerdictV1::Unsupported
    );
    req.scope = VerificationScopeV1::WorkspacePaths {
        relative_paths: vec!["file".into()],
    };
    req.validator_version = Some("unknown".into());
    assert_eq!(
        batch.execute_workspace(&req).0,
        ValidationVerdictV1::Unsupported
    );
    req.validator_version = Some("1".into());
    req.resources.timeout_ms = 0;
    assert_eq!(
        batch.execute_workspace(&req).0,
        ValidationVerdictV1::Unsupported
    );
    req.resources.timeout_ms = 5000;
    req.arguments = json!({"extra":true});
    assert_eq!(
        batch.execute_workspace(&req).0,
        ValidationVerdictV1::Unsupported
    );
    req.validator_id = "workspace-file-contains-v1".into();
    req.arguments = json!({"text":"x".repeat(2049)});
    assert_eq!(
        batch.execute_workspace(&req).0,
        ValidationVerdictV1::Unsupported
    );
    assert!(batch.files.is_empty());
}

#[test]
fn oversized_and_aggregate_path_limits_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::File::create(dir.path().join("large.bin"))
        .unwrap()
        .set_len(MAX_FILE_BYTES + 1)
        .unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    let req = requirement("workspace-file-exists-v1", &["large.bin"], json!({}));
    assert_eq!(
        batch.execute_workspace(&req).0,
        ValidationVerdictV1::Unsupported
    );
    assert_eq!(batch.remaining_bytes, MAX_SNAPSHOT_BYTES);
    for index in 1..MAX_SNAPSHOT_PATHS {
        let _ = batch.file(&format!("missing-{index}"));
    }
    assert_eq!(batch.files.len(), MAX_SNAPSHOT_PATHS);
    let next = requirement("workspace-file-exists-v1", &["another"], json!({}));
    assert_eq!(
        batch.execute_workspace(&next).0,
        ValidationVerdictV1::Unverified
    );
}

#[test]
fn binary_files_have_valid_identity_without_being_valid_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("binary"), [0xff, 0x00, 0xfe]).unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    let exists = requirement("workspace-file-exists-v1", &["binary"], json!({}));
    let (verdict, _, hashes) = batch.execute_workspace(&exists);
    assert_eq!(verdict, ValidationVerdictV1::Passed);
    assert_eq!(
        hashes["workspace-path:binary"],
        crate::CasStore::hash_of(&[0xff, 0x00, 0xfe])
    );
    let contains = requirement(
        "workspace-file-contains-v1",
        &["binary"],
        json!({"text":"ready"}),
    );
    assert_eq!(
        batch.execute_workspace(&contains).0,
        ValidationVerdictV1::Failed
    );
}

#[test]
fn missing_root_is_unverified_and_output_validation_still_works() {
    let mut batch = WorkspaceValidationBatch::new(None);
    let exists = requirement("workspace-file-exists-v1", &["result"], json!({}));
    assert_eq!(
        batch.execute_registered(&exists, "ready").0,
        ValidationVerdictV1::Unverified
    );
    let mut output = super::super::requirement_for_spec(
        "output",
        &crate::plan::VerificationSpec::OutputContains("ready".into()),
    );
    assert_eq!(
        batch.execute_registered(&output, "service ready").0,
        ValidationVerdictV1::Passed
    );
    output.validator_version = Some("unsupported".into());
    assert_eq!(
        batch.execute_registered(&output, "service ready").0,
        ValidationVerdictV1::Unsupported
    );
}

#[test]
fn absence_and_failure_observations_are_not_silently_refreshed_mid_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    let exists = requirement("workspace-file-exists-v1", &["later"], json!({}));
    assert_eq!(
        batch.execute_workspace(&exists).0,
        ValidationVerdictV1::Failed
    );
    std::fs::write(dir.path().join("later"), b"created").unwrap();
    assert_eq!(
        batch.execute_workspace(&exists).0,
        ValidationVerdictV1::Failed
    );
    assert_eq!(
        WorkspaceValidationBatch::new(Some(dir.path()))
            .execute_workspace(&exists)
            .0,
        ValidationVerdictV1::Passed
    );
}

#[test]
fn behavioral_commands_still_require_host_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let mut batch = WorkspaceValidationBatch::new(Some(dir.path()));
    let command = requirement(
        "workspace-command-success-v1",
        &["source.rs"],
        json!({"command":"cargo test"}),
    );
    assert_eq!(
        batch.execute_workspace(&command).0,
        ValidationVerdictV1::Unsupported
    );
    assert!(batch.files.is_empty());
}
