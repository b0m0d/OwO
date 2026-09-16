// §12.3 shell 拆分批次一：统一本地 API 边界与连接状态（自 app.js 机械外移，零行为变化）。
// 顶层声明经经典脚本全局词法环境供 app.js/panels 裸名引用（非 window 属性）。

"use strict";

// 由 Tauri 壳注入核心服务地址；经核心服务同源托管时为空字符串。
const API_BASE = (window.OWO_API_BASE || "").replace(/\/+$/, "");

// ---------- 统一本地 API 边界 ----------

const apiClient = window.OwoApi || new window.OwoApiClient(API_BASE);
window.OwoApi = apiClient;

// §12-15 请求节流指标：每分钟请求数、不可见窗口请求数。
// §3.3：事件域刷新的细分指标（eventRefreshes/coalesced/duplicate/pollFallback/
// hiddenWindow）由 core/events.js 状态机内部统计，诊断只读快照，不再在业务
// handler 里累加（旧 duplicateRefreshes 统计的是事件刷新次数，语义错误，已移除）。
const REQUEST_STATS = {
  thisMinute: 0,
  minuteStarted: 0,
  hiddenRequests: 0,
  history: [],
};
function nowSeconds() {
  return Math.floor((typeof performance !== "undefined" && performance.now ? performance.now() : Date.now()) / 1000);
}
function bumpRequestStat(hidden) {
  const now = nowSeconds();
  if (now - REQUEST_STATS.minuteStarted >= 60) {
    REQUEST_STATS.history.push({ t: now, requests: REQUEST_STATS.thisMinute });
    if (REQUEST_STATS.history.length > 60) REQUEST_STATS.history.shift();
    REQUEST_STATS.thisMinute = 0;
    REQUEST_STATS.minuteStarted = now;
  }
  REQUEST_STATS.thisMinute += 1;
  if (hidden) REQUEST_STATS.hiddenRequests += 1;
}
function requestStatsSnapshot() {
  return {
    thisMinute: REQUEST_STATS.thisMinute,
    hiddenRequests: REQUEST_STATS.hiddenRequests,
    history: REQUEST_STATS.history.slice(-60),
    // §3.3：事件失效指标快照（events.js 状态机内部计数）。
    events: invalidator ? invalidator.snapshot() : null,
  };
}

function markConnectionUnavailable() {
  const health = document.getElementById("health");
  if (health) {
    health.textContent = "本地服务未连接";
    health.style.color = "var(--yellow)";
  }
  const summary = document.getElementById("connectionSummary");
  if (summary) summary.textContent = "本地服务未连接";
}

function markConnectionReady() {
  const health = document.getElementById("health");
  if (health) {
    health.textContent = "本地服务已连接";
    health.style.color = "var(--green)";
  }
  const summary = document.getElementById("connectionSummary");
  if (summary) summary.textContent = "服务已连接";
}

window.addEventListener("owo:connection", (event) => {
  if (event.detail && event.detail.ready) markConnectionReady();
  else markConnectionUnavailable();
});

async function api(path, options = {}) {
  bumpRequestStat(document.visibilityState === "hidden");
  return apiClient.request(path, options);
}

// 统一友好错误：404/405/5xx 提示"服务接口不可用"，其余透传原错误。
// resource=true 时（契约资源型 404 路径），404/405 提示"资源不存在"而非接口不可用。
function friendlyError(error, options = {}) {
  const msg = String((error && error.message) || error || "");
  const match = msg.match(/^(\d{3}):/);
  const status = match ? Number(match[1]) : 0;
  if (status === 404 || status === 405 || status >= 500) {
    return options.resource ? `资源不存在（HTTP ${status}）` : `服务接口不可用（HTTP ${status}）`;
  }
  return msg || "未知错误";
}

function addMessage(kind, text, meta = "") {
  const div = document.createElement("div");
  div.className = `msg ${kind}`;
  if (meta) {
    const span = document.createElement("span");
    span.className = "meta";
    span.textContent = meta;
    div.appendChild(span);
  }
  if (kind === "assistant" || kind === "user") {
    div.innerHTML = renderMarkdown(text);
  } else {
    div.appendChild(document.createTextNode(text));
  }
  bindCopyButtons(div);
  $("messages").appendChild(div);
  $("messages").scrollTop = $("messages").scrollHeight;
  return div;
}

function bindCopyButtons(root) {
  for (const button of root.querySelectorAll(".md-copy")) {
    button.addEventListener("click", () => {
      const code = decodeURIComponent(button.dataset.code || "");
      navigator.clipboard.writeText(code).then(() => {
        button.textContent = "已复制";
        setTimeout(() => {
          button.textContent = "复制";
        }, 1200);
      });
    });
  }
}

function esc(text) {
  const div = document.createElement("div");
  div.textContent = text;
  return div.innerHTML;
}
