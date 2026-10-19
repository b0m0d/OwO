//! Authorized dependency pagination, context previews, and cancellable CAS I/O.
//! The kernel owns byte/UTF-8/hash correctness; Core binds it to current task scope.
use super::artifact_catalog::artifact_hash;
use super::*;
use crate::cas_store::CasTextPage;

#[derive(Debug, Clone)]
pub struct DependencyArtifactRead {
    pub offset_bytes: u64,
    pub max_bytes: usize,
    pub expected_sha256: Option<String>,
}
impl Default for DependencyArtifactRead {
    fn default() -> Self {
        Self {
            offset_bytes: 0,
            max_bytes: 16 * 1024,
            expected_sha256: None,
        }
    }
}

struct ReadCancelGuard(Arc<AtomicBool>);
impl Drop for ReadCancelGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(super) async fn stream_cas_page(
    cas: CasStore,
    hash: String,
    offset: u64,
    max: usize,
) -> WorkSwarmResult<CasTextPage> {
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = ReadCancelGuard(Arc::clone(&cancel));
    tokio::task::spawn_blocking(move || {
        cas.read_text_page_with_cancel(&hash, offset, max, || cancel.load(Ordering::Acquire))
    })
    .await
    .map_err(|error| WorkSwarmError::Run(format!("CAS 读取任务失败：{error}")))?
    .map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => WorkSwarmError::NotFound("CAS 正文缺失".into()),
        std::io::ErrorKind::InvalidInput => WorkSwarmError::Validation(error.to_string()),
        std::io::ErrorKind::InvalidData => WorkSwarmError::Conflict(error.to_string()),
        std::io::ErrorKind::Interrupted => WorkSwarmError::Conflict("产物读取已取消".into()),
        _ => WorkSwarmError::Io(error.to_string()),
    })
}

pub(super) async fn read_context_cas_text(
    cas: &CasStore,
    hash: &str,
    max_bytes: usize,
) -> WorkSwarmResult<String> {
    use std::io;
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = ReadCancelGuard(Arc::clone(&cancel));
    let cas = cas.clone();
    let hash = hash.to_string();
    tokio::task::spawn_blocking(move || -> io::Result<String> {
        let page =
            cas.read_text_prefix_with_cancel(&hash, max_bytes, || cancel.load(Ordering::Acquire))?;
        if page.total_bytes > max_bytes as u64 || !page.eof {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "context CAS object exceeds its size limit",
            ));
        }
        Ok(page.content)
    })
    .await
    .map_err(|error| WorkSwarmError::Run(format!("context CAS read task failed: {error}")))?
    .map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => WorkSwarmError::NotFound("context CAS content missing".into()),
        io::ErrorKind::InvalidInput => WorkSwarmError::Validation(error.to_string()),
        io::ErrorKind::InvalidData => WorkSwarmError::Conflict(error.to_string()),
        io::ErrorKind::Interrupted => WorkSwarmError::Conflict("context CAS read canceled".into()),
        _ => WorkSwarmError::Io(error.to_string()),
    })
}

