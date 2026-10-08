
import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { readFileSync } from "node:fs";
const source = readFileSync(new URL("../core/workbench-refresh.js", import.meta.url), "utf8");
const flush = () => new Promise(resolve => setImmediate(resolve));
function fixture(plans, concurrency = 3) {
  let time = 0, id = 0, background = false;
  const timers = new Map(), listeners = new Map(), delays = [];
  const document = { hidden: false, body: { classList: { contains: () => background } },
    addEventListener: (name, fn) => listeners.set(name, fn),
    removeEventListener: name => listeners.delete(name) };
  const ctx = { document }; vm.createContext(ctx); vm.runInContext(source, ctx);
  const scheduler = ctx.OwoRefresh.createScheduler(plans, {
    document, concurrency, now: () => time,
    setTimeout: (fn, delay) => { delays.push(delay); timers.set(++id, fn); return id; },
    clearTimeout: key => timers.delete(key),
  });
  return { scheduler, document, timers, listeners, delays,
    background(value) { background = value; },
    advance(ms) { time += ms; const entries = [...timers.values()]; timers.clear(); for (const fn of entries) fn(); },
  };
}

test("refreshes are bounded globally and independently drained as slots free", async () => {
  const starts = [], releases = [];
  const plans = Array.from({ length: 7 }, (_, index) => ({
    intervalMs: 1000, refresh: () => { starts.push(index); return new Promise(r => releases.push(r)); },
  }));
  const f = fixture(plans); f.scheduler.start(); f.advance(1000); await flush();
  assert.deepEqual(starts, [0, 1, 2]); assert.equal(f.scheduler.stats.running, 3);
  releases[0](); await flush();
  assert.deepEqual(starts, [0, 1, 2, 3]); assert.equal(f.scheduler.stats.peakConcurrent, 3);
  f.scheduler.stop();
  for (const release of releases) release(); await flush();
  assert.equal(starts.length, 4); assert.equal(f.timers.size, 0);
});

test("slow refresh never overlaps itself or queues catch-up requests", async () => {
  let count = 0, release;
  const f = fixture([{ intervalMs: 1000, refresh: () => {
    count++; return new Promise(r => { release = r; });
  } }]);
  f.scheduler.start(); f.advance(1000); await flush();
  f.advance(10000); await flush(); assert.equal(count, 1);
  assert.equal(f.scheduler.stats.queued, 0);
  release(); await flush(); f.advance(1000); await flush();
  assert.equal(count, 2); f.scheduler.stop(); release(); await flush();
});

test("hidden page and native background state suppress fresh requests", async () => {
  let count = 0;
  const f = fixture([{ intervalMs: 1000, refresh: () => { count++; } }]);
  f.scheduler.start(); f.document.hidden = true; f.advance(5000); await flush(); assert.equal(count, 0);
  f.document.hidden = false; f.background(true); f.advance(5000); await flush(); assert.equal(count, 0);
  f.background(false); f.listeners.get("visibilitychange")(); await flush();
  assert.equal(count, 1); assert.equal(f.timers.size, 1); f.scheduler.stop();
});

test("queued work is discarded if the page hides while requests are in flight", async () => {
  let starts = 0, release;
  const f = fixture(Array.from({ length: 4 }, () => ({ intervalMs: 1000,
    refresh: () => { starts++; return new Promise(r => { release = r; }); } })), 1);
  f.scheduler.start(); f.advance(1000); await flush();
  f.document.hidden = true; release(); await flush();
  assert.equal(starts, 1); assert.equal(f.scheduler.stats.queued, 0);
  assert.equal(f.scheduler.stats.hiddenSkipped, 3); f.scheduler.stop();
});

