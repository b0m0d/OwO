//! §9.3 工具 schema 预算与稳定指纹。
//!
//! - **稳定指纹**：对工具「模型可见面」（name + description + input_schema）
//!   按名称排序后做 SHA-256。同一注册表内容跨进程/跨轮次稳定，是 provider
//!   schema 缓存复用的 key 基础（effect 是宿主侧元数据，不参与指纹）。
//! - **schema 预算**：工具面序列化总体积超过预算时按序压缩——description
//!   截断 + input_schema 剥离噪声键（description/examples/default/title），
//!   保留 type/properties/required/enum/items 等结构键；**不删除工具本身**。
//! - 预算来自 `OWO_TOOL_SCHEMA_BUDGET_BYTES`（0 = 关闭；未设 = 默认
//!   200_000 字节）。常规注册表远低于默认值，行为零变化。

use crate::tools::ToolSpec;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// schema 预算缺省上限（字节）。
pub const DEFAULT_SCHEMA_BUDGET_BYTES: usize = 200_000;
/// 压缩后 description 保留的最大字符数。
const COMPRESSED_DESCRIPTION_CHARS: usize = 120;

/// 读取预算上限：`None` = 关闭预算。
pub fn budget_from_env() -> Option<usize> {
    match std::env::var("OWO_TOOL_SCHEMA_BUDGET_BYTES") {
        Ok(value) => value.parse::<usize>().ok().filter(|n| *n > 0),
        Err(_) => Some(DEFAULT_SCHEMA_BUDGET_BYTES),
    }
}

/// 单个 spec 的模型可见面（provider 请求体里实际发送的部分）。
fn visible_json(spec: &ToolSpec) -> Value {
    json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
}

/// 工具面总体积（可见面 JSON 序列化字节数之和）。
fn serialized_bytes(specs: &[ToolSpec]) -> usize {
    specs
        .iter()
        .map(|spec| serde_json::to_vec(&visible_json(spec)).map_or(0, |bytes| bytes.len()))
        .sum()
}

/// 工具面稳定指纹：名称排序后对可见面逐一 SHA-256。
pub fn tools_fingerprint(specs: &[ToolSpec]) -> String {
    let mut visible: Vec<Value> = specs.iter().map(visible_json).collect();
    visible.sort_by_key(|value| {
        value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    });
    let mut hasher = Sha256::new();
    for value in &visible {
        hasher.update(serde_json::to_vec(value).unwrap_or_default());
    }
    format!("{:x}", hasher.finalize())
}

/// 预算执行报告。
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaBudgetReport {
    /// 压缩前总体积（字节）。
    pub original_bytes: usize,
    /// 压缩后总体积（字节）。
    pub final_bytes: usize,
    /// 被压缩的 spec 数量。
    pub compressed: usize,
    /// 压缩后可见面的稳定指纹。
    pub fingerprint: String,
}

/// 递归剥离 schema 噪声键（保留结构键）。
fn strip_schema_noise(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();
            for (key, child) in map {
                match key.as_str() {
                    "description"
                    | "examples"
                    | "default"
                    | "title"
                    | "$schema"
                    | "additionalProperties" => continue,
                    _ => {
                        cleaned.insert(key, strip_schema_noise(child));
                    }
                }
            }
            Value::Object(cleaned)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(strip_schema_noise).collect()),
        other => other,
    }
}

/// 压缩单个 spec：description 截断 + input_schema 噪声剥离。
fn compress_spec(spec: &mut ToolSpec) {
    if spec.description.chars().count() > COMPRESSED_DESCRIPTION_CHARS {
        spec.description = spec
            .description
            .chars()
            .take(COMPRESSED_DESCRIPTION_CHARS)
            .collect::<String>()
            + "…（已压缩，详见工具文档）";
    }
    spec.input_schema = strip_schema_noise(spec.input_schema.clone());
}

