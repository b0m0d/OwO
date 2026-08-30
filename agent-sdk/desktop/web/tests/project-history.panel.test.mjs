// ============================================================================
// 项目与运行历史面板 Node 前端测试 —— desktop/web/tests/project-history.panel.test.mjs
//
// 运行：node --test "tests/*.test.mjs"（在 agent-sdk/desktop/web 下）
// 依赖：仅 node:test / node:assert；无 DOM、无网络（传输层经 _test.setTransport 注入）。
//
// 覆盖面（八期第四路验收守卫）：
//   - 纯逻辑：状态归一、耗时人类可读、计划预算容错、任务计数、模板/项目过滤选项
//     （动态组队桶）、四维过滤、可见列表排序截断、objective 提取三级回退
//     （additive → audit 启发式 → null）、复跑请求体（模板/工作区六期冻结形状）；
//   - 渲染守卫：面板骨架、过滤下拉 selected、行卡片（徽标/耗时/预算/交付物/
//     data 属性）、复跑提交锁禁用、结果区、空态文案；
//   - 行为：load() 聚合落 state（deliverables 404 静默、workspace 404 → 未绑定、
//     详情失败降级不阻塞）、startRerun 成功/失败/提交锁（快速双击只发一次）、
//     缺 objective 不发请求；
//   - 接线守卫：index.html 脚本、app.js PANEL_ORDER、style.css 面板样式节在场。
// ============================================================================
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const panel = require(join(here, "../panels/project-history.panel.js"));
const T = panel._test;
const shellCss = readFileSync(join(here, "../style.css"), "utf8");
const indexHtml = readFileSync(join(here, "../index.html"), "utf8");
const appJs = readFileSync(join(here, "../app.js"), "utf8");

// ---------- fixture 工具 ----------

function team(id, status, extra) {
  return Object.assign(
    {
      team_id: id,
      status: status,
      mode: "Team",
      template_id: null,
      project_space_id: "proj-" + id,
      created_at: "2026-08-30T04:00:00+00:00",
      updated_at: "2026-08-30T04:02:00+00:00",
      budget: {},
    },
    extra || {}
  );
}

function detail(teamId, tasks, auditTail, extra) {
  return Object.assign(
    {
      team: { team_id: teamId, mode: "Team", template_id: null },
      tasks: tasks || [],
      interrupted: false,
      audit_tail: auditTail || [],
    },
    extra || {}
  );
}

function resetState() {
  T.state.loading = false;
  T.state.loadedOnce = false;
  T.state.teams = [];
  T.state.details = {};
  T.state.detailErrors = {};
  T.state.deliverables = {};
  T.state.workspaces = {};
  T.state.filters = { status: "", template: "", project: "", q: "" };
  T.state.rerunBusy = {};
  T.state.rerunResults = {};
  T.state.errors = [];
  T.state.lastLoadedAt = "";
  T.setRoot(null);
}

// ---------- 纯逻辑 ----------

test("normStatus：Debug 形式与 snake_case 归一一致", () => {
  assert.equal(T.normStatus("Succeeded"), "succeeded");
  assert.equal(T.normStatus("AwaitingHuman"), "awaiting_human");
  assert.equal(T.normStatus("awaiting_human"), "awaiting_human");
  assert.equal(T.normStatus(null), "");
});

test("durationText：人类可读且容错缺失/倒挂", () => {
  assert.equal(T.durationText("2026-08-30T04:00:00Z", "2026-08-30T04:00:45Z"), "45s");
  assert.equal(T.durationText("2026-08-30T04:00:00Z", "2026-08-30T04:02:00Z"), "2m 0s");
  assert.equal(T.durationText("2026-08-30T04:00:00Z", "2026-08-30T05:02:00Z"), "1h 2m");
  assert.equal(T.durationText(null, "2026-08-30T04:00:00Z"), "");
  assert.equal(T.durationText("2026-08-30T05:00:00Z", "2026-08-30T04:00:00Z"), "");
  assert.equal(T.durationText("not-a-date", "also-bad"), "");
});

test("plannedCalls：strategy_decision.budget_calls_total 容错", () => {
  assert.equal(T.plannedCalls({ strategy_decision: { budget_calls_total: 7 } }), 7);
  assert.equal(T.plannedCalls({ strategy_decision: null }), null);
  assert.equal(T.plannedCalls({}), null);
  assert.equal(T.plannedCalls({ strategy_decision: { budget_calls_total: 0 } }), null);
});

