//! §7.2 工具副作用元数据（ToolEffect）：工具声明副作用，Policy 只做统一判定。
//!
//! - 内置工具：以 builtin 注册表固化原 `level_for` 矩阵（单一事实源）；
//! - MCP 工具：按 MCP annotations（`readOnlyHint` / `destructiveHint`）推导等级；
//!   未声明 annotations 时落 `Execute`（deny-by-default）并给出统一风险提示，
//!   审批卡据此展示「该工具未声明风险信息」。
//!
//! 注册表为进程级全局：MCP 工具按服务器前缀注册/移除，重连时覆盖旧条目。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Once;
use std::sync::RwLock;

/// 工具未声明风险信息时的统一提示（审批卡直接展示给用户）。
pub const UNDECLARED_RISK_NOTE: &str = "该工具未声明风险信息（未提供 MCP annotations）";

/// 副作用等级（与 `permissions::Level` 对齐；独立定义避免策略模块反向依赖元数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Read,
    Write,
    Execute,
    Inject,
}

impl EffectClass {
    pub fn label(&self) -> &'static str {
        match self {
            EffectClass::Read => "read",
            EffectClass::Write => "write",
            EffectClass::Execute => "execute",
            EffectClass::Inject => "inject",
        }
    }
}

impl From<EffectClass> for crate::permissions::Level {
    fn from(class: EffectClass) -> Self {
        match class {
            EffectClass::Read => crate::permissions::Level::Read,
            EffectClass::Write => crate::permissions::Level::Write,
            EffectClass::Execute => crate::permissions::Level::Execute,
            EffectClass::Inject => crate::permissions::Level::Inject,
        }
    }
}

/// 单个工具的副作用元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolEffect {
    /// 模型可见的完整工具名（MCP 工具为 `server_tool` 前缀名）。
    pub tool: String,
    pub class: EffectClass,
    /// 来源：`builtin` 或 `mcp:<server>`。
    pub source: String,
    /// 风险说明（审批卡展示）；None = 已知内置工具，未额外声明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_note: Option<String>,
    /// MCP annotations 原文（审计/诊断用；内置工具为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
}

static REGISTRY: RwLock<BTreeMap<String, ToolEffect>> = RwLock::new(BTreeMap::new());
static SEED: Once = Once::new();

/// 内置矩阵只播种一次；MCP 注册发生在 connect 之后的运行期。
fn ensure_builtin_seeded() {
    SEED.call_once(|| {
        let mut guard = REGISTRY
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (tool, class) in builtin_effects() {
            guard.insert(
                tool.to_string(),
                ToolEffect {
                    tool: tool.to_string(),
                    class,
                    source: "builtin".to_string(),
                    risk_note: None,
                    annotations: None,
                },
            );
        }
    });
}

fn write_effect(effect: ToolEffect) {
    REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(effect.tool.clone(), effect);
}

/// 注册 MCP 工具副作用：annotations 缺失/未声明只读 → `Execute` + 风险提示。
/// 返回注册后的元数据（供日志与测试断言）。
pub fn register_mcp_effect(
    server_name: &str,
    tool_name: &str,
    annotations: Option<&Value>,
) -> ToolEffect {
    ensure_builtin_seeded();
    let full_name = format!(
        "{}_{}",
        crate::tools::sanitize_tool_name(server_name),
        crate::tools::sanitize_tool_name(tool_name)
    );
    let (class, risk_note) = classify_mcp_annotations(annotations);
    let effect = ToolEffect {
        tool: full_name,
        class,
        source: format!("mcp:{server_name}"),
        risk_note,
        annotations: annotations.cloned(),
    };
    write_effect(effect.clone());
    effect
}

/// 按 MCP 服务器前缀移除副作用条目（工具卸载/服务器下线时调用）。
pub fn remove_prefix(prefix: &str) -> usize {
    let mut guard = REGISTRY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let before = guard.len();
    guard.retain(|name, _| !name.starts_with(prefix));
    before - guard.len()
}

/// 查询单个工具的副作用元数据（自动播种内置矩阵）。
pub fn effect_for(tool: &str) -> Option<ToolEffect> {
    ensure_builtin_seeded();
    REGISTRY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(tool)
        .cloned()
}

/// 判定工具副作用等级：注册表 → 内置矩阵 → 兜底 `Execute`（deny-by-default）。
pub fn effect_class_for(tool: &str) -> EffectClass {
    match effect_for(tool) {
        Some(effect) => effect.class,
        None => builtin_class_for(tool).unwrap_or(EffectClass::Execute),
    }
}

