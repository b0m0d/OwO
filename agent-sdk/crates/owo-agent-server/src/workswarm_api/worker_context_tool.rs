//! Versioned shared-context tools available to authorized Team workers.
//!
//! Worker orchestration decides whether a task receives these tools; this module owns
//! their contracts, visibility/freshness checks, and CAS-backed context reads/writes.

use super::workspace_change_tracker;
use async_trait::async_trait;
use owo_agent_core::tool_effects::{EffectClass, ToolEffect};
use owo_agent_core::tools::{Tool, ToolContext, ToolSpec};
use owo_agent_core::workswarm::TeamCoordinator;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn team_context_fact_is_visible(
    fact: &owo_agent_protocol::SharedContextFact,
    task_id: Option<&str>,
    step_id: &str,
    refs: &[String],
) -> bool {
    fact.task_id.is_none()
        || fact.task_id.as_deref() == task_id
        || fact.task_id.as_deref() == Some(step_id)
        || fact.source_refs.iter().any(|source| refs.contains(source))
}

const MAX_CONTEXT_FACT_SOURCE_HASH_BYTES: u64 = 64 * 1024 * 1024;

fn hash_workspace_source(source: &std::path::Path, expected: &str, max_bytes: u64) -> &'static str {
    let Ok(metadata) = std::fs::metadata(source) else {
        return "stale";
    };
    if !metadata.is_file() {
        return "stale";
    }
    if metadata.len() > max_bytes {
        return "unverifiable";
    }
    let Ok(mut file) = std::fs::File::open(source) else {
        return "stale";
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total_bytes = 0_u64;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(_) => return "stale",
        };
        if read == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(read as u64);
        if total_bytes > max_bytes {
            return "unverifiable";
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("sha256:{:x}", hasher.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        "current"
    } else {
        "stale"
    }
}

pub(super) async fn team_context_fact_freshness(
    fact: &owo_agent_protocol::SharedContextFact,
    workspace_root: &std::path::Path,
) -> &'static str {
    let Some(expected) = fact.file_hash.as_deref() else {
        return "untracked";
    };
    if fact.source_refs.len() != 1 {
        return "unverifiable";
    }
    let relative = std::path::Path::new(&fact.source_refs[0]);
    if relative.is_absolute()
        || !relative
            .components()
            .any(|c| matches!(c, std::path::Component::Normal(_)))
        || relative.components().any(|c| {
            !matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return "stale";
    }
    let root = workspace_change_tracker::simplify_path(
        &workspace_root
            .canonicalize()
            .unwrap_or_else(|_| workspace_root.to_path_buf()),
    );
    let source = canonicalize_task_path(&root.join(relative));
    if !source.starts_with(&root) {
        return "stale";
    }
    let expected = expected.to_string();
    tokio::task::spawn_blocking(move || {
        hash_workspace_source(&source, &expected, MAX_CONTEXT_FACT_SOURCE_HASH_BYTES)
    })
    .await
    .unwrap_or("unverifiable")
}

fn take_utf8_bytes(value: &str, max_bytes: usize) -> String {
    value
        .char_indices()
        .take_while(|(offset, ch)| offset + ch.len_utf8() <= max_bytes)
        .map(|(_, ch)| ch)
        .collect()
}

pub(super) fn validate_context_publish_sources(
    source_refs: &[String],
    file_hash: Option<&str>,
    allowed_source_refs: &[String],
) -> Result<(), String> {
    if source_refs.len() > 8 {
        return Err("source_refs 最多允许 8 项".to_string());
    }
    let mut seen = HashSet::new();
    for source in source_refs {
        if source.trim().is_empty() || !seen.insert(source.as_str()) {
            return Err("source_refs 不能为空且不能重复".to_string());
        }
        if !allowed_source_refs.iter().any(|allowed| allowed == source) {
            return Err(format!(
                "共享事实来源不在宿主分配的 read_refs/contract_refs 中：{source}"
            ));
        }
    }
    if file_hash.is_some() {
        if source_refs.len() != 1 {
            return Err("带 file_hash 的共享事实必须绑定唯一工作区相对路径".to_string());
        }
        let path = std::path::Path::new(&source_refs[0]);
        let has_normal = path
            .components()
            .any(|component| matches!(component, std::path::Component::Normal(_)));
        if source_refs[0].contains("://")
            || path.is_absolute()
            || !has_normal
            || path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return Err("file_hash 来源必须是安全的工作区相对文件路径".to_string());
        }
    }
    Ok(())
}

