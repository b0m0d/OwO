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



test("Goal create is single-flight across remount and ignores an old success response", async () => {
  const elements = new Map();
  const makeElement = (value = "") => ({
    value,
    innerHTML: "",
    textContent: "",
    disabled: false,
    attributes: {},
    setAttribute(name, value) { this.attributes[name] = value; },
    removeAttribute(name) { delete this.attributes[name]; },
    addEventListener() {},
  });
  let root = { set innerHTML(_) {}, querySelector() { return makeElement(); } };
  let goalReads = 0;
  let postCalls = 0;
  let resolveCreate;
  const document = {
    getElementById(id) { return elements.get(id) || null; },
    querySelector() { return null; },
  };
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  vm.runInNewContext(source, { window, document, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const panel = window.OwoPanels.goal;
  const helpers = {
    get(path) {
      if (path === "/goal") { goalReads += 1; return Promise.resolve({ goals: [] }); }
      return Promise.reject(new Error("unexpected path: " + path));
    },
    post(path) {
      assert.equal(path, "/goal");
      postCalls += 1;
      return new Promise(resolve => { resolveCreate = resolve; });
    },
    esc: value => String(value),
    friendlyError: error => String(error),
    notify() {},
  };
  function mount(objectiveValue) {
    const input = makeElement(objectiveValue);
    const button = makeElement();
    elements.set("owo-goal-objective", input);
    elements.set("owo-goal-create", button);
    root = {
      set innerHTML(_) {},
      querySelector(selector) {
        const id = selector.replace(/^#/, "");
        return elements.get(id) || makeElement();
      },
    };
    panel.mount(root, helpers);
    return { input, button };
  }

  const oldView = mount("first objective");
  const first = panel._test.createGoal();
  const duplicate = await panel._test.createGoal();
  assert.equal(duplicate, null);
  assert.equal(postCalls, 1);
  assert.equal(oldView.button.disabled, true);

  panel.dispose();
  const currentView = mount("keep this text");
  assert.equal(currentView.button.disabled, true, "new mount reflects the in-flight create lock");
  resolveCreate({ goal: { id: "late-goal" } });
  await first;
  await Promise.resolve();
  assert.equal(goalReads, 2, "late response must not refresh the newly mounted page");
  assert.equal(currentView.input.value, "keep this text", "late response must not clear current input");
  assert.equal(currentView.button.disabled, false);
  assert.equal(currentView.button.attributes["aria-busy"], undefined);
});

test("saving an old goal plan cannot replace a newer selection after its request returns", async () => {
  const elements = new Map();
  const makeElement = (value = "") => ({
    value,
    innerHTML: "",
    textContent: "",
    disabled: false,
    attributes: {},
    addEventListener() {},
    setAttribute(name, value) { this.attributes[name] = value; },
    removeAttribute(name) { delete this.attributes[name]; },
  });
  const detail = makeElement();
  const preview = makeElement();
  const goalReads = new Map();
  let resolveSave;
  const document = {
    getElementById(id) { return elements.get(id) || null; },
    querySelector() { return preview; },
  };
  detail.querySelector = selector => elements.get(selector.replace(/^#/, "")) || makeElement();
  elements.set("owo-goal-detail", detail);
  elements.set("owo-goal-status", makeElement());
  elements.set("owo-goal-audit-box", makeElement());
  elements.set("owo-goal-steps", makeElement('[{"id":"step","worker":"echo"}]'));
  elements.set("owo-goal-save-plan", makeElement());
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  vm.runInNewContext(source, { window, document, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const panel = window.OwoPanels.goal;
  const helpers = {
    get(path) {
      if (path === "/goal") return Promise.resolve({ goals: [] });
      if (path.endsWith("/status")) return Promise.resolve({ goal_status: "Running", steps: [] });
      if (path.endsWith("/plan")) return Promise.resolve({ plan: { steps: [] }, waves: [] });
      const id = path.split("/")[2];
      goalReads.set(id, (goalReads.get(id) || 0) + 1);
      return Promise.resolve({ id, objective: id === "first" ? "First objective" : "Second objective", status: "Running" });
    },
    post(path) {
      if (path === "/goal/first/plan") return new Promise(resolve => { resolveSave = resolve; });
      return Promise.reject(new Error("unexpected path: " + path));
    },
    esc: value => String(value),
    friendlyError: error => String(error),
    notify() {},
  };
  const root = {
    set innerHTML(_) {},
    querySelector() { return makeElement(); },
  };
  panel.mount(root, helpers);
  await panel._test.loadGoal("first");
  const saving = panel._test.savePlan("first");
  assert.equal(elements.get("owo-goal-save-plan").disabled, true);
  const duplicate = await panel._test.savePlan("first");
  assert.equal(duplicate, null);
  await panel._test.loadGoal("second");
  assert.match(detail.innerHTML, /Second objective/);

  resolveSave({ waves: [["step"]] });
  await saving;
  await Promise.resolve();
  assert.equal(goalReads.get("first"), 1, "stale success must not reload the old goal");
  assert.equal(goalReads.get("second"), 1);
  assert.match(detail.innerHTML, /Second objective/);
  assert.doesNotMatch(detail.innerHTML, /First objective/);
  assert.equal(elements.get("owo-goal-save-plan").disabled, false);
});

test("Goal run and abort actions are single-flight per goal and release their buttons", async () => {
  const elements = new Map();
  const makeElement = () => ({
    innerHTML: "",
    textContent: "",
    disabled: false,
    attributes: {},
    addEventListener() {},
    querySelectorAll() { return []; },
    setAttribute(name, value) { this.attributes[name] = value; },
    removeAttribute(name) { delete this.attributes[name]; },
  });
  const document = {
    getElementById(id) { return elements.get(id) || null; },
    querySelector() { return null; },
  };
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  vm.runInNewContext(source, { window, document, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const panel = window.OwoPanels.goal;
  let resolveAction;
  const posts = [];
  const helpers = {
    get(path) {
      assert.equal(path, "/goal");
      return Promise.resolve({ goals: [] });
    },
    post(path, body) {
      posts.push({ path, body });
      return new Promise(resolve => { resolveAction = resolve; });
    },
    esc: value => String(value),
    friendlyError: error => String(error),
    notify() {},
  };
  const root = {
    set innerHTML(_) {},
    querySelector(selector) {
      const id = selector.replace(/^#/, "");
      if (!elements.has(id)) elements.set(id, makeElement());
      return elements.get(id);
    },
  };
  panel.mount(root, helpers);
  const button = makeElement();

  const running = panel._test.runGoal("goal-1", button, "运行");
  assert.equal(button.disabled, true);
  assert.equal(button.attributes["aria-busy"], "true");
  assert.equal(await panel._test.abortGoal("goal-1", makeElement(), "中止"), null,
    "run and abort cannot race for the same goal");
  await Promise.resolve();
  assert.equal(posts.length, 1);
  assert.equal(posts[0].path, "/goal/goal-1/run");
  resolveAction({ status: "Running" });
  await running;
  assert.equal(button.disabled, false);
  assert.equal(button.attributes["aria-busy"], undefined);

  const abortButton = makeElement();
  const aborting = panel._test.abortGoal("goal-1", abortButton, "中止");
  assert.equal(await panel._test.abortGoal("goal-1", makeElement(), "中止"), null);
  await Promise.resolve();
  assert.equal(posts.length, 2);
  assert.equal(posts[1].path, "/goal/goal-1/abort");
  resolveAction({ status: "Aborted" });
  await aborting;
  assert.equal(abortButton.disabled, false);
  assert.equal(abortButton.attributes["aria-busy"], undefined);
  panel.dispose();
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


test("Goal wave previews escape model or server supplied step ids", async () => {
  const window = { location: { origin: "http://localhost" }, OwoPanels: {} };
  const document = { getElementById: () => null };
  vm.runInNewContext(source, { window, document, Promise, JSON, String, Error, setTimeout, clearTimeout });
  const root = {
    set innerHTML(_) {},
    querySelector() { return { addEventListener() {} }; },
  };
  const escapeHtml = value => String(value == null ? "" : value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
  const panel = window.OwoPanels.goal;
  panel.mount(root, {
    get: async () => ({ goals: [] }),
    post: async () => ({}),
    esc: escapeHtml,
    friendlyError: error => String(error),
    renderMarkdown: value => String(value),
  });

  const html = panel._test.renderWaves([["<img src=x onerror=alert(1)>", "req&2"]]);
  assert.equal(html, "wave1: &lt;img src=x onerror=alert(1)&gt;, req&amp;2");
  assert.doesNotMatch(html, /<img/);
  assert.equal(panel._test.renderWaves([]), "");
  panel.dispose();
});