/// 全部已注册副作用（诊断面板/审计导出用只读快照）。
pub fn all_effects() -> Vec<ToolEffect> {
    ensure_builtin_seeded();
    REGISTRY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .cloned()
        .collect()
}

/// MCP annotations → 副作用等级 + 风险说明。
/// 规则（重构方案 5.2）：`destructiveHint=true` 永远覆盖 `readOnlyHint=true`；
/// 仅当宿主已确认只读且无破坏性标注时才降级为 Read，其余一律 Execute
/// （deny-by-default；MCP annotations 是服务端提示，不是宿主验证的安全证明）。
pub fn classify_mcp_annotations(annotations: Option<&Value>) -> (EffectClass, Option<String>) {
    match annotations {
        None => (EffectClass::Execute, Some(UNDECLARED_RISK_NOTE.to_string())),
        Some(value) => {
            let read_only = value.get("readOnlyHint").and_then(Value::as_bool);
            let destructive = value.get("destructiveHint").and_then(Value::as_bool);
            match destructive {
                Some(true) => (
                    EffectClass::Execute,
                    Some("MCP 标注 destructiveHint：可能产生不可逆副作用".to_string()),
                ),
                _ => match read_only {
                    Some(true) => (
                        EffectClass::Read,
                        Some("MCP 标注 readOnlyHint：只读工具".to_string()),
                    ),
                    _ => (
                        EffectClass::Execute,
                        Some("MCP 未声明只读标注，按最高风险等级（execute）处理".to_string()),
                    ),
                },
            }
        }
    }
}

/// 内置工具矩阵（与原 `Policy::level_for` 完全一致；未知工具返回 None）。
pub fn builtin_class_for(tool: &str) -> Option<EffectClass> {
    let class = match tool {
        "read_file" | "list_dir" | "search_files" => EffectClass::Read,
        "write_file" => EffectClass::Write,
        "run_command" => EffectClass::Execute,
        "text.inject" | "clipboard" => EffectClass::Inject,
        "screen_ocr"
        | "desktop_window_ocr"
        | "ocr_region"
        | "desktop_foreground"
        | "desktop_window_list"
        | "desktop_wait"
        | "desktop_wait_until"
        | "browser_snapshot"
        | "screen_vision"
        | "vision_verify"
        | "vision_ground" => EffectClass::Read,
        "desktop_click" | "desktop_type" | "desktop_key" | "desktop_shortcut"
        | "desktop_activate" | "desktop_launch" | "desktop_scroll" => EffectClass::Inject,
        "browser_navigate" | "browser_search" | "browser_click" | "browser_type"
        | "browser_press" | "browser_close" => EffectClass::Execute,
        "browser_screenshot" | "browser_download_image" => EffectClass::Write,
        _ => return None,
    };
    Some(class)
}