pub(super) fn team_context_read_effect() -> ToolEffect {
    ToolEffect {
        tool: "team_context_read".to_string(),
        class: EffectClass::Read,
        source: "builtin".to_string(),
        risk_note: None,
        annotations: None,
        host_verified_readonly: true,
    }
}

pub(super) fn team_context_publish_effect() -> ToolEffect {
    ToolEffect {
        tool: "team_context_publish".into(),
        class: EffectClass::Write,
        source: "builtin".into(),
        risk_note: Some("发布团队共享事实，使用 revision CAS；保持 candidate/unverified".into()),
        annotations: None,
        host_verified_readonly: false,
    }
}
pub(super) struct TeamContextPublishTool {
    pub(super) coordinator: Arc<TeamCoordinator>,
    pub(super) team_id: String,
    pub(super) member_id: String,
    pub(super) task_id: String,
    pub(super) allowed_source_refs: Vec<String>,
}
#[async_trait]
impl Tool for TeamContextPublishTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            "team_context_publish",
            "使用 expected_revision/CAS 发布当前任务的候选事实；source_refs 必须属于宿主分配的 read_refs/contract_refs；带 file_hash 时只能绑定一个工作区相对文件路径。不会提升可信等级。".into(),
            json!({"type":"object","properties":{
                "key":{"type":"string","minLength":1,"maxLength":160},
                "value":{"type":"string","minLength":1,"maxLength":65536},
                "expected_revision":{"type":"integer","minimum":0},
                "source_refs":{"type":"array","items":{"type":"string"},"maxItems":8},
                "file_hash":{"type":"string","pattern":"^sha256:[0-9a-fA-F]{64}$"}},
                "required":["key","value","expected_revision"],"additionalProperties":false}),
            Some(team_context_publish_effect()),
        )
    }
    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| "key 必须是字符串".to_string())?
            .to_string();
        let value = args
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| "value 必须是字符串".to_string())?
            .to_string();
        let expected_revision = args
            .get("expected_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| "expected_revision 必须是非负整数".to_string())?;
        let source_refs = args
            .get("source_refs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "source_refs 只能包含字符串".to_string())
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let file_hash = args
            .get("file_hash")
            .and_then(Value::as_str)
            .map(str::to_string);
        validate_context_publish_sources(
            &source_refs,
            file_hash.as_deref(),
            &self.allowed_source_refs,
        )?;
        let fact = self
            .coordinator
            .publish_team_context_fact(
                &self.team_id,
                expected_revision,
                owo_agent_core::workswarm::SharedContextFactDraft {
                    key,
                    value,
                    producer: self.member_id.clone(),
                    task_id: Some(self.task_id.clone()),
                    source_refs,
                    file_hash,
                },
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            json!({"key":fact.key,"revision":fact.revision,"producer":fact.producer,
            "task_id":fact.task_id,"source_refs":fact.source_refs,"file_hash":fact.file_hash,
            "confidence":fact.confidence,"status":fact.status}),
        )
    }
}
pub(super) struct TeamContextReadTool {
    pub(super) coordinator: Arc<TeamCoordinator>,
    pub(super) team_id: String,
    pub(super) member_id: String,
    pub(super) step_id: String,
    pub(super) task_id: Option<String>,
    pub(super) refs: Vec<String>,
    pub(super) workspace_root: PathBuf,
}

