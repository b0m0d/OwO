//! Review identity, durable issue resolution and owner repair planning.
use super::*;

use super::coord_rework::ReworkRequest;

use super::review_evidence::{combine_review_requests, plan_review_changes, resolve_review_issues};

impl TeamCoordinator {
    pub(super) async fn collect_review_repairs(
        &self,
        team_id: &str,
        space: &ProjectSpace,
        state: &mut GoalRunState,
        meta: &RunMeta,
    ) -> WorkSwarmResult<Vec<ReworkRequest>> {
        let steps = state
            .plan
            .steps
            .iter()
            .filter(|step| {
                state.records.get(&step.id).is_some_and(|record| {
                    record.status == StepStatus::Succeeded && record.skip_reason.is_none()
                }) && Self::role_spec_of_member(meta, &step.worker).is_ok_and(RoleSpec::is_reviewer)
            })
            .collect::<Vec<_>>();
        // Most worker phases have no completed reviewer: skip storage and CAS I/O.
        if steps.is_empty() {
            return Ok(Vec::new());
        }
        let catalog = self.artifact_catalog(team_id, space).await?;
        let mut text_cache = super::artifact_catalog::ArtifactTextCache::new(&self.cas);
        let mut issues = Vec::new();
        let mut repairs = Vec::new();
        let mut approvals = Vec::new();
        for step in steps {
            let artifact = catalog
                .current_for_step(state, step)?
                .filter(|artifact| artifact.kind == "review")
                .ok_or_else(|| {
                    WorkSwarmError::Validation(format!("评审 {} 缺少当前 attempt 产物", step.id))
                })?;
            let content = text_cache.read(artifact).await?;
            let Ok(document) = serde_json::from_str::<Value>(content.as_ref()) else {
                // Preserve legacy text reviews; they still cannot supply source acceptance.
                continue;
            };
            match document.pointer("/result/verdict").and_then(Value::as_str) {
                Some("approved") => {
                    if let Some(reviewed) =
                        document.get("reviewed_artifacts").and_then(Value::as_array)
                    {
                        approvals.push((
                            artifact.artifact_id.clone(),
                            artifact.sha256.clone(),
                            reviewed.clone(),
                        ));
                    }
                }
                Some("changes_requested") => {
                    let planned = plan_review_changes(team_id, state, artifact, &document)
                        .map_err(WorkSwarmError::Validation)?;
                    issues.extend(planned.issues);
                    repairs.extend(planned.repairs);
                }
                _ => {}
            }
        }
        let repairs = combine_review_requests(repairs).map_err(WorkSwarmError::Validation)?;
        // Do not overwrite a dispatched/resolved issue when a phase is replayed.
        for issue in issues {
            if !state
                .delivery_issues
                .iter()
                .any(|existing| existing.issue_id == issue.issue_id)
            {
                state.delivery_issues.push(issue);
            }
        }
        for (id, hash, reviewed) in approvals {
            resolve_review_issues(state, &id, &hash, &reviewed);
        }
        Ok(repairs)
    }
}
