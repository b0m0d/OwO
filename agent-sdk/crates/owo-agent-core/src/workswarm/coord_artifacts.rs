use super::*;
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
        self.register_step_output_checked(team_id, member_id, role, step_id, output, None)
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
        self.register_step_output_inner(team_id, member_id, role, step_id, &out, phase_epoch)
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
        if is_critic_role(role) {
            // critic：评审结论（kind=review/markdown），证据与未决问题随落盘。
            let out = StepOutput {
                content: output.summary.clone(),
                kind: "review".to_string(),
                format: "markdown".to_string(),
                media_type: "text/markdown".to_string(),
                file_name: file_name_of("review", "markdown"),
                evidence_refs: evidence_refs_of(&output.evidence),
                open_issues: Some(output.open_issues.clone()),
                known_risks: Some(Vec::new()),
                validation: None,
                handoff_note: Some(output.summary.clone()),
            };
            return self
                .register_step_output_inner(team_id, member_id, role, step_id, &out, phase_epoch)
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
        let validation = validate_artifact_content(&eff, &declared.content, &output.evidence);
        if !validation.valid {
            // 门控（登记前）：未通过格式校验的产物不进任何登记流程。
            self.audit(
                team_id,
                "team.artifact.validation_rejected",
                format!(
                    "产物格式校验未通过（{eff}，{}）：member={member_id} step={step_id}，不登记",
                    validation.reason.as_deref().unwrap_or("")
                ),
            );
            return Err(WorkSwarmError::Run(format!(
                "artifact_invalid: {}",
                validation.reason.as_deref().unwrap_or("未知原因")
            )));
        }
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
        self.register_step_output_inner(team_id, member_id, role, step_id, &out, phase_epoch)
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
        phase_epoch: Option<u64>,
    ) -> WorkSwarmResult<Artifact> {
        if let Some(epoch) = phase_epoch {
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
        let (_team, space, state) = self.load_bundle(team_id).await?;
        let project_id = space.project_id.clone();
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let correlation = meta.correlation_id.clone();

        let version = self.next_artifact_version(&space, role).await?;
        let hash = self
            .cas
            .put(out.content.as_bytes())
            .map_err(|e| WorkSwarmError::Run(format!("产物 CAS 落盘失败：{e}")))?;
        let content_ref = format!("cas://sha256:{hash}");
        let kind = out.kind.clone();

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
            let Some(dep_role) = worker_role(&dep_step.worker) else {
                continue;
            };
            if let Some(a) = self.latest_artifact_for_role(&space, &dep_role).await {
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
            if let Some(prev) = self.latest_artifact_for_role(&space, role).await {
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
        let artifact = Artifact {
            artifact_id: format!("{team_id}:{role}:v{version}"),
            kind,
            version,
            producer: member_id.to_string(),
            content_ref: content_ref.clone(),
            schema_ref: None,
            source_refs,
            classification: ArtifactClassification::Private,
            review_state: if is_critic_role(role) {
                ReviewState::PendingReview
            } else {
                ReviewState::Draft
            },
            supersedes_artifact_id,
            created_at: now_ts(),
            team_id: team_id.to_string(),
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
        let kind = role_kind(role);
        let mut max = 0u32;
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                if a.kind == kind && a.version > max {
                    max = a.version;
                }
            }
        }
        Ok(max + 1)
    }

    pub(crate) async fn latest_artifact_for_role(
        &self,
        space: &ProjectSpace,
        role: &str,
    ) -> Option<Artifact> {
        let kind = role_kind(role);
        let mut best: Option<Artifact> = None;
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                if a.kind == kind && best.as_ref().is_none_or(|b| a.version > b.version) {
                    best = Some(a);
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
        let (_team, space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let spec = Self::role_spec_of_member(&meta, member_id)?;
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
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
            if let Some(a) = self.latest_artifact_for_role(&space, &dep_role).await {
                let content = self.cas_content_text(&a.content_ref);
                upstream.push(json!({
                    "role": dep_role,
                    "artifact_id": a.artifact_id,
                    "version": a.version,
                    "content": content,
                    // 八期一路：CAS ref 随切片透出（大 Artifact 摘要块需带哈希与 ref）。
                    "cas_ref": a.content_ref,
                    "review_state": format!("{:?}", a.review_state),
                }));
            }
        }
        Ok(json!({
            "team_id": team_id,
            "objective_text": state.goal.objective,
            "role": spec.role,
            "member_id": member_id,
            "handoff_contract": spec.handoff_contract,
            // 八期一路：模板 id + 角色调用预算（角色专属 Prompt 编译输入）。
            "template_id": meta.template_id,
            "budget_calls": meta.budgets.get(&spec.role).copied().unwrap_or(0),
            "upstream": upstream,
        }))
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

    /// 八期一路：服务端 Git 变更跟踪记录是否存在实际工作区变更
    /// （读 `<run_dir>/<team_id>-workspace-changes.json`；文件缺失/无记录/解析
    /// 失败一律视为无变更——运行期 reviewer 跳过判定的输入）。
    pub(crate) fn workspace_has_changes(&self, team_id: &str) -> bool {
        let path = self
            .run_dir
            .join(format!("{team_id}-workspace-changes.json"));
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return false;
        };
        serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| {
                v.as_array().map(|arr| {
                    arr.iter().any(|r| {
                        r.get("changed_files")
                            .and_then(Value::as_array)
                            .is_some_and(|files| !files.is_empty())
                    })
                })
            })
            .unwrap_or(false)
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
                let (prompt_text, prompt_meta) = Self::compile_role_prompt_with_meta(ctx);
                obj.insert("prompt".to_string(), json!(prompt_text));
                if let Some(ws) = obj.get_mut("_workswarm").and_then(Value::as_object_mut) {
                    ws.insert("prompt_meta".to_string(), prompt_meta);
                }
            }
            let role = ctx.get("role").and_then(Value::as_str).unwrap_or("");
            obj.insert("read_only".to_string(), json!(is_critic_role(role)));
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
        let pctx = crate::team_prompt::PromptContext {
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
            is_critic: is_critic_role(role),
            upstream: &compiled,
        };
        let prompt = crate::team_prompt::compile_prompt(&pctx);
        let meta = json!({
            "context_bytes": compiled.context_bytes,
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
