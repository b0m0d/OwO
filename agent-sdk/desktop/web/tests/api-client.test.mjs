import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { ApiClient } from "../core/api-client.js";

const here = fileURLToPath(new URL(".", import.meta.url));

test("业务脚本不绕过统一 API 客户端", () => {
  const webRoot = join(here, "..");
  const files = [];
  const visit = (dir) => {
    for (const name of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, name.name);
      if (name.isDirectory() && !name.name.startsWith("tests")) visit(path);
      else if (name.isFile() && name.name.endsWith(".js")) files.push(path);
    }
  };
  visit(webRoot);
  const offenders = files.filter((path) => !path.endsWith(join("core", "api-client.js")) && /fetch\s*\(/.test(readFileSync(path, "utf8")));
  assert.deepEqual(offenders, [], "只有 core/api-client.js 可以直接访问网络");
});

test("路由切换在清空页面前保留设置节点，自动恢复会启动周期刷新", () => {
  const app = readFileSync(join(here, "../app.js"), "utf8");
  const index = readFileSync(join(here, "../index.html"), "utf8");
  assert.match(
    app,
    /setSettingsLocation\(false\);\s*content\.replaceChildren\(\);\s*setSettingsLocation\(route === "settings"\);/,
    "设置节点必须在 routeContent 清空前回迁，避免 settings -> 其他页 -> settings 丢失节点"
  );
  assert.match(
    app,
    /await hydrateShell\(\);\s*serviceReady = true;\s*if \(window\.owoRouter\) window\.owoRouter\.start\(\);\s*startRefreshTimers\(\);/,
    "自动恢复完成后必须启用后台刷新"
  );
  assert.match(index, /var localCore = "http:\/\/127\.0\.0\.1:4096"/);
  assert.match(index, /window\.OWO_API_BASE = window\.OWO_API_BASE/);
});

test("请求自动带 token，401 只刷新并重试一次", async () => {
  const calls = [];
  const original = global.fetch;
  let protectedCalls = 0;
  let tokenRequests = 0;
  global.fetch = async (url, options = {}) => {
    calls.push({ url, options });
    if (String(url).endsWith("/auth/token")) {
      tokenRequests += 1;
      return new Response(JSON.stringify({ token: tokenRequests === 1 ? "token-a" : "token-b" }), { status: 200 });
    }
    protectedCalls += 1;
    if (protectedCalls === 1) return new Response("expired", { status: 401 });
    return new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "content-type": "application/json" } });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    assert.deepEqual(await client.get("/sessions"), { ok: true });
    assert.equal(calls.filter((call) => call.url.endsWith("/auth/token")).length, 2);
    assert.equal(calls.filter((call) => call.url.endsWith("/sessions")).length, 2);
    assert.equal(calls[1].options.headers.get("Authorization"), "Bearer token-a");
    assert.equal(calls[3].options.headers.get("Authorization"), "Bearer token-b");
  } finally {
    global.fetch = original;
  }
});

test("上传和下载复用同一鉴权路径", async () => {
  const original = global.fetch;
  const seen = [];
  global.fetch = async (url, options = {}) => {
    seen.push({ url, options });
    if (String(url).endsWith("/auth/token")) return new Response(JSON.stringify({ token: "token" }), { status: 200 });
    return String(url).endsWith("/download")
      ? new Response("zip", { status: 200 })
      : new Response(JSON.stringify({ text: "ok" }), { status: 200 });
  };
  try {
    const client = new ApiClient("");
    const upload = await client.upload("/upload", new Blob(["wav"]), "audio/wav");
    const download = await client.download("/download");
    assert.equal(upload.text, "ok");
    assert.equal(await download.text(), "zip");
    assert.equal(seen.filter((item) => item.url === "/upload")[0].options.headers.get("Content-Type"), "audio/wav");
    assert.equal(seen.filter((item) => item.url === "/download")[0].options.headers.get("Authorization"), "Bearer token");
  } finally {
    global.fetch = original;
  }
});

