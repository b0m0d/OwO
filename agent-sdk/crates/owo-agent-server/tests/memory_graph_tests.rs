//! 记忆图谱 API 契约测试（Lane：第二大脑 · 子任务 1）。
//!
//! 覆盖：时间过滤/分桶边界、实体聚合稳定、关系 CRUD 往返、recall 附实体命中、非法参数 400。
//! 存储全部落在 tempfile 临时目录。

#[path = "../src/memory_graph_api.rs"]
mod memory_graph_api;

use owo_agent_server::AppState;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

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

async fn test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = owo_agent_core::Agent::new(
        Arc::new(IdleProvider),
        owo_agent_core::tools::ToolRegistry::new(),
        owo_agent_core::permissions::Policy::new(&workspace),
        Default::default(),
    );
    let store = owo_agent_core::sqlite_store::SqliteSessionStore::open(&workspace.join("index.db"))
        .unwrap();
    let state = Arc::new(AppState::new(
        agent,
        store,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    // 预置记忆：两条不同 app/时间/内容的条目（append Observation，recall 走语义索引）。
    {
        let mut memory = state.memory.lock().unwrap();
        memory
            .append(owo_agent_core::Observation {
                ts: fixture_ts(6),
                app_id: "qq".to_string(),
                kind: "sim_event".to_string(),
                summary: "张子豪约定今晚八点开会".to_string(),
                detail: json!({}),
                state_hash: 1,
            })
            .unwrap();
        memory
            .append(owo_agent_core::Observation {
                ts: fixture_ts(4),
                app_id: "browser".to_string(),
                kind: "sim_event".to_string(),
                summary: "搜索 Rust 并发编程教程".to_string(),
                detail: json!({}),
                state_hash: 2,
            })
            .unwrap();
    }
    (state, temp)
}

/// 空记忆的干净 state（empty 测试用，避免预置泄漏）。
async fn test_state_empty() -> (Arc<AppState>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let agent = owo_agent_core::Agent::new(
        Arc::new(IdleProvider),
        owo_agent_core::tools::ToolRegistry::new(),
        owo_agent_core::permissions::Policy::new(&workspace),
        Default::default(),
    );
    let store = owo_agent_core::sqlite_store::SqliteSessionStore::open(&workspace.join("index.db"))
        .unwrap();
    let state = Arc::new(AppState::new(
        agent,
        store,
        workspace.join("traces"),
        temp.path().to_path_buf(),
        workspace,
    ));
    (state, temp)
}

fn request(method: &str, path: &str, body: Option<&str>) -> axum::http::Request<axum::body::Body> {
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

// ---------- 固定时间锚（R0 门禁收口） ----------
// MemoryStore 默认 30 天保留期会在加载时 prune 超期条目：fixture 若用固定
// 绝对日期（如 2026-08-10），日期滑出窗口后整个预置记忆被清空，四个用例
// 集体失败（时间炸弹）。现以进程级锚点相对偏移，锚点只初始化一次，
// 跨 UTC 午夜也保持 fixture 与断言一致。

fn anchor() -> chrono::DateTime<chrono::Utc> {
    static ANCHOR: std::sync::OnceLock<chrono::DateTime<chrono::Utc>> = std::sync::OnceLock::new();
    *ANCHOR.get_or_init(chrono::Utc::now)
}

/// 锚点前 `days_ago` 天的 RFC3339（UTC `Z` 后缀：query 串中 `+` 会被
/// www-form 解码成空格，禁用 to_rfc3339 默认偏移格式）。
fn fixture_ts(days_ago: i64) -> String {
    (anchor() - chrono::Duration::days(days_ago)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn fixture_day(days_ago: i64) -> String {
    (anchor() - chrono::Duration::days(days_ago))
        .format("%Y-%m-%d")
        .to_string()
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(request(method, path, body))
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn entries_filter_by_app_and_time() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);

    let (_, all) = call(&app, "GET", "/memory/graph/entries", None).await;
    assert_eq!(all["count"].as_u64().unwrap(), 2);

    let (_, by_app) = call(&app, "GET", "/memory/graph/entries?app=qq", None).await;
    assert_eq!(by_app["count"].as_u64().unwrap(), 1);
    assert_eq!(by_app["entries"][0]["app_id"], "qq");

    let (_, ranged) = call(
        &app,
        "GET",
        &format!(
            "/memory/graph/entries?from={}&to={}",
            fixture_ts(5),
            fixture_ts(1)
        ),
        None,
    )
    .await;
    assert_eq!(ranged["count"].as_u64().unwrap(), 1);
    assert_eq!(ranged["entries"][0]["app_id"], "browser");
}

#[tokio::test]
async fn entries_limit_respected() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (_, limited) = call(&app, "GET", "/memory/graph/entries?limit=1", None).await;
    assert_eq!(limited["count"].as_u64().unwrap(), 1);
}

#[tokio::test]
async fn timeline_buckets_by_day() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (_, timeline) = call(&app, "GET", "/memory/graph/timeline", None).await;
    let buckets = timeline["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 2);
    assert_eq!(buckets[0]["day"], fixture_day(6));
    assert_eq!(buckets[0]["count"].as_u64().unwrap(), 1);
    // 边界：只取一天。
    let (_, ranged) = call(
        &app,
        "GET",
        &format!(
            "/memory/graph/timeline?from={}T00:00:00Z&to={}T23:59:59Z",
            fixture_day(4),
            fixture_day(4)
        ),
        None,
    )
    .await;
    assert_eq!(ranged["buckets"].as_array().unwrap().len(), 1);
    // 空区间。
    let (_, empty) = call(
        &app,
        "GET",
        &format!(
            "/memory/graph/timeline?from={}T00:00:00Z",
            fixture_day(-365)
        ),
        None,
    )
    .await;
    assert_eq!(empty["buckets"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn entities_aggregation_stable() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (_, first) = call(&app, "GET", "/memory/graph/entities?limit=10", None).await;
    let (_, second) = call(&app, "GET", "/memory/graph/entities?limit=10", None).await;
    assert_eq!(first, second, "聚合应稳定");
    let entities = first["entities"].as_array().unwrap();
    assert!(!entities.is_empty());
    // 频次最高的词元（normalized 前缀词）应出现在前列。
    let top = entities[0]["entity"].as_str().unwrap();
    assert!(!top.is_empty());
    // related 结构完整。
    for entity in entities {
        assert!(entity.get("related").is_some());
        assert!(entity["count"].as_u64().unwrap() >= 1);
    }
}

#[tokio::test]
async fn relation_crud_roundtrip() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (status, created) = call(
        &app,
        "POST",
        "/memory/graph/link",
        Some(r#"{"a":"张子豪","b":"今晚八点","relation":"约定","note":"开会"}"#),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["count"].as_u64().unwrap(), 1);

    let (_, links) = call(&app, "GET", "/memory/graph/links", None).await;
    assert_eq!(links["count"].as_u64().unwrap(), 1);
    assert_eq!(links["links"][0]["relation"], "约定");
    assert_eq!(links["links"][0]["note"], "开会");

    let (status, deleted) = call(
        &app,
        "DELETE",
        "/memory/graph/link",
        Some(r#"{"a":"张子豪","b":"今晚八点","relation":"约定"}"#),
    )
    .await;
    assert_eq!(status, 200, "{deleted}");
    let (_, links) = call(&app, "GET", "/memory/graph/links", None).await;
    assert_eq!(links["count"].as_u64().unwrap(), 0);

    // 删除不存在 → 404。
    let (status, _) = call(
        &app,
        "DELETE",
        "/memory/graph/link",
        Some(r#"{"a":"x","b":"y","relation":"z"}"#),
    )
    .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn relation_invalid_input_400() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (status, value) = call(
        &app,
        "POST",
        "/memory/graph/link",
        Some(r#"{"a":"","b":"y","relation":""}"#),
    )
    .await;
    assert_eq!(status, 400);
    assert!(value["error"].as_str().unwrap().contains("不能为空"));
}

#[tokio::test]
async fn recall_attaches_entity_hits() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (status, value) = call(
        &app,
        "GET",
        "/memory/graph/recall?q=%E5%BC%A0%E5%AD%90%E8%B1%AA&top_k=3",
        None,
    )
    .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(
        value["count"].as_u64().unwrap(),
        1,
        "命中张子豪条目：{value}"
    );
    let hit = &value["hits"][0];
    let matched = hit["matched_entities"].as_array().unwrap();
    // 中文 tokenize 产出二元字符组（"张子豪" → ["张子","子豪"]）。
    assert!(!matched.is_empty(), "应有实体命中：{value}");
    assert!(
        matched.contains(&json!("张子")),
        "应含二元组实体：{matched:?}"
    );
    assert!(hit["id"].as_str().unwrap().contains(&fixture_day(6)));
}

#[tokio::test]
async fn recall_empty_query_400() {
    let (state, _temp) = test_state().await;
    let app = memory_graph_api::router(state);
    let (status, value) = call(&app, "GET", "/memory/graph/recall?q=", None).await;
    assert_eq!(status, 400);
    assert!(value["error"].as_str().unwrap().contains("q"));
}

#[tokio::test]
async fn empty_memory_no_panic() {
    let (state, _temp) = test_state_empty().await;
    let app = memory_graph_api::router(state);
    // 空记忆：所有端点不 panic，返回空结构。
    let (_, entries) = call(&app, "GET", "/memory/graph/entries", None).await;
    assert_eq!(entries["count"].as_u64().unwrap(), 0);
    let (_, timeline) = call(&app, "GET", "/memory/graph/timeline", None).await;
    assert_eq!(timeline["buckets"].as_array().unwrap().len(), 0);
    let (_, entities) = call(&app, "GET", "/memory/graph/entities", None).await;
    assert_eq!(entities["count"].as_u64().unwrap(), 0);
    let (_, links) = call(&app, "GET", "/memory/graph/links", None).await;
    assert_eq!(links["count"].as_u64().unwrap(), 0);
}
