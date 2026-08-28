//! Project Workspace 工作区绑定集成测试（六期 · 第二路）。
//!
//! 覆盖（全部使用内置 echo worker + 真实 Git 仓库，不依赖模型凭据）：
//! 1. PUT/GET 绑定往返：真实 Git 仓库绑定、字段回读；
//! 2. 路径安全：root `..` 拒绝、白名单 `../` 越界拒绝、不存在 root 拒绝；
//! 3. tree：受限深度目录树（文件/目录）；
//! 4. git-status：真实 `git status --porcelain`；
//! 5. POST /teams 可选 workspace 直接绑定；cancel 后绑定保留（生命周期独立）。
//! （审批器/Spec 缺省语义单元测试在 `project_workspace_api.rs` 模块内 `#[cfg(test)]`。）

use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

struct IdleProvider;

#[async_trait::async_trait]
impl ModelProvider for IdleProvider {
    async fn complete(
        &self,
        _messages: &[owo_agent_core::ChatMessage],
        _tools: &[owo_agent_core::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        Err("IdleProvider 不应被调用".to_string())
    }
}

async fn test_state() -> (Arc<owo_agent_server::AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = Agent::new(
        Arc::new(IdleProvider),
        ToolRegistry::new(),
        owo_agent_core::permissions::Policy::new(&workspace),
        Default::default(),
    );
    let store = SqliteSessionStore::open(&workspace.join("index.db")).unwrap();
    let state = Arc::new(owo_agent_server::AppState::new(
        agent,
        store,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    (state, temp)
}

fn request(
    state: &Arc<owo_agent_server::AppState>,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> axum::http::Request<axum::body::Body> {
    use axum::http::{header, Method, Request};
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path)
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", state.auth_token.token()),
        );
    if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder.body(axum::body::Body::from(b.to_string())).unwrap();
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

async fn call(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, Value) {
    let resp = app
        .clone()
        .oneshot(request(state, method, path, body))
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        json!(null)
    } else {
        serde_json::from_slice(&bytes).unwrap_or(json!(null))
    };
    (status, value)
}

/// 建一个真实 Git 仓库（init + 身份配置；porcelain 无需提交）。
fn init_git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "workspace-test"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .output()
            .expect("git 应可用（本测试依赖 git CLI）");
        assert!(out.status.success(), "git {args:?} 失败：{out:?}");
    }
}

fn workspace_body(root: &Path, read_only: bool, allowed: &[&str], depth: u32) -> String {
    json!({
        "root": root.to_string_lossy(),
        "read_only": read_only,
        "write_allowed_paths": allowed,
        "tree_depth": depth,
    })
    .to_string()
}

