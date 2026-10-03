use super::delivery_gate_evidence::{
    collect_attempt_changeset_evidence, make_validation_receipt, store_validation_receipt,
    ValidationReceiptInput,
};
use super::*;

impl TeamCoordinator {
    /// 失败收尾（team → Failed；产物保留；成员 Degraded）。
    pub(crate) async fn fail_run_internal(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
        reason: &str,
    ) -> WorkSwarmResult<()> {
        if state.goal.status == GoalStatus::Succeeded {
            // A host validator may reject the candidate after GoalRunner completes but
            // before DeliveryGate commits the TeamRun; this is a valid Succeeded → Failed
            // delivery transition because no user-facing delivery has been committed.
            state.goal.status = GoalStatus::Failed;
        } else if !state.goal.status.is_terminal() || state.goal.status == GoalStatus::Aborted {
            state.goal.transition(GoalStatus::Failed);
        }
        state.goal.error = Some(reason.to_string());
        self.persist_state(state)?;
        let failed_members: Vec<String> = state
            .records
            .values()
            .filter(|r| matches!(r.status, StepStatus::Failed | StepStatus::Aborted))
            .filter_map(|r| {
                state
                    .plan
                    .steps
                    .iter()
                    .find(|s| s.id == r.step_id)
                    .map(|s| s.worker.clone())
            })
            .collect();
        team.status = TeamRunStatus::Failed;
        team.updated_at = now_ts();
        for m in &mut team.members {
            if failed_members.contains(&m.member_id) {
                m.health = MemberHealth::Degraded;
            }
        }
        self.store.save_team_run(team).await?;
        self.space_activity(team_id, &format!("team.failed：{reason}（已完成产物保留）"))
            .await?;
        self.audit(team_id, "team.failed", format!("失败：{reason}"));
        // 终态落定：过期中断标记不再有意义。
        self.clear_interrupted_marker(team_id);
        Ok(())
    }

    /// 取消收尾（team → Cancelled；未完成步骤 Aborted；已完成产物保留）。
    pub(crate) async fn cancel_run_internal(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
        reason: &str,
    ) -> WorkSwarmResult<()> {
        state.aborted = true;
        for r in state.records.values_mut() {
            if !r.status.is_terminal() {
                r.status = StepStatus::Aborted;
            }
        }
        if !state.goal.status.is_terminal() {
            state.goal.transition(GoalStatus::Aborted);
        }
        self.persist_state(state)?;
        team.status = TeamRunStatus::Cancelled;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        self.space_activity(
            team_id,
            &format!("team.cancelled：{reason}（已完成产物保留）"),
        )
        .await?;
        // 十期·四路 R5：取消审计明确写入 Provider 计费限制——客户端取消（置位
        // 取消令牌/abort 标志、断开流、终止子进程）只能停止**我方发起**的后续
        // 请求与执行；对云端 Provider 的**已在途请求**，我方无法证明其对账侧
        // 已停止计费（不同 Provider 的结算粒度/停账语义各异），因此**不得宣称**
        // 「继续计费为 0」。可证明为零的只有由本进程全程掌控计数的离线/脚本化
        // Provider（详见 eval 执行器）。此限制同样适用于 gateway 的流式响应丢弃。
        self.audit(
            team_id,
            "team.cancelled",
            format!(
                "取消：{reason}（完成后快照/变更登记已在协作收尾中完成；\
                 Provider 在途请求计费停止无法由客户端证明，不以「继续计费为 0」宣称）"
            ),
        );
        // 终态落定：过期中断标记不再有意义。
        self.clear_interrupted_marker(team_id);
        Ok(())
    }

