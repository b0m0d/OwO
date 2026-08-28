// ============================================================================
// 产品评测中心面板 Node 前端测试 —— desktop/web/tests/product-eval.panel.test.mjs
//
// 运行：node tests/product-eval.panel.test.mjs（在 agent-sdk/desktop/web 下）
// 依赖：仅 node:test / node:assert / node 内置模块；无 DOM、无网络。
//
// 覆盖面（第三路 V1-R1-UI 验收的结构/行为守卫）：
//   - 纯逻辑：运行状态机（queued/running/interrupted/cancelled/completed/failed）、
//     启动请求体规范化（冻结契约 POST /product-eval/runs）、指标格式化与汇总视图；
//   - 渲染守卫：配置区控件在场、对比表（单 vs 多/检查/失败步骤/Artifact 复制/
//     可展开错误）、XSS 转义、长文本省略类、空态；
//   - 行为：启动提交锁（连点只发一次请求）、取消仅 queued/running 可发且幂等、
//     取消后一个轮询周期内呈现 cancelled、终态自动停轮询、面板切走即停轮询、
//     切换运行后晚到响应作废（seq 守卫）；
//   - 错误文案：400/422/401/404/409/5xx/网络映射；
//   - 外壳缺陷修复守卫：notes.panel.js 空节点防护在场；index.html favicon 声明；
//     favicon.svg 在场；style.css 第 17 节防溢出/六态徽标/焦点环规则在场。
// ============================================================================
import { test, after } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const panel = require(join(here, "../panels/eval.panel.js"));
const T = panel._test;
const shellCss = readFileSync(join(here, "../style.css"), "utf8");
const indexHtml = readFileSync(join(here, "../index.html"), "utf8");
const notesSrc = readFileSync(join(here, "../panels/notes.panel.js"), "utf8");

const drain = () => new Promise((r) => setTimeout(r, 5));

function resetState() {
  T.state.config = { suite: "v1", execution: "reference", modes: ["single", "workswarm"], repetitions: 1, category: "", only: "" };
  T.state.submitting = false;
  T.state.cancelling = false;
  T.state.runs = [];
  T.state.runId = null;
  T.state.detail = null;
  T.state.loadFailed = "";
  T.state.loading = false;
  T.state.pollTimer = null;
  // Node 无 DOM：默认探针视为"在文档中"（响应可落地）；切走即停测试单独覆盖为 false。
  T.setAttachmentProbe(() => true);
  T.stopPolling();
}

// 记录调用的假传输层。
function recorder(overrides = {}) {
  const calls = { get: [], post: [] };
  const t = {
    calls,
    get(path) {
      calls.get.push(path);
      return overrides.get ? overrides.get(path) : Promise.resolve({});
    },
    post(path, body) {
      calls.post.push({ path, body });
      return overrides.post ? overrides.post(path, body) : Promise.resolve({});
    },
  };
  return t;
}

// 第四路真实详情形状：record_to_summary + report（ProductEvalReport）。
// 进度键 progress.done；逐格结果在 report.runs（key.agent_mode serde 为 single/multi）；
// 聚合在 report.metrics/per_case；pending 为未完成单元格（当前任务来源）。
const DETAIL_RUNNING = {
  run_id: "eval-1",
  status: "running",
  suite: "v1",
  execution: "live",
  modes: ["single", "workswarm"],
  repetitions: 1,
  category: null,
  only: null,
  model: "test-model",
  planned_total: 3,
  progress: { done: 3, total: 20 },
  error: null,
  report: {
    schema_version: 1,
    suite_name: "v1",
    suite_hash: "abc",
    execution: "live",
    model: "test-model",
    generated_at: "2026-08-28T10:00:00Z",
    runs: [
      { key: { case_id: "code-a", repetition: 1, agent_mode: "single" }, category: "code", status: "passed", wall_ms: 900, model_calls: 3, total_tokens: 100, cost_usd: 0.004, failed_steps: [], artifact_refs: ["cas://a1"], error: null },
      { key: { case_id: "code-a", repetition: 1, agent_mode: "multi" }, category: "code", status: "failed", wall_ms: 2100, model_calls: 6, total_tokens: 200, cost_usd: 0.009, failed_steps: ["critic"], artifact_refs: ["cas://a1", "cas://a2"], error: "critic 验证失败：缺少单元测试" },
      { key: { case_id: "doc-b", repetition: 1, agent_mode: "single" }, category: "document", status: "passed", wall_ms: 1500, model_calls: 3, total_tokens: 80, cost_usd: 0.003, failed_steps: [], artifact_refs: ["cas://b1"], error: null },
    ],
    pending: [{ case_id: "code-bug-fix", repetition: 1, agent_mode: "single" }],
    metrics: { runs_total: 3, passed: 2, failed: 1, errors: 0, timeouts: 0, cancelled: 0, success_rate: 2 / 3, mean_wall_ms: 1500, total_model_calls: 12, total_tokens: 380, estimated_cost_usd: 0.016 },
    per_case: [],
  },
};

// ----------------------------------------------------------------------------
// 1. 模块导出与骨架
// ----------------------------------------------------------------------------

