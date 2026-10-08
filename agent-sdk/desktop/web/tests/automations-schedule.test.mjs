import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../panels/automations.panel.js", import.meta.url), "utf8");
const stylesheet = readFileSync(new URL("../style.css", import.meta.url), "utf8");
const window = { OwoPanels: {} };
runInNewContext(source, { window, Date, Number, String, Math, JSON, Promise, document: {} });
const buildSchedule = window.OwoPanels.automations._test.buildSchedule;

test("interval accepts only positive safe integers", () => {
  assert.deepEqual(JSON.parse(JSON.stringify(buildSchedule("interval", "60"))), {
    kind: "interval",
    every_secs: 60,
  });
  for (const value of ["0", "-1", "1.5", "12abc", "9007199254740992", ""]) {
    assert.throws(() => buildSchedule("interval", value), /正整数/);
  }
});

test("daily schedule requires valid 24-hour HH:MM", () => {
  assert.deepEqual(JSON.parse(JSON.stringify(buildSchedule("daily", "09:05"))), {
    kind: "daily",
    time: "09:05",
  });
  for (const value of ["24:00", "9:05", "12:60", "noon"]) {
    assert.throws(() => buildSchedule("daily", value), /HH:MM/);
  }
});

test("one-shot local time becomes RFC3339 with explicit local offset", () => {
  assert.deepEqual(JSON.parse(JSON.stringify(buildSchedule("oneshot", "2030-06-15T12:30", -480))), {
    kind: "one_shot",
    at: "2030-06-15T12:30:00+08:00",
  });
  assert.throws(() => buildSchedule("oneshot", "2030-02-30T12:30", 0), /有效/);
  assert.throws(() => buildSchedule("oneshot", "2030-06-15 12:30", 0), /有效/);
});


test("automation actions preserve reminder and read-only Agent task intent", () => {
  const buildAction = window.OwoPanels.automations._test.buildAction;
  assert.deepEqual(JSON.parse(JSON.stringify(buildAction("reminder", " review status "))), {
    kind: "reminder",
    text: "review status",
  });
  assert.deepEqual(JSON.parse(JSON.stringify(buildAction("run_prompt", " summarize changes "))), {
    kind: "run_prompt",
    prompt: "summarize changes",
  });
  assert.throws(() => buildAction("run_prompt", "  "), /不能为空/);
  assert.throws(() => buildAction("unknown", "x"), /未知/);
});


test("automation form exposes the supported read-only Agent action", () => {
  assert.match(source, /<option value="run_prompt">跑只读 Agent 任务<\/option>/);
  assert.match(source, /定时任务无人值守，只读模式/);
});

function makeAutomationHarness(overrides = {}) {
  const nodes = new Map();
  function element(tag = "div") {
    return {
      tagName: tag.toUpperCase(),
      children: [],
      handlers: {},
      style: {},
      value: "",
      textContent: "",
      _innerHTML: "",
      get innerHTML() { return this._innerHTML; },
      set innerHTML(value) { this._innerHTML = String(value); this.children = []; },
      disabled: false,
      addEventListener(name, handler) { this.handlers[name] = handler; },
      appendChild(child) { child.parentElement = this; this.children.push(child); return child; },
      setAttribute(name, value) { this[name] = value; },
      removeAttribute(name) { delete this[name]; },
      parentElement: null,
      querySelector(selector) {
        return this.children.find((child) => selector === ".automation-runs" && child.className === "list automation-runs") || null;
      },
      remove() {
        if (this.parentElement) this.parentElement.children = this.parentElement.children.filter((child) => child !== this);
      },
      focus() {},
    };
  }
  const document = {
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, element());
      return nodes.get(id);
    },
    querySelector() { return element(); },
    createElement(tag) { return element(tag); },
  };
  const isolatedWindow = { OwoPanels: {} };
  runInNewContext(source, { window: isolatedWindow, document, Date, Number, String, Math, JSON, Promise });
  const panel = isolatedWindow.OwoPanels.automations;
  const calls = [];
  const notifications = [];
  const helpers = {
    get(path) {
      calls.push(["GET", path]);
      if (overrides.get) return overrides.get(path);
      if (path === "/automations/reminders") return Promise.resolve([]);
      return Promise.resolve([{ id: "daily/1", name: "每日检查", enabled: true, schedule: {}, action: {} }]);
    },
    post(path, body) { calls.push(["POST", path, body]); return overrides.post ? overrides.post(path, body) : Promise.resolve({}); },
    call(path, options) { calls.push(["CALL", path, options]); return overrides.call ? overrides.call(path, options) : Promise.resolve({}); },
    del(path) { calls.push(["DELETE", path]); return overrides.del ? overrides.del(path) : Promise.resolve(null); },
    confirm(options) { calls.push(["CONFIRM", options]); return overrides.confirm ? overrides.confirm(options) : Promise.resolve(true); },
    notify(message, kind) { notifications.push([message, kind]); },
    friendlyError(error) { return error.message || String(error); },
    esc(value) { return String(value); },
  };
  panel.mount(element(), helpers);
  return new Promise((resolve) => setTimeout(() => {
    const list = document.getElementById("owo-aut-list");
    const actions = list.children[0] && list.children[0].children[0];
    resolve({
      calls,
      notifications,
      panel,
      taskList: list,
      reminderList: document.getElementById("owo-aut-reminders"),
      toggleButton: actions && actions.children[0] || element("button"),
      runsButton: actions && actions.children[1] || element("button"),
      deleteButton: actions && actions.children[2] || element("button"),
      status: document.getElementById("owo-aut-status"),
      nodes,
      submitButton: element("button"),
      form: document.getElementById("owo-aut-form"),
      remount() { panel.mount(element(), helpers); },
    });
  }, 0));
}