/// Read CAS prefixes concurrently with a strict in-flight cap and stable output order.
/// Identical (hash, byte limit) requests are deduplicated within this batch.
pub(super) async fn read_context_pages(
    cas: &CasStore,
    requests: Vec<(String, usize)>,
    max_concurrency: usize,
) -> WorkSwarmResult<Vec<Arc<CasTextPage>>> {
    type PreviewKey = (String, usize);
    if requests.iter().any(|(_, max_bytes)| *max_bytes > 64 * 1024) {
        return Err(WorkSwarmError::Validation(
            "context CAS preview exceeds 65536 bytes".into(),
        ));
    }
    let mut request_misses = Vec::with_capacity(requests.len());
    let mut miss_by_key: HashMap<PreviewKey, usize> = HashMap::new();
    let mut misses = Vec::new();
    for (hash, max_bytes) in requests {
        let key = (hash.clone(), max_bytes);
        let index = if let Some(index) = miss_by_key.get(&key).copied() {
            index
        } else {
            let index = misses.len();
            misses.push(key.clone());
            miss_by_key.insert(key, index);
            index
        };
        request_misses.push(index);
    }

    type PreviewResult = WorkSwarmResult<(usize, Arc<CasTextPage>)>;
    let mut pending = misses.into_iter().enumerate();
    let mut tasks: tokio::task::JoinSet<PreviewResult> = tokio::task::JoinSet::new();
    let concurrency = max_concurrency.max(1);
    let spawn_read = |tasks: &mut tokio::task::JoinSet<PreviewResult>,
                      index: usize,
                      hash: String,
                      max_bytes: usize,
                      cas: CasStore| {
        tasks.spawn(async move {
            let read_max = if max_bytes == 0 { 0 } else { max_bytes.max(4) };
            let mut page = stream_cas_page(cas, hash, 0, read_max).await?;
            if page.content.len() > max_bytes {
                let mut end = max_bytes;
                while !page.content.is_char_boundary(end) {
                    end -= 1;
                }
                page.content.truncate(end);
                page.next_offset_bytes = end as u64;
                page.eof = page.next_offset_bytes == page.total_bytes;
            }
            Ok((index, Arc::new(page)))
        })
    };
    for (index, (hash, max_bytes)) in pending.by_ref().take(concurrency) {
        spawn_read(&mut tasks, index, hash, max_bytes, cas.clone());
    }
    let mut loaded: Vec<Option<Arc<CasTextPage>>> = (0..miss_by_key.len()).map(|_| None).collect();
    while let Some(joined) = tasks.join_next().await {
        let (index, page) = match joined {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                tasks.abort_all();
                return Err(error);
            }
            Err(error) => {
                tasks.abort_all();
                return Err(WorkSwarmError::Run(format!(
                    "并行 CAS 预览读取失败：{error}"
                )));
            }
        };
        loaded[index] = Some(page);
        if let Some((next_index, (hash, max_bytes))) = pending.next() {
            spawn_read(&mut tasks, next_index, hash, max_bytes, cas.clone());
        }
    }

    request_misses
        .into_iter()
        .map(|index| {
            loaded[index]
                .as_ref()
                .map(Arc::clone)
                .ok_or_else(|| WorkSwarmError::Run("并行 CAS 预览缺少已完成读取结果".into()))
        })
        .collect()
}

/// Coordinator-wide immutable preview cache and bounded per-key single-flight locks.
const SHARED_PREVIEW_CACHE_BYTES: usize = 32 * 1024 * 1024;
const SHARED_PREVIEW_CACHE_ENTRIES: usize = 4096;

#[derive(Debug)]
pub(super) struct SharedArtifactPreviewCache {
    pages: HashMap<(String, usize), Arc<CasTextPage>>,
    insertion_order: std::collections::VecDeque<(String, usize)>,
    inflight: HashMap<(String, usize), Arc<tokio::sync::Mutex<()>>>,
    inflight_order: std::collections::VecDeque<(String, usize)>,
    retained_bytes: usize,
    max_bytes: usize,
    max_entries: usize,
}

impl Default for SharedArtifactPreviewCache {
    fn default() -> Self {
        Self::with_limits(SHARED_PREVIEW_CACHE_BYTES, SHARED_PREVIEW_CACHE_ENTRIES)
    }
}

impl SharedArtifactPreviewCache {
    fn with_limits(max_bytes: usize, max_entries: usize) -> Self {
        Self {
            pages: HashMap::new(),
            insertion_order: std::collections::VecDeque::new(),
            inflight: HashMap::new(),
            inflight_order: std::collections::VecDeque::new(),
            retained_bytes: 0,
            max_bytes,
            max_entries,
        }
    }

