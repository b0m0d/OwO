import test from "node:test";
import { runInNewContext } from "node:vm";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const app = readFileSync(join(here, "..", "app.js"), "utf8");
const domain = readFileSync(join(here, "..", "app-domain.js"), "utf8");
const keySource = /function workspacePathKey\(workspace\) \{[\s\S]*?\n\}/.exec(domain)?.[0];
const adoptSource = /function adoptSessionWorkspace\(workspace\) \{[\s\S]*?\n\}/.exec(domain)?.[0];
const restoreSource = /async function restoreLastSession\(\) \{[\s\S]*?\n\}/.exec(domain)?.[0];
const titleSource = /function owoSessionHasAutoTitle\(session\) \{[\s\S]*?\n\}/.exec(domain)?.[0];

test("opening a session adopts its workspace into the current-workspace chip", () => {
  assert.ok(keySource && adoptSource);
  const input = { value: "D:\\OwO", dataset: {} };
  const stored = [];
  let persisted = null;
  let chipSyncs = 0;
  let headerSyncs = 0;
  const context = {
    $: (id) => (id === "workspace" ? input : null),
    localStorage: { setItem: (key, value) => stored.push([key, value]) },
    rememberWorkspace: () => {},
    syncProjectChip: () => { chipSyncs += 1; },
    syncTaskDetails: () => { headerSyncs += 1; },
    persistWorkspace: (target) => { persisted = target; return Promise.resolve(); },
    showToast: () => {},
    Promise,
  };
  const adopt = runInNewContext(
    keySource + "\n" + adoptSource + "\nadoptSessionWorkspace;",
    context
  );
  assert.equal(adopt("D:\\牛来"), true);
  assert.equal(input.value, "D:\\牛来");
  assert.equal(input.dataset.path, "D:\\牛来");
  assert.deepEqual(stored, [["owo.workspace", "D:\\牛来"]]);
  assert.equal(persisted, "D:\\牛来");
  assert.equal(chipSyncs, 1);
  assert.equal(headerSyncs, 1);
  assert.equal(adopt("D:\\牛来\\"), false);
  assert.equal(chipSyncs, 1);
  assert.equal(persisted, "D:\\牛来");
});

test("startup restore stays inside the current workspace", async () => {
  assert.ok(keySource && restoreSource);
  const sessions = [
    { id: "niu", workspace: "D:\\牛来", archived: false, title: "牛来任务" },
    { id: "owo", workspace: "D:\\OwO", archived: false, title: "OwO 任务" },
  ];
  let selected = null;
  const context = {
    state: { sessionId: null },
    api: async () => sessions,
    $: (id) => (id === "workspace" ? { value: "D:\\OwO" } : null),
    localStorage: { getItem: () => "niu", removeItem: () => {} },
    selectSession: async (id) => { selected = id; },
    Promise,
  };
  const restore = runInNewContext(
    keySource + "\n" + titleSource + "\n" + restoreSource + "\nrestoreLastSession;",
    context
  );
  await restore();
  assert.equal(selected, "owo");
});

test("startup restore keeps the home view when the workspace has no sessions", async () => {
  assert.ok(keySource && restoreSource);
  let selected = null;
  const context = {
    state: { sessionId: null },
    api: async () => [{ id: "niu", workspace: "D:\\牛来", archived: false, title: "牛来任务" }],
    $: (id) => (id === "workspace" ? { value: "D:\\OwO" } : null),
    localStorage: { getItem: () => "niu", removeItem: () => {} },
    selectSession: async (id) => { selected = id; },
    Promise,
  };
  const restore = runInNewContext(
    keySource + "\n" + titleSource + "\n" + restoreSource + "\nrestoreLastSession;",
    context
  );
  await restore();
  assert.equal(selected, null);
});

test("startup restore can adopt the remembered session when no workspace is set", async () => {
  assert.ok(keySource && restoreSource);
  let selected = null;
  const context = {
    state: { sessionId: null },
    api: async () => [{ id: "niu", workspace: "D:\\牛来", archived: false, title: "牛来任务" }],
    $: (id) => (id === "workspace" ? { value: "" } : null),
    localStorage: { getItem: () => "niu", removeItem: () => {} },
    selectSession: async (id) => { selected = id; },
    Promise,
  };
  const restore = runInNewContext(
    keySource + "\n" + titleSource + "\n" + restoreSource + "\nrestoreLastSession;",
    context
  );
  await restore();
  assert.equal(selected, "niu");
});

test("startup restore skips empty sessions with placeholder titles", async () => {
  assert.ok(keySource && titleSource && restoreSource);
  const sessions = [
    { id: "empty", workspace: "D:\\OwO", archived: false, title: "会话 084d869b" },
    { id: "real", workspace: "D:\\OwO", archived: false, title: "分析项目结构" },
  ];
  let selected = null;
  const context = {
    state: { sessionId: null },
    api: async () => sessions,
    $: (id) => (id === "workspace" ? { value: "D:\\OwO" } : null),
    localStorage: { getItem: () => "empty", removeItem: () => {} },
    selectSession: async (id) => { selected = id; },
    Promise,
  };
  const restore = runInNewContext(
    keySource + "\n" + titleSource + "\n" + restoreSource + "\nrestoreLastSession;",
    context
  );
  await restore();
  assert.equal(selected, "real");
});

test("selection and workspace switches keep the chip and header on one workspace", () => {
  assert.match(domain, /adoptSessionWorkspace\(knownSession\.workspace\)/);
  assert.match(domain, /if \(detail\?\.workspace\) adoptSessionWorkspace\(detail\.workspace\)/);
  assert.match(domain, /const restorable = \(session\) =>[\s\S]*?inCurrentWorkspace\(session\) && !owoSessionHasAutoTitle\(session\)/);
  assert.match(domain, /sessions\.find\(\(session\) => session\.id === last && restorable\(session\)\)/);
  assert.match(app, /const active = \(state\.sessionIndex \|\| \[\]\)\.find\(\(session\) => session\.id === state\.sessionId\)/);
  assert.match(app, /workspacePathKey\(activeWorkspace\) !== workspacePathKey\(path\)/);
  assert.match(app, /workspacePathKey\(activeWorkspace\) !== workspacePathKey\(path\)[\s\S]{0,300}showHomeView\(\)/);
  assert.match(app, /void Promise\.resolve\(refreshSessions\(state\.sessionId\)\)/);
});