test("Tauri 桌面端先向壳询问核心连接，携带实例身份与配对证明引导 token", async () => {
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  const commands = [];
  const seen = [];
  global.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      commands.push(command);
      if (command === "get_core_connection") {
        return {
          port: 6071,
          instanceId: "a1b2c3d4e5f60718293a4b5c6d7e8f90",
          pairing: "0123456789abcdef0123456789abcdef",
          apiVersion: "0.7",
          pid: 1234,
          buildId: "abc123",
          state: "ready",
        };
      }
      return "0123456789abcdef0123456789abcdef";
    },
  };
  global.fetch = async (url, options = {}) => {
    seen.push({ url, options });
    if (String(url).endsWith("/auth/token")) return new Response(JSON.stringify({ token: "token" }), { status: 200 });
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    await client.get("/sessions");
    assert.deepEqual(commands, ["get_core_connection"], "配对证明改由连接描述符提供，无需再问 desktop_pairing");
    assert.equal(String(seen[0].url), "http://127.0.0.1:6071/auth/token", "基址必须切换到壳报告的动态端口");
    assert.equal(seen[0].options.headers.get("x-owo-desktop-instance"), "a1b2c3d4e5f60718293a4b5c6d7e8f90");
    assert.equal(seen[0].options.headers.get("X-Owo-Desktop-Pairing"), "0123456789abcdef0123456789abcdef");
    // §8.1：ledger 来源标签——web 出口一律自带 x-owo-client（含 token 引导请求）。
    assert.equal(seen[0].options.headers.get("x-owo-client"), "web");
    assert.deepEqual(global.__owoCoreDiagnostics, {
      state: "ready",
      port: 6071,
      pid: 1234,
      buildId: "abc123",
      // §6.1.4：旧壳不带 expectedBuildId → 归一为 null（比对降级为不可用，不误报）。
      expectedBuildId: null,
      apiVersion: "0.7",
      instanceId: "a1b2c3d4e5f60718293a4b5c6d7e8f90",
    });
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

test("§6.1.4 诊断对象同时暴露 expected/actual build id（不一致时供错误页消费）", async () => {
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  global.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      if (command === "get_core_connection") {
        return {
          port: 6074,
          instanceId: "inst-mismatch",
          pairing: "0123456789abcdef0123456789abcdef",
          apiVersion: "0.7",
          pid: 4321,
          buildId: "actual-core-build",
          // 壳编译期期望值 ≠ core 实际上报值：诊断必须两个都带上，
          // 是否判定为不匹配由错误页/恢复流程消费方决定。
          expectedBuildId: "shell-expected-build",
          state: "ready",
        };
      }
      throw new Error("unexpected command " + command);
    },
  };
  global.fetch = async (url) => {
    if (String(url).endsWith("/auth/token")) return new Response(JSON.stringify({ token: "token" }), { status: 200 });
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    await client.get("/sessions");
    assert.deepEqual(global.__owoCoreDiagnostics, {
      state: "ready",
      port: 6074,
      pid: 4321,
      buildId: "actual-core-build",
      expectedBuildId: "shell-expected-build",
      apiVersion: "0.7",
      instanceId: "inst-mismatch",
    });
    assert.notEqual(global.__owoCoreDiagnostics.buildId, global.__owoCoreDiagnostics.expectedBuildId);
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

test("§4 壳注入 token：桌面模式冷启动零 /auth/token 请求，重连后失效", async () => {
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  const seen = [];
  let connectionGeneration = 0;
  global.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      if (command === "get_core_connection") {
        connectionGeneration += 1;
        return {
          port: 6070 + connectionGeneration,
          instanceId: "inst-" + connectionGeneration,
          pairing: "0123456789abcdef0123456789abcdef",
          token: "injected-token-" + connectionGeneration,
          state: "ready",
        };
      }
      throw new Error("unexpected command " + command);
    },
  };
  global.fetch = async (url, options = {}) => {
    seen.push({ url: String(url), options });
    if (String(url).endsWith("/auth/token")) {
      return new Response(JSON.stringify({ token: "bootstrap-token" }), { status: 200 });
    }
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    await client.get("/sessions");
    assert.ok(
      !seen.some((item) => item.url.endsWith("/auth/token")),
      "壳注入 token 后不得再请求 /auth/token（§4 冷启动 ≤5）"
    );
    assert.equal(seen[0].options.headers.get("Authorization"), "Bearer injected-token-1");
    // 重连（core 被壳重启）：注入凭据必须失效并重新向壳取新 token。
    client.resetCoreConnection();
    await client.get("/sessions");
    assert.equal(seen.filter((i) => i.url.includes("127.0.0.1:6072")).length, 1, "重查连接后基址切换到新实例");
    assert.equal(seen.filter((i) => i.url.endsWith("/sessions"))[1].options.headers.get("Authorization"), "Bearer injected-token-2");
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

test("§8.2 第5条：401 必须整体重查壳连接（旧注入 token 不得复用）", async () => {
  // core 每次启动换发 bearer。若 401 重试只清 this.token，injectedToken 会把
  // 上一代 token 再注入一遍 → 二次 401 → 界面永久"未授权"。这条测试锁死修复。
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  const seen = [];
  const commands = [];
  let connectionGeneration = 0;
  global.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      commands.push(command);
      if (command === "get_core_connection") {
        connectionGeneration += 1;
        return {
          port: 6110 + connectionGeneration,
          instanceId: "inst-" + connectionGeneration,
          pairing: "0123456789abcdef0123456789abcdef",
          token: "injected-token-" + connectionGeneration,
          state: "ready",
        };
      }
      throw new Error("unexpected command " + command);
    },
  };
  global.fetch = async (url, options = {}) => {
    const text = String(url);
    seen.push({ url: text, options });
    if (text.includes(":6111")) {
      return new Response("expired", { status: 401 }); // 上一代端口：token 已换发
    }
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    const result = await client.get("/sessions");
    assert.deepEqual(result, { ok: true }, "401 重试后应成功");
    assert.equal(
      commands.filter((c) => c === "get_core_connection").length,
      2,
      "401 后必须重新向壳查询连接（而非只清本地 token）"
    );
    assert.ok(
      !seen.some((item) => item.url.endsWith("/auth/token")),
      "桌面模式重连不得回落到公开引导端点"
    );
    const retry = seen[seen.length - 1];
    assert.ok(retry.url.includes(":6112"), `重试应打到新一代端口：${retry.url}`);
    assert.equal(retry.options.headers.get("Authorization"), "Bearer injected-token-2");
    assert.equal(
      seen.filter((item) => item.options.headers.get("x-owo-client")).length,
      seen.length,
      "每次尝试（含重试）都必须带来源标签"
    );
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

test("§8.2 第5条：core 重启换端口后，网络失败必须触发重查连接并重试成功", async () => {
  // 旧端口上的 fetch 是**网络错误**（不是 401）：只修 401 分支时运行期重启会永久失联。
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  const seen = [];
  const commands = [];
  let generation = 0;
  global.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      commands.push(command);
      if (command === "get_core_connection") {
        generation += 1;
        return {
          port: 6210 + generation,
          instanceId: "gen-" + generation,
          pairing: "0123456789abcdef0123456789abcdef",
          token: "tok-" + generation,
          state: "ready",
        };
      }
      throw new Error("unexpected command " + command);
    },
  };
  global.fetch = async (url, options = {}) => {
    const text = String(url);
    seen.push({ url: text, auth: options.headers.get("Authorization") });
    if (text.includes(":6211")) {
      throw new TypeError("Failed to fetch"); // 上一代端口：进程已没了
    }
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    const result = await client.get("/sessions");
    assert.deepEqual(result, { ok: true }, "网络失败后应经重查连接自愈");
    assert.ok(
      commands.filter((c) => c === "get_core_connection").length >= 2,
      `连接层失败必须重查壳连接，实际命令：${commands.join(",")}`
    );
    assert.equal(seen[seen.length - 1].url, "http://127.0.0.1:6212/sessions");
    assert.equal(seen[seen.length - 1].auth, "Bearer tok-2", "重试必须带新一代 bearer");
    assert.equal(client.networkFailures, 0, "成功后网络失败计数归零");
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

test("§8.2 第5条：核心始终不可达时网络失败只重试一次并受冷却约束（不得风暴重查）", async () => {
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  let connectionQueries = 0;
  global.__TAURI_INTERNALS__ = {
    invoke: async () => {
      connectionQueries += 1;
      return {
        port: 6310,
        instanceId: "down",
        pairing: "0123456789abcdef0123456789abcdef",
        token: "tok-down",
        state: "ready",
      };
    },
  };
  global.fetch = async () => {
    throw new TypeError("Failed to fetch");
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    // 第 1 次请求：失败 → 重查连接 → 再失败 → 上抛（不得吞错，UI 才能显示未连接）。
    await assert.rejects(() => client.get("/sessions"), "核心不可达必须上抛");
    const afterFirst = connectionQueries;
    assert.ok(afterFirst >= 1, "首次失败应至少重查一次连接");
    // 第 2、3 次请求：处于重查冷却窗口内，不得再向壳发起查询（风暴放大防护）。
    await assert.rejects(() => client.get("/sessions"));
    await assert.rejects(() => client.get("/skills"));
    assert.equal(connectionQueries, afterFirst, "冷却期内不得重复重查壳连接");
    // 尝试计数：req1 两次尝试（重查前 1 次 + 重查后 1 次，重查时归零）、
    // req2/req3 各一次 → 稳定为 3。用它验证"失败被如实记录，不被吞掉"。
    assert.equal(client.networkFailures, 3, "网络失败计数供诊断读取（重查后归零再累计）");
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

test("核心未就绪时保留原基址，并暴露启动诊断供恢复流程消费", async () => {
  const originalFetch = global.fetch;
  const originalTauri = global.__TAURI_INTERNALS__;
  const originalDiagnostics = global.__owoCoreDiagnostics;
  const commands = [];
  global.__TAURI_INTERNALS__ = {
    invoke: async (command) => {
      commands.push(command);
      if (command === "get_core_connection") {
        return { port: 0, state: "failed", errorCode: "core/spawn_failed", message: "核心服务启动失败", logPath: "C:\\logs\\core.log" };
      }
      return "0123456789abcdef0123456789abcdef";
    },
  };
  global.fetch = async (url, options = {}) => {
    if (String(url).endsWith("/auth/token")) return new Response(JSON.stringify({ token: "token" }), { status: 200 });
    return new Response(JSON.stringify({ ok: true }), { status: 200 });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    await client.get("/sessions");
    assert.ok(commands.includes("desktop_pairing"), "连接未就绪时回退到 desktop_pairing 证明");
    assert.ok(String(commands[0]) === "get_core_connection", "首个命令仍是连接询问");
    assert.deepEqual(global.__owoCoreDiagnostics, {
      state: "failed",
      errorCode: "core/spawn_failed",
      message: "核心服务启动失败",
      logPath: "C:\\logs\\core.log",
    });
    // 恢复前重查连接：缓存与基址必须复位
    client.resetCoreConnection();
    assert.equal(client.baseUrl, "http://127.0.0.1:4096");
    assert.equal(client.coreInstanceId, null);
  } finally {
    global.fetch = originalFetch;
    global.__owoCoreDiagnostics = originalDiagnostics;
    if (originalTauri === undefined) delete global.__TAURI_INTERNALS__;
    else global.__TAURI_INTERNALS__ = originalTauri;
  }
});

function sseBody(chunks) {
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(new TextEncoder().encode(chunk));
      controller.close();
    },
  });
}

test("§3.1 openEventStream：Bearer 头 + 401 单次刷新重试（token 不进 URL）", async () => {
  const original = global.fetch;
  const seen = [];
  let protectedCalls = 0;
  let tokenRequests = 0;
  global.fetch = async (url, options = {}) => {
    seen.push({ url: String(url), options });
    if (String(url).endsWith("/auth/token")) {
      tokenRequests += 1;
      return new Response(JSON.stringify({ token: tokenRequests === 1 ? "tok-a" : "tok-b" }), { status: 200 });
    }
    protectedCalls += 1;
    if (protectedCalls === 1) return new Response("expired", { status: 401 });
    return new Response(sseBody([]), { status: 200, headers: { "content-type": "text/event-stream" } });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    await client.openEventStream("/events/stream", {});
    assert.equal(protectedCalls, 2, "401 后必须单次刷新重试");
    const [first, second] = seen.filter((item) => item.url.endsWith("/events/stream"));
    assert.equal(first.options.headers.get("Authorization"), "Bearer tok-a");
    assert.equal(second.options.headers.get("Authorization"), "Bearer tok-b");
    assert.equal(second.options.headers.get("Accept"), "text/event-stream");
    assert.ok(!String(second.url).includes("tok"), "token 不得出现在 URL 中");
  } finally {
    global.fetch = original;
  }
});

test("§3.1 openEventStream：Last-Event-ID 头续传 + SSE 逐帧解析（id/多行/注释帧）", async () => {
  const original = global.fetch;
  const inner = JSON.stringify({ domain: "mcp", version: 3 });
  const frameData = JSON.stringify({ seq: 1, kind: "invalidate", data: inner });
  global.fetch = async (url) => {
    if (String(url).endsWith("/auth/token")) return new Response(JSON.stringify({ token: "tok" }), { status: 200 });
    const body = sseBody([
      ': keep-alive\n\n',
      "event: invalidate\nid: 1\ndata: " + frameData + "\n\n",
      "event: heartbeat\nid: 2\ndata: {\"type\":\"heartbeat\"}\n\n",
    ]);
    return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    const frames = [];
    let opened = 0;
    await client.openEventStream("/events/stream", {
      lastEventId: 7,
      onOpen: () => { opened += 1; },
      onEvent: (frame) => frames.push(frame),
    });
    assert.equal(opened, 1, "流建立时必须回调 onOpen");
    assert.equal(frames.length, 2, "注释帧不得分发");
    assert.equal(frames[0].event, "invalidate");
    assert.equal(frames[0].id, "1");
    assert.equal(JSON.parse(frames[0].data).data, inner);
    assert.equal(frames[1].event, "heartbeat");
    assert.equal(frames[1].id, "2");
    // 复核续传头确实随请求发送。
  } finally {
    global.fetch = original;
  }
});

test("§3.1 openEventStream：Last-Event-ID 头随请求发送", async () => {
  const original = global.fetch;
  const seen = [];
  global.fetch = async (url, options = {}) => {
    seen.push({ url: String(url), options });
    if (String(url).endsWith("/auth/token")) return new Response(JSON.stringify({ token: "tok" }), { status: 200 });
    return new Response(sseBody([]), { status: 200, headers: { "content-type": "text/event-stream" } });
  };
  try {
    const client = new ApiClient("http://127.0.0.1:4096");
    await client.openEventStream("/events/stream", { lastEventId: 42 });
    const request = seen.find((item) => item.url.endsWith("/events/stream"));
    assert.equal(request.options.headers.get("Last-Event-ID"), "42", "断线续传必须经 Last-Event-ID 头");
  } finally {
    global.fetch = original;
  }
});
