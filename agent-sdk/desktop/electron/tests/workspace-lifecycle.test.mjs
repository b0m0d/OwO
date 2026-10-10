import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../src/main/main.js", import.meta.url), "utf8");
function workspaceSurface() {
  let saved;
  let starts = 0;
  const context = {
    path: { resolve: value => value },
    fs: { realpathSync: value => { if (value === "missing") throw Error("missing"); return value; }, statSync: value => ({ isDirectory: () => value !== "file" }) },
    saveWorkspace: value => { saved = value; },
    startCore: async () => { starts++; },
    coreState: { state: "ready", port: 4096 }, generation: 7, restartAttempts: 2,
  };
  vm.createContext(context);
  const start = source.indexOf("async function setWorkspaceTarget(");
  const end = source.indexOf("const SHELL_COMMAND_HANDLERS", start);
  vm.runInContext(source.slice(start, end), context);
  return { context, saved: () => saved, starts: () => starts };
}

test("changing the selected workspace preserves the shared daemon and running turn connection", async () => {
  const fixture = workspaceSurface();
  const result = await fixture.context.setWorkspaceTarget("project-b");
  assert.equal(result.ok, true);
  assert.equal(result.workspace, "project-b");
  assert.equal(fixture.saved(), "project-b");
  assert.equal(fixture.starts(), 0);
  assert.equal(result.generation, 7);
  assert.equal(fixture.context.coreState.port, 4096);
  assert.equal(fixture.context.restartAttempts, 2);
});

test("unavailable workspace does not persist a selection or restart the daemon", async () => {
  for (const path of ["missing", "file"]) {
    const fixture = workspaceSurface();
    const result = await fixture.context.setWorkspaceTarget(path);
    assert.equal(result.ok, false);
    assert.equal(fixture.saved(), undefined);
    assert.equal(fixture.starts(), 0);
  }
});

test("native workspace picker delegates to the same session workspace selection", async () => {
  let handler;
  let selected;
  const context = {
    ipcMain: { handle: (_, callback) => { handler = callback; } }, mainWindow: null,
    dialog: { showOpenDialog: async () => ({ canceled: false, filePaths: ["project-b"] }) },
    setWorkspaceTarget: async value => { selected = value; return { ok: true, workspace: value }; },
  };
  vm.createContext(context);
  const start = source.indexOf('ipcMain.handle("workspace:choose"');
  const end = source.indexOf('ipcMain.handle("app:openExternal"', start);
  vm.runInContext(source.slice(start, end), context);
  const result = await handler();
  assert.equal(result.workspace, "project-b");
  assert.equal(selected, "project-b");
});
