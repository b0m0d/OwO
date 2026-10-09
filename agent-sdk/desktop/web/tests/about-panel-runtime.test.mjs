import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../panels/about.panel.js", import.meta.url), "utf8");
function loadPanel() {
  const sandbox = { window: { OwoPanels: {} }, Promise, Date, navigator: { userAgent: "test" }, document: {} };
  vm.createContext(sandbox);
  vm.runInContext(source, sandbox, { filename: "about.panel.js" });
  return sandbox.window.OwoPanels.about;
}
function makeRoot() {
  const elements = new Map([
    ["owo-about-health", { textContent: "加载中…", innerHTML: "", addEventListener() {} }],
    ["owo-about-caps", { textContent: "", innerHTML: "正在读取能力状态…", addEventListener() {} }],
    ["owo-about-diag", { textContent: "正在读取诊断…", innerHTML: "", addEventListener() {} }],
  ]);
  const root = {
    innerHTML: "",
    querySelector(selector) {
      const key = selector.replace(/^#/, "");
      if (!elements.has(key)) elements.set(key, { textContent: "", innerHTML: "", addEventListener() {} });
      return elements.get(key);
    },
    element(id) { return this.querySelector("#" + id); },
  };
  return root;
}
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function responseFor(path, label) {
  if (path === "/health") return { version: label, healthy: true, auto_approve: false };
  if (path === "/server/status") return { workspace: label, model: "test-model", read_only: false };
  if (path === "/skills" || path === "/plugins") return [{ name: "one" }];
  if (path === "/mcp") return { connected: [], count: 0, servers: [] };
  if (path === "/automations") return { count: 0 };
  if (path === "/notes") return { notes: [] };
  if (path === "/sessions") return { sessions: [] };
  if (path === "/plugins/market") return { count: 0 };
  return { provider: label, ready: true };
}
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

test("离开帮助页后的旧响应不会覆盖重新打开页面的数据", async () => {
  const panel = loadPanel();
  const makeCalls = () => [];
  const firstCalls = makeCalls();
  const secondCalls = makeCalls();
  const helpersFor = (calls) => ({ baseUrl: "", get(path) { const request = deferred(); calls.push({ path, ...request }); return request.promise; } });
  const oldRoot = makeRoot();
  const newRoot = makeRoot();

  panel.mount(oldRoot, helpersFor(firstCalls));
  panel.dispose();
  panel.mount(newRoot, helpersFor(secondCalls));
  assert.equal(firstCalls.some((item) => item.path === "/usage/summary"), false, "不应发出未展示的用量请求");

  for (const request of firstCalls) request.resolve(responseFor(request.path, "旧页面"));
  await flush();
  assert.equal(newRoot.element("owo-about-health").textContent, "加载中…");
  assert.equal(newRoot.element("owo-about-caps").innerHTML, "正在读取能力状态…");

  for (const request of secondCalls) request.resolve(responseFor(request.path, "新页面"));
  await flush();
  assert.match(newRoot.element("owo-about-health").textContent, /新页面/);
  assert.doesNotMatch(newRoot.element("owo-about-health").textContent, /旧页面/);
});

test("能力请求失败显示不可用和重试提示，不伪装成零项", async () => {
  const panel = loadPanel();
  const root = makeRoot();
  const helpers = {
    baseUrl: "",
    get(path) {
      if (path === "/skills") return Promise.reject(new Error("offline"));
      return Promise.resolve(responseFor(path, "online"));
    },
  };
  panel.mount(root, helpers);
  await flush();
  const cards = root.element("owo-about-caps").innerHTML;
  assert.match(cards, /技能/);
  assert.match(cards, /不可用/);
  assert.match(cards, /读取失败；点击上方刷新重试/);
  assert.match(cards, /MCP 服务器/);
  assert.match(cards, /<b>0<\/b>/, "成功读取到的空列表仍然显示真实的 0");
});


test("MCP 服务器名称作为卡片文本转义，不能注入 HTML", async () => {
  const panel = loadPanel();
  const root = makeRoot();
  const helpers = {
    baseUrl: "",
    esc(value) {
      return String(value == null ? "" : value).replace(/[&<>"']/g, (char) => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      })[char]);
    },
    get(path) {
      if (path === "/mcp") {
        return Promise.resolve({ connected: ['<img src=x onerror=alert(1)>'], count: 1, servers: [] });
      }
      return Promise.resolve(responseFor(path, "online"));
    },
  };
  panel.mount(root, helpers);
  await flush();

  const cards = root.element("owo-about-caps").innerHTML;
  assert.match(cards, /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.doesNotMatch(cards, /<img src=x onerror=/);
});

test("未知响应形状显示未知而不是空列表", async () => {
  const panel = loadPanel();
  const root = makeRoot();
  const helpers = {
    baseUrl: "",
    get(path) {
      if (["/skills", "/plugins", "/mcp", "/automations", "/notes", "/sessions"].includes(path)) {
        return Promise.resolve({ unexpected: true });
      }
      return Promise.resolve(responseFor(path, "online"));
    },
  };
  panel.mount(root, helpers);
  await flush();
  const cards = root.element("owo-about-caps").innerHTML;
  assert.match(cards, /<b>未知<\/b>/);
  assert.match(cards, /服务未返回可识别的数量/);
  assert.match(cards, /服务未返回可识别的连接状态/);
});
