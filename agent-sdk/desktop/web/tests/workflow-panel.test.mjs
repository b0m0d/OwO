import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../panels/workflow.panel.js", import.meta.url), "utf8");

function harness(get, post) {
  const nodes = new Map();
  function element() {
    return {
      children: [],
      handlers: {},
      style: {},
      innerHTML: "",
      textContent: "",
      className: "",
      value: "",
      disabled: false,
      attributes: {},
      setAttribute(name, value) { this.attributes[name] = value; },
      removeAttribute(name) { delete this.attributes[name]; },
      appendChild(child) { this.children.push(child); return child; },
      addEventListener(name, handler) { this.handlers[name] = handler; },
      querySelector() { return null; },
    };
  }
  const document = {
    getElementById(id) {
      if (!nodes.has(id)) nodes.set(id, element());
      return nodes.get(id);
    },
    createElement() { return element(); },
  };
  const timers = [];
  const toasts = [];
  class FakeEventSource {
    constructor(url) { this.url = url; this.closed = false; }
    close() { this.closed = true; }
  }
  const window = { OwoPanels: {}, location: { origin: "http://localhost" }, showToast: (text) => toasts.push(text) };
  runInNewContext(source, {
    window,
    document,
    EventSource: FakeEventSource,
    Promise,
    Set,
    Math,
    JSON,
    String,
    Date,
    encodeURIComponent,
    setTimeout(fn, delay) {
      const timer = { fn, delay };
      timers.push(timer);
      return timer;
    },
    clearTimeout(timer) {
      const index = timers.indexOf(timer);
      if (index >= 0) timers.splice(index, 1);
    },
  });
  const panel = window.OwoPanels.workflow;
  function mount(root = element()) {
    root.querySelector = (selector) => document.getElementById(selector.replace(/^#/, ""));
    panel.mount(root, { get, post, baseUrl: "http://localhost" });
    return root;
  }
  mount();
  return { panel, nodes, timers, window, toasts, mount };
}

test("workflow polling retries transient failures with backoff and keeps status visible", async () => {
  let runReads = 0;
  const h = harness((path) => {
    if (path === "/workflow") return Promise.resolve({ flows: [] });
    if (path === "/workflow/run/r1") {
      runReads += 1;
      if (runReads === 1) return Promise.reject(new Error("temporary network error"));
      return Promise.resolve({ run_id: "r1", state: "succeeded", steps: [] });
    }
    throw new Error("unexpected path: " + path);
  });

  await h.panel._test.pollRun("r1", 0);
  const runner = h.nodes.get("owo-workflow-runner");
  assert.equal(h.timers.length, 1);
  assert.equal(h.timers[0].delay, 1000);
  assert.match(runner.children[0].textContent, /状态同步暂时失败/);

  const retry = h.timers.shift();
  await retry.fn();
  assert.match(runner.innerHTML, /succeeded/);
  assert.equal(h.timers.length, 0);
});

test("workflow results arriving after panel disposal cannot repaint or restart polling", async () => {
  let resolveRun;
  const h = harness((path) => {
    if (path === "/workflow") return Promise.resolve({ flows: [] });
    if (path === "/workflow/run/r2") {
      return new Promise((resolve) => { resolveRun = resolve; });
    }
    throw new Error("unexpected path: " + path);
  });

  const pending = h.panel._test.pollRun("r2", 0);
  h.panel.dispose();
  resolveRun({ run_id: "r2", state: "running", steps: [] });
  await pending;
  assert.equal(h.nodes.has("owo-workflow-runner"), false);
  assert.equal(h.timers.length, 0);
});


test("workflow audit failures render inline without throwing from the error handler", async () => {
  const h = harness((path) => {
    if (path === "/workflow") return Promise.resolve({ flows: [] });
    if (path === "/workflow/run/r-audit/audit") return Promise.reject(new Error("audit service unavailable"));
    throw new Error("unexpected path: " + path);
  });

  await assert.doesNotReject(h.panel._test.loadAudit("r-audit"));
  assert.match(h.nodes.get("owo-workflow-audit").innerHTML, /audit service unavailable/);
});

test("late workflow definition responses cannot replace the most recently selected flow", async () => {
  const pending = new Map();
  const h = harness((path) => {
    if (path === "/workflow") return Promise.resolve({ flows: [] });
    if (path.startsWith("/workflow/")) {
      return new Promise((resolve) => pending.set(path, resolve));
    }
    throw new Error("unexpected path: " + path);
  });

  const oldRequest = h.panel._test.loadFlow("old-flow");
  const newRequest = h.panel._test.loadFlow("new-flow");
  pending.get("/workflow/new-flow")({ valid: true, definition: { steps: [] }, issues: [] });
  await newRequest;
  pending.get("/workflow/old-flow")({ valid: true, definition: { steps: [] }, issues: [] });
  await oldRequest;
  assert.match(h.nodes.get("owo-workflow-runner").innerHTML, /new-flow/);
  assert.doesNotMatch(h.nodes.get("owo-workflow-runner").innerHTML, /old-flow/);
});


test("workflow start suppresses duplicate submissions while the first request is pending", async () => {
  let resolveStart;
  let startCalls = 0;
  const h = harness(
    (path) => {
      if (path === "/workflow") return Promise.resolve({ flows: [] });
      if (path === "/workflow/run/r-start") return Promise.resolve({ run_id: "r-start", state: "succeeded", steps: [] });
      if (path === "/workflow/sample/runs") return Promise.resolve({ runs: [] });
      throw new Error("unexpected path: " + path);
    },
    () => {
      startCalls += 1;
      return new Promise((resolve) => { resolveStart = resolve; });
    },
  );

  const first = h.panel._test.runFlow("sample", "");
  const duplicate = await h.panel._test.runFlow("sample", "");
  assert.equal(duplicate, null);
  assert.equal(startCalls, 1);
  assert.match(h.toasts.join(" "), /避免重复/);
  resolveStart({ run_id: "r-start" });
  await first;
  assert.equal(startCalls, 1);
});

test("a workflow start response arriving after panel disposal does not repaint a new panel or subscribe stale events", async () => {
  let resolveStart;
  let runReads = 0;
  const h = harness(
    (path) => {
      if (path === "/workflow") return Promise.resolve({ flows: [] });
      if (path === "/workflow/run/r-late") {
        runReads += 1;
        return Promise.resolve({ run_id: "r-late", state: "succeeded", steps: [] });
      }
      if (path === "/workflow/sample/runs") return Promise.resolve({ runs: [] });
      throw new Error("unexpected path: " + path);
    },
    () => new Promise((resolve) => { resolveStart = resolve; }),
  );

  const oldRunner = { innerHTML: "选择左侧流程查看定义并运行" };
  h.nodes.set("owo-workflow-runner", oldRunner);
  const pending = h.panel._test.runFlow("sample", "");
  h.panel.dispose();
  h.nodes.delete("owo-workflow-runner");
  resolveStart({ run_id: "r-late" });
  await pending;
  assert.equal(oldRunner.innerHTML, "选择左侧流程查看定义并运行");
  assert.equal(runReads, 0);
  assert.equal(h.window.OwoWorkflowEventSource, undefined);
  assert.equal(h.toasts.length, 0);
});

test("workflow DSL reports JSON syntax locally while core actions are disabled", () => {
  let postCalls = 0;
  const h = harness(
    path => path === "/workflow" ? Promise.resolve({ flows: [] }) : Promise.reject(new Error("unexpected path: " + path)),
    () => { postCalls += 1; return Promise.resolve({ valid: true }); },
  );
  const input = h.nodes.get("owo-workflow-validate-dsl");
  const result = h.nodes.get("owo-workflow-validate-result");
  const button = h.nodes.get("owo-workflow-validate-btn");
  button.disabled = true;

  input.value = "{ invalid";
  input.handlers.input();
  assert.match(result.innerHTML, /JSON 解析失败/);
  assert.equal(postCalls, 0);

  input.value = JSON.stringify({ steps: [] });
  input.handlers.input();
  assert.match(result.innerHTML, /JSON 语法有效/);
  assert.match(result.innerHTML, /连接核心后/);
  assert.equal(postCalls, 0);
  assert.equal(button.disabled, true);
});

test("workflow validation locks duplicate submits and ignores results from an old mount", async () => {
  let resolveValidation;
  let validateCalls = 0;
  const get = (path) => path === "/workflow" ? Promise.resolve({ flows: [] }) : Promise.reject(new Error("unexpected path: " + path));
  const post = (path) => {
    if (path !== "/workflow/validate") return Promise.reject(new Error("unexpected path: " + path));
    validateCalls += 1;
    return new Promise((resolve) => { resolveValidation = resolve; });
  };
  const h = harness(get, post);
  const oldInput = h.nodes.get("owo-workflow-validate-dsl");
  const oldResult = h.nodes.get("owo-workflow-validate-result");
  const oldButton = h.nodes.get("owo-workflow-validate-btn");
  oldInput.value = "{}";
  const pending = oldButton.handlers.click();
  assert.equal(oldButton.disabled, true);
  assert.equal(oldButton.attributes["aria-busy"], "true");
  assert.equal(await oldButton.handlers.click(), null);
  assert.equal(validateCalls, 1);

  h.panel.dispose();
  h.nodes.clear();
  h.mount();
  const currentResult = h.nodes.get("owo-workflow-validate-result");
  resolveValidation({ valid: true });
  await pending;
  assert.equal(currentResult.innerHTML, "");
  assert.equal(oldResult.innerHTML, "");
  const currentButton = h.nodes.get("owo-workflow-validate-btn");
  assert.equal(currentButton.disabled, false);
  h.nodes.get("owo-workflow-validate-dsl").value = "{}";

  const currentValidation = currentButton.handlers.click();
  assert.equal(currentButton.disabled, true);
  resolveValidation({ valid: true });
  await currentValidation;
  assert.match(currentResult.innerHTML, /valid/);
  assert.equal(currentButton.disabled, false);
  assert.equal(currentButton.attributes["aria-busy"], undefined);
  assert.equal(validateCalls, 2);
});

test("workflow approval is single-flight per run and locks approve/reject together", async () => {
  let resolveApproval;
  const posts = [];
  const h = harness(
    path => {
      if (path === "/workflow") return Promise.resolve({ flows: [] });
      if (path === "/workflow/run/r-approval") return Promise.resolve({ run_id: "r-approval", state: "succeeded", steps: [] });
      throw new Error("unexpected path: " + path);
    },
    (path, body) => {
      posts.push({ path, body });
      return new Promise(resolve => { resolveApproval = resolve; });
    },
  );
  h.panel._test.renderSnapshot({
    run_id: "r-approval", state: "waiting_approval", steps: [],
    pending_approval: { id: "approval-1", prompt: "Continue?" },
  });
  const approve = h.nodes.get("owo-workflow-approve");
  const reject = h.nodes.get("owo-workflow-reject");
  approve.setAttribute("data-run-id", "r-approval");
  reject.setAttribute("data-run-id", "r-approval");

  const first = h.panel._test.decideApproval("r-approval", "approve");
  assert.equal(approve.disabled, true);
  assert.equal(reject.disabled, true);
  assert.equal(approve.attributes["aria-busy"], "true");
  assert.equal(await h.panel._test.decideApproval("r-approval", "reject"), null);
  assert.equal(posts.length, 1, "opposite choices must not race after one decision starts");
  assert.equal(posts[0].path, "/workflow/run/r-approval/approval");
  assert.equal(posts[0].body.decision, "approve");

  resolveApproval({ accepted: true });
  await first;
  await Promise.resolve();
  assert.equal(approve.disabled, false);
  assert.equal(reject.disabled, false);
  assert.equal(approve.attributes["aria-busy"], undefined);
});

test("workflow abort is single-flight per run and terminal snapshots do not offer abort", async () => {
  let resolveAbort;
  const posts = [];
  const h = harness(
    path => {
      if (path === "/workflow") return Promise.resolve({ flows: [] });
      if (path === "/workflow/run/r-abort") return Promise.resolve({ run_id: "r-abort", state: "succeeded", steps: [] });
      throw new Error("unexpected path: " + path);
    },
    path => {
      posts.push(path);
      return new Promise(resolve => { resolveAbort = resolve; });
    },
  );
  h.panel._test.renderSnapshot({ run_id: "r-abort", state: "running", steps: [] });
  const abort = h.nodes.get("owo-workflow-abort");
  abort.setAttribute("data-run-id", "r-abort");
  const first = h.panel._test.startRunAction("r-abort", "abort", {});
  assert.equal(abort.disabled, true);
  assert.equal(abort.textContent, "正在中止…");
  assert.equal(await h.panel._test.startRunAction("r-abort", "abort", {}), null);
  assert.equal(posts.length, 1);
  resolveAbort({ accepted: true });
  await first;
  assert.equal(posts[0], "/workflow/run/r-abort/abort");

  h.panel._test.renderSnapshot({ run_id: "r-abort", state: "succeeded", steps: [] });
  assert.match(h.nodes.get("owo-workflow-runner").innerHTML, /运行已结束/);
  const before = posts.length;
  await h.panel._test.startRunAction("r-abort", "abort", {});
  await h.panel._test.startRunAction("r-abort", "approve", { decision: "approve" });
  assert.equal(posts.length, before, "terminal runs must reject abort and approval actions");
});

test("workflow list labels the direct mock run as sandbox behavior", () => {
  assert.match(source, /运行（沙箱）/);
  assert.match(source, /title="直接以 mock 沙箱运行，不执行真实桌面动作"/);
});


test("workflow DSL validation explains its core dependency and is gated while disconnected", () => {
  assert.match(source, /id="owo-workflow-validate-btn"[^>]*data-core-action/);
  assert.match(source, /JSON 语法会在本地检查；流程结构校验需要连接并授权本地核心/);
});