    pub(crate) async fn space_activity(&self, team_id: &str, msg: &str) -> WorkSwarmResult<()> {
        let team = self.store.get_team_run(team_id).await?;
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run(format!("团队 {team_id} 缺少 project_space_id")))?;
        let mut space = self.store.get_project_space(&pid).await?;
        space.activity_stream.push(msg.to_string());
        if space.activity_stream.len() > 200 {
            let drain = space.activity_stream.len() - 200;
            space.activity_stream.drain(..drain);
        }
        space.version += 1;
        space.updated_at = now_ts();
        self.store.save_project_space(&space).await?;
        Ok(())
    }

    // -- 收尾：交付清单 + 模板提案（§6.7：只提案，不自动启用） --

    /// 宿主交付门：校验步骤绑定的交接、CAS 内容哈希、产物格式、任务断言和未解决问题，
    /// 并为通过验收的实际产物版本生成可追溯收据。
    async fn validate_delivery_gate(
        &self,
        team_id: &str,
        space: &ProjectSpace,
        state: &mut GoalRunState,
    ) -> WorkSwarmResult<Vec<Value>> {
        if state.aborted || !Self::all_succeeded(state) {
            return Err(WorkSwarmError::Conflict(
                "存在未完成或已取消的步骤，不能收尾".to_string(),
            ));
        }
        if let Some(issue) = state
            .delivery_issues
            .iter()
            .find(|issue| issue.status != crate::goal::DeliveryIssueStatusV1::Resolved)
        {
            return Err(WorkSwarmError::Conflict(format!(
                "存在未关闭的评审 Issue {}（task={} attempt={}），不能收尾",
                issue.issue_id, issue.target_task_id, issue.target_attempt_id
            )));
        }

        let handoffs = self
            .store
            .list_handoffs_by_project(&space.project_id)
            .await?;
        let run_meta = RunMeta::load(&self.run_dir, team_id)?;
        let change_sets = crate::change_set_store::ChangeSetStore::new(&self.run_dir)
            .list_for_team(team_id)
            .map_err(|error| WorkSwarmError::Run(format!("ChangeSet 读取失败：{error}")))?;
        let runtime_command_receipts =
            self.runtime_event_details(team_id, "team.command.executed");
        let reviewer_step_count = state
            .plan
            .steps
            .iter()
            .filter(|step| {
                run_meta.roles.iter().any(|role| {
                    step.worker == format!("m-{}", role.role) && role.is_reviewer()
                })
            })
            .count();
        let mut has_code_changes = false;
        let mut validated_review_count = 0usize;
        let mut validated_review_closures = HashSet::new();
        let mut acceptance_receipts = Vec::new();
        for step in state.plan.steps.clone() {
            let record = state.records.get(&step.id).cloned().ok_or_else(|| {
                WorkSwarmError::Conflict(format!("任务 {} 缺少执行记录", step.id))
            })?;
            if let Some(skip_reason) = record.skip_reason.as_deref() {
                let role = worker_role(&step.worker).unwrap_or_default();
                let reviewer = run_meta
                    .roles
                    .iter()
                    .find(|role| step.worker == format!("m-{}", role.role))
                    .is_some_and(|role| role.is_reviewer());
                let allowed_review_skip = reviewer
                    && run_meta.template_id.as_deref()
                        == Some(crate::builtin_team_templates::CODE_CHANGE_V1)
                    && self.workspace_change_status(team_id) == Some(false);
                let allowed_host_manifest = run_meta.parallel
                    && role == "leader"
                    && skip_reason == super::coord_run::PARALLEL_LEADER_HOST_MANIFEST_SKIP_REASON
                    && !super::coord_run::parallel_tasks_require_integration(&state.plan.steps);
                if !allowed_review_skip && !allowed_host_manifest {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的运行期跳过没有得到允许或有效的宿主交付证据，不能作为已验收交付",
                        step.id
                    )));
                }
                continue;
            }
            if record
                .output
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            {
                return Err(WorkSwarmError::Conflict(format!(
                    "必需任务 {} 已报告成功，但没有提交候选输出",
                    step.id
                )));
            }
            let prefix = format!("{team_id}:{}:", step.id);
            let handoff = handoffs
                .iter()
                .filter(|handoff| {
                    handoff.handoff_id.starts_with(&prefix) && handoff.from_member == step.worker
                })
                .max_by(|left, right| left.created_at.cmp(&right.created_at))
                .ok_or_else(|| {
                    WorkSwarmError::Conflict(format!(
                        "任务 {} 已报告完成，但没有绑定该任务的交付记录",
                        step.id
                    ))
                })?;
            if !handoff.open_issues.is_empty() {
                return Err(WorkSwarmError::Conflict(format!(
                    "任务 {} 的交付记录仍有未解决问题：{}",
                    step.id,
                    handoff.open_issues.join("；")
                )));
            }
            if handoff.output_artifact_refs.is_empty() {
                return Err(WorkSwarmError::Conflict(format!(
                    "任务 {} 的交付记录没有产物引用",
                    step.id
                )));
            }
            let verification_plan = step.verification_plan.as_ref().ok_or_else(|| {
                WorkSwarmError::Conflict(format!(
                    "任务 {} 没有宿主解析的 VerificationPlan，结果保持未验证",
                    step.id
                ))
            })?;
            verification_plan.validate().map_err(|reason| {
                WorkSwarmError::Conflict(format!(
                    "任务 {} 的 VerificationPlan 非法：{reason}",
                    step.id
                ))
            })?;
            let epoch = record.phase_epoch.ok_or_else(|| {
                WorkSwarmError::Conflict(format!(
                    "任务 {} 缺少宿主 attempt epoch，验证回执不能绑定当前执行",
                    step.id
                ))
            })?;
            let attempt_id = record.attempt_id.as_deref().ok_or_else(|| {
                WorkSwarmError::Conflict(format!(
                    "任务 {} 缺少宿主 attempt_id，不能绑定验收结果",
                    step.id
                ))
            })?;
            let changeset_contains_code =
                super::delivery_gate_evidence::attempt_changeset_contains_code(
                    &change_sets,
                    team_id,
                    &step.id,
                    attempt_id,
                );
            has_code_changes |= changeset_contains_code;
            let has_behavior_command = verification_plan.requirements.iter().any(|requirement| {
                requirement.required
                    && requirement.validator_id == "workspace-command-success-v1"
            });
            if changeset_contains_code && !has_behavior_command {
                return Err(WorkSwarmError::Conflict(format!(
                    "任务 {} 的实际 ChangeSet 修改了源代码，但没有必需的宿主行为验证命令",
                    step.id
                )));
            }
            let uncovered_source_paths =
                super::delivery_gate_evidence::uncovered_source_paths(
                    &change_sets,
                    team_id,
                    &step.id,
                    attempt_id,
                    verification_plan,
                );
            if !uncovered_source_paths.is_empty() {
                return Err(WorkSwarmError::Conflict(format!(
                    "任务 {} 的行为验证范围未覆盖所有已修改源码：{}",
                    step.id,
                    uncovered_source_paths.join(", ")
                )));
            }
            let input_snapshot = json!({
                "task_input": &step.input,
                "attempt_id": attempt_id,
                "epoch": epoch,
            });
            let input_bytes = serde_json::to_vec(&input_snapshot)?;
            let input_sha256 = CasStore::hash_of(&input_bytes);
            for artifact_id in &handoff.output_artifact_refs {
                let artifact = self.store.get_artifact(artifact_id).await?;
                let code_artifact =
                    super::delivery_gate_evidence::is_code_artifact_kind(&artifact.kind);
                if code_artifact && !changeset_contains_code {
                    return Err(WorkSwarmError::Conflict(format!(
                        "代码产物 {} 没有关联当前 attempt 的源文件 ChangeSet，不能作为交付",
                        artifact.artifact_id
                    )));
                }
                if (changeset_contains_code || code_artifact) && !has_behavior_command {
                    return Err(WorkSwarmError::Conflict(format!(
                        "代码任务 {} 缺少必需的宿主行为验证命令，静态文件检查不能通过交付门",
                        step.id
                    )));
                }
                if artifact.team_id != team_id || artifact.producer != step.worker {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物 {} 与当前团队/成员不匹配",
                        step.id, artifact_id
                    )));
                }
                if artifact.task_id.as_deref() != Some(step.id.as_str())
                    || artifact.attempt_id.as_deref() != Some(attempt_id)
                {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物 {} 缺少匹配当前 task/attempt 的宿主身份",
                        step.id, artifact_id
                    )));
                }
                let content_hash = artifact
                    .content_ref
                    .strip_prefix("cas://sha256:")
                    .ok_or_else(|| {
                        WorkSwarmError::Conflict(format!(
                            "任务 {} 的产物 {} 没有可验证的 CAS 内容引用",
                            step.id, artifact_id
                        ))
                    })?;
                let content_bytes = self.cas.get(content_hash).ok_or_else(|| {
                    WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物 {} 的 CAS 内容不存在",
                        step.id, artifact_id
                    ))
                })?;
                let actual_hash = CasStore::hash_of(&content_bytes);
                if actual_hash != content_hash
                    || artifact.sha256 != actual_hash
                    || artifact.size_bytes != content_bytes.len() as u64
                {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物 {} 内容哈希或字节数与登记信息不一致",
                        step.id, artifact_id
                    )));
                }
                let content = std::str::from_utf8(&content_bytes).map_err(|_| {
                    WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物 {} 不是可验收的 UTF-8 文本",
                        step.id, artifact_id
                    ))
                })?;
                let reviewer = run_meta
                    .roles
                    .iter()
                    .find(|role| step.worker == format!("m-{}", role.role))
                    .is_some_and(|role| role.is_reviewer());
                super::delivery_gate_evidence::validate_review_artifact_kind(
                    reviewer,
                    &artifact.kind,
                )
                .map_err(|reason| {
                    WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物类型不符合评审职责（kind={}）：{reason}",
                        step.id, artifact.kind
                    ))
                })?;
                if reviewer {
                    let review: Value = serde_json::from_str(content).map_err(|_| {
                        WorkSwarmError::Conflict(format!(
                            "review Artifact {} 缺少有效的结构化 ReviewResult",
                            artifact.artifact_id
                        ))
                    })?;
                    let review_schema = review.get("schema").and_then(Value::as_str);
                    let reviewer_id = review.get("reviewer_id").and_then(Value::as_str);
                    if review_schema != Some("team-review-result-v1") {
                        return Err(WorkSwarmError::Conflict(format!(
                            "review Artifact {} 的 schema 无效: {:?}",
                            artifact.artifact_id, review_schema
                        )));
                    }
                    if reviewer_id != Some(artifact.producer.as_str()) {
                        return Err(WorkSwarmError::Conflict(format!(
                            "review Artifact {} 的 reviewer 身份无效: reviewer_id={:?}, producer={:?}",
                            artifact.artifact_id, reviewer_id, artifact.producer
                        )));
                    }
                    let review_result = review.get("result").ok_or_else(|| {
                        WorkSwarmError::Conflict(format!(
                            "review Artifact {} 缺少结构化 ReviewResult",
                            artifact.artifact_id
                        ))
                    })?;
                    super::delivery_gate_evidence::validate_review_approval(review_result)
                        .map_err(|reason| {
                            WorkSwarmError::Conflict(format!(
                                "review Artifact {} 未通过宿主裁决：{reason}",
                                artifact.artifact_id
                            ))
                        })?;
                    let review_is_approved =
                        review_result.get("verdict").and_then(Value::as_str) == Some("approved");
                    let bound = review
                        .get("reviewed_artifacts")
                        .and_then(Value::as_array)
                        .ok_or_else(|| {
                            WorkSwarmError::Conflict(
                                "ReviewResult 缺少宿主绑定的 reviewed_artifacts".to_string(),
                            )
                        })?;
                    let review_workspace = self.verification_workspace(team_id);
                    let mut expected = Vec::new();
                    for dependency in &step.depends_on {
                        let Some(dep_step) =
                            state.plan.steps.iter().find(|item| item.id == *dependency)
                        else {
                            continue;
                        };
                        let Some(_dep_role) = worker_role(&dep_step.worker) else {
                            continue;
                        };
                        if let Some(current) = self
                            .latest_artifact_for_step(space, state, &dep_step.id)
                            .await
                        {
                            let current_attempt = state
                                .records
                                .get(&dep_step.id)
                                .and_then(|record| record.attempt_id.as_deref())
                                .unwrap_or_default();
                            let reviewed_source =
                                super::delivery_gate_evidence::review_source_snapshot(
                                    team_id,
                                    &dep_step.id,
                                    current_attempt,
                                    &change_sets,
                                    review_workspace.as_deref(),
                                );
                            expected.push((
                                current.artifact_id,
                                current.sha256,
                                current.producer,
                                current.kind,
                                reviewed_source,
                                dep_step.id.clone(),
                                current_attempt.to_string(),
                            ));
                        }
                    }
                    if expected.is_empty() || bound.len() != expected.len() {
                        return Err(WorkSwarmError::Conflict(format!(
                            "ReviewResult {} 的被审查范围与当前任务上游不一致",
                            artifact.artifact_id
                        )));
                    }
                    for (
                        artifact_id,
                        hash,
                        producer,
                        kind,
                        reviewed_source,
                        reviewed_task_id,
                        reviewed_attempt_id,
                    ) in expected
                    {
                        let Some(binding) = bound.iter().find(|item| {
                            item.get("artifact_id").and_then(Value::as_str)
                                == Some(artifact_id.as_str())
                        }) else {
                            let bound_ids = bound
                                .iter()
                                .filter_map(|item| item.get("artifact_id").and_then(Value::as_str))
                                .collect::<Vec<_>>();
                            return Err(WorkSwarmError::Conflict(format!(
                                "ReviewResult {} 未覆盖当前上游产物 {}；收据快照包含 {:?}",
                                artifact.artifact_id, artifact_id, bound_ids
                            )));
                        };
                        if binding.get("sha256").and_then(Value::as_str) != Some(hash.as_str())
                            || binding.get("producer").and_then(Value::as_str)
                                != Some(producer.as_str())
                            || binding.get("task_id").and_then(Value::as_str)
                                != Some(reviewed_task_id.as_str())
                            || binding.get("attempt_id").and_then(Value::as_str)
                                != Some(reviewed_attempt_id.as_str())
                            || binding.get("reviewed_source") != Some(&reviewed_source)
                            || producer == artifact.producer
                        {
                            return Err(WorkSwarmError::Conflict(format!(
                                "ReviewResult {} 的 Artifact、源码快照或独立评审身份已过期",
                                artifact.artifact_id
                            )));
                        }
                        if review_is_approved {
                            validated_review_closures.insert((
                                artifact.artifact_id.clone(),
                                actual_hash.clone(),
                                reviewed_task_id,
                                reviewed_attempt_id,
                            ));
                        }
                        if super::delivery_gate_evidence::is_code_artifact_kind(&kind)
                            || reviewed_source
                                .get("contains_source_code")
                                .and_then(Value::as_bool)
                                == Some(true)
                        {
                            let source_hashes = reviewed_source
                                .get("source_hashes")
                                .and_then(Value::as_object);
                            if reviewed_source
                                .get("workspace_observed")
                                .and_then(Value::as_bool)
                                != Some(true)
                                || reviewed_source
                                    .get("changeset_source_consistent")
                                    .and_then(Value::as_bool)
                                    != Some(true)
                                || source_hashes.is_none_or(|hashes| {
                                    hashes.is_empty()
                                        || hashes.values().any(|entry| {
                                            entry.get("observed").and_then(Value::as_bool)
                                                != Some(true)
                                        })
                                })
                            {
                                return Err(WorkSwarmError::Conflict(format!(
                                    "ReviewResult {} 没有完整的最终源码读取证据",
                                    artifact.artifact_id
                                )));
                            }
                        }
                    }
                }
                if !artifact.open_issues.is_empty() {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物仍有未解决问题：{}",
                        step.id,
                        artifact.open_issues.join("；")
                    )));
                }
                let evidence = artifact
                    .evidence_refs
                    .iter()
                    .map(|source| crate::workswarm_output::WorkerEvidenceV1 {
                        source: source.clone(),
                        note: None,
                    })
                    .collect::<Vec<_>>();
                let mut validation_receipts = Vec::new();
                let (changeset_sha256, changeset_refs) = collect_attempt_changeset_evidence(
                    team_id,
                    &step.id,
                    attempt_id,
                    &change_sets,
                )?;
                let format_started_at = now_ts();
                let format_arguments_sha256 = CasStore::hash_of(
                    serde_json::json!({"format": &artifact.format})
                        .to_string()
                        .as_bytes(),
                );
                let format_validation = crate::artifact_pipeline::validate_artifact_content(
                    &artifact.format,
                    content,
                    &evidence,
                );
                let format_detail = format_validation.reason.clone();
                let format_verdict = if format_validation.valid {
                    crate::plan::ValidationVerdictV1::Passed
                } else {
                    crate::plan::ValidationVerdictV1::Failed
                };
                let format_receipt = make_validation_receipt(ValidationReceiptInput {
                    team_id,
                    step_id: &step.id,
                    attempts: record.attempts,
                    attempt_id,
                    epoch,
                    requirement_id: &format!("{}:artifact-format", step.id),
                    scope: &crate::plan::VerificationScopeV1::ArtifactRefs {
                        artifact_ids: vec![artifact.artifact_id.clone()],
                    },
                    validator_id: "artifact-format-v1",
                    validator_version: "1",
                    arguments_sha256: &format_arguments_sha256,
                    input_sha256: &input_sha256,
                    artifact_id: &artifact.artifact_id,
                    artifact_sha256: &actual_hash,
                    changeset_sha256: changeset_sha256.clone(),
                    changeset_refs: changeset_refs.clone(),
                    evidence_ref: &artifact.content_ref,
                    started_at: &format_started_at,
                    verdict: format_verdict,
                    detail: format_detail.clone(),
                    subject_hashes: std::collections::BTreeMap::new(),
                    additional_evidence_refs: Vec::new(),
                });
                store_validation_receipt(state, &step.id, &format_receipt);
                validation_receipts.push(format_receipt);
                if !format_validation.valid {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的产物 {} 格式验收失败：{}",
                        step.id,
                        artifact_id,
                        format_detail.as_deref().unwrap_or("无原因")
                    )));
                }

                for requirement in &verification_plan.requirements {
                    let started_at = now_ts();
                    let verification_workspace = self.verification_workspace(team_id);
                    let (mut verdict, mut detail, subject_hashes, command_evidence_ref) =
                        if requirement.validator_id == "workspace-command-success-v1" {
                            super::delivery_gate_evidence::evaluate_workspace_command_receipt(
                                team_id,
                                requirement,
                                &runtime_command_receipts,
                                &step.id,
                                attempt_id,
                                &change_sets,
                            )
                        } else {
                            let (verdict, detail, subject_hashes) =
                                crate::verification::execute_registered_requirement(
                                    requirement,
                                    content,
                                    verification_workspace.as_deref(),
                                );
                            (verdict, detail, subject_hashes, None)
                        };
                    if let crate::plan::VerificationScopeV1::WorkspacePaths { relative_paths } =
                        &requirement.scope
                    {
                        if let Err(reason) =
                            super::delivery_gate_evidence::validate_workspace_receipt_snapshot(
                                team_id,
                                &step.id,
                                attempt_id,
                                relative_paths,
                                &subject_hashes,
                                &change_sets,
                            )
                        {
                            verdict = crate::plan::ValidationVerdictV1::Failed;
                            detail = Some(reason);
                        }
                    }
                    let arguments_sha256 =
                        CasStore::hash_of(requirement.arguments.to_string().as_bytes());
                    let mut additional_evidence_refs = subject_hashes
                        .iter()
                        .map(|(subject, hash)| format!("{subject}@sha256:{hash}"))
                        .collect::<Vec<_>>();
                    if let Some(evidence_ref) = command_evidence_ref {
                        additional_evidence_refs.push(evidence_ref);
                    }
                    let receipt = make_validation_receipt(ValidationReceiptInput {
                        team_id,
                        step_id: &step.id,
                        attempts: record.attempts,
                        attempt_id,
                        epoch,
                        requirement_id: &requirement.requirement_id,
                        scope: &requirement.scope,
                        validator_id: &requirement.validator_id,
                        validator_version: requirement.validator_version.as_deref().unwrap_or(""),
                        arguments_sha256: &arguments_sha256,
                        input_sha256: &input_sha256,
                        artifact_id: &artifact.artifact_id,
                        artifact_sha256: &actual_hash,
                        changeset_sha256: changeset_sha256.clone(),
                        changeset_refs: changeset_refs.clone(),
                        evidence_ref: &artifact.content_ref,
                        started_at: &started_at,
                        verdict,
                        detail: detail.clone(),
                        additional_evidence_refs,
                        subject_hashes,
                    });
                    store_validation_receipt(state, &step.id, &receipt);
                    validation_receipts.push(receipt);
                    if requirement.required && verdict != crate::plan::ValidationVerdictV1::Passed {
                        return Err(WorkSwarmError::Conflict(format!(
                            "任务 {} 的验收要求 {} 未通过或未验证：{}",
                            step.id,
                            requirement.requirement_id,
                            detail.as_deref().unwrap_or("验证器未返回通过")
                        )));
                    }
                }
                let required_validation_count = 1 + verification_plan
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.required)
                    .count();
                let passed_required_validation_count = 1 + verification_plan
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.required)
                    .filter(|requirement| {
                        validation_receipts.iter().any(|receipt| {
                            receipt.requirement_id == requirement.requirement_id
                                && receipt.verdict == crate::plan::ValidationVerdictV1::Passed
                        })
                    })
                    .count();
                let failed_required_validation_count = verification_plan
                    .requirements
                    .iter()
                    .filter(|requirement| requirement.required)
                    .filter(|requirement| {
                        validation_receipts.iter().any(|receipt| {
                            receipt.requirement_id == requirement.requirement_id
                                && receipt.verdict == crate::plan::ValidationVerdictV1::Failed
                        })
                    })
                    .count();
                let completion_status = crate::completion::decide_completion(
                    crate::completion::CompletionEvidence {
                        response_finished: true,
                        has_candidate_changes: true,
                        required_validation_count,
                        passed_required_validation_count,
                        failed_required_validation_count,
                        ..crate::completion::CompletionEvidence::default()
                    },
                );
                if completion_status != owo_agent_protocol::CompletionStatusV1::Accepted {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 的共享完成裁决没有接受该候选结果：{completion_status:?}",
                        step.id
                    )));
                }
                if reviewer {
                    validated_review_count += 1;
                }
                acceptance_receipts.push(json!({
                    "completion_status": completion_status,
                    "step_id": &step.id,
                    "attempt_id": attempt_id,
                    "artifact_id": &artifact.artifact_id,
                    "artifact_kind": &artifact.kind,
                    "content_sha256": &actual_hash,
                    "format_validator": {
                        "id": "artifact-format-v1",
                        "format": &format_validation.format,
                        "verdict": "passed",
                    },
                    "validation_receipts": validation_receipts,
                }));
            }
        }

        for issue in &state.delivery_issues {
            let Some(review_id) = issue.resolution_review_artifact_id.as_deref() else {
                return Err(WorkSwarmError::Conflict(format!(
                    "已关闭评审 Issue {} 缺少关闭评审身份",
                    issue.issue_id
                )));
            };
            let Some(review_sha256) = issue.resolution_review_sha256.as_deref() else {
                return Err(WorkSwarmError::Conflict(format!(
                    "已关闭评审 Issue {} 缺少关闭评审版本哈希",
                    issue.issue_id
                )));
            };
            let Some(attempt_id) = issue.resolution_attempt_id.as_deref() else {
                return Err(WorkSwarmError::Conflict(format!(
                    "已关闭评审 Issue {} 缺少关闭 attempt 身份",
                    issue.issue_id
                )));
            };
            if issue.target_attempt_id == attempt_id
                || !validated_review_closures.contains(&(
                    review_id.to_string(),
                    review_sha256.to_string(),
                    issue.target_task_id.clone(),
                    attempt_id.to_string(),
                ))
            {
                return Err(WorkSwarmError::Conflict(format!(
                    "评审 Issue {} 的关闭证据没有绑定到修复后的最终 task/attempt 版本",
                    issue.issue_id
                )));
            }
        }
        let required_validation_count = acceptance_receipts.len();
        let independent_review_required = has_code_changes && reviewer_step_count > 0;
        let completion_status = crate::completion::decide_completion(
            crate::completion::CompletionEvidence {
                response_finished: true,
                has_candidate_changes: !acceptance_receipts.is_empty(),
                required_validation_count,
                passed_required_validation_count: required_validation_count,
                independent_review_required,
                independent_review_passed: validated_review_count >= reviewer_step_count,
                ..crate::completion::CompletionEvidence::default()
            },
        );
        if completion_status != owo_agent_protocol::CompletionStatusV1::Accepted {
            return Err(WorkSwarmError::Conflict(format!(
                "Team DeliveryGate 的共享完成裁决未接受最终候选版本：{completion_status:?}"
            )));
        }

        // 仅检查本次 ProjectSpace 产物，确保传入空间仍属于本次运行。
        if space.project_id.is_empty() {
            return Err(WorkSwarmError::Conflict(
                "交付空间缺少 project_id，不能确认产物归属".to_string(),
            ));
        }
        Ok(acceptance_receipts)
    }

    /// Persist a host-side validation failure before delivery has been committed.
    ///
    /// This is the terminal counterpart to the success finalizer: it preserves candidate
    /// artifacts while ensuring a stopped coordinator cannot leave the TeamRun Running.
    pub async fn fail_delivery_validation(
        &self,
        team_id: &str,
        reason: &str,
    ) -> WorkSwarmResult<TeamRun> {
        if reason.trim().is_empty() {
            return Err(WorkSwarmError::Validation(
                "宿主验收失败原因不能为空".to_string(),
            ));
        }
        if self.is_run_active(team_id) {
            return Err(WorkSwarmError::Conflict(
                "TeamRun 仍有活动阶段，不能收尾宿主验收失败".to_string(),
            ));
        }
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        if self.is_run_active(team_id) {
            return Err(WorkSwarmError::Conflict(
                "TeamRun 阶段已启动，不能收尾宿主验收失败".to_string(),
            ));
        }
        let (mut team, mut state) = {
            let (team, _space, state) = self.load_bundle(team_id).await?;
            (team, state)
        };
        if team.status == TeamRunStatus::Succeeded {
            return Err(WorkSwarmError::Conflict(
                "DeliveryGate 已提交成功交付，宿主验收不能事后撤销".to_string(),
            ));
        }
        if team.status.is_terminal() {
            return Ok(team);
        }
        self.fail_run_internal(team_id, &mut team, &mut state, reason)
            .await?;
        Ok(team)
    }

    /// 成功收尾：交付清单（CAS ref）+ ProjectSpace Completed + 模板提案。
    pub async fn finalize_success(&self, team_id: &str) -> WorkSwarmResult<TeamRun> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (mut team, mut space, mut state) = self.load_bundle(team_id).await?;
        let acceptance_receipts = match self
            .validate_delivery_gate(team_id, &space, &mut state)
            .await
        {
            Ok(receipts) => receipts,
            Err(error @ WorkSwarmError::DeliveryPending(_)) => {
                let reason = format!("delivery_pending:changeset:{error}");
                state.goal.transition(GoalStatus::Verifying);
                state.goal.error = Some(reason);
                self.persist_state(&state)?;
                team.status = TeamRunStatus::AwaitingHuman;
                team.updated_at = now_ts();
                self.store.save_team_run(&team).await?;
                self.audit(
                    team_id,
                    "team.delivery_awaiting_human",
                    format!("交付门等待 ChangeSet 人工处理：{error}"),
                );
                return Err(error);
            }
            Err(error) => {
                let reason = format!("交付验收未通过：{error}");
                self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                    .await?;
                return Err(error);
            }
        };
        // ValidationReceipt 与失败/成功的 task state 一起落盘；若后续发布清单失败，
        // 也不能丢失刚刚执行过的宿主验收证据。
        self.persist_state(&state)?;

        // 解析 ProjectSpace 的全部引用以拒绝悬空/跨团队索引；最终清单只发布通过
        // 本次任务验收的产物，返工旧版和未验收的附加产物继续保留在工作空间历史中。
        let accepted_artifact_ids: HashSet<String> = acceptance_receipts
            .iter()
            .filter_map(|receipt| receipt.get("artifact_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let delivery_artifact_ids: HashSet<String> = acceptance_receipts
            .iter()
            .filter(|receipt| {
                receipt
                    .get("artifact_kind")
                    .and_then(Value::as_str)
                    .is_some_and(super::delivery_gate_evidence::is_user_delivery_artifact)
            })
            .filter_map(|receipt| receipt.get("artifact_id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        let mut indexed_accepted_artifact_ids = HashSet::new();
        let mut published_artifact_ids = HashSet::new();
        let mut final_artifacts: Vec<Value> = Vec::new();
        for id in &space.artifacts {
            let artifact = self.store.get_artifact(id).await?;
            if artifact.team_id != team_id {
                return Err(WorkSwarmError::Conflict(format!(
                    "交付产物 {} 不属于当前团队 {}",
                    artifact.artifact_id, team_id
                )));
            }
            if accepted_artifact_ids.contains(id) {
                indexed_accepted_artifact_ids.insert(id.clone());
                if delivery_artifact_ids.contains(id) {
                    published_artifact_ids.insert(id.clone());
                    final_artifacts.push(json!({
                        "artifact_id": artifact.artifact_id,
                        "kind": artifact.kind,
                        "version": artifact.version,
                        "content_ref": artifact.content_ref,
                        "producer": artifact.producer,
                    }));
                }
            }
        }
        if indexed_accepted_artifact_ids != accepted_artifact_ids
            || published_artifact_ids != delivery_artifact_ids
        {
            return Err(WorkSwarmError::Conflict(
                "验收收据与 ProjectSpace 产物索引不一致，不能发布交付清单".to_string(),
            ));
        }
        let manifest = json!({
            "team_id": team_id,
            "objective": state.goal.objective,
            "artifacts": final_artifacts,
            "acceptance_receipts": acceptance_receipts,
            "delivery_issues": &state.delivery_issues,
            "created_at": now_ts(),
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let manifest_hash = self
            .cas
            .put(&manifest_bytes)
            .map_err(|e| WorkSwarmError::Run(format!("交付清单 CAS 落盘失败：{e}")))?;

        let mut succeeded_state = state.clone();
        succeeded_state.goal.transition(GoalStatus::Succeeded);
        self.persist_state(&succeeded_state)?;
        team.status = TeamRunStatus::Succeeded;
        team.updated_at = now_ts();

        space.status = ProjectSpaceStatus::Completed;
        space.delivery_manifest_ref = Some(format!("cas://sha256:{manifest_hash}"));
        space.version += 1;
        space.updated_at = now_ts();
        space.activity_stream.push(format!(
            "{} team.succeeded：交付 {} 项产物",
            now_ts(),
            final_artifacts.len()
        ));
        if let Err(error) = self.store.commit_team_delivery(&team, &space).await {
            self.persist_state(&state)?;
            return Err(error.into());
        }
        state = succeeded_state;

        // 模板提案是成功交付后的可选沉淀；不能让提案存储故障把已提交的交付
        // 伪装成 finalize 失败。
        if team.mode != TeamMode::Single {
            let meta = RunMeta::load(&self.run_dir, team_id)?;
            let proposal =
                self.build_template_proposal(team_id, &team, &state, &meta, &final_artifacts);
            match self.templates.save_proposal(&proposal) {
                Ok(()) => {
                    let _ = self
                        .space_activity(
                            team_id,
                            &format!(
                                "template.proposed：{}（只提案，未自动启用；采纳后进入模板注册表）",
                                proposal.proposal_id
                            ),
                        )
                        .await;
                    self.audit(
                        team_id,
                        "team.template_proposed",
                        format!("模板提案 {}（来源运行 {}）", proposal.proposal_id, team_id),
                    );
                }
                Err(error) => self.audit(
                    team_id,
                    "team.template_proposal_failed",
                    format!(
                        "交付已成功；模板提案 {} 落盘失败：{error}",
                        proposal.proposal_id
                    ),
                ),
            }
        }
        self.audit(
            team_id,
            "team.succeeded",
            format!("目标达成：{}", state.goal.objective),
        );
        // 终态落定：过期中断标记不再有意义。
        self.clear_interrupted_marker(team_id);
        Ok(team)
    }

    pub(crate) fn build_template_proposal(
        &self,
        team_id: &str,
        team: &TeamRun,
        state: &GoalRunState,
        meta: &RunMeta,
        final_artifacts: &[Value],
    ) -> TeamTemplateProposal {
        let roles: Vec<TeamTemplateRole> = meta
            .roles
            .iter()
            .map(|r| TeamTemplateRole {
                role: r.role.clone(),
                assignee: r.assignee.clone(),
                worker: r.worker.clone(),
                depends_on: r.depends_on.clone(),
                handoff_contract: r.handoff_contract.clone(),
                verify: r.verify.clone(),
                model: r.model.clone(),
                write_paths: r.write_paths.clone(),
                capabilities: r.capabilities.clone(),
            })
            .collect();
        let template = TeamTemplate {
            template_id: format!("tpl-{team_id}"),
            name: preview(&state.goal.objective, 60),
            mode: team.mode,
            roles,
            applicability: preview(&state.goal.objective, 200),
            source_team_id: Some(team_id.to_string()),
            created_at: now_ts(),
        };
        TeamTemplateProposal {
            proposal_id: format!("prop-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            template,
            source_team_id: team_id.to_string(),
            evidence: final_artifacts
                .iter()
                .filter_map(|a| {
                    a.get("artifact_id")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect(),
            status: TeamTemplateProposalStatus::Proposed,
            created_at: now_ts(),
        }
    }

    // -- 产物注册 / 接力（A3：handoff 使用结构化 context slice） --
}

#[cfg(test)]
mod validation_receipt_identity_tests {
    use super::{
        collect_attempt_changeset_evidence, make_validation_receipt, ValidationReceiptInput,
    };
    use crate::workswarm::delivery_gate_evidence::{
        attempt_changeset_contains_code, is_source_code_path, uncovered_source_paths,
    };
    use crate::plan::{ValidationVerdictV1, VerificationScopeV1};

    const STEP_SCOPE: VerificationScopeV1 = VerificationScopeV1::StepOutput;
    const MANUAL_SCOPE: VerificationScopeV1 = VerificationScopeV1::Manual;

    fn changeset(
        id: &str,
        status: owo_agent_protocol::ChangeSetStatus,
        created_at: &str,
    ) -> owo_agent_protocol::ChangeSet {
        owo_agent_protocol::ChangeSet {
            change_set_id: id.to_string(),
            team_id: "team-1".to_string(),
            step_id: "task-1".to_string(),
            attempt_id: Some("attempt-1".to_string()),
            role: "builder".to_string(),
            base_hashes: Vec::new(),
            result_hashes: Vec::new(),
            changed_files: vec![format!("src/{id}.rs")],
            diff_ref: None,
            status,
            created_at: created_at.to_string(),
            decision: (status == owo_agent_protocol::ChangeSetStatus::Accepted).then(|| {
                owo_agent_protocol::ChangeSetDecision {
                    action: "accept".to_string(),
                    idempotency_key: format!("accept-{id}"),
                    decided_at: created_at.to_string(),
                    note: None,
                }
            }),
            conflicts: Vec::new(),
        }
    }

    #[test]
    fn behavior_validation_scope_must_cover_every_changed_source_path() {
        use owo_agent_protocol::{ChangeSetFileHash, ChangeSetStatus};
        use crate::plan::{VerificationRequirementV1, VerificationResourcesV1, VerificationScopeV1};

        let mut change_set = changeset(
            "cs-source",
            ChangeSetStatus::Accepted,
            "2026-10-04",
        );
        change_set.changed_files = vec!["src/lib.rs".to_string(), "src/api.rs".to_string()];
        change_set.result_hashes = change_set
            .changed_files
            .iter()
            .map(|path| ChangeSetFileHash {
                path: path.clone(),
                sha256: Some("final-hash".to_string()),
                content_available: false,
            })
            .collect();
        let requirement = |paths: Vec<String>| VerificationRequirementV1 {
            requirement_id: "source-behavior".to_string(),
            covers_requirement_ids: Vec::new(),
            validator_id: "workspace-command-success-v1".to_string(),
            validator_version: Some("1".to_string()),
            scope: VerificationScopeV1::WorkspacePaths {
                relative_paths: paths,
            },
            arguments: serde_json::json!({"command":"cargo test -p owo-agent-core"}),
            required: true,
            resources: VerificationResourcesV1 {
                cpu_slots: 1,
                memory_mb: 8,
                exclusive_workspace: false,
                timeout_ms: 30_000,
            },
        };
        let plan = crate::plan::VerificationPlanV1 {
            plan_id: "source-plan".to_string(),
            requirements: vec![requirement(vec!["src/lib.rs".to_string()])],
        };
        assert_eq!(
            uncovered_source_paths(
                std::slice::from_ref(&change_set),
                "team-1",
                "task-1",
                "attempt-1",
                &plan,
            ),
            vec!["src/api.rs".to_string()]
        );
        let covered = crate::plan::VerificationPlanV1 {
            plan_id: "source-plan".to_string(),
            requirements: vec![requirement(vec![
                "src/lib.rs".to_string(),
                "src/api.rs".to_string(),
            ])],
        };
        assert!(uncovered_source_paths(
            std::slice::from_ref(&change_set),
            "team-1",
            "task-1",
            "attempt-1",
            &covered,
        )
        .is_empty());
    }

    #[test]
    fn host_classifies_source_from_paths_and_exact_attempt_not_artifact_label() {
        assert!(is_source_code_path("src/lib.rs"));
        assert!(is_source_code_path("apps/web/src/App.tsx"));
        assert!(is_source_code_path("package.json"));
        assert!(is_source_code_path("Sources/AppDelegate.m"));
        assert!(is_source_code_path("infra/main.tf"));
        assert!(is_source_code_path("CMakeLists.txt"));
        assert!(is_source_code_path("app/Example.csproj"));
        assert!(!is_source_code_path("docs/design.md"));
        assert!(!is_source_code_path("src"));

        let mut source_change = changeset(
            "cs-source",
            owo_agent_protocol::ChangeSetStatus::Accepted,
            "2026-10-04",
        );
        source_change.changed_files = vec!["src/lib.rs".to_string()];
        assert!(attempt_changeset_contains_code(
            std::slice::from_ref(&source_change),
            "team-1",
            "task-1",
            "attempt-1",
        ));
        assert!(!attempt_changeset_contains_code(
            std::slice::from_ref(&source_change),
            "team-1",
            "task-1",
            "attempt-stale",
        ));
        assert!(!attempt_changeset_contains_code(
            std::slice::from_ref(&source_change),
            "other-team",
            "task-1",
            "attempt-1",
        ));
    }

    fn input() -> ValidationReceiptInput<'static> {
        ValidationReceiptInput {
            team_id: "team-1",
            step_id: "task-1",
            attempts: 1,
            attempt_id: "attempt-1",
            epoch: 2,
            requirement_id: "req-1",
            scope: &STEP_SCOPE,
            validator_id: "artifact-output-non-empty-v1",
            validator_version: "1",
            arguments_sha256: "args",
            input_sha256: "input",
            artifact_id: "artifact-a",
            artifact_sha256: "same-content",
            changeset_sha256: Some("changeset-a".to_string()),
            changeset_refs: vec!["changeset://a".to_string()],
            evidence_ref: "cas://sha256:same-content",
            started_at: "2026-10-03T00:00:00Z",
            verdict: ValidationVerdictV1::Passed,
            detail: None,
            subject_hashes: std::collections::BTreeMap::new(),
            additional_evidence_refs: Vec::new(),
        }
    }

    #[test]
    fn review_artifacts_are_evidence_but_not_user_delivery_items() {
        let is_delivery = super::super::delivery_gate_evidence::is_user_delivery_artifact;
        assert!(is_delivery("code"));
        assert!(is_delivery("markdown"));
        assert!(!is_delivery("review"));
    }

    #[test]
    fn reviewer_cannot_pass_with_a_non_review_artifact() {
        let validate = super::super::delivery_gate_evidence::validate_review_artifact_kind;
        assert!(validate(true, "review").is_ok());
        assert!(validate(false, "code").is_ok());
        assert!(validate(true, "markdown")
            .unwrap_err()
            .contains("必须提交 review"));
        assert!(validate(false, "review")
            .unwrap_err()
            .contains("只有独立 reviewer"));
    }

    #[test]
    fn approved_review_cannot_hide_blockers_or_malformed_findings() {
        let approved = serde_json::json!({"verdict":"approved", "findings":[]});
        assert!(super::super::delivery_gate_evidence::validate_review_approval(&approved).is_ok());

        let hidden_blocker = serde_json::json!({
            "verdict":"approved",
            "findings":[{"severity":"blocker", "detail":"authentication bypass", "evidence_refs":[]}]
        });
        assert!(
            super::super::delivery_gate_evidence::validate_review_approval(&hidden_blocker)
                .unwrap_err()
                .contains("blocker")
        );

        let malformed = serde_json::json!({
            "verdict":"approved",
            "findings":[{"severity":"urgent", "detail":"unknown severity"}]
        });
        assert!(
            super::super::delivery_gate_evidence::validate_review_approval(&malformed).is_err()
        );

        let changes_requested = serde_json::json!({"verdict":"changes_requested", "findings":[]});
        assert!(
            super::super::delivery_gate_evidence::validate_review_approval(&changes_requested)
                .unwrap_err()
                .contains("未批准")
        );
    }

    #[test]
    fn delivery_requires_accepted_changesets() {
        use super::super::delivery_gate_evidence::changeset_delivery_error;
        use owo_agent_protocol::ChangeSetStatus;

        assert_eq!(changeset_delivery_error(ChangeSetStatus::Accepted), None);
        assert!(changeset_delivery_error(ChangeSetStatus::PendingReview).is_some());
        assert!(changeset_delivery_error(ChangeSetStatus::Conflicted).is_some());
        assert!(changeset_delivery_error(ChangeSetStatus::Rejected).is_some());
        assert!(changeset_delivery_error(ChangeSetStatus::Reverted).is_some());
    }

    #[test]
    fn changeset_evidence_checks_every_record_and_is_order_independent() {
        use owo_agent_protocol::ChangeSetStatus;

        let pending = changeset("cs-old", ChangeSetStatus::PendingReview, "2026-10-02");
        let accepted = changeset("cs-new", ChangeSetStatus::Accepted, "2026-10-03");
        let error = collect_attempt_changeset_evidence(
            "team-1",
            "task-1",
            "attempt-1",
            &[pending.clone(), accepted.clone()],
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("cs-old") && error.to_string().contains("仍待人工接受"),
            "{error}"
        );

        let mut accepted_without_decision = accepted.clone();
        accepted_without_decision.decision = None;
        let error = collect_attempt_changeset_evidence(
            "team-1",
            "task-1",
            "attempt-1",
            &[accepted_without_decision],
        )
        .unwrap_err();
        assert!(error.to_string().contains("缺少宿主接受决定"), "{error}");

        let accepted_old = changeset("cs-old", ChangeSetStatus::Accepted, "2026-10-02");
        let forward = collect_attempt_changeset_evidence(
            "team-1",
            "task-1",
            "attempt-1",
            &[accepted_old.clone(), accepted.clone()],
        )
        .unwrap();
        let reversed = collect_attempt_changeset_evidence(
            "team-1",
            "task-1",
            "attempt-1",
            &[accepted, accepted_old],
        )
        .unwrap();
        assert_eq!(forward, reversed);
        assert_eq!(forward.1, vec!["changeset://cs-new", "changeset://cs-old"]);
    }

    #[test]
    fn workspace_receipts_must_match_the_accepted_changeset_result_hash() {
        use owo_agent_protocol::{ChangeSetFileHash, ChangeSetStatus};

        let mut accepted = changeset("cs-source", ChangeSetStatus::Accepted, "2026-10-03");
        accepted.changed_files = vec!["src\\lib.rs".to_string()];
        accepted.result_hashes = vec![ChangeSetFileHash {
            path: "src/lib.rs".to_string(),
            sha256: Some("final-source-hash".to_string()),
            content_available: false,
        }];
        let scope = vec!["src\\lib.rs".to_string()];
        let evidence = std::collections::BTreeMap::from([(
            "workspace-path:src\\lib.rs".to_string(),
            "final-source-hash".to_string(),
        )]);
        let validate = super::super::delivery_gate_evidence::validate_workspace_receipt_snapshot;
        assert!(validate("task-1", "attempt-1", &scope, &evidence, &[accepted.clone()]).is_ok());

        let stale_evidence = std::collections::BTreeMap::from([(
            "workspace-path:src\\lib.rs".to_string(),
            "stale-source-hash".to_string(),
        )]);
        assert!(validate("task-1", "attempt-1", &scope, &stale_evidence, &[accepted.clone()])
            .unwrap_err()
            .contains("快照不一致"));

        assert!(validate("task-other", "attempt-1", &scope, &evidence, &[accepted]).is_ok());
    }

    #[test]
    fn review_source_snapshot_binds_the_seen_workspace_and_stable_changeset_bytes() {
        use owo_agent_protocol::{ChangeSetFileHash, ChangeSetStatus};

        let workspace = std::env::temp_dir().join(format!(
            "owo-review-snapshot-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(workspace.join("src")).expect("source directory");
        let source_path = workspace.join("src").join("task-1.rs");
        std::fs::write(&source_path, b"final source").expect("initial source");
        let mut accepted = changeset("cs-source", ChangeSetStatus::Accepted, "2026-10-03");
        accepted.changed_files = vec!["src/task-1.rs".to_string()];
        accepted.result_hashes = vec![ChangeSetFileHash {
            path: "src/task-1.rs".to_string(),
            sha256: Some(crate::CasStore::hash_of(b"final source")),
            content_available: false,
        }];

        let mut foreign_team_change = accepted.clone();
        foreign_team_change.team_id = "team-2".to_string();
        foreign_team_change.change_set_id = "cs-foreign-team".to_string();
        foreign_team_change.changed_files = vec!["src/foreign.rs".to_string()];
        let mixed_team_changes = [accepted.clone(), foreign_team_change];
        let accepted_snapshot =
            super::super::delivery_gate_evidence::review_source_snapshot(
                "team-1",
                "task-1",
                "attempt-1",
                &mixed_team_changes,
                Some(&workspace),
            );
        assert_eq!(
            accepted_snapshot
                .get("change_set_ids")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            accepted_snapshot
                .get("changeset_source_consistent")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            accepted_snapshot
                .get("contains_source_code")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let mut pending = accepted.clone();
        pending.status = ChangeSetStatus::PendingReview;
        pending.decision = None;
        let pending_snapshot = super::super::delivery_gate_evidence::review_source_snapshot(
            "team-1",
            "task-1",
            "attempt-1",
            &[pending],
            Some(&workspace),
        );
        assert_eq!(
            accepted_snapshot.get("change_set_sha256"),
            pending_snapshot.get("change_set_sha256"),
            "accept/review status transitions do not alter the reviewed source identity"
        );

        std::fs::write(&source_path, b"edited after review").expect("changed source");
        let stale_snapshot = super::super::delivery_gate_evidence::review_source_snapshot(
            "team-1",
            "task-1",
            "attempt-1",
            std::slice::from_ref(&accepted),
            Some(&workspace),
        );
        assert_ne!(
            accepted_snapshot.get("source_hashes"),
            stale_snapshot.get("source_hashes")
        );
        assert_eq!(
            stale_snapshot
                .get("changeset_source_consistent")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        std::fs::remove_dir_all(workspace).expect("remove temporary workspace");
    }

    #[test]
    fn behavior_receipts_require_approved_matching_attempt_and_final_source_hashes() {
        use owo_agent_protocol::{ChangeSetFileHash, ChangeSetStatus};
        use crate::plan::{VerificationRequirementV1, VerificationResourcesV1, VerificationScopeV1};

        let mut accepted = changeset("cs-source", ChangeSetStatus::Accepted, "2026-10-03");
        accepted.changed_files = vec!["src/lib.rs".to_string()];
        accepted.result_hashes = vec![ChangeSetFileHash {
            path: "src/lib.rs".to_string(),
            sha256: Some("source-final".to_string()),
            content_available: false,
        }];
        let requirement = VerificationRequirementV1 {
            requirement_id: "task-1:behavior".to_string(),
            covers_requirement_ids: Vec::new(),
            validator_id: "workspace-command-success-v1".to_string(),
            validator_version: Some("1".to_string()),
            scope: VerificationScopeV1::WorkspacePaths {
                relative_paths: vec!["src/lib.rs".to_string()],
            },
            arguments: serde_json::json!({"command":"cargo test -p owo-agent-core"}),
            required: true,
            resources: VerificationResourcesV1 {
                cpu_slots: 1,
                memory_mb: 8,
                exclusive_workspace: false,
                timeout_ms: 30_000,
            },
        };
        let command_hash = crate::CasStore::hash_of(b"cargo test -p owo-agent-core");
        let event = serde_json::json!({
            "step_id":"task-1",
            "attempt_id":"attempt-1",
            "receipt": {
                "command_sha256":command_hash,
                "exit_code":0,
                "result_sha256":"command-output-hash",
                "duration_ms":12,
                "workspace_hashes_complete":true,
                "validator_id":"workspace-command-success-v1",
                "validator_version":"1",
                "workspace_hashes":{"src/lib.rs":"source-final"}
            }
        })
        .to_string();
        let evaluate = super::super::delivery_gate_evidence::evaluate_workspace_command_receipt;
        let mut foreign_team_change = accepted.clone();
        foreign_team_change.team_id = "team-2".to_string();
        foreign_team_change.change_set_id = "cs-foreign-team".to_string();
        foreign_team_change.result_hashes[0].sha256 = Some("other-team-hash".to_string());
        let mixed_team_changes = [accepted.clone(), foreign_team_change];
        let (verdict, _, subjects, output_ref) = evaluate(
            "team-1",
            &requirement,
            std::slice::from_ref(&event),
            "task-1",
            "attempt-1",
            &mixed_team_changes,
        );
        assert_eq!(verdict, ValidationVerdictV1::Passed);
        assert_eq!(subjects.get("workspace-path:src/lib.rs"), Some(&"source-final".to_string()));
        assert_eq!(output_ref.as_deref(), Some("command-result:sha256:command-output-hash"));

        let bad_validator = event.replace(
            "\"validator_id\":\"workspace-command-success-v1\"",
            "\"validator_id\":\"unregistered-validator\"",
        );
        let (verdict, _, _, _) = evaluate(
            "team-1",
            &requirement,
            &[bad_validator],
            "task-1",
            "attempt-1",
            std::slice::from_ref(&accepted),
        );
        assert_eq!(verdict, ValidationVerdictV1::Unverified);

        let over_budget = event.replace("\"duration_ms\":12", "\"duration_ms\":30001");
        let (verdict, detail, _, _) = evaluate(
            "team-1",
            &requirement,
            &[over_budget],
            "task-1",
            "attempt-1",
            &[accepted.clone()],
        );
        assert_eq!(verdict, ValidationVerdictV1::Failed);
        assert!(detail.unwrap().contains("超过验证计划预算"));

        let (verdict, _, _, _) = evaluate(
            "team-1",
            &requirement,
            std::slice::from_ref(&event),
            "task-1",
            "attempt-old",
            &[accepted.clone()],
        );
        assert_eq!(verdict, ValidationVerdictV1::Unverified);

        let stale = event.replace("source-final", "source-before");
        let (verdict, detail, _, _) = evaluate(
            "team-1",
            &requirement,
            &[stale],
            "task-1",
            "attempt-1",
            &[accepted],
        );
        assert_eq!(verdict, ValidationVerdictV1::Failed);
        assert!(detail.unwrap().contains("快照不一致"));

        let mut deleted = changeset("cs-deleted", ChangeSetStatus::Accepted, "2026-10-04");
        deleted.changed_files = vec!["src/lib.rs".to_string()];
        deleted.result_hashes = vec![ChangeSetFileHash {
            path: "src/lib.rs".to_string(),
            sha256: None,
            content_available: false,
        }];
        let deleted_event = serde_json::json!({
            "step_id":"task-1",
            "attempt_id":"attempt-1",
            "receipt": {
                "command_sha256":command_hash,
                "exit_code":0,
                "result_sha256":"deletion-command-output-hash",
                "duration_ms":12,
                "workspace_hashes_complete":true,
                "validator_id":"workspace-command-success-v1",
                "validator_version":"1",
                "workspace_hashes":{"src/lib.rs":null}
            }
        })
        .to_string();
        let (verdict, _, subjects, _) = evaluate(
            "team-1",
            &requirement,
            &[deleted_event],
            "task-1",
            "attempt-1",
            &[deleted],
        );
        assert_eq!(verdict, ValidationVerdictV1::Passed);
        assert_eq!(
            subjects.get("workspace-path:src/lib.rs"),
            Some(&crate::verification::workspace_path_absence_sha256())
        );
    }

    #[test]
    fn receipt_identity_is_idempotent_but_separates_subject_and_scope() {
        let base_id = make_validation_receipt(input()).receipt_id;
        assert_eq!(base_id, make_validation_receipt(input()).receipt_id);

        let mut other_artifact = input();
        other_artifact.artifact_id = "artifact-b";
        assert_ne!(base_id, make_validation_receipt(other_artifact).receipt_id);

        let mut other_changeset = input();
        other_changeset.changeset_sha256 = Some("changeset-b".to_string());
        assert_ne!(base_id, make_validation_receipt(other_changeset).receipt_id);

        let mut other_scope = input();
        other_scope.scope = &MANUAL_SCOPE;
        assert_ne!(base_id, make_validation_receipt(other_scope).receipt_id);
    }
}
