//! 宿主验证、候选版本摘要与目标完成收尾。
//!
//! 该模块不运行 Worker；它只根据本次 Goal 状态和宿主验证回执决定目标是否完成。

use super::{GoalRunner, GoalStatus};
use crate::plan::{StepSpec, StepStatus};

impl GoalRunner {
    /// Invalidate Passed receipts if their workspace snapshot no longer matches the final tree.
    pub(super) fn invalidate_stale_workspace_receipts(&mut self) -> Vec<String> {
        fn invalidate(
            receipts: &mut [crate::plan::ValidationReceiptV1],
            snapshot: &mut crate::workspace_snapshot::WorkspaceSnapshotBatch,
            stale_ids: &mut Vec<String>,
        ) {
            for receipt in receipts {
                if receipt.verdict != crate::plan::ValidationVerdictV1::Passed
                    || !receipt
                        .subject_sha256
                        .keys()
                        .any(|subject| subject.starts_with("workspace-path:"))
                {
                    continue;
                }
                let matches = snapshot.subjects_match(&receipt.subject_sha256);
                if !matches {
                    receipt.verdict = crate::plan::ValidationVerdictV1::Stale;
                    receipt.detail =
                        Some("Goal 最终交付时工作区文件已偏离该验证回执绑定的快照".to_string());
                    receipt.completed_at = chrono::Utc::now().to_rfc3339();
                    stale_ids.push(receipt.receipt_id.clone());
                }
            }
        }

        let workspace_root = self.workspace_verification_root.clone();
        let mut snapshot =
            crate::workspace_snapshot::WorkspaceSnapshotBatch::new(workspace_root.as_deref());
        let mut stale_ids = Vec::new();
        for record in self.state.records.values_mut() {
            invalidate(
                &mut record.validation_receipts,
                &mut snapshot,
                &mut stale_ids,
            );
        }
        invalidate(
            &mut self.state.validation_receipts,
            &mut snapshot,
            &mut stale_ids,
        );
        stale_ids
    }

