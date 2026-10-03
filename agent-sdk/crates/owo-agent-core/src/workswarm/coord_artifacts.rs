use super::*;

pub(crate) fn workspace_change_status(path: &std::path::Path) -> Option<bool> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let records = value.as_array()?;
    if records.is_empty() {
        return None;
    }
    let mut changed = false;
    for record in records {
        let files = record.get("changed_files")?.as_array()?;
        changed |= !files.is_empty();
    }
    Some(changed)
}

fn context_capabilities(ctx: &Value) -> Vec<String> {
    ctx.get("capabilities")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

const MAX_SHARED_FACT_INLINE_BYTES: usize = 2400;
const MAX_SHARED_FACT_CONTEXT_BYTES: usize = 8 * 1024;

fn truncate_utf8_to_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn context_fact_matches_step(
    fact: &owo_agent_protocol::SharedContextFact,
    step: &crate::plan::StepSpec,
) -> bool {
    let assigned_task_id = step.input.get("assigned_task_id").and_then(Value::as_str);
    if fact.task_id.is_none()
        || fact.task_id.as_deref() == Some(step.id.as_str())
        || fact.task_id.as_deref() == assigned_task_id
    {
        return true;
    }
    ["assigned_read_refs", "assigned_contract_refs"]
        .iter()
        .filter_map(|key| step.input.get(*key).and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .any(|reference| fact.source_refs.iter().any(|source| source == reference))
}

impl TeamCoordinator {
    /// 步骤完成 → 版本化 Artifact（CAS ref）+ HandoffRecord + 项目空间更新 + 总线消息 + 审计。
    ///
    /// 由 [`RoleWorker`] 在 worker 成功后调用；人节点结果经 [`Self::record_human_result`]。
    pub async fn register_step_output(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &str,
    ) -> WorkSwarmResult<Artifact> {
        self.register_step_output_checked_bound(
            team_id,
            member_id,
            role,
            step_id,
            output,
            OutputAttemptBinding::default(),
        )
        .await
    }

    /// 带阶段代次校验的产物登记（legacy 纯文本路径，行为不变）：`phase_epoch`
    /// 与当前代次不一致（cancel/retry/replace 已接管现场）时，
    /// **只记审计事件，不创建 Artifact、不改状态**。
    ///
    /// `phase_epoch = None` 为兼容入口（人节点/诊断路径），跳过代次校验。
    /// 本路径不做格式门控（校验记录为 None），空内容照旧登记——
    /// 供 echo 演示 worker 与旧流程保持兼容；契约路径见
    /// [`TeamCoordinator::register_step_output_contract`]。
    pub async fn register_step_output_checked(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &str,
        phase_epoch: Option<u64>,
    ) -> WorkSwarmResult<Artifact> {
        self.register_step_output_checked_bound(
            team_id,
            member_id,
            role,
            step_id,
            output,
            OutputAttemptBinding {
                phase_epoch,
                attempt_id: None,
                reviewed_sources: None,
            },
        )
        .await
    }

    pub(super) async fn register_step_output_checked_bound(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &str,
        attempt: OutputAttemptBinding<'_>,
    ) -> WorkSwarmResult<Artifact> {
        let out = StepOutput {
            content: output.to_string(),
            kind: role_kind(role).to_string(),
            format: "text".to_string(),
            media_type: "text/plain".to_string(),
            file_name: file_name_of(role_kind(role), "text"),
            evidence_refs: Vec::new(),
            open_issues: None,
            known_risks: None,
            validation: None,
            handoff_note: None,
        };
        self.register_step_output_inner(team_id, member_id, role, step_id, &out, attempt)
            .await
    }

    /// 结构化契约产物登记（七期 · 第三路）：Worker 输出经输出契约（V1）解析后，
    /// 以 [`WorkerOutputV1`] 提交——交付元数据（format/media_type/file_name/
    /// sha256/size_bytes）、证据链（evidence_refs/open_issues/validation）与
    /// 交接说明（handoff_note）随 Artifact 与 HandoffRecord 落盘，供下载交付
    /// 端点与交付清单使用。
    ///
    /// **格式门控（登记前）**：有效格式（[`effective_format`]）未通过
    /// [`validate_artifact_content`] 的产物**不登记**——不进 CAS、不进版本链、
    /// 不进 PendingReview、不写 HandoffRecord，只记审计事件并返回
    /// `Run("artifact_invalid: …")`（步骤失败，可局部重试）。
    ///
    /// critic 角色登记评审结论（kind=review/markdown），不做格式门控；
    /// producer 必须携带 artifact（缺失即 Validation 错误）。
    pub async fn register_step_output_contract(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &WorkerOutputV1,
        phase_epoch: Option<u64>,
    ) -> WorkSwarmResult<Artifact> {
        self.register_step_output_contract_bound(
            team_id,
            member_id,
            role,
            step_id,
            output,
            OutputAttemptBinding {
                phase_epoch,
                attempt_id: None,
                reviewed_sources: None,
            },
        )
        .await
    }

    pub(super) async fn register_step_output_contract_bound(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &WorkerOutputV1,
        attempt: OutputAttemptBinding<'_>,
    ) -> WorkSwarmResult<Artifact> {
        let reviewer = RunMeta::load(&self.run_dir, team_id)
            .ok()
            .and_then(|meta| {
                Self::role_spec_of_member(&meta, member_id)
                    .ok()
                    .map(|spec| super::util::is_review_role(&spec.role, &spec.capabilities))
            })
            .unwrap_or_else(|| is_review_role_name(role));
        if reviewer {
            let result = output.review_result.as_ref().ok_or_else(|| {
                WorkSwarmError::Validation(
                    "review capability 必须提交结构化 review_result".to_string(),
                )
            })?;
            // 审查范围必须是 Worker 实际收到的宿主上下文快照，且在提交时仍然有效。
            let context = self
                .assemble_context_slice(team_id, member_id, step_id)
                .await?;
            let supplied_sources = attempt.reviewed_sources.ok_or_else(|| {
                WorkSwarmError::Validation(
                    "review capability 缺少 Worker 实际读取的宿主源码快照".to_string(),
                )
            })?;
            let current_upstream = context
                .get("upstream")
                .and_then(Value::as_array)
                .ok_or_else(|| WorkSwarmError::Validation("review context 缺少 upstream".to_string()))?;
            let reviewed_artifacts = current_upstream
                .iter()
                .map(|artifact| {
                    let producer = artifact
                        .get("producer")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if producer == member_id {
                        return Err(WorkSwarmError::Validation(
                            "独立 reviewer 不能评审自己生产的产物".to_string(),
                        ));
                    }
                    let artifact_id = artifact
                        .get("artifact_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| WorkSwarmError::Validation("上游产物缺少 artifact_id".to_string()))?;
                    let seen = supplied_sources
                        .iter()
                        .find(|item| item.get("artifact_id").and_then(Value::as_str) == Some(artifact_id))
                        .ok_or_else(|| WorkSwarmError::Conflict(format!(
                            "reviewer 未收到当前上游产物 {artifact_id}"
                        )))?;
                    if seen.get("sha256") != artifact.get("sha256")
                        || seen.get("reviewed_source") != artifact.get("reviewed_source")
                    {
                        return Err(WorkSwarmError::Conflict(format!(
                            "reviewer 的 Artifact 或源码快照已过期：{artifact_id}"
                        )));
                    }
                    let reviewed_source = artifact
                        .get("reviewed_source")
                        .cloned()
                        .unwrap_or(Value::Null);
                    if super::delivery_gate_evidence::is_code_artifact_kind(
                        artifact.get("kind").and_then(Value::as_str).unwrap_or_default(),
                    ) && (reviewed_source.get("workspace_observed").and_then(Value::as_bool) != Some(true)
                        || reviewed_source.get("change_set_ids").and_then(Value::as_array).is_none_or(Vec::is_empty)
                        || reviewed_source.get("changeset_source_consistent").and_then(Value::as_bool) != Some(true)
                        || reviewed_source
                            .get("source_hashes")
                            .and_then(Value::as_object)
                            .is_none_or(|hashes| {
                                hashes.is_empty()
                                    || hashes.values().any(|entry| {
                                        entry.get("observed").and_then(Value::as_bool) != Some(true)
                                    })
                            })
                    ) {
                        return Err(WorkSwarmError::Validation(
                            "代码评审没有可核对的 ChangeSet 与工作区源码快照".to_string(),
                        ));
                    }
                    Ok(json!({
                        "artifact_id": artifact.get("artifact_id").cloned().unwrap_or(Value::Null),
                        "version": artifact.get("version").cloned().unwrap_or(Value::Null),
                        "sha256": artifact.get("sha256").cloned().unwrap_or(Value::Null),
                        "producer": producer,
                        "reviewed_source": reviewed_source,
                    }))
                })
                .collect::<WorkSwarmResult<Vec<_>>>()?;
            if supplied_sources.len() != current_upstream.len() {
                return Err(WorkSwarmError::Conflict(
                    "reviewer 上下文包含的上游产物集合已变化".to_string(),
                ));
            }
            if reviewed_artifacts.is_empty() {
                return Err(WorkSwarmError::Validation(
                    "review capability 没有可绑定的上游产物快照".to_string(),
                ));
            }
            let review_document = json!({
                "schema": "team-review-result-v1",
                "reviewer_id": member_id,
                "reviewed_artifacts": reviewed_artifacts,
                "result": result,
            });
            let review_content = serde_json::to_string_pretty(&review_document)
                .map_err(|e| WorkSwarmError::Run(format!("ReviewResult 序列化失败：{e}")))?;
            let blocking_issues = result
                .findings
                .iter()
                .filter(|finding| {
                    finding.severity == crate::workswarm_output::WorkerReviewSeverity::Blocker
                })
                .map(|finding| finding.detail.clone())
                .collect::<Vec<_>>();
            let mut review_evidence_refs = evidence_refs_of(&output.evidence);
            review_evidence_refs.extend(
                result
                    .findings
                    .iter()
                    .flat_map(|finding| finding.evidence_refs.iter().cloned()),
            );
            review_evidence_refs.sort();
            review_evidence_refs.dedup();
            // review capability：结构化结论、身份、不可变快照和阻断项共同封存。
            let out = StepOutput {
                content: review_content,
                kind: "review".to_string(),
                format: "json".to_string(),
                media_type: "application/json".to_string(),
                file_name: file_name_of("review", "json"),
                evidence_refs: review_evidence_refs,
                open_issues: Some(blocking_issues),
                known_risks: Some(Vec::new()),
                validation: None,
                handoff_note: Some(output.summary.clone()),
            };
            return self
                .register_step_output_inner(team_id, member_id, role, step_id, &out, attempt)
                .await;
        }

        // producer：交付物正文 + 声明格式。kind 取交付物声明的产物分类
        //（空则回退角色链 kind），驱动文件名与有效格式（research 证据链规则）。
        let declared = output.artifact.as_ref().ok_or_else(|| {
            WorkSwarmError::Validation("producer 契约产物必须携带 artifact".to_string())
        })?;
        let chain_kind = role_kind(role).to_string();
        let declared_kind = declared.kind.trim();
        let kind_for_meta = if declared_kind.is_empty() {
            chain_kind.clone()
        } else {
            declared_kind.to_string()
        };
        let eff = effective_format(&declared.format, &kind_for_meta);
        self.audit(
            team_id,
            "team.artifact.validation_started",
            format!("member={member_id} step={step_id} format={eff}"),
        );
        let validation_started = std::time::Instant::now();
        let validation = validate_artifact_content(&eff, &declared.content, &output.evidence);
        let validation_ms = validation_started.elapsed().as_millis() as u64;
        if !validation.valid {
            // 门控（登记前）：未通过格式校验的产物不进任何登记流程。
            self.audit(
                team_id,
                "team.artifact.validation_rejected",
                format!(
                    "产物格式校验未通过（{eff}，{}）：member={member_id} step={step_id} duration_ms={validation_ms}，不登记",
                    validation.reason.as_deref().unwrap_or("")
                ),
            );
            return Err(WorkSwarmError::Run(format!(
                "artifact_invalid: {}",
                validation.reason.as_deref().unwrap_or("未知原因")
            )));
        }
        self.audit(
            team_id,
            "team.artifact.validation_passed",
            format!(
                "member={member_id} step={step_id} kind={kind_for_meta} format={eff} duration_ms={validation_ms}"
            ),
        );
        let out = StepOutput {
            content: declared.content.clone(),
            kind: chain_kind,
            format: eff.clone(),
            media_type: media_type_of(&eff).to_string(),
            file_name: file_name_of(&kind_for_meta, &eff),
            evidence_refs: evidence_refs_of(&output.evidence),
            open_issues: Some(output.open_issues.clone()),
            known_risks: Some(Vec::new()),
            validation: Some(validation),
            handoff_note: output.handoff.clone(),
        };
        self.register_step_output_inner(team_id, member_id, role, step_id, &out, attempt)
            .await
    }

    /// 产物登记内部实现（legacy / 契约两路径共用）：CAS 落盘、版本链、
    /// Artifact / HandoffRecord 持久化、空间活动流与总线交接消息。
    /// 格式门控已在契约路径入口完成，此处假定内容已通过（或无需门控）。
    pub(crate) async fn register_step_output_inner(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        out: &StepOutput,
        attempt: OutputAttemptBinding<'_>,
    ) -> WorkSwarmResult<Artifact> {
        if let Some(epoch) = attempt.phase_epoch {
            let current = self.phase_epoch(team_id);
            if current != epoch {
                self.audit(
                    team_id,
                    "team.phase.stale_drop",
                    format!(
                        "过期阶段产物回传丢弃：member={member_id} step={step_id} epoch={epoch}（当前 {current}）"
                    ),
                );
                return Err(WorkSwarmError::Conflict(format!(
                    "阶段已过期（epoch {epoch} < {current}）：回传结果已丢弃（cancel/retry/replace 已接管）"
                )));
            }
        }
        let (_team, space, mut state) = self.load_bundle(team_id).await?;
        if let Some(expected_attempt_id) = attempt.attempt_id {
            let record = state
                .records
                .get(step_id)
                .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {step_id} 缺少执行记录")))?;
            if record.attempt_id.as_deref() != Some(expected_attempt_id)
                || attempt
                    .phase_epoch
                    .is_none_or(|epoch| record.phase_epoch != Some(epoch))
            {
                self.audit(
                    team_id,
                    "team.phase.stale_drop",
                    format!("过期 attempt 产物回传丢弃：step={step_id}"),
                );
                return Err(WorkSwarmError::Conflict(format!(
                    "任务 {step_id} 的 attempt/epoch 已失效，回传结果已丢弃"
                )));
            }
        } else {
            // Legacy/direct registration has no worker-supplied identity. Bind it to
            // the active host attempt, or mint and persist an explicit compatibility
            // attempt before publishing any artifact. Production RoleWorker paths use
            // the bound API above and cannot refresh a stale attempt this way.
            let current_epoch = self.phase_epoch(team_id);
            let record = state
                .records
                .get(step_id)
                .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {step_id} 缺少执行记录")))?;
            if record
                .phase_epoch
                .is_some_and(|epoch| epoch != current_epoch)
            {
                self.audit(
                    team_id,
                    "team.phase.stale_drop",
                    format!("过期兼容产物回传丢弃：step={step_id}"),
                );
                return Err(WorkSwarmError::Conflict(format!(
                    "任务 {step_id} 的阶段已失效，兼容回传结果已丢弃"
                )));
            }
            if record.attempt_id.is_none() || record.phase_epoch.is_none() {
                let record = state
                    .records
                    .get_mut(step_id)
                    .expect("record checked above");
                record.attempt_id = Some(format!(
                    "{team_id}:{step_id}:legacy:{}",
                    uuid::Uuid::new_v4()
                ));
                record.phase_epoch = Some(current_epoch);
                record.attempts = record.attempts.saturating_add(1);
                self.persist_state(&state)?;
                self.audit(
                    team_id,
                    "team.attempt.legacy_registration",
                    format!("兼容产物登记由宿主创建 attempt：step={step_id}"),
                );
            }
        }
        let project_id = space.project_id.clone();
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let correlation = meta.correlation_id.clone();

        let kind = out.kind.clone();
        let version = self.next_artifact_version(&space, role).await?;
        let hash = self
            .cas
            .put(out.content.as_bytes())
            .map_err(|e| WorkSwarmError::Run(format!("产物 CAS 落盘失败：{e}")))?;
        let content_ref = format!("cas://sha256:{hash}");

        // 来源引用：直接上游的最新产物（ref 传递）。
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
        let mut source_refs: Vec<String> = Vec::new();
        for dep in &step.depends_on {
            let Some(dep_step) = state
                .plan
                .steps
                .iter()
                .find(|s| s.id.as_str() == dep.as_str())
            else {
                continue;
            };
            let Some(_dep_role) = worker_role(&dep_step.worker) else {
                continue;
            };
            if let Some(a) = self
                .latest_artifact_for_step(&space, &state, &dep_step.id)
                .await
            {
                source_refs.push(a.artifact_id);
            }
        }

        // 五期：返工重跑登记 → supersedes 指向前版（版本链合并；approved head
        // 不受影响，仍由评审闭环在 v2 批准时切换）。非返工登记保持 None。
        let is_rework = step
            .input
            .get("rework")
            .and_then(|r| r.get("instruction"))
            .map(|v| !v.as_str().unwrap_or_default().trim().is_empty())
            .unwrap_or(false);
        let mut supersedes_artifact_id: Option<String> = None;
        let mut retire_prev: Option<Artifact> = None;
        if is_rework {
            if let Some(prev) = self.latest_artifact_for_task(&space, step_id).await {
                if prev.review_state != ReviewState::Superseded {
                    supersedes_artifact_id = Some(prev.artifact_id.clone());
                    if prev.review_state != ReviewState::Approved {
                        let mut retired = prev.clone();
                        retired.review_state = ReviewState::Superseded;
                        retire_prev = Some(retired);
                    }
                }
            }
        }
        let parsed = Self::parse_optional_json_lists(&out.content);
        let open_issues = out.open_issues.clone().unwrap_or_else(|| parsed.0.clone());
        let known_risks = out.known_risks.clone().unwrap_or(parsed.1);
        let attempt_id = state
            .records
            .get(step_id)
            .and_then(|record| record.attempt_id.clone());
        let artifact = Artifact {
            artifact_id: format!("{team_id}:{role}:v{version}"),
            kind,
            version,
            producer: member_id.to_string(),
            content_ref: content_ref.clone(),
            schema_ref: None,
            source_refs,
            classification: ArtifactClassification::Private,
            review_state: if out.kind == "review" {
                ReviewState::PendingReview
            } else {
                ReviewState::Draft
            },
            supersedes_artifact_id,
            created_at: now_ts(),
            team_id: team_id.to_string(),
            task_id: Some(step_id.to_string()),
            attempt_id,
            format: out.format.clone(),
            media_type: out.media_type.clone(),
            file_name: out.file_name.clone(),
            sha256: hash.clone(),
            size_bytes: out.content.len() as u64,
            evidence_refs: out.evidence_refs.clone(),
            open_issues: open_issues.clone(),
            validation: out.validation.clone(),
            handoff: out.handoff_note.clone(),
        };
        self.store.save_artifact(&artifact, &project_id).await?;
        // 返工登记：前版让位（Superseded）——已批准前版不动（head 语义归评审闭环）。
        if let Some(retired) = retire_prev {
            self.audit(
                team_id,
                "artifact.rework.supersede",
                format!(
                    "返工重跑登记 {}，前版 {} 让位（Superseded）",
                    artifact.artifact_id, retired.artifact_id
                ),
            );
            self.store.save_artifact(&retired, &project_id).await?;
        }

        // 交接（结构化 context slice 的摘要视图；完整内容在 CAS，下游按 ref 读取）。
        let downstream: Vec<&StepSpec> = state
            .plan
            .steps
            .iter()
            .filter(|s| s.depends_on.iter().any(|d| d == step_id))
            .collect();
        let to_member = downstream
            .first()
            .map(|d| d.worker.clone())
            .unwrap_or_else(|| "*".to_string());
        // 证据链（同源）：CAS 内容引用 + Worker 证据引用。
        let handoff_evidence_refs = {
            let mut refs = vec![content_ref];
            refs.extend(out.evidence_refs.iter().cloned());
            refs
        };
        let handoff = HandoffRecord {
            handoff_id: format!("{team_id}:{step_id}:v{version}"),
            from_member: member_id.to_string(),
            to_member: to_member.clone(),
            completed_summary: preview(&out.content, 500),
            open_issues,
            output_artifact_refs: vec![artifact.artifact_id.clone()],
            evidence_refs: handoff_evidence_refs,
            suggested_next_actions: downstream
                .iter()
                .filter_map(|d| {
                    let r = worker_role(&d.worker)?;
                    Some(format!("{r}：{}", self.contract_of(&meta, &r)))
                })
                .collect(),
            known_risks,
            created_at: now_ts(),
            handoff_note: out.handoff_note.clone(),
        };
        self.store.save_handoff(&handoff, &project_id).await?;

        let mut new_space = space;
        new_space.artifacts.push(artifact.artifact_id.clone());
        new_space.version += 1;
        if is_rework {
            if let Some(previous_artifact_id) = artifact.supersedes_artifact_id.as_deref() {
                for rework in &mut new_space.rework_tasks {
                    if rework.team_id == team_id
                        && rework.step_id == step_id
                        && rework.artifact_id == previous_artifact_id
                        && matches!(
                            rework.status,
                            owo_agent_protocol::ArtifactReworkStatus::Dispatching
                                | owo_agent_protocol::ArtifactReworkStatus::Requested
                        )
                    {
                        rework.status = owo_agent_protocol::ArtifactReworkStatus::Completed;
                        rework.reworked_artifact_id = Some(artifact.artifact_id.clone());
                        rework.error.clear();
                    }
                }
            }
        }
        new_space.updated_at = now_ts();
        new_space.activity_stream.push(format!(
            "{} step.completed {step_id} → {}",
            now_ts(),
            artifact.artifact_id
        ));
        if new_space.activity_stream.len() > 200 {
            let drain = new_space.activity_stream.len() - 200;
            new_space.activity_stream.drain(..drain);
        }
        self.store.save_project_space(&new_space).await?;

        // 总线：交接消息（correlation_id 贯通；关键消息溢出拒绝不丢弃）。
        for d in &downstream {
            let _ = self
                .bus
                .send(
                    member_id,
                    &d.worker,
                    MessageKind::Task,
                    correlation.clone(),
                    serde_json::to_value(&handoff).unwrap_or(Value::Null),
                    OverflowPolicy::Reject,
                )
                .await;
        }
        let artifact_id = artifact.artifact_id.clone();
        self.audit(
            team_id,
            "team.handoff",
            format!(
                "{member_id}({role}) 交付 {artifact_id} → {to_member}（correlation={correlation}）"
            ),
        );
        Ok(artifact)
    }

    pub(crate) fn contract_of(&self, meta: &RunMeta, role: &str) -> String {
        meta.roles
            .iter()
            .find(|r| r.role == role)
            .and_then(|r| r.handoff_contract.clone())
            .unwrap_or_else(|| "按角色职责继续".to_string())
    }

    pub(crate) async fn next_artifact_version(
        &self,
        space: &ProjectSpace,
        role: &str,
    ) -> WorkSwarmResult<u32> {
        let identity = format!(":{role}:v");
        let mut max = 0u32;
        for id in &space.artifacts {
            if let Ok(artifact) = self.store.get_artifact(id).await {
                if artifact.artifact_id.contains(&identity) && artifact.version > max {
                    max = artifact.version;
                }
            }
        }
        Ok(max + 1)
    }

    /// 依赖产物必须同时匹配 task_id 与当前宿主 attempt_id；历史/迟到 attempt 不参与读取。
    pub(crate) async fn latest_artifact_for_step(
        &self,
        space: &ProjectSpace,
        state: &GoalRunState,
        step_id: &str,
    ) -> Option<Artifact> {
        let record = state.records.get(step_id)?;
        let attempt_id = record.attempt_id.as_deref()?;
        let mut best: Option<Artifact> = None;
        for id in &space.artifacts {
            if let Ok(artifact) = self.store.get_artifact(id).await {
                if artifact.task_id.as_deref() == Some(step_id)
                    && artifact.attempt_id.as_deref() == Some(attempt_id)
                    && best
                        .as_ref()
                        .is_none_or(|current| artifact.created_at > current.created_at)
                {
                    best = Some(artifact);
                }
            }
        }
        best
    }

    /// 返工版本链只从同一 task_id 选择前版，避免同角色的其他任务串链。
    pub(crate) async fn latest_artifact_for_task(
        &self,
        space: &ProjectSpace,
        step_id: &str,
    ) -> Option<Artifact> {
        let mut best: Option<Artifact> = None;
        for id in &space.artifacts {
            if let Ok(artifact) = self.store.get_artifact(id).await {
                if artifact.task_id.as_deref() == Some(step_id)
                    && best
                        .as_ref()
                        .is_none_or(|current| artifact.created_at > current.created_at)
                {
                    best = Some(artifact);
                }
            }
        }
        best
    }

    // -- 上下文切片（handoff 的运行时视图；A3 结构化 context slice） --

    /// 为 (member, step) 组装结构化上下文切片：
    /// `{ team_id, objective, role, handoff_contract, upstream: [{role, artifact_id, version, content, review_state}] }`。
    pub async fn assemble_context_slice(
        &self,
        team_id: &str,
        member_id: &str,
        step_id: &str,
    ) -> WorkSwarmResult<Value> {
        let (team, space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let spec = Self::role_spec_of_member(&meta, member_id)?;
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
        let mut core_specs = Vec::new();
        for reference in &team.shared_context_refs {
            let raw = self.cas_content_text(reference);
            if let Ok(snapshot) = serde_json::from_str::<Value>(&raw) {
                if snapshot.get("kind").and_then(Value::as_str) == Some("source_session_context_v1")
                {
                    if let Some(spec) = snapshot.get("core_spec") {
                        core_specs.push(spec.clone());
                    }
                }
            }
        }
        let shared_context = self
            .store
            .get_team_context(team_id)
            .await
            .map_err(|e| WorkSwarmError::Run(e.to_string()))?;
        let mut shared_facts = Vec::new();
        let mut latest_fact_keys = HashSet::new();
        // Keep shared facts within the same scale as upstream artifacts; workers can fetch full CAS content on demand.
        let mut fact_budget = MAX_SHARED_FACT_CONTEXT_BYTES;
        for fact in shared_context.facts.iter().rev().take(128) {
            if !latest_fact_keys.insert(fact.key.as_str()) {
                continue;
            }
            // A file-hash-bound fact must be checked against the bound workspace by
            // the server before it can enter a model prompt. Workers can request it
            // through team_context_read, which performs that freshness check.
            if fact.file_hash.is_some() {
                continue;
            }
            if !context_fact_matches_step(fact, step) {
                continue;
            }
            if fact.status != "candidate" && fact.status != "confirmed" {
                continue;
            }
            if fact_budget == 0 {
                break;
            }
            let full = self.cas_content_text(&fact.value_ref);
            if full.is_empty() {
                continue;
            }
            let (value, truncated) =
                truncate_utf8_to_bytes(&full, fact_budget.min(MAX_SHARED_FACT_INLINE_BYTES));
            let value = value.to_string();
            fact_budget = fact_budget.saturating_sub(value.len());
            shared_facts.push(json!({
                "key": fact.key, "value": value, "value_ref": fact.value_ref,
                "truncated": truncated,
                "revision": fact.revision, "producer": fact.producer,
                "task_id": fact.task_id, "source_refs": fact.source_refs,
                "file_hash": fact.file_hash, "confidence": fact.confidence,
                "status": fact.status
            }));
        }
        let review_change_sets = crate::change_set_store::ChangeSetStore::new(&self.run_dir)
            .list_for_team(team_id)
            .map_err(|error| WorkSwarmError::Run(format!("Review ChangeSet 读取失败：{error}")))?;
        let review_workspace = self.verification_workspace(team_id);
        let mut upstream = Vec::new();
        for dep in &step.depends_on {
            let dep_step = match state
                .plan
                .steps
                .iter()
                .find(|s| s.id.as_str() == dep.as_str())
            {
                Some(s) => s,
                None => continue,
            };
            let Some(dep_role) = worker_role(&dep_step.worker) else {
                continue;
            };
            if let Some(a) = self
                .latest_artifact_for_step(&space, &state, &dep_step.id)
                .await
            {
                let content = self.cas_content_text(&a.content_ref);
                upstream.push(json!({
                    "role": dep_role,
                    "task_id": a.task_id,
                    "attempt_id": a.attempt_id,
                    "artifact_id": a.artifact_id,
                    "kind": a.kind,
                    "reviewed_source": super::delivery_gate_evidence::review_source_snapshot(
                        &dep_step.id,
                        a.attempt_id.as_deref().unwrap_or_default(),
                        &review_change_sets,
                        review_workspace.as_deref(),
                    ),
                    "version": a.version,
                    "content": content,
                    // 评审绑定实际读取到的 CAS 字节，不信任模型声明或可变角色索引。
                    "sha256": CasStore::hash_of(content.as_bytes()),
                    "producer": a.producer,
                    // 八期一路：CAS ref 随切片透出（大 Artifact 摘要块需带哈希与 ref）。
                    "cas_ref": a.content_ref,
                    "review_state": format!("{:?}", a.review_state),
                }));
            }
        }
        let mut review_handoff_contract = spec.handoff_contract.clone();
        if super::util::is_review_role(&spec.role, &spec.capabilities) {
            let source_manifest = upstream
                .iter()
                .filter_map(|artifact| {
                    let source = artifact.get("reviewed_source")?;
                    let hashes = source.get("source_hashes")?.as_object()?;
                    (!hashes.is_empty()).then(|| {
                        json!({
                            "artifact_id": artifact.get("artifact_id"),
                            "task_id": artifact.get("task_id"),
                            "attempt_id": artifact.get("attempt_id"),
                            "kind": artifact.get("kind"),
                            "change_set_ids": source.get("change_set_ids"),
                            "change_set_sha256": source.get("change_set_sha256"),
                            "source_hashes": hashes,
                        })
                    })
                })
                .collect::<Vec<_>>();
            let manifest = serde_json::to_string(&source_manifest)
                .map_err(|error| WorkSwarmError::Serialization(error.to_string()))?;
            if manifest.len() > 64 * 1024 {
                return Err(WorkSwarmError::Validation(
                    "Reviewer 源码快照超过 64 KiB 上限，拒绝以不完整清单继续评审".to_string(),
                ));
            }
            if !source_manifest.is_empty() {
                let base = review_handoff_contract
                    .as_deref()
                    .unwrap_or_default()
                    .trim();
                review_handoff_contract = Some(format!(
                    "{base}

宿主绑定的最终源码审查清单（只读）：{manifest}。请逐个读取清单中的工作区文件，按实际源码提交 findings；交付门会校验这些哈希在评审期间和交付时未变化。"
                ));
            }
        }
        Ok(json!({
            "team_id": team_id,
            "objective_text": state.goal.objective,
            "role": spec.role,
            "capabilities": spec.capabilities,
            "write_paths": spec.write_paths,
            "member_id": member_id,
            "handoff_contract": review_handoff_contract,
            "core_spec": core_specs,
            "shared_context_revision": shared_context.revision,
            "shared_facts": shared_facts,
            // 八期一路：模板 id + 角色调用预算（角色专属 Prompt 编译输入）。
            "template_id": meta.template_id,
            "budget_calls": meta.budgets.get(&spec.role).copied().unwrap_or(0),
            "retry_note": step.input.get("_workswarm").and_then(|meta| meta.get("retry_note")),
            "upstream": upstream,
        }))
    }

    /// Read a dependency artifact by id with step-level authorization and bounded text output.
    pub async fn read_dependency_artifact(
        &self,
        team_id: &str,
        member_id: &str,
        step_id: &str,
        artifact_id: &str,
        max_bytes: usize,
    ) -> WorkSwarmResult<Value> {
        let (_team, space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let _spec = Self::role_spec_of_member(&meta, member_id)?;
        let step = state
            .plan
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
        for dependency in &step.depends_on {
            let Some(dep_step) = state.plan.steps.iter().find(|item| item.id == *dependency) else {
                continue;
            };
            let Some(_dep_role) = worker_role(&dep_step.worker) else {
                continue;
            };
            let Some(artifact) = self
                .latest_artifact_for_step(&space, &state, &dep_step.id)
                .await
            else {
                continue;
            };
            if artifact.artifact_id != artifact_id || artifact.team_id != team_id {
                continue;
            }
            let full = self.cas_content_text(&artifact.content_ref);
            if full.is_empty() {
                return Err(WorkSwarmError::NotFound(format!(
                    "产物正文不存在：{artifact_id}"
                )));
            }
            let budget = max_bytes.clamp(1, 64 * 1024);
            let content = full
                .char_indices()
                .take_while(|(offset, ch)| offset + ch.len_utf8() <= budget)
                .map(|(_, ch)| ch)
                .collect::<String>();
            return Ok(json!({
                "artifact_id": artifact.artifact_id,
                "kind": artifact.kind,
                "version": artifact.version,
                "producer": artifact.producer,
                "task_id": artifact.task_id,
                "attempt_id": artifact.attempt_id,
                "review_state": format!("{:?}", artifact.review_state),
                "content_ref": artifact.content_ref,
                "content": content,
                "truncated": content.len() < full.len()
            }));
        }
        Err(WorkSwarmError::Validation(format!(
            "产物不属于当前步骤的直接依赖：{artifact_id}"
        )))
    }

    pub(crate) fn cas_content_text(&self, content_ref: &str) -> String {
        content_ref
            .strip_prefix("cas://sha256:")
            .and_then(|h| self.cas.get_text(h))
            .unwrap_or_default()
    }

    /// 八期一路：自适应指标追加落盘（best-effort——任何失败都不阻塞运行）。
    ///
    /// 事件写入 `strategy_decision.adaptive.events`（上限 64 条），并按事件种类
    /// 维护聚合字段：`context_bytes_total`（context 事件累计）、`runtime_skipped`
    /// （运行期跳过名单，上限 16 条）、`early_exit`（提前结束原因）。事件 kind：
    /// `context` | `role_skipped` | `early_exit`；第四路 UI 直接读 strategy_decision。
    pub async fn note_adaptive_event(&self, team_id: &str, event: Value) {
        // 仅把白名单诊断字段送入实时事件流；不把 context 详情或任意 JSON 写进审计。
        let kind = event
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let role = event
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let step_id = event
            .get("step_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let context_bytes = event
            .get("context_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let audit_event = match kind {
            "context" => "team.context.revision",
            "role_skipped" => "team.task.skipped",
            "early_exit" => "team.execution.early_exit",
            _ => "team.runtime.diagnostic",
        };
        self.audit(
            team_id,
            audit_event,
            format!("kind={kind} role={role} step_id={step_id} context_bytes={context_bytes}"),
        );
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let Ok(mut team) = self.store.get_team_run(team_id).await else {
            return;
        };
        let mut sd = team.strategy_decision.clone().unwrap_or_else(|| json!({}));
        if !sd.is_object() {
            sd = json!({});
        }
        if let Some(obj) = sd.as_object_mut() {
            let adaptive = obj
                .entry("adaptive".to_string())
                .or_insert_with(|| json!({}));
            if !adaptive.is_object() {
                *adaptive = json!({});
            }
            if let Some(a) = adaptive.as_object_mut() {
                if let Some(bytes) = event.get("context_bytes").and_then(Value::as_u64) {
                    let total = a
                        .get("context_bytes_total")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let sum = total + bytes;
                    // 八期四路冻结口径：`context_bytes`（平铺）；保留 `context_bytes_total` 同值别名。
                    a.insert("context_bytes".to_string(), json!(sum));
                    a.insert("context_bytes_total".to_string(), json!(sum));
                }
                if let Some(skip) = event.get("role_skipped") {
                    let arr = a
                        .entry("runtime_skipped".to_string())
                        .or_insert_with(|| json!([]));
                    if let Some(list) = arr.as_array_mut() {
                        if list.len() < 16 {
                            list.push(skip.clone());
                        }
                    }
                    // 冻结口径 `skip_reason`：最近一次运行期跳过原因（逐角色原因在
                    // skipped_roles[].reason / runtime_skipped[].reason）。
                    if let Some(reason) = skip.get("reason").and_then(Value::as_str) {
                        a.insert("skip_reason".to_string(), json!(reason));
                    }
                }
                if let Some(exit) = event.get("early_exit") {
                    a.insert("early_exit".to_string(), exit.clone());
                    // 冻结口径：`early_exit_reason?`（字符串平铺别名）。
                    if let Some(reason) = exit.get("reason").and_then(Value::as_str) {
                        a.insert("early_exit_reason".to_string(), json!(reason));
                    }
                }
                let events = a.entry("events".to_string()).or_insert_with(|| json!([]));
                if let Some(list) = events.as_array_mut() {
                    if list.len() < 64 {
                        list.push(event);
                    }
                }
            }
        }
        team.strategy_decision = Some(sd);
        team.updated_at = now_ts();
        let _ = self.store.save_team_run(&team).await;
    }

    /// Read tracked workspace changes. None means the tracker has not produced
    /// an authoritative snapshot; callers must treat that as unknown, not clean.
    pub(crate) fn workspace_change_status(&self, team_id: &str) -> Option<bool> {
        let path = self
            .run_dir
            .join(format!("{team_id}-workspace-changes.json"));
        workspace_change_status(&path)
    }

    /// 组装内层 worker 输入：agent 角色注入 prompt（critic 只读）；内置 worker 注入 text。
    pub fn build_enriched_input(ctx: &Value, input: &Value, worker_kind: &str) -> Value {
        let mut out = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        let Some(obj) = out.as_object_mut() else {
            return out;
        };
        // 可信角色元数据对所有 Worker adapter 可见；内置/自定义 worker 也必须能
        // 按 capability 选择契约行为，而不是靠猜测 text 中的上下文字符串。
        obj.insert(
            "role".to_string(),
            ctx.get("role").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "capabilities".to_string(),
            ctx.get("capabilities")
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        if worker_kind == "agent" {
            let has_prompt = obj
                .get("prompt")
                .and_then(Value::as_str)
                .map(|p| !p.trim().is_empty())
                .unwrap_or(false);
            if !has_prompt {
                // 八期一路：角色专属 Prompt 由 TeamPromptCompiler 编译（模板段 +
                // 上下文字节预算 + 截断记录）；prompt 元数据随步骤输入回传，
                // RoleWorker 转报自适应指标（best-effort，不阻塞执行）。
                let (mut prompt_text, prompt_meta) = Self::compile_role_prompt_with_meta(ctx);
                if let Some(rework) = input.get("rework") {
                    let instruction = rework
                        .get("instruction")
                        .and_then(Value::as_str)
                        .unwrap_or("按评审意见修复指定问题");
                    prompt_text.push_str(&format!(
                        "\n\n## 本次评审返修（保持原任务范围与写权限）\n{instruction}\n"
                    ));
                }
                obj.insert("prompt".to_string(), json!(prompt_text));
                if let Some(ws) = obj.get_mut("_workswarm").and_then(Value::as_object_mut) {
                    ws.insert("prompt_meta".to_string(), prompt_meta);
                }
            }
            let role = ctx.get("role").and_then(Value::as_str).unwrap_or("");
            let is_reviewer = super::util::is_review_role(role, &context_capabilities(ctx));
            obj.insert("read_only".to_string(), json!(is_reviewer));
            // 输出契约需要角色身份（producer 类 / critic 类的修复提示不同）。
            obj.insert("role".to_string(), json!(role));
        } else if obj.get("text").map(Value::is_null).unwrap_or(true) {
            // 内置 worker（echo 等）：text 承载上下文切片 → 接力链在产物内容中可见。
            obj.insert("text".to_string(), json!(ctx.to_string()));
        }
        out
    }

    /// 角色 prompt 编译（八期一路）：`TeamPromptCompiler` 按模板 + 角色 + 工具权限
    /// 生成角色专属 Prompt——当前目标 / 输入 Artifact（字节预算：小传正文、大传
    /// 摘要+哈希+ref、超总预算仅引用）/ 必须完成 / 禁止执行 / 输出格式 / 验收条件 /
    /// 剩余调用预算。返回 (prompt, prompt_meta)；prompt_meta 含 context_bytes 与
    /// 截断记录（进自适应指标，UI 可展示上下文大小）。
    pub(crate) fn compile_role_prompt_with_meta(ctx: &Value) -> (String, Value) {
        let upstream_items = ctx
            .get("upstream")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let compiled = crate::team_prompt::compile_upstream(
            &upstream_items,
            crate::team_prompt::PromptBudget::default(),
        );
        let role = ctx.get("role").and_then(Value::as_str).unwrap_or("member");
        let core_spec = ctx
            .get("core_spec")
            .map(Value::to_string)
            .unwrap_or_default();
        let shared_fact_value = ctx
            .get("shared_facts")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let shared_facts = shared_fact_value.to_string();
        let shared_fact_truncated_count = shared_fact_value
            .as_array()
            .map(|facts| {
                facts
                    .iter()
                    .filter(|fact| fact.get("truncated").and_then(Value::as_bool) == Some(true))
                    .count()
            })
            .unwrap_or(0);
        let shared_context_revision = ctx
            .get("shared_context_revision")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let capabilities = context_capabilities(ctx);
        let is_reviewer = super::util::is_review_role(role, &capabilities);
        let pctx = crate::team_prompt::PromptContext {
            core_spec: &core_spec,
            shared_facts: &shared_facts,
            shared_context_revision,
            objective: ctx
                .get("objective_text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            role,
            handoff_contract: ctx
                .get("handoff_contract")
                .and_then(Value::as_str)
                .unwrap_or("按角色职责交付产物"),
            template_id: ctx.get("template_id").and_then(Value::as_str),
            budget_calls: ctx
                .get("budget_calls")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(0),
            explicit_writer: !is_reviewer
                && ctx
                    .get("write_paths")
                    .and_then(Value::as_array)
                    .is_some_and(|paths| !paths.is_empty()),
            task_scoped: ctx
                .get("task_scoped")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            is_critic: is_reviewer,
            upstream: &compiled,
        };
        let prompt = crate::team_prompt::compile_prompt(&pctx);
        let shared_fact_bytes = shared_facts.len();
        let meta = json!({
            "context_bytes": compiled.context_bytes.saturating_add(shared_fact_bytes),
            "upstream_context_bytes": compiled.context_bytes,
            "shared_fact_bytes": shared_fact_bytes,
            "shared_fact_truncated_count": shared_fact_truncated_count,
            "full_count": compiled.full_count,
            "summarized_count": compiled.summarized_count,
            "ref_only_count": compiled.ref_only_count,
            "truncated": compiled.truncations,
        });
        (prompt, meta)
    }

    /// 输出中可选的结构化字段（`{"open_issues":[..],"known_risks":[..]}`；非对象 → 空）。
    pub(crate) fn parse_optional_json_lists(output: &str) -> (Vec<String>, Vec<String>) {
        let Ok(v) = serde_json::from_str::<Value>(output) else {
            return (Vec::new(), Vec::new());
        };
        if !v.is_object() {
            return (Vec::new(), Vec::new());
        }
        let strings = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        (strings("open_issues"), strings("known_risks"))
    }

    // -- 人节点（结果录入 → 产物 + 状态推进；运行任务自动唤醒下游） --
}

#[cfg(test)]
mod context_fact_filter_tests {
    use super::{context_fact_matches_step, workspace_change_status};
    use crate::plan::StepSpec;
    use owo_agent_protocol::SharedContextFact;
    use serde_json::json;

    #[test]
    fn missing_or_invalid_workspace_tracker_is_unknown_not_clean() {
        let path =
            std::env::temp_dir().join(format!("owo-change-status-{}.json", uuid::Uuid::new_v4()));
        assert_eq!(workspace_change_status(&path), None);
        std::fs::write(&path, "not-json").unwrap();
        assert_eq!(workspace_change_status(&path), None);
        std::fs::write(&path, "[]").unwrap();
        assert_eq!(workspace_change_status(&path), None);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn workspace_tracker_distinguishes_confirmed_clean_and_changed() {
        let path =
            std::env::temp_dir().join(format!("owo-change-status-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, r#"[{"changed_files":[]}]"#).unwrap();
        assert_eq!(workspace_change_status(&path), Some(false));
        std::fs::write(&path, r#"[{"changed_files":["src/lib.rs"]}]"#).unwrap();
        assert_eq!(workspace_change_status(&path), Some(true));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn shared_fact_truncation_respects_utf8_byte_budget() {
        let full = "\u{4e8b}\u{5b9e}".repeat(8);
        let (kept, truncated) = super::truncate_utf8_to_bytes(&full, 5);
        assert!(truncated);
        assert!(kept.len() <= 5);
        assert_eq!(kept, "\u{4e8b}");
        let (empty, truncated) = super::truncate_utf8_to_bytes(&full, 0);
        assert!(truncated);
        assert!(empty.is_empty());
        let (unchanged, truncated) = super::truncate_utf8_to_bytes("ok", 2);
        assert!(!truncated);
        assert_eq!(unchanged, "ok");
    }

    fn fact(task_id: Option<&str>, source_refs: &[&str]) -> SharedContextFact {
        SharedContextFact {
            key: "contract".to_string(),
            value_ref: "cas://sha256:abc".to_string(),
            revision: 1,
            producer: "m-worker".to_string(),
            task_id: task_id.map(str::to_string),
            source_refs: source_refs
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            file_hash: None,
            confidence: "unverified".to_string(),
            status: "candidate".to_string(),
            created_at: "2026-10-02T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn task_slice_keeps_global_task_and_referenced_facts_only() {
        let mut step = StepSpec::new("s-worker", "m-worker");
        step.input = json!({
            "assigned_task_id": "task-api",
            "assigned_read_refs": ["src/api.rs"],
            "assigned_contract_refs": ["contract:v2"]
        });

        assert!(context_fact_matches_step(&fact(None, &[]), &step));
        assert!(context_fact_matches_step(
            &fact(Some("task-api"), &[]),
            &step
        ));
        assert!(context_fact_matches_step(
            &fact(Some("other-task"), &["contract:v2"]),
            &step
        ));
        assert!(!context_fact_matches_step(
            &fact(Some("other-task"), &["docs/unrelated.md"]),
            &step
        ));
    }
}
