import test from "node:test";
import assert from "node:assert/strict";
import { createClient } from "../src/index.js";

const BASE = process.env.OWO_TS_SDK_BASE ?? "http://127.0.0.1:4097";
const client = createClient({ baseUrl: BASE });

test("health 返回可用", async () => {
  const ok = await client.health();
  assert.equal(ok, true);
});

test("创建会话并列出（场景 4 集成）", async () => {
  // X03 鉴权链路：业务 API 需 bearer；/auth/token 为公开同源引导端点
  // （浏览器侧由 CORS 白名单限制跨源读取；Node 直连可正常完成自举配对）。
  const boot = await fetch(`${BASE}/auth/token`);
  assert.ok(boot.ok, `/auth/token 引导应可用：HTTP ${boot.status}`);
  const { token } = (await boot.json()) as { token?: string };
  assert.ok(token, "引导响应应携带 token");
  const authorized = createClient({
    baseUrl: BASE,
    headers: { Authorization: `Bearer ${token}` },
  });
  const session = await authorized.createSession({
    workspace: process.cwd(),
    prompt: "SDK 集成测试",
  });
  assert.ok(session.id, "应返回会话 id");
  const list = await authorized.GET("/sessions");
  assert.ok(list.response.ok);
  const sessions = (list.data ?? []) as Array<{ id: string }>;
  assert.ok(
    sessions.some((s) => s.id === session.id),
    "新会话应出现在列表",
  );
});
