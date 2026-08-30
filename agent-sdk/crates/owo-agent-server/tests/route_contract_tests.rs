//! 路由面契约测试：验证 v0.5/v0.6 HTTP 接口全部注册且可达（防回归）。
//!
//! 背景（2026-08-14）：/locate/query、/memory/recall、/skills/health、/plugins、
//! /traces、/subagent/run、/project/rules、/mcp、/session/{id}/context 曾在服务端回归
//! 丢失（404），而桌面端仍在调用。
//!
//! 本文件以 `clients/ts/openapi.json`（2026-08-13 权威契约快照）为基准：
//! 1. 断言每个契约路径 + 方法可到达（非 404/405，合法输入 2xx、最小非法输入 400/422 亦可）。
//! 2. 断言 `/openapi.json` 登记了契约快照全部路径与 lib.rs 实际注册的全部路由。
//! 3. 真实 HTTP 服务启动 smoke（防 Router 构建 panic）。

use owo_agent_core::permissions::Policy;
use owo_agent_core::sqlite_store::SqliteSessionStore;
use owo_agent_core::tools::ToolRegistry;
use owo_agent_core::Agent;
use owo_agent_server::build_router;
use std::sync::Arc;
use tower::ServiceExt;

/// 契约快照（权威）：agent-sdk/clients/ts/openapi.json。
const CONTRACT_SNAPSHOT: &str = include_str!("../../../clients/ts/openapi.json");

/// lib.rs 源码（用于提取实际注册路由，验证 openapi_spec 无漏登）。
const LIB_RS: &str = include_str!("../src/lib.rs");

/// 返回契约快照中的全部 (path, [method...])。
fn contract_endpoints() -> Vec<(String, Vec<String>)> {
    let snapshot = CONTRACT_SNAPSHOT.trim_start_matches('\u{feff}');
    let json: serde_json::Value = serde_json::from_str(snapshot).unwrap();
    json["paths"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(path, methods)| {
            let list = methods
                .as_object()
                .unwrap()
                .keys()
                .map(|m| m.to_uppercase())
                .collect();
            (path.clone(), list)
        })
        .collect()
}

/// 从 lib.rs 提取实际注册的路由路径。
fn registered_routes() -> Vec<String> {
    let mut routes = Vec::new();
    for line in LIB_RS.lines() {
        if let Some(start) = line.find(".route(") {
            let rest = &line[start + ".route(".len()..];
            if let Some(quote) = rest.find('"') {
                let after = &rest[quote + 1..];
                if let Some(end) = after.find('"') {
                    routes.push(after[..end].to_string());
                }
            }
        }
    }
    routes.sort();
    routes.dedup();
    routes
}

/// 从全部 server 源文件（含模块路由）提取注册路由，防“模块内路由漏登记 OpenAPI”。
fn all_source_routes() -> Vec<String> {
    let mut routes: Vec<String> = Vec::new();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in source.lines() {
                if let Some(start) = line.find(".route(") {
                    let rest = &line[start + ".route(".len()..];
                    if let Some(quote) = rest.find('"') {
                        let after = &rest[quote + 1..];
                        if let Some(end) = after.find('"') {
                            let route = after[..end].to_string();
                            if !routes.contains(&route) {
                                routes.push(route);
                            }
                        }
                    }
                }
            }
        }
    }
    routes.sort();
    routes
}

/// 无外部依赖的最小模型 Provider（任何模型调用即失败）。
struct IdleProvider;

#[async_trait::async_trait]
impl owo_agent_core::gateway::ModelProvider for IdleProvider {
    async fn complete(
        &self,
        _messages: &[owo_agent_core::ChatMessage],
        _tools: &[owo_agent_core::ToolSpec],
    ) -> Result<owo_agent_core::ModelOutput, String> {
        Err("IdleProvider 不应被调用".to_string())
    }
}

/// 环境变量是进程级共享（OWO_API_RPM_GLOBAL 等），而 AppState::new 构建时读取它们；
/// 并行测试若一方 set_var、另一方同时 new state，就会把限流配置泄漏给对方。
/// 因此所有「env 读取敏感」的 state 构建统一经此锁串行化。
static STATE_ENV_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

async fn test_state() -> (Arc<owo_agent_server::AppState>, tempfile::TempDir) {
    let _guard = STATE_ENV_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    // 限流用例可能已把进程级 RPM 调小；恢复默认（600）再构建，避免污染其他用例。
    std::env::remove_var("OWO_API_RPM_GLOBAL");
    build_state_inner().await
}

