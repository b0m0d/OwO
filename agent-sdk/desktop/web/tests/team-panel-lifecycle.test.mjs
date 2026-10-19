import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../panels/team.panel.js", import.meta.url), "utf8");

function harness() {
  const window = { OwoPanels: {}, location: { origin: "http://localhost" } };
  const document = { querySelector: () => null };
  vm.runInNewContext(source, { window, document, Promise, String, encodeURIComponent }, { filename: "team.panel.js" });
  const element = () => ({
    value: "",
    innerHTML: "",
    textContent: "",
    listeners: {},
    addEventListener(type, callback) { this.listeners[type] = callback; },
  });
  const root = () => {
    const nodes = new Map();
    return {
      nodes,
      innerHTML: "",
      querySelector(selector) {
        if (!nodes.has(selector)) nodes.set(selector, element());
        return nodes.get(selector);
      },
    };
  };
  return { panel: window.OwoPanels.team, root };
}

const flush = () => new Promise(resolve => setTimeout(resolve, 0));

test("late team review response cannot overwrite the newly mounted panel", async () => {
  const { panel, root } = harness();
  const oldRoot = root();
  const newRoot = root();
  let finishOldReview;
  let finishNewReview;
  const oldHelpers = {
    get: async () => ({ entries: [] }),
    post: () => new Promise(resolve => { finishOldReview = resolve; }),
    esc: value => String(value == null ? "" : value),
    friendlyError: error => String(error),
  };
  const newHelpers = {
    get: async () => ({ entries: [] }),
    post: () => new Promise(resolve => { finishNewReview = resolve; }),
    esc: value => String(value == null ? "" : value),
    friendlyError: error => String(error),
  };

  panel.mount(oldRoot, oldHelpers);
  panel.doReview("old-package");
  panel.dispose();
  panel.mount(newRoot, newHelpers);
  panel.doReview("new-package");

  finishOldReview({ package: { id: "stale-package" }, findings: [] });
  await flush();
  assert.equal(newRoot.querySelector(".owo-team-findings").innerHTML, "");

  finishNewReview({ package: { id: "current-package" }, findings: [] });
  await flush();
  assert.match(newRoot.querySelector(".owo-team-findings").innerHTML, /current-package/);
  assert.doesNotMatch(newRoot.querySelector(".owo-team-findings").innerHTML, /stale-package/);
});

test("team import ignores duplicate clicks and unlocks the active mount after completion", async () => {
  const { panel, root } = harness();
  const firstRoot = root();
  const currentRoot = root();
  let finishImport;
  let importRequests = 0;
  const helpers = {
    get: async () => ({ entries: [] }),
    post: path => {
      if (path !== "/team/import") throw new Error("unexpected write: " + path);
      importRequests += 1;
      return new Promise(resolve => { finishImport = resolve; });
    },
    esc: value => String(value == null ? "" : value),
    friendlyError: error => String(error),
  };
  panel.mount(firstRoot, helpers);
  panel.doImport("encoded-package");
  panel.doImport("encoded-package");
  const firstButton = firstRoot.querySelector(".owo-team-importbtn");
  assert.equal(importRequests, 1);
  assert.equal(firstButton.disabled, true);
  assert.equal(firstButton.textContent, "正在导入…");

  panel.dispose();
  panel.mount(currentRoot, helpers);
  const button = currentRoot.querySelector(".owo-team-importbtn");
  assert.equal(button.disabled, true);

  finishImport({ blocked: false, package: { id: "team-package", version: "1" }, versions: [] });
  await flush();
  assert.equal(button.disabled, false);
  assert.equal(button.textContent, "导入（评审通过才落盘）");
  assert.match(currentRoot.querySelector(".owo-team-findings").textContent, /导入已在后台完成/);
});

test("late team review result cannot replace a newer review in the same mount", async () => {
  const { panel, root } = harness();
  const currentRoot = root();
  const finish = new Map();
  panel.mount(currentRoot, {
    get: async () => ({ entries: [] }),
    post: (_path, body) => new Promise(resolve => { finish.set(body.package_b64, resolve); }),
    esc: value => String(value == null ? "" : value),
    friendlyError: error => String(error),
  });
  panel.doReview("older-review");
  panel.doReview("newer-review");
  finish.get("newer-review")({ package: { id: "newer-result" }, findings: [] });
  await flush();
  finish.get("older-review")({ package: { id: "older-result" }, findings: [] });
  await flush();
  const findings = currentRoot.querySelector(".owo-team-findings").innerHTML;
  assert.match(findings, /newer-result/);
  assert.doesNotMatch(findings, /older-result/);
});
