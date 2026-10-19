//! Explicit host-mediated acceptance for candidates without a runnable behavior check.

use crate::session::Session;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use super::{single_workspace_path_matches, wait_for_abort};

pub(super) const SINGLE_MANUAL_ACCEPT_OPTION: &str = "验收当前候选版本";
pub(super) const SINGLE_MANUAL_REJECT_OPTION: &str = "暂不验收";

pub(super) fn completion_notice(session: &Session, turn_id: &str) -> Option<String> {
    let accepted_requirements = session
        .validation_receipts
        .iter()
        .filter(|receipt| {
            receipt.attempt_id == turn_id
                && receipt.validator_id == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
                && receipt.verdict == crate::plan::ValidationVerdictV1::ManualAccepted
        })
        .map(|receipt| receipt.requirement_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if accepted_requirements.is_empty() {
        return None;
    }
    Some(format!(
        "\n\n验收说明：人工验收要求已由用户针对当前候选快照明确接受（{}）；这不等同于自动行为测试通过。",
        accepted_requirements.into_iter().collect::<Vec<_>>().join(", ")
    ))
}

pub(super) fn manual_acceptance_answer_verdict(
    answer: Option<&crate::question::QuestionAnswer>,
    question_id: &str,
) -> crate::plan::ValidationVerdictV1 {
    match answer {
        Some(answer)
            if answer.question_id == question_id
                && answer.answer == SINGLE_MANUAL_ACCEPT_OPTION =>
        {
            crate::plan::ValidationVerdictV1::ManualAccepted
        }
        Some(answer)
            if answer.question_id == question_id
                && answer.answer == SINGLE_MANUAL_REJECT_OPTION =>
        {
            crate::plan::ValidationVerdictV1::Failed
        }
        _ => crate::plan::ValidationVerdictV1::Unverified,
    }
}

fn single_candidate_hashes(
    session: &Session,
    turn_id: &str,
) -> std::collections::BTreeMap<String, Option<String>> {
    let mut pending_hashes = std::collections::BTreeMap::new();
    for execution in session.execution_receipts.iter().filter(|execution| {
        execution.status == "executed"
            || (execution.turn_id == turn_id && execution.status == "accepted")
    }) {
        for relative in &execution.changed_files {
            let normalized = relative.replace('\\', "/");
            if let Some((_, hash)) = execution
                .after_hashes
                .iter()
                .find(|(path, _)| path.replace('\\', "/") == normalized)
            {
                pending_hashes.insert(normalized, hash.clone());
            }
        }
    }
    pending_hashes
}

fn current_candidate_changeset_sha256(session: &Session, turn_id: &str) -> Option<String> {
    let pending_hashes = single_candidate_hashes(session, turn_id);
    if pending_hashes.is_empty() {
        return None;
    }
    serde_json::to_vec(&pending_hashes)
        .ok()
        .map(|bytes| crate::CasStore::hash_of(&bytes))
}

fn verification_plan_sha256(plan: &crate::plan::VerificationPlanV1) -> String {
    serde_json::to_vec(plan)
        .map(|bytes| crate::CasStore::hash_of(&bytes))
        .unwrap_or_default()
}

fn current_plan_receipt<'a>(
    session: &'a Session,
    plan: &crate::plan::VerificationPlanV1,
    turn_id: &str,
    requirement: &crate::plan::VerificationRequirementV1,
) -> Option<&'a crate::plan::ValidationReceiptV1> {
    if session.single_verification_plan.as_ref() != Some(plan) {
        return None;
    }
    let input_sha256 = session.single_verification_plan_input_sha256.as_deref()?;
    let changeset_sha256 = current_candidate_changeset_sha256(session, turn_id)?;
    let environment_id = crate::CasStore::hash_of(session.workspace.to_string_lossy().as_bytes());
    let validator_version = requirement
        .validator_version
        .as_deref()
        .unwrap_or("unknown");
    let arguments_sha256 = crate::CasStore::hash_of(requirement.arguments.to_string().as_bytes());
    let plan_sha256 = verification_plan_sha256(plan);
    session.validation_receipts.iter().rev().find(|receipt| {
        receipt.task_id == session.id
            && receipt.attempt_id == turn_id
            && receipt.requirement_id == requirement.requirement_id
            && receipt.validator_id == requirement.validator_id
            && receipt.validator_version == validator_version
            && receipt.arguments_sha256 == arguments_sha256
            && receipt.input_sha256 == input_sha256
            && receipt.environment_id == environment_id
            && receipt.changeset_sha256.as_deref() == Some(changeset_sha256.as_str())
            && receipt
                .evidence_refs
                .iter()
                .any(|reference| reference == &format!("verification-plan-sha256:{plan_sha256}"))
    })
}

