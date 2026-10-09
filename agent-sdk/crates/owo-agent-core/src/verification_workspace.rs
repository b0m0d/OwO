//! One verification operation shares bounded, immutable file observations.
//! A fresh WorkspaceSnapshotBatch must fence receipts before acceptance.
use super::{exact_string_argument, is_registered_behavior_command};
use crate::plan::{ValidationVerdictV1, VerificationRequirementV1, VerificationScopeV1};
use crate::workspace_snapshot::{
    workspace_relative_key, MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES, MAX_SNAPSHOT_PATHS,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Shared path bounds used by the provider schema, plan gate, and workspace executor.
pub const MIN_WORKSPACE_VALIDATION_PATHS: usize = 1;
pub const MAX_WORKSPACE_VALIDATION_PATHS: usize = 16;

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
                if (MIN_WORKSPACE_VALIDATION_PATHS..=MAX_WORKSPACE_VALIDATION_PATHS)
                    .contains(&relative_paths.len()) =>
            {
                relative_paths
            }
            _ => {
                return unsupported(format!(
                    "workspace validator 要求 {}..={} 个 WorkspacePaths",
                    MIN_WORKSPACE_VALIDATION_PATHS, MAX_WORKSPACE_VALIDATION_PATHS
                ));
            }
        };
        if self.root.is_none() {
            return (
                ValidationVerdictV1::Unverified,
                Some("workspace 根目录不可用，验证未执行".to_string()),
                BTreeMap::new(),
            );
        }
        if let Err(error) = validate_workspace_validator_arguments(
            &requirement.validator_id,
            &requirement.arguments,
        ) {
            return unsupported(error);
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
                );
            }
            unknown => {
                return unsupported(format!(
                    "workspace validator「{unknown}」未注册，当前为 unsupported/unverified"
                ));
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceValidatorArgumentKind {
    Empty,
    Text,
    JsonFieldEquals,
    RegisteredCommand,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceValidatorContract {
    pub validator_id: &'static str,
    pub arguments: WorkspaceValidatorArgumentKind,
}
impl WorkspaceValidatorContract {
    pub fn arguments_schema(self) -> Value {
        match self.arguments {
            WorkspaceValidatorArgumentKind::Empty => {
                json!({"type":"object","properties":{},"additionalProperties":false})
            }
            WorkspaceValidatorArgumentKind::Text => {
                json!({"type":"object","properties":{"text":{"type":"string","minLength":1,"maxLength":2048,"description":"文件中必须出现的精确文本"}},"required":["text"],"additionalProperties":false})
            }
            WorkspaceValidatorArgumentKind::JsonFieldEquals => {
                json!({"type":"object","properties":{"field":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[^.]+$","description":"JSON 顶层字段名"},"expected":{"type":"string","maxLength":1024,"description":"该字段应有的字符串值"}},"required":["field","expected"],"additionalProperties":false})
            }
            WorkspaceValidatorArgumentKind::RegisteredCommand => {
                json!({"type":"object","properties":{"command":{"type":"string","minLength":1,"maxLength":512,"description":"宿主允许的行为检查命令，如 cargo test、npm test 或 python -m pytest；禁止 shell 链接和绕过测试的参数"}},"required":["command"],"additionalProperties":false})
            }
        }
    }
}
pub fn workspace_validator_contracts() -> &'static [WorkspaceValidatorContract] {
    const CONTRACTS: &[WorkspaceValidatorContract] = &[
        WorkspaceValidatorContract {
            validator_id: "workspace-file-exists-v1",
            arguments: WorkspaceValidatorArgumentKind::Empty,
        },
        WorkspaceValidatorContract {
            validator_id: "workspace-file-non-empty-v1",
            arguments: WorkspaceValidatorArgumentKind::Empty,
        },
        WorkspaceValidatorContract {
            validator_id: "workspace-file-contains-v1",
            arguments: WorkspaceValidatorArgumentKind::Text,
        },
        WorkspaceValidatorContract {
            validator_id: "workspace-json-field-equals-v1",
            arguments: WorkspaceValidatorArgumentKind::JsonFieldEquals,
        },
        WorkspaceValidatorContract {
            validator_id: "workspace-command-success-v1",
            arguments: WorkspaceValidatorArgumentKind::RegisteredCommand,
        },
    ];
    CONTRACTS
}
pub fn validate_workspace_validator_arguments(
    validator_id: &str,
    arguments: &Value,
) -> Result<(), String> {
    let contract = workspace_validator_contracts()
        .iter()
        .find(|item| item.validator_id == validator_id)
        .ok_or_else(|| format!("validator {validator_id} is not registered"))?;
    crate::json_schema::validate(arguments, &contract.arguments_schema(), "arguments")
        .map_err(|error| format!("validator {validator_id}: {error}"))?;
    // A JSON Schema describes the command shape; the host registry controls execution.
    if contract.arguments == WorkspaceValidatorArgumentKind::RegisteredCommand
        && !exact_string_argument(arguments, "command").is_some_and(is_registered_behavior_command)
    {
        return Err(format!(
            "validator {validator_id}: command is not in the host behavior-command registry"
        ));
    }
    Ok(())
}

pub fn workspace_validator_arguments_supported(validator_id: &str, arguments: &Value) -> bool {
    validate_workspace_validator_arguments(validator_id, arguments).is_ok()
}
pub fn is_registered_workspace_validator(validator_id: &str) -> bool {
    workspace_validator_contracts()
        .iter()
        .any(|item| item.validator_id == validator_id)
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
