// OwO Agent 工作台（v0.4 P1 桌面壳，纯静态，直连本地 HTTP API + SSE）
"use strict";

const state = {
  sessionId: null,
  pendingApproval: null,
  // 跨会话审批队列：request_id → { tool, reason, sessionId }。
  // 多对话并行时每个会话都可能挂起等待审批，单值卡会互相覆盖/不可见。
  pendingApprovals: new Map(),
  // 仍在运行的回合：sessionId → { controller, startedAt }。切走会话不打断任务，
  // 切回时恢复该会话的本地实时视图。
  activeTurns: new Map(),
  // 当前回合待回答的提问（ask_user：回合挂起等待用户答复）
  pendingQuestion: null,
  reading: false,
  attachments: [],
  abortController: null,
  selectionVersion: 0,
  // 流式渲染：粘性滚动 + 当前回合的思考块/工具分组句柄
  autoScroll: true,
  thinking: null,
  toolRun: null,
  // 当前回合统计（工具次数/模型调用轮次/耗时），供回合汇报卡使用
  turn: null,
  // 运行状态条（转圈 + 阶段文案 + 计时）：让用户明确感知 agent 正在工作
  runStartedAt: 0,
  runTicker: null,
};

const $ = (id) => document.getElementById(id);
// 由 Tauri 壳注入核心服务地址；经核心服务同源托管时为空字符串。
const API_BASE = (window.OWO_API_BASE || "").replace(/\/+$/, "");

let recognition = null;
let listening = false;
let localRecorder = null;

function initSpeech() {
  const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!SpeechRecognition) return;
  recognition = new SpeechRecognition();
  recognition.lang = "zh-CN";
  recognition.continuous = false;
  recognition.interimResults = false;
  recognition.onresult = (event) => {
    const text = event.results[0][0].transcript;
    const prompt = $("prompt");
    prompt.value = (prompt.value ? prompt.value + " " : "") + text;
  };
  recognition.onend = () => {
    listening = false;
    setMicRecording(false);
  };
}

function encodeWav(samples, sampleRate) {
  const buffer = new ArrayBuffer(44 + samples.length * 2);
  const view = new DataView(buffer);
  const writeString = (offset, str) => {
    for (let i = 0; i < str.length; i++) view.setUint8(offset + i, str.charCodeAt(i));
  };
  writeString(0, "RIFF");
  view.setUint32(4, 36 + samples.length * 2, true);
  writeString(8, "WAVE");
  writeString(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  writeString(36, "data");
  view.setUint32(40, samples.length * 2, true);
  let offset = 44;
  for (const sample of samples) {
    const clamped = Math.max(-1, Math.min(1, sample));
    view.setInt16(offset, clamped * 0x7fff, true);
    offset += 2;
  }
  return new Blob([buffer], { type: "audio/wav" });
}

// 本地优先语音输入：麦克风 → 16k WAV → /stt/transcribe（SenseVoice-Small）。
// 本地 STT 不可用时回退到系统 Web Speech。
async function startLocalRecording() {
  const AudioCtx = window.AudioContext || window.webkitAudioContext;
  if (!AudioCtx) return false;
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (_) {
    return false;
  }
  const context = new AudioCtx({ sampleRate: 16000 });
  const source = context.createMediaStreamSource(stream);
  const processor = context.createScriptProcessor(4096, 1, 1);
  const chunks = [];
  source.connect(processor);
  processor.connect(context.destination);
  processor.onaudioprocess = (event) => {
    chunks.push(new Float32Array(event.inputBuffer.getChannelData(0)));
  };
  localRecorder = {
    stop: async () => {
      source.disconnect();
      processor.disconnect();
      stream.getTracks().forEach((track) => track.stop());
      const sampleRate = context.sampleRate;
      await context.close();
      const samples = [];
      for (const chunk of chunks) samples.push(...chunk);
      return { blob: encodeWav(samples, sampleRate), sampleCount: samples.length };
    },
  };
  return true;
}

// ---------- R7 X03：本地 API bearer token（/auth/token 公开引导配对） ----------

let apiToken = null;
let apiTokenRequest = null;
let connectionUnavailableUntil = 0;

function markConnectionUnavailable() {
  // 启动中的桌面壳会在同一时刻加载二十多个面板。短暂断连时只允许
  // 一次探测，避免每个面板都向 /auth/token 发请求并刷满控制台。
  connectionUnavailableUntil = Date.now() + 5000;
  const health = $("health");
  if (health) {
    health.textContent = "本地服务未连接";
    health.style.color = "var(--yellow)";
  }
  const bar = $("menubarHealth");
  if (bar) {
    bar.textContent = "服务未连接";
    bar.style.color = "var(--yellow)";
    bar.style.background = "var(--yellow-soft)";
    bar.style.borderColor = "transparent";
  }
  const summary = $("connectionSummary");
  if (summary) {
    summary.textContent = "本地服务未连接";
    summary.classList.add("offline");
  }
  // 交给就绪探针低频确认：真断连才亮横幅，单次抖动不打扰用户。
  serviceWatch.notifyOffline();
}

function markConnectionReady() {
  connectionUnavailableUntil = 0;
  const health = $("health");
  if (health && health.textContent === "本地服务未连接") {
    health.textContent = "本地服务已连接";
    health.style.color = "var(--green)";
  }
  const bar = $("menubarHealth");
  if (bar) {
    bar.textContent = "服务已连接";
    bar.style.color = "var(--green)";
    bar.style.background = "var(--green-soft)";
    bar.style.borderColor = "transparent";
  }
  const summary = $("connectionSummary");
  if (summary) {
    summary.textContent = "服务已连接";
    summary.classList.remove("offline");
  }
  serviceWatch.markOnline();
}

// ---------- 服务就绪探针与故障横幅 ----------
// 启动时以 100→800ms 指数退避在约 10s 窗口内静默等待核心服务（/health 为公开接口，
// 不经过 token 引导）；窗口耗尽或运行中断连后切到低频重试（3s→6s→12s，封顶 15s）
// 并显示非阻塞横幅。恢复后横幅自动消失，并沿用既有 markConnection* 文案通道。
// 不做秒级轮询：页面隐藏时停发请求，转回可见时立即补一次探测。

const serviceWatch = (() => {
  const STARTUP_DEADLINE_MS = 10000;
  const RETRY_BASE_MS = 3000;
  const RETRY_MAX_MS = 15000;
  let phase = "idle"; // idle | startup | offline | online
  let attempts = 0;
  let startupDeadline = 0;
  let startupDelay = 100;
  let retryDelay = RETRY_BASE_MS;
  let timer = null;

  async function probeOnce() {
    const response = await fetch(`${API_BASE}/health`, { cache: "no-store" });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    const data = await response.json().catch(() => null);
    if (!data || data.healthy !== true) throw new Error("核心服务未报告 healthy");
    return data;
  }

  function clearTimer() {
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
  }

  function showBanner() {
    const text = $("serviceBannerText");
    if (text) text.textContent = `本地服务未连接，正在重试（第 ${attempts} 次）…`;
    const banner = $("serviceBanner");
    if (banner) banner.classList.remove("hidden");
  }

  function hideBanner() {
    const banner = $("serviceBanner");
    if (banner) banner.classList.add("hidden");
  }

  function schedule(delay) {
    clearTimer();
    if (document.hidden) return; // 页面隐藏时跳过探测，转可见时补一次
    timer = setTimeout(tick, delay);
  }

  function markOnline() {
    phase = "online";
    attempts = 0;
    retryDelay = RETRY_BASE_MS;
    clearTimer();
    hideBanner();
  }

  async function tick() {
    timer = null;
    attempts += 1;
    try {
      await probeOnce();
      markConnectionReady(); // 内部 markOnline：复位状态、清定时器、隐藏横幅
    } catch (error) {
      markConnectionUnavailable();
      if (phase === "startup" && Date.now() < startupDeadline) {
        // 启动窗口内：静默快速重试，暂不打扰用户。
        const delay = startupDelay;
        startupDelay = Math.min(startupDelay * 2, 800);
        schedule(delay);
        return;
      }
      phase = "offline";
      showBanner();
      schedule(retryDelay);
      retryDelay = Math.min(retryDelay * 2, RETRY_MAX_MS);
    }
  }

  function start() {
    if (phase === "online" || timer) return;
    phase = "startup";
    attempts = 0;
    startupDelay = 100;
    startupDeadline = Date.now() + STARTUP_DEADLINE_MS;
    tick();
  }

  function notifyOffline() {
    if (phase === "startup" || phase === "offline") return; // 已在探测或重试
    phase = "offline";
    retryDelay = RETRY_BASE_MS;
    schedule(RETRY_BASE_MS); // 先确认再亮横幅，避免单次抖动误报
  }

  const retryBtn = $("serviceBannerRetry");
  if (retryBtn) {
    retryBtn.addEventListener("click", () => {
      clearTimer();
      tick();
    });
  }
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && phase !== "online" && !timer) tick();
  });

  return { start, notifyOffline, markOnline };
})();

async function ensureApiToken() {
  if (apiToken) return apiToken;
  if (Date.now() < connectionUnavailableUntil) {
    throw new Error("本地服务尚未就绪，请稍候重试");
  }
  if (apiTokenRequest) return apiTokenRequest;
  apiTokenRequest = (async () => {
    try {
      const response = await fetch(API_BASE + "/auth/token");
      if (!response.ok) throw new Error(`token 引导失败（HTTP ${response.status}）`);
      const data = await response.json();
      apiToken = data && data.token ? data.token : null;
      if (!apiToken) throw new Error("token 引导响应缺少 token");
      connectionUnavailableUntil = 0;
      return apiToken;
    } catch (error) {
      markConnectionUnavailable();
      throw error;
    } finally {
      apiTokenRequest = null;
    }
  })();
  return apiTokenRequest;
}

async function api(path, options = {}) {
  const headers = new Headers(options.headers || {});
  if (options.body != null && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }
  let response = await fetchWithToken(path, options, headers);
  if (response.ok) markConnectionReady();
  // 401：token 过期/服务重启 → 重新引导一次后重试。
  if (response.status === 401) {
    apiToken = null;
    await ensureApiToken().catch(() => {});
    response = await fetchWithToken(path, options, headers);
  }
  if (!response.ok) {
    const body = await response.text();
    throw new Error(`${response.status}: ${body}`);
  }
  return response.status === 204 ? null : response.json();
}

async function fetchWithToken(path, options, headers) {
  if (!headers.has("Authorization")) {
    const token = apiToken || (await ensureApiToken().catch(() => null));
    if (token) headers.set("Authorization", `Bearer ${token}`);
  }
  try {
    return await fetch(API_BASE + path, { ...options, headers });
  } catch (error) {
    markConnectionUnavailable();
    throw error;
  }
}

