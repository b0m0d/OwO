//! Team artifact tool schema and argument contract; Core owns authorization and I/O.
use async_trait::async_trait;
use owo_agent_core::tool_effects::{EffectClass, ToolEffect};
use owo_agent_core::tools::{Tool, ToolContext, ToolSpec};
use owo_agent_core::workswarm::{DependencyArtifactRead, TeamCoordinator};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn team_artifact_read_effect() -> ToolEffect {
    ToolEffect {
        tool: "team_artifact_read".into(),
        class: EffectClass::Read,
        source: "builtin".into(),
        risk_note: None,
        annotations: None,
        host_verified_readonly: true,
    }
}
pub(super) struct TeamArtifactReadTool {
    pub(super) coordinator: Arc<TeamCoordinator>,
    pub(super) team_id: String,
    pub(super) member_id: String,
    pub(super) step_id: String,
}

fn parse_request(args: &Value) -> Result<(&str, DependencyArtifactRead), String> {
    let object = args
        .as_object()
        .ok_or_else(|| "参数必须是对象".to_string())?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "artifact_id" | "max_bytes" | "offset_bytes" | "expected_sha256"
        )
    }) {
        return Err("存在未支持的产物读取参数".into());
    }
    let id = args
        .get("artifact_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty() && id.chars().count() <= 256)
        .ok_or_else(|| "artifact_id 必须是 1..=256 字符的非空字符串".to_string())?;
    let max = match args.get("max_bytes") {
        None => 16 * 1024,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=65536).contains(n))
            .ok_or_else(|| "max_bytes 必须是 1..=65536 的整数".to_string())?
            as usize,
    };
    let offset = match args.get("offset_bytes") {
        None => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "offset_bytes 必须是非负整数".to_string())?,
    };
    let expected = match args.get("expected_sha256") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .filter(|hash| {
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                .ok_or_else(|| "expected_sha256 必须是 64 位小写十六进制哈希".to_string())?
                .to_string(),
        ),
    };
    if offset > 0 && expected.is_none() {
        return Err("续读必须提供上一页的 sha256 作为 expected_sha256".into());
    }
    Ok((
        id,
        DependencyArtifactRead {
            offset_bytes: offset,
            max_bytes: max,
            expected_sha256: expected,
        },
    ))
}

#[async_trait]
impl Tool for TeamArtifactReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::with_effect("team_artifact_read",
            "分页读取当前步骤直接依赖的产物正文。首次从 offset_bytes=0 开始；eof=false 时使用返回的 next_offset_bytes 与 sha256 续读；不要重复读取第一页。".into(),
            json!({"type":"object","properties":{
                "artifact_id":{"type":"string","minLength":1,"maxLength":256},
                "max_bytes":{"type":"integer","minimum":1,"maximum":65536},
                "offset_bytes":{"type":"integer","minimum":0},
                "expected_sha256":{"type":"string","pattern":"^[a-f0-9]{64}$"}
            },"required":["artifact_id"],"additionalProperties":false}),
            Some(team_artifact_read_effect()))
    }
    async fn run(&self, _ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let (id, request) = parse_request(&args)?;
        self.coordinator
            .read_dependency_artifact_page(
                &self.team_id,
                &self.member_id,
                &self.step_id,
                id,
                &request,
            )
            .await
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_read_defaults_to_first_page_and_original_budget() {
        let (_, request) = parse_request(&json!({"artifact_id":"a"})).unwrap();
        assert_eq!(request.offset_bytes, 0);
        assert_eq!(request.max_bytes, 16 * 1024);
        assert!(request.expected_sha256.is_none());
    }
    #[test]
    fn cursor_requires_explicit_hash_and_rejects_malformed_parameters() {
        assert!(parse_request(&json!({"artifact_id":"a","offset_bytes":1})).is_err());
        for value in [json!(0), json!(-1), json!("5"), json!(65537), Value::Null] {
            assert!(parse_request(&json!({"artifact_id":"a","max_bytes":value})).is_err());
        }
        assert!(parse_request(&json!({"artifact_id":"a","offset_bytes":-1})).is_err());
        assert!(parse_request(&json!({"artifact_id":"a","expected_sha256":"../secret"})).is_err());
        assert!(parse_request(&json!({"artifact_id":"a","extra":true})).is_err());
    }
    #[test]
    fn valid_cursor_and_hash_are_passed_to_core_without_clamping() {
        let hash = "a".repeat(64);
        let (_, request) = parse_request(&json!({"artifact_id":"a","offset_bytes":70000,
            "max_bytes":4096,"expected_sha256":hash}))
        .unwrap();
        assert_eq!(request.offset_bytes, 70000);
        assert_eq!(request.max_bytes, 4096);
        assert_eq!(request.expected_sha256.as_deref(), Some(hash.as_str()));
    }
}
