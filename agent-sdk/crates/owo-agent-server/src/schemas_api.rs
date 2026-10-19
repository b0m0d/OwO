//! JSON Schema 版本化发布 API（§12：从 lib.rs 机械外移的 R10 契约治理域）。
//!
//! 路由面（`GET /schemas`、`GET /schemas/{kind}/{version}`）与 /openapi.json
//! 登记保持不变，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::OWO_API_VERSION`。

use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

/// /schemas 列表（R10：JSON Schema 版本化发布索引）。
pub(super) async fn schemas_list() -> Json<Value> {
    Json(json!({
        "api_version": owo_agent_server::OWO_API_VERSION,
        "schemas": {
            "plugin-manifest": ["v1"],
            "owskill": ["v1"],
            "owflow": ["v1"],
        },
        "note": "GET /schemas/{kind}/{version} 获取 JSON Schema（draft-07）",
    }))
}

const SCHEMA_PLUGIN_MANIFEST_V1: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://owo.local/schemas/plugin-manifest/v1",
  "title": "OwO Plugin Manifest",
  "type": "object",
  "required": ["id", "name", "version"],
  "properties": {
    "id": { "type": "string", "minLength": 1 },
    "name": { "type": "string", "minLength": 1 },
    "version": { "type": "string", "pattern": "^[0-9]+\\.[0-9]+\\.[0-9]+" },
    "description": { "type": "string" },
    "permissions": { "type": "array", "items": { "type": "string" } },
    "mcp": { "type": "object" },
    "min_app_version": { "type": "string" },
    "entry": { "type": "string" },
    "network_allowlist": { "type": "array", "items": { "type": "string" } },
    "signature": { "type": "string" }
  },
  "additionalProperties": false
}"#;

const SCHEMA_OWSKILL_V1: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://owo.local/schemas/owskill/v1",
  "title": "OwO Flow Skill Package (.owskill)",
  "type": "object",
  "required": ["manifest", "graph", "skill_md"],
  "properties": {
    "manifest": {
      "type": "object",
      "required": ["id", "name", "version", "min_app_version", "target_apps", "sensitivity"],
      "properties": {
        "id": { "type": "string", "minLength": 1 },
        "name": { "type": "string", "minLength": 1 },
        "version": { "type": "string", "pattern": "^[0-9]+\\.[0-9]+\\.[0-9]+" },
        "min_app_version": { "type": "string" },
        "target_apps": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
        "permissions": { "type": "array", "items": { "type": "string" } },
        "variables": { "type": "array", "items": { "type": "string" } },
        "sensitivity": { "enum": ["none", "low", "medium", "high"] }
      },
      "additionalProperties": false
    },
    "graph": { "type": "object" },
    "skill_md": { "type": "string" }
  },
  "additionalProperties": false
}"#;

const SCHEMA_OWFLOW_V1: &str = r#"{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "$id": "https://owo.local/schemas/owflow/v1",
  "title": "OwO Workflow Definition (.owflow)",
  "type": "object",
  "required": ["id", "name"],
  "properties": {
    "id": { "type": "string", "minLength": 1 },
    "name": { "type": "string", "minLength": 1 },
    "version": { "type": "integer", "minimum": 1 },
    "triggers": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["id", "kind"],
        "properties": {
          "id": { "type": "string" },
          "kind": { "type": "object" }
        }
      }
    },
    "permissions": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["scope", "mode"],
        "properties": {
          "scope": { "type": "string" },
          "mode": { "enum": ["allow", "ask", "deny"] }
        }
      }
    },
    "preconditions": { "type": "array", "items": { "type": "string" } },
    "rollback_points": { "type": "array", "items": { "type": "string" } },
    "max_steps": { "type": "integer", "minimum": 1 },
    "subflow_depth_limit": { "type": "integer", "minimum": 1 },
    "steps": { "type": "array", "items": { "type": "object" } }
  },
  "additionalProperties": false
}"#;

/// GET /schemas/{kind}/{version}：静态 JSON Schema（版本化发布）。
pub(super) async fn schema_get(
    AxumPath((kind, version)): AxumPath<(String, String)>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let raw = match (kind.as_str(), version.as_str()) {
        ("plugin-manifest", "v1") => SCHEMA_PLUGIN_MANIFEST_V1,
        ("owskill", "v1") => SCHEMA_OWSKILL_V1,
        ("owflow", "v1") => SCHEMA_OWFLOW_V1,
        _ => {
            return Err((
                StatusCode::NOT_FOUND,
                format!("未知 schema：{kind}/{version}（GET /schemas 查看列表）"),
            ));
        }
    };
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Json(value))
}