fn builtin_effects() -> Vec<(&'static str, EffectClass)> {
    [
        ("read_file", EffectClass::Read),
        ("list_dir", EffectClass::Read),
        ("search_files", EffectClass::Read),
        ("screen_ocr", EffectClass::Read),
        ("desktop_window_ocr", EffectClass::Read),
        ("ocr_region", EffectClass::Read),
        ("desktop_foreground", EffectClass::Read),
        ("desktop_window_list", EffectClass::Read),
        ("desktop_wait", EffectClass::Read),
        ("desktop_wait_until", EffectClass::Read),
        ("browser_snapshot", EffectClass::Read),
        ("screen_vision", EffectClass::Read),
        ("vision_verify", EffectClass::Read),
        ("vision_ground", EffectClass::Read),
        ("write_file", EffectClass::Write),
        ("browser_screenshot", EffectClass::Write),
        ("browser_download_image", EffectClass::Write),
        ("run_command", EffectClass::Execute),
        ("browser_navigate", EffectClass::Execute),
        ("browser_search", EffectClass::Execute),
        ("browser_click", EffectClass::Execute),
        ("browser_type", EffectClass::Execute),
        ("browser_press", EffectClass::Execute),
        ("browser_close", EffectClass::Execute),
        ("text.inject", EffectClass::Inject),
        ("clipboard", EffectClass::Inject),
        ("desktop_click", EffectClass::Inject),
        ("desktop_type", EffectClass::Inject),
        ("desktop_key", EffectClass::Inject),
        ("desktop_shortcut", EffectClass::Inject),
        ("desktop_activate", EffectClass::Inject),
        ("desktop_launch", EffectClass::Inject),
        ("desktop_scroll", EffectClass::Inject),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_matrix_is_seeded_and_classified() {
        assert_eq!(effect_class_for("read_file"), EffectClass::Read);
        assert_eq!(effect_class_for("write_file"), EffectClass::Write);
        assert_eq!(effect_class_for("run_command"), EffectClass::Execute);
        assert_eq!(effect_class_for("desktop_click"), EffectClass::Inject);
        assert_eq!(effect_class_for("browser_snapshot"), EffectClass::Read);
        assert_eq!(effect_class_for("text.inject"), EffectClass::Inject);
        let builtin = all_effects()
            .into_iter()
            .filter(|effect| effect.source == "builtin")
            .count();
        assert_eq!(builtin, 33, "内置矩阵应完整播种 33 个工具");
    }

    #[test]
    fn unknown_tool_falls_back_to_execute() {
        assert_eq!(
            effect_class_for("definitely_not_a_tool_owo"),
            EffectClass::Execute,
            "未知工具必须按最高风险等级兜底"
        );
    }

    #[test]
    fn mcp_annotations_read_only_hint_maps_to_read() {
        let (class, note) = classify_mcp_annotations(Some(&json!({
            "title": "查天气",
            "readOnlyHint": true
        })));
        assert_eq!(class, EffectClass::Read);
        assert!(note.unwrap().contains("readOnlyHint"));
    }

    #[test]
    fn mcp_destructive_hint_maps_to_execute_with_note() {
        let (class, note) = classify_mcp_annotations(Some(&json!({
            "readOnlyHint": false,
            "destructiveHint": true
        })));
        assert_eq!(class, EffectClass::Execute);
        assert!(note.unwrap().contains("destructiveHint"));
    }

    #[test]
    fn mcp_destructive_hint_overrides_read_only_hint() {
        // 重构方案 5.2：destructiveHint=true 永远覆盖 readOnlyHint=true。
        let (class, _) = classify_mcp_annotations(Some(&json!({
            "readOnlyHint": true,
            "destructiveHint": true
        })));
        assert_eq!(
            class,
            EffectClass::Execute,
            "同时声明时破坏性标注优先，不得降级为 Read"
        );
    }

    #[test]
    fn mcp_without_annotations_gets_undeclared_risk_note() {
        let (class, note) = classify_mcp_annotations(None);
        assert_eq!(class, EffectClass::Execute);
        assert_eq!(note.as_deref(), Some(UNDECLARED_RISK_NOTE));
    }

    #[test]
    fn register_and_remove_prefix_roundtrip() {
        let server = format!("tst-{}", uuid::Uuid::new_v4().simple());
        let effect = register_mcp_effect(&server, "query", Some(&json!({ "readOnlyHint": true })));
        // sanitize_tool_name 保留连字符（模型 API 允许 `-`），前缀名与注册名同源。
        let full = format!("{}_query", server);
        assert_eq!(effect.tool, full);
        assert_eq!(effect.class, EffectClass::Read);
        assert_eq!(effect.source, format!("mcp:{server}"));
        assert!(effect_for(&full).is_some());
        let removed = remove_prefix(&format!("{}_", server));
        assert_eq!(removed, 1);
        assert!(effect_for(&full).is_none());
    }

    #[test]
    fn re_register_overwrites_previous_class() {
        let server = format!("tst-{}", uuid::Uuid::new_v4().simple());
        let tool = "flip";
        let first = register_mcp_effect(&server, tool, None);
        assert_eq!(first.class, EffectClass::Execute);
        let second = register_mcp_effect(&server, tool, Some(&json!({ "readOnlyHint": true })));
        assert_eq!(second.class, EffectClass::Read);
        let full = format!("{}_{}", server, tool);
        assert_eq!(effect_for(&full).unwrap().class, EffectClass::Read);
        remove_prefix(&format!("{}_", server));
    }

    #[test]
    fn effect_class_converts_into_permission_level() {
        let levels: Vec<crate::permissions::Level> = [
            EffectClass::Read,
            EffectClass::Write,
            EffectClass::Execute,
            EffectClass::Inject,
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        assert_eq!(
            levels,
            vec![
                crate::permissions::Level::Read,
                crate::permissions::Level::Write,
                crate::permissions::Level::Execute,
                crate::permissions::Level::Inject,
            ]
        );
    }
}
