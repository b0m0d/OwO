use super::*;

fn plan_has_context_task_id(plan: &crate::plan::Plan, task_id: &str) -> bool {
    plan.steps.iter().any(|step| {
        step.id == task_id
            || step
                .input
                .get("assigned_task_id")
                .and_then(serde_json::Value::as_str)
                == Some(task_id)
    })
}

impl TeamCoordinator {
    /// 读取团队的版本化共享事实元数据与不可变 CAS 正文。
    pub async fn read_team_context(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<owo_agent_protocol::SharedContextSnapshot> {
        self.store
            .get_team_run(team_id)
            .await
            .map_err(|e| WorkSwarmError::NotFound(e.to_string()))?;
        self.store
            .get_team_context(team_id)
            .await
            .map_err(|e| WorkSwarmError::Run(e.to_string()))
    }

    /// 以 expected_revision/CAS 发布一个候选事实。写入内容本身不会扩大工具权限。
    pub async fn publish_team_context_fact(
        &self,
        team_id: &str,
        expected_revision: u64,
        draft: SharedContextFactDraft,
    ) -> WorkSwarmResult<owo_agent_protocol::SharedContextFact> {
        let team = self
            .store
            .get_team_run(team_id)
            .await
            .map_err(|e| WorkSwarmError::NotFound(e.to_string()))?;
        if draft.key.trim().is_empty() || draft.key.chars().count() > 160 {
            return Err(WorkSwarmError::Validation(
                "共享事实 key 长度必须为 1..=160 字符".to_string(),
            ));
        }
        if draft.producer != "user"
            && !team
                .members
                .iter()
                .any(|member| member.member_id == draft.producer)
        {
            return Err(WorkSwarmError::Validation(
                "producer 必须是 user 或当前团队成员 ID".to_string(),
            ));
        }
        if draft.producer.trim().is_empty() || draft.producer.chars().count() > 120 {
            return Err(WorkSwarmError::Validation(
                "共享事实 producer 长度必须为 1..=120 字符".to_string(),
            ));
        }
        if expected_revision >= i64::MAX as u64 {
            return Err(WorkSwarmError::Validation(
                "共享上下文 revision 已达到 SQLite 整数上限".to_string(),
            ));
        }
        if draft.value.is_empty() || draft.value.len() > 64 * 1024 {
            return Err(WorkSwarmError::Validation(
                "共享事实正文必须为 1..=65536 字节".to_string(),
            ));
        }
        if draft.source_refs.len() > 8
            || draft
                .source_refs
                .iter()
                .any(|r| r.trim().is_empty() || r.len() > 512)
        {
            return Err(WorkSwarmError::Validation(
                "source_refs 最多 8 项且每项不超过 512 字节".to_string(),
            ));
        }
        if let Some(hash) = draft.file_hash.as_deref() {
            let valid = hash
                .strip_prefix("sha256:")
                .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
            if !valid {
                return Err(WorkSwarmError::Validation(
                    "file_hash 必须为 sha256:<64位十六进制>".to_string(),
                ));
            }
        }
        if let Some(task) = draft.task_id.as_deref() {
            let state = self.load_run_state(team_id)?;
            if !plan_has_context_task_id(&state.plan, task) {
                return Err(WorkSwarmError::Validation(format!(
                    "task_id 不属于当前团队：{task}"
                )));
            }
        }
        let hash = self
            .cas
            .put(draft.value.as_bytes())
            .map_err(WorkSwarmError::Run)?;
        let fact = owo_agent_protocol::SharedContextFact {
            key: draft.key.trim().to_string(),
            value_ref: format!("cas://sha256:{hash}"),
            revision: expected_revision + 1,
            producer: draft.producer.trim().to_string(),
            task_id: draft.task_id,
            source_refs: draft.source_refs,
            file_hash: draft.file_hash,
            confidence: "unverified".to_string(),
            status: "candidate".to_string(),
            created_at: now_ts(),
        };
        let committed = self
            .store
            .compare_and_swap_team_context(team_id, expected_revision, &fact)
            .await
            .map_err(|e| WorkSwarmError::Run(e.to_string()))?;
        if !committed {
            return Err(WorkSwarmError::Conflict(
                "团队共享上下文 revision 已变化，请先重新读取".to_string(),
            ));
        }
        self.audit(
            team_id,
            "team.context.fact_published",
            format!(
                "key={} revision={} producer={} task_id={}",
                fact.key,
                fact.revision,
                fact.producer,
                fact.task_id.as_deref().unwrap_or("none")
            ),
        );
        Ok(fact)
    }
    /// Mark a previously published fact stale with a CAS append, so older versions
    /// under the same key are no longer selected by readers or context slices.
    pub async fn mark_team_context_fact_stale(
        &self,
        team_id: &str,
        key: &str,
        fact_revision: u64,
        expected_revision: u64,
    ) -> WorkSwarmResult<owo_agent_protocol::SharedContextFact> {
        let snapshot = self.read_team_context(team_id).await?;
        if snapshot.revision != expected_revision {
            return Err(WorkSwarmError::Conflict(
                "团队共享上下文 revision 已变化，无法标记 stale".to_string(),
            ));
        }
        let latest = snapshot
            .facts
            .iter()
            .rev()
            .find(|fact| fact.key == key)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("共享事实不存在：{key}")))?;
        if latest.revision != fact_revision
            || (latest.status != "candidate" && latest.status != "confirmed")
        {
            return Err(WorkSwarmError::Conflict(
                "共享事实已更新或已失效，拒绝覆盖当前版本".to_string(),
            ));
        }
        if expected_revision >= i64::MAX as u64 {
            return Err(WorkSwarmError::Validation(
                "共享上下文 revision 已达到 SQLite 整数上限".to_string(),
            ));
        }
        let mut stale = latest.clone();
        stale.revision = expected_revision + 1;
        stale.status = "stale".to_string();
        stale.confidence = "stale".to_string();
        stale.created_at = now_ts();
        let committed = self
            .store
            .compare_and_swap_team_context(team_id, expected_revision, &stale)
            .await
            .map_err(|e| WorkSwarmError::Run(e.to_string()))?;
        if !committed {
            return Err(WorkSwarmError::Conflict(
                "团队共享上下文 revision 已变化，无法标记 stale".to_string(),
            ));
        }
        self.audit(
            team_id,
            "team.context.fact_stale",
            format!(
                "key={} source_revision={} revision={}",
                stale.key, fact_revision, stale.revision
            ),
        );
        Ok(stale)
    }
}

#[cfg(test)]
mod tests {
    use super::plan_has_context_task_id;
    use crate::plan::{Plan, StepSpec};
    use serde_json::json;

    #[test]
    fn context_accepts_step_and_assigned_task_ids_only() {
        let mut plan = Plan::new("plan", "goal");
        let mut step = StepSpec::new("s-worker", "m-worker");
        step.input = json!({"assigned_task_id":"task-api"});
        plan.add_step(step);

        assert!(plan_has_context_task_id(&plan, "s-worker"));
        assert!(plan_has_context_task_id(&plan, "task-api"));
        assert!(!plan_has_context_task_id(&plan, "task-from-another-team"));
    }
}
