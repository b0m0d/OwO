//! Operation-local artifact metadata snapshot. Never cached across phases or attempts.
use super::*;
use std::collections::BTreeSet;

pub(super) struct ArtifactCatalog {
    team_id: String,
    by_id: HashMap<String, Artifact>,
    by_task: HashMap<String, Vec<String>>,
}

impl ArtifactCatalog {
    pub(super) fn new(
        team_id: &str,
        space: &ProjectSpace,
        artifacts: Vec<Artifact>,
    ) -> WorkSwarmResult<Self> {
        let published: HashSet<&str> = space.artifacts.iter().map(String::as_str).collect();
        let mut catalog = Self {
            team_id: team_id.into(),
            by_id: HashMap::new(),
            by_task: HashMap::new(),
        };
        for artifact in artifacts {
            if !published.contains(artifact.artifact_id.as_str()) {
                continue;
            }
            if !artifact.team_id.is_empty() && artifact.team_id != team_id {
                return Err(WorkSwarmError::Conflict(format!(
                    "项目产物 {} 不属于当前团队",
                    artifact.artifact_id
                )));
            }
            let id = artifact.artifact_id.clone();
            if catalog.by_id.contains_key(&id) {
                return Err(WorkSwarmError::Conflict(format!("项目产物身份重复：{id}")));
            }
            if let Some(task) = artifact.task_id.as_ref() {
                catalog
                    .by_task
                    .entry(task.clone())
                    .or_default()
                    .push(id.clone());
            }
            catalog.by_id.insert(id, artifact);
        }
        Ok(catalog)
    }

    pub(super) fn get(&self, id: &str) -> WorkSwarmResult<&Artifact> {
        self.by_id
            .get(id)
            .ok_or_else(|| WorkSwarmError::Conflict(format!("当前项目未发布或缺失产物：{id}")))
    }

    fn select<'a>(
        &'a self,
        step: &StepSpec,
        attempt: Option<&str>,
    ) -> WorkSwarmResult<Option<&'a Artifact>> {
        let mut best: Option<&Artifact> = None;
        let mut ambiguous = false;
        for id in self.by_task.get(&step.id).into_iter().flatten() {
            let artifact = &self.by_id[id];
            if artifact.producer != step.worker
                || artifact.team_id != self.team_id
                || attempt.is_some_and(|attempt| artifact.attempt_id.as_deref() != Some(attempt))
            {
                continue;
            }
            match best {
                Some(current) if current.version == artifact.version => {
                    ambiguous = true;
                }
                Some(current) if current.version > artifact.version => {}
                _ => {
                    best = Some(artifact);
                    ambiguous = false;
                }
            }
        }
        if ambiguous {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {} 当前版本存在多个产物，不能靠时间戳选择",
                step.id
            )));
        }
        Ok(best)
    }

    pub(super) fn current_for_step(
        &self,
        state: &GoalRunState,
        step: &StepSpec,
    ) -> WorkSwarmResult<Option<&Artifact>> {
        let attempt = state
            .records
            .get(&step.id)
            .and_then(|record| record.attempt_id.as_deref());
        self.current_for_attempt(step, attempt)
    }

    pub(super) fn current_for_attempt(
        &self,
        step: &StepSpec,
        attempt: Option<&str>,
    ) -> WorkSwarmResult<Option<&Artifact>> {
        let Some(attempt) = attempt else {
            return Ok(None);
        };
        self.select(step, Some(attempt))
    }

    pub(super) fn current(
        &self,
        state: &GoalRunState,
        step_id: &str,
    ) -> WorkSwarmResult<Option<&Artifact>> {
        let Some(step) = state.plan.steps.iter().find(|step| step.id == step_id) else {
            return Ok(None);
        };
        self.current_for_step(state, step)
    }

    pub(super) fn previous_for_step(&self, step: &StepSpec) -> WorkSwarmResult<Option<&Artifact>> {
        self.select(step, None)
    }

    pub(super) fn next_version(&self, producer: &str) -> WorkSwarmResult<u32> {
        self.by_id
            .values()
            .filter(|artifact| artifact.producer == producer)
            .map(|artifact| artifact.version)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| WorkSwarmError::Validation("产物版本已达到上限".into()))
    }

    /// Only a handoff containing the exact current artifact set may govern delivery.
    /// Storage order may choose among equivalent submissions only after exact binding;
    /// a timestamp can never admit an obsolete attempt or older artifact version.
    pub(super) fn current_handoff<'a>(
        &self,
        state: &GoalRunState,
        step: &StepSpec,
        handoffs: &'a [HandoffRecord],
    ) -> WorkSwarmResult<Option<&'a HandoffRecord>> {
        let Some(current) = self.current_for_step(state, step)? else {
            return Ok(None);
        };
        let prefix = format!("{}:{}:", self.team_id, step.id);
        for handoff in handoffs.iter().rev().filter(|handoff| {
            handoff.from_member == step.worker && handoff.handoff_id.starts_with(&prefix)
        }) {
            if !handoff
                .output_artifact_refs
                .iter()
                .any(|artifact_id| artifact_id == &current.artifact_id)
            {
                continue;
            }
            let mut refs = BTreeSet::<&str>::new();
            let mut all_current = true;
            for id in &handoff.output_artifact_refs {
                let artifact = self.get(id)?;
                if artifact.task_id.as_deref() != Some(step.id.as_str())
                    || artifact.producer != step.worker
                    || artifact.attempt_id != current.attempt_id
                    || artifact.version != current.version
                    || !refs.insert(id.as_str())
                {
                    all_current = false;
                }
            }
            if all_current && refs.contains(current.artifact_id.as_str()) {
                return Ok(Some(handoff));
            }
        }
        Ok(None)
    }
}

