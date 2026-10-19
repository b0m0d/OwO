import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const app = readFileSync(new URL("../app.js", import.meta.url), "utf8");
// Markdown/LaTeX 渲染器唯一来源：core/markdown.js（§12.3 单文件单职责拆分后）。
const markdownSource = readFileSync(new URL("../core/markdown.js", import.meta.url), "utf8");
const sandbox = { window: { location: { href: "http://localhost/" } }, URL };
vm.createContext(sandbox);
vm.runInContext(markdownSource, sandbox);
const renderMarkdown = sandbox.window.OwoMarkdown && sandbox.window.OwoMarkdown.renderMarkdown;
assert.equal(typeof renderMarkdown, "function", "core/markdown.js 必须导出 renderMarkdown");

test("ordinary prose containing pipes is not misclassified as a table", () => {
  const html = renderMarkdown("条件 a | b | c 仍是普通正文");
  assert.doesNotMatch(html, /<table/);
  assert.match(html, /<div class="md-p">条件 a \| b \| c 仍是普通正文<\/div>/);
});

test("Markdown tables preserve empty cells, escaped/code pipes, and column alignment", () => {
  const html = renderMarkdown([
    "| 项目 | 说明 | 数值 |",
    "|:---|:---:|---:|",
    "| A | 含 \\| 分隔符 |  |",
    "| B | `x|y` | 42 |",
  ].join("\n"));
  assert.match(html, /<th style="text-align:left">项目<\/th>/);
  assert.match(html, /<th style="text-align:center">说明<\/th>/);
  assert.match(html, /<th style="text-align:right">数值<\/th>/);
  assert.match(html, /<td style="text-align:center">含 \| 分隔符<\/td>/);
  assert.match(html, /<td style="text-align:right"><\/td>/);
  assert.match(html, /<code>x\|y<\/code>/);
});

test("fenced code accepts language metadata and matching longer tilde fences", () => {
  const code = "const ok = true;\n```\nreturn ok;";
  const html = renderMarkdown("~~~~typescript title=sample.ts\n" + code + "\n~~~~");
  assert.match(html, /<span>typescript<\/span>/);
  assert.match(html, new RegExp("data-code=\"" + encodeURIComponent(code) + "\""));
  assert.match(html, /return ok;/);
});

test("unsafe links stay unlinked and raw HTML remains escaped", () => {
  const html = renderMarkdown('[unsafe](javascript:alert(1)) <img src=x onerror=alert(1)>');
  assert.doesNotMatch(html, /href="javascript:/i);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
});


function copyButtonHarness({ clipboardReject = false, fallbackSucceeds = true } = {}) {
  const timers = [];
  const attributes = {};
  let copied = null;
  const area = { value: "", style: {}, setAttribute() {}, select() {} };
  const button = {
    dataset: { code: encodeURIComponent("const answer = 42;\n") },
    disabled: false,
    textContent: "复制",
    title: "",
    setAttribute(name, value) { attributes[name] = value; },
    addEventListener(_name, handler) { this.handler = handler; },
  };
  const sandbox = {
    Promise,
    navigator: { clipboard: { writeText: async value => {
      copied = value;
      if (clipboardReject) throw new Error("permission denied");
    } } },
    document: {
      createElement() { return area; },
      body: { appendChild() {}, removeChild() {} },
      execCommand(command) {
        assert.equal(command, "copy");
        copied = area.value;
        return fallbackSucceeds;
      },
    },
    setTimeout(callback) { timers.push(callback); return timers.length; },
  };
  vm.createContext(sandbox);
  const copySource = /function copyText\(text\) \{[\s\S]*?\n\}/.exec(app)?.[0];
  const bindSource = /function bindCopyButtons\(root\) \{[\s\S]*?\n\}/.exec(app)?.[0];
  assert.ok(copySource && bindSource);
  const bind = vm.runInContext(copySource + "\n" + bindSource + "\nbindCopyButtons", sandbox);
  bind({ querySelectorAll: () => [button] });
  return { button, attributes, timers, copied: () => copied, click: () => button.handler() };
}

test("code-copy falls back when clipboard permission is denied and announces success", async () => {
  const h = copyButtonHarness({ clipboardReject: true });
  await h.click();
  assert.equal(h.copied(), "const answer = 42;\n");
  assert.equal(h.button.textContent, "已复制");
  assert.equal(h.attributes["aria-label"], "代码已复制");
  h.timers[0]();
  assert.equal(h.button.disabled, false);
  assert.equal(h.button.textContent, "复制");
});

test("code-copy reports failure without leaking a rejected clipboard promise", async () => {
  const h = copyButtonHarness({ clipboardReject: true, fallbackSucceeds: false });
  await h.click();
  assert.equal(h.button.textContent, "复制失败");
  assert.equal(h.attributes["aria-label"], "复制失败，请重试");
  h.timers[0]();
});