/// echo 单角色建队（可带 workspace），返回 (team_id, project_id)。
async fn create_echo_team(
    state: &Arc<owo_agent_server::AppState>,
    app: &axum::Router,
    workspace: Option<&Path>,
) -> (String, String) {
    let mut body = json!({
        "objective": "工作区绑定测试",
        "mode": "team",
        "roles": [
            { "role": "builder", "assignee": "agent", "worker": "echo", "verify": "non_empty" }
        ]
    });
    if let Some(root) = workspace {
        body["workspace"] = json!({
            "root": root.to_string_lossy(),
            "read_only": true,
        });
    }
    let (status, created) = call(state, app, "POST", "/teams", Some(&body.to_string())).await;
    assert_eq!(status, 202, "{created}");
    (
        created["team_id"].as_str().unwrap().to_string(),
        created["project_space_id"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn workspace_bind_tree_git_status_roundtrip() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let repo = temp.path().join("repo");
    init_git_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "hello").unwrap();

    let (team_id, project_id) = create_echo_team(&state, &app, None).await;
    let _ = team_id;

    // PUT 绑定（只读 + 白名单 docs/）。
    let (status, bound) = call(
        &state,
        &app,
        "PUT",
        &format!("/projects/{project_id}/workspace"),
        Some(&workspace_body(&repo, true, &["docs"], 2)),
    )
    .await;
    assert_eq!(status, 200, "{bound}");
    let ws = &bound["workspace"];
    assert_eq!(ws["read_only"], json!(true));
    assert_eq!(ws["tree_depth"], json!(2));
    assert!(
        ws["root_canonical"].as_str().unwrap().ends_with("repo"),
        "root_canonical 应指向真实仓库：{ws}"
    );

    // GET 回读一致。
    let (_, fetched) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/workspace"),
        None,
    )
    .await;
    assert_eq!(fetched["workspace"]["root_canonical"], ws["root_canonical"]);

    // 路径安全：root 带 `..` → 400；白名单越界 → 400；root 不存在 → 400。
    let (status, resp) = call(
        &state,
        &app,
        "PUT",
        &format!("/projects/{project_id}/workspace"),
        Some(&workspace_body(
            &repo.join("..").join("outside"),
            true,
            &[],
            3,
        )),
    )
    .await;
    assert_eq!(status, 400, "root `..` 应拒绝：{resp}");
    let (status, resp) = call(
        &state,
        &app,
        "PUT",
        &format!("/projects/{project_id}/workspace"),
        Some(&workspace_body(&repo, true, &["../outside"], 3)),
    )
    .await;
    assert_eq!(status, 400, "白名单越界应拒绝：{resp}");
    let (status, resp) = call(
        &state,
        &app,
        "PUT",
        &format!("/projects/{project_id}/workspace"),
        Some(&workspace_body(
            &temp.path().join("no-such-dir"),
            true,
            &[],
            3,
        )),
    )
    .await;
    assert_eq!(status, 400, "不存在的 root 应拒绝：{resp}");

    // tree：受限深度，含目录与文件。
    std::fs::create_dir_all(repo.join("docs").join("deep")).unwrap();
    std::fs::write(repo.join("docs").join("ok.txt"), "x").unwrap();
    std::fs::write(repo.join("docs").join("deep").join("z.txt"), "y").unwrap();
    let (_, tree) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/workspace/tree"),
        None,
    )
    .await;
    let entries = tree["entries"].as_array().unwrap();
    let paths: Vec<&str> = entries.iter().filter_map(|e| e["path"].as_str()).collect();
    assert!(
        paths.iter().any(|p| *p == "tracked.txt"),
        "顶层文件应在树中：{tree}"
    );
    assert!(paths.iter().any(|p| *p == "docs"), "目录应在树中：{tree}");
    assert!(
        paths.iter().any(|p| *p == "docs/ok.txt"),
        "深度内文件应在树中：{tree}"
    );
    assert!(
        !paths.iter().any(|p| p.contains("deep/z.txt")),
        "超过 tree_depth 的文件不得出现：{tree}"
    );

    // git-status：porcelain 捕获未跟踪文件。
    let (_, status_json) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/workspace/git-status"),
        None,
    )
    .await;
    assert_eq!(status_json["git"], json!(true), "{status_json}");
    let porcelain = status_json["porcelain"].as_str().unwrap_or("");
    assert!(
        porcelain.contains("tracked.txt") || porcelain.contains("??"),
        "git status 应包含未跟踪条目：{porcelain}"
    );
}

#[tokio::test]
async fn create_team_with_workspace_binds_and_survives_cancel() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let repo = temp.path().join("repo2");
    init_git_repo(&repo);

    // POST /teams 直接带 workspace → workspace_bound=true。
    let (team_id, project_id) = create_echo_team(&state, &app, Some(&repo)).await;

    // 等 echo 团队到终态。
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (_, detail) = call(&state, &app, "GET", &format!("/teams/{team_id}"), None).await;
        if detail["team"]["status"] == json!("succeeded") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "echo 团队未在超时内完成：{detail}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 绑定已生效。
    let (_, bound) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/workspace"),
        None,
    )
    .await;
    assert_eq!(bound["workspace"]["read_only"], json!(true), "{bound}");

    // cancel（终态后 no-op，验证幂等）→ 绑定保留（生命周期独立于运行状态）。
    let (status, _) = call(
        &state,
        &app,
        "POST",
        &format!("/teams/{team_id}/steer"),
        Some(&json!({ "command": "cancel" }).to_string()),
    )
    .await;
    assert_eq!(status, 200);
    let (_, after_cancel) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/workspace"),
        None,
    )
    .await;
    assert_eq!(after_cancel["workspace"]["team_id"], json!(team_id));

    // PUT 更新绑定（放开写白名单）→ GET 反映新值（retry/resume 循环重读生效）。
    let (status, updated) = call(
        &state,
        &app,
        "PUT",
        &format!("/projects/{project_id}/workspace"),
        Some(&workspace_body(&repo, false, &[], 3)),
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["workspace"]["read_only"], json!(false));
}

/// 未绑定项目 → 404。
#[tokio::test]
async fn workspace_of_unbound_project_is_404() {
    let (state, temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let repo = temp.path().join("repo3");
    init_git_repo(&repo);
    let (_team_id, project_id) = create_echo_team(&state, &app, None).await;
    let (status, resp) = call(
        &state,
        &app,
        "GET",
        &format!("/projects/{project_id}/workspace"),
        None,
    )
    .await;
    assert_eq!(status, 404, "{resp}");
    let _ = repo;
}