impl TeamCoordinator {
    pub(super) async fn artifact_catalog(
        &self,
        team_id: &str,
        space: &ProjectSpace,
    ) -> WorkSwarmResult<ArtifactCatalog> {
        ArtifactCatalog::new(
            team_id,
            space,
            self.store
                .list_artifacts_by_project(&space.project_id)
                .await?,
        )
    }
}

/// A single delivery/review artifact is never materialized above the kernel's hard text bound.
pub(super) const MAX_DELIVERY_TEXT_BYTES: usize = crate::cas_store::MAX_FULL_TEXT_BYTES;

struct CasReadCancelGuard(Arc<AtomicBool>);
impl Drop for CasReadCancelGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Request-local verified CAS text reuse. Total retained bytes are separately bounded;
/// each individual object also obeys MAX_DELIVERY_TEXT_BYTES.
pub(super) struct ArtifactTextCache {
    cas: CasStore,
    cached: HashMap<String, Arc<str>>,
    retained_bytes: usize,
}
pub(super) fn artifact_hash(artifact: &Artifact) -> WorkSwarmResult<&str> {
    let hash = artifact
        .content_ref
        .strip_prefix("cas://sha256:")
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| WorkSwarmError::Validation("产物缺少合法 CAS 哈希引用".into()))?;
    if hash != artifact.sha256 {
        return Err(WorkSwarmError::Conflict(
            "产物 SHA256 与 CAS 引用不一致".into(),
        ));
    }
    Ok(hash)
}

