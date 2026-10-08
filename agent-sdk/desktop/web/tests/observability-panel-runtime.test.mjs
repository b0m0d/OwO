import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../panels/observability.panel.js", import.meta.url), "utf8");

function harness(helpers, windowOptions = {}) {
  const elements = new Map();
  function element() {
    return {
      innerHTML: "", textContent: "", style: {}, disabled: false, handlers: {},
      addEventListener(name, callback) { this.handlers[name] = callback; },
      setAttribute(name, value) { this[name] = value; },
      removeAttribute(name) { delete this[name]; },
      querySelector(selector) { return get(selector.replace(/^#/, "")); },
    };
  }
  function get(key) {
    if (!elements.has(key)) elements.set(key, element());
    return elements.get(key);
  }
  const section = element();
  section.querySelector = selector => get(selector.replace(/^#/, ""));
  const root = {
    set innerHTML(_) {},
    querySelector(selector) { return selector === '[data-panel="observability"]' ? section : get(selector.replace(/^#/, "")); },
  };
  const document = {
    getElementById: get,
    querySelector(selector) { return get(selector.replace(/^#/, "")); },
  };
  const window = { OwoPanels: {}, location: { origin: "http://localhost" }, ...windowOptions };
  runInNewContext(source, { window, document, Promise, String, JSON, Number, Math, Object, isFinite });
  const panel = window.OwoPanels.observability;
  panel.mount(root, helpers);
  return { panel, elements, section, get };
}

function deferred() {
  let resolve;
  const promise = new Promise(res => { resolve = res; });
  return { promise, resolve };
}

function dataFor(path) {
  if (path === "/metrics/overview") return { traces_count: 1, avg_turn_ms: 2, updated_at: "2030-01-01T00:00:00Z" };
  if (path === "/metrics/turns?limit=50") return { turns: [] };
  if (path === "/metrics/tools") return { tools: [] };
  if (path === "/metrics/health") return { components: {} };
  if (path === "/metrics/runtime") return {};
  if (path === "/metrics/slo") return { slo: [] };
  if (path === "/usage/summary") return { dimensions: [] };
  if (path === "/metrics/slo/alerts") return { rules: [], alerts: [] };
  if (path === "/metrics/telemetry/status") return { enabled: false };
  return {};
}

test("observability refresh uses only the latest response and invalidates work on disposal", async () => {
  const overview = [];
  const h = harness({
    get(path) {
      if (path === "/metrics/overview") {
        const request = deferred();
        overview.push(request);
        return request.promise;
      }
      return Promise.resolve(dataFor(path));
    },
    esc: value => String(value), friendlyError: error => String(error),
  });
  const first = h.panel.refresh();
  const second = h.panel.refresh();
  overview[2].resolve({ traces_count: 22, avg_turn_ms: 2 });
  await second;
  overview[1].resolve({ traces_count: 11, avg_turn_ms: 1 });
  await first;
  assert.match(h.get("owo-mtr-cards").innerHTML, />22</);
  assert.doesNotMatch(h.get("owo-mtr-cards").innerHTML, />11</);

  const afterDispose = h.panel.refresh();
  h.panel.dispose();
  overview[3].resolve({ traces_count: 33, avg_turn_ms: 3 });
  await afterDispose;
  assert.match(h.get("owo-mtr-cards").innerHTML, />22</);
});

test("observability panel initializes default API helpers and reports HTTP failures", async () => {
  const paths = [];
  const h = harness(undefined, {
    OwoApi: {
      async stream(path) {
        paths.push(path);
        return { ok: false, status: 503, json: async () => ({ message: "metrics unavailable" }) };
      },
    },
  });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.ok(paths.includes("/metrics/overview"));
  assert.match(h.get("owo-mtr-cards").textContent, /metrics unavailable/);
});

test("observability report requests are fenced when the page is disposed", async () => {
  const pending = deferred();
  const h = harness({
    get(path) { return path.includes("/report?") ? pending.promise : Promise.resolve(dataFor(path)); },
    esc: value => String(value), friendlyError: error => String(error),
  });
  const report = h.panel._test.loadReport();
  h.panel.dispose();
  pending.resolve({ period_days: 7, slo: [{ name: "late report" }] });
  await report;
  assert.equal(h.get("owo-mtr-report").innerHTML, "");
});
