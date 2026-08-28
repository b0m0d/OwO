// ============================================================================
// 产品评测中心面板（eval.panel.js · 第三路 V1-R1-UI 重写）
//
// 旧版是"eval 护栏"同步运行按钮（POST /eval/gate/run 等旧端点）；本版升级为
// 可操作的产品评测中心，对接第四路冻结的异步评测 API：
//   POST   /product-eval/runs            {suite, execution, modes, repetitions, category, only} → 202 {run_id, status}
//   GET    /product-eval/runs            历史运行列表
//   GET    /product-eval/runs/{id}       运行详情（进度/聚合指标/逐 case 单 vs 多对比/Artifact）
//   POST   /product-eval/runs/{id}/cancel  幂等取消
//
// 运行状态机（服务端权威）：queued → running → completed | failed | cancelled
//   interrupted：服务重启时未结束的运行被标记，不自动重跑、不自动计费。
//
// 面板行为契约：
//   - 启动按钮同步提交锁：连点/双击只发出一次 POST，请求期间禁用；
//   - 2 秒轮询运行详情；面板被切走（DOM 脱离文档）立即停止轮询；
//     运行进入终态（completed/failed/cancelled/interrupted）自动停轮询；
//   - 取消按钮仅在 queued/running 可用；取消后下一轮询周期内呈现 cancelled；
//   - 单 Agent 与 WorkSwarm 指标并列对比；逐 case 行可展开失败明细；
//   - Artifact ref 一键复制；长错误单行省略 + tooltip，不撑破布局；
//   - loading / empty / failed 三态与终态徽标沿用 WorkSwarm 面板视觉语言；
//   - 防溢出与状态样式在 style.css 第 17 节（.owo-pe-* 作用域）。
//
// Node 兼容：window 回退 globalThis + module.exports 导出 _test 纯逻辑挂钩，
// 供 tests/product-eval.panel.test.mjs 做结构/行为断言（浏览器零差异）。
// ============================================================================
(function () {
  "use strict";

  // Node 测试环境兼容：window 未定义时回退 globalThis（浏览器行为不变）。
  var win = typeof window !== "undefined" ? window : globalThis;

  win.OwoPanels = win.OwoPanels || {};

  win.OwoPanels.eval = (function () {
    var id = "eval"; // 与 app.js PANEL_ORDER 中的注册名一致

    // ---------- helpers（优先 app.js 注入，缺失时自建回退） ----------
    var H = {};
    var rootEl = null;
    var tokenPromise = null;

    function defaultToken() {
      if (!tokenPromise) {
        tokenPromise = fetch(H.baseUrl + "/auth/token").then(function (r) {
          if (!r.ok) throw new Error("token 引导失败（HTTP " + r.status + "）");
          return r.json().then(function (d) {
            var t = d && d.token;
            if (!t) throw new Error("token 引导响应缺少 token");
            return t;
          });
        }).catch(function (e) {
          tokenPromise = null;
          throw e;
        });
      }
      return tokenPromise;
    }

    function httpFinish(r) {
      if (!r.ok) {
        return r.text().then(function (b) {
          throw new Error(r.status + ": " + b);
        });
      }
      if (r.status === 204) return null;
      return r.json();
    }

    function defaultGet(path) {
      return defaultToken().then(function (tok) {
        return fetch(H.baseUrl + path, {
          headers: { "Authorization": "Bearer " + tok, "Accept": "application/json" },
        }).then(httpFinish);
      });
    }

    function defaultPost(path, body) {
      return defaultToken().then(function (tok) {
        return fetch(H.baseUrl + path, {
          method: "POST",
          headers: {
            "Authorization": "Bearer " + tok,
            "Content-Type": "application/json",
            "Accept": "application/json",
          },
          body: JSON.stringify(body || {}),
        }).then(httpFinish);
      });
    }

    function defaultEsc(s) {
      return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
        return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
      });
    }

    function esc(s) {
      return H.esc ? H.esc(s) : defaultEsc(s);
    }

    // ---------- 状态 ----------
    var state = {
      config: { suite: "v1", execution: "reference", modes: ["single", "workswarm"], repetitions: 1, category: "", only: "" },
      submitting: false, // 启动按钮同步提交锁（连点只发一次）
      cancelling: false,
      runs: [], // 历史运行列表
      runId: null, // 当前查看的 run_id
      detail: null, // 最新详情响应
      loadFailed: "", // 详情/列表加载失败的友好文案（failed 三态）
      loading: false,
      seq: 0, // 详情响应流水号：晚到的旧响应一律作废
      pollTimer: null,
    };

    // ---------- 常量与纯逻辑（_test 挂钩的一部分） ----------
    var RUN_STATUSES = ["queued", "running", "interrupted", "cancelled", "completed", "failed"];
    var STATUS_CN = {
      queued: "已排队",
      running: "运行中",
      interrupted: "已中断",
      cancelled: "已取消",
      completed: "已完成",
      failed: "失败",
    };
    var TERMINAL_STATUSES = { completed: true, failed: true, cancelled: true, interrupted: true };
    var CANCELLABLE = { queued: true, running: true };
    var EXECUTION_CN = { reference: "reference（离线基线）", live: "live（真实模型计费）" };
    var ENGINE_CN = { single: "单 Agent", workswarm: "WorkSwarm" };

    function normRunStatus(s) {
      var v = String(s == null ? "" : s).trim().toLowerCase();
      return RUN_STATUSES.indexOf(v) >= 0 ? v : v;
    }

    function isTerminalRun(s) {
      return !!TERMINAL_STATUSES[normRunStatus(s)];
    }

    function isCancellable(s) {
      return !!CANCELLABLE[normRunStatus(s)];
    }

    function badgeClass(s) {
      var st = normRunStatus(s);
      if (st === "completed") return "ok";
      if (st === "failed") return "bad";
      if (st === "running" || st === "queued") return "run";
      if (st === "interrupted") return "warn";
      return "off"; // cancelled 及未知
    }

    // 组装 POST /product-eval/runs 请求体：全部做规范化，禁止脏值上行。
    function buildRunBody(cfg) {
      var c = cfg || {};
      var modes = Array.isArray(c.modes) ? c.modes.filter(function (m) { return m === "single" || m === "workswarm"; }) : [];
      if (!modes.length) modes = ["single", "workswarm"];
      var reps = parseInt(c.repetitions, 10);
      if (!isFinite(reps) || reps < 1) reps = 1;
      if (reps > 50) reps = 50;
      var suite = String(c.suite == null ? "" : c.suite).trim() || "v1";
      var execution = c.execution === "live" ? "live" : "reference";
      var category = String(c.category == null ? "" : c.category).trim();
      var only = String(c.only == null ? "" : c.only).trim();
      return {
        suite: suite,
        execution: execution,
        modes: modes,
        repetitions: reps,
        category: category || null,
        only: only || null,
      };
    }

    // —— 展示格式化（null 安全；"—" 表示未知） ——
    function fmtRate(x) {
      if (x == null || !isFinite(Number(x))) return "—";
      return (Number(x) * 100).toFixed(1) + "%";
    }

    function fmtDur(ms) {
      var v = Number(ms);
      if (ms == null || !isFinite(v)) return "—";
      if (v < 0) v = 0;
      if (v < 60000) return (v / 1000).toFixed(1) + "s";
      var m = Math.floor(v / 60000);
      var s = Math.round((v % 60000) / 1000);
      return m + "m" + (s < 10 ? "0" : "") + s + "s";
    }

    function fmtInt(x) {
      var v = Number(x);
      if (x == null || !isFinite(v)) return "—";
      return String(Math.round(v));
    }

    function fmtCost(usd) {
      var v = Number(usd);
      if (usd == null || !isFinite(v)) return "—";
      if (v === 0) return "$0.00";
      // 评测单引擎成本通常在分级以下：<$1 用 4 位小数保留分辨率，其余 2 位。
      if (v > 0 && v < 1) return "$" + v.toFixed(4);
      return "$" + v.toFixed(2);
    }

    // 从指标对象宽容取键（第四路字段命名演进期间不碎）。
    function pickMetric(obj, keys) {
      if (!obj) return null;
      for (var i = 0; i < keys.length; i++) {
        if (obj[keys[i]] != null) return obj[keys[i]];
      }
      return null;
    }

    // 引擎模式归一：服务端 AgentMode serde 为 "single"/"multi"，请求侧用 "workswarm"。
    function normMode(m) {
      var v = String(m == null ? "" : m).trim().toLowerCase();
      return v === "multi" || v === "workswarm" ? "workswarm" : v;
    }

    // 按引擎聚合 report.runs（原始矩阵单元格）：成功率/平均耗时/模型调用/Token/费用。
    // detail.report 为 null（首格未完成）时返回 null，上层显示 "—"。
    function engineAgg(report, mode) {
      var runs = (report && report.runs) || [];
      var mine = [];
      for (var i = 0; i < runs.length; i++) {
        if (normMode(runs[i] && runs[i].key && runs[i].key.agent_mode) === mode) mine.push(runs[i]);
      }
      if (!mine.length) return null;
      var passed = 0, wall = 0, calls = 0, tokens = null, cost = null;
      for (var j = 0; j < mine.length; j++) {
        var r = mine[j];
        if (String(r.status) === "passed") passed++;
        wall += Number(r.wall_ms) || 0;
        calls += Number(r.model_calls) || 0;
        if (r.total_tokens != null) tokens = (tokens || 0) + Number(r.total_tokens);
        if (r.cost_usd != null) cost = (cost || 0) + Number(r.cost_usd);
      }
      return { total: mine.length, passed: passed, rate: passed / mine.length, meanWall: wall / mine.length, calls: calls, tokens: tokens, cost: cost };
    }

    // ==================== 四期：统计判读（第一路统计口径） ====================
    // Wilson 95% 置信区间（纯函数）：n<=0 或非法输入返回 null。
    function wilsonCI(passed, n) {
      if (!isFinite(n) || !isFinite(passed) || n <= 0 || passed < 0 || passed > n) return null;
      var z = 1.959963984540054; // 双侧 95%
      var ph = passed / n;
      var denom = 1 + (z * z) / n;
      var center = (ph + (z * z) / (2 * n)) / denom;
      var half = (z * Math.sqrt((ph * (1 - ph)) / n + (z * z) / (4 * n * n))) / denom;
      return { lo: Math.max(0, center - half), hi: Math.min(1, center + half) };
    }

    // 最近序统计量（纯函数）：p95 = 第 ⌈p·n⌉ 个（升序）。
    function percentileOf(values, p) {
      if (!Array.isArray(values) || !values.length) return null;
      var a = values
        .map(function (x) { return Number(x); })
        .filter(function (x) { return isFinite(x); });
      if (!a.length) return null;
      a.sort(function (x, y) { return x - y; });
      if (a.length === 1) return a[0];
      var idx = Math.ceil((p / 100) * a.length) - 1;
      if (idx < 0) idx = 0;
      if (idx > a.length - 1) idx = a.length - 1;
      return a[idx];
    }

    // 单引擎样本统计：n/通过数/成功率/CI/p50/p95/调用/Token/费用。
    function engineStats(report, mode) {
      var runs = (report && report.runs) || [];
      var mine = [];
      for (var i = 0; i < runs.length; i++) {
        if (normMode(runs[i] && runs[i].key && runs[i].key.agent_mode) === mode) mine.push(runs[i]);
      }
      if (!mine.length) return null;
      var passed = 0, walls = [], calls = 0, tokens = 0, cost = 0, tokenKnown = 0, costKnown = 0;
      for (var j = 0; j < mine.length; j++) {
        var r = mine[j];
        if (String(r.status) === "passed") passed++;
        walls.push(Number(r.wall_ms) || 0);
        calls += Number(r.model_calls) || 0;
        if (r.total_tokens != null) { tokens += Number(r.total_tokens); tokenKnown++; }
        if (r.cost_usd != null) { cost += Number(r.cost_usd); costKnown++; }
      }
      var ci = wilsonCI(passed, mine.length);
      return {
        n: mine.length,
        passed: passed,
        rate: passed / mine.length,
        ci95: ci ? [ci.lo, ci.hi] : null,
        p50Ms: percentileOf(walls, 50),
        p95Ms: percentileOf(walls, 95),
        calls: calls,
        tokens: tokenKnown ? tokens : null,
        cost: costKnown ? cost : null,
      };
    }

    // 服务端统计（第一路 statistics 字段）宽容归一：识别 {lo,hi}|[lo,hi] 区间、
    // p50/p95 耗时、verdict/recommendation 文本；形状不符返回 null（走客户端推导）。
    function serverStats(report) {
      var s = report && (report.statistics || report.stats);
      if (!s || typeof s !== "object" || Array.isArray(s)) return null;
      function pair(v) {
        if (Array.isArray(v) && v.length === 2 && isFinite(Number(v[0])) && isFinite(Number(v[1]))) return [Number(v[0]), Number(v[1])];
        if (v && typeof v === "object" && !Array.isArray(v)) {
          var lo = v.lo != null ? Number(v.lo) : v.lower != null ? Number(v.lower) : NaN;
          var hi = v.hi != null ? Number(v.hi) : v.upper != null ? Number(v.upper) : NaN;
          if (isFinite(lo) && isFinite(hi)) return [lo, hi];
        }
        return null;
      }
      var per = s.per_engine || s.engines || {};
      function eng(x) {
        if (!x || typeof x !== "object" || Array.isArray(x)) return null;
        var rate = x.success_rate != null ? Number(x.success_rate) : x.rate != null ? Number(x.rate) : NaN;
        var ci = pair(x.success_rate_ci95 || x.ci95 || x.ci);
        var n = x.n != null ? Number(x.n) : x.samples != null ? Number(x.samples) : NaN;
        if (!isFinite(rate) && !ci && !isFinite(n)) return null;
        return {
          n: isFinite(n) ? n : null,
          rate: isFinite(rate) ? rate : null,
          ci95: ci,
          p50Ms: x.p50_wall_ms != null ? Number(x.p50_wall_ms) : null,
          p95Ms: x.p95_wall_ms != null ? Number(x.p95_wall_ms) : null,
        };
      }
      var single = eng(per.single || per.single_agent);
      var workswarm = eng(per.workswarm || per.multi || per.multi_agent);
      var verdict = s.verdict || s.recommendation || s.decision || null;
      var verdictText = verdict && typeof verdict === "object" ? verdict.text || verdict.verdict || verdict.summary || null : verdict;
      if (!single && !workswarm && !verdictText) return null;
      return { single: single, workswarm: workswarm, verdictText: verdictText };
    }

    // 统计视图模型：服务端统计在场时优先，否则从 report.runs 客户端推导。
    // n<30 一律标注"样本不足"；启用阈值（计划冻结）：成功率 +5% / 耗时 -30%。
    function statisticsFromReport(report) {
      var srv = serverStats(report);
      var single = (srv && srv.single) || engineStats(report, "single");
      var workswarm = (srv && srv.workswarm) || engineStats(report, "workswarm");
      if (!single && !workswarm && !(srv && srv.verdictText)) return null;
      function pctChange(a, b) {
        return a > 0 && b != null ? ((b - a) / a) * 100 : null;
      }
      var deltas = null;
      if (single && workswarm) {
        deltas = {
          rateDiff: workswarm.rate != null && single.rate != null ? workswarm.rate - single.rate : null,
          wallChangePct: pctChange(single.p50Ms, workswarm.p50Ms),
          callsChangePct: pctChange(single.calls, workswarm.calls),
          tokensChangePct: pctChange(single.tokens, workswarm.tokens),
          costChangePct: pctChange(single.cost, workswarm.cost),
        };
      }
      var rec = null;
      if (srv && srv.verdictText) {
        rec = { verdict: String(srv.verdictText), source: "server" };
      } else if (deltas && deltas.rateDiff != null) {
        var hits = [];
        if (deltas.rateDiff >= 0.05) hits.push("成功率 +" + (deltas.rateDiff * 100).toFixed(1) + "pp（≥ +5%）");
        if (deltas.wallChangePct != null && deltas.wallChangePct <= -30) hits.push("p50 耗时 " + deltas.wallChangePct.toFixed(0) + "%（≤ -30%）");
        rec = {
          verdict: hits.length ? "建议启用 WorkSwarm：" + hits.join("；") : "暂不建议启用 WorkSwarm（未满足成功率/耗时启用阈值）",
          source: "client",
        };
      }
      var sampleSmall =
        (single && single.n != null && single.n < 30) || (workswarm && workswarm.n != null && workswarm.n < 30);
      return { single: single, workswarm: workswarm, deltas: deltas, recommendation: rec, sampleSmall: sampleSmall };
    }

    function statEngineLine(label, st) {
      if (!st) return '<div class="owo-pe-stat-row sub">' + esc(label) + "：暂无样本</div>";
      var ci = st.ci95 ? "CI95 [" + (st.ci95[0] * 100).toFixed(1) + "%, " + (st.ci95[1] * 100).toFixed(1) + "%]" : "CI —";
      var small = st.n != null && st.n < 30;
      return (
        '<div class="owo-pe-stat-row">' +
        "<b>" + esc(label) + "</b>" +
        '<span class="owo-pe-mono">n=' + esc(st.n == null ? "—" : st.n) + "</span>" +
        '<span class="owo-pe-mono">成功率 ' + esc(st.rate == null ? "—" : (st.rate * 100).toFixed(1) + "%") + "</span>" +
        '<span class="owo-pe-mono">' + esc(ci) + "</span>" +
        '<span class="owo-pe-mono">p50 ' + esc(st.p50Ms == null ? "—" : fmtDur(st.p50Ms)) + "</span>" +
        '<span class="owo-pe-mono">p95 ' + esc(st.p95Ms == null ? "—" : fmtDur(st.p95Ms)) + "</span>" +
        (small ? '<span class="owo-pe-badge warn" title="n<30：置信区间宽，结论仅供观察">样本不足</span>' : "") +
        "</div>"
      );
    }

    function renderStatistics(stats) {
      if (!stats) return "";
      var html = '<div class="owo-pe-stats"><div class="owo-pe-stat-head">统计判读 <span class="hint">成功率 95% 置信区间 · p50/p95 耗时 · 启用阈值（成功率 +5% / p50 耗时 -30%）</span></div>';
      html += statEngineLine("单 Agent", stats.single);
      html += statEngineLine("WorkSwarm", stats.workswarm);
      if (stats.deltas) {
        var d = stats.deltas;
        function dPct(x) { return x == null ? "—" : (x > 0 ? "+" : "") + x.toFixed(0) + "%"; }
        function dPp(x) { return x == null ? "—" : (x > 0 ? "+" : "") + (x * 100).toFixed(1) + "pp"; }
        html +=
          '<div class="owo-pe-stat-row sub">' +
          "<b>差值（多 − 单）</b>" +
          '<span class="owo-pe-mono">成功率 ' + esc(dPp(d.rateDiff)) + "</span>" +
          '<span class="owo-pe-mono">p50 耗时 ' + esc(dPct(d.wallChangePct)) + "</span>" +
          '<span class="owo-pe-mono">模型调用 ' + esc(dPct(d.callsChangePct)) + "</span>" +
          '<span class="owo-pe-mono">Token ' + esc(dPct(d.tokensChangePct)) + "</span>" +
          '<span class="owo-pe-mono">费用 ' + esc(dPct(d.costChangePct)) + "</span>" +
          "</div>";
      }
      if (stats.recommendation) {
        html +=
          '<div class="owo-pe-stat-rec' + (String(stats.recommendation.verdict).indexOf("建议启用") === 0 ? " ok" : "") + '">' +
          "<b>启用建议：</b>" + esc(stats.recommendation.verdict) +
          '<span class="sub">（判定来源：' + (stats.recommendation.source === "server" ? "服务端统计" : "客户端推导") + "）</span>" +
          "</div>";
      }
      if (stats.sampleSmall) {
        html += '<div class="owo-pe-stat-note bad">样本不足：当前样本量 n<30，置信区间偏宽、差值与启用建议仅供观察，不构成上线依据。</div>';
      }
      html += "</div>";
      return html;
    }


    // 详情 → 逐 case 归一化行（单 vs 多对比）。聚合规则：
    //   状态 = 该 case×mode 全部单元格通过则为 passed，否则取最坏非通过状态；
    //   耗时 = 单元格均值；检查 = "通过/总数"；失败步骤/错误取首个失败单元格；
    //   Artifact refs = 各单元格并集去重（顺序保持）。
    function casesFromDetail(detail) {
      var report = detail && detail.report;
      var runs = (report && report.runs) || [];
      var byCase = {};
      var order = [];
      for (var i = 0; i < runs.length; i++) {
        var r = runs[i];
        var cid = (r.key && r.key.case_id) || "";
        if (!cid) continue;
        if (!byCase[cid]) { byCase[cid] = {}; order.push(cid); }
        var mode = normMode(r.key && r.key.agent_mode) === "workswarm" ? "workswarm" : "single";
        var bucket = byCase[cid][mode] || (byCase[cid][mode] = { cells: [] });
        bucket.cells.push(r);
      }
      var BAD_RANK = { error: 0, timeout: 1, failed: 2, cancelled: 3 };
      function worst(sts) {
        var worstS = "passed";
        for (var k = 0; k < sts.length; k++) {
          var s = String(sts[k]);
          if (s === "passed") continue;
          if (worstS === "passed") worstS = s;
          else if ((BAD_RANK[s] != null) && (BAD_RANK[s] < (BAD_RANK[worstS] != null ? BAD_RANK[worstS] : 99))) worstS = s;
        }
        return worstS;
      }
      function cellOf(bucket) {
        if (!bucket) return null;
        var cells = bucket.cells || [];
        if (!cells.length) return null;
        var passed = 0, wall = 0, failedStep = "", err = "";
        var refs = [];
        for (var k = 0; k < cells.length; k++) {
          var c = cells[k];
          if (String(c.status) === "passed") passed++;
          wall += Number(c.wall_ms) || 0;
          if (!failedStep && Array.isArray(c.failed_steps) && c.failed_steps.length) failedStep = c.failed_steps[0];
          if (!err && c.error) err = String(c.error);
          var crs = Array.isArray(c.artifact_refs) ? c.artifact_refs : [];
          for (var q = 0; q < crs.length; q++) if (refs.indexOf(crs[q]) < 0) refs.push(crs[q]);
        }
        return {
          status: passed === cells.length ? "passed" : worst(cells.map(function (c) { return c.status; })),
          duration_ms: wall / cells.length,
          checks: passed + "/" + cells.length + " 通过",
          failed_step: failedStep,
          error: err,
          artifact_refs: refs,
        };
      }
      var out = [];
      for (var c2 = 0; c2 < order.length; c2++) {
        var cid2 = order[c2];
        var firstCell = (byCase[cid2].single || byCase[cid2].workswarm || {}).cells ? (byCase[cid2].single || byCase[cid2].workswarm).cells[0] : null;
        out.push({
          case_id: cid2,
          category: (firstCell && firstCell.category) || "",
          single: cellOf(byCase[cid2].single),
          workswarm: cellOf(byCase[cid2].workswarm),
        });
      }
      return out;
    }

    // 汇总详情 → 面板视图模型（进度/双引擎指标/错误态）。
    // 进度键宽容：服务端为 progress.done，兼容 progress.completed。
    function computeSummary(detail) {
      var d = detail || {};
      var prog = d.progress || {};
      var report = d.report || null;
      var rawTotal = pickMetric(prog, ["total"]);
      var rawDone = pickMetric(prog, ["done", "completed"]);
      var total = rawTotal == null ? NaN : Number(rawTotal);
      var completed = rawDone == null ? NaN : Number(rawDone);
      var current = prog.current || "";
      if (!current && report && Array.isArray(report.pending) && report.pending.length && report.pending[0]) {
        current = report.pending[0].case_id || "";
      }
      var aggS = engineAgg(report, "single");
      var aggW = engineAgg(report, "workswarm");
      return {
        runId: d.run_id || state.runId || "—",
        status: normRunStatus(d.status),
        statusCn: STATUS_CN[normRunStatus(d.status)] || String(d.status || "—"),
        suite: d.suite || "—",
        execution: d.execution || "—",
        modes: Array.isArray(d.modes) ? d.modes : [],
        total: isFinite(total) ? total : null,
        completed: isFinite(completed) ? completed : null,
        pct: isFinite(total) && total > 0 && isFinite(completed) ? Math.min(100, Math.round((completed / total) * 100)) : null,
        current: current,
        stats: statisticsFromReport(report),
        singleRate: aggS ? fmtRate(aggS.rate) : "—",
        wsRate: aggW ? fmtRate(aggW.rate) : "—",
        singleDur: aggS ? fmtDur(aggS.meanWall) : "—",
        wsDur: aggW ? fmtDur(aggW.meanWall) : "—",
        singleCalls: aggS ? fmtInt(aggS.calls) : "—",
        wsCalls: aggW ? fmtInt(aggW.calls) : "—",
        singleTokens: aggS ? fmtInt(aggS.tokens) : "—",
        wsTokens: aggW ? fmtInt(aggW.tokens) : "—",
        singleCost: aggS ? fmtCost(aggS.cost) : "—",
        wsCost: aggW ? fmtCost(aggW.cost) : "—",
        error: d.error || "",
      };
    }

    // 友好错误文案：与 app.js 的状态码风格一致，面板自解释。
    function explainError(e) {
      var msg = String((e && e.message) || e || "");
      var m = msg.match(/^(\d{3}):/);
      var code = m ? Number(m[1]) : 0;
      if (code === 400 || code === 422) return "请求被拒绝（" + code + "）：请检查套件名 / 次数 / 过滤参数";
      if (code === 401 || code === 403) return "未认证或无权限（" + code + "）";
      if (code === 404) return "资源不存在（404）：运行不存在或已被清理";
      if (code === 409) return "状态冲突（409）：运行不处于可操作状态";
      if (code >= 500) return "服务接口不可用（HTTP " + code + "）";
      if (!code && msg) return "操作失败：" + msg;
      return "操作失败：未知错误";
    }

    // ---------- 面板内联样式（.owo-pe-* 作用域；防溢出关键规则在 style.css 第 17 节） ----------
    // 注意：必须留在 IIFE 内部 —— 顶层 var CSS 会覆盖浏览器内建 window.CSS（CSSOM 命名空间）。
    var CSS = "" +
      ".owo-pe-panel{display:flex;flex-direction:column;gap:10px;min-width:0;}" +
      ".owo-pe-root{display:flex;flex-direction:column;gap:10px;min-width:0;}" +
      ".owo-pe-title{font-weight:600;}" +
      ".owo-pe-card{border:1px solid var(--soft-border,#edf0f5);border-radius:11px;background:var(--surface,#fff);padding:10px 12px;display:flex;flex-direction:column;gap:8px;min-width:0;}" +
      ".owo-pe-cardhead{font-size:13px;font-weight:700;color:var(--text,#1d2433);}" +
      ".owo-pe-config{display:grid;grid-template-columns:repeat(auto-fit,minmax(170px,1fr));gap:8px;align-items:end;}" +
      ".owo-pe-field{display:flex;flex-direction:column;gap:3px;font-size:12px;color:var(--muted,#5b6577);min-width:0;}" +
      ".owo-pe-field input,.owo-pe-field select{max-width:100%;padding:6px 8px;font-size:12px;}" +
      ".owo-pe-wide{grid-column:1/-1;}" +
      ".owo-pe-check{display:inline-flex;align-items:center;gap:4px;font-size:12px;margin-right:8px;}" +
      ".owo-pe-actions{display:flex;gap:8px;flex-wrap:wrap;}" +
      ".owo-pe-actions button:disabled{opacity:.5;cursor:not-allowed;}" +
      ".owo-pe-note-bad{color:var(--red,#cd3f35);}" +
      ".owo-pe-sumline{display:flex;flex-wrap:wrap;align-items:center;gap:8px;font-size:12px;color:var(--muted,#5b6577);min-width:0;}" +
      ".owo-pe-sumline b{color:var(--text,#1d2433);}" +
      ".owo-pe-modes{color:var(--accent,#2563eb);}" +
      ".owo-pe-progress{height:6px;border-radius:4px;background:var(--raised,#f1f3f8);overflow:hidden;min-width:0;}" +
      ".owo-pe-progress-bar{height:100%;border-radius:4px;background:var(--accent,#2563eb);transition:width .3s ease;}" +
      ".owo-pe-progress-bar.bad{background:var(--red,#cd3f35);}" +
      ".owo-pe-engines{display:grid;grid-template-columns:repeat(auto-fit,minmax(230px,1fr));gap:8px;}" +
      ".owo-pe-engine{border:1px solid var(--soft-border,#edf0f5);border-radius:9px;background:var(--raised,#f1f3f8);padding:8px 10px;display:flex;flex-direction:column;gap:4px;min-width:0;}" +
      ".owo-pe-engine-head{display:flex;justify-content:space-between;align-items:center;gap:6px;font-size:12px;}" +
      ".owo-pe-rate{font-weight:700;font-variant-numeric:tabular-nums;}" +
      ".owo-pe-kv{display:flex;justify-content:space-between;gap:6px;font-size:11px;color:var(--muted,#5b6577);}" +
      ".owo-pe-kv b{color:var(--text,#1d2433);font-variant-numeric:tabular-nums;}" +
      ".owo-pe-runerror{border:1px solid var(--red-line,rgba(207,63,53,.32));background:var(--red-soft,rgba(207,63,53,.08));color:var(--red,#cd3f35);border-radius:8px;padding:6px 9px;font-size:12px;}" +
      ".owo-pe-badge{display:inline-block;padding:1px 8px;border-radius:8px;font-size:11px;color:#fff;background:var(--faint,#8a93a5);white-space:nowrap;}" +
      ".owo-pe-badge.ok{background:var(--green,#14855a);}" +
      ".owo-pe-badge.bad{background:var(--red,#cd3f35);}" +
      ".owo-pe-badge.run{background:var(--accent,#2563eb);}" +
      ".owo-pe-badge.warn{background:var(--yellow,#a06a04);}" +
      ".owo-pe-badge.off{background:var(--faint,#8a93a5);}" +
      ".owo-pe-runlist{display:flex;flex-direction:column;gap:2px;max-height:280px;overflow:auto;}" +
      ".owo-pe-runrow{display:flex;gap:8px;align-items:center;padding:6px 6px;border-radius:8px;cursor:pointer;font-size:12px;min-width:0;}" +
      ".owo-pe-runrow:hover{background:var(--raised,#f1f3f8);}" +
      ".owo-pe-runrow.sel{background:var(--accent-soft,rgba(37,99,235,.1));}" +
      ".owo-pe-runrow .owo-pe-mono{max-width:34%;}" +
      ".owo-pe-runrow .sub{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}" +
      ".owo-pe-mono{font-family:Consolas,ui-monospace,monospace;}" +
      ".owo-pe-ellip{display:inline-block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:100%;vertical-align:bottom;}" +
      ".owo-pe-empty{padding:8px 4px;}" +
      ".owo-pe-empty.bad{color:var(--red,#cd3f35);}" +
      ".owo-pe-tablewrap{overflow-x:auto;max-width:100%;border:1px solid var(--soft-border,#edf0f5);border-radius:10px;background:var(--surface,#fff);}" +
      ".owo-pe-table{border-collapse:collapse;width:100%;min-width:640px;font-size:12px;}" +
      ".owo-pe-table th,.owo-pe-table td{border-bottom:1px solid var(--soft-border,#edf0f5);padding:6px 8px;text-align:left;vertical-align:top;max-width:260px;}" +
      ".owo-pe-table th{color:var(--muted,#5b6577);font-weight:600;white-space:nowrap;}" +
      ".owo-pe-failedstep{color:var(--red,#cd3f35);}" +
      ".owo-pe-ref{display:inline-flex;align-items:center;gap:4px;margin:1px 6px 1px 0;max-width:100%;}" +
      ".owo-pe-ref code{font-size:11px;background:var(--raised,#f1f3f8);border-radius:5px;padding:1px 5px;max-width:150px;}" +
      ".owo-pe-copy{border:1px solid var(--soft-border,#edf0f5);background:transparent;border-radius:5px;font-size:10px;padding:1px 6px;cursor:pointer;color:var(--muted,#5b6577);}" +
      ".owo-pe-copy:hover{border-color:var(--accent,#2563eb);color:var(--accent,#2563eb);}" +
      ".owo-pe-errbox summary{cursor:pointer;color:var(--red,#cd3f35);font-size:11px;}" +
      ".owo-pe-errbox pre{margin:4px 0 0;background:var(--raised,#f1f3f8);padding:6px;font-size:11px;max-height:180px;overflow:auto;white-space:pre-wrap;word-break:break-all;max-width:260px;}" +
      "";

    // ---------- 渲染（纯字符串构建，mount 后 paint 到 DOM） ----------
    function statusBadge(s) {
      var st = normRunStatus(s);
      return '<span class="owo-pe-badge st-' + esc(st || "unknown") + " " + badgeClass(st) + '">' +
        esc(STATUS_CN[st] || s || "—") + "</span>";
    }

    function engineStatBlock(key, label, sum) {
      return (
        '<div class="owo-pe-engine">' +
        '<div class="owo-pe-engine-head"><b>' + esc(label) + "</b>" +
        '<span class="owo-pe-rate" data-pe-rate="' + esc(key) + '">' + esc(key === "single" ? sum.singleRate : sum.wsRate) + "</span></div>" +
        '<div class="owo-pe-kv"><span>平均耗时</span><b>' + esc(key === "single" ? sum.singleDur : sum.wsDur) + "</b></div>" +
        '<div class="owo-pe-kv"><span>模型调用</span><b>' + esc(key === "single" ? sum.singleCalls : sum.wsCalls) + "</b></div>" +
        '<div class="owo-pe-kv"><span>Token</span><b>' + esc(key === "single" ? sum.singleTokens : sum.wsTokens) + "</b></div>" +
        '<div class="owo-pe-kv"><span>费用</span><b>' + esc(key === "single" ? sum.singleCost : sum.wsCost) + "</b></div>" +
        "</div>"
      );
    }

    function summaryHtml(sum) {
      var prog = sum.pct == null ? "—" : sum.completed + "/" + sum.total + "（" + sum.pct + "%）";
      return (
        '<div class="owo-pe-sumline">' +
        statusBadge(sum.status) +
        '<span class="owo-pe-mono owo-pe-ellip" title="' + esc(sum.runId) + '">run: ' + esc(sum.runId) + "</span>" +
        "<span>" + esc(sum.suite) + " · " + esc(sum.execution === "live" ? "live" : "reference") + "</span>" +
        '<span class="owo-pe-modes">' + (sum.modes.length ? sum.modes.map(function (m) { return esc(ENGINE_CN[m] || m); }).join(" + ") : "—") + "</span>" +
        "</div>" +
        '<div class="owo-pe-progress" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="' + (sum.pct == null ? 0 : sum.pct) + '">' +
        '<div class="owo-pe-progress-bar' + (sum.status === "failed" ? " bad" : "") + '" style="width:' + (sum.pct == null ? 0 : sum.pct) + '%"></div>' +
        "</div>" +
        '<div class="owo-pe-sumline sub">进度 <b data-pe-progress>' + esc(prog) + "</b>" +
        (sum.current ? '<span class="owo-pe-ellip" title="' + esc(sum.current) + '">当前：' + esc(sum.current) + "</span>" : "") +
        "</div>" +
        (sum.error ? '<div class="owo-pe-runerror owo-pe-ellip" title="' + esc(sum.error) + '">' + esc(sum.error) + "</div>" : "") +
        '<div class="owo-pe-engines">' +
        engineStatBlock("single", "单 Agent", sum) +
        engineStatBlock("workswarm", "WorkSwarm", sum) +
        "</div>" +
        renderStatistics(sum.stats)
      );
    }

    // 单个 case 行：单 vs 多对比 + 检查 + 失败步骤 + Artifact 复制 + 可展开错误。
    function caseOutcome(cell) {
      if (!cell) return '<span class="sub">—</span>';
      var st = String(cell.status == null ? "" : cell.status);
      var cls = st === "passed" || st === "succeeded" || st === "completed" ? "ok" : st === "skipped" || st === "cancelled" ? "off" : "bad";
      return (
        '<span class="owo-pe-badge ' + cls + '">' + esc(st || "—") + "</span> " +
        '<span class="sub">' + esc(fmtDur(cell.duration_ms)) + "</span>"
      );
    }

    function caseRow(c) {
      var sc = c.single || null;
      var wc = c.workswarm || null;
      var err = (sc && sc.error) || (wc && wc.error) || "";
      var failedStep = (sc && sc.failed_step) || (wc && wc.failed_step) || "";
      var refs = []
        .concat((sc && sc.artifact_refs) || [], (wc && wc.artifact_refs) || [])
        .filter(function (x, i, a) { return x && a.indexOf(x) === i; });
      var refsHtml = refs.length
        ? refs.map(function (r) {
            return '<span class="owo-pe-ref"><code class="owo-pe-ellip" title="' + esc(r) + '">' + esc(r) + "</code>" +
              '<button type="button" class="owo-pe-copy" data-pe-copy="' + esc(r) + '" title="复制 Artifact ref">复制</button></span>';
          }).join("")
        : '<span class="sub">—</span>';
      return (
        "<tr>" +
        '<td><b class="owo-pe-ellip" title="' + esc(c.case_id) + '">' + esc(c.case_id) + "</b></td>" +
        "<td>" + esc(c.category || "—") + "</td>" +
        "<td>" + caseOutcome(sc) + "</td>" +
        "<td>" + caseOutcome(wc) + "</td>" +
        "<td>" + esc((sc && sc.checks) || (wc && wc.checks) || "—") + "</td>" +
        "<td>" + (failedStep ? '<span class="owo-pe-failedstep owo-pe-ellip" title="' + esc(failedStep) + '">' + esc(failedStep) + "</span>" : '<span class="sub">—</span>') + "</td>" +
        "<td>" + refsHtml + "</td>" +
        (err
          ? '<td><details class="owo-pe-errbox"><summary>错误</summary><pre>' + esc(err) + "</pre></details></td>"
          : "<td><span class='sub'>—</span></td>") +
        "</tr>"
      );
    }

    function renderCasesTable(cases) {
      if (!cases || !cases.length) {
        return '<div class="owo-pe-empty sub">暂无逐 case 结果（运行开始后在此按任务对比单 Agent 与 WorkSwarm）</div>';
      }
      return (
        '<div class="owo-pe-tablewrap"><table class="owo-pe-table">' +
        "<thead><tr><th>任务</th><th>类别</th><th>单 Agent</th><th>WorkSwarm</th><th>质量检查</th><th>失败步骤</th><th>Artifact refs</th><th>明细</th></tr></thead>" +
        "<tbody>" + cases.map(caseRow).join("") + "</tbody>" +
        "</table></div>"
      );
    }

    function listHtml(runs) {
      if (!runs || !runs.length) {
        return '<div class="owo-pe-empty sub">暂无评测运行（配置上方参数后点「启动评测」）</div>';
      }
      return runs.map(function (r) {
        var sel = state.runId && r.run_id === state.runId;
        return (
          '<div class="owo-pe-runrow' + (sel ? " sel" : "") + '" data-pe-run="' + esc(r.run_id) + '" role="button" tabindex="0">' +
          statusBadge(r.status) +
          '<span class="owo-pe-mono owo-pe-ellip" title="' + esc(r.run_id) + '">' + esc(r.run_id) + "</span>" +
          '<span class="sub">' + esc(r.suite || "—") + " · " + esc(r.execution || "—") + " · " +
          esc((r.modes || []).join("+") || "—") + "</span>" +
          "</div>"
        );
      }).join("");
    }

    function nav() {
      return (
        '<section data-panel="' + id + '" class="owo-pe-panel">' +
        "<style>" + CSS + "</style>" +
        '<div id="owo-pe-root" class="owo-pe-root">' +
        '<div class="sub owo-pe-title">产品评测中心（真实单 Agent / WorkSwarm · 异步运行）</div>' +
        // 运行配置区
        '<div class="owo-pe-card">' +
        '<div class="owo-pe-cardhead">运行配置</div>' +
        '<div class="owo-pe-config">' +
        '<label class="owo-pe-field"><span>套件</span><input id="owo-pe-suite" value="v1" placeholder="注册套件名（如 v1）"></label>' +
        '<label class="owo-pe-field"><span>执行方式</span><select id="owo-pe-execution">' +
        '<option value="reference">reference（离线基线）</option><option value="live">live（真实模型计费）</option></select></label>' +
        '<label class="owo-pe-field"><span>重复次数</span><input id="owo-pe-reps" type="number" min="1" max="50" step="1" value="1"></label>' +
        '<label class="owo-pe-field"><span>类别过滤</span><select id="owo-pe-category">' +
        '<option value="">全部</option><option value="code">代码</option><option value="research">研究</option><option value="document">文档</option></select></label>' +
        '<label class="owo-pe-field owo-pe-wide"><span>仅运行 case（可空）</span><input id="owo-pe-only" placeholder="case id，留空运行全部"></label>' +
        '<div class="owo-pe-field"><span>引擎</span>' +
        '<label class="owo-pe-check"><input type="checkbox" id="owo-pe-mode-single" checked> 单 Agent</label>' +
        '<label class="owo-pe-check"><input type="checkbox" id="owo-pe-mode-workswarm" checked> WorkSwarm</label></div>' +
        '<div class="owo-pe-actions">' +
        '<button class="primary" id="owo-pe-start">启动评测</button>' +
        '<button id="owo-pe-cancel" disabled>取消运行</button>' +
        "</div>" +
        "</div>" +
        '<div id="owo-pe-note" class="sub" aria-live="polite">—</div>' +
        "</div>" +
        // 运行进度区
        '<div class="owo-pe-card">' +
        '<div class="owo-pe-cardhead">运行进度</div>' +
        '<div id="owo-pe-summary"><div class="owo-pe-empty sub">尚未选择运行（启动评测或从历史列表选择）</div></div>' +
        "</div>" +
        // 结果对比区
        '<div class="owo-pe-card">' +
        '<div class="owo-pe-cardhead">逐 case 对比（单 Agent vs WorkSwarm）</div>' +
        '<div id="owo-pe-cases"><div class="owo-pe-empty sub">—</div></div>' +
        "</div>" +
        // 历史运行
        '<div class="owo-pe-card">' +
        '<div class="owo-pe-cardhead">历史运行</div>' +
        '<div id="owo-pe-list" class="owo-pe-runlist"><div class="owo-pe-empty sub">加载中…</div></div>' +
        "</div>" +
        "</div>" +
        "</section>"
      );
    }

    // ---------- IO 与行为 ----------
    function note(msg, cls) {
      state.note = msg;
      var el = rootEl && rootEl.querySelector ? rootEl.querySelector("#owo-pe-note") : null;
      if (el) {
        el.textContent = msg;
        el.className = "sub" + (cls ? " " + cls : "");
      }
    }

    function paintSummary() {
      var box = rootEl && rootEl.querySelector ? rootEl.querySelector("#owo-pe-summary") : null;
      if (!box) return;
      if (state.loadFailed && !state.detail) {
        box.innerHTML = '<div class="owo-pe-empty bad">' + esc(state.loadFailed) + "</div>";
        return;
      }
      if (!state.detail) {
        box.innerHTML = '<div class="owo-pe-empty sub">尚未选择运行（启动评测或从历史列表选择）</div>';
        return;
      }
      box.innerHTML = summaryHtml(computeSummary(state.detail));
    }

    function paintCases() {
      var box = rootEl && rootEl.querySelector ? rootEl.querySelector("#owo-pe-cases") : null;
      if (!box) return;
      box.innerHTML = renderCasesTable(casesFromDetail(state.detail));
    }

    function paintList() {
      var el = rootEl && rootEl.querySelector ? rootEl.querySelector("#owo-pe-list") : null;
      if (el) el.innerHTML = listHtml(state.runs);
    }

    function paintButtons() {
      var startBtn = rootEl && rootEl.querySelector ? rootEl.querySelector("#owo-pe-start") : null;
      var cancelBtn = rootEl && rootEl.querySelector ? rootEl.querySelector("#owo-pe-cancel") : null;
      if (startBtn) startBtn.disabled = !!state.submitting;
      if (cancelBtn) {
        var st = normRunStatus(state.detail && state.detail.status);
        cancelBtn.disabled = state.cancelling || !isCancellable(st);
      }
    }

    function paintAll() {
      paintSummary();
      paintCases();
      paintList();
      paintButtons();
    }

    // 面板是否仍在文档中：切走即停止轮询（注入点供 Node 测试）。
    var attachmentProbe = null;
    function panelAttached() {
      if (attachmentProbe) return !!attachmentProbe();
      return !!(rootEl && rootEl.querySelector && rootEl.querySelector("#owo-pe-root"));
    }

    function stopPolling(reason) {
      if (state.pollTimer) {
        clearInterval(state.pollTimer);
        state.pollTimer = null;
      }
      if (reason) note(reason, "");
    }

    function syncDetail() {
      var rid = state.runId;
      if (!rid) return Promise.resolve();
      var seq = ++state.seq;
      return H.get("/product-eval/runs/" + encodeURIComponent(rid))
        .then(function (d) {
          if (!panelAttached()) { stopPolling(); return; }
          if (state.runId !== rid || seq !== state.seq) return; // 已切换/晚到响应作废
          state.detail = d || null;
          state.loadFailed = "";
          paintSummary();
          paintCases();
          paintButtons();
          if (isTerminalRun(d && d.status)) stopPolling("运行已结束：" + STATUS_CN[normRunStatus(d.status)] + "（" + normRunStatus(d.status) + "）");
        })
        .catch(function (e) {
          if (state.runId !== rid || seq !== state.seq) return;
          // 运行中偶发网络抖动不抹掉已有视图，只在无详情时呈现 failed 态。
          if (!state.detail) state.loadFailed = explainError(e);
          note("刷新失败：" + explainError(e), "owo-pe-note-bad");
          paintSummary();
        });
    }

    // 单次轮询体：面板已切走（DOM 脱离文档）立即停止并放弃本次刷新。
    // 抽出为独立函数：startPolling 的 interval 与 Node 测试共用同一行为。
    function pollTick() {
      if (!panelAttached()) {
        stopPolling();
        return Promise.resolve();
      }
      return syncDetail();
    }

    function startPolling() {
      stopPolling();
      state.pollTimer = setInterval(pollTick, 2000);
      return syncDetail();
    }

    function selectRun(runId) {
      state.runId = runId;
      state.detail = null;
      state.loadFailed = "";
      state.seq++; // 旧运行在途响应作废
      paintList();
      paintSummary();
      paintCases();
      paintButtons();
      if (!runId) return Promise.resolve();
      return syncDetail().then(function () {
        var st = normRunStatus(state.detail && state.detail.status);
        if (!isTerminalRun(st)) startPolling();
        else stopPolling();
      });
    }

    function readConfigFromDom() {
      if (!rootEl || !rootEl.querySelector) return state.config;
      function val(sel) {
        var el = rootEl.querySelector(sel);
        return el ? el.value : "";
      }
      function checked(sel) {
        var el = rootEl.querySelector(sel);
        return !!(el && el.checked);
      }
      var modes = [];
      if (checked("#owo-pe-mode-single")) modes.push("single");
      if (checked("#owo-pe-mode-workswarm")) modes.push("workswarm");
      return {
        suite: val("#owo-pe-suite"),
        execution: val("#owo-pe-execution"),
        modes: modes,
        repetitions: val("#owo-pe-reps"),
        category: val("#owo-pe-category"),
        only: val("#owo-pe-only"),
      };
    }

    // 启动评测：同步提交锁保证连点/双击只发一次请求。
    function handleStartClick() {
      if (state.submitting) return Promise.resolve();
      state.submitting = true;
      state.config = readConfigFromDom();
      paintButtons();
      var body = buildRunBody(state.config);
      return H.post("/product-eval/runs", body)
        .then(function (resp) {
          var rid = resp && (resp.run_id || resp.runId);
          if (!rid) throw new Error("服务端响应缺少 run_id");
          note("已创建运行 " + rid + "（queued），正在轮询进度…");
          return selectRun(rid);
        })
        .catch(function (e) {
          note(explainError(e), "owo-pe-note-bad");
        })
        .then(function () {
          state.submitting = false;
          paintButtons();
        });
    }

    // 取消运行：服务端幂等；UI 在下一轮询周期内呈现 cancelled。
    function handleCancelClick() {
      if (state.cancelling) return Promise.resolve();
      var st = normRunStatus(state.detail && state.detail.status);
      if (!isCancellable(st)) return Promise.resolve();
      state.cancelling = true;
      paintButtons();
      return H.post("/product-eval/runs/" + encodeURIComponent(state.runId) + "/cancel", {})
        .then(function () {
          note("已请求取消，等待服务端确认…");
          return syncDetail();
        })
        .catch(function (e) {
          note(explainError(e), "owo-pe-note-bad");
        })
        .then(function () {
          state.cancelling = false;
          paintButtons();
        });
    }

    // Artifact ref 复制：优先 clipboard API，回退隐藏 textarea。
    function copyText(text) {
      if (navigator && navigator.clipboard && navigator.clipboard.writeText) {
        return navigator.clipboard.writeText(text);
      }
      return new Promise(function (resolve, reject) {
        try {
          var ta = document.createElement("textarea");
          ta.value = text;
          ta.style.position = "fixed";
          ta.style.opacity = "0";
          document.body.appendChild(ta);
          ta.select();
          var ok = document.execCommand("copy");
          document.body.removeChild(ta);
          if (ok) resolve(); else reject(new Error("execCommand copy 失败"));
        } catch (e) { reject(e); }
      });
    }

    function handleCopyClick(btn) {
      var ref = btn && btn.getAttribute ? btn.getAttribute("data-pe-copy") : "";
      if (!ref) return;
      copyText(ref).then(function () {
        var old = btn.textContent;
        btn.textContent = "已复制";
        btn.disabled = true;
        setTimeout(function () {
          btn.textContent = old;
          btn.disabled = false;
        }, 1200);
      }).catch(function () {
        note("复制失败：请手动选择 " + ref, "owo-pe-note-bad");
      });
    }

    function loadHistory() {
      return H.get("/product-eval/runs")
        .then(function (data) {
          if (!panelAttached()) return;
          var runs = Array.isArray(data) ? data : (data && data.runs) || [];
          state.runs = runs;
          paintList();
        })
        .catch(function (e) {
          if (!panelAttached()) return;
          state.runs = [];
          paintList();
          note("历史加载失败：" + explainError(e), "owo-pe-note-bad");
        });
    }

    function bind(root) {
      var section = root.querySelector ? root.querySelector("section.owo-pe-panel") : null;
      var on = function (sel, ev, fn) {
        var el = root.querySelector(sel);
        if (el) el.addEventListener(ev, fn);
      };
      on("#owo-pe-start", "click", function () { handleStartClick(); });
      on("#owo-pe-cancel", "click", function () { handleCancelClick(); });
      // 事件委托：历史行点击 / Artifact 复制按钮（innerHTML 重建不丢监听）。
      if (section) {
        section.addEventListener("click", function (ev) {
          var copyBtn = ev.target && ev.target.closest ? ev.target.closest("[data-pe-copy]") : null;
          if (copyBtn) { handleCopyClick(copyBtn); return; }
          var row = ev.target && ev.target.closest ? ev.target.closest("[data-pe-run]") : null;
          if (row) selectRun(row.getAttribute("data-pe-run"));
        });
        section.addEventListener("keydown", function (ev) {
          if (ev.key !== "Enter" && ev.key !== " ") return;
          var row = ev.target && ev.target.closest ? ev.target.closest("[data-pe-run]") : null;
          if (row) { ev.preventDefault(); selectRun(row.getAttribute("data-pe-run")); }
        });
      }
    }

    // ---------- 生命周期 ----------
    function mount(root, helpers) {
      rootEl = root;
      H = helpers || {};
      H.baseUrl = H.baseUrl || (win.OwoPanels && win.OwoPanels.baseUrl) || "";
      H.get = H.get || defaultGet;
      H.post = H.post || defaultPost;
      H.esc = H.esc || defaultEsc;
      stopPolling(); // 重挂载：旧轮询立即停（面板切换即停的另一重保险）
      state.seq++; // 旧在途响应作废
      state.submitting = false;
      state.cancelling = false;
      state.runId = null;
      state.detail = null;
      state.loadFailed = "";
      state.runs = [];
      root.innerHTML = nav();
      bind(root);
      note("选择或启动一次评测运行。live 模式调用真实模型并计费（凭据取自服务端环境变量）。");
      loadHistory();
    }

    function refresh() {
      if (!rootEl) return;
      paintAll();
      loadHistory();
    }

    // —— 测试挂钩（tests/product-eval.panel.test.mjs 专用；浏览器运行时不读取） ——
    var TEST_API = {
      normRunStatus: normRunStatus,
      isTerminalRun: isTerminalRun,
      isCancellable: isCancellable,
      badgeClass: badgeClass,
      buildRunBody: buildRunBody,
      fmtRate: fmtRate,
      fmtDur: fmtDur,
      fmtInt: fmtInt,
      fmtCost: fmtCost,
      computeSummary: computeSummary,
      casesFromDetail: casesFromDetail,
      engineAgg: engineAgg,
      // —— 四期挂钩：统计判读 ——
      wilsonCI: wilsonCI,
      percentileOf: percentileOf,
      engineStats: engineStats,
      serverStats: serverStats,
      statisticsFromReport: statisticsFromReport,
      renderStatistics: renderStatistics,
      normMode: normMode,
      summaryHtml: summaryHtml,
      renderCasesTable: renderCasesTable,
      caseRow: caseRow,
      listHtml: listHtml,
      explainError: explainError,
      STATUS_CN: STATUS_CN,
      copyText: copyText,
      handleStartClick: handleStartClick,
      handleCancelClick: handleCancelClick,
      handleCopyClick: handleCopyClick,
      syncDetail: syncDetail,
      pollTick: pollTick,
      startPolling: startPolling,
      stopPolling: stopPolling,
      selectRun: selectRun,
      loadHistory: loadHistory,
      css: function () { return CSS; },
      state: state,
      setAttachmentProbe: function (fn) { attachmentProbe = fn; },
      getTransport: function () { return { get: H.get, post: H.post }; },
      setTransport: function (t) {
        if (t && t.get) H.get = t.get;
        if (t && t.post) H.post = t.post;
      },
    };

    return {
      id: id,
      title: "产品评测中心",
      nav: nav,
      mount: mount,
      refresh: refresh,
      _test: TEST_API,
    };
  })();

  // Node（CommonJS）测试环境导出；浏览器无 module 定义，此分支不生效、零运行时差异。
  if (typeof module !== "undefined" && module.exports) {
    var __peWin = typeof window !== "undefined" ? window : globalThis;
    module.exports = __peWin.OwoPanels.eval;
  }
})();

