use super::*;
impl TeamCoordinator {
    /// 录入人节点结果：校验（人节点、未完成、verify）→ 版本化 Artifact + Handoff + 步骤置 Succeeded。
    pub async fn record_human_result(
        &self,
        team_id: &str,
        step_id: &str,
        result: &str,
    ) -> WorkSwarmResult<Artifact> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (team, _space, mut state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        if team.status.is_terminal() {
            return Err(WorkSwarmError::Conflict(format!(
                "运行已终结（{:?}），不能录入人节点结果",
                team.status
            )));
        }
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {step_id} 不存在")))?;
        let spec = Self::role_spec_of_member(&meta, &step.worker)?;
        if spec.assignee != "human" {
            return Err(WorkSwarmError::Validation(format!(
                "任务 {step_id} 不是人节点（assignee={}",
                spec.assignee
            )));
        }
        let record = &state.records[step_id];
        if record.status.is_terminal() {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {step_id} 已终结（{:?}），不能重复录入",
                record.status
            )));
        }
        if let Some(v) = &spec.verify {
            verify_output(&parse_verify(v), result)
                .map_err(|e| WorkSwarmError::Validation(format!("人节点结果未通过验证：{e}")))?;
        }
        if result.trim().is_empty() {
            return Err(WorkSwarmError::Validation("人节点结果不能为空".to_string()));
        }
        // 产物注册（复用步骤产物通道；producer = 人成员）。
        let artifact = self
            .register_step_output(team_id, &step.worker, &spec.role, step_id, result)
            .await?;
        // 步骤置 Succeeded（运行任务据此自动唤醒下游）。
        if let Some(r) = state.records.get_mut(step_id) {
            if !r.status.is_terminal() || r.status == StepStatus::Aborted {
                r.status = StepStatus::Succeeded;
                r.output = Some(result.to_string());
                r.error = None;
            }
        }
        self.persist_state(&state)?;
        self.space_activity(
            team_id,
            &format!(
                "team.human_result：{} 已录入 {}（等待运行循环唤醒下游）",
                step_id, artifact.artifact_id
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.human_result",
            format!("{} 录入人节点结果 → {}", step_id, artifact.artifact_id),
        );
        Ok(artifact)
    }

    // -- 显式交接（§8.5 POST /tasks/{id}/handoff） --

    /// 手动提交结构化交接（成员完成交付后显式登记；校验：本人、已完成）。
    pub async fn submit_handoff(
        &self,
        team_id: &str,
        task_id: &str,
        from_member: &str,
        fields: &HandoffFields,
    ) -> WorkSwarmResult<HandoffRecord> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (team, _space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let correlation = meta.correlation_id.clone();
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == task_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {task_id} 不存在")))?;
        if step.worker != from_member {
            return Err(WorkSwarmError::Validation(format!(
                "只有任务 {} 的承担者 {} 可以交接（收到 from_member={from_member}）",
                task_id, step.worker
            )));
        }
        let record = &state.records[task_id];
        if record.status != StepStatus::Succeeded {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {task_id} 未完成（{:?}），不能交接",
                record.status
            )));
        }
        let role = worker_role(&step.worker).unwrap_or_default();
        let source_artifact = self
            .latest_artifact_for_role_by_producer(team_id, from_member, &role)
            .await?;
        let handoff = HandoffRecord {
            handoff_id: format!("{team_id}:{task_id}:manual:{}", now_ms()),
            from_member: from_member.to_string(),
            to_member: fields.to_member.clone().unwrap_or_else(|| "*".to_string()),
            completed_summary: fields.completed_summary.clone(),
            open_issues: fields.open_issues.clone(),
            output_artifact_refs: if fields.output_artifact_refs.is_empty() {
                source_artifact.map(|a| vec![a]).unwrap_or_default()
            } else {
                fields.output_artifact_refs.clone()
            },
            evidence_refs: fields.evidence_refs.clone(),
            suggested_next_actions: fields.suggested_next_actions.clone(),
            known_risks: fields.known_risks.clone(),
            created_at: now_ts(),
            // 显式交接无 WorkerOutputV1.handoff 概念（七期 · 第三路）：保持 None。
            handoff_note: None,
        };
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        self.store.save_handoff(&handoff, &pid).await?;
        let _ = self
            .bus
            .send(
                from_member,
                &handoff.to_member,
                MessageKind::Task,
                correlation.clone(),
                serde_json::to_value(&handoff).unwrap_or(Value::Null),
                OverflowPolicy::Reject,
            )
            .await;
        self.space_activity(
            team_id,
            &format!(
                "handoff.manual：{from_member} 显式交接 {}",
                handoff.handoff_id
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.handoff.manual",
            format!("{from_member} 手动交接任务 {task_id}（correlation={correlation}）"),
        );
        Ok(handoff)
    }

    pub(crate) async fn latest_artifact_for_role_by_producer(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
    ) -> WorkSwarmResult<Option<String>> {
        let team = self.store.get_team_run(team_id).await?;
        let pid = team
            .project_space_id
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        let space = self.store.get_project_space(&pid).await?;
        let kind = role_kind(role);
        let mut best: Option<Artifact> = None;
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                if a.kind == kind
                    && a.producer == member_id
                    && best.as_ref().is_none_or(|b| a.version > b.version)
                {
                    best = Some(a);
                }
            }
        }
        Ok(best.map(|a| a.artifact_id))
    }

    // -- steer（continue / steer / replace / cancel / retry；只改未完成节点） --
}