test("模块导出与浏览器骨架完好", () => {
  assert.equal(panel.id, "eval");
  assert.equal(panel.title, "产品评测中心");
  assert.equal(typeof panel.mount, "function");
  const html = panel.nav();
  assert.match(html, /data-panel="eval"/);
  assert.match(html, /<style>/);
  assert.ok(!html.includes("##"), "禁止出现 '##' 非法选择器（历史空白面板回归）");
  // 运行配置区：suite / reference-live / single-workswarm / 次数 / 类别与 case 过滤 / 启动取消
  assert.match(html, /id="owo-pe-suite"/);
  assert.match(html, /id="owo-pe-execution"/);
  assert.match(html, /<option value="reference">/);
  assert.match(html, /<option value="live">/);
  assert.match(html, /id="owo-pe-mode-single"/);
  assert.match(html, /id="owo-pe-mode-workswarm"/);
  assert.match(html, /id="owo-pe-reps"/);
  assert.match(html, /id="owo-pe-category"/);
  assert.match(html, /id="owo-pe-only"/);
  assert.match(html, /id="owo-pe-start"/);
  assert.match(html, /id="owo-pe-cancel"/);
  // 进度区 / 对比区 / 历史区
  assert.match(html, /id="owo-pe-summary"/);
  assert.match(html, /id="owo-pe-cases"/);
  assert.match(html, /id="owo-pe-list"/);
});

// ----------------------------------------------------------------------------
// 2. 状态机
// ----------------------------------------------------------------------------

test("normRunStatus / isTerminalRun / isCancellable 状态机", () => {
  assert.equal(T.normRunStatus("Running"), "running");
  assert.equal(T.normRunStatus(" QUEUED "), "queued");
  assert.equal(T.normRunStatus(null), "");
  for (const s of ["completed", "failed", "cancelled", "interrupted"]) {
    assert.equal(T.isTerminalRun(s), true, s + " 应为终态");
  }
  assert.equal(T.isTerminalRun("running"), false);
  assert.equal(T.isTerminalRun("queued"), false);
  assert.equal(T.isCancellable("queued"), true);
  assert.equal(T.isCancellable("running"), true);
  for (const s of ["completed", "failed", "cancelled", "interrupted"]) {
    assert.equal(T.isCancellable(s), false, s + " 不可再取消");
  }
});

test("badgeClass 六态语义色分级", () => {
  assert.equal(T.badgeClass("completed"), "ok");
  assert.equal(T.badgeClass("failed"), "bad");
  assert.equal(T.badgeClass("running"), "run");
  assert.equal(T.badgeClass("queued"), "run");
  assert.equal(T.badgeClass("interrupted"), "warn");
  assert.equal(T.badgeClass("cancelled"), "off");
  assert.equal(T.badgeClass(""), "off");
});

// ----------------------------------------------------------------------------
// 3. 启动请求体（冻结契约 POST /product-eval/runs）
// ----------------------------------------------------------------------------

test("buildRunBody：完整配置规范化", () => {
  const body = T.buildRunBody({
    suite: " v1 ",
    execution: "live",
    modes: ["single", "workswarm"],
    repetitions: "2",
    category: "code",
    only: " code-bug-fix ",
  });
  assert.deepEqual(body, {
    suite: "v1",
    execution: "live",
    modes: ["single", "workswarm"],
    repetitions: 2,
    category: "code",
    only: "code-bug-fix",
  });
});

test("buildRunBody：缺省与脏值防护", () => {
  const body = T.buildRunBody({ suite: "", execution: "yolo", modes: ["x", 42], repetitions: "abc", category: "", only: "" });
  assert.equal(body.suite, "v1", "空套件回退默认 v1");
  assert.equal(body.execution, "reference", "非法执行方式回退 reference");
  assert.deepEqual(body.modes, ["single", "workswarm"], "非法引擎过滤后回退双引擎");
  assert.equal(body.repetitions, 1, "非法次数回退 1");
  assert.equal(body.category, null);
  assert.equal(body.only, null);

  const big = T.buildRunBody({ repetitions: 9999 });
  assert.equal(big.repetitions, 50, "次数封顶 50");
  const zero = T.buildRunBody({ repetitions: 0 });
  assert.equal(zero.repetitions, 1, "次数下限 1");
});

// ----------------------------------------------------------------------------
// 4. 格式化与汇总视图
// ----------------------------------------------------------------------------

test("fmt 系列 null 安全格式化", () => {
  assert.equal(T.fmtRate(null), "—");
  assert.equal(T.fmtRate(0.75), "75.0%");
  assert.equal(T.fmtDur(null), "—");
  assert.equal(T.fmtDur(1200), "1.2s");
  assert.equal(T.fmtDur(65000), "1m05s");
  assert.equal(T.fmtInt("42"), "42");
  assert.equal(T.fmtInt(undefined), "—");
  assert.equal(T.fmtCost(0.008), "$0.0080");
  assert.equal(T.fmtCost(0.5), "$0.5000");
  assert.equal(T.fmtCost(0.021), "$0.0210");
  assert.equal(T.fmtCost(2.5), "$2.50");
  assert.equal(T.fmtCost(null), "—");
});

