use crate::plan::{
    verify_output, ValidationVerdictV1, VerificationPlanV1, VerificationRequirementV1,
    VerificationResourcesV1, VerificationScopeV1, VerificationSpec,
};
use serde_json::Value;

/// Domain-separated digest used in receipt subject_sha256 when a changed workspace path is absent.
pub(crate) fn workspace_path_absence_sha256() -> String {
    crate::CasStore::hash_of(b"owo-agent:workspace-path-absence:v1")
}

/// Recheck every workspace file hash carried by a host validation receipt.
/// A receipt with no workspace subjects is independent of the workspace snapshot.
pub(crate) fn workspace_subjects_match_current(
    workspace_root: &Path,
    subjects: &std::collections::HashMap<String, String>,
) -> bool {
    let workspace_subjects = subjects
        .iter()
        .filter_map(|(subject, expected)| {
            subject
                .strip_prefix("workspace-path:")
                .map(|relative| (relative, expected))
        })
        .collect::<Vec<_>>();
    if workspace_subjects.is_empty() {
        return true;
    }
    let Ok(root) = workspace_root.canonicalize() else {
        return false;
    };
    if !root.is_dir() {
        return false;
    }

    for (raw, expected) in workspace_subjects {
        let relative = Path::new(raw);
        if raw.trim().is_empty()
            || raw.len() > 512
            || raw.contains('\0')
            || relative.is_absolute()
            || relative.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::Prefix(_) | Component::RootDir
                )
            })
        {
            return false;
        }
        let target = root.join(relative);
        match std::fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if expected != &workspace_path_absence_sha256() {
                    return false;
                }
                let mut ancestor = target.as_path();
                let mut contained = false;
                loop {
                    if let Ok(canonical) = ancestor.canonicalize() {
                        contained = canonical.starts_with(&root);
                        break;
                    }
                    let Some(parent) = ancestor.parent() else {
                        break;
                    };
                    ancestor = parent;
                }
                if !contained {
                    return false;
                }
            }
            Err(_) => return false,
            Ok(_) => {
                let Ok(canonical) = target.canonicalize() else {
                    return false;
                };
                if !canonical.starts_with(&root) {
                    return false;
                }
                let Ok(metadata) = std::fs::metadata(&canonical) else {
                    return false;
                };
                if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
                    return false;
                }
                let Ok(bytes) = std::fs::read(&canonical) else {
                    return false;
                };
                if bytes.len() as u64 != metadata.len()
                    || format!("{:x}", Sha256::digest(&bytes)) != *expected
                    || expected == &workspace_path_absence_sha256()
                {
                    return false;
                }
            }
        }
    }
    true
}
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path};

/// Compile the legacy output assertion into the same host-owned validator contract
/// used by Team delivery checks. Legacy names never create executable validators.
pub fn requirement_for_spec(
    requirement_id: &str,
    spec: &VerificationSpec,
) -> VerificationRequirementV1 {
    let (validator_id, arguments) = match spec {
        VerificationSpec::OutputNonEmpty => ("artifact-output-non-empty-v1", serde_json::json!({})),
        VerificationSpec::OutputContains(value) => (
            "artifact-output-contains-v1",
            serde_json::json!({"value": value}),
        ),
        VerificationSpec::OutputEquals(value) => (
            "artifact-output-equals-v1",
            serde_json::json!({"value": value}),
        ),
        VerificationSpec::Custom(name) => (name.as_str(), serde_json::json!({})),
    };
    VerificationRequirementV1 {
        requirement_id: requirement_id.to_string(),
        covers_requirement_ids: Vec::new(),
        validator_id: validator_id.to_string(),
        validator_version: Some("1".to_string()),
        scope: VerificationScopeV1::StepOutput,
        arguments,
        required: true,
        resources: VerificationResourcesV1 {
            cpu_slots: 1,
            memory_mb: 1,
            exclusive_workspace: false,
            timeout_ms: 5_000,
        },
    }
}

/// Turn compatibility assertions into an explicit plan. Empty input remains an
/// absent plan at the caller; this helper always creates one obligation per item.
pub fn plan_for_specs(plan_id: &str, specs: &[VerificationSpec]) -> VerificationPlanV1 {
    VerificationPlanV1 {
        plan_id: plan_id.to_string(),
        requirements: specs
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                requirement_for_spec(&format!("{plan_id}:requirement:{index}"), spec)
            })
            .collect(),
    }
}