    /// Goal completion is backed by host receipts over the accepted aggregate output.
    pub(super) fn verify_goal(&mut self) -> Result<GoalStatus, String> {
        self.state.goal.transition(GoalStatus::Verifying);
        self.log("goal.verifying", "全部步骤成功，进入目标验收");
        let plan = if let Some(plan) = self.state.goal.verification_plan.clone() {
            Some(plan)
        } else if self.state.goal.acceptance.is_empty() {
            None
        } else {
            Some(crate::verification::plan_for_specs(
                &format!("verify-goal-{}", self.state.goal.id),
                &self.state.goal.acceptance,
            ))
        };
        let has_candidate_changes = self.state.plan.steps.iter().any(|step| {
            self.state
                .records
                .get(&step.id)
                .is_some_and(|record| record.status == StepStatus::Succeeded)
        });
        let mut required_validation_count = 0usize;
        let mut passed_required_validation_count = 0usize;
        let mut failed_required_validation_count = 0usize;
        let mut completion_evidence_receipt_ids = Vec::new();
        for step in &self.state.plan.steps {
            let step_plan = step.verification_plan.clone().or_else(|| {
                step.verify.as_ref().map(|spec| {
                    crate::verification::plan_for_specs(
                        &format!("verify-step-{}", step.id),
                        std::slice::from_ref(spec),
                    )
                })
            });
            let Some(step_plan) = step_plan else {
                continue;
            };
            let record = self.state.records.get(&step.id);
            let receipts = record
                .map(|record| record.validation_receipts.as_slice())
                .unwrap_or_default();
            let active_attempt_id = record.and_then(|record| record.attempt_id.as_deref());
            let active_epoch = record.and_then(|record| record.phase_epoch);
            let output_sha256 = record
                .and_then(|record| record.output.as_deref())
                .map(|output| crate::cas_store::CasStore::hash_of(output.as_bytes()));
            for requirement in step_plan.requirements.iter().filter(|item| item.required) {
                required_validation_count += 1;
                let receipt = active_attempt_id
                    .zip(active_epoch)
                    .zip(output_sha256.as_deref())
                    .and_then(|((attempt_id, epoch), output_sha256)| {
                        receipts.iter().rev().find(|receipt| {
                            super::step_validation_receipt_matches(
                                &step.id,
                                attempt_id,
                                epoch,
                                output_sha256,
                                requirement,
                                receipt,
                            )
                        })
                    });
                match receipt.map(|receipt| receipt.verdict) {
                    Some(crate::plan::ValidationVerdictV1::Passed) => {
                        passed_required_validation_count += 1;
                        if let Some(receipt) = receipt {
                            completion_evidence_receipt_ids.push(receipt.receipt_id.clone());
                        }
                    }
                    Some(crate::plan::ValidationVerdictV1::Failed) => {
                        failed_required_validation_count += 1;
                    }
                    _ => {}
                }
            }
        }
        if let Some(plan) = plan {
            if let Err(reason) = plan.validate() {
                return self.fail_goal(format!("目标 VerificationPlan 非法：{reason}"));
            }
            let accepted_steps = self
                .state
                .records
                .values()
                .filter(|record| record.status == StepStatus::Succeeded)
                .filter_map(|record| record.output.as_ref().map(|output| (record, output)))
                .collect::<Vec<_>>();
            let summary = accepted_steps
                .iter()
                .map(|(_, output)| output.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let input_sha256 = crate::cas_store::CasStore::hash_of(summary.as_bytes());
            let verification_root = self.workspace_verification_root.clone();
            let environment_id = verification_root
                .as_deref()
                .and_then(|root| root.canonicalize().ok())
                .map(|root| {
                    format!(
                        "goal-workspace-v1:{}",
                        crate::cas_store::CasStore::hash_of(root.to_string_lossy().as_bytes())
                    )
                })
                .unwrap_or_else(|| "goal-aggregate-output-v1".to_string());
            let evidence_refs = accepted_steps
                .iter()
                .map(|(record, _)| {
                    format!(
                        "goal-step:{}:attempt:{}",
                        record.step_id,
                        record.attempt_id.as_deref().unwrap_or("legacy")
                    )
                })
                .collect::<Vec<_>>();
            let mut workspace_validation =
                crate::verification::WorkspaceValidationBatch::new(verification_root.as_deref());
            for requirement in &plan.requirements {
                let (verdict, detail, workspace_subjects) =
                    workspace_validation.execute_registered(requirement, &summary);
                let mut receipt_subjects = std::collections::HashMap::from([(
                    "goal-aggregate-output".to_string(),
                    input_sha256.clone(),
                )]);
                receipt_subjects.extend(workspace_subjects.clone());
                let workspace_subjects_sha256 =
                    serde_json::to_vec(&workspace_subjects).unwrap_or_default();
                let changeset_sha256 = (!workspace_subjects.is_empty())
                    .then(|| crate::cas_store::CasStore::hash_of(&workspace_subjects_sha256));
                let timestamp = chrono::Utc::now().to_rfc3339();
                let arguments = serde_json::to_string(&requirement.arguments)
                    .unwrap_or_else(|_| "null".to_string());
                let attempt_id = format!("{}:goal-verification", self.state.run_id);
                let arguments_sha256 = crate::cas_store::CasStore::hash_of(arguments.as_bytes());
                let validator_version = requirement
                    .validator_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let verdict_label =
                    serde_json::to_string(&verdict).unwrap_or_else(|_| "unknown".to_string());
                let identity = format!(
                    "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                    self.state.goal.id,
                    attempt_id,
                    self.state.replan_count,
                    requirement.requirement_id,
                    requirement.validator_id,
                    validator_version,
                    arguments_sha256,
                    input_sha256,
                    crate::cas_store::CasStore::hash_of(&workspace_subjects_sha256),
                    verdict_label
                );
                let receipt = crate::plan::ValidationReceiptV1 {
                    receipt_id: crate::cas_store::CasStore::hash_of(identity.as_bytes()),
                    task_id: self.state.goal.id.clone(),
                    attempt_id,
                    epoch: self.state.replan_count as u64,
                    requirement_id: requirement.requirement_id.clone(),
                    validator_id: requirement.validator_id.clone(),
                    validator_version,
                    arguments_sha256,
                    input_sha256: input_sha256.clone(),
                    environment_id: environment_id.clone(),
                    changeset_sha256,
                    detail: detail.clone(),
                    subject_sha256: receipt_subjects,
                    verdict,
                    evidence_refs: evidence_refs.clone(),
                    review_result: None,
                    started_at: timestamp.clone(),
                    completed_at: timestamp,
                };
                if requirement.required && verdict == crate::plan::ValidationVerdictV1::Passed {
                    completion_evidence_receipt_ids.push(receipt.receipt_id.clone());
                }
                if !self
                    .state
                    .validation_receipts
                    .iter()
                    .any(|stored| stored.receipt_id == receipt.receipt_id)
                {
                    self.state.validation_receipts.push(receipt);
                }
                if requirement.required {
                    required_validation_count += 1;
                    match verdict {
                        crate::plan::ValidationVerdictV1::Passed => {
                            passed_required_validation_count += 1;
                        }
                        crate::plan::ValidationVerdictV1::Failed => {
                            failed_required_validation_count += 1;
                        }
                        _ => {}
                    }
                    if verdict != crate::plan::ValidationVerdictV1::Passed {
                        let completion_status = crate::completion::decide_completion(
                            crate::completion::CompletionEvidence {
                                response_finished: true,
                                has_candidate_changes: true,
                                required_validation_count: 1,
                                passed_required_validation_count: usize::from(
                                    verdict == crate::plan::ValidationVerdictV1::Passed,
                                ),
                                failed_required_validation_count: usize::from(
                                    verdict == crate::plan::ValidationVerdictV1::Failed,
                                ),
                                stale_evidence: verdict == crate::plan::ValidationVerdictV1::Stale,
                                ..crate::completion::CompletionEvidence::default()
                            },
                        );
                        return self.fail_goal_with_status(
                            format!(
                                "目标验收要求 {} 未通过或未验证：{}",
                                requirement.requirement_id,
                                detail.as_deref().unwrap_or("验证器未返回通过")
                            ),
                            completion_status,
                        );
                    }
                }
            }
        }
        let stale_workspace_receipts = self.invalidate_stale_workspace_receipts();
        if !stale_workspace_receipts.is_empty() {
            return self.fail_goal_with_status(
                format!(
                    "目标最终验收发现工作区文件已偏离通过验证的快照：{}",
                    stale_workspace_receipts.join(", ")
                ),
                owo_agent_protocol::CompletionStatusV1::Unverified,
            );
        }

        let completion_status =
            crate::completion::decide_completion(crate::completion::CompletionEvidence {
                response_finished: true,
                has_candidate_changes,
                required_validation_count,
                passed_required_validation_count,
                failed_required_validation_count,
                ..crate::completion::CompletionEvidence::default()
            });
        if !matches!(
            completion_status,
            owo_agent_protocol::CompletionStatusV1::ResponseComplete
                | owo_agent_protocol::CompletionStatusV1::Accepted
        ) {
            return self.fail_goal_with_status(
                format!(
                    "目标候选结果未达到共享完成条件：{completion_status:?}；需要宿主验证计划或人工验收"
                ),
                completion_status,
            );
        }
        let evidence_receipt_ids = completion_evidence_receipt_ids;
        let candidate_version_sha256 = if has_candidate_changes {
            match self.candidate_version_sha256(false, true) {
                Ok(candidate_version_sha256) => candidate_version_sha256,
                Err(error) => return self.fail_goal(error),
            }
        } else {
            None
        };
        let final_stale_workspace_receipts = self.invalidate_stale_workspace_receipts();
        if !final_stale_workspace_receipts.is_empty() {
            return self.fail_goal_with_status(
                format!(
                    "目标候选摘要生成后工作区再次变化，验证快照已失效：{}",
                    final_stale_workspace_receipts.join(", ")
                ),
                owo_agent_protocol::CompletionStatusV1::Unverified,
            );
        }
        self.state.completion_record = Some(crate::completion::build_completion_record(
            &self.state.goal.id,
            &self.state.run_id,
            completion_status,
            evidence_receipt_ids,
            candidate_version_sha256,
        ));
        self.state.goal.transition(GoalStatus::Succeeded);
        self.log("goal.succeeded", "目标验收通过");
        self.persist_if_needed();
        Ok(GoalStatus::Succeeded)
    }

    pub(super) fn failed_step_verification_status(
        &self,
        failed_steps: &[StepSpec],
    ) -> Option<owo_agent_protocol::CompletionStatusV1> {
        use crate::plan::ValidationVerdictV1;

        let mut required = 0usize;
        let mut passed = 0usize;
        let mut failed = 0usize;
        let mut stale = false;
        let mut observed_current_receipt = false;
        let mut non_validation_failure = false;

        for step in failed_steps {
            let plan = step.verification_plan.clone().or_else(|| {
                step.verify.as_ref().map(|spec| {
                    crate::verification::plan_for_specs(
                        &format!("verify-step-{}", step.id),
                        std::slice::from_ref(spec),
                    )
                })
            });
            let Some(plan) = plan else {
                non_validation_failure = true;
                continue;
            };
            let Some(record) = self.state.records.get(&step.id) else {
                non_validation_failure = true;
                continue;
            };
            let Some(attempt_id) = record.attempt_id.as_deref() else {
                non_validation_failure = true;
                continue;
            };
            let Some(epoch) = record.phase_epoch else {
                non_validation_failure = true;
                continue;
            };
            let mut step_observed_receipt = false;

            for requirement in plan.requirements.iter().filter(|item| item.required) {
                required = required.saturating_add(1);
                let arguments_sha256 = crate::cas_store::CasStore::hash_of(
                    &serde_json::to_vec(&requirement.arguments).unwrap_or_default(),
                );
                let receipt = record.validation_receipts.iter().rev().find(|receipt| {
                    receipt.task_id == step.id
                        && receipt.attempt_id == attempt_id
                        && receipt.epoch == epoch
                        && receipt.requirement_id == requirement.requirement_id
                        && receipt.validator_id == requirement.validator_id
                        && receipt.validator_version
                            == requirement
                                .validator_version
                                .as_deref()
                                .unwrap_or("unknown")
                        && receipt.arguments_sha256 == arguments_sha256
                });
                let Some(receipt) = receipt else {
                    continue;
                };
                observed_current_receipt = true;
                step_observed_receipt = true;
                match receipt.verdict {
                    ValidationVerdictV1::Passed => passed = passed.saturating_add(1),
                    ValidationVerdictV1::Failed => failed = failed.saturating_add(1),
                    ValidationVerdictV1::Stale => stale = true,
                    ValidationVerdictV1::Unsupported
                    | ValidationVerdictV1::Unverified
                    | ValidationVerdictV1::ManualAccepted => {}
                }
            }
            if !step_observed_receipt {
                non_validation_failure = true;
            }
        }

        if non_validation_failure || !observed_current_receipt || required == 0 {
            return None;
        }
        let status = crate::completion::decide_completion(crate::completion::CompletionEvidence {
            response_finished: true,
            has_candidate_changes: true,
            required_validation_count: required,
            passed_required_validation_count: passed,
            failed_required_validation_count: failed,
            stale_evidence: stale,
            ..crate::completion::CompletionEvidence::default()
        });
        (!matches!(
            status,
            owo_agent_protocol::CompletionStatusV1::Accepted
                | owo_agent_protocol::CompletionStatusV1::ResponseComplete
        ))
        .then_some(status)
    }

    pub(super) fn candidate_version_sha256(
        &self,
        include_unpassed_receipts: bool,
        force_snapshot: bool,
    ) -> Result<Option<String>, String> {
        let accepted_outputs = self
            .state
            .plan
            .steps
            .iter()
            .filter_map(|step| {
                let record = self.state.records.get(&step.id)?;
                (record.status == StepStatus::Succeeded).then(|| {
                    serde_json::json!({
                        "step_id": step.id,
                        "attempt_id": record.attempt_id,
                        "output_sha256": record.output.as_deref().map(|output| {
                            crate::cas_store::CasStore::hash_of(output.as_bytes())
                        }),
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut workspace_paths = std::collections::BTreeMap::new();
        let receipts = self.state.validation_receipts.iter().chain(
            self.state
                .records
                .values()
                .flat_map(|record| record.validation_receipts.iter()),
        );
        for receipt in receipts.filter(|receipt| {
            receipt.verdict == crate::plan::ValidationVerdictV1::Passed
                || (include_unpassed_receipts
                    && !matches!(
                        receipt.verdict,
                        crate::plan::ValidationVerdictV1::Stale
                            | crate::plan::ValidationVerdictV1::Unsupported
                    ))
        }) {
            for (subject, hash) in &receipt.subject_sha256 {
                if let Some(relative) = subject.strip_prefix("workspace-path:") {
                    if workspace_paths
                        .insert(relative.to_string(), hash.clone())
                        .is_some_and(|previous| previous != *hash)
                    {
                        return Err(format!(
                            "目标候选快照中同一路径存在冲突验证摘要：{relative}"
                        ));
                    }
                }
            }
        }
        if !force_snapshot && accepted_outputs.is_empty() && workspace_paths.is_empty() {
            return Ok(None);
        }
        let candidate_snapshot = serde_json::json!({
            "accepted_step_outputs": accepted_outputs,
            "workspace_paths": workspace_paths,
        });
        crate::completion::hash_candidate_version(&candidate_snapshot)
            .map(Some)
            .map_err(|error| format!("目标候选版本摘要生成失败：{error}"))
    }

    pub(super) fn fail_goal(&mut self, reason: String) -> Result<GoalStatus, String> {
        self.fail_goal_with_status(reason, owo_agent_protocol::CompletionStatusV1::Blocked)
    }

    pub(super) fn fail_goal_with_status(
        &mut self,
        reason: String,
        completion_status: owo_agent_protocol::CompletionStatusV1,
    ) -> Result<GoalStatus, String> {
        let evidence_receipt_ids = self
            .state
            .validation_receipts
            .iter()
            .chain(
                self.state
                    .records
                    .values()
                    .flat_map(|record| record.validation_receipts.iter()),
            )
            .map(|receipt| receipt.receipt_id.clone())
            .collect::<Vec<_>>();
        let candidate_version_sha256 = self.candidate_version_sha256(true, false).ok().flatten();
        self.state.completion_record = Some(crate::completion::build_completion_record(
            &self.state.goal.id,
            &self.state.run_id,
            completion_status,
            evidence_receipt_ids,
            candidate_version_sha256,
        ));
        self.state.goal.error = Some(reason.clone());
        self.state.goal.transition(GoalStatus::Failed);
        self.log("goal.failed", reason);
        self.persist_if_needed();
        Ok(GoalStatus::Failed)
    }

    pub(super) fn mark_remaining(&mut self, status: StepStatus) {
        for record in self.state.records.values_mut() {
            if !record.status.is_terminal() {
                record.status = status;
            }
        }
    }

    pub(super) fn persist_if_needed(&mut self) {
        let Some(dir) = self.config.persist_dir.clone() else {
            return;
        };
        match self.state.persist(&dir) {
            Ok(_) => self.persistence_error = None,
            Err(error) => {
                let message = format!("运行状态检查点写入失败：{error}");
                self.persistence_error = Some(message.clone());
                self.log("goal.persistence.failed", message);
            }
        }
    }
}
