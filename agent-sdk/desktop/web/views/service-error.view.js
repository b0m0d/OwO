/* §5.1.6/§5.6：所有路由复用的统一 ServiceUnavailable 卡片。
 * 展示启动诊断（错误码/文案/日志路径，来自壳 get_core_connection），
 * 并保留「重试 / 查看日志 / 模型设置」三个可操作出口。
 */
(function (global) {
  "use strict";

  function esc(value) {
    return String(value == null ? "" : value).replace(/[&<>"']/g, function (ch) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch];
    });
  }

  global.renderOwoServiceError = function (root, error, retry) {
    if (!root) return;
    const diagnostics = global.__owoCoreDiagnostics || null;
    const message = String((error && error.message) || error || "未知错误").replace(/[&<>]/g, "");
    const codeLine = diagnostics && diagnostics.errorCode
      ? '<p class="sub">错误码：<code>' + esc(diagnostics.errorCode) + "</code>" +
        (diagnostics.message ? " · " + esc(diagnostics.message) : "") + "</p>"
      : "";
    const logLine = diagnostics && diagnostics.logPath
      ? '<p class="sub">核心日志：<code>' + esc(diagnostics.logPath) + "</code></p>"
      : "";
    const tauriOwner = global.__TAURI_INTERNALS__ || (global.__TAURI__ && global.__TAURI__.core);
    const logButton = tauriOwner && typeof tauriOwner.invoke === "function"
      ? '<button type="button" data-role="logs">查看日志</button>'
      : "";
    root.innerHTML =
      '<div class="service-error" role="alert" aria-live="assertive">' +
      "<strong>本地核心服务暂不可用</strong>" +
      "<p>工作台会自动重试。断连期间仍可进入模型设置调整连接。</p>" +
      "<code>" + esc(message) + "</code>" +
      codeLine +
      logLine +
      '<div class="inline">' +
      '<button type="button" class="primary" data-role="retry">重试服务</button>' +
      logButton +
      '<button type="button" data-role="settings">打开模型设置</button>' +
      "</div></div>";
    const retryButton = root.querySelector('[data-role="retry"]');
    if (retryButton) retryButton.addEventListener("click", retry);
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
