use super::*;
impl TeamCoordinator {
    pub(crate) fn build_human_waits(
        &self,
        _team_id: &str,
        team: &TeamRun,
        _state: &GoalRunState,
        _meta: &RunMeta,
        human_steps: &[StepSpec],
    ) -> Vec<HumanWait> {
        human_steps
            .iter()
            .filter_map(|s| {
                let member = team.members.iter().find(|m| m.member_id == s.worker)?;
                let user_id = match &member.runtime_binding {
                    RuntimeBinding::Human { user_id } => user_id.clone(),
                    _ => "user".to_string(),
                };
                Some(HumanWait {
                    step_id: s.id.clone(),
                    member_id: member.member_id.clone(),
                    user_id,
                    role: member.role.clone(),
                })
            })
            .collect()
    }

    pub(crate) async fn mark_awaiting_human(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        waits: &[HumanWait],
    ) -> WorkSwarmResult<()> {
        let detail = waits
            .iter()
            .map(|w| format!("{} 等待 {}", w.step_id, w.user_id))
            .collect::<Vec<_>>()
            .join("；");
        team.status = TeamRunStatus::AwaitingHuman;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        self.space_activity(team_id, &format!("team.awaiting_human：{detail}"))
            .await?;
        self.audit(team_id, "team.awaiting_human", detail);
        Ok(())
    }

    // -- R2：中断识别与恢复（进程重启闭环） --

    /// 阶段开批：磁盘 TeamRun → Running（已为 Running 时不重复写盘）。
    ///
    /// 进程在阶段执行期间崩溃时磁盘保留 Running 且无活动循环，
    /// 重启后据此识别为 interrupted（见 [`Self::detect_interrupted`]）。
    pub(crate) async fn mark_team_running(&self, team: &mut TeamRun) -> WorkSwarmResult<()> {
        if team.status == TeamRunStatus::Running {
            return Ok(());
        }
        team.status = TeamRunStatus::Running;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        Ok(())
    }

    pub(crate) fn interrupted_marker_path(&self, team_id: &str) -> PathBuf {
        self.run_dir.join(format!("{team_id}-interrupted.json"))
    }

    pub(crate) fn save_interrupted_marker(&self, run: &InterruptedRun) -> WorkSwarmResult<()> {
        let json = serde_json::to_string_pretty(run)?;
        std::fs::write(self.interrupted_marker_path(&run.team_id), json)
            .map_err(|e| WorkSwarmError::Io(e.to_string()))
    }

