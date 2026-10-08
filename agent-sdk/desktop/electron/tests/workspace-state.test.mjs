import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const { configuredWorkspacePath } = require("../src/main/workspace-state.js");

test("workspace is configured only when workspace.json contains an explicit path", () => {
  const root = mkdtempSync(join(tmpdir(), "owo-workspace-state-"));
  try {
    assert.equal(configuredWorkspacePath(root), "");
    writeFileSync(join(root, "workspace.json"), "{", "utf8");
    assert.equal(configuredWorkspacePath(root), "");
    writeFileSync(join(root, "workspace.json"), JSON.stringify({ path: "T:\\Projects\\Demo" }), "utf8");
    assert.equal(configuredWorkspacePath(root), "T:\\Projects\\Demo");
    writeFileSync(join(root, "workspace.json"), JSON.stringify({ path: "   " }), "utf8");
    assert.equal(configuredWorkspacePath(root), "");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
