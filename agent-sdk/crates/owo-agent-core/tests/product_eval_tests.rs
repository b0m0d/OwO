//! ProductEval（V1-R1）集成测试：
//! 校验器、检查器语义、权限范围、dry 参考回放、单/多对照一致性、
//! journal 续跑与失败保留、live 生成器（Stub Provider）、报告与对照。

use async_trait::async_trait;
use owo_agent_core::gateway::{ChatMessage, ModelOutput, ModelProvider, TokenUsage};
use owo_agent_core::product_eval::*;
use owo_agent_core::tools::ToolSpec;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// 构造工具
// ---------------------------------------------------------------------------

fn bundle(cases: Vec<ProductEvalCase>) -> SuiteBundle {
    SuiteBundle {
        dir: std::env::temp_dir(),
        suite: ProductEvalSuite {
            schema_version: PRODUCT_EVAL_SCHEMA_VERSION,
            name: "test-suite".to_string(),
            description: String::new(),
            defaults: SuiteDefaults::default(),
            tasks: cases
                .iter()
                .map(|c| format!("tasks/{}.json", c.id))
                .collect(),
        },
        cases,
    }
}

fn make_case(id: &str, category: EvalCategory) -> ProductEvalCase {
    ProductEvalCase {
        schema_version: PRODUCT_EVAL_SCHEMA_VERSION,
        id: id.to_string(),
        category,
        title: format!("任务 {id}"),
        instruction: "完成并输出报告。".to_string(),
        inputs: vec![InputFixture {
            path: "src/a.txt".to_string(),
            content: "alpha".to_string(),
        }],
        allow_read: vec!["src/**".to_string()],
        allow_write: vec!["out/**".to_string()],
        expected_artifacts: vec!["out/report.md".to_string()],
        checkers: vec![
            ArtifactChecker::Contains {
                path: "out/report.md".to_string(),
                text: "结论".to_string(),
            },
            ArtifactChecker::Regex {
                path: "out/report.md".to_string(),
                pattern: r"^# ".to_string(),
            },
            ArtifactChecker::LineCountMin {
                path: "out/report.md".to_string(),
                min_lines: 2,
            },
        ],
        reference_outputs: BTreeMap::from([(
            "out/report.md".to_string(),
            "# 报告\n结论 ok\n".to_string(),
        )]),
        timeout_secs: None,
        max_model_calls: None,
        repetitions: None,
        allow_commands: Vec::new(),
    }
}

fn make_json_case(id: &str) -> ProductEvalCase {
    ProductEvalCase {
        schema_version: PRODUCT_EVAL_SCHEMA_VERSION,
        id: id.to_string(),
        category: EvalCategory::Document,
        title: format!("任务 {id}"),
        instruction: "抽取结构化字段。".to_string(),
        inputs: vec![InputFixture {
            path: "notes/meeting.md".to_string(),
            content: "owner: 张三".to_string(),
        }],
        allow_read: vec!["notes/**".to_string()],
        allow_write: vec!["out/**".to_string()],
        expected_artifacts: vec!["out/extract.json".to_string()],
        checkers: vec![ArtifactChecker::JsonFieldEquals {
            path: "out/extract.json".to_string(),
            field: "owner".to_string(),
            expected: serde_json::json!("张三"),
        }],
        reference_outputs: BTreeMap::from([(
            "out/extract.json".to_string(),
            "{\"owner\":\"张三\",\"count\":2}".to_string(),
        )]),
        timeout_secs: None,
        max_model_calls: None,
        repetitions: None,
        allow_commands: Vec::new(),
    }
}

fn dummy_run(key: MatrixKey, status: RunStatus) -> ProductEvalRun {
    ProductEvalRun {
        key,
        category: EvalCategory::Code,
        status,
        wall_ms: 10,
        model_calls: 0,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        cost_usd: None,
        failed_steps: vec![],
        retries: 0,
        cancellations: 0,
        artifact_refs: vec![],
        tool_log: vec![],
        model: None,
        started_at: "2026-01-01T00:00:00Z".to_string(),
        finished_at: "2026-01-01T00:00:01Z".to_string(),
        error: None,
    }
}

