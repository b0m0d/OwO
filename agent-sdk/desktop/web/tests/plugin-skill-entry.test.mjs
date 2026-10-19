import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const app = readFileSync(new URL("../app.js", import.meta.url), "utf8");
const domain = readFileSync(new URL("../app-domain.js", import.meta.url), "utf8");

test("opening the tools overview or plugin group loads extension data on demand", () => {
  assert.match(app, /async function apiWithTimeout\(path, timeoutMs = 15000\)[\s\S]*?controller\.abort\(\)[\s\S]*?clearTimeout\(timer\)/);
  assert.match(app, /function refreshPluginSkillOverview\(\)\s*\{[\s\S]*?refreshPlugins\(\),[\s\S]*?refreshPluginMarket\(\),[\s\S]*?refreshSkills\(\),[\s\S]*?refreshSkillHealth\(\)/);
  assert.match(app, /function openToolsView\(\)\s*\{[\s\S]*?setToolsVisible\(true\);\s*void refreshPluginSkillOverview\(\);/);
  assert.match(app, /if \(group === "system"\) void refreshPluginSkillOverview\(\);/);
  assert.match(app, /if \(target === "system" \|\| target === "all"\) void refreshPluginSkillOverview\(\);/);
});

test("extension failures replace loading counts and explain the existing retry controls", () => {
  assert.match(app, /if \(count\) count\.textContent = "加载失败";[\s\S]*?加载插件失败：[\s\S]*?请点击上方“刷新”重试/);
  assert.match(app, /加载插件市场失败：[\s\S]*?请点击“刷新目录”重试/);
  assert.match(domain, /if \(count\) count\.textContent = "加载失败";[\s\S]*?加载技能失败：[\s\S]*?请点击上方“刷新”重试/);
  assert.match(domain, /加载技能健康度失败：[\s\S]*?请点击“刷新”重试/);
});


test("主工作台插件推荐显示最低支持版本字段，不把描述误当版本", async () => {
  const refresh = /async function refreshPluginMarket\(\)\s*\{[\s\S]*?\n\}/.exec(app)?.[0];
  assert.ok(refresh, "应能定位插件市场渲染函数");
  const rows = [];
  const box = { innerHTML: "", appendChild(row) { rows.push(row); } };
  const sandbox = {
    Promise,
    $: id => id === "pluginPopular" ? box : null,
    apiWithTimeout: async () => ({
      plugins: [{ source: "market", id: "sample", name: "Sample", version: "1.2.0", min_app_version: "1.5.0", description: "人类可读的说明" }],
    }),
    esc: value => String(value),
    document: {
      createElement: tag => ({
        tagName: tag,
        className: "",
        innerHTML: "",
        textContent: "",
        handlers: {},
        children: [],
        appendChild(child) { this.children.push(child); return child; },
        addEventListener(name, callback) { this.handlers[name] = callback; },
      }),
    },
    confirmModal() {},
  };
  vm.createContext(sandbox);
  const pending = vm.runInContext(refresh + "\nrefreshPluginMarket()", sandbox);
  await pending;
  assert.equal(rows.length, 1);
  assert.match(rows[0].innerHTML, /最低支持 App 1\.5\.0/);
  assert.doesNotMatch(rows[0].innerHTML, /最低支持 App 人类可读的说明/);
});

test("扩展请求在认证引导一直未返回时仍会超时并中止底层请求", async () => {
  const helper = /async function apiWithTimeout\(path, timeoutMs = 15000\) \{[\s\S]*?\n\}/.exec(app)?.[0];
  assert.ok(helper);
  let receivedSignal;
  const sandbox = {
    AbortController,
    Promise,
    setTimeout,
    clearTimeout,
    api(_path, options) {
      receivedSignal = options.signal;
      // 模拟卡在 fetch 之前的凭证引导：不响应 abort，UI 层仍应按时收敛。
      return new Promise(() => {});
    },
  };
  vm.createContext(sandbox);
  const pending = vm.runInContext(helper + '\napiWithTimeout("/plugins", 10)', sandbox);
  await assert.rejects(pending, /请求超时（1 秒）/);
  assert.equal(receivedSignal.aborted, true);
});