#[async_trait]
impl Tool for TeamContextReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect(
            "team_context_read",
            "按需读取当前任务相关的版本化团队事实正文；返回 revision 和来源引用。".to_string(),
            json!({"type":"object","properties":{
                "key":{"type":"string"},
                "limit":{"type":"integer","minimum":1,"maximum":32}
            },"additionalProperties":false}),
            Some(team_context_read_effect()),
        )
    }

    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let key_filter = args.get("key").and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(12)
            .clamp(1, 32) as usize;
        let snapshot = self
            .coordinator
            .read_team_context(&self.team_id)
            .await
            .map_err(|error| error.to_string())?;
        let mut latest_keys = HashSet::new();
        let mut facts = Vec::new();
        let mut remaining_bytes = 16 * 1024usize;
        let mut context_revision = snapshot.revision;
        for fact in snapshot.facts.iter().rev() {
            if !latest_keys.insert(fact.key.as_str())
                || (fact.status != "candidate" && fact.status != "confirmed")
                || key_filter.is_some_and(|key| key != fact.key)
            {
                continue;
            }
            if !team_context_fact_is_visible(
                fact,
                self.task_id.as_deref(),
                &self.step_id,
                &self.refs,
            ) {
                continue;
            }
            if facts.len() >= limit || remaining_bytes == 0 {
                break;
            }
            let freshness = team_context_fact_freshness(fact, &self.workspace_root).await;
            if freshness == "stale" {
                if let Ok(stale) = self
                    .coordinator
                    .mark_team_context_fact_stale(
                        &self.team_id,
                        &fact.key,
                        fact.revision,
                        context_revision,
                    )
                    .await
                {
                    context_revision = stale.revision;
                } else if let Ok(current) = self.coordinator.read_team_context(&self.team_id).await
                {
                    context_revision = current.revision;
                }
                continue;
            }
            if fact.file_hash.is_some() && freshness != "current" {
                continue;
            }
            let hash = fact
                .value_ref
                .strip_prefix("cas://sha256:")
                .ok_or_else(|| "共享事实 CAS 引用格式无效".to_string())?;
            let full = self
                .coordinator
                .cas()
                .get_text(hash)
                .ok_or_else(|| format!("共享事实正文缺失：{}", fact.key))?;
            let value = take_utf8_bytes(&full, remaining_bytes);
            remaining_bytes = remaining_bytes.saturating_sub(value.len());
            facts.push(json!({
                "key":fact.key,"value":value,"revision":fact.revision,"producer":fact.producer,
                "task_id":fact.task_id,"source_refs":fact.source_refs,"file_hash":fact.file_hash,
                "confidence":fact.confidence,"status":fact.status,"freshness":freshness
            }));
        }
        Ok(json!({"team_id":self.team_id,"member_id":self.member_id,
            "step_id":self.step_id,"revision":context_revision,"facts":facts}))
    }
}

/// Canonicalize a potentially non-existing task path for read-only freshness checks.
fn canonicalize_task_path(path: &std::path::Path) -> PathBuf {
    let mut current = path.to_path_buf();
    let mut suffix = Vec::new();
    while !current.exists() {
        let Some(name) = current.file_name().map(std::ffi::OsString::from) else {
            break;
        };
        suffix.push(name);
        if !current.pop() {
            break;
        }
    }
    let mut canonical = current.canonicalize().unwrap_or(current);
    for part in suffix.iter().rev() {
        canonical.push(part);
    }
    workspace_change_tracker::simplify_path(&canonical)
}

#[cfg(test)]
mod freshness_hash_tests {
    use super::hash_workspace_source;
    use sha2::{Digest, Sha256};

    #[test]
    fn streaming_hash_verifies_content_and_returns_unverifiable_past_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.rs");
        std::fs::write(&path, b"four").unwrap();
        let expected = format!("sha256:{:x}", Sha256::digest(b"four"));
        assert_eq!(hash_workspace_source(&path, &expected, 4), "current");
        assert_eq!(hash_workspace_source(&path, &expected, 3), "unverifiable");
        assert_eq!(hash_workspace_source(&path, "sha256:wrong", 4), "stale");
    }
}

#[cfg(test)]
mod tests {
    use super::validate_context_publish_sources;

    #[test]
    fn context_fact_sources_must_be_host_assigned_and_file_hash_path_bound() {
        let allowed = vec!["src/api.rs".to_string(), "contract://api-v1".to_string()];
        assert!(
            validate_context_publish_sources(&["src/api.rs".to_string()], None, &allowed,).is_ok()
        );
        assert!(
            validate_context_publish_sources(&["docs/secret.md".to_string()], None, &allowed,)
                .is_err()
        );
        assert!(validate_context_publish_sources(
            &["src/api.rs".to_string()],
            Some(&format!("sha256:{}", "a".repeat(64))),
            &allowed,
        )
        .is_ok());
        assert!(validate_context_publish_sources(
            &["contract://api-v1".to_string()],
            Some(&format!("sha256:{}", "a".repeat(64))),
            &allowed,
        )
        .is_err());
        assert!(validate_context_publish_sources(
            &["../outside.rs".to_string()],
            Some(&format!("sha256:{}", "a".repeat(64))),
            &["../outside.rs".to_string()],
        )
        .is_err());
        assert!(validate_context_publish_sources(
            &["src/api.rs".to_string(), "src/api.rs".to_string(),],
            None,
            &allowed,
        )
        .is_err());
    }
}
