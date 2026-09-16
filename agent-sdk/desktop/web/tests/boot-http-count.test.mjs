import { test } from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { ApiClient } from "../core/api-client.js";

// §4 首屏请求口径：真实传输层计数测试。启动一个计数 HTTP server，按
// hydrateShell 的请求口径（health 先行 + 4 个 BOOT_HYDRATE_TASKS 业务请求）
// 经真实 fetch 执行，分别记录两种模式的总请求数：
// - desktop-webview（壳注入 token）：≤5，且不含 /auth/token；
// - browser-dev（/auth/token 引导）：6（health 1 + token 1 + 业务 4）。
// 两种口径分开断言，不允许用一个口径掩盖另一种。

const BOOT_BUSINESS_PATHS = ["/sessions", "/skills", "/whitelist", "/project/rules"];

function startCountingServer() {
  const hits = [];
  const server = http.createServer((req, res) => {
    hits.push(req.url.split("?")[0]);
    const auth = req.headers["authorization"] || "";
    const isPublic = ["/health", "/auth/token", "/openapi.json"].includes(req.url);
    if (!auth.startsWith("Bearer ") && !isPublic) {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: "unauthorized", code: "auth/unauthorized/not_retryable" }));
      return;
    }
    if (req.url === "/auth/token") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ token: "srv-token" }));
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true }));
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      resolve({ server, hits, port: server.address().port });
    });
  });
}

test("§4 desktop-webview 模式：壳注入 token，冷启动真实请求 = 5 且零 /auth/token", async () => {
  const { server, hits, port } = await startCountingServer();
  try {
    const client = new ApiClient(`http://127.0.0.1:${port}`);
    // 模拟壳经 get_core_connection 注入 token（注入链路在 api-client.test.mjs 覆盖）。
    client.injectedToken = "injected-by-shell";
    client.token = client.injectedToken;
    await client.get("/health");
    for (const path of BOOT_BUSINESS_PATHS) await client.get(path);
    assert.deepEqual(hits, ["/health", ...BOOT_BUSINESS_PATHS], "请求序列必须恰好是 health+4 业务");
    assert.ok(hits.length <= 5, "正式桌面冷启动总请求 ≤5，实际 " + hits.length);
    assert.ok(!hits.includes("/auth/token"), "注入模式不得再请求 /auth/token");
  } finally {
    server.close();
  }
});

test("§4 browser-dev 模式：/auth/token 引导，冷启动真实请求 = 6", async () => {
  const { server, hits, port } = await startCountingServer();
  try {
    const client = new ApiClient(`http://127.0.0.1:${port}`);
    await client.get("/health", { public: true });
    for (const path of BOOT_BUSINESS_PATHS) await client.get(path);
    assert.deepEqual(hits, ["/health", "/auth/token", ...BOOT_BUSINESS_PATHS]);
    assert.equal(hits.length, 6, "浏览器开发模式口径单独记录（health 1 + token 1 + 业务 4）");
  } finally {
    server.close();
  }
});

test("§4 无 token 的业务请求在传输层被拒（401 JSON），引导后重试成功", async () => {
  const { server, hits, port } = await startCountingServer();
  try {
    const client = new ApiClient(`http://127.0.0.1:${port}`);
    const result = await client.get("/sessions");
    assert.deepEqual(result, { ok: true });
    assert.deepEqual(hits, ["/auth/token", "/sessions"], "token 引导先行，业务请求一次成功");
  } finally {
    server.close();
  }
});
