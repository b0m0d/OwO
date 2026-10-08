import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../app.js", import.meta.url), "utf8");
const unavailable = /function markConnectionUnavailable\(error\) \{[\s\S]*?\n\}/.exec(source)?.[0];
const ready = /function markConnectionReady\(authenticated\) \{[\s\S]*?\n\}/.exec(source)?.[0];
assert.ok(unavailable && ready, "connection status transitions should be present");

function mountedStatus() {
  const nodes = {};
  for (const id of ["health", "menubarHealth", "connectionSummary"]) {
    const node = {
      textContent: "",
      style: {},
      classes: new Set(),
      classList: {
        add(name) { node.classes.add(name); },
        remove(name) { node.classes.delete(name); },
      },
    };
    nodes[id] = node;
  }
  const calls = { offline: 0, online: 0, invalidationStarted: 0, invalidationStopped: 0 };
  let connectionHandler = null;
  const shell = { addEventListener(name, handler) { if (name === "owo:connection") connectionHandler = handler; } };
  const sandbox = {
    window: shell,
    authorizationUnavailable: false,
    connectionUnavailableUntil: 0,
    shellHydrated: true,
    Date: { now: () => 1000 },
    $(id) { return nodes[id] || null; },
    serviceWatch: {
      notifyOffline() { calls.offline += 1; },
      markOnline() { calls.online += 1; },
    },
    startInvalidation() { calls.invalidationStarted += 1; },
    stopInvalidation() { calls.invalidationStopped += 1; },
  };
  const listener = /window\.addEventListener\("owo:connection", \(event\) => \{[\s\S]*?\n\}\);/.exec(source)?.[0];
  assert.ok(listener, "connection event handler should be present");
  runInNewContext(unavailable + "\n" + ready + "\n" + listener +
    "\nthis.statusHandlers = { markConnectionUnavailable, markConnectionReady };", sandbox);
  return { ...sandbox.statusHandlers, connectionHandler, nodes, calls };
}

test("HTTP health success does not erase a token-pairing authorization failure", () => {
  const h = mountedStatus();
  h.markConnectionUnavailable({ status: 403, message: "token 引导失败（HTTP 403）" });
  assert.equal(h.nodes.menubarHealth.textContent, "授权异常");
  assert.equal(h.nodes.connectionSummary.textContent, "服务在线 · 桌面授权失败");
  assert.ok(h.nodes.connectionSummary.classes.has("auth-unavailable"));
  assert.equal(h.calls.offline, 0, "an HTTP 403 proves the service is reachable");

  h.markConnectionReady(false);
  assert.equal(h.nodes.menubarHealth.textContent, "授权异常", "the public /health probe cannot clear auth state");
  assert.equal(h.nodes.connectionSummary.classes.has("auth-unavailable"), true);
  assert.equal(h.calls.invalidationStarted, 0);

  h.markConnectionReady();
  assert.equal(h.nodes.menubarHealth.textContent, "服务已连接", "an authenticated request restores the normal status");
  assert.equal(h.nodes.connectionSummary.textContent, "服务已连接");
  assert.equal(h.nodes.connectionSummary.classes.has("auth-unavailable"), false);
  assert.equal(h.calls.invalidationStarted, 1);
});

test("transport errors still show the offline state and start low-frequency recovery", () => {
  const h = mountedStatus();
  h.markConnectionUnavailable(new Error("Failed to fetch"));
  assert.equal(h.nodes.menubarHealth.textContent, "服务未连接");
  assert.equal(h.nodes.connectionSummary.classes.has("offline"), true);
  assert.equal(h.nodes.connectionSummary.classes.has("auth-unavailable"), false);
  assert.equal(h.calls.offline, 1);
});


test("ApiClient authorization events update the global status and stop authenticated streams", () => {
  const h = mountedStatus();
  h.connectionHandler({
    detail: {
      ready: false,
      error: {
        status: 403,
        code: "auth/pairing_required/not_retryable",
        message: "此浏览器未获得桌面授权",
      },
    },
  });
  assert.equal(h.nodes.menubarHealth.textContent, "授权异常");
  assert.equal(h.nodes.connectionSummary.textContent, "服务在线 · 桌面授权失败");
  assert.equal(h.calls.online, 1, "403 confirms that the HTTP service answered");
  assert.equal(h.calls.offline, 0);
  assert.equal(h.calls.invalidationStopped, 1, "an unauthenticated client must stop SSE");
  assert.equal(h.calls.invalidationStopped, 1, "an unauthenticated client must stop SSE");
});