fn temp_out(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pe-tests-{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn no_cancel() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// 测试用选项：固定 1 次重复（套件默认 20 次会让矩阵膨胀）。
fn opts1(modes: Vec<AgentMode>) -> RunOptions {
    RunOptions {
        modes,
        reps_override: Some(1),
        ..Default::default()
    }
}

async fn run_matrix(
    runner: &MatrixRunner,
    executor: Arc<dyn CaseExecutor>,
    execution: &str,
    opts: &RunOptions,
    cancel: Arc<AtomicBool>,
) -> ProductEvalReport {
    runner
        .run(executor, execution, None, opts, cancel)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// 脚本化执行器：按 case id 决定成败
// ---------------------------------------------------------------------------

struct ScriptedExecutor {
    /// 这些 case 一律产出"坏"内容（检查器失败）。
    fail_cases: Vec<String>,
    /// 写完后额外 sleep（测超时用）。
    sleep_ms: u64,
}

impl ScriptedExecutor {
    fn passing() -> Arc<Self> {
        Arc::new(Self {
            fail_cases: vec![],
            sleep_ms: 0,
        })
    }
}

#[async_trait]
impl CaseExecutor for ScriptedExecutor {
    async fn execute<'ctx>(&self, ctx: &mut ExecContext<'ctx>) -> RawExecOutcome {
        if self.sleep_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.sleep_ms)).await;
        }
        let artifact = ctx.case.expected_artifacts[0].clone();
        let content = if self.fail_cases.contains(&ctx.case.id) {
            "# 报告\n（缺少关键词）\n".to_string()
        } else {
            "# 报告\n结论 ok\n".to_string()
        };
        let mut outcome = RawExecOutcome::default();
        if let Err(error) = ctx.write_file(&artifact, &content) {
            outcome.error = Some(error);
        }
        outcome
    }
}

// ---------------------------------------------------------------------------
// 队列 Provider：live 生成器的确定性替身
// ---------------------------------------------------------------------------

struct QueueProvider {
    replies: Mutex<Vec<String>>,
    calls: AtomicU32,
}

impl QueueProvider {
    fn new(replies: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().map(str::to_string).collect()),
            calls: AtomicU32::new(0),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ModelProvider for QueueProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.replies.lock().unwrap();
        if queue.is_empty() {
            return Err("队列耗尽：脚本回复不足".to_string());
        }
        Ok(ModelOutput::Text(queue.remove(0)))
    }

    fn usage_snapshot(&self) -> TokenUsage {
        TokenUsage::default()
    }
}

fn file_block(path: &str, content: &str) -> String {
    format!("=== FILE: {path} ===\n{content}\n=== END FILE ===")
}

// ---------------------------------------------------------------------------
// 1) 路径与权限范围
// ---------------------------------------------------------------------------

#[test]
fn sanitize_and_scope_semantics() {
    assert_eq!(sanitize_rel_path("a/b/c.md").unwrap(), "a/b/c.md");
    assert_eq!(sanitize_rel_path("a\\b\\c.md").unwrap(), "a/b/c.md");
    assert_eq!(sanitize_rel_path("./a//b/./").unwrap(), "a/b");
    assert!(sanitize_rel_path("/abs").is_err());
    assert!(sanitize_rel_path("a/../..").is_err());
    assert!(sanitize_rel_path("C:/x").is_err());
    assert!(sanitize_rel_path("").is_err());

    // 目录前缀语义
    assert!(scope_matches("out", "out/report.md"));
    assert!(!scope_matches("out", "out2/report.md"));
    // 段通配
    assert!(scope_matches("out/*.md", "out/report.md"));
    assert!(!scope_matches("out/*.md", "out/sub/report.md"));
    // ** 跨段
    assert!(scope_matches("src/**", "src/a/b/c.txt"));
    assert!(scope_matches("notes/??/x.md", "notes/ab/x.md"));
    assert!(!scope_matches("notes/??/x.md", "notes/abc/x.md"));
    // in_scope 聚合
    let rules = vec!["out/**".to_string(), "tmp".to_string()];
    assert!(in_scope(&rules, "out/a.md"));
    assert!(in_scope(&rules, "tmp/b.md"));
    assert!(!in_scope(&rules, "etc/passwd"));
}

// ---------------------------------------------------------------------------
// 2) 检查器语义
// ---------------------------------------------------------------------------

