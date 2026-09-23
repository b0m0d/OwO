//! 指南 §3.2 依赖方向守卫：`owo-agent-client` 只能依赖 `owo-agent-protocol`。
//!
//! 这是契约测试，不是文档说明：CI 跑 `cargo test -p owo-agent-client` 时即拒绝
//! "客户端偷偷依赖 core/server/sqlite/mcp/perception" 这类反向依赖回归。

/// 逐行解析 Cargo.toml，返回被检查依赖段（dependencies / build-dependencies /
/// target.*.dependencies，排除 dev-dependencies）里声明的 crate 名。
fn checked_dependency_names(manifest: &str) -> Vec<String> {
    let mut section = String::new();
    let mut names = Vec::new();
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') && line.ends_with(']') {
            section = line.trim_matches(['[', ']']).to_string();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let checked = (section == "dependencies"
            || section == "build-dependencies"
            || section.ends_with(".dependencies"))
            && !section.contains("dev-dependencies");
        if !checked {
            continue;
        }
        // `name = ...` 或 `name.workspace = true` 或 `name = { path = ... }`
        let head = line.split('=').next().unwrap_or("").trim();
        let name = head.split('.').next().unwrap_or("").trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
    }
    names
}

#[test]
fn client_dependencies_are_protocol_only() {
    let manifest = include_str!("../Cargo.toml");
    let names = checked_dependency_names(manifest);
    let forbidden = [
        "owo-agent-core",
        "owo-agent-server",
        "owo-agent-mcp",
        "owo-agent-perception",
        "owo-agent-executor",
        "owo-agent-policy",
        "owo-agent-tool-safety",
        "owo-agent-workflow",
        "owo-agent-workswarm",
        "owo-agent-memory",
        "owo-agent-plugins",
        "owo-agent-extensions",
        "owo-agent-env",
        "owo-agent-kernel",
        "owo-agent-eval-facade",
        "rusqlite",
        "sqlx",
        "libsqlite3-sys",
    ];
    let violations: Vec<&String> = names
        .iter()
        .filter(|name| forbidden.contains(&name.as_str()))
        .collect();
    assert!(
        violations.is_empty(),
        "client 出现禁止依赖（§3.2）：{violations:?}；实际依赖={names:?}"
    );
    assert!(
        names.iter().any(|name| name == "owo-agent-protocol"),
        "client 必须依赖 owo-agent-protocol（契约源）；实际={names:?}"
    );
}

#[test]
fn dependency_parser_detects_inline_and_workspace_forms() {
    let manifest = "[dependencies]\nserde.workspace = true\ntempfile = \"3\"\n\
                    owo-agent-core.workspace = true\n[dev-dependencies]\nowo-agent-server.workspace = true\n";
    let names = checked_dependency_names(manifest);
    assert!(names.contains(&"serde".to_string()));
    assert!(names.contains(&"tempfile".to_string()));
    assert!(names.contains(&"owo-agent-core".to_string()));
    // dev-dependencies 不在检查范围内（测试可起假服务器）。
    assert!(!names.contains(&"owo-agent-server".to_string()));
}
