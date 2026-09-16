import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const OwoInvalidation = require("../core/events.js");

// 服务端 event_json 帧形状：{seq, kind, critical, data: "<json>", created_at}。
function invalidateFrame(domain, version, seq) {
  return {
    event: "invalidate",
    id: String(seq),
    data: JSON.stringify({
      seq,
      kind: "invalidate",
      critical: false,
      data: JSON.stringify({ domain, version }),
      created_at: new Date().toISOString(),
    }),
  };
}

function fakeStream() {
  const api = { calls: [], current: null };
  api.openStream = function (path, opts) {
    const record = { path, opts };
    api.calls.push(record);
    api.current = record;
    return new Promise((resolve, reject) => {
      record.resolve = () => resolve();
      record.reject = (error) => reject(error);
    });
  };
  api.frame = function (frame) {
    if (api.current) api.current.opts.onEvent(frame);
  };
  api.open = function (status = 200) {
    if (api.current && api.current.opts.onOpen) api.current.opts.onOpen(status);
  };
  api.end = function () {
    if (api.current) api.current.resolve();
  };
  api.fail = function (error) {
    if (api.current) api.current.reject(error);
  };
  return api;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

test("§3.1 经 openStream（带认证 fetch-stream）建立唯一连接并收帧", async () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 0,
    reconnectBaseMs: 5,
    openStream: stream.openStream,
  });
  let calls = 0;
  inv.on("automations", () => { calls += 1; });
  inv.start();
  assert.equal(stream.calls.length, 1, "start 只建立一条连接");
  assert.equal(stream.calls[0].path, "http://127.0.0.1:4096/events/stream");
  assert.ok(stream.calls[0].opts.signal instanceof AbortSignal, "必须可取消（AbortSignal）");
  stream.open(200);
  assert.equal(inv.state(), "live", "onOpen 后进入 live");
  stream.frame(invalidateFrame("automations", 1, 1));
  assert.equal(calls, 1);
  assert.equal(inv.snapshot().lastEventId, 1, "id 行必须推进续传游标");
  inv.stop();
  assert.equal(inv.state(), "stopped");
});

test("§3.2 域失效：版本号只触发一次刷新，旧版本计 duplicate 丢弃", () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 0,
    openStream: stream.openStream,
  });
  let calls = 0;
  inv.on("automations", () => { calls += 1; });
  inv.start();
  stream.open();
  stream.frame(invalidateFrame("automations", 1, 1));
  stream.frame(invalidateFrame("automations", 1, 2)); // 重复版本 → 丢弃
  stream.frame(invalidateFrame("automations", 2, 3));
  assert.equal(calls, 2, "每个新版本只触发一次刷新");
  assert.equal(inv.versionOf("automations"), 2);
  const snap = inv.snapshot();
  assert.equal(snap.eventRefreshes, 2);
  assert.equal(snap.duplicateInvalidations, 1, "重复失效必须计数（§3.3 语义）");
  assert.equal(snap.coalescedInvalidations, 0);
  inv.stop();
});

test("§3.2 防抖：同域短窗口多次失效合并为一次（coalesced 计数）", async () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 40,
    openStream: stream.openStream,
  });
  let calls = 0;
  inv.on("mcp", () => { calls += 1; });
  inv.start();
  stream.open();
  stream.frame(invalidateFrame("mcp", 1, 1));
  stream.frame(invalidateFrame("mcp", 2, 2));
  stream.frame(invalidateFrame("mcp", 3, 3));
  await sleep(90);
  assert.equal(calls, 1, "防抖窗口内多次失效只刷一次");
  const snap = inv.snapshot();
  assert.equal(snap.coalescedInvalidations, 2, "被合并的 2 次失效必须计数");
  assert.equal(snap.eventRefreshes, 1);
  inv.stop();
});

test("§3.2 无关域不触发；解绑后不再触发；坏帧不抛异常", () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 0,
    openStream: stream.openStream,
  });
  let automations = 0;
  let whitelist = 0;
  const off = inv.on("automations", () => { automations += 1; });
  inv.on("whitelist", () => { whitelist += 1; });
  inv.start();
  stream.open();
  assert.doesNotThrow(() => {
    stream.frame({ event: "heartbeat", id: "9", data: JSON.stringify({ type: "heartbeat" }) });
    stream.frame({ event: "invalidate", id: "10", data: "{not json" });
    stream.frame({ event: "invalidate", id: "11", data: JSON.stringify({ no: "payload" }) });
  });
  assert.equal(automations + whitelist, 0, "坏帧/心跳帧不得触发业务刷新");
  stream.frame(invalidateFrame("whitelist", 5, 12));
  assert.equal(automations, 0, "无关域不得触发");
  assert.equal(whitelist, 1);
  off(); // 面板销毁解绑
  stream.frame(invalidateFrame("automations", 3, 13));
  assert.equal(automations, 0, "解绑后不得再触发");
  inv.stop();
});

