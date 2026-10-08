// R11:observability 面板质量收尾完成。
// R12:observability 面板复核完成（SLO/用量/告警/周报/遥测展示，无需改动）。
/* R5 Agent 3 面板：可观测性（概览卡/回合耗时折线/工具排行/健康清单）。
 * R8 增：SLO 基线 + 用量与预算；R9 增：告警规则/最近告警 + 周期报告入口；
 * R10 增：可选遥测状态展示（默认关，仅聚合指标）。
 * 纯脚本 IIFE，注册 window.OwoPanels.observability；helpers 防御性降级。
 * 折线用内联 SVG 绘制（无外部依赖）。
 */
window.OwoPanels = window.OwoPanels || {};
window.OwoPanels.observability = (function () {
  "use strict";

  var id = "observability";
  var panelGeneration = 0;
  var refreshGeneration = 0;
  var reportGeneration = 0;
  var sectionEl = null;

  function defaultHelpers() {
    var baseUrl = (window.OwoPanels && window.OwoPanels.baseUrl) || window.location.origin;
    function get(path) {
      return window.OwoApi.stream(path).then(function (r) {
        if (!r.ok) return r.json().catch(function () { return {}; }).then(function (body) {
          throw new Error((body && (body.message || body.error)) || "HTTP " + r.status);
        });
        return r.status === 204 ? null : r.json();
      });
    }
    function esc(s) {
      return String(s == null ? "" : s)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    }
    function friendlyError(e) {
      return "操作失败：" + (e && e.message ? e.message : String(e));
    }
    return { baseUrl: baseUrl, get: get, esc: esc, friendlyError: friendlyError };
  }

  var H = defaultHelpers();
  var state = { overview: null, turns: [], tools: [], health: null, runtime: null, slo: null, usage: null, alerts: null, report: null, telemetry: null };

  function nav() {
    return (
      '<section data-panel="' + id + '">' +
      '<style>' +
      '.owo-mtr-row{display:flex;gap:8px;align-items:center;padding:4px 0;border-bottom:1px solid var(--border)}' +
      // KPI 卡片：自适应网格（此前 8 张 inline-block 挤成一行，数字与标签贴在一起）
      '#owo-mtr-cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(116px,1fr));gap:8px}' +
      '.owo-mtr-card{display:flex;flex-direction:column;gap:2px;justify-content:center;padding:9px 12px;background:var(--surface-2);border:1px solid var(--border);border-radius:var(--r-md);text-align:left}' +
      '.owo-mtr-card b{font-size:19px;font-variant-numeric:tabular-nums;line-height:1.1;color:var(--text);overflow-wrap:anywhere}' +
      '.owo-mtr-card span{font-size:11px;color:var(--text-2)}' +
      '.owo-mtr-table{width:100%;border-collapse:collapse;font-size:12px}' +
      '.owo-mtr-table td,.owo-mtr-table th{border:0;border-bottom:1px solid var(--border);padding:5px 9px;text-align:left}' +
      '.owo-mtr-table th{background:var(--surface-2);color:var(--text-2);font-weight:650;white-space:nowrap}' +
      '.owo-mtr-table tbody tr:nth-child(even){background:var(--surface-2)}' +
      '.owo-mtr-table tbody tr:hover{background:var(--accent-soft)}' +
      '.owo-mtr-table tbody tr:last-child td{border-bottom:0}' +
      '.owo-mtr-table td:first-child{width:38%;color:var(--text-2)}' +
      '.owo-mtr-health b.ok{color:var(--green)}.owo-mtr-health b.bad{color:var(--red)}' +
      '</style>' +
      '<div class="stack">' +
      '<div class="sub">可观测性 / 性能护栏</div>' +
      '<div class="owo-mtr-row"><button class="primary" id="owo-mtr-refresh">刷新</button>' +
      '<span id="owo-mtr-updated" class="sub">—</span></div>' +
      '<div id="owo-mtr-cards"></div>' +
      '<div class="sub">运行时韧性指标（Wave 1/2）</div>' +
      '<div id="owo-mtr-runtime" class="owo-mtr-runtime">—</div>' +
      '<div class="sub">SLO 基线（Wave 2）</div>' +
      '<div id="owo-mtr-slo" class="owo-mtr-slo">—</div>' +
      '<div class="sub">用量与预算（R8）</div>' +
      '<div id="owo-mtr-usage" class="owo-mtr-usage">—</div>' +
      '<div class="sub">SLO 告警（R9）</div>' +
      '<div id="owo-mtr-alerts" class="owo-mtr-alerts">—</div>' +
      '<div class="sub">周期报告（R9，7 天窗口）</div>' +
      '<div id="owo-mtr-report" class="owo-mtr-report"><button class="primary" id="owo-mtr-report-refresh">加载周报</button></div>' +
      '<div class="sub">可选遥测（R10，默认关，仅聚合指标）</div>' +
      '<div id="owo-mtr-telemetry" class="owo-mtr-telemetry">—</div>' +
      '<div class="sub">回合耗时（最近 50 次，ms）</div>' +
      '<div id="owo-mtr-chart">—</div>' +
      '<div class="sub">工具调用排行</div>' +
      '<table class="owo-mtr-table" id="owo-mtr-tools"><thead><tr><th>工具</th><th>调用</th><th>失败</th><th>失败率</th></tr></thead><tbody></tbody></table>' +
      '<div class="sub">组件健康</div>' +
      '<div id="owo-mtr-health" class="owo-mtr-health">—</div>' +
      '</div>' +
      '</section>'
    );
  }

  function mount(root, helpers) {
    dispose();
    H = helpers || defaultHelpers();
    root.innerHTML = nav();
    sectionEl = root.querySelector('[data-panel="observability"]');
    root.querySelector("#owo-mtr-refresh").addEventListener("click", refresh);
    var reportBtn = root.querySelector("#owo-mtr-report-refresh");
    if (reportBtn) reportBtn.addEventListener("click", loadReport);
    refresh();
  }

  function showMetricError(selector, error) {
    var target = sectionEl && sectionEl.querySelector(selector);
    if (!target) return;
    var message = "加载失败：" + H.friendlyError(error);
    if (selector === "#owo-mtr-tools tbody") {
      target.innerHTML = '<tr><td colspan="4" style="color:var(--red)">' + H.esc(message) + "</td></tr>";
    } else {
      target.textContent = message;
      target.style.color = "var(--red)";
    }
  }

  function refresh() {
    var request = ++refreshGeneration;
    var owner = panelGeneration;
    var refreshButton = sectionEl && sectionEl.querySelector("#owo-mtr-refresh");
    if (refreshButton) {
      refreshButton.disabled = true;
      refreshButton.setAttribute("aria-busy", "true");
      refreshButton.textContent = "刷新中…";
    }
    var endpoints = [
      ["/metrics/overview", function (data) { state.overview = data; renderCards(); }, "#owo-mtr-cards"],
      ["/metrics/turns?limit=50", function (data) { state.turns = (data && data.turns) || []; renderChart(); }, "#owo-mtr-chart"],
      ["/metrics/tools", function (data) { state.tools = (data && data.tools) || []; renderTools(); }, "#owo-mtr-tools tbody"],
      ["/metrics/health", function (data) { state.health = data; renderHealth(); }, "#owo-mtr-health"],
      ["/metrics/runtime", function (data) { state.runtime = data; renderRuntime(); }, "#owo-mtr-runtime"],
      ["/metrics/slo", function (data) { state.slo = data; renderSlo(); }, "#owo-mtr-slo"],
      ["/usage/summary", function (data) { state.usage = data; renderUsage(); }, "#owo-mtr-usage"],
      ["/metrics/slo/alerts", function (data) { state.alerts = data; renderAlerts(); }, "#owo-mtr-alerts"],
      ["/metrics/telemetry/status", function (data) { state.telemetry = data; renderTelemetry(); }, "#owo-mtr-telemetry"],
    ];
    return Promise.all(endpoints.map(function (endpoint) {
      return H.get(endpoint[0])
        .then(function (data) {
          if (request !== refreshGeneration || owner !== panelGeneration) return;
          endpoint[1](data || {});
        })
        .catch(function (error) {
          if (request !== refreshGeneration || owner !== panelGeneration) return;
          showMetricError(endpoint[2], error);
        });
    })).finally(function () {
      if (request !== refreshGeneration || owner !== panelGeneration) return;
      var button = sectionEl && sectionEl.querySelector("#owo-mtr-refresh");
      if (button) {
        button.disabled = false;
        button.removeAttribute("aria-busy");
        button.textContent = "刷新";
      }
    });
  }

  function loadReport() {
    var request = ++reportGeneration;
    var owner = panelGeneration;
    var reportEl = sectionEl && sectionEl.querySelector("#owo-mtr-report");
    if (reportEl) reportEl.textContent = "正在加载周报…";
    return H.get("/metrics/slo/report?days=7")
      .then(function (data) {
        if (request !== reportGeneration || owner !== panelGeneration) return;
        state.report = data || {};
        renderReport();
      })
      .catch(function (error) {
        if (request !== reportGeneration || owner !== panelGeneration) return;
        var currentReport = sectionEl && sectionEl.querySelector("#owo-mtr-report");
        if (currentReport) currentReport.innerHTML = '<span style="color:var(--red)">' + H.esc(H.friendlyError(error)) + "</span>";
      });
  }

  function renderCards() {
    var el = document.getElementById("owo-mtr-cards");
    if (!el || !state.overview) return;
    var o = state.overview;
    el.innerHTML =
      card(o.traces_count, "traces") +
      card(o.avg_turn_ms, "均耗时 ms") +
      card(o.p50_ms, "p50 ms") +
      card(o.p95_ms, "p95 ms") +
      card(o.tool_calls_total, "工具调用") +
      card(o.approvals_total, "审批") +
      card(o.denied, "拒绝") +
      card(o.failures, "失败");
    var updated = document.getElementById("owo-mtr-updated");
    if (updated) updated.textContent = "更新于 " + H.esc((o.updated_at || "").slice(0, 19).replace("T", " "));
  }

  function card(value, label) {
    return '<div class="owo-mtr-card"><b>' + (value == null ? "—" : H.esc(String(value))) + "</b><span>" + H.esc(label) + "</span></div>";
  }

  function renderChart() {
    var el = document.getElementById("owo-mtr-chart");
    if (!el) return;
    var data = state.turns;
    if (!data.length) {
      el.textContent = "暂无回合数据";
      return;
    }
    var values = data.map(function (t) { return t.duration_ms; }).slice(0, 50).reverse();
    var width = 560;
    var height = 120;
    var max = Math.max.apply(null, values.concat([1]));
    var points = values
      .map(function (v, i) {
        var x = (i / Math.max(1, values.length - 1)) * (width - 8) + 4;
        var y = height - 8 - (v / max) * (height - 20);
        return x.toFixed(1) + "," + y.toFixed(1);
      })
      .join(" ");
    el.innerHTML =
      '<svg width="100%" viewBox="0 0 ' + width + " " + height + '" style="max-width:560px">' +
      '<polyline points="' + points + '" fill="none" style="stroke:var(--green)" stroke-width="1.5"></polyline>' +
      "<text x=\"4\" y=\"14\" font-size=\"10\" style=\"fill:var(--text-3)\">峰值 " + max + " ms（最近 " + values.length + " 次）</text>" +
      "</svg>";
  }

  function renderTools() {
    var el = document.querySelector("#owo-mtr-tools tbody");
    if (!el) return;
    if (!state.tools.length) {
      el.innerHTML = '<tr><td colspan="4" class="sub">暂无工具调用</td></tr>';
      return;
    }
    el.innerHTML = state.tools
      .map(function (t) {
        return (
          "<tr><td>" + H.esc(t.tool) + "</td><td>" + t.calls + "</td><td>" + t.failures +
          '</td><td>' + (t.failure_rate * 100).toFixed(1) + "%</td></tr>"
        );
      })
      .join("");
  }

  function renderHealth() {
    var el = document.getElementById("owo-mtr-health");
    if (!el || !state.health) return;
    var c = state.health.components || {};
    var stt = c.stt && c.stt.ready;
    el.innerHTML =
      "STT：<b class=\"" + (stt ? "ok" : "bad") + "\">" + (stt ? "就绪" : "未就绪") + "</b> ｜ " +
      "云端传输：<b class=\"ok\">" + H.esc((c.cloud_transport && c.cloud_transport.kind) || "?") + "</b> ｜ " +
      "插件：" + ((c.plugins && c.plugins.count) || 0) + " ｜ " +
      "笔记：" + ((c.notes && c.notes.count) || 0) + " ｜ " +
      "traces：" + ((c.traces && c.traces.count) || 0);
  }

  function renderRuntime() {
    var el = document.getElementById("owo-mtr-runtime");
    if (!el || !state.runtime) return;
    var r = state.runtime;
    var tool = r.tool || {};
    var approval = r.approval || {};
    var sse = r.sse || {};
    var events = r.events || {};
    var pct = function (v) {
      return v == null ? "—" : (v * 100).toFixed(1) + "%";
    };
    var ms = function (v) {
      return v == null ? "—" : H.esc(String(v)) + " ms";
    };
    el.innerHTML =
      '<table class="owo-mtr-table">' +
      "<tr><td>工具调度 p95 / p50</td><td>" + ms(tool.p95_ms) + " / " + ms(tool.p50_ms) + "（样本 " + (tool.samples || 0) + "）</td></tr>" +
      "<tr><td>审批通过率</td><td>" + pct(approval.pass_rate) + "</td></tr>" +
      "<tr><td>审批拦截率</td><td>" + pct(approval.intercept_rate) + "（通过 " + (approval.approved || 0) + " / 拦截 " + (approval.denied || 0) + " / 共 " + (approval.total || 0) + "）</td></tr>" +
      "<tr><td>事件队列深度</td><td>" + (r.queue_depth == null ? 0 : r.queue_depth) + "</td></tr>" +
      "<tr><td>SSE 活跃连接</td><td>" + (sse.active_connections || 0) + "（累计 " + (sse.total_connections || 0) + "，慢消费者断开 " + (sse.lagged_total || 0) + "）</td></tr>" +
      "<tr><td>事件流发布/丢弃</td><td>" + (events.published || 0) + " / " + (events.dropped || 0) + "</td></tr>" +
      "</table>";
  }

  function renderSlo() {
    var el = document.getElementById("owo-mtr-slo");
    if (!el || !state.slo) return;
    var items = (state.slo.slo || []).slice().sort(function (a, b) {
      return (a.name || "").localeCompare(b.name || "");
    });
    if (!items.length) {
      el.innerHTML = '<span class="sub">暂无 SLO 数据（服务端未注册报告探针）</span>';
      return;
    }
    var rows = items
      .map(function (item) {
        var budget = item.error_budget || {};
        var target = item.target_ms == null ? (item.success_floor == null ? "—" : (item.success_floor * 100).toFixed(1) + "%") : item.target_ms + " ms";
        var p95 = item.p95_ms == null ? "—" : item.p95_ms + " ms";
        var rate = item.success_rate == null ? "—" : (item.success_rate * 100).toFixed(2) + "%";
        // 样本为 0 时不能报"达标"：没有观测数据就无达标可言，显示灰色"样本不足"避免误判。
        var status = (item.samples || 0) === 0
          ? '<b style="color:var(--text-3)">样本不足</b>'
          : item.achieving
            ? '<b class="ok">达标</b>'
            : '<b class="bad">未达标</b>';
        return (
          "<tr><td>" + H.esc(item.name) + "</td><td>" + H.esc(target) +
          "</td><td>" + p95 + "</td><td>" + rate +
          "</td><td>" + (item.samples || 0) +
          "</td><td>" + (budget.bad || 0) + " / " + (budget.allowed_bad || 0) +
          "</td><td>" + status + "</td></tr>"
        );
      })
      .join("");
    el.innerHTML =
      '<table class="owo-mtr-table">' +
      "<thead><tr><th>SLO</th><th>目标</th><th>p95</th><th>成功率</th><th>样本</th><th>违规/预算</th><th>状态</th></tr></thead>" +
      "<tbody>" + rows + "</tbody></table>";
  }

  function renderUsage() {
    var el = document.getElementById("owo-mtr-usage");
    if (!el || !state.usage) return;
    var u = state.usage;
    if (u.error) {
      el.innerHTML = '<span class="sub">用量端点未就绪（主控接线后可用）</span>';
      return;
    }
    var dims = u.dimensions || [];
    var rows = dims
      .map(function (d) {
        var budget = d.budget ? "，预算 " + H.esc(String(d.budget.limit_usd)) + " USD" : "";
        var exceeded = d.budget && d.budget.exceeded ? ' <b class="bad">超限</b>' : "";
        return (
          "<tr><td>" + H.esc(d.dimension) + "</td><td>" + (d.calls || 0) +
          "</td><td>" + (d.total_tokens || 0) +
          "</td><td>" + H.esc(String(d.cost_usd)) + " USD" +
          "</td><td>" + H.esc(String(d.budget ? d.budget.spent_usd : 0)) + " / " +
          H.esc(String(d.budget ? d.budget.limit_usd : "—")) + budget + exceeded + "</td></tr>"
        );
      })
      .join("");
    var stop = u.hard_stop
      ? ' <b class="bad">硬熔断中</b>' + (u.hard_stop_reason ? "（" + H.esc(u.hard_stop_reason) + "）" : "")
      : "";
    el.innerHTML =
      "<div class=\"owo-mtr-row\">记录 " + (u.count || 0) + " 条，单价 " + H.esc(String(u.price_per_mtok)) + " $/Mtok" + stop + "</div>" +
      '<table class="owo-mtr-table">' +
      "<thead><tr><th>维度</th><th>调用</th><th>Token</th><th>成本</th><th>花费/预算</th></tr></thead>" +
      "<tbody>" + (rows || '<tr><td colspan="5" class="sub">暂无用量记录</td></tr>') + "</tbody></table>";
  }

  function renderAlerts() {
    var el = document.getElementById("owo-mtr-alerts");
    if (!el || !state.alerts) return;
    var data = state.alerts;
    if (data.note) {
      el.innerHTML = '<span class="sub">告警探针未注册（主控接线后可用）</span>';
      return;
    }
    var rules = data.rules || [];
    var ruleHtml = rules
      .map(function (r) {
        return "<tr><td>" + H.esc(r.name) + "</td><td>" + H.esc(r.slo_name) +
          "</td><td>" + H.esc(String(r.kind)) + " &gt; " + H.esc(String(r.threshold)) +
          "</td><td>连续 " + (r.consecutive || 0) + " 次</td><td>" +
          H.esc(r.severity || "") + "</td></tr>";
      })
      .join("");
    var alerts = (data.alerts || []).slice(0, 8);
    var alertHtml = alerts
      .map(function (a) {
        var color = a.kind === "recovered" ? "var(--green)" : a.severity === "critical" ? "var(--red)" : "var(--yellow)";
        return "<div class=\"owo-mtr-row\"><span style=\"color:" + color + "\">[" + H.esc(a.kind) +
          "] " + H.esc(a.rule) + "</span><span class=\"sub\">" +
          H.esc(String(a.at || "").slice(11, 19)) + "</span></div><div class=\"sub\">" +
          H.esc(a.detail || "") + "</div>";
      })
      .join("");
    el.innerHTML =
      '<table class="owo-mtr-table">' +
      "<thead><tr><th>规则</th><th>SLO</th><th>判定</th><th>连续</th><th>级别</th></tr></thead>" +
      "<tbody>" + (ruleHtml || '<tr><td colspan="5" class="sub">暂无规则</td></tr>') + "</tbody></table>" +
      '<div class="sub">最近告警（' + (data.count || 0) + '）</div>' +
      (alertHtml || '<div class="sub">暂无告警</div>');
  }

  function renderReport() {
    var el = sectionEl && sectionEl.querySelector("#owo-mtr-report");
    if (!el || !state.report) return;
    var data = state.report;
    if (data.note) {
      el.innerHTML = '<span class="sub">周期报告探针未注册（主控接线后可用）</span>';
      return;
    }
    var items = (data.slo || []).slice().sort(function (a, b) {
      return (a.name || "").localeCompare(b.name || "");
    });
    var rows = items
      .map(function (item) {
        var p95 = item.p95_ms == null ? "—" : item.p95_ms + " ms";
        var rate = item.success_rate == null ? "—" : (item.success_rate * 100).toFixed(2) + "%";
        // 样本为 0 时不能报"达标"：没有观测数据就无达标可言，显示灰色"样本不足"避免误判。
        var status = (item.samples || 0) === 0
          ? '<b style="color:var(--text-3)">样本不足</b>'
          : item.achieving
            ? '<b class="ok">达标</b>'
            : '<b class="bad">未达标</b>';
        return (
          "<tr><td>" + H.esc(item.name) + "</td><td>" + p95 +
          "</td><td>" + rate + "</td><td>" + (item.samples || 0) +
          "</td><td>" + (item.violations_in_window || 0) + "</td><td>" + status + "</td></tr>"
        );
      })
      .join("");
    el.innerHTML =
      '<div class="owo-mtr-row">周期 ' + (data.period_days || 7) + " 天，共 " + items.length + " 项</div>" +
      '<table class="owo-mtr-table">' +
      "<thead><tr><th>SLO</th><th>p95</th><th>成功率</th><th>样本</th><th>违规</th><th>状态</th></tr></thead>" +
      "<tbody>" + (rows || '<tr><td colspan="6" class="sub">暂无周期数据</td></tr>') + "</tbody></table>" +
      '<div class="sub"><button class="primary" id="owo-mtr-report-refresh">重新加载</button></div>';
    var btn = el.querySelector("#owo-mtr-report-refresh");
    if (btn) btn.addEventListener("click", loadReport);
  }

  function renderTelemetry() {
    var el = document.getElementById("owo-mtr-telemetry");
    if (!el || !state.telemetry) return;
    var t = state.telemetry;
    if (t.error) {
      el.innerHTML = '<span class="sub">遥测端点未就绪（主控接线后可用）</span>';
      return;
    }
    var enabled = !!t.enabled;
    var status = enabled
      ? '<b style="color:var(--yellow)">开（仅聚合指标，不含内容）</b>'
      : '<b class="ok">关（默认）</b>';
    var counters = t.counters || {};
    var codes = t.error_codes || {};
    var perf = t.performance || {};
    var counterSummary = Object.keys(counters)
      .map(function (k) { return H.esc(k) + "=" + counters[k]; })
      .join("，") || "无";
    var codeSummary = Object.keys(codes)
      .map(function (k) { return H.esc(k) + "×" + codes[k]; })
      .join("，") || "无";
    var dict = t.data_dictionary || {};
    el.innerHTML =
      '<table class="owo-mtr-table">' +
      "<tr><td>开关</td><td>" + status + "</td></tr>" +
      "<tr><td>功能计数</td><td>" + counterSummary + "</td></tr>" +
      "<tr><td>错误码分布</td><td>" + codeSummary + "</td></tr>" +
      "<tr><td>性能分位</td><td>工具 p50=" + (perf.tool_p50_ms == null ? "—" : H.esc(String(perf.tool_p50_ms)) + " ms") +
      "，p95=" + (perf.tool_p95_ms == null ? "—" : H.esc(String(perf.tool_p95_ms)) + " ms") + "</td></tr>" +
      "<tr><td>数据字典</td><td class=\"sub\">" + H.esc(t.note || "") +
      (dict.note ? "；" + H.esc(dict.note) : "") + "</td></tr>" +
      "</table>";
  }

  function dispose() {
    panelGeneration += 1;
    refreshGeneration += 1;
    reportGeneration += 1;
    sectionEl = null;
  }

  return {
    id: id,
    title: "可观测性",
    nav: nav,
    mount: mount,
    refresh: refresh,
    dispose: dispose,
    _test: { loadReport: loadReport },
  };
})();