fn current_coverage_receipt<'a>(
    session: &'a Session,
    plan: &crate::plan::VerificationPlanV1,
    turn_id: &str,
) -> Option<&'a crate::plan::ValidationReceiptV1> {
    if session.single_verification_plan.as_ref() != Some(plan) {
        return None;
    }
    let input_sha256 = session.single_verification_plan_input_sha256.as_deref()?;
    let changeset_sha256 = current_candidate_changeset_sha256(session, turn_id)?;
    let environment_id = crate::CasStore::hash_of(session.workspace.to_string_lossy().as_bytes());
    let arguments_sha256 = crate::CasStore::hash_of(plan.plan_id.as_bytes());
    let plan_sha256 = verification_plan_sha256(plan);
    session.validation_receipts.iter().rev().find(|receipt| {
        receipt.task_id == session.id
            && receipt.attempt_id == turn_id
            && receipt.requirement_id == "host-change-scope-coverage"
            && receipt.validator_id == "host-change-scope-coverage-v1"
            && receipt.validator_version == "1"
            && receipt.arguments_sha256 == arguments_sha256
            && receipt.input_sha256 == input_sha256
            && receipt.environment_id == environment_id
            && receipt.changeset_sha256.as_deref() == Some(changeset_sha256.as_str())
            && receipt
                .evidence_refs
                .iter()
                .any(|reference| reference == &format!("verification-plan-sha256:{plan_sha256}"))
    })
}

fn completion_from_single_plan_receipts(
    session: &Session,
    plan: &crate::plan::VerificationPlanV1,
    turn_id: &str,
) -> owo_agent_protocol::CompletionStatusV1 {
    use crate::plan::ValidationVerdictV1;
    if current_candidate_changeset_sha256(session, turn_id).is_none() {
        return owo_agent_protocol::CompletionStatusV1::Unverified;
    }
    let mut required_count = 0usize;
    let mut passed_count = 0usize;
    let mut failed_count = 0usize;
    let mut stale_evidence = false;

    for requirement in plan
        .requirements
        .iter()
        .filter(|requirement| requirement.required)
    {
        required_count += 1;
        match current_plan_receipt(session, plan, turn_id, requirement)
            .map(|receipt| receipt.verdict)
        {
            Some(ValidationVerdictV1::Passed | ValidationVerdictV1::ManualAccepted) => {
                passed_count += 1
            }
            Some(ValidationVerdictV1::Failed) => failed_count += 1,
            Some(ValidationVerdictV1::Stale) => stale_evidence = true,
            _ => {}
        }
    }

    required_count += 1;
    match current_coverage_receipt(session, plan, turn_id).map(|receipt| receipt.verdict) {
        Some(ValidationVerdictV1::Passed) => passed_count += 1,
        Some(ValidationVerdictV1::Failed) => failed_count += 1,
        Some(ValidationVerdictV1::Stale) => stale_evidence = true,
        _ => {}
    }

    crate::completion::decide_completion(crate::completion::CompletionEvidence {
        response_finished: true,
        has_candidate_changes: true,
        required_validation_count: required_count,
        passed_required_validation_count: passed_count,
        failed_required_validation_count: failed_count,
        stale_evidence,
        ..crate::completion::CompletionEvidence::default()
    })
}