test("taskCounts：成功/失败归类；空任务 → null", () => {
  const c = T.taskCounts({
    tasks: [
      { status: "Succeeded" },
      { status: "succeeded" },
      { status: "Failed" },
      { status: "Pending" },
    ],
  });
  assert.deepEqual(c, { total: 4, done: 2, failed: 1 });
  assert.equal(T.taskCounts({ tasks: [] }), null);
  assert.equal(T.taskCounts(null), null);
});

test("templateOptions/projectOptions：去重 + 动态组队桶 + 派生项目键", () => {
  const teams = [
    team("t1", "succeeded", { template_id: "code-change-v1" }),
    team("t2", "failed", { template_id: "code-change-v1" }),
    team("t3", "running", { template_id: null }),
    team("t4", "succeeded", { template_id: null, project_space_id: null }),
  ];
  const tpls = T.templateOptions(teams);
  assert.deepEqual(
    tpls.map((o) => o.value),
    ["", "code-change-v1"]
  );
  assert.equal(tpls.find((o) => o.value === "").label, "动态组队（无模板）");
  const projs = T.projectOptions(teams);
  assert.deepEqual(
    projs.map((o) => o.value).sort(),
    ["proj-t1", "proj-t2", "proj-t3", "proj-t4"]
  );
  assert.equal(T.projectKeyOf({ team_id: "t9", project_space_id: null }), "proj-t9");
});

test("matchesFilters：四维过滤（状态归一/模板含动态桶/项目/搜索）", () => {
  const t = team("team-a", "Succeeded", { template_id: "code-change-v1" });
  assert.ok(T.matchesFilters(t, {}));
  assert.ok(T.matchesFilters(t, { status: "succeeded" }));
  assert.ok(!T.matchesFilters(t, { status: "failed" }));
  assert.ok(T.matchesFilters(t, { template: "code-change-v1" }));
  assert.ok(!T.matchesFilters(t, { template: "research-brief-v1" }));
  assert.ok(T.matchesFilters(t, { project: "proj-team-a" }));
  assert.ok(!T.matchesFilters(t, { project: "proj-other" }));
  assert.ok(T.matchesFilters(t, { q: "TEAM-A" }));
  assert.ok(!T.matchesFilters(t, { q: "zzz" }));
  assert.ok(!T.matchesFilters(null, {}));
});

test("visibleTeams：过滤 + created_at 降序 + 截断", () => {
  const teams = [
    team("old", "succeeded", { created_at: "2026-08-28T00:00:00Z" }),
    team("new", "failed", { created_at: "2026-08-30T00:00:00Z" }),
    team("mid", "succeeded", { created_at: "2026-08-29T00:00:00Z" }),
    team("hidden", "failed", { created_at: "2026-08-31T00:00:00Z" }),
  ];
  const out = T.visibleTeams(teams, { status: "succeeded" }, 2);
  assert.deepEqual(
    out.map((t) => t.team_id),
    ["mid", "old"]
  );
});

test("objectiveFromDetail：additive 优先 → audit「目标 …」启发式 → null", () => {
  assert.equal(T.objectiveFromDetail({ objective: " 直接目标 " }), "直接目标");
  assert.equal(
    T.objectiveFromDetail({
      audit_tail: [
        { event: "team.succeeded", detail: "目标达成" },
        { event: "goal.started", detail: "目标 调研 A 与 B 的差异" },
      ],
    }),
    "调研 A 与 B 的差异"
  );
  assert.equal(T.objectiveFromDetail({ audit_tail: [{ detail: "无关" }] }), null);
  assert.equal(T.objectiveFromDetail(null), null);
});

test("rerunBody：同目标+模板+工作区（六期冻结形状）；缺 objective 报错", () => {
  const d = detail("t1", [], [{ event: "goal.started", detail: "目标 做一份研究简报" }], {
    team: { team_id: "t1", mode: "Team", template_id: "research-brief-v1" },
  });
  const ws = {
    root: "T:/ws",
    read_only: false,
    write_allowed_paths: ["src/out"],
    tree_depth: 3,
  };
  const body = T.rerunBody(d, ws);
  assert.equal(body.objective, "做一份研究简报");
  assert.equal(body.mode, "team");
  assert.equal(body.template_id, "research-brief-v1");
  assert.deepEqual(body.workspace, {
    root: "T:/ws",
    read_only: false,
    write_allowed_paths: ["src/out"],
    tree_depth: 3,
  });

  // 未绑定工作区 → 不带 workspace 字段
  const body2 = T.rerunBody(d, null);
  assert.ok(!("workspace" in body2));

  // objective 覆盖优先
  const body3 = T.rerunBody(d, null, "手工目标");
  assert.equal(body3.objective, "手工目标");

  // 无 objective → 明确错误标记（不发请求）
  assert.deepEqual(T.rerunBody({ team: {}, audit_tail: [] }, null), {
    error: "missing_objective",
  });
});