test("automation toggle failures stay visible and use an encoded task id", async () => {
  const h = await makeAutomationHarness({
    call: async () => { throw new Error("offline"); },
  });
  await h.toggleButton.handlers.click({ stopPropagation() {} });
  assert.ok(h.calls.some(([method, path]) => method === "CALL" && path === "/automations/daily%2F1/toggle"));
  assert.match(h.status.textContent, /启停失败：offline/);
  assert.equal(h.notifications.length, 1);
  assert.equal(h.toggleButton.disabled, false);
});

test("automation deletion requires confirmation and reports request failures", async () => {
  let confirmed = false;
  const cancelled = await makeAutomationHarness({
    confirm: async (options) => { confirmed = true; assert.equal(options.confirmText, "删除任务"); return false; },
  });
  await cancelled.deleteButton.handlers.click({ stopPropagation() {} });
  assert.equal(confirmed, true);
  assert.equal(cancelled.calls.some(([method]) => method === "DELETE"), false);
  assert.equal(cancelled.deleteButton.disabled, false);

  const failed = await makeAutomationHarness({
    del: async () => { throw new Error("offline"); },
  });
  await failed.deleteButton.handlers.click({ stopPropagation() {} });
  assert.ok(failed.calls.some(([method, path]) => method === "DELETE" && path === "/automations/daily%2F1"));
  assert.match(failed.status.textContent, /删除自动化失败：offline/);
  assert.equal(failed.notifications.length, 1);
  assert.equal(failed.deleteButton.disabled, false);
});



