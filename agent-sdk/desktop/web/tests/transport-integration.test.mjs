
import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { readFileSync } from "node:fs";
import { ApiClient } from "../core/api-client.js";

const source = (path) => readFileSync(new URL("../" + path, import.meta.url), "utf8");
function team(client) {
  const window = { OwoApi: client };
  vm.runInNewContext(source("panels/team.panel.js"), { window });
  return window.OwoPanels.team;
}

test("panel fallback shares authentication, refreshes stale token once and parses responses", async () => {
  const original = global.fetch, calls = [];
  let issued = 0, protectedCalls = 0;
  global.fetch = async (url, options) => {
    calls.push({ url, headers: new Headers(options.headers), body: options.body });
    if (url.endsWith("/auth/token")) return Response.json({ token: "token-" + (++issued) });
    if (++protectedCalls === 1) return new Response("expired", { status: 401 });
    return Response.json({ ok: true });
  };
  try {
    const client = new ApiClient("http://daemon");
    const panel = team(client);
    panel.baseUrl = "http://obsolete-port";
    assert.deepEqual(await panel._get("/team/audit"), { ok: true });
    assert.deepEqual(await panel._post("/team/import", { package_b64: "fixture" }), { ok: true });
    const business = calls.filter(call => !call.url.endsWith("/auth/token"));
    assert.equal(issued, 2);
    assert.equal(business.length, 3);
    assert.ok(business.every(call => call.url.startsWith("http://daemon")));
    assert.equal(business[0].headers.get("Authorization"), "Bearer token-1");
    assert.equal(business[1].headers.get("Authorization"), "Bearer token-2");
    assert.equal(business[2].headers.get("x-owo-client"), "web");
    assert.equal(JSON.parse(business[2].body).package_b64, "fixture");
  } finally { global.fetch = original; }
});

test("panel fallback surfaces final failure and cannot retry writes indefinitely", async () => {
  const original = global.fetch;
  let calls = 0;
  global.fetch = async () => { calls++; return new Response("denied", { status: 403 }); };
  try {
    const client = new ApiClient(""); client.token = "fixture";
    await assert.rejects(team(client)._post("/team/import", {}), error => error.status === 403);
    assert.equal(calls, 1);
  } finally { global.fetch = original; }
});

test("static resources and provider probes never inherit daemon credentials or retry lifecycle", async () => {
  const original = global.fetch, calls = [];
  global.fetch = async (url, options) => {
    calls.push({ url, options });
    return new Response("provider denied", { status: 401 });
  };
  try {
    const client = new ApiClient("http://daemon");
    client.token = "daemon-secret"; client.injectedToken = "daemon-secret";
    const response = await client.resource("https://provider.example/models", {
      headers: { Authorization: "Bearer provider-key" },
    });
    assert.equal(response.status, 401);
    await client.resource("/pet/skins/index.json", { cache: "no-store" });
    assert.equal(calls.length, 2);
    assert.equal(calls[0].url, "https://provider.example/models");
    assert.equal(calls[0].options.headers.Authorization, "Bearer provider-key");
    assert.equal(calls[0].options.credentials, "omit");
    assert.equal(calls[1].options.headers, undefined);
    assert.equal(client.token, "daemon-secret");
    assert.equal(client.networkFailures, 0);
  } finally { global.fetch = original; }
});

test("public health uses current shell port without bootstrapping auth", async () => {
  const original = global.fetch, oldShell = global.__TAURI_INTERNALS__, calls = [];
  global.__TAURI_INTERNALS__ = { invoke: async () => ({
    state: "ready", port: 6401, instanceId: "instance", token: "injected",
  }) };
  global.fetch = async (url, options) => {
    calls.push({ url, headers: new Headers(options.headers) });
    return Response.json({ healthy: true });
  };
  try {
    const client = new ApiClient("http://obsolete");
    assert.deepEqual(await client.get("/health", { public: true }), { healthy: true });
    assert.equal(calls.length, 1);
    assert.equal(calls[0].url, "http://127.0.0.1:6401/health");
    assert.equal(calls[0].headers.get("Authorization"), null);
  } finally { global.fetch = original; global.__TAURI_INTERNALS__ = oldShell; }
});

test("actual app JSON and binary wrappers share the same request boundary", async () => {
  const app = source("app.js"), calls = [], marks = [];
  const begin = app.indexOf("async function api(path");
  const end = app.indexOf("// 统一友好错误", begin);
  const context = {
    window: { OwoApi: { request: async (path, options) => {
      calls.push({ path, options });
      return options.responseType === "response" ? new Response("archive") : { ok: true };
    } } },
    markConnectionReady: () => marks.push("ready"),
    markConnectionUnavailable: () => marks.push("offline"),
  };
  vm.createContext(context); vm.runInContext(app.slice(begin, end), context);
  assert.deepEqual(await context.api("/sessions", { method: "POST", body: "{}" }), { ok: true });
  assert.equal(await (await context.apiRaw("/export")).text(), "archive");
  assert.equal(calls[1].options.responseType, "response");
  assert.deepEqual(marks, ["ready", "ready"]);
});