// 非 JSON 响应（blob / zip / 文本）专用：与 api() 同款 bearer 鉴权 + 401 重试，
// 但不强制 Content-Type、不解析 JSON（下载与上传类请求走这里）。
async function apiRaw(path, options = {}) {
  const headers = new Headers(options.headers || {});
  let response = await fetchWithToken(path, options, headers);
  if (response.status === 401) {
    apiToken = null;
    await ensureApiToken().catch(() => {});
    response = await fetchWithToken(path, options, headers);
  }
  return response;
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

// ---------- 粘性滚动：用户上滑后不被强行拉底 ----------

function messagesAtBottom() {
  const box = $("messages");
  if (!box) return true;
  return box.scrollHeight - box.scrollTop - box.clientHeight < 40;
}

// 仅在「跟随底部」时滚动；force 用于用户主动点回到底部。
function followScroll(force = false) {
  const box = $("messages");
  if (!box) return;
  if (!force && !state.autoScroll) return;
  box.scrollTop = box.scrollHeight;
}

function updateScrollBottomBtn() {
  const button = $("scrollBottomBtn");
  if (!button) return;
  button.classList.toggle("hidden", state.autoScroll);
}

function initChatScroll() {
  const box = $("messages");
  if (box) {
    box.addEventListener("scroll", () => {
      state.autoScroll = messagesAtBottom();
      updateScrollBottomBtn();
    });
  }
  const button = $("scrollBottomBtn");
  if (button) {
    button.addEventListener("click", () => {
      state.autoScroll = true;
      followScroll(true);
      updateScrollBottomBtn();
    });
  }
}

// ---------- 思考流与工具计数：思考过程常显，工具调用只汇报次数 ----------

// ---------- 每会话独立消息视图：多对话并行时各自的输出留在自己的容器里 ----------
// 流写入目标是「发起回合的会话」（writeTargetSid）：切走会话任务继续跑且不串台；
// 未结束回合的消息在服务端 commit 前只存在于本地视图，切回时原样恢复。
let writeTargetSid = null;

function sessionView(sessionId) {
  const sid = sessionId || state.sessionId;
  if (!sid) return $("messages");
  let view = $("messages").querySelector(`.session-view[data-sid="${sid}"]`);
  if (!view) {
    view = document.createElement("div");
    view.className = "session-view";
    view.dataset.sid = sid;
    $("messages").appendChild(view);
  }
  return view;
}

function writeBox() {
  return sessionView(writeTargetSid || state.sessionId);
}

function showSessionView(sessionId) {
  $("messages").querySelectorAll(".session-view").forEach((view) => {
    view.style.display = view.dataset.sid === sessionId ? "" : "none";
  });
}

function newMessageBlock(node) {
  writeBox().appendChild(node);
  followScroll();
  const empty = $("emptyState");
  if (empty) empty.classList.add("hidden");
  return node;
}

// 思考块渲染约束：实测单回合推理可超 10 万字，整段灌 DOM 会拖垮页面、
// 也让用户误以为「只有思考没有结果」。超出上限的部分只计数不渲染；
// 增量按 120ms 批量落 DOM，避免每个 delta 一次重排 + 滚动。
const THINKING_RENDER_LIMIT = 30000;
const THINKING_FLUSH_MS = 120;

// 思考过程块：流式追加；结束后保持展开可见（点摘要可手动折叠）。
function ensureThinking() {
  if (state.thinking) return state.thinking;
  const el = document.createElement("details");
  el.className = "thinking-block";
  el.open = true;
  const summary = document.createElement("summary");
  summary.innerHTML =
    '<span class="thinking-label">思考过程</span><span class="thinking-hint">思考中…</span>';
  const body = document.createElement("div");
  body.className = "thinking-body";
  el.append(summary, body);
  el.classList.add("is-live");
  newMessageBlock(el);
  state.thinking = { el, summary, body, chars: 0, pending: "", flushTimer: null };
  return state.thinking;
}

function pushReasoning(delta) {
  if (!delta) return;
  // 新思考开始：上一段工具分组到此为止，后续工具归入新组。
  state.toolRun = null;
  const block = ensureThinking();
  block.chars += delta.length;
  if (block.chars > THINKING_RENDER_LIMIT) return;
  block.pending += delta;
  if (block.flushTimer) return;
  block.flushTimer = setTimeout(() => {
    block.flushTimer = null;
    if (!block.pending) return;
    block.body.appendChild(document.createTextNode(block.pending));
    block.pending = "";
    followScroll();
  }, THINKING_FLUSH_MS);
}

// 思考结束：不再自动折叠，只把提示更新为字数（点摘要可手动折叠；
// 超限时注明仅展示前 N 字，避免用户误以为思考被截断丢失）。
function finishThinking() {
  const block = state.thinking;
  if (!block) return;
  if (block.flushTimer) {
    clearTimeout(block.flushTimer);
    block.flushTimer = null;
  }
  if (block.pending) {
    block.body.appendChild(document.createTextNode(block.pending));
    block.pending = "";
  }
  block.el.classList.remove("is-live");
  const hint = block.summary.querySelector(".thinking-hint");
  if (hint) {
    hint.textContent =
      block.chars > THINKING_RENDER_LIMIT
        ? `（${block.chars} 字，仅展示前 ${THINKING_RENDER_LIMIT} 字）`
        : `（${block.chars} 字）`;
  }
  state.thinking = null;
}

// ---------- 工具步骤时间线：每次调用一枚可展开的 chip（对标 Codex 逐步可追溯） ----------
// 旧实现只汇报「已执行 N 个工具」，用户无法复核 agent 到底做了什么。现在每步落一枚 chip：
// 工具名 + 一行入参摘要 + 状态点，展开可见完整入参与结果预览（服务端 preview）或失败原因。
// 历史回放复用同一套渲染，不再出现「已折叠未显示」。

const TOOL_LABELS = {
  read_file: "读取文件",
  write_file: "写入文件",
  edit_file: "编辑文件",
  multi_edit: "批量编辑",
  list_dir: "列出目录",
  search_files: "搜索文件",
  grep: "搜索内容",
  run_command: "运行命令",
  explore: "探索代码库",
  subagent: "子代理",
  use_skill: "调用技能",
  ask_user: "询问用户",
  web_search: "网络搜索",
  web_fetch: "抓取网页",
  git_status: "仓库状态",
  git_diff: "查看改动",
  git_log: "提交历史",
  fan_out_subagents: "并行子代理",
  update_plan: "更新计划",
  clipboard_read: "读剪贴板",
  clipboard_write: "写剪贴板",
  screen_ocr: "屏幕识别",
  screenshot: "截屏",
};

function toolLabel(tool) {
  const name = String(tool || "").trim();
  if (!name) return "工具";
  if (TOOL_LABELS[name]) return TOOL_LABELS[name];
  if (name.startsWith("mcp_")) return `MCP · ${name.slice(4)}`;
  if (name.startsWith("owo_plugin_")) return `插件 · ${name.slice(11)}`;
  return name;
}

/// 入参摘要：一行说清这一步在做什么（Codex 的 chip 也是这个信息密度）。
function toolStepSummary(tool, args) {
  const input = args && typeof args === "object" ? args : {};
  const pick = (...keys) => {
    for (const key of keys) {
      const value = input[key];
      if (typeof value === "string" && value.trim()) return value.trim();
    }
    return "";
  };
  const joined = (...parts) => parts.filter(Boolean).join(" · ");
  switch (tool) {
    case "run_command":
      return pick("command", "cmd", "script");
    case "read_file":
    case "write_file":
    case "edit_file":
    case "multi_edit":
      return pick("path", "file_path");
    case "list_dir":
      return pick("path", "dir");
    case "search_files":
    case "grep":
      return joined(pick("pattern", "query"), pick("path", "dir"));
    case "explore":
    case "subagent":
      return pick("task", "prompt", "query");
    case "use_skill":
      return pick("name", "skill");
    case "ask_user":
      return pick("question", "prompt");
    case "web_search":
      return pick("query");
    case "web_fetch":
      return pick("url", "urls");
    case "git_status":
      return pick("path");
    case "git_diff":
      return joined(pick("path"), input.staged ? "暂存区" : "");
    case "git_log":
      return joined(pick("path"), input.limit ? `最近 ${input.limit} 条` : "");
    case "fan_out_subagents": {
      const tasks = Array.isArray(input.tasks) ? input.tasks.length : 0;
      return tasks ? `${tasks} 个并行子任务` : "";
    }
    default: {
      const first = Object.values(input).find(
        (value) => typeof value === "string" && value.trim()
      );
      return first ? String(first).trim() : "";
    }
  }
}

const TOOL_PREVIEW_RENDER_LIMIT = 4000;

/// 结果预览：历史记录里的工具输出可能到 5 万字，落 DOM 前必须截断。
function clipToolPreview(text) {
  const raw = String(text == null ? "" : text);
  if (raw.length <= TOOL_PREVIEW_RENDER_LIMIT) return raw;
  return `${raw.slice(0, TOOL_PREVIEW_RENDER_LIMIT)}\n…[预览已截断，完整内容见会话记录]`;
}

/// 可点击打开的路径：只有文件/目录类工具给「打开」语义（命令、搜索模式不在此列）。
const FILE_TOOLS = new Set([
  "read_file",
  "write_file",
  "edit_file",
  "multi_edit",
  "list_dir",
]);

function toolStepPath(tool, args) {
  if (!FILE_TOOLS.has(String(tool || ""))) return "";
  const input = args && typeof args === "object" ? args : {};
  const value = input.path || input.file_path || input.dir || "";
  return typeof value === "string" ? value.trim() : "";
}

/// 点击路径 → 交本机服务在工作区内打开（越界路径服务端直接 403，不执行任何命令）。
/// 打开方式取本机偏好「默认文件打开方式」；所选程序不可用时后端回落系统默认并如实回报。
async function openWorkspacePath(path, line) {
  const target = String(path || "").trim();
  if (!target) return;
  const preferred = loadLocalPrefs().fileOpener || "system";
  try {
    const result = await api("/fs/open", {
      method: "POST",
      body: JSON.stringify({ path: target, line: line || null, opener: preferred }),
    });
    const used = (result && result.opener) || preferred;
    const labels = { system: "系统默认程序", notepad: "记事本", vscode: "VS Code" };
    const fallback = used === preferred ? "" : "（所选程序不可用，已回落系统默认）";
    showToast(`已用${labels[used] || used}打开：${target}${fallback}`, "ok");
  } catch (error) {
    showToast(`打开失败：${friendlyError(error, { resource: true })}`, "error");
  }
}

/// 单枚步骤 chip：details 承载「摘要一行 + 展开入参/结果」。
function createToolStep(payload = {}) {
  const row = document.createElement("details");
  row.className = "tool-step is-running";
  if (payload.id) row.dataset.toolId = String(payload.id);
  const summaryText = toolStepSummary(payload.tool, payload.args);
  const filePath = toolStepPath(payload.tool, payload.args);
  const summary = document.createElement("summary");
  const dot = document.createElement("span");
  dot.className = "tool-step-dot";
  dot.setAttribute("aria-hidden", "true");
  const label = document.createElement("span");
  label.className = "tool-step-label";
  label.textContent = toolLabel(payload.tool);
  summary.append(dot, label);
  if (summaryText) {
    if (filePath) {
      // 路径可点：直接交给编辑器打开。点击不冒泡，避免顺带把 chip 展开/收起。
      const link = document.createElement("button");
      link.type = "button";
      link.className = "tool-step-text path-link";
      link.dataset.path = filePath;
      link.title = `在编辑器中打开：${filePath}`;
      link.textContent = summaryText;
      link.addEventListener("click", (event) => {
        event.preventDefault();
        event.stopPropagation();
        openWorkspacePath(filePath);
      });
      summary.appendChild(link);
    } else {
      const text = document.createElement("span");
      text.className = "tool-step-text";
      text.title = summaryText;
      text.textContent = summaryText;
      summary.appendChild(text);
    }
  }
  const stateEl = document.createElement("span");
  stateEl.className = "tool-step-state";
  stateEl.textContent = "进行中";
  const chev = document.createElement("span");
  chev.className = "tool-step-chev";
  chev.setAttribute("aria-hidden", "true");
  chev.textContent = "⌄";
  summary.append(stateEl, chev);
  const body = document.createElement("div");
  body.className = "tool-step-body";
  const argsText =
    payload.args && typeof payload.args === "object" && Object.keys(payload.args).length
      ? JSON.stringify(payload.args, null, 2)
      : "";
  if (argsText) {
    const section = document.createElement("div");
    section.className = "tool-step-section";
    const caption = document.createElement("span");
    caption.className = "tool-step-caption";
    caption.textContent = "入参";
    const pre = document.createElement("pre");
    pre.textContent = argsText;
    section.append(caption, pre);
    body.appendChild(section);
  }
  const result = document.createElement("div");
  result.className = "tool-step-section tool-step-result hidden";
  const resultCaption = document.createElement("span");
  resultCaption.className = "tool-step-caption";
  resultCaption.textContent = "结果";
  const resultPre = document.createElement("pre");
  result.append(resultCaption, resultPre);
  body.appendChild(result);
  row.append(summary, body);
  // 结算：成功写结果预览（服务端 preview），失败写原因并自动展开（失败必须一眼可见）。
  const settle = (outcome = {}) => {
    const ok = outcome.ok !== false;
    row.classList.remove("is-running");
    row.classList.add(ok ? "is-ok" : "is-fail");
    stateEl.textContent = ok ? "完成" : "失败";
    const text = ok ? clipToolPreview(outcome.preview) : String(outcome.error || "未知原因");
    if (text) {
      resultPre.textContent = text;
      result.classList.remove("hidden");
      if (!ok) row.open = true;
    } else {
      result.remove();
    }
    if (!argsText && !text) body.remove();
    return ok;
  };
  return { row, body, result, resultPre, state: stateEl, settle };
}

function ensureToolRun() {
  if (state.toolRun) return state.toolRun;
  const el = document.createElement("div");
  el.className = "tool-steps";
  const head = document.createElement("div");
  head.className = "tool-steps-head";
  const list = document.createElement("div");
  list.className = "tool-step-list";
  el.append(head, list);
  newMessageBlock(el);
  state.toolRun = {
    el,
    head,
    list,
    total: 0,
    running: 0,
    failed: 0,
    steps: new Map(),
  };
  updateToolRun(state.toolRun);
  return state.toolRun;
}

function updateToolRun(run) {
  if (!run) return;
  const parts = [`已执行 ${run.total} 个工具`];
  if (run.running > 0) parts.push(`${run.running} 个进行中`);
  if (run.failed > 0) parts.push(`${run.failed} 个失败`);
  run.head.textContent = `⚙ ${parts.join(" · ")}`;
  run.head.classList.toggle("has-failure", run.failed > 0);
}

function pushToolUse(payload) {
  finishThinking();
  const run = ensureToolRun();
  run.total += 1;
  run.running += 1;
  const step = createToolStep(payload);
  run.list.appendChild(step.row);
  if (payload.id) run.steps.set(String(payload.id), step);
  if (state.turn) {
    state.turn.tools += 1;
    // 记录本回合改动的文件（diff 接口是会话累计，汇报卡按回合口径过滤）。
    const tool = payload.tool || "";
    const path = payload.args && (payload.args.path || payload.args.file_path);
    if (path && (tool === "write_file" || tool === "edit_file")) {
      state.turn.files.add(String(path));
    }
  }
  updateToolRun(run);
  followScroll();
}

function pushToolResult(payload) {
  const run = state.toolRun || ensureToolRun();
  const step = payload.id ? run.steps.get(String(payload.id)) : null;
  let ok;
  if (step) {
    ok = step.settle(payload);
    run.running = Math.max(0, run.running - 1);
  } else {
    // 没有配对 tool_use 的终态（权限被拒 / 插件热卸载 / 断线重连）：补一枚 chip，
    // 否则这一步在前端会永远停在「进行中」。
    run.total += 1;
    const fallback = createToolStep({ id: payload.id, tool: payload.tool });
    run.list.appendChild(fallback.row);
    ok = fallback.settle(payload);
  }
  if (!ok) run.failed += 1;
  updateToolRun(run);
  if (ok) return;
  if (state.turn) state.turn.failed += 1;
}

/// 历史回放：把 role=tool 的结果按 tool_call_id 配回 assistant 的 tool_calls，重建成同样的 chip。
function appendHistoryToolSteps(calls, resultsById) {
  const el = document.createElement("div");
  el.className = "tool-steps";
  const head = document.createElement("div");
  head.className = "tool-steps-head";
  const list = document.createElement("div");
  list.className = "tool-step-list";
  el.append(head, list);
  let failed = 0;
  for (const call of calls) {
    const step = createToolStep({ id: call.id, tool: call.name, args: call.arguments });
    const outcome = resultsById.get(String(call.id || ""));
    if (outcome) {
      const ok = step.settle(outcome);
      if (!ok) failed += 1;
    } else {
      step.row.classList.remove("is-running");
      step.state.textContent = "无结果";
    }
    list.appendChild(step.row);
  }
  head.textContent = `已执行 ${calls.length} 个工具${failed ? ` · ${failed} 个失败` : ""}`;
  head.classList.toggle("has-failure", failed > 0);
  newMessageBlock(el);
}

function resetRunBlocks() {
  state.thinking = null;
  state.toolRun = null;
}

// ---------- 回合汇报卡：每次调用结束后的工作总结（文件改动 + 耗时 + 消耗） ----------

function diffLineStats(diff) {
  const beforeLines = diff.before != null ? String(diff.before).split("\n") : [];
  const afterLines = diff.after != null ? String(diff.after).split("\n") : [];
  const maxLen = Math.max(beforeLines.length, afterLines.length);
  let added = 0;
  let removed = 0;
  for (let index = 0; index < maxLen; index += 1) {
    const before = index < beforeLines.length ? beforeLines[index] : null;
    const after = index < afterLines.length ? afterLines[index] : null;
    if (before === null) added += 1;
    else if (after === null) removed += 1;
    else if (before !== after) {
      removed += 1;
      added += 1;
    }
  }
  return { added, removed };
}

function fillTurnSummaryMeta(turn) {
  const card = turn && turn.card;
  if (!card) return;
  const server = turn.server || {};
  const wall = server.duration_ms
    ? Math.round(server.duration_ms / 1000)
    : Math.max(1, Math.round((Date.now() - turn.startedAt) / 1000));
  const parts = [`耗时 ${wall}s`, `工具 ${turn.tools} 次`];
  if (turn.stopped) parts.push("未完成");
  if (turn.modelCalls) parts.push(`模型调用 ${turn.modelCalls} 轮`);
  if (server.steps) parts.push(`共 ${server.steps} 步`);
  if (server.total_tokens) {
    const tokens =
      server.total_tokens >= 1000
        ? `${(server.total_tokens / 1000).toFixed(1)}k`
        : String(server.total_tokens);
    parts.push(`消耗 ${tokens} tokens`);
  }
  if (server.cost_usd > 0) parts.push(`$${Number(server.cost_usd).toFixed(4)}`);
  const meta = card.querySelector(".turn-summary-meta");
  if (meta) meta.textContent = parts.join(" · ");
}

// 路径归一：比较 diff 相对路径与工具入参路径（容忍 ./ 前缀、绝对/相对、分隔符差异）。
function samePath(left, right) {
  const norm = (value) =>
    String(value || "")
      .replace(/\\/g, "/")
      .replace(/^\.\//, "")
      .replace(/\/+$/, "")
      .toLowerCase();
  const a = norm(left);
  const b = norm(right);
  if (!a || !b) return false;
  return a === b || a.endsWith(`/${b}`) || b.endsWith(`/${a}`);
}

// 汇报卡：文件改动为本回合口径（本回合未记录到文件时退回会话累计并标注）。
async function renderTurnSummary(sessionId, turn) {
  if (!turn) return;
  const card = document.createElement("div");
  card.className = "turn-summary";
  card.innerHTML =
    '<div class="turn-summary-head">' +
    '<span class="turn-summary-icon">📄</span>' +
    '<strong class="turn-summary-files">读取改动…</strong>' +
    '<span class="turn-summary-chev">⌄</span>' +
    '<span class="turn-summary-deltas"></span>' +
    '<button type="button" class="turn-summary-open" title="在检查器中查看改动">↗</button>' +
    "</div>" +
    '<div class="turn-summary-list hidden"></div>' +
    '<div class="turn-summary-meta"></div>';
  writeBox().appendChild(card);
  followScroll();
  turn.card = card;

  const head = card.querySelector(".turn-summary-head");
  const list = card.querySelector(".turn-summary-list");
  head.addEventListener("click", (event) => {
    if (event.target.closest(".turn-summary-open")) return;
    list.classList.toggle("hidden");
  });
  card.querySelector(".turn-summary-open").addEventListener("click", () => {
    if (!document.body.classList.contains("inspector-open")) $("inspectorToggle").click();
    list.classList.remove("hidden");
  });

  fillTurnSummaryMeta(turn);
  try {
    const diffs = await api(`/session/${sessionId}/diff`);
    const all = (diffs || []).map((diff) => ({ path: diff.path, ...diffLineStats(diff) }));
    const touched = turn.files || new Set();
    const scoped = touched.size
      ? all.filter((file) => [...touched].some((path) => samePath(file.path, path)))
      : all;
    const cumulative = touched.size === 0 && all.length > 0;
    const addedTotal = scoped.reduce((sum, file) => sum + file.added, 0);
    const removedTotal = scoped.reduce((sum, file) => sum + file.removed, 0);
    card.querySelector(".turn-summary-files").textContent = scoped.length
      ? `${scoped.length} 个文件已更改${cumulative ? "（会话累计）" : ""}`
      : "本次没有文件改动";
    card.querySelector(".turn-summary-deltas").innerHTML =
      (addedTotal ? `<span class="plus">+${addedTotal}</span>` : "") +
      (removedTotal ? `<span class="minus">-${removedTotal}</span>` : "");
    if (scoped.length) {
      // 用 DOM 构建而非拼 HTML：路径里可能出现引号等字符，逐节点赋值才不会破坏结构；
      // 同时让每行路径可点，直接送编辑器打开。
      list.innerHTML = "";
      for (const file of scoped) {
        const row = document.createElement("div");
        row.className = "turn-summary-row";
        const link = document.createElement("button");
        link.type = "button";
        link.className = "path path-link";
        link.dataset.path = file.path;
        link.title = `在编辑器中打开：${file.path}`;
        link.textContent = file.path;
        link.addEventListener("click", (event) => {
          event.stopPropagation();
          openWorkspacePath(file.path);
        });
        const added = document.createElement("span");
        added.className = "plus";
        added.textContent = `+${file.added}`;
        const removed = document.createElement("span");
        removed.className = "minus";
        removed.textContent = `-${file.removed}`;
        row.append(link, added, removed);
        list.appendChild(row);
      }
    } else {
      card.querySelector(".turn-summary-chev").classList.add("hidden");
    }
  } catch (_) {
    card.querySelector(".turn-summary-files").textContent = "改动读取失败";
  }
}

function addMessage(kind, text, meta = "") {
  // 循环防护等「对模型的提醒」不该以对话气泡展示：转成折叠的时间线 chip。
  // （LOOP_NUDGE / LOOP_WRAP_UP / EMPTY_REPLY_RETRY 都以「（系统提示）」开头，
  // 历史里以 user 角色入库，这里统一折叠，不占对话流。）
  if (typeof text === "string" && text.startsWith("（系统提示）")) {
    return addEventChip(
      "loop",
      "循环防护提醒",
      text.replace(/^（系统提示）/, "")
    );
  }
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
  writeBox().appendChild(div);
  followScroll();
  // 有消息时隐藏中央空状态
  const empty = $("emptyState");
  if (empty) empty.classList.add("hidden");
  return div;
}

// ---------- 事件 chip：把「中断 / 审批拒绝 / 上下文压缩」写成时间线里的显式事件 ----------
// 对标 Codex：这些状态变化属于回合的一部分，不能只藏在 toast、title 或一行灰字里。

const EVENT_CHIP_ICONS = {
  stop: "■",
  deny: "⊘",
  compact: "⤓",
  fail: "!",
  info: "•",
};

function addEventChip(kind, text, detail = "") {
  const chip = document.createElement("details");
  chip.className = `event-chip kind-${kind}`;
  const summary = document.createElement("summary");
  summary.innerHTML =
    `<span class="event-chip-icon" aria-hidden="true">${EVENT_CHIP_ICONS[kind] || EVENT_CHIP_ICONS.info}</span>` +
    `<span class="event-chip-text">${esc(text)}</span>` +
    (detail ? '<span class="event-chip-chev" aria-hidden="true">⌄</span>' : "");
  chip.appendChild(summary);
  if (detail) {
    const body = document.createElement("div");
    body.className = "event-chip-body";
    body.textContent = detail;
    chip.appendChild(body);
  }
  writeBox().appendChild(chip);
  followScroll();
  const empty = $("emptyState");
  if (empty) empty.classList.add("hidden");
  return chip;
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

// ---------- 轻量 Markdown 渲染（对标 Codex 桌面：代码块/标题/列表/表格/行内样式） ----------

function escapeHtml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function escapeAttribute(text) {
  return escapeHtml(text).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

function safeMarkdownHref(raw) {
  const href = raw.replace(/&amp;/g, "&").trim();
  if (!href || /^(?:javascript|data|vbscript):/i.test(href)) return "";
  try {
    const url = new URL(href, window.location.href);
    if (["http:", "https:", "mailto:"].includes(url.protocol)) return url.href;
  } catch (_) {
    // 无法解析的链接按普通文本显示。
  }
  return /^(?:\.|\/|#)/.test(href) ? href : "";
}

function inlineMarkdown(text) {
  let out = escapeHtml(text);
  out = out.replace(/`([^`]+)`/g, "<code>$1</code>");
  out = out.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  out = out.replace(/\*([^*]+)\*/g, "<em>$1</em>");
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (match, label, rawHref) => {
    const href = safeMarkdownHref(rawHref);
    return href
      ? `<a href="${escapeAttribute(href)}" target="_blank" rel="noopener">${label}</a>`
      : label;
  });
  return out;
}

// 把 markdown 文本渲染为 HTML。代码块保留原样（pre/code），行内元素转义。
function renderMarkdown(text) {
  if (!text) return "";
  const lines = text.split("\n");
  const html = [];
  let inCode = false;
  let codeLang = "";
  let codeLines = [];
  let inList = false;
  let inTable = false;
  let tableHeader = null;
  let tableAlign = null;

  const flushCode = () => {
    if (codeLines.length) {
      html.push(
        `<pre class="md-code"><div class="md-code-head"><span>${escapeHtml(codeLang || "code")}</span><button class="md-copy" data-code="${encodeURIComponent(codeLines.join("\n"))}">复制</button></div><code>${escapeHtml(codeLines.join("\n"))}</code></pre>`
      );
      codeLines = [];
    }
    inCode = false;
    codeLang = "";
  };
  const flushList = () => {
    if (inList) {
      html.push("</ul>");
      inList = false;
    }
  };
  const flushTable = () => {
    if (inTable) {
      html.push("</table>");
      inTable = false;
    }
    tableHeader = null;
    tableAlign = null;
  };

  for (const line of lines) {
    const fence = line.match(/^```(\w*)\s*$/);
    if (fence) {
      if (inCode) flushCode();
      else {
        flushList();
        flushTable();
        inCode = true;
        codeLang = fence[1];
      }
      continue;
    }
    if (inCode) {
      codeLines.push(line);
      continue;
    }
    if (/^\s*$/.test(line)) {
      flushList();
      flushTable();
      html.push("");
      continue;
    }
    const heading = line.match(/^(#{1,4})\s+(.*)$/);
    if (heading) {
      flushList();
      flushTable();
      const level = heading[1].length;
      html.push(`<h${level} class="md-h${level}">${inlineMarkdown(heading[2])}</h${level}>`);
      continue;
    }
    const hr = line.match(/^\s*(-{3,}|\*{3,})\s*$/);
    if (hr) {
      flushList();
      flushTable();
      html.push('<hr class="md-hr">');
      continue;
    }
    const li = line.match(/^\s*[-*+]\s+(.*)$/) || line.match(/^\s*\d+\.\s+(.*)$/);
    if (li) {
      flushTable();
      if (!inList) {
        html.push("<ul class=\"md-list\">");
        inList = true;
      }
      html.push(`<li>${inlineMarkdown(li[1])}</li>`);
      continue;
    }
    flushList();
    const tableLine = line.match(/^\|?\s*(.*?)\s*\|?$/);
    const cells = line.split("|").slice(1, -1);
    const allCells = line.split("|").filter((cell) => cell.trim() !== "");
    if (allCells.length > 1 && !tableHeader) {
      tableHeader = allCells.map((cell) => cell.trim());
      inTable = true;
      html.push('<table class="md-table"><thead><tr>');
      for (const cell of tableHeader) {
        html.push(`<th>${inlineMarkdown(cell)}</th>`);
      }
      html.push("</tr></thead><tbody>");
      continue;
    }
    if (inTable) {
      if (tableHeader && allCells.every((cell) => /^:?-{2,}:?$/.test(cell.trim()))) {
        tableAlign = allCells.map((cell) => cell.trim());
        continue;
      }
      if (allCells.length) {
        html.push("<tr>");
        for (let index = 0; index < allCells.length; index++) {
          html.push(`<td>${inlineMarkdown(allCells[index])}</td>`);
        }
        html.push("</tr>");
        continue;
      }
      flushTable();
      tableHeader = null;
    }
    html.push(`<div class="md-p">${inlineMarkdown(line)}</div>`);
  }
  flushCode();
  flushList();
  flushTable();
  return html.join("\n");
}

// ---------- 头部状态 ----------

async function refreshHealth() {
  try {
    const health = await api("/health");
    $("health").textContent = `API 就绪 ${health.version}`;
    $("health").style.color = "var(--green)";
    const bar = $("menubarHealth");
    if (bar) {
      bar.textContent = "服务已连接";
      bar.style.color = "var(--green)";
      bar.style.background = "var(--green-soft)";
      bar.style.borderColor = "transparent";
    }
  } catch (error) {
    $("health").textContent = "本地服务未连接";
    $("health").style.color = "var(--yellow)";
    const bar = $("menubarHealth");
    if (bar) {
      bar.textContent = "服务未连接";
      bar.style.color = "var(--yellow)";
      bar.style.background = "var(--yellow-soft)";
      bar.style.borderColor = "transparent";
    }
  }
}

async function refreshPerception() {
  try {
    const snapshot = await api("/context/snapshot");
    $("permission").textContent = `感知：${snapshot.permission_level || "l0_l1"}`;
    const level = snapshot.permission_level || "l0_l1";
    const actions = Array.isArray(snapshot.recent_actions) && snapshot.recent_actions.length
      ? snapshot.recent_actions.join("、")
      : "暂无近期操作";
    $("snapshot").textContent = `权限等级：${level}\n近期操作：${actions}`;
  } catch (_) {
    $("snapshot").textContent = "（无法获取情景快照）";
  }
}

async function refreshLearn() {
  try {
    const status = await api("/learn/status");
    $("learnState").textContent = `学习：${status.state}（${status.samples}）`;
  } catch (_) {
    $("learnState").textContent = "学习：—";
  }
}

async function learnControl(action) {
  try {
    await api(`/learn/${action}`, { method: "POST" });
    await refreshLearn();
  } catch (error) {
    addMessage("error", `学习控制失败：${error.message}`);
  }
}

async function sinkSkill() {
  const name = $("sinkName").value.trim();
  const apps = $("sinkApps").value.split(",").map((item) => item.trim()).filter(Boolean);
  const sensitivity = $("sinkSensitivity").value;
  const description = $("sinkDesc").value.trim();
  if (!name || !apps.length) return;
  try {
    const result = await api("/learn/sink", {
      method: "POST",
      body: JSON.stringify({ name, target_apps: apps, sensitivity, description }),
    });
    $("sinkName").value = "";
    $("sinkDesc").value = "";
    addMessage("system", `已沉淀技能包 ${result.name}（变量：${result.variables.join(", ") || "无"}）`);
    await refreshPackages();
  } catch (error) {
    addMessage("error", `沉淀失败：${error.message}`);
  }
}

// 插件页（Codex 风格）：已安装网格 + 热门推荐列表。
// 防御：面板元素不存在（视图切换中）时直接跳过，避免竞态报错。
async function refreshPlugins() {
  const grid = $("pluginGrid");
  if (!grid) return;
  try {
    const data = await api("/plugins");
    const plugins = data.plugins || [];
    const count = $("pluginCount");
    if (count) count.textContent = plugins.length ? `${plugins.length} 个` : "暂无";
    grid.innerHTML = "";
    for (const plugin of plugins) grid.appendChild(pluginCard(plugin));
    if (!plugins.length) grid.innerHTML = '<div class="sub">尚未安装插件</div>';
  } catch (error) {
    grid.innerHTML = `<div class="sub">${esc(friendlyError(error))}</div>`;
  }
}

function pluginCard(plugin) {
  const enabled = plugin.enabled !== false;
  const card = document.createElement("div");
  card.className = "ps-card";
  const initial = esc((plugin.name || plugin.id || "?").trim().slice(0, 1).toUpperCase());
  const mcp = plugin.mcp
    ? `${esc(plugin.mcp.transport)}｜${esc(plugin.mcp.command)}`
    : "未配置 MCP 服务器";
  card.innerHTML =
    '<div class="ps-card-top">' +
    `<span class="ps-tile" aria-hidden="true">${initial}</span>` +
    '<div class="ps-card-meta">' +
    `<strong>${esc(plugin.name)}</strong>` +
    `<span class="sub">v${esc(plugin.version)} ｜ ${esc(plugin.id)}</span>` +
    "</div>" +
    (enabled ? '<span class="ps-check" title="已启用">✓</span>' : "") +
    "</div>" +
    `<p class="ps-card-desc">${esc(plugin.description || "暂无描述")}</p>` +
    '<div class="ps-chips">' +
    `<span class="ps-chip">${enabled ? "已启用" : "已禁用"}</span>` +
    `<span class="ps-chip">${esc((plugin.permissions || []).join("、") || "无权限声明")}</span>` +
    "</div>" +
    '<div class="ps-card-foot">' +
    `<span class="sub" title="${mcp}">${mcp}</span>` +
    `<button type="button" class="ps-card-btn">${enabled ? "禁用" : "启用"}</button>` +
    "</div>";
  card.querySelector("button").addEventListener("click", async () => {
    try {
      await api(`/plugins/${encodeURIComponent(plugin.id)}/enabled`, {
        method: "POST",
        body: JSON.stringify({ enabled: !enabled }),
      });
      showToast(`插件 ${plugin.name} 已${enabled ? "禁用" : "启用"}`);
      await refreshPlugins();
    } catch (error) {
      showToast(`切换失败：${friendlyError(error)}`);
    }
  });
  return card;
}

// 热门推荐：来自市场目录（source=market，尚未本地安装），支持一键安装。
async function refreshPluginMarket() {
  const box = $("pluginPopular");
  if (!box) return;
  try {
    const data = await api("/plugins/market");
    const entries = (data.plugins || []).filter((item) => item.source === "market");
    box.innerHTML = "";
    if (!entries.length) {
      box.innerHTML = '<div class="sub">市场目录为空，可在「扩展面板 → 插件市场」中刷新远端目录</div>';
      return;
    }
    for (const entry of entries) {
      const row = document.createElement("div");
      row.className = "ps-row";
      const initial = esc((entry.name || entry.id || "?").trim().slice(0, 1).toUpperCase());
      row.innerHTML =
        `<span class="ps-tile ps-tile-sm" aria-hidden="true">${initial}</span>` +
        '<div class="ps-row-meta">' +
        `<strong>${esc(entry.name || entry.id)}</strong>` +
        `<span class="sub">v${esc(entry.version || "?")} ｜ 最低支持 App ${esc(entry.description || "—")}</span>` +
        "</div>";
      const button = document.createElement("button");
      button.type = "button";
      button.className = "ps-install";
      button.textContent = "安装";
      button.addEventListener("click", () => {
        confirmModal({
          title: "安装插件",
          message: `从市场安装 ${entry.name || entry.id}${entry.version ? " v" + entry.version : ""}？安装时会进行签名校验。`,
          confirmText: "安装",
          onConfirm: async () => {
            try {
              await api("/plugins/market/install-remote", {
                method: "POST",
                body: JSON.stringify({ id: entry.id, version: entry.version || undefined }),
              });
              showToast(`已安装 ${entry.name || entry.id}`);
              await refreshPluginMarket();
              await refreshPlugins();
            } catch (error) {
              showToast(`安装失败：${friendlyError(error)}`);
            }
          },
        });
      });
      row.appendChild(button);
      box.appendChild(row);
    }
  } catch (error) {
    box.innerHTML = `<div class="sub">${esc(friendlyError(error))}</div>`;
  }
}

async function refreshPackages() {
  try {
    const packages = await api("/learn/packages");
    const list = $("packageList");
    list.innerHTML = "";
    for (const pkg of packages) {
      const li = document.createElement("li");
      const health = pkg.health || "active";
      li.innerHTML = `<strong>${esc(pkg.name)}</strong><span class="sub">健康：${esc(health)} ｜ 目标：${esc(pkg.target_apps.join(","))} ｜ 变量：${esc(pkg.variables.join(",")) || "无"}</span>`;
      li.addEventListener("click", () => executePackage(pkg));
      if (health === "degraded") {
        const resetBtn = document.createElement("button");
        resetBtn.textContent = "重置健康";
        resetBtn.addEventListener("click", async (event) => {
          event.stopPropagation();
          try {
            await api(`/skills/health/${encodeURIComponent(pkg.name)}/reset`, {
              method: "POST",
            });
            await refreshPackages();
            addMessage("system", `已重置 ${pkg.name} 健康度`);
          } catch (error) {
            addMessage("system", `重置失败：${error.message || error}`);
          }
        });
        li.appendChild(resetBtn);
      }
      const exportBtn = document.createElement("button");
      exportBtn.textContent = "导出";
      exportBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        await exportPackage(pkg.name);
      });
      li.appendChild(exportBtn);
      const detailBtn = document.createElement("button");
      detailBtn.textContent = "查看";
      detailBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          const detail = await api(`/learn/packages/${encodeURIComponent(pkg.name)}`);
          $("packageDetail").textContent = JSON.stringify(detail, null, 2);
        } catch (error) {
          addMessage("system", `查看失败：${friendlyError(error, { resource: true })}`);
        }
      });
      li.appendChild(detailBtn);
      const deleteBtn = document.createElement("button");
      deleteBtn.textContent = "删除";
      deleteBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        confirmModal({
          title: "删除流程技能包",
          message: `删除后无法恢复，确认删除「${pkg.name}」？`,
          confirmText: "删除",
          kind: "danger",
          onConfirm: async () => {
            try {
              await api(`/learn/packages/${encodeURIComponent(pkg.name)}`, { method: "DELETE" });
              await refreshPackages();
              showToast(`已删除流程技能包 ${pkg.name}`, "ok");
            } catch (error) {
              showToast(`删除失败：${friendlyError(error, { resource: true })}`, "error");
            }
          },
        });
      });
      li.appendChild(deleteBtn);
      list.appendChild(li);
    }
    if (!packages.length) list.innerHTML = '<li class="sub">暂无流程技能包（先录制再沉淀）</li>';
  } catch (error) {
    $("packageList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function exportPackage(name) {
  try {
    const response = await apiRaw(`/learn/export/${encodeURIComponent(name)}`);
    if (!response.ok) throw new Error(await response.text());
    const blob = await response.blob();
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `${name}.owskill`;
    link.click();
    URL.revokeObjectURL(url);
  } catch (error) {
    addMessage("error", `导出失败：${friendlyError(error, { resource: true })}`);
  }
}

async function importPackage(file) {
  try {
    const response = await apiRaw("/learn/import", {
      method: "POST",
      headers: { "Content-Type": "application/zip" },
      body: file,
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || response.statusText);
    showToast(`已导入技能包 ${result.name}`, "ok");
    await refreshPackages();
  } catch (error) {
    showToast(`导入失败：${error.message}`, "error");
  }
}

// 自动化 UI（表单/任务列表/提醒）已迁入扩展面板 panels/automations.panel.js，
// 此处的零散函数与全局轮询一并移除（面板自带挂载期轮询）。

async function refreshSettings() {
  try {
    const settings = await api("/settings");
    state.settings = settings;
    // 自定义模型只存本机，服务端回读不携带该字段
    state.settings.custom_models = loadCustomModels();
    const cloudEnabled = !!(settings.egress && settings.egress.cloud_enabled);
    const toggle = $("egressToggle");
    toggle.classList.toggle("on", cloudEnabled);
    toggle.setAttribute("aria-checked", String(cloudEnabled));
    toggle.dataset.enabled = String(cloudEnabled);
    const model = settings.model || "qwen3.8-max";
    const modelSelect = $("settingsModel");
    if (modelSelect.querySelector(`option[value="${CSS.escape(model)}"]`)) {
      modelSelect.value = model;
    } else {
      // 当前模型不在候选里（如端点实际在用的模型名不在预设中）：
      // 补一个选项并选中，否则下拉回落 HTML 默认值，
      // 保存模型接入时会把这个错误值写回 settings.json。
      const option = document.createElement("option");
      option.value = model;
      option.textContent = model;
      modelSelect.appendChild(option);
      modelSelect.value = model;
    }
    $("modelChipText").textContent = settings.model || "默认模型";
    renderEffortChip();
    $("connectionSummary").textContent = cloudEnabled ? "云端模型已启用" : "云端模型已关闭";
    // 主动建议（个性化）
    const proactive = !!(settings.proactive && settings.proactive.enabled);
    const proactiveToggle = $("prefProactive");
    if (proactiveToggle) {
      proactiveToggle.classList.toggle("on", proactive);
      proactiveToggle.setAttribute("aria-checked", String(proactive));
    }
    syncLocalPrefs();
    renderCustomModels();
    renderProviderForm();
    updateModelGate();
  } catch (error) {
    const summary = $("connectionSummary");
    if (summary) summary.textContent = friendlyError(error);
  }
}

// 本地偏好（仅存 localStorage，不进入服务端设置）
const LOCAL_PREFS = {
  fileOpener: "system",
  shell: "powershell",
  language: "zh-CN",
  compact: false,
  speed: "standard",
  theme: "light",
  speechLang: "zh-CN",
  tone: "balanced",
  reminder: true,
  sound: false,
  approvalPin: true,
};
function loadLocalPrefs() {
  try {
    Object.assign(LOCAL_PREFS, JSON.parse(localStorage.getItem("owo.prefs") || "{}"));
  } catch (_) {
    // 本地偏好损坏时回退默认值
  }
  return LOCAL_PREFS;
}
function saveLocalPrefs() {
  localStorage.setItem("owo.prefs", JSON.stringify(LOCAL_PREFS));
}
function applyLocalPrefs() {
  document.body.classList.toggle("compact-mode", LOCAL_PREFS.compact);
  const recognitionLang = LOCAL_PREFS.speechLang;
  if (recognition) recognition.lang = recognitionLang;
  $("defaultWorkspacePath").textContent =
    localStorage.getItem("owo.workspace") || "未设置";
}
function syncLocalPrefs() {
  const prefs = loadLocalPrefs();
  const setSelect = (id, value) => {
    const el = $(id);
    if (el) el.value = value;
  };
  setSelect("prefFileOpener", prefs.fileOpener);
  setSelect("prefShell", prefs.shell);
  setSelect("prefLanguage", prefs.language);
  setSelect("prefSpeed", prefs.speed);
  setSelect("prefSpeechLang", prefs.speechLang);
  setSelect("prefTone", prefs.tone);
  setSelect("prefTheme", prefs.theme);
  setToggle("prefCompact", prefs.compact);
  setToggle("prefCompactAppearance", prefs.compact);
  setToggle("prefReminder", prefs.reminder);
  setToggle("prefSound", prefs.sound);
  setToggle("prefApprovalPin", prefs.approvalPin);
}

// ---------- 桌面桌宠（A8-3：经引擎通道控制桌面端挂件） ----------
// 工作台不再内嵌自己的桌宠（会与桌面端悬浮桌宠叠加成「两只」）；这里的开关
// 直接控制桌面端（LingXi overlay）的桌宠：写期望值 → 桌面端心跳应用 → 回读实际值。

/// 开关按钮状态（`.on` + aria-checked）；本地偏好与引擎驱动的开关共用。
function setToggle(id, value) {
  const el = $(id);
  if (!el) return;
  el.classList.toggle("on", !!value);
  el.setAttribute("aria-checked", String(!!value));
}

let petLinkState = {
  desired: null,
  actual: null,
  desiredAt: 0,
  actualAt: 0,
  overlayOnline: false,
  effective: null,
};

async function refreshPetState() {
  try {
    const data = await api("/desktop/pet");
    petLinkState = {
      desired: typeof data.desired === "boolean" ? data.desired : null,
      actual: typeof data.actual === "boolean" ? data.actual : null,
      desiredAt: Date.parse(data.desired_at || "") || 0,
      actualAt: Date.parse(data.actual_at || "") || 0,
      overlayOnline: !!data.overlay_online,
      effective: null,
    };
  } catch (_) {
    petLinkState = {
      desired: null,
      actual: null,
      desiredAt: 0,
      actualAt: 0,
      overlayOnline: false,
      effective: null,
    };
  }
  // 桌面端在线时取「较新的一端」为准：刚写入期望值时先显示期望，
  // 等桌面端心跳追上来（实际值时间戳更新）再以实际值为准。
  let effective = petLinkState.desired;
  if (petLinkState.overlayOnline && petLinkState.actual !== null) {
    effective =
      petLinkState.desiredAt > petLinkState.actualAt
        ? petLinkState.desired
        : petLinkState.actual;
  }
  petLinkState.effective = effective;
  setToggle("prefPet", effective === true);
  const note = $("petLinkNote");
  if (note) {
    note.textContent = petLinkState.overlayOnline
      ? effective === false
        ? "桌面端在线 · 桌宠已隐藏"
        : "桌面端在线 · 桌宠显示中"
      : "桌面端未运行：开关会在它启动后生效";
  }
}

/// 切换桌面桌宠显隐（写期望值；桌面端约 2.5 秒内跟进）。
async function toggleDesktopPet() {
  await refreshPetState();
  const next = !(petLinkState.effective === true);
  // 乐观更新：先把开关拨过去，网络失败时由 refreshPetState 纠正。
  setToggle("prefPet", next);
  try {
    await api("/desktop/pet", {
      method: "POST",
      body: JSON.stringify({ visible: next }),
    });
    showToast(next ? "已请求显示桌面桌宠" : "已请求隐藏桌面桌宠", "ok");
  } catch (error) {
    showToast(`桌宠切换失败：${friendlyError(error, { resource: true })}`, "error");
  }
  await refreshPetState();
}

// ---------- 设置页交互：分类导航 / 模态框 / 自定义模型 / 本地偏好 ----------

// 左侧分类 ↔ 右侧表单页
function setSettingsTab(name) {
  for (const item of document.querySelectorAll(".settings-nav-item")) {
    item.classList.toggle("active", item.dataset.settingsTab === name);
  }
  for (const tab of document.querySelectorAll(".settings-tab")) {
    tab.hidden = tab.dataset.settingsTab !== name;
  }
}
for (const item of document.querySelectorAll(".settings-nav-item")) {
  item.addEventListener("click", () => setSettingsTab(item.dataset.settingsTab));
}

// 通用模态框（替代 alert / confirm / prompt）
const modalRoot = $("modalRoot");
let modalOnClose = null;
function openModal({ title, body, actions = [], onClose = null }) {
  $("modalTitle").textContent = title || "";
  const bodyEl = $("modalBody");
  bodyEl.innerHTML = "";
  if (typeof body === "string") bodyEl.innerHTML = body;
  else if (body) bodyEl.appendChild(body);
  const foot = $("modalFoot");
  foot.innerHTML = "";
  for (const action of actions) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = action.label;
    if (action.kind) button.className = action.kind;
    button.addEventListener("click", () =>
      action.onClick && action.onClick({ close: closeModal, body: bodyEl })
    );
    foot.appendChild(button);
  }
  modalOnClose = onClose;
  modalRoot.classList.remove("hidden");
  const focusTarget = bodyEl.querySelector("input,select,textarea");
  if (focusTarget) focusTarget.focus();
}
function closeModal() {
  modalRoot.classList.add("hidden");
  const onClose = modalOnClose;
  modalOnClose = null;
  if (onClose) onClose();
}
function confirmModal({ title, message, confirmText = "确定", cancelText = "取消", kind = "", onConfirm, onCancel = null }) {
  // done 标记确认路径，避免「确认后 close()」又被 onClose 当成取消回调一次。
  let done = false;
  openModal({
    title,
    body: `<p class="modal-message">${esc(message)}</p>`,
    actions: [
      { label: cancelText, kind: "ghost", onClick: ({ close }) => close() },
      {
        label: confirmText,
        kind: kind === "danger" ? "danger" : "primary",
        onClick: async ({ close }) => {
          done = true;
          close();
          if (onConfirm) await onConfirm();
        },
      },
    ],
    onClose: () => {
      if (!done && onCancel) onCancel();
    },
  });
}
/// 单行文本输入弹窗：替代 window.prompt（原生弹窗无法样式化、会阻塞渲染）。
function promptModal({ title, label, value = "", placeholder = "", confirmText = "确定", onConfirm, onCancel = null }) {
  let done = false;
  openModal({
    title,
    body:
      `<p class="modal-message">${esc(label)}</p>` +
      `<input id="modalPromptInput" type="text" value="${esc(value)}" placeholder="${esc(placeholder)}" />`,
    actions: [
      { label: "取消", kind: "ghost", onClick: ({ close }) => close() },
      {
        label: confirmText,
        kind: "primary",
        onClick: ({ close, body }) => {
          const input = body.querySelector("#modalPromptInput");
          const text = input ? input.value.trim() : "";
          done = true;
          close();
          if (onConfirm) onConfirm(text);
        },
      },
    ],
    onClose: () => {
      if (!done && onCancel) onCancel();
    },
  });
  const input = $("modalPromptInput");
  if (input) {
    input.addEventListener("keydown", (event) => {
      if (event.key !== "Enter") return;
      event.preventDefault();
      const text = input.value.trim();
      done = true;
      closeModal();
      if (onConfirm) onConfirm(text);
    });
  }
}
// Promise 版封装：给「先弹窗、后继续」的线性流程用（取消返回 null / false）。
function askConfirm(options) {
  return new Promise((resolve) => {
    confirmModal({ ...options, onConfirm: () => resolve(true), onCancel: () => resolve(false) });
  });
}
function askText(options) {
  return new Promise((resolve) => {
    promptModal({ ...options, onConfirm: (text) => resolve(text), onCancel: () => resolve(null) });
  });
}
$("modalMask").addEventListener("click", closeModal);
$("modalClose").addEventListener("click", closeModal);
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    closeComposerMenu();
    if (!modalRoot.classList.contains("hidden")) closeModal();
  }
});

