import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const source = await readFile(new URL("../panels/goal.panel.js", import.meta.url), "utf8");

test("Goal cloud panel renders named progress events and slow-consumer gap notices", async () => {
  const elements = {
    "owo-goal-cloud-task": { value: "task-42" },
    "owo-goal-cloudlog": { textContent: "", scrollTop: 0, scrollHeight: 0 },
    "owo-goal-list": { innerHTML: "" },
  };
  const callbacks = {};
  let sourceInstance;
  class FakeEventSource {
    constructor(url) { this.url = url; this.closed = false; sourceInstance = this; }
    addEventListener(name, callback) { callbacks[name] = callback; }
    close() { this.closed = true; }
  }
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  const document = { getElementById: id => elements[id] || null };
  const context = { window, document, EventSource: FakeEventSource, Promise, JSON, String, Error, setTimeout, clearTimeout };
  vm.runInNewContext(source, context, { filename: "goal.panel.js" });
  const listeners = {};
  const root = {
    set innerHTML(_) {},
    querySelector(selector) {
      return { addEventListener: (event, callback) => { listeners[selector] = callback; } };
    },
  };
  window.OwoPanels.goal.mount(root, {
    baseUrl: "http://localhost",
    get: async () => ({ goals: [] }),
    post: async () => ({}),
    esc: value => String(value),
    friendlyError: error => String(error),
    renderMarkdown: value => String(value),
  });
  listeners["#owo-goal-cloud-sub"]();
  assert.equal(sourceInstance.url, "http://localhost/cloud/tasks/task-42/events");
  assert.equal(typeof callbacks.progress, "function");

  callbacks.progress({ data: JSON.stringify({ kind: "executing", task_id: "task-42" }) });
  callbacks.progress({ data: JSON.stringify({ kind: "stream_gap", message: "正在自动续传" }) });
  sourceInstance.onerror();
  assert.equal(sourceInstance.closed, false, "transient failures must leave EventSource retry enabled");
  assert.match(elements["owo-goal-cloudlog"].textContent, /executing/);
  assert.match(elements["owo-goal-cloudlog"].textContent, /正在自动续传/);

  callbacks.progress({ data: JSON.stringify({ kind: "succeeded", event: "succeeded" }) });
  assert.equal(sourceInstance.closed, true, "terminal events must stop native reconnect loops");
  assert.match(elements["owo-goal-cloudlog"].textContent, /succeeded/);
});


test("Goal cloud panel closes a cursor replay after the task is already complete", async () => {
  const elements = {
    "owo-goal-cloud-task": { value: "task-done" },
    "owo-goal-cloudlog": { textContent: "", scrollTop: 0, scrollHeight: 0 },
    "owo-goal-list": { innerHTML: "" },
  };
  const callbacks = {};
  let closed = false;
  class FakeEventSource {
    constructor() {}
    addEventListener(name, callback) { callbacks[name] = callback; }
    close() { closed = true; }
  }
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  const document = { getElementById: id => elements[id] || null };
  vm.runInNewContext(source, { window, document, EventSource: FakeEventSource, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const listeners = {};
  const root = {
    set innerHTML(_) {},
    querySelector(selector) { return { addEventListener: (_event, callback) => { listeners[selector] = callback; } }; },
  };
  window.OwoPanels.goal.mount(root, {
    baseUrl: "http://localhost", get: async () => ({ goals: [] }), post: async () => ({}),
    esc: value => String(value), friendlyError: error => String(error), renderMarkdown: value => String(value),
  });
  listeners["#owo-goal-cloud-sub"]();
  callbacks.progress({ data: JSON.stringify({ kind: "stream_complete", message: "任务已结束" }) });
  assert.equal(closed, true);
  assert.match(elements["owo-goal-cloudlog"].textContent, /任务已结束/);
});

test("Goal disposes cloud SSE and ignores callbacks delivered after unmount", async () => {
  const elements = {
    "owo-goal-cloud-task": { value: "task-live" },
    "owo-goal-cloudlog": { textContent: "", scrollTop: 0, scrollHeight: 0 },
    "owo-goal-list": { innerHTML: "" },
  };
  let stream;
  class FakeEventSource {
    constructor() { this.closed = false; stream = this; }
    addEventListener(name, callback) { this[name] = callback; }
    close() { this.closed = true; }
  }
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  const document = { getElementById: id => elements[id] || null };
  vm.runInNewContext(source, { window, document, EventSource: FakeEventSource, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const listeners = {};
  const root = {
    set innerHTML(_) {},
    querySelector(selector) { return { addEventListener: (_event, callback) => { listeners[selector] = callback; } }; },
  };
  const panel = window.OwoPanels.goal;
  panel.mount(root, {
    baseUrl: "http://localhost", get: async () => ({ goals: [] }), post: async () => ({}),
    esc: value => String(value), friendlyError: error => String(error), renderMarkdown: value => String(value),
  });
  listeners["#owo-goal-cloud-sub"]();
  const lateMessage = stream.onmessage;
  const lateProgress = stream.progress;
  panel.dispose();
  assert.equal(stream.closed, true);
  lateMessage({ data: "late message" });
  lateProgress({ data: "late progress" });
  stream.onerror();
  assert.doesNotMatch(elements["owo-goal-cloudlog"].textContent, /late (message|progress)/);
});



test("opening goals out of order keeps the newest selected goal detail", async () => {
  const pendingGoals = new Map();
  const detail = {
    innerHTML: "",
    querySelector() { return { addEventListener() {} }; },
  };
  const status = { innerHTML: "" };
  const elements = {
    "owo-goal-detail": detail,
    "owo-goal-status": status,
    "owo-goal-audit-box": { innerHTML: "" },
  };
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  const document = { getElementById: id => elements[id] || null };
  vm.runInNewContext(source, { window, document, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const panel = window.OwoPanels.goal;
  panel.mount({
    set innerHTML(_) {},
    querySelector() { return { addEventListener() {} }; },
  }, {
    get(path) {
      if (path === "/goal") return Promise.resolve({ goals: [] });
      if (path.endsWith("/status")) return Promise.resolve({ goal_status: "Running", steps: [] });
      if (path.endsWith("/plan")) return Promise.resolve({ plan: { steps: [] }, waves: [] });
      return new Promise(resolve => pendingGoals.set(path, resolve));
    },
    post: async () => ({}),
    esc: value => String(value),
    friendlyError: error => String(error),
    renderMarkdown: value => String(value),
  });

  const older = panel._test.loadGoal("older");
  const newer = panel._test.loadGoal("newer");
  pendingGoals.get("/goal/newer")({ id: "newer", objective: "Newest objective", status: "Running" });
  await newer;
  pendingGoals.get("/goal/older")({ id: "older", objective: "Stale objective", status: "Running" });
  await older;
  assert.match(detail.innerHTML, /Newest objective/);
  assert.doesNotMatch(detail.innerHTML, /Stale objective/);
});
