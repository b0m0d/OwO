/* §4.3 全局状态条：主窗口顶部五段（后台 / 工作区 / 模型 / 权限 / 当前任务）。
 *
 * 设计约束（都是实测踩出来的，改动前请一并读）：
 * 1) **零 HTTP**：状态条在首屏就在，任何 fetch 都会挤爆 §8.2「首次可输入前 ≤5 请求」
 *    口径。所以这里只读三类既有事实——壳 IPC 快照（window.__owoCoreDiagnostics）、
 *    前端运行态（app.js 的 state）、以及页面自己上报的缓存（report*）。
 *    模型段用壳命令 get_provider_status（IPC，不计 HTTP），带 TTL 缓存。
 * 2) **可点击**（§4.3：不能只是文字灯）：每段都是真按钮，键盘可聚焦可回车，
 *    点击派发 owo:statusbar-navigate 由 app.js 决定去哪个页面。
 * 3) **重绘不重建**：只改 textContent/tone，不 innerHTML 覆盖，避免用户 Tab 焦点
 *    被 1 秒定时器抢走（§4.10 键盘可达性）。
 * 4) **隐藏期不刷**：uiHidden() 时跳过重绘与 IPC（§3.4 隐藏窗口零业务请求）。
 */
(function (global) {
  "use strict";

  const SEGMENTS = [
    { key: "backend", label: "后台", target: "settings", hint: "核心服务状态 · 点击进入设置与诊断" },
    { key: "workspace", label: "工作区", target: "projects", hint: "当前项目与读写范围 · 点击进入工作区" },
    { key: "model", label: "模型", target: "settings", hint: "提供商与模型 · 点击进入设置" },
    { key: "permission", label: "权限", target: "permissions", hint: "权限档位与待审批 · 点击进入权限中心" },
    { key: "task", label: "当前任务", target: "chat", hint: "任务进展 · 点击进入会话" },
  ];

  const PROVIDER_TTL_MS = 15000;
  // 工作区走 IPC（零 HTTP），可以比提供商更勤：引导页里换目录后 5s 内状态条就该跟上。
  const WORKSPACE_TTL_MS = 5000;
  const REFRESH_MS = 1000;

  const cache = {
    provider: { at: 0, value: null, pending: false },
    workspace: { at: 0, root: "", pending: false },
    permission: { profile: null, pendingApprovals: null, grants: null },
    // 设置页从 core 水合到的实际生效模型（唯一权威源），回灌给模型段。
    model: null,
  };

  function backendFromDiagnostics(diag, streamState) {
    const info = diag || {};
    const state = String(info.state || "unknown");
    const code = info.errorCode ? String(info.errorCode) : null;
    if (state === "ready") {
      // 「可用」与「降级」的差别只在事件流：SSE 掉到 degraded 时界面仍可用但会滞后。
      if (streamState === "degraded") {
        return { text: "可用（事件流降级）", tone: "warn" };
      }
      return { text: "可用", tone: "ok" };
    }
    if (code) return { text: terminalBackendText(state, code), tone: "bad" };
    if (state === "starting" || state === "restarting") return { text: "正在启动", tone: "warn" };
    if (state === "no_workspace") return { text: "未选工作区", tone: "warn" };
    if (state === "stopped") return { text: "已停止", tone: "muted" };
    return { text: "检查中", tone: "muted" };
  }

  // 稳定码 → 用户术语（与 §4.7「相同错误码相同术语」同源，不在此页另起口径）。
  function terminalBackendText(state, code) {
    if (state === "restarting") return "重启中";
    if (code.indexOf("handshake") >= 0) return "启动超时";
    if (code.indexOf("binary_missing") >= 0 || code.indexOf("identity") >= 0) return "不可用";
    if (code.indexOf("workspace") >= 0) return "未选工作区";
    if (code.indexOf("provider") >= 0) return "模型未配置";
    if (code.indexOf("storage") >= 0) return "存储不可写";
    if (code.indexOf("cloud_disabled") >= 0) return "已拒绝联网";
    if (code.indexOf("exited") >= 0 || code.indexOf("hang") >= 0) return "失败";
    return "失败";
  }

  /** 路径摘要：只给末两级目录（§4.3「路径摘要」），账户目录与盘符都不出现。 */
  function pathSummary(root) {
    const parts = String(root || "").split(/[\\/]+/).filter(Boolean);
    if (!parts.length) return "";
    if (parts.length <= 2) return parts.join("\\");
    return "…" + "\\" + parts.slice(-2).join("\\");
  }

  function workspaceFromFacts(source) {
    // 真机实测（R4 验收）：全新 WebView2 存储下 localStorage 里没有 owo.workspace，
    // 只靠前端 state.workspaceRoot 会让工作区段在"壳其实已经带着工作区启动"时显示
    // 「未选择」——壳的 get_workspace 才是权威来源（IPC，零 HTTP）。
    const root = String((source && source.workspaceRoot) || (cache.workspace && cache.workspace.root) || "");
    const display = global.OwoWorkspaceDisplay;
    if (!root) return { text: "未选择", tone: "warn", detail: "尚未选择项目目录" };
    const alias = display ? display.alias(root) : pathSummary(root).split("\\").pop();
    const writable = cache.permission.profile
      ? profileWritable(cache.permission.profile)
      : null;
    const mode = writable === null ? "" : writable ? " · 可写" : " · 只读";
    // 摘要=末两级目录：够用户分辨同名项目，又不把账户目录/盘符带到 title 里。
    return { text: alias + mode, tone: writable === false ? "warn" : "ok", detail: pathSummary(root) };
  }

  function profileWritable(profile) {
    const text = String(profile || "").toLowerCase();
    if (text.indexOf("read_only") >= 0 || text.indexOf("只读") >= 0) return false;
    if (text.indexOf("workspace") >= 0 || text.indexOf("auto") >= 0 || text.indexOf("full") >= 0) return true;
    return null;
  }

  function modelFromCache() {
    // 两份真相不能混着说（R3-B 缺陷 23 的同族问题，真机截图抓到）：
    // 密钥由壳注入 sidecar 环境时，壳自己的 get_provider_status 仍会报
    // provider=unset/ready=false，而 core 实际已就绪并在用 glm-5.3-flash。
    // 所以：① 设置页水合到的 core 事实优先回灌；② 只有壳视图时不得把
    // 「未配置」当结论标红，必须说明这是壳侧视图。
    if (cache.model) {
      const info = cache.model;
      const label = [info.provider, info.model].filter(Boolean).join(" · ") || "已配置";
      return { text: label, tone: "ok", detail: "来源：core 实际生效配置（设置页水合）" };
    }
    const entry = cache.provider.value;
    if (!entry) return { text: "读取中…", tone: "muted" };
    if (entry.error) return { text: "未读取", tone: "warn", detail: entry.error };
    const model = entry.model || "未设置";
    const provider = entry.provider || "未知";
    const coreReady = String((global.__owoCoreDiagnostics || {}).state || "") === "ready";
    if (entry.ready === false && coreReady) {
      return {
        text: provider + " · 壳侧未配置",
        tone: "muted",
        detail: "core 已就绪并在服务；壳侧配置视图与 core 不一致，实际生效模型见设置页",
      };
    }
    if (entry.ready === false) return { text: provider + " · 未配置", tone: "warn" };
    return { text: provider + " · " + model, tone: "ok" };
  }

  function permissionFromCache(source) {
    // 待审批数量以前端运行态为准（审批条就是唯一真相），权限中心可回灌权威值。
    let pending = cache.permission.pendingApprovals;
    if (pending == null) pending = source && source.pendingApproval ? 1 : 0;
    const profile = cache.permission.profile;
    const profileText = profile ? String(profile) : "点按查看";
    const pendingText = pending ? " · 待审批 " + pending : "";
    return {
      text: profileText + pendingText,
      tone: pending ? "warn" : "muted",
      detail: profile ? "当前档位：" + profile : "权限中心会给出实际范围与撤销入口",
    };
  }

  function taskFromFacts(source) {
    const state = source || {};
    if (state.pendingApproval) return { text: "等待审批", tone: "warn" };
    if (state.reading) return { text: "运行中", tone: "ok" };
    const outcome = String(state.lastTurnOutcome || "");
    if (outcome === "cancelled") return { text: "已取消", tone: "muted" };
    if (outcome === "failed") return { text: "上一轮失败", tone: "bad" };
    if (outcome === "completed") return { text: "已完成", tone: "muted" };
    return { text: "空闲", tone: "muted" };
  }

  function computeFacts(source, streamState) {
    const facts = {
      backend: backendFromDiagnostics(global.__owoCoreDiagnostics, streamState),
      workspace: workspaceFromFacts(source),
      model: modelFromCache(),
      permission: permissionFromCache(source),
      task: taskFromFacts(source),
    };
    return SEGMENTS.map((segment) =>
      Object.assign({}, segment, facts[segment.key] || { text: "—", tone: "muted" }),
    );
  }

  function invokeOwner() {
    const internal = global.__TAURI_INTERNALS__;
    const publicCore = global.__TAURI__ && global.__TAURI__.core;
    const owner = publicCore && typeof publicCore.invoke === "function" ? publicCore : internal;
    return owner && typeof owner.invoke === "function" ? owner : null;
  }

  function invokeShell(owner, command) {
    return Promise.resolve(owner.invoke(command, {}));
  }

  function refreshProviderCache(force) {
    if (cache.provider.pending) return;
    const age = Date.now() - cache.provider.at;
    if (!force && cache.provider.value && age < PROVIDER_TTL_MS) return;
    const owner = invokeOwner();
    if (!owner) {
      cache.provider.at = Date.now();
      cache.provider.value = { error: "非桌面壳（浏览器直连时模型段由设置页提供）" };
      return;
    }
    cache.provider.pending = true;
    invokeShell(owner, "get_provider_status").then(
      (result) => {
        cache.provider.pending = false;
        cache.provider.at = Date.now();
        cache.provider.value = result && typeof result === "object" ? result : { error: "壳未返回提供商状态" };
        repaint();
      },
      (error) => {
        cache.provider.pending = false;
        cache.provider.at = Date.now();
        cache.provider.value = { error: String((error && error.message) || error) };
        repaint();
      },
    );
  }

  /** 工作区段壳侧水合：get_workspace 是权威源，localStorage 只是用户刚改过的即时反馈。 */
  function refreshWorkspaceCache(force) {
    if (cache.workspace.pending) return;
    const age = Date.now() - cache.workspace.at;
    if (!force && age < WORKSPACE_TTL_MS) return;
    const owner = invokeOwner();
    if (!owner) {
      cache.workspace.at = Date.now();
      return;
    }
    cache.workspace.pending = true;
    invokeShell(owner, "get_workspace").then(
      (result) => {
        cache.workspace.pending = false;
        cache.workspace.at = Date.now();
        const root = result && typeof result.workspace === "string" ? result.workspace : "";
        // 壳报空也要清缓存：引导页里用户撤销/换目录后，状态条不得停留在旧值上。
        cache.workspace.root = root;
        repaint();
      },
      () => {
        cache.workspace.pending = false;
        cache.workspace.at = Date.now();
        repaint();
      },
    );
  }

  function uiHidden() {
    if (typeof global.document === "undefined" || !global.document.documentElement) return false;
    if (global.document.visibilityState === "hidden") return true;
    if (typeof global.owoWindowHidden === "function") return Boolean(global.owoWindowHidden());
    return Boolean(global.__owoWindowHidden);
  }

  let host = null;
  let source = null;
  let timer = 0;
  let lastSerialized = "";

  function paint(list) {
    const serialized = list.map((item) => item.key + ":" + item.text + ":" + item.tone).join("|");
    if (serialized === lastSerialized) return;
    lastSerialized = serialized;
    for (const item of list) {
      const node = host.querySelector('[data-owo-status="' + item.key + '"]');
      if (!node) continue;
      const value = node.querySelector(".owo-status-value");
      if (value && value.textContent !== item.text) value.textContent = item.text;
      node.setAttribute("data-tone", item.tone);
      node.setAttribute("data-target", item.target);
      node.title = item.label + "：" + item.text + "（" + (item.detail ? item.detail + " · " : "") + item.hint + "）";
      node.setAttribute("aria-label", node.title);
    }
  }

  function repaint() {
    if (!host || uiHidden()) return;
    paint(computeFacts(source ? source() : {}, streamState()));
  }

  function streamState() {
    try {
      const snap = typeof global.owoInvalidatorState === "function" ? global.owoInvalidatorState() : null;
      return snap && snap.state ? String(snap.state) : null;
    } catch (error) {
      return null;
    }
  }

  function build() {
    host.innerHTML = "";
    for (const segment of SEGMENTS) {
      const button = global.document.createElement("button");
      button.type = "button";
      button.className = "owo-status-item";
      button.setAttribute("data-owo-status", segment.key);
      button.setAttribute("data-tone", "muted");
      const label = global.document.createElement("span");
      label.className = "owo-status-label";
      label.textContent = segment.label;
      const value = global.document.createElement("span");
      value.className = "owo-status-value";
      value.textContent = "—";
      button.appendChild(label);
      button.appendChild(value);
      button.addEventListener("click", () => {
        if (segment.key === "model" || segment.key === "backend") refreshProviderCache(true);
        if (segment.key === "workspace") refreshWorkspaceCache(true);
        const detail = { key: segment.key, target: segment.target };
        if (typeof global.dispatchEvent === "function" && typeof global.CustomEvent === "function") {
          global.dispatchEvent(new global.CustomEvent("owo:statusbar-navigate", { detail }));
        }
        repaint();
      });
      host.appendChild(button);
    }
  }

  /**
   * 挂载状态条。
   * @param {Element} root   #globalStatusBar 容器
   * @param {Object} options { getFacts: () => ({workspaceRoot, reading, pendingApproval, lastTurnOutcome}) }
   */
  function mount(root, options) {
    if (!root || typeof global.document === "undefined") return;
    host = root;
    source = (options && options.getFacts) || null;
    build();
    if (!timer && typeof global.setInterval === "function") {
      timer = global.setInterval(() => {
        if (uiHidden()) return;
        refreshProviderCache(false);
        refreshWorkspaceCache(false);
        repaint();
      }, REFRESH_MS);
    }
    refreshProviderCache(true);
    refreshWorkspaceCache(true);
    repaint();
    if (global.document && global.document.body) global.document.body.classList.add("has-global-status-bar");
  }

  function unmount() {
    if (timer && typeof global.clearInterval === "function") global.clearInterval(timer);
    timer = 0;
    host = null;
    source = null;
    // 缓存必须一起清：验收脚本会在同一进程里连跑多个场景（不同私有工作区），
    // 留着上一轮的工作区/提供商结果就是"看起来成功"的假绿。
    cache.provider = { at: 0, value: null, pending: false };
    cache.workspace = { at: 0, root: "", pending: false };
    cache.permission = { profile: null, pendingApprovals: null, grants: null };
    cache.model = null;
    lastSerialized = "";
  }

  /** 权限中心/设置页把权威档位回灌给状态条（零额外请求）。 */
  function reportPermission(info) {
    cache.permission = Object.assign({}, cache.permission, info || {});
    repaint();
  }

  /**
   * 设置页把 core 实际生效的提供商/模型回灌给模型段（§4.3「provider、模型、连接状态」
   * 的唯一权威源；壳侧 get_provider_status 只是进程配置视图，两者不一致时以 core 为准）。
   */
  function reportModel(info) {
    cache.model = info && typeof info === "object" ? info : null;
    repaint();
  }

  /** 真机验收（§4.10）只读访问器：不改变任何状态。 */
  function snapshotFacts() {
    return computeFacts(source ? source() : {}, streamState());
  }

  global.OwoStatusBar = {
    SEGMENTS: SEGMENTS,
    backendFromDiagnostics: backendFromDiagnostics,
    terminalBackendText: terminalBackendText,
    profileWritable: profileWritable,
    computeFacts: computeFacts,
    reportPermission: reportPermission,
    reportModel: reportModel,
    mount: mount,
    unmount: unmount,
    repaint: repaint,
    snapshotFacts: snapshotFacts,
  };
  global.__owoStatusBarFacts = snapshotFacts;
})(typeof window !== "undefined" ? window : globalThis);