// ---------- Composer 交互：模型选择 / 访问级别下拉、发送-中断双态按钮 ----------

const SEND_SVG =
  '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 12.4V4.2M4.6 7.6 8 4.2l3.4 3.4"/></svg>';
const STOP_SVG =
  '<svg viewBox="0 0 16 16" width="12" height="12" fill="currentColor"><rect x="4" y="4" width="8" height="8" rx="1.4"/></svg>';
const MIC_SVG =
  '<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="2.2" width="4" height="7.2" rx="2"/><path d="M3.8 7.8a4.2 4.2 0 0 0 8.4 0M8 12v1.8"/></svg>';
const MIC_REC_SVG =
  '<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="8" cy="8" r="5"/><circle cx="8" cy="8" r="1.8" fill="currentColor" stroke="none"/></svg>';

/// 当前视图会话的活跃回合（并行回合按会话隔离；本会话未运行则为 null）。
function currentTurn() {
  return (state.sessionId && state.activeTurns.get(state.sessionId)) || null;
}

// 回合进行中：发送钮变为中断钮（Codex 行为）——按「当前会话是否在跑」判定。
// 此前用全局 `state.reading`：A 会话在跑时切到 B，B 的发送钮也显示成停止，
// 点下去会把 A 的回合掐掉（并行对话被打断的根因）。
function updateComposerRunning() {
  const running = !!currentTurn();
  const button = $("sendBtn");
  if (!button) return;
  button.classList.toggle("is-stop", running);
  button.innerHTML = running ? STOP_SVG : SEND_SVG;
  button.title = running ? "中断当前回合" : "发送";
}
function setMicRecording(on) {
  const button = $("micBtn");
  if (!button) return;
  button.classList.toggle("recording", on);
  button.innerHTML = on ? MIC_REC_SVG : MIC_SVG;
  button.title = on ? "停止录音" : "语音输入";
}
function abortTurn() {
  // 只中断「当前视图会话」的回合：**绝不**回落到全局 controller
  // （曾导致：在 B 会话点停止 → 把仍在运行的 A 会话回合掐掉）。
  const turn = currentTurn();
  if (!turn || !state.sessionId) return;
  // 主动中断后 SSE 会断开、收不到 user_answered：先把未答提问卡显式作废。
  settlePendingQuestion("aborted");
  api(`/session/${state.sessionId}/abort`, { method: "POST" }).catch(() => {
    // 服务端可能已结束回合，忽略
  });
  if (turn.controller) turn.controller.abort();
}

// ---------- 运行状态条：转圈 + 阶段文案 + 计时（阻塞态一眼可见） ----------

const RUN_PHASE_DEFAULT = {
  thinking: "正在思考…",
  reasoning: "深度思考中…",
  answering: "正在生成回答…",
  waiting: "等待你的审批",
  asking: "等待你的回答",
};

function startRunStatus() {
  const el = $("runStatus");
  if (!el) return;
  state.runStartedAt = Date.now();
  el.classList.remove("hidden", "is-waiting");
  el.classList.add("is-live");
  $("runStatusText").textContent = RUN_PHASE_DEFAULT.thinking;
  $("runStatusTimer").textContent = "0s";
  if (state.runTicker) clearInterval(state.runTicker);
  state.runTicker = setInterval(() => {
    const timer = $("runStatusTimer");
    if (!timer) return;
    const seconds = Math.round((Date.now() - state.runStartedAt) / 1000);
    timer.textContent = `${seconds}s`;
  }, 1000);
}

// 阶段切换：文案可覆盖（如带工具名），等待审批为黄色阻塞态。
function setRunPhase(phase, text) {
  const el = $("runStatus");
  if (!el || el.classList.contains("hidden")) return;
  el.classList.toggle("is-waiting", phase === "waiting");
  const label = $("runStatusText");
  const next = text || RUN_PHASE_DEFAULT[phase];
  if (label && next) label.textContent = next;
}

function stopRunStatus() {
  if (state.runTicker) {
    clearInterval(state.runTicker);
    state.runTicker = null;
  }
  const el = $("runStatus");
  if (el) {
    el.classList.add("hidden");
    el.classList.remove("is-live", "is-waiting");
  }
}

// 回合失败卡：每回合都必须有明确终态（失败原因 + 汇报卡），不能静默停住。
function showTurnFailure(message) {
  finishThinking();
  settlePendingQuestion("aborted");
  const run = state.toolRun;
  if (run && run.running > 0) {
    run.running = 0;
    updateToolRun(run);
  }
  addMessage("error", `回合未完成：${message}`);
  const turn = state.turn;
  // 只有真的跑过（有模型调用/工具）才补汇报卡；请求级失败（如鉴权）不打扰。
  if (turn && (turn.tools > 0 || turn.modelCalls > 0)) {
    turn.stopped = true;
    renderTurnSummary(state.sessionId, turn);
  }
}

// ---------- composer 通用下拉浮层（模型 / 访问级别共用） ----------

let composerMenuEl = null;
function closeComposerMenu() {
  if (composerMenuEl) {
    composerMenuEl.remove();
    composerMenuEl = null;
  }
  document.removeEventListener("pointerdown", onComposerMenuOutside, true);
}
function onComposerMenuOutside(event) {
  if (!composerMenuEl) return;
  if (composerMenuEl.contains(event.target)) return;
  const trigger = event.target.closest && event.target.closest("#modelChip,#accessChip");
  if (trigger) return;
  // 命令补全依附于输入框：在输入框内点击不应关闭
  if (composerMenuEl.dataset.owner === "slash" && event.target.closest("#prompt")) return;
  closeComposerMenu();
}
function openComposerMenu(trigger, html, bind) {
  closeComposerMenu();
  const menu = document.createElement("div");
  menu.className = "menu-popover composer-menu";
  menu.setAttribute("role", "menu");
  menu.innerHTML = html;
  document.body.appendChild(menu);
  const rect = trigger.getBoundingClientRect();
  const box = menu.getBoundingClientRect();
  let left = rect.right - box.width;
  if (left < 12) left = 12;
  if (left + box.width > window.innerWidth - 12) left = window.innerWidth - box.width - 12;
  let top = rect.top - box.height - 8;
  if (top < 48) top = rect.bottom + 8;
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
  composerMenuEl = menu;
  if (bind) bind(menu);
  document.addEventListener("pointerdown", onComposerMenuOutside, true);
}

// ---------- 模型选择下拉 ----------

function openModelMenu() {
  const current = (state.settings && state.settings.model) || "";
  const preset = [];
  const custom = [];
  for (const option of $("settingsModel").options) {
    const item = { id: option.value, label: option.textContent };
    if (option.dataset.custom === "1") custom.push(item);
    else preset.push(item);
  }
  let html = '<div class="composer-menu-title">选择模型</div>';
  const renderGroup = (label, list) => {
    if (!list.length) return "";
    let out = `<div class="composer-menu-group">${esc(label)}</div>`;
    for (const item of list) {
      const active = item.id === current ? " active" : "";
      const check = item.id === current ? '<span class="composer-menu-check">✓</span>' : "";
      out += `<button type="button" class="composer-menu-item${active}" data-model="${esc(item.id)}"><span>${esc(item.label)}</span>${check}</button>`;
    }
    return out;
  };
  html += renderGroup("可用模型", preset);
  html += renderGroup("自定义模型", custom);
  html +=
    '<div class="composer-menu-foot"><button type="button" class="composer-menu-link" data-go-settings>管理模型与预设…</button></div>';
  openComposerMenu($("modelChip"), html, (menu) => {
    for (const button of menu.querySelectorAll("[data-model]")) {
      button.addEventListener("click", async () => {
        const id = button.dataset.model;
        closeComposerMenu();
        await selectModel(id);
      });
    }
    const go = menu.querySelector("[data-go-settings]");
    if (go) {
      go.addEventListener("click", () => {
        closeComposerMenu();
        setSettingsPageVisible(true);
        setSettingsTab("models");
      });
    }
  });
}

async function selectModel(id) {
  const select = $("settingsModel");
  if (!select.querySelector(`option[value="${CSS.escape(id)}"]`)) {
    const option = document.createElement("option");
    option.value = id;
    option.textContent = id;
    select.appendChild(option);
  }
  select.value = id;
  state.settings = state.settings || {};
  state.settings.model = id;
  $("modelChipText").textContent = id;
  const result = await saveSettings();
  if (result.ok) showToast(`已切换模型：${id}`, "ok");
}

// ---------- 推理档位（reasoning_effort）：对标 Codex 输入框里的「模型 + 档位」 ----------
// 默认值不下发该参数：不支持 reasoning_effort 的 OpenAI 兼容端点遇到未知字段可能直接 400，
// 因此只有用户显式选择档位时才写进请求体（后端 settings → OWO_REASONING_EFFORT → 请求体）。

const EFFORT_MODES = [
  { id: "", label: "默认", desc: "跟随模型自身默认，不发送 reasoning_effort" },
  { id: "minimal", label: "极简", desc: "最少思考、最快返回（仅部分模型支持）" },
  { id: "low", label: "低", desc: "轻度思考，偏向速度" },
  { id: "medium", label: "中", desc: "速度与深度平衡" },
  { id: "high", label: "高", desc: "尽量深入思考，更慢也更贵" },
];

function getEffort() {
  const raw = (state.settings && state.settings.reasoning_effort) || "";
  const id = String(raw).trim().toLowerCase();
  return EFFORT_MODES.some((mode) => mode.id === id) ? id : "";
}

function renderEffortChip() {
  const mode = EFFORT_MODES.find((item) => item.id === getEffort()) || EFFORT_MODES[0];
  const text = $("effortChipText");
  const chip = $("effortChip");
  if (text) text.textContent = mode.id ? `推理 ${mode.label}` : "推理默认";
  if (chip) {
    chip.dataset.effort = mode.id;
    chip.title = `推理档位：${mode.label}（${mode.desc}）`;
  }
}

async function selectEffort(id) {
  state.settings = state.settings || {};
  state.settings.reasoning_effort = id || null;
  renderEffortChip();
  const result = await saveSettings({ silent: true });
  const mode = EFFORT_MODES.find((item) => item.id === id) || EFFORT_MODES[0];
  if (result.ok) showToast(`推理档位：${mode.label}`, "ok");
}

function openEffortMenu() {
  const current = getEffort();
  let html = '<div class="composer-menu-title">推理档位（thinking effort）</div>';
  for (const mode of EFFORT_MODES) {
    const active = mode.id === current ? " active" : "";
    const check = mode.id === current ? '<span class="composer-menu-check">✓</span>' : "";
    html +=
      `<button type="button" class="composer-menu-item has-desc${active}" data-effort="${mode.id}">` +
      `<span class="composer-menu-label">${esc(mode.label)}</span>` +
      `<span class="composer-menu-desc">${esc(mode.desc)}</span>` +
      `${check}</button>`;
  }
  openComposerMenu($("effortChip"), html, (menu) => {
    for (const button of menu.querySelectorAll("[data-effort]")) {
      button.addEventListener("click", () => {
        const id = button.dataset.effort;
        closeComposerMenu();
        if (id === getEffort()) return;
        selectEffort(id);
      });
    }
  });
}

// ---------- 访问级别（完全访问）下拉 ----------
// 前端实现的审批策略：ask=逐次询问；auto=自动放行只读类工具；full=全部自动放行。
// 只影响本机审批行为，不修改服务端设置。

const ACCESS_MODES = [
  { id: "ask", label: "逐次询问", desc: "每个工具调用都需你确认" },
  { id: "auto", label: "自动允许", desc: "自动放行只读类工具（读取 / 搜索 / 列表）" },
  { id: "full", label: "完全访问", desc: "自动放行所有工具调用，请谨慎使用" },
];
const SAFE_TOOL_PATTERN = /(read|list|search|glob|grep|find|diff|status|health|stat|view|snapshot|observ|tree|cat\b|ls\b)/i;

function getAccessMode() {
  return localStorage.getItem("owo.access-mode") || "ask";
}
function setAccessMode(mode) {
  localStorage.setItem("owo.access-mode", mode);
  renderAccessChip();
}
function renderAccessChip() {
  const mode = ACCESS_MODES.find((item) => item.id === getAccessMode()) || ACCESS_MODES[0];
  const text = $("accessChipText");
  const dot = $("accessDot");
  const chip = $("accessChip");
  if (text) text.textContent = mode.label;
  if (dot) dot.className = `codex-access-dot mode-${mode.id}`;
  if (chip) {
    chip.dataset.mode = mode.id;
    chip.title = `工具访问级别：${mode.label}（${mode.desc}）`;
  }
}
function openAccessMenu() {
  const current = getAccessMode();
  let html = '<div class="composer-menu-title">工具访问级别</div>';
  for (const mode of ACCESS_MODES) {
    const active = mode.id === current ? " active" : "";
    const check = mode.id === current ? '<span class="composer-menu-check">✓</span>' : "";
    html +=
      `<button type="button" class="composer-menu-item has-desc${active}" data-access="${mode.id}">` +
      `<span class="composer-menu-label"><span class="codex-access-dot mode-${mode.id}"></span>${esc(mode.label)}</span>` +
      `<span class="composer-menu-desc">${esc(mode.desc)}</span>` +
      `${check}</button>`;
  }
  openComposerMenu($("accessChip"), html, (menu) => {
    for (const button of menu.querySelectorAll("[data-access]")) {
      button.addEventListener("click", () => {
        const mode = button.dataset.access;
        closeComposerMenu();
        if (mode === getAccessMode()) return;
        setAccessMode(mode);
        const label = (ACCESS_MODES.find((item) => item.id === mode) || {}).label;
        showToast(`访问级别已切换：${label}`, mode === "full" ? "error" : "ok");
      });
    }
  });
}

// ---------- 斜杠命令（composer 内输入 / 唤起，对标 Codex/Claude Code 会话内命令） ----------

const SLASH_COMMANDS = [
  { name: "new", desc: "新建会话", run: () => newSession() },
  { name: "undo", desc: "回退最近一个回合", run: () => sessionUndo() },
  { name: "redo", desc: "重做被回退的回合", run: () => sessionRedo() },
  { name: "export", desc: "导出当前会话为 Markdown", run: () => exportSession("md") },
  { name: "stop", desc: "中断正在执行的回合", run: () => abortTurn() },
  { name: "settings", desc: "打开设置页", run: () => setSettingsPageVisible(true) },
  { name: "help", desc: "查看全部命令", run: () => showSlashHelp() },
];

let slashMatches = [];
let slashActiveIndex = 0;

/// 当前输入是否处于命令补全状态（以 / 开头且尚未输入空格）。
function slashQuery() {
  const promptEl = $("prompt");
  if (!promptEl) return null;
  const value = promptEl.value;
  if (!value.startsWith("/")) return null;
  const rest = value.slice(1);
  if (/\s/.test(rest)) return null;
  return rest.toLowerCase();
}

function matchingSlashCommands(query) {
  return SLASH_COMMANDS.filter((cmd) => cmd.name.startsWith(query));
}

function closeSlashMenu() {
  if (composerMenuEl && composerMenuEl.dataset.owner === "slash") closeComposerMenu();
  slashMatches = [];
  slashActiveIndex = 0;
}

function refreshSlashMenu() {
  const query = slashQuery();
  if (query === null) {
    closeSlashMenu();
    return;
  }
  const matches = matchingSlashCommands(query);
  if (!matches.length) {
    closeSlashMenu();
    return;
  }
  const sameSet =
    composerMenuEl &&
    composerMenuEl.dataset.owner === "slash" &&
    slashMatches.length === matches.length &&
    slashMatches.every((cmd, i) => cmd.name === matches[i].name);
  slashMatches = matches;
  if (slashActiveIndex >= matches.length) slashActiveIndex = 0;
  const html =
    '<div class="composer-menu-title">命令</div>' +
    matches
      .map(
        (cmd, index) =>
          `<button type="button" class="composer-menu-item has-desc${index === slashActiveIndex ? " active" : ""}" data-slash="${index}">` +
          `<span class="composer-menu-label">/${esc(cmd.name)}</span>` +
          `<span class="composer-menu-desc">${esc(cmd.desc)}</span></button>`
      )
      .join("");
  if (sameSet) {
    // 复用已打开的浮层，只刷新高亮，避免每次按键都重建 DOM
    composerMenuEl.querySelectorAll("[data-slash]").forEach((button, index) => {
      button.classList.toggle("active", index === slashActiveIndex);
    });
    return;
  }
  openComposerMenu($("prompt"), html, (menu) => {
    menu.dataset.owner = "slash";
    menu.addEventListener("mousemove", (event) => {
      const button = event.target.closest("[data-slash]");
      if (!button) return;
      slashActiveIndex = Number(button.dataset.slash);
      menu.querySelectorAll("[data-slash]").forEach((item, index) => {
        item.classList.toggle("active", index === slashActiveIndex);
      });
    });
    menu.addEventListener("click", (event) => {
      const button = event.target.closest("[data-slash]");
      if (!button) return;
      runSlashCommand(slashMatches[Number(button.dataset.slash)]);
    });
  });
  if (composerMenuEl) composerMenuEl.dataset.owner = "slash";
}

/// 执行命令并清空输入框；命令返回 Promise 时静默吞掉错误交由各命令自身提示。
function runSlashCommand(command) {
  if (!command) return;
  closeSlashMenu();
  $("prompt").value = "";
  updateComposerHint();
  try {
    const result = command.run();
    if (result && typeof result.catch === "function") {
      result.catch((error) => {
        showToast(friendlyError(error, { resource: true }), "error");
      });
    }
  } catch (error) {
    showToast(friendlyError(error, { resource: true }), "error");
  }
}

function showSlashHelp() {
  const rows = SLASH_COMMANDS.map(
    (cmd) => `<li><strong>/${esc(cmd.name)}</strong><span>${esc(cmd.desc)}</span></li>`
  ).join("");
  openModal({
    title: "可用命令",
    body: `<ul class="slash-help">${rows}</ul>`,
    actions: [{ label: "关闭", kind: "primary", onClick: ({ close }) => close() }],
  });
}

// 输入框为空时给出命令入口提示（不干扰正常输入）
function updateComposerHint() {
  const chip = $("slashHint");
  if (!chip) return;
  const promptEl = $("prompt");
  chip.classList.toggle("hidden", !!promptEl.value);
}

// ---------- 自定义模型：元数据存 localStorage（服务端 settings 不保留该字段，避免往返丢失）；
// 密钥同样只进 localStorage，永不写入设置。
function customModelKeys() {
  try {
    return JSON.parse(localStorage.getItem("owo.model-keys") || "{}");
  } catch (_) {
    return {};
  }
}
function loadCustomModels() {
  try {
    return JSON.parse(localStorage.getItem("owo.custom-models") || "[]");
  } catch (_) {
    return [];
  }
}
function saveCustomModels(models) {
  localStorage.setItem("owo.custom-models", JSON.stringify(models));
  state.settings = state.settings || {};
  state.settings.custom_models = models;
}
function renderCustomModels() {
  const list = $("customModelList");
  if (!list) return;
  const models = loadCustomModels();
  const select = $("settingsModel");
  for (const option of [...select.options]) {
    if (option.dataset.custom === "1") option.remove();
  }
  for (const model of models) {
    const option = document.createElement("option");
    option.value = model.id;
    option.textContent = model.label || model.id;
    option.dataset.custom = "1";
    select.appendChild(option);
  }
  if (!models.length) {
    list.innerHTML = '<div class="sub">暂无自定义模型，点击右上角添加。</div>';
    return;
  }
  const keys = customModelKeys();
  list.innerHTML = "";
  for (const model of models) {
    const row = document.createElement("div");
    row.className = "model-row";
    row.innerHTML = `
      <div class="model-row-main">
        <strong>${esc(model.label || model.id)}</strong>
        <span class="sub">${esc(model.id)} ｜ ${esc(model.apiFormat === "anthropic" ? "Anthropic" : "OpenAI 兼容")} ｜ ${esc(model.baseUrl || "默认端点")}${keys[model.id] ? " ｜ 密钥已存" : ""}</span>
      </div>
      <div class="model-row-actions">
        <button type="button" class="ghost" data-act="default">设为默认</button>
        <button type="button" class="ghost" data-act="edit">编辑</button>
        <button type="button" class="ghost danger-text" data-act="remove">移除</button>
      </div>`;
    row.querySelector('[data-act="default"]').addEventListener("click", async () => {
      select.value = model.id;
      await saveSettings();
      showToast(`默认模型已切换为 ${model.label || model.id}`, "ok");
    });
    row.querySelector('[data-act="edit"]').addEventListener("click", () => openCustomModelDialog(model));
    row.querySelector('[data-act="remove"]').addEventListener("click", () =>
      confirmModal({
        title: "移除自定义模型",
        message: `确定移除「${model.label || model.id}」？该操作只影响本机设置。`,
        confirmText: "移除",
        kind: "danger",
        onConfirm: async () => {
          saveCustomModels(loadCustomModels().filter((item) => item.id !== model.id));
          renderCustomModels();
          showToast("已移除自定义模型", "ok");
        },
      })
    );
    list.appendChild(row);
  }
}