    /// 读取中断识别记录（无记录 → None）。
    pub fn load_interrupted(&self, team_id: &str) -> Option<InterruptedRun> {
        let raw = std::fs::read_to_string(self.interrupted_marker_path(team_id)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// 该团队当前是否被标记为「运行中断，可显式恢复」（continue/retry 恢复后自动清除）。
    pub fn is_interrupted(&self, team_id: &str) -> bool {
        self.load_interrupted(team_id).is_some()
    }

    pub(crate) fn clear_interrupted_marker(&self, team_id: &str) {
        let _ = std::fs::remove_file(self.interrupted_marker_path(team_id));
    }

    /// 中断候选判定：磁盘状态 Running、且本进程既没有活动阶段也没有存活运行循环。
    pub(crate) fn is_interrupt_candidate(&self, team_id: &str, status: TeamRunStatus) -> bool {
        status == TeamRunStatus::Running
            && !self.is_run_active(team_id)
            && !self.is_loop_alive(team_id)
    }

    /// 中断标记核心步骤（**不取锁**；调用方必须已持有 team_lock，避免重入死锁）。
    ///
    /// 语义（R2 冻结）：
    /// - 原记录状态为 Running 的步骤转为 Aborted（可恢复），error 注明「进程中断」；
    /// - 已成功步骤 / 已有 Artifact / Handoff / DecisionRecord 一律不动（禁止静默重放写操作）；
    /// - 仅落 sidecar 标记 + 状态文件标记位——不触发任何步骤执行。
    pub(crate) async fn mark_interrupted_inner(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<Option<InterruptedRun>> {
        if self.load_interrupted(team_id).is_some() {
            return Ok(None); // 已识别过，幂等跳过。
        }
        let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
        if !self.is_interrupt_candidate(team_id, team.status) {
            return Ok(None);
        }
        let steps: Vec<String> = state
            .records
            .values()
            .filter(|r| r.status == StepStatus::Running)
            .map(|r| r.step_id.clone())
            .collect();
        // Running 步骤转为可恢复状态（Aborted）；其余记录不触碰。
        for r in state.records.values_mut() {
            if r.status == StepStatus::Running {
                r.status = StepStatus::Aborted;
                r.error = Some("进程重启：执行被中断（可 continue / retry 显式恢复）".to_string());
            }
        }
        self.persist_state(&state)?;
        let record = InterruptedRun {
            team_id: team_id.to_string(),
            detected_at: now_ts(),
            interrupted_steps: steps.clone(),
            reason: "process_restart".to_string(),
        };
        self.save_interrupted_marker(&record)?;
        let detail = if steps.is_empty() {
            "无正在执行的步骤".to_string()
        } else {
            format!("中断步骤：{}", steps.join(", "))
        };
        team.updated_at = now_ts();
        self.store.save_team_run(&team).await?;
        self.space_activity(
            team_id,
            &format!(
                "team.interrupted：运行在进程重启后识别为中断（{detail}；未自动重放任何写操作）"
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.interrupted",
            format!("进程重启中断识别：{detail}"),
        );
        Ok(Some(record))
    }

    /// 单团队中断标记（apply_steer 路径专用：**调用方已持有 team_lock**）。
    pub(crate) async fn mark_interrupted_if_applicable(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<Option<InterruptedRun>> {
        self.mark_interrupted_inner(team_id).await
    }

    /// 全量扫描：把「磁盘 Running 但无活动运行」的团队识别为 interrupted。
    ///
    /// 启动/首次访问时调用一次即可；幂等（已识别团队不再重复报告）。
    /// 返回新识别列表 + 状态文件无法解析的团队清单（原样保留、需人工修复）。
    pub async fn detect_interrupted(&self) -> WorkSwarmResult<InterruptionScan> {
        let teams = self.list_team_runs().await?;
        let mut scan = InterruptionScan::default();
        for team in teams {
            match self.detect_interrupted_for(&team.team_id).await {
                Ok(Some(record)) => scan.interrupted.push(record),
                Ok(None) => {}
                Err(WorkSwarmError::CorruptState(_)) => {
                    scan.unreadable_states.push(team.team_id.clone());
                }
                Err(WorkSwarmError::NotFound(_)) => {} // 尚无运行状态文件（未开跑）
                Err(e) => return Err(e),
            }
        }
        Ok(scan)
    }

    /// 单团队版 [`Self::detect_interrupted`]（HTTP 详情视图按需调用；幂等）。
    pub async fn detect_interrupted_for(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<Option<InterruptedRun>> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        self.mark_interrupted_inner(team_id).await
    }

    /// 目标步骤的下游闭包（传递闭包）：plan 中经由 depends_on 可达的步骤里，
    /// 尚未成功（status != Succeeded）的部分。retry 只重置该闭包。
    pub(crate) fn downstream_reset_closure(
        state: &GoalRunState,
        target_step_id: &str,
    ) -> Vec<String> {
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for s in &state.plan.steps {
            for d in &s.depends_on {
                dependents
                    .entry(d.as_str())
                    .or_default()
                    .push(s.id.as_str());
            }
        }
        let mut closure = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: Vec<&str> = vec![target_step_id];
        while let Some(cur) = queue.pop() {
            let Some(next) = dependents.get(cur) else {
                continue;
            };
            for &n in next {
                if !visited.insert(n.to_string()) {
                    continue;
                }
                if let Some(rec) = state.records.get(n) {
                    if rec.status != StepStatus::Succeeded {
                        closure.push(n.to_string());
                        queue.push(n);
                    }
                }
            }
        }
        closure.sort();
        closure
    }

    /// 合并阶段子状态到完整状态（仅阶段内步骤记录 + 计数器增量；已完成步骤记录不被覆盖）。
    pub(crate) fn merge_phase_into_full(&self, full: &mut GoalRunState, sub: &GoalRunState) {
        let sub_ids: HashSet<String> = sub.plan.steps.iter().map(|s| s.id.clone()).collect();
        for (id, sr) in &sub.records {
            if !sub_ids.contains(id) {
                continue;
            }
            if let Some(fr) = full.records.get_mut(id) {
                if fr.status == StepStatus::Succeeded {
                    continue; // 已完成永不回退
                }
                *fr = sr.clone();
            }
        }
        full.steps_taken = full.steps_taken.saturating_add(sub.steps_taken);
        full.total_retries = full.total_retries.saturating_add(sub.total_retries);
        if sub.goal.error.is_some() {
            full.goal.error = sub.goal.error.clone();
        }
    }
}