test("computeSummary：进度(done 键)/双引擎聚合/当前任务来自 pending", () => {
  const s = T.computeSummary(DETAIL_RUNNING);
  assert.equal(s.status, "running");
  assert.equal(s.completed, 3);
  assert.equal(s.total, 20);
  assert.equal(s.pct, 15);
  assert.equal(s.current, "code-bug-fix", "无 progress.current 时从 report.pending 取当前任务");
  // single：2 格全过 → 100%，均值 (900+1500)/2=1200ms
  assert.equal(s.singleRate, "100.0%");
  assert.equal(s.singleDur, "1.2s");
  assert.equal(s.singleCalls, "6");
  assert.equal(s.singleTokens, "180");
  assert.equal(s.singleCost, "$0.0070");
  // workswarm(multi)：1 格失败 → 0%
  assert.equal(s.wsRate, "0.0%");
  assert.equal(s.wsDur, "2.1s");
  assert.equal(s.wsCalls, "6");
  assert.equal(s.wsTokens, "200");
  assert.equal(s.wsCost, "$0.0090");
  assert.deepEqual(s.modes, ["single", "workswarm"]);
});

test("computeSummary：兼容 progress.completed 备用键", () => {
  const s = T.computeSummary({ status: "running", progress: { total: 10, completed: 5 } });
  assert.equal(s.pct, 50);
});

test("casesFromDetail：逐 case 归一（multi→workswarm/检查计数/失败步骤/refs 去重）", () => {
  const cases = T.casesFromDetail(DETAIL_RUNNING);
  assert.equal(cases.length, 2);
  const codeA = cases.find((c) => c.case_id === "code-a");
  assert.equal(codeA.category, "code");
  assert.equal(codeA.single.status, "passed");
  assert.equal(codeA.single.checks, "1/1 通过");
  assert.equal(codeA.workswarm.status, "failed");
  assert.equal(codeA.workswarm.checks, "0/1 通过");
  assert.equal(codeA.workswarm.failed_step, "critic");
  assert.equal(codeA.workswarm.error, "critic 验证失败：缺少单元测试");
  assert.deepEqual(codeA.workswarm.artifact_refs, ["cas://a1", "cas://a2"]);
  const docB = cases.find((c) => c.case_id === "doc-b");
  assert.equal(docB.single.status, "passed");
  assert.equal(docB.workswarm, null, "未运行的引擎侧显示空");
});

test("normMode：multi/workswarm 归一为 workswarm", () => {
  assert.equal(T.normMode("multi"), "workswarm");
  assert.equal(T.normMode("workswarm"), "workswarm");
  assert.equal(T.normMode("single"), "single");
});

test("engineAgg：report 缺席返回 null；按引擎过滤聚合", () => {
  assert.equal(T.engineAgg(null, "single"), null);
  const agg = T.engineAgg(DETAIL_RUNNING.report, "single");
  assert.equal(agg.total, 2);
  assert.equal(agg.passed, 2);
  assert.equal(agg.calls, 6);
});

test("computeSummary：空详情与缺 report 不炸", () => {
  const s = T.computeSummary(null);
  assert.equal(s.total, null);
  assert.equal(s.pct, null);
  assert.equal(s.singleRate, "—");
  assert.equal(T.computeSummary({ status: "queued" }).wsTokens, "—");
  assert.equal(T.computeSummary({ status: "queued", report: { runs: [] } }).current, "");
});

// ----------------------------------------------------------------------------
// 5. 渲染守卫
// ----------------------------------------------------------------------------

test("renderCasesTable：对比列/复制按钮/失败步骤/可展开错误/空态", () => {
  const html = T.renderCasesTable([
    {
      case_id: "code-bug-fix",
      category: "code",
      single: { status: "passed", duration_ms: 900, checks: "4/4 通过", artifact_refs: ["cas://abc"] },
      workswarm: { status: "failed", duration_ms: 2100, checks: "2/3 通过", failed_step: "critic", error: "很长的错误信息".repeat(40), artifact_refs: ["cas://abc", "cas://def"] },
    },
  ]);
  assert.match(html, /owo-pe-tablewrap/);
  assert.match(html, /单 Agent/);
  assert.match(html, /WorkSwarm/);
  assert.match(html, /质量检查/);
  assert.match(html, /失败步骤/);
  assert.match(html, /4\/4 通过/);
  assert.match(html, /critic/);
  assert.match(html, /data-pe-copy="cas:\/\/abc"/, "Artifact ref 复制按钮在场");
  assert.match(html, /owo-pe-ellip/, "长文本省略类在场");
  assert.match(html, /owo-pe-errbox/, "可展开错误区在场");
  assert.ok(html.includes("很长的错误信息"), "错误正文保留在展开区");
  assert.equal(html.match(/cas:\/\/abc/g).length >= 1, true);

  const empty = T.renderCasesTable([]);
  assert.match(empty, /暂无逐 case 结果/);
});

