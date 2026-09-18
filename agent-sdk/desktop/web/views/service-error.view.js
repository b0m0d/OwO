/* §7 / R3-B（§3.4 契约 + §4.7 稳定动作 ID）：所有路由复用的统一 ServiceUnavailable 状态卡。
 * 三段式：发生了什么 / 自动做了什么 / 用户下一步能做什么；
 * 技术详情（端口、PID、instance id、build id、错误码、日志路径）折叠展示。
 * R3-B：动作按钮**按稳定错误码渲染**（UI 不匹配中文文案判型）——
 * 每个错误码对应 §3.4「必须提供的动作」，按钮携带 data-action（稳定 ID）。
 * 数据来自壳 get_core_state / get_core_connection 归一后的 window.__owoCoreDiagnostics。
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

  // §3.4 契约：错误码 → 必须提供的动作（label 即验收断言文案，id 稳定不变）。
  // role：retry=重试（含终止并重试）/ logs=打开诊断位置 / settings=打开模型设置 /
  // test=测试连接 / data-dir=更换数据目录 / workspace=选择文件夹。
  const ERROR_ACTION_PRESETS = {
    "core/binary_missing": [
      { id: "recheck_core", label: "重新检查", role: "retry" },
      { id: "open_diagnostics_location", label: "打开诊断位置", role: "logs" }
    ],
    "core/handshake_timeout": [
      { id: "terminate_retry", label: "终止并重试", role: "retry" }
    ],
    "core/exited": [
      { id: "retry_core", label: "重启后台", role: "retry" },
      { id: "open_diagnostics", label: "查看诊断", role: "logs" }
    ],
    "core/identity_mismatch": [
      { id: "reinstall_component", label: "重新安装或重建", role: "retry" }
    ],
    "core/spawn_failed": [
      { id: "retry_core", label: "重试", role: "retry" },
      { id: "open_diagnostics", label: "查看诊断", role: "logs" }
    ],
    "storage/not_writable": [
      { id: "choose_data_directory", label: "更换数据目录", role: "data-dir" },
      { id: "retry_core", label: "重试", role: "retry" }
    ],
    "provider/not_configured": [
      { id: "open_provider_settings", label: "打开模型设置", role: "settings" },
      { id: "test_connection", label: "测试连接", role: "test" }
    ],
    "workspace/required": [
      { id: "choose_workspace", label: "选择文件夹", role: "workspace" }
    ]
  };

  // 无错误码（旧壳/未知故障）：保持历史三出口，不空转。
  const DEFAULT_ACTIONS = [
    { id: "retry_core", label: "重新连接", role: "retry" },
    { id: "open_diagnostics_location", label: "查看日志", role: "logs" },
    { id: "open_provider_settings", label: "打开模型设置", role: "settings" }
  ];

  // §3.4 终态跟查（本卡自身的"别把过程当结论"机制）。
  // 历史缺陷：卡面在 t≈10s 渲染一次就定型，而那时壳常常还在 starting——
  // core-hang 的最终判定要到 45s、缺目录写失败也在握手期之后。于是真机矩阵里
  // core-hang / data-dir-unwritable 报的是"没有稳定码"，实际是**码还没轮到上屏**，
  // 界面却已经把自己锁死在默认三出口。这里在有界预算内持续重查壳连接（纯 Tauri
  // IPC，零 HTTP，不占 §8.2 首屏请求预算），壳一进入带码终态立刻以该码重绘本卡。
  let terminalWatchSeq = 0;

  function startTerminalFollowUp(root, error, retry) {
    const seq = ++terminalWatchSeq;
    const deadline = Date.now() + 45000;   // §3.4 最长最终预算：挂死场景 45s
    function tick() {
      // 只有"这张卡还在屏上、且仍是本次渲染"才继续：用户已切页/已恢复或已被
      // 更新的卡取代时，旧 watcher 必须自行终止，不得抢占新界面。
      if (seq !== terminalWatchSeq || !root || !root.isConnected) return;
      const d = global.__owoCoreDiagnostics;
      if (d && typeof d.errorCode === "string" && d.errorCode) {
        terminalWatchSeq = seq + 1;         // 认领本次重绘，终止包括自己在内的所有跟查
        global.renderOwoServiceError(root, error, retry);
        return;
      }
      if (Date.now() >= deadline) return;   // 预算用尽：保持默认出口，不再空转
      Promise.resolve()
        .then(function () {
          // ensureCoreConnection 自带单飞；非 ready 快照不再被缓存（R3-B 修复），
          // 所以这里每次 tick 都会真的重新问一次壳，而不是拿旧快照自欺。
          if (global.OwoApi && typeof global.OwoApi.ensureCoreConnection === "function") {
            return global.OwoApi.ensureCoreConnection();
          }
        })
        .catch(function () { /* 重查失败本身不是终态，下一 tick 再看 */ })
        .then(function () { setTimeout(tick, 1000); });
    }
    setTimeout(tick, 1000);
  }

  global.renderOwoServiceError = function (root, error, retry) {
    if (!root) return;
    const d = global.__owoCoreDiagnostics || null;
    const message = String((error && error.message) || error || "未知错误").replace(/[&<>]/g, "");
    const tauriOwner = global.__TAURI_INTERNALS__ || (global.__TAURI__ && global.__TAURI__.core);
    const hasInvoke = !!(tauriOwner && typeof tauriOwner.invoke === "function");
    // §6.1.4：壳期望的 build id 与核心上报不一致时提示版本错配。
    const buildMismatch = !!(d && d.buildId && d.expectedBuildId && d.buildId !== d.expectedBuildId);
    const code = d && typeof d.errorCode === "string" ? d.errorCode : "";
    const actions = ERROR_ACTION_PRESETS[code] || DEFAULT_ACTIONS;
    // 现在还没有稳定码、且运行在桌面壳里 → 终态大概率稍后才到，挂上跟查。
    // 非壳环境（浏览器直连 4096）没有"壳稍后上报"可等，不挂，避免无意义轮询。
    if (!code && hasInvoke) startTerminalFollowUp(root, error, retry);

    function shellInvoke(command, args) {
      if (!hasInvoke) return Promise.resolve(null);
      try {
        return Promise.resolve(tauriOwner.invoke.call(tauriOwner, command, args || {}));
      } catch (e) {
        return Promise.reject(e);
      }
    }

    function runRetry() {
      // 「终止并重试/重启后台/重新检查」统一走壳 retry_core_start（R3-B：该命令
      // 现在先终止当前子进程树，不留孤儿），再重查连接并恢复。
      shellInvoke("retry_core_start").catch(function () { /* 非壳环境降级为纯前端恢复 */ });
      if (global.OwoApi && global.OwoApi.resetCoreConnection) global.OwoApi.resetCoreConnection();
      Promise.resolve().then(function () { if (typeof retry === "function") retry(); });
    }

    const actionHtml = actions.map(function (a, idx) {
      const primary = idx === 0 ? ' class="primary"' : "";
      const disabled = (a.role === "data-dir" && !hasInvoke)
        ? " disabled title=\"仅桌面环境可用\""
        : "";
      return "<button type=\"button\"" + primary +
        " data-action=\"" + esc(a.id) + "\" data-role=\"" + esc(a.role) + "\"" + disabled + ">" +
        esc(a.label) + "</button>";
    }).join("");

    root.innerHTML =
      '<div class="service-error" role="alert" aria-live="assertive">' +
      "<strong>本地核心服务暂不可用</strong>" +
      // 1) 发生了什么
      "<p>" + esc(message) + "</p>" +
      (code
        ? '<p class="sub">错误码：<code>' + esc(code) + "</code>" +
          (d.message ? " · " + esc(d.message) : "") + "</p>"
        : "") +
      // 2) 自动做了什么（单飞恢复 + 退避序列，来自 core/recovery.js）
      '<p class="sub">已自动：查询壳连接诊断 → 单飞恢复（0/0.5/1/2/5 秒退避）→ 断连期间停止后台刷新，不重复请求。</p>' +
      // 3) 用户下一步能做什么（按错误码呈现 §3.4 契约动作）
      '<div class="inline">' + actionHtml + "</div>" +
      // 测试结果行（测试连接动作的落点）
      '<p class="sub" data-role="test-result" hidden></p>' +
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

    const testLine = root.querySelector('[data-role="test-result"]');

    root.querySelectorAll(".service-error button[data-action]").forEach(function (button) {
      const role = button.getAttribute("data-role");
      button.addEventListener("click", function () {
        if (role === "retry" || role === "workspace") {
          runRetry();
        } else if (role === "logs") {
          shellInvoke("open_core_logs").catch(function () { /* 打不开日志不影响其余操作 */ });
        } else if (role === "settings") {
          if (global.owoRouter && typeof global.owoRouter.go === "function") global.owoRouter.go("settings");
        } else if (role === "data-dir") {
          button.disabled = true;
          shellInvoke("choose_data_directory").then(function (result) {
            button.disabled = false;
            if (testLine) {
              testLine.hidden = false;
              testLine.textContent = result && result.ok
                ? "数据目录已更新：" + String(result.data_root || "") + "（核心正在重启）"
                : result && result.canceled
                  ? "已取消选择，数据目录保持不变"
                  : "更换数据目录失败：" + String((result && result.error) || "未知原因");
            }
            if (result && result.ok && typeof retry === "function") retry();
          }).catch(function (e) {
            button.disabled = false;
            if (testLine) { testLine.hidden = false; testLine.textContent = "更换数据目录失败：" + String((e && e.message) || e); }
          });
        } else if (role === "test") {
          button.disabled = true;
          const api = global.OwoApi;
          if (!api || typeof api.post !== "function") {
            button.disabled = false;
            if (testLine) { testLine.hidden = false; testLine.textContent = "测试连接不可用（API 客户端未就绪）"; }
            return;
          }
          api.post("/settings/provider-test", {}).then(function (result) {
            button.disabled = false;
            if (testLine) {
              testLine.hidden = false;
              testLine.textContent = "测试结果：" + String((result && result.code) || "unknown") +
                (result && result.endpoint ? " · " + result.endpoint : "") +
                (result && typeof result.latency_ms === "number" ? " · " + result.latency_ms + "ms" : "");
            }
          }).catch(function (e) {
            button.disabled = false;
            if (testLine) { testLine.hidden = false; testLine.textContent = "测试连接失败：" + String((e && e.message) || e); }
          });
        }
      });
    });
  };
})(typeof window !== "undefined" ? window : globalThis);
