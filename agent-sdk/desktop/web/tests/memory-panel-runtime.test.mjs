import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../panels/memory.panel.js", import.meta.url), "utf8");

function harness(helpers, windowOptions = {}) {
  const elements = new Map();
  function element() {
    return {
      value: "", textContent: "", innerHTML: "", disabled: false, handlers: {},
      addEventListener(name, callback) { this.handlers[name] = callback; },
    };
  }
  const document = {
    getElementById(id) {
      if (!elements.has(id)) elements.set(id, element());
      return elements.get(id);
    },
  };
  const root = {
    set innerHTML(_) {},
    querySelector(selector) { return document.getElementById(selector.replace(/^#/, "")); },
  };
  const window = { OwoPanels: {}, location: { origin: "http://localhost" }, ...windowOptions };
  runInNewContext(source, { window, document, Promise, String, JSON, Number, encodeURIComponent, Object });
  const panel = window.OwoPanels.memory;
  panel.mount(root, helpers);
  return { panel, document, elements };
}

function deferred() {
  let resolve;
  const promise = new Promise(res => { resolve = res; });
  return { promise, resolve };
}

test("memory panel initializes its default API helpers when none are injected", async () => {
  const requested = [];
  const h = harness(undefined, {
    OwoApi: {
      stream: async path => {
        requested.push(path);
        return { ok: true, status: 200, json: async () => ({ timeline: [], entities: [], links: [], entries: [] }) };
      },
    },
  });
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.ok(requested.includes("/memory/graph/timeline"));
  assert.ok(requested.includes("/memory/graph/entities?limit=30"));
  assert.ok(requested.includes("/memory/graph/links"));
  assert.ok(requested.includes("/memory/graph/entries?limit=50"));
});

test("memory recall keeps the newest query and ignores late responses after disposal", async () => {
  const pending = new Map();
  const h = harness({
    get(path) {
      if (path.includes("/recall?")) {
        const request = deferred();
        pending.set(path, request);
        return request.promise;
      }
      return Promise.resolve({ buckets: [], entities: [], links: [], entries: [] });
    },
    post: async () => ({}), esc: value => String(value), friendlyError: error => String(error),
  });
  const input = h.document.getElementById("owo-memory-recall");
  const box = h.document.getElementById("owo-memory-recall-box");
  input.value = "first";
  const first = h.panel._test.doRecall();
  input.value = "second";
  const second = h.panel._test.doRecall();
  pending.get("/memory/graph/recall?q=second&top_k=5").resolve({ count: 1, hits: [{ app_id: "b", ts: "now", summary: "new query result" }] });
  await second;
  pending.get("/memory/graph/recall?q=first&top_k=5").resolve({ count: 1, hits: [{ app_id: "a", ts: "then", summary: "old query result" }] });
  await first;
  assert.match(box.innerHTML, /new query result/);
  assert.doesNotMatch(box.innerHTML, /old query result/);

  input.value = "after-leave";
  const afterLeave = h.panel._test.doRecall();
  h.panel.dispose();
  pending.get("/memory/graph/recall?q=after-leave&top_k=5").resolve({ count: 1, hits: [{ summary: "detached result" }] });
  await afterLeave;
  assert.doesNotMatch(box.innerHTML, /detached result/);
});

test("manual memory relation rejects incomplete input and suppresses duplicate submissions", async () => {
  const pending = deferred();
  const posts = [];
  const h = harness({
    get: async () => ({ buckets: [], entities: [], links: [], entries: [] }),
    post(path, body) { posts.push([path, body]); return pending.promise; },
    esc: value => String(value), friendlyError: error => String(error), notify() {},
  });
  const a = h.document.getElementById("owo-memory-rel-a");
  const b = h.document.getElementById("owo-memory-rel-b");
  const relation = h.document.getElementById("owo-memory-rel-r");
  const button = h.document.getElementById("owo-memory-rel-add");
  await button.handlers.click();
  assert.equal(posts.length, 0);
  a.value = "Ada"; b.value = "Project"; relation.value = "owns";
  const first = button.handlers.click();
  const second = button.handlers.click();
  await second;
  assert.equal(posts.length, 1);
  assert.equal(button.disabled, true);
  pending.resolve({});
  await first;
  assert.equal(button.disabled, false);
  assert.equal(a.value, "");
});