test("渲染对不可信内容转义（XSS 守卫）", () => {
  const evil = '<script>alert(1)</script>';
  const html = T.caseRow({
    case_id: evil,
    category: evil,
    single: { status: evil, error: evil, artifact_refs: [evil] },
    workswarm: null,
  });
  assert.ok(!html.includes("<script>"), "script 标签必须被转义");
  assert.ok(html.includes("&lt;script&gt;"), "转义实体在场");
});

test("listHtml：六态徽标 + 选中高亮 + 空态", () => {
  const html = T.listHtml([
    { run_id: "eval-1", status: "interrupted", suite: "v1", execution: "live", modes: ["single"] },
    { run_id: "eval-2", status: "cancelled", suite: "v1", execution: "reference", modes: ["workswarm"] },
  ]);
  assert.match(html, /st-interrupted/);
  assert.match(html, /st-cancelled/);
  assert.match(html, /data-pe-run="eval-1"/);
  const empty = T.listHtml([]);
  assert.match(empty, /暂无评测运行/);
});

// ----------------------------------------------------------------------------
// 6. 行为：启动提交锁 / 取消幂等 / 轮询生命周期
// ----------------------------------------------------------------------------

test("handleStartClick：连点只发一次请求，请求体符合冻结契约", async () => {
  resetState();
  const t = recorder({
    post(path, body) {
      // 模拟服务端延迟：锁必须在在途期间保持
      return new Promise((resolve) => setTimeout(() => resolve({ run_id: "eval-new", status: "queued" }), 15));
    },
  });
  T.setTransport(t);
  T.state.config = { suite: "v1", execution: "live", modes: ["single", "workswarm"], repetitions: 1, category: "", only: "" };

  const p1 = T.handleStartClick();
  const p2 = T.handleStartClick(); // 在途期间的第二次点击
  const p3 = T.handleStartClick();
  await Promise.all([p1, p2, p3]);
  await drain();

  assert.equal(t.calls.post.length, 1, "连点只发一次 POST");
  assert.deepEqual(t.calls.post[0].body, {
    suite: "v1",
    execution: "live",
    modes: ["single", "workswarm"],
    repetitions: 1,
    category: null,
    only: null,
  });
  assert.match(t.calls.post[0].path, /^\/product-eval\/runs$/);
  assert.equal(T.state.runId, "eval-new", "创建后自动选中新运行");
  assert.equal(T.state.submitting, false, "完成后锁释放");
  resetState();
});

test("handleStartClick：服务端拒绝（400）呈现友好错误且不选中运行", async () => {
  resetState();
  const t = recorder({
    post() {
      return Promise.reject(new Error("400: {\"error\":\"invalid suite\"}"));
    },
  });
  T.setTransport(t);
  await T.handleStartClick();
  await drain();
  assert.equal(t.calls.post.length, 1);
  assert.equal(T.state.runId, null, "失败不选中运行");
  assert.match(T.state.note, /请求被拒绝（400）/);
  assert.equal(T.state.submitting, false);
  resetState();
});

test("handleCancelClick：仅 queued/running 可取消；取消后一个轮询周期内呈现 cancelled", async () => {
  resetState();
  const t = recorder({
    post(path, body) {
      assert.match(path, /\/product-eval\/runs\/eval-1\/cancel$/);
      return Promise.resolve({ run_id: "eval-1", status: "cancelling" });
    },
    get() {
      return Promise.resolve({ ...DETAIL_RUNNING, status: "cancelled" });
    },
  });
  T.setTransport(t);
  T.state.runId = "eval-1";
  T.state.detail = { status: "completed" }; // 终态：取消按钮应拒绝发送
  await T.handleCancelClick();
  assert.equal(t.calls.post.length, 0, "终态不发送取消请求");

  T.state.detail = { status: "running" };
  await T.handleCancelClick();
  await drain();
  assert.equal(t.calls.post.length, 1, "运行中发送一次取消");
  assert.equal(T.state.detail.status, "cancelled", "一个轮询周期内呈现 cancelled");
  assert.equal(T.state.cancelling, false, "取消锁释放");
  resetState();
});

test("pollTick：面板切走即停轮询且不发起请求", async () => {
  resetState();
  const t = recorder({ get: () => Promise.resolve(DETAIL_RUNNING) });
  T.setTransport(t);
  T.state.runId = "eval-1";
  T.setAttachmentProbe(() => false); // 面板已不在文档
  await T.pollTick();
  await drain();
  assert.equal(t.calls.get.length, 0, "切走后不再请求");
  assert.equal(T.state.pollTimer, null, "轮询定时器已清除");
  resetState();
});

test("pollTick：运行中轮询刷新；终态自动停轮询", async () => {
  resetState();
  let status = "running";
  const t = recorder({ get: () => Promise.resolve({ ...DETAIL_RUNNING, status }) });
  T.setTransport(t);
  T.state.runId = "eval-1";
  T.setAttachmentProbe(() => true);
  T.startPolling();
  await drain();
  assert.ok(T.state.pollTimer, "运行中保持轮询");
  const firstCount = t.calls.get.length;
  assert.ok(firstCount >= 1, "至少一次详情请求");

  status = "completed"; // 服务端进入终态
  await T.pollTick();
  await drain();
  assert.equal(T.state.pollTimer, null, "终态后自动停止轮询");
  assert.equal(T.state.detail.status, "completed");
  assert.match(T.state.note, /运行已结束：已完成/);
  resetState();
});