function openCustomModelDialog(existing) {
  const model = existing || { apiFormat: "openai" };
  const keys = customModelKeys();
  openModal({
    title: existing ? "编辑自定义模型" : "添加自定义模型",
    body: `
      <div class="form-grid">
        <label class="field-label">API 格式
          <select id="cmApiFormat">
            <option value="openai">OpenAI 兼容（/chat/completions）</option>
            <option value="anthropic">Anthropic（/messages）</option>
          </select>
        </label>
        <label class="field-label">模型 ID
          <input id="cmId" placeholder="例如 my-model-v1" value="${esc(model.id || "")}">
        </label>
        <label class="field-label">展示名称
          <input id="cmLabel" placeholder="例如 我的模型" value="${esc(model.label || "")}">
        </label>
        <label class="field-label">请求地址（Base URL）
          <input id="cmBaseUrl" placeholder="https://api.example.com/v1" value="${esc(model.baseUrl || "")}">
        </label>
        <label class="form-check">
          <input type="checkbox" id="cmFullUrl" ${model.useFullUrl ? "checked" : ""}>
          <span>使用完整 URL（不做路径拼接）</span>
        </label>
        <label class="field-label">API 密钥（只保存在本机浏览器，不写入设置文件）
          <input type="password" id="cmKey" autocomplete="new-password" placeholder="${model.id && keys[model.id] ? "已保存，留空则不修改" : "sk-…"}">
        </label>
        <details class="form-advanced">
          <summary>高级配置</summary>
          <div class="form-grid">
            <label class="field-label">温度（0–2，留空为默认）
              <input id="cmTemperature" placeholder="例如 0.7" value="${model.temperature ?? ""}">
            </label>
            <label class="field-label">超时（秒，留空为默认 120）
              <input id="cmTimeout" placeholder="120" value="${model.timeoutSecs ?? ""}">
            </label>
          </div>
        </details>
        <div class="form-inline-result" id="cmTestResult" hidden></div>
      </div>`,
    actions: [
      {
        label: "重置",
        kind: "ghost",
        onClick: ({ body }) => {
          body.querySelector("#cmApiFormat").value = "openai";
          for (const input of body.querySelectorAll("input")) {
            if (input.type === "checkbox") input.checked = false;
            else input.value = "";
          }
          const result = body.querySelector("#cmTestResult");
          result.hidden = true;
        },
      },
      {
        label: "连通性测试",
        onClick: async ({ body }) => {
          const result = body.querySelector("#cmTestResult");
          const baseUrl = body.querySelector("#cmBaseUrl").value.trim();
          const key = body.querySelector("#cmKey").value.trim() || (model.id && keys[model.id]) || "";
          result.hidden = false;
          if (!baseUrl) {
            result.textContent = "请先填写请求地址";
            return;
          }
          result.textContent = "正在测试连接…";
          try {
            const target = baseUrl.replace(/\/+$/, "") + "/models";
            const response = await fetch(target, {
              headers: key ? { Authorization: `Bearer ${key}` } : {},
            });
            if (response.ok) {
              const data = await response.json().catch(() => null);
              const count = data && Array.isArray(data.data) ? data.data.length : "?";
              result.textContent = `连接成功：端点返回 ${count} 个模型`;
            } else {
              result.textContent = `端点返回 ${response.status} ${response.statusText}`;
            }
          } catch (error) {
            result.textContent = `连接失败：${error.message || error}（可能受跨域或网络限制）`;
          }
        },
      },
      {
        label: existing ? "保存修改" : "添加模型",
        kind: "primary",
        onClick: async ({ close }) => {
          const id = $("cmId").value.trim();
          if (!id) {
            showToast("请填写模型 ID", "error");
            return;
          }
          const temperature = $("cmTemperature").value.trim();
          const timeoutSecs = $("cmTimeout").value.trim();
          const next = {
            id,
            label: $("cmLabel").value.trim() || id,
            apiFormat: $("cmApiFormat").value,
            baseUrl: $("cmBaseUrl").value.trim(),
            useFullUrl: $("cmFullUrl").checked,
            ...(temperature !== "" ? { temperature: Number(temperature) } : {}),
            ...(timeoutSecs !== "" ? { timeoutSecs: Number(timeoutSecs) } : {}),
          };
          const keyInput = $("cmKey").value.trim();
          if (keyInput) {
            const allKeys = customModelKeys();
            allKeys[id] = keyInput;
            localStorage.setItem("owo.model-keys", JSON.stringify(allKeys));
          }
          const models = loadCustomModels().filter((item) => item.id !== id);
          models.push(next);
          saveCustomModels(models);
          renderCustomModels();
          close();
          showToast(`已保存自定义模型 ${next.label}`, "ok");
        },
      },
    ],
  });
  $("cmApiFormat").value = model.apiFormat || "openai";
}
$("customModelAddBtn").addEventListener("click", () => openCustomModelDialog(null));
$("settingsModel").addEventListener("change", () => saveSettings());

// 本地偏好：select / toggle → LOCAL_PREFS（仅本机生效）
const PREF_SELECT_MAP = {
  prefFileOpener: "fileOpener",
  prefShell: "shell",
  prefLanguage: "language",
  prefSpeed: "speed",
  prefSpeechLang: "speechLang",
  prefTone: "tone",
  prefTheme: "theme",
};
for (const [id, key] of Object.entries(PREF_SELECT_MAP)) {
  $(id).addEventListener("change", () => {
    LOCAL_PREFS[key] = $(id).value;
    saveLocalPrefs();
    applyLocalPrefs();
    if (key === "theme") {
      const dark =
        LOCAL_PREFS.theme === "dark" ||
        (LOCAL_PREFS.theme === "system" &&
          window.matchMedia("(prefers-color-scheme: dark)").matches);
      applyTheme(dark ? "dark" : "light");
    }
  });
}
const PREF_TOGGLE_MAP = {
  prefCompact: "compact",
  prefCompactAppearance: "compact",
  prefReminder: "reminder",
  prefSound: "sound",
  prefApprovalPin: "approvalPin",
};
for (const [id, key] of Object.entries(PREF_TOGGLE_MAP)) {
  $(id).addEventListener("click", () => {
    LOCAL_PREFS[key] = !LOCAL_PREFS[key];
    saveLocalPrefs();
    syncLocalPrefs();
    applyLocalPrefs();
  });
}
// 桌面桌宠开关（A8-3）：走引擎通道控制桌面端挂件，不是本地偏好。
$("prefPet").addEventListener("click", () => {
  toggleDesktopPet();
});
// 主动建议是服务端设置（settings.proactive.enabled）
$("prefProactive").addEventListener("click", async () => {
  const next = !(state.settings.proactive && state.settings.proactive.enabled);
  try {
    const settings = { ...(state.settings || {}) };
    delete settings.custom_models;
    settings.proactive = { ...(settings.proactive || {}), enabled: next };
    await api("/settings", { method: "POST", body: JSON.stringify(settings) });
    await refreshSettings();
    showToast(`主动建议已${next ? "开启" : "关闭"}`, "ok");
  } catch (error) {
    showToast(`切换失败：${error.message || error}`, "error");
  }
});

// 常规：更改默认工作区 → 原生文件夹选择器
$("changeWorkspaceBtn").addEventListener("click", () => pickDirectory());
// 外观：恢复默认布局（会话栏 / 检查器宽度）
$("resetLayoutBtn").addEventListener("click", () => {
  document.body.style.removeProperty("--session-width");
  document.body.style.removeProperty("--inspect-width");
  showToast("已恢复默认布局", "ok");
});
// 语音：麦克风权限测试
$("micTestBtn").addEventListener("click", async () => {
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showToast("当前浏览器不支持麦克风采集", "error");
    return;
  }
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    stream.getTracks().forEach((track) => track.stop());
    showToast("麦克风可用（权限已授予）", "ok");
  } catch (error) {
    showToast(`麦克风不可用：${error.message || error}`, "error");
  }
});
// 语音：输入模式状态胶囊（本地录音优先，系统识别回退）
(function initSttModePill() {
  const pill = $("sttModePill");
  if (!pill) return;
  const canRecord = !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia);
  const canRecognize = !!(window.SpeechRecognition || window.webkitSpeechRecognition);
  pill.textContent = canRecord
    ? "本地录音优先（16k WAV）"
    : canRecognize
      ? "系统识别回退"
      : "当前环境不可用";
  pill.classList.toggle("offline", !canRecord && !canRecognize);
})();

// 账户：重连 / 退出登录（与左下用户卡菜单同款逻辑）
async function reconnectService() {
  apiToken = null;
  try {
    await ensureApiToken();
    await refreshHealth();
    showToast("服务已重新连接", "ok");
  } catch (error) {
    showToast(`重连失败：${error.message || error}`, "error");
  }
}
function logoutLocal() {
  apiToken = null;
  localStorage.removeItem("owo.workspace");
  showToast("已退出登录，下次操作将重新配对");
  serviceWatch.start();
}
$("accountReconnectBtn").addEventListener("click", reconnectService);
$("accountLogoutBtn").addEventListener("click", () =>
  confirmModal({
    title: "退出登录",
    message: "将清除本机配对信息，下次操作时重新连接服务。设置与数据不受影响。",
    confirmText: "退出登录",
    kind: "danger",
    onConfirm: logoutLocal,
  })
);

// 保存模型与服务端设置：基于最近一次 GET /settings 的全量副本改字段，
// 不再暴露原始 JSON 编辑器（表单是唯一入口）。
async function saveSettings(options = {}) {
  try {
    const settings = { ...(state.settings || {}) };
    delete settings.custom_models; // 自定义模型只存本机，不发给服务端
    settings.model = $("settingsModel").value;
    const resp = await api("/settings", {
      method: "POST",
      body: JSON.stringify(settings),
    });
    const note = `设置已保存：${(resp && resp.note) || "ok"}`;
    if (!options.silent) showToast(note, "ok");
    await refreshSettings();
    return { ok: true, note };
  } catch (error) {
    const note = `设置保存失败：${error.message || error}`;
    if (!options.silent) showToast(note, "error");
    return { ok: false, note };
  }
}

// ---------- 模型服务预设（config/provider-presets.js 为唯一事实源） ----------
// 前端只展示"端点 / 默认模型 / 所需环境变量"并生成可复制的设置命令；
// 端点与密钥由启动核心服务的终端环境变量决定，前端不写入、不假装保存。

function findPreset(id) {
  const registry = window.OwoProviderPresets;
  if (!registry || !id) return null;
  return registry.presets().find((preset) => preset.id === id) || null;
}

function renderPresetInfo() {
  const info = $("presetInfo");
  if (!info) return;
  const preset = findPreset($("providerPreset").value);
  if (!preset) {
    info.classList.add("hidden");
    return;
  }
  const command = window.OwoProviderPresets.envCommand(preset);
  $("presetBaseUrl").textContent = preset.baseUrl || "（自定义，请在终端设置 OPENAI_BASE_URL）";
  $("presetModel").textContent = preset.model || "（不指定，保持当前模型）";
  $("presetKeyEnv").textContent = preset.keyEnv || "（本地端点无需密钥）";
  $("presetCmdPreview").textContent = command || "（自定义端点无预设命令）";
  $("presetHint").textContent = `${preset.note} 选预设会填入上方端点与模型，密钥填在「API 密钥」后点「保存并连接」。`;
  $("presetApplyModelBtn").disabled = !preset.model;
  // 预设一键填入表单：端点 + 默认模型，用户只需补密钥。
  if (preset.baseUrl) $("providerBaseUrl").value = preset.baseUrl;
  if (preset.model && $("settingsModel").querySelector(`option[value="${CSS.escape(preset.model)}"]`)) {
    $("settingsModel").value = preset.model;
  }
  info.classList.remove("hidden");
}

// 回填模型接入表单：端点与密钥状态来自 GET /settings（密钥永不明文回传）。
function renderProviderForm() {
  const settings = state.settings || {};
  const provider = settings.provider || {};
  $("providerBaseUrl").value = provider.base_url || "";
  // 密钥只显示状态，不回填明文；已保存时留空即代表"不改动"。
  $("providerApiKey").value = "";
  $("providerApiKey").placeholder = provider.api_key_set ? "已保存（加密）· 留空则不修改" : "未保存";
  const state$ = $("providerKeyState");
  if (provider.api_key_set) {
    state$.textContent = "已保存";
    state$.classList.add("ok");
    state$.classList.remove("warn");
  } else {
    state$.textContent = "未保存";
    state$.classList.add("warn");
    state$.classList.remove("ok");
  }
}

// 保存模型接入：端点 + 模型 + 密钥（留空密钥 = 保留已存值），写本机设置并即时生效。
async function saveProvider() {
  const baseUrl = $("providerBaseUrl").value.trim();
  const apiKey = $("providerApiKey").value.trim();
  if (!baseUrl) {
    // 端点缺失时不能只存密钥：服务端会回落到内置默认 https://api.openai.com/v1，
    // 用户的密钥会被静默打到错误端点（曾实测请求发往 OpenAI 而非用户的服务商）。
    showToast("请填写 API 端点（可从上方服务商预设一键填入）", "error");
    return null;
  }
  try {
    const settings = { ...(state.settings || {}) };
    delete settings.custom_models; // 自定义模型只存本机，不发给服务端
    settings.model = $("settingsModel").value;
    settings.provider = {
      base_url: baseUrl,
      // 留空不下发密钥字段，服务端据此保留加密信封里的旧值。
      ...(apiKey ? { api_key: apiKey } : {}),
    };
    const resp = await api("/settings", {
      method: "POST",
      body: JSON.stringify(settings),
    });
    await refreshSettings();
    renderProviderForm();
    updateModelGate();
    showToast(`模型服务已连接：${settings.model}`, "ok");
    return { ok: true, note: (resp && resp.note) || "ok" };
  } catch (error) {
    showToast(`连接失败：${error.message || error}`, "error");
    return { ok: false };
  }
}

function copyText(text) {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text);
  }
  // 非安全上下文没有 clipboard API：回退到临时 textarea + execCommand。
  return new Promise((resolve, reject) => {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch (_) {
      ok = false;
    }
    document.body.removeChild(area);
    if (ok) resolve();
    else reject(new Error("复制失败"));
  });
}

async function copyPresetCommand() {
  const preset = findPreset($("providerPreset").value);
  const button = $("presetCopyCmdBtn");
  if (!preset || !button) return;
  const command = window.OwoProviderPresets.envCommand(preset);
  if (!command) return;
  const original = button.textContent;
  try {
    await copyText(command);
    button.textContent = "已复制";
  } catch (error) {
    button.textContent = "复制失败，请手动选择";
  }
  setTimeout(() => {
    button.textContent = original;
  }, 1600);
}

function initProviderPresets() {
  const select = $("providerPreset");
  const registry = window.OwoProviderPresets;
  if (!select || !registry) return;
  for (const preset of registry.presets()) {
    const option = document.createElement("option");
    option.value = preset.id;
    option.textContent = preset.label;
    select.appendChild(option);
  }
  select.addEventListener("change", renderPresetInfo);
  $("presetCopyCmdBtn").addEventListener("click", copyPresetCommand);
  $("presetApplyModelBtn").addEventListener("click", async () => {
    const preset = findPreset(select.value);
    if (!preset || !preset.model) return;
    const modelSelect = $("settingsModel");
    if (!modelSelect.querySelector(`option[value="${CSS.escape(preset.model)}"]`)) {
      $("presetHint").textContent = `预设默认模型 ${preset.model} 不在可选列表中，请在终端设置 OPENAI_MODEL 后重启服务。`;
      return;
    }
    modelSelect.value = preset.model;
    const result = await saveSettings({ silent: true });
    $("presetHint").textContent = result.ok
      ? `已保存模型 ${preset.model}（下一回合生效，无需重启；若终端里设了 OPENAI_MODEL，此处会覆盖它）。`
      : result.note;
  });
  renderPresetInfo();
  $("providerSaveBtn").addEventListener("click", saveProvider);
  const gateBtn = $("modelGateBtn");
  if (gateBtn) gateBtn.addEventListener("click", openModelGateSettings);
}

// ---------- 首启门：未接入自己的模型服务前，拦住发送 ----------
// 判定完全以服务端 /settings 的 provider_ready 为准（已保存配置或环境变量任一可用即放行）。

function modelGateMissing() {
  const settings = state.settings || {};
  if (settings.provider_ready) return false;
  const provider = settings.provider || {};
  return !provider.base_url && !provider.api_key_set;
}

function updateModelGate() {
  const gate = $("modelGate");
  const missing = modelGateMissing();
  if (gate) {
    gate.classList.toggle("hidden", !missing);
    if (missing) {
      const hint = $("modelGateHint");
      if (hint) {
        hint.textContent =
          state.settings && state.settings.provider_ready === false
            ? "服务端报告模型接入未就绪：请填写端点与密钥后保存。"
            : "填写你自己的 OpenAI 兼容端点与密钥即可开始，密钥只加密保存在本机。";
      }
    }
  }
  const send = $("sendBtn");
  if (send) send.disabled = missing;
  document.body.classList.toggle("model-gate-active", missing);
}

function openModelGateSettings() {
  setSettingsPageVisible(true);
  setSettingsTab("models");
  showToast("请先连接你自己的模型服务", "error");
}

function formatCount(value) {
  return Number(value || 0).toLocaleString("zh-CN");
}

async function refreshUsage() {
  try {
    const data = await api("/usage");
    const usage = data.usage || {};
    const budget = data.budget || {};
    const cards = [
      { label: "累计 Tokens", value: formatCount(usage.total_tokens) },
      { label: "输入 Tokens", value: formatCount(usage.prompt_tokens) },
      { label: "输出 Tokens", value: formatCount(usage.completion_tokens) },
      { label: "累计成本", value: `$${(data.cost_usd || 0).toFixed(4)}` },
      { label: "Token 预算", value: budget.token_cap != null ? formatCount(budget.token_cap) : "未配置" },
      { label: "成本预算", value: budget.cost_cap_usd != null ? `$${budget.cost_cap_usd}` : "未配置" },
    ];
    const violation = budget.violation
      ? '<div class="usage-alert">已超出预算上限，新的云端请求会被拒绝</div>'
      : "";
    $("usagePanel").innerHTML =
      cards
        .map(
          (card) =>
            `<div class="usage-card"><span class="sub">${esc(card.label)}</span><strong>${esc(card.value)}</strong></div>`
        )
        .join("") + violation;
  } catch (error) {
    $("usagePanel").innerHTML = `<div class="sub">${esc(friendlyError(error))}</div>`;
  }
}

// ---------- R8 存储与恢复（/storage/* + /server/status） ----------

async function refreshServerStatus() {
  try {
    const status = await api("/server/status");
    const gate = status.shutdown_gate || {};
    const storage = status.storage || {};
    const chips = [
      { label: "并发回合", value: `${gate.active_turns ?? 0} / ${gate.max_concurrent_turns ?? "?"}` },
      { label: "运行状态", value: gate.shutting_down ? "关闭中" : "运行中" },
      { label: "存储", value: storage.read_only ? "只读降级" : "正常" },
    ];
    if (storage.migration_warning) {
      chips.push({ label: "提示", value: storage.migration_warning, warn: true });
    }
    $("serverStatusPanel").innerHTML = chips
      .map(
        (chip) =>
          `<span class="status-chip${chip.warn ? " warn" : ""}"><span class="sub">${esc(chip.label)}</span><strong>${esc(chip.value)}</strong></span>`
      )
      .join("");
  } catch (error) {
    $("serverStatusPanel").innerHTML = `<span class="status-chip"><strong>${esc(friendlyError(error))}</strong></span>`;
  }
}

// 存储操作反馈：toast（即时）+ 账户页行内结果（留存细节）
function showStorageResult(text, kind = "ok") {
  const el = $("storageResult");
  el.textContent = text;
  el.hidden = false;
  el.classList.toggle("error", kind === "error");
}

async function storageBackup() {
  try {
    const result = await api("/storage/backup", { method: "POST", body: "{}" });
    const size = (result.size_bytes / 1024 / 1024).toFixed(2);
    showStorageResult(`备份完成（${size} MB），保存于：${result.saved_to}`);
    showToast(`备份完成（${size} MB）`, "ok");
    refreshServerStatus();
  } catch (error) {
    showStorageResult(friendlyError(error), "error");
    showToast(`备份失败：${error.message || error}`, "error");
  }
}

async function storageExport() {
  try {
    const data = await api("/storage/export", { method: "POST", body: "{}" });
    const counts = data.counts || {};
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `owo-export-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-")}.json`;
    link.click();
    URL.revokeObjectURL(url);
    showStorageResult(
      `导出完成：${counts.sessions ?? 0} 会话 / ${counts.audit ?? 0} 审计 / ` +
        `${counts.notes ?? 0} 笔记 / ${counts.skills ?? 0} 技能 / ${counts.workflows ?? 0} 工作流（标准 JSON 已下载）`
    );
    showToast("导出完成，JSON 已开始下载", "ok");
  } catch (error) {
    showStorageResult(friendlyError(error), "error");
    showToast(`导出失败：${error.message || error}`, "error");
  }
}

async function storageRestore(file) {
  if (!file) return;
  let archive_b64;
  try {
    archive_b64 = await new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => {
        const raw = reader.result;
        const bytes = typeof raw === "string" ? atob(raw.split(",")[1] || "") : raw;
        resolve(bytes);
      };
      reader.onerror = () => reject(new Error("读取备份文件失败"));
      reader.readAsDataURL(file);
    });
  } catch (error) {
    showStorageResult(`读取备份文件失败：${error.message || error}`, "error");
    return;
  }
  confirmModal({
    title: "从备份恢复",
    message:
      "恢复会覆盖当前 settings / notes / skills / workflows（恢复前会自动再备份一份），index.db 需重启核心服务后生效。确认继续？",
    confirmText: "恢复",
    kind: "danger",
    onConfirm: async () => {
      try {
        const result = await api("/storage/restore", {
          method: "POST",
          body: JSON.stringify({ archive_b64 }),
        });
        showStorageResult(
          `恢复完成：${result.restored.length} 项已还原，${result.staged.length} 项暂存` +
            `${result.restart_required ? "（index.db 重启后生效）" : ""}；恢复前自动备份：${result.pre_backup}`
        );
        showToast("恢复完成", "ok");
        refreshServerStatus();
      } catch (error) {
        showStorageResult(friendlyError(error), "error");
        showToast(`恢复失败：${error.message || error}`, "error");
      }
    },
  });
}

async function storageClear() {
  openModal({
    title: "一键清空数据",
    body: `
      <p class="modal-message">将清空全部会话 / 审计 / 笔记 / 记忆 / 自动化（技能与工作流保留），<strong>不可恢复</strong>。</p>
      <label class="field-label">请输入 <code>CLEAR_ALL</code> 以确认
        <input id="clearToken" placeholder="CLEAR_ALL" autocomplete="off">
      </label>`,
    actions: [
      { label: "取消", kind: "ghost", onClick: ({ close }) => close() },
      {
        label: "确认清空",
        kind: "danger",
        onClick: async ({ close }) => {
          if ($("clearToken").value.trim() !== "CLEAR_ALL") {
            showToast("确认码不正确，已取消", "error");
            return;
          }
          close();
          try {
            const result = await api("/storage/clear", {
              method: "POST",
              body: JSON.stringify({ confirm: "CLEAR_ALL" }),
            });
            showStorageResult(
              `已清空：${(result.cleared || []).join("、")}；完整性校验：${result.integrity}`
            );
            showToast("数据已清空", "ok");
            refreshSessions(null);
            refreshAudit();
            refreshServerStatus();
          } catch (error) {
            showStorageResult(friendlyError(error), "error");
            showToast(`清空失败：${error.message || error}`, "error");
          }
        },
      },
    ],
  });
}

async function executePackage(pkg) {
  let variables = {};
  if (pkg.variables && pkg.variables.length) {
    const raw = await askText({
      title: "填写变量",
      label: `为技能包填写变量（JSON，如 {"value":"小李"}）：`,
      value: "{}",
      confirmText: "继续",
    });
    if (raw === null) return;
    try {
      variables = JSON.parse(raw || "{}");
    } catch (_) {
      addMessage("error", "变量 JSON 解析失败");
      return;
    }
  }
  const ok = await askConfirm({
    title: "确认执行",
    message: `确认执行技能包 ${pkg.name}？首次执行需要审批。`,
    confirmText: "执行",
  });
  if (!ok) return;
  let highRiskAck = false;
  if (pkg.sensitivity === "high") {
    const riskOk = await askConfirm({
      title: "高敏感操作",
      message: `⚠ ${pkg.name} 是高敏感技能包（可能操作支付/验证码等场景），再次确认执行？`,
      confirmText: "仍要执行",
      kind: "danger",
    });
    if (!riskOk) return;
    highRiskAck = true;
  }
  try {
    const report = await api("/learn/execute-package", {
      method: "POST",
      body: JSON.stringify({ name: pkg.name, variables, confirm: true, high_risk_ack: highRiskAck }),
    });
    if (report.ok) {
      addMessage("system", `技能包 ${pkg.name} 执行成功（${report.steps.length} 步）`);
    } else {
      addMessage("error", `技能包 ${pkg.name} 执行失败：${report.error || ""}`);
    }
    for (const step of report.steps) {
      addMessage("tool", `${step.status.toUpperCase()} ${step.node_id}（${step.action}）：${step.detail || "ok"}`, "执行步骤");
    }
  } catch (error) {
    addMessage("error", `执行失败：${error.message}`);
  }
}