// ---------- 渲染守卫 ----------

test("shellHtml：骨架完整（标题/刷新/错误区/过滤区/列表区）", () => {
  const html = T.shellHtml();
  assert.match(html, /项目与运行历史/);
  assert.match(html, /id="ph-refresh"/);
  assert.match(html, /id="ph-errors"/);
  assert.match(html, /id="ph-filters-box"/);
  assert.match(html, /id="ph-list"/);
});

test("filtersHtml：状态下拉 selected 与模板/项目选项", () => {
  T.state.teams = [
    team("t1", "succeeded", { template_id: "code-change-v1" }),
    team("t2", "failed", { template_id: null }),
  ];
  T.state.filters = { status: "failed", template: "", project: "", q: "" };
  const html = T.filtersHtml(T.state);
  assert.match(html, /value="failed" selected/);
  assert.match(html, /动态组队（无模板）/);
  assert.match(html, /code-change-v1/);
});

test("rowHtml：徽标/耗时/预算/交付物/动作 data 属性 + 提交锁 + 结果区", () => {
  const t = team("team-x", "Succeeded", {
    template_id: "code-change-v1",
    strategy_decision: { budget_calls_total: 7 },
  });
  const html = T.rowHtml(t, {
    detail: { tasks: [{ status: "Succeeded" }, { status: "Failed" }] },
    deliverables: { approved: 2, pending: 1 },
    workspace: null,
    objective: "做一个研究简报",
  });
  assert.match(html, /data-ph-row="team-x"/);
  assert.match(html, /ph-st-succeeded/);
  assert.match(html, /已完成/);
  assert.match(html, /耗时 2m 0s/);
  assert.match(html, /预算 7 次调用|预算/);
  assert.match(html, /任务 1\/2（失败 1）/);
  assert.match(html, /批准 2 · 待评审 1/);
  assert.match(html, /工作区未绑定/);
  assert.match(html, /data-ph-open="team-x"/);
  assert.match(html, /data-ph-rerun="team-x"/);
  assert.match(html, /做一个研究简报/);

  // 提交锁：busy 禁用 + 结果文本
  T.state.rerunBusy["team-x"] = true;
  T.state.rerunResults["team-x"] = { ok: true, text: "已按同配置创建新运行：team-y。" };
  const html2 = T.rowHtml(t, {});
  assert.match(html2, /disabled/);
  assert.match(html2, /已按同配置创建新运行：team-y。/);
  delete T.state.rerunBusy["team-x"];
  delete T.state.rerunResults["team-x"];
});

test("listHtml：空态文案", () => {
  const html = T.listHtml([], T.state);
  assert.match(html, /没有匹配的历史运行/);
});

// ---------- 行为（传输层注入） ----------

function transportMock(map, log) {
  return {
    get(path) {
      log.push("GET " + path);
      const h = map[path];
      if (h === undefined) return Promise.reject(new Error("404: not found"));
      if (h instanceof Error) return Promise.reject(h);
      return Promise.resolve(h);
    },
    post(path, body) {
      log.push("POST " + path + " " + JSON.stringify(body));
      const h = map["POST " + path];
      if (h === undefined) return Promise.reject(new Error("404: not found"));
      if (h instanceof Error) return Promise.reject(h);
      return Promise.resolve(typeof h === "function" ? h(body) : h);
    },
  };
}