/// 预算执行：超预算时按序压缩 spec，直到达标或全部压缩过。
/// `max_bytes: None` = 不压缩（原样返回）。
pub fn enforce_budget(
    mut specs: Vec<ToolSpec>,
    max_bytes: Option<usize>,
) -> (Vec<ToolSpec>, SchemaBudgetReport) {
    let original_bytes = serialized_bytes(&specs);
    let mut compressed = 0;
    if let Some(max_bytes) = max_bytes {
        let mut index = 0;
        while serialized_bytes(&specs) > max_bytes && index < specs.len() {
            compress_spec(&mut specs[index]);
            compressed += 1;
            index += 1;
        }
    }
    let final_bytes = serialized_bytes(&specs);
    let fingerprint = tools_fingerprint(&specs);
    (
        specs,
        SchemaBudgetReport {
            original_bytes,
            final_bytes,
            compressed,
            fingerprint,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, description: &str, schema: Value) -> ToolSpec {
        ToolSpec::with_effect(name, description.to_string(), schema, None)
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let a = vec![
            spec("t1", "d1", json!({ "type": "object" })),
            spec("t2", "d2", json!({ "type": "object" })),
        ];
        let reordered = vec![
            spec("t2", "d2", json!({ "type": "object" })),
            spec("t1", "d1", json!({ "type": "object" })),
        ];
        let changed = vec![
            spec("t1", "changed", json!({ "type": "object" })),
            spec("t2", "d2", json!({ "type": "object" })),
        ];
        let fingerprint = tools_fingerprint(&a);
        assert_eq!(fingerprint, tools_fingerprint(&a), "同输入必须稳定");
        assert_eq!(
            fingerprint,
            tools_fingerprint(&reordered),
            "指纹与顺序无关（排序后哈希）"
        );
        assert_ne!(
            fingerprint,
            tools_fingerprint(&changed),
            "内容变化必须改变指纹"
        );
    }

    #[test]
    fn budget_compression_shrinks_without_dropping_tools() {
        let big_description = "x".repeat(5_000);
        let big_schema = json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "y".repeat(2_000),
                    "examples": ["a.txt"],
                },
                "count": { "type": "integer", "default": 1 },
            },
        });
        let specs = vec![
            spec("big_tool", &big_description, big_schema),
            spec("small_tool", "ok", json!({ "type": "object" })),
        ];
        let (compressed, report) = enforce_budget(specs, Some(2_000));
        assert_eq!(compressed.len(), 2, "工具本身不得被删除");
        assert_eq!(report.compressed, 1, "只压缩到预算所需的最少 spec");
        assert!(report.final_bytes < report.original_bytes);
        assert!(report.final_bytes <= 2_000, "压缩后应达标");
        // 小工具未被压缩：description 原样保留。
        let small = compressed
            .iter()
            .find(|spec| spec.name == "small_tool")
            .expect("small_tool 仍在");
        assert_eq!(small.description, "ok");
    }

    #[test]
    fn no_budget_is_noop() {
        let specs = vec![spec("t", "d", json!({ "type": "object" }))];
        let (out, report) = enforce_budget(specs, None);
        assert_eq!(out[0].description, "d");
        assert_eq!(report.compressed, 0);
        assert_eq!(report.original_bytes, report.final_bytes);
    }

    #[test]
    fn compression_preserves_structural_schema_keys() {
        let specs = vec![spec(
            "t",
            &"长".repeat(300),
            json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {
                        "type": "string",
                        "enum": ["a", "b"],
                        "description": "丢我",
                    },
                },
            }),
        )];
        let (out, _) = enforce_budget(specs, Some(100));
        let schema = &out[0].input_schema;
        assert_eq!(schema["required"], json!(["path"]), "required 必须保留");
        assert_eq!(
            schema["properties"]["path"]["enum"],
            json!(["a", "b"]),
            "enum 必须保留"
        );
        assert_eq!(
            schema["properties"]["path"]["type"], "string",
            "type 必须保留"
        );
        assert!(
            schema["properties"]["path"].get("description").is_none(),
            "噪声键 description 应被剥离"
        );
    }
}
