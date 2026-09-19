//! ProductEval HTTP API 测试（V1 三日 · 第四路）。
//!
//! 覆盖（冻结契约见 AGENTS-COORD.md 留言区「第四路（三期开工）」）：
//! - reference 免模型全链路：受理 202 → 轮询详情 → completed + 完整报告（聚合指标/
//!   每 case×mode 对比/Artifact refs/进度 done==total）；
//! - 校验矩阵：结构错误 422、语义错误 400（未知 suite/路径、execution、mode、
//!   repetitions 越界、category、空 only）；
//! - 404（未知运行详情/取消）；取消幂等（终态后取消零副作用）；
//! - live 工厂错误 → failed（不调模型、不伪造结果）；live 慢执行器 → 取消立即 cancelled
//!   且报告保留；
//! - 重启恢复：queued/running → interrupted（不自动重跑），completed 原样可查；
//! - 列表排序与进度字段；category 过滤。

use axum::http::StatusCode;
use owo_agent_eval_facade::product_eval::{CaseExecutor, ExecContext, RawExecOutcome};
use owo_agent_server::product_eval_api::{router_with_hub, ProductEvalHub};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

type App = axum::Router;

// ---------------------------------------------------------------------------
// fixture：最小 suite（document 通过 + code 检查必败，供失败进报告断言）
// ---------------------------------------------------------------------------

const SUITE_JSON: &str = r#"{
  "schema_version": 1,
  "name": "test-suite",
  "description": "API 测试套件",
  "defaults": { "repetitions": 1, "timeout_secs": 5, "max_model_calls": 2 },
  "tasks": ["tasks/document-draft-note.json", "tasks/code-broken.json"]
}"#;

const CASE_DRAFT_NOTE: &str = r##"{
  "schema_version": 1,
  "id": "document-draft-note",
  "category": "document",
  "title": "草拟便签",
  "instruction": "写一份包含 OwO 的便签",
  "inputs": [],
  "allow_read": ["inputs/**"],
  "allow_write": ["artifacts/**"],
  "expected_artifacts": ["artifacts/note.md"],
  "checkers": [
    { "type": "exists", "path": "artifacts/note.md" },
    { "type": "contains", "path": "artifacts/note.md", "text": "OwO" }
  ],
  "reference_outputs": { "artifacts/note.md": "# 便签\n\nOwO 参考输出\n" },
  "timeout_secs": 5,
  "max_model_calls": 2,
  "repetitions": 1,
  "allow_commands": []
}"##;

/// 参考回放也必败（contains 断言不满足）——失败运行必须进报告，不得被剔除。
const CASE_CODE_BROKEN: &str = r##"{
  "schema_version": 1,
  "id": "code-broken",
  "category": "code",
  "title": "必败任务（检查器断言）",
  "instruction": "写一个文件",
  "inputs": [],
  "allow_read": ["inputs/**"],
  "allow_write": ["artifacts/**"],
  "expected_artifacts": ["artifacts/out.txt"],
  "checkers": [
    { "type": "contains", "path": "artifacts/out.txt", "text": "never-present-marker" }
  ],
  "reference_outputs": { "artifacts/out.txt": "no marker here\n" },
  "timeout_secs": 5,
  "max_model_calls": 2,
  "repetitions": 1,
  "allow_commands": []
}"##;

struct TestEnv {
    app: App,
    /// 持有所有权保活（Router/HUB 生命周期与 tempdir 绑定），测试体不读取。
    #[allow(dead_code)]
    hub: Arc<ProductEvalHub>,
    /// 同上：保活 runs_root 目录句柄语义（tempdir drop 顺序依赖）。
    #[allow(dead_code)]
    runs_root: PathBuf,
}

fn write_suite(root: &Path) {
    let dir = root.join("v1").join("tasks");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(root.join("v1").join("suite.json"), SUITE_JSON).unwrap();
    std::fs::write(dir.join("document-draft-note.json"), CASE_DRAFT_NOTE).unwrap();
    std::fs::write(dir.join("code-broken.json"), CASE_CODE_BROKEN).unwrap();
}

