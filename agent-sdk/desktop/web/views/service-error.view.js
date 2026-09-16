/* §7：所有路由复用的统一 ServiceUnavailable 状态卡。
 * 三段式：发生了什么 / 自动做了什么 / 用户下一步能做什么；
 * 技术详情（端口、PID、instance id、build id、错误码、日志路径）折叠展示，
 * 不再要求用户翻日志才能理解失败。数据来自壳 get_core_state /
 * get_core_connection 归一后的 window.__owoCoreDiagnostics。
 */
(function (global) {
  "use strict";

  function esc(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  // 技术详情行：值为空则整行省略（折叠面板不留空洞）。
  function detailRow(label, value) {
    if (value === undefined || value === null || value === "") return "";
    return '<div class="sub"><span>' + esc(label) + "：</span><code>" + esc(value) + "</code></div>";
  }

  global.renderOwoServiceError = function (root, error, retry) {
    if (!root) return;
    const d = global.__owoCoreDiagnostics || null;
    const message = String((error && error.message) || error || "未知错误").replace(/[&<>]/g, "");
    const tauriOwner = global.__TAURI_INTERNALS__ || (global.__TAURI__ && global.__TAURI__.core);
    const logButton = tauriOwner && typeof tauriOwner.invoke === "function"
      ? '<button type="button" data-role="logs">查看日志</button>'
      : "";
    // §6.1.4：壳期望的 build id 与核心上报不一致时提示版本错配。
    const buildMismatch = !!(d && d.buildId && d.expectedBuildId && d.buildId !== d.expectedBuildId);

    root.innerHTML =
      '<div class="service-error" role="alert" aria-live="assertive">' +
      "<strong>本地核心服务暂不可用</strong>" +
      // 1) 发生了什么
      "<p>" + esc(message) + "</p>" +
      (d && d.errorCode
        ? '<p class="sub">错误码：<code>' + esc(d.errorCode) + "</code>" +
          (d.message ? " · " + esc(d.message) : "") + "</p>"
        : "") +
      // 2) 自动做了什么（单飞恢复 + 退避序列，来自 core/recovery.js）
      '<p class="sub">已自动：查询壳连接诊断 → 单飞恢复（0/0.5/1/2/5 秒退避）→ 断连期间停止后台刷新，不重复请求。</p>' +
      // 3) 用户下一步能做什么
      '<div class="inline">' +
      '<button type="button" class="primary" data-role="retry">重新连接</button>' +
      logButton +
      '<button type="button" data-role="settings">打开模型设置</button>' +
      "</div>" +
      // 技术详情折叠（§7：端口/PID/instance id/build id/日志路径）
      "<details><summary>技术详情</summary>" +
      detailRow("状态", d && d.state) +
      detailRow("端口", d && d.port) +
      detailRow("进程 PID", d && d.pid) +
      detailRow("实例 ID", d && d.instanceId) +
      detailRow("核心构建 ID", d && d.buildId) +
      detailRow("期望构建 ID", d && d.expectedBuildId) +
      (buildMismatch ? '<p class="sub">构建不一致：核心与安装包可能版本错配。</p>' : "") +
      detailRow("核心日志", d && d.logPath) +
      "</details>" +
      "</div>";

    const retryButton = root.querySelector('[data-role="retry"]');
    if (retryButton) {
      // §7：连点重试由 recover() 单飞合并，不会并发多次恢复。
      retryButton.addEventListener("click", retry);
    }
    const logsButton = root.querySelector('[data-role="logs"]');
    if (logsButton) {
      logsButton.addEventListener("click", function () {
        try {
          const owner = global.__TAURI_INTERNALS__ || (global.__TAURI__ && global.__TAURI__.core);
          if (owner && typeof owner.invoke === "function") owner.invoke("open_core_logs");
        } catch (_) {
          // 日志打开失败不影响卡片其余操作
        }
      });
    }
    const settingsButton = root.querySelector('[data-role="settings"]');
    if (settingsButton) {
      settingsButton.addEventListener("click", function () {
        if (global.owoRouter && typeof global.owoRouter.go === "function") global.owoRouter.go("settings");
      });
    }
  };
})(typeof window !== "undefined" ? window : globalThis);
