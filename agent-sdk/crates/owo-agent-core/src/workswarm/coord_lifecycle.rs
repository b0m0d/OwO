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
        if !state.goal.status.is_terminal() || state.goal.status == GoalStatus::Aborted {
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

    /// 成功收尾：交付清单（CAS ref）+ ProjectSpace Completed + 模板提案。
    pub async fn finalize_success(&self, team_id: &str) -> WorkSwarmResult<TeamRun> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (mut team, mut space, mut state) = self.load_bundle(team_id).await?;
        if !Self::all_succeeded(&state) {
            return Err(WorkSwarmError::Conflict(
                "存在未完成步骤，不能收尾".to_string(),
            ));
        }
        state.goal.transition(GoalStatus::Succeeded);
        self.persist_state(&state)?;

        team.status = TeamRunStatus::Succeeded;
        team.updated_at = now_ts();
        self.store.save_team_run(&team).await?;

        let mut final_artifacts: Vec<Value> = Vec::new();
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                final_artifacts.push(json!({
                    "artifact_id": a.artifact_id,
                    "kind": a.kind,
                    "version": a.version,
                    "content_ref": a.content_ref,
                    "producer": a.producer,
                }));
            }
        }
        let manifest = json!({
            "team_id": team_id,
            "objective": state.goal.objective,
            "artifacts": final_artifacts,
            "created_at": now_ts(),
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let manifest_hash = self
            .cas
            .put(&manifest_bytes)
            .map_err(|e| WorkSwarmError::Run(format!("交付清单 CAS 落盘失败：{e}")))?;
        space.status = ProjectSpaceStatus::Completed;
        space.delivery_manifest_ref = Some(format!("cas://sha256:{manifest_hash}"));
        space.version += 1;
        space.updated_at = now_ts();
        space.activity_stream.push(format!(
            "{} team.succeeded：交付 {} 项产物",
            now_ts(),
            final_artifacts.len()
        ));
        self.store.save_project_space(&space).await?;

        // 模板提案（single 不产生：单角色无团队经验可沉淀）。
        if team.mode != TeamMode::Single {
            let meta = RunMeta::load(&self.run_dir, team_id)?;
            let proposal =
                self.build_template_proposal(team_id, &team, &state, &meta, &final_artifacts);
            self.templates
                .save_proposal(&proposal)
                .map_err(|e| WorkSwarmError::Io(format!("模板提案落盘失败：{e}")))?;
            self.space_activity(
                team_id,
                &format!(
                    "template.proposed：{}（只提案，未自动启用；采纳后进入模板注册表）",
                    proposal.proposal_id
                ),
            )
            .await?;
            self.audit(
                team_id,
                "team.template_proposed",
                format!("模板提案 {}（来源运行 {}）", proposal.proposal_id, team_id),
            );
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