pub(super) async fn request_single_manual_acceptance(
    session: &mut Session,
    plan: &crate::plan::VerificationPlanV1,
    turn_id: &str,
    questioner: Option<&dyn crate::question::Questioner>,
    abort: &AtomicBool,
) -> owo_agent_protocol::CompletionStatusV1 {
    use crate::plan::{ValidationVerdictV1, VerificationScopeV1};
    use std::collections::{BTreeMap, BTreeSet};

    let manual_requirements = plan
        .requirements
        .iter()
        .filter(|requirement| {
            requirement.required
                && requirement.validator_id
                    == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
                && requirement.validator_version.as_deref() == Some("1")
                && matches!(&requirement.scope, VerificationScopeV1::Manual)
                && requirement.arguments == serde_json::json!({})
        })
        .collect::<Vec<_>>();
    if manual_requirements.is_empty() {
        return owo_agent_protocol::CompletionStatusV1::Unverified;
    }

    let required_manual_ids = manual_requirements
        .iter()
        .map(|requirement| requirement.requirement_id.as_str())
        .collect::<BTreeSet<_>>();
    let other_requirements_passed = plan
        .requirements
        .iter()
        .filter(|requirement| {
            requirement.required
                && requirement.validator_id
                    != crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
        })
        .all(|requirement| {
            current_plan_receipt(session, plan, turn_id, requirement)
                .is_some_and(|receipt| receipt.verdict == ValidationVerdictV1::Passed)
        });
    let coverage_passed = current_coverage_receipt(session, plan, turn_id)
        .is_some_and(|receipt| receipt.verdict == ValidationVerdictV1::Passed);
    if !other_requirements_passed || !coverage_passed {
        return completion_from_single_plan_receipts(session, plan, turn_id);
    }

    let pending_hashes = single_candidate_hashes(session, turn_id);
    if pending_hashes.is_empty() {
        return completion_from_single_plan_receipts(session, plan, turn_id);
    }

    let snapshot_subjects = pending_hashes
        .iter()
        .map(|(path, hash)| {
            (
                format!("workspace-path:{path}"),
                hash.clone()
                    .unwrap_or_else(crate::verification::workspace_path_absence_sha256),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let candidate_version_sha256 = match crate::completion::hash_candidate_version(&pending_hashes)
    {
        Ok(hash) => hash,
        Err(_) => return owo_agent_protocol::CompletionStatusV1::Unverified,
    };
    let changeset_sha256 = match serde_json::to_vec(&pending_hashes) {
        Ok(bytes) => crate::CasStore::hash_of(&bytes),
        Err(_) => return owo_agent_protocol::CompletionStatusV1::Unverified,
    };
    let Some(root) = session.workspace.canonicalize().ok() else {
        return owo_agent_protocol::CompletionStatusV1::Unverified;
    };
    let snapshot_matches = || {
        pending_hashes.iter().all(|(path, hash)| {
            let expected = hash
                .clone()
                .unwrap_or_else(crate::verification::workspace_path_absence_sha256);
            single_workspace_path_matches(&root, path, &expected)
        })
    };
    if !snapshot_matches() {
        for receipt in session.validation_receipts.iter_mut().filter(|receipt| {
            receipt.attempt_id == turn_id
                && required_manual_ids.contains(receipt.requirement_id.as_str())
                && receipt.changeset_sha256.as_deref() == Some(changeset_sha256.as_str())
        }) {
            receipt.verdict = ValidationVerdictV1::Stale;
            receipt.detail = Some("等待人工验收前候选文件已变化，人工验收未执行".to_string());
            receipt.completed_at = chrono::Utc::now().to_rfc3339();
        }
        return completion_from_single_plan_receipts(session, plan, turn_id);
    }

    let plan_evidence_ref = format!(
        "verification-plan-sha256:{}",
        verification_plan_sha256(plan)
    );
    let prior_decision = session
        .validation_receipts
        .iter()
        .rev()
        .find(|receipt| {
            receipt.attempt_id == turn_id
                && receipt.validator_id == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
                && receipt.changeset_sha256.as_deref() == Some(changeset_sha256.as_str())
                && receipt
                    .evidence_refs
                    .iter()
                    .any(|reference| reference == &plan_evidence_ref)
                && receipt.evidence_refs.iter().any(|reference| {
                    reference.starts_with("manual-question:")
                        || reference == "manual-question-unavailable"
                })
                && matches!(
                    receipt.verdict,
                    ValidationVerdictV1::ManualAccepted
                        | ValidationVerdictV1::Failed
                        | ValidationVerdictV1::Unverified
                )
        })
        .cloned();
    let reused_prior_acceptance = prior_decision
        .as_ref()
        .is_some_and(|prior| prior.verdict == ValidationVerdictV1::ManualAccepted);
    if let Some(prior) = prior_decision {
        for receipt in session.validation_receipts.iter_mut().filter(|receipt| {
            receipt.attempt_id == turn_id
                && required_manual_ids.contains(receipt.requirement_id.as_str())
                && receipt.changeset_sha256.as_deref() == Some(changeset_sha256.as_str())
                && receipt.verdict == ValidationVerdictV1::Unverified
        }) {
            receipt.verdict = prior.verdict;
            receipt.detail = prior.detail.clone();
            receipt.subject_sha256 = prior.subject_sha256.clone();
            receipt.evidence_refs = prior.evidence_refs.clone();
            receipt.completed_at = chrono::Utc::now().to_rfc3339();
        }
        if !reused_prior_acceptance {
            return completion_from_single_plan_receipts(session, plan, turn_id);
        }
    }

    let quote_lines = manual_requirements
        .iter()
        .flat_map(|requirement| {
            requirement
                .covers_requirement_ids
                .iter()
                .filter_map(|id| id.strip_prefix("user-request:"))
        })
        .map(|quote| format!("- {quote}"))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let file_lines = pending_hashes
        .iter()
        .map(|(path, hash)| {
            let hash = hash.as_deref().unwrap_or("deleted");
            format!("- {path}  sha256:{hash}")
        })
        .collect::<Vec<_>>();
    let question_id = format!("single-manual-accept-{}", uuid::Uuid::new_v4());
    let question = crate::question::UserQuestion {
        question_id: question_id.clone(),
        question: format!(
            "请检查当前工作区中的候选版本，并确认是否满足这些用户验收点。候选快照 SHA-256：{candidate_version_sha256}\n用户验收点：\n{}\n变更文件：\n{}\n只有当前列出的文件内容与哈希完全可接受时，才选择验收。",
            quote_lines.join("\n"),
            file_lines.join("\n")
        ),
        options: vec![SINGLE_MANUAL_ACCEPT_OPTION.to_string(), SINGLE_MANUAL_REJECT_OPTION.to_string()],
    };
    let started_at = chrono::Utc::now().to_rfc3339();
    let answer = if reused_prior_acceptance {
        None
    } else if let Some(questioner) = questioner {
        tokio::select! {
            biased;
            _ = wait_for_abort(abort) => None,
            answer = questioner.ask(&question) => answer,
        }
    } else {
        None
    };
    let mut verdict = if reused_prior_acceptance {
        ValidationVerdictV1::ManualAccepted
    } else {
        manual_acceptance_answer_verdict(answer.as_ref(), &question_id)
    };
    if verdict == ValidationVerdictV1::ManualAccepted && !snapshot_matches() {
        verdict = ValidationVerdictV1::Stale;
    }
    let completed_at = chrono::Utc::now().to_rfc3339();
    let mut manual_receipt_ids = BTreeMap::new();
    for requirement in &manual_requirements {
        let existing = session.validation_receipts.iter().rposition(|receipt| {
            receipt.attempt_id == turn_id
                && receipt.requirement_id == requirement.requirement_id
                && receipt.validator_id == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
                && receipt.changeset_sha256.as_deref() == Some(changeset_sha256.as_str())
                && (receipt.verdict == ValidationVerdictV1::Unverified
                    || (reused_prior_acceptance
                        && receipt.verdict == ValidationVerdictV1::ManualAccepted))
        });
        let index = if let Some(index) = existing {
            index
        } else {
            let input_sha256 = session
                .single_verification_plan_input_sha256
                .clone()
                .unwrap_or_default();
            session
                .validation_receipts
                .push(crate::plan::ValidationReceiptV1 {
                    receipt_id: format!("single-manual-validation-{}", uuid::Uuid::new_v4()),
                    task_id: session.id.clone(),
                    attempt_id: turn_id.to_string(),
                    epoch: session.validation_receipts.len() as u64 + 1,
                    requirement_id: requirement.requirement_id.clone(),
                    validator_id: crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
                        .to_string(),
                    validator_version: "1".to_string(),
                    arguments_sha256: crate::CasStore::hash_of(b"{}"),
                    input_sha256,
                    environment_id: crate::CasStore::hash_of(
                        session.workspace.to_string_lossy().as_bytes(),
                    ),
                    changeset_sha256: Some(changeset_sha256.clone()),
                    detail: None,
                    subject_sha256: HashMap::new(),
                    verdict: ValidationVerdictV1::Unverified,
                    evidence_refs: Vec::new(),
                    review_result: None,
                    started_at: started_at.clone(),
                    completed_at: started_at.clone(),
                });
            session.validation_receipts.len() - 1
        };
        let receipt = &mut session.validation_receipts[index];
        receipt.verdict = verdict;
        receipt.changeset_sha256 = Some(changeset_sha256.clone());
        receipt.subject_sha256 = snapshot_subjects.clone();
        receipt.detail = Some(match verdict {
            ValidationVerdictV1::ManualAccepted => {
                "用户明确验收了与该回执绑定的候选快照".to_string()
            }
            ValidationVerdictV1::Failed => "用户明确拒绝验收当前候选快照".to_string(),
            ValidationVerdictV1::Stale => "等待用户答复期间候选文件发生变化".to_string(),
            _ => "没有取得与候选快照匹配的明确用户验收答复".to_string(),
        });
        if !reused_prior_acceptance {
            receipt.evidence_refs = if questioner.is_some() {
                vec![format!("manual-question:{question_id}")]
            } else {
                vec!["manual-question-unavailable".to_string()]
            };
            if let Some(answer_hash) = answer
                .as_ref()
                .map(|answer| crate::CasStore::hash_of(answer.answer.as_bytes()))
            {
                receipt
                    .evidence_refs
                    .push(format!("user-answer-sha256:{answer_hash}"));
            }
        }
        receipt.evidence_refs.push(format!(
            "verification-plan-sha256:{}",
            verification_plan_sha256(plan)
        ));
        receipt.evidence_refs.push(format!(
            "candidate-version-sha256:{candidate_version_sha256}"
        ));
        receipt.completed_at = completed_at.clone();
        manual_receipt_ids.insert(
            requirement.requirement_id.as_str(),
            receipt.receipt_id.clone(),
        );
    }

    let status = completion_from_single_plan_receipts(session, plan, turn_id);
    if status == owo_agent_protocol::CompletionStatusV1::Accepted {
        let accepted_receipt_id = manual_requirements
            .first()
            .and_then(|requirement| manual_receipt_ids.get(requirement.requirement_id.as_str()))
            .cloned()
            .unwrap_or_default();
        let mut latest_receipt_by_path = BTreeMap::new();
        for (index, execution) in session.execution_receipts.iter().enumerate() {
            for path in &execution.changed_files {
                latest_receipt_by_path.insert(path.replace('\\', "/"), index);
            }
        }
        for (index, execution) in session.execution_receipts.iter_mut().enumerate() {
            if execution.status != "executed" {
                continue;
            }
            let paths = execution
                .changed_files
                .iter()
                .map(|path| path.replace('\\', "/"))
                .collect::<Vec<_>>();
            if !paths.is_empty()
                && paths.iter().all(|path| {
                    pending_hashes.contains_key(path)
                        && latest_receipt_by_path.get(path) == Some(&index)
                })
            {
                execution.status = "accepted".to_string();
                execution.validation_receipt_id = Some(accepted_receipt_id.clone());
            }
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        ValidationReceiptV1, ValidationVerdictV1, VerificationPlanV1, VerificationRequirementV1,
        VerificationResourcesV1, VerificationScopeV1,
    };
    use crate::session::{ExecutionReceipt, Session};
    use std::collections::HashMap;

    #[test]
    fn completion_ignores_receipt_from_another_validator_for_same_requirement_id() {
        let workspace = tempfile::tempdir().unwrap();
        let mut session = Session::new(workspace.path(), "mock", None);
        let turn_id = "turn-bound-receipt";
        let input_sha256 = crate::CasStore::hash_of(b"implement behavior");
        let requirement = VerificationRequirementV1 {
            requirement_id: "user-behavior".to_string(),
            covers_requirement_ids: vec!["user-request:implement behavior".to_string()],
            validator_id: "workspace-file-exists-v1".to_string(),
            validator_version: Some("1".to_string()),
            scope: VerificationScopeV1::WorkspacePaths {
                relative_paths: vec!["src/lib.rs".to_string()],
            },
            arguments: serde_json::json!({}),
            required: true,
            resources: VerificationResourcesV1::default(),
        };
        let plan = VerificationPlanV1 {
            plan_id: "bound-plan".to_string(),
            requirements: vec![requirement.clone()],
        };
        session.single_verification_plan = Some(plan.clone());
        session.single_verification_plan_input_sha256 = Some(input_sha256.clone());
        session.single_verification_plan_turn_id = Some(turn_id.to_string());
        session.execution_receipts.push(ExecutionReceipt {
            receipt_id: "write-1".to_string(),
            tool: "write_file".to_string(),
            turn_id: turn_id.to_string(),
            changed_files: vec!["src/lib.rs".to_string()],
            snapshot_keys: HashMap::new(),
            before_hashes: HashMap::from([("src/lib.rs".to_string(), None)]),
            after_hashes: HashMap::from([(
                "src/lib.rs".to_string(),
                Some("source-hash".to_string()),
            )]),
            diff_sha256: "diff-hash".to_string(),
            created_at: "2026-10-04T00:00:00Z".to_string(),
            status: "executed".to_string(),
            validation_receipt_id: None,
        });
        let changeset_sha256 = current_candidate_changeset_sha256(&session, turn_id).unwrap();
        let environment_id =
            crate::CasStore::hash_of(session.workspace.to_string_lossy().as_bytes());
        let plan_ref = format!(
            "verification-plan-sha256:{}",
            verification_plan_sha256(&plan)
        );
        session.validation_receipts.push(ValidationReceiptV1 {
            receipt_id: "wrong-validator".to_string(),
            task_id: session.id.clone(),
            attempt_id: turn_id.to_string(),
            epoch: 1,
            requirement_id: requirement.requirement_id.clone(),
            validator_id: "different-validator-v1".to_string(),
            validator_version: "1".to_string(),
            arguments_sha256: crate::CasStore::hash_of(
                requirement.arguments.to_string().as_bytes(),
            ),
            input_sha256,
            environment_id: environment_id.clone(),
            changeset_sha256: Some(changeset_sha256.clone()),
            detail: None,
            subject_sha256: HashMap::from([(
                "workspace-path:src/lib.rs".to_string(),
                "source-hash".to_string(),
            )]),
            verdict: ValidationVerdictV1::Passed,
            evidence_refs: vec![plan_ref.clone()],
            review_result: None,
            started_at: "2026-10-04T00:00:00Z".to_string(),
            completed_at: "2026-10-04T00:00:01Z".to_string(),
        });
        session.validation_receipts.push(ValidationReceiptV1 {
            receipt_id: "coverage-passed".to_string(),
            task_id: session.id.clone(),
            attempt_id: turn_id.to_string(),
            epoch: 1,
            requirement_id: "host-change-scope-coverage".to_string(),
            validator_id: "host-change-scope-coverage-v1".to_string(),
            validator_version: "1".to_string(),
            arguments_sha256: crate::CasStore::hash_of(plan.plan_id.as_bytes()),
            input_sha256: crate::CasStore::hash_of(b"implement behavior"),
            environment_id,
            changeset_sha256: Some(changeset_sha256),
            detail: None,
            subject_sha256: HashMap::from([(
                "workspace-path:src/lib.rs".to_string(),
                "source-hash".to_string(),
            )]),
            verdict: ValidationVerdictV1::Passed,
            evidence_refs: vec![plan_ref],
            review_result: None,
            started_at: "2026-10-04T00:00:00Z".to_string(),
            completed_at: "2026-10-04T00:00:01Z".to_string(),
        });

        assert!(current_plan_receipt(&session, &plan, turn_id, &requirement).is_none());
        assert_eq!(
            completion_from_single_plan_receipts(&session, &plan, turn_id),
            owo_agent_protocol::CompletionStatusV1::Unverified
        );
    }
}
