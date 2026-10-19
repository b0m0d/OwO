import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import observerScope from "../core/observer-scope.js";

test("panel observer scope disconnects observers when a feature panel is replaced", () => {
  const instances = [];
  class FakeResizeObserver {
    constructor(callback) { this.callback = callback; this.disconnected = false; instances.push(this); }
    observe(target) { this.target = target; }
    disconnect() { this.disconnected = true; }
  }
  const scope = observerScope.createObserverScope(FakeResizeObserver);
  scope.observe({ id: "panel-a" }, () => {});
  scope.observe({ id: "panel-a-content" }, () => {});
  assert.equal(scope.size, 2);
  scope.clear();
  assert.equal(scope.size, 0);
  assert.deepEqual(instances.map((item) => item.disconnected), [true, true]);
  scope.clear();
  assert.equal(instances.every((item) => item.disconnected), true);
});

test("observer scope can be reused across panel mounts and tolerates unsupported browsers", () => {
  const instances = [];
  class FakeResizeObserver {
    constructor() { this.disconnected = false; instances.push(this); }
    observe() {}
    disconnect() { this.disconnected = true; }
  }
  const scope = observerScope.createObserverScope(FakeResizeObserver);
  scope.observe({}, () => {});
  scope.clear();
  scope.observe({}, () => {});
  assert.equal(scope.size, 1);
  assert.deepEqual(instances.map((item) => item.disconnected), [true, false]);
  const unsupported = observerScope.createObserverScope(undefined);
  assert.equal(unsupported.observe({}, () => {}), null);
  unsupported.clear();
  assert.equal(unsupported.size, 0);
});


test("feature panel mounts replace stale roots and release layout observers", () => {
  const domainPath = fileURLToPath(new URL("../app-domain.js", import.meta.url));
  const domain = readFileSync(domainPath, "utf8");
  const mount = domain.match(/function mountPanel\(id, writeHash = true\) \{([\s\S]*?)\n\}/);
  assert.ok(mount);
  assert.match(mount[1], /^\s*clearPanelLayoutObservers\(\)/);
  assert.match(mount[1], /previousRoot\.cloneNode\(false\)[\s\S]*previousRoot\.replaceWith\(root\)[\s\S]*OwoPanelRuntime\.mount\(panel, root/);
  assert.equal((domain.match(/panelObserverScope\.observe\(container,/g) || []).length, 2);
});
