use super::*;
impl TeamCoordinator {
    /// 创建团队运行：成员/角色/assignee 绑定 + 任务图 + ProjectSpace + TeamRun。
    pub async fn create_team_run(&self, req: &CreateTeamRequest) -> WorkSwarmResult<TeamRun> {
        let objective = req.objective.trim();
        if objective.is_empty() {
            return Err(WorkSwarmError::Validation("objective 不能为空".to_string()));
        }
        // 角色来源：显式 roles（可搭配显式模板记录来源）> 模板（指定/匹配）> 模式默认。
        let (roles, template_id): (Vec<RoleSpec>, Option<String>) = if !req.roles.is_empty() {
            if let Some(id) = &req.template_id {
                self.templates
                    .get_template(id)
                    .ok_or_else(|| WorkSwarmError::NotFound(format!("模板 {id} 不存在")))?;
            }
            (req.roles.clone(), req.template_id.clone())
        } else {
            let tpl = match &req.template_id {
                Some(id) => Some(
                    self.templates
                        .get_template(id)
                        .ok_or_else(|| WorkSwarmError::NotFound(format!("模板 {id} 不存在")))?,
                ),
                None if req.mode == TeamMode::Swarmflow => {
                    let m = self
                        .templates
                        .find_match(req.mode, objective)
                        .ok_or_else(|| {
                            WorkSwarmError::Validation(
                                "swarmflow 模式必须基于版本化模板（无匹配模板；请先提供 template_id 或 roles）"
                                    .to_string(),
                            )
                        })?;
                    Some(m)
                }
                None => self.templates.find_match(req.mode, objective),
            };
            match tpl {
                Some(t) => (
                    t.roles.iter().cloned().map(RoleSpec::from).collect(),
                    Some(t.template_id),
                ),
                None => {
                    if req.mode == TeamMode::Single {
                        // single：单一角色短任务。
                        (vec![RoleSpec::agent("runner")], None)
                    } else {
                        // team 动态组队：默认接力样例。
                        (default_relay_roles(), None)
                    }
                }
            }
        };

        // 约束：角色唯一；Agent 成员 ≤ max_agent_members。
        let mut seen = std::collections::HashSet::new();
        let mut agent_count = 0usize;
        for r in &roles {
            if r.role.trim().is_empty() {
                return Err(WorkSwarmError::Validation("角色名不能为空".to_string()));
            }
            if !seen.insert(r.role.clone()) {
                return Err(WorkSwarmError::Validation(format!("角色重复：{}", r.role)));
            }
            if r.assignee == "agent" {
                agent_count += 1;
            }
        }
        if agent_count > self.max_agent_members {
            return Err(WorkSwarmError::Validation(format!(
                "动态团队 Agent 成员超过上限（{agent_count} > {}）",
                self.max_agent_members
            )));
        }
        // swarmflow 且显式给了非模板角色 → 仍允许（模板角色 + 补充），此处不额外限制。
        if roles.is_empty() {
            return Err(WorkSwarmError::Validation(
                "团队至少需要一个角色".to_string(),
            ));
        }

        let team_id = format!("team-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let project_id = format!("proj-{team_id}");
        let correlation_id = new_correlation_id();

        // 角色规格（worker 缺省：agent 角色 = "agent" 模型驱动；human = user_id）。
        let mut specs: Vec<RoleSpec> = roles
            .into_iter()
            .map(|mut r| {
                if r.assignee.is_empty() {
                    r.assignee = "agent".to_string();
                }
                if r.worker.is_none() && r.assignee == "agent" {
                    r.worker = Some("agent".to_string());
                }
                r
            })
            .collect();

        // 五期：组队策略判定（auto 判定 / single / team 强制；缺省 auto）。
        // 默认不再盲目启用多 Agent：auto 判定为 single 时裁剪到单角色，
        // 角色数与模型调用量随之下降；判定理由随 strategy_decision 暴露给 UI。
        let engine = crate::team_strategy::TeamStrategyEngine::default();
        let profile = crate::team_strategy::TaskProfile {
            category: None,
            artifact_count: 1,
            input_count: 0,
            needs_independent_review: false,
            risk: crate::team_strategy::RiskLevel::Normal,
            single_agent_success_rate: None,
            expects_json: objective.to_ascii_lowercase().ends_with(".json"),
        };
        let selection = req.strategy.unwrap_or_default();
        // 十期·四路：接入三路冻结的收益策略 gate——auto 判定先过
        // `gate_auto`（无证据/不达标/样本不足/过期/绑定不匹配/非预选组 → 默认
        // single，附理由）；显式 single/team 不被 gate 降级（decide_with_policy
        // 内部保证）。gate 理由随 strategy_decision 暴露给 UI。
        let (gate, gate_verdict, gate_evidence) =
            benefit_gate_for_runtime(template_id.as_deref(), objective, &self.run_dir);
        let mut strategy_plan = engine.decide_with_policy(selection, &profile, Some(&gate));
        let gate_reason = if gate.allow_team {
            format!("收益 gate 放行组队：{gate_evidence}")
        } else {
            format!("收益 gate 默认 single（{gate_evidence}）")
        };
        if !strategy_plan
            .reasons
            .iter()
            .any(|r| r.contains("收益 gate"))
        {
            strategy_plan.reasons.push(gate_reason.clone());
        }
        // 裁剪口径：显式 single 强制单角色；auto 判定 single 仅在「未显式给角色
        // 且未命中模板」时裁剪——用户显式编排与已采纳模板（复用编排）始终尊重。
        let trim_to_single = strategy_plan.is_single()
            && specs.len() > 1
            && req.mode != TeamMode::Swarmflow
            && template_id.is_none()
            && (selection == crate::team_strategy::TeamSelectionMode::ForceSingle
                || req.roles.is_empty());
        if trim_to_single {
            // 单 Agent 判定：**优先保留可交付的写角色**（builder/producer/writer/leader），
            // 而不是盲目保留首个角色——默认接力的首个是只读 planner，裁到它会让团队
            // "成功"却零产出（"草草了事"的根因）。研究类任务没有写角色时保留首个
            // （researcher 只读是正确语义）。
            let keep_index = specs
                .iter()
                .position(|spec| {
                    crate::worker_profile::WorkerProfile::for_role(&spec.role, 0).is_writer()
                })
                .unwrap_or(0);
            let mut kept = specs.remove(keep_index);
            // 上游角色已被裁掉：清空依赖（保持 DAG 可拓扑排序）。
            kept.depends_on.clear();
            strategy_plan.reasons.push(format!(
                "判定单 Agent：保留可交付角色 {}（已裁剪 {} 个附加角色，上游依赖解除）",
                kept.role,
                specs.len()
            ));
            specs = vec![kept];
        }

        // 八期一路：模板级自适应角色策略——简单任务自动减少 Worker（创建期裁剪）。
        // 仅作用于「角色来自模板」（req.roles 为空）且多角色团队——用户显式编排
        // 始终尊重（与 trim_to_single 同口径），其 reviewer 由运行期无变更跳过兜底；
        // 被跳过角色的依赖重定向到其上游（保持 DAG 可拓扑排序）；跳过名单与节省
        // 预算进 strategy_decision.adaptive + 审计。
        let budget_map: BTreeMap<String, usize> = template_id
            .as_deref()
            .and_then(crate::builtin_team_templates::descriptor)
            .map(|d| {
                d.budget_calls_per_role
                    .iter()
                    .map(|rb| (rb.role.clone(), rb.budget_calls))
                    .collect()
            })
            .unwrap_or_default();
        let mut adaptive_skips: Vec<crate::team_strategy::SkippedRole> = Vec::new();
        let mut adaptive_saved_calls = 0usize;
        if specs.len() > 1 && req.roles.is_empty() {
            let role_names: Vec<String> = specs.iter().map(|s| s.role.clone()).collect();
            let adaptive = crate::team_strategy::plan_adaptive_roles(
                template_id.as_deref(),
                &role_names,
                &budget_map,
                &profile,
            );
            if !adaptive.skipped.is_empty() {
                for skip in &adaptive.skipped {
                    let deps_of_skip = specs
                        .iter()
                        .find(|s| s.role == skip.role)
                        .map(|s| s.depends_on.clone())
                        .unwrap_or_default();
                    // 指向被跳过角色的依赖 → 重定向到该角色的上游（去重保序）。
                    for s in &mut specs {
                        if s.role == skip.role || !s.depends_on.iter().any(|d| d == &skip.role) {
                            continue;
                        }
                        let mut rewritten: Vec<String> = Vec::new();
                        for d in &s.depends_on {
                            if d == &skip.role {
                                for up in &deps_of_skip {
                                    if !rewritten.contains(up) {
                                        rewritten.push(up.clone());
                                    }
                                }
                            } else if !rewritten.contains(d) {
                                rewritten.push(d.clone());
                            }
                        }
                        s.depends_on = rewritten;
                    }
                    specs.retain(|s| s.role != skip.role);
                    strategy_plan.reasons.push(format!(
                        "自适应裁剪：跳过角色 {}（{}）",
                        skip.role, skip.reason
                    ));
                    adaptive_skips.push(skip.clone());
                }
                adaptive_saved_calls = adaptive.saved_budget_calls;
                strategy_plan.budget_calls_total = strategy_plan
                    .budget_calls_total
                    .saturating_sub(adaptive.saved_budget_calls);
            }
        }
        let mut strategy_decision = serde_json::to_value(&strategy_plan).ok();
        if let Some(obj) = strategy_decision.as_mut().and_then(Value::as_object_mut) {
            // 八期一路 additive：自适应指标（skipped_roles/skip_reason/saved_budget_calls/
            // context_bytes/提前结束原因）。运行期事件由 note_adaptive_event 追加。
            obj.insert(
                "adaptive".to_string(),
                json!({
                    "skipped_roles": adaptive_skips
                        .iter()
                        .map(|s| json!({"role": s.role, "reason": s.reason}))
                        .collect::<Vec<_>>(),
                    "saved_budget_calls": adaptive_saved_calls,
                    "context_bytes_total": 0,
                    "runtime_skipped": [],
                    "events": [],
                    "early_exit": Value::Null,
                }),
            );
            // 十期·四路：收益策略 gate 判定随 strategy_decision 暴露（UI/审计可追溯
            // 为什么 auto 走了 single——无证据/不达标等逐条理由）。
            obj.insert(
                "benefit_gate".to_string(),
                json!({
                    "allow_team": gate.allow_team,
                    "mandatory_review": gate.mandatory_review,
                    "reasons": gate.reasons,
                    "evidence": gate_verdict.as_ref().map(|v| json!({
                        "task_group": v.task_group,
                        "eligible": v.eligible,
                        "sample_sufficient": v.sample_sufficient,
                        "single_n": v.single_n,
                        "multi_n": v.multi_n,
                        "generated_at": v.generated_at,
                        "bindings": v.bindings,
                    })),
                    "note": gate_reason,
                }),
            );
        }
        let strategy_mode = strategy_plan.mode.clone();

        // 成员（agent/human/worker 运行时绑定）。
        let mut members = Vec::new();
        for spec in &specs {
            let member_id = format!("m-{}", spec.role);
            let binding = match spec.assignee.as_str() {
                "human" => RuntimeBinding::Human {
                    user_id: spec.worker.clone().unwrap_or_else(|| "user".to_string()),
                },
                "worker" => RuntimeBinding::Worker {
                    worker_name: spec.worker.clone().unwrap_or_else(|| spec.role.clone()),
                },
                _ => RuntimeBinding::Agent {
                    agent_id: format!("{team_id}:{member_id}"),
                },
            };
            let read_only = is_critic_role(&spec.role);
            self.bus.register(member_id.clone(), 64).await;
            members.push(TeamMember {
                member_id: member_id.clone(),
                role: spec.role.clone(),
                runtime_binding: binding,
                capabilities: vec![spec.role.clone()],
                tool_scope: if read_only {
                    vec!["read".to_string()]
                } else {
                    vec!["read".to_string(), "write".to_string()]
                },
                read_scope: vec!["project_space".to_string(), "artifacts".to_string()],
                write_scope: if read_only {
                    Vec::new()
                } else {
                    vec!["artifacts".to_string()]
                },
                budget: Value::Null,
                handoff_contract: spec.handoff_contract.clone(),
                health: MemberHealth::Active,
            });
        }

        // 任务图（步骤 ↔ 成员 绑定；worker 名 = member_id，由 run 注册表按名派发角色 worker）。
        let mut plan = Plan::new(format!("{team_id}-plan"), team_id.clone());
        plan.description = format!("WorkSwarm 团队 {team_id}：{objective}");
        for spec in &specs {
            let step_id = format!("s-{}", spec.role);
            let member_id = format!("m-{}", spec.role);
            let mut input = spec.extra_input.clone();
            if !input.is_object() {
                input = json!({});
            }
            if let Some(obj) = input.as_object_mut() {
                obj.insert("objective".to_string(), json!(objective));
                obj.insert(
                    "_workswarm".to_string(),
                    json!({ "team_id": team_id, "member_id": member_id, "step_id": step_id }),
                );
            }
            let step = StepSpec {
                id: step_id.clone(),
                depends_on: spec.depends_on.iter().map(|d| format!("s-{d}")).collect(),
                parallel: true,
                worker: member_id,
                input,
                verify: spec.verify.as_ref().map(|v| parse_verify(v)),
                retries: 0,
            };
            plan.add_step(step);
        }
        plan.validate()
            .map_err(|e| WorkSwarmError::Validation(format!("任务图非法：{e}")))?;

        // 目标（预算映射到 GoalBudget）。
        let mut goal = Goal::new(team_id.clone(), objective.to_string());
        goal.budget = parse_goal_budget(&req.budget);
        let mut state = GoalRunState::new(goal, plan);
        state.run_id = team_id.clone();
        state.persist(&self.run_dir).map_err(WorkSwarmError::Run)?;

        // ProjectSpace（统一事实空间：任务/产物/决策/交接/活动）。
        let now = now_ts();
        let space = ProjectSpace {
            project_id: project_id.clone(),
            goal_id: Some(team_id.clone()),
            team_id: Some(team_id.clone()),
            tasks: state.plan.steps.iter().map(|s| s.id.clone()).collect(),
            artifacts: Vec::new(),
            decisions: Vec::new(),
            approvals: Vec::new(),
            discussions: Vec::new(),
            activity_stream: vec![format!(
                "{now} team.created（mode={}，{} 个成员，来源={}）",
                match req.mode {
                    TeamMode::Single => "single",
                    TeamMode::Team => "team",
                    TeamMode::Swarmflow => "swarmflow",
                },
                members.len(),
                template_id
                    .as_deref()
                    .map(|t| format!("template:{t}"))
                    .unwrap_or_else(|| "dynamic".to_string()),
            )],
            delivery_manifest_ref: None,
            rework_tasks: Vec::new(),
            status: ProjectSpaceStatus::Active,
            version: 1,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        self.store.save_project_space(&space).await?;

        // TeamRun（Leader 产出的结构化协作计划）。
        let team = TeamRun {
            team_id: team_id.clone(),
            goal_id: req.goal_id.clone().or_else(|| Some(team_id.clone())),
            mode: req.mode,
            members: members.clone(),
            task_graph_ref: Some(format!("{team_id}-plan")),
            project_space_id: Some(project_id),
            template_id: template_id.clone(),
            shared_context_refs: Vec::new(),
            budget: req.budget.clone(),
            human_policy: req.human_policy.clone(),
            strategy_decision,
            status: TeamRunStatus::Created,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.save_team_run(&team).await?;
        self.audit(
            &team_id,
            "team.strategy",
            format!(
                "组队策略：{}（selection={}，角色 {} 个，预算 {} 次调用）",
                strategy_mode,
                selection.as_str(),
                strategy_plan.roles.len(),
                strategy_plan.budget_calls_total
            ),
        );

        // 运行元数据（correlation + 角色规格 sidecar）。
        RunMeta {
            team_id: team_id.clone(),
            correlation_id,
            roles: specs,
            template_id: template_id.clone(),
            budgets: budget_map,
        }
        .save(&self.run_dir)?;

        // 八期一路：创建期自适应裁剪审计（跳过角色/节省预算，best-effort 可读性）。
        if !adaptive_skips.is_empty() {
            self.audit(
                &team_id,
                "team.adaptive_skip",
                format!(
                    "自适应裁剪 {} 个角色（节省预算 {} 次调用）：{}",
                    adaptive_skips.len(),
                    adaptive_saved_calls,
                    adaptive_skips
                        .iter()
                        .map(|s| s.role.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                ),
            );
        }

        self.audit(
            &team_id,
            "team.created",
            format!(
                "组队：mode={} 成员={} 来源={}（correlation 已建立）",
                match team.mode {
                    TeamMode::Single => "single",
                    TeamMode::Team => "team",
                    TeamMode::Swarmflow => "swarmflow",
                },
                members.len(),
                template_id
                    .clone()
                    .map(|t| format!("template:{t}"))
                    .unwrap_or_else(|| "dynamic".to_string()),
            ),
        );
        Ok(team)
    }

    // -- 运行阶段（server 运行循环驱动） --

    /// 执行一个阶段（当前就绪的 agent 批次）。人节点不进入 runner（由运行任务开门闩等待）。
    ///
    /// 每阶段结束后把子状态合并回完整状态并落盘（steer/replace/human 的改盘变更在
    /// 下一阶段重新加载时生效）。
    pub async fn run_phase(
        &self,
        team_id: &str,
        registry: &WorkerRegistry,
    ) -> WorkSwarmResult<PhaseOutcome> {
        // ---- 阶段 A（短锁）：领取 ready 步骤、标记 Running、持久化 ----
        // 锁只覆盖领取与落盘，Worker/模型执行的整段时间**不持锁**，
        // GET 详情 / 任务图 / 产物 / SSE 等读路径不再被长 Worker 阻塞。
        struct PhaseClaimPlan {
            epoch: u64,
            sub_state: GoalRunState,
            claimed: Vec<ProgressStep>,
            meta: RunMeta,
            /// 八期一路：本阶段运行期跳过的角色（role, reason, saved_calls）。
            runtime_skips: Vec<(String, String, usize)>,
        }
        let claim: PhaseClaimPlan = {
            let lock = self.team_lock(team_id);
            let _guard = lock.lock().await;

            let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
            if team.status.is_terminal() {
                return Ok(PhaseOutcome::Finished);
            }
            let meta = RunMeta::load(&self.run_dir, team_id)?;

            let ready = Self::ready_steps(&state);
            let is_human_step = |s: &StepSpec, meta: &RunMeta| -> bool {
                Self::role_spec_of_member(meta, &s.worker)
                    .map(|r| r.assignee == "human")
                    .unwrap_or(false)
            };
            let mut agent_steps: Vec<StepSpec> = ready
                .iter()
                .filter(|s| !is_human_step(s, &meta))
                .cloned()
                .collect();
            let human_steps: Vec<StepSpec> = ready
                .iter()
                .filter(|s| is_human_step(s, &meta))
                .cloned()
                .collect();

            // 八期一路：运行期可选角色跳过——code-change 模板的 reviewer 在上游实现
            // 步骤未产生任何实际工作区变更时无可评审对象 → 跳过（标记 Succeeded，
            // 下游不再等待）；有实际变更（Git 变更跟踪文件有记录）→ 正常执行。
            // 高风险/要求评审的团队在创建期即保留 reviewer，本判定不影响其执行。
            let mut runtime_skips: Vec<(String, String, usize)> = Vec::new();
            if meta.template_id.as_deref() == Some(crate::builtin_team_templates::CODE_CHANGE_V1) {
                let has_changes = self.workspace_has_changes(team_id);
                let mut remaining: Vec<StepSpec> = Vec::with_capacity(agent_steps.len());
                for step in agent_steps {
                    let role = worker_role(&step.worker).unwrap_or_default();
                    if let Some(reason) =
                        crate::team_strategy::reviewer_runtime_skip_reason(&role, has_changes)
                    {
                        let skippable = state
                            .records
                            .get(&step.id)
                            .is_some_and(|r| r.status.can_resume());
                        if skippable {
                            if let Some(record) = state.records.get_mut(&step.id) {
                                record.status = StepStatus::Succeeded;
                            }
                            let saved = meta.budgets.get(&role).copied().unwrap_or(0);
                            runtime_skips.push((role, reason, saved));
                            continue;
                        }
                    }
                    remaining.push(step);
                }
                agent_steps = remaining;
            }

            // 无就绪：全部完成 → Done；否则死锁（上游失败等）→ Failed。
            if agent_steps.is_empty() && human_steps.is_empty() {
                if Self::all_succeeded(&state) {
                    if !runtime_skips.is_empty() {
                        // 跳过标记必须先落盘（否则磁盘 reviewer 停留在 Pending 而团队已终态）。
                        self.persist_state(&state)?;
                        drop(_guard);
                        // 锁外记录自适应指标与审计（note_adaptive_event 自行持锁）。
                        for (role, reason, saved) in &runtime_skips {
                            self.audit(
                                team_id,
                                "team.role_skipped",
                                format!("运行期跳过角色 {role}：{reason}"),
                            );
                            self.note_adaptive_event(
                                team_id,
                                json!({
                                    "kind": "role_skipped",
                                    "role": role,
                                    "reason": reason,
                                    "saved_budget_calls": saved,
                                    "role_skipped": {"role": role, "reason": reason},
                                }),
                            )
                            .await;
                        }
                        self.audit(
                            team_id,
                            "team.early_exit",
                            "运行期跳过使全部完成条件满足，DAG 提前结束".to_string(),
                        );
                        self.note_adaptive_event(
                            team_id,
                            json!({
                                "kind": "early_exit",
                                "early_exit": {
                                    "reason": "运行期跳过使全部完成条件满足，DAG 提前结束",
                                    "skipped_roles": runtime_skips
                                        .iter()
                                        .map(|(r, _, _)| r.clone())
                                        .collect::<Vec<_>>(),
                                },
                            }),
                        )
                        .await;
                    }
                    return Ok(PhaseOutcome::Done);
                }
                self.fail_run_internal(
                    team_id,
                    &mut team,
                    &mut state,
                    "死锁：存在未完成步骤但无就绪步骤（检查上游失败依赖）",
                )
                .await?;
                return Ok(PhaseOutcome::Failed);
            }
            if agent_steps.is_empty() {
                // 仅人节点就绪：进入门闩（无 Worker 执行，无长锁窗口）。
                let waits = self.build_human_waits(team_id, &team, &state, &meta, &human_steps);
                self.mark_awaiting_human(team_id, &mut team, &waits).await?;
                self.persist_state(&state)?;
                return Ok(PhaseOutcome::AwaitingHuman { waits });
            }

            let epoch = self.phase_epoch(team_id);
            self.set_run_active(team_id, true);
            // 磁盘状态 → Running（R2 恢复底座）：进程若在本阶段内崩溃，
            // 磁盘留下 Running 且无活动循环 → 重启后被识别为 interrupted。
            self.mark_team_running(&mut team).await?;
            let started_at = now_ts();
            let mut claimed: Vec<ProgressStep> = Vec::with_capacity(agent_steps.len());
            for step in &agent_steps {
                if let Some(record) = state.records.get_mut(&step.id) {
                    record.status = StepStatus::Running;
                    let role = worker_role(&step.worker).unwrap_or_else(|| step.worker.clone());
                    claimed.push(ProgressStep {
                        step_id: step.id.clone(),
                        worker: role,
                        status: "Running".to_string(),
                        attempts: record.attempts.saturating_add(1),
                        started_at: started_at.clone(),
                    });
                }
            }
            self.persist_state(&state)?;
            self.note_phase_claim(
                team_id,
                PhaseClaim {
                    epoch,
                    steps: claimed.clone(),
                },
            );
            self.advance_progress(team_id);

            // 子计划 = 已完成步骤 + 本批 agent 步骤（人节点/未来步骤不在子计划内）。
            // 领取步在完整状态中标记为 Running（可观测/可恢复），子计划内转换回
            // Pending 供 GoalRunner 执行；合并时以终态覆盖。
            let batch_ids: std::collections::HashSet<&str> =
                agent_steps.iter().map(|s| s.id.as_str()).collect();
            let sub_ids: HashSet<String> = state
                .plan
                .steps
                .iter()
                .filter(|s| {
                    batch_ids.contains(s.id.as_str())
                        || state.records[&s.id].status == StepStatus::Succeeded
                })
                .map(|s| s.id.clone())
                .collect();
            let sub_steps: Vec<StepSpec> = state
                .plan
                .steps
                .iter()
                .filter(|s| sub_ids.contains(&s.id))
                .cloned()
                .map(|mut step| {
                    if batch_ids.contains(step.id.as_str()) {
                        // 领取代次注入步骤输入：RoleWorker 回传产物时凭此校验，
                        // 过期阶段的回传在 register_step_output_checked 被拒收。
                        if let Some(obj) = step.input.as_object_mut() {
                            let workswarm = obj
                                .entry("_workswarm".to_string())
                                .or_insert_with(|| json!({}));
                            if let Some(workswarm_obj) = workswarm.as_object_mut() {
                                workswarm_obj.insert("phase_epoch".to_string(), json!(epoch));
                            }
                        }
                    }
                    step
                })
                .collect();
            let sub_records: BTreeMap<String, _> = state
                .records
                .iter()
                .filter(|(k, _)| sub_ids.contains(*k))
                .map(|(k, v)| {
                    let mut record = v.clone();
                    if batch_ids.contains(k.as_str()) && record.status == StepStatus::Running {
                        record.status = StepStatus::Pending;
                    }
                    (k.clone(), record)
                })
                .collect();
            let mut sub_state = GoalRunState {
                run_id: state.run_id.clone(),
                goal: state.goal.clone(),
                plan: Plan {
                    id: state.plan.id.clone(),
                    goal_id: state.plan.goal_id.clone(),
                    description: format!("{}（阶段子计划）", state.plan.description),
                    steps: sub_steps,
                    created_at: now_ts(),
                },
                records: sub_records,
                steps_taken: 0,
                total_retries: 0,
                replan_count: 0,
                started_at: now_ts(),
                events: Vec::new(),
                aborted: false,
            };
            if let Err(e) = sub_state.plan.validate() {
                self.set_run_active(team_id, false);
                self.clear_phase_claim(team_id, epoch);
                return Err(WorkSwarmError::Run(format!("阶段子计划非法：{e}")));
            }
            // 子目标状态：强制可运行（整体 goal 已终态时上面会 Finished；这里处理 Failed 后 continue 的场景）。
            if sub_state.goal.status.is_terminal() {
                sub_state.goal.transition(GoalStatus::Running);
                sub_state.goal.error = None;
            }
            PhaseClaimPlan {
                epoch,
                sub_state,
                claimed,
                meta,
                runtime_skips,
            }
        }; // —— 阶段 A 结束：锁已释放 ——

        // ---- 阶段 A'（锁外）：运行期跳过 → 自适应指标 + 审计（Done 路径已在锁内处理）。
        for (role, reason, saved) in &claim.runtime_skips {
            self.audit(
                team_id,
                "team.role_skipped",
                format!("运行期跳过角色 {role}：{reason}"),
            );
            self.note_adaptive_event(
                team_id,
                json!({
                    "kind": "role_skipped",
                    "role": role,
                    "reason": reason,
                    "saved_budget_calls": saved,
                    "role_skipped": {"role": role, "reason": reason},
                }),
            )
            .await;
        }

        // ---- 阶段 B（无锁）：Worker/模型执行 ----
        let config = RunnerConfig {
            max_parallel: 4,
            persist_dir: None,   // 阶段结束由协调器合并完整状态后统一落盘
            allow_replan: false, // 团队运行失败 = 显式失败（由 continue 决定重试）
            ..Default::default()
        };
        let mut runner = GoalRunner::from_state(claim.sub_state, config);
        if let Some(audit) = &self.audit {
            runner.attach_audit(Arc::clone(audit));
        }
        let cancel = self.cancel_token(team_id);
        // 十期·四路 R2：取消链「先通知停止、再有界清理」。不直接丢弃 run Future——
        // 丢弃会跳过在飞 worker 的变更收尾（TrackedRoleWorker 的后快照/变更登记
        // 在其 Future 内）。取消时置位 abort 标志（协作式 worker 在回合边界快速
        // 返回并完成收尾），再等 run 以 [`GoalRunner::run`] 的协作退出路径自然收束。
        // 内层作用域：run_fut 借用 runner 到 select 结束即释放，便于阶段 C 读取状态。
        let result = {
            let abort_signal = runner.abort_signal();
            let run_fut = runner.run(registry);
            tokio::pin!(run_fut);
            tokio::select! {
                r = &mut run_fut => r,
                _ = wait_cancel(&cancel) => {
                    abort_signal.store(true, std::sync::atomic::Ordering::SeqCst);
                    // 有界清理：run 收到 abort 后协作退出；超时才强制终止
                    //（进程树由沙箱 Job kill-on-close 兜底）。
                    match tokio::time::timeout(
                        crate::goal::PHASE_CANCELLATION_CLEANUP_GRACE,
                        &mut run_fut,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Ok(GoalStatus::Aborted),
                    }
                }
            }
        };

        // ---- 阶段 C（短锁）：校验代次后合并结果 ----
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let current_epoch = self.phase_epoch(team_id);
        if current_epoch != claim.epoch {
            // 过期阶段：cancel/retry/replace 已接管现场。旧结果只记审计——
            // 不创建 Artifact、不合并记录、不改终态（新阶段会重新领取执行）。
            self.clear_phase_claim(team_id, claim.epoch);
            self.set_run_active(team_id, false);
            self.advance_progress(team_id);
            self.audit(
                team_id,
                "team.phase.stale_drop",
                format!(
                    "阶段 epoch={} 结果丢弃（当前 epoch={}；步骤 {:?}；cancel/retry/replace 已接管）",
                    claim.epoch,
                    current_epoch,
                    claim
                        .claimed
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

        let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
        // 合并子状态 → 完整状态（记录 + 计数器增量），落盘。
        self.merge_phase_into_full(&mut state, &runner.state);
        let meta = claim.meta;
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
                self.clear_phase_claim(team_id, claim.epoch);
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
                self.clear_phase_claim(team_id, claim.epoch);
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
                self.clear_phase_claim(team_id, claim.epoch);
                self.fail_run_internal(team_id, &mut team, &mut state, &format!("执行异常：{e}"))
                    .await?;
                return Ok(PhaseOutcome::Failed);
            }
        }
        self.advance_progress(team_id);
        self.clear_phase_claim(team_id, claim.epoch);

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
