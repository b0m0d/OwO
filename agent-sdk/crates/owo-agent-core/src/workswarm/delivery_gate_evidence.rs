use super::*;

pub(super) fn is_code_artifact_kind(kind: &str) -> bool {
    matches!(
        kind.trim().to_ascii_lowercase().as_str(),
        "code" | "source" | "patch" | "implementation" | "frontend" | "backend" | "integrated"
    )
}

pub(super) fn validate_review_artifact_kind(is_reviewer: bool, kind: &str) -> Result<(), String> {
    match (is_reviewer, kind == "review") {
        (true, true) | (false, false) => Ok(()),
        (true, false) => Err("review capability 必须提交 review 类型的结构化评审产物".to_string()),
        (false, true) => Err("只有独立 reviewer 才能提交 review 类型产物".to_string()),
    }
}

pub(super) fn validate_review_approval(result: &Value) -> Result<(), String> {
    let result: owo_agent_workswarm::WorkerReviewResultV1 = serde_json::from_value(result.clone())
        .map_err(|error| format!("ReviewResult 结构非法：{error}"))?;
    if result.verdict != owo_agent_workswarm::WorkerReviewVerdict::Approved {
        return Err("ReviewResult 未批准交付，需先完成 Issue 修复与复验".to_string());
    }
    for finding in &result.findings {
        if finding.detail.trim().is_empty() {
            return Err("ReviewResult 存在缺少 detail 的 finding".to_string());
        }
    }
    if result
        .findings
        .iter()
        .any(|finding| finding.severity == owo_agent_workswarm::WorkerReviewSeverity::Blocker)
    {
        return Err("ReviewResult 含 blocker finding，不能批准交付".to_string());
    }
    Ok(())
}

pub(super) fn is_user_delivery_artifact(kind: &str) -> bool {
    kind != "review"
}

pub(super) fn changeset_delivery_error(
    status: owo_agent_protocol::ChangeSetStatus,
) -> Option<&'static str> {
    match status {
        owo_agent_protocol::ChangeSetStatus::Accepted => None,
        owo_agent_protocol::ChangeSetStatus::PendingReview => Some("仍待人工接受"),
        owo_agent_protocol::ChangeSetStatus::Conflicted => Some("存在未解决冲突"),
        owo_agent_protocol::ChangeSetStatus::Rejected => Some("已被拒绝"),
        owo_agent_protocol::ChangeSetStatus::Reverted => Some("已撤销"),
    }
}

pub(super) fn collect_attempt_changeset_evidence(
    team_id: &str,
    step_id: &str,
    attempt_id: &str,
    change_sets: &[owo_agent_protocol::ChangeSet],
) -> WorkSwarmResult<(Option<String>, Vec<String>)> {
    let mut matching: Vec<_> = change_sets
        .iter()
        .filter(|change_set| {
            change_set.step_id == step_id && change_set.attempt_id.as_deref() == Some(attempt_id)
        })
        .collect();
    for change_set in &matching {
        if change_set.team_id != team_id {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {step_id} 的 ChangeSet {} 团队归属不匹配",
                change_set.change_set_id
            )));
        }
        if let Some(reason) = changeset_delivery_error(change_set.status) {
            let detail = format!(
                "任务 {step_id} 的 ChangeSet {} {}，不能进入已验收交付",
                change_set.change_set_id, reason
            );
            return Err(match change_set.status {
                owo_agent_protocol::ChangeSetStatus::PendingReview
                | owo_agent_protocol::ChangeSetStatus::Conflicted => {
                    WorkSwarmError::DeliveryPending(detail)
                }
                _ => WorkSwarmError::Conflict(detail),
            });
        }
        if change_set.status == owo_agent_protocol::ChangeSetStatus::Accepted
            && change_set
                .decision
                .as_ref()
                .map(|decision| decision.action.as_str())
                != Some("accept")
        {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {step_id} 的 ChangeSet {} 缺少宿主接受决定",
                change_set.change_set_id
            )));
        }
    }
    matching.sort_by(|left, right| left.change_set_id.cmp(&right.change_set_id));
    let snapshot: Vec<_> = matching
        .iter()
        .map(|change_set| {
            json!({
                "change_set_id": &change_set.change_set_id,
                "team_id": &change_set.team_id,
                "step_id": &change_set.step_id,
                "attempt_id": &change_set.attempt_id,
                "role": &change_set.role,
                "base_hashes": &change_set.base_hashes,
                "result_hashes": &change_set.result_hashes,
                "changed_files": &change_set.changed_files,
                "diff_ref": &change_set.diff_ref,
                "status": &change_set.status,
                "decision": &change_set.decision,
            })
        })
        .collect();
    let digest = if snapshot.is_empty() {
        None
    } else {
        let bytes = serde_json::to_vec(&snapshot)
            .map_err(|error| WorkSwarmError::Serialization(error.to_string()))?;
        Some(CasStore::hash_of(&bytes))
    };
    let refs = matching
        .iter()
        .map(|change_set| format!("changeset://{}", change_set.change_set_id))
        .collect();
    Ok((digest, refs))
}