test("native hidden controls stay hidden despite component display styles", () => {
  assert.match(stylesheet, /\.hidden,\s*\[hidden\]\s*\{\s*display:\s*none\s*!important/);
  assert.match(source, /data-schedule-group="daily" hidden/);
  assert.match(source, /data-schedule-group="oneshot" hidden/);
});

test("automation and reminder lists show inline errors and recover through retry", async () => {
  const attempts = { tasks: 0, reminders: 0 };
  const h = await makeAutomationHarness({
    get(path) {
      if (path === "/automations") {
        attempts.tasks += 1;
        return attempts.tasks === 1
          ? Promise.reject(new Error("service unavailable"))
          : Promise.resolve([{ id: "task-2", name: "恢复后的任务", enabled: true, schedule: {}, action: {} }]);
      }
      attempts.reminders += 1;
      return attempts.reminders === 1
        ? Promise.reject(new Error("reminders unavailable"))
        : Promise.resolve(["恢复后的提醒"]);
    },
  });
  assert.match(h.taskList.innerHTML, /自动化任务加载失败：service unavailable/);
  assert.match(h.taskList.innerHTML, /data-aut-retry="tasks"/);
  assert.match(h.reminderList.innerHTML, /提醒加载失败：reminders unavailable/);
  assert.match(h.reminderList.innerHTML, /data-aut-retry="reminders"/);

  function retry(list, kind) {
    list.handlers.click({
      target: { closest: (selector) => selector === '[data-aut-retry="' + kind + '"]' ? {} : null },
      preventDefault() {},
    });
  }
  retry(h.taskList, "tasks");
  retry(h.reminderList, "reminders");
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(attempts.tasks, 2);
  assert.equal(attempts.reminders, 2);
  assert.match(h.taskList.children[0].innerHTML, /恢复后的任务/);
  assert.equal(h.reminderList.children[0].textContent, "⏰ 恢复后的提醒");
});


test("rapid automation form submissions create only one task and restore the submit control", async () => {
  let resolvePost;
  const h = await makeAutomationHarness({
    post: () => new Promise((resolve) => { resolvePost = resolve; }),
  });
  function node(id) {
    if (!h.nodes.has(id)) h.nodes.set(id, { value: "", focus() {} });
    return h.nodes.get(id);
  }
  node("owo-aut-name").value = "Every hour";
  node("owo-aut-kind").value = "interval";
  node("owo-aut-interval").value = "3600";
  node("owo-aut-action").value = "reminder";
  node("owo-aut-content").value = "Review updates";
  const event = { preventDefault() {}, submitter: h.submitButton };

  const first = h.panel ? h.panel._test.createTask(event) : undefined;
  const second = h.panel ? h.panel._test.createTask(event) : undefined;
  await second;
  assert.equal(h.calls.filter(([method, path]) => method === "POST" && path === "/automations").length, 1);
  assert.equal(h.submitButton.disabled, true);
  resolvePost({});
  await first;
  assert.equal(h.submitButton.disabled, false);
});

test("late automation list responses cannot repaint after a newer refresh or disposal", async () => {
  const pending = [];
  const h = await makeAutomationHarness({
    get(path) {
      if (path === "/automations/reminders") return Promise.resolve([]);
      return new Promise((resolve) => pending.push(resolve));
    },
  });
  const first = h.panel.refresh();
  const second = h.panel.refresh();
  pending[2]([{ id: "new", name: "Newest result", enabled: true, schedule: {}, action: {} }]);
  await new Promise((resolve) => setTimeout(resolve, 0));
  pending[0]([{ id: "old", name: "Stale result", enabled: true, schedule: {}, action: {} }]);
  await Promise.all([first, second]);
  pending[0]([{ id: "stale", name: "Disposed result", enabled: true, schedule: {}, action: {} }]);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.match(h.taskList.children[0].innerHTML, /Newest result/);
  assert.doesNotMatch(h.taskList.children[0].innerHTML, /Stale result/);
  const afterDispose = h.panel.refresh();
  h.panel.dispose();
  pending[3]([{ id: "disposed", name: "Disposed result", enabled: true, schedule: {}, action: {} }]);
  await afterDispose;
  assert.equal(h.taskList.children.length, 0);
  assert.equal(h.taskList.innerHTML, '<li class="sub" role="status">正在加载自动化任务…</li>');
  assert.doesNotMatch(h.taskList.innerHTML, /Disposed result/);
});

test("automatic toggle stays locked across panel remounts until the original request settles", async () => {
  let resolveToggle;
  const h = await makeAutomationHarness({
    call(path) {
      if (path.endsWith("/toggle")) return new Promise((resolve) => { resolveToggle = resolve; });
      return Promise.resolve({});
    },
  });
  const oldToggle = h.toggleButton;
  const pending = oldToggle.handlers.click({ stopPropagation() {} });
  assert.equal(oldToggle.disabled, true);

  h.panel.dispose();
  h.remount();
  await new Promise((resolve) => setTimeout(resolve, 0));
  const currentToggle = h.taskList.children[0].children[0].children[0];
  assert.equal(currentToggle.disabled, true, "重新打开页面时同一任务仍显示处理中");
  assert.equal(currentToggle.textContent, "处理中…");
  await currentToggle.handlers.click({ stopPropagation() {} });
  assert.equal(h.calls.filter(([method, path]) => method === "CALL" && path.endsWith("/toggle")).length, 1,
    "旧请求完成前再次点击不能发出第二次反向切换");

  resolveToggle({});
  await pending;
  await new Promise((resolve) => setTimeout(resolve, 0));
  const refreshedToggle = h.taskList.children[0].children[0].children[0];
  assert.equal(refreshedToggle.disabled, false, "原请求结束后新页面重新读取状态并解锁");
});


test("failed automation run history offers retry and ignores a response after reopening", async () => {
  let runAttempts = 0;
  let resolveStale;
  const h = await makeAutomationHarness({
    get(path) {
      if (path.indexOf("/automations/runs?") === 0) {
        runAttempts += 1;
        if (runAttempts === 1) return Promise.reject(new Error("temporary offline"));
        if (runAttempts === 2) return new Promise((resolve) => { resolveStale = resolve; });
        return Promise.resolve([{ at: "2030-06-15T12:30:00Z", status: "ok", output: "fresh record" }]);
      }
      if (path === "/automations/reminders") return Promise.resolve([]);
      return Promise.resolve([{ id: "task/1", name: "Daily", enabled: true, schedule: {}, action: {} }]);
    },
  });
  const click = () => h.runsButton.handlers.click({ stopPropagation() {} });
  await click();
  await new Promise((resolve) => setTimeout(resolve, 0));
  let runBox = h.taskList.children[0].children[1];
  assert.equal(runBox.children[0].role, "alert");
  const retry = runBox.children[0].children[1];
  assert.equal(retry["aria-label"], "重新加载执行记录");
  retry.handlers.click();
  await click(); // close the pending list; its response must become stale
  await click(); // reopen and load a fresh result
  await new Promise((resolve) => setTimeout(resolve, 0));
  resolveStale([{ at: "2030-06-15T11:00:00Z", status: "failed", output: "stale record" }]);
  await new Promise((resolve) => setTimeout(resolve, 0));
  runBox = h.taskList.children[0].children[1];
  assert.equal(runAttempts, 3);
  assert.match(runBox.children[0].innerHTML, /fresh record/);
  assert.doesNotMatch(runBox.children[0].innerHTML, /stale record/);
});