    fn lock_for(&mut self, key: &(String, usize)) -> Arc<tokio::sync::Mutex<()>> {
        if let Some(lock) = self.inflight.get(key) {
            return Arc::clone(lock);
        }
        let max_locks = self.max_entries.max(1);
        while self.inflight.len() >= max_locks {
            let Some(oldest) = self.inflight_order.pop_front() else {
                break;
            };
            self.inflight.remove(&oldest);
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        self.inflight.insert(key.clone(), Arc::clone(&lock));
        self.inflight_order.push_back(key.clone());
        lock
    }

    fn get(
        &self,
        key: &(String, usize),
        expected_size_bytes: u64,
    ) -> WorkSwarmResult<Option<Arc<CasTextPage>>> {
        let Some(page) = self.pages.get(key) else {
            return Ok(None);
        };
        if page.total_bytes != expected_size_bytes {
            return Err(WorkSwarmError::Conflict(
                "产物大小与已验证共享 CAS 预览不一致".into(),
            ));
        }
        Ok(Some(Arc::clone(page)))
    }

    fn insert(&mut self, key: (String, usize), page: Arc<CasTextPage>) -> WorkSwarmResult<()> {
        if let Some(existing) = self.pages.get(&key) {
            if existing.total_bytes != page.total_bytes {
                return Err(WorkSwarmError::Conflict(
                    "相同 CAS 预览键对应不同的产物大小".into(),
                ));
            }
            return Ok(());
        }
        let page_bytes = page.content.len();
        if self.max_entries == 0 || page_bytes > self.max_bytes {
            return Ok(());
        }
        while self.pages.len() >= self.max_entries
            || page_bytes > self.max_bytes.saturating_sub(self.retained_bytes)
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.pages.remove(&oldest) {
                self.retained_bytes = self.retained_bytes.saturating_sub(evicted.content.len());
            }
        }
        self.retained_bytes = self.retained_bytes.saturating_add(page_bytes);
        self.insertion_order.push_back(key.clone());
        self.pages.insert(key, page);
        Ok(())
    }
}

/// Locally deduplicated verified previews with an optional shared immutable cache; never retain a full large object in a prompt.
pub(super) struct ArtifactPreviewCache {
    cas: CasStore,
    pages: HashMap<(String, usize), Arc<CasTextPage>>,
    retained_bytes: usize,
    shared: Option<Arc<Mutex<SharedArtifactPreviewCache>>>,
}
impl ArtifactPreviewCache {
    pub(super) fn with_shared(
        cas: &CasStore,
        shared: Arc<Mutex<SharedArtifactPreviewCache>>,
    ) -> Self {
        Self {
            cas: cas.clone(),
            pages: HashMap::new(),
            retained_bytes: 0,
            shared: Some(shared),
        }
    }

    /// Read independent bounded previews concurrently while preserving request order.
    /// Duplicate (CAS hash, byte limit) requests share one verified disk read.
    pub(super) async fn read_many(
        &mut self,
        requests: Vec<(&Artifact, usize)>,
        max_concurrency: usize,
    ) -> WorkSwarmResult<Vec<Arc<CasTextPage>>> {
        type PreviewKey = (String, usize);
        let mut output: Vec<Option<Arc<CasTextPage>>> = vec![None; requests.len()];
        let mut request_misses = Vec::with_capacity(requests.len());
        let mut cached_hits: Vec<(usize, String, u64, Arc<CasTextPage>)> = Vec::new();
        let mut miss_by_key: HashMap<PreviewKey, usize> = HashMap::new();
        let mut misses: Vec<(String, usize, u64)> = Vec::new();

        for (request_index, (artifact, max_bytes)) in requests.iter().enumerate() {
            let hash = artifact_hash(artifact)?.to_string();
            let key = (hash.clone(), *max_bytes);
            if let Some(page) = self.pages.get(&key) {
                if page.total_bytes != artifact.size_bytes || page.sha256 != hash {
                    return Err(WorkSwarmError::Conflict(
                        "缓存预览身份与当前产物声明不一致".into(),
                    ));
                }
                cached_hits.push((request_index, hash, artifact.size_bytes, Arc::clone(page)));
                request_misses.push(None);
                continue;
            }
            if let Some(shared) = &self.shared {
                let page = shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&key, artifact.size_bytes)?;
                if let Some(page) = page {
                    if page.sha256 != hash {
                        return Err(WorkSwarmError::Conflict(
                            "缓存预览哈希与当前 CAS 引用不一致".into(),
                        ));
                    }
                    cached_hits.push((request_index, hash, artifact.size_bytes, page));
                    request_misses.push(None);
                    continue;
                }
            }

            let miss_index = if let Some(index) = miss_by_key.get(&key).copied() {
                if misses[index].2 != artifact.size_bytes {
                    return Err(WorkSwarmError::Conflict(
                        "相同 CAS 预览请求声明了不同的产物大小".into(),
                    ));
                }
                index
            } else {
                let index = misses.len();
                misses.push((hash, *max_bytes, artifact.size_bytes));
                miss_by_key.insert(key, index);
                index
            };
            request_misses.push(Some(miss_index));
        }

