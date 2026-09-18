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
  const css = read("../style.css");
  assert.match(css, /\.chip-group/, "chips 容器样式");
  assert.match(css, /\.chip\.selected/, "chips 选中态样式");
  assert.match(css, /\.auto-group\[hidden\]/, "受控表单隐藏规则");
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