test("syncDetail：切换运行后晚到的旧响应作废（seq 守卫）", async () => {
  resetState();
  let resolveA;
  const t = recorder({
    get(path) {
      if (path.includes("eval-a")) {
        return new Promise((resolve) => { resolveA = resolve; });
      }
      return Promise.resolve({ ...DETAIL_RUNNING, run_id: "eval-b", status: "completed" });
    },
  });
  T.setTransport(t);
  T.setAttachmentProbe(() => true);

  T.state.runId = "eval-a";
  const slow = T.syncDetail(); // 在途
  T.state.runId = "eval-b";
  T.state.seq++; // selectRun 的切换作废语义
  await T.syncDetail();
  resolveA({ ...DETAIL_RUNNING, run_id: "eval-a" });
  await slow;
  await drain();
  assert.equal(T.state.runId, "eval-b");
  assert.equal(T.state.detail.run_id, "eval-b", "旧运行的晚到响应不得覆盖新运行视图");
  resetState();
});

test("loadHistory：数组与 {runs} 两种响应形态兼容", async () => {
  resetState();
  const t1 = recorder({ get: () => Promise.resolve([{ run_id: "r1", status: "failed" }]) });
  T.setTransport(t1);
  await T.loadHistory();
  await drain();
  assert.equal(T.state.runs.length, 1);

  const t2 = recorder({ get: () => Promise.resolve({ runs: [{ run_id: "r2", status: "queued" }] }) });
  T.setTransport(t2);
  await T.loadHistory();
  await drain();
  assert.equal(T.state.runs[0].run_id, "r2");
  resetState();
});

// ----------------------------------------------------------------------------
// 7. 错误文案
// ----------------------------------------------------------------------------

test("explainError 状态码映射", () => {
  assert.match(T.explainError(new Error("400: bad")), /请求被拒绝（400）/);
  assert.match(T.explainError(new Error("422: nope")), /请求被拒绝（422）/);
  assert.match(T.explainError(new Error("401: x")), /未认证/);
  assert.match(T.explainError(new Error("404: x")), /资源不存在（404）/);
  assert.match(T.explainError(new Error("409: x")), /状态冲突（409）/);
  assert.match(T.explainError(new Error("500: x")), /服务接口不可用（HTTP 500）/);
  assert.match(T.explainError(new Error("网络断开")), /操作失败：网络断开/);
});

// ----------------------------------------------------------------------------
// 8. 外壳缺陷修复守卫（notes 空节点 / favicon 404）
// ----------------------------------------------------------------------------

test("notes.panel.js 空节点防护在场（挂载切走不再抛错）", () => {
  assert.match(notesSrc, /if \(!panel\) return;/, "refresh/renderDetail 的 null 面板守卫");
  assert.match(notesSrc, /currentDetailId/, "详情 id 的空节点安全读取");
  assert.match(notesSrc, /var editingId = detail && detail\.dataset \? detail\.dataset\.id : null;/, "保存路径空节点守卫");
});

test("index.html 声明 favicon；favicon.svg 在场（消除 /favicon.ico 404）", () => {
  assert.match(indexHtml, /rel="icon"[^>]*href="favicon\.svg"/);
  assert.ok(existsSync(join(here, "../favicon.svg")), "favicon.svg 文件在场");
  const svg = readFileSync(join(here, "../favicon.svg"), "utf8");
  assert.match(svg, /<svg[\s>]/);
  assert.match(svg, /xmlns="http:\/\/www\.w3\.org\/2000\/svg"/);
});

// ----------------------------------------------------------------------------
// 9. style.css 第 17 节守卫（防溢出/六态/焦点）
// ----------------------------------------------------------------------------

test("style.css 第 17 节守卫：配置网格换行/表格包裹/省略/中断徽标/焦点环/窄栏断点", () => {
  const flat = shellCss.replace(/\s+/g, "");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-config{display:grid;grid-template-columns:repeat(auto-fit,minmax(170px,1fr))"), "配置网格换行在场（防 1280 溢出）");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-config>*{min-width:0;max-width:100%;}"), "配置子项最小宽度约束在场");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-tablewrap{overflow-x:auto;max-width:100%;"), "对比表格横向滚动包裹在场");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-ellip{display:inline-block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:100%;min-width:0;}"), "长文本省略工具类在场");
  assert.ok(flat.includes(".owo-pe-badge.st-interrupted"), "中断徽标样式在场");
  assert.ok(flat.includes(".owo-pe-badge.st-cancelled"), "取消徽标样式在场");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-runrow:focus-visible"), "历史行键盘焦点环在场");
  assert.ok(flat.includes("@media(max-width:1280px)"), "窄栏断点在场");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-engines{grid-template-columns:1fr;}"), "窄栏引擎指标单列在场");
});

// ----------------------------------------------------------------------------
// 10. 统计判读（第四路四期：95% CI / p50/p95 / 启用建议 / 样本不足）
// ----------------------------------------------------------------------------

