/* 帮助与关于面板：服务与版本 / 能力面速览 / 快捷键 / 斜杠命令 / 文档与契约 / 许可与致谢 / 诊断。
 * 纯脚本 IIFE，注册 window.OwoPanels.about；helpers 缺失时自建 fetch（防御性降级）。
 * 版本号等运行时信息一律从 /health 与 /server/status 读取，不在前端硬编码。
 */
window.OwoPanels = window.OwoPanels || {};
window.OwoPanels.about = (function () {
  "use strict";

  var id = "about";
  var H = null;
  var rootEl = null;

  function defaultHelpers() {
    var baseUrl = (window.OwoPanels && window.OwoPanels.baseUrl) || "";
    function get(path) {
      return fetch(baseUrl + path).then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.json();
      });
    }
    function esc(s) {
      return String(s == null ? "" : s)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    }
    return { baseUrl: baseUrl, get: get, esc: esc };
  }

  var SHORTCUTS = [
    ["Ctrl + N", "新对话"],
    ["Ctrl + O", "打开文件夹（选择工作区）"],
    ["Ctrl + B", "显示 / 隐藏会话栏"],
    ["Ctrl + ,", "打开设置"],
    ["Ctrl + L", "聚焦输入框"],
    ["F11", "全屏 / 窗口"],
    ["Enter", "发送（Shift + Enter 换行）"],
    ["Esc", "关闭菜单 / 弹窗"],
  ];

  var SLASH = [
    ["/new", "新开一个会话"],
    ["/undo", "撤销上一次文件改动"],
    ["/redo", "重做被撤销的改动"],
    ["/export", "导出当前会话"],
    ["/stop", "中止正在跑的回合"],
    ["/settings", "打开设置页"],
    ["/help", "查看全部斜杠命令"],
  ];

  var DOCS = [
    ["OpenAPI 3.1 契约", "openapi.json", "全部 HTTP 端点与 SSE 事件定义"],
    ["隐私声明", "privacy.md", "本地优先、数据出境与感知分层的边界"],
    ["接口形态速查", "schemas", "按版本查询 wire 契约"],
  ];

  var THIRD_PARTY = [
    "axum（HTTP/SSE）",
    "tokio（异步运行时）",
    "serde / serde_json（序列化）",
    "reqwest（模型网关）",
    "clap（命令行）",
    "tracing / tracing-subscriber（日志）",
    "ed25519-dalek / sha2（插件签名与校验）",
    "chrono / uuid / regex / zip / base64",
  ];

  function nav() {
    var shortcutRows = SHORTCUTS.map(function (row) {
      return "<tr><td><kbd>" + row[0].replace(/ \+ /g, "</kbd> + <kbd>") + "</kbd></td><td>" + row[1] + "</td></tr>";
    }).join("");
    var slashRows = SLASH.map(function (row) {
      return "<tr><td><code>" + row[0] + "</code></td><td>" + row[1] + "</td></tr>";
    }).join("");
    var docRows = DOCS.map(function (row) {
      return (
        '<div class="settings-row"><div class="settings-row-label"><strong>' +
        row[0] +
        "</strong><span>" +
        row[2] +
        '</span></div><div class="settings-row-control"><a href="' +
        row[1] +
        '" target="_blank" rel="noopener">打开</a></div></div>'
      );
    }).join("");
    var thirdParty = THIRD_PARTY.map(function (name) {
      return "<li>" + name + "</li>";
    }).join("");

    return (
      '<section data-panel="' +
      id +
      '">' +
      "<style>" +
      // 固定 3 列：6 张卡片排成 2×3，避免 auto-fit 出现「5 张 + 1 张孤儿」
      ".owo-about-cards{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:10px}" +
      "@media (max-width:900px){.owo-about-cards{grid-template-columns:repeat(2,minmax(0,1fr))}}" +
      "@media (max-width:560px){.owo-about-cards{grid-template-columns:minmax(0,1fr)}}" +
      ".owo-about-card{display:flex;flex-direction:column;gap:2px;border:1px solid var(--border);border-radius:var(--r-md);padding:10px 12px;background:var(--surface-2)}" +
      ".owo-about-card b{font-size:22px;font-variant-numeric:tabular-nums;line-height:1.1;color:var(--text)}" +
      // 说明文字限 2 行：MCP 已连接列表这类长文本不再把卡片撑高
      ".owo-about-card span{font-size:11.5px;color:var(--text-2);line-height:1.45;display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}" +
      ".owo-about-pre{background:var(--surface-2);border:1px solid var(--border-strong);border-radius:6px;padding:8px;font-size:12px;max-height:180px;overflow:auto;white-space:pre-wrap}" +
      ".owo-about-table{width:100%;border-collapse:collapse;font-size:12px}" +
      ".owo-about-table th,.owo-about-table td{border-bottom:1px solid var(--border-strong);padding:4px 6px;text-align:left}" +
      "</style>" +
      '<div class="stack">' +
      '<div class="sub">服务与版本</div><div id="owo-about-health" class="owo-about-pre">加载中…</div>' +
      '<div class="sub">能力面速览</div><div class="owo-about-cards" id="owo-about-caps"></div>' +
      '<div class="sub">快捷键</div><table class="owo-about-table"><thead><tr><th>快捷键</th><th>功能</th></tr></thead><tbody>' +
      shortcutRows +
      "</tbody></table>" +
      '<div class="sub">斜杠命令</div><table class="owo-about-table"><thead><tr><th>命令</th><th>作用</th></tr></thead><tbody>' +
      slashRows +
      "</tbody></table>" +
      '<div class="sub">文档与契约</div><div class="settings-card-list">' +
      docRows +
      "</div>" +
      '<div class="sub">许可与致谢</div><div class="settings-card-list">' +
      '<div class="settings-row"><div class="settings-row-label"><strong>许可证</strong><span>GPL-3.0-only（见仓库根 Cargo.toml 的 license 字段）</span></div>' +
      '<div class="settings-row-control"><span class="sub">GPL-3.0-only</span></div></div>' +
      '<div class="settings-row"><div class="settings-row-label"><strong>第三方组件</strong><span>主要依赖（完整清单见 Cargo.lock）</span></div></div>' +
      '<ul class="sub" style="margin:0 0 6px 18px">' +
      thirdParty +
      "</ul>" +
      "</div>" +
      '<div class="sub">诊断</div>' +
      '<div style="display:flex;gap:8px;align-items:center">' +
      '<button type="button" id="owo-about-refresh">刷新</button>' +
      '<button type="button" id="owo-about-copy">复制诊断信息</button>' +
      "</div>" +
      '<div id="owo-about-diag" class="owo-about-pre">（点击刷新）</div>' +
      "</div>"
    );
  }

  function text(id, value) {
    var el = rootEl && rootEl.querySelector("#" + id);
    if (el) el.textContent = value;
  }

  function countOf(data, key) {
    if (data == null) return "—";
    if (Object.prototype.toString.call(data) === "[object Array]") return String(data.length);
    if (typeof data === "object") {
      if (typeof data.count === "number") return String(data.count);
      if (data[key] && Object.prototype.toString.call(data[key]) === "[object Array]") return String(data[key].length);
    }
    return "—";
  }

  function card(label, value, hint) {
    return (
      '<div class="owo-about-card"><span>' +
      label +
      "</span><b>" +
      value +
      "</b><span>" +
      (hint || "") +
      "</span></div>"
    );
  }

  function refresh() {
    var base = (H && H.baseUrl) || "";
    // 1) 服务与版本
    Promise.all([
      H.get("/health").catch(function () { return null; }),
      H.get("/server/status").catch(function () { return null; }),
      H.get("/usage/summary").catch(function () { return null; }),
    ]).then(function (result) {
      var health = result[0] || {};
      var status = result[1] || {};
      var lines = [];
      lines.push("版本：" + (health.version || "未知") + "（/health）");
      lines.push("健康：" + (health.healthy === true ? "正常" : health.healthy === false ? "异常" : "未知"));
      lines.push("自动审批：" + (health.auto_approve ? "已开启（OWO_AUTO_APPROVE）" : "关闭（逐次审批）"));
      if (status.workspace) lines.push("工作区：" + status.workspace);
      if (status.model) lines.push("模型：" + status.model);
      if (status.read_only !== undefined) lines.push("只读模式：" + (status.read_only ? "是" : "否"));
      lines.push("API Base：" + (base || "同源"));
      text("owo-about-health", lines.join("\n"));
    });

    // 2) 能力面速览
    Promise.all([
      H.get("/skills").catch(function () { return null; }),
      H.get("/plugins").catch(function () { return null; }),
      H.get("/mcp").catch(function () { return null; }),
      H.get("/automations").catch(function () { return null; }),
      H.get("/notes").catch(function () { return null; }),
      H.get("/sessions").catch(function () { return null; }),
    ]).then(function (r) {
      var skills = r[0];
      var plugins = r[1];
      var mcp = r[2];
      var connected = (mcp && mcp.connected) || [];
      var hint = connected.length
        ? "已连接：" + connected.join("、")
        : mcp && mcp.count
          ? "仅配置、未连接"
          : "无（插件声明的服务器启动后在此显示）";
      // 大数字优先显示"实际连上的数量"：插件 manifest 声明的服务器不落盘，
      // 只看配置会出现「0 个服务器 / 已连接 3 个」这种自相矛盾的展示。
      var mcpValue = connected.length ? String(connected.length) : countOf(mcp, "servers");
      var caps = []
        .concat([card("技能", countOf(skills, "skills"), "internal + 用户技能")])
        .concat([card("插件", countOf(plugins, "plugins"), "工作区 plugins/ 与数据目录")])
        .concat([card("MCP 服务器", mcpValue, hint)])
        .concat([card("自动化任务", countOf(r[3], null), "定时/间隔提醒")])
        .concat([card("笔记", countOf(r[4], "notes"), "结构块 + 全文检索")])
        .concat([card("会话", countOf(r[5], "sessions"), "含子会话")]);
      var el = rootEl && rootEl.querySelector("#owo-about-caps");
      if (el) el.innerHTML = caps.join("");
    });

    // 3) 诊断（感知 / 视觉 / 模型链路就绪度）
    Promise.all([
      H.get("/perception/ocr/status").catch(function (e) { return { error: String(e && e.message) }; }),
      H.get("/vision/status").catch(function (e) { return { error: String(e && e.message) }; }),
      H.get("/plugins/market").catch(function (e) { return { error: String(e && e.message) }; }),
    ]).then(function (r) {
      var ocr = r[0] || {};
      var vision = r[1] || {};
      var market = r[2] || {};
      var lines = [];
      lines.push("OCR：" + (ocr.error ? "不可用（" + ocr.error + "）" : (ocr.provider || ocr.engine || "就绪") + (ocr.ready === false ? "（未就绪）" : "")));
      lines.push("视觉：" + (vision.error ? "不可用（" + vision.error + "）" : vision.provider || vision.model || "就绪"));
      lines.push("插件市场：" + (market.error ? "不可用（" + market.error + "）" : "可用，条目 " + (market.count != null ? market.count : "—") + "（远端目录走 OWO_MARKET_URL）"));
      lines.push("采样时间：" + new Date().toLocaleString());
      text("owo-about-diag", lines.join("\n"));
    });
  }

  function copyDiagnostics() {
    var health = rootEl && rootEl.querySelector("#owo-about-health");
    var diag = rootEl && rootEl.querySelector("#owo-about-diag");
    var payload = [
      "# OwO Agent 诊断信息",
      "",
      health ? health.textContent : "",
      "",
      diag ? diag.textContent : "",
      "",
      "userAgent: " + navigator.userAgent,
    ].join("\n");
    function fallback() {
      var area = document.createElement("textarea");
      area.value = payload;
      document.body.appendChild(area);
      area.select();
      try {
        document.execCommand("copy");
      } catch (e) {
        /* 忽略：无剪贴板权限时保持静默 */
      }
      document.body.removeChild(area);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(payload).catch(fallback);
    } else {
      fallback();
    }
  }

  function mount(root, helpers) {
    if (helpers) H = helpers;
    if (!H) H = defaultHelpers();
    rootEl = root;
    root.innerHTML = nav();
    var refreshBtn = root.querySelector("#owo-about-refresh");
    if (refreshBtn) refreshBtn.addEventListener("click", refresh);
    var copyBtn = root.querySelector("#owo-about-copy");
    if (copyBtn) copyBtn.addEventListener("click", copyDiagnostics);
    refresh();
  }

  return { id: id, title: "帮助与关于", mount: mount };
})();