        // A preview cache hit still validates the immutable CAS backing object. Batch
        // identical hashes and cap validation I/O to the same per-call concurrency.
        let mut cached_hashes = HashMap::<String, (u64, Arc<CasTextPage>)>::new();
        for (_, hash, expected_size, page) in &cached_hits {
            if page.total_bytes != *expected_size {
                return Err(WorkSwarmError::Conflict(
                    "缓存预览大小与当前 Artifact 声明不一致".into(),
                ));
            }
            if let Some((known_size, _)) = cached_hashes.get(hash) {
                if *known_size != *expected_size {
                    return Err(WorkSwarmError::Conflict(
                        "相同 CAS 哈希对应不同的声明大小".into(),
                    ));
                }
            } else {
                cached_hashes.insert(hash.clone(), (*expected_size, Arc::clone(page)));
            }
        }
        if !cached_hashes.is_empty() {
            let hashes = cached_hashes.keys().cloned().collect::<Vec<_>>();
            let verified = read_context_pages(
                &self.cas,
                hashes.iter().cloned().map(|hash| (hash, 0)).collect(),
                max_concurrency,
            )
            .await?;
            for (hash, backing) in hashes.into_iter().zip(verified) {
                let expected_size = cached_hashes[&hash].0;
                if backing.sha256 != hash || backing.total_bytes != expected_size {
                    return Err(WorkSwarmError::Conflict(
                        "CAS 正文与缓存预览身份不一致".into(),
                    ));
                }
            }
            for (request_index, _, _, page) in cached_hits {
                output[request_index] = Some(page);
            }
        }

        // Acquire per-key locks in sorted order to single-flight concurrent context
        // assemblies without holding the cache mutex while any disk I/O is in progress.
        let mut shared_guards = Vec::new();
        if let Some(shared) = &self.shared {
            let mut lock_entries = {
                let mut cache = shared.lock().unwrap_or_else(|error| error.into_inner());
                misses
                    .iter()
                    .map(|(hash, max_bytes, _)| {
                        let key = (hash.clone(), *max_bytes);
                        (key.clone(), cache.lock_for(&key))
                    })
                    .collect::<Vec<_>>()
            };
            lock_entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (_, lock) in lock_entries {
                shared_guards.push(lock.lock_owned().await);
            }
        }

        let mut loaded: Vec<Option<Arc<CasTextPage>>> = (0..misses.len()).map(|_| None).collect();
        let mut disk_misses = Vec::new();
        for (index, (hash, max_bytes, expected_size)) in misses.iter().enumerate() {
            if let Some(shared) = &self.shared {
                let page = shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get(&(hash.clone(), *max_bytes), *expected_size)?;
                if let Some(page) = page {
                    loaded[index] = Some(page);
                    continue;
                }
            }
            disk_misses.push(index);
        }

        let pages = read_context_pages(
            &self.cas,
            disk_misses
                .iter()
                .map(|index| (misses[*index].0.clone(), misses[*index].1))
                .collect(),
            max_concurrency,
        )
        .await?;
        for (index, page) in disk_misses.into_iter().zip(pages) {
            if page.total_bytes != misses[index].2 {
                return Err(WorkSwarmError::Conflict("产物大小与 CAS 正文不一致".into()));
            }
            let key = (misses[index].0.clone(), misses[index].1);
            if page.content.len() <= (32 * 1024 * 1024usize).saturating_sub(self.retained_bytes) {
                self.retained_bytes += page.content.len();
                self.pages.insert(key.clone(), Arc::clone(&page));
            }
            if let Some(shared) = &self.shared {
                shared
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .insert(key, Arc::clone(&page))?;
            }
            loaded[index] = Some(page);
        }
        drop(shared_guards);
        for (request_index, miss_index) in request_misses.into_iter().enumerate() {
            if let Some(miss_index) = miss_index {
                output[request_index] = loaded[miss_index].as_ref().map(Arc::clone);
            }
        }
        output
            .into_iter()
            .map(|page| {
                page.ok_or_else(|| WorkSwarmError::Run("并行 CAS 预览缺少已完成读取结果".into()))
            })
            .collect()
    }
}

