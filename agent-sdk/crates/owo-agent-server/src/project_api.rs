//! 项目规则 HTTP API（§12：从 lib.rs 机械外移的 AGENTS.md/CLAUDE.md 规则域）。
//!
//! 路由面（`GET/POST /project/rules`、`POST /project/rules/template`）与
//! /openapi.json 登记保持不变，零行为变化。
//!
//! 独立编译约束（沿 notes_api 惯例）：不使用 `crate::`/`super::`；引用 server
//! 类型一律写全限定名 `owo_agent_server::AppState`。

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use owo_agent_server::AppState;

/// 项目规则列表：`GET /project/rules`。
/// 读取工作区 AGENTS.md / CLAUDE.md 并报告注入状态（会话启动时是否会加载）。
pub(super) async fn project_rules_get(State(state): State<Arc<AppState>>) -> Json<Value> {
    let names = ["AGENTS.md", "CLAUDE.md"];
    let rules: Vec<Value> = names
        .iter()
        .map(|name| {
            let path = state.workspace.join(name);
            let exists = path.is_file();
            let content = if exists {
                std::fs::read_to_string(&path).unwrap_or_default()
            } else {
                String::new()
            };
            json!({
                "name": name,
                "path": path.to_string_lossy(),
                "exists": exists,
                "injected": exists,
                "content": content,
            })
        })
        .collect();
    Json(json!({
        "workspace": state.workspace.to_string_lossy(),
        "count": rules.len(),
        "rules": rules,
    }))
}

#[derive(Deserialize)]
pub(super) struct ProjectRulesRequest {
    content: String,
}

pub(super) async fn project_rules_post(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ProjectRulesRequest>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = state.workspace.join("AGENTS.md");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    }
    std::fs::write(&path, &request.content)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "project",
            "rules-write",
            Some("AGENTS.md".to_string()),
            Some(true),
            format!(
                "写入项目规则 AGENTS.md（{} 字符）",
                request.content.chars().count()
            ),
        );
    }
    // §3.2：项目规则变更成功后发布 projects 域失效。
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Projects);
    Ok(Json(json!({
        "ok": true,
        "path": path.to_string_lossy(),
        "chars": request.content.chars().count(),
    })))
}

pub(super) async fn project_rules_template(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    const TEMPLATE: &str = "# AGENTS.md

<!-- 由 OwO Agent 生成，按项目实际情况修改。
     该文件会被 Agent 在每次会话开始时注入，作为项目级规则。 -->

## 项目说明

- 一句话描述本项目做什么。

## 开发规则

- 写清楚构建命令、测试命令与代码约定。
- 说明哪些目录/文件禁止修改。
";
    let path = state.workspace.join("AGENTS.md");
    if path.exists() {
        return Err((
            StatusCode::CONFLICT,
            format!("AGENTS.md 已存在（{}），未覆盖", path.display()),
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    }
    std::fs::write(&path, TEMPLATE)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    if let Ok(mut audit) = state.agent.audit_log().lock() {
        audit.record(
            "project",
            "rules-template",
            Some("AGENTS.md".to_string()),
            Some(true),
            "生成 AGENTS.md 模板：init 等价操作".to_string(),
        );
    }
    // §3.2：项目规则生成成功后发布 projects 域失效（409 已存在路径不发布）。
    owo_agent_server::event_stream::hub()
        .publish_invalidate(owo_agent_server::event_stream::InvalidateDomain::Projects);
    Ok(Json(json!({
        "ok": true,
        "path": path.to_string_lossy(),
        "chars": TEMPLATE.chars().count(),
    })))
}
