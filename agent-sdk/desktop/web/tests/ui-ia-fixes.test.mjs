import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const read = (relative) => readFileSync(join(here, relative), "utf8");

test("§5.1.4 用户界面不出现 API 路径（团队/产物页）", () => {
  for (const file of ["../panels/workswarm.panel.js", "../panels/action-center.panel.js"]) {
    const source = read(file);
    const hintSpans = source.match(/<span class="hint">[^<]*<\/span>/g) || [];
    for (const span of hintSpans) {
      assert.doesNotMatch(span, /(GET|POST|PUT|DELETE) \//, `${file} 的 hint 文案不得包含 API 路径：${span}`);
    }
    const titleAttrs = source.match(/title="[^"]*"/g) || [];
    for (const attr of titleAttrs) {
      assert.doesNotMatch(attr, /(GET|POST|PUT|DELETE) \//, `${file} 的 title 提示不得包含 API 路径：${attr}`);
    }
  }
  const actionCenter = read("../panels/action-center.panel.js");
  assert.ok(!actionCenter.includes("正式 Inbox（/human/inbox）"), "产物页不得用 API 路径当标题");
  const workswarm = read("../panels/workswarm.panel.js");
  assert.ok(!workswarm.includes("团队列表 GET /teams"), "错误文案不得包含 API 路径");
});

test("§5.1.6 断连态统一：所有路由复用同一 ServiceUnavailable 卡片", () => {
  const view = read("../views/service-error.view.js");
  for (const marker of ["errorCode", "logPath", "open_core_logs", "打开模型设置", "owoRouter"]) {
    assert.ok(view.includes(marker), `统一断连卡缺少能力：${marker}`);
  }
  const app = read("../app.js");
  assert.match(
    app,
    /if \(!serviceReady\) \{\s*root\.className = "";\s*\/\/ §5\.1\.6：断连时所有路由复用同一 ServiceUnavailable 卡片（含重试\/日志\/设置出口）。\s*window\.renderOwoServiceError\(root, new Error\("核心服务未就绪"\), \(\) => recover\(\)\);/,
    "路由等待态必须复用统一断连卡，不得退回纯文本"
  );
  assert.ok(!app.includes("正在等待本地核心服务就绪…"), "旧的纯文本等待态必须移除");
});

test("§5.1.8 OpenAPI 链接由 API base 构造绝对地址", () => {
  const index = read("../index.html");
  assert.match(index, /id="openapiLink"/);
  const app = read("../app.js");
  assert.match(app, /function syncOpenApiLink\(\) \{\s*const link = \$\("openapiLink"\);\s*if \(link\) link\.href = apiClient\.baseUrl \+ "\/openapi\.json";/);
  assert.match(app, /syncOpenApiLink\(\);\s*if \(await needsSetup\(\)\) \{\s*renderSetupGuide\(\);\s*return;\s*\}\s*\n\s*const readiness = new window\.OwoServiceReadiness/, "boot 时必须同步一次（§4.6 no_workspace 先分流引导）");
});

test("§4.6/§4.8 首次配置引导：NoWorkspace 分流 + 提供商选择", () => {
  const app = read("../app.js");
  assert.match(app, /async function needsSetup\(\)/, "必须提供 needsSetup 判定");
  // 断言"接线意图"而不是抄实现字面量：上一版这里逐字写了 `global.__owoCoreDiagnostics`，
  // 等于把 ReferenceError 缺陷固化进契约（app.js 是顶层脚本，没有 global 标识符）。
  // 现在要求：渲染时携带壳快照（window.__owoCoreDiagnostics）且完成后回调 recover。
  assert.match(
    app,
    /renderOwoSetupGuide\(\s*content,\s*window\.__owoCoreDiagnostics[\s\S]{0,60}?=>\s*recover\(\)\s*\)/,
    "引导渲染必须带壳快照并把完成动作接回 recover"
  );
  assert.match(app, /catch\s*\(error\)[\s\S]{0,200}renderOwoServiceError\(content,\s*error,\s*recover\)/,
    "引导抛错必须回落错误卡（不允许主区空白）");
  const guide = read("../views/setup-guide.view.js");
  assert.match(guide, /get_workspace/, "引导读取当前工作区");
  assert.match(guide, /set_workspace/, "引导可设置工作区");
  assert.match(guide, /get_provider_status/, "引导读取提供商状态");
  assert.match(guide, /set_provider/, "引导可保存提供商选择");
  assert.ok(!guide.includes("OPENAI_API_KEY") || guide.includes("不显示内容"), "密钥内容永不回前端（仅布尔/端点/模型名）");
  const index = read("../index.html");
  assert.match(index, /views\/setup-guide\.view\.js/, "引导视图必须加载");
  const css = read("../style.css");
  assert.match(css, /\.setup-card/, "引导卡片样式必须存在");
});

test("§5.1.2 最小窗口（900×600）rail 不再越界", () => {
  const css = read("../style.css");
  const compact = css.match(/@media \(max-width: 1180px\) \{[\s\S]*?\n\}/);
  assert.ok(compact, "1180px 断点必须存在");
  assert.match(compact[0], /\.rail-button \{ width: 48px/, "窄布局必须压缩 rail 按钮宽度（列宽 56px）");
  assert.match(css, /\.rail-button \{ width: 44px; \}/, "860px 断点进一步压缩");
});

test("§5.1.3 项目页：深度改三段选项，原始数值进高级折叠", () => {
  const panel = read("../panels/project-launcher.panel.js");
  assert.match(panel, /data-pl-depth="2"/);
  assert.match(panel, /data-pl-depth="4"/);
  assert.match(panel, /data-pl-depth="8"/);
  assert.match(panel, /快速 · 2 层/);
  assert.match(panel, /标准 · 4 层/);
  assert.match(panel, /深入 · 8 层/);
  assert.match(panel, /<details class="owo-pl-advanced"><summary>自定义深度（1–8）<\/summary>/, "原始深度输入必须收进高级折叠");
  assert.ok(!panel.includes("TeamRun 的 Agent Worker 将以该目录为工作区"), "内部执行参数不得出现在任务创建流程");
  assert.match(panel, /允许写入路径/, "受控写入约束标签必须保留");
});

test("§5.5 审批卡：脱敏展示 + 原始 JSON 收进开发者详情 + scope 选项", () => {
  // §12.3 模块拆分后审批卡实现位于 app-domain.js（唯一归属，见模块边界守卫）。
  const domain = read("../app-domain.js");
  assert.match(domain, /function describeApproval\(payload\)/, "审批卡必须提供可解释摘要");
  assert.match(domain, /将写入：/, "写文件显示目标路径");
  assert.match(domain, /将执行命令：/, "命令显示可执行程序与参数");
  assert.match(domain, /将联网访问：/, "联网显示域名");
  assert.match(domain, /能否撤销/, "审批卡回答能否撤销");
  assert.match(domain, /approvalRawJson/, "原始 JSON 收进开发者详情");
  assert.match(domain, /alwaysAllowBtn/, "破坏性操作隐藏始终允许按钮");
  assert.match(domain, /body\.scope = scope/, "响应携带 scope 选项");
  const index = read("../index.html");
  assert.match(index, /id="approvalExplain"/, "审批条含说明区");
  assert.match(index, /id="approvalRaw"/, "审批条含开发者详情折叠");
  assert.match(index, /data-scope="session"/, "审批卡提供「此会话」选项");
  assert.match(index, /data-scope="one_hour"/, "审批卡提供「此项目一小时」");
  assert.match(index, /data-scope="always_readonly"/, "审批卡提供「始终允许此只读动作」");
  const css = read("../style.css");
  assert.match(css, /\.approval-actions/, "审批选项样式存在");
});

test("§12-13 自由文本改约束控件：自动化三态 / MCP 传输切换 / CSV chips / 白名单候选", () => {
  const index = read("../index.html");
  const app = read("../app.js");
  // 自动化：不再有 60 / 09:00 / RFC3339 混合自由文本框。
  assert.ok(!index.includes('id="autoValue"'), "旧的自由文本 autoValue 必须移除");
  for (const id of ["autoIntervalSecs", "autoDailyTime", "autoOnceAt"]) {
    assert.match(index, new RegExp(`id="${id}"`), `自动化三态表单缺少 ${id}`);
  }
  assert.match(index, /<div id="autoIntervalGroup" class="auto-group"/, "间隔表单分组");
  assert.match(index, /<div id="autoDailyGroup" class="auto-group"/, "每日表单分组");
  assert.match(index, /<div id="autoOnceGroup" class="auto-group"/, "指定时间表单分组");
  assert.match(app, /function syncAutomationFields\(\)/, "自动化表单必须按触发方式切换显隐");
  assert.match(app, /new Date\(local\)\.getTimezoneOffset/, "指定时间必须转本地显式时区 RFC3339");
  assert.match(index, /<div id="mcpCommandGroup" class="auto-group"/, "MCP 命令分组");
  assert.match(index, /<div id="mcpUrlGroup" class="auto-group"/, "MCP URL 分组");
  assert.match(app, /function syncMcpFields\(\)/, "MCP 传输切换必须同步字段显隐");
  // CSV → chips。
  assert.ok(!index.includes('id="cuActions"'), "cuActions 自由文本必须移除");
  assert.ok(!index.includes('id="sinkApps"'), "sinkApps 自由文本必须移除");
  assert.match(index, /id="cuActionsChips"/, "Computer-use 动作必须用 chips 多选");
  assert.match(index, /id="sinkAppsChips"/, "技能应用必须用 chips 多选");
  assert.match(app, /function renderChipGroup\(hostId, catalogKey/, "必须提供 chips 渲染工具");
  assert.match(app, /function getSelectedChips\(hostId\)/, "必须提供 chips 取值工具");
  assert.match(app, /chipOptionsCatalog\(\)/, "chips 候选必须来自能力注册表目录");
  // 白名单：可搜索 datalist 候选（§12.3 拆分后 refreshWhitelist 位于 app-domain.js）。
  assert.match(index, /id="wlAppId"[^>]*list="wlAppCandidates"/, "白名单 app id 必须接 datalist");
  assert.match(index, /<datalist id="wlAppCandidates">/, "白名单 datalist 候选容器");
  assert.match(read("../app-domain.js"), /const datalist = \$\("wlAppCandidates"\)/, "白名单刷新必须回填候选");
  assert.match(app, /function initConstrainedControls\(\)/, "约束控件必须一次性初始化");
  // 工作区路径：最近项目 datalist（普通模式不要求手写完整路径）。
  assert.match(index, /id="workspace"[^>]*list="workspaceCandidates"/, "工作区输入必须接最近项目 datalist");
  assert.match(index, /<datalist id="workspaceCandidates">/, "最近项目 datalist 容器");
  assert.match(app, /localStorage\.getItem\("owo\.recent-workspaces"\)/, "最近项目必须本地记忆");
  assert.match(app, /function rememberWorkspace\(path\)/, "选择工作区必须记录到最近项目");
  assert.match(app, /function refreshWorkspaceCandidates\(\)/, "最近项目候选必须可刷新");
  // settings 原始 JSON 编辑保持 json-fallback 兜底（普通模式不裸露）。
  assert.match(index, /id="settingsEditor" class="json-fallback"/, "原始 JSON 编辑器必须保持在无 JS 兜底层");
  for (const id of ["toolCapDesktopObservation", "toolCapDesktopControl", "toolCapBrowser"]) {
    assert.match(index, new RegExp(`id="${id}" type="checkbox"`), `能力设置页缺少显式开关 ${id}`);
  }
  assert.match(index, /id="toolCapabilityStatus"[^>]*aria-live="polite"/, "能力设置要报告当前运行状态");
  assert.match(app, /settings\.tool_capabilities = \{/, "保存设置必须提交能力开关状态");
  assert.match(app, /runtime\.active_tool_names/, "页面必须展示服务端报告的当前活跃工具");
  assert.match(app, /control\.dataset\.dirty !== "true"/, "刷新不得覆盖尚未保存的用户选择");
  const css = read("../style.css");
  assert.match(css, /\.chip-group/, "chips 容器样式");
  assert.match(css, /\.chip\.selected/, "chips 选中态样式");
  assert.match(css, /\.auto-group\[hidden\]/, "受控表单隐藏规则");
  assert.match(css, /\.tool-capabilities-grid/, "可选 Agent 工具能力卡片必须有响应式布局");
});

// §5.1.5 模块边界守卫：同一函数不得同时在 app.js 和 app-domain.js 定义，
// 防止把已拆出的实现复制回巨型文件（经典脚本下同名声明会直接冲突或静默覆盖）。
test("模块边界守卫：app.js 与 app-domain.js 函数定义零交集", () => {
  const fnNames = (src) => new Set([...src.matchAll(/^\s*(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/gm)].map((m) => m[1]));
  const inApp = fnNames(read("../app.js"));
  const inDomain = fnNames(read("../app-domain.js"));
  const dup = [...inApp].filter((n) => inDomain.has(n));
  assert.deepEqual(dup, [], "重复定义的函数：" + dup.join(", "));
});

test("§12-12 工具与高级系统移入路由化设置页 + 开发者模式门控", () => {
  const index = read("../index.html");
  // 工具分区包进可路由容器；开发者分区单独标记（默认隐藏）。
  assert.match(index, /<div id="toolsPanel" class="tools-panel">/, "工具与高级系统必须包进可路由容器");
  assert.match(index, /id="devModeToggle"/, "必须提供开发者模式开关");
  const toolsSections = index.match(/<section data-(tools|dev)>/g) || [];
  assert.ok(toolsSections.length >= 10, `工具分区必须带分组标记（当前 ${toolsSections.length} 个）`);
  assert.ok(toolsSections.some((tag) => tag.includes("data-dev")), "MCP/Trace/Eval 等必须标记为开发者分区");
  // 开发者分区（MCP/Trace/Eval）不得出现在普通分组里。
  const devArea = index.match(/<section data-dev>[\s\S]*?<\/section>/g) || [];
  const devText = devArea.join("\n");
  for (const marker of ["MCP 服务器", "Traces（回合轨迹）", "Eval 评估", "扩展面板"]) {
    assert.ok(devText.includes(marker), `开发者分区必须收进数据机构：${marker}`);
  }
  const app = read("../app.js");
  assert.match(app, /function initDeveloperMode\(\)/, "必须提供开发者模式初始化");
  assert.match(app, /function applyDeveloperMode\(enabled\)/, "必须提供开发者模式应用函数");
  assert.match(app, /localStorage\.getItem\("owo\.dev-mode"\)/, "开发者模式必须跨会话持久化");
  assert.match(app, /function renderSettingsTabs\(content\)/, "设置路由必须提供子页导航");
  assert.match(app, /"模型与数据"/, "设置子页：模型与数据");
  assert.match(app, /"工具与自动化"/, "设置子页：工具与自动化");
  assert.match(app, /"开发者选项"/, "设置子页：开发者选项");
  const css = read("../style.css");
  assert.match(css, /section\[data-dev\] \{ display: none; \}/, "普通模式必须隐藏开发者分区");
  assert.match(css, /\.settings-tabs/, "设置子页标签样式必须存在");
  assert.match(css, /body:not\(\.route-chat\)\.settings-tab-tools #toolsPanel/, "工具子页必须只显示普通工具分区");
});

// R10（2026-09-22 用户反馈）："左侧工作区有一堆参数" 与 "找不到模型切换、
// 自定义模型地址和名称"。前者要求工具/开发者表单区不再挂在侧栏常显位置，
// 后者要求模型配置成为一级入口且地址/模型名默认可见。以下断言锁住这两个意图，
// 防止后续重构把入口又藏回去（真机上是"功能存在但用户找不到"＝等效缺失）。
test("R10 侧栏瘦身：工具/开发者分区不再挤在侧栏常显位置", () => {
  const index = read("../index.html");
  // 侧栏里只允许保留工作区/任务/技能/子代理四个常显区块 + 底部设置入口；
  // toolsPanel 内部的重型表单区不在此列（它们默认隐藏，只在设置页/开发者模式出现）。
  const sidebar = index.match(/<aside id="sidebar">[\s\S]*?<div id="toolsPanel"/)[0];
  const sidebarSections = sidebar.match(/<section/g) || [];
  assert.ok(sidebarSections.length <= 4,
    `侧栏常显区块必须收敛到 ≤4（当前 ${sidebarSections.length}）`);
  assert.match(sidebar, /id="openSettingsBtn"/, "侧栏必须有不依赖折叠开关的设置入口");
  assert.ok(!/id="toggleTools"/.test(index), "旧的「显示工具与设置」折叠开关不得再出现");
  // 工具面板仍在（设置页与开发者模式依赖它），但默认不可见。
  assert.match(index, /<div id="toolsPanel" class="tools-panel">/);
  const css = read("../style.css");
  assert.match(css, /#toolsPanel \{ display: none; \}/, "工具面板默认必须隐藏");
  assert.match(css, /\.owo-sidebar-settings/, "侧栏设置入口必须有样式");
});

test("R12 会话卡片：低频操作收进 ⋯ 弹层，卡片上不再堆按钮", () => {
  const domain = read("../app-domain.js");
  // 卡片主体点一下就是"打开会话"，不该再有一个常显的"继续"按钮。
  assert.ok(!/data-act="open"/.test(domain), "会话卡片不得再有常显的「继续」按钮");
  assert.match(domain, /class="owo-session-more"[\s\S]{0,120}data-act="menu"/, "必须有 ⋯ 菜单触发器");
  assert.match(domain, /class="owo-session-menu"[\s\S]*?role="menu"/, "必须有 role=menu 的弹层");
  for (const act of ["rename", "pin", "archive", "fork", "rewind", "redo"]) {
    assert.ok(domain.includes(`data-act="${act}"`), `弹层里必须有 ${act} 操作`);
  }
  // 弹层默认隐藏 + 点空白/Esc 关闭（否则会"粘"在界面上）。
  assert.match(domain, /class="owo-session-menu" role="menu" hidden/, "弹层必须默认 hidden");
  assert.match(domain, /addEventListener\("keydown"[\s\S]{0,200}Escape/, "Esc 必须能关掉弹层");
  const css = read("../style.css");
  // 窄侧栏里中文被压成逐字竖排就是缺这几条：关键元素一律 nowrap + 省略。
  assert.match(css, /\.owo-session-menu button \{[\s\S]*?white-space: nowrap;/, "菜单项不得换行断字");
  assert.match(css, /\.owo-session-title \{[\s\S]*?text-overflow: ellipsis;/, "标题必须省略号截断而非换行");
});

test("R13 模型参数全部文件驱动：地址/模型名/上下文/输出/温度/超时 都不写死", () => {
  const index = read("../index.html");
  const panel = read("../views/settings-panel.view.js");
  // 用户明确要求"模型服务地址、模型名称、上下文等全都通过文件随时更改，不能写死"。
  for (const id of ["settingsBaseUrl", "settingsModelName", "settingsContextWindow",
                    "settingsMaxOutput", "settingsTemperature", "settingsTimeout",
                    "settingsKeepRecent", "settingsCompaction"]) {
    assert.ok(index.includes(`id="${id}"`), `必须提供可编辑字段：${id}`);
  }
  // 模型名必须是自由输入（能写下任意模型），而不是只有固定选项。
  assert.match(index, /id="settingsModelName"[^>]*type="text"/, "模型名必须是文本输入（不得只有固定下拉）");
  assert.match(index, /id="modelReloadBtn"/, "必须有「从文件重载」（手改 config.json 后不必重开应用）");
  // 面板要把可调参数一起提交，并把配置里的模型清单变成建议项。
  assert.match(panel, /context_window:/, "保存时必须提交上下文窗口");
  assert.match(panel, /max_output_tokens:/, "保存时必须提交输出上限");
  assert.match(panel, /temperature:/, "保存时必须提交温度");
  assert.match(panel, /timeout_secs:/, "保存时必须提交超时");
  assert.match(panel, /reload_model_config/, "必须调用壳的重载命令");
  assert.match(panel, /Array\.isArray\(state\.models\)/, "配置里的模型清单必须变成界面建议项");
  // 壳侧：配置文件 schema 必须带这些字段（含用户可维护的模型清单）。
  const rust = read("../../tauri/src-tauri/src/provider.rs");
  for (const field of ["context_window", "max_output_tokens", "keep_recent", "compaction", "pub models: Vec<String>"]) {
    assert.ok(rust.includes(field), `provider.rs 必须支持字段：${field}`);
  }
  // 核心侧：可调参数要能被消费（否则保存了也不生效）。
  const agent = read("../../../crates/owo-agent-core/src/agent.rs");
  for (const envName of ["OWO_MODEL_CONTEXT_WINDOW", "OWO_MODEL_MAX_OUTPUT_TOKENS", "OWO_MODEL_TEMPERATURE", "OWO_AGENT_KEEP_RECENT"]) {
    assert.ok(agent.includes(envName), `核心必须消费 ${envName}`);
  }
});

test("R10 模型配置：一级入口 + 自定义地址/模型名默认可见 + 会话级切换", () => {
  const index = read("../index.html");
  const app = read("../app.js");
  assert.match(index, /data-rail-target="model"/, "左栏必须有一级「模型」入口");
  assert.match(index, /id="settingsBaseUrl"/, "必须提供自定义接口地址（base_url）输入框");
  assert.match(index, /id="settingsModelName"/, "必须提供自定义模型名输入框");
  assert.match(index, /id="modelApplyBtn"/, "必须有显式保存按钮（改完不点不生效）");
  assert.match(index, /id="sessionModelCard"/, "必须提供会话级模型切换卡");
  assert.match(index, /views\/settings-panel\.view\.js/, "模型面板脚本必须挂载");
  assert.match(app, /model: \{ title: "模型"/, "模型必须是一级路由");
  assert.match(app, /route === "model"/, "模型路由必须有独立渲染分支");
  // 状态条「模型」段直达模型页（此前落在设置页，用户找不到）。
  const statusBar = read("../views/status-bar.view.js");
  assert.match(statusBar, /key: "model", label: "模型", target: "model"/, "状态条模型段必须直达模型页");
  // 引导页：地址与模型名默认可见（不再 hidden 到选中云端才出现）。
  const setup = read("../views/setup-guide.view.js");
  assert.ok(!/data-role="base-url"[^>]*hidden/.test(setup),
    "引导页的接口地址不得默认隐藏（这正是用户看不到自定义项的形态）");
  assert.ok(!/data-role="model"[^>]*hidden/.test(setup), "引导页的模型名不得默认隐藏");
  // 预设唯一事实源，且本地端点走动态端口拼接（lint 禁止渲染脚本硬编码本机 URL）。
  const presets = read("../config/provider-presets.js");
  assert.match(presets, /ollamaBaseUrl/, "本地 Ollama 端点必须由函数生成（禁止渲染脚本硬编码）");
  assert.ok(!/apiUrl = "http:\/\/127\.0\.0\.1:11434/.test(presets), "端点必须拼接端口常量而非整串字面量");
});