/// Ensure workspace validation evidence for changed files describes the exact
/// accepted ChangeSet result snapshot. This prevents a validator run before a
/// later edit from being reused as proof for different delivered bytes.
pub(super) fn evaluate_workspace_command_receipt(
    requirement: &crate::plan::VerificationRequirementV1,
    event_details: &[String],
    step_id: &str,
    attempt_id: &str,
    change_sets: &[owo_agent_protocol::ChangeSet],
) -> (
    crate::plan::ValidationVerdictV1,
    Option<String>,
    std::collections::BTreeMap<String, String>,
    Option<String>,
) {
    use crate::plan::{ValidationVerdictV1, VerificationScopeV1};

    let unsupported = |detail: String| {
        (ValidationVerdictV1::Unsupported, Some(detail), std::collections::BTreeMap::new(), None)
    };
    let VerificationScopeV1::WorkspacePaths { relative_paths } = &requirement.scope else {
        return unsupported("行为检查要求绑定 WorkspacePaths 文件集合".to_string());
    };
    let Some(command) = requirement.arguments.get("command").and_then(Value::as_str) else {
        return unsupported("行为检查缺少 command 参数".to_string());
    };
    if !crate::verification::is_registered_behavior_command(command) {
        return unsupported("行为检查命令不在宿主登记的测试命令集合内".to_string());
    }
    let command_sha256 = CasStore::hash_of(command.trim().as_bytes());
    let matching = event_details.iter().filter_map(|detail| {
        let event: Value = serde_json::from_str(detail).ok()?;
        if event.get("step_id").and_then(Value::as_str) != Some(step_id)
            || event.get("attempt_id").and_then(Value::as_str) != Some(attempt_id)
        {
            return None;
        }
        let receipt = event.get("receipt")?;
        if receipt.get("command_sha256").and_then(Value::as_str)
            != Some(command_sha256.as_str())
        {
            return None;
        }
        Some(receipt.clone())
    });
    let Some(receipt_value) = matching.last() else {
        return (
            ValidationVerdictV1::Unverified,
            Some("当前 task/attempt 没有匹配的宿主命令执行回执".to_string()),
            std::collections::BTreeMap::new(),
            None,
        );
    };
    let receipt: crate::CommandExecutionReceipt = match serde_json::from_value(receipt_value) {
        Ok(receipt) => receipt,
        Err(error) => return unsupported(format!("宿主命令回执结构无效：{error}")),
    };
    let mut subject_hashes = std::collections::BTreeMap::new();
    for raw_path in relative_paths {
        let path = raw_path.replace('\\', "/");
        let Some(hash) = receipt.workspace_hashes.get(&path) else {
            return (
                ValidationVerdictV1::Unverified,
                Some(format!("命令执行时没有宿主快照证据：{path}")),
                subject_hashes,
                Some(format!("command-result:sha256:{}", receipt.result_sha256)),
            );
        };
        let Some(hash) = hash else {
            return (
                ValidationVerdictV1::Failed,
                Some(format!("命令执行时工作区文件不存在或不可读：{path}")),
                subject_hashes,
                Some(format!("command-result:sha256:{}", receipt.result_sha256)),
            );
        };
        subject_hashes.insert(format!("workspace-path:{raw_path}"), hash.clone());
    }
    if let Err(reason) = validate_workspace_receipt_snapshot(
        step_id,
        attempt_id,
        relative_paths,
        &subject_hashes,
        change_sets,
    ) {
        return (
            ValidationVerdictV1::Failed,
            Some(reason),
            subject_hashes,
            Some(format!("command-result:sha256:{}", receipt.result_sha256)),
        );
    }
    let evidence_ref = Some(format!("command-result:sha256:{}", receipt.result_sha256));
    if receipt.exit_code != 0 {
        return (
            ValidationVerdictV1::Failed,
            Some(format!("宿主命令退出码为 {}", receipt.exit_code)),
            subject_hashes,
            evidence_ref,
        );
    }
    (
        ValidationVerdictV1::Passed,
        Some(format!("宿主登记命令成功，exit_code=0 command_sha256={command_sha256}")),
        subject_hashes,
        evidence_ref,
    )
}