// 稳定样本构造：n 个 runs，passed 个通过，耗时 1000..1000+n-1 ms。
function makeRuns(n, passed, mode) {
  const out = [];
  for (let i = 0; i < n; i++) {
    out.push({
      key: { case_id: "c" + i, agent_mode: mode || "single" },
      status: i < passed ? "passed" : "failed",
      wall_ms: 1000 + i,
      model_calls: 2,
      total_tokens: 100 + i,
      cost_usd: 0.01,
      failed_steps: i < passed ? [] : ["检查器"],
    });
  }
  return out;
}

test("wilsonCI 边界：0 样本 / 全成功 / 全失败 / 半数 / 非法输入", () => {
  assert.equal(T.wilsonCI(0, 0), null, "0 样本无区间");
  assert.equal(T.wilsonCI(3, -1), null, "非法 n");
  assert.equal(T.wilsonCI(-1, 5), null, "非法 passed");
  assert.equal(T.wilsonCI(6, 5), null, "passed > n");
  const all = T.wilsonCI(10, 10);
  assert.equal(all.lo > 0.7 && all.lo < 1, true, "全成功下界 > 0.7（10/10）");
  assert.ok(all.hi > 0.999, "全成功上界收敛到 1（浮点容差）");
  const none = T.wilsonCI(0, 10);
  assert.equal(none.lo, 0, "全失败下界收敛到 0");
  assert.equal(none.hi < 0.35, true, "全失败上界 < 0.35（10/10）");
  const half = T.wilsonCI(5, 10);
  assert.ok(Math.abs(half.lo - 0.2366) < 0.02 && Math.abs(half.hi - 0.7634) < 0.02, "5/10 区间 ≈ [23.7%, 76.3%]");
  // 单调性：通过率越高，区间整体越高
  assert.ok(T.wilsonCI(8, 10).lo > T.wilsonCI(2, 10).lo);
});