/// 无锁的 state 构建（调用方负责持有 STATE_ENV_LOCK 并管理 OWO_API_* 环境变量）。
async fn build_state_inner() -> (Arc<owo_agent_server::AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = Agent::new(
        Arc::new(IdleProvider),
        ToolRegistry::new(),
        Policy::new(&workspace),
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

/// 构造请求（R7：自动附带本 state 的 bearer token）。
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

/// 不带 token 的请求（鉴权负例）。
fn anonymous_request(
    method: &str,
    path: &str,
    body: Option<&str>,
) -> axum::http::Request<axum::body::Body> {
    use axum::http::{header, Method, Request};
    let mut builder = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(path);
    if let Some(b) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        return builder.body(axum::body::Body::from(b.to_string())).unwrap();
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

/// 契约路径的最小合法/非法请求体（缺省 {}，避免 route 存在性判断被 body 校验干扰）。
fn sample_body(path: &str) -> Option<&'static str> {
    match path {
        "/session" => Some(r#"{"workspace":".","model":"idle"}"#),
        "/session/{id}/turn" => Some(r#"{"prompt":"hi"}"#),
        "/session/{id}/permission/{request_id}" => Some(r#"{"allow":true}"#),
        "/plugins/{id}/enabled" => Some(r#"{"enabled":false}"#),
        "/subagent/run" => Some(r#"{"prompt":"hi","read_only":true}"#),
        "/project/rules" => Some(r#"{"content":"rules"}"#),
        "/mcp/add" => {
            Some(r#"{"name":"__missing__","transport":"http","url":"http://127.0.0.1:1"}"#)
        }
        "/mcp/remove" => Some(r#"{"name":"__missing__"}"#),
        "/locate/query" => Some(r#"{}"#),
        "/memory/mine-skill" => {
            Some(r#"{"name":"t","target_apps":[],"sensitivity":"low","description":"d"}"#)
        }
        "/skills/{name}/enabled" => Some(r#"{"enabled":false}"#),
        "/computer-use/task" => Some(r#"{"target_app":"notepad","description":"d"}"#),
        "/computer-use/task/{id}/{action}" => Some(r#"{"reason":"r"}"#),
        "/computer-use/sensitive-check" => Some(r#"{"name":"PasswordBox"}"#),
        "/notes" => Some(r#"{"title":"契约测试笔记"}"#),
        "/notes/import" => Some(r##"{"title":"t","markdown":"# hi"}"##),
        "/notes/{id}/blocks" => Some(r#"{"kind":"paragraph","text":"t"}"#),
        "/notes/{id}/blocks/move" => Some(r#"{"block_id":"no-such-block"}"#),
        "/goal" => Some(r#"{"objective":"契约测试目标"}"#),
        "/goal/{id}/plan" => Some(r#"{"steps":[{"id":"a","worker":"echo"}]}"#),
        "/goal/{id}/run" => Some(r#"{}"#),
        "/goal/{id}/abort" => Some(r#"{}"#),
        "/plugins/market/seed" => Some(r#"{"entries":[]}"#),
        "/plugins/market/verify" => Some(r#"{"dir":"."}"#),
        "/plugins/market/install" => Some(r#"{"dir":"."}"#),
        "/plugins/market/update" => Some(r#"{"id":"x","dir":"."}"#),
        "/plugins/market/uninstall" => Some(r#"{"id":"x"}"#),
        "/workflow/validate" => Some(
            r#"{"id":"ct","name":"ct-flow","version":1,"triggers":[{"id":"t1","kind":{"kind":"manual"}}],"permissions":[{"scope":"fs.write","mode":"allow"}],"preconditions":[],"rollback_points":[],"max_steps":100,"subflow_depth_limit":5,"steps":[{"kind":"notify","id":"n1","message":"ok"}]}"#,
        ),
        "/workflow/{name}/run" => Some(r#"{}"#),
        "/workflow/run/{run_id}/abort" => Some(r#"{}"#),
        "/workflow/run/{run_id}/approval" => Some(r#"{"decision":"approve"}"#),
        "/plugins/market/refresh" => Some(r#"{}"#),
        "/plugins/market/install-remote" => Some(r#"{"id":"x"}"#),
        "/team/export" => Some(r#"{"type":"flow","id":"x"}"#),
        "/team/review" => Some(r#"{"package_b64":"aGVsbG8="}"#),
        "/team/import" => Some(r#"{"package_b64":"aGVsbG8="}"#),
        // 故意给不存在的套件路径：无论是否配置 OPENAI_API_KEY 都快速失败（400），
        // 避免契约测试在真实凭据环境下触发真实模型 eval（分钟级挂起）。
        "/eval/gate/run" => Some(r#"{"suite":"__contract_missing_suite__"}"#),
        "/memory/graph/link" => Some(r#"{"a":"x","b":"y","relation":"r"}"#),
        "/intent/parse" => Some(r#"{"text":"hi"}"#),
        "/command/run" => Some(r#"{"mode":"text","text":"hi"}"#),
        // R12 /fleet/*（Agent 2 控制面）：合法最小输入应 200。
        "/fleet/nodes/register" => {
            Some(r#"{"node_id":"ct-node","card":{"worker":"ct-node","actions":["shell"]}}"#)
        }
        "/fleet/tasks/submit" => Some(r#"{"task_id":"ct-task","worker":"ct-node","input":{}}"#),
        "/fleet/approvals/{id}/respond" => Some(r#"{"decision":"reject","approved_by":"ct"}"#),
        // V1 三日 /product-eval/*（第四路）：reference 免模型最小合法体（202 受理；
        // test_state 的 workspace 不含 evals/v1 时则 400「suite 加载失败」，两种都证明路由可达）。
        "/product-eval/runs" => Some(
            r#"{"suite":"v1","execution":"reference","modes":["single"],"repetitions":1,"category":null,"only":null}"#,
        ),
        // R13 WorkSwarm（§8.5）：空 objective 快速 400，避免契约测试触发真实模型运行。
        "/teams" => Some(r#"{"objective":"","roles":[]}"#),
        // 五期（第一路）：组队策略（auto 缺省）；空 objective 快速 400 语义不变。
        // "/teams" 的 strategy 字段随 auto 缺省可省略；此处维持最小体。
        "/teams/{id}/steer" => Some(r#"{"command":"cancel"}"#),
        // 五期（第二路）：返工请求冻结契约；占位 team/review 不存在 → 404（资源缺失）。
        "/artifacts/{id}/rework" => Some(
            r#"{"team_id":"no-such-team","review_id":"no-such-review","instruction":"契约测试返工指令"}"#,
        ),
        // 六期（第二路）：工作区绑定冻结契约；占位 project 不存在 → 404（资源缺失）。
        "/projects/{id}/workspace" => {
            Some(r#"{"root":".","read_only":true,"write_allowed_paths":[],"tree_depth":2}"#)
        }
        // 六期（第三路）：模板目录安装（幂等）；占位 id 不存在 → 404（资源缺失）。
        "/teams/templates/catalog/{id}/install" => Some(r#"{}"#),
        "/tasks/{id}/handoff" => {
            Some(r#"{"team_id":"no-such-team","from_member":"m-x","completed_summary":"done"}"#)
        }
        "/tasks/{id}/human-result" => Some(r#"{"team_id":"no-such-team","result":"ok"}"#),
        "/teams/templates/proposals/{proposal_id}/adopt" => Some(r#"{}"#),
        "/teams/templates/proposals/{proposal_id}/reject" => Some(r#"{}"#),
        // R1 DesktopWorld/WorldModel 训练闭环（§5.11/§5.12）：合法最小输入；
        // 资源型路径用占位 {id}（不存在资源 → 404 由资源缺失产生，见 resource_404_ok）。
        "/desktop-envs" => Some(r#"{"task":{"task_id":"ct-dw","app":"chat","seed":1}}"#),
        "/desktop-envs/{id}/reset" => Some(
            r#"{"task":{"task_id":"ct-dw","app":"chat","seed":1},"lease":{"owner":"ct","token":"ct","epoch":0}}"#,
        ),
        "/desktop-envs/{id}/lease" => Some(r#"{"op":"acquire"}"#),
        "/desktop-envs/{id}/step" => Some(
            r#"{"action":{"action_id":"ct-a1","kind":"wait","semantic_intent":"等待"},"lease":{"owner":"ct","token":"ct","epoch":0}}"#,
        ),
        "/desktop-envs/{id}/snapshot" => Some(r#"{}"#),
        "/desktop-envs/{id}/restore" => {
            Some(r#"{"snapshot":"no-such-snapshot","lease":{"owner":"ct","token":"ct","epoch":0}}"#)
        }
        "/desktop-envs/{id}/judge" => Some(r#"{"success":{"name":"ct","assertions":[]}}"#),
        "/desktop-envs/{id}/inject-fault" => Some(
            r#"{"fault":{"type":"sluggish_steps","steps":1},"lease":{"owner":"ct","token":"ct","epoch":0}}"#,
        ),
        "/world-model/predict" => Some(
            r#"{"env_id":"no-such-env","action":{"action_id":"ct-a1","kind":"wait","semantic_intent":"等待"}}"#,
        ),
        "/transitions/{id}" => Some(r#"{}"#),
        "/datasets/build" => Some(r#"{}"#),
        "/datasets/{id}/manifest" => Some(r#"{}"#),
        "/model-candidates" => Some(r#"{"model_id":"ct-model","model_version":"0.0.1"}"#),
        "/model-candidates/{id}/promote" => Some(r#"{"ack":true,"reason":"契约测试晋升"}"#),
        // 八期（第二路）：ChangeSet 动作——idempotency_key 必填；占位 id → 404（资源缺失）。
        "/change-sets/{id}/accept" => Some(r#"{"idempotency_key":"contract-ct"}"#),
        "/change-sets/{id}/reject" => Some(r#"{"idempotency_key":"contract-ct"}"#),
        "/change-sets/{id}/revert" => Some(r#"{"idempotency_key":"contract-ct"}"#),
        // 八期（第三路）：Human Inbox 领取/释放/处理——user 必填；占位 id → 404（资源缺失）。
        "/human/inbox/{id}/claim" => Some(r#"{"user":"contract-ct"}"#),
        "/human/inbox/{id}/release" => Some(r#"{"user":"contract-ct"}"#),
        "/human/inbox/{id}/resolve" => Some(r#"{"user":"contract-ct"}"#),
        _ => Some(r#"{}"#),
    }
}

/// 占位路径参数（资源不存在时应返回 400/422/500，而不是 404——404 意味着路由缺失）。
fn sample_path(path: &str, session_id: &str) -> String {
    path.replace("{id}", session_id)
        .replace("{request_id}", "no-such-request")
        .replace("{name}", "no-such-name")
        .replace("{index}", "0")
        .replace("{app_id}", "no-such-app")
        .replace("{format}", "md")
        .replace("{action}", "cancel")
        .replace("{block_id}", "no-such-block")
        .replace("{run_id}", "no-such-run")
        .replace("{node_id}", "no-such-node")
        // R10：/schemas/{kind}/{version} 契约路径（合法值应 200）。
        .replace("{kind}", "owflow")
        .replace("{version}", "v1")
        // R13：/teams/templates/proposals/{proposal_id}/adopt
        .replace("{proposal_id}", "no-such-proposal")
}

/// 资源型 404 白名单：路由已注册且方法匹配，但目标资源不存在时 handler 正确地返回 404。
/// 此类路径的契约断言为「非 405」+（非 404 或 404 由资源缺失产生）。
fn resource_404_ok(path: &str) -> bool {
    matches!(
        path,
        "/skills/{name}"
            | "/skills/{name}/enabled"
            | "/learn/packages/{name}"
            | "/learn/export/{name}"
            | "/traces/{index}"
            | "/mcp/remove"
            | "/automations/{id}/toggle"
            | "/perception/template/{app_id}"
            | "/computer-use/task/{id}/run"
            | "/cloud/tasks/{id}"
            | "/cloud/tasks/{id}/result"
            | "/notes/{id}"
            | "/notes/{id}/export/{format}"
            | "/notes/{id}/blocks"
            | "/notes/{id}/blocks/move"
            | "/notes/{id}/blocks/{block_id}"
            | "/notes/{id}/reindex"
            | "/goal/{id}"
            | "/goal/{id}/plan"
            | "/goal/{id}/run"
            | "/goal/{id}/status"
            | "/goal/{id}/abort"
            | "/goal/{id}/audit"
            | "/goal/{id}/runs"
            | "/workflow/{name}"
            | "/workflow/{name}/run"
            | "/workflow/{name}/runs"
            | "/workflow/run/{run_id}"
            | "/workflow/run/{run_id}/abort"
            | "/workflow/run/{run_id}/audit"
            | "/workflow/run/{run_id}/approval"
            | "/workflow/run/{run_id}/events"
            | "/plugins/market/uninstall"
            | "/eval/gate/report"
            | "/team/export"
            | "/eval/run"
            | "/session/{id}/permission/{request_id}"
            // R12 /fleet/*（任务/审批资源不存在 → 404 由资源缺失产生，非路由缺失）。
            | "/fleet/nodes/{node_id}/heartbeat"
            | "/fleet/nodes/{node_id}/tasks"
            | "/fleet/tasks/{id}"
            | "/fleet/tasks/{id}/claim"
            | "/fleet/tasks/{id}/progress"
            | "/fleet/tasks/{id}/result"
            | "/fleet/tasks/{id}/cancel-ack"
            | "/fleet/tasks/{id}/cancel"
            | "/fleet/tasks/{id}/events"
            | "/fleet/approvals/{id}/respond"
            // R13 WorkSwarm（§8.5）：占位 id 指向不存在资源 → 404 由资源缺失产生，非路由缺失。
            | "/teams/{id}"
            | "/teams/{id}/tasks"
            | "/teams/{id}/steer"
            | "/teams/{id}/events"
            | "/projects/{id}"
            | "/projects/{id}/artifacts"
            | "/tasks/{id}/handoff"
            | "/tasks/{id}/human-result"
            | "/teams/templates/proposals/{proposal_id}/adopt"
            | "/teams/templates/proposals/{proposal_id}/reject"
            // R1 DesktopWorld/WorldModel（§5.11/§5.12）：占位 id 指向不存在资源 → 404 由资源缺失产生，非路由缺失。
            | "/desktop-envs/{id}/reset"
            | "/desktop-envs/{id}/lease"
            | "/desktop-envs/{id}/observe"
            | "/desktop-envs/{id}/step"
            | "/desktop-envs/{id}/snapshot"
            | "/desktop-envs/{id}/restore"
            | "/desktop-envs/{id}/judge"
            | "/desktop-envs/{id}/inject-fault"
            | "/world-model/predict"
            | "/transitions/{id}"
            | "/datasets/{id}/manifest"
            | "/model-candidates/{id}/promote"
            // V1 三日 ProductEval（第四路）：占位 id 指向不存在运行 → 404 由资源缺失产生，非路由缺失。
            | "/product-eval/runs/{id}"
            | "/product-eval/runs/{id}/cancel"
            // V1 四期（第三路）：评审闭环——占位 id 指向不存在产物 → 404 由资源缺失产生，非路由缺失。
            | "/artifacts/{id}/review"
            | "/artifacts/{id}/history"
            // 五期：返工/交付物/团队指标/脱敏诊断——占位 id 指向不存在资源 → 404 非路由缺失。
            | "/artifacts/{id}/rework"
            | "/projects/{id}/deliverables"
            | "/teams/{id}/metrics"
            | "/teams/{id}/diagnostic"
            // 六期：工作区绑定四路由 + 模板目录安装——占位 id 指向不存在资源 → 404 非路由缺失。
            | "/projects/{id}/workspace"
            | "/projects/{id}/workspace/tree"
            | "/projects/{id}/workspace/git-status"
            | "/teams/templates/catalog/{id}/install"
            // 七期（第二路）：Worker 代码变更追踪——占位 id 指向不存在项目/团队 → 404 非路由缺失。
            | "/projects/{id}/workspace/changes"
            // 七期（第三路）：交付三路由——接线前路由未注册（fallback 404），接线后 404 由资源缺失产生；均非路由缺失。
            | "/artifacts/{id}/content"
            | "/artifacts/{id}/metadata"
            | "/projects/{id}/delivery-manifest"
            // 八期（第二路）：ChangeSet 审批闭环——占位 id 指向不存在资源 → 404 非路由缺失。
            | "/teams/{id}/change-sets"
            | "/change-sets/{id}"
            | "/change-sets/{id}/accept"
            | "/change-sets/{id}/reject"
            | "/change-sets/{id}/revert"
            // 八期（第三路）：统一 Human Inbox——占位 id 指向不存在待办 → 404 非路由缺失。
            | "/human/inbox/{id}"
            | "/human/inbox/{id}/claim"
            | "/human/inbox/{id}/release"
            | "/human/inbox/{id}/resolve"
    )
}

#[tokio::test]
async fn all_contract_endpoints_are_reachable() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 创建真实会话，使 /session/{id}/* 系列走真实资源路径（强断言非 404）。
    let create_resp = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/session",
            Some(r#"{"workspace":".","model":"idle"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(create_resp.status().as_u16(), 200, "POST /session 应 200");
    let create_bytes = axum::body::to_bytes(create_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let session_id = serde_json::from_slice::<serde_json::Value>(&create_bytes)
        .unwrap()
        .get("id")
        .and_then(serde_json::Value::as_str)
        .expect("create_session 响应应含 id")
        .to_string();

    let mut failed: Vec<String> = Vec::new();
    for (path, methods) in contract_endpoints() {
        for method in methods {
            let is_get_like = matches!(method.as_str(), "GET" | "DELETE");
            let body = if is_get_like {
                None
            } else {
                Some(sample_body(&path).unwrap_or("{}"))
            };
            let uri = sample_path(&path, &session_id);
            // 每请求加超时：SSE/慢路径不应拖垮契约测试（挂起即视为失败并指明路径）。
            let response = match tokio::time::timeout(
                std::time::Duration::from_secs(60),
                app.clone().oneshot(request(&state, &method, &uri, body)),
            )
            .await
            {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    failed.push(format!("{method} {path} → 请求错误 {error}"));
                    continue;
                }
                Err(_) => {
                    failed.push(format!("{method} {path} → 超时（60s 未返回）"));
                    continue;
                }
            };
            let status = response.status().as_u16();
            let ok = if status == 405 {
                false
            } else if status == 404 {
                resource_404_ok(&path)
            } else {
                true
            };
            eprintln!("contract {method} {uri} → {status}");
            if !ok {
                failed.push(format!("{method} {path} → {status}"));
            }
        }
    }
    assert!(
        failed.is_empty(),
        "契约路径不可达（404/405）：\n{}",
        failed.join("\n")
    );
}

#[tokio::test]
async fn openapi_json_covers_snapshot_and_registered_routes() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(request(&state, "GET", "/openapi.json", None))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200, "/openapi.json 应可访问");
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let served: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let served_paths: Vec<&str> = served["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();

    // 1) 契约快照全部路径已登记
    let mut missing: Vec<String> = Vec::new();
    for (path, _) in contract_endpoints() {
        if !served_paths.contains(&path.as_str()) {
            missing.push(path.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "契约快照路径漏登记 /openapi.json：{missing:?}"
    );

    // 2) lib.rs 实际注册的路由全部已登记
    let mut missing_routes: Vec<String> = Vec::new();
    for route in registered_routes() {
        if !served_paths.contains(&route.as_str()) {
            missing_routes.push(route);
        }
    }
    assert!(
        missing_routes.is_empty(),
        "已注册路由漏登记 /openapi.json：{missing_routes:?}"
    );

    // 3) 全部源文件（含模块内 router）注册的路由全部已登记
    let mut missing_module_routes: Vec<String> = Vec::new();
    for route in all_source_routes() {
        if !served_paths.contains(&route.as_str()) {
            missing_module_routes.push(route);
        }
    }
    assert!(
        missing_module_routes.is_empty(),
        "模块路由漏登记 /openapi.json：{missing_module_routes:?}"
    );

    // 4) OpenAPI 快照与 served spec 路径集合双向一致（防快照过期/超前漂移）
    let endpoints = contract_endpoints();
    let snapshot_paths: Vec<&str> = endpoints.iter().map(|(path, _)| path.as_str()).collect();
    let mut snapshot_extra: Vec<String> = Vec::new();
    for path in &snapshot_paths {
        if !served_paths.contains(path) {
            snapshot_extra.push(path.to_string());
        }
    }
    assert!(
        snapshot_extra.is_empty(),
        "快照路径未在 served /openapi.json 中：{snapshot_extra:?}"
    );
    let mut served_extra: Vec<String> = Vec::new();
    for path in &served_paths {
        if !snapshot_paths.contains(path) {
            served_extra.push(path.to_string());
        }
    }
    assert!(
        served_extra.is_empty(),
        "served /openapi.json 存在快照未登记路径（快照需同步）：{served_extra:?}"
    );
}

/// R2 冻结契约（第三路实现、第一路接线同步）：
/// `POST /teams/{id}/steer` 的 `{"command":"retry","step_id":"…","note":"…"}`。
/// 本用例只锁 HTTP 契约形状，不依赖第三路业务实现细节：
/// - retry 缺失/空/空白 step_id → 400（校验先于团队存在性，无需已存在团队）；
/// - 合法 retry + 未知团队 → 404（路由在、资源不在，证明 retry 形状已被契约接受）；
/// - 未知 command → 400。
#[tokio::test]
async fn steer_retry_contract_shape_is_frozen() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    for body in [
        r#"{"command":"retry","note":"契约"}"#,
        r#"{"command":"retry","step_id":"","note":"契约"}"#,
        r#"{"command":"retry","step_id":"   ","note":"契约"}"#,
    ] {
        let response = app
            .clone()
            .oneshot(request(
                &state,
                "POST",
                "/teams/ct-retry-team/steer",
                Some(body),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            400,
            "retry 缺 step_id 必须 400：{body}"
        );
    }

    let unknown_team = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/teams/ct-retry-team/steer",
            Some(r#"{"command":"retry","step_id":"builder","note":"契约"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(
        unknown_team.status().as_u16(),
        404,
        "合法 retry + 未知团队应 404（路由在、资源不在）"
    );

    let unknown_command = app
        .oneshot(request(
            &state,
            "POST",
            "/teams/ct-retry-team/steer",
            Some(r#"{"command":"no-such-command"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(
        unknown_command.status().as_u16(),
        400,
        "未知 steer command 应 400"
    );
}

/// V1 三日冻结契约（第四路实现并接线，见 AGENTS-COORD.md 留言区「第四路（三期开工）」）：
/// ProductEval 四路由的校验形状——只锁 HTTP 契约，不依赖后台执行：
/// - 缺字段/类型错 → 422；未知 suite（含客户端路径）/execution/mode、repetitions 越界 → 400；
/// - 未知运行：GET /product-eval/runs/{id} 与 POST …/cancel → 404（路由在、资源不在）。
#[tokio::test]
async fn product_eval_contract_shapes_are_frozen() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 结构错误 → 422
    for (body, why) in [
        (
            r#"{"execution":"reference","modes":["single"],"repetitions":1}"#,
            "缺 suite",
        ),
        (
            r#"{"suite":"v1","modes":["single"],"repetitions":1}"#,
            "缺 execution",
        ),
        (
            r#"{"suite":"v1","execution":"reference","repetitions":1}"#,
            "缺 modes",
        ),
        (
            r#"{"suite":"v1","execution":"reference","modes":"single","repetitions":1}"#,
            "modes 非数组",
        ),
        (
            r#"{"suite":"v1","execution":"reference","modes":["single"],"repetitions":"1"}"#,
            "repetitions 非整数",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(&state, "POST", "/product-eval/runs", Some(body)))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 422, "{why} 必须 422：{body}");
    }

    // 语义错误 → 400
    for (body, why) in [
        (
            r#"{"suite":"../evil","execution":"reference","modes":["single"],"repetitions":1}"#,
            "客户端路径拒绝",
        ),
        (
            r#"{"suite":"nope","execution":"reference","modes":["single"],"repetitions":1}"#,
            "未知 suite 名",
        ),
        (
            r#"{"suite":"v1","execution":"dry","modes":["single"],"repetitions":1}"#,
            "未知 execution",
        ),
        (
            r#"{"suite":"v1","execution":"reference","modes":["team"],"repetitions":1}"#,
            "未知 mode",
        ),
        (
            r#"{"suite":"v1","execution":"reference","modes":[],"repetitions":1}"#,
            "modes 空",
        ),
        (
            r#"{"suite":"v1","execution":"reference","modes":["single"],"repetitions":0}"#,
            "repetitions 0",
        ),
        (
            r#"{"suite":"v1","execution":"reference","modes":["single"],"repetitions":21}"#,
            "repetitions 21",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(request(&state, "POST", "/product-eval/runs", Some(body)))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400, "{why} 必须 400：{body}");
    }

    // 未知运行 → 404（GET 详情 + POST 取消）
    let detail = app
        .clone()
        .oneshot(request(
            &state,
            "GET",
            "/product-eval/runs/eval-ct-unknown",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(detail.status().as_u16(), 404, "未知运行详情应 404");
    let cancel = app
        .oneshot(request(
            &state,
            "POST",
            "/product-eval/runs/eval-ct-unknown/cancel",
            Some("{}"),
        ))
        .await
        .unwrap();
    assert_eq!(cancel.status().as_u16(), 404, "未知运行取消应 404");
}

#[tokio::test]
async fn v05_routes_are_registered_not_404_via_real_http() {
    let (state, _temp) = test_state().await;
    let token = state.auth_token.token().to_string();
    let app = build_router(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let base = format!("http://{addr}");

    let client = reqwest::Client::new();
    let get_cases = [
        "/health",
        "/usage",
        "/skills/health",
        "/plugins",
        "/project/rules",
        "/mcp",
        "/traces",
        "/memory/observations",
        "/memory/recall?q=t&top_k=3",
        "/openapi.json",
        "/auth/token",
    ];
    for path in get_cases {
        let status = client
            .get(format!("{base}{path}"))
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert!(
            status != 404 && status != 405,
            "GET {path} → {status}（路由缺失）"
        );
    }

    let post_cases: &[(&str, &str)] = &[
        ("/subagent/run", r#"{"prompt":"hi","read_only":true}"#),
        ("/locate/query", "{}"),
        (
            "/memory/mine-skill",
            r#"{"name":"t","target_apps":[],"sensitivity":"low","description":"d"}"#,
        ),
        ("/memory/clear", "{}"),
        (
            "/computer-use/task",
            r#"{"target_app":"notepad","description":"d","allowed_actions":[],"max_duration_ms":60000}"#,
        ),
        ("/project/rules", r#"{"content":"project-rules"}"#),
    ];
    for (path, body) in post_cases {
        let status = client
            .post(format!("{base}{path}"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16();
        assert!(
            status != 404 && status != 405,
            "POST {path} → {status}（路由缺失）"
        );
    }

    handle.abort();
}

// ---------- R7 X03：本地 API 安全边界契约 ----------

/// 无 token 的 API 请求一律 401；公开端点与 SSE 资源型路径豁免。
#[tokio::test]
async fn unauthorized_requests_get_401() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    for (method, path, body) in [
        ("GET", "/usage", None),
        ("GET", "/sessions", None),
        ("GET", "/session/x/diff", None),
        ("POST", "/goal", Some(r#"{"objective":"t"}"#)),
        ("GET", "/audit", None),
    ] {
        let response = app
            .clone()
            .oneshot(anonymous_request(method, path, body))
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            401,
            "{method} {path} 无 token 应 401"
        );
    }
}

/// 公开端点：health/openapi/token 引导无 token 可访问；错误 token 401。
#[tokio::test]
async fn public_endpoints_are_token_free_and_bad_token_rejected() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    for path in ["/health", "/openapi.json", "/auth/token"] {
        let response = app
            .clone()
            .oneshot(anonymous_request("GET", path, None))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "GET {path} 应公开可达");
    }
    // 引导端点返回真实 token。
    let response = app
        .clone()
        .oneshot(anonymous_request("GET", "/auth/token", None))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let served: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        served["token"].as_str().unwrap(),
        state.auth_token.token(),
        "引导端点应返回同一 token"
    );

    // 错误 token → 401。
    use axum::http::{header, Method, Request};
    let bad = Request::builder()
        .method(Method::GET)
        .uri("/sessions")
        .header(header::AUTHORIZATION, "Bearer wrong-token")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(bad).await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
}

/// SSE 资源型路径豁免鉴权（EventSource 无法携带自定义头；只读遥测）。
#[tokio::test]
async fn sse_paths_are_exempt_from_auth() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    for path in [
        "/cloud/tasks/x/events",
        "/workflow/run/x/events",
        "/events/stream",
    ] {
        let response = app
            .clone()
            .oneshot(anonymous_request("GET", path, None))
            .await
            .unwrap();
        let status = response.status().as_u16();
        // 不应 401（鉴权豁免）；404 表示路由存在但资源缺失（workflow run 不存在）。
        assert!(
            status != 401,
            "GET {path} 无 token 不应 401（SSE 豁免），实际 {status}"
        );
    }
}

/// CORS：不允许的跨源请求无 Access-Control-Allow-Origin（浏览器侧拒绝）；
/// webview/localhost 白名单放行并回显 ACAO。
#[tokio::test]
async fn cors_whitelist_enforces_origins() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    fn preflight(origin: &str) -> axum::http::Request<axum::body::Body> {
        use axum::http::{header, Method, Request};
        Request::builder()
            .method(Method::OPTIONS)
            .uri("/session")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn acao(response: &axum::http::Response<axum::body::Body>) -> String {
        response
            .headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }

    // 恶意跨源 → 预检不回显 ACAO（浏览器拒绝跨源读取）。
    for evil in [
        "https://evil.example",
        "https://attacker.com",
        "http://192.168.0.1:8080",
    ] {
        let response = app.clone().oneshot(preflight(evil)).await.unwrap();
        assert_eq!(
            acao(&response),
            "",
            "Origin {evil} 预检不应回显 Access-Control-Allow-Origin"
        );
    }

    // webview / localhost 白名单 → 预检放行且回显 ACAO。
    for allowed in [
        "tauri://localhost",
        "http://tauri.localhost",
        "http://localhost:1420",
        "http://127.0.0.1:4096",
    ] {
        let response = app.clone().oneshot(preflight(allowed)).await.unwrap();
        assert_eq!(
            response.status().as_u16(),
            200,
            "Origin {allowed} 预检应放行"
        );
        assert!(
            !acao(&response).is_empty(),
            "Origin {allowed} 应回显 Access-Control-Allow-Origin"
        );
    }

    // 真实请求（非预检）同样不回显 ACAO 给恶意 origin。
    use axum::http::{header, Method, Request};
    let actual = Request::builder()
        .method(Method::GET)
        .uri("/health")
        .header(header::ORIGIN, "https://evil.example")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(actual).await.unwrap();
    assert_eq!(acao(&response), "", "恶意 origin 的真实请求不应回显 ACAO");
}

/// 限流：超全局 RPM 后 429 + Retry-After + 审计记录。
#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    // 环境变量进程级共享：与 test_state 共用 STATE_ENV_LOCK 串行化，
    // 防止 set_var("...","5") 泄漏进并行用例正在构建的 AppState（ tokio Mutex 跨 await 安全）。
    let _guard = STATE_ENV_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    std::env::set_var("OWO_API_RPM_GLOBAL", "5");
    let (state, _temp) = build_state_inner().await;
    std::env::remove_var("OWO_API_RPM_GLOBAL");
    let app = build_router(Arc::clone(&state));

    let mut statuses: Vec<u16> = Vec::new();
    for _ in 0..10 {
        let response = app
            .clone()
            .oneshot(request(
                &state,
                "POST",
                "/goal",
                Some(r#"{"objective":"rl"}"#),
            ))
            .await
            .unwrap();
        statuses.push(response.status().as_u16());
    }
    let ok_count = statuses.iter().filter(|s| **s == 201).count();
    let limited = statuses.iter().filter(|s| **s == 429).count();
    assert!((1..=5).contains(&ok_count), "前 5 个应放行：{statuses:?}");
    assert!(limited >= 1, "超限应有 429：{statuses:?}");

    // Retry-After 头存在。
    let response = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/goal",
            Some(r#"{"objective":"rl"}"#),
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 429);
    assert!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_some(),
        "429 应携带 Retry-After"
    );

    // 限流拒绝已写审计。
    let response = app
        .clone()
        .oneshot(request(&state, "GET", "/audit?limit=50", None))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e["event"].as_str() == Some("rate_limited")),
        "限流拒绝应产生审计记录（event=rate_limited）"
    );
}

// ---------- R12 契约治理：/schemas/* 版本化发布 + Deprecation 头机制 ----------

/// /schemas/* 语义契约：列表含三份 schema 与 api_version；各版本 JSON Schema 可解析且 draft-07；
/// 未知 kind/version 404；api_version 与 OpenAPI x-owo-api-version 一致。
#[tokio::test]
async fn schemas_endpoints_serve_versioned_json_schema() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 列表：200 + 三份 schema + api_version。
    let response = app
        .clone()
        .oneshot(request(&state, "GET", "/schemas", None))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200, "GET /schemas 应 200");
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        list["api_version"].as_str(),
        Some(owo_agent_server::OWO_API_VERSION),
        "/schemas api_version 应等于 OWO_API_VERSION"
    );
    let kinds = list["schemas"].as_object().unwrap();
    for kind in ["plugin-manifest", "owskill", "owflow"] {
        assert!(kinds.contains_key(kind), "/schemas 应登记 {kind}");
    }

    // 每份 schema：200 + draft-07 + 合法 JSON object。
    for kind in ["plugin-manifest", "owskill", "owflow"] {
        let uri = format!("/schemas/{kind}/v1");
        let response = app
            .clone()
            .oneshot(request(&state, "GET", &uri, None))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "GET {uri} 应 200");
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let schema: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            schema["$schema"].as_str(),
            Some("http://json-schema.org/draft-07/schema#"),
            "{uri} 应为 draft-07"
        );
        assert_eq!(schema["type"].as_str(), Some("object"), "{uri} 应为 object");
    }

    // 未知 kind/version → 404（路由存在，资源缺失语义）。
    for uri in ["/schemas/bogus/v1", "/schemas/owflow/v9"] {
        let response = app
            .clone()
            .oneshot(request(&state, "GET", uri, None))
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            404,
            "GET {uri} 应 404（未知 schema）"
        );
    }

    // api_version 与 OpenAPI x-owo-api-version 一致。
    let response = app
        .clone()
        .oneshot(request(&state, "GET", "/openapi.json", None))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let openapi: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        openapi["x-owo-api-version"].as_str(),
        Some(owo_agent_server::OWO_API_VERSION),
        "OpenAPI x-owo-api-version 应等于 OWO_API_VERSION"
    );
}

/// Deprecation 头机制纯函数契约：命中生成正确头值、未命中 None（无需真实弃用路由即可覆盖生成逻辑）。
#[test]
fn deprecation_header_value_contract() {
    let routes: &[(&str, &str, &str, &str)] = &[("/old", "0.5", "0.7", "/new")];

    // 命中（前缀匹配）：头值格式 = "{route}: since {since}, until {until} (use {alternative})"。
    assert_eq!(
        owo_agent_server::deprecation_header_value_for(routes, "/old/session").as_deref(),
        Some("/old: since 0.5, until 0.7 (use /new)")
    );
    // 未命中（前缀不匹配）：None。
    assert!(owo_agent_server::deprecation_header_value_for(routes, "/new/session").is_none());
    assert!(
        owo_agent_server::deprecation_header_value_for(routes, "/ol").is_none(),
        "前缀边界：/ol 不应命中 /old"
    );
}

/// HTTP 负例：当前 DEPRECATED_ROUTES 为空，正常路由不附加 Deprecation 头（中间件已接线且无误报）。
#[tokio::test]
async fn deprecation_header_absent_on_current_routes() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));
    for (method, path) in [
        ("GET", "/health"),
        ("GET", "/usage"),
        ("GET", "/sessions"),
        ("GET", "/schemas"),
    ] {
        let response = app
            .clone()
            .oneshot(request(&state, method, path, None))
            .await
            .unwrap();
        assert!(
            response.headers().get("Deprecation").is_none(),
            "{method} {path} 不应携带 Deprecation 头（当前无弃用路由）"
        );
    }
}

/// V1 四期（第三路）：Artifact 评审闭环契约冻结——路由形状 + 错误语义面
/// （404 资源缺失 / 400 未知 decision / 鉴权外的路由可达性）。
#[tokio::test]
async fn artifact_review_contract_shape_is_frozen() {
    let (state, _temp) = test_state().await;
    let app = build_router(Arc::clone(&state));

    // 1) POST /artifacts/{id}/review：未知产物 → 404 + 结构化错误（路由已注册，非 405）。
    let body = r#"{
        "team_id": "team-nope",
        "decision": "approve",
        "reviewer": "critic",
        "comment": "契约探针",
        "expected_version": 1,
        "idempotency_key": "contract-probe-approve"
    }"#;
    let response = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/artifacts/contract-probe:planner:v1/review",
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status().as_u16(),
        404,
        "未知产物应 404（路由已注册）"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // handler 先查团队再查产物：探针两者都不存在，命中团队缺失分支（产物缺失分支由 API 集成测试覆盖）。
    assert!(
        value["error"].as_str().unwrap_or("").contains("团队不存在"),
        "404 错误应结构化：{value}"
    );

    // 2) 未知 decision → 400（handler 先校验决定枚举，再查团队/产物）。
    let bad_decision = r#"{
        "team_id": "team-nope",
        "decision": "maybe",
        "reviewer": "critic",
        "idempotency_key": "contract-probe-bad-decision"
    }"#;
    let response = app
        .clone()
        .oneshot(request(
            &state,
            "POST",
            "/artifacts/contract-probe:planner:v1/review",
            Some(bad_decision),
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400, "未知 decision 应 400");

    // 3) GET /artifacts/{id}/history：未知产物 → 404 + 结构化错误。
    let response = app
        .clone()
        .oneshot(request(
            &state,
            "GET",
            "/artifacts/contract-probe:planner:v1/history",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        value["error"].as_str().is_some(),
        "404 应带 error 字段：{value}"
    );

    // 4) /openapi.json 登记评审路由与 operationId（契约面自描述）。
    let response = app
        .clone()
        .oneshot(request(&state, "GET", "/openapi.json", None))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let spec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        spec["paths"]["/artifacts/{id}/review"]["post"]["operationId"],
        serde_json::json!("artifactSubmitReview")
    );
    assert_eq!(
        spec["paths"]["/artifacts/{id}/history"]["get"]["operationId"],
        serde_json::json!("artifactReviewHistory")
    );
    assert!(
        spec["components"]["schemas"]["ArtifactReviewRecord"].is_object(),
        "ArtifactReviewRecord 组件应登记"
    );
}