test("load()：列表 + 详情/交付物/工作区富化；404 静默降级", async () => {
  resetState();
  const log = [];
  const teams = {
    teams: [
      team("t1", "Succeeded", { project_space_id: "proj-p1" }),
      team("t2", "Failed", { project_space_id: null }),
    ],
  };
  const map = {
    "/teams": teams,
    "/teams/t1": detail("t1", [{ status: "Succeeded" }], [
      { detail: "目标 调研要点" },
    ]),
    "/teams/t2": new Error("500: boom"),
    "/projects/proj-p1/deliverables": { approved: [{}], pending_review: [{}] },
    "/projects/proj-p1/workspace": { root: "T:/ws", read_only: true },
    "/projects/proj-t2/deliverables": new Error("404: none"),
    "/projects/proj-t2/workspace": new Error("404: none"),
  };
  T.setTransport(transportMock(map, log));
  T.setRoot({ innerHTML: "", querySelector: () => null });
  await T.load();
  assert.equal(T.state.teams.length, 2);
  assert.ok(T.state.details["t1"]);
  assert.ok(T.state.detailErrors["t2"]);
  assert.deepEqual(T.state.deliverables["proj-p1"], { approved: 1, pending: 1 });
  // 404 交付物 → 空计数且无错误文案
  assert.deepEqual(T.state.deliverables["proj-t2"], { approved: 0, pending: 0, error: "" });
  // 404 工作区 → 未绑定（null）
  assert.equal(T.state.workspaces["proj-p1"].root, "T:/ws");
  assert.equal(T.state.workspaces["proj-t2"], null);
  assert.equal(T.state.loading, false);
  const paths = log.map((x) => x.split(" ")[1]);
  assert.ok(paths.includes("/teams/t1"));
  assert.ok(paths.includes("/projects/proj-p1/deliverables"));
  assert.ok(!log.some((x) => x.startsWith("POST ")));
});

test("startRerun()：成功路径（请求体含模板与工作区）+ 提交锁双击只发一次", async () => {
  resetState();
  const log = [];
  T.state.teams = [team("t1", "Succeeded", { template_id: "code-change-v1", project_space_id: "proj-p1" })];
  T.state.details["t1"] = detail("t1", [], [{ detail: "目标 修复计算bug" }], {
    team: { team_id: "t1", mode: "Team", template_id: "code-change-v1" },
  });
  T.state.workspaces["proj-p1"] = { root: "T:/ws", read_only: true };
  const map = {
    "POST /teams": { team_id: "team-new", project_space_id: "proj-team-new" },
  };
  T.setTransport(transportMock(map, log));
  const p1 = T.startRerun("t1");
  const p2 = T.startRerun("t1"); // 提交锁：第二次调用在 busy 期间直接返回
  await Promise.all([p1, p2]);
  const posts = log.filter((x) => x.startsWith("POST /teams"));
  assert.equal(posts.length, 1, "双击只发一次：" + JSON.stringify(posts));
  assert.match(posts[0], /"objective":"修复计算bug"/);
  assert.match(posts[0], /"template_id":"code-change-v1"/);
  assert.match(posts[0], /"workspace":\{"root":"T:\/ws"/);
  assert.equal(T.state.rerunResults["t1"].ok, true);
  assert.equal(T.state.rerunResults["t1"].new_team_id, "team-new");
  assert.equal(T.state.rerunBusy["t1"], false);
});

test("startRerun()：失败兜底（friendly 文本，不抛错）", async () => {
  resetState();
  T.state.teams = [team("t1", "Succeeded")];
  T.state.details["t1"] = detail("t1", [], [{ detail: "目标 X" }], {
    team: { team_id: "t1", mode: "Team" },
  });
  T.setTransport(
    transportMock({ "POST /teams": new Error("400: bad request") }, [])
  );
  await T.startRerun("t1");
  assert.equal(T.state.rerunResults["t1"].ok, false);
  assert.match(T.state.rerunResults["t1"].text, /复跑失败/);
});

test("startRerun()：缺 objective 不发请求", async () => {
  resetState();
  T.state.teams = [team("t1", "Succeeded")];
  T.state.details["t1"] = detail("t1", [], [{ detail: "无关事件" }], {
    team: { team_id: "t1", mode: "Team" },
  });
  const log = [];
  T.setTransport(transportMock({}, log));
  await T.startRerun("t1");
  assert.ok(!log.some((x) => x.startsWith("POST ")));
  assert.match(T.state.rerunResults["t1"].text, /无法自动提取原目标/);
});

// ---------- 接线守卫 ----------

test("index.html 引入 project-history 面板脚本", () => {
  assert.match(indexHtml, /panels\/project-history\.panel\.js/);
});

test("app.js PANEL_ORDER 注册 project-history", () => {
  assert.match(appJs, /"project-history"/);
});

test("style.css 提供面板样式节（.owo-ac-* 复用 + ph 专属类）", () => {
  assert.match(shellCss, /\.ph-row\s*\{|\.ph-row\b/);
  assert.match(shellCss, /\.ph-st-/);
});
