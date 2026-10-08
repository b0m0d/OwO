use super::*;

impl TeamCoordinator {
    /// Finalize one claimed phase under epoch fencing and the Team state lock.
    pub(super) async fn finalize_phase(
        &self,
        team_id: &str,
        epoch: u64,
        claimed_steps: Vec<ProgressStep>,
        runner_state: GoalRunState,
        meta: RunMeta,
        result: Result<GoalStatus, String>,
        cancel: &CancelToken,
    ) -> WorkSwarmResult<PhaseOutcome> {
        self.clear_phase_context_snapshot(team_id, epoch);
        // ---- 阶段 C（短锁）：校验代次后合并结果 ----
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let current_epoch = self.phase_epoch(team_id);
        if current_epoch != epoch {
            // 过期阶段：cancel/retry/replace 已接管现场。旧结果只记审计——
            // 不创建 Artifact、不合并记录、不改终态（新阶段会重新领取执行）。
            self.clear_phase_claim(team_id, epoch);
            self.set_run_active(team_id, false);
            self.advance_progress(team_id);
            self.audit(
                team_id,
                "team.phase.stale_drop",
                format!(
                    "阶段 epoch={} 结果丢弃（当前 epoch={}；步骤 {:?}；cancel/retry/replace 已接管）",
                    epoch,
                    current_epoch,
                    claimed_steps
                        .iter()
                        .map(|step| step.step_id.as_str())
                        .collect::<Vec<_>>()
                ),
            );
            let team = self
                .store
                .get_team_run(team_id)
                .await
                .map_err(|e| match e {
                    ProjectSpaceStoreError::NotFound(_) => {
                        WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
                    }
                    other => WorkSwarmError::Store(other),
                })?;
            return Ok(match team.status {
                TeamRunStatus::Cancelled => PhaseOutcome::Aborted,
                TeamRunStatus::Failed | TeamRunStatus::Succeeded => PhaseOutcome::Finished,
                _ if cancel.is_cancelled() => PhaseOutcome::Aborted,
                _ => PhaseOutcome::MoreReady,
            });
        }

        let (mut team, space, mut state) = self.load_bundle(team_id).await?;
        // Attempt admissions are the sole writer of run-wide execution budgets.
        // Phase GoalRunner state is a record projection only, never budget authority.
        Self::merge_phase_records_into_full(&mut state, &runner_state);
        let mut meta = meta;
        // 十一期：并行开发的任务主动分配——lead 产物就绪时，把 `subtasks`
        // （子任务说明 + 写范围）动态应用到对应 writer 角色（下一次注册表重建生效：
        // 写范围 → 范围租约并发；契约 → 子任务说明进 Prompt）。幂等：无变化不落盘。
        if let Err(e) = self.apply_parallel_assignment(team_id, &mut state, &mut meta) {
            let reason = format!("并行子任务计划无效，已停止调度：{e}");
            self.audit(team_id, "team.parallel_assign_error", reason.clone());
            self.set_run_active(team_id, false);
            self.persist_state(&state)?;
            self.advance_progress(team_id);
            self.clear_phase_claim(team_id, epoch);
            self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                .await?;
            return Ok(PhaseOutcome::Failed);
        }
        let is_human_step = |s: &StepSpec, meta: &RunMeta| -> bool {
            Self::role_spec_of_member(meta, &s.worker)
                .map(|r| r.assignee == "human")
                .unwrap_or(false)
        };
        match result {
            Ok(GoalStatus::Succeeded) => {
                self.set_run_active(team_id, false);
                self.persist_state(&state)?;
            }
            Ok(GoalStatus::Aborted) => {
                self.set_run_active(team_id, false);
                self.advance_progress(team_id);
                self.clear_phase_claim(team_id, epoch);
                self.cancel_run_internal(team_id, &mut team, &mut state, "调度器 abort")
                    .await?;
                return Ok(PhaseOutcome::Aborted);
            }
            Ok(GoalStatus::Failed) => {
                self.set_run_active(team_id, false);
                let reason = state
                    .goal
                    .error
                    .clone()
                    .unwrap_or_else(|| "步骤失败".to_string());
                self.persist_state(&state)?;
                self.advance_progress(team_id);
                self.clear_phase_claim(team_id, epoch);
                self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                    .await?;
                return Ok(PhaseOutcome::Failed);
            }
            Ok(_) => {
                self.set_run_active(team_id, false);
                self.persist_state(&state)?;
            }
            Err(e) => {
                self.set_run_active(team_id, false);
                self.persist_state(&state)?;
                self.advance_progress(team_id);
                self.clear_phase_claim(team_id, epoch);
                self.fail_run_internal(team_id, &mut team, &mut state, &format!("执行异常：{e}"))
                    .await?;
                return Ok(PhaseOutcome::Failed);
            }
        }
        self.advance_progress(team_id);
        self.clear_phase_claim(team_id, epoch);

        // One review scan yields complete issue sets and one repair request per owner.
        // The batch transition validates all attempts before any dependent owner resets.
        let repairs = match self
            .collect_review_repairs(team_id, &space, &mut state, &meta)
            .await
        {
            Ok(repairs) => repairs,
            Err(error) => {
                self.fail_run_internal(
                    team_id,
                    &mut team,
                    &mut state,
                    &format!("评审返修计划无效：{error}"),
                )
                .await?;
                return Ok(PhaseOutcome::Failed);
            }
        };
        self.persist_state(&state)?;
        if !repairs.is_empty() {
            drop(_guard);
            self.rework_batch(team_id, &repairs, Some(epoch)).await?;
            self.audit(
                team_id,
                "team.review.repair_dispatched",
                format!(
                    "owners={} issues={} epoch={}",
                    repairs.len(),
                    repairs
                        .iter()
                        .map(|request| request.issue_ids.len())
                        .sum::<usize>(),
                    epoch
                ),
            );
            return Ok(PhaseOutcome::MoreReady);
        }

        // 批次后重评就绪（基于落盘前的最新内存状态）。
        let ready = Self::ready_steps(&state);
        let agent_ready = ready.iter().filter(|s| !is_human_step(s, &meta)).count();
        let human_ready: Vec<StepSpec> = ready
            .iter()
            .filter(|s| is_human_step(s, &meta))
            .cloned()
            .collect();
        if agent_ready > 0 {
            return Ok(PhaseOutcome::MoreReady);
        }
        if !human_ready.is_empty() {
            let waits = self.build_human_waits(team_id, &team, &state, &meta, &human_ready);
            self.mark_awaiting_human(team_id, &mut team, &waits).await?;
            self.persist_state(&state)?;
            return Ok(PhaseOutcome::AwaitingHuman { waits });
        }
        if Self::all_succeeded(&state) {
            return Ok(PhaseOutcome::Done);
        }
        self.set_run_active(team_id, false);
        self.persist_state(&state)?;
        self.fail_run_internal(
            team_id,
            &mut team,
            &mut state,
            "死锁：存在未完成步骤但无就绪步骤",
        )
        .await?;
        Ok(PhaseOutcome::Failed)
    }
}