impl TeamCoordinator {
    /// Compatible first-page API. New callers use the cursor-aware page API.
    pub async fn read_dependency_artifact(
        &self,
        team_id: &str,
        member_id: &str,
        step_id: &str,
        artifact_id: &str,
        max_bytes: usize,
    ) -> WorkSwarmResult<Value> {
        self.read_dependency_artifact_page(
            team_id,
            member_id,
            step_id,
            artifact_id,
            &DependencyArtifactRead {
                max_bytes: max_bytes.clamp(1, 64 * 1024),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn read_dependency_artifact_page(
        &self,
        team_id: &str,
        member_id: &str,
        step_id: &str,
        artifact_id: &str,
        request: &DependencyArtifactRead,
    ) -> WorkSwarmResult<Value> {
        if request.max_bytes == 0 || request.max_bytes > 64 * 1024 {
            return Err(WorkSwarmError::Validation(
                "max_bytes 必须为 1..=65536".into(),
            ));
        }
        if request.offset_bytes > 0 && request.expected_sha256.is_none() {
            return Err(WorkSwarmError::Validation(
                "续读必须提供第一页的 expected_sha256".into(),
            ));
        }
        let (_team, space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        Self::role_spec_of_member(&meta, member_id)?;
        let step = state
            .plan
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
        if step.worker != member_id {
            return Err(WorkSwarmError::Validation(
                "请求成员不是当前步骤的承担者".into(),
            ));
        }
        let catalog = self.artifact_catalog(team_id, &space).await?;
        for dependency in &step.depends_on {
            let Some(artifact) = catalog.current(&state, dependency)? else {
                continue;
            };
            if artifact.artifact_id != artifact_id {
                continue;
            }
            let hash = artifact_hash(artifact)?;
            if request
                .expected_sha256
                .as_deref()
                .is_some_and(|expected| expected != hash)
            {
                return Err(WorkSwarmError::Conflict(
                    "产物身份已变化，不能将不同版本的分页拼接".into(),
                ));
            }
            if request.offset_bytes > artifact.size_bytes {
                return Err(WorkSwarmError::Validation(
                    "offset_bytes 超过产物长度".into(),
                ));
            }
            let page = stream_cas_page(
                self.cas.clone(),
                hash.to_string(),
                request.offset_bytes,
                request.max_bytes,
            )
            .await?;
            if page.total_bytes != artifact.size_bytes {
                return Err(WorkSwarmError::Conflict("产物大小与 CAS 正文不一致".into()));
            }
            // Disk I/O suspends this operation. Recheck the published scope and
            // execution identity before giving an old Worker a successful result.
            let (_latest_team, latest_space, latest_state) = self.load_bundle(team_id).await?;
            let latest_step = latest_state
                .plan
                .steps
                .iter()
                .find(|candidate| candidate.id == step_id)
                .ok_or_else(|| WorkSwarmError::Conflict("读取期间步骤已失效".into()))?;
            let consumer_attempt = |snapshot: &GoalRunState| {
                snapshot
                    .records
                    .get(step_id)
                    .and_then(|record| record.attempt_id.clone())
            };
            if latest_space.project_id != space.project_id
                || latest_state.execution_epoch != state.execution_epoch
                || latest_step.worker != member_id
                || latest_step.depends_on != step.depends_on
                || consumer_attempt(&latest_state) != consumer_attempt(&state)
            {
                return Err(WorkSwarmError::Conflict(
                    "读取期间执行身份或依赖范围已变化".into(),
                ));
            }
            let refreshed;
            let latest_catalog = if latest_space.artifacts != space.artifacts {
                refreshed = self.artifact_catalog(team_id, &latest_space).await?;
                &refreshed
            } else {
                &catalog
            };
            let current = latest_catalog
                .current(&latest_state, dependency)?
                .ok_or_else(|| WorkSwarmError::Conflict("读取期间上游产物已失效".into()))?;
            if current.artifact_id != artifact.artifact_id || current.sha256 != artifact.sha256 {
                return Err(WorkSwarmError::Conflict(
                    "读取期间上游产物版本已变化".into(),
                ));
            }
            return Ok(json!({
                "artifact_id":artifact.artifact_id,"kind":artifact.kind,"version":artifact.version,
                "producer":artifact.producer,"task_id":artifact.task_id,"attempt_id":artifact.attempt_id,
                "review_state":format!("{:?}",artifact.review_state),"content_ref":artifact.content_ref,
                "sha256":page.sha256,"content":page.content,"content_bytes":page.total_bytes,
                "offset_bytes":page.offset_bytes,"next_offset_bytes":page.next_offset_bytes,
                "eof":page.eof,"truncated":page.offset_bytes>0 || !page.eof,
            }));
        }
        Err(WorkSwarmError::Validation(format!(
            "产物不属于当前步骤的直接依赖：{artifact_id}"
        )))
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[tokio::test]
    async fn tiny_preview_remains_verified_and_bounded_without_losing_unicode_identity() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path().join("cas")).unwrap();
        let full = "中文正文".repeat(10000);
        let hash = cas.put(full.as_bytes()).unwrap();
        let artifact: Artifact = serde_json::from_value(json!({
            "artifact_id":"a","kind":"document","producer":"m-a","version":1,"created_at":"now",
            "content_ref":format!("cas://sha256:{hash}"),"sha256":hash,"size_bytes":full.len()
        }))
        .unwrap();
        let mut cache = ArtifactPreviewCache::with_shared(
            &cas,
            Arc::new(Mutex::new(SharedArtifactPreviewCache::default())),
        );
        for max in 0..7 {
            let page = cache
                .read_many(vec![(&artifact, max)], 1)
                .await
                .unwrap()
                .remove(0);
            assert!(page.content.len() <= max && full.starts_with(&page.content));
            assert_eq!(page.total_bytes, full.len() as u64);
            assert_eq!(page.sha256, artifact.sha256);
        }
        let first = cache
            .read_many(vec![(&artifact, 6)], 1)
            .await
            .unwrap()
            .remove(0);
        let again = cache
            .read_many(vec![(&artifact, 6)], 1)
            .await
            .unwrap()
            .remove(0);
        assert!(Arc::ptr_eq(&first, &again));
        let mut wrong = artifact;
        wrong.size_bytes += 1;
        assert!(cache.read_many(vec![(&wrong, 6)], 1).await.is_err());
    }

    #[tokio::test]
    async fn shared_cache_reuses_verified_pages_across_context_assemblies() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path().join("cas")).unwrap();
        let body = "shared immutable artifact";
        let hash = cas.put(body.as_bytes()).unwrap();
        let artifact: Artifact = serde_json::from_value(json!({
            "artifact_id":"shared","kind":"document","producer":"m-a","version":1,
            "created_at":"now","content_ref":format!("cas://sha256:{hash}"),
            "sha256":hash,"size_bytes":body.len()
        }))
        .unwrap();
        let shared = Arc::new(Mutex::new(SharedArtifactPreviewCache::default()));
        let mut first_context = ArtifactPreviewCache::with_shared(&cas, Arc::clone(&shared));
        let mut second_context = ArtifactPreviewCache::with_shared(&cas, shared);
        let (first, second) = tokio::join!(
            first_context.read_many(vec![(&artifact, 12)], 1),
            second_context.read_many(vec![(&artifact, 12)], 1),
        );
        let first = first.unwrap().remove(0);
        let second = second.unwrap().remove(0);

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.content, "shared immut");
        assert_eq!(second.total_bytes, body.len() as u64);
    }

