//! 独立评审问题回执：关闭已修复问题 / 标记返工已派发（从 single_review.rs 拆出）。

use super::*;

pub(in crate::agent) fn apply_review_issue_receipt(
    session: &mut Session,
    turn_id: &str,
    receipt: &ValidationReceiptV1,
) {
    if receipt.validator_id != "workspace-independent-review-v1" || receipt.attempt_id != turn_id {
        return;
    }
    let Some(review) = receipt.review_result.as_ref() else {
        return;
    };
    let reviewed_ids = review
        .get("reviewed_requirement_ids")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    let task_id = session
        .active_task_context
        .as_ref()
        .and_then(|context| context.task_id.as_deref())
        .unwrap_or(session.id.as_str())
        .to_string();
    let review_sha256 = receipt
        .evidence_refs
        .iter()
        .find_map(|reference| reference.strip_prefix("review-result:sha256:"))
        .map(str::to_string)
        .unwrap_or_else(|| {
            crate::CasStore::hash_of(&serde_json::to_vec(review).unwrap_or_default())
        });

    if receipt.verdict == ValidationVerdictV1::Passed {
        let now = chrono::Utc::now().to_rfc3339();
        for issue in &mut session.single_review_issues {
            if issue.target_task_id != task_id
                || issue.target_attempt_id != turn_id
                || issue.status == crate::goal::DeliveryIssueStatusV1::Resolved
                || issue
                    .requirement_id
                    .as_deref()
                    .is_some_and(|requirement| !reviewed_ids.contains(requirement))
            {
                continue;
            }
            issue.status = crate::goal::DeliveryIssueStatusV1::Resolved;
            issue.resolution_review_artifact_id = Some(receipt.receipt_id.clone());
            issue.resolution_review_sha256 = Some(review_sha256.clone());
            issue.resolution_attempt_id = Some(turn_id.to_string());
            issue.updated_at = now.clone();
        }
        return;
    }
    if !matches!(
        receipt.verdict,
        ValidationVerdictV1::Failed | ValidationVerdictV1::Unverified
    ) {
        return;
    }
    let Some(findings) = review.get("findings").and_then(serde_json::Value::as_array) else {
        return;
    };
    let now = chrono::Utc::now().to_rfc3339();
    for finding in findings {
        let Some(severity) = finding.get("severity").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !matches!(severity, "blocker" | "major") {
            continue;
        }
        let finding_sha256 =
            crate::CasStore::hash_of(&serde_json::to_vec(finding).unwrap_or_default());
        let requirement_id = finding
            .get("requirement_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let identity = serde_json::json!({
            "task_id": &task_id,
            "attempt_id": turn_id,
            "requirement_id": &requirement_id,
            "severity": severity,
            "finding_sha256": &finding_sha256,
        });
        let issue_id = format!(
            "single-review-issue-{}",
            crate::CasStore::hash_of(&serde_json::to_vec(&identity).unwrap_or_default())
        );
        let detail = finding
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("评审指出阻断问题")
            .to_string();
        if let Some(existing) = session
            .single_review_issues
            .iter_mut()
            .find(|issue| issue.issue_id == issue_id)
        {
            if existing.source_review_artifact_id == receipt.receipt_id {
                continue;
            }
            existing.source_review_artifact_id = receipt.receipt_id.clone();
            existing.source_review_sha256 = review_sha256.clone();
            existing.detail = detail;
            if matches!(
                existing.status,
                crate::goal::DeliveryIssueStatusV1::RepairDispatched
                    | crate::goal::DeliveryIssueStatusV1::Resolved
            ) {
                existing.repair_attempt = existing.repair_attempt.saturating_add(1);
            }
            existing.status = crate::goal::DeliveryIssueStatusV1::Open;
            existing.resolution_review_artifact_id = None;
            existing.resolution_review_sha256 = None;
            existing.resolution_attempt_id = None;
            existing.updated_at = now.clone();
            continue;
        }
        session
            .single_review_issues
            .push(crate::goal::DeliveryIssueV1 {
                issue_id,
                source_review_artifact_id: receipt.receipt_id.clone(),
                source_review_sha256: review_sha256.clone(),
                finding_sha256,
                severity: severity.to_string(),
                detail,
                requirement_id,
                target_task_id: task_id.clone(),
                target_attempt_id: turn_id.to_string(),
                target_artifact_id: None,
                owner_step_id: format!("single:{}", session.id),
                status: crate::goal::DeliveryIssueStatusV1::Open,
                repair_attempt: 1,
                resolution_review_artifact_id: None,
                resolution_review_sha256: None,
                resolution_attempt_id: None,
                opened_at: now.clone(),
                updated_at: now.clone(),
            });
    }
}

pub(in crate::agent) fn mark_review_issue_repair_dispatched(session: &mut Session, turn_id: &str) {
    let task_id = session
        .active_task_context
        .as_ref()
        .and_then(|context| context.task_id.as_deref())
        .unwrap_or(session.id.as_str());
    let now = chrono::Utc::now().to_rfc3339();
    for issue in &mut session.single_review_issues {
        if issue.target_task_id == task_id
            && issue.target_attempt_id == turn_id
            && issue.status == crate::goal::DeliveryIssueStatusV1::Open
        {
            issue.status = crate::goal::DeliveryIssueStatusV1::RepairDispatched;
            issue.updated_at = now.clone();
        }
    }
}
