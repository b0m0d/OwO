// OwO Agent 工作台（v0.4 P1 桌面壳，纯静态，直连本地 HTTP API + SSE）
"use strict";

const state = {
  sessionId: null,
  workspaceRoot: "",
  pendingApproval: null,
  reading: false,
  attachments: [],
  abortController: null,
  selectionVersion: 0,
  // §4.3 状态条「当前任务」段的判据：completed | cancelled | failed（空=本轮未跑过）。
  lastTurnOutcome: "",
};

const $ = (id) => document.getElementById(id);

// ---------- 头部状态 ----------

async function refreshHealth() {
  try {
    const health = await apiClient.get("/health", { public: true });
    const commit = health.build && health.build.commit && health.build.commit !== "unknown"
      ? ` · ${health.build.commit.slice(0, 8)}` : "";
    $("health").textContent = `API 就绪 ${health.version}${commit}`;
    $("health").style.color = "var(--green)";
    return health;
  } catch (error) {
    markConnectionUnavailable();
    throw error;
  }
}

async function refreshPerception() {
  try {
    const snapshot = await api("/context/snapshot");
    // §8.2：感知等级以用户文案展示，原始 l0_l1 代码折叠为斜杠形式（l0/l1）。
    const levelCode = (snapshot.permission_level || "l0_l1").replace(/_/g, "/");
    $("permission").textContent = `感知：${levelCode}`;
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

// §12-13 约束控件：chips 多选（替代动作/应用 CSV 自由文本）。
// hostId 为容器元素 id；选项在 renderChipGroup 时注入。选中值由
// data-chip 值汇总，读取处经 getSelectedChips 取回。
function chipOptionsCatalog() {
  return {
    // Computer-use 允许动作（能力注册表固定候选）。
    actions: [
      { value: "click", label: "点击" },
      { value: "type", label: "键入" },
      { value: "scroll", label: "滚动" },
      { value: "drag", label: "拖拽" },
      { value: "open", label: "打开应用" },
      { value: "close", label: "关闭应用" },
    ],
    // 技能沉淀常见目标应用（能力注册表候选）。
    apps: [
      { value: "qq", label: "QQ" },
      { value: "wechat", label: "微信" },
      { value: "feishu", label: "飞书" },
      { value: "dingtalk", label: "钉钉" },
      { value: "chrome", label: "Chrome" },
      { value: "edge", label: "Edge" },
      { value: "explorer", label: "文件资源管理器" },
    ],
  };
}
function renderChipGroup(hostId, catalogKey, selected = []) {
  const host = document.getElementById(hostId);
  if (!host) return;
  host.innerHTML = "";
  for (const option of chipOptionsCatalog()[catalogKey] || []) {
    const chip = document.createElement("button");
    chip.type = "button";
    chip.className = "chip" + (selected.includes(option.value) ? " selected" : "");
    chip.dataset.chip = option.value;
    chip.textContent = option.label;
    chip.setAttribute("aria-pressed", String(selected.includes(option.value)));
    chip.addEventListener("click", () => {
      chip.classList.toggle("selected");
      chip.setAttribute("aria-pressed", String(chip.classList.contains("selected")));
    });
    host.appendChild(chip);
  }
}
function getSelectedChips(hostId) {
  const host = document.getElementById(hostId);
  if (!host) return [];
  return Array.from(host.querySelectorAll(".chip.selected")).map((chip) => chip.dataset.chip);
}
function setSelectedChips(hostId, values) {
  const host = document.getElementById(hostId);
  if (!host) return;
  for (const chip of host.querySelectorAll(".chip")) {
    const on = values.includes(chip.dataset.chip);
    chip.classList.toggle("selected", on);
    chip.setAttribute("aria-pressed", String(on));
  }
}

async function sinkSkill() {
  const name = $("sinkName").value.trim();
  const apps = getSelectedChips("sinkAppsChips");
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
    setSelectedChips("sinkAppsChips", []);
    addMessage("system", `已沉淀技能包 ${result.name}（变量：${result.variables.join(", ") || "无"}）`);
    await refreshPackages();
  } catch (error) {
    addMessage("error", `沉淀失败：${error.message}`);
  }
}

async function refreshPlugins() {
  try {
    const data = await api("/plugins");
    const list = $("pluginList");
    list.innerHTML = "";
    const plugins = data.plugins || [];
    for (const plugin of plugins) {
      const li = document.createElement("li");
      const enabled = plugin.enabled !== false;
      const mcp = plugin.mcp
        ? `${esc(plugin.mcp.transport)}｜${esc(plugin.mcp.command)}`
        : "无 MCP 服务器";
      li.innerHTML =
        `<strong>${esc(plugin.name)}</strong>` +
        `<span class="sub">${enabled ? "已启用" : "已禁用"} ｜ ${esc(plugin.id)} v${esc(plugin.version)}</span>` +
        `<span class="sub">${esc(plugin.description || "")}</span>` +
        `<span class="sub">权限：${esc((plugin.permissions || []).join(", ") || "无")} ｜ ${mcp}</span>`;
      const toggleBtn = document.createElement("button");
      toggleBtn.textContent = enabled ? "禁用" : "启用";
      toggleBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          await api(`/plugins/${encodeURIComponent(plugin.id)}/enabled`, {
            method: "POST",
            body: JSON.stringify({ enabled: !enabled }),
          });
          await refreshPlugins();
        } catch (error) {
          addMessage("system", `插件切换失败：${error.message || error}`);
        }
      });
      li.appendChild(toggleBtn);
      list.appendChild(li);
    }
    if (!plugins.length) list.innerHTML = '<li class="sub">未发现插件</li>';
  } catch (error) {
    $("pluginList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
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
        if (!window.confirm(`删除流程技能包 ${pkg.name}？`)) return;
        try {
          await api(`/learn/packages/${encodeURIComponent(pkg.name)}`, { method: "DELETE" });
          await refreshPackages();
          addMessage("system", `已删除流程技能包 ${pkg.name}`);
        } catch (error) {
          addMessage("system", `删除失败：${friendlyError(error, { resource: true })}`);
        }
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
    const blob = await apiClient.download(`/learn/export/${encodeURIComponent(name)}`);
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
    const result = await apiClient.upload("/learn/import", file, "application/zip");
    addMessage("system", `已导入技能包 ${result.name}`);
    await refreshPackages();
  } catch (error) {
    addMessage("error", `导入失败：${error.message}`);
  }
}

