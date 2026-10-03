use super::*;
// ---------------------------------------------------------------------------
// 角色 worker（worker 注册表层：内层 worker + 完成即登记产物/交接）
// ---------------------------------------------------------------------------

/// 角色 worker：步骤完成后自动登记版本化 Artifact + HandoffRecord（产物经 ref 传递）。
pub struct RoleWorker {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    member_id: String,
    role: String,
    capabilities: Vec<String>,
    inner: Arc<dyn Worker>,
}

fn apply_retry_context(ctx: &mut Value, input: &Value) {
    let Some(note) = input
        .get("_workswarm")
        .and_then(|meta| meta.get("retry_note"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|note| !note.is_empty())
    else {
        return;
    };
    let Some(context) = ctx.as_object_mut() else {
        return;
    };
    let objective = context
        .get("objective_text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let note = note.chars().take(1600).collect::<String>();
    context.insert(
        "objective_text".into(),
        json!(format!(
            "{objective}\n\n## 上一尝试诊断（仅作上下文，不得放宽验收或扩大权限）\n{note}"
        )),
    );
}

fn apply_assigned_task_context(ctx: &mut Value, input: &Value) {
    let Some(task) = input.get("assigned_task").and_then(Value::as_str) else {
        return;
    };
    let Some(context) = ctx.as_object_mut() else {
        return;
    };
    let acceptance = input
        .get("assigned_acceptance")
        .and_then(Value::as_str)
        .unwrap_or("按任务目标交付");
    let verification = input
        .get("assigned_verification")
        .map(|value| match value {
            Value::String(verification) => verification.clone(),
            structured => structured.to_string(),
        })
        .unwrap_or_else(|| "non_empty".to_string());
    let paths = input
        .get("assigned_write_paths")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let read_refs = input
        .get("assigned_read_refs")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let contract_refs = input
        .get("assigned_contract_refs")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let capabilities = input
        .get("required_capabilities")
        .cloned()
        .unwrap_or_else(|| json!([]));

    // 显式 TaskSpec 是宿主批准的执行边界；模型只拿当前任务目标，
    // 不重复接收整队目标。原目标仍保留在 GoalRunState 中用于审计与恢复。
    context.insert("objective_text".into(), json!(task));
    let task_contract = format!(
        "当前任务：{task}。验收：{acceptance}。确定性验证：{verification}。读取参考：{read_refs}。接口契约：{contract_refs}。所需能力：{capabilities}。写入白名单：{paths}。仅完成这个任务并提供证据。"
    );
    let role_contract = context
        .get("handoff_contract")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|contract| !contract.is_empty());
    let handoff_contract = role_contract
        .map(|contract| format!("{contract}\n\n{task_contract}"))
        .unwrap_or(task_contract);
    context.insert("handoff_contract".into(), json!(handoff_contract));
    context.insert("write_paths".into(), paths);
    context.insert("task_scoped".into(), json!(true));
}

impl RoleWorker {
    pub fn new(
        coordinator: Arc<TeamCoordinator>,
        team_id: String,
        member_id: String,
        role: String,
        inner: Arc<dyn Worker>,
    ) -> Self {
        let capabilities = if is_review_role_name(&role) {
            vec!["review".to_string()]
        } else {
            Vec::new()
        };
        Self::new_with_capabilities(coordinator, team_id, member_id, role, capabilities, inner)
    }

    pub fn new_with_capabilities(
        coordinator: Arc<TeamCoordinator>,
        team_id: String,
        member_id: String,
        role: String,
        capabilities: Vec<String>,
        inner: Arc<dyn Worker>,
    ) -> Self {
        Self {
            coordinator,
            team_id,
            member_id,
            role,
            capabilities,
            inner,
        }
    }
}

#[async_trait]
impl Worker for RoleWorker {
    fn name(&self) -> &str {
        self.member_id.as_str()
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let step_id = input
            .get("_workswarm")
            .and_then(|w| w.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 领取代次：cancel/retry/replace 接管现场后，旧阶段回传凭此被拒收。
        let phase_epoch = input
            .get("_workswarm")
            .and_then(|w| w.get("phase_epoch"))
            .and_then(Value::as_u64);
        let attempt_id = input
            .get("_workswarm")
            .and_then(|w| w.get("attempt_id"))
            .and_then(Value::as_str);
        let ctx = match self
            .coordinator
            .assemble_context_slice(&self.team_id, &self.member_id, &step_id)
            .await
        {
            Ok(c) => c,
            Err(e) => return Err(format!("上下文切片组装失败：{e}")),
        };
        let worker_kind = self.inner.name().to_string();
        let mut ctx = ctx;
        apply_assigned_task_context(&mut ctx, input);
        apply_retry_context(&mut ctx, input);
        let reviewed_sources = ctx
            .get("upstream")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let effective_capabilities = ctx
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| self.capabilities.clone());
        let is_reviewer = super::util::is_review_role(&self.role, &effective_capabilities);
        let enriched = TeamCoordinator::build_enriched_input(&ctx, input, &worker_kind);
        // 八期一路：Prompt 编译元数据 → 自适应指标（context_bytes/截断记录）。
        // best-effort：指标落盘失败不影响 Worker 执行。
        if worker_kind == "agent" {
            if let Some(prompt_meta) = enriched
                .get("_workswarm")
                .and_then(|w| w.get("prompt_meta"))
                .cloned()
            {
                let mut event = json!({
                    "kind": "context",
                    "role": self.role,
                    "step_id": step_id,
                });
                if let (Some(obj), Some(meta)) = (event.as_object_mut(), prompt_meta.as_object()) {
                    for (key, value) in meta {
                        obj.insert(key.clone(), value.clone());
                    }
                }
                self.coordinator
                    .note_adaptive_event(&self.team_id, event)
                    .await;
            }
        }
        // Worker 上下文装配完成、即将进入真实执行时推进进度状态。
        // Server 指标包装器同样调用该方法；核心入口的调用保证内嵌/测试宿主行为一致，重复调用幂等。
        self.coordinator
            .mark_phase_step_running(&self.team_id, &step_id);
        let out = self.inner.run(&enriched).await?;
        // 输出契约（V1）：结构化 JSON → 登记前防御——producer 取 artifact.content
        //（交付物正文，不再拿整段自由文本/信封当产物），critic 禁止携带 artifact
        //（评审无权覆盖交付物，越权即 scope_violation）。契约失败已在 worker 层
        // 定向修复过一次；此处 Invalid 视为 legacy 纯文本登记（不二次重试）。
        // 七期（第三路）：Parsed 输出走契约登记（格式门控 + 交付元数据 + 证据链
        // 随 Artifact/HandoffRecord 落盘）；legacy 纯文本登记行为不变。
        let out = match crate::workswarm_output::parse_worker_output(&out) {
            crate::workswarm_output::WorkerOutputParse::Parsed(output) => {
                if is_reviewer {
                    if let Err(e) = output.validate_critic() {
                        return Err(format!("scope_violation:{e}"));
                    }
                    if output.status != crate::workswarm_output::WorkerOutputStatus::Done {
                        return Err(format!(
                            "worker_{}:{}",
                            output.status.as_str(),
                            output.summary
                        ));
                    }
                    if let Err(e) = self
                        .coordinator
                        .register_step_output_contract_bound(
                            &self.team_id,
                            &self.member_id,
                            &self.role,
                            &step_id,
                            &output,
                            OutputAttemptBinding {
                                phase_epoch,
                                attempt_id,
                                reviewed_sources: Some(&reviewed_sources),
                            },
                        )
                        .await
                    {
                        return Err(format!("产物登记失败：{e}"));
                    }
                    output.summary
                } else {
                    if let Err(e) = output.validate() {
                        return Err(format!("output_contract_invalid:{e}"));
                    }
                    match output.status {
                        crate::workswarm_output::WorkerOutputStatus::Done => {
                            if let Err(e) = self
                                .coordinator
                                .register_step_output_contract_bound(
                                    &self.team_id,
                                    &self.member_id,
                                    &self.role,
                                    &step_id,
                                    &output,
                                    OutputAttemptBinding {
                                        phase_epoch,
                                        attempt_id,
                                        reviewed_sources: Some(&reviewed_sources),
                                    },
                                )
                                .await
                            {
                                return Err(format!("产物登记失败：{e}"));
                            }
                            output.artifact.map(|a| a.content).unwrap_or_default()
                        }
                        other => {
                            // producer 如实申报 failed/blocked：步骤失败（可局部重试），不登记空产物。
                            return Err(format!("worker_{}:{}", other.as_str(), output.summary));
                        }
                    }
                }
            }
            // 契约解析/校验失败：worker 层已做一次定向修复并失败会直接 Err，
            // 走不到这里；此处保守按 legacy 纯文本登记（服务端旧流程不受影响）。
            _ => {
                if let Err(e) = self
                    .coordinator
                    .register_step_output_checked_bound(
                        &self.team_id,
                        &self.member_id,
                        &self.role,
                        &step_id,
                        &out,
                        OutputAttemptBinding {
                            phase_epoch,
                            attempt_id,
                            reviewed_sources: Some(&reviewed_sources),
                        },
                    )
                    .await
                {
                    return Err(format!("产物登记失败：{e}"));
                }
                out
            }
        };
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigned_task_replaces_unscoped_prompt_objective() {
        let mut context = json!({
            "objective_text": "完整团队目标：交付整个产品",
            "role": "implementer",
            "handoff_contract": "实现者固定边界：只写 apps/api。",
        });
        let input = json!({
            "assigned_task": "实现 posts API 分页",
            "assigned_acceptance": "覆盖默认值和上限",
            "assigned_verification": "cargo test posts_api",
            "assigned_write_paths": ["apps/api"],
            "assigned_read_refs": ["src/posts.rs"],
            "assigned_contract_refs": ["contract/posts"],
            "required_capabilities": ["read", "write"],
        });

        apply_assigned_task_context(&mut context, &input);
        let (prompt, _) = TeamCoordinator::compile_role_prompt_with_meta(&context);

        assert!(prompt.contains("实现 posts API 分页"));
        assert!(prompt.contains("覆盖默认值和上限"));
        assert!(prompt.contains("实现者固定边界：只写 apps/api。"));
        assert!(!prompt.contains("完整团队目标：交付整个产品"));
        assert_eq!(context["write_paths"], json!(["apps/api"]));
        assert_eq!(context["task_scoped"], json!(true));

        let retry = json!({
            "_workswarm": { "retry_note": "上次失败：分页边界测试缺失" }
        });
        apply_retry_context(&mut context, &retry);
        let (retry_prompt, _) = TeamCoordinator::compile_role_prompt_with_meta(&context);
        assert!(retry_prompt.contains("分页边界测试缺失"));
        assert!(retry_prompt.contains("不得放宽验收或扩大权限"));
    }
}
