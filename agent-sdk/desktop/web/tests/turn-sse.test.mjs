import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import "../core/turn-sse.js";

const here = fileURLToPath(new URL(".", import.meta.url));
const turnSse = globalThis.OwoTurnSse;

test("SSE parser preserves event id and joins multiline data", () => {
  assert.deepEqual(turnSse.parseBlock("id: 42\r\nevent: token_delta\r\ndata: {\"type\":\r\ndata: \"token_delta\"}"), {
    event: "token_delta",
    id: "42",
    data: "{\"type\":\n\"token_delta\"}",
  });
});

test("replay URL encodes the session and sends a session sequence cursor", () => {
  const url = new URL(turnSse.replayPath("session /1", "turn-2", 17), "http://local");
  assert.equal(url.pathname, "/session/session%20%2F1/turn/events");
  assert.equal(url.searchParams.get("turn_id"), "turn-2");
  assert.equal(url.searchParams.get("after_seq"), "17");
  assert.equal(url.searchParams.get("limit"), "256");
});

test("replay records are turn-scoped, strictly after cursor, and ordered", () => {
  const page = { events: [
    { turn_id: "other", seq: 9 },
    { turn_id: "turn-1", seq: 12 },
    { turn_id: "turn-1", seq: 11 },
    { turn_id: "turn-1", seq: 10 },
  ] };
  assert.deepEqual(turnSse.eventsAfterCursor(page, "turn-1", 10).map((event) => event.seq), [11, 12]);
});

test("active desktop entry loads the durable consumer before app.js", () => {
  const index = readFileSync(join(here, "../index.html"), "utf8");
  const app = readFileSync(join(here, "../app.js"), "utf8");
  const domain = readFileSync(join(here, "../app-domain.js"), "utf8");
  const core = readFileSync(join(here, "../core/turn-sse.js"), "utf8");
  const streamIndex = index.indexOf('src="core/turn-sse.js"');
  const domainIndex = index.indexOf('src="app-domain.js"');
  const appIndex = index.indexOf('src="app.js"');
  assert.ok(streamIndex >= 0 && streamIndex < domainIndex && domainIndex < appIndex);
  assert.ok(domain.includes("OwoTurnSse.consumeResponse"));
  assert.ok(domain.includes("OwoTurnSse.replayPath"));
  assert.ok(domain.includes("OwoApi.stream"));
  assert.ok(core.includes("x-owo-turn-id"));
  assert.ok(domain.includes('case "turn_failed"'));
  assert.ok(!/async function sendPrompt\(/.test(app), "sendPrompt 的唯一实现应归属 app-domain.js");
});
