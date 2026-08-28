// ============================================================================
// WorkSwarm 面板 Node 前端测试 —— desktop/web/tests/workswarm.panel.test.mjs
//
// 运行：node tests/workswarm.panel.test.mjs（在 agent-sdk/desktop/web 下）
// 依赖：仅 node:test / node:assert / node 内置模块；无 DOM、无网络。
//
// 覆盖面（第四路二轮验收的结构/行为守卫）：
//   - 纯逻辑：状态归一化、运行摘要计算、重试目标/门控、冻结 retry 请求体；
//   - 渲染守卫：DAG 重试按钮可见性、详情骨架（摘要/中断横幅/审计焦点）、
//     列表三态、无 "## 选择器" 回归；
//   - 行为：重试提交锁（快速双击只发一次请求）、终态成功/取消拒绝重试、
//     interrupted 标记经详情响应流入面板状态；
//   - 错误文案：400/404/409/网络/凭据提示映射；
//   - 样式守卫：style.css 第 16 节防溢出/中断/重试/焦点规则在场。
// ============================================================================
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const panel = require(join(here, "../panels/workswarm.panel.js"));
const T = panel._test;
const shellCss = readFileSync(join(here, "../style.css"), "utf8");

const drain = () => new Promise((r) => setTimeout(r, 5));

const TASKS_MIXED = [
  { task_id: "plan", role: "planner", worker: "m-planner", status: "Succeeded", attempts: 1, depends_on: [], error: null },
  {
    task_id: "build",
    role: "builder",
    worker: "m-builder",
    status: "Failed",
    attempts: 3,
    depends_on: ["plan"],
    error: "验证失败：输出缺少单元测试（此处是很长的错误信息，用于验证摘要行的省略与 tooltip 行为）",
  },
  { task_id: "review", role: "critic", worker: "m-critic", status: "Pending", attempts: 0, depends_on: ["build"], error: null },
];

function resetState() {
  T.state.current = null;
  T.state.team = null;
  T.state.teamStatus = "";
  T.state.tasks = [];
  T.state.interrupted = false;
  T.state.artifactCount = null;
  T.state.active = false;
}

// 可记录属性读写的假按钮（模拟 lockBtn/unlockBtn 所需的最小接口）。
function fakeBtn(stepId) {
  const attrs = { "data-ws-retry": stepId };
  return {
    disabled: false,
    textContent: "↻ 重试此节点",
    __idleText: null,
    getAttribute: (k) => (k in attrs ? attrs[k] : null),
    setAttribute: (k, v) => {
      attrs[k] = String(v);
    },
    removeAttribute: (k) => {
      delete attrs[k];
    },
  };
}

test("模块导出与浏览器骨架完好", () => {
  assert.equal(panel.id, "workswarm");
  assert.equal(typeof panel.mount, "function");
  const html = panel.nav();
  assert.match(html, /data-panel="workswarm"/);
  assert.match(html, /<style>/);
  assert.ok(!html.includes("##"), "禁止出现 '##' 非法选择器（历史空白面板回归）");
});

test("normStatus / isTerminalTeam 状态归一化", () => {
  assert.equal(T.normStatus("AwaitingHuman"), "awaiting_human");
  assert.equal(T.normStatus("Running"), "running");
  assert.equal(T.normStatus(null), "");
  assert.equal(T.isTerminalTeam("Succeeded"), true);
  assert.equal(T.isTerminalTeam("Cancelled"), true);
  assert.equal(T.isTerminalTeam("Failed"), true);
  assert.equal(T.isTerminalTeam("Running"), false);
});

test("computeRunSummary：混合 DAG 计数/失败步骤/累计尝试/阻塞", () => {
  const s = T.computeRunSummary({ team: { status: "Failed" }, tasks: TASKS_MIXED, artifactCount: 2 });
  assert.deepEqual(s.counts, { total: 3, succeeded: 1, failed: 1, waiting: 1, running: 0, blocked: 1 });
  assert.equal(s.totalAttempts, 4);
  assert.equal(s.failedStep.task_id, "build");
  assert.equal(s.failedStep.attempts, 3);
  assert.equal(s.phase, "已停止：存在失败步骤");
  assert.equal(s.canRetry, true);
  assert.equal(s.artifactCount, 2);
});

test("computeRunSummary：中断团队不再显示为正常执行中", () => {
  const s = T.computeRunSummary({ team: { status: "Running" }, tasks: [], interrupted: true });
  assert.equal(s.statusKey, "interrupted");
  assert.equal(s.statusLabel, "已中断（可恢复）");
  assert.equal(s.phase, "已中断，可恢复");
  assert.notEqual(s.phase, "调度中");
});