/// Execute a registered text-output validator. Unknown IDs, versions, scopes, or
/// malformed arguments remain Unsupported and cannot pass as non-empty output.
pub fn execute_requirement(
    requirement: &VerificationRequirementV1,
    content: &str,
) -> (ValidationVerdictV1, Option<String>) {
    if requirement.validator_version.as_deref() != Some("1") {
        return (
            ValidationVerdictV1::Unsupported,
            Some(format!(
                "validator {} 版本未注册，当前为 unsupported/unverified",
                requirement.validator_id
            )),
        );
    }
    if requirement.scope != VerificationScopeV1::StepOutput {
        return (
            ValidationVerdictV1::Unsupported,
            Some(format!(
                "validator {} 的 scope 未注册，当前为 unsupported/unverified",
                requirement.validator_id
            )),
        );
    }
    let spec = match requirement.validator_id.as_str() {
        "artifact-output-non-empty-v1" => {
            if requirement
                .arguments
                .as_object()
                .is_some_and(|arguments| arguments.is_empty())
            {
                VerificationSpec::OutputNonEmpty
            } else {
                return (
                    ValidationVerdictV1::Unsupported,
                    Some("artifact-output-non-empty-v1 不接受参数".to_string()),
                );
            }
        }
        "artifact-output-contains-v1" => {
            match exact_string_argument(&requirement.arguments, "value") {
                Some(value) => VerificationSpec::OutputContains(value.to_string()),
                None => {
                    return (
                        ValidationVerdictV1::Unsupported,
                        Some(
                            "artifact-output-contains-v1 要求且仅接受字符串参数 value".to_string(),
                        ),
                    )
                }
            }
        }
        "artifact-output-equals-v1" => {
            match exact_string_argument(&requirement.arguments, "value") {
                Some(value) => VerificationSpec::OutputEquals(value.to_string()),
                None => {
                    return (
                        ValidationVerdictV1::Unsupported,
                        Some("artifact-output-equals-v1 要求且仅接受字符串参数 value".to_string()),
                    )
                }
            }
        }
        unknown => {
            return (
                ValidationVerdictV1::Unsupported,
                Some(format!(
                    "validator「{unknown}」未注册，当前为 unsupported/unverified"
                )),
            )
        }
    };
    match verify_output(&spec, content) {
        Ok(()) => (ValidationVerdictV1::Passed, None),
        Err(detail) => (ValidationVerdictV1::Failed, Some(detail)),
    }
}

/// Dispatch a requirement through the single host-owned validator registry used by
/// GoalRunner and WorkSwarm. Dynamic model output can select only registered
/// validators; it never supplies an executable command.
pub fn execute_registered_requirement(
    requirement: &VerificationRequirementV1,
    content: &str,
    workspace_root: Option<&Path>,
) -> (
    ValidationVerdictV1,
    Option<String>,
    BTreeMap<String, String>,
) {
    match &requirement.scope {
        VerificationScopeV1::StepOutput => {
            let (verdict, detail) = execute_requirement(requirement, content);
            (verdict, detail, BTreeMap::new())
        }
        VerificationScopeV1::WorkspacePaths { .. } => match workspace_root {
            Some(root) => execute_workspace_requirement(requirement, root),
            None => (
                ValidationVerdictV1::Unverified,
                Some("未绑定宿主工作区，WorkspacePaths 验证未执行".to_string()),
                BTreeMap::new(),
            ),
        },
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
        "workspace-command-success-v1" => exact_string_argument(arguments, "command")
            .is_some_and(is_registered_behavior_command),
        _ => false,
    }
}

