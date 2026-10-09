import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { createWorkspaceSelectionController, requireWorkspacePersistence, projectCreationFailureMessage } = require("../core/workspace-routing.js");
const deferred = () => {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const flush = () => new Promise(resolve => setTimeout(resolve, 0));

test("workspace updates serialize in user order and only the latest result reaches the UI", async () => {
  const calls = [];
  const applied = [];
  const pending = new Map();
  const controller = createWorkspaceSelectionController(
    path => {
      calls.push(path);
      const task = deferred();
      pending.set(path, task);
      return task.promise;
    },
    path => applied.push(path),
  );

  const first = controller.select("T:/first");
  await flush();
  const second = controller.select("T:/second");
  await flush();
  assert.deepEqual(calls, ["T:/first"]);

  pending.get("T:/first").resolve({ ok: true });
  await flush();
  await flush();
  assert.deepEqual(calls, ["T:/first", "T:/second"]);
  pending.get("T:/second").resolve({ ok: true });

  const results = await Promise.all([first, second]);
  assert.equal(results[0].latest, false);
  assert.equal(results[1].latest, true);
  assert.deepEqual(applied, ["T:/second"]);
});

test("a project creation that finishes after a newer workspace choice stays inactive", async () => {
  const calls = [];
  const applied = [];
  const controller = createWorkspaceSelectionController(
    async path => { calls.push(path); return { ok: true }; },
    path => applied.push(path),
  );
  const createRevision = controller.begin();
  const current = await controller.select("T:/current");
  const staleCreate = await controller.select("T:/created-later", createRevision);

  assert.equal(current.latest, true);
  assert.equal(staleCreate.latest, false);
  assert.deepEqual(calls, ["T:/current"]);
  assert.deepEqual(applied, ["T:/current"]);
});

test("a failed workspace write does not poison the queue", async () => {
  const calls = [];
  const controller = createWorkspaceSelectionController(
    async path => {
      calls.push(path);
      if (path === "T:/broken") throw new Error("offline");
      return { ok: true };
    },
    () => {},
  );
  await assert.rejects(controller.select("T:/broken"), /offline/);
  const next = await controller.select("T:/working");
  assert.equal(next.latest, true);
  assert.deepEqual(calls, ["T:/broken", "T:/working"]);
});

test("project creation failures identify its completed stage", () => {
  assert.match(projectCreationFailureMessage("create", new Error("picker unavailable")), /^新建项目文件夹失败：picker unavailable$/);
  assert.match(projectCreationFailureMessage("activate", new Error("service restart failed")), /项目文件夹已创建，但工作区切换失败/);
  assert.match(projectCreationFailureMessage("activate", new Error("service restart failed")), /最近目录重试/);
  assert.match(projectCreationFailureMessage("session", new Error("model unavailable")), /项目文件夹已创建并切换为当前工作区，但新建会话失败/);
  assert.match(projectCreationFailureMessage("session", new Error("model unavailable")), /新对话.*重试/);
});


test("workspace persistence rejects a missing desktop response before the active path can be applied", async () => {
  const applied = [];
  const controller = createWorkspaceSelectionController(
    async () => requireWorkspacePersistence(null, "Electron bridge unavailable"),
    path => applied.push(path),
  );
  await assert.rejects(controller.select("T:/preview-only"), /Electron bridge unavailable/);
  assert.deepEqual(applied, []);
});

test("workspace persistence rejects an explicit host failure and accepts its success envelope", () => {
  assert.throws(() => requireWorkspacePersistence({ ok: false, error: "access denied" }), /access denied/);
  assert.throws(() => requireWorkspacePersistence({}), /not confirmed/);
  const result = { ok: true, path: "T:/project" };
  assert.equal(requireWorkspacePersistence(result), result);
});