impl ArtifactTextCache {
    pub(super) fn new(cas: &CasStore) -> Self {
        Self {
            cas: cas.clone(),
            cached: HashMap::new(),
            retained_bytes: 0,
        }
    }
    pub(super) async fn read(&mut self, artifact: &Artifact) -> WorkSwarmResult<Arc<str>> {
        let hash = artifact_hash(artifact)?.to_string();
        if let Some(text) = self.cached.get(&hash) {
            if text.len() as u64 != artifact.size_bytes {
                return Err(WorkSwarmError::Conflict(
                    "产物大小与已验证 CAS 字节不一致".into(),
                ));
            }
            return Ok(Arc::clone(text));
        }
        let expected_size = artifact.size_bytes;
        let cache_key = hash.clone();
        if expected_size > MAX_DELIVERY_TEXT_BYTES as u64 {
            return Err(WorkSwarmError::Validation(format!(
                "交付/评审产物超过单体文本上限 {MAX_DELIVERY_TEXT_BYTES} bytes；请拆分产物，完整验收不会截断"
            )));
        }
        let cas = self.cas.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = Arc::clone(&cancel);
        let _cancel_guard = CasReadCancelGuard(cancel);
        let text = tokio::task::spawn_blocking(move || -> WorkSwarmResult<Arc<str>> {
            let page = cas
                .read_text_all_with_cancel(&hash, MAX_DELIVERY_TEXT_BYTES, || {
                    cancel_worker.load(Ordering::Acquire)
                })
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => {
                        WorkSwarmError::NotFound("产物 CAS 正文缺失".into())
                    }
                    std::io::ErrorKind::InvalidInput => {
                        WorkSwarmError::Validation(error.to_string())
                    }
                    std::io::ErrorKind::InvalidData => WorkSwarmError::Conflict(error.to_string()),
                    std::io::ErrorKind::Interrupted => {
                        WorkSwarmError::Conflict("产物读取已取消".into())
                    }
                    _ => WorkSwarmError::Io(error.to_string()),
                })?;
            if page.total_bytes != expected_size || page.sha256 != hash {
                return Err(WorkSwarmError::Conflict(
                    "产物 CAS 内容身份或大小不一致".into(),
                ));
            }
            Ok(Arc::from(page.content))
        })
        .await
        .map_err(|error| WorkSwarmError::Run(format!("CAS 产物校验任务失败：{error}")))??;
        if text.len() <= (32 * 1024 * 1024usize).saturating_sub(self.retained_bytes) {
            self.retained_bytes += text.len();
            self.cached.insert(cache_key, Arc::clone(&text));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::StepRecord;

    fn state() -> GoalRunState {
        let mut plan = Plan::new("plan", "goal");
        plan.add_step(StepSpec::new("task", "m-writer"));
        let mut state = GoalRunState::new(Goal::new("goal", "read"), plan);
        state.records.insert(
            "task".into(),
            StepRecord {
                step_id: "task".into(),
                status: StepStatus::Succeeded,
                attempts: 1,
                attempt_id: Some("now".into()),
                output: Some("done".into()),
                error: None,
                skip_reason: None,
                phase_epoch: Some(1),
                validation_receipts: Vec::new(),
            },
        );
        state
    }
    fn artifact(id: &str, version: u32, attempt: &str) -> Artifact {
        serde_json::from_value(json!({"artifact_id":id,"kind":"document","version":version,
            "producer":"m-writer","content_ref":"cas://sha256:hash","created_at":"same-time",
            "team_id":"team","task_id":"task","attempt_id":attempt,"sha256":"hash"}))
        .unwrap()
    }
    fn space(artifacts: &[Artifact]) -> ProjectSpace {
        serde_json::from_value(json!({"project_id":"project","team_id":"team","version":1,"created_at":"now",
            "artifacts":artifacts.iter().map(|artifact|artifact.artifact_id.clone()).collect::<Vec<_>>()})).unwrap()
    }
    fn handoff(id: &str, refs: &[&str]) -> HandoffRecord {
        serde_json::from_value(
            json!({"handoff_id":id,"from_member":"m-writer","to_member":"*",
            "completed_summary":"done","output_artifact_refs":refs,"created_at":"now"}),
        )
        .unwrap()
    }

    #[test]
    fn reviewer_selection_uses_attempt_and_version_when_timestamps_tie() {
        let artifacts = vec![
            artifact("old", 100, "old"),
            artifact("first", 1, "now"),
            artifact("second", 2, "now"),
        ];
        let catalog = ArtifactCatalog::new("team", &space(&artifacts), artifacts).unwrap();
        assert_eq!(
            catalog
                .current_for_step(&state(), &StepSpec::new("task", "m-writer"))
                .unwrap()
                .unwrap()
                .artifact_id,
            "second"
        );
    }
    #[test]
    fn unpublished_candidate_is_not_visible_and_legacy_version_still_reserved() {
        let mut legacy = artifact("legacy", 40, "old");
        legacy.team_id.clear();
        let published = vec![legacy, artifact("published", 1, "now")];
        let mut all = published.clone();
        all.push(artifact("not-committed", 99, "now"));
        let catalog = ArtifactCatalog::new("team", &space(&published), all).unwrap();
        assert_eq!(
            catalog
                .current_for_step(&state(), &StepSpec::new("task", "m-writer"))
                .unwrap()
                .unwrap()
                .artifact_id,
            "published"
        );
        assert_eq!(catalog.next_version("m-writer").unwrap(), 41);
        assert!(catalog.get("not-committed").is_err());
    }
    #[test]
    fn foreign_team_producer_or_task_cannot_supply_current_artifact() {
        let mut foreign = artifact("foreign", 1, "now");
        foreign.team_id = "other".into();
        assert!(ArtifactCatalog::new("team", &space(&[foreign.clone()]), vec![foreign]).is_err());
        let mut wrong = artifact("wrong", 1, "now");
        wrong.producer = "m-other".into();
        let catalog = ArtifactCatalog::new("team", &space(&[wrong.clone()]), vec![wrong]).unwrap();
        assert!(catalog
            .current_for_step(&state(), &StepSpec::new("task", "m-writer"))
            .unwrap()
            .is_none());
    }
    #[test]
    fn ambiguous_current_version_fails_and_old_conflict_does_not_override_new_version() {
        let artifacts = vec![artifact("a", 1, "now"), artifact("b", 1, "now")];
        let catalog = ArtifactCatalog::new("team", &space(&artifacts), artifacts.clone()).unwrap();
        assert!(catalog
            .current_for_step(&state(), &StepSpec::new("task", "m-writer"))
            .is_err());
        let mut newer = artifacts;
        newer.push(artifact("new", 2, "now"));
        let catalog = ArtifactCatalog::new("team", &space(&newer), newer).unwrap();
        assert_eq!(
            catalog
                .current_for_step(&state(), &StepSpec::new("task", "m-writer"))
                .unwrap()
                .unwrap()
                .artifact_id,
            "new"
        );
    }
    #[test]
    fn new_timestamp_for_old_handoff_cannot_override_current_attempt() {
        let artifacts = vec![artifact("old", 1, "old"), artifact("new", 2, "now")];
        let catalog = ArtifactCatalog::new("team", &space(&artifacts), artifacts).unwrap();
        let mut obsolete = handoff("team:task:v1", &["old"]);
        obsolete.created_at = "9999".into();
        let handoffs = vec![handoff("team:task:v2", &["new"]), obsolete];
        assert_eq!(
            catalog
                .current_handoff(&state(), &state().plan.steps[0], &handoffs)
                .unwrap()
                .unwrap()
                .handoff_id,
            "team:task:v2"
        );
    }
    #[test]
    fn current_handoff_cannot_mix_old_refs_and_version_overflow_is_rejected() {
        let artifacts = vec![artifact("old", 1, "old"), artifact("new", u32::MAX, "now")];
        let catalog = ArtifactCatalog::new("team", &space(&artifacts), artifacts).unwrap();
        assert!(catalog
            .current_handoff(
                &state(),
                &state().plan.steps[0],
                &[handoff("team:task:manual:1", &["new", "old"])]
            )
            .unwrap()
            .is_none());
        assert!(catalog.next_version("m-writer").is_err());
    }
    fn cas_fixture(bytes: &[u8]) -> (tempfile::TempDir, CasStore, Artifact) {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path().join("cas")).unwrap();
        let hash = cas.put(bytes).unwrap();
        let mut artifact = artifact("text", 1, "now");
        artifact.sha256 = hash.clone();
        artifact.content_ref = format!("cas://sha256:{hash}");
        artifact.size_bytes = bytes.len() as u64;
        (dir, cas, artifact)
    }
    #[tokio::test]
    async fn same_cas_hash_reuses_verified_text_but_still_checks_size() {
        let (_dir, cas, artifact) = cas_fixture("你好".as_bytes());
        let mut cache = ArtifactTextCache::new(&cas);
        let first = cache.read(&artifact).await.unwrap();
        let second = cache.read(&artifact).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(cache.retained_bytes, 6);
        let mut wrong = artifact;
        wrong.size_bytes = 1;
        assert!(cache.read(&wrong).await.is_err());
    }
    #[tokio::test]
    async fn delivery_text_limit_and_declared_size_mismatch_fail_closed() {
        let (_dir, cas, artifact) = cas_fixture(b"valid");
        let mut cache = ArtifactTextCache::new(&cas);
        let mut oversized = artifact.clone();
        oversized.size_bytes = MAX_DELIVERY_TEXT_BYTES as u64 + 1;
        assert!(matches!(
            cache.read(&oversized).await,
            Err(WorkSwarmError::Validation(_))
        ));

        let mut mismatched = artifact;
        mismatched.size_bytes = 1;
        assert!(matches!(
            cache.read(&mismatched).await,
            Err(WorkSwarmError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn corrupt_missing_or_non_utf8_cas_never_becomes_empty_context() {
        let (dir, cas, artifact) = cas_fixture(b"valid");
        std::fs::write(dir.path().join("cas").join(&artifact.sha256), b"bad").unwrap();
        assert!(ArtifactTextCache::new(&cas).read(&artifact).await.is_err());
        std::fs::remove_file(dir.path().join("cas").join(&artifact.sha256)).unwrap();
        assert!(ArtifactTextCache::new(&cas).read(&artifact).await.is_err());
        let (_dir, cas, artifact) = cas_fixture(&[0xff, 0xfe]);
        assert!(ArtifactTextCache::new(&cas).read(&artifact).await.is_err());
    }
}