test("a rejected refresh releases its slot and allows later attempts", async () => {
  let calls = 0;
  const f = fixture([{ intervalMs: 1000, refresh: async () => { calls++; throw Error("offline"); } }]);
  f.scheduler.start(); f.advance(1000); await flush();
  assert.equal(f.scheduler.stats.failed, 1); assert.equal(f.scheduler.stats.running, 0);
  f.advance(1000); await flush(); assert.equal(calls, 2); f.scheduler.stop();
});

test("start is idempotent and stop removes timers/listeners without cancelling active calls", async () => {
  let release, starts = 0;
  const f = fixture([{ intervalMs: 1000, refresh: () => { starts++; return new Promise(r => { release = r; }); } }]);
  f.scheduler.start(); f.scheduler.start();
  assert.equal(f.timers.size, 1); assert.equal(f.listeners.size, 1);
  f.advance(1000); await flush(); f.scheduler.stop();
  assert.equal(f.listeners.size, 0); assert.equal(f.timers.size, 0);
  release(); await flush(); f.advance(10000); await flush(); assert.equal(starts, 1);
});

test("actual bootstrap health-checks first and runs only the four essential hydration requests", async () => {
  const app = readFileSync(new URL("../app.js", import.meta.url), "utf8");
  const taskMatch = app.match(/const BOOT_HYDRATE_TASKS = \[([^\]]+)\];/);
  assert.ok(taskMatch, "startup hydration contract must be explicit");
  const tasks = [...taskMatch[1].matchAll(/\b(refresh[A-Z]\w*)\b/g)].map(match => match[1]);
  assert.deepEqual(tasks, ["refreshSessions", "refreshSettings", "refreshSkills", "refreshWhitelist"]);
  const hydrateStart = app.indexOf("const BOOT_HYDRATE_TASKS =");
  const bootStart = app.indexOf("async function boot()", hydrateStart);
  const hydrateSource = app.slice(hydrateStart, bootStart);
  const names = [...new Set(tasks)];
  let running = 0, peak = 0, hydrated = 0;
  const order = [];
  const context = {
    serviceWatch: { start: async () => { order.push("health"); } },
    window: { OwoRecovery: { runWithConcurrency: async (items, limit) => {
      assert.equal(limit, 4);
      return Promise.all(items.map(item => item()));
    } } },
  };
  for (const name of names) context[name] = async () => {
    running++; hydrated++; peak = Math.max(peak, running);
    await flush(); running--; order.push(name);
  };
  vm.createContext(context); vm.runInContext(hydrateSource, context);
  await context.hydrateShell();
  assert.equal(hydrated, 4); assert.equal(peak, 4);
  assert.equal(order[0], "health", "health probe must precede business hydration");
  const planBlock = app.match(/const WORKBENCH_REFRESH_PLANS = \[([\s\S]*?)\n\];/);
  assert.ok(planBlock);
  assert.equal([...planBlock[1].matchAll(/\{ refresh:/g)].length, 3, "steady-state polling is limited to health, perception, and pet state");
  const bootBody = app.slice(bootStart, app.indexOf("\nboot();", bootStart));
  assert.match(bootBody, /await hydrateShell\(\);[\s\S]*?await restoreLastSession\(\);[\s\S]*?shellHydrated = true;[\s\S]*?startInvalidation\(\);[\s\S]*?workbenchRefresh\.start\(\);/);
});


test("scheduler sleeps until the nearest refresh deadline instead of waking every second", () => {
  const f = fixture([
    { intervalMs: 30000, refresh() {} },
    { intervalMs: 60000, refresh() {} },
  ]);
  f.scheduler.start();
  assert.deepEqual(f.delays, [30000]);
  f.advance(30000);
  assert.equal(f.delays.at(-1), 30000);
  f.scheduler.stop();
});

test("有效初始深链在服务水合与会话恢复后应用", () => {
  const app = readFileSync(new URL("../app.js", import.meta.url), "utf8");
  assert.match(app, /await restoreLastSession\(\);[\s\S]{0,160}applyDeepLink\(\);[\s\S]{0,120}startInvalidation\(\)/);
});