pub fn is_registered_behavior_command(command: &str) -> bool {
    let command = command.trim();
    if command.is_empty()
        || command.len() > 512
        || !command.is_ascii()
        || command.chars().any(|ch| {
            matches!(
                ch as u32,
                38 | 124 | 60 | 62 | 94 | 37 | 40 | 41 | 59 | 96 | 34 | 39 | 33 | 10 | 13
            )
        })
    {
        return false;
    }
    let tokens: Vec<_> = command.split_whitespace().collect();
    let bypass_flags = [
        "--no-run",
        "--list",
        "--help",
        "-h",
        "/?",
        "/help",
        "--dry-run",
        "--collect-only",
        "--co",
        "--if-present",
        "--passwithnotests",
        "--skip",
        "--exclude-task",
        "-x",
        "-dskiptests",
        "-dmaven.test.skip=true",
    ];
    if tokens.iter().any(|token| {
        let normalized = token.to_ascii_lowercase();
        bypass_flags.contains(&normalized.as_str())
            || normalized.starts_with("--no-run=")
            || normalized.starts_with("--collect-only=")
            || normalized.starts_with("-dmaven.test.skip=")
            || normalized == "--"
                && tokens.iter().any(|candidate| {
                    matches!(
                        candidate.to_ascii_lowercase().as_str(),
                        "--list" | "--help" | "-h" | "--collect-only" | "--co"
                    )
                })
    }) {
        return false;
    }
    match tokens.as_slice() {
        ["cargo", "test", ..] => true,
        ["npm", "test"] | ["npm", "run", "test", ..] => true,
        ["pnpm", "test", ..] | ["yarn", "test", ..] | ["bun", "test", ..] => true,
        ["pytest", ..] | ["python", "-m", "pytest", ..] => true,
        ["dotnet", "test", ..] | ["go", "test", ..] | ["mvn", "test", ..] => true,
        ["gradle", "test", ..] | ["ctest", ..] => true,
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
pub fn execute_workspace_requirement(
    requirement: &VerificationRequirementV1,
    workspace_root: &Path,
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
        _ => return unsupported("workspace validator 要求 1..=16 个 WorkspacePaths".to_string()),
    };
    let root = match workspace_root.canonicalize() {
        Ok(root) if root.is_dir() => root,
        _ => {
            return (
                ValidationVerdictV1::Unverified,
                Some("workspace 根目录不可用，验证未执行".to_string()),
                BTreeMap::new(),
            )
        }
    };
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
                    "workspace-json-field-equals-v1 只接受 field 与 expected 两个参数".to_string(),
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
        let relative = Path::new(raw);
        if raw.trim().is_empty()
            || raw.len() > 512
            || raw.contains('\0')
            || relative.is_absolute()
            || relative.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::Prefix(_) | Component::RootDir
                )
            })
        {
            return unsupported(format!("workspace scope 路径非法：{raw}"));
        }
        let target = root.join(relative);
        let canonical = match target.canonicalize() {
            Ok(path) if path.starts_with(&root) => path,
            Ok(_) => return unsupported(format!("workspace 路径越界：{raw}")),
            Err(error) => {
                failure = Some(format!("workspace 文件不可读取 {raw}：{error}"));
                continue;
            }
        };
        let metadata = match std::fs::metadata(&canonical) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= 8 * 1024 * 1024 => metadata,
            Ok(metadata) if metadata.len() > 8 * 1024 * 1024 => {
                return unsupported(format!("workspace 文件超过 8 MiB 验证上限：{raw}"))
            }
            Ok(_) => {
                failure = Some(format!("workspace 路径不是普通文件：{raw}"));
                continue;
            }
            Err(error) => {
                failure = Some(format!("workspace 文件元数据不可读取 {raw}：{error}"));
                continue;
            }
        };
        let bytes = match std::fs::read(&canonical) {
            Ok(bytes) if bytes.len() as u64 == metadata.len() => bytes,
            Ok(_) => {
                failure = Some(format!("workspace 文件在读取期间发生变化：{raw}"));
                continue;
            }
            Err(error) => {
                failure = Some(format!("workspace 文件读取失败 {raw}：{error}"));
                continue;
            }
        };
        if started.elapsed().as_millis() > u128::from(requirement.resources.timeout_ms) {
            return (
                ValidationVerdictV1::Unverified,
                Some("workspace validator 超过声明的执行时间预算".to_string()),
                hashes,
            );
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        hashes.insert(format!("workspace-path:{raw}"), digest);
        let valid = match requirement.validator_id.as_str() {
            "workspace-file-exists-v1" => true,
            "workspace-file-non-empty-v1" => !bytes.is_empty(),
            "workspace-file-contains-v1" => {
                std::str::from_utf8(&bytes).ok().is_some_and(|content| {
                    expected_text
                        .as_deref()
                        .is_some_and(|text| content.contains(text))
                })
            }
            "workspace-json-field-equals-v1" => {
                let parsed = serde_json::from_slice::<Value>(&bytes);
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

fn exact_string_argument<'a>(arguments: &'a Value, key: &str) -> Option<&'a str> {
    let object = arguments.as_object()?;
    if object.len() != 1 {
        return None;
    }
    object.get(key)?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_assertion_compiles_to_registered_requirement() {
        let requirement = requirement_for_spec(
            "step-1",
            &VerificationSpec::OutputContains("ready".to_string()),
        );
        assert_eq!(requirement.requirement_id, "step-1");
        assert_eq!(requirement.validator_id, "artifact-output-contains-v1");
        assert_eq!(requirement.validator_version.as_deref(), Some("1"));
        assert_eq!(
            execute_requirement(&requirement, "service ready").0,
            ValidationVerdictV1::Passed
        );
        assert_eq!(
            execute_requirement(&requirement, "service pending"),
            (
                ValidationVerdictV1::Failed,
                Some("验证失败：输出缺少「ready」（实际：service pending）".to_string())
            )
        );
    }

    #[test]
    fn shared_dispatch_fails_closed_for_unbound_or_unsupported_scopes() {
        let workspace_requirement = VerificationRequirementV1 {
            requirement_id: "task:workspace".to_string(),
            covers_requirement_ids: Vec::new(),
            validator_id: "workspace-file-exists-v1".to_string(),
            validator_version: Some("1".to_string()),
            scope: VerificationScopeV1::WorkspacePaths {
                relative_paths: vec!["README.md".to_string()],
            },
            arguments: serde_json::json!({}),
            required: true,
            resources: VerificationResourcesV1 {
                cpu_slots: 1,
                memory_mb: 8,
                exclusive_workspace: false,
                timeout_ms: 1_000,
            },
        };
        let (verdict, detail, subjects) =
            execute_registered_requirement(&workspace_requirement, "output", None);
        assert_eq!(verdict, ValidationVerdictV1::Unverified);
        assert!(detail.unwrap().contains("未绑定宿主工作区"));
        assert!(subjects.is_empty());

        let artifact_scope_requirement = VerificationRequirementV1 {
            scope: VerificationScopeV1::ArtifactRefs {
                artifact_ids: vec!["artifact-1".to_string()],
            },
            ..workspace_requirement
        };
        let (verdict, detail, subjects) =
            execute_registered_requirement(&artifact_scope_requirement, "output", None);
        assert_eq!(verdict, ValidationVerdictV1::Unsupported);
        assert!(detail.unwrap().contains("scope 未注册"));
        assert!(subjects.is_empty());
    }

    #[test]
    fn workspace_validators_are_read_only_path_scoped_and_hash_bound() {
        let root = std::env::temp_dir().join(format!(
            "owo-workspace-verification-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/app.js");
        std::fs::write(&file, "export const state = 'accepted';").unwrap();
        let make_requirement =
            |validator_id: &str, arguments: Value, path: &str| VerificationRequirementV1 {
                requirement_id: "task-verify".to_string(),
                covers_requirement_ids: Vec::new(),
                validator_id: validator_id.to_string(),
                validator_version: Some("1".to_string()),
                scope: VerificationScopeV1::WorkspacePaths {
                    relative_paths: vec![path.to_string()],
                },
                arguments,
                required: true,
                resources: VerificationResourcesV1 {
                    cpu_slots: 1,
                    memory_mb: 8,
                    exclusive_workspace: false,
                    timeout_ms: 1_000,
                },
            };
        let contains = make_requirement(
            "workspace-file-contains-v1",
            serde_json::json!({"text":"accepted"}),
            "src/app.js",
        );
        let (verdict, detail, hashes) = execute_workspace_requirement(&contains, &root);
        assert_eq!(verdict, ValidationVerdictV1::Passed, "{detail:?}");
        let initial_hash = format!("{:x}", Sha256::digest(b"export const state = 'accepted';"));
        assert_eq!(
            hashes.get("workspace-path:src/app.js"),
            Some(&initial_hash),
            "receipt subject must bind the exact checked file version"
        );

        let json_file = root.join("src/state.json");
        std::fs::write(&json_file, r#"{"status":"ready"}"#).unwrap();
        let json_requirement = make_requirement(
            "workspace-json-field-equals-v1",
            serde_json::json!({"field":"status","expected":"ready"}),
            "src/state.json",
        );
        assert_eq!(
            execute_workspace_requirement(&json_requirement, &root).0,
            ValidationVerdictV1::Passed
        );

        std::fs::write(&file, "export const state = 'pending';").unwrap();
        let (verdict, _, hashes) = execute_workspace_requirement(&contains, &root);
        assert_eq!(verdict, ValidationVerdictV1::Failed);
        assert_ne!(hashes.get("workspace-path:src/app.js"), Some(&initial_hash));

        let traversal = make_requirement(
            "workspace-file-exists-v1",
            serde_json::json!({}),
            "../outside.txt",
        );
        assert_eq!(
            execute_workspace_requirement(&traversal, &root).0,
            ValidationVerdictV1::Unsupported
        );
        let unknown = make_requirement("shell-command-v1", serde_json::json!({}), "src/app.js");
        assert_eq!(
            execute_workspace_requirement(&unknown, &root).0,
            ValidationVerdictV1::Unsupported
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_validator_version_scope_and_arguments_fail_closed() {
        let mut requirement = requirement_for_spec("step-1", &VerificationSpec::OutputNonEmpty);
        requirement.validator_id = "custom-check".to_string();
        assert_eq!(
            execute_requirement(&requirement, "non-empty").0,
            ValidationVerdictV1::Unsupported
        );

        requirement.validator_id = "artifact-output-non-empty-v1".to_string();
        requirement.validator_version = Some("2".to_string());
        assert_eq!(
            execute_requirement(&requirement, "non-empty").0,
            ValidationVerdictV1::Unsupported
        );

        requirement.validator_version = Some("1".to_string());
        requirement.scope = VerificationScopeV1::Manual;
        assert_eq!(
            execute_requirement(&requirement, "non-empty").0,
            ValidationVerdictV1::Unsupported
        );

        requirement.scope = VerificationScopeV1::StepOutput;
        requirement.validator_id = "artifact-output-contains-v1".to_string();
        assert_eq!(
            execute_requirement(&requirement, "non-empty").0,
            ValidationVerdictV1::Unsupported
        );
        requirement.arguments = serde_json::json!({"value": "ok", "ignored": true});
        assert_eq!(
            execute_requirement(&requirement, "ok").0,
            ValidationVerdictV1::Unsupported
        );

        requirement.validator_id = "artifact-output-non-empty-v1".to_string();
        requirement.arguments = serde_json::json!({"ignored": true});
        assert_eq!(
            execute_requirement(&requirement, "non-empty").0,
            ValidationVerdictV1::Unsupported
        );
    }

    #[test]
    fn behavior_commands_are_registered_test_runners_without_shell_chaining() {
        assert!(is_registered_behavior_command("cargo test -p owo-agent-core"));
        assert!(is_registered_behavior_command("npm test"));
        assert!(is_registered_behavior_command("python -m pytest tests/test_api.py"));
        assert!(!is_registered_behavior_command("echo passed"));
        assert!(!is_registered_behavior_command("cargo test --no-run"));
        assert!(!is_registered_behavior_command("cargo test -- --list"));
        assert!(!is_registered_behavior_command("npm test --if-present"));
        assert!(!is_registered_behavior_command("pytest --collect-only"));
        assert!(!is_registered_behavior_command("npm test && echo passed"));
        assert!(!is_registered_behavior_command("cargo check"));
    }


    #[test]
    fn workspace_subject_snapshot_recheck_detects_mutation_and_escape() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        let path = root.path().join("src/lib.rs");
        std::fs::write(&path, "pub fn ready() {}\n").unwrap();
        let subjects = std::collections::HashMap::from([(
            "workspace-path:src/lib.rs".to_string(),
            crate::CasStore::hash_of(b"pub fn ready() {}\n"),
        )]);
        assert!(workspace_subjects_match_current(root.path(), &subjects));

        std::fs::write(&path, "pub fn broken() {}\n").unwrap();
        assert!(!workspace_subjects_match_current(root.path(), &subjects));

        std::fs::remove_file(&path).unwrap();
        let absent = std::collections::HashMap::from([(
            "workspace-path:src/lib.rs".to_string(),
            workspace_path_absence_sha256(),
        )]);
        assert!(workspace_subjects_match_current(root.path(), &absent));
        std::fs::write(&path, "recreated").unwrap();
        assert!(!workspace_subjects_match_current(root.path(), &absent));

        let escaped = std::collections::HashMap::from([(
            "workspace-path:../outside".to_string(),
            "any-hash".to_string(),
        )]);
        assert!(!workspace_subjects_match_current(root.path(), &escaped));
    }

}
