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
    inner: Arc<dyn Worker>,
}

impl RoleWorker {
    pub fn new(
        coordinator: Arc<TeamCoordinator>,
        team_id: String,
        member_id: String,
        role: String,
        inner: Arc<dyn Worker>,
    ) -> Self {
        Self {
            coordinator,
            team_id,
            member_id,
            role,
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
        let ctx = match self
            .coordinator
            .assemble_context_slice(&self.team_id, &self.member_id, &step_id)
            .await
        {
            Ok(c) => c,
            Err(e) => return Err(format!("上下文切片组装失败：{e}")),
        };
        let worker_kind = self.inner.name().to_string();
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
        let out = self.inner.run(&enriched).await?;
        // 输出契约（V1）：结构化 JSON → 登记前防御——producer 取 artifact.content
        //（交付物正文，不再拿整段自由文本/信封当产物），critic 禁止携带 artifact
        //（评审无权覆盖交付物，越权即 scope_violation）。契约失败已在 worker 层
        // 定向修复过一次；此处 Invalid 视为 legacy 纯文本登记（不二次重试）。
        // 七期（第三路）：Parsed 输出走契约登记（格式门控 + 交付元数据 + 证据链
        // 随 Artifact/HandoffRecord 落盘）；legacy 纯文本登记行为不变。
        let out = match crate::workswarm_output::parse_worker_output(&out) {
            crate::workswarm_output::WorkerOutputParse::Parsed(output) => {
                if is_critic_role(&self.role) {
                    if let Err(e) = output.validate_critic() {
                        return Err(format!("scope_violation:{e}"));
                    }
                    if let Err(e) = self
                        .coordinator
                        .register_step_output_contract(
                            &self.team_id,
                            &self.member_id,
                            &self.role,
                            &step_id,
                            &output,
                            phase_epoch,
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
                                .register_step_output_contract(
                                    &self.team_id,
                                    &self.member_id,
                                    &self.role,
                                    &step_id,
                                    &output,
                                    phase_epoch,
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
                    .register_step_output_checked(
                        &self.team_id,
                        &self.member_id,
                        &self.role,
                        &step_id,
                        &out,
                        phase_epoch,
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