pub(super) fn validate_workspace_receipt_snapshot(
    step_id: &str,
    attempt_id: &str,
    relative_paths: &[String],
    subject_hashes: &std::collections::BTreeMap<String, String>,
    change_sets: &[owo_agent_protocol::ChangeSet],
) -> Result<(), String> {
    let matching: Vec<_> = change_sets
        .iter()
        .filter(|change_set| {
            change_set.step_id == step_id && change_set.attempt_id.as_deref() == Some(attempt_id)
        })
        .collect();
    if matching.is_empty() {
        return Ok(());
    }

    let mut expected = std::collections::BTreeMap::new();
    let mut changed = std::collections::BTreeSet::new();
    for change_set in matching {
        changed.extend(change_set.changed_files.iter().map(|path| path.replace('\\', "/")));
        for file in &change_set.result_hashes {
            let path = file.path.replace('\\', "/");
            if expected
                .insert(path.clone(), file.sha256.clone())
                .is_some_and(|previous| previous != file.sha256)
            {
                return Err(format!("ChangeSet 对同一文件 {path} 包含冲突结果哈希"));
            }
        }
    }
    for raw_path in relative_paths {
        let path = raw_path.replace('\\', "/");
        if changed.contains(&path) && !expected.contains_key(&path) {
            return Err(format!("ChangeSet 缺少已变更文件 {path} 的结果哈希"));
        }
        let Some(expected_hash) = expected.get(&path) else {
            continue;
        };
        let evidence_key = format!("workspace-path:{raw_path}");
        let actual_hash = subject_hashes.get(&evidence_key).ok_or_else(|| {
            format!("workspace 验证未为 ChangeSet 文件 {path} 产生最终源码哈希")
        })?;
        if expected_hash.as_deref() != Some(actual_hash.as_str()) {
            return Err(format!(
                "workspace 验证哈希与接受的 ChangeSet 快照不一致：{path}"
            ));
        }
    }
    Ok(())
}

pub(super) fn store_validation_receipt(
    state: &mut GoalRunState,
    step_id: &str,
    receipt: &crate::plan::ValidationReceiptV1,
) {
    let Some(record) = state.records.get_mut(step_id) else {
        return;
    };
    if let Some(existing) = record
        .validation_receipts
        .iter_mut()
        .find(|existing| existing.receipt_id == receipt.receipt_id)
    {
        *existing = receipt.clone();
    } else {
        record.validation_receipts.push(receipt.clone());
    }
}

pub(super) struct ValidationReceiptInput<'a> {
    pub(super) team_id: &'a str,
    pub(super) step_id: &'a str,
    pub(super) attempts: u32,
    pub(super) attempt_id: &'a str,
    pub(super) epoch: u64,
    pub(super) requirement_id: &'a str,
    pub(super) scope: &'a crate::plan::VerificationScopeV1,
    pub(super) validator_id: &'a str,
    pub(super) validator_version: &'a str,
    pub(super) arguments_sha256: &'a str,
    pub(super) input_sha256: &'a str,
    pub(super) artifact_id: &'a str,
    pub(super) artifact_sha256: &'a str,
    pub(super) changeset_sha256: Option<String>,
    pub(super) changeset_refs: Vec<String>,
    pub(super) evidence_ref: &'a str,
    pub(super) started_at: &'a str,
    pub(super) verdict: crate::plan::ValidationVerdictV1,
    pub(super) detail: Option<String>,
    pub(super) subject_hashes: std::collections::BTreeMap<String, String>,
    pub(super) additional_evidence_refs: Vec<String>,
}

pub(super) fn make_validation_receipt(
    input: ValidationReceiptInput<'_>,
) -> crate::plan::ValidationReceiptV1 {
    let receipt_identity = json!({
        "team_id": input.team_id,
        "step_id": input.step_id,
        "attempts": input.attempts,
        "attempt_id": input.attempt_id,
        "epoch": input.epoch,
        "requirement_id": input.requirement_id,
        "scope": input.scope,
        "validator_id": input.validator_id,
        "validator_version": input.validator_version,
        "arguments_sha256": input.arguments_sha256,
        "input_sha256": input.input_sha256,
        "artifact_id": input.artifact_id,
        "artifact_sha256": input.artifact_sha256,
        "changeset_sha256": input.changeset_sha256,
        "changeset_refs": input.changeset_refs,
        "evidence_ref": input.evidence_ref,
        "subject_hashes": &input.subject_hashes,
        "additional_evidence_refs": &input.additional_evidence_refs,
    })
    .to_string();
    let mut subject_sha256 = std::collections::HashMap::from([(
        input.artifact_id.to_string(),
        input.artifact_sha256.to_string(),
    )]);
    subject_sha256.extend(input.subject_hashes);
    let evidence_refs = std::iter::once(input.evidence_ref.to_string())
        .chain(input.changeset_refs)
        .chain(input.additional_evidence_refs)
        .collect();
    crate::plan::ValidationReceiptV1 {
        receipt_id: CasStore::hash_of(receipt_identity.as_bytes()),
        task_id: input.step_id.to_string(),
        attempt_id: input.attempt_id.to_string(),
        epoch: input.epoch,
        requirement_id: input.requirement_id.to_string(),
        validator_id: input.validator_id.to_string(),
        validator_version: input.validator_version.to_string(),
        arguments_sha256: input.arguments_sha256.to_string(),
        input_sha256: input.input_sha256.to_string(),
        environment_id: format!("owo-agent-core/{}", env!("CARGO_PKG_VERSION")),
        changeset_sha256: input.changeset_sha256,
        detail: input.detail,
        subject_sha256,
        verdict: input.verdict,
        evidence_refs,
        started_at: input.started_at.to_string(),
        completed_at: now_ts(),
    }
}
