/* §4.6 诊断请求台账页（设置与诊断 → 诊断请求台账）。
 *
 * 数据面全部来自服务端权威事实，前端只做展示与聚合，不做二次计数：
 *   GET /diagnostics/requests?limit=200 → 环形窗口 ledger（六字段白名单）
 *   GET /metrics/overview               → SSE 当前连接数 / 累计 / lagged
 *   window.__owoCoreDiagnostics         → 壳侧 core 状态（IPC 快照，零 HTTP）
 *
 * §4.6 禁止回显清单（渲染与导出两处都受同一脱敏函数约束）：
 *   原始 URL query、Bearer、配对秘密、用户输入全文、未经处理的本地绝对路径。
 * ledger 本身只有六字段，因此这里的脱敏重点是**壳快照里可能出现的本地路径**
 * （logPath 等）与 instanceId（只保留前缀，用于跨日志对齐）。
 *
 * 网络请求只经 core/api-client.js（global.OwoApi）——本模块不自建 fetch。
 */
(function (global) {
  "use strict";

  const LEDGER_PATH = "/diagnostics/requests?limit=200";
  const OVERVIEW_PATH = "/metrics/overview";
  const SLOW_TOP_N = 8;
  const MIN_REFRESH_MS = 2000;
  const EXPORT_SCHEMA = "owo-diagnostic-bundle/1";

  function esc(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  // ---- §4.6 脱敏：本地绝对路径 / query / Bearer 一律不得出现在页面与导出包 ----
  const ABSOLUTE_PATH = /(^|[^\w])([A-Za-z]:[\\/]|[\\/]{2}|\/(?:Users|home|var|tmp|private)\/)/;

  function maskLocalPath(value) {
    const text = String(value == null ? "" : value);
    if (!text) return "";
    if (ABSOLUTE_PATH.test(text) || /^[A-Za-z]:[\\/]/.test(text) || text.startsWith("\\\\")) {
      return "<本地路径已脱敏>";
    }
    return text;
  }

  function stripQuery(value) {
    return String(value == null ? "" : value).split("?")[0].split("#")[0];
  }

  function maskSecretValue(value) {
    const text = String(value == null ? "" : value);
    if (!text) return "";
    if (/(?:Bearer|Basic|token=|api[_-]?key)[\s:=]/i.test(text)) return "<含授权信息，已脱敏>";
    if (/eyJ[A-Za-z0-9_-]{8,}/.test(text)) return "<疑似令牌已脱敏>";
    return maskLocalPath(text);
  }

  function instancePrefix(value) {
    const text = String(value == null ? "" : value);
    return text ? text.slice(0, 8) + (text.length > 8 ? "…" : "") : "";
  }

  // ---- 纯统计函数（契约测试直接消费，不依赖 DOM）----
  function percentile(sortedAsc, p) {
    if (!sortedAsc || !sortedAsc.length) return null;
    const rank = Math.max(1, Math.ceil(p * sortedAsc.length));
    return sortedAsc[Math.min(sortedAsc.length, rank) - 1];
  }

  function isEventsRoute(template) {
    const route = stripQuery(template);
    return route === "/events" || route.endsWith("/events");
  }

  /**
   * 四类计数（§4.6：health / auth / events / business）。
   * 服务端 aggregates.business 含 SSE，为让四类互斥可核对，这里把 events 单列，
   * business 显示为「不含 SSE」，同时保留服务端原值口径（business_server）。
   */
  function bucketCounts(ledger) {
    const records = (ledger && Array.isArray(ledger.records) ? ledger.records : []).filter(Boolean);
    const agg = (ledger && ledger.aggregates) || {};
    let health = 0;
    let auth = 0;
    let events = 0;
    for (const rec of records) {
      const route = stripQuery(rec.route_template);
      if (route === "/health") health += 1;
      else if (route === "/auth/token") auth += 1;
      if (isEventsRoute(route)) events += 1;
    }
    const businessServer = Number(agg.business) || 0;
    return {
      health: Number(agg.health) || health,
      auth: Number(agg.auth_token ?? agg.auth) || auth,
      events: events,
      business: Math.max(0, businessServer - events),
      business_server: businessServer,
      total_records: records.length,
    };
  }

  /** 按 route_template 聚合：次数、P50/P95/最大耗时、错误数（status>=400）。 */
  function routeAggregate(records) {
    const byRoute = new Map();
    for (const rec of records || []) {
      if (!rec) continue;
      const key = stripQuery(rec.route_template) || "(未知路由)";
      let slot = byRoute.get(key);
      if (!slot) {
        slot = { route: key, count: 0, errors: 0, durations: [] };
        byRoute.set(key, slot);
      }
      slot.count += 1;
      if (Number(rec.status) >= 400) slot.errors += 1;
      const ms = Number(rec.duration_ms);
      if (Number.isFinite(ms)) slot.durations.push(ms);
    }
    const rows = [];
    for (const slot of byRoute.values()) {
      slot.durations.sort((a, b) => a - b);
      rows.push({
        route: slot.route,
        count: slot.count,
        errors: slot.errors,
        p50_ms: percentile(slot.durations, 0.5),
        p95_ms: percentile(slot.durations, 0.95),
        max_ms: slot.durations.length ? slot.durations[slot.durations.length - 1] : null,
      });
    }
    rows.sort((a, b) => (b.p95_ms == null ? -1 : b.p95_ms) - (a.p95_ms == null ? -1 : a.p95_ms));
    return rows;
  }

  /** 慢请求 Top N（按 duration_ms 降序，同值按时间新→旧）。 */
  function slowest(records, n) {
    const list = (records || []).filter(Boolean).slice();
    list.sort((a, b) => {
      const diff = Number(b.duration_ms || 0) - Number(a.duration_ms || 0);
      if (diff !== 0) return diff;
      return String(b.started_at || "").localeCompare(String(a.started_at || ""));
    });
    return list.slice(0, n == null ? SLOW_TOP_N : n);
  }

  /** 来源分布（web / shell / cli / other——服务端 sanitize_source 白名单值）。 */
  function sourceCounts(records) {
    const out = {};
    for (const rec of records || []) {
      if (!rec) continue;
      const src = String(rec.source || "other");
      out[src] = (out[src] || 0) + 1;
    }
    return out;
  }

  /** 最近一次 core 引导（= /auth/token 交换）：壳重启后恰好重新引导一次（§8.2）。 */
  function lastBootstrap(records) {
    let count = 0;
    let last = null;
    for (const rec of records || []) {
      if (!rec) continue;
      if (stripQuery(rec.route_template) === "/auth/token") {
        count += 1;
        if (!last || String(rec.started_at) > String(last)) last = rec.started_at;
      }
    }
    return { count: count, last_started_at: last };
  }

  function formatClock(rfc3339) {
    const raw = String(rfc3339 || "");
    if (!raw) return "—";
    const stamp = Date.parse(raw);
    if (Number.isNaN(stamp)) return raw.slice(11, 19) || raw;
    const d = new Date(stamp);
    const pad = (v) => String(v).padStart(2, "0");
    return pad(d.getHours()) + ":" + pad(d.getMinutes()) + ":" + pad(d.getSeconds());
  }

  function msText(value) {
    return value == null ? "—" : Number(value) + " ms";
  }

  // ---- 导出脱敏诊断包（§4.6：一键导出，内容不含任何被禁字段）----
  function buildExportBundle(data) {
    const ledger = (data && data.ledger) || {};
    const overview = (data && data.overview) || {};
    const core = (data && data.core) || {};
    const records = Array.isArray(ledger.records) ? ledger.records : [];
    const sse = overview.sse || {};
    const buckets = bucketCounts(ledger);
    const boot = lastBootstrap(records);
    return {
      schema: EXPORT_SCHEMA,
      generated_at: new Date().toISOString(),
      ledger: {
        total: Number(ledger.total) || 0,
        returned: Number(ledger.returned) || 0,
        cap: Number(ledger.cap) || 0,
        buckets: buckets,
        routes: routeAggregate(records),
        slowest: slowest(records, SLOW_TOP_N).map((rec) => ({
          method: rec.method,
          route_template: stripQuery(rec.route_template),
          started_at: rec.started_at,
          duration_ms: Number(rec.duration_ms) || 0,
          status: Number(rec.status) || 0,
          source: String(rec.source || "other"),
        })),
        sources: sourceCounts(records),
      },
      core: {
        state: String(core.state || "unknown"),
        error_code: core.errorCode ? String(core.errorCode) : null,
        pid: Number.isFinite(Number(core.pid)) ? Number(core.pid) : null,
        instance_prefix: instancePrefix(core.instanceId),
        // 壳侧 logPath 是本地绝对路径：只导出「是否可打开」，不导出路径本身。
        log_available: Boolean(core.logPath),
        // §4.6 重启口径：壳未上报时保持 null（不得用引导次数冒充重启次数，
        // 也不得把 null 归零——`Number(null) === 0` 是个真陷阱）。
        shell_generation: core.generation === null || core.generation === undefined || !Number.isFinite(Number(core.generation))
          ? null
          : Number(core.generation),
        shell_attempt: core.attempt === null || core.attempt === undefined || !Number.isFinite(Number(core.attempt))
          ? null
          : Number(core.attempt),
        bootstraps: { count: boot.count, last_started_at: boot.last_started_at },
      },
      sse: {
        server_active_connections: Number(sse.active_connections) || 0,
        server_total_connections: Number(sse.total_connections) || 0,
        server_lagged_total: Number(sse.lagged_total) || 0,
        client_state: String(core.clientStreamState || "unknown"),
        client_reconnects: Number.isFinite(Number(core.clientReconnectAttempts))
          ? Number(core.clientReconnectAttempts)
          : null,
      },
      redaction: {
        policy: "禁止回显：原始 URL query、授权头、配对秘密、用户输入全文、本地绝对路径",
        absolute_paths_removed: true,
        authorization_headers_removed: true,
      },
    };
  }

  function copyText(text) {
    const value = String(text || "");
    const settle = (ok) => Promise.resolve(Boolean(ok));
    const clipboard = global.navigator && global.navigator.clipboard;
    if (clipboard && typeof clipboard.writeText === "function") {
      // 宿主实现可能返回非 Promise（壳内某些注入桩），统一 Promise.resolve 兜住。
      return Promise.resolve()
        .then(() => clipboard.writeText(value))
        .then(() => settle(true), () => settle(false));
    }
    try {
      const area = global.document.createElement("textarea");
      area.value = value;
      area.setAttribute("aria-hidden", "true");
      area.style.position = "fixed";
      area.style.opacity = "0";
      global.document.body.appendChild(area);
      area.select();
      const ok = global.document.execCommand && global.document.execCommand("copy");
      area.remove();
      return settle(ok);
    } catch (error) {
      return settle(false);
    }
  }

  function downloadJson(bundle) {
    const text = JSON.stringify(bundle, null, 2);
    const url = global.URL.createObjectURL(new Blob([text], { type: "application/json" }));
    const link = global.document.createElement("a");
    link.href = url;
    link.download = "owo-diagnostics-" + new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-") + ".json";
    global.document.body.appendChild(link);
    link.click();
    link.remove();
    global.setTimeout(() => global.URL.revokeObjectURL(url), 4000);
    return text.length;
  }

  // ---- 渲染 ----
  function tableHead(cells) {
    return "<thead><tr>" + cells.map((c) => "<th>" + esc(c) + "</th>").join("") + "</tr></thead>";
  }

  function renderBuckets(buckets) {
    const card = (label, value, hint) =>
      '<div class="owo-ledger-card"><b>' +
      esc(value == null ? "—" : value) +
      "</b><span>" +
      esc(label) +
      "</span>" +
      (hint ? '<small class="sub">' + esc(hint) + "</small>" : "") +
      "</div>";
    return (
      '<div class="owo-ledger-cards">' +
      card("health 探测", buckets.health, "壳就绪轮询/诊断页，不计业务") +
      card("auth 引导", buckets.auth, "壳注入 token 时为 0，不等于未引导") +
      card("events（SSE）", buckets.events, "business 子集，单列") +
      card("业务请求", buckets.business, "不含 SSE") +
      "</div>"
    );
  }

  function renderSlowest(rows) {
    if (!rows.length) return '<div class="sub">窗口内暂无请求记录。</div>';
    const body = rows
      .map(
        (rec) =>
          "<tr><td>" +
          esc(formatClock(rec.started_at)) +
          "</td><td>" +
          esc(rec.method) +
          "</td><td><code>" +
          esc(stripQuery(rec.route_template)) +
          "</code></td><td>" +
          esc(msText(Number(rec.duration_ms) || 0)) +
          "</td><td>" +
          esc(Number(rec.status) || 0) +
          "</td><td>" +
          esc(String(rec.source || "other")) +
          "</td></tr>",
      )
      .join("");
    return (
      '<table class="owo-ledger-table">' +
      tableHead(["时间", "方法", "路由模板", "耗时", "状态", "来源"]) +
      "<tbody>" +
      body +
      "</tbody></table>"
    );
  }

  function renderRoutes(rows) {
    if (!rows.length) return '<div class="sub">窗口内暂无可聚合记录。</div>';
    const body = rows
      .slice(0, 20)
      .map(
        (row) =>
          "<tr><td><code>" +
          esc(row.route) +
          "</code></td><td>" +
          esc(row.count) +
          "</td><td>" +
          esc(msText(row.p50_ms)) +
          "</td><td>" +
          esc(msText(row.p95_ms)) +
          "</td><td>" +
          esc(msText(row.max_ms)) +
          "</td><td" +
          (row.errors ? ' style="color:#c62828"' : "") +
          ">" +
          esc(row.errors) +
          "</td></tr>",
      )
      .join("");
    return (
      '<table class="owo-ledger-table">' +
      tableHead(["路由模板", "次数", "P50", "P95", "最大", "≥400"]) +
      "<tbody>" +
      body +
      "</tbody></table>" +
      (rows.length > 20 ? '<div class="sub">另有 ' + (rows.length - 20) + " 条路由未展示（导出包含全部）。</div>" : "")
    );
  }

  function renderSources(sources) {
    const keys = Object.keys(sources || {});
    if (!keys.length) return '<div class="sub">窗口内暂无来源记录。</div>';
    const zh = { web: "web（界面）", shell: "shell（桌面壳）", cli: "cli", other: "other（异常/未标注）" };
    const body = keys
      .sort((a, b) => sources[b] - sources[a])
      .map((k) => "<tr><td>" + esc(zh[k] || k) + "</td><td>" + esc(sources[k]) + "</td></tr>")
      .join("");
    return (
      '<table class="owo-ledger-table">' +
      tableHead(["来源", "次数"]) +
      "<tbody>" +
      body +
      "</tbody></table>"
    );
  }

  function kvRow(label, value, danger) {
    return (
      '<div class="owo-ledger-kv"><span>' +
      esc(label) +
      "</span><strong" +
      (danger ? ' style="color:#c62828"' : "") +
      ">" +
      esc(value == null ? "—" : value) +
      "</strong></div>"
    );
  }

  function renderCoreFacts(core, boot) {
    const zh = {
      ready: "就绪",
      starting: "启动中",
      restarting: "重启中",
      failed: "失败",
      stopped: "已停止",
      no_workspace: "未选工作区",
      unknown: "未知",
    };
    const stateText = zh[String(core.state || "unknown")] || String(core.state || "unknown");
    const row = kvRow;
    // §4.6「最近一次 core 重启」的权威口径来自壳侧：generation = 手动重连/换目录
    // 换代次数，attempt = 当代崩溃自动重启次数。两者缺一时不得假装知道
    // （注意 `Number(null) === 0`，所以判定必须先把 null/undefined 排除掉）。
    const counted = (value) => value !== null && value !== undefined && Number.isFinite(Number(value));
    const hasGeneration = counted(core.generation);
    const hasAttempt = counted(core.attempt);
    const restartText = hasGeneration
      ? "第 " + Number(core.generation) + " 代" + (hasAttempt ? " · 当代自动重启 " + Number(core.attempt) + " 次" : "")
      : "壳未上报";
    const caveat = hasGeneration
      ? "口径：重启代际/当代自动重启次数取自壳侧权威快照（Tauri IPC，不经 HTTP）；「窗口内引导次数」只统计 /auth/token，桌面正式冷启动多为 0，不能当重启数读。"
      : "口径：壳未上报重启计数时，以 /auth/token 引导次数近似「最近一次 core 重启」。";
    return (
      '<div class="owo-ledger-kvlist">' +
      row("当前状态", stateText) +
      row("稳定错误码", core.errorCode ? maskSecretValue(core.errorCode) : "无", Boolean(core.errorCode)) +
      row("最近一次 core 重启", restartText) +
      row("core PID", Number.isFinite(Number(core.pid)) ? Number(core.pid) : "—") +
      row("实例前缀", core.instanceId ? instancePrefix(core.instanceId) : "—") +
      row("窗口内引导次数", boot.count) +
      row("最近一次引导", boot.last_started_at ? formatClock(boot.last_started_at) : "窗口内未见 /auth/token") +
      row("日志可打开", core.logPath ? "是（路径不回显）" : "否") +
      "</div>" +
      '<div class="sub">' +
      esc(caveat) +
      "</div>"
    );
  }

  function renderSse(overview, core) {
    const sse = (overview && overview.sse) || null;
    if (!sse) {
      return '<div class="sub">/metrics/overview 未就绪，暂无服务端 SSE 计数' +
        (core.clientStreamState ? "；本客户端事件流状态：" + esc(core.clientStreamState) : "") +
        "。</div>";
    }
    return (
      '<div class="owo-ledger-kvlist">' +
      kvRow("当前连接数", Number(sse.active_connections) || 0) +
      kvRow("累计连接数", Number(sse.total_connections) || 0) +
      kvRow("背压丢弃（lagged）", Number(sse.lagged_total) || 0) +
      "</div>"
    );
  }

  function render(root, data) {
    if (!root) return;
    const ledger = (data && data.ledger) || {};
    const overview = (data && data.overview) || {};
    const core = (data && data.core) || {};
    const records = Array.isArray(ledger.records) ? ledger.records : [];
    // ⚠ 骨架态不得播报假数字：真机验收抓到过「最近请求数量：0 条（环形容量 0）」
    // 在数据到达前闪现并被当成事实断言（§4.6 页面是给人下结论的地方，0 ≠ 未知）。
    const loading = Boolean(data && data.loading);
    const failed = Boolean(data && data.failed);
    const buckets = bucketCounts(ledger);
    const boot = lastBootstrap(records);
    const state = { data: data };
    root.innerHTML =
      '<style>' +
      ".owo-ledger-cards{display:flex;flex-wrap:wrap;gap:6px}" +
      ".owo-ledger-card{min-width:120px;padding:6px 10px;background:#f4f6f8;border-radius:6px;text-align:center}" +
      ".owo-ledger-card b{display:block;font-size:16px}" +
      ".owo-ledger-card span{font-size:11px;color:#666}" +
      ".owo-ledger-table{width:100%;border-collapse:collapse;font-size:12px}" +
      ".owo-ledger-table td,.owo-ledger-table th{border:1px solid #ddd;padding:3px 6px;text-align:left}" +
      ".owo-ledger-kvlist{display:grid;grid-template-columns:repeat(auto-fit,minmax(190px,1fr));gap:4px}" +
      ".owo-ledger-kv{display:flex;gap:6px;align-items:baseline;font-size:12px}" +
      ".owo-ledger-kv span{color:#666}" +
      "</style>" +
      '<div class="owo-ledger-actions">' +
      '<button type="button" id="owoLedgerRefresh">刷新台账</button>' +
      '<button type="button" id="owoLedgerExport">生成脱敏诊断包</button>' +
      '<button type="button" id="owoLedgerCopy">复制诊断包 JSON</button>' +
      '<button type="button" id="owoLedgerDownload">下载诊断包文件</button>' +
      '<span id="owoLedgerUpdated" class="sub">' +
      esc(data && data.updated_at ? "更新于 " + formatClock(data.updated_at) : "加载中…") +
      "</span></div>" +
      (failed
        ? '<div class="sub" style="color:#c62828">台账端点不可达：' +
          esc(data.errorText || "GET /diagnostics/requests 未返回数据") +
          "（core 可能未就绪）。下方为壳侧可得事实，服务端计数以恢复后刷新为准。</div>" +
          "<h4>core 重启与引导</h4>" +
          renderCoreFacts(core, boot) +
          "<h4>SSE 事件流</h4>" +
          renderSse(overview, core)
        : loading
        ? '<div class="sub">正在读取服务端 ledger 与运行指标…</div>'
        : '<div class="sub">最近请求数量：<b>' +
          esc(Number(ledger.returned) || 0) +
          "</b> 条（窗口累计 " +
          esc(Number(ledger.total) || 0) +
          "，环形容量 " +
          esc(Number(ledger.cap) || 0) +
          "）</div>" +
          renderBuckets(buckets) +
          '<h4>慢请求 Top ' +
          SLOW_TOP_N +
          "</h4>" +
          renderSlowest(slowest(records, SLOW_TOP_N)) +
          "<h4>按路由模板聚合（P50 / P95）</h4>" +
          renderRoutes(routeAggregate(records)) +
          "<h4>来源（x-owo-client）</h4>" +
          renderSources(sourceCounts(records)) +
          "<h4>core 重启与引导</h4>" +
          renderCoreFacts(core, boot) +
          "<h4>SSE 事件流</h4>" +
          renderSse(overview, core)) +
      '<pre id="owoLedgerBundle" class="json-fallback" hidden></pre>';

    root.__owoLedgerState = state;
    const refreshBtn = root.querySelector("#owoLedgerRefresh");
    if (refreshBtn) refreshBtn.addEventListener("click", () => load(root, { force: true }));
    const exportBtn = root.querySelector("#owoLedgerExport");
    if (exportBtn) {
      // 「生成」只在页面内展开脱敏包：一键落盘是写用户目录的副作用，拆成显式的
      // 「下载」按钮（验收脚本只点生成，不会把文件写进真实下载目录）。
      exportBtn.addEventListener("click", () => {
        const bundle = buildExportBundle(currentData(root));
        const text = JSON.stringify(bundle, null, 2);
        const pre = root.querySelector("#owoLedgerBundle");
        if (pre) {
          pre.hidden = false;
          pre.textContent = text;
        }
        root.__owoLedgerBundle = bundle;
        note(root, "已生成脱敏诊断包（" + text.length + " 字节，已在页面展开；可复制或下载）。");
      });
    }
    const downloadBtn = root.querySelector("#owoLedgerDownload");
    if (downloadBtn) {
      downloadBtn.addEventListener("click", () => {
        const bundle = buildExportBundle(currentData(root));
        try {
          const size = downloadJson(bundle);
          note(root, "诊断包文件已提交下载（" + size + " 字节）。");
        } catch (error) {
          note(root, "下载不可用：" + (error && error.message ? error.message : String(error)) + "（可在页面展开后复制）");
        }
      });
    }
    const copyBtn = root.querySelector("#owoLedgerCopy");
    if (copyBtn) {
      copyBtn.addEventListener("click", () => {
        const bundle = buildExportBundle(currentData(root));
        copyText(JSON.stringify(bundle, null, 2)).then((ok) =>
          note(root, ok ? "诊断包 JSON 已复制到剪贴板（已脱敏）。" : "剪贴板不可用，已在页面展开 JSON 供手动复制。"),
        );
      });
    }
  }

  function currentData(root) {
    return (root && root.__owoLedgerState && root.__owoLedgerState.data) || {};
  }

  function note(root, text) {
    const el = root.querySelector("#owoLedgerUpdated");
    if (el) el.textContent = text;
  }

  function readCoreSnapshot() {
    const diag = global.__owoCoreDiagnostics || {};
    let stream = null;
    try {
      stream = typeof global.owoInvalidatorState === "function" ? global.owoInvalidatorState() : null;
    } catch (error) {
      stream = null;
    }
    return {
      state: diag.state || (diag.connection && diag.connection.state) || "unknown",
      errorCode: diag.errorCode || (diag.connection && diag.connection.errorCode) || null,
      pid: diag.pid,
      instanceId: diag.instanceId,
      // §4.6：壳侧重启口径（旧壳/未上报时为 undefined，渲染层必须显式降级）。
      generation: diag.generation,
      attempt: diag.attempt,
      // 展示与导出都先过脱敏：绝不把本地绝对路径带进 DOM。
      logPath: maskLocalPath(diag.logPath),
      message: maskSecretValue(maskLocalPath(diag.message)),
      clientStreamState: stream && stream.state,
      clientReconnectAttempts: stream && stream.reconnectAttempts,
    };
  }

  function apiGet(path) {
    const api = global.OwoApi;
    if (!api || typeof api.get !== "function") return Promise.reject(new Error("API 客户端未就绪"));
    return Promise.resolve(api.get(path));
  }

  let lastLoadAt = 0;

  /** 拉取 ledger + overview 并渲染（首次进入设置路由时由 app.js 调用）。 */
  function load(root, options) {
    if (!root) return Promise.resolve(null);
    const force = Boolean(options && options.force);
    const now = Date.now();
    if (!force && now - lastLoadAt < MIN_REFRESH_MS) return Promise.resolve(null);
    lastLoadAt = now;
    render(root, { loading: true });
    return Promise.all([
      apiGet(LEDGER_PATH).catch(() => null),
      apiGet(OVERVIEW_PATH).catch(() => null),
    ]).then(
      (results) => {
        const data = {
          ledger: results[0] || { records: [], aggregates: {} },
          overview: results[1] || null,
          core: readCoreSnapshot(),
          updated_at: new Date().toISOString(),
        };
        if (!results[0]) {
          // 失败态必须**显式渲染**：留在「正在读取…」骨架上就是永久 loading（真机
          // 抓到过一次：轮询在骨架上误判为已加载，四类计数全 0 还被当成事实）。
          data.failed = true;
          data.errorText = "GET /diagnostics/requests 未返回数据（core 可能未就绪）";
          root.__owoLedgerState = { data: data };
          render(root, data);
          return data;
        }
        root.__owoLedgerState = { data: data };
        render(root, data);
        return data;
      },
      (error) => {
        const data = {
          failed: true,
          errorText: String((error && error.message) || error),
          ledger: { records: [], aggregates: {} },
          overview: null,
          core: readCoreSnapshot(),
        };
        root.__owoLedgerState = { data: data };
        render(root, data);
        return null;
      },
    );
  }

  global.OwoDiagnosticsLedger = {
    LEDGER_PATH: LEDGER_PATH,
    OVERVIEW_PATH: OVERVIEW_PATH,
    SLOW_TOP_N: SLOW_TOP_N,
    EXPORT_SCHEMA: EXPORT_SCHEMA,
    percentile: percentile,
    bucketCounts: bucketCounts,
    routeAggregate: routeAggregate,
    slowest: slowest,
    sourceCounts: sourceCounts,
    lastBootstrap: lastBootstrap,
    maskLocalPath: maskLocalPath,
    maskSecretValue: maskSecretValue,
    stripQuery: stripQuery,
    instancePrefix: instancePrefix,
    buildExportBundle: buildExportBundle,
    render: render,
    load: load,
    readCoreSnapshot: readCoreSnapshot,
  };
  global.renderOwoDiagnosticsLedger = render;
})(typeof window !== "undefined" ? window : globalThis);
