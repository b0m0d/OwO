//! One verification operation shares bounded, immutable file observations.
//! A fresh WorkspaceSnapshotBatch must fence receipts before acceptance.
use super::{exact_string_argument, is_registered_behavior_command};
use crate::plan::{ValidationVerdictV1, VerificationRequirementV1, VerificationScopeV1};
use crate::workspace_snapshot::{
    workspace_relative_key, MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_PATHS,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
enum WorkspaceReadFailure {
    Unsupported(String),
    Failed(String),
    Unverified(String),
}
#[derive(Debug)]
struct WorkspaceFile {
    bytes: Vec<u8>,
    sha256: String,
}
/// This batch is never shared between tasks, attempts or verification operations.
pub(crate) struct WorkspaceValidationBatch {
    root: Option<PathBuf>,
    files: BTreeMap<String, Result<Arc<WorkspaceFile>, WorkspaceReadFailure>>,
    remaining_bytes: u64,
}
impl WorkspaceValidationBatch {
    pub(crate) fn new(root: Option<&Path>) -> Self {
        Self {
            root: root
                .and_then(|path| path.canonicalize().ok())
                .filter(|path| path.is_dir()),
            files: BTreeMap::new(),
            remaining_bytes: MAX_SNAPSHOT_BYTES,
        }
    }

    fn file(&mut self, raw: &str) -> Result<Arc<WorkspaceFile>, WorkspaceReadFailure> {
        let key = workspace_relative_key(raw).ok_or_else(|| {
            WorkspaceReadFailure::Unsupported(format!("workspace scope 路径非法：{raw}"))
        })?;
        if let Some(observation) = self.files.get(&key) {
            return observation.clone();
        }
        if self.files.len() >= MAX_SNAPSHOT_PATHS {
            return Err(WorkspaceReadFailure::Unverified(
                "本轮 workspace 验证路径超过 256 项上限".into(),
            ));
        }
        let result = self.read_file(raw);
        self.files.insert(key, result.clone());
        result
    }

    fn read_file(&mut self, raw: &str) -> Result<Arc<WorkspaceFile>, WorkspaceReadFailure> {
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| WorkspaceReadFailure::Unverified("workspace 根目录不可用".into()))?;
        let canonical = root.join(raw).canonicalize().map_err(|error| {
            WorkspaceReadFailure::Failed(format!("workspace 文件不可读取 {raw}：{error}"))
        })?;
        if !canonical.starts_with(root) {
            return Err(WorkspaceReadFailure::Unsupported(format!(
                "workspace 路径越界：{raw}"
            )));
        }
        // Check the target before open so a directory or FIFO cannot block the
        // validation executor while opening an obvious non-file path.
        let before = std::fs::metadata(&canonical).map_err(|error| {
            WorkspaceReadFailure::Failed(format!("workspace 文件元数据不可读取 {raw}：{error}"))
        })?;
        if !before.is_file() {
            return Err(WorkspaceReadFailure::Failed(format!(
                "workspace 路径不是普通文件：{raw}"
            )));
        }
        if before.len() > MAX_FILE_BYTES {
            return Err(WorkspaceReadFailure::Unsupported(format!(
                "workspace 文件超过 8 MiB 验证上限：{raw}"
            )));
        }
        if before.len() > self.remaining_bytes {
            return Err(WorkspaceReadFailure::Unverified(
                "本轮 workspace 验证读取超过 32 MiB 累计上限".into(),
            ));
        }
        let mut file = std::fs::File::open(&canonical).map_err(|error| {
            WorkspaceReadFailure::Failed(format!("workspace 文件读取失败 {raw}：{error}"))
        })?;
        let metadata = file.metadata().map_err(|error| {
            WorkspaceReadFailure::Failed(format!("workspace 文件元数据不可读取 {raw}：{error}"))
        })?;
        if !metadata.is_file() || metadata.len() != before.len() {
            return Err(WorkspaceReadFailure::Failed(format!(
                "workspace 文件在打开期间发生变化：{raw}"
            )));
        }
        let expected = metadata.len();
        // Even a concurrently growing file cannot turn read_to_end into an unbounded allocation.
        let mut bytes = Vec::with_capacity(expected as usize + 1);
        let result = (&mut file).take(expected + 1).read_to_end(&mut bytes);
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes.len() as u64);
        result.map_err(|error| {
            WorkspaceReadFailure::Failed(format!("workspace 文件读取失败 {raw}：{error}"))
        })?;
        if bytes.len() as u64 != expected
            || file.metadata().map(|meta| meta.len()).ok() != Some(expected)
        {
            return Err(WorkspaceReadFailure::Failed(format!(
                "workspace 文件在读取期间发生变化：{raw}"
            )));
        }
        let sha256 = crate::CasStore::hash_of(&bytes);
        Ok(Arc::new(WorkspaceFile { bytes, sha256 }))
    }

    pub(crate) fn execute_registered(
        &mut self,
        requirement: &VerificationRequirementV1,
        content: &str,
    ) -> (
        ValidationVerdictV1,
        Option<String>,
        BTreeMap<String, String>,
    ) {
        match &requirement.scope {
            VerificationScopeV1::StepOutput => {
                let (verdict, detail) = super::execute_requirement(requirement, content);
                (verdict, detail, BTreeMap::new())
            }
            VerificationScopeV1::WorkspacePaths { .. } => self.execute_workspace(requirement),
            _ => (
                ValidationVerdictV1::Unsupported,
                Some(format!(
                    "validator {} 的 scope 未注册，当前为 unsupported/unverified",
                    requirement.validator_id
                )),
                BTreeMap::new(),
            ),
        }
    }

    pub(crate) fn execute_workspace(
        &mut self,
        requirement: &VerificationRequirementV1,
    ) -> (
        ValidationVerdictV1,
        Option<String>,
        BTreeMap<String, String>,
    ) {
        let unsupported = |detail: String| {
            (
                ValidationVerdictV1::Unsupported,
                Some(detail),
                BTreeMap::new(),
            )
        };
        if requirement.validator_version.as_deref() != Some("1") {
            return unsupported(format!(
                "validator {} 版本未注册，当前为 unsupported/unverified",
                requirement.validator_id
            ));
        }
        if requirement.resources.cpu_slots != 1
            || requirement.resources.memory_mb < 8
            || requirement.resources.memory_mb > 128
            || requirement.resources.exclusive_workspace
            || requirement.resources.timeout_ms == 0
            || requirement.resources.timeout_ms > 30_000
        {
            return unsupported("workspace validator 资源声明超出宿主注册范围".to_string());
        }
        let started = std::time::Instant::now();
        let paths = match &requirement.scope {
            VerificationScopeV1::WorkspacePaths { relative_paths }
                if !relative_paths.is_empty() && relative_paths.len() <= 16 =>
            {
                relative_paths
            }
            _ => {
                return unsupported("workspace validator 要求 1..=16 个 WorkspacePaths".to_string())
            }
        };
        if self.root.is_none() {
            return (
                ValidationVerdictV1::Unverified,
                Some("workspace 根目录不可用，验证未执行".to_string()),
                BTreeMap::new(),
            );
        }
        if !workspace_validator_arguments_supported(
            &requirement.validator_id,
            &requirement.arguments,
        ) {
            return unsupported("workspace validator 参数不符合宿主注册契约".to_string());
        }
        let expected_text = match requirement.validator_id.as_str() {
            "workspace-file-exists-v1" | "workspace-file-non-empty-v1" => {
                if !requirement
                    .arguments
                    .as_object()
                    .is_some_and(|args| args.is_empty())
                {
                    return unsupported(format!("{} 不接受参数", requirement.validator_id));
                }
                None
            }
            "workspace-file-contains-v1" => {
                let Some(text) = exact_string_argument(&requirement.arguments, "text") else {
                    return unsupported(
                        "workspace-file-contains-v1 要求且仅接受字符串参数 text".to_string(),
                    );
                };
                if text.is_empty() {
                    return unsupported("workspace-file-contains-v1 的 text 不能为空".to_string());
                }
                Some(text.to_string())
            }
            "workspace-json-field-equals-v1" => {
                let Some(arguments) = requirement.arguments.as_object() else {
                    return unsupported(
                        "workspace-json-field-equals-v1 要求对象参数 field 和 expected".to_string(),
                    );
                };
                if arguments.len() != 2 {
                    return unsupported(
                        "workspace-json-field-equals-v1 只接受 field 与 expected 两个参数"
                            .to_string(),
                    );
                }
                let Some(field) = arguments.get("field").and_then(Value::as_str) else {
                    return unsupported(
                        "workspace-json-field-equals-v1 要求字符串参数 field".to_string(),
                    );
                };
                let Some(expected) = arguments.get("expected").and_then(Value::as_str) else {
                    return unsupported(
                        "workspace-json-field-equals-v1 要求字符串参数 expected".to_string(),
                    );
                };
                if field.is_empty() || field.contains('.') {
                    return unsupported(
                        "workspace-json-field-equals-v1 只接受顶层 JSON 字段与字符串 expected"
                            .to_string(),
                    );
                }
                Some(format!("{field}\0{expected}"))
            }
            "workspace-command-success-v1" => {
                return unsupported(
                    "workspace-command-success-v1 必须由 DeliveryGate 消费宿主命令回执".to_string(),
                )
            }
            unknown => {
                return unsupported(format!(
                    "workspace validator「{unknown}」未注册，当前为 unsupported/unverified"
                ))
            }
        };

        let mut hashes = BTreeMap::new();
        let mut failure = None;
        for raw in paths {
            let observed = match self.file(raw) {
                Ok(file) => file,
                Err(WorkspaceReadFailure::Unsupported(reason)) => return unsupported(reason),
                Err(WorkspaceReadFailure::Unverified(reason)) => {
                    return (ValidationVerdictV1::Unverified, Some(reason), hashes);
                }
                Err(WorkspaceReadFailure::Failed(reason)) => {
                    failure = Some(reason);
                    continue;
                }
            };
            let bytes = observed.bytes.as_slice();
            if started.elapsed().as_millis() > u128::from(requirement.resources.timeout_ms) {
                return (
                    ValidationVerdictV1::Unverified,
                    Some("workspace validator 超过声明的执行时间预算".to_string()),
                    hashes,
                );
            }
            let digest = observed.sha256.clone();
            hashes.insert(format!("workspace-path:{raw}"), digest);
            let valid = match requirement.validator_id.as_str() {
                "workspace-file-exists-v1" => true,
                "workspace-file-non-empty-v1" => !bytes.is_empty(),
                "workspace-file-contains-v1" => {
                    std::str::from_utf8(bytes).ok().is_some_and(|content| {
                        expected_text
                            .as_deref()
                            .is_some_and(|text| content.contains(text))
                    })
                }
                "workspace-json-field-equals-v1" => {
                    let parsed = serde_json::from_slice::<Value>(bytes);
                    match (parsed.ok(), expected_text.as_deref()) {
                        (Some(value), Some(encoded)) => {
                            let mut pair = encoded.splitn(2, '\0');
                            let field = pair.next().unwrap_or_default();
                            let expected = pair.next().unwrap_or_default();
                            value.get(field).and_then(Value::as_str) == Some(expected)
                        }
                        _ => false,
                    }
                }
                _ => unreachable!("validator id was checked above"),
            };
            if !valid {
                failure = Some(format!(
                    "workspace validator {} 对文件 {} 未通过",
                    requirement.validator_id, raw
                ));
            }
        }
        match failure {
            Some(detail) => (ValidationVerdictV1::Failed, Some(detail), hashes),
            None => (ValidationVerdictV1::Passed, None, hashes),
        }
    }
}