test("§3.4 隐藏窗口：不触发刷新（canary=0），回可见补刷一次", async () => {
  const stream = fakeStream();
  const visibilityListeners = {};
  const fakeDocument = {
    visibilityState: "hidden",
    addEventListener(type, fn) { visibilityListeners[type] = fn; },
    removeEventListener(type) { delete visibilityListeners[type]; },
  };
  const originalDocument = globalThis.document;
  globalThis.document = fakeDocument;
  try {
    const inv = OwoInvalidation.createDomainInvalidator({
      baseUrl: "http://127.0.0.1:4096",
      debounceMs: 5,
      openStream: stream.openStream,
    });
    let calls = 0;
    inv.on("automations", () => { calls += 1; });
    inv.start();
    stream.open();
    stream.frame(invalidateFrame("automations", 1, 1));
    await sleep(30);
    assert.equal(calls, 0, "隐藏窗口不得触发业务刷新");
    assert.equal(inv.versionOf("automations"), 1, "隐藏期间版本仍推进（防重放）");
    fakeDocument.visibilityState = "visible";
    visibilityListeners.visibilitychange();
    await sleep(30);
    assert.equal(calls, 1, "回可见后补刷一次");
    assert.equal(inv.snapshot().hiddenWindowRefreshes, 0, "隐藏窗口业务刷新必须为 0");
    inv.stop();
  } finally {
    if (originalDocument) globalThis.document = originalDocument;
    else delete globalThis.document;
  }
});

test("§3.1 断线重连带 Last-Event-ID 续传", async () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 0,
    reconnectBaseMs: 10,
    openStream: stream.openStream,
  });
  inv.start();
  stream.open();
  stream.frame(invalidateFrame("automations", 1, 7));
  assert.equal(inv.snapshot().lastEventId, 7);
  stream.end(); // 服务端关闭流
  await sleep(50);
  assert.equal(stream.calls.length, 2, "流结束后必须重连");
  assert.equal(stream.calls[1].opts.lastEventId, 7, "重连必须携带 Last-Event-ID 续传");
  inv.stop();
});

test("§3.4 连续失败 → Degraded 单一兜底调度；恢复后关闭轮询", async () => {
  const stream = fakeStream();
  let attempt = 0;
  let pollTicks = 0;
  const failing = function (path, opts) {
    attempt += 1;
    if (attempt <= 2) {
      return Promise.reject(new Error("ECONNREFUSED"));
    }
    return stream.openStream(path, opts); // 第 3 次起成功
  };
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 0,
    reconnectBaseMs: 5,
    pollIntervalMs: 10,
    openStream: failing,
  });
  inv.setPollFallback(() => { pollTicks += 1; });
  inv.start();
  await sleep(80); // 两次失败 → degraded + 立即 tick + 周期 tick
  assert.equal(inv.state(), "degraded", "连续 2 次失败必须进入 Degraded");
  const degradedTicks = pollTicks;
  assert.ok(degradedTicks >= 1, "Degraded 后兜底调度必须启动");
  await sleep(30);
  assert.ok(pollTicks > degradedTicks, "Degraded 期间兜底按周期触发");
  assert.equal(stream.calls.length, 1, "恢复成功后建立新流");
  stream.open();
  assert.equal(inv.state(), "live", "重连成功回到 live");
  await sleep(40);
  const afterRecover = pollTicks;
  await sleep(40);
  assert.equal(pollTicks, afterRecover, "恢复后必须关闭轮询兜底（无双调度）");
  inv.stop();
});

test("§3.4 stop()：取消 signal、清空 timer、不再重连", async () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    debounceMs: 0,
    reconnectBaseMs: 10,
    openStream: stream.openStream,
  });
  inv.start();
  const signal = stream.calls[0].opts.signal;
  inv.stop();
  assert.ok(signal.aborted, "stop 必须 abort 当前流");
  stream.end();
  await sleep(40);
  assert.equal(stream.calls.length, 1, "stop 后不得重连");
  assert.equal(inv.state(), "stopped");
  // start 可恢复运行（幂等/重入安全）。
  inv.start();
  assert.equal(stream.calls.length, 2, "stop 后 start 必须重建连接");
  inv.stop();
});

test("§3.4 幂等：重复 start 不叠加连接", () => {
  const stream = fakeStream();
  const inv = OwoInvalidation.createDomainInvalidator({
    baseUrl: "http://127.0.0.1:4096",
    openStream: stream.openStream,
  });
  inv.start();
  inv.start();
  inv.start();
  assert.equal(stream.calls.length, 1, "重复 start 只建立一条连接");
  inv.stop();
  inv.stop(); // stop 幂等
});