async function refreshSuggestions() {
  try {
    const suggestions = await api("/proactive/suggestions");
    const list = $("suggestionList");
    list.innerHTML = "";
    for (const suggestion of suggestions) {
      const li = document.createElement("li");
      li.innerHTML = `<strong>${esc(suggestion.app_id)}</strong><span class="sub">${esc(suggestion.summary)}</span><span class="sub">${esc(suggestion.sequence.join(" → "))}</span>`;
      const actions = ["learn", "execute_once", "ignore", "mute_forever"];
      const labels = { learn: "学习", execute: "执行一次", ignore: "忽略", mute: "静默" };
      for (const action of actions) {
        const button = document.createElement("button");
        button.textContent = labels[action];
        button.addEventListener("click", async () => {
          try {
            const result = await api("/proactive/decide", {
              method: "POST",
              body: JSON.stringify({ suggestion_id: suggestion.id, action }),
            });
            await refreshSuggestions();
            if (action === "learn" && result && result.package) {
              addMessage(
                "system",
                `建议已沉淀为技能包 ${result.package.name}（变量：${(result.package.variables || []).join(",") || "无"}）`
              );
              await refreshPackages();
            }
          } catch (error) {
            addMessage("error", `建议处理失败：${error.message}`);
          }
        });
        li.appendChild(button);
      }
      list.appendChild(li);
    }
    if (!suggestions.length) list.innerHTML = '<li class="sub">暂无建议</li>';
  } catch (error) {
    $("suggestionList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function refreshAudit() {
  try {
    const params = new URLSearchParams({ limit: "50" });
    const eventFilter = $("auditEvent").value.trim();
    const toolFilter = $("auditTool").value.trim();
    const qFilter = $("auditQ").value.trim();
    if (eventFilter) params.set("event", eventFilter);
    if (toolFilter) params.set("tool", toolFilter);
    if (qFilter) params.set("q", qFilter);
    const entries = await api(`/audit?${params.toString()}`);
    const list = $("auditList");
    list.innerHTML = "";
    for (const entry of entries) {
      const li = document.createElement("li");
      const ok = entry.approved === undefined ? "" : entry.approved ? "✅" : "⛔";
      const tool = entry.tool ? ` [${esc(entry.tool)}]` : "";
      li.innerHTML = `<span class="sub">${esc((entry.ts || "").slice(0, 19).replace("T", " "))} ${esc(entry.event)} ${ok}${tool}</span><span class="sub">${esc(entry.detail || "")}</span>`;
      list.appendChild(li);
    }
    if (!entries.length) list.innerHTML = '<li class="sub">暂无审计记录</li>';
  } catch (error) {
    $("auditList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

// ---------- 会话 / 任务 ----------

async function refreshSessions(selectId) {
  try {
    await refreshSessionsImpl(selectId);
  } catch (error) {
    $("sessionList").innerHTML = `<li class="sub">会话读取失败：${esc(error.message || error)}</li>`;
  }
}

async function refreshSessionsImpl(selectId) {
  const sessions = await api("/sessions");
  const showArchived = $("showArchived").checked;
  const visible = sessions.filter((session) => showArchived || !session.archived);
  const visibleIds = new Set(visible.map((session) => session.id));
  const byParent = new Map();
  for (const session of visible) {
    const key = session.parent_id || "";
    if (!byParent.has(key)) byParent.set(key, []);
    byParent.get(key).push(session);
  }
  const isRoot = (session) => !session.parent_id || !visibleIds.has(session.parent_id);
  const list = $("sessionList");
  list.innerHTML = "";
  const renderSession = (session, depth) => {
    const li = document.createElement("li");
    // 运行中徽标：该会话仍有未完成回合（后台并行执行）。
    if (state.activeTurns.has(session.id)) {
      li.classList.add("running");
      li.title = (li.title ? `${li.title} ` : "") + "任务运行中";
    }
    if (session.id === selectId) {
      li.className = "active";
      state.sessionId = session.id;
    }
    const badges = [];
    if (session.pinned) badges.push("📌");
    if (session.archived) badges.push("🗄");
    // Codex 风格：列表只显示标题 + 短时间，模型与完整时间收入 title 提示
    const raw = session.updated_at || session.created_at || "";
    const updated = raw ? raw.slice(5, 16).replace("T", " ") : "";
    li.title = `${session.model || "默认模型"} · ${raw.slice(0, 19).replace("T", " ")}`;
    li.innerHTML = `
      <div style="margin-left:${depth * 14}px">
        <strong>${esc(session.title || session.id.slice(0, 12))} ${badges.join(" ")}</strong>
        <span class="sub">${esc(updated)}</span>
        <div class="inline">
          <button data-act="open">继续</button>
          <button data-act="rename">重命名</button>
          <button data-act="pin">${session.pinned ? "取消置顶" : "置顶"}</button>
          <button data-act="archive">${session.archived ? "取消归档" : "归档"}</button>
          <button data-act="fork">fork</button>
          <button data-act="rewind">回退</button>
          <button data-act="redo">重做</button>
          <button data-act="delete" class="danger">删除</button>
        </div>
      </div>`;
    for (const button of li.querySelectorAll("button")) {
      button.addEventListener("click", async (event) => {
        event.stopPropagation();
        const act = button.dataset.act;
        try {
          if (act === "open") {
            await selectSession(session.id);
            return;
          }
          if (act === "rename") {
            promptModal({
              title: "重命名会话",
              label: "输入新的会话标题：",
              value: session.title || "",
              placeholder: "例如：重构登录模块",
              confirmText: "保存",
              onConfirm: async (title) => {
                if (!title) return;
                await api(`/session/${session.id}/rename`, {
                  method: "POST",
                  body: JSON.stringify({ title }),
                });
                await refreshSessions(selectId || state.sessionId);
              },
            });
            return;
          } else if (act === "pin") {
            await api(`/session/${session.id}/pin`, {
              method: "POST",
              body: JSON.stringify({ pinned: !session.pinned }),
            });
          } else if (act === "archive") {
            await api(`/session/${session.id}/archive`, {
              method: "POST",
              body: JSON.stringify({ archived: !session.archived }),
            });
          } else if (act === "fork") {
            promptModal({
              title: "分叉子会话",
              label: "从第几条消息处分叉？留空表示最后一条。",
              placeholder: "0",
              confirmText: "分叉",
              onConfirm: async (raw) => {
                const parsed = parseInt(raw, 10);
                const message_index = Number.isFinite(parsed) ? parsed : 999999;
                const child = await api(`/session/${session.id}/fork`, {
                  method: "POST",
                  body: JSON.stringify({ message_index }),
                });
                addMessage("system", `已 fork 子会话 ${child.id}（可在列表中选择）`);
                await refreshSessions(selectId || state.sessionId);
              },
            });
            return;
          } else if (act === "delete") {
            // 只能归档不能删除会让长列表迟早爆掉（Codex 侧栏可删线程）。
            confirmModal({
              title: "删除会话",
              message: `删除后不可恢复：${session.title || session.id.slice(0, 12)}`,
              confirmText: "删除",
              kind: "danger",
              onConfirm: async () => {
                await api(`/session/${session.id}`, { method: "DELETE" });
                if (session.id === state.sessionId) {
                  state.sessionId = null;
                  localStorage.removeItem("owo.lastSession");
                  const view = $("messages").querySelector(
                    `.session-view[data-sid="${session.id}"]`
                  );
                  if (view) view.remove();
                  $("emptyState").classList.remove("hidden");
                }
                showToast("会话已删除", "ok");
                await refreshSessions(state.sessionId);
              },
            });
            return;
          } else if (act === "rewind") {
            await sessionUndo(session.id);
            return;
          } else if (act === "redo") {
            await sessionRedo(session.id);
            return;
          }
          await refreshSessions(selectId || state.sessionId);
        } catch (error) {
          addMessage("system", `操作失败：${friendlyError(error, { resource: true })}`);
        }
      });
    }
    li.addEventListener("click", () => selectSession(session.id));
    list.appendChild(li);
    for (const child of byParent.get(session.id) || []) {
      renderSession(child, depth + 1);
    }
  };
  // Codex 风格：一级按项目（工作区）分桶，fork 父子树留在组内。
  // 会话记录本就带 workspace 字段，之前只在列表里裸铺，多项目下无法分辨。
  const groups = new Map();
  for (const session of visible) {
    if (!isRoot(session)) continue;
    const workspace = String(session.workspace || "").trim();
    const key = workspace.toLowerCase();
    if (!groups.has(key)) groups.set(key, { workspace, roots: [] });
    groups.get(key).roots.push(session);
  }
  for (const group of groups.values()) {
    const header = document.createElement("li");
    header.className = "codex-session-group";
    header.textContent = workspaceLabel(group.workspace);
    header.title = group.workspace || "该会话未记录工作区";
    list.appendChild(header);
    for (const session of group.roots) renderSession(session, 0);
  }
  if (!list.children.length) list.innerHTML = '<li class="sub">暂无会话</li>';
  filterSessionList();
}

/// 工作区显示名：取路径最后一段（Windows 反斜杠与 POSIX 斜杠都支持）。
function workspaceLabel(workspace) {
  const raw = String(workspace || "").trim().replace(/[\\/]+$/, "");
  if (!raw) return "未指定工作区";
  const parts = raw.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] || raw;
}

// 会话列表搜索过滤（Codex 侧栏搜索框）
function filterSessionList() {
  const input = $("sessionSearch");
  const query = (input.value || "").trim().toLowerCase();
  const items = [...$("sessionList").querySelectorAll("li")];
  for (const li of items) {
    if (li.classList.contains("codex-session-group")) continue;
    li.classList.toggle("hidden", !!query && !li.textContent.toLowerCase().includes(query));
  }
  // 组标题：搜索后没有可见子项的组一并隐藏，不留空标题。
  for (const li of items) {
    if (!li.classList.contains("codex-session-group")) continue;
    let node = li.nextElementSibling;
    let anyVisible = false;
    while (node && !node.classList.contains("codex-session-group")) {
      if (!node.classList.contains("hidden")) {
        anyVisible = true;
        break;
      }
      node = node.nextElementSibling;
    }
    li.classList.toggle("hidden", !anyVisible);
  }
}

/// 回退一个回合：把会话截断到最后一条 user 消息之前（连带其后的助手回复）。
/// 后端 rewind 需要显式 keep 值，这里从会话详情推导，避免让用户手填条数。
async function sessionUndo(sessionId) {
  const id = sessionId || state.sessionId;
  if (!id) {
    showToast("请先选择会话", "error");
    return;
  }
  const detail = await api(`/session/${id}`);
  const messages = detail.messages || [];
  let lastUser = -1;
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (messages[i].role === "user") {
      lastUser = i;
      break;
    }
  }
  if (lastUser < 0) {
    showToast("没有可回退的回合", "error");
    return;
  }
  const result = await api(`/session/${id}/rewind`, {
    method: "POST",
    body: JSON.stringify({ keep: lastUser }),
  });
  showToast(`已回退 ${result.removed || 0} 条消息`, "ok");
  if (id === state.sessionId) await selectSession(id);
  else await refreshSessions(state.sessionId);
}

/// 重做：恢复最近一次回退掉的回合。
async function sessionRedo(sessionId) {
  const id = sessionId || state.sessionId;
  if (!id) {
    showToast("请先选择会话", "error");
    return;
  }
  const result = await api(`/session/${id}/redo`, { method: "POST" });
  showToast(`已恢复 ${result.restored || 0} 条消息`, "ok");
  if (id === state.sessionId) await selectSession(id);
  else await refreshSessions(state.sessionId);
}

async function selectSession(id) {
  const selectionVersion = ++state.selectionVersion;
  state.sessionId = id;
  // 记住当前对话：下次进入自动恢复，不再要求手动切换。
  localStorage.setItem("owo.lastSession", id);
  state.attachments = [];
  renderAttachmentChips();
  // 切会话：清掉上一回合的流式句柄与未答提问，回复底部跟随。
  resetRunBlocks();
  state.pendingQuestion = null;
  state.autoScroll = true;
  updateScrollBottomBtn();
  // 每会话独立视图：切回时保留该会话的本地输出（含仍在运行回合的实时内容）。
  showSessionView(id);
  // 并行回合：状态条/发送钮跟随切回来的会话（该会话在跑 → 恢复运行中视图）。
  const runningTurn = currentTurn();
  if (runningTurn) {
    startRunStatus();
    state.runStartedAt = runningTurn.startedAt || Date.now();
  } else {
    stopRunStatus();
  }
  updateComposerRunning();
  const reusedView = Boolean(
    $("messages").querySelector(`.session-view[data-sid="${id}"]`)
  );
  try {
    const detail = await api(`/session/${id}`);
    if (selectionVersion !== state.selectionVersion) return;
    if (reusedView) {
      // 本地视图还在（可能正有运行中回合的实时输出）：不重拉历史覆盖，只刷新侧栏。
      $("emptyState").classList.add("hidden");
      await refreshSessions(id);
      await refreshDiff(id);
      await refreshSessionContext(id);
      return;
    }
    sessionView(id).innerHTML = "";
    let rendered = 0;
    // 历史回放：把 role=tool 的结果按 tool_call_id 配回 assistant 的 tool_calls，
    // 还原成与会话进行中一致的步骤 chip。旧实现把这些记录折叠成一句
    // 「已折叠未显示」，用户无从复核 agent 到底做过什么。
    const history = detail.messages || [];
    const toolResults = new Map();
    for (const message of history) {
      if (message.role !== "tool") continue;
      const content = String(message.content || "");
      const failed = content.startsWith("工具错误：");
      toolResults.set(String(message.tool_call_id || ""), {
        ok: !failed,
        preview: content,
        error: failed ? content.replace(/^工具错误：/, "") : "",
      });
    }
    // 仍检测「最后一条是用户消息但没回复」的失败回合，显式告诉用户。
    let lastRole = null;
    for (const message of history) {
      if (message.role === "system") {
        // 压缩摘要等系统记录：按系统提示显示，避免被当作助手回复。
        addMessage("system", message.content);
        lastRole = "system";
        continue;
      }
      if (message.role === "tool") {
        lastRole = "tool";
        continue;
      }
      if (message.role === "assistant") {
        const calls = Array.isArray(message.tool_calls) ? message.tool_calls : [];
        if (calls.length) {
          appendHistoryToolSteps(calls, toolResults);
          lastRole = "tool";
          rendered += 1;
        }
        if (!message.content) continue;
      } else if (!message.content) {
        continue;
      }
      addMessage(message.role === "user" ? "user" : "assistant", message.content);
      lastRole = message.role === "user" ? "user" : "assistant";
      rendered += 1;
    }
    if (lastRole === "user") {
      addMessage("system", "上一条消息还没有回复（回合可能失败或被中断），可重新发送让助手继续");
    }
    addMessage("system", `已恢复会话：${detail.title || id.slice(0, 12)}`);
    // 空会话（无任何消息）时展示中央空状态；决策须在系统提示之后，避免被其掩埋
    if (rendered) $("emptyState").classList.add("hidden");
    else $("emptyState").classList.remove("hidden");
  } catch (error) {
    if (selectionVersion !== state.selectionVersion) return;
    sessionView(id).innerHTML = "";
    addMessage("system", `会话加载失败：${friendlyError(error, { resource: true })}`);
  }
  await refreshSessions(id);
  await refreshDiff(id);
  await refreshSessionContext(id);
}

// ---------- 会话上下文仪表（v0.5.7，对标 Codex 上下文状态显示） ----------

async function refreshSessionContext(sessionId) {
  const bar = $("contextBar");
  if (!sessionId) {
    bar.classList.add("hidden");
    return;
  }
  try {
    const ctx = await api(`/session/${sessionId}/context`);
    const fill = $("contextFill");
    const ratio = ctx.token_budget > 0 ? ctx.estimated_tokens / ctx.token_budget : 0;
    fill.style.width = `${Math.min(100, Math.round(ratio * 100))}%`;
    fill.className = ratio > 1 ? "over" : ratio > 0.8 ? "warn" : "";
    const compactionBadge = ctx.last_compaction ? "已压缩" : "未压缩";
    $("contextLabel").textContent =
      `上下文 ${ctx.messages} 条 · 估算 ${ctx.estimated_tokens}/${ctx.token_budget} tokens`;
    bar.title =
      `规则注入：${ctx.rules_injected ? "是" : "否"} · 压缩：${compactionBadge}` +
      (ctx.compaction_enabled ? "" : "（压缩关闭）") +
      (ctx.last_compaction ? `\n最近压缩摘要：${ctx.last_compaction.slice(0, 300)}` : "");
    bar.classList.remove("hidden");
  } catch (error) {
    $("contextLabel").textContent = friendlyError(error, { resource: true });
    bar.title = "会话上下文状态";
    bar.classList.remove("hidden");
  }
}

async function newSession() {
  const workspace = $("workspace").value.trim();
  if (!workspace) {
    showToast("请先填写工作区绝对路径", "error");
    return;
  }
  localStorage.setItem("owo.workspace", workspace);
  const session = await api("/session", {
    method: "POST",
    body: JSON.stringify({ workspace }),
  });
  await selectSession(session.id);
  addMessage("system", `已创建会话 ${session.id}`);
}

// 进入时自动恢复上次的对话：优先 localStorage 记忆，其次最近更新的会话；
// 都没有则保持空状态，不强制用户手动切换。
async function restoreLastSession() {
  if (state.sessionId) return;
  try {
    const sessions = (await api("/sessions")) || [];
    const last = localStorage.getItem("owo.lastSession");
    const remembered =
      last && sessions.some((session) => session.id === last && !session.archived);
    if (last && !remembered) localStorage.removeItem("owo.lastSession");
    const fallback = sessions.find((session) => !session.archived);
    const target = remembered ? last : fallback && fallback.id;
    if (!target) return;
    await selectSession(target);
  } catch (_) {
    /* 恢复失败保持空状态，不打扰用户 */
  }
}

// ---------- 对话（SSE） ----------

function parseSseBlock(block) {
  let event = "message";
  let data = "";
  for (const line of block.split("\n")) {
    if (line.startsWith("event:")) event = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  return { event, data };
}

async function sendPrompt() {
  if (!state.sessionId) {
    addMessage("system", "请先新建或选择一个会话");
    return;
  }
  // 只有「本会话」已有回合才拒绝：其它会话在跑不影响这里（并行对话互不干扰）。
  if (currentTurn()) {
    showToast("本会话已有回合在运行，可先中断再发送", "error");
    return;
  }
  const prompt = $("prompt").value.trim();
  if (!prompt) return;
  // 首启门：未接入自己的模型服务前不让发送，直接引导去配置。
  if (modelGateMissing()) {
    openModelGateSettings();
    return;
  }
  $("prompt").value = "";
  updateComposerHint();
  // 主动发送：回到跟随底部（此前上滑过也要跟住新回合）。
  state.autoScroll = true;
  updateScrollBottomBtn();
  addMessage("user", prompt);
  const attachments = state.attachments.map((attachment) => attachment.id);
  if (attachments.length) {
    addMessage("system", `附带 ${attachments.length} 个附件`);
  }
  // 助手气泡按需创建：让思考块/工具分组先于回答出现，保持时间顺序。
  let streaming = null;
  const ensureStreaming = () => {
    if (!streaming) streaming = addMessage("assistant", "");
    return streaming;
  };
  let assistantText = "";
  let finished = false;
  let reader = null;
  // 本回合统计：汇报卡数据源（工具次数/模型调用轮次/耗时/服务端补发的 token 消耗）。
  state.turn = { tools: 0, failed: 0, modelCalls: 0, startedAt: Date.now(), server: null, card: null, files: new Set() };

  state.reading = true;
  state.abortController = new AbortController();
  updateComposerRunning();
  startRunStatus();
  // 流写入绑定发起回合的会话：切走后任务继续跑、输出留在原视图，切回即恢复。
  const turnSessionId = state.sessionId;
  writeTargetSid = turnSessionId;
  state.activeTurns.set(turnSessionId, {
    controller: state.abortController,
    startedAt: Date.now(),
  });
  try {
    const headers = { "Content-Type": "application/json" };
    const token = await ensureApiToken().catch(() => null);
    if (token) headers.Authorization = `Bearer ${token}`;
    const response = await fetch(`${API_BASE}/session/${turnSessionId}/turn`, {
      method: "POST",
      headers,
      body: JSON.stringify({ prompt, attachments }),
      signal: state.abortController.signal,
    });
    if (!response.ok || !response.body) {
      throw new Error(await response.text());
    }
    reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    const handleBlock = (block) => {
      // 每帧绑定写入目标：并行流互不覆盖，各写各的会话视图（JS 单线程同步段内一致）。
      writeTargetSid = turnSessionId;
      const { event, data } = parseSseBlock(block);
      if (!data) return;
      let payload;
      try {
        payload = JSON.parse(data);
      } catch (_) {
        return;
      }
      switch (event) {
        case "reasoning_delta":
          setRunPhase("reasoning");
          pushReasoning(payload.delta || "");
          break;
        case "token_delta": {
          finishThinking();
          setRunPhase("answering");
          assistantText += payload.delta || "";
          const bubble = ensureStreaming();
          bubble.innerHTML = renderMarkdown(assistantText);
          bindCopyButtons(bubble);
          followScroll();
          break;
        }
        case "progress":
          // 「模型调用」每个回合都发，不再成行，只计入汇报卡的轮次统计。
          if (payload.message === "模型调用") {
            if (state.turn) state.turn.modelCalls += 1;
            // 桌宠气泡带轮次，让「在想…」有进度感。
            setRunPhase("thinking", state.turn ? `第 ${state.turn.modelCalls} 轮思考…` : undefined);
          } else if (payload.message) {
            addMessage("system", `[${payload.message}]`);
          }
          break;
        case "tool_use":
          setRunPhase("tool", `正在执行工具：${payload.tool || "工具"}`);
          pushToolUse(payload);
          break;
        case "tool_result":
          setRunPhase("thinking", "继续思考…");
          pushToolResult(payload);
          break;
        case "permission_request":
          setRunPhase("waiting");
          showApproval(payload);
          break;
        case "permission_resolved":
          // B1-5 审批终态（超时/中止/其他通道响应）：即时收尾，不等 5s 轮询。
          markApprovalResolved(payload);
          break;
        case "user_question":
          // ask_user：回合挂起等待回答，桌宠/状态条进入阻塞态。
          setRunPhase("asking");
          showQuestionCard(payload);
          break;
        case "user_answered":
          markQuestionAnswered(payload);
          break;
        case "turn_stats":
          // 回合统计补发（步数/耗时/token 消耗）：补齐汇报卡底部数据。
          if (state.turn) {
            state.turn.server = payload;
            fillTurnSummaryMeta(state.turn);
          }
          break;
        case "final": {
          finishThinking();
          settlePendingQuestion("ended");
          stopRunStatus();
          assistantText = payload.text || assistantText;
          // 空回答（网关截断/模型超载）：不能留一个空气泡让用户以为「卡住」。
          if (!String(assistantText).trim()) {
            finished = true;
            state.toolRun = null;
            showTurnFailure("模型未返回任何内容，请重试");
            break;
          }
          const bubble = ensureStreaming();
          bubble.innerHTML = renderMarkdown(assistantText);
          bindCopyButtons(bubble);
          finished = true;
          state.toolRun = null;
          // 完成摘要气泡由 renderTurnSummary 在算出文件改动后回报。
          if (state.turn) renderTurnSummary(state.sessionId, state.turn);
          break;
        }
        case "turn_failed":
          // 服务端失败终态：明确告诉用户失败原因并补汇报卡，不让界面停在「执行中」。
          finished = true;
          stopRunStatus();
          state.toolRun = null;
          showTurnFailure(payload.message || "未知原因");
          break;
        case "compaction":
          addEventChip("compact", "上下文已自动压缩", payload.summary || "");
          break;
      }
    };

    const consumeBlocks = (text, flush) => {
      const blocks = text.split(/\r?\n\r?\n/);
      const remainder = flush ? "" : blocks.pop() || "";
      for (const block of blocks) handleBlock(block);
      return remainder;
    };

    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        buffer += decoder.decode();
        if (buffer.trim()) consumeBlocks(buffer, true);
        buffer = "";
        break;
      }
      buffer += decoder.decode(value, { stream: true });
      buffer = consumeBlocks(buffer, false);
    }
    if (!finished) {
      // 断流兜底：服务端未给出终态（连接被切断/任务异常结束）也要显式收尾，
      // 不能静默删泡——那正是「思考完就卡住、什么也没给」的观感来源。
      settlePendingQuestion("ended");
      if (assistantText.trim()) {
        addMessage("system", "连接提前结束，以上回答可能不完整");
      } else {
        if (streaming) streaming.remove();
        showTurnFailure("连接提前结束（未收到结束标记），请重试");
      }
    }
    hideApproval();
    state.attachments = [];
    renderAttachmentChips();
    await refreshSessions(state.sessionId);
    await refreshDiff(state.sessionId);
    await refreshSessionContext(state.sessionId);
  } catch (error) {
    if (!assistantText && streaming) streaming.remove();
    if (error.name !== "AbortError") {
      showTurnFailure(error.message);
    } else {
      addEventChip("stop", "已被你停止", "本回合在收到中断请求后结束，未完成的部分可重新发送继续。");
    }
  } finally {
    reader?.releaseLock();
    if (typeof turnSessionId !== "undefined" && turnSessionId) {
      state.activeTurns.delete(turnSessionId);
      if (writeTargetSid === turnSessionId) writeTargetSid = null;
    }
    // 视图级收尾只在本回合仍属于「当前视图会话」时执行：并行回合下，
    // 后台会话结束不能把前台会话的流式句柄/状态条/审批卡一起清掉。
    const isVisible = state.sessionId === turnSessionId;
    if (isVisible) {
      hideApproval();
      finishThinking();
      // 终态兜底：任何路径（中断/异常/断流）结束后，未答的提问卡都必须显式作废。
      settlePendingQuestion("aborted");
      stopRunStatus();
      resetRunBlocks();
      state.turn = null;
      state.abortController = null;
    }
    state.reading = state.activeTurns.size > 0;
    updateComposerRunning();
  }
}

function renderAttachmentChips() {
  const container = $("attachmentChips");
  container.innerHTML = "";
  for (const attachment of state.attachments) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = `${attachment.name} ×`;
    chip.title = attachment.mime || "attachment";
    chip.addEventListener("click", () => {
      state.attachments = state.attachments.filter(
        (item) => item.id !== attachment.id
      );
      renderAttachmentChips();
    });
    container.appendChild(chip);
  }
}

async function uploadAttachments(files) {
  if (!state.sessionId) {
    addMessage("system", "请先新建或选择一个会话，再添加附件");
    return;
  }
  for (const file of files) {
    try {
      const dataUrl = await new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(reader.result);
        reader.onerror = () => reject(reader.error);
        reader.readAsDataURL(file);
      });
      const comma = String(dataUrl).indexOf(",");
      const dataB64 = comma >= 0 ? String(dataUrl).slice(comma + 1) : String(dataUrl);
      const uploaded = await api(`/session/${state.sessionId}/attachments`, {
        method: "POST",
        body: JSON.stringify({
          name: file.name,
          mime: file.type || "application/octet-stream",
          data_b64: dataB64,
        }),
      });
      state.attachments.push({ id: uploaded.id, name: uploaded.name, mime: uploaded.mime });
      renderAttachmentChips();
    } catch (error) {
      addMessage("system", `附件上传失败 ${file.name}：${error.message || error}`);
    }
  }
  $("attachmentInput").value = "";
}

// ---------- 审批条 ----------
// 访问级别（composer 下拉）：ask 逐次询问；auto 自动放行只读类工具；full 全部放行。