pub fn workspace_validator_arguments_supported(validator_id: &str, arguments: &Value) -> bool {
    match validator_id {
        "workspace-file-exists-v1" | "workspace-file-non-empty-v1" => arguments
            .as_object()
            .is_some_and(|object| object.is_empty()),
        "workspace-file-contains-v1" => exact_string_argument(arguments, "text")
            .is_some_and(|text| !text.is_empty() && text.len() <= 2_048),
        "workspace-json-field-equals-v1" => arguments.as_object().is_some_and(|object| {
            object.len() == 2
                && object
                    .get("field")
                    .and_then(Value::as_str)
                    .is_some_and(|field| {
                        !field.is_empty() && field.len() <= 128 && !field.contains('.')
                    })
                && object
                    .get("expected")
                    .and_then(Value::as_str)
                    .is_some_and(|expected| expected.len() <= 1_024)
        }),
        "workspace-command-success-v1" => {
            exact_string_argument(arguments, "command").is_some_and(is_registered_behavior_command)
        }
        _ => false,
    }
}

pub fn is_registered_workspace_validator(validator_id: &str) -> bool {
    matches!(
        validator_id,
        "workspace-file-exists-v1"
            | "workspace-file-non-empty-v1"
            | "workspace-file-contains-v1"
            | "workspace-json-field-equals-v1"
            | "workspace-command-success-v1"
    )
}

/// Execute only the host-registered, read-only workspace validators. The scope is
/// path-bound and every file is canonicalized under the supplied workspace root.
/// Compatible one-requirement entry point. Production plans use one shared batch.
pub fn execute_workspace_requirement(
    requirement: &VerificationRequirementV1,
    workspace_root: &Path,
) -> (
    ValidationVerdictV1,
    Option<String>,
    BTreeMap<String, String>,
) {
    WorkspaceValidationBatch::new(Some(workspace_root)).execute_workspace(requirement)
}

#[cfg(test)]
#[path = "verification_workspace_tests.rs"]
mod tests;
