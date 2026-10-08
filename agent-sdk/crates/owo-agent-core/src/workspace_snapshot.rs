//! Bounded source hashing shared by command receipts and final acceptance checks.
//! This module reads source identities only; it never authorizes or executes tools.
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_SNAPSHOT_PATHS: usize = 256;
pub(crate) const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;
const HASH_BUFFER_BYTES: usize = 64 * 1024;

/// Shared lexical scope check and harmless ./ alias normalization.
/// Canonical containment still has to be checked when opening the file.
pub(crate) fn workspace_relative_key(raw: &str) -> Option<String> {
    let path = Path::new(raw);
    if raw.trim().is_empty()
        || raw.len() > 512
        || raw.contains('\0')
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let key = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    (!key.is_empty()).then_some(key)
}

pub(crate) struct WorkspaceSnapshotReader {
    root: PathBuf,
    remaining_bytes: u64,
}

impl WorkspaceSnapshotReader {
    pub(crate) fn new(root: &Path) -> Option<Self> {
        Self::with_budget(root, MAX_SNAPSHOT_BYTES)
    }

    fn with_budget(root: &Path, remaining_bytes: u64) -> Option<Self> {
        let root = root.canonicalize().ok()?;
        root.is_dir().then_some(Self {
            root,
            remaining_bytes,
        })
    }

    /// Some(None) proves absence within the canonical workspace.
    /// None denotes unreadable, escaping, oversized, changed-length or over-budget data.
    pub(crate) fn read(&mut self, relative: &str) -> Option<Option<String>> {
        workspace_relative_key(relative)?;
        let path = Path::new(relative);
        let target = self.root.join(path);
        match std::fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut ancestor = target.parent()?;
                loop {
                    if let Ok(existing) = ancestor.canonicalize() {
                        return existing.starts_with(&self.root).then_some(None);
                    }
                    ancestor = ancestor.parent()?;
                }
            }
            Err(_) => None,
            Ok(_) => {
                let canonical = target.canonicalize().ok()?;
                if !canonical.starts_with(&self.root) {
                    return None;
                }
                // Reject obvious non-file targets before open; in particular, do
                // not block the synchronous evidence path on a FIFO.
                let before = std::fs::metadata(&canonical).ok()?;
                if !before.is_file()
                    || before.len() > MAX_FILE_BYTES
                    || before.len() > self.remaining_bytes
                {
                    return None;
                }
                let mut file = std::fs::File::open(canonical).ok()?;
                let metadata = file.metadata().ok()?;
                if !metadata.is_file() || metadata.len() != before.len() {
                    return None;
                }
                let expected = metadata.len();
                let mut read_bytes = 0u64;
                let mut hasher = Sha256::new();
                let mut buffer = [0u8; HASH_BUFFER_BYTES];
                while read_bytes < expected {
                    let count =
                        usize::try_from((expected - read_bytes).min(HASH_BUFFER_BYTES as u64))
                            .ok()?;
                    let actual = file.read(&mut buffer[..count]).ok()?;
                    if actual == 0 {
                        return None;
                    }
                    self.remaining_bytes = self.remaining_bytes.checked_sub(actual as u64)?;
                    read_bytes += actual as u64;
                    hasher.update(&buffer[..actual]);
                }
                // Growth or truncation during the bounded read cannot prove stable identity.
                if file.metadata().ok()?.len() != expected {
                    return None;
                }
                Some(Some(format!("{:x}", hasher.finalize())))
            }
        }
    }
}

/// Fresh hash-only fence shared by all receipts in one acceptance operation.
/// Never reuse this object after tools, another phase, or a verification batch.
pub(crate) struct WorkspaceSnapshotBatch {
    reader: Option<WorkspaceSnapshotReader>,
    observations: std::collections::HashMap<String, Option<Option<String>>>,
}
impl WorkspaceSnapshotBatch {
    pub(crate) fn new(root: Option<&Path>) -> Self {
        Self {
            reader: root.and_then(WorkspaceSnapshotReader::new),
            observations: Default::default(),
        }
    }
    pub(crate) fn is_bound(&self) -> bool {
        self.reader.is_some()
    }
    pub(crate) fn read(&mut self, raw: &str) -> Option<Option<String>> {
        let key = workspace_relative_key(raw)?;
        if let Some(observed) = self.observations.get(&key) {
            return observed.clone();
        }
        if self.observations.len() >= MAX_SNAPSHOT_PATHS {
            return None;
        }
        let observed = self.reader.as_mut().and_then(|reader| reader.read(raw));
        self.observations.insert(key, observed.clone());
        observed
    }
    pub(crate) fn subjects_match(
        &mut self,
        subjects: &std::collections::HashMap<String, String>,
    ) -> bool {
        let absent = crate::verification::workspace_path_absence_sha256();
        subjects
            .iter()
            .filter_map(|(subject, expected)| {
                subject
                    .strip_prefix("workspace-path:")
                    .map(|path| (path, expected))
            })
            .all(|(path, expected)| match self.read(path) {
                Some(None) => expected == &absent,
                Some(Some(actual)) => &actual == expected && expected != &absent,
                None => false,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_hash_matches_cas_for_empty_and_multi_chunk_sources() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = vec![0x73; HASH_BUFFER_BYTES * 3 + 7];
        std::fs::write(dir.path().join("source.rs"), &bytes).unwrap();
        std::fs::write(dir.path().join("empty.rs"), b"").unwrap();
        let mut reader = WorkspaceSnapshotReader::new(dir.path()).unwrap();
        assert_eq!(
            reader.read("source.rs"),
            Some(Some(crate::CasStore::hash_of(&bytes)))
        );
        assert_eq!(
            reader.read("empty.rs"),
            Some(Some(crate::CasStore::hash_of(&[])))
        );
    }

    #[test]
    fn aggregate_budget_is_shared_and_refuses_whole_files_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"123456").unwrap();
        std::fs::write(dir.path().join("b.rs"), b"abcdef").unwrap();
        let mut reader = WorkspaceSnapshotReader::with_budget(dir.path(), 10).unwrap();
        assert!(reader.read("a.rs").is_some());
        assert_eq!(reader.remaining_bytes, 4);
        assert!(reader.read("b.rs").is_none());
        assert_eq!(reader.remaining_bytes, 4);
        assert_eq!(reader.read("missing.rs"), Some(None));
        assert_eq!(reader.remaining_bytes, 4);
    }