test("computeRunSummary：终态中断标记不覆盖终态显示；产物未知显示 null", () => {
  const s = T.computeRunSummary({ team: { status: "Failed" }, tasks: TASKS_MIXED, interrupted: true, artifactCount: null });
  assert.equal(s.statusKey, "failed");
  assert.equal(s.statusLabel, "失败");
  assert.equal(s.artifactCount, null);
});

test("computeRunSummary：等待人节点/运行阶段推导", () => {
  const awaiting = T.computeRunSummary({
    team: { status: "AwaitingHuman" },
    tasks: [{ task_id: "h", role: "human-review", worker: "u-1", status: "Ready", attempts: 0, depends_on: [] }],
  });
  assert.equal(awaiting.phase, "等待人节点结果");
  const running = T.computeRunSummary({
    team: { status: "Running" },
    tasks: [{ task_id: "b", role: "builder", worker: "m-b", status: "Running", attempts: 1, depends_on: [] }],
  });
  assert.equal(running.phase, "执行：builder");
  assert.equal(running.counts.running, 1);
});

test("shouldShowRetry / retryableTasks 门控矩阵", () => {
  assert.equal(T.shouldShowRetry(TASKS_MIXED, "Succeeded"), false);
  assert.equal(T.shouldShowRetry(TASKS_MIXED, "Cancelled"), false);
  assert.equal(T.shouldShowRetry(TASKS_MIXED, "Failed"), true);
  assert.equal(T.shouldShowRetry(TASKS_MIXED, "Running"), true);
  assert.equal(T.shouldShowRetry([], "Failed"), false);
  assert.deepEqual(
    T.retryableTasks(TASKS_MIXED).map((t) => t.task_id),
    ["build"]
  );
  assert.equal(T.isRetryableStep({ status: "Aborted" }), true);
  assert.equal(T.isRetryableStep({ status: "Succeeded" }), false);
});

test("buildRetryBody：第三路冻结契约形状", () => {
  const b = T.buildRetryBody("builder");
  assert.deepEqual(b, { command: "retry", step_id: "builder", note: "重试此节点：builder" });
  assert.equal(Object.keys(b).sort().join(","), "command,note,step_id");
  const b2 = T.buildRetryBody(" builder ", " 修复输入后重试 ");
  assert.equal(b2.step_id, "builder");
  assert.equal(b2.note, "修复输入后重试");
});

test("dagSvg：仅 Failed/Aborted 节点出现重试按钮；终态成功团队不出现", () => {
  const svgFailed = T.dagSvg(TASKS_MIXED, "Failed");
  assert.match(svgFailed, /data-ws-retry="build"/);
  assert.ok(!svgFailed.includes('data-ws-retry="plan"'));
  assert.ok(!svgFailed.includes('data-ws-retry="review"'));
  assert.match(svgFailed, /class="owo-ws-node st-pending blocked"/);
  assert.match(svgFailed, /marker-end="url\(#ws-arrow-dead\)"/);
  const svgDone = T.dagSvg(TASKS_MIXED, "Succeeded");
  assert.ok(!svgDone.includes("data-ws-retry"), "终态成功团队不得出现重试入口");
});

test("renderDetail 骨架：运行摘要 / 中断横幅 / 审计键盘焦点 / retry 提示", () => {
  const html = T.renderDetail();
  assert.match(html, /id="ws-d-summary" class="owo-ws-sumbox" aria-live="polite"/);
  assert.match(html, /id="ws-d-interrupted" role="alert"/);
  assert.match(html, /运行已中断，可恢复/);
  assert.match(html, /id="ws-d-audit"[^>]*tabindex="0"/);
  assert.match(html, /continue \/ retry（失败节点按钮）/);
  assert.match(html, /id="ws-act-continue"/);
  assert.match(html, /id="ws-act-cancel"/);
  assert.ok(!html.includes("##"));
});

test("renderList 骨架：加载三态在场", () => {
  const html = T.renderList();
  assert.match(html, /id="ws-list-body"/);
  assert.match(html, /owo-ws-loading/);
});

