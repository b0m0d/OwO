import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const app = readFileSync(new URL("../app.js", import.meta.url), "utf8");
const watchSource = /const serviceWatch = \(\(\) => \{[\s\S]*?\n\}\)\(\);/.exec(app);
assert.ok(watchSource, "service watch implementation should be present in app.js");

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function harness(request) {
  let nextTimer = 0;
  const timers = new Map();
  const banner = { hidden: true, classList: {
    add() { banner.hidden = true; },
    remove() { banner.hidden = false; },
  } };
  const text = { textContent: "" };
  const retry = { addEventListener(_name, fn) { this.click = fn; } };
  let now = 1000;
  const context = {
    window: { OwoApi: { request } },
    $: (id) => id === "serviceBannerText" ? text : id === "serviceBanner" ? banner : id === "serviceBannerRetry" ? retry : null,
    uiHidden: () => false,
    markConnectionReady: () => watch.markOnline(),
    markConnectionUnavailable: () => {},
    document: { addEventListener() {} },
    Date: { now: () => now },
    setTimeout(fn, delay) { const id = ++nextTimer; timers.set(id, { fn, delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  let watch;
  watch = runInNewContext(watchSource[0] + "\nserviceWatch;", context);
  return {
    watch, banner, text, timers,
    advance(ms) { now += ms; },
    async fireNext() {
      const first = timers.entries().next();
      assert.equal(first.done, false);
      const [id, timer] = first.value;
      timers.delete(id);
      timer.fn();
      await new Promise((resolve) => setImmediate(resolve));
    },
  };
}

test("startup and manual recovery share one in-flight health probe", async () => {
  const pending = deferred();
  let probes = 0;
  const h = harness(() => { probes += 1; return pending.promise; });
  const first = h.watch.start();
  const second = h.watch.start();
  assert.equal(probes, 1);
  pending.reject(new Error("offline"));
  assert.deepEqual(await Promise.all([first, second]), [false, false]);
  assert.equal(probes, 1);
  assert.deepEqual([...h.timers.values()].map((timer) => timer.delay), [100]);
});

test("an older failed probe cannot undo a later successful API connection", async () => {
  const pending = deferred();
  const h = harness(() => pending.promise);
  const probing = h.watch.start();
  h.watch.markOnline();
  pending.reject(new Error("stale failure"));
  assert.equal(await probing, true);
  assert.equal(h.banner.hidden, true);
  assert.equal(h.timers.size, 0);
  assert.equal(h.text.textContent, "");
});