    #[test]
    fn exact_budget_and_zero_budget_empty_source_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exact.rs"), b"1234").unwrap();
        std::fs::write(dir.path().join("empty.rs"), b"").unwrap();
        let mut reader = WorkspaceSnapshotReader::with_budget(dir.path(), 4).unwrap();
        assert!(reader.read("exact.rs").is_some());
        assert_eq!(reader.remaining_bytes, 0);
        assert!(reader.read("empty.rs").is_some());
        assert!(reader.read("exact.rs").is_none());
    }

    #[test]
    fn invalid_roots_directories_escape_and_sparse_oversize_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(WorkspaceSnapshotReader::new(&dir.path().join("missing")).is_none());
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let mut reader = WorkspaceSnapshotReader::new(dir.path()).unwrap();
        for path in ["", "  ", "../outside", "subdir", "a\0b"] {
            assert!(reader.read(path).is_none());
        }
        std::fs::File::create(dir.path().join("large.bin"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(reader.read("large.bin").is_none());
        assert_eq!(reader.remaining_bytes, MAX_SNAPSHOT_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_cannot_prove_absence_outside_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let mut reader = WorkspaceSnapshotReader::new(dir.path()).unwrap();
        assert!(reader.read("escape/missing.rs").is_none());
    }

    #[test]
    fn batch_reuses_aliases_while_a_new_fence_detects_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"old").unwrap();
        let mut batch = WorkspaceSnapshotBatch::new(Some(dir.path()));
        assert_eq!(
            batch.read("a.rs"),
            Some(Some(crate::CasStore::hash_of(b"old")))
        );
        std::fs::write(dir.path().join("a.rs"), b"new").unwrap();
        assert_eq!(
            batch.read("./a.rs"),
            Some(Some(crate::CasStore::hash_of(b"old")))
        );
        assert_eq!(batch.observations.len(), 1);
        assert_eq!(
            batch.reader.as_ref().unwrap().remaining_bytes,
            MAX_SNAPSHOT_BYTES - 3
        );
        assert_eq!(
            WorkspaceSnapshotBatch::new(Some(dir.path())).read("a.rs"),
            Some(Some(crate::CasStore::hash_of(b"new")))
        );
    }

    #[test]
    fn batch_absence_is_scoped_and_subject_namespaces_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let subjects = std::collections::HashMap::from([
            (
                "workspace-path:removed.rs".into(),
                crate::verification::workspace_path_absence_sha256(),
            ),
            ("step-output".into(), "output identity".into()),
        ]);
        let mut batch = WorkspaceSnapshotBatch::new(Some(dir.path()));
        assert!(batch.subjects_match(&subjects));
        std::fs::write(dir.path().join("removed.rs"), b"recreated").unwrap();
        assert!(batch.subjects_match(&subjects));
        assert!(!WorkspaceSnapshotBatch::new(Some(dir.path())).subjects_match(&subjects));
        let output_only = std::collections::HashMap::from([("artifact-id".into(), "hash".into())]);
        assert!(WorkspaceSnapshotBatch::new(None).subjects_match(&output_only));
        assert!(!WorkspaceSnapshotBatch::new(None).subjects_match(&subjects));
    }

    #[test]
    fn batch_unique_path_limit_rejects_new_paths_without_breaking_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let mut batch = WorkspaceSnapshotBatch::new(Some(dir.path()));
        for index in 0..MAX_SNAPSHOT_PATHS {
            assert_eq!(batch.read(&format!("missing-{index}")), Some(None));
        }
        assert_eq!(batch.read("missing-extra"), None);
        assert_eq!(batch.read("./missing-0"), Some(None));
        assert_eq!(batch.observations.len(), MAX_SNAPSHOT_PATHS);
    }
}
