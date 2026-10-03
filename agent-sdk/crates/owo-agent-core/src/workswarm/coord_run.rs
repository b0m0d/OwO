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

        // 约束：角色唯一；Agent 成员 ≤ max_agent_members；写路径合法（权限默认 deny）。
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
            super::roles::validate_role_write_paths_with_capabilities(
                &r.role,
                &r.capabilities,
                &r.write_paths,
            )
            .map_err(WorkSwarmError::Validation)?;
        }
        if agent_count > req.max_agent_members.unwrap_or(self.max_agent_members) {
            return Err(WorkSwarmError::Validation(format!(
                "动态团队 Agent 成员超过上限（{agent_count} > {}）",
                req.max_agent_members.unwrap_or(self.max_agent_members)
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

        // 旧 API/模板按名称推导一次职责能力；之后授权与执行只读取 capability。
        for role in &mut specs {
            if role.capabilities.is_empty() && is_review_role_name(&role.role) {
                role.capabilities.push("review".to_string());
            }
        }

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
            let read_only = spec.is_reviewer();
            self.bus.register(member_id.clone(), 64).await;
            // 写面声明（十一期）：角色级写路径优先；未声明沿用 "artifacts" 标记
            // （legacy wire 语义，运行期写面仍按工作区级处理）。
            let write_scope = if read_only {
                Vec::new()
            } else if !spec.write_paths.is_empty() {
                spec.write_paths.clone()
            } else {
                vec!["artifacts".to_string()]
            };
            members.push(TeamMember {
                member_id: member_id.clone(),
                role: spec.role.clone(),
                runtime_binding: binding,
                capabilities: if spec.capabilities.is_empty() {
                    vec![spec.role.clone()]
                } else {
                    spec.capabilities.clone()
                },
                tool_scope: if read_only {
                    vec!["read".to_string()]
                } else {
                    vec!["read".to_string(), "write".to_string()]
                },
                read_scope: vec!["project_space".to_string(), "artifacts".to_string()],
                write_scope,
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
                // 十一期：模型解析——角色显式 model 优先，否则团队统一模型
                // （`req.model`，来自请求或 `<workspace>/settings.json` 的 team 段）；
                // `extra_input.model` 显式值优先于两者。
                let effective_model = spec
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .or_else(|| {
                        req.model
                            .as_deref()
                            .map(str::trim)
                            .filter(|m| !m.is_empty())
                    });
                let has_model = obj
                    .get("model")
                    .and_then(Value::as_str)
                    .map(|m| !m.trim().is_empty())
                    .unwrap_or(false);
                if !has_model {
                    if let Some(model) = effective_model {
                        obj.insert("model".to_string(), json!(model));
                    }
                }
            }
            let verification = spec.verify.as_ref().map(|value| parse_verify(value));
            let verification_plan = verification
                .as_ref()
                .map(|value| verification_plan_for_step(&step_id, value));
            let step = StepSpec {
                id: step_id.clone(),
                depends_on: spec.depends_on.iter().map(|d| format!("s-{d}")).collect(),
                parallel: true,
                worker: member_id,
                input,
                verify: verification,
                verification_plan,
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

        // P1：父会话 CoreSpec 以不可变 CAS 快照保存；Worker 仅通过引用读取。
        let mut shared_context_refs = Vec::new();
        if let Some(snapshot) = req.parent_context_snapshot.as_deref() {
            let hash = self
                .cas
                .put(snapshot.as_bytes())
                .map_err(WorkSwarmError::Run)?;
            shared_context_refs.push(format!("cas://sha256:{hash}"));
        }
        // TeamRun（Leader 产出的结构化协作计划）。
        let team = TeamRun {
            team_id: team_id.clone(),
            goal_id: req.goal_id.clone().or_else(|| Some(team_id.clone())),
            mode: req.mode,
            members: members.clone(),
            task_graph_ref: Some(format!("{team_id}-plan")),
            project_space_id: Some(project_id),
            template_id: template_id.clone(),
            shared_context_refs,
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

        // A selected template may introduce lead only after the server has
        // expanded its role list. Derive dynamic assignment from the resolved roles too.
        let parallel_assignment = should_enable_parallel_assignment(req.parallel, &specs);

        // 运行元数据（correlation + 角色规格 sidecar）。
        RunMeta {
            team_id: team_id.clone(),
            correlation_id,
            roles: specs,
            template_id: template_id.clone(),
            budgets: budget_map,
            parallel: parallel_assignment,
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

    /// 持久化当前 epoch 内刚完成的单步；模型/工具执行不在 team_lock 内。
    async fn persist_step_progress(
        &self,
        team_id: &str,
        epoch: u64,
        base_steps_taken: u32,
        base_total_retries: u32,
        update: StepProgressUpdate,
    ) -> WorkSwarmResult<()> {
        let lock = self.team_lock(team_id);
        let guard = lock.lock().await;
        if self.phase_epoch(team_id) != epoch {
            return Ok(());
        }
        let StepProgressUpdate {
            step_id,
            worker,
            record: mut next,
            steps_taken: update_steps_taken,
            total_retries: update_total_retries,
            skip_reason,
        } = update;
        if skip_reason.is_some() {
            next.skip_reason = skip_reason.clone();
        }
        let (_team, _space, mut state) = self.load_bundle(team_id).await?;
        let record_changed = if let Some(current) = state.records.get_mut(&step_id) {
            let changed = current.status != next.status
                || current.attempts != next.attempts
                || current.output != next.output
                || current.error != next.error
                || current.skip_reason != next.skip_reason;
            if changed {
                *current = next;
            }
            changed
        } else {
            false
        };
        let steps_taken = base_steps_taken.saturating_add(update_steps_taken);
        let total_retries = base_total_retries.saturating_add(update_total_retries);
        let counters_changed =
            state.steps_taken != steps_taken || state.total_retries != total_retries;
        if record_changed || counters_changed {
            state.steps_taken = steps_taken;
            state.total_retries = total_retries;
            self.persist_state(&state)?;
            self.advance_progress(team_id);
        }
        drop(guard);
        if let Some(reason) = skip_reason {
            let role = worker.strip_prefix("m-").unwrap_or(&worker);
            self.note_adaptive_event(
                team_id,
                json!({
                    "kind": "role_skipped",
                    "role": role,
                    "step_id": step_id,
                    "reason": reason,
                    "role_skipped": {"role": role, "reason": reason},
                }),
            )
            .await;
        }
        Ok(())
    }

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
            review_issues_pending: bool,
            base_steps_taken: u32,
            base_total_retries: u32,
        }
        let claim: PhaseClaimPlan = {
            let lock = self.team_lock(team_id);
            let _guard = lock.lock().await;

            let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
            if team.status.is_terminal() {
                return Ok(PhaseOutcome::Finished);
            }
            let mut meta = RunMeta::load(&self.run_dir, team_id)?;
            let parallel_assignment_missing = meta.parallel
                && !state
                    .plan
                    .steps
                    .iter()
                    .any(|step| step.input.get("assigned_task_id").is_some());
            let lead_already_succeeded = state.plan.steps.iter().any(|step| {
                worker_role(&step.worker).as_deref() == Some("lead")
                    && state
                        .records
                        .get(&step.id)
                        .is_some_and(|record| record.status == StepStatus::Succeeded)
            });
            if parallel_assignment_missing && lead_already_succeeded {
                if let Err(error) = self.apply_parallel_assignment(team_id, &mut state, &mut meta) {
                    let reason = format!("并行子任务计划无效，已停止调度：{error}");
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                }
                self.persist_state(&state)?;
            }

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
            {
                let has_changes = self.workspace_change_status(team_id) != Some(false);
                let parallel_tasks_require_integration =
                    parallel_tasks_require_integration(&state.plan.steps);
                let mut remaining: Vec<StepSpec> = Vec::with_capacity(agent_steps.len());
                for step in agent_steps {
                    let role = worker_role(&step.worker).unwrap_or_default();
                    let leader_can_use_host_manifest =
                        meta.parallel && role == "leader" && !parallel_tasks_require_integration;
                    let skip_reason = if leader_can_use_host_manifest {
                        Some(PARALLEL_LEADER_HOST_MANIFEST_SKIP_REASON.to_string())
                    } else if meta.template_id.as_deref()
                        == Some(crate::builtin_team_templates::CODE_CHANGE_V1)
                    {
                        let is_reviewer = meta
                            .roles
                            .iter()
                            .find(|spec| spec.role == role)
                            .is_some_and(RoleSpec::is_reviewer);
                        crate::team_strategy::review_runtime_skip_reason(is_reviewer, has_changes)
                    } else {
                        None
                    };
                    if let Some(reason) = skip_reason {
                        let skippable = state
                            .records
                            .get(&step.id)
                            .is_some_and(|r| r.status.can_resume());
                        if skippable {
                            if let Some(record) = state.records.get_mut(&step.id) {
                                record.status = StepStatus::Succeeded;
                                record.skip_reason = Some(reason.clone());
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
            let claimed_at = now_ts();
            let mut claimed: Vec<ProgressStep> = Vec::with_capacity(agent_steps.len());
            for step in &agent_steps {
                if let Some(record) = state.records.get_mut(&step.id) {
                    // Disk remains Running for crash recovery; progress says Claimed until Worker::run starts.
                    record.status = StepStatus::Running;
                    record.phase_epoch = Some(epoch);
                    record.attempt_id = Some(uuid::Uuid::new_v4().to_string());
                    for receipt in &mut record.validation_receipts {
                        receipt.verdict = crate::plan::ValidationVerdictV1::Stale;
                        receipt.detail =
                            Some("new attempt claimed; prior receipt invalidated".into());
                    }
                    let role = worker_role(&step.worker).unwrap_or_else(|| step.worker.clone());
                    claimed.push(ProgressStep {
                        step_id: step.id.clone(),
                        worker: role,
                        status: "Claimed".to_string(),
                        attempts: record.attempts.saturating_add(1),
                        claimed_at: claimed_at.clone(),
                        started_at: String::new(),
                    });
                }
            }
            self.persist_state(&state)?;
            // 子计划覆盖当前已就绪任务及其所有无需等待 Human 的 agent 后继。
            // GoalRunner 收到每个任务结果后重算 ready 队列，使 A2 不必等同批慢任务 B。
            // 完成节点与本阶段待执行节点都保留在子图，Human 节点及依赖它的分支留待门闩。
            let batch_ids: std::collections::HashSet<&str> =
                agent_steps.iter().map(|s| s.id.as_str()).collect();
            let mut sub_ids: HashSet<String> = state
                .plan
                .steps
                .iter()
                .filter(|s| state.records[&s.id].status == StepStatus::Succeeded)
                .map(|s| s.id.clone())
                .collect();
            sub_ids.extend(agent_steps.iter().map(|step| step.id.clone()));
            let defer_parallel_fanout = meta.parallel
                && !state
                    .plan
                    .steps
                    .iter()
                    .any(|step| step.input.get("assigned_task_id").is_some())
                && agent_steps
                    .iter()
                    .any(|step| worker_role(&step.worker).as_deref() == Some("lead"));
            if !defer_parallel_fanout {
                loop {
                    let mut added = false;
                    for step in &state.plan.steps {
                        if sub_ids.contains(&step.id)
                            || !state.records[&step.id].status.can_resume()
                            || Self::role_spec_of_member(&meta, &step.worker)
                                .is_ok_and(|role| role.assignee == "human")
                        {
                            continue;
                        }
                        if step.depends_on.iter().all(|dependency| {
                            sub_ids.contains(dependency)
                                || state
                                    .records
                                    .get(dependency)
                                    .is_some_and(|record| record.status == StepStatus::Succeeded)
                        }) {
                            sub_ids.insert(step.id.clone());
                            added = true;
                        }
                    }
                    if !added {
                        break;
                    }
                }
            }
            for step in &state.plan.steps {
                if !sub_ids.contains(&step.id)
                    || batch_ids.contains(step.id.as_str())
                    || state.records[&step.id].status == StepStatus::Succeeded
                {
                    continue;
                }
                let record = state.records.get_mut(&step.id).expect("step record exists");
                record.phase_epoch = Some(epoch);
                record.attempt_id = Some(uuid::Uuid::new_v4().to_string());
                for receipt in &mut record.validation_receipts {
                    receipt.verdict = crate::plan::ValidationVerdictV1::Stale;
                    receipt.detail =
                        Some("queued in a new phase; prior receipt invalidated".into());
                }
                let role = worker_role(&step.worker).unwrap_or_else(|| step.worker.clone());
                claimed.push(ProgressStep {
                    step_id: step.id.clone(),
                    worker: role,
                    status: "Queued".to_string(),
                    attempts: record.attempts.saturating_add(1),
                    claimed_at: claimed_at.clone(),
                    started_at: String::new(),
                });
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

            let sub_steps: Vec<StepSpec> = state
                .plan
                .steps
                .iter()
                .filter(|s| sub_ids.contains(&s.id))
                .cloned()
                .map(|mut step| {
                    if state.records[&step.id].status != StepStatus::Succeeded {
                        // 本阶段会运行的全部节点均注入代次；RoleWorker 回传产物时校验，
                        // 过期阶段的回传在 register_step_output_checked 被拒收。
                        if let Some(obj) = step.input.as_object_mut() {
                            let workswarm = obj
                                .entry("_workswarm".to_string())
                                .or_insert_with(|| json!({}));
                            if let Some(workswarm_obj) = workswarm.as_object_mut() {
                                workswarm_obj.insert("phase_epoch".to_string(), json!(epoch));
                                let attempt_id = state.records[&step.id]
                                    .attempt_id
                                    .clone()
                                    .expect("claimed task must have host attempt identity");
                                workswarm_obj.insert("attempt_id".to_string(), json!(attempt_id));
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
                validation_receipts: Vec::new(),
                delivery_issues: Vec::new(),
                steps_taken: 0,
                total_retries: 0,
                replan_count: 0,
                started_at: now_ts(),
                events: Vec::new(),
                aborted: false,
            };
            // Phase runners validate individual tasks; root Goal acceptance belongs to
            // the final Team DeliveryGate and must not be applied to an incomplete phase.
            sub_state.goal.acceptance.clear();
            sub_state.goal.verification_plan = None;
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
                review_issues_pending: state
                    .delivery_issues
                    .iter()
                    .any(|issue| issue.status != crate::goal::DeliveryIssueStatusV1::Resolved),
                base_steps_taken: state.steps_taken,
                base_total_retries: state.total_retries,
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
        // 十一期：并行度来自团队预算 `max_parallel`（缺省 4；1..=8 收敛）。
        let config = RunnerConfig {
            max_parallel: (claim.sub_state.goal.budget.max_parallel as usize).clamp(1, 8),
            persist_dir: None,   // 阶段结束由协调器合并完整状态后统一落盘
            allow_replan: false, // 团队运行失败 = 显式失败（由 continue 决定重试）
            ..Default::default()
        };
        let is_code_change_template = claim.meta.template_id.as_deref()
            == Some(crate::builtin_team_templates::CODE_CHANGE_V1);
        let parallel_leader_uses_host_manifest =
            claim.meta.parallel && !parallel_tasks_require_integration(&claim.sub_state.plan.steps);
        let mut runner = GoalRunner::from_state(claim.sub_state, config);
        if let Some(root) = self.verification_workspace(team_id) {
            runner.attach_workspace_verification_root(root);
        }
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        runner.attach_step_progress(progress_tx);
        if is_code_change_template || parallel_leader_uses_host_manifest {
            let changes_path = self
                .run_dir
                .join(format!("{team_id}-workspace-changes.json"));
            let reviewer_roles = claim
                .meta
                .roles
                .iter()
                .filter(|spec| spec.is_reviewer())
                .map(|spec| spec.role.clone())
                .collect::<std::collections::HashSet<_>>();
            let review_issues_pending = claim.review_issues_pending;
            runner.attach_step_skipper(move |step| {
                let role = worker_role(&step.worker).unwrap_or_default();
                if parallel_leader_uses_host_manifest && role == "leader" {
                    return Some(PARALLEL_LEADER_HOST_MANIFEST_SKIP_REASON.to_string());
                }
                if is_code_change_template {
                    let is_reviewer = reviewer_roles.contains(&role);
                    if is_reviewer && review_issues_pending {
                        return None;
                    }
                    let has_changes =
                        super::coord_artifacts::workspace_change_status(&changes_path)
                            != Some(false);
                    return crate::team_strategy::review_runtime_skip_reason(
                        is_reviewer,
                        has_changes,
                    );
                }
                None
            });
        }
        let mut dynamic_skips: Vec<(String, String)> = Vec::new();
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
            let cancel_fut = wait_cancel(&cancel);
            tokio::pin!(cancel_fut);
            let mut cancel_watch_done = false;
            let result = 'run: loop {
                tokio::select! {
                    r = &mut run_fut => break 'run r,
                    Some(update) = progress_rx.recv() => {
                        if let Some(reason) = update.skip_reason.clone() {
                            let role = update.worker.strip_prefix("m-").unwrap_or(&update.worker);
                            dynamic_skips.push((role.to_string(), reason));
                        }
                        self.persist_step_progress(
                            team_id,
                            claim.epoch,
                            claim.base_steps_taken,
                            claim.base_total_retries,
                            update,
                        )
                        .await?;
                    }
                    cancelled = &mut cancel_fut, if !cancel_watch_done => {
                        if !cancelled {
                            cancel_watch_done = true;
                            continue;
                        }
                        abort_signal.store(true, std::sync::atomic::Ordering::SeqCst);
                        let deadline = tokio::time::Instant::now()
                            + crate::goal::PHASE_CANCELLATION_CLEANUP_GRACE;
                        loop {
                            tokio::select! {
                                r = &mut run_fut => break 'run r,
                                Some(update) = progress_rx.recv() => {
                                    if let Some(reason) = update.skip_reason.clone() {
                                        let role = update.worker.strip_prefix("m-").unwrap_or(&update.worker);
                                        dynamic_skips.push((role.to_string(), reason));
                                    }
                                    self.persist_step_progress(
                                        team_id,
                                        claim.epoch,
                                        claim.base_steps_taken,
                                        claim.base_total_retries,
                                        update,
                                    )
                                    .await?;
                                }
                                _ = tokio::time::sleep_until(deadline) => {
                                    break 'run Ok(GoalStatus::Aborted);
                                }
                            }
                        }
                    }
                }
            };
            while let Ok(update) = progress_rx.try_recv() {
                if let Some(reason) = update.skip_reason.clone() {
                    let role = update.worker.strip_prefix("m-").unwrap_or(&update.worker);
                    dynamic_skips.push((role.to_string(), reason));
                }
                self.persist_step_progress(
                    team_id,
                    claim.epoch,
                    claim.base_steps_taken,
                    claim.base_total_retries,
                    update,
                )
                .await?;
            }
            result
        };
        if matches!(result, Ok(GoalStatus::Succeeded)) && !dynamic_skips.is_empty() {
            self.note_adaptive_event(
                team_id,
                json!({
                    "kind": "early_exit",
                    "early_exit": {
                        "reason": "运行期跳过使全部完成条件满足，DAG 提前结束",
                        "skipped_roles": dynamic_skips.iter().map(|(role, _)| role).collect::<Vec<_>>(),
                    },
                }),
            )
            .await;
        }

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
        state.steps_taken = claim
            .base_steps_taken
            .saturating_add(runner.state.steps_taken);
        state.total_retries = claim
            .base_total_retries
            .saturating_add(runner.state.total_retries);
        let mut meta = claim.meta;
        // 十一期：并行开发的任务主动分配——lead 产物就绪时，把 `subtasks`
        // （子任务说明 + 写范围）动态应用到对应 writer 角色（下一次注册表重建生效：
        // 写范围 → 范围租约并发；契约 → 子任务说明进 Prompt）。幂等：无变化不落盘。
        if let Err(e) = self.apply_parallel_assignment(team_id, &mut state, &mut meta) {
            let reason = format!("并行子任务计划无效，已停止调度：{e}");
            self.audit(team_id, "team.parallel_assign_error", reason.clone());
            self.set_run_active(team_id, false);
            self.persist_state(&state)?;
            self.advance_progress(team_id);
            self.clear_phase_claim(team_id, claim.epoch);
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

        // 结构化 changes_requested 在继续下游前收敛为原 owner 的有限返修。
        // 新 attempt 由既有 rework_step 重置 owner 与其下游，保留旧 Artifact/Review 历史。
        let handoffs = self
            .store
            .list_handoffs_by_project(
                team.project_space_id
                    .as_deref()
                    .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?,
            )
            .await?;
        let mut repair_request: Option<(String, String, String, u64, String)> = None;
        'review_scan: for review_step in &state.plan.steps {
            if state
                .records
                .get(&review_step.id)
                .map(|record| record.status)
                != Some(StepStatus::Succeeded)
                || state
                    .records
                    .get(&review_step.id)
                    .is_some_and(|record| record.skip_reason.is_some())
                || !Self::role_spec_of_member(&meta, &review_step.worker)
                    .is_ok_and(|role| role.is_reviewer())
            {
                continue;
            }
            let prefix = format!("{team_id}:{}:", review_step.id);
            let Some(handoff) = handoffs
                .iter()
                .filter(|handoff| {
                    handoff.handoff_id.starts_with(&prefix)
                        && handoff.from_member == review_step.worker
                })
                .max_by(|left, right| left.created_at.cmp(&right.created_at))
            else {
                continue;
            };
            for review_artifact_id in &handoff.output_artifact_refs {
                let Ok(review_artifact) = self.store.get_artifact(review_artifact_id).await else {
                    continue;
                };
                if review_artifact.kind != "review" {
                    continue;
                }
                let Some(hash) = review_artifact.content_ref.strip_prefix("cas://sha256:") else {
                    continue;
                };
                let Some(content) = self.cas.get_text(hash) else {
                    continue;
                };
                let Ok(document) = serde_json::from_str::<Value>(&content) else {
                    continue;
                };
                let review_verdict = document.pointer("/result/verdict").and_then(Value::as_str);
                if review_verdict == Some("approved") {
                    if let Some(reviewed) = document.get("reviewed_artifacts").and_then(Value::as_array) {
                        resolve_review_issues(
                            &mut state,
                            &review_artifact.artifact_id,
                            &review_artifact.sha256,
                            reviewed,
                        );
                    }
                    continue;
                }
                if review_verdict != Some("changes_requested") {
                    continue;
                }
                let Some(finding) = document
                    .pointer("/result/findings")
                    .and_then(Value::as_array)
                    .and_then(|findings| {
                        findings.iter().find(|finding| {
                            finding
                                .get("detail")
                                .and_then(Value::as_str)
                                .is_some_and(|detail| !detail.trim().is_empty())
                        })
                    })
                else {
                    self.set_run_active(team_id, false);
                    let reason = format!(
                        "review {} 请求修改但没有可派发的结构化 finding",
                        review_artifact.artifact_id
                    );
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                };
                let Some(reviewed) = document.get("reviewed_artifacts").and_then(Value::as_array)
                else {
                    self.set_run_active(team_id, false);
                    let reason = format!(
                        "review {} 缺少被审快照，不能安全派发返修",
                        review_artifact.artifact_id
                    );
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                };
                let owner_binding = select_reviewed_owner_binding(reviewed, finding);
                let Some(owner_binding) = owner_binding else {
                    self.set_run_active(team_id, false);
                    let reason = format!(
                        "review {} 未能唯一关联被审任务；请为多任务 owner 提供 target_task_id 或 target_artifact_id",
                        review_artifact.artifact_id
                    );
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                };
                let owner = owner_binding
                    .get("producer")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if owner == review_artifact.producer || owner.trim().is_empty() {
                    self.set_run_active(team_id, false);
                    let reason = format!(
                        "review {} 建议的 owner 不是独立生产者",
                        review_artifact.artifact_id
                    );
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                }
                let bound_task_id = owner_binding.get("task_id").and_then(Value::as_str);
                let bound_attempt_id = owner_binding.get("attempt_id").and_then(Value::as_str);
                let owner_steps = state
                    .plan
                    .steps
                    .iter()
                    .filter(|step| {
                        step.worker == owner
                            && state.records.get(&step.id).is_some_and(|record| {
                                record.status == StepStatus::Succeeded
                                    && bound_attempt_id.is_none_or(|attempt_id| {
                                        record.attempt_id.as_deref() == Some(attempt_id)
                                    })
                            })
                            && bound_task_id.is_none_or(|task_id| {
                                step.id == task_id
                                    || step.input.get("assigned_task_id").and_then(Value::as_str)
                                        == Some(task_id)
                            })
                    })
                    .collect::<Vec<_>>();
                let Some(owner_step) = (owner_steps.len() == 1).then(|| owner_steps[0]) else {
                    self.set_run_active(team_id, false);
                    let reason = format!(
                        "review {} 的 owner {} 未能唯一对应已接受的生产步骤",
                        review_artifact.artifact_id, owner
                    );
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                };
                let attempt = owner_step
                    .input
                    .get("rework")
                    .and_then(|rework| rework.get("attempt"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let detail = finding
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let evidence = finding
                    .get("evidence_refs")
                    .and_then(Value::as_array)
                    .map(|refs| refs.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                    .unwrap_or_default();
                let instruction = format!(
                    "修复评审 finding：{detail}。依据：{}。保持该步骤原有任务、验收条件和写范围，只改动解决该 finding 所需的最小范围。",
                    if evidence.is_empty() {
                        "未提供可定位证据".to_string()
                    } else {
                        evidence.join(", ")
                    }
                );
                let severity = finding
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("major")
                    .to_string();
                let requirement_id = finding
                    .get("requirement_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let bound_task_id = bound_task_id.unwrap_or(owner_step.id.as_str()).to_string();
                let bound_attempt_id = bound_attempt_id
                    .or_else(|| {
                        state
                            .records
                            .get(&owner_step.id)
                            .and_then(|record| record.attempt_id.as_deref())
                    })
                    .unwrap_or_default()
                    .to_string();
                if bound_attempt_id.is_empty() {
                    self.set_run_active(team_id, false);
                    let reason = format!(
                        "review {} 的被审任务缺少当前 attempt 身份，不能登记可追溯 Issue",
                        review_artifact.artifact_id
                    );
                    self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                        .await?;
                    return Ok(PhaseOutcome::Failed);
                }
                let finding_bytes = serde_json::to_vec(finding)
                    .map_err(|error| WorkSwarmError::Run(format!("finding 序列化失败：{error}")))?;
                let finding_sha256 = crate::CasStore::hash_of(&finding_bytes);
                let issue_identity = serde_json::json!({
                    "team_id": team_id,
                    "task_id": &bound_task_id,
                    "requirement_id": &requirement_id,
                    "severity": &severity,
                    "finding_sha256": &finding_sha256,
                });
                let issue_identity_bytes = serde_json::to_vec(&issue_identity)
                    .map_err(|error| WorkSwarmError::Run(format!("Issue 身份序列化失败：{error}")))?;
                let issue_id = format!(
                    "issue-{}",
                    crate::CasStore::hash_of(&issue_identity_bytes)
                );
                let now = now_ts();
                let issue = crate::goal::DeliveryIssueV1 {
                    issue_id: issue_id.clone(),
                    source_review_artifact_id: review_artifact.artifact_id.clone(),
                    source_review_sha256: review_artifact.sha256.clone(),
                    finding_sha256,
                    severity,
                    detail: detail.to_string(),
                    requirement_id,
                    target_task_id: bound_task_id,
                    target_attempt_id: bound_attempt_id,
                    target_artifact_id: owner_binding
                        .get("artifact_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    owner_step_id: owner_step.id.clone(),
                    status: crate::goal::DeliveryIssueStatusV1::Open,
                    repair_attempt: attempt.saturating_add(1) as u32,
                    resolution_review_artifact_id: None,
                    resolution_review_sha256: None,
                    resolution_attempt_id: None,
                    opened_at: now.clone(),
                    updated_at: now,
                };
                if let Some(existing) = state
                    .delivery_issues
                    .iter_mut()
                    .find(|existing| existing.issue_id == issue_id)
                {
                    *existing = issue;
                } else {
                    state.delivery_issues.push(issue);
                }
                repair_request = Some((
                    owner_step.id.clone(),
                    instruction,
                    review_artifact.artifact_id.clone(),
                    attempt,
                    issue_id,
                ));
                break 'review_scan;
            }
        }
        if let Some((owner_step_id, instruction, review_id, attempt, issue_id)) = repair_request {
            if attempt >= 2 {
                self.set_run_active(team_id, false);
                let reason = format!("评审问题在两次局部返修后仍未关闭：{}", review_id);
                self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                    .await?;
                return Ok(PhaseOutcome::Failed);
            }
            self.persist_state(&state)?;
            drop(_guard);
            self.rework_from_review(team_id, &owner_step_id, &instruction, &review_id)
                .await?;
            self.audit(
                team_id,
                "team.review.repair_dispatched",
                format!(
                    "review={review_id} issue={issue_id} owner_step={owner_step_id} attempt={}",
                    attempt + 1
                ),
            );
            return Ok(PhaseOutcome::MoreReady);
        }

        // Persist issue resolutions produced by the review scan before returning Done or
        // scheduling unrelated ready work; DeliveryGate reads this durable ledger.
        self.persist_state(&state)?;

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

    /// 十一期：并行开发的任务主动分配（幂等）。
    ///
    /// 运行条件：`RunMeta.parallel` 为真，且 **lead 步骤已成功**且产物可解析为
    /// TaskGraphV1（兼容旧 subtasks 数组、围栏与前后缀文本）。先校验任务字段、依赖
    /// DAG、任务数量和写范围，再绑定到有限 worker 槽位；同一槽位内的任务顺序串行。
    /// 动态步骤保留任务 ID、验收、验证、引用和能力需求，写路径权限不超出已有授权。
    ///
    /// 无变化不落盘、不重复审计；lead 产物不可解析时保持 writer 原有权限（缺省
    /// 只读），并记 `team.parallel_assign_skip` 审计——绝不因调度信息缺失而放宽权限。
    fn apply_parallel_assignment(
        &self,
        team_id: &str,
        state: &mut GoalRunState,
        meta: &mut RunMeta,
    ) -> WorkSwarmResult<()> {
        if !meta.parallel {
            return Ok(());
        }
        // Lead output is revisited after every phase; do not append the same DAG twice.
        if state
            .plan
            .steps
            .iter()
            .any(|step| step.input.get("assigned_task_id").is_some())
        {
            return Ok(());
        }
        let Some(lead_step_id) = state
            .plan
            .steps
            .iter()
            .find(|s| worker_role(&s.worker).as_deref() == Some("lead"))
            .map(|step| step.id.clone())
        else {
            return Ok(());
        };
        if state.records.get(&lead_step_id).map(|record| record.status)
            != Some(StepStatus::Succeeded)
        {
            return Ok(());
        }
        let output = state
            .records
            .get(&lead_step_id)
            .and_then(|record| record.output.clone())
            .ok_or_else(|| WorkSwarmError::Validation("lead 成功步骤缺少子任务产物".to_string()))?;
        let (subtasks, versioned) = parse_parallel_subtasks(&output).ok_or_else(|| {
            WorkSwarmError::Validation("lead 产物不是可解析的 TaskGraph JSON".to_string())
        })?;
        let task_graph = validate_parallel_subtasks(&meta.roles, &subtasks, versioned)
            .map_err(WorkSwarmError::Validation)?;
        if task_graph.version != 1 {
            return Err(WorkSwarmError::Validation(
                "unsupported TaskGraph version".to_string(),
            ));
        }
        let assignments = task_graph.tasks;
        let writers: Vec<String> = meta
            .roles
            .iter()
            .filter(|role| is_parallel_writer_name(&role.role))
            .map(|role| role.role.clone())
            .collect();
        let original_inputs: std::collections::BTreeMap<String, Value> = writers
            .iter()
            .filter_map(|worker| {
                state
                    .plan
                    .steps
                    .iter()
                    .find(|step| step.id == format!("s-{worker}"))
                    .map(|step| (worker.clone(), step.input.clone()))
            })
            .collect();
        let original_ids: std::collections::BTreeSet<String> =
            writers.iter().map(|worker| format!("s-{worker}")).collect();
        state
            .plan
            .steps
            .retain(|step| !original_ids.contains(&step.id));
        for worker in &writers {
            state.records.remove(&format!("s-{worker}"));
        }

        // Bind a DAG of any size to the finite set of worker slots. A slot is a
        // capacity limit, not a serial dependency: independent tasks may overlap and
        // task-scoped sessions, permissions, and write leases preserve isolation.
        let mut step_ids = std::collections::BTreeMap::new();
        let mut first_for_worker = std::collections::BTreeSet::new();
        for task in &assignments {
            let id = if first_for_worker.insert(task.worker.clone()) {
                format!("s-{}", task.worker)
            } else {
                format!("s-task-{}", task.task_id)
            };
            step_ids.insert(task.task_id.clone(), id);
        }
        // Bind stable task-to-step identities using source order; sort only the runnable
        // plan entries so priority hints never rename or reassign an existing task slot.
        let mut scheduled_indices: Vec<usize> = (0..assignments.len()).collect();
        scheduled_indices.sort_by(|left, right| {
            let left = &assignments[*left];
            let right = &assignments[*right];
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| right.estimated_effort.cmp(&left.estimated_effort))
                .then_with(|| left.task_id.cmp(&right.task_id))
        });
        let mut scope_union = std::collections::BTreeMap::<String, Vec<String>>::new();
        for task_index in scheduled_indices {
            let task = &assignments[task_index];
            let step_id = step_ids[&task.task_id].clone();
            let mut dependencies: Vec<String> = task
                .depends_on
                .iter()
                .map(|dep| step_ids[dep].clone())
                .collect();
            if dependencies.is_empty() {
                dependencies.push(lead_step_id.clone());
            }
            let mut input = original_inputs
                .get(&task.worker)
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !input.is_object() {
                input = json!({});
            }
            if let Some(obj) = input.as_object_mut() {
                obj.insert("assigned_task_id".into(), json!(task.task_id));
                obj.insert("assigned_task".into(), json!(task.task));
                obj.insert("assigned_acceptance".into(), json!(task.acceptance));
                let assigned_verification = task
                    .verification_plan
                    .as_ref()
                    .map(|plan| json!(plan))
                    .or_else(|| task.verification.as_ref().map(|value| json!(value)))
                    .unwrap_or(Value::Null);
                obj.insert("assigned_verification".into(), assigned_verification);
                obj.insert("assigned_write_paths".into(), json!(task.effective_paths));
                obj.insert("assigned_read_refs".into(), json!(task.read_refs));
                obj.insert("assigned_contract_refs".into(), json!(task.contract_refs));
                obj.insert(
                    "required_capabilities".into(),
                    json!(task.required_capabilities),
                );
                obj.insert("estimated_effort".into(), json!(task.estimated_effort));
                obj.insert("risk".into(), json!(task.risk));
                obj.insert("priority".into(), json!(task.priority));
                let ws = obj.entry("_workswarm").or_insert_with(|| json!({}));
                if let Some(ws) = ws.as_object_mut() {
                    ws.insert("step_id".into(), json!(step_id));
                    ws.insert("member_id".into(), json!(format!("m-{}", task.worker)));
                }
            }
            scope_union
                .entry(task.worker.clone())
                .or_default()
                .extend(task.effective_paths.iter().cloned());
            let (verification, verification_plan) = if let Some(plan) = &task.verification_plan {
                (None, plan.clone())
            } else {
                let verification = parse_verify(
                    task.verification
                        .as_deref()
                        .expect("TaskGraph parser requires legacy verification or a plan"),
                );
                let plan = verification_plan_for_step(&step_id, &verification);
                (Some(verification), plan)
            };
            state.plan.steps.push(StepSpec {
                id: step_id.clone(),
                depends_on: dependencies,
                parallel: true,
                worker: format!("m-{}", task.worker),
                input,
                verify: verification,
                verification_plan: Some(verification_plan),
                retries: 0,
            });
            state.records.insert(
                step_id.clone(),
                crate::goal::StepRecord {
                    step_id,
                    status: StepStatus::Pending,
                    attempts: 0,
                    output: None,
                    error: None,
                    skip_reason: None,
                    phase_epoch: None,
                    attempt_id: None,
                    validation_receipts: Vec::new(),
                },
            );
        }
        let high_risk_task_step_ids: Vec<String> = assignments
            .iter()
            .filter(|task| matches!(task.risk.as_str(), "high" | "critical"))
            .map(|task| step_ids[&task.task_id].clone())
            .collect();
        let assignment_step_ids = assignments
            .iter()
            .map(|task| step_ids[&task.task_id].clone())
            .collect::<Vec<_>>();
        let scheduled_reviewer_step_ids = state
            .plan
            .steps
            .iter()
            .filter(|step| {
                meta.roles.iter().any(|role| {
                    step.worker == format!("m-{}", role.role) && is_independent_reviewer_role(role)
                })
            })
            .map(|step| step.id.clone())
            .collect::<Vec<_>>();
        if !high_risk_task_step_ids.is_empty() && scheduled_reviewer_step_ids.is_empty() {
            return Err(WorkSwarmError::Validation(
                "high-risk TaskGraph has no scheduled independent reviewer step".to_string(),
            ));
        }
        bind_dynamic_follow_up_dependencies(
            &mut state.plan.steps,
            &assignment_step_ids,
            &scheduled_reviewer_step_ids,
        );
        for role in meta
            .roles
            .iter_mut()
            .filter(|role| is_parallel_writer_name(&role.role))
        {
            let mut paths = scope_union.remove(&role.role).unwrap_or_default();
            paths.sort();
            paths.dedup();
            role.write_paths = paths;
            let role_contract = role
                .handoff_contract
                .as_deref()
                .filter(|contract| !contract.trim().is_empty())
                .unwrap_or("遵守模板为该 Worker 槽位声明的任务边界。");
            role.handoff_contract = Some(format!(
                "{role_contract}\n\n你是并行执行者 {}。只执行当前输入 assigned_task，按 assigned_acceptance 验收；\
                 写操作只允许命中 assigned_write_paths。逐任务提交结果和证据。",
                role.role
            ));
        }
        state.plan.validate().map_err(WorkSwarmError::Validation)?;
        meta.save(&self.run_dir)?;
        self.audit(
            team_id,
            "team.parallel_assigned",
            format!(
                "并行任务图已应用：{} 个任务复用 {} 个 Worker 槽位",
                assignments.len(),
                writers.len()
            ),
        );

        Ok(())
    }
}

/// Resolve a finding only to a reviewed host-bound Artifact. Old findings remain
/// compatible when the suggested producer owns exactly one reviewed task.
fn resolve_review_issues(
    state: &mut GoalRunState,
    review_artifact_id: &str,
    review_sha256: &str,
    reviewed_artifacts: &[Value],
) -> usize {
    let now = now_ts();
    let mut resolved = 0usize;
    for binding in reviewed_artifacts {
        let Some(task_id) = binding.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(attempt_id) = binding.get("attempt_id").and_then(Value::as_str) else {
            continue;
        };
        let current_attempt_matches = state
            .records
            .get(task_id)
            .and_then(|record| record.attempt_id.as_deref())
            == Some(attempt_id);
        if !current_attempt_matches {
            continue;
        }
        for issue in &mut state.delivery_issues {
            if issue.status == crate::goal::DeliveryIssueStatusV1::RepairDispatched
                && issue.target_task_id == task_id
                && issue.target_attempt_id != attempt_id
            {
                issue.status = crate::goal::DeliveryIssueStatusV1::Resolved;
                issue.resolution_review_artifact_id = Some(review_artifact_id.to_string());
                issue.resolution_review_sha256 = Some(review_sha256.to_string());
                issue.resolution_attempt_id = Some(attempt_id.to_string());
                issue.updated_at = now.clone();
                resolved += 1;
            }
        }
    }
    resolved
}

fn select_reviewed_owner_binding<'a>(reviewed: &'a [Value], finding: &Value) -> Option<&'a Value> {
    let owner = finding
        .get("suggested_owner")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let task_id = finding
        .get("target_task_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let artifact_id = finding
        .get("target_artifact_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let matches = reviewed
        .iter()
        .filter(|artifact| {
            owner.is_none_or(|owner| {
                artifact.get("producer").and_then(Value::as_str) == Some(owner)
            }) && task_id.is_none_or(|task_id| {
                artifact.get("task_id").and_then(Value::as_str) == Some(task_id)
            }) && artifact_id.is_none_or(|artifact_id| {
                artifact.get("artifact_id").and_then(Value::as_str) == Some(artifact_id)
            })
        })
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0])
}

/// `w<数字>`（并行 writer 角色名；lead/leader 不在其列）。
#[derive(Debug)]
struct TaskGraphV1 {
    version: u32,
    tasks: Vec<ParallelTaskAssignment>,
}

#[derive(Debug)]
struct ParallelTaskAssignment {
    task_id: String,
    worker: String,
    worker_explicit: bool,
    task: String,
    acceptance: String,
    depends_on: Vec<String>,
    effective_paths: Vec<String>,
    read_refs: Vec<String>,
    contract_refs: Vec<String>,
    required_capabilities: Vec<String>,
    estimated_effort: u64,
    verification: Option<String>,
    verification_plan: Option<crate::plan::VerificationPlanV1>,
    risk: String,
    priority: u8,
}

/// Validate a bounded DAG separately from worker capacity, then bind omitted workers.
fn is_independent_reviewer_role(role: &RoleSpec) -> bool {
    role.is_reviewer()
        && role.role != "lead"
        && role.role != "leader"
        && !is_parallel_writer_name(&role.role)
}

pub(super) const PARALLEL_LEADER_HOST_MANIFEST_SKIP_REASON: &str =
    "host_manifest:independent_task_graph";

fn should_enable_parallel_assignment(explicit: bool, roles: &[RoleSpec]) -> bool {
    explicit || roles.iter().any(|role| role.role == "lead")
}

/// Rebind fixed integration steps to every dynamic task, including additional tasks
/// assigned to a worker whose first task reuses the legacy s-wN step ID.
fn bind_dynamic_follow_up_dependencies(
    steps: &mut [StepSpec],
    assignment_step_ids: &[String],
    scheduled_reviewer_step_ids: &[String],
) {
    let available_step_ids = steps
        .iter()
        .map(|step| step.id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let integration_step_ids = steps
        .iter()
        .filter(|step| {
            worker_role(&step.worker)
                .is_some_and(|role| matches!(role.as_str(), "leader" | "project_integrator"))
        })
        .map(|step| step.id.clone())
        .collect::<Vec<_>>();
    for step in steps {
        let role = worker_role(&step.worker);
        let Some(role) = role.as_deref() else {
            continue;
        };
        let is_scheduled_reviewer = scheduled_reviewer_step_ids.contains(&step.id);
        let is_integrator = matches!(role, "leader" | "project_integrator");
        if is_scheduled_reviewer || is_integrator {
            // Review the integrated source snapshot: validators wait for every dynamic
            // task and every integration step. Integrators consume candidate artifacts
            // without waiting for review, so the final review cannot precede later edits.
            let mut dependencies = step
                .depends_on
                .iter()
                .filter(|dependency| {
                    available_step_ids.contains(*dependency)
                        && !(is_integrator && scheduled_reviewer_step_ids.contains(*dependency))
                })
                .cloned()
                .collect::<Vec<_>>();
            dependencies.extend(assignment_step_ids.iter().cloned());
            if is_scheduled_reviewer {
                dependencies.extend(integration_step_ids.iter().cloned());
            }
            dependencies.retain(|dependency| dependency != &step.id);
            dependencies.sort();
            dependencies.dedup();
            step.depends_on = dependencies;
        }
    }
}

/// Independent dynamic tasks need no model-based leader pass: the host publishes
/// their individually accepted artifacts in the final manifest. Dependencies,
/// shared write surfaces, or shared contract references require explicit integration.
pub(super) fn parallel_tasks_require_integration(steps: &[StepSpec]) -> bool {
    let tasks = steps
        .iter()
        .filter(|step| {
            step.input
                .get("assigned_task_id")
                .and_then(Value::as_str)
                .is_some()
        })
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        return true;
    }
    for (index, task) in tasks.iter().enumerate() {
        if task
            .depends_on
            .iter()
            .any(|dependency| tasks.iter().any(|candidate| candidate.id == *dependency))
        {
            return true;
        }
        let paths = task
            .input
            .get("assigned_write_paths")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .collect::<Vec<_>>();
        let contracts = task
            .input
            .get("assigned_contract_refs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();
        for other in tasks.iter().skip(index + 1) {
            let other_paths = other
                .input
                .get("assigned_write_paths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|path| !path.trim().is_empty());
            if paths.iter().any(|left| {
                other_paths
                    .clone()
                    .any(|right| write_paths_overlap(left, right))
            }) {
                return true;
            }
            let other_contracts = other
                .input
                .get("assigned_contract_refs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str);
            if other_contracts
                .into_iter()
                .any(|item| contracts.contains(item))
            {
                return true;
            }
        }
    }
    false
}

fn validate_parallel_subtasks(
    roles: &[RoleSpec],
    subtasks: &[Value],
    versioned: bool,
) -> Result<TaskGraphV1, String> {
    let writers: Vec<&RoleSpec> = roles
        .iter()
        .filter(|role| is_parallel_writer_name(&role.role))
        .collect();
    if writers.is_empty() {
        return Err("RunMeta contains no parallel writer roles".into());
    }
    if subtasks.is_empty() || subtasks.len() > 128 {
        return Err(format!(
            "task count must be 1..=128, got {}",
            subtasks.len()
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut tasks = Vec::with_capacity(subtasks.len());
    for (index, item) in subtasks.iter().enumerate() {
        let task_id = item
            .get("task_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| (!versioned).then(|| format!("task-{}", index + 1)))
            .ok_or_else(|| format!("task at index {index} requires task_id"))?;
        if task_id.len() > 120
            || !task_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("invalid task_id {task_id}"));
        }
        if !ids.insert(task_id.clone()) {
            return Err(format!("duplicate task_id {task_id}"));
        }
        let estimated_effort = item
            .get("estimated_effort")
            .and_then(Value::as_u64)
            .or_else(|| (!versioned).then_some(1))
            .filter(|effort| (1..=1_000_000).contains(effort))
            .ok_or_else(|| format!("task {task_id} requires estimated_effort in 1..=1000000"))?;
        let (worker, worker_explicit) = match item.get("worker") {
            Some(Value::String(worker)) => {
                if !writers
                    .iter()
                    .any(|role| role.role.as_str() == worker.as_str())
                {
                    return Err(format!("task {task_id} refers to unknown worker {worker}"));
                }
                (worker.clone(), true)
            }
            None => (String::new(), false),
            Some(_) => return Err(format!("task {task_id} worker must be a string")),
        };
        let task = item
            .get("task")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("task {task_id} has empty goal"))?
            .to_owned();
        if task.chars().count() > 4_000 {
            return Err(format!("task {task_id} goal exceeds 4000 characters"));
        }
        let acceptance = item
            .get("acceptance")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("task {task_id} has empty acceptance"))?
            .to_owned();
        if acceptance.chars().count() > 2_000 {
            return Err(format!("task {task_id} acceptance exceeds 2000 characters"));
        }
        let raw = item
            .get("write_paths")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("task {task_id} has no write_paths array"))?;
        if raw.len() > 64 {
            return Err(format!("task {task_id} has more than 64 write paths"));
        }
        let paths: Vec<String> = raw
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && s.len() <= 512)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        format!("task {task_id} has invalid write path (empty or over 512 bytes)")
                    })
            })
            .collect::<Result<_, _>>()?;
        if paths
            .iter()
            .any(|path| normalized_write_path(path).is_empty())
        {
            return Err(format!(
                "task {task_id} write_paths cannot target the workspace root"
            ));
        }
        let string_refs = |key: &str| -> Result<Vec<String>, String> {
            match item.get(key) {
                Some(Value::Array(values)) if values.len() <= 64 => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::trim)
                            .filter(|value| !value.is_empty() && value.len() <= 512)
                            .map(str::to_owned)
                            .ok_or_else(|| format!("task {task_id} has invalid {key} entry"))
                    })
                    .collect(),
                None if !versioned => Ok(Vec::new()),
                _ => Err(format!("task {task_id} requires {key} string array")),
            }
        };
        let read_refs = string_refs("read_refs")?;
        let contract_refs = string_refs("contract_refs")?;
        let required_capabilities = string_refs("required_capabilities")?;
        const TASK_CAPABILITIES: &[&str] = &[
            "read_file",
            "list_dir",
            "search_files",
            "write_file",
            "apply_patch",
            "run_command",
        ];
        if required_capabilities.len() > 8 {
            return Err(format!(
                "task {task_id} has more than 8 required capabilities"
            ));
        }
        if let Some(unsupported) = required_capabilities
            .iter()
            .find(|cap| !TASK_CAPABILITIES.contains(&cap.as_str()))
        {
            return Err(format!(
                "task {task_id} requires unsupported scoped capability {unsupported}"
            ));
        }
        if versioned
            && paths.is_empty()
            && required_capabilities
                .iter()
                .any(|cap| matches!(cap.as_str(), "write_file" | "apply_patch"))
        {
            return Err(format!(
                "task {task_id} requires a write capability but declares no write_paths"
            ));
        }
        let (verification, verification_plan) = match item.get("verification") {
            Some(Value::String(value)) => {
                let value = value.trim();
                let valid = value.len() <= 2_048
                    && (value == "non_empty"
                        || value
                            .strip_prefix("contains:")
                            .is_some_and(|expected| !expected.is_empty())
                        || value
                            .strip_prefix("equals:")
                            .is_some_and(|expected| !expected.is_empty()));
                if !valid {
                    return Err(format!("task {task_id} has invalid legacy verification"));
                }
                (Some(value.to_string()), None)
            }
            Some(value @ Value::Object(_)) => {
                let mut plan: crate::plan::VerificationPlanV1 =
                    serde_json::from_value(value.clone()).map_err(|error| {
                        format!("task {task_id} has invalid VerificationPlan: {error}")
                    })?;
                plan.plan_id = format!("verify-{task_id}");
                if plan.requirements.len() > 16 {
                    return Err(format!("task {task_id} has more than 16 verification requirements"));
                }
                plan.validate()
                    .map_err(|error| format!("task {task_id} has invalid VerificationPlan: {error}"))?;
                for (index, requirement) in plan.requirements.iter_mut().enumerate() {
                    let crate::plan::VerificationScopeV1::WorkspacePaths { relative_paths } =
                        &mut requirement.scope
                    else {
                        return Err(format!(
                            "task {task_id} dynamic VerificationPlan only permits WorkspacePaths"
                        ));
                    };
                    if !requirement.required
                        || requirement.resources.cpu_slots != 1
                        || !(8..=128).contains(&requirement.resources.memory_mb)
                        || requirement.resources.exclusive_workspace
                        || requirement.resources.timeout_ms == 0
                        || requirement.resources.timeout_ms > 30_000
                    {
                        return Err(format!(
                            "task {task_id} verification requirements must be required and stay within the host resource budget"
                        ));
                    }
                    if !crate::verification::is_registered_workspace_validator(
                        &requirement.validator_id,
                    ) || !crate::verification::workspace_validator_arguments_supported(
                        &requirement.validator_id,
                        &requirement.arguments,
                    ) {
                        return Err(format!(
                            "task {task_id} uses unregistered workspace validator {}",
                            requirement.validator_id
                        ));
                    }
                    if relative_paths.iter().any(|path| path.len() > 512) {
                        return Err(format!(
                            "task {task_id} VerificationPlan path exceeds 512 bytes"
                        ));
                    }
                    super::roles::validate_role_write_paths(
                        &format!("task-{task_id}-validator"),
                        relative_paths,
                    )?;
                    if relative_paths.iter().any(|path| {
                        !paths.iter().any(|write_scope| write_path_is_within(path, write_scope))
                    }) {
                        return Err(format!(
                            "task {task_id} VerificationPlan scope exceeds its assigned write_paths"
                        ));
                    }
                    requirement.requirement_id =
                        format!("{task_id}:requirement:{index}");
                    requirement.covers_requirement_ids.clear();
                }
                plan.validate()
                    .map_err(|error| format!("task {task_id} has invalid VerificationPlan: {error}"))?;
                (None, Some(plan))
            }
            None if !versioned => (Some("non_empty".to_string()), None),
            _ => {
                return Err(format!(
                    "task {task_id} requires verification: a registered WorkspacePaths VerificationPlan or legacy non_empty|contains:<text>|equals:<text>"
                ))
            }
        };
        let has_command_validator = verification_plan.as_ref().is_some_and(|plan| {
            plan.requirements
                .iter()
                .any(|requirement| requirement.validator_id == "workspace-command-success-v1")
        });
        let requires_command_capability = required_capabilities
            .iter()
            .any(|capability| capability == "run_command");
        let declared_source_code = paths
            .iter()
            .any(|path| super::delivery_gate_evidence::is_source_code_path(path));
        if declared_source_code && !has_command_validator {
            return Err(format!(
                "task {task_id} declares a source-code write scope but no registered behavior command"
            ));
        }
        if has_command_validator != requires_command_capability {
            return Err(format!(
                "task {task_id} must request run_command exactly when its VerificationPlan includes workspace-command-success-v1"
            ));
        }
        let risk = item
            .get("risk")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| matches!(*value, "low" | "normal" | "high" | "critical"))
            .map(str::to_owned)
            .or_else(|| (!versioned).then(|| "normal".to_string()))
            .ok_or_else(|| format!("task {task_id} requires risk: low|normal|high|critical"))?;
        if matches!(risk.as_str(), "high" | "critical")
            && !roles.iter().any(is_independent_reviewer_role)
        {
            return Err(format!(
                "high-risk task {task_id} requires an independent reviewer role",
            ));
        }
        let priority = item
            .get("priority")
            .and_then(Value::as_u64)
            .or_else(|| (!versioned).then_some(50))
            .filter(|priority| *priority <= 100)
            .map(|priority| priority as u8)
            .ok_or_else(|| format!("task {task_id} requires priority in 0..=100"))?;
        let depends_on = match item.get("depends_on") {
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::trim)
                        .filter(|dependency| !dependency.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| format!("task {task_id} has invalid dependency"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            None if !versioned => Vec::new(),
            _ => return Err(format!("task {task_id} requires a depends_on string array")),
        };
        if depends_on.len() > 128 {
            return Err(format!("task {task_id} has more than 128 dependencies"));
        }
        let unique_dependencies: std::collections::BTreeSet<_> = depends_on.iter().collect();
        if unique_dependencies.len() != depends_on.len() {
            return Err(format!("task {task_id} has duplicate dependencies"));
        }
        tasks.push(ParallelTaskAssignment {
            task_id,
            worker,
            worker_explicit,
            task,
            acceptance,
            depends_on,
            effective_paths: paths,
            read_refs,
            contract_refs,
            required_capabilities,
            estimated_effort,
            verification,
            verification_plan,
            risk,
            priority,
        });
    }
    for task in &tasks {
        for dep in &task.depends_on {
            if dep == &task.task_id || !ids.contains(dep) {
                return Err(format!(
                    "task {} has invalid dependency {dep}",
                    task.task_id
                ));
            }
        }
    }
    fn visit(
        id: &str,
        tasks: &[ParallelTaskAssignment],
        path: &mut std::collections::BTreeSet<String>,
        done: &mut std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        if done.contains(id) {
            return Ok(());
        }
        if !path.insert(id.to_owned()) {
            return Err(format!("task graph cycle at {id}"));
        }
        if let Some(task) = tasks.iter().find(|task| task.task_id == id) {
            for dep in &task.depends_on {
                visit(dep, tasks, path, done)?;
            }
        }
        path.remove(id);
        done.insert(id.to_owned());
        Ok(())
    }
    let mut path = std::collections::BTreeSet::new();
    let mut done = std::collections::BTreeSet::new();
    for id in &ids {
        visit(id, &tasks, &mut path, &mut done)?;
    }
    let mut critical_paths = std::collections::BTreeMap::new();
    for id in &ids {
        let effort = task_critical_path_effort(id, &tasks, &mut critical_paths);
        critical_paths.insert(id.clone(), effort);
    }

    // Respect explicit worker commitments first, then place implicit tasks largest/longest
    // first onto the currently lightest worker. This avoids JSON input order bias and keeps
    // declared work in the load estimate before assigning the remaining queue.
    let mut assigned_effort = std::collections::BTreeMap::<String, u64>::new();
    for task in tasks.iter().filter(|task| task.worker_explicit) {
        let load = assigned_effort.entry(task.worker.clone()).or_default();
        *load = load.saturating_add(task.estimated_effort);
    }
    let mut implicit_indices: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| (!task.worker_explicit).then_some(index))
        .collect();
    implicit_indices.sort_by(|left, right| {
        let left_task = &tasks[*left];
        let right_task = &tasks[*right];
        let left_path = critical_paths
            .get(&left_task.task_id)
            .copied()
            .unwrap_or(left_task.estimated_effort);
        let right_path = critical_paths
            .get(&right_task.task_id)
            .copied()
            .unwrap_or(right_task.estimated_effort);
        (
            right_path,
            right_task.priority,
            right_task.estimated_effort,
            &right_task.task_id,
        )
            .cmp(&(
                left_path,
                left_task.priority,
                left_task.estimated_effort,
                &left_task.task_id,
            ))
    });
    for index in implicit_indices {
        let worker = writers
            .iter()
            .min_by_key(|role| {
                (
                    assigned_effort.get(&role.role).copied().unwrap_or(0),
                    role.role.as_str(),
                )
            })
            .expect("writer roles are non-empty")
            .role
            .clone();
        let task = &mut tasks[index];
        task.worker = worker.clone();
        let load = assigned_effort.entry(worker).or_default();
        *load = load.saturating_add(task.estimated_effort);
    }
    for task in &tasks {
        let role = writers
            .iter()
            .find(|role| role.role == task.worker)
            .expect("all tasks are assigned to a writer");
        super::roles::validate_role_write_paths(&task.worker, &task.effective_paths)?;
        if !role.write_paths.is_empty() {
            for path in &task.effective_paths {
                let inside = role
                    .write_paths
                    .iter()
                    .any(|base| write_path_is_within(path, base));
                if !inside {
                    return Err(format!(
                        "task {} exceeds pre-authorized writer scope: {path}",
                        task.task_id
                    ));
                }
            }
        }
    }

    let mut ordered = Vec::with_capacity(tasks.len());
    let mut remaining = tasks;
    while !remaining.is_empty() {
        let ready = remaining
            .iter()
            .enumerate()
            .filter(|(_, task)| {
                task.depends_on.iter().all(|dependency| {
                    ordered
                        .iter()
                        .any(|done: &ParallelTaskAssignment| done.task_id == *dependency)
                })
            })
            .max_by_key(|(_, task)| {
                (
                    critical_paths
                        .get(&task.task_id)
                        .copied()
                        .unwrap_or(task.estimated_effort),
                    task.priority,
                    task.estimated_effort,
                )
            })
            .map(|(index, _)| index);
        let Some(index) = ready else {
            return Err("task graph has no topological order".into());
        };
        ordered.push(remaining.remove(index));
    }
    let tasks = ordered;
    for left in 0..tasks.len() {
        for right in left + 1..tasks.len() {
            if tasks[left].effective_paths.iter().any(|a| {
                tasks[right]
                    .effective_paths
                    .iter()
                    .any(|b| write_paths_overlap(a, b))
            }) {
                let ordered = depends_on(&tasks[left].task_id, &tasks[right].task_id, &tasks)
                    || depends_on(&tasks[right].task_id, &tasks[left].task_id, &tasks);
                if !ordered {
                    return Err(format!(
                        "overlapping task write scopes need a dependency: {} / {}",
                        tasks[left].task_id, tasks[right].task_id
                    ));
                }
            }
        }
    }
    Ok(TaskGraphV1 { version: 1, tasks })
}

fn task_critical_path_effort(
    task_id: &str,
    tasks: &[ParallelTaskAssignment],
    memo: &mut std::collections::BTreeMap<String, u64>,
) -> u64 {
    if let Some(effort) = memo.get(task_id) {
        return *effort;
    }
    let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
        return 0;
    };
    let longest_child = tasks
        .iter()
        .filter(|candidate| {
            candidate
                .depends_on
                .iter()
                .any(|dependency| dependency == task_id)
        })
        .map(|child| task_critical_path_effort(&child.task_id, tasks, memo))
        .max()
        .unwrap_or(0);
    let effort = task.estimated_effort.saturating_add(longest_child);
    memo.insert(task_id.to_string(), effort);
    effort
}

fn depends_on(before: &str, after: &str, tasks: &[ParallelTaskAssignment]) -> bool {
    let mut pending = vec![after.to_owned()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if id == before {
            return true;
        }
        if seen.insert(id.clone()) {
            if let Some(task) = tasks.iter().find(|task| task.task_id == id) {
                pending.extend(task.depends_on.iter().cloned());
            }
        }
    }
    false
}

fn normalized_write_path(path: &str) -> String {
    path.trim()
        .replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>()
        .join("/")
        .to_lowercase()
}

fn write_path_is_within(path: &str, base: &str) -> bool {
    let path = normalized_write_path(path);
    let base = normalized_write_path(base);
    base.is_empty() || path == base || path.starts_with(&format!("{base}/"))
}

fn write_paths_overlap(left: &str, right: &str) -> bool {
    let left = normalized_write_path(left);
    let right = normalized_write_path(right);
    left.is_empty()
        || right.is_empty()
        || left == right
        || left.starts_with(&format!("{right}/"))
        || right.starts_with(&format!("{left}/"))
}

fn is_parallel_writer_name(role: &str) -> bool {
    crate::worker_profile::is_parallel_writer_name(role)
}

/// 解析 lead 产物的 TaskGraphV1：裸 JSON / 围栏 / 前后缀文本取首个 `{...}`；
/// 兼容旧裸数组与 subtasks 对象。解析失败返回 None，由调用方终止团队，避免虚假成功。
fn parse_parallel_subtasks(output: &str) -> Option<(Vec<Value>, bool)> {
    let text = crate::workswarm_output::strip_code_fences(output);
    let parsed: Value = serde_json::from_str(text.trim()).ok().or_else(|| {
        let start = text.find('{')?;
        let end = text.rfind('}')?;
        (end > start).then(|| serde_json::from_str(&text[start..=end]).ok())?
    })?;
    match parsed {
        Value::Array(items) => Some((items, false)),
        Value::Object(obj) => {
            let versioned = obj.contains_key("tasks");
            if versioned && obj.get("version").and_then(Value::as_u64) != Some(1) {
                return None;
            }
            if obj
                .get("version")
                .and_then(Value::as_u64)
                .is_some_and(|version| version != 1)
            {
                return None;
            }
            obj.get("tasks")
                .or_else(|| obj.get("subtasks"))
                .and_then(Value::as_array)
                .cloned()
                .map(|items| (items, versioned))
        }
        _ => None,
    }
}

#[cfg(test)]
mod review_owner_binding_tests {
    use super::select_reviewed_owner_binding;
    use serde_json::{json, Value};

    fn reviewed() -> Vec<Value> {
        vec![
            json!({"producer":"m-w1", "task_id":"step-a", "artifact_id":"artifact-a"}),
            json!({"producer":"m-w1", "task_id":"step-b", "artifact_id":"artifact-b"}),
        ]
    }

    #[test]
    fn legacy_owner_binding_remains_compatible_when_unique() {
        let finding = json!({"suggested_owner":"m-w1"});
        let only_artifact = vec![reviewed()[0].clone()];
        assert_eq!(
            select_reviewed_owner_binding(&only_artifact, &finding)
                .and_then(|artifact| artifact.get("task_id"))
                .and_then(Value::as_str),
            Some("step-a")
        );
    }

    #[test]
    fn ambiguous_legacy_owner_does_not_pick_the_first_task() {
        let finding = json!({"suggested_owner":"m-w1"});
        assert!(select_reviewed_owner_binding(&reviewed(), &finding).is_none());
    }

    #[test]
    fn task_or_artifact_identity_resolves_the_exact_reviewed_task() {
        let finding = json!({"suggested_owner":"m-w1", "target_task_id":"step-b"});
        assert_eq!(
            select_reviewed_owner_binding(&reviewed(), &finding)
                .and_then(|artifact| artifact.get("artifact_id"))
                .and_then(Value::as_str),
            Some("artifact-b")
        );
        let finding = json!({"target_artifact_id":"artifact-a"});
        assert_eq!(
            select_reviewed_owner_binding(&reviewed(), &finding)
                .and_then(|artifact| artifact.get("task_id"))
                .and_then(Value::as_str),
            Some("step-a")
        );
        let inconsistent = json!({
            "target_task_id":"step-b",
            "target_artifact_id":"artifact-a"
        });
        assert!(select_reviewed_owner_binding(&reviewed(), &inconsistent).is_none());
    }
}

#[cfg(test)]
mod delivery_issue_resolution_tests {
    use super::resolve_review_issues;
    use crate::goal::{
        DeliveryIssueStatusV1, DeliveryIssueV1, Goal, GoalRunState, StepRecord,
    };
    use crate::plan::{Plan, StepStatus};
    use serde_json::json;

    fn state_with_issue() -> GoalRunState {
        let mut state = GoalRunState::new(
            Goal::new("issue-goal", "review issue closure"),
            Plan::new("issue-plan", "issue-goal"),
        );
        state.records.insert(
            "step-a".to_string(),
            StepRecord {
                step_id: "step-a".to_string(),
                status: StepStatus::Succeeded,
                attempts: 2,
                attempt_id: Some("attempt-new".to_string()),
                output: Some("fixed".to_string()),
                error: None,
                skip_reason: None,
                phase_epoch: Some(2),
                validation_receipts: Vec::new(),
            },
        );
        state.delivery_issues.push(DeliveryIssueV1 {
            issue_id: "issue-1".to_string(),
            source_review_artifact_id: "review-old".to_string(),
            source_review_sha256: "review-hash".to_string(),
            finding_sha256: "finding-hash".to_string(),
            severity: "major".to_string(),
            detail: "fix boundary".to_string(),
            requirement_id: Some("req-1".to_string()),
            target_task_id: "step-a".to_string(),
            target_attempt_id: "attempt-old".to_string(),
            target_artifact_id: Some("artifact-old".to_string()),
            owner_step_id: "step-a".to_string(),
            status: DeliveryIssueStatusV1::RepairDispatched,
            repair_attempt: 1,
            resolution_review_artifact_id: None,
            resolution_review_sha256: None,
            resolution_attempt_id: None,
            opened_at: "t1".to_string(),
            updated_at: "t1".to_string(),
        });
        state
    }

    #[test]
    fn approved_review_closes_only_repaired_current_task_attempts() {
        let mut state = state_with_issue();
        let reviewed = vec![json!({"task_id":"step-a","attempt_id":"attempt-new"})];

        assert_eq!(resolve_review_issues(&mut state, "review-new", "review-sha-new", &reviewed), 1);
        let issue = &state.delivery_issues[0];
        assert_eq!(issue.status, DeliveryIssueStatusV1::Resolved);
        assert_eq!(issue.resolution_review_artifact_id.as_deref(), Some("review-new"));
        assert_eq!(issue.resolution_review_sha256.as_deref(), Some("review-sha-new"));
        assert_eq!(issue.resolution_attempt_id.as_deref(), Some("attempt-new"));
    }

    #[test]
    fn approved_review_cannot_close_an_issue_with_a_stale_attempt() {
        let mut state = state_with_issue();
        let stale_review = vec![json!({"task_id":"step-a","attempt_id":"attempt-old"})];

        assert_eq!(resolve_review_issues(&mut state, "review-stale", "review-sha-stale", &stale_review), 0);
        assert_eq!(state.delivery_issues[0].status, DeliveryIssueStatusV1::RepairDispatched);
    }
}

#[cfg(test)]
mod parallel_assignment_validation_tests {
    use super::{
        bind_dynamic_follow_up_dependencies, parallel_tasks_require_integration,
        should_enable_parallel_assignment, validate_parallel_subtasks, RoleSpec,
    };
    use owo_agent_contracts::plan::StepSpec;

    fn roles() -> Vec<RoleSpec> {
        vec![
            RoleSpec::agent("lead"),
            RoleSpec::agent("w1"),
            RoleSpec::agent("w2"),
            RoleSpec::agent("leader"),
        ]
    }

    fn assigned_step(id: &str, task_id: &str, input: serde_json::Value) -> StepSpec {
        let mut step = StepSpec::new(id, "writer");
        step.input = input;
        step.input["assigned_task_id"] = serde_json::json!(task_id);
        step
    }

    #[test]
    fn host_manifest_skips_leader_only_for_independent_task_graphs() {
        let independent = vec![
            assigned_step(
                "step-a",
                "task-a",
                serde_json::json!({"assigned_write_paths":["src/a.rs"]}),
            ),
            assigned_step(
                "step-b",
                "task-b",
                serde_json::json!({"assigned_write_paths":["src/b.rs"]}),
            ),
        ];
        assert!(!parallel_tasks_require_integration(&independent));

        let dependent = vec![assigned_step("step-a", "task-a", serde_json::json!({})), {
            let mut step = assigned_step("step-b", "task-b", serde_json::json!({}));
            step.depends_on.push("step-a".to_string());
            step
        }];
        assert!(parallel_tasks_require_integration(&dependent));

        let overlapping_writes = vec![
            assigned_step(
                "step-a",
                "task-a",
                serde_json::json!({"assigned_write_paths":["src"]}),
            ),
            assigned_step(
                "step-b",
                "task-b",
                serde_json::json!({"assigned_write_paths":["src/lib.rs"]}),
            ),
        ];
        assert!(parallel_tasks_require_integration(&overlapping_writes));

        let shared_contract = vec![
            assigned_step(
                "step-a",
                "task-a",
                serde_json::json!({"assigned_contract_refs":["api-v1"]}),
            ),
            assigned_step(
                "step-b",
                "task-b",
                serde_json::json!({"assigned_contract_refs":["api-v1"]}),
            ),
        ];
        assert!(parallel_tasks_require_integration(&shared_contract));
        assert!(parallel_tasks_require_integration(&[]));
    }

    #[test]
    fn resolved_template_lead_enables_dynamic_assignment_without_request_flag() {
        assert!(should_enable_parallel_assignment(
            false,
            &[RoleSpec::agent("lead"), RoleSpec::agent("w1")]
        ));
        assert!(should_enable_parallel_assignment(
            true,
            &[RoleSpec::agent("implementer")]
        ));
        assert!(!should_enable_parallel_assignment(
            false,
            &[RoleSpec::agent("implementer")]
        ));
    }

    #[test]
    fn integration_waits_for_every_task_even_when_one_worker_has_multiple_tasks() {
        let contract_prep = StepSpec::new("s-contract-prep", "m-contract-prep");
        let coordinator = StepSpec::new("s-coordinator", "m-coordinator");
        let review_brief = StepSpec::new("s-review-brief", "m-review-brief");
        let mut integrator = StepSpec::new("s-project_integrator", "m-project_integrator");
        integrator.depends_on.push("s-contract-prep".to_string());
        integrator.depends_on.push("s-w3".to_string());
        let mut leader = StepSpec::new("s-leader", "m-leader");
        leader.depends_on.push("s-coordinator".to_string());
        let mut reviewer = StepSpec::new("s-reviewer", "m-reviewer");
        reviewer.depends_on.push("s-review-brief".to_string());
        let mut steps = vec![
            contract_prep,
            coordinator,
            review_brief,
            integrator,
            leader,
            reviewer,
        ];
        let task_ids = vec![
            "s-w1".to_string(),
            "s-task-c".to_string(),
            "s-w2".to_string(),
        ];
        let reviewer_ids = vec!["s-reviewer".to_string()];
        bind_dynamic_follow_up_dependencies(&mut steps, &task_ids, &reviewer_ids);

        assert_eq!(
            steps[3].depends_on,
            vec![
                "s-contract-prep".to_string(),
                "s-task-c".to_string(),
                "s-w1".to_string(),
                "s-w2".to_string(),
            ]
        );
        assert!(
            !steps[3]
                .depends_on
                .iter()
                .any(|dependency| dependency == "s-w3"),
            "removed, unassigned writer slots must not remain as dangling prerequisites"
        );
        assert_eq!(
            steps[4].depends_on,
            vec![
                "s-coordinator".to_string(),
                "s-task-c".to_string(),
                "s-w1".to_string(),
                "s-w2".to_string(),
            ]
        );
        assert_eq!(
            steps[5].depends_on,
            vec![
                "s-project_integrator".to_string(),
                "s-review-brief".to_string(),
                "s-task-c".to_string(),
                "s-w1".to_string(),
                "s-w2".to_string(),
            ]
        );
    }

    #[test]
    fn taskgraph_accepts_only_registered_workspace_plans_within_write_scope() {
        let make_task = |scope_path: &str, validator_id: &str| {
            serde_json::json!({
                "task_id":"verified-file",
                "worker":"w1",
                "task":"write the module",
                "acceptance":"the module contains the expected marker",
                "write_paths":["src"],
                "verification": {
                    "plan_id":"model-controlled-id-is-replaced",
                    "requirements":[{
                        "requirement_id":"model-controlled-id-is-replaced",
                        "validator_id":validator_id,
                        "validator_version":"1",
                        "scope":{"kind":"workspace_paths","relative_paths":[scope_path]},
                        "arguments":{"text":"pub fn ready"},
                        "required":true,
                        "resources":{"cpu_slots":1,"memory_mb":16,"exclusive_workspace":false,"timeout_ms":3000}
                    }]
                }
            })
        };
        let valid = validate_parallel_subtasks(
            &roles(),
            &[make_task("src/lib.rs", "workspace-file-contains-v1")],
            false,
        )
        .unwrap();
        let plan = valid.tasks[0].verification_plan.as_ref().unwrap();
        assert_eq!(plan.plan_id, "verify-verified-file");
        assert_eq!(
            plan.requirements[0].requirement_id,
            "verified-file:requirement:0"
        );
        assert!(validate_parallel_subtasks(
            &roles(),
            &[make_task("../outside.rs", "workspace-file-contains-v1")],
            false,
        )
        .is_err());
        assert!(validate_parallel_subtasks(
            &roles(),
            &[make_task("src/lib.rs", "shell-command-v1")],
            false,
        )
        .is_err());
    }

    #[test]
    fn declared_source_code_scope_requires_registered_behavior_command() {
        let mut task = serde_json::json!({
            "task_id":"source-file",
            "worker":"w1",
            "task":"implement the module",
            "acceptance":"module behavior passes its tests",
            "write_paths":["src/lib.rs"],
            "required_capabilities":["write_file"],
            "verification": {
                "plan_id":"source-plan",
                "requirements":[{
                    "requirement_id":"source-file-ready",
                    "validator_id":"workspace-file-non-empty-v1",
                    "validator_version":"1",
                    "scope":{"kind":"workspace_paths","relative_paths":["src/lib.rs"]},
                    "arguments":{},
                    "required":true,
                    "resources":{"cpu_slots":1,"memory_mb":8,"exclusive_workspace":false,"timeout_ms":5000}
                }]
            }
        });
        let error = validate_parallel_subtasks(&roles(), &[task.clone()], false).unwrap_err();
        assert!(error.contains("source-code write scope"), "{error}");

        task["required_capabilities"] = serde_json::json!(["write_file", "run_command"]);
        task["verification"]["requirements"].as_array_mut().unwrap().push(serde_json::json!({
            "requirement_id":"source-behavior",
            "validator_id":"workspace-command-success-v1",
            "validator_version":"1",
            "scope":{"kind":"workspace_paths","relative_paths":["src/lib.rs"]},
            "arguments":{"command":"cargo test -p owo-agent-core"},
            "required":true,
            "resources":{"cpu_slots":1,"memory_mb":8,"exclusive_workspace":false,"timeout_ms":30000}
        }));
        assert!(validate_parallel_subtasks(&roles(), &[task], false).is_ok());
    }

    #[test]
    fn supports_more_tasks_than_workers_and_validates_identity_and_acceptance() {
        let tasks = vec![
            serde_json::json!({"task_id":"a", "worker":"w1", "task":"one", "acceptance":"done", "write_paths":["src/a"]}),
            serde_json::json!({"task_id":"b", "worker":"w2", "task":"two", "acceptance":"done", "write_paths":["src/b"]}),
            serde_json::json!({"task_id":"c", "worker":"w1", "task":"three", "depends_on":["a"], "acceptance":"done", "write_paths":["src/c"]}),
        ];
        let validated = validate_parallel_subtasks(&roles(), &tasks, false).unwrap();
        assert_eq!(validated.tasks.len(), 3);
        let task_c = validated
            .tasks
            .iter()
            .find(|task| task.task_id == "c")
            .expect("dependent task c is retained");
        assert_eq!(task_c.worker, "w1");

        let duplicate_id = vec![
            serde_json::json!({"task_id":"a", "worker":"w1", "task":"one", "acceptance":"done", "write_paths":[]}),
            serde_json::json!({"task_id":"a", "worker":"w2", "task":"two", "acceptance":"done", "write_paths":[]}),
        ];
        assert!(validate_parallel_subtasks(&roles(), &duplicate_id, false)
            .unwrap_err()
            .contains("duplicate"));
        let empty_acceptance = vec![serde_json::json!({
            "worker":"w1", "task":"one", "acceptance":" ", "write_paths":[]
        })];
        assert!(
            validate_parallel_subtasks(&roles(), &empty_acceptance, false)
                .unwrap_err()
                .contains("acceptance")
        );
    }

    #[test]
    fn implicit_worker_assignment_balances_estimated_effort() {
        let tasks = vec![
            serde_json::json!({"task_id":"a", "task":"large", "acceptance":"done", "write_paths":[], "estimated_effort":5}),
            serde_json::json!({"task_id":"b", "task":"small", "acceptance":"done", "write_paths":[], "estimated_effort":1}),
            serde_json::json!({"task_id":"c", "task":"large", "acceptance":"done", "write_paths":[], "estimated_effort":4}),
            serde_json::json!({"task_id":"d", "task":"small", "acceptance":"done", "write_paths":[], "estimated_effort":2}),
        ];
        let validated = validate_parallel_subtasks(&roles(), &tasks, false).unwrap();
        let worker_for = |task_id: &str| {
            validated
                .tasks
                .iter()
                .find(|task| task.task_id == task_id)
                .unwrap()
                .worker
                .as_str()
        };
        assert_eq!(worker_for("a"), "w1");
        assert_eq!(worker_for("b"), "w1");
        assert_eq!(worker_for("c"), "w2");
        assert_eq!(worker_for("d"), "w2");
    }

    #[test]
    fn implicit_assignment_accounts_for_explicit_load_before_input_order() {
        let tasks = vec![
            serde_json::json!({"task_id":"implicit-large", "task":"large", "acceptance":"done", "write_paths":[], "estimated_effort":6}),
            serde_json::json!({"task_id":"explicit", "worker":"w1", "task":"reserved", "acceptance":"done", "write_paths":[], "estimated_effort":8}),
            serde_json::json!({"task_id":"implicit-small", "task":"small", "acceptance":"done", "write_paths":[], "estimated_effort":5}),
        ];
        let validated = validate_parallel_subtasks(&roles(), &tasks, false).unwrap();
        let worker_for = |task_id: &str| {
            validated
                .tasks
                .iter()
                .find(|task| task.task_id == task_id)
                .unwrap()
                .worker
                .as_str()
        };
        assert_eq!(worker_for("explicit"), "w1");
        assert_eq!(worker_for("implicit-large"), "w2");
        assert_eq!(worker_for("implicit-small"), "w2");
    }

    #[test]
    fn ready_task_order_prioritizes_estimated_critical_path() {
        let tasks = vec![
            serde_json::json!({"task_id":"a", "worker":"w1", "task":"path start", "acceptance":"done", "write_paths":[], "estimated_effort":2, "priority":10}),
            serde_json::json!({"task_id":"a2", "worker":"w1", "task":"path continuation", "depends_on":["a"], "acceptance":"done", "write_paths":[], "estimated_effort":10, "priority":10}),
            serde_json::json!({"task_id":"b", "worker":"w2", "task":"urgent short path", "acceptance":"done", "write_paths":[], "estimated_effort":9, "priority":100}),
        ];
        let validated = validate_parallel_subtasks(&roles(), &tasks, false).unwrap();
        assert_eq!(validated.tasks[0].task_id, "a");
        assert_eq!(validated.tasks[1].task_id, "a2");
        assert_eq!(validated.tasks[2].task_id, "b");
    }

    #[test]
    fn rejects_missing_dependencies_cycles_and_unordered_write_conflicts() {
        let missing_dep = vec![serde_json::json!({
            "task_id":"a", "worker":"w1", "task":"one", "depends_on":["ghost"], "acceptance":"done", "write_paths":[]
        })];
        assert!(validate_parallel_subtasks(&roles(), &missing_dep, false)
            .unwrap_err()
            .contains("dependency"));
        let cycle = vec![
            serde_json::json!({"task_id":"a", "worker":"w1", "task":"one", "depends_on":["b"], "acceptance":"done", "write_paths":[]}),
            serde_json::json!({"task_id":"b", "worker":"w2", "task":"two", "depends_on":["a"], "acceptance":"done", "write_paths":[]}),
        ];
        assert!(validate_parallel_subtasks(&roles(), &cycle, false)
            .unwrap_err()
            .contains("cycle"));
        let conflicting = vec![
            serde_json::json!({"task_id":"a", "worker":"w1", "task":"one", "acceptance":"done", "write_paths":["src"]}),
            serde_json::json!({"task_id":"b", "worker":"w2", "task":"two", "acceptance":"done", "write_paths":["src/b"]}),
        ];
        assert!(validate_parallel_subtasks(&roles(), &conflicting, false)
            .unwrap_err()
            .contains("overlap"));
    }

    #[test]
    fn preauthorized_task_paths_normalize_separators_without_prefix_escape() {
        let mut roles = roles();
        roles[1].write_paths = vec![r"src\user".to_string()];
        let valid = vec![serde_json::json!({
            "task_id":"nested", "worker":"w1", "task":"edit nested file",
            "acceptance":"done", "write_paths":["src/user/file.rs"]
        })];
        assert!(validate_parallel_subtasks(&roles, &valid, false).is_ok());

        let prefix_escape = vec![serde_json::json!({
            "task_id":"outside", "worker":"w1", "task":"edit sibling",
            "acceptance":"done", "write_paths":["src/user-old/file.rs"]
        })];
        assert!(validate_parallel_subtasks(&roles, &prefix_escape, false)
            .unwrap_err()
            .contains("pre-authorized"));

        let root_write = vec![serde_json::json!({
            "task_id":"root", "worker":"w1", "task":"edit repository",
            "acceptance":"done", "write_paths":["."]
        })];
        assert!(validate_parallel_subtasks(&roles, &root_write, false)
            .unwrap_err()
            .contains("workspace root"));
    }

    #[test]
    fn write_scope_comparison_normalizes_windows_paths_and_workspace_root() {
        assert!(super::write_paths_overlap(r"src\a", "src/a/b"));
        assert!(super::write_paths_overlap(".", "src/a"));
        assert!(super::write_path_is_within("src/a.rs", r"src\"));
        assert!(!super::write_path_is_within("src-old/a.rs", "src"));
    }

    #[test]
    fn high_risk_tasks_require_a_separate_reviewer_role() {
        let task = serde_json::json!({
            "task_id":"sensitive-change",
            "worker":"w1",
            "task":"change authentication boundary",
            "depends_on":[],
            "read_refs":[],
            "write_paths":["src/auth"],
            "contract_refs":[],
            "required_capabilities":["write_file"],
            "estimated_effort":4,
            "verification":"non_empty",
            "risk":"critical",
            "priority":90,
            "acceptance":"authentication behavior is verified"
        });
        let error =
            validate_parallel_subtasks(&roles(), std::slice::from_ref(&task), true).unwrap_err();
        assert!(error.contains("independent reviewer"));

        let mut roles_with_reviewer = roles();
        let mut reviewer = RoleSpec::agent("reviewer");
        reviewer.depends_on = vec!["lead".to_string()];
        roles_with_reviewer.push(reviewer);
        assert!(validate_parallel_subtasks(&roles_with_reviewer, &[task], true).is_ok());
    }

    #[test]
    fn versioned_graph_requires_stable_task_ids_and_metadata() {
        let task = serde_json::json!({
            "worker":"w1", "task":"implement", "acceptance":"done", "depends_on":[],
            "read_refs":[], "write_paths":[], "contract_refs":[], "required_capabilities":[],
            "estimated_effort":1, "verification":"non_empty", "risk":"normal", "priority":50
        });
        let error =
            validate_parallel_subtasks(&roles(), std::slice::from_ref(&task), true).unwrap_err();
        assert!(error.contains("task_id"));
        let mut task = task;
        task["task_id"] = serde_json::json!("readonly-task");
        let mut missing_dependencies = task.clone();
        missing_dependencies["write_paths"] = serde_json::json!(["src/a"]);
        missing_dependencies
            .as_object_mut()
            .unwrap()
            .remove("depends_on");
        let error =
            validate_parallel_subtasks(&roles(), &[missing_dependencies], true).unwrap_err();
        assert!(error.contains("depends_on"));

        for capability in ["write_file", "apply_patch"] {
            task["required_capabilities"] = serde_json::json!([capability]);
            let error = validate_parallel_subtasks(&roles(), &[task.clone()], true).unwrap_err();
            assert!(error.contains("no write_paths"));
        }

        task["write_paths"] = serde_json::json!(["src/a"]);
        task["verification"] = serde_json::json!("run:cargo test");
        let error = validate_parallel_subtasks(&roles(), &[task], true).unwrap_err();
        assert!(error.contains("verification"));
    }

    #[test]
    fn rejects_overlapping_writer_paths_including_parent_child() {
        let overlapping = vec![
            serde_json::json!({"worker": "w1", "task": "one", "acceptance": "done", "write_paths": ["src"]}),
            serde_json::json!({"worker": "w2", "task": "two", "acceptance": "done", "write_paths": ["src/b"]}),
        ];
        let error = validate_parallel_subtasks(&roles(), &overlapping, false).unwrap_err();
        assert!(error.contains("overlap"), "{error}");
    }
}