#[test]
fn checker_semantics_on_map_and_dir() {
    let snapshot = BTreeMap::from([
        ("a.md".to_string(), "# 标题\n结论 ok\n".to_string()),
        ("b.md".to_string(), "普通文本".to_string()),
        ("empty.md".to_string(), String::new()),
        ("c.json".to_string(), "{\"owner\":\"张三\"}".to_string()),
    ]);
    let ok_cases = vec![
        ArtifactChecker::Exists {
            path: "a.md".to_string(),
        },
        ArtifactChecker::Contains {
            path: "a.md".to_string(),
            text: "结论".to_string(),
        },
        ArtifactChecker::NotContains {
            path: "b.md".to_string(),
            text: "禁止词".to_string(),
        },
        ArtifactChecker::Regex {
            path: "a.md".to_string(),
            pattern: r"^# ".to_string(),
        },
        // multi_line 语义：^ 按行锚定，第 2 行行首也应命中。
        ArtifactChecker::Regex {
            path: "a.md".to_string(),
            pattern: "^结论".to_string(),
        },
        ArtifactChecker::LineCountMin {
            path: "a.md".to_string(),
            min_lines: 2,
        },
        ArtifactChecker::JsonFieldEquals {
            path: "c.json".to_string(),
            field: "owner".to_string(),
            expected: serde_json::json!("张三"),
        },
    ];
    for checker in &ok_cases {
        assert!(
            evaluate_checker_on_map(checker, &snapshot).is_ok(),
            "应当通过：{}",
            checker.describe()
        );
    }
    let fail_cases = vec![
        ArtifactChecker::Exists {
            path: "missing.md".to_string(),
        },
        ArtifactChecker::Exists {
            path: "empty.md".to_string(),
        },
        ArtifactChecker::Contains {
            path: "b.md".to_string(),
            text: "结论".to_string(),
        },
        ArtifactChecker::NotContains {
            path: "a.md".to_string(),
            text: "结论".to_string(),
        },
        ArtifactChecker::LineCountMin {
            path: "b.md".to_string(),
            min_lines: 5,
        },
        ArtifactChecker::JsonFieldEquals {
            path: "c.json".to_string(),
            field: "owner".to_string(),
            expected: serde_json::json!("李四"),
        },
    ];
    for checker in &fail_cases {
        assert!(
            evaluate_checker_on_map(checker, &snapshot).is_err(),
            "应当失败：{}",
            checker.describe()
        );
    }
    // 磁盘来源
    let dir = temp_out("checker-dir");
    std::fs::write(dir.join("d.md"), "# t\n结论\n").unwrap();
    assert!(evaluate_checker_on_dir(
        &ArtifactChecker::Contains {
            path: "d.md".to_string(),
            text: "结论".to_string()
        },
        &dir
    )
    .is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3) validate：合法与各类违规
// ---------------------------------------------------------------------------

#[test]
fn validate_accepts_well_formed_cases() {
    let b = bundle(vec![
        make_case("good-code", EvalCategory::Code),
        make_json_case("good-doc"),
    ]);
    let validation = validate_suite(&b);
    assert!(validation.all_ok, "issues={:?}", validation.tasks);
    assert_eq!(validation.ok_count, 2);
}

#[test]
fn validate_rejects_missing_reference_and_scope_and_checker_drift() {
    let mut missing_ref = make_case("missing-ref", EvalCategory::Code);
    missing_ref.reference_outputs.clear();

    let mut out_of_scope = make_case("out-of-scope", EvalCategory::Code);
    out_of_scope.expected_artifacts = vec!["etc/leak.md".to_string()];
    out_of_scope
        .reference_outputs
        .insert("etc/leak.md".to_string(), "# 报告\n结论 ok\n".to_string());

    let mut checker_drift = make_case("checker-drift", EvalCategory::Code);
    checker_drift.checkers.push(ArtifactChecker::Exists {
        path: "elsewhere.md".to_string(),
    });

    let mut bad_regex = make_case("bad-regex", EvalCategory::Code);
    bad_regex.checkers.push(ArtifactChecker::Regex {
        path: "out/report.md".to_string(),
        pattern: "([".to_string(),
    });

    let mut ref_fails = make_case("ref-fails", EvalCategory::Code);
    ref_fails
        .reference_outputs
        .insert("out/report.md".to_string(), "没有关键词的内容".to_string());

    let b = bundle(vec![
        missing_ref,
        out_of_scope,
        checker_drift,
        bad_regex,
        ref_fails,
    ]);
    let validation = validate_suite(&b);
    assert!(!validation.all_ok);
    let joined = validation
        .tasks
        .iter()
        .flat_map(|t| t.issues.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("reference_outputs 缺少"), "{joined}");
    assert!(joined.contains("allow_write"), "{joined}");
    assert!(joined.contains("不在 expected_artifacts"), "{joined}");
    assert!(joined.contains("正则无效"), "{joined}");
    assert!(joined.contains("参考输出未通过检查器"), "{joined}");
}

#[test]
fn validate_rejects_duplicate_ids() {
    let b = bundle(vec![
        make_case("dup", EvalCategory::Code),
        make_case("dup", EvalCategory::Research),
    ]);
    let validation = validate_suite(&b);
    assert!(!validation.all_ok);
    assert!(format_validation(&validation).contains("重复"));
}

// ---------------------------------------------------------------------------
// 4) dry 参考回放 + 续跑不重复
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dry_run_reference_passes_and_resume_skips_completed() {
    let b = bundle(vec![make_case("dry-a", EvalCategory::Code)]);
    let out = temp_out("dry-resume");
    let runner = MatrixRunner::new(b.clone(), &out);
    let opts = opts1(vec![AgentMode::Single]);
    let report = run_matrix(
        &runner,
        Arc::new(ReferenceDryExecutor),
        "dry-reference",
        &opts,
        no_cancel(),
    )
    .await;
    assert_eq!(report.runs.len(), 1);
    assert_eq!(report.runs[0].status, RunStatus::Passed);
    assert_eq!(report.runs[0].model_calls, 0);
    assert_eq!(report.runs[0].artifact_refs, vec!["out/report.md"]);
    assert!(report.pending.is_empty());
    assert!(report.metrics.success_rate == 1.0);

    // 重跑同一 out：已完成 case 不重复执行。
    let again = run_matrix(
        &runner,
        Arc::new(ReferenceDryExecutor),
        "dry-reference",
        &opts,
        no_cancel(),
    )
    .await;
    assert_eq!(again.runs.len(), 1, "续跑不得重复已完成 case");
    let _ = std::fs::remove_dir_all(&out);
}

#[tokio::test]
async fn dry_run_single_multi_parity() {
    let b = bundle(vec![make_case("parity-a", EvalCategory::Research)]);
    let out = temp_out("parity");
    let runner = MatrixRunner::new(b, &out);
    let opts = opts1(AgentMode::all().to_vec()); // single + multi
    let report = run_matrix(
        &runner,
        Arc::new(ReferenceDryExecutor),
        "dry-reference",
        &opts,
        no_cancel(),
    )
    .await;
    assert_eq!(report.runs.len(), 2);
    assert!(report.runs.iter().all(|r| r.status == RunStatus::Passed));
    let modes: Vec<AgentMode> = report.per_case.iter().map(|r| r.agent_mode).collect();
    assert_eq!(modes, vec![AgentMode::Single, AgentMode::Multi]);
    assert!(report.per_case.iter().all(|r| r.success_rate == 1.0));
    let _ = std::fs::remove_dir_all(&out);
}

// ---------------------------------------------------------------------------
// 5) 失败保留 + 续跑 + fresh 重置
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failed_runs_retained_and_never_rerun() {
    let b = bundle(vec![
        make_case("keep-a", EvalCategory::Code),
        make_case("keep-b", EvalCategory::Document),
    ]);
    let out = temp_out("retain");
    let runner = MatrixRunner::new(b, &out);
    let opts = opts1(AgentMode::all().to_vec());

    let failing = Arc::new(ScriptedExecutor {
        fail_cases: vec!["keep-b".to_string()],
        sleep_ms: 0,
    });
    let first = run_matrix(&runner, failing, "dry-reference", &opts, no_cancel()).await;
    assert_eq!(first.runs.len(), 4); // 2 case × 2 mode
    assert_eq!(first.metrics.failed, 2); // keep-b × single/multi
    assert!(first.metrics.success_rate < 1.0);

    // 换"全过"执行器续跑：失败记录仍在、不重算、不重跑。
    let second = run_matrix(
        &runner,
        ScriptedExecutor::passing(),
        "dry-reference",
        &opts,
        no_cancel(),
    )
    .await;
    assert_eq!(second.runs.len(), 4, "续跑不得产生新运行");
    assert_eq!(second.metrics.failed, 2, "失败运行必须保留在报告中");
    assert!(second.runs.iter().any(|r| r.status == RunStatus::Failed));
    assert!(second.pending.is_empty());

    // fresh 是唯一允许归零重跑的入口（沿用同样的 reps=1 覆盖）。
    let fresh_opts = RunOptions {
        fresh: true,
        ..opts.clone()
    };
    let third = run_matrix(
        &runner,
        ScriptedExecutor::passing(),
        "dry-reference",
        &fresh_opts,
        no_cancel(),
    )
    .await;
    assert_eq!(third.runs.len(), 4);
    assert_eq!(third.metrics.passed, 4);
    let _ = std::fs::remove_dir_all(&out);
}

// ---------------------------------------------------------------------------
// 6) 取消：未完成单元格进入 pending，已完成记录保留
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_leaves_pending_and_keeps_journal() {
    let b = bundle(vec![make_case("cancel-a", EvalCategory::Code)]);
    let out = temp_out("cancel");
    let runner = MatrixRunner::new(b, &out);
    let opts = opts1(vec![AgentMode::Single]);
    let report = run_matrix(
        &runner,
        ScriptedExecutor::passing(),
        "dry-reference",
        &opts,
        Arc::new(AtomicBool::new(true)),
    )
    .await;
    assert_eq!(report.runs.len(), 0, "取消后不得有新运行");
    assert_eq!(report.pending.len(), 1, "未完成单元格应出现在 pending");
    let _ = std::fs::remove_dir_all(&out);
}

// ---------------------------------------------------------------------------
// 7) journal 损坏的容错语义：尾行撕裂容忍、中部损坏拒绝
// ---------------------------------------------------------------------------

fn write_meta(out: &Path, hash: &str) {
    std::fs::create_dir_all(out).unwrap();
    std::fs::write(
        out.join("meta.json"),
        format!(
            r#"{{"schema_version":1,"suite_name":"t","suite_hash":"{hash}","model":null,"created_at":"2026-01-01T00:00:00Z"}}"#
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn torn_journal_tail_is_tolerated_mid_corruption_is_fatal() {
    let b = bundle(vec![
        make_case("jrnl-a", EvalCategory::Code),
        make_case("jrnl-b", EvalCategory::Code),
    ]);
    let hash = suite_hash(&b);
    let opts = opts1(vec![AgentMode::Single]);

    // 尾行撕裂：容忍，对应单元格视为未完成并续跑。
    let out = temp_out("torn-tail");
    write_meta(&out, &hash);
    let a_line = serde_json::to_string(&dummy_run(
        MatrixKey::new("jrnl-a", AgentMode::Single, 0),
        RunStatus::Passed,
    ))
    .unwrap();
    std::fs::write(out.join("state.jsonl"), format!("{a_line}\n{{torn")).unwrap();
    let runner = MatrixRunner::new(b.clone(), &out);
    let report = run_matrix(
        &runner,
        Arc::new(ReferenceDryExecutor),
        "dry-reference",
        &opts,
        no_cancel(),
    )
    .await;
    assert_eq!(report.runs.len(), 2, "撕裂尾 + 补跑 1 格 = 2");
    assert_eq!(report.metrics.passed, 2);

    // 中部损坏：拒绝静默丢弃，明确失败。
    let out2 = temp_out("mid-corrupt");
    write_meta(&out2, &hash);
    let b_line = serde_json::to_string(&dummy_run(
        MatrixKey::new("jrnl-b", AgentMode::Single, 0),
        RunStatus::Passed,
    ))
    .unwrap();
    std::fs::write(
        out2.join("state.jsonl"),
        format!("{a_line}\n{{broken-middle}}\n{b_line}\n"),
    )
    .unwrap();
    let runner2 = MatrixRunner::new(b, &out2);
    let result = runner2
        .run(
            Arc::new(ReferenceDryExecutor),
            "dry-reference",
            None,
            &opts,
            no_cancel(),
        )
        .await;
    let message = result.expect_err("中部损坏必须报错").0;
    assert!(message.contains("损坏"), "{message}");
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(&out2);
}

// ---------------------------------------------------------------------------
// 8) live 生成器（Stub Provider）：single / multi / 返工 / 越权写入
// ---------------------------------------------------------------------------

fn live_case(id: &str) -> ProductEvalCase {
    let mut case = make_case(id, EvalCategory::Code);
    case.max_model_calls = Some(8);
    case
}

#[tokio::test]
async fn live_single_mode_generates_and_passes() {
    let b = bundle(vec![live_case("live-s")]);
    let out = temp_out("live-single");
    let runner = MatrixRunner::new(b, &out);
    let provider = QueueProvider::new(vec![&file_block(
        "out/report.md",
        "# 报告\n结论 generated\n",
    )]);
    let executor: Arc<dyn CaseExecutor> = Arc::new(GenerativeExecutor {
        provider: provider.clone(),
        model: "stub-model".to_string(),
    });
    let opts = opts1(vec![AgentMode::Single]);
    let report = run_matrix(&runner, executor, "live-stub", &opts, no_cancel()).await;
    assert_eq!(report.runs.len(), 1);
    assert_eq!(
        report.runs[0].status,
        RunStatus::Passed,
        "{:?}",
        report.runs[0]
    );
    assert_eq!(report.runs[0].model_calls, 1);
    assert_eq!(provider.calls(), 1);
    assert_eq!(report.runs[0].total_tokens, None, "Stub 不报用量 → null");
    let _ = std::fs::remove_dir_all(&out);
}

#[tokio::test]
async fn live_multi_mode_planner_reviewer_and_rework() {
    let b = bundle(vec![live_case("live-m")]);
    let out = temp_out("live-multi");
    let runner = MatrixRunner::new(b, &out);

    // APPROVED 路径：plan → draft → approved = 3 次调用。
    let provider = QueueProvider::new(vec![
        "1. 读输入 2. 写报告",
        &file_block("out/report.md", "# 报告\n结论 draft\n"),
        "APPROVED",
    ]);
    let executor: Arc<dyn CaseExecutor> = Arc::new(GenerativeExecutor {
        provider: provider.clone(),
        model: "stub-model".to_string(),
    });
    let opts = opts1(vec![AgentMode::Multi]);
    let report = run_matrix(&runner, executor, "live-stub", &opts, no_cancel()).await;
    assert_eq!(report.runs[0].status, RunStatus::Passed);
    assert_eq!(report.runs[0].model_calls, 3);
    assert_eq!(provider.calls(), 3);
    assert_eq!(report.runs[0].retries, 0);
    let _ = std::fs::remove_dir_all(&out);

    // REVISE 路径：草稿不合规 → 返工后通过 = 4 次调用、retries=1。
    let out2 = temp_out("live-multi-revise");
    let runner2 = MatrixRunner::new(bundle(vec![live_case("live-m2")]), &out2);
    let provider2 = QueueProvider::new(vec![
        "plan",
        &file_block("out/report.md", "# 报告\n（缺关键词）\n"),
        "REVISE: 报告缺少结论关键词",
        &file_block("out/report.md", "# 报告\n结论 revised-ok\n"),
    ]);
    let executor2: Arc<dyn CaseExecutor> = Arc::new(GenerativeExecutor {
        provider: provider2.clone(),
        model: "stub-model".to_string(),
    });
    let report2 = run_matrix(&runner2, executor2, "live-stub", &opts, no_cancel()).await;
    assert_eq!(
        report2.runs[0].status,
        RunStatus::Passed,
        "{:?}",
        report2.runs[0]
    );
    assert_eq!(report2.runs[0].model_calls, 4);
    assert_eq!(report2.runs[0].retries, 1);
    let _ = std::fs::remove_dir_all(&out2);
}

#[tokio::test]
async fn live_scope_violation_fails_run() {
    let b = bundle(vec![live_case("live-scope")]);
    let out = temp_out("live-scope");
    let runner = MatrixRunner::new(b, &out);
    let provider = QueueProvider::new(vec![&file_block("outside/evil.md", "# 越权写入\n")]);
    let executor: Arc<dyn CaseExecutor> = Arc::new(GenerativeExecutor {
        provider,
        model: "stub-model".to_string(),
    });
    let opts = opts1(vec![AgentMode::Single]);
    let report = run_matrix(&runner, executor, "live-stub", &opts, no_cancel()).await;
    assert_eq!(report.runs[0].status, RunStatus::Error);
    let run = &report.runs[0];
    assert!(
        run.failed_steps
            .iter()
            .any(|s| s.contains("write_denied:outside/evil.md")),
        "{:?}",
        run.failed_steps
    );
    assert!(run.error.as_deref().unwrap_or("").contains("allow_write"));
    let _ = std::fs::remove_dir_all(&out);
}

// ---------------------------------------------------------------------------
// 9) 超时 → Timeout 且保留现场
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slow_executor_times_out_and_is_recorded() {
    let mut case = make_case("slow-a", EvalCategory::Code);
    case.timeout_secs = Some(1);
    let b = bundle(vec![case]);
    let out = temp_out("timeout");
    let runner = MatrixRunner::new(b, &out);
    let executor = Arc::new(ScriptedExecutor {
        fail_cases: vec![],
        sleep_ms: 1500,
    });
    let opts = opts1(vec![AgentMode::Single]);
    let report = run_matrix(&runner, executor, "dry-reference", &opts, no_cancel()).await;
    assert_eq!(report.runs[0].status, RunStatus::Timeout);
    assert!(report.runs[0].wall_ms >= 900);
    assert!(report.runs[0]
        .failed_steps
        .iter()
        .any(|s| s.starts_with("timeout:")));
    let _ = std::fs::remove_dir_all(&out);
}

// ---------------------------------------------------------------------------
// 10) 指标聚合与 compare
// ---------------------------------------------------------------------------

#[test]
fn metrics_aggregation_counts_every_attempt() {
    let runs = vec![
        dummy_run(MatrixKey::new("a", AgentMode::Single, 0), RunStatus::Passed),
        dummy_run(MatrixKey::new("a", AgentMode::Single, 1), RunStatus::Failed),
        dummy_run(MatrixKey::new("a", AgentMode::Single, 2), RunStatus::Error),
        dummy_run(
            MatrixKey::new("a", AgentMode::Single, 3),
            RunStatus::Timeout,
        ),
        dummy_run(
            MatrixKey::new("a", AgentMode::Single, 4),
            RunStatus::Cancelled,
        ),
    ];
    let m = aggregate_metrics(&runs);
    assert_eq!(m.runs_total, 5);
    assert_eq!(m.passed, 1);
    assert_eq!(m.failed, 1);
    assert_eq!(m.errors, 1);
    assert_eq!(m.timeouts, 1);
    assert_eq!(m.cancelled, 1);
    assert!((m.success_rate - 0.2).abs() < 1e-9, "分母必须含全部尝试");
    let per_case = aggregate_per_case(&runs);
    assert_eq!(per_case.len(), 1);
    assert_eq!(per_case[0].runs_total, 5);
}

#[test]
fn compare_reports_flags_regressions_and_mismatched_execution() {
    let report_a = ProductEvalReport {
        schema_version: 1,
        suite_name: "s".into(),
        suite_hash: "h1".into(),
        execution: "dry-reference".into(),
        model: None,
        generated_at: "t1".into(),
        runs: vec![],
        pending: vec![],
        metrics: ProductEvalMetrics::default(),
        per_case: vec![CaseModeMetrics {
            case_id: "x".into(),
            category: EvalCategory::Code,
            agent_mode: AgentMode::Single,
            runs_total: 4,
            passed: 4,
            success_rate: 1.0,
            mean_wall_ms: 10.0,
            mean_model_calls: 1.0,
            total_tokens: None,
        }],
    };
    let mut report_b = report_a.clone();
    report_b.execution = "dry-reference".into();
    report_b.per_case[0].passed = 2;
    report_b.per_case[0].runs_total = 4;
    report_b.per_case[0].success_rate = 0.5;
    let text = compare_reports(&report_a, &report_b, false);
    assert!(text.contains("回归 1 组"), "{text}");
    assert!(text.contains('↓'), "{text}");
    assert!(!text.contains("execution 不同"), "{text}");

    report_b.execution = "live-generative".into();
    let text2 = compare_reports(&report_a, &report_b, false);
    assert!(text2.contains("execution 不同"), "{text2}");
}

// ---------------------------------------------------------------------------
// 11) 文件块解析
// ---------------------------------------------------------------------------

#[test]
fn parse_file_blocks_handles_fences_and_reports_residuals() {
    // 围栏包裹的完整块 + 未闭合块被后续 FILE 覆盖（残块告警）。
    let text = format!(
        "```\n{}\n```\n=== FILE: c.md ===\n残块内容（未闭合）\n{}",
        file_block("a.md", "内容甲"),
        file_block("b.md", "内容乙")
    );
    let (blocks, warnings) = parse_file_blocks(&text);
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    let a = blocks.iter().find(|(p, _)| p == "a.md").unwrap();
    assert_eq!(a.1.trim(), "内容甲");
    let b = blocks.iter().find(|(p, _)| p == "b.md").unwrap();
    assert_eq!(b.1.trim(), "内容乙");
    assert!(!blocks.iter().any(|(p, _)| p == "c.md"), "残块必须被丢弃");
    assert!(warnings.iter().any(|w| w.contains("嵌套")), "{warnings:?}");
}

// ---------------------------------------------------------------------------
// 12) 生效参数优先级与过滤
// ---------------------------------------------------------------------------

#[test]
fn effective_values_and_filters() {
    let mut case = make_case("eff-a", EvalCategory::Code);
    case.repetitions = Some(7);
    case.timeout_secs = Some(30);
    case.max_model_calls = Some(3);
    let defaults = SuiteDefaults::default();
    assert_eq!(case.effective_repetitions(&defaults, None), 7);
    assert_eq!(case.effective_repetitions(&defaults, Some(2)), 2);
    assert_eq!(case.effective_timeout_secs(&defaults), 30);
    assert_eq!(case.effective_max_model_calls(&defaults), 3);
    let bare = make_case("eff-b", EvalCategory::Research);
    assert_eq!(bare.effective_repetitions(&defaults, None), 20);
    assert_eq!(bare.effective_timeout_secs(&defaults), 180);
    assert_eq!(bare.effective_max_model_calls(&defaults), 6);

    let b = bundle(vec![
        make_case("code-one", EvalCategory::Code),
        make_case("doc-one", EvalCategory::Document),
    ]);
    let only = filter_cases(
        &b,
        &RunOptions {
            only: Some("code".to_string()),
            ..Default::default()
        },
    );
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].id, "code-one");
    let cat = filter_cases(
        &b,
        &RunOptions {
            category: Some(EvalCategory::Document),
            ..Default::default()
        },
    );
    assert_eq!(cat.len(), 1);
    assert_eq!(cat[0].id, "doc-one");
}

// ---------------------------------------------------------------------------
// 13) R1 统计段：模式快照按拓扑过滤 + 对照 JSON 形状（live 基线用）
// ---------------------------------------------------------------------------

#[test]
fn mode_statistics_and_report_statistics_shapes() {
    let runs = vec![
        dummy_run(
            MatrixKey::new(String::from("c"), AgentMode::Single, 0),
            RunStatus::Passed,
        ),
        dummy_run(
            MatrixKey::new(String::from("c"), AgentMode::Single, 1),
            RunStatus::Failed,
        ),
        dummy_run(
            MatrixKey::new(String::from("c"), AgentMode::Multi, 0),
            RunStatus::Passed,
        ),
    ];
    let single = mode_statistics(&runs, AgentMode::Single);
    assert_eq!(single.runs_total, 2);
    assert_eq!(single.passed, 1);
    assert!((single.success_rate - 0.5).abs() < 1e-9);
    assert!(single.ci95_low < 0.5 && single.ci95_high > 0.5);
    let multi = mode_statistics(&runs, AgentMode::Multi);
    assert_eq!(multi.runs_total, 1);
    assert!((multi.success_rate - 1.0).abs() < 1e-9);

    let json = report_statistics(&runs);
    assert_eq!(
        json["comparison"]["multi_success_rate_diff"],
        serde_json::json!(0.5)
    );
    assert_eq!(json["comparison"]["rules"].as_array().unwrap().len(), 3);
    // 渲染段落含统计与启用条件两个小节。
    let text = format_mode_statistics(&runs);
    assert!(text.contains("CI95"));
    assert!(text.contains("多 Agent 启用条件"));
}
