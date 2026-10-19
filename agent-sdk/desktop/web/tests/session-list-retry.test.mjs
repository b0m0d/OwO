import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const domain = readFileSync(new URL("../app-domain.js", import.meta.url), "utf8");
const retryBody = /async function refreshSessions\(selectId\) \{[\s\S]*?\n\}/.exec(domain)?.[0];
const failureMessageBody = /function sessionListFailureMessage\(error\) \{[\s\S]*?\n\}/.exec(domain)?.[0];
const errorBody = /function renderSessionListFailure\(error, selectId\) \{[\s\S]*?\n\}/.exec(domain)?.[0];
assert.ok(retryBody && errorBody && failureMessageBody, "session-list error and retry boundary must be present");

function button() {
  return {
    attributes: {}, listeners: {}, disabled: false, textContent: "",
    setAttribute(name, value) { this.attributes[name] = value; },
    addEventListener(name, callback) { this.listeners[name] = callback; },
  };
}
function harness() {
  const list = { children: [], replaceChildren(...children) { this.children = children; } };
  const document = {
    createElement(tag) {
      const element = tag === "button" ? button() : {
        tag, children: [], attributes: {}, className: "", textContent: "",
        setAttribute(name, value) { this.attributes[name] = value; },
        addEventListener() {}, append(...children) { this.children.push(...children); },
      };
      return element;
    },
  };
  let attempts = 0;
  const sandbox = {
    document,
    state: { selectionVersion: 0 },
    sessionListRefreshGeneration: 0,
    $(id) { return id === "sessionList" ? list : null; },
    refreshSessionsImpl: async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("token 403 <check>");
      list.replaceChildren({ textContent: "loaded sessions" });
    },
  };
  vm.createContext(sandbox);
  const failureMessage = vm.runInContext(failureMessageBody + "\nsessionListFailureMessage", sandbox);
  const refresh = vm.runInContext(errorBody + "\n" + retryBody + "\nrefreshSessions", sandbox);
  return { list, refresh, failureMessage, attempts: () => attempts };
}

test("会话读取失败可从侧栏重试，错误安全显示，成功后替换错误行", async () => {
  const h = harness();
  await h.refresh();
  assert.equal(h.attempts(), 1);
  assert.equal(h.list.children.length, 1);
  const row = h.list.children[0];
  assert.equal(row.attributes.role, "alert");
  assert.equal(row.children[0].textContent, "会话读取失败：本地授权暂不可用。请在 Electron 工作台重新连接后重试。");
  assert.equal(row.children[1].tag, "details");
  assert.equal(row.children[1].children[0].textContent, "技术详情");
  assert.equal(row.children[1].children[1].textContent, "token 403 <check>", "技术信息作为文本呈现，不解释为 HTML");
  const retry = row.children[2];
  assert.equal(retry.textContent, "重试");
  assert.equal(retry.attributes["aria-label"], "重新加载会话列表");

  await Promise.all([retry.listeners.click(), retry.listeners.click()]);
  assert.equal(h.attempts(), 2, "重复点击应由禁用态合并为一次重试");
  assert.equal(h.list.children[0].textContent, "loaded sessions");
});


test("会话错误按状态和桌面桥接来源给出可操作提示，原错误可单独展开", () => {
  const h = harness();
  const browserFailure = vm.runInNewContext(failureMessageBody + "\nsessionListFailureMessage", { window: {} });
  const desktopFailure = vm.runInNewContext(failureMessageBody + "\nsessionListFailureMessage", { window: { owo: {} } });
  assert.match(browserFailure(Object.assign(new Error("HTTP 404"), { status: 404 })), /浏览器预览/);
  assert.match(desktopFailure(Object.assign(new Error("HTTP 404"), { status: 404 })), /重启 Electron 工作台/);
  assert.match(h.failureMessage(Object.assign(new Error("token 引导失败（HTTP 403）"), { status: 403 })), /重新连接后重试/);
  assert.match(h.failureMessage(new Error("Failed to fetch")), /无法连接本地核心服务/);
  assert.match(h.failureMessage(new Error("此浏览器未获得桌面授权，请在 Electron 工作台中打开会话。")), /此浏览器未获得桌面授权/);
});
