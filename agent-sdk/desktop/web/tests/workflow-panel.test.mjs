import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../panels/workflow.panel.js", import.meta.url), "utf8");

function harness(get) {
  const nodes = new Map();
  function element() {
    return {
      children: [],
      handlers: {},
      style: {},
      innerHTML: "",
      textContent: "",
      className: "",
      setAttribute(name, value) { this[name] = value; },
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
  const window = { OwoPanels: {}, location: { origin: "http://localhost" } };
  runInNewContext(source, {
    window,
    document,
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
  panel.mount(element(), { get, baseUrl: "http://localhost" });
  return { panel, nodes, timers };
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