test("explainError：400/404/409/网络/凭据映射", () => {
  const e409 = T.explainError({ message: '409: {"error":"任务 build 已成功，不能 retry"}' }, "重试");
  assert.match(e409, /状态冲突（409）/);
  const e404 = T.explainError({ message: '404: {"error":"任务 x 不存在"}' }, "重试");
  assert.match(e404, /资源不存在（404）/);
  const e400 = T.explainError({ message: '400: {"error":"retry 需要 step_id 字段"}' }, "重试");
  assert.match(e400, /请求被拒绝（400）/);
  assert.match(T.explainError({ message: "Failed to fetch" }), /无法连接 owo-agent-server/);
  const cred = T.explainError({ message: '500: {"error":"OPENAI_API_KEY missing"}' });
  assert.match(cred, /环境变量/);
});

test("handleRetryClick：快速双击只产生一次请求；结束后解锁并恢复文案", async () => {
  resetState();
  T.state.current = "team-1";
  T.state.team = { team_id: "team-1", status: "Failed" };
  T.state.teamStatus = "Failed";
  const posts = [];
  T.setTransport({
    get: () =>
      Promise.resolve({ team: { team_id: "team-1", status: "Failed" }, tasks: TASKS_MIXED, interrupted: false, audit_tail: [] }),
    post: (path, body) => {
      posts.push({ path, body });
      return Promise.resolve({ team_id: "team-1", status: "Running", interrupted: false });
    },
  });
  const btn = fakeBtn("build");
  assert.equal(T.handleRetryClick(btn), true);
  assert.equal(btn.getAttribute("data-busy"), "1"); // 锁在入口同步生效
  assert.equal(T.handleRetryClick(btn), false); // 双击第二击被丢弃
  assert.equal(T.handleRetryClick(fakeBtnDisabled()), false); // 禁用不触发
  await drain();
  await drain();
  assert.equal(posts.length, 1, "快速双击必须只发一次请求");
  assert.equal(posts[0].path, "/teams/team-1/steer");
  assert.deepEqual(posts[0].body, { command: "retry", step_id: "build", note: "重试此节点：build" });
  assert.equal(btn.getAttribute("data-busy"), null, "结束后必须解锁");
  assert.equal(btn.getAttribute("aria-busy"), null);
  assert.equal(btn.disabled, false);
  assert.equal(btn.textContent, "↻ 重试此节点");
});

function fakeBtnDisabled() {
  const b = fakeBtn("build");
  b.disabled = true;
  return b;
}

test("submitRetry：终态成功/取消团队拒绝且零请求", async () => {
  resetState();
  T.state.current = "team-2";
  T.state.team = { team_id: "team-2", status: "Succeeded" };
  T.state.teamStatus = "Succeeded";
  const posts = [];
  T.setTransport({ post: (p, b) => (posts.push({ p, b }), Promise.resolve({})) });
  await T.submitRetry("build");
  assert.equal(posts.length, 0);
  T.state.teamStatus = "Cancelled";
  T.state.team = { team_id: "team-2", status: "Cancelled" };
  await T.submitRetry("build");
  assert.equal(posts.length, 0);
});

test("syncDetail：interrupted 标记从详情响应流入面板状态", async () => {
  resetState();
  T.state.current = "team-3";
  T.setTransport({
    get: () =>
      Promise.resolve({ team: { team_id: "team-3", status: "Running" }, tasks: [], interrupted: true, audit_tail: [] }),
  });
  await T.syncDetail();
  assert.equal(T.state.interrupted, true);
  assert.equal(T.state.team.team_id, "team-3");
});

test("style.css 第 16 节守卫：inline 换行/中断徽标/重试按钮/表格包裹/窄栏断点/焦点环", () => {
  const flat = shellCss.replace(/\s+/g, "");
  assert.ok(flat.includes(".owo-ws-panel.owo-ws-inline{display:flex;flex-wrap:wrap"), "inline 行必须可换行（防 1280 溢出）");
  assert.ok(flat.includes(".owo-ws-badge.st-interrupted"), "中断徽标样式在场");
  assert.ok(flat.includes(".owo-ws-panel.owo-ws-interrupted"), "中断横幅样式在场");
  assert.ok(flat.includes("button.owo-ws-retry"), "重试按钮样式在场");
  assert.ok(flat.includes(".owo-ws-panel.owo-ws-tablewrap"), "表格横向滚动包裹在场");
  assert.ok(flat.includes("@media(max-width:1280px)"), "窄栏断点在场");
  assert.ok(flat.includes(".owo-ws-panel.owo-ws-audit:focus-visible"), "审计区键盘焦点环在场");
  assert.ok(flat.includes(".owo-ws-panel.owo-ws-ellip"), "长文本省略工具类在场");
});
