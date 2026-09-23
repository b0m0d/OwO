//! P1 静态依赖守卫（指南 §8 P1 完成条件）：
//! `turn` / `daemon` 普通路径**不得**构造第二套运行时（Agent / SQLite / MCP）。
//!
//! 为什么是测试而不是注释：M0–M15 的搬迁期兼容桥证明"写在注释里的约束"会漂移。
//! 这里对源码做机械扫描（先剥注释），任何一次把 `Agent::new` 写回交互路径都会红。

fn source_without_comments(path: &str) -> String {
    let full = format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path);
    let text =
        std::fs::read_to_string(&full).unwrap_or_else(|error| panic!("读取 {full} 失败：{error}"));
    let without_block = strip_block_comments(&text);
    without_block
        .lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 简单块注释剥离（本仓库 Rust 源码无嵌套块注释）。
fn strip_block_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_block = false;
    while let Some(ch) = chars.next() {
        if in_block {
            if ch == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            in_block = true;
            continue;
        }
        output.push(ch);
    }
    output
}

const SECOND_RUNTIME_TOKENS: &[&str] = &[
    "Agent::new",
    "SqliteSessionStore",
    "connect_mcp_clients",
    "build_agent",
    "owo_agent_core",
    "McpClient",
    "OpenAiCompatibleProvider",
    "ResilientProvider",
];

#[test]
fn turn_and_daemon_paths_have_no_second_runtime() {
    for file in [
        "src/commands/turn.rs",
        "src/commands/daemon.rs",
        "src/commands/repl_daemon.rs",
        "src/tui.rs",
    ] {
        let source = source_without_comments(file);
        for token in SECOND_RUNTIME_TOKENS {
            assert!(
                !source.contains(token),
                "{file} 仍引用 `{token}`（P1：普通路径必须经 Daemon 客户端，禁止第二套运行时）"
            );
        }
    }
}

#[test]
fn turn_path_uses_shared_client() {
    let source = source_without_comments("src/commands/turn.rs");
    assert!(
        source.contains("ensure_daemon_client"),
        "turn 必须经共享单实例启动协议（support::ensure_daemon_client）"
    );
    assert!(
        source.contains("open_turn"),
        "turn 必须消费客户端 SSE 事件流（AgentClient::open_turn）"
    );
}

#[test]
fn comment_stripping_actually_removes_tokens() {
    // 负例自检：确保守卫不是"永远通过"（注释里的禁用词不应触发误报）。
    let sample = "// Agent::new 已删除\nlet x = 1; // SqliteSessionStore\n";
    let stripped = sample
        .lines()
        .map(|line| match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!stripped.contains("Agent::new"));
    assert!(!stripped.contains("SqliteSessionStore"));
}
