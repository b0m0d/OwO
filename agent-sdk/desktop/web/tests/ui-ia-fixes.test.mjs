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
  assert.match(app, /renderOwoSetupGuide\(content, global\.__owoCoreDiagnostics \|\| null, \(\) => recover\(\)\)/, "引导完成后落回 recover");
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
  const app = read("../app.js");
  assert.match(app, /function describeApproval\(payload\)/, "审批卡必须提供可解释摘要");
  assert.match(app, /将写入：/, "写文件显示目标路径");
  assert.match(app, /将执行命令：/, "命令显示可执行程序与参数");
  assert.match(app, /将联网访问：/, "联网显示域名");
  assert.match(app, /能否撤销/, "审批卡回答能否撤销");
  assert.match(app, /approvalRawJson/, "原始 JSON 收进开发者详情");
  assert.match(app, /alwaysAllowBtn/, "破坏性操作隐藏始终允许按钮");
  assert.match(app, /body\.scope = scope/, "响应携带 scope 选项");
  const index = read("../index.html");
  assert.match(index, /id="approvalExplain"/, "审批条含说明区");
  assert.match(index, /id="approvalRaw"/, "审批条含开发者详情折叠");
  assert.match(index, /data-scope="session"/, "审批卡提供「此会话」选项");
  assert.match(index, /data-scope="one_hour"/, "审批卡提供「此项目一小时」");
  assert.match(index, /data-scope="always_readonly"/, "审批卡提供「始终允许此只读动作」");
  const css = read("../style.css");
  assert.match(css, /\.approval-actions/, "审批选项样式存在");
});