function showApproval(payload) {
  const requestId = payload.request_id;
  if (!requestId) return;
  state.pendingApprovals.set(requestId, {
    tool: payload.tool || "",
    reason: payload.reason || "",
    // 流帧驱动时 writeTargetSid 即发起回合的会话（跨会话审批归属正确）。
    sessionId: writeTargetSid || state.sessionId,
  });
  const mode = getAccessMode();
  if (mode !== "ask") {
    const safe = SAFE_TOOL_PATTERN.test(payload.tool || "");
    if (mode === "full" || safe) {
      addMessage(
        "system",
        `已自动允许（${mode === "full" ? "完全访问" : "自动允许"}）：${payload.tool}`
      );
      respondApproval(requestId, true);
      return;
    }
  }
  renderApprovals();
}

// 渲染审批队列：当前会话与其它会话的待审批卡并存，各自独立允许/拒绝。
function renderApprovals() {
  const list = $("approvalList");
  const bar = $("approvalBar");
  if (!list || !bar) return;
  list.innerHTML = "";
  const items = [...state.pendingApprovals.entries()];
  bar.classList.toggle("hidden", items.length === 0);
  for (const [requestId, entry] of items) {
    const row = document.createElement("div");
    row.className = "approval-item";
    const cross =
      entry.sessionId && entry.sessionId !== state.sessionId;
    const text = document.createElement("span");
    text.className = "approval-item-text";
    const prefix = cross
      ? `[会话 ${String(entry.sessionId).slice(0, 6)}…] `
      : "";
    text.textContent = `${prefix}需要审批：${entry.tool || "未知工具"}${
      entry.reason ? `（${entry.reason}）` : ""
    }`;
    const allow = document.createElement("button");
    allow.className = "allow";
    allow.textContent = "允许";
    allow.dataset.rid = requestId;
    const deny = document.createElement("button");
    deny.className = "deny";
    deny.textContent = "拒绝";
    deny.dataset.rid = requestId;
    row.append(text, allow, deny);
    list.appendChild(row);
  }
}

// 跨会话待审批同步：轮询服务端 pending 列表，把错过 SSE 事件/其它会话的审批并入队列；
// 服务端已不存在的（已响应/超时/会话结束）同步移除，避免点了必 404 的僵尸卡。
async function syncPendingApprovals() {
  if (Date.now() < connectionUnavailableUntil) return;
  try {
    const data = await api("/approvals/pending");
    const remote = Array.isArray(data && data.pending) ? data.pending : [];
    let changed = false;
    for (const item of remote) {
      if (!item.request_id || state.pendingApprovals.has(item.request_id)) continue;
      state.pendingApprovals.set(item.request_id, {
        tool: item.tool || "",
        reason: item.args_summary || "",
        sessionId: item.session_id || "",
      });
      changed = true;
    }
    for (const requestId of [...state.pendingApprovals.keys()]) {
      if (!remote.some((item) => item.request_id === requestId)) {
        state.pendingApprovals.delete(requestId);
        changed = true;
      }
    }
    if (changed) renderApprovals();
  } catch {
    /* 引擎未就绪时静默 */
  }
}
setInterval(syncPendingApprovals, 5000);

function hideApproval() {
  // 兼容旧调用（turn 终态）：清掉当前会话的待审批卡（该回合已结束，审批不再有意义）。
  clearSessionApprovals(state.sessionId);
}

/// B1-5：审批终态回执（引擎侧已解决）——即时从队列移除卡片，并在时间线留下
/// 终态说明。超时/中止场景此前要等 5s 轮询兜底且时间线无痕。
function markApprovalResolved(payload) {
  const requestId = payload && payload.request_id;
  if (!requestId) return;
  const entry = state.pendingApprovals.get(requestId);
  state.pendingApprovals.delete(requestId);
  renderApprovals();
  const source = (payload && payload.source) || "user";
  // 用户自己点的允许/拒绝已有系统消息/事件 chip，不重复。
  if (source === "user") return;
  const tool = entry && entry.tool ? toolLabel(entry.tool) : "工具调用";
  if (source === "timeout") {
    addEventChip("deny", `审批超时：${tool}`, "300 秒未响应，引擎已按拒绝处理。");
  } else if (source === "aborted") {
    addEventChip("deny", `审批作废：${tool}`, "回合已中止，本次审批无需再响应。");
  }
}

function clearSessionApprovals(sessionId) {
  if (!sessionId) return;
  let changed = false;
  for (const [requestId, entry] of state.pendingApprovals) {
    if (entry.sessionId === sessionId) {
      state.pendingApprovals.delete(requestId);
      changed = true;
    }
  }
  if (changed) renderApprovals();
}

async function respondApproval(requestId, allow) {
  const entry = state.pendingApprovals.get(requestId);
  if (!entry) return;
  const tool = entry.tool || "";
  const sessionId = entry.sessionId || state.sessionId;
  state.pendingApprovals.delete(requestId);
  renderApprovals();
  // 审批提示写进卡片所属会话的视图（点卡时用户可能停在别的会话）。
  const prevTarget = writeTargetSid;
  writeTargetSid = sessionId;
  try {
    await api(`/session/${sessionId}/permission/${requestId}`, {
      method: "POST",
      body: JSON.stringify({ allow }),
    });
    if (allow) {
      addMessage("system", "已允许该操作");
    } else {
      // 拒绝是回合内的关键事件：写成时间线 chip，附工具与原因，便于事后复盘。
      addEventChip(
        "deny",
        tool ? `已拒绝「${toolLabel(tool)}」` : "已拒绝该操作",
        tool ? `工具：${tool}` : "该工具调用已按你的选择拒绝执行。"
      );
    }
  } catch (error) {
    addMessage("error", `审批失败：${error.message}`);
  } finally {
    writeTargetSid = prevTarget;
  }
}

// ---------- 用户提问卡（ask_user：回合挂起，等你回答） ----------

function showQuestionCard(payload) {
  const card = document.createElement("div");
  card.className = "msg question";
  card.dataset.questionId = payload.question_id || "";
  state.pendingQuestion = payload.question_id || null;

  const title = document.createElement("div");
  title.className = "question-title";
  title.textContent = "助手需要你确认";
  const text = document.createElement("div");
  text.className = "question-text";
  text.textContent = payload.question || "";
  card.append(title, text);

  const options = Array.isArray(payload.options) ? payload.options.filter(Boolean) : [];
  if (options.length) {
    const row = document.createElement("div");
    row.className = "question-options";
    for (const option of options) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = option;
      button.addEventListener("click", () => submitQuestionAnswer(card, option));
      row.appendChild(button);
    }
    card.appendChild(row);
  }

  const form = document.createElement("form");
  form.className = "question-form";
  const input = document.createElement("input");
  input.type = "text";
  input.placeholder = options.length ? "或输入其他回答…" : "输入你的回答后回车…";
  const submit = document.createElement("button");
  submit.type = "submit";
  submit.textContent = "回答";
  form.append(input, submit);
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    submitQuestionAnswer(card, input.value);
  });
  card.appendChild(form);

  newMessageBlock(card);
  input.focus();
}

async function submitQuestionAnswer(card, answer) {
  const questionId = card.dataset.questionId;
  const value = String(answer || "").trim();
  if (!questionId || !value || card.dataset.answered === "1") return;
  card.dataset.answered = "1";
  // 立即锁定输入，避免重复提交；服务端 user_answered 事件补最终状态。
  const form = card.querySelector(".question-form");
  if (form) {
    const input = form.querySelector("input");
    if (input) input.disabled = true;
    const button = form.querySelector("button");
    if (button) button.disabled = true;
  }
  setRunPhase("thinking", "已提交回答，继续执行…");
  try {
    await api(`/session/${state.sessionId}/answer/${questionId}`, {
      method: "POST",
      body: JSON.stringify({ answer: value }),
    });
    markQuestionResult(card, "user", value);
  } catch (error) {
    card.dataset.answered = "";
    if (form) {
      const input = form.querySelector("input");
      if (input) input.disabled = false;
      const button = form.querySelector("button");
      if (button) button.disabled = false;
    }
    addMessage("error", `回答提交失败：${friendlyError(error)}`);
  }
}

function markQuestionAnswered(payload) {
  const questionId = payload.question_id || "";
  const card = document.querySelector(
    `.msg.question[data-question-id="${CSS.escape(questionId)}"]`
  );
  if (!card) return;
  if (state.pendingQuestion === questionId) state.pendingQuestion = null;
  markQuestionResult(card, payload.source || "user", payload.answer || "");
}

function markQuestionResult(card, source, answer) {
  card.classList.add("is-answered");
  const form = card.querySelector(".question-form");
  if (form) form.remove();
  const options = card.querySelector(".question-options");
  if (options) options.remove();
  if (card.querySelector(".question-stamp")) return;
  const stamp = document.createElement("div");
  stamp.className = "question-stamp";
  if (source === "user") stamp.textContent = `已回答：${answer}`;
  else if (source === "timeout") stamp.textContent = "提问长时间未回答，助手已按已有信息继续";
  else if (source === "aborted") stamp.textContent = "回合已中止，该提问作废";
  else stamp.textContent = "回合已结束，该提问作废";
  card.appendChild(stamp);
}

/// 回合终态兜底：若提问卡仍未作答（SSE 断流/回合提前结束），显式作废，
/// 不让界面上留一张还能输入的「僵尸提问卡」。
function settlePendingQuestion(source = "ended") {
  const questionId = state.pendingQuestion;
  if (!questionId) return;
  state.pendingQuestion = null;
  const card = document.querySelector(
    `.msg.question[data-question-id="${CSS.escape(questionId)}"]`
  );
  if (card) markQuestionResult(card, source, "");
}

// ---------- diff 审阅 ----------

async function refreshDiff(sessionId) {
  const list = $("diffList");
  list.innerHTML = "";
  if (!sessionId) return;
  try {
    const diffs = await api(`/session/${sessionId}/diff`);
    for (const diff of diffs) {
      const li = document.createElement("li");
      li.className = "diff-item";
      const changed = diff.before != null && diff.after != null ? "修改" : diff.after != null ? "新增" : "删除";
      const marker = changed === "删除" ? "🗑" : changed === "新增" ? "➕" : "✏️";
      li.innerHTML = `<strong>${marker} ${esc(diff.path)}</strong><span class="sub">${changed}</span>`;
      const body = document.createElement("pre");
      body.className = "diff-body hidden";
      body.textContent = diffText(diff);
      li.appendChild(body);
      li.addEventListener("click", (event) => {
        if (event.target.tagName === "BUTTON") return;
        body.classList.toggle("hidden");
      });
      list.appendChild(li);
    }
    if (!diffs.length) list.innerHTML = '<li class="sub">暂无改动</li>';
  } catch (_) {
    list.innerHTML = '<li class="sub">无会话或读取失败</li>';
  }
}

// 生成行级 diff 文本（统一格式，参照 git diff 风格）。
function diffText(diff) {
  const beforeLines = diff.before != null ? diff.before.split("\n") : [];
  const afterLines = diff.after != null ? diff.after.split("\n") : [];
  const lines = [];
  const maxLen = Math.max(beforeLines.length, afterLines.length);
  for (let index = 0; index < maxLen; index++) {
    const before = index < beforeLines.length ? beforeLines[index] : null;
    const after = index < afterLines.length ? afterLines[index] : null;
    if (before === null) lines.push(`+ ${after}`);
    else if (after === null) lines.push(`- ${before}`);
    else if (before !== after) {
      lines.push(`- ${before}`);
      lines.push(`+ ${after}`);
    } else {
      lines.push(`  ${before}`);
    }
  }
  return lines.join("\n");
}

async function revertAll() {
  if (!state.sessionId) return;
  const ok = await askConfirm({
    title: "回滚改动",
    message: "确定回滚当前会话全部写操作？",
    confirmText: "回滚",
    kind: "danger",
  });
  if (!ok) return;
  await api(`/session/${state.sessionId}/revert`, { method: "POST" });
  await refreshDiff(state.sessionId);
  addMessage("system", "已回滚全部改动");
}

// ---------- 技能中心 ----------

// 技能页（Codex 风格）：已安装网格（✓ 标记启用态）+ 查看/编辑弹窗。
async function refreshSkills() {
  const grid = $("skillGrid");
  if (!grid) return;
  try {
    const skills = await api("/skills");
    const count = $("skillCount");
    if (count) count.textContent = skills.length ? `${skills.length} 个` : "暂无";
    grid.innerHTML = "";
    for (const skill of skills) grid.appendChild(skillCard(skill));
    if (!skills.length) grid.innerHTML = '<div class="sub">暂无技能</div>';
  } catch (error) {
    grid.innerHTML = `<div class="sub">${esc(friendlyError(error, { resource: true }))}</div>`;
  }
}

function skillCard(skill) {
  const enabled = skill.enabled !== false;
  const card = document.createElement("div");
  card.className = "ps-card";
  const initial = esc((skill.name || "?").trim().slice(0, 1).toUpperCase());
  card.innerHTML =
    '<div class="ps-card-top">' +
    `<span class="ps-tile" aria-hidden="true">${initial}</span>` +
    '<div class="ps-card-meta">' +
    `<strong>${esc(skill.name)}</strong>` +
    `<span class="sub">${enabled ? "已启用" : "已禁用"}</span>` +
    "</div>" +
    (enabled ? '<span class="ps-check" title="已启用">✓</span>' : "") +
    "</div>" +
    `<p class="ps-card-desc">${esc(skill.description || "暂无描述")}</p>` +
    '<div class="ps-card-foot">' +
    '<span class="sub">SKILL.md</span>' +
    '<span class="ps-card-actions">' +
    '<button type="button" class="ps-card-btn" data-act="view">查看</button>' +
    '<button type="button" class="ps-card-btn" data-act="edit">编辑</button>' +
    `<button type="button" class="ps-card-btn" data-act="toggle">${enabled ? "禁用" : "启用"}</button>` +
    "</span>" +
    "</div>";
  const [viewBtn, editBtn, toggleBtn] = card.querySelectorAll("button");
  viewBtn.addEventListener("click", async () => {
    try {
      const detail = await api(`/skills/${encodeURIComponent(skill.name)}`);
      openModal({
        title: skill.name,
        body: `<div class="ps-skill-path sub">${esc(detail.path || "")}</div><pre class="ps-skill-content">${esc(detail.content || "（空）")}</pre>`,
        actions: [{ label: "关闭", kind: "ghost", onClick: ({ close }) => close() }],
      });
    } catch (error) {
      showToast(`查看失败：${friendlyError(error, { resource: true })}`);
    }
  });
  editBtn.addEventListener("click", async () => {
    try {
      const detail = await api(`/skills/${encodeURIComponent(skill.name)}`);
      openModal({
        title: `编辑 ${skill.name}`,
        body: `<p class="sub" style="margin:0 0 8px">${esc(detail.path || "")}（注册表内技能重启核心服务后生效）</p><textarea id="skillEditArea" rows="14" spellcheck="false">${esc(detail.content || "")}</textarea>`,
        actions: [
          { label: "取消", kind: "ghost", onClick: ({ close }) => close() },
          {
            label: "保存",
            kind: "primary",
            onClick: async ({ close }) => {
              const content = $("skillEditArea").value;
              try {
                await api(`/skills/${encodeURIComponent(skill.name)}`, {
                  method: "POST",
                  body: JSON.stringify({ content }),
                });
                showToast(`技能 ${skill.name} 已保存`);
                close();
              } catch (error) {
                showToast(`保存失败：${friendlyError(error, { resource: true })}`);
              }
            },
          },
        ],
      });
    } catch (error) {
      showToast(`读取失败：${friendlyError(error, { resource: true })}`);
    }
  });
  toggleBtn.addEventListener("click", async () => {
    try {
      await api(`/skills/${encodeURIComponent(skill.name)}/enabled`, {
        method: "POST",
        body: JSON.stringify({ enabled: !enabled }),
      });
      showToast(`技能 ${skill.name} 已${enabled ? "禁用" : "启用"}（即时生效）`);
      await refreshSkills();
    } catch (error) {
      showToast(`操作失败：${friendlyError(error, { resource: true })}`);
    }
  });
  return card;
}

// ---------- 情景记忆 ----------

