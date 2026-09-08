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
  assert.match(app, /syncOpenApiLink\(\);\s*const readiness = new window\.OwoServiceReadiness/, "boot 时必须同步一次");
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