fn env_with_factory(
    live_factory: owo_agent_server::product_eval_api::LiveExecutorFactory,
) -> TestEnv {
    let temp = tempfile::tempdir().unwrap();
    let suite_root = temp.path().join("evals");
    let runs_root = temp.path().join("product_eval").join("runs");
    write_suite(&suite_root);
    let hub = Arc::new(ProductEvalHub::new(
        runs_root.clone(),
        suite_root,
        live_factory,
    ));
    let app = router_with_hub(Arc::clone(&hub));
    // tempdir 需要活到断言结束：泄漏（测试进程退出即回收）。
    std::mem::forget(temp);
    TestEnv {
        app,
        hub,
        runs_root,
    }
}

fn default_env() -> TestEnv {
    env_with_factory(Arc::new(|| Err("live 未接线（测试默认工厂）".to_string())))
}

async fn post(env: &TestEnv, body: Value) -> (StatusCode, Value) {
    let response = env
        .app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/product-eval/runs")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get(env: &TestEnv, uri: &str) -> (StatusCode, Value) {
    let response = env
        .app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn cancel(env: &TestEnv, run_id: &str) -> (StatusCode, Value) {
    let response = env
        .app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/product-eval/runs/{run_id}/cancel"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// 轮询详情直到状态命中 needle（毫秒级步进；超时返回最后快照）。
async fn wait_status(env: &TestEnv, run_id: &str, needles: &[&str], timeout: Duration) -> Value {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = Value::Null;
    while std::time::Instant::now() < deadline {
        let (_, body) = get(env, &format!("/product-eval/runs/{run_id}")).await;
        last = body.clone();
        let status = body["status"].as_str().unwrap_or("");
        if needles.contains(&status) {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    panic!("等待状态 {needles:?} 超时；最后快照：{last}");
}

// ---------------------------------------------------------------------------
// 1. reference 全链路
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reference_run_completes_with_full_report_and_progress() {
    let env = default_env();
    let (status, body) = post(
        &env,
        json!({
            "suite": "v1",
            "execution": "reference",
            "modes": ["single", "workswarm"],
            "repetitions": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "受理必须 202：{body}");
    assert_eq!(body["status"], "queued");
    let run_id = body["run_id"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("eval-"), "run_id 形如 eval-…：{run_id}");

    let detail = wait_status(&env, &run_id, &["completed"], Duration::from_secs(20)).await;
    // 进度：done == total（2 模式 × 2 任务 × 1 次）
    assert_eq!(detail["progress"]["done"], 4, "进度：{detail}");
    assert_eq!(detail["progress"]["total"], 4);

    let report = &detail["report"];
    assert!(report.is_object(), "completed 运行必须带报告：{detail}");
    assert_eq!(report["metrics"]["runs_total"], 4);
    assert_eq!(
        report["metrics"]["passed"], 2,
        "document 参考回放双拓扑通过"
    );
    assert_eq!(
        report["metrics"]["failed"], 2,
        "code-broken 参考回放双拓扑必败（失败进报告）"
    );
    assert_eq!(
        report["metrics"]["success_rate"], 0.5,
        "分母=全部已尝试，禁止剔除重算"
    );
    assert!(report["pending"].as_array().unwrap().is_empty());

    // 每 case×mode 对比 + Artifact refs（核心 wire：agent_mode 为 single/multi）
    let per_case = report["per_case"].as_array().unwrap();
    let modes: Vec<&str> = per_case
        .iter()
        .map(|r| r["agent_mode"].as_str().unwrap())
        .collect();
    assert!(modes.contains(&"single") && modes.contains(&"multi"));
    let runs = report["runs"].as_array().unwrap();
    let draft = runs
        .iter()
        .find(|r| r["key"]["case_id"] == "document-draft-note" && r["key"]["agent_mode"] == "multi")
        .expect("workswarm（multi）拓扑单元格必须存在");
    assert_eq!(draft["status"], "passed");
    assert_eq!(
        draft["artifact_refs"],
        json!(["artifacts/note.md"]),
        "最终 Artifact 引用进入报告"
    );
    assert_eq!(draft["model_calls"], 0, "reference 模式零模型调用");
    assert!(draft["total_tokens"].is_null(), "免模型运行 token 为 null");
    // 失败单元格保留失败步骤（检查器描述），不被删除重算
    let broken = runs
        .iter()
        .find(|r| r["key"]["case_id"] == "code-broken")
        .unwrap();
    assert_eq!(broken["status"], "failed");
    assert!(
        !broken["failed_steps"].as_array().unwrap().is_empty(),
        "失败步骤必须记录"
    );
}

#[tokio::test]
async fn category_filter_limits_matrix() {
    let env = default_env();
    let (status, body) = post(
        &env,
        json!({
            "suite": "v1",
            "execution": "reference",
            "modes": ["single"],
            "repetitions": 1,
            "category": "document"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["run_id"].as_str().unwrap().to_string();
    let detail = wait_status(&env, &run_id, &["completed"], Duration::from_secs(20)).await;
    assert_eq!(detail["progress"]["done"], 1);
    assert_eq!(detail["progress"]["total"], 1);
    let runs = detail["report"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["key"]["case_id"], "document-draft-note");
    assert_eq!(runs[0]["category"], "document");
}

#[tokio::test]
async fn list_returns_summaries_newest_first() {
    let env = default_env();
    let (_, first) = post(
        &env,
        json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1 }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1100)).await; // 保证 created_at 秒级可分
    let (_, second) = post(
        &env,
        json!({ "suite": "v1", "execution": "live", "modes": ["workswarm"], "repetitions": 1, "only": "code-broken" }),
    )
    .await;
    let (status, body) = get(&env, "/product-eval/runs").await;
    assert_eq!(status, StatusCode::OK);
    let runs = body["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "列表含全部运行：{body}");
    assert_eq!(runs[0]["run_id"], second["run_id"], "created_at 倒序");
    assert_eq!(runs[1]["run_id"], first["run_id"]);
    for run in runs {
        assert!(run["progress"]["total"].as_u64().unwrap() >= 1);
        assert!(run["planned_total"].as_u64().unwrap() >= 1);
        assert!(run["status"].is_string());
    }
}

// ---------------------------------------------------------------------------
// 2. 校验矩阵：422 结构 / 400 语义
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_matrix_rejects_semantic_and_structural_errors() {
    let env = default_env();
    let valid = json!({
        "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1
    });
    let cases: Vec<(Value, StatusCode, &str)> = vec![
        // 结构错误 → 422
        (
            json!({ "execution": "reference", "modes": ["single"], "repetitions": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "缺 suite",
        ),
        (
            json!({ "suite": "v1", "modes": ["single"], "repetitions": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "缺 execution",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "repetitions": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "缺 modes",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "缺 repetitions",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": "single", "repetitions": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "modes 非数组",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": "1" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "repetitions 非整数",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1, "category": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "category 非字符串",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1, "only": 7 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "only 非字符串",
        ),
        // 语义错误 → 400
        (
            json!({ "suite": "../evil", "execution": "reference", "modes": ["single"], "repetitions": 1 }),
            StatusCode::BAD_REQUEST,
            "客户端路径拒绝",
        ),
        (
            json!({ "suite": "nope", "execution": "reference", "modes": ["single"], "repetitions": 1 }),
            StatusCode::BAD_REQUEST,
            "未知 suite 名",
        ),
        (
            json!({ "suite": "v1", "execution": "dry", "modes": ["single"], "repetitions": 1 }),
            StatusCode::BAD_REQUEST,
            "未知 execution",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": [], "repetitions": 1 }),
            StatusCode::BAD_REQUEST,
            "modes 空",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["team"], "repetitions": 1 }),
            StatusCode::BAD_REQUEST,
            "未知 mode",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 0 }),
            StatusCode::BAD_REQUEST,
            "repetitions 0",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 21 }),
            StatusCode::BAD_REQUEST,
            "repetitions 21",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1, "category": "math" }),
            StatusCode::BAD_REQUEST,
            "未知 category",
        ),
        (
            json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1, "only": "  " }),
            StatusCode::BAD_REQUEST,
            "only 空白",
        ),
    ];
    for (body, expected, why) in cases {
        let (status, resp) = post(&env, body).await;
        assert_eq!(status, expected, "{why}：{resp}");
        assert!(
            resp["error"].is_string(),
            "{why} 必须返回明确 error：{resp}"
        );
    }
    // 合法请求仍可受理（校验矩阵没有误伤）
    let (status, resp) = post(&env, valid).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
}

#[tokio::test]
async fn empty_filter_result_is_rejected_with_clear_error() {
    let env = default_env();
    let (status, resp) = post(
        &env,
        json!({
            "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1,
            "only": "no-such-case-substring"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    assert!(resp["error"].as_str().unwrap().contains("没有可执行的任务"));
}

// ---------------------------------------------------------------------------
// 3. 404 与取消幂等
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_run_404_on_detail_and_cancel() {
    let env = default_env();
    let (status, body) = get(&env, "/product-eval/runs/eval-nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = cancel(&env, "eval-nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn cancel_after_completion_is_idempotent_no_side_effect() {
    let env = default_env();
    let (_, body) = post(
        &env,
        json!({ "suite": "v1", "execution": "reference", "modes": ["single"], "repetitions": 1, "only": "document-draft-note" }),
    )
    .await;
    let run_id = body["run_id"].as_str().unwrap().to_string();
    let detail = wait_status(&env, &run_id, &["completed"], Duration::from_secs(20)).await;
    let done = detail["progress"]["done"].as_u64().unwrap();

    let (status, first) = cancel(&env, &run_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        first["status"], "completed",
        "终态后 cancel 原样返回：{first}"
    );
    let (_, second) = cancel(&env, &run_id).await;
    assert_eq!(second["status"], "completed", "重复取消零副作用：{second}");
    let (_, after) = get(&env, &format!("/product-eval/runs/{run_id}")).await;
    assert_eq!(after["status"], "completed");
    assert_eq!(
        after["progress"]["done"].as_u64().unwrap(),
        done,
        "完成后取消不得改写进度"
    );
}

// ---------------------------------------------------------------------------
// 4. live 工厂：失败不伪造 + 慢执行器可取消
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_factory_error_fails_run_without_fabricating_results() {
    let env = default_env(); // 工厂恒 Err
    let (_, body) = post(
        &env,
        json!({ "suite": "v1", "execution": "live", "modes": ["single"], "repetitions": 1, "only": "document-draft-note" }),
    )
    .await;
    let run_id = body["run_id"].as_str().unwrap().to_string();
    let detail = wait_status(&env, &run_id, &["failed"], Duration::from_secs(10)).await;
    let message = detail["error"].as_str().unwrap();
    assert!(message.contains("live"), "错误需可定位：{message}");
    // 不伪造：没有报告（零单元格执行、零模型调用）
    assert!(
        detail["report"].is_null(),
        "工厂失败的运行不得带报告：{detail}"
    );
}

struct SlowStubExecutor {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl CaseExecutor for SlowStubExecutor {
    async fn execute<'ctx>(&self, _ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        self.started.store(true, Ordering::SeqCst);
        // 等待测试放行（取消语义不依赖执行器自觉：单元格边界由 hub 令牌兜底）。
        while !self.release.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        RawExecOutcome {
            aborted: true,
            ..RawExecOutcome::default()
        }
    }
}

#[tokio::test]
async fn cancel_marks_running_run_cancelled_immediately_and_keeps_partial_report() {
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let started_for_factory = Arc::clone(&started);
    let release_for_factory = Arc::clone(&release);
    let env = env_with_factory(Arc::new(move || {
        Ok(Arc::new(SlowStubExecutor {
            started: Arc::clone(&started_for_factory),
            release: Arc::clone(&release_for_factory),
        }) as Arc<dyn CaseExecutor>)
    }));
    let (_, body) = post(
        &env,
        json!({
            "suite": "v1", "execution": "live", "modes": ["single", "workswarm"], "repetitions": 1,
            "only": "document-draft-note"
        }),
    )
    .await;
    let run_id = body["run_id"].as_str().unwrap().to_string();
    // 等执行器真实启动（live 工厂被调用）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !started.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        started.load(Ordering::SeqCst),
        "live 工厂应已构造执行器并开始执行"
    );

    let (status, cancelled) = cancel(&env, &run_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        cancelled["status"], "cancelled",
        "取消立即落账（一次轮询内可见）：{cancelled}"
    );

    // 放行执行器；runner 收尾不得把状态改回 completed
    release.store(true, Ordering::SeqCst);
    let detail = wait_status(
        &env,
        &run_id,
        &["cancelled", "completed", "failed"],
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        detail["status"], "cancelled",
        "取消后状态不被后台收尾覆盖：{detail}"
    );
    // 重复取消幂等
    let (_, again) = cancel(&env, &run_id).await;
    assert_eq!(again["status"], "cancelled");
}

// ---------------------------------------------------------------------------
// 5. 重启恢复：queued/running → interrupted；completed 原样可查
// ---------------------------------------------------------------------------

#[tokio::test]
async fn restart_marks_incomplete_runs_interrupted_and_keeps_completed() {
    let temp = tempfile::tempdir().unwrap();
    let suite_root = temp.path().join("evals");
    let runs_root = temp.path().join("product_eval").join("runs");
    write_suite(&suite_root);

    // 手工落两个 hub.json：一个 running（待标 interrupted）、一个 completed（保留）
    let dir_a = runs_root.join("eval-running");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::write(
        dir_a.join("hub.json"),
        json!({
            "run_id": "eval-running", "suite": "v1", "execution": "live",
            "modes": ["single"], "repetitions": 1, "status": "running",
            "created_at": "2026-08-27T01:00:00+00:00", "planned_total": 2
        })
        .to_string(),
    )
    .unwrap();
    let dir_b = runs_root.join("eval-done");
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(
        dir_b.join("hub.json"),
        json!({
            "run_id": "eval-done", "suite": "v1", "execution": "reference",
            "modes": ["single"], "repetitions": 1, "status": "completed",
            "created_at": "2026-08-27T00:00:00+00:00", "finished_at": "2026-08-27T00:01:00+00:00",
            "planned_total": 1
        })
        .to_string(),
    )
    .unwrap();

    // 新进程语义：重新构造 hub（recover 在 new 内执行）
    let hub = Arc::new(ProductEvalHub::new(
        runs_root.clone(),
        suite_root,
        Arc::new(|| Err("live 未接线".to_string())),
    ));
    let app = router_with_hub(Arc::clone(&hub));
    let env = TestEnv {
        app,
        hub,
        runs_root,
    };

    let (_, interrupted) = get(&env, "/product-eval/runs/eval-running").await;
    assert_eq!(
        interrupted["status"], "interrupted",
        "Running 必须被标记为 interrupted：{interrupted}"
    );
    assert!(interrupted["finished_at"].is_string());
    assert!(
        interrupted["error"]
            .as_str()
            .unwrap()
            .contains("不自动重跑"),
        "明确不自动重跑语义：{interrupted}"
    );

    let (_, completed) = get(&env, "/product-eval/runs/eval-done").await;
    assert_eq!(completed["status"], "completed", "已完成报告继续可查");
    assert!(
        completed["report"].is_null(),
        "无报告文件时 report 为 null（不伪造）"
    );

    // interrupted 后取消：幂等 no-op（不复活、不重跑）
    let (status, again) = cancel(&env, "eval-running").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["status"], "interrupted");
}