async function refreshObservations() {
  try {
    const data = await api("/memory/observations?limit=30");
    const list = $("observationList");
    list.innerHTML = "";
    for (const observation of data.observations || []) {
      const li = document.createElement("li");
      li.innerHTML =
        `<strong>${esc(observation.app_id)}｜${esc(observation.kind)}</strong>` +
        `<span class="sub">${esc(observation.summary)}</span>` +
        `<span class="sub">${esc((observation.ts || "").slice(0, 19).replace("T", " "))}</span>`;
      list.appendChild(li);
    }
    if (!(data.observations || []).length) list.innerHTML = '<li class="sub">暂无观察记录</li>';
  } catch (error) {
    $("observationList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function recallMemory() {
  const query = $("recallQ").value.trim();
  if (!query) return;
  try {
    const data = await api(`/memory/recall?q=${encodeURIComponent(query)}&top_k=8`);
    const list = $("recallList");
    list.innerHTML = "";
    for (const hit of data.hits || []) {
      const li = document.createElement("li");
      const score = hit.confidence != null ? `（${(hit.confidence * 100).toFixed(0)}%）` : "";
      li.innerHTML =
        `<strong>${esc(hit.app_id || "")} ${score}</strong>` +
        `<span class="sub">${esc(hit.summary || "")}</span>` +
        `<span class="sub">${esc((hit.ts || "").slice(0, 19).replace("T", " "))}</span>`;
      list.appendChild(li);
    }
    if (!(data.hits || []).length) list.innerHTML = '<li class="sub">无匹配结果</li>';
  } catch (error) {
    $("recallList").innerHTML = `<li class="sub">检索失败：${esc(error.message)}</li>`;
  }
}

// ---------- 技能健康度 ----------

async function refreshSkillHealth() {
  const container = $("skillHealth");
  if (!container) return;
  try {
    const data = await api("/skills/health");
    const skills = data.skills || [];
    container.innerHTML = "";
    for (const skill of skills) {
      const row = document.createElement("div");
      row.className = "ps-row";
      const badge = skill.state === "active" ? "✅" : skill.state === "degraded" ? "⚠️" : "⛔";
      row.innerHTML =
        `<span class="ps-row-icon">${badge}</span>` +
        '<div class="ps-row-meta">' +
        `<strong>${esc(skill.name)}</strong>` +
        `<span class="sub">${esc(skill.state)} ｜ 成功 ${skill.successes}/${skill.attempts} ｜ 成功率 ${(skill.success_rate * 100).toFixed(0)}% ｜ 模板命中 ${(skill.template_hit_rate * 100).toFixed(0)}% ｜ 连续失败 ${skill.consecutive_failures}</span>` +
        "</div>";
      container.appendChild(row);
    }
    if (!skills.length) container.innerHTML = '<div class="sub">暂无技能健康度数据</div>';
  } catch (error) {
    container.innerHTML = `<div class="sub">${esc(friendlyError(error))}</div>`;
  }
}

// ---------- 插件与技能页：tabs 切换与刷新入口 ----------

function initPluginSkillTabs() {
  const tabs = document.querySelectorAll(".ps-tab");
  for (const tab of tabs) {
    tab.addEventListener("click", () => {
      for (const item of tabs) {
        const active = item === tab;
        item.classList.toggle("active", active);
        item.setAttribute("aria-selected", String(active));
      }
      for (const pane of document.querySelectorAll(".ps-pane")) {
        pane.hidden = pane.dataset.psPane !== tab.dataset.psTab;
      }
    });
  }
  const on = (id, fn) => {
    const el = $(id);
    if (el) el.addEventListener("click", fn);
  };
  on("pluginRefreshBtn", () => {
    refreshPlugins();
    refreshPluginMarket();
  });
  on("marketRefreshBtn", () => refreshPluginMarket());
  on("skillRefreshBtn", () => {
    refreshSkills();
    refreshSkillHealth();
  });
}

// ---------- Eval 评估 ----------

async function runEval() {
  const suiteId = $("evalSuite").value;
  const button = $("evalRunBtn");
  const box = $("evalReport");
  button.disabled = true;
  button.textContent = "运行中…";
  box.className = "owo-result";
  box.innerHTML = '<span class="owo-result-empty">评估运行中（调用真实模型，请稍候）…</span>';
  try {
    const report = await api("/eval/run", {
      method: "POST",
      body: JSON.stringify({ suite_id: suiteId }),
    });
    const rate = Number(report.pass_rate || 0) * 100;
    const allPass = report.passed === report.total;
    let html =
      '<div class="owo-result-head">' +
      `<span class="owo-tag ${allPass ? "ok" : "bad"}">通过 <strong>${report.passed}/${report.total}</strong></span>` +
      `<span class="owo-tag">${rate.toFixed(1)}%</span>` +
      `<span class="owo-tag">总耗时 ${((report.total_duration_ms || 0) / 1000).toFixed(1)}s</span>` +
      `<span class="owo-tag">${esc(report.suite || suiteId)}</span>` +
      "</div>" +
      '<div class="owo-rows">';
    for (const caseResult of report.cases || []) {
      const ok = !!caseResult.passed;
      html +=
        '<div class="owo-row">' +
        `<span class="owo-row-mark ${ok ? "ok" : "bad"}">${ok ? "✔" : "✘"}</span>` +
        `<span class="owo-row-main" title="${esc(caseResult.name)}">${esc(caseResult.name)}</span>` +
        `<span class="owo-row-meta">${((caseResult.duration_ms || 0) / 1000).toFixed(1)}s · ${caseResult.steps || 0} 步</span>` +
        (caseResult.error ? `<span class="owo-row-note">${esc(caseResult.error)}</span>` : "") +
        "</div>";
    }
    html += "</div>";
    if (!(report.cases || []).length) html += '<span class="owo-result-empty">该套件没有用例</span>';
    box.innerHTML = html;
  } catch (error) {
    box.className = "owo-result";
    box.innerHTML = `<span class="owo-result-empty">评估失败：${esc(error.message)}</span>`;
  } finally {
    button.disabled = false;
    button.textContent = "运行评估";
  }
}

// ---------- Traces 可观测（v0.5.6） ----------

async function refreshTraces() {
  try {
    const data = await api("/traces");
    const list = $("traceList");
    list.innerHTML = "";
    const traces = data.traces || [];
    for (let index = 0; index < traces.length; index++) {
      const trace = traces[index];
      const li = document.createElement("li");
      const final = trace.has_final ? "✅" : "—";
      const usage = trace.usage && trace.usage.total_tokens ? ` ｜ ${trace.usage.total_tokens} tokens` : "";
      li.innerHTML =
        `<strong>${index} ${esc(trace.prompt_preview || trace.prompt || "")}</strong>` +
        `<span class="sub">${final} ${(trace.duration_ms / 1000).toFixed(1)}s ｜ ${trace.steps} 步 ｜ ${esc(trace.model)}${usage}</span>` +
        `<span class="sub">${esc((trace.started_at || "").slice(0, 19).replace("T", " "))}</span>`;
      li.addEventListener("click", () => showTrace(index));
      list.appendChild(li);
    }
    if (!traces.length) list.innerHTML = '<li class="sub">暂无轨迹（完成回合后自动记录）</li>';
  } catch (error) {
    $("traceList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function showTrace(index) {
  const box = $("traceReplay");
  box.className = "owo-result";
  box.innerHTML = '<span class="owo-result-empty">回放加载中…</span>';
  try {
    const trace = await api(`/traces/${index}`);
    const usage = trace.usage || {};
    const tags = [
      `<span class="owo-tag">${esc(trace.model || "未知模型")}</span>`,
      `<span class="owo-tag">${((trace.duration_ms || 0) / 1000).toFixed(1)}s</span>`,
      `<span class="owo-tag">${trace.steps || 0} 步</span>`,
    ];
    if (usage.total_tokens) {
      tags.push(
        `<span class="owo-tag">${usage.total_tokens} tokens` +
          (usage.prompt_tokens || usage.completion_tokens
            ? `（↑${usage.prompt_tokens || 0} ↓${usage.completion_tokens || 0}）`
            : "") +
          "</span>"
      );
    }
    let html =
      '<div class="owo-result-head">' + tags.join("") + "</div>" +
      '<div class="owo-rows">' +
      '<div class="owo-row"><span class="owo-row-main" title="' + esc(trace.prompt || "") + '">' +
      esc(trace.prompt || "（无 prompt）") +
      "</span></div>" +
      "</div>" +
      '<ol class="owo-timeline">';
    // token 流式增量合并成一条，避免时间线被几十条 delta 淹没。
    let deltaCount = 0;
    let deltaChars = 0;
    const flushDelta = () => {
      if (!deltaCount) return;
      html +=
        '<li><strong>流式输出</strong> <code>' +
        `${deltaCount} 段 / ${deltaChars} 字符` +
        "</code></li>";
      deltaCount = 0;
      deltaChars = 0;
    };
    for (const event of trace.events || []) {
      const type = event.type || "?";
      if (type === "token_delta") {
        deltaCount += 1;
        deltaChars += (event.delta || "").length;
        continue;
      }
      flushDelta();
      if (type === "model_call") {
        html += '<li class="ev-tool"><strong>模型调用</strong></li>';
      } else if (type === "tool_start") {
        html += `<li class="ev-tool"><strong>工具开始</strong> <code>${esc(event.tool || "")}</code></li>`;
      } else if (type === "tool_result") {
        html +=
          `<li class="${event.ok ? "ev-ok" : "ev-bad"}"><strong>工具结果</strong> ` +
          `<code>${esc(event.tool || "")}${event.ok ? "" : "（失败：" + esc(event.error || "未知") + "）"}</code></li>`;
      } else if (type === "permission_request") {
        html +=
          `<li class="ev-warn"><strong>审批请求</strong> <code>${esc(event.tool || "")}` +
          `${event.reason ? " · " + esc(event.reason) : ""}</code></li>`;
      } else if (type === "compaction") {
        html += `<li class="ev-warn"><strong>上下文压缩</strong> <code>${esc(event.summary || "")}</code></li>`;
      } else if (type === "final") {
        html += `<li class="ev-ok"><strong>最终汇报</strong> <code>${esc((event.text || "").slice(0, 160))}</code></li>`;
      } else {
        html += `<li><code>${esc(type)}</code></li>`;
      }
    }
    flushDelta();
    html += "</ol>";
    if (!(trace.events || []).length) html += '<span class="owo-result-empty">该轨迹没有事件</span>';
    box.innerHTML = html;
  } catch (error) {
    box.className = "owo-result";
    box.innerHTML = `<span class="owo-result-empty">回放失败：${esc(friendlyError(error, { resource: true }))}</span>`;
  }
}

async function exportSession(format) {
  if (!state.sessionId) {
    showToast("请先选择会话", "error");
    return;
  }
  try {
    const response = await apiRaw(
      `/session/${state.sessionId}/export/${format}`
    );
    if (!response.ok) throw new Error(await response.text());
    const blob = await response.blob();
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `session-${state.sessionId.slice(0, 8)}.${format === "html" ? "html" : "md"}`;
    link.click();
    URL.revokeObjectURL(url);
    addMessage("system", `已导出会话为 ${format.toUpperCase()}`);
  } catch (error) {
    addMessage("error", `导出失败：${friendlyError(error, { resource: true })}`);
  }
}

// ---------- 子代理 / 项目规则 / MCP 管理（v0.5.5，对标 Codex @explore/@subagent、AGENTS.md、/mcp） ----------

async function runSubagent() {
  const prompt = $("subagentPrompt").value.trim();
  if (!prompt) {
    addMessage("system", "请输入子代理任务");
    return;
  }
  const readOnly = $("subagentMode").value === "read_only";
  const button = $("subagentRunBtn");
  const box = $("subagentResult");
  button.disabled = true;
  button.textContent = "运行中…";
  box.className = "owo-result";
  box.innerHTML = '<span class="owo-result-empty">子代理执行中（调用真实模型，请稍候）…</span>';
  try {
    const result = await api("/subagent/run", {
      method: "POST",
      body: JSON.stringify({ prompt, read_only: readOnly }),
    });
    box.innerHTML =
      '<div class="owo-result-head">' +
      `<span class="owo-tag ${result.read_only ? "" : "warn"}">${result.read_only ? "只读探索" : "通用子代理"}</span>` +
      `<span class="owo-tag">${((result.duration_ms || 0) / 1000).toFixed(1)}s</span>` +
      "</div>" +
      `<div class="owo-result-body">${renderMarkdown(result.text || "（无汇报）")}</div>`;
  } catch (error) {
    box.className = "owo-result";
    box.innerHTML = `<span class="owo-result-empty">子代理失败：${esc(error.message)}</span>`;
  } finally {
    button.disabled = false;
    button.textContent = "运行子代理";
  }
}

async function refreshProjectRules() {
  try {
    const data = await api("/project/rules");
    const info = $("projectRulesInfo");
    info.innerHTML = "";
    for (const rule of data.rules || []) {
      const badge = rule.exists ? (rule.injected ? "✅ 注入" : "⚠️ 未注入") : "— 不存在";
      const div = document.createElement("div");
      div.textContent = `${rule.name}：${badge}`;
      info.appendChild(div);
      if (rule.name === "AGENTS.md") {
        const editor = $("agentsEditor");
        if (!editor.dataset.seeded) {
          editor.value = rule.content || "";
          editor.dataset.seeded = "1";
        }
      }
    }
  } catch (error) {
    $("projectRulesInfo").textContent = friendlyError(error);
  }
}

async function saveAgentsRules() {
  const content = $("agentsEditor").value;
  try {
    const result = await api("/project/rules", {
      method: "POST",
      body: JSON.stringify({ content }),
    });
    addMessage("system", `已保存 AGENTS.md（${result.chars} 字符），下次会话注入生效`);
    await refreshProjectRules();
  } catch (error) {
    addMessage("error", `保存失败：${error.message}`);
  }
}

async function generateAgentsTemplate() {
  try {
    const result = await api("/project/rules/template", { method: "POST" });
    showToast(`已生成 AGENTS.md 模板（${result.chars} 字符）`, "ok");
    $("agentsEditor").dataset.seeded = "0";
    await refreshProjectRules();
  } catch (error) {
    const msg = String((error && error.message) || error || "");
    if (msg.startsWith("409")) showToast("AGENTS.md 已存在，未生成模板（幂等）", "error");
    else showToast(`生成失败：${friendlyError(error)}`, "error");
  }
}

async function refreshMcp() {
  try {
    const data = await api("/mcp");
    const list = $("mcpList");
    const configured = data.servers || [];
    const connected = data.connected || [];
    list.innerHTML = "";
    let rendered = 0;
    for (const server of configured) {
      const li = document.createElement("li");
      const target = server.transport === "http" ? server.url : server.command;
      const isUp = connected.includes(server.name);
      li.innerHTML = `<strong>${esc(server.name)}</strong><span class="sub">${esc(server.transport)} ｜ ${esc(target || "")}${server.args && server.args.length ? " " + esc(server.args.join(" ")) : ""}${isUp ? " ｜ 已连接" : ""}</span>`;
      const removeBtn = document.createElement("button");
      removeBtn.textContent = "移除";
      removeBtn.addEventListener("click", async () => {
        try {
          await api("/mcp/remove", {
            method: "POST",
            body: JSON.stringify({ name: server.name }),
          });
          await refreshMcp();
          addMessage("system", `已移除 MCP 服务器 ${server.name}`);
        } catch (error) {
          addMessage("error", `移除失败：${friendlyError(error, { resource: true })}`);
        }
      });
      li.appendChild(removeBtn);
      list.appendChild(li);
      rendered++;
    }
    // 已连接但不在持久化配置里的服务器（插件 manifest 声明的 stdio 服务器随插件启用接入）：
    // 不展示就会出现「实际连着 3 个、页面却说没有」的误导。
    for (const name of connected) {
      if (configured.some((server) => server.name === name)) continue;
      const li = document.createElement("li");
      li.innerHTML = `<strong>${esc(name)}</strong><span class="sub">已连接（来源：插件 manifest，随插件启用自动接入，不可单独移除）</span>`;
      list.appendChild(li);
      rendered++;
    }
    if (!rendered) {
      list.innerHTML =
        '<li class="sub">暂无 MCP 服务器：可在下方表单添加，或启用带 mcp 段的插件</li>';
    }
  } catch (error) {
    $("mcpList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function addMcpServer() {
  const name = $("mcpName").value.trim();
  const transport = $("mcpTransport").value;
  const command = $("mcpCommand").value.trim();
  const url = $("mcpUrl").value.trim();
  if (!name) return;
  if (transport === "stdio" && !command) {
    addMessage("error", "stdio 传输需要填写启动命令");
    return;
  }
  if (transport === "http" && !url) {
    addMessage("error", "http 传输需要填写 URL");
    return;
  }
  try {
    const result = await api("/mcp/add", {
      method: "POST",
      body: JSON.stringify({ name, transport, command, url: url || null }),
    });
    addMessage("system", `MCP 服务器 ${name} 已连接（${result.tools} 个工具）`);
    $("mcpName").value = "";
    $("mcpCommand").value = "";
    $("mcpUrl").value = "";
    await refreshMcp();
  } catch (error) {
    addMessage("error", `添加失败：${error.message}`);
  }
}

// ---------- 白名单 ----------

async function refreshWhitelist() {
  try {
    const entries = await api("/whitelist");
    const list = $("whitelistList");
    list.innerHTML = "";
    for (const entry of entries) {
      const li = document.createElement("li");
      li.innerHTML = `<strong>${esc(entry.name)}</strong><span class="sub">${esc(entry.app_id)} ｜ ${esc(entry.tier)} ｜ 操作:${entry.auto_ops_allowed ? "开" : "关"}</span>`;
      const removeBtn = document.createElement("button");
      removeBtn.textContent = "移除";
      removeBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          await api("/whitelist/manage", {
            method: "POST",
            body: JSON.stringify({ action: "remove", app_id: entry.app_id }),
          });
          await refreshWhitelist();
        } catch (error) {
          addMessage("error", `白名单删除失败：${error.message || error}`);
        }
      });
      li.appendChild(removeBtn);
      list.appendChild(li);
    }
  } catch (error) {
    $("whitelistList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

// ---------- Computer-use 任务级审批（P2，文档 7.3 语义：批准目标应用+描述+最长时长+允许动作） ----------

async function refreshComputerTasks() {
  try {
    const data = await api("/computer-use/tasks");
    const list = $("computerTaskList");
    list.innerHTML = "";
    const tasks = data.tasks || [];
    for (const task of tasks) {
      const li = document.createElement("li");
      const status = String(task.state || "unknown").toLowerCase();
      const elapsed = task.elapsed_ms != null ? `${Math.round(task.elapsed_ms / 1000)}s / ` : "";
      const cap = task.max_duration_ms != null ? `${Math.round(task.max_duration_ms / 1000)}s` : "无上限";
      li.innerHTML =
        `<strong>${esc(task.target_app || task.app || "")}：${esc(task.description || "")}</strong>` +
        `<span class="sub">${esc(status)} ｜ ${elapsed}${cap} ｜ 动作 ${esc((task.allowed_actions || []).join(",")) || "全部"}</span>`;
      if (status.startsWith("pending")) {
        const approve = document.createElement("button");
        approve.textContent = "批准";
        approve.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/approve`, { method: "POST", body: JSON.stringify({}) });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `批准失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(approve);
        const deny = document.createElement("button");
        deny.textContent = "拒绝";
        deny.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/reject`, { method: "POST", body: JSON.stringify({}) });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `拒绝失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(deny);
        const cancel = document.createElement("button");
        cancel.textContent = "取消";
        cancel.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/cancel`, { method: "POST", body: JSON.stringify({}) });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `取消失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(cancel);
      } else if (status.startsWith("running")) {
        const pause = document.createElement("button");
        pause.textContent = "暂停";
        pause.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/pause`, { method: "POST", body: JSON.stringify({}) });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `暂停失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(pause);
        const abort = document.createElement("button");
        abort.textContent = "终止";
        abort.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/cancel`, { method: "POST", body: JSON.stringify({}) });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `终止失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(abort);
      } else if (status.startsWith("paused")) {
        const resume = document.createElement("button");
        resume.textContent = "恢复";
        resume.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/resume`, { method: "POST", body: JSON.stringify({}) });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `恢复失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(resume);
      }
      list.appendChild(li);
    }
    if (!tasks.length) list.innerHTML = '<li class="sub">暂无 computer-use 任务</li>';
  } catch (error) {
    $("computerTaskList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function createComputerTask() {
  const targetApp = $("cuApp").value.trim();
  const description = $("cuDesc").value.trim();
  if (!targetApp || !description) {
    addMessage("system", "请填写目标应用与任务描述");
    return;
  }
  const maxDurationMs = (parseInt($("cuMaxDur").value, 10) || 120) * 1000;
  const allowedActions = $("cuActions").value.split(",").map((s) => s.trim()).filter(Boolean);
  try {
    const result = await api("/computer-use/task", {
      method: "POST",
      body: JSON.stringify({
        target_app: targetApp,
        description,
        max_duration_ms: maxDurationMs,
        allowed_actions: allowedActions,
      }),
    });
    addMessage("system", `已创建 computer-use 任务（${result.id}），等待任务级审批`);
    $("cuApp").value = "";
    $("cuDesc").value = "";
    $("cuMaxDur").value = "";
    $("cuActions").value = "";
    await refreshComputerTasks();
  } catch (error) {
    addMessage("error", `创建失败：${friendlyError(error)}`);
  }
}

// ---------- 扩展面板（第四轮：notes / plugin-market / workflow / goal；第五轮：team / eval / observability / memory / command） ----------

// 挂载顺序（与 index.html 的 script 引入顺序一致）。
const PANEL_ORDER = [
  "notes",
  "automations",
  "plugin-market",
  "workflow",
  "goal",
  "team",
  "eval",
  "observability",
  "memory",
  "command",
  "fleet",
  "capabilities",
  "action-center",
  "project-history",
  "project-launcher",
  "workswarm",
  "about",
];

function panelHelpers() {
  return {
    baseUrl: API_BASE,
    get(path) {
      return api(path);
    },
    post(path, body) {
      return api(path, { method: "POST", body: JSON.stringify(body || {}) });
    },
    put(path, body) {
      return api(path, { method: "PUT", body: JSON.stringify(body || {}) });
    },
    del(path) {
      return api(path, { method: "DELETE" });
    },
    // 任意 method/无 body 的请求（如自动化 toggle/clear 的空体 POST）。
    call(path, options) {
      return api(path, options);
    },
    // 面板内统一走样式化弹窗（原生 confirm/prompt 会阻塞渲染且无法定制）
    confirm: askConfirm,
    prompt: askText,
    esc,
    friendlyError,
    renderMarkdown,
  };
}

let currentPanel = null;

function mountPanel(id, writeHash = true) {
  const panel = window.OwoPanels && window.OwoPanels[id];
  const root = $("panelRoot");
  if (!panel || !root) return;
  // 切出带 dispose 生命周期的面板（workswarm 等有内部定时器/监听）时先清理。
  if (currentPanel && currentPanel !== id) {
    const prev = window.OwoPanels[currentPanel];
    if (prev && typeof prev.dispose === "function") {
      try { prev.dispose(); } catch { /* 清理失败不打断挂载 */ }
    }
  }
  currentPanel = id;
  // 面板深链：#<panel-id>，便于分享/直达/自动化验证（不改动其它状态）。
  // 引导时的默认挂载传 writeHash=false：否则地址栏被钉上 #notes，刷新后
  // applyDeepLink 又把它当深链 → 每次打开工作台都停在工具视图（用户实测反馈）。
  if (writeHash && location.hash !== "#" + id) {
    history.replaceState(null, "", "#" + id);
  }
  for (const button of document.querySelectorAll("#panelNav button")) {
    button.classList.toggle("active", button.dataset.panel === id);
  }
  panel.mount(root, panelHelpers());
  layoutPanel(root);
}

/// 扩展面板统一排版：把面板根容器的"卡片型"子元素排成响应式两列网格，
/// 宽内容（表格/编辑区/表单/代码块）整行跨列。各面板根容器类名不一
/// （`.stack` / 自有类 / 直接挂在 section 下），因此在挂载后按 DOM 结构判定，
/// 免去逐个改面板 HTML。
function layoutPanel(root) {
  const section = root.querySelector("section[data-panel]");
  if (!section) return;
  const isBox = (el) => el.tagName === "DIV" || el.tagName === "SECTION";
  const wrapper = Array.from(section.children).find(isBox);
  if (wrapper) buildPanelToc(section, wrapper);
  // 先试「单一根容器」（.stack 等），不行再退回把 section 自身当容器
  // （notes/team/fleet/plugin-market 的骨架直接挂在 section 下）。
  const candidates = [];
  if (wrapper && wrapper.children.length >= 3) candidates.push(wrapper);
  if (section !== wrapper) candidates.push(section);
  for (const container of candidates) {
    const children = Array.from(container.children).filter((el) => el.tagName !== "STYLE");
    if (children.length < 3) continue;
    const wideFlags = children.map((child) => panelBlockIsWide(child));
    const cards = wideFlags.filter((flag) => !flag).length;
    // 可分的卡片太少（几乎全是整行块）就不折腾，保持原单列。
    if (cards < 2 || cards / children.length < 0.2) continue;
    children.forEach((child, index) => {
      if (wideFlags[index]) child.classList.add("owo-span-all");
    });
    container.classList.add("owo-cols");
    // 自愈：
    // ① 内容溢出（超宽表格/长串）→ 整行跨列，避免横向滚动；
    // ② 异常高的块（长表/图表/长编辑器）→ 限高内滚，避免把整页拉成"一长条"。
    // 面板内容多为异步加载，块高度在挂载后才长出来 → 用 ResizeObserver 持续自愈。
    const heal = () => {
      for (const child of children) {
        if (child.scrollWidth > child.clientWidth + 4) {
          child.classList.add("owo-span-all");
        } else if (child.getBoundingClientRect().height > 700) {
          child.classList.add("owo-tall");
        }
      }
    };
    requestAnimationFrame(heal);
    if (window.ResizeObserver) {
      let pending = false;
      const observer = new ResizeObserver(() => {
        if (pending) return;
        pending = true;
        requestAnimationFrame(() => {
          pending = false;
          heal();
        });
      });
      observer.observe(container);
    }
    return;
  }
}

/// 当前面板目录的滚动联动函数（document 捕获阶段滚动监听只注册一次）。
let panelTocSync = null;

/// 面板分区目录（锚点侧栏）：分区标题（`.sub` / H2~H4）≥3 个时，
/// 在面板左侧生成吸顶目录，点击滚到对应分区——长面板不必一路往下找。
function buildPanelToc(section, container) {
  const headings = Array.from(container.children).filter(
    (el) => el.classList.contains("sub") || /^H[2-4]$/.test(el.tagName)
  );
  if (headings.length < 3) return;
  const nav = document.createElement("nav");
  nav.className = "owo-toc";
  nav.setAttribute("aria-label", "面板分区");
  const items = [];
  headings.forEach((heading, index) => {
    if (!heading.id) heading.id = `owo-sec-${index}-${Math.random().toString(36).slice(2, 7)}`;
    const label = (heading.textContent || "").trim().replace(/\s+/g, " ");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "owo-toc-item";
    button.textContent = label.length > 16 ? `${label.slice(0, 16)}…` : label || `分区 ${index + 1}`;
    button.title = label;
    button.addEventListener("click", () => {
      heading.scrollIntoView({ behavior: "smooth", block: "start" });
      setActiveTocItem(items, button);
    });
    items.push({ button, heading });
    nav.appendChild(button);
  });
  // 滚动联动高亮（rAF 节流）：视口顶部最近的已越过分区即为当前分区。
  // 注意滚动发生在内层容器（`body.tools-open #sidebar` 是滚动容器，html/body 不滚），
  // 因此用 document 的捕获阶段监听（scroll 不冒泡但可捕获），一次注册常驻。
  const sync = () => {
    let current = items[0];
    for (const item of items) {
      // 阈值略高于吸顶条高度：点目录跳转后（标题停在 scroll-margin-top 处）即为当前项。
      if (item.heading.getBoundingClientRect().top <= 90) current = item;
    }
    setActiveTocItem(items, current && current.button);
  };
  panelTocSync = sync;
  if (!document.body.dataset.owoTocBound) {
    document.body.dataset.owoTocBound = "1";
    let ticking = false;
    document.addEventListener(
      "scroll",
      () => {
        if (ticking || !panelTocSync) return;
        ticking = true;
        requestAnimationFrame(() => {
          ticking = false;
          panelTocSync();
        });
      },
      { capture: true, passive: true }
    );
  }
  section.insertBefore(nav, container);
  section.classList.add("owo-toc-layout");
  addSectionFolding(headings);
  sync();
  // 面板内容异步加载：块高度在挂载后才长出来，用 ResizeObserver 复查折叠条件与高亮。
  if (window.ResizeObserver) {
    let revisiting = false;
    const observer = new ResizeObserver(() => {
      if (revisiting) return;
      revisiting = true;
      requestAnimationFrame(() => {
        revisiting = false;
        addSectionFolding(headings);
        sync();
      });
    });
    observer.observe(container);
  }
}

/// 长分区折叠（内容级"精简短页"的安全做法：不删内容，可展开）：
/// 某分区（标题到下一个标题之间）的块总高 >700px 时，在标题尾部加「收起/展开」。
/// 幂等——已加过按钮的标题不重复添加（ResizeObserver 自愈会多次调用）。
function addSectionFolding(headings) {
  headings.forEach((heading, index) => {
    if (heading.querySelector(".owo-fold")) return;
    const next = headings[index + 1];
    const blocks = [];
    for (let el = heading.nextElementSibling; el && el !== next; el = el.nextElementSibling) {
      if (el.tagName !== "STYLE") blocks.push(el);
    }
    if (blocks.length < 2) return;
    const total = blocks.reduce((sum, el) => sum + el.getBoundingClientRect().height, 0);
    if (total < 700) return;
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "owo-fold";
    toggle.textContent = "收起";
    toggle.title = "折叠该分区（内容不丢，可随时展开）";
    toggle.addEventListener("click", (event) => {
      event.stopPropagation();
      const collapsed = blocks.every((el) => el.classList.contains("owo-folded"));
      for (const el of blocks) el.classList.toggle("owo-folded", !collapsed);
      toggle.textContent = collapsed ? "收起" : "展开";
    });
    heading.appendChild(toggle);
  });
}

function setActiveTocItem(items, button) {
  for (const item of items) item.button.classList.toggle("active", item.button === button);
}

/// 该面板块是否应当整行跨列（宽内容 / 标题 / 工具条）。
/// 判定偏保守：宁可整行（不会破版），也不要硬塞进窄列。
/// 注意单行 `input/select` 不算宽内容（否则可编辑行永远分不了栏）。
function panelBlockIsWide(el) {
  if (/^(H[1-4]|BUTTON|FORM|TABLE|TEXTAREA|PRE)$/.test(el.tagName)) return true;
  if (el.tagName === "UL" || el.tagName === "OL") return el.children.length > 6;
  if (
    el.matches("canvas, svg, .owo-editor") ||
    el.querySelector(
      "table, textarea, form, pre, canvas, iframe, .owo-editor, .owo-mtr-table"
    )
  ) {
    return true;
  }
  const classes = String(el.className || "").split(/\s+/);
  return classes.some(
    (name) =>
      name === "sub" ||
      name === "toolbar" ||
      name === "actions" ||
      name.endsWith("-head") ||
      name.endsWith("-toolbar") ||
      name.endsWith("-actions") ||
      name.endsWith("-bar")
  );
}

function panelFromHash() {
  const id = (location.hash || "").replace(/^#/, "");
  return id && window.OwoPanels && window.OwoPanels[id] ? id : null;
}

// 深链打开：确保工具视图可见，再把对应面板挂上。
function openPanelById(id) {
  if (!id || !window.OwoPanels || !window.OwoPanels[id]) return false;
  if (document.body.classList.contains("settings-open")) setSettingsPageVisible(false);
  setToolsVisible(true);
  mountPanel(id);
  // 深链落到面板本身：扩展面板区在工具视图下方，需要滚动过去才算"打开"。
  const target = $("panelRoot");
  if (target && target.scrollIntoView) target.scrollIntoView({ block: "start" });
  return true;
}

function initPanels() {
  const nav = $("panelNav");
  if (!nav) return;
  window.OwoPanels = window.OwoPanels || {};
  window.OwoPanels.baseUrl = API_BASE;
  nav.innerHTML = "";
  for (const id of PANEL_ORDER) {
    const panel = window.OwoPanels[id];
    if (!panel) continue;
    const button = document.createElement("button");
    button.textContent = panel.title || id;
    button.dataset.panel = id;
    button.addEventListener("click", () => mountPanel(id));
    nav.appendChild(button);
  }
  // 首屏一律落回会话视图：清掉地址栏里可能残留的面板 hash（见 mountPanel 注释）。
  // 之后手动改 hash 仍可通过 hashchange 深链（便于分享/逐页截图验证）。
  if (location.hash) {
    history.replaceState(null, "", location.pathname + location.search);
  }
  const first = PANEL_ORDER.find((id) => window.OwoPanels[id]);
  if (first) mountPanel(first, false);
}

window.addEventListener("hashchange", () => applyDeepLink());

// 深链：#<panel-id> 打开对应面板，#settings 打开设置页（便于分享与逐页截图验证）。
function applyDeepLink() {
  const raw = (location.hash || "").replace(/^#/, "");
  if (!raw) return false;
  if (raw === "settings") {
    setToolsVisible(false);
    setSettingsPageVisible(true);
    return true;
  }
  return openPanelById(panelFromHash());
}

// ---------- 事件绑定 ----------

const savedWorkspace = localStorage.getItem("owo.workspace");
if (savedWorkspace) $("workspace").value = savedWorkspace;
function applyTheme(theme) {
  const dark = theme === "dark";
  document.body.classList.toggle("dark-theme", dark);
  $("themeToggle").textContent = dark ? "☀" : "☾";
  $("themeToggle").title = dark ? "切换白色主题" : "切换深色主题";
  const side = $("themeToggleSide");
  if (side) {
    side.textContent = dark ? "☀" : "☾";
    side.title = $("themeToggle").title;
  }
  const bar = $("menubarTheme");
  if (bar) {
    bar.textContent = dark ? "☀" : "☾";
    bar.title = $("themeToggle").title;
  }
  localStorage.setItem("owo.theme", dark ? "dark" : "light");
}
applyTheme(localStorage.getItem("owo.theme") || "light");
$("themeToggle").addEventListener("click", () => {
  applyTheme(document.body.classList.contains("dark-theme") ? "light" : "dark");
});
function enableResize(handleId, variable, min, max, fromRight = false) {
  const handle = $(handleId);
  handle.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    handle.setPointerCapture(event.pointerId);
    const railWidth = $("sidebar").getBoundingClientRect().left;
    const move = (next) => {
      const raw = fromRight ? window.innerWidth - next.clientX : next.clientX - railWidth;
      document.body.style.setProperty(variable, `${Math.max(min, Math.min(max, raw))}px`);
    };
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
  });
}
enableResize("sidebarResize", "--session-width", 220, 460);
enableResize("rightResize", "--inspect-width", 260, 520, true);
// 视图状态机：默认（会话） / 工具全屏 / 设置页，三态互斥。
// show-tools 只影响 toggleTools 按钮的文案与侧栏默认显隐，tools-open 才是工具全屏。
function setToolsVisible(visible) {
  document.body.classList.toggle("show-tools", visible);
  document.body.classList.toggle("tools-open", visible);
  if (visible) document.body.classList.remove("settings-open");
  $("toggleTools").setAttribute("aria-expanded", String(visible));
  $("toggleTools").textContent = visible ? "收起工具与设置" : "显示工具与设置";
}
function setSettingsPageVisible(visible) {
  document.body.classList.toggle("settings-open", visible);
  if (visible) {
    // 设置页是独立全屏态：收起工具视图，但保留 show-tools 便于返回。
    document.body.classList.remove("tools-open");
    document.body.classList.add("show-tools");
    $("toggleTools").setAttribute("aria-expanded", "true");
    $("toggleTools").textContent = "收起工具与设置";
    document.querySelector("#sidebar section.settings-section")?.scrollIntoView({ block: "start" });
  } else {
    // 回到默认会话视图：彻底退出工具/设置全屏。
    document.body.classList.remove("tools-open", "show-tools");
    clearCodexGroup();
    $("toggleTools").setAttribute("aria-expanded", "false");
    $("toggleTools").textContent = "显示工具与设置";
  }
}
// toggleTools 仅为状态机保留（视觉入口为 codexToolsEntry / sidebarToolsBtn）
$("toggleTools").addEventListener("click", () => {
  if (document.body.classList.contains("show-tools")) setSettingsPageVisible(false);
  else setToolsVisible(true);
});
// Codex 侧栏导航：进入工具视图并聚焦对应分组；再点一次返回会话。
const CODEX_GROUPS = ["workspace", "intelligence", "automation", "system"];
function clearCodexGroup() {
  for (const group of CODEX_GROUPS) document.body.classList.remove(`tools-group-${group}`);
  document.querySelectorAll("[data-codex-group]").forEach((b) => b.classList.remove("active"));
  syncToolsJump();
}

// 工具视图吸顶导航：点分组只看该组卡片（长页面立刻变短），点「全部」恢复所有卡片。
function syncToolsJump() {
  for (const button of document.querySelectorAll("[data-jump]")) {
    const target = button.dataset.jump;
    const active =
      target === "all"
        ? !CODEX_GROUPS.some((group) => document.body.classList.contains(`tools-group-${group}`))
        : document.body.classList.contains(`tools-group-${target}`);
    button.classList.toggle("active", active);
  }
}
for (const button of document.querySelectorAll("[data-jump]")) {
  button.addEventListener("click", () => {
    const target = button.dataset.jump;
    clearCodexGroup();
    if (target !== "all") document.body.classList.add(`tools-group-${target}`);
    setToolsVisible(true);
    syncToolsJump();
    const sidebar = $("sidebar");
    if (sidebar && sidebar.scrollIntoView) {
      sidebar.scrollIntoView({ block: "start", behavior: "smooth" });
    }
  });
}
function openToolsView() {
  clearCodexGroup();
  setToolsVisible(true);
}
for (const button of document.querySelectorAll("[data-codex-group]")) {
  button.addEventListener("click", () => {
    const group = button.dataset.codexGroup;
    const focused = document.body.classList.contains(`tools-group-${group}`);
    clearCodexGroup();
    if (focused) {
      // 已聚焦该分组 → 返回会话
      setSettingsPageVisible(false);
      return;
    }
    document.body.classList.add(`tools-group-${group}`);
    button.classList.add("active");
    setToolsVisible(true);
    syncToolsJump();
  });
}
function bindToolsEntry(id) {
  $(id).addEventListener("click", () => {
    if (document.body.classList.contains("tools-open")) setSettingsPageVisible(false);
    else openToolsView();
  });
}
bindToolsEntry("codexToolsEntry");
bindToolsEntry("sidebarToolsBtn");
$("codexSettingsEntry").addEventListener("click", () => setSettingsPageVisible(true));
$("codexViewBack").addEventListener("click", () => setSettingsPageVisible(false));
$("inspectorToggle").addEventListener("click", () => {
  document.body.classList.toggle("inspector-open");
});
$("themeToggleSide").addEventListener("click", () => {
  applyTheme(document.body.classList.contains("dark-theme") ? "light" : "dark");
});

// ---------- 全局菜单栏（文件/编辑/视图/帮助 + 左下用户卡菜单） ----------

function showToast(text, kind = "") {
  let root = $("toastRoot");
  if (!root) {
    root = document.createElement("div");
    root.id = "toastRoot";
    document.body.appendChild(root);
  }
  const toast = document.createElement("div");
  toast.className = `toast${kind ? ` toast-${kind}` : ""}`;
  toast.textContent = text;
  root.appendChild(toast);
  setTimeout(() => {
    toast.style.opacity = "0";
    toast.style.transition = "opacity .3s";
    setTimeout(() => toast.remove(), 320);
  }, 2600);
}

// 菜单项图标（与侧栏同一套 16px 线性图标）
const MENU_ICONS = {
  newChat:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"><path d="M8 3.2v9.6M3.2 8h9.6"/></svg>',
  folder:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M2.2 4.4c0-.66.54-1.2 1.2-1.2h2.3l1.4 1.6h4.5c.66 0 1.2.54 1.2 1.2v5.6c0 .66-.54 1.2-1.2 1.2H3.4c-.66 0-1.2-.54-1.2-1.2V4.4z"/></svg>',
  download:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2.8v8M4.8 7.4 8 10.6l3.2-3.2M2.8 13.2h10.4"/></svg>',
  focus:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M2.4 8h11.2M8 2.4v11.2"/><circle cx="8" cy="8" r="5.6"/></svg>',
  copy:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><rect x="5.4" y="5.4" width="8.2" height="8.2" rx="1.4"/><path d="M10.6 5.4V3.8c0-.66-.54-1.2-1.2-1.2H3.8c-.66 0-1.2.54-1.2 1.2v5.6c0 .66.54 1.2 1.2 1.2h1.6"/></svg>',
  eraser:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M6.4 13.2h6.4M2.8 10.4l5.2-5.2 3.6 3.6-5.2 5.2H4.4l-1.6-1.6z"/></svg>',
  sidebar:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2.2" y="2.8" width="11.6" height="10.4" rx="1.6"/><path d="M6.4 2.8v10.4"/></svg>',
  inspector:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="2.2" y="2.8" width="11.6" height="10.4" rx="1.6"/><path d="M9.6 2.8v10.4"/></svg>',
  moon:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M13.4 9.6A5.8 5.8 0 0 1 6.4 2.6a5.8 5.8 0 1 0 7 7z"/></svg>',
  expand:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6V3h3M13 10v3h-3M3 10v3h3M13 6V3h-3"/></svg>',
  tools:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6"><rect x="2.2" y="2.2" width="5" height="5" rx="1.2"/><rect x="8.8" y="2.2" width="5" height="5" rx="1.2"/><rect x="2.2" y="8.8" width="5" height="5" rx="1.2"/><rect x="8.8" y="8.8" width="5" height="5" rx="1.2"/></svg>',
  keyboard:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="1.8" y="4.2" width="12.4" height="7.6" rx="1.4"/><path d="M4.4 6.4h.01M6.6 6.4h.01M8.8 6.4h.01M11 6.4h.01M4.4 9h7.2"/></svg>',
  privacy:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M8 2.2 13 4v4c0 3-2.2 4.6-5 5.6-2.8-1-5-2.6-5-5.6V4z"/><path d="M5.8 8l1.6 1.6 3-3.2"/></svg>',
  book:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"><path d="M2.6 3.4c1.8-.8 3.6-.8 5.4 0v9.4c-1.8-.8-3.6-.8-5.4 0zM13.4 3.4c-1.8-.8-3.6-.8-5.4 0v9.4c1.8-.8 3.6-.8 5.4 0z"/></svg>',
  info:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="5.8"/><path d="M8 7.2v4M8 4.9h.01"/></svg>',
  gear:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="8" cy="8" r="2.1"/><path d="M8 2.6v1.6M8 11.8v1.6M3.5 3.5l1.1 1.1M11.4 11.4l1.1 1.1M2.6 8h1.6M11.8 8h1.6M3.5 12.5l1.1-1.1M11.4 4.6l1.1-1.1"/></svg>',
  chart:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M2.8 13.2V6.4M7.2 13.2V2.8M11.6 13.2V9"/></svg>',
  refresh:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M13.2 8a5.2 5.2 0 1 1-1.6-3.7M13.4 2.6v3h-3"/></svg>',
  logout:
    '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M6.4 13.2H3.6c-.66 0-1.2-.54-1.2-1.2V4c0-.66.54-1.2 1.2-1.2h2.8M10.4 11 13.2 8l-2.8-3M13.2 8H6"/></svg>',
};

const menuPopover = $("menuPopover");
let menuAnchor = null;

function buildMenuItems(items) {
  const frag = document.createDocumentFragment();
  for (const item of items) {
    if (item.sep) {
      const sep = document.createElement("div");
      sep.className = "menu-sep";
      frag.appendChild(sep);
      continue;
    }
    if (item.title) {
      const title = document.createElement("div");
      title.className = "menu-group-title";
      title.textContent = item.title;
      frag.appendChild(title);
      continue;
    }
    const button = document.createElement("button");
    button.type = "button";
    button.className = "menu-item";
    button.setAttribute("role", "menuitem");
    if (item.disabled) button.disabled = true;
    button.innerHTML =
      `${item.icon || ""}<span class="menu-label">${esc(item.label)}</span>` +
      `${item.shortcut ? `<span class="menu-shortcut">${esc(item.shortcut)}</span>` : ""}` +
      `${item.hint ? `<span class="menu-hint">${esc(item.hint)}</span>` : ""}`;
    button.addEventListener("click", () => {
      closeMenu();
      if (item.action) item.action();
    });
    frag.appendChild(button);
  }
  return frag;
}

function openMenuAt(anchor, items) {
  menuPopover.innerHTML = "";
  menuPopover.appendChild(buildMenuItems(items));
  menuPopover.classList.remove("hidden");
  menuPopover.setAttribute("aria-hidden", "false");
  const rect = anchor.getBoundingClientRect();
  const menuRect = menuPopover.getBoundingClientRect();
  let left = rect.left;
  let top = rect.bottom + 4;
  if (left + menuRect.width > window.innerWidth - 8) {
    left = Math.max(8, window.innerWidth - menuRect.width - 8);
  }
  if (top + menuRect.height > window.innerHeight - 8) {
    top = Math.max(8, rect.top - menuRect.height - 4);
  }
  menuPopover.style.left = `${left}px`;
  menuPopover.style.top = `${top}px`;
  menuAnchor = anchor;
  for (const button of document.querySelectorAll(".menubar-menu-btn")) {
    button.classList.toggle("open", button === anchor);
  }
}

function closeMenu() {
  menuPopover.classList.add("hidden");
  menuPopover.setAttribute("aria-hidden", "true");
  menuPopover.innerHTML = "";
  menuAnchor = null;
  for (const button of document.querySelectorAll(".menubar-menu-btn")) {
    button.classList.remove("open");
  }
}

// 打开系统原生文件夹选择器（由同机核心服务代劳；取消返回空路径）。
// 服务暂不支持该端点时退回手动打开工作区分组，保证旧版本可用。
async function pickDirectory() {
  let result;
  try {
    result = await api("/fs/pick-directory", {
      method: "POST",
      body: JSON.stringify({
        initial: $("workspace").value.trim() || null,
        timeout_secs: 180,
      }),
    });
  } catch (error) {
    showToast("打开文件夹选择器失败：当前核心服务版本暂不支持，请更新后使用", "error");
    document.querySelector('[data-codex-group="workspace"]')?.click();
    return null;
  }
  if (result && result.path) {
    const input = $("workspace");
    input.value = result.path;
    input.dispatchEvent(new Event("change"));
    showToast(`工作区已切换到 ${result.path}`, "ok");
    return result.path;
  }
  showToast("已取消选择文件夹");
  return null;
}

function copyLastReply() {
  const replies = document.querySelectorAll("#messages .msg.assistant");
  const last = replies[replies.length - 1];
  if (!last) {
    showToast("当前会话还没有助手回复");
    return;
  }
  copyText(last.innerText)
    .then(() => showToast("已复制最后一条回复", "ok"))
    .catch(() => showToast("复制失败，请手动选择", "error"));
}

const MENU_DEFS = {
  file: () => [
    { icon: MENU_ICONS.newChat, label: "新对话", shortcut: "Ctrl+N", action: () => newSession() },
    { icon: MENU_ICONS.folder, label: "打开文件夹…", shortcut: "Ctrl+O", action: () => pickDirectory() },
    { sep: true },
    {
      icon: MENU_ICONS.download,
      label: "导出会话 Markdown",
      disabled: !state.sessionId,
      action: () => exportSession("md"),
    },
    {
      icon: MENU_ICONS.download,
      label: "导出会话 HTML",
      disabled: !state.sessionId,
      action: () => exportSession("html"),
    },
  ],
  edit: () => [
    { icon: MENU_ICONS.focus, label: "聚焦输入框", shortcut: "Ctrl+L", action: () => $("prompt").focus() },
    { icon: MENU_ICONS.copy, label: "复制最后一条回复", action: copyLastReply },
    { icon: MENU_ICONS.eraser, label: "清空输入框", action: () => { $("prompt").value = ""; $("prompt").focus(); } },
  ],
  view: () => [
    {
      icon: MENU_ICONS.sidebar,
      label: document.body.classList.contains("sidebar-collapsed") ? "显示会话栏" : "隐藏会话栏",
      shortcut: "Ctrl+B",
      action: () => document.body.classList.toggle("sidebar-collapsed"),
    },
    {
      icon: MENU_ICONS.inspector,
      label: "检查器（情景/审计/diff）",
      action: () => $("inspectorToggle").click(),
    },
    {
      icon: MENU_ICONS.moon,
      label: document.body.classList.contains("dark-theme") ? "切换到浅色主题" : "切换到深色主题",
      action: () => applyTheme(document.body.classList.contains("dark-theme") ? "light" : "dark"),
    },
    { sep: true },
    { icon: MENU_ICONS.expand, label: "全屏 / 窗口", shortcut: "F11", action: () => toggleFullscreen() },
    { icon: MENU_ICONS.tools, label: "工具与设置", action: () => openToolsView() },
  ],
  help: () => [
    {
      icon: MENU_ICONS.keyboard,
      label: "快捷键与帮助",
      action: () => openAboutPanel(),
    },
    { icon: MENU_ICONS.book, label: "OpenAPI 文档", action: () => window.open("openapi.json", "_blank", "noopener") },
    { icon: MENU_ICONS.privacy, label: "隐私声明", action: () => window.open("privacy.md", "_blank", "noopener") },
    { sep: true },
    { icon: MENU_ICONS.info, label: "关于 OwO Agent", action: () => showAboutVersion() },
  ],
};

// 打开「帮助与关于」面板（快捷键表 / 能力面速览 / 文档 / 许可 / 诊断）。
function openAboutPanel() {
  clearCodexGroup();
  setSettingsPageVisible(false);
  setToolsVisible(true);
  mountPanel("about");
}

// 关于：版本号取自 /health，避免前端硬编码与后端漂移。
function showAboutVersion() {
  api("/health")
    .then((health) => showToast(`OwO Agent 工作台 · 本地优先 · v${(health && health.version) || "未知"}`, ""))
    .catch(() => showToast("OwO Agent 工作台 · 本地优先", ""));
}

function toggleFullscreen() {
  if (document.fullscreenElement) {
    document.exitFullscreen().catch(() => {});
  } else {
    document.documentElement.requestFullscreen().catch(() => showToast("当前环境不允许全屏", "error"));
  }
}

for (const button of document.querySelectorAll(".menubar-menu-btn")) {
  const name = button.dataset.menu;
  const open = () => {
    openMenuAt(button, MENU_DEFS[name]());
    for (const other of document.querySelectorAll(".menubar-menu-btn")) {
      other.classList.toggle("open", other === button);
    }
  };
  button.addEventListener("click", (event) => {
    event.stopPropagation();
    if (menuAnchor === button) closeMenu();
    else open();
  });
  // 桌面端惯例：菜单已打开时，悬停其他菜单直接切换
  button.addEventListener("mouseenter", () => {
    if (menuAnchor && menuAnchor !== button) open();
  });
}

// 左下用户卡菜单（设置 / 使用情况和计费 / 重连 / 退出登录）
const userCard = $("codexUserCard");
function openUserMenu() {
  openMenuAt(userCard, [
    { title: "用户1 · 本地账户" },
    {
      icon: MENU_ICONS.gear,
      label: "设置",
      shortcut: "Ctrl+,",
      action: () => setSettingsPageVisible(true),
    },
    {
      icon: MENU_ICONS.chart,
      label: "使用情况和计费",
      hint: "设置页",
      action: () => {
        setSettingsPageVisible(true);
        setTimeout(() => $("usagePanel")?.scrollIntoView({ block: "center", behavior: "smooth" }), 80);
      },
    },
    { sep: true },
    {
      icon: MENU_ICONS.refresh,
      label: "重新连接服务",
      action: reconnectService,
    },
    {
      icon: MENU_ICONS.logout,
      label: "退出登录（清除本地配对）",
      action: () =>
        confirmModal({
          title: "退出登录",
          message: "将清除本机配对信息，下次操作时重新连接服务。设置与数据不受影响。",
          confirmText: "退出登录",
          kind: "danger",
          onConfirm: logoutLocal,
        }),
    },
  ]);
}
userCard.addEventListener("click", (event) => {
  if (event.target.closest("#themeToggleSide")) return; // 主题按钮独立处理
  event.stopPropagation();
  if (menuAnchor === userCard) closeMenu();
  else openUserMenu();
});
userCard.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") {
    event.preventDefault();
    userCard.click();
  }
});

// 点击空白处 / Esc 关闭菜单
document.addEventListener("click", (event) => {
  if (!menuAnchor) return;
  if (menuPopover.contains(event.target)) return;
  if (event.target.closest(".menubar-menu-btn") || event.target.closest("#codexUserCard")) return;
  closeMenu();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && menuAnchor) closeMenu();
});

// 全局快捷键：Ctrl+N 新对话 / Ctrl+O 打开文件夹 / Ctrl+B 会话栏 / Ctrl+, 设置 / F11 全屏
document.addEventListener("keydown", (event) => {
  if (!(event.ctrlKey || event.metaKey)) {
    if (event.key === "F11") {
      event.preventDefault();
      toggleFullscreen();
    }
    return;
  }
  const key = event.key.toLowerCase();
  if (key === "n") {
    event.preventDefault();
    newSession().catch((error) => addMessage("error", `创建会话失败：${error.message || error}`));
  } else if (key === "o") {
    event.preventDefault();
    pickDirectory();
  } else if (key === "b") {
    event.preventDefault();
    document.body.classList.toggle("sidebar-collapsed");
  } else if (key === ",") {
    event.preventDefault();
    setSettingsPageVisible(true);
  }
});

// 菜单栏主题按钮与连接状态
$("menubarTheme").addEventListener("click", () => {
  applyTheme(document.body.classList.contains("dark-theme") ? "light" : "dark");
});

// composer 项目 chip 与工作区「浏览…」：直接调原生文件夹选择器
$("workspaceBrowseBtn").addEventListener("click", () => pickDirectory());
$("composerProjectBtn").addEventListener("click", () => pickDirectory());
// 会话搜索：图标按钮切换输入框，输入实时过滤
$("sidebarSearchBtn").addEventListener("click", () => {
  const input = $("sessionSearch");
  input.hidden = !input.hidden;
  if (input.hidden) {
    input.value = "";
    filterSessionList();
  } else {
    input.focus();
  }
});
$("sessionSearch").addEventListener("input", filterSessionList);
function syncProjectChip() {
  const value = $("workspace").value.trim();
  const name = value ? value.split(/[\\/]/).filter(Boolean).pop() : "";
  $("composerProjectName").textContent = name || "本地项目";
  $("composerProjectBtn").title = value ? `当前工作区：${value}` : "未设置工作区（点击选择）";
  // 顶栏工作区文案（工具/设置视图可见）与 composer chip 保持同源
  const headerProject = document.querySelector(".project-label strong");
  if (headerProject) headerProject.textContent = name || "本地项目";
}
syncProjectChip();
$("workspace").addEventListener("change", () => {
  const workspace = $("workspace").value.trim();
  if (workspace) localStorage.setItem("owo.workspace", workspace);
  syncProjectChip();
});
$("newSession").addEventListener("click", () => {
  newSession().catch((error) => addMessage("error", `创建会话失败：${error.message || error}`));
});
$("showArchived").addEventListener("change", () => refreshSessions(state.sessionId));
$("attachmentBtn").addEventListener("click", () => $("attachmentInput").click());
$("attachmentInput").addEventListener("change", () => uploadAttachments($("attachmentInput").files));
$("chatForm").addEventListener("submit", (event) => {
  event.preventDefault();
  sendPrompt();
});
// 审批队列：事件委托（多张卡各自带 request_id，允许/拒绝按类名区分）。
$("approvalList").addEventListener("click", (event) => {
  const button = event.target.closest("button[data-rid]");
  if (!button) return;
  respondApproval(button.dataset.rid, button.classList.contains("allow"));
});
// 发送钮双态：空闲=发送，回合进行中=中断（Codex 行为）
$("sendBtn").addEventListener("click", () => {
  // 双态按钮按「当前会话」判定：本会话在跑 = 停止，否则 = 发送。
  if (currentTurn()) {
    abortTurn();
    return;
  }
  sendPrompt();
});
$("modelChip").addEventListener("click", () => {
  if (composerMenuEl && composerMenuEl.dataset.owner === "model") {
    closeComposerMenu();
    return;
  }
  openModelMenu();
  if (composerMenuEl) composerMenuEl.dataset.owner = "model";
});
$("effortChip").addEventListener("click", () => {
  if (composerMenuEl && composerMenuEl.dataset.owner === "effort") {
    closeComposerMenu();
    return;
  }
  openEffortMenu();
  if (composerMenuEl) composerMenuEl.dataset.owner = "effort";
});
$("accessChip").addEventListener("click", () => {
  if (composerMenuEl && composerMenuEl.dataset.owner === "access") {
    closeComposerMenu();
    return;
  }
  openAccessMenu();
  if (composerMenuEl) composerMenuEl.dataset.owner = "access";
});
$("revertBtn").addEventListener("click", revertAll);
$("auditFilterBtn").addEventListener("click", () => refreshAudit());
$("learnStart").addEventListener("click", () => learnControl("start"));
$("learnPause").addEventListener("click", () => learnControl("pause"));
$("learnResume").addEventListener("click", () => learnControl("resume"));
$("learnStop").addEventListener("click", () => learnControl("stop"));
$("learnClear").addEventListener("click", () => learnControl("clear"));
$("sinkForm").addEventListener("submit", (event) => {
  event.preventDefault();
  sinkSkill();
});
$("skillImportBtn").addEventListener("click", () => {
  const file = $("skillImport").files[0];
  if (!file) {
    showToast("请先选择 .owskill 文件", "error");
    return;
  }
  importPackage(file);
  $("skillImport").value = "";
});
// 自动化已迁入扩展面板：分组页卡片只留入口按钮。
$("openAutomationsPanel").addEventListener("click", () => {
  openPanelById("automations");
});
$("egressToggle").addEventListener("click", async () => {
  const enabled = $("egressToggle").dataset.enabled !== "true";
  try {
    await api("/settings/egress", {
      method: "POST",
      body: JSON.stringify({ cloud_enabled: enabled }),
    });
    await refreshSettings();
    showToast(`云端模型已${enabled ? "开启" : "关闭"}（已即时生效）`, "ok");
  } catch (error) {
    showToast(`切换云端模型失败：${error.message || error}`, "error");
  }
});
$("storageBackupBtn").addEventListener("click", () => storageBackup());
$("storageExportBtn").addEventListener("click", () => storageExport());
const settingsHelpBtn = $("settingsHelpBtn");
if (settingsHelpBtn) settingsHelpBtn.addEventListener("click", () => openAboutPanel());
$("storageRestoreBtn").addEventListener("click", () => $("storageRestoreFile").click());
$("storageRestoreFile").addEventListener("change", (event) => {
  const file = event.target.files && event.target.files[0];
  if (file) storageRestore(file);
  event.target.value = "";
});
$("storageClearBtn").addEventListener("click", () => storageClear());
$("recallBtn").addEventListener("click", () => recallMemory());
$("recallQ").addEventListener("keydown", (event) => {
  if (event.key === "Enter") recallMemory();
});
$("evalRunBtn").addEventListener("click", () => runEval());
$("tracesRefreshBtn").addEventListener("click", () => refreshTraces());
$("exportMdBtn").addEventListener("click", () => exportSession("md"));
$("exportHtmlBtn").addEventListener("click", () => exportSession("html"));
$("subagentRunBtn").addEventListener("click", () => runSubagent());
$("agentsSaveBtn").addEventListener("click", () => saveAgentsRules());
$("agentsTemplateBtn").addEventListener("click", () => generateAgentsTemplate());
$("mcpForm").addEventListener("submit", (event) => {
  event.preventDefault();
  addMcpServer();
});
$("computerTaskForm").addEventListener("submit", (event) => {
  event.preventDefault();
  createComputerTask();
});
$("whitelistForm").addEventListener("submit", async (event) => {
  event.preventDefault();
  const appId = $("wlAppId").value.trim();
  if (!appId) return;
  try {
    await api("/whitelist/manage", {
      method: "POST",
      body: JSON.stringify({
        action: "upsert",
        entry: {
          app_id: appId,
          name: appId,
          tier: "productivity",
          learn_allowed: true,
          auto_ops_allowed: true,
          chat_authorized: false,
          sensitive: false,
        },
      }),
    });
    $("wlAppId").value = "";
    await refreshWhitelist();
  } catch (error) {
    addMessage("error", `白名单更新失败：${error.message || error}`);
  }
});
// 斜杠命令：输入 / 唤起补全，↑↓ 选择，Enter 执行，Esc 关闭
$("prompt").addEventListener("input", () => {
  updateComposerHint();
  refreshSlashMenu();
});
$("prompt").addEventListener("keydown", (event) => {
  const slashOpen = composerMenuEl && composerMenuEl.dataset.owner === "slash";
  if (slashOpen) {
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      slashActiveIndex = (slashActiveIndex + step + slashMatches.length) % slashMatches.length;
      refreshSlashMenu();
      return;
    }
    if (event.key === "Tab") {
      event.preventDefault();
      const cmd = slashMatches[slashActiveIndex];
      if (cmd) {
        $("prompt").value = `/${cmd.name} `;
        closeSlashMenu();
        updateComposerHint();
      }
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      runSlashCommand(slashMatches[slashActiveIndex]);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      closeSlashMenu();
      return;
    }
  }
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    sendPrompt();
  }
});
updateComposerHint();
$("slashHint").addEventListener("click", () => {
  const promptEl = $("prompt");
  promptEl.value = "/";
  promptEl.focus();
  updateComposerHint();
  refreshSlashMenu();
});
$("micBtn").addEventListener("click", async () => {
  if (listening) {
    if (localRecorder) {
      const { blob, sampleCount } = await localRecorder.stop();
      localRecorder = null;
      listening = false;
      setMicRecording(false);
      if (sampleCount < 1600) {
        addMessage("system", "录音太短，未识别");
        return;
      }
      try {
        const response = await apiRaw("/stt/transcribe", {
          method: "POST",
          headers: { "Content-Type": "audio/wav" },
          body: blob,
        });
        const result = await response.json();
        if (!response.ok) throw new Error(result.error || response.statusText);
        const prompt = $("prompt");
        prompt.value = (prompt.value ? prompt.value + " " : "") + result.text;
      } catch (error) {
        addMessage("error", `本地语音识别失败（${error.message}）`);
      }
      return;
    }
    if (recognition) recognition.stop();
    return;
  }
  listening = true;
  setMicRecording(true);
  const started = await startLocalRecording();
  if (!started) {
    listening = false;
    setMicRecording(false);
    if (!recognition) {
      initSpeech();
    }
    if (recognition) {
      try {
        recognition.start();
      } catch (_) {
        listening = false;
        setMicRecording(false);
        showToast("无法访问麦克风或系统语音识别", "error");
      }
    } else {
      showToast("无法访问麦克风（请允许麦克风权限）", "error");
    }
  } else {
    setTimeout(async () => {
      if (listening && localRecorder) {
        $("micBtn").click();
      }
    }, 10000);
  }
});

// ---------- 启动 ----------

async function boot() {
  initSpeech();
  initPanels();
  initProviderPresets();
  initPluginSkillTabs();
  initChatScroll();
  renderAccessChip();
  renderEffortChip();
  setMicRecording(false);
  updateComposerRunning();
  // 先读回本机偏好再应用：否则刷新后已保存的紧凑模式/桌宠等开关会被内存默认值覆盖。
  loadLocalPrefs();
  applyLocalPrefs();
  // A8-3：桌面桌宠开关状态来自引擎（每 15 秒刷新，跟随桌面端侧的改动）。
  refreshPetState();
  setInterval(refreshPetState, 15000);
  serviceWatch.start();
  await refreshHealth();
  await Promise.all([
    refreshSessions(),
    refreshSkills(),
    refreshPlugins(),
    refreshPluginMarket(),
    refreshPackages(),
    refreshSuggestions(),
    refreshSettings(),
    refreshUsage(),
    refreshServerStatus(),
    refreshAudit(),
    refreshWhitelist(),
    refreshPerception(),
    refreshLearn(),
    refreshObservations(),
    refreshSkillHealth(),
    refreshProjectRules(),
    refreshMcp(),
    refreshTraces(),
    refreshComputerTasks(),
  ]);
  await restoreLastSession();
  setInterval(refreshPerception, 3000);
  setInterval(refreshLearn, 5000);
  setInterval(refreshPlugins, 15000);
  setInterval(refreshPluginMarket, 30000);
  setInterval(refreshPackages, 10000);
  setInterval(refreshSuggestions, 10000);
  setInterval(refreshAudit, 5000);
  setInterval(refreshSettings, 15000);
  setInterval(refreshUsage, 10000);
  setInterval(refreshHealth, 30000);
  setInterval(refreshSkillHealth, 15000);
  setInterval(refreshObservations, 30000);
  setInterval(refreshMcp, 20000);
  setInterval(refreshTraces, 15000);
  setInterval(refreshComputerTasks, 15000);
}

boot();