    #[tokio::test]
    async fn shared_cache_does_not_hide_cas_corruption_after_a_verified_read() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path().join("cas")).unwrap();
        let body = "cache cannot bless later CAS corruption";
        let hash = cas.put(body.as_bytes()).unwrap();
        let artifact: Artifact = serde_json::from_value(json!({
            "artifact_id":"corrupt-after-cache","kind":"document","producer":"m-a","version":1,
            "created_at":"now","content_ref":format!("cas://sha256:{hash}"),
            "sha256":hash,"size_bytes":body.len()
        }))
        .unwrap();
        let shared = Arc::new(Mutex::new(SharedArtifactPreviewCache::default()));
        let mut first = ArtifactPreviewCache::with_shared(&cas, Arc::clone(&shared));
        assert_eq!(
            first.read_many(vec![(&artifact, 12)], 1).await.unwrap()[0].content,
            "cache cannot"
        );
        std::fs::write(dir.path().join("cas").join(&hash), b"tampered").unwrap();
        let mut second = ArtifactPreviewCache::with_shared(&cas, shared);
        assert!(second.read_many(vec![(&artifact, 12)], 1).await.is_err());
    }

    #[test]
    fn shared_cache_evicts_old_pages_to_respect_byte_and_entry_limits() {
        let mut cache = SharedArtifactPreviewCache::with_limits(5, 2);
        let page = |content: &str| {
            Arc::new(CasTextPage {
                content: content.to_string(),
                offset_bytes: 0,
                next_offset_bytes: content.len() as u64,
                total_bytes: content.len() as u64,
                sha256: "hash".to_string(),
                eof: true,
            })
        };
        cache.insert(("a".to_string(), 2), page("aa")).unwrap();
        cache.insert(("b".to_string(), 2), page("bb")).unwrap();
        cache.insert(("c".to_string(), 2), page("ccc")).unwrap();

        assert_eq!(cache.pages.len(), 2);
        assert_eq!(cache.retained_bytes, 5);
        assert!(cache.get(&("a".to_string(), 2), 2).unwrap().is_none());
        assert!(cache.get(&("b".to_string(), 2), 2).unwrap().is_some());
        assert!(cache.get(&("c".to_string(), 2), 3).unwrap().is_some());
        assert!(cache.get(&("b".to_string(), 2), 99).is_err());
    }

    #[tokio::test]
    async fn bounded_batch_preview_reads_keep_order_and_deduplicate_verified_pages() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path().join("cas")).unwrap();
        let first_body = "甲甲甲";
        let second_body = "bravo";
        let first_hash = cas.put(first_body.as_bytes()).unwrap();
        let second_hash = cas.put(second_body.as_bytes()).unwrap();
        let artifact = |id: &str, body: &str, hash: &str| -> Artifact {
            serde_json::from_value(json!({
                "artifact_id": id, "kind": "document", "producer": "worker",
                "version": 1, "created_at": "now",
                "content_ref": format!("cas://sha256:{hash}"),
                "sha256": hash, "size_bytes": body.len()
            }))
            .unwrap()
        };
        let first = artifact("first", first_body, &first_hash);
        let second = artifact("second", second_body, &second_hash);
        let mut cache = ArtifactPreviewCache::with_shared(
            &cas,
            Arc::new(Mutex::new(SharedArtifactPreviewCache::default())),
        );
        let pages = cache
            .read_many(vec![(&second, 3), (&first, 4), (&second, 3)], 2)
            .await
            .unwrap();

        assert_eq!(pages.len(), 3);
        assert_eq!(pages[0].content, "bra");
        assert_eq!(pages[1].content, "甲");
        assert_eq!(pages[2].content, "bra");
        assert!(Arc::ptr_eq(&pages[0], &pages[2]));
        assert_eq!(pages[0].sha256, second_hash);
        assert_eq!(pages[1].sha256, first_hash);

        let mut wrong_size = first.clone();
        wrong_size.size_bytes += 1;
        assert!(cache
            .read_many(vec![(&first, 4), (&wrong_size, 4)], 2)
            .await
            .is_err());
    }
}