async function refreshAutomations() {
  try {
    const tasks = await api("/automations");
    const list = $("automationList");
    list.innerHTML = "";
    for (const task of tasks) {
      const li = document.createElement("li");
      const schedule = JSON.stringify(task.schedule);
      li.innerHTML = `<strong>${esc(task.name)}</strong><span class="sub">${esc(schedule)} ｜ ${task.enabled ? "启用" : "停用"}</span>`;
      const toggleBtn = document.createElement("button");
      toggleBtn.textContent = task.enabled ? "停用" : "启用";
      toggleBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        await api(`/automations/${task.id}/toggle`, { method: "POST" });
        await refreshAutomations();
      });
      const deleteBtn = document.createElement("button");
      deleteBtn.textContent = "删除";
      deleteBtn.addEventListener("click", async (event) => {
        event.stopPropagation();
        await api(`/automations/${task.id}`, { method: "DELETE" });
        await refreshAutomations();
      });
      li.appendChild(toggleBtn);
      li.appendChild(deleteBtn);
      list.appendChild(li);
    }
    if (!tasks.length) list.innerHTML = '<li class="sub">暂无自动化任务</li>';
  } catch (error) {
    $("automationList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

// §12-13 约束控件：自动化按触发方式切换三种受控表单（间隔/每日/指定时间），
// 不再要求用户手写 60 / 09:00 / RFC3339 混合字符串。
const AUTO_SCHEDULE_FIELDS = {
  interval: "autoIntervalSecs",
  daily: "autoDailyTime",
  oneshot: "autoOnceAt",
};
function syncAutomationFields() {
  const kind = $("autoKind") ? $("autoKind").value : "interval";
  $("autoIntervalGroup").hidden = kind !== "interval";
  $("autoDailyGroup").hidden = kind !== "daily";
  $("autoOnceGroup").hidden = kind !== "oneshot";
}
if (document.getElementById("autoKind")) {
  $("autoKind").addEventListener("change", syncAutomationFields);
}

async function createAutomation() {
  const name = $("autoName").value.trim();
  const kind = $("autoKind").value;
  const reminder = $("autoReminder").value.trim();
  let schedule;
  if (kind === "interval") {
    const everySecs = parseInt($("autoIntervalSecs").value, 10);
    if (!Number.isFinite(everySecs) || everySecs <= 0) {
      addMessage("error", "间隔需为正整数（秒）");
      return;
    }
    schedule = { kind: "interval", every_secs: everySecs };
  } else if (kind === "daily") {
    const time = $("autoDailyTime").value;
    if (!/^\d{2}:\d{2}$/.test(time)) {
      addMessage("error", "请选择每日触发时刻");
      return;
    }
    schedule = { kind: "daily", time };
  } else {
    const local = $("autoOnceAt").value;
    if (!local) {
      addMessage("error", "请选择触发时间");
      return;
    }
    // datetime-local → RFC3339（本地时区显式化，避免服务端按 UTC 误判当日）。
    const offsetMinutes = -new Date(local).getTimezoneOffset();
    const sign = offsetMinutes >= 0 ? "+" : "-";
    const abs = Math.abs(offsetMinutes);
    const tz = `${sign}${String(Math.floor(abs / 60)).padStart(2, "0")}:${String(abs % 60).padStart(2, "0")}`;
    schedule = { kind: "oneshot", at: `${local}:00${tz}` };
  }
  if (!name || !reminder) return;
  try {
    await api("/automations", {
      method: "POST",
      body: JSON.stringify({ name, schedule, reminder }),
    });
    $("autoName").value = "";
    $("autoReminder").value = "";
    syncAutomationFields();
    await refreshAutomations();
  } catch (error) {
    addMessage("error", `创建自动化失败：${error.message}`);
  }
}

async function refreshReminders() {
  try {
    const reminders = await api("/automations/reminders");
    const list = $("reminderList");
    list.innerHTML = "";
    for (const text of reminders) {
      const li = document.createElement("li");
      li.textContent = `⏰ ${text}`;
      list.appendChild(li);
    }
    if (!reminders.length) list.innerHTML = '<li class="sub">暂无提醒</li>';
  } catch (error) {
    $("reminderList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function refreshSettings() {
  try {
    const settings = await api("/settings");
    const runtime = settings.runtime || {};
    const cloudEnabled = runtime.cloud_enabled != null
      ? runtime.cloud_enabled : settings.egress && settings.egress.cloud_enabled;
    const button = $("egressToggle");
    button.textContent = cloudEnabled ? "开" : "关";
    button.dataset.enabled = String(cloudEnabled);
    const model = runtime.model || settings.model || "";
    const modelSelect = $("settingsModel");
    modelSelect.replaceChildren(new Option(model || "未配置", model));
    modelSelect.value = model;
    const toolCapabilities = settings.tool_capabilities || {};
    const capabilityControls = [
      ["toolCapDesktopObservation", "desktop_observation"],
      ["toolCapDesktopControl", "desktop_control"],
      ["toolCapBrowser", "browser"],
    ];
    for (const [elementId, key] of capabilityControls) {
      const control = $(elementId);
      if (control && control.dataset.dirty !== "true") {
        control.checked = toolCapabilities[key] === true;
      }
    }
    const activeTools = Array.isArray(runtime.active_tool_names) ? runtime.active_tool_names : [];
    $("toolCapabilityStatus").textContent = activeTools.length
      ? `当前运行中的工具（${activeTools.length}）：${activeTools.join("、")}。下方变更保存后需重启核心服务。`
      : "当前运行中的工具清单暂不可用；下方变更保存后需重启核心服务。";
    $("runtimeProvider").textContent = runtime.provider || "未知提供商";
    $("runtimeEndpoint").textContent = `${runtime.endpoint_kind === "local" ? "本地兼容接口" : "云端兼容接口"} · ${runtime.credential_source || "未知凭据来源"}`;
    $("runtimeCredential").textContent = runtime.credential_source === "environment" ? "系统环境变量" : (runtime.credential_source || "未配置");
    $("connectionSummary").textContent = `${runtime.provider || "未知提供商"} / ${model || "未配置模型"} · ${cloudEnabled ? "云端已启用" : "云端已关闭"}`;
    // §4.3 模型段回灌：core 实际生效配置才是权威源。壳侧 get_provider_status 只是
    // 进程配置视图——密钥经 sidecar 注入时它会误报 unset（真机截图抓到过）。
    if (window.OwoStatusBar && typeof window.OwoStatusBar.reportModel === "function") {
      window.OwoStatusBar.reportModel({
        provider: runtime.provider || "",
        model: model || "",
        credential: runtime.credential_source || "",
        cloud: cloudEnabled !== false,
      });
    }
    $("settingsPreview").textContent = JSON.stringify(
      {
        runtime,
        model: settings.model,
        stt: settings.stt,
        proactive: settings.proactive,
        egress: settings.egress,
      usage: settings.usage,
      tool_capabilities: settings.tool_capabilities,
      active_tool_names: runtime.active_tool_names,
      },
      null,
      2
    );
    if (!$("settingsEditor").dataset.seeded) {
      $("settingsEditor").value = JSON.stringify(settings, null, 2);
      $("settingsEditor").dataset.seeded = "1";
    }
  } catch (error) {
    $("settingsPreview").textContent = friendlyError(error);
  }
}

async function refreshUsage() {
  try {
    const data = await api("/usage");
    const usage = data.usage || {};
    const budget = data.budget || {};
    const status = budget.violation ? "⚠️ 已超限" : "正常";
    $("usagePanel").textContent =
      `累计 tokens：${usage.total_tokens || 0}` +
      `（输入 ${usage.prompt_tokens || 0} / 输出 ${usage.completion_tokens || 0}）` +
      ` ｜ 成本 ≈ $${(data.cost_usd || 0).toFixed(6)}` +
      ` ｜ token 预算：${budget.token_cap ?? "未配置"}` +
      ` ｜ 成本预算：${budget.cost_cap_usd != null ? "$" + budget.cost_cap_usd : "未配置"}` +
      ` ｜ 状态：${status}`;
  } catch (error) {
    $("usagePanel").textContent = friendlyError(error);
  }
}

// ---------- 事件绑定 ----------

// §12-13 约束控件：工作区路径收敛为「最近项目」datalist（本地记忆最近 6 个），
// 普通模式不再要求手填完整路径；选中后显示目录别名。
function workspaceRecentList() {
  const raw = localStorage.getItem("owo.recent-workspaces");
  try {
    const list = JSON.parse(raw);
    return Array.isArray(list) ? list.filter((item) => typeof item === "string") : [];
  } catch (_) {
    return [];
  }
}
function rememberWorkspace(path) {
  const list = workspaceRecentList().filter((item) => item !== path);
  list.unshift(path);
  localStorage.setItem("owo.recent-workspaces", JSON.stringify(list.slice(0, 6)));
}
function refreshWorkspaceCandidates() {
  const datalist = $("workspaceCandidates");
  if (!datalist) return;
  datalist.innerHTML = "";
  for (const path of workspaceRecentList()) {
    const option = document.createElement("option");
    option.value = window.OwoWorkspaceDisplay
      ? window.OwoWorkspaceDisplay.alias(path) : path;
    option.dataset.path = path;
    datalist.appendChild(option);
  }
}
if (document.getElementById("workspaceCandidates")) {
  $("workspace").addEventListener("focus", refreshWorkspaceCandidates);
}

const savedWorkspace = localStorage.getItem("owo.workspace");
if (savedWorkspace) {
  state.workspaceRoot = savedWorkspace;
  $("workspace").value = window.OwoWorkspaceDisplay
    ? window.OwoWorkspaceDisplay.alias(savedWorkspace) : "本地项目";
  $("workspace").title = "当前项目：" + $("workspace").value;
}
function applyTheme(theme) {
  const dark = theme === "dark";
  document.body.classList.toggle("dark-theme", dark);
  $("themeToggle").textContent = dark ? "☀" : "☾";
  $("themeToggle").title = dark ? "切换白色主题" : "切换深色主题";
  localStorage.setItem("owo.theme", dark ? "dark" : "light");
}
applyTheme(localStorage.getItem("owo.theme") || "light");
$("themeToggle").addEventListener("click", () => {
  const next = document.body.classList.contains("dark-theme") ? "light" : "dark";
  // 主题切换做整页交叉淡入（apple-design：明暗切换忌讳亮度跳变）。
  // View Transition 不可用或用户偏好减弱动态时直接切换，不引入动效。
  const reduceMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (document.startViewTransition && !reduceMotion) {
    document.startViewTransition(() => applyTheme(next));
  } else {
    applyTheme(next);
  }
});
function enableResize(handleId, variable, min, max, fromRight = false) {
  const handle = $(handleId);
  handle.addEventListener("pointerdown", (event) => {
    event.preventDefault();
    handle.setPointerCapture(event.pointerId);
    // 拖拽基准取会话栏左缘的实时位置，而不是硬编码 64px：
    // 功能栏宽度随断点变化（88/56/52px），旧值会让拖拽结果系统性偏移。
    const baseLeft = fromRight
      ? 0
      : Math.round(document.getElementById("sidebar").getBoundingClientRect().left);
    const move = (next) => {
      const raw = fromRight ? window.innerWidth - next.clientX : next.clientX - baseLeft;
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
// R10：原「显示工具与设置」折叠开关已移除（工具/开发者分区改由设置页子页承载）。
// 函数保留为 no-op 兼容旧调用点：`#toggleTools` 不存在时不得抛错（裸 addEventListener
// 会在 boot 阶段炸掉整个脚本，界面全白）。
function setToolsVisible(visible) {
  document.body.classList.toggle("show-tools", !!visible);
  const toggle = $("toggleTools");
  if (toggle) {
    toggle.setAttribute("aria-expanded", String(!!visible));
    toggle.textContent = visible ? "收起工具与设置" : "显示工具与设置";
  }
}
const legacyToolsToggle = $("toggleTools");
if (legacyToolsToggle) {
  legacyToolsToggle.addEventListener("click", () => {
    setToolsVisible(!document.body.classList.contains("show-tools"));
  });
}

// §12-12 开发者模式：MCP 原始配置 / Trace / Eval / 插件等内部能力分区
// 默认对普通用户隐藏，由明确开关启用并持久化（跨会话记忆）。
function applyDeveloperMode(enabled) {
  document.body.classList.toggle("dev-mode", enabled);
  const toggle = $("devModeToggle");
  if (toggle) toggle.checked = enabled;
}
function initDeveloperMode() {
  const stored = localStorage.getItem("owo.dev-mode");
  applyDeveloperMode(stored === "1");
  const toggle = $("devModeToggle");
  if (toggle) {
    toggle.addEventListener("change", () => {
      const enabled = toggle.checked;
      applyDeveloperMode(enabled);
      localStorage.setItem("owo.dev-mode", enabled ? "1" : "0");
    });
  }
}

const ROUTE_META = {
  chat: { title: "任务", description: "与单 Agent 对话，查看会话、审批和变更。" },
  projects: { title: "项目", description: "选择工作区、查看项目历史，并预览权限范围。" },
  workswarm: { title: "WorkSwarm", description: "把复杂任务拆给 Coordinator 与 Worker，集中查看进度与产物。" },
  artifacts: { title: "产物与待办", description: "处理 Human、Artifact 评审和 ChangeSet，再交付最终结果。" },
  // §4.5 统一权限中心：档位与实际范围、待审批、已授权与撤销、完全访问风险拆解。
  permissions: { title: "权限中心", description: "查看生效范围与来源，处理审批、撤销授权，并管理完全访问的风险与时长。" },
  // R10：模型一级入口（用户反馈"找不到模型切换/自定义地址与名称"）。
  model: { title: "模型", description: "切换服务提供方，设置自定义接口地址（base_url）与模型名称。" },
  settings: { title: "设置", description: "查看实际生效的模型连接、权限、用量和存储状态。" },
};

/// R10：模型页与设置页共用同一个 #settingsSection 容器（同一份 DOM，避免两套
/// 表单状态互相漂移），差异只在显示哪些块：
/// - model 页：只显示模型卡 + 工具能力 + 用量（最常改的三件事）；
/// - settings 页：全部子页（模型与数据 / 工具与自动化 / 开发者选项）。
function setModelPageMode(on) {
  document.body.classList.toggle("route-model", !!on);
  // 老 CSS 用 settings-open 控制侧栏收起动作；模型页借用同一语义（都是全幅设置面）。
  document.body.classList.toggle("settings-open", !!on);
}

function setSettingsLocation(inRoute, withTools) {
  const settings = $("settingsSection");
  const tools = $("toolsPanel");
  if (!settings) return;
  const sidebar = $("sidebar");
  const target = inRoute ? $("routeContent") : sidebar;
  if (target && settings.parentElement !== target) target.appendChild(settings);
  // §12-12 工具与高级系统并入设置路由：路由内作为可展开的工具子页随设置移动，
  // 聊天态回到侧栏折叠容器（普通用户默认只看技能/子代理/自动化/白名单）。
  // R10：模型页只借用 settingsSection，不接管 toolsPanel（否则模型页又会
  // 被"一堆参数"淹没——这正是用户投诉的形态）。
  const toolsTarget = withTools && inRoute ? target : sidebar;
  if (tools && toolsTarget && tools.parentElement !== toolsTarget) toolsTarget.appendChild(tools);
}

function navigate(route) {
  if (window.owoRouter) return window.owoRouter.go(route);
  renderRoute(route);
}

// §12-12 设置路由子页：模型与数据 / 工具与自动化 / 开发者选项。
// 工具与高级系统随设置路由挂载（setSettingsLocation 已把 toolsPanel 并入
// routeContent），子页切换只做显示/隐藏，不销毁状态；开发者选项分区
// 仍受 dev-mode 开关门控。
function renderSettingsTabs(content) {
  const nav = document.createElement("nav");
  nav.className = "settings-tabs";
  nav.setAttribute("aria-label", "设置子页");
  const tabs = [
    { key: "", label: "模型与数据", title: "模型连接、用量预算、存储与恢复" },
    { key: "tools", label: "工具与自动化", title: "技能、子代理、自动化、白名单与工具设置" },
    { key: "dev", label: "开发者选项", title: "MCP 原始配置、Trace、Eval 与扩展面板（需开发者模式）" },
  ];
  for (const tab of tabs) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = tab.label;
    button.title = tab.title;
    button.dataset.settingsTab = tab.key;
    button.classList.toggle("active", !tab.key);
    button.addEventListener("click", () => {
      const key = button.dataset.settingsTab;
      document.body.classList.toggle("settings-tab-tools", key === "tools");
      document.body.classList.toggle("settings-tab-dev", key === "dev");
      for (const sibling of nav.querySelectorAll("button")) {
        sibling.classList.toggle("active", sibling === button);
      }
    });
    nav.appendChild(button);
  }
  content.appendChild(nav);
  content.appendChild($("settingsSection"));
}

// §4.3 全局状态条：五段（后台/工作区/模型/权限/当前任务）全部可点击。
// **零新增 HTTP**（§8.2 首屏 ≤5 口径）：后台读壳 IPC 快照、工作区读前端 state、
// 模型走壳命令 get_provider_status（IPC）、权限档位由权限中心回灌、任务读运行态。
function initGlobalStatusBar() {
  const root = $("globalStatusBar");
  if (!root) return;
  // R12：状态条在 HTML 里位于 #main 内部，而设置/模型页会把 #main 收起（内容容器
  // 换成侧栏/内容区），于是状态条被一起藏掉、甚至在别的路由里挤进窄侧栏被裁切
  // （实测截图：五个状态块横着塞进 280px 的侧栏）。把它挪到 body 直属位置，
  // 生命周期与"哪条路由在用哪个容器"解耦：聊天页照常居中显示，非聊天页由 CSS 收起。
  if (root.parentElement !== document.body) {
    document.body.insertBefore(root, document.body.firstChild);
  }
  if (!window.OwoStatusBar || typeof window.OwoStatusBar.mount !== "function") {
    root.textContent = "状态条视图未加载";
    return;
  }
  window.OwoStatusBar.mount(root, {
    getFacts: () => ({
      workspaceRoot: state.workspaceRoot || $("workspace").value.trim(),
      reading: state.reading,
      pendingApproval: state.pendingApproval,
      lastTurnOutcome: state.lastTurnOutcome || "",
    }),
  });
}

window.addEventListener("owo:statusbar-navigate", (event) => {
  const detail = (event && event.detail) || {};
  const target = String(detail.target || "");
  if (detail.key === "permission") {
    // §4.5 权限中心已落地：状态条权限段直达权限页（此前临时落设置页）。
    navigate("permissions");
    return;
  }
  if (ROUTE_META[target]) navigate(target);
  else navigate("chat");
});

// §4.6 诊断请求台账：只在「设置」路由按需加载（不占用首屏 ≤5 请求口径）。
function refreshDiagnosticsLedger() {
  const root = $("diagnosticsLedger");
  if (!root) return;
  if (!window.OwoDiagnosticsLedger || typeof window.OwoDiagnosticsLedger.load !== "function") {
    root.textContent = "诊断台账视图未加载";
    return;
  }
  Promise.resolve(window.OwoDiagnosticsLedger.load(root, { force: true })).catch(() => {});
}

function renderRoute(route) {
  const meta = ROUTE_META[route] || ROUTE_META.chat;
  const isChat = route === "chat";
  document.body.classList.toggle("route-chat", isChat);
  document.body.classList.remove("tools-open", "settings-open", "settings-tab-tools", "settings-tab-dev");
  setModelPageMode(route === "model");
  document.querySelectorAll("[data-rail-target]").forEach((button) => {
    button.classList.toggle("active", button.dataset.railTarget === route);
  });
  const view = $("routeView");
  const content = $("routeContent");
  if (!view || !content) return;
  view.hidden = isChat;
  // §4.5 权限中心：离开路由即 dispose（清挂起 timer 与确认卡，本页禁止 setInterval）。
  if (window.OwoPanels && window.OwoPanels.permissions && window.OwoPanels.permissions.dispose) {
    window.OwoPanels.permissions.dispose();
  }
  if (isChat) {
    if (window.OwoPanels && window.OwoPanels.workswarm && window.OwoPanels.workswarm.dispose) {
      window.OwoPanels.workswarm.dispose();
    }
    setSettingsLocation(false);
    setToolsVisible(false);
    content.replaceChildren();
    return;
  }
  if (window.OwoPanels && window.OwoPanels.workswarm && window.OwoPanels.workswarm.dispose) {
    window.OwoPanels.workswarm.dispose();
  }
  // ⚠ 顺序是硬约束（2026-09-22 实测踩到并修）：`replaceChildren()` 会把**刚搬进去
  // 的** #settingsSection 一起删掉，节点于是从文档里彻底消失——现象就是"模型页/
  // 设置页完全空白"，而且因为节点没了，后续 refreshUsage/refreshSettings 全在对
  // null 赋值（真机栈：refreshUsage → app.js usagePanel textContent）。
  // 正确顺序：先把节点搬回侧栏（脱离内容区）→ 清空内容区 → 再按需搬进来。
  setSettingsLocation(false);
  content.replaceChildren();
  setSettingsLocation(route === "settings");
  $("routeHeader").innerHTML = `<div><h2>${esc(meta.title)}</h2><p>${esc(meta.description)}</p></div>`;
  if (route === "model") {
    // R10：模型页 = 同容器的"模型"视图（无子页签，避免再次把参数堆给用户）。
    setModelPageMode(true);
    if (window.OwoSettings && typeof window.OwoSettings.refresh === "function") window.OwoSettings.refresh();
    if (!serviceReady) return;
    refreshUsage();
    return;
  }
  if (route === "settings") {
    setModelPageMode(false);
    document.body.classList.add("settings-open");
    renderSettingsTabs(content);
    if (!serviceReady) return;
    refreshSettings();
    refreshUsage();
    refreshServerStatus();
    refreshDiagnosticsLedger();
    return;
  }
  const intro = document.createElement("div");
  intro.className = "route-intro-card";
  intro.innerHTML = `<strong>${esc(meta.title)}</strong><span>${esc(meta.description)}</span>`;
  content.appendChild(intro);
  const root = document.createElement("div");
  root.id = "routePanelRoot";
  content.appendChild(root);
  if (!serviceReady) {
    root.className = "";
    // §5.1.6：断连时所有路由复用同一 ServiceUnavailable 卡片（含重试/日志/设置出口）。
    window.renderOwoServiceError(root, new Error("核心服务未就绪"), () => recover());
    return;
  }
  if (route === "projects") mountPanel("project-launcher", root);
  if (route === "workswarm") mountPanel("workswarm", root);
  if (route === "artifacts") mountPanel("action-center", root);
  if (route === "permissions") mountPermissionsPanel(root);
}

// §4.5 权限中心：进入 permissions 路由时把面板挂到本路由专属容器（离开即 dispose）。
// 与其余一级路由同构：**只有挂载那一刻才发第一个请求**，不占用首屏 ≤5 口径。
function mountPermissionsPanel(root) {
  if (!root || !window.OwoPanels || !window.OwoPanels.permissions) return;
  window.OwoPanels.permissions.mount(root, panelHelpers(root));
  for (const button of document.querySelectorAll("#panelNav button")) {
    button.classList.toggle("active", button.dataset.panel === "permissions");
  }
}

window.owoRouter = window.OwoRouter
  ? new window.OwoRouter(ROUTE_META, renderRoute)
  : null;
for (const button of document.querySelectorAll("[data-rail-target]")) {
  button.addEventListener("click", () => navigate(button.dataset.railTarget));
}
// R10：侧栏底部「设置与诊断」按钮（取代原先需要先展开工具面板才能看到的入口）。
const openSettingsButton = $("openSettingsBtn");
if (openSettingsButton) {
  openSettingsButton.addEventListener("click", () => navigate("settings"));
}
// R10：模型设置面板（模型切换 / 自定义地址与模型名）随壳一起初始化；
// 表单状态只读壳 IPC，不占用首屏 HTTP 预算。
if (window.OwoSettings && typeof window.OwoSettings.init === "function") {
  window.OwoSettings.init();
}
$("workspace").addEventListener("change", () => {
  const workspace = $("workspace").value.trim();
  if (!workspace) return;
  // 兼容旧版直接粘贴绝对路径；日常显示收敛为目录别名。
  if (/[\\/]|^[A-Za-z]:/.test(workspace)) {
    state.workspaceRoot = workspace;
    localStorage.setItem("owo.workspace", workspace);
    rememberWorkspace(workspace);
    $("workspace").value = window.OwoWorkspaceDisplay
      ? window.OwoWorkspaceDisplay.alias(workspace) : "本地项目";
    $("workspace").title = "当前项目：" + $("workspace").value;
  }
});

// §4.4 文件夹必须走 Tauri 原生目录选择器：桌面壳里输入框转只读、由
// `choose_project_directory` 设定（壳侧校验 + 持久化 + 受控重启核心）。
// 上面那段手输兼容只在浏览器 dev 模式生效——那里没有原生选择器，
// 模块会把按钮禁用并写明降级原因，不会假装成功。
function applyWorkspacePicked(workspace) {
  state.workspaceRoot = workspace;
  localStorage.setItem("owo.workspace", workspace);
  rememberWorkspace(workspace);
  refreshWorkspaceCandidates();
  if (window.OwoStatusBar && typeof window.OwoStatusBar.repaint === "function") {
    window.OwoStatusBar.repaint();
  }
  // 换工作区 = 壳已重拉核心（新代际）：连接描述符里的端口/token 全部作废。
  if (window.OwoApi && typeof window.OwoApi.resetCoreConnection === "function") {
    window.OwoApi.resetCoreConnection();
  }
  addMessage("system", `项目工作区已切换为 ${window.OwoFolderPicker.aliasFor(workspace)}，正在等待核心就绪…`);
  recover().catch(() => { /* 终态由服务错误卡呈现 */ });
}
if (window.OwoFolderPicker && $("workspace") && $("chooseWorkspace")) {
  const picker = window.OwoFolderPicker.attach($("workspace"), $("chooseWorkspace"), {
    title: "选择项目工作区（原生目录选择器）",
    onPicked: applyWorkspacePicked,
    onCanceled: () => { /* 取消是合法终态：保持原值，不改状态、不报错 */ },
    onError: (message) => addMessage("error", `选择工作区失败：${message}`),
  });
  const hint = $("workspacePickerHint");
  if (hint && !picker.native) {
    hint.textContent = "浏览器开发模式：无原生目录选择器，可直接填写路径（仅开发用，桌面版不适用）。";
  }
}
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
document.querySelectorAll("#approvalBar .approval-actions button").forEach((btn) => {
  btn.addEventListener("click", () => respondApproval(btn.dataset.scope || "once"));
});
$("abortBtn").addEventListener("click", async () => {
  if (!state.reading || !state.sessionId) return;
  try {
    await api(`/session/${state.sessionId}/abort`, { method: "POST" });
  } catch (_) {
    // 服务端可能已结束回合，忽略
  }
  if (state.abortController) state.abortController.abort();
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
    addMessage("system", "请先选择 .owskill 文件");
    return;
  }
  importPackage(file);
  $("skillImport").value = "";
});
$("automationForm").addEventListener("submit", (event) => {
  event.preventDefault();
  createAutomation();
});
$("clearReminders").addEventListener("click", async () => {
  try {
    await api("/automations/reminders/clear", { method: "POST" });
    await refreshReminders();
  } catch (error) {
    addMessage("error", `清除提醒失败：${error.message || error}`);
  }
});
$("egressToggle").addEventListener("click", async () => {
  const enabled = $("egressToggle").dataset.enabled !== "true";
  try {
    await api("/settings/egress", {
      method: "POST",
      body: JSON.stringify({ cloud_enabled: enabled }),
    });
    await refreshSettings();
    addMessage("system", `云端模型已${enabled ? "开启" : "关闭"}（已即时生效）`);
  } catch (error) {
    addMessage("error", `切换云端模型失败：${error.message || error}`);
  }
});
$("settingsSave").addEventListener("click", async () => {
  try {
    const settings = JSON.parse($("settingsEditor").value);
    settings.model = $("settingsModel").value;
    settings.tool_capabilities = {
      desktop_observation: $("toolCapDesktopObservation").checked,
      desktop_control: $("toolCapDesktopControl").checked,
      browser: $("toolCapBrowser").checked,
    };
    const resp = await api("/settings", {
      method: "POST",
      body: JSON.stringify(settings),
    });
    addMessage("system", `设置已保存：${(resp && resp.note) || "ok"}`);
    for (const elementId of ["toolCapDesktopObservation", "toolCapDesktopControl", "toolCapBrowser"]) {
      $(elementId).dataset.dirty = "false";
    }
    await refreshSettings();
  } catch (error) {
    addMessage("system", `设置保存失败：${error.message || error}`);
  }
});
for (const elementId of ["toolCapDesktopObservation", "toolCapDesktopControl", "toolCapBrowser"]) {
  $(elementId).addEventListener("change", () => {
    $(elementId).dataset.dirty = "true";
  });
}
$("storageBackupBtn").addEventListener("click", () => storageBackup());
$("storageExportBtn").addEventListener("click", () => storageExport());
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
// §12-13 MCP 按传输切换字段（本地命令 vs HTTP 服务），避免两字段同时裸露。
function syncMcpFields() {
  const transport = $("mcpTransport") ? $("mcpTransport").value : "stdio";
  $("mcpCommandGroup").hidden = transport !== "stdio";
  $("mcpUrlGroup").hidden = transport !== "http";
}
if (document.getElementById("mcpTransport")) {
  $("mcpTransport").addEventListener("change", syncMcpFields);
}
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
$("prompt").addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    sendPrompt();
  }
});
$("micBtn").addEventListener("click", async () => {
  if (listening) {
    if (localRecorder) {
      const { blob, sampleCount } = await localRecorder.stop();
      localRecorder = null;
      listening = false;
      $("micBtn").textContent = "🎤";
      if (sampleCount < 1600) {
        addMessage("system", "录音太短，未识别");
        return;
      }
      try {
        const result = await apiClient.upload("/stt/transcribe", blob, "audio/wav");
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
  $("micBtn").textContent = "🔴";
  const started = await startLocalRecording();
  if (!started) {
    listening = false;
    $("micBtn").textContent = "🎤";
    if (!recognition) {
      initSpeech();
    }
    if (recognition) {
      try {
        recognition.start();
      } catch (_) {
        listening = false;
        $("micBtn").textContent = "🎤";
        alert("无法访问麦克风或系统语音识别");
      }
    } else {
      alert("无法访问麦克风（请允许麦克风权限）");
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

// §3.1/§3.2/§3.4 事件失效网络：域变化经 /events/stream（Bearer fetch-stream）
// 推送 invalidate 事件，前端只刷新对应域一次（对比版本号防重复），替代按域轮询。
// 单一连接；core 连接描述变化 / 页面卸载 / 恢复流程重建时由 stopInvalidation()
// 取消旧 reader 与 pending timer，再建立新连接（无双连接）。
const INVALIDATE_HANDLERS = {
  automations: refreshAutomations,
  whitelist: refreshWhitelist,
  mcp: refreshMcp,
  settings: refreshSettings,
  packages: refreshPackages,
  learn: refreshLearn,
  plugins: refreshPlugins,
  computer: refreshComputerTasks,
  traces: refreshTraces,
  projects: refreshProjectRules,
  // 任务 2：mutation 失效发布补全的四个领域（服务端 event_stream 同步新增）。
  memory: refreshObservations,
  skills: refreshSkills,
  sessions: refreshSessions,
  usage: refreshUsage,
};
let invalidator = null;
let invalidationKey = null;
// §8.2/§3.4 唯一"界面是否不可见"口径。
//
// 只看 document.visibilityState 在桌面壳里是不够的：实测 `window.hide()`
// （wry set_visible → WebView2 SetIsVisible）后窗口在 Win32 层已不可见，
// 页面可见性却仍是 "visible"，于是隐藏期照样按定时器打业务请求。
// 壳在隐藏/唤回时注入 owoSetBackground（main.rs sync_window_background），
// 浏览器直连场景仍走 visibilitychange——两个信号取并集，任一为隐藏即视为隐藏。
let shellBackgroundHidden = false;
function uiHidden() {
  if (shellBackgroundHidden) return true;
  return typeof document !== "undefined" && document.visibilityState === "hidden";
}
window.owoSetBackground = function (hidden) {
  shellBackgroundHidden = Boolean(hidden);
  if (window.OwoInvalidation && typeof window.OwoInvalidation.setShellBackground === "function") {
    // events.js 的可见性口径与这里保持同一事实（避免两套判断）。
    window.OwoInvalidation.setShellBackground(shellBackgroundHidden);
  }
  if (!shellBackgroundHidden && invalidator && typeof invalidator.onVisibility === "function") {
    invalidator.onVisibility(); // 补刷隐藏期间攒下的域失效（恰好一次）
  }
  return uiHidden();
};
window.owoUiHidden = uiHidden;

function startInvalidation() {
  if (!window.OwoInvalidation) return;
  const base = apiClient.baseUrl || API_BASE;
  // R3（§8.2）：幂等键必须含**实例身份**——core 被重启后端口可能被系统复用，
  // 仅比对 base 会保留旧订阅器（其 Last-Event-ID 游标属于上一代进程，seq 已
  // 重新计数），新事件的版本号落在游标之下 → 被当作 duplicate 丢弃，域不再刷新。
  const key = base + "#" + (apiClient.coreInstanceId || "");
  if (invalidator && invalidationKey === key) return;
  stopInvalidation();
  invalidationKey = key;
  invalidator = window.OwoInvalidation.createDomainInvalidator({
    baseUrl: base,
    openStream: (path, streamOptions) => apiClient.openEventStream(path, streamOptions),
  });
  for (const [domain, handler] of Object.entries(INVALIDATE_HANDLERS)) {
    invalidator.on(domain, handler);
  }
  // §3.4：事件流 Degraded 时的唯一兜底调度器（隐藏窗口不发业务请求）。
  invalidator.setPollFallback(() => {
    if (uiHidden()) return; // 隐藏期不发业务请求（§3.4/§8.2）
    for (const handler of Object.values(INVALIDATE_HANDLERS)) {
      Promise.resolve().then(handler).catch(() => {});
    }
  });
  invalidator.start();
  // 任务 3 尾：诊断页只读访问器（observability 面板消费 snapshot，不改计数）。
  window.owoInvalidatorState = () => (invalidator ? invalidator.snapshot() : null);
}
function stopInvalidation() {
  if (invalidator) {
    invalidator.stop();
    invalidator = null;
  }
  invalidationKey = null;
}

// §3.4 生命周期：core 连接断开（owo:connection ready=false）即停事件流；
// 恢复后由 recover()/refresh 流程重建。页面卸载取消 reader 与全部 timer。
window.addEventListener("owo:connection", (event) => {
  if (event.detail && event.detail.ready) markConnectionReady();
  else {
    markConnectionUnavailable();
    stopInvalidation();
  }
});
window.addEventListener("beforeunload", () => {
  stopInvalidation();
});

// §12-14 首屏请求收敛：总请求 ≤5（健康检查先行 1 个 + 4 个无定时兜底的水合任务）。
// 其余面板刷新器（plugins/packages/suggestions/automations/reminders/settings/
// usage/serverStatus/audit/perception/learn/observations/skillHealth/mcp/traces/
// computerTasks）一律由 REFRESH_PLANS 定时器按路由可见性兜底，不再占用首屏窗口；
// 这些函数必须出现在下面的 BOOT_LAZY_TASKS 中，防止从两处同时遗漏。
const BOOT_LAZY_TASKS = [
  refreshPlugins,
  refreshPackages,
  refreshSuggestions,
  refreshAutomations,
  refreshReminders,
  refreshSettings,
  refreshUsage,
  refreshServerStatus,
  refreshAudit,
  refreshPerception,
  refreshLearn,
  refreshObservations,
  refreshSkillHealth,
  refreshMcp,
  refreshTraces,
  refreshComputerTasks,
];
const BOOT_HYDRATE_TASKS = [
  // 首屏必需且 REFRESH_PLANS 没有兜底的 4 项：会话、技能、白名单、项目规则。
  refreshSessions,
  refreshSkills,
  refreshWhitelist,
  refreshProjectRules,
];

async function hydrateShell() {
  await refreshHealth();
  return window.OwoRecovery.runWithConcurrency(BOOT_HYDRATE_TASKS, 5);
}

// 单飞恢复控制器（boot 失败路径创建；recover() 复用同一实例合并触发）。
let activeRecovery = null;

// §5.1.8：OpenAPI 链接必须由 API base 构造绝对地址；Tauri 静态资源下相对
// openapi.json 会指向壳内不存在的路径。
function syncOpenApiLink() {
  const link = $("openapiLink");
  if (link) link.href = apiClient.baseUrl + "/openapi.json";
}

// §4.6 首次配置判定：壳报告 no_workspace 时 core 不会启动，
// readiness 轮询永远不成功，必须直接进入工作区/提供商引导。
// R3-B（§3.4 provider 契约）：core ready 但提供商未配置时同样先分流引导页
// ——模型不可用的正确终态是"模型配置引导"，不是错误卡/loading（壳 IPC 判定，零 HTTP）。
// 非 Tauri 环境（浏览器直连 4096）跳过——那里没有壳来管理工作区。
// §3.4 引导门的取样窗口：壳侧过渡态（starting/restarting）里"提供商未配置"这个
// 事实还没成立，一次性快照会把 §3.4 规定的引导页终态判成"直接进主界面"
// （真机矩阵 provider-unset 三条红：core 现在**按契约正常启动**，占位提供商只在
// 模型调用时返回稳定码，所以引导判据只能等壳侧出终态之后再取）。
// 等待只走 Tauri IPC（ensureCoreConnection 非 ready 时不缓存），零 HTTP，
// 不影响 §8.2 首屏 ≤5 请求口径；超时按"不进引导"处理，让真正的故障走错误卡。
const SETUP_GATE_SETTLE_MS = 6000;
const SETUP_GATE_TICK_MS = 250;

async function needsSetup() {
  const owner = window.OwoApiClient && window.OwoApiClient.tauriInvokeOwner(window);
  if (!owner) return false;
  const deadline = Date.now() + SETUP_GATE_SETTLE_MS;
  let connection = null;
  try {
    for (;;) {
      connection = await apiClient.ensureCoreConnection();
      if (!connection) return false;
      if (connection.state === "no_workspace") return true;
      // §3.4「provider 未配置」的**规定终态是模型配置引导，不是错误卡**。历史上只认
      // ready + 壳配置视图一条路径，无密钥场景被渲染成通用错误卡（矩阵三条断言全红），
      // 而归因错误的错误卡会让用户去查网络/重装，永远修不好。
      if (connection.errorCode === "provider/not_configured") return true;
      if (connection.state === "ready" || connection.state === "failed") break;
      if (Date.now() >= deadline) return false;
      await new Promise((resolve) => setTimeout(resolve, SETUP_GATE_TICK_MS));
    }
    // 已出终态的失败（core/exited、storage/not_writable…）不是"未配置提供商"：
    // 那类必须落错误卡 + 稳定码动作，不能被引导页盖掉归因。
    if (connection.state !== "ready") return false;
    // 到这一步两份视图已经同口径：壳的 provider_status 与 core 一样按
    // "显式选择 > 环境凭据"判定（provider.rs），因此 ready 之后取一次不会
    // 把有密钥的健康启动顶进引导页（R3-BUG-23 的教训），也不会漏掉真没凭据的机器。
    const status = await owner.invoke.call(owner, "get_provider_status");
    return !!(status && status.ready === false);
  } catch (_) {
    return false;
  }
}

// §4.6：渲染首次配置引导；完成后落在 recover()（服务恢复轮询）。
// R3-B 缺陷修正：app.js 是顶层脚本（非 (function(global){…}) 包装的视图模块），
// 作用域里没有 `global` 标识符——上一版这里写 `global.__owoCoreDiagnostics` 抛
// ReferenceError，`content.replaceChildren()` 已清空却在渲染前中断，界面表现为
// "标题有了、正文空白"（矩阵取证：contentLen=0 / errs=rej:global is not defined）。
// 除修标识符外，这里再加兜底：引导页自身异常也必须落到错误卡，绝不允许白屏。
function renderSetupGuide() {
  document.body.classList.remove("route-chat");
  const view = $("routeView");
  const content = $("routeContent");
  if (!view || !content) return;
  view.hidden = false;
  $("routeHeader").innerHTML =
    "<div><h2>首次配置</h2><p>选择项目工作区与模型提供商后即可开始使用。</p></div>";
  content.replaceChildren();
  if (!window.renderOwoSetupGuide) {
    if (window.renderOwoServiceError) {
      renderOwoServiceError(content, new Error("引导视图未加载"), recover);
    }
    return;
  }
  try {
    window.renderOwoSetupGuide(content, window.__owoCoreDiagnostics || null, () => recover());
  } catch (error) {
    // 引导页抛错时不能留下空白主区：回落到错误卡，保留可操作出口。
    content.replaceChildren();
    if (window.renderOwoServiceError) renderOwoServiceError(content, error, recover);
  }
}

// §7：直连路径（重试按钮/恢复服务）也单飞——连点合并为一次恢复流程，
// 失败路径的单飞控制器见 boot() 中的 activeRecovery（定时驱动共用）。
let recoverInFlight = null;
async function recover() {
  if (activeRecovery) return activeRecovery.trigger();
  if (recoverInFlight) return recoverInFlight;
  recoverInFlight = (async () => {
    if (await needsSetup()) {
      renderSetupGuide();
      return;
    }
    const readiness = new window.OwoServiceReadiness(apiClient);
    try {
      await readiness.wait(10000);
      await hydrateShell();
      // 与 boot() 同一结论：ready 之后不得用壳的 provider 配置面二次改判终态，
      // 唯一判据是核心上报的 provider/not_configured 稳定码（见 boot() 里的注记）。
      serviceReady = true;
      if (window.owoRouter) window.owoRouter.start();
      startRefreshTimers();
      startInvalidation();
    } catch (error) {
      const routeContent = $("routeContent");
      if (routeContent && window.renderOwoServiceError) renderOwoServiceError(routeContent, error, recover);
    }
  })().finally(() => {
    recoverInFlight = null;
  });
  return recoverInFlight;
}

// §12-13 约束控件：自动化三态、MCP 传输切换、动作/应用 chips 一次初始化。
function initConstrainedControls() {
  if (document.getElementById("autoKind")) syncAutomationFields();
  if (document.getElementById("mcpTransport")) syncMcpFields();
  renderChipGroup("cuActionsChips", "actions");
  renderChipGroup("sinkAppsChips", "apps");
}

async function boot() {
  initSpeech();
  initPanels();
  initDeveloperMode();
  initConstrainedControls();
  // 状态条必须在水合之前就位：引导页/错误页也要能看出「后台在启动」还是「失败」，
  // 而且它在 boot 早期挂载，不参与任何 HTTP 计数。
  initGlobalStatusBar();
  syncOpenApiLink();
  if (await needsSetup()) {
    renderSetupGuide();
    return;
  }
  const readiness = new window.OwoServiceReadiness(apiClient);
  try {
    await readiness.wait(10000);
    await hydrateShell();
    // ⚠ 这里**不得**再用壳的 get_provider_status 复查一次提供商来决定进不进引导页。
    // 实测（§8.2 冷启动复跑）：密钥由 sidecar 环境注入、core 正常 ready 的健康启动里，
    // 壳自己的 provider 配置面仍可报 ready=false —— 于是引导页把健康主界面整个顶掉
    // （composerVisible=false、setupGuide=407 字），并且因为 boot() 提前 return，
    // 首屏请求/SSE 计数全部失控（business=16、events=4、同路由 x4）。
    // 「provider 未配置」的正确判据是**核心自己上报的稳定码**
    // （needsSetup 里 connection.errorCode === "provider/not_configured"）：
    // 它是唯一权威归因，不用两份互不一致的配置视图去猜。
    serviceReady = true;
    if (window.owoRouter) window.owoRouter.start();
  } catch (error) {
    document.body.classList.remove("route-chat");
    $("routeView").hidden = false;
    $("routeHeader").innerHTML = "<div><h2>服务连接</h2><p>正在等待本地核心服务恢复。</p></div>";
    const routeContent = $("routeContent");
    if (routeContent && window.renderOwoServiceError) {
      renderOwoServiceError(routeContent, error, () => recover());
    }
    // §6.1 单飞恢复：定时驱动、错误卡重试、owoRecoverService 并发触发时
    // 合并为同一次恢复；失败按 [0,500,1000,2000,5000] 退避，成功后重置。
    const recovery = window.OwoRecovery.createRecoveryController(async () => {
      if (window.OwoApi && window.OwoApi.resetCoreConnection) window.OwoApi.resetCoreConnection();
      await readiness.wait(2500);
      await hydrateShell();
      serviceReady = true;
      if (window.owoRouter) window.owoRouter.start();
      startRefreshTimers();
      startInvalidation();
      syncOpenApiLink();
      window.owoRecoverService = null;
      activeRecovery = null;
    });
    activeRecovery = recovery;
    const recoveryDriver = setInterval(() => {
      if (serviceReady) {
        clearInterval(recoveryDriver);
        return;
      }
      recovery.trigger().catch(() => {}).then(() => {
        if (serviceReady) clearInterval(recoveryDriver);
      });
    }, 2500);
    window.owoRecoverService = () => recovery.trigger();
    return;
  }
  startRefreshTimers();
  startInvalidation();
}

let refreshTimersStarted = false;

// §6.1/§12-15：后台刷新带「路由可见性」标注——侧栏刷新器只在任务页跑，
// 设置区刷新器只在路由页跑；页面隐藏（最小化/切走）时整体暂停。
// 已由 invalidate 事件驱动的域降为 ≤10 分钟低频兜底（事件断线时仍能自愈），
// 不再按 3~30 秒轮询；health 保留 30 秒心跳。
const REFRESH_PLANS = [
  { refresh: refreshHealth, intervalMs: 30000, routes: "*" },
  { refresh: refreshPerception, intervalMs: 180000, routes: "chat" },
  { refresh: refreshLearn, intervalMs: 600000, routes: "chat" },
  { refresh: refreshPlugins, intervalMs: 600000, routes: "chat" },
  { refresh: refreshPackages, intervalMs: 600000, routes: "chat" },
  { refresh: refreshSuggestions, intervalMs: 300000, routes: "chat" },
  { refresh: refreshAudit, intervalMs: 300000, routes: "chat" },
  { refresh: refreshAutomations, intervalMs: 600000, routes: "chat" },
  { refresh: refreshReminders, intervalMs: 300000, routes: "chat" },
  { refresh: refreshObservations, intervalMs: 600000, routes: "chat" },
  { refresh: refreshSkillHealth, intervalMs: 600000, routes: "chat" },
  { refresh: refreshMcp, intervalMs: 600000, routes: "chat" },
  { refresh: refreshTraces, intervalMs: 600000, routes: "chat" },
  { refresh: refreshComputerTasks, intervalMs: 600000, routes: "chat" },
  { refresh: refreshSettings, intervalMs: 300000, routes: "notChat" },
  { refresh: refreshUsage, intervalMs: 120000, routes: "notChat" },
  { refresh: refreshServerStatus, intervalMs: 300000, routes: "notChat" },
];

function routeActive(routes) {
  if (routes === "*") return true;
  const current = window.owoRouter && window.owoRouter.current ? window.owoRouter.current : "chat";
  if (routes === "notChat") return current !== "chat";
  if (Array.isArray(routes)) return routes.includes(current);
  return current === routes;
}

function scheduleRefresh(refresh, intervalMs, routes) {
  let running = false; // 防重入：上一轮未完成时不叠加下一轮
  setInterval(() => {
    // uiHidden() 而不是 visibilityState：桌面壳收进后台后页面仍是 "visible"
    // （实测隐藏 5 分钟里 30s 健康轮询照打 10 次）。
    if (running || uiHidden()) return;
    if (!routeActive(routes)) return;
    running = true;
    Promise.resolve()
      .then(refresh)
      .catch(() => {
        // API 客户端已负责更新连接状态；后台刷新失败不应制造未处理拒绝。
      })
      .finally(() => {
        running = false;
      });
  }, intervalMs);
}

function startRefreshTimers() {
  if (refreshTimersStarted) return;
  refreshTimersStarted = true;
  for (const plan of REFRESH_PLANS) {
    scheduleRefresh(plan.refresh, plan.intervalMs, plan.routes);
  }
}

boot();
