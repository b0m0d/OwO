use super::*;

pub(super) fn reject_attempt_admissions(
    requests: Vec<crate::goal::AttemptAdmissionRequest>,
    reason: &str,
) {
    for request in requests {
        request.respond(Err(reason.to_string()));
    }
}

impl TeamCoordinator {
    /// Durably admit a bounded batch of step/critic calls before their Worker futures proceed.
    pub(super) async fn persist_attempt_admissions(
        &self,
        team_id: &str,
        epoch: u64,
        cancel: &CancelToken,
        requests: Vec<crate::goal::AttemptAdmissionRequest>,
    ) -> WorkSwarmResult<()> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        if self.phase_epoch(team_id) != epoch {
            for request in requests {
                request.respond(Err("执行阶段已过期，拒绝准入".to_string()));
            }
            return Ok(());
        }
        let (team, _space, mut state) = self.load_bundle(team_id).await?;
        if team.status.is_terminal() {
            for request in requests {
                request.respond(Err("团队运行已终止，拒绝执行准入".to_string()));
            }
            return Ok(());
        }

        let mut accepted = Vec::new();
        for request in requests {
            match super::coord_progress::apply_attempt_admission(
                &mut state,
                epoch,
                &request.step_id,
                &request.attempt_id,
                request.is_retry,
            ) {
                Ok(()) => accepted.push(request),
                Err(reason) => request.respond(Err(reason)),
            }
        }
        if accepted.is_empty() {
            return Ok(());
        }
        if let Err(error) = self.persist_state(&state) {
            let reason = error.to_string();
            for request in accepted {
                request.respond(Err(reason.clone()));
            }
            return Err(error);
        }
        self.advance_progress(team_id);
        // Acknowledge while holding the Team lock so the admission is linearized
        // before a concurrent rework can replace its attempt identity. Cancellation
        // is checked again after the durable write to avoid starting late work.
        if cancel.is_cancelled() {
            reject_attempt_admissions(accepted, "团队已取消，Worker 调用准入被撤销");
        } else {
            for request in accepted {
                request.respond(Ok(()));
            }
        }
        drop(_guard);
        Ok(())
    }
}