test("percentileOf：空 / 单元素 / 已知 p50 与 p95 / 非数值剔除", () => {
  assert.equal(T.percentileOf([], 50), null);
  assert.equal(T.percentileOf(null, 50), null);
  assert.equal(T.percentileOf([42], 95), 42);
  assert.equal(T.percentileOf([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 50), 5, "p50 = ⌈0.5·10⌉ = 第 5 个");
  assert.equal(T.percentileOf([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 95), 10, "p95 = ⌈0.95·10⌉ = 第 10 个");
  assert.equal(T.percentileOf([3, "x", 1, 2], 50), 2, "非数值剔除后 3 个取第 2 个");
});

test("engineStats：无样本 → null；样本齐全时 n/CI/p50/p95/汇总字段正确", () => {
  assert.equal(T.engineStats({ runs: [] }, "single"), null);
  assert.equal(T.engineStats(null, "single"), null);
  const st = T.engineStats({ runs: makeRuns(20, 15, "single") }, "single");
  assert.equal(st.n, 20);
  assert.equal(st.passed, 15);
  assert.equal(st.rate, 0.75);
  assert.ok(st.ci95[0] > 0.5 && st.ci95[0] < st.rate && st.ci95[1] > st.rate && st.ci95[1] < 0.95);
  assert.equal(st.p50Ms, 1009, "p50 = 第 ⌈0.5·20⌉=10 个（1000..1019 升序第 10 个 = 1009）");
  assert.equal(st.p95Ms, 1018, "p95 = 第 ⌈0.95·20⌉=19 个（= 1018）");
  assert.equal(st.calls, 40);
  assert.equal(typeof st.cost, "number");
});

test("engineStats：全失败样本 rate=0，CI 下界收敛 0", () => {
  const st = T.engineStats({ runs: makeRuns(4, 0, "workswarm") }, "workswarm");
  assert.equal(st.rate, 0);
  assert.equal(st.ci95[0], 0);
});

test("serverStats：宽容识别服务端 statistics（{lo,hi} 与 [lo,hi]、p50/p95、verdict）", () => {
  const obj = T.serverStats({ statistics: { per_engine: { single: { n: 30, success_rate: 0.9, success_rate_ci95: { lo: 0.8, hi: 0.95 }, p50_wall_ms: 120, p95_wall_ms: 400 }, workswarm: { n: 30, success_rate: 0.95, success_rate_ci95: [0.86, 0.99], p50_wall_ms: 90, p95_wall_ms: 300 } }, verdict: { text: "建议启用 WorkSwarm" } } });
  assert.equal(obj.single.ci95[0], 0.8);
  assert.equal(obj.workswarm.ci95[0], 0.86);
  assert.equal(obj.verdictText, "建议启用 WorkSwarm");
  assert.equal(T.serverStats({ statistics: { junk: 1 } }), null, "形状不符 → null");
  assert.equal(T.serverStats({}), null);
  assert.equal(T.serverStats(null), null);
});

test("statisticsFromReport：服务端统计在场时优先且 sampleSmall 按服务端 n 判定", () => {
  const vm = T.statisticsFromReport({
    runs: makeRuns(2, 1, "single"),
    statistics: {
      per_engine: { single: { n: 60, success_rate: 0.8, ci95: [0.7, 0.88] }, workswarm: { n: 60, success_rate: 0.9, ci95: [0.82, 0.95] } },
      verdict: { text: "建议启用 WorkSwarm（服务端判定）" },
    },
  });
  assert.equal(vm.single.n, 60, "采用服务端 n，而非 runs 推导的 2");
  assert.equal(vm.sampleSmall, false);
  assert.equal(vm.recommendation.source, "server");
  assert.match(vm.recommendation.verdict, /服务端判定/);
  // 服务端给了统计但没给 verdict：判定来源如实标注为客户端推导
  const vm2 = T.statisticsFromReport({
    runs: makeRuns(2, 1, "single"),
    statistics: { per_engine: { single: { n: 60, success_rate: 0.8 }, workswarm: { n: 60, success_rate: 0.9 } } },
  });
  assert.equal(vm2.recommendation.source, "client");
});

test("statisticsFromReport：客户端推导路径 + 样本不足 + 启用建议（阈值判定）", () => {
  // 10 样本：多引擎成功率 +20pp（≥+5% 触发），p50 相同（不触发耗时项）→ 建议启用
  const runs = [...makeRuns(10, 6, "single"), ...makeRuns(10, 8, "workswarm")];
  const vm1 = T.statisticsFromReport({ runs });
  assert.equal(vm1.single.n, 10);
  assert.equal(vm1.workswarm.n, 10);
  assert.equal(vm1.sampleSmall, true, "n<30 必须标注样本不足");
  assert.ok(Math.abs(vm1.deltas.rateDiff - 0.2) < 1e-9, "成功率差 +0.2");
  assert.match(vm1.recommendation.verdict, /建议启用 WorkSwarm：成功率 \+20\.0pp/);
  assert.equal(vm1.recommendation.source, "client");
  // 相同通过数：不满足阈值 → 暂不建议
  const runs2 = [...makeRuns(5, 3, "single"), ...makeRuns(5, 3, "workswarm")];
  const vm2 = T.statisticsFromReport({ runs: runs2 });
  assert.match(vm2.recommendation.verdict, /暂不建议启用/);
});

test("statisticsFromReport：无 runs / 单引擎样本各有明确结果", () => {
  assert.equal(T.statisticsFromReport(null), null);
  assert.equal(T.statisticsFromReport({ runs: [] }), null);
  const onlySingle = T.statisticsFromReport({ runs: makeRuns(8, 8, "single") });
  assert.ok(onlySingle && onlySingle.single && !onlySingle.workswarm, "仅单引擎也有统计（多引擎侧显示暂无样本）");
  assert.equal(onlySingle.deltas, null);
  assert.equal(onlySingle.recommendation, null, "缺另一引擎不产生启用建议");
});

test("renderStatistics：null → 空串（无数据不渲染）；有数据 → CI/p50/p95/样本不足/启用建议在场", () => {
  assert.equal(T.renderStatistics(null), "");
  const vm = T.statisticsFromReport({ runs: [...makeRuns(10, 6, "single"), ...makeRuns(10, 8, "workswarm")] });
  const html = T.renderStatistics(vm);
  assert.match(html, /统计判读/);
  assert.match(html, /CI95 \[/);
  assert.match(html, /p50 /);
  assert.match(html, /p95 /);
  assert.match(html, /样本不足/);
  assert.match(html, /启用建议：/);
  assert.match(html, /样本不足：当前样本量 n<30/);
  assert.match(html, /判定来源：客户端推导/);
});

test("summaryHtml 挂载统计区：computeSummary 带 stats，完成态报告渲染统计判读", () => {
  const detail = {
    run_id: "run-x",
    status: "completed",
    suite: "v1",
    execution: "reference",
    modes: ["single", "workswarm"],
    progress: { total: 2, done: 2 },
    report: { runs: [...makeRuns(6, 4, "single"), ...makeRuns(6, 5, "workswarm")] },
  };
  const sum = T.computeSummary(detail);
  assert.ok(sum.stats, "computeSummary 暴露 stats 视图模型");
  const html = T.summaryHtml(sum);
  assert.match(html, /owo-pe-stats/);
  assert.match(html, /统计判读/);
});

test("四期样式守卫：style.css 第 18 节统计判读区在场", () => {
  const flat = shellCss.replace(/\s+/g, "");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-stats"), "统计判读区样式在场");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-stat-rec.ok"), "建议启用绿色语义在场");
  assert.ok(flat.includes(".owo-pe-panel.owo-pe-stat-note.bad"), "样本不足提示样式在场");
});

// ----------------------------------------------------------------------------
// 11. 进程收敛守卫
// ----------------------------------------------------------------------------

// —— 五期：服务端统计真实形状（product_eval.rs::report_statistics）同步 ——

const SERVER_STATS_REPORT = {
  runs: [],
  statistics: {
    modes: [
      {
        mode: "single", runs_total: 30, passed: 24, success_rate: 0.8,
        ci95_low: 0.627, ci95_high: 0.905, p50_wall_ms: 30300, p95_wall_ms: 107400,
        mean_wall_ms: 40500, mean_model_calls: 2.9, total_tokens: 126611,
        total_cost_usd: null, sample_sufficient: true,
      },
      {
        mode: "multi", runs_total: 10, passed: 5, success_rate: 0.5,
        ci95_low: 0.237, ci95_high: 0.763, p50_wall_ms: 95500, p95_wall_ms: 171500,
        mean_wall_ms: 106400, mean_model_calls: 6.8, total_tokens: 84798,
        total_cost_usd: null, sample_sufficient: false,
      },
    ],
    comparison: {
      multi_success_rate_diff: -0.3,
      multi_wall_rel_change: 1.63,
      multi_calls_rel_change: 1.34,
      multi_tokens_rel_change: null,
      multi_cost_rel_change: null,
      rules: [
        { name: "成功率 +5pp", satisfied: false, detail: "-30.0pp" },
        { name: "成功率相对提升 +10%", satisfied: false, detail: "-37.5%" },
        { name: "平均耗时 -30%", satisfied: false, detail: "+163%" },
      ],
      enabled: false,
      sample_sufficient: false,
    },
  },
};

test("serverStats：识别 report_statistics 真实形状（modes[2] + comparison）", () => {
  const s = T.serverStats(SERVER_STATS_REPORT);
  assert.ok(s, "真实形状应被识别");
  assert.equal(s.single.n, 30);
  assert.equal(s.single.rate, 0.8);
  assert.deepEqual(s.single.ci95, [0.627, 0.905].map((x) => x));
  assert.equal(s.single.p50Ms, 30300);
  assert.equal(s.single.p95Ms, 107400);
  assert.equal(s.single.calls, 2.9);
  assert.equal(s.single.sampleSufficient, true);
  assert.equal(s.workswarm.n, 10);
  assert.equal(s.workswarm.rate, 0.5);
  assert.equal(s.workswarm.sampleSufficient, false);
  assert.equal(s.comparisonSampleSufficient, false);
  assert.ok(s.serverDeltas);
  assert.equal(s.serverDeltas.rateDiff, -0.3);
  assert.equal(s.serverDeltas.wallChangePct, 163);
  // mode 标签互换顺序仍可识别
  const swapped = T.serverStats({ statistics: { modes: [SERVER_STATS_REPORT.statistics.modes[1], SERVER_STATS_REPORT.statistics.modes[0]] } });
  assert.equal(swapped.single.n, 30);
  assert.equal(swapped.workswarm.n, 10);
});

test("statisticsFromReport：服务端 comparison 优先给出启用建议与样本充分性", () => {
  const stats = T.statisticsFromReport(SERVER_STATS_REPORT);
  assert.ok(stats.recommendation, "服务端 enabled 字段在场时给建议");
  assert.equal(stats.recommendation.source, "server");
  assert.match(stats.recommendation.verdict, /暂不建议启用多 Agent/);
  assert.equal(stats.sampleSmall, true, "comparison.sample_sufficient=false → 样本不足");
  assert.equal(stats.deltas.rateDiff, -0.3);
  assert.equal(stats.deltas.wallChangePct, 163);
  const html = T.renderStatistics(stats);
  assert.match(html, /样本不足/);
  assert.match(html, /判定来源：服务端统计/);
});

test("statisticsFromReport：服务端 enabled=true 时建议启用并列出满足规则", () => {
  const rep = JSON.parse(JSON.stringify(SERVER_STATS_REPORT));
  rep.statistics.comparison.enabled = true;
  rep.statistics.comparison.sample_sufficient = true; // 两组 n≥30 才算充分（服务端口径）
  rep.statistics.comparison.rules = [{ name: "成功率 +5pp", satisfied: true, detail: "+8pp" }];
  const stats = T.statisticsFromReport(rep);
  assert.match(stats.recommendation.verdict, /建议启用多 Agent/);
  assert.match(stats.recommendation.verdict, /成功率 \+5pp/);
  assert.equal(stats.sampleSmall, false);
});

test("statisticsFromReport：无服务端统计时回退客户端推导（回归守卫）", () => {
  const stats = T.statisticsFromReport({
    runs: [
      { key: { case_id: "c1", agent_mode: "single" }, status: "passed", wall_ms: 100, model_calls: 2 },
      { key: { case_id: "c2", agent_mode: "single" }, status: "passed", wall_ms: 200, model_calls: 2 },
      { key: { case_id: "c1", agent_mode: "workswarm" }, status: "failed", wall_ms: 400, model_calls: 5 },
    ],
  });
  assert.ok(stats.single, "客户端推导单 Agent 统计在场");
  assert.equal(stats.single.n, 2);
  if (stats.recommendation) assert.equal(stats.recommendation.source, "client");
});

// 本机（node v24.19 + Windows 管道/重定向 stdio）下，含真实 setInterval 的套件
// 跑完后 node:test 可能不自然挂起（裸跑可退；`node --test` 门禁形态挂起）。
// 全部用例结束后显式收敛进程；失败时 runner 已把 process.exitCode 置 1
//（实测确认），据此保留退出码语义供门禁判定。
after(() => {
  setImmediate(() => process.exit(process.exitCode || 0));
});
