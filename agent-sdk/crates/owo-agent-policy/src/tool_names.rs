//! 工具命名契约：模型 API 约束下的工具名规范化。
//!
//! 这个函数原本是 `owo-agent-core::tools` 里的 `pub(crate)` 私有工具（M12 前只有
//! `tools.rs` 自己与 `tool_effects.rs` 用它）。之所以在 M12 随策略内核一起下沉，
//! 是因为它**同时是权限判定的输入**：
//!
//! * `tool_effects::effect_class_for(name)` 按工具名查效应表；
//! * MCP 工具注册前缀 `{server}_{tool}` 由名字生成，效应表登记也按同一前缀；
//! * 内置工具矩阵（`tool_effects` 的 `Once` 初始化）同样按名字分类。
//!
//! 两条消费链（工具注册表与效应/权限表）必须用**同一份**命名规则，否则"登记的名字"
//! 与"执行时的名字"会漂移——那正是"权限不可绕过"最怕的一类漏口。core 的 `tools.rs`
//! 因此改为 `pub(crate) use owo_agent_policy::tool_names::sanitize_tool_name;`
//! （公共面不变：原来就是 `pub(crate)`）。
//!
//! 待指南 §9 A4 的 executor/tools 段一并迁入 Tool Host 后，这条反向 `use` 会自然消失。

/// 工具名只允许字母数字、下划线与连字符（模型 API 约束）。
pub fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_allowed_characters_and_maps_the_rest() {
        assert_eq!(sanitize_tool_name("echo"), "echo");
        assert_eq!(sanitize_tool_name("a b/c"), "a_b_c");
        // 连字符是模型 API 允许的，必须保留（效应表前缀与注册名同源）。
        assert_eq!(sanitize_tool_name("owo-translate"), "owo-translate");
    }

    #[test]
    fn dotted_plugin_id_becomes_underscored_prefix() {
        assert_eq!(
            sanitize_tool_name("owo.plugin.example-hello"),
            "owo_plugin_example-hello"
        );
    }
}
