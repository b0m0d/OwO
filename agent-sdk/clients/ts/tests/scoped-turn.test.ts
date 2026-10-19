import test from "node:test";
import assert from "node:assert/strict";
import { createClient } from "../src/index.js";

const id = "01234567-89ab-4def-8123-456789abcdef";
const completed = 'data: {"type":"final","text":"ok"}\n\ndata: {"type":"turn_stats","completion_status":"response_complete"}\n\n';
function requestUrl(input: RequestInfo | URL): string { return input instanceof Request ? input.url : String(input); }

test("unsupported constraints fail before submitting a turn", async () => {
  const saved = globalThis.fetch;
  const urls: string[] = [];
  globalThis.fetch = async input => { urls.push(requestUrl(input)); return Response.json({ constraints: {} }); };
  try {
    const client = createClient({ baseUrl: "http://localhost:4096" });
    await assert.rejects(client.runTurn({ id: "s", prompt: "write", read_only: true }, { onEvent() {} }), /read_only_unsupported/);
    await assert.rejects(client.runTurn({ id: "s", prompt: "write", turn_id: id }, { onEvent() {} }), /scoped_cancellation_unsupported/);
    assert.equal(urls.length, 2);
    assert.ok(urls.every(url => url.endsWith("/capabilities")));
  } finally { globalThis.fetch = saved; }
});

test("read-only and canonical turn identity reach the wire", async () => {
  const saved = globalThis.fetch;
  let submitted: unknown;
  globalThis.fetch = async (input, init) => {
    if (requestUrl(input).endsWith("/capabilities")) return Response.json({ constraints: { request_read_only: true, scoped_turn_cancellation: true } });
    submitted = JSON.parse(String(init?.body));
    return new Response(completed, { headers: { "x-owo-turn-id": id } });
  };
  try {
    const client = createClient({ baseUrl: "http://localhost:4096" });
    const result = await client.runTurn({ id: "s", prompt: "inspect", read_only: true, turn_id: id.toUpperCase() }, { onEvent() {} });
    assert.equal(result.completionStatus, "response_complete");
    assert.deepEqual(submitted, { prompt: "inspect", read_only: true, turn_id: id });
  } finally { globalThis.fetch = saved; }
});

test("abort after submission is fenced to the submitted turn", async () => {
  const saved = globalThis.fetch;
  const controller = new AbortController();
  let submittedId: string | undefined;
  let abortedId: string | undefined;
  globalThis.fetch = async (input, init) => {
    const url = requestUrl(input);
    if (url.endsWith("/capabilities")) return Response.json({ constraints: { scoped_turn_cancellation: true } });
    if (url.endsWith("/abort")) { abortedId = JSON.parse(String(init?.body)).turn_id; return Response.json({ status: "cancellation_requested" }); }
    submittedId = JSON.parse(String(init?.body)).turn_id;
    controller.abort();
    throw new DOMException("Aborted", "AbortError");
  };
  try {
    const client = createClient({ baseUrl: "http://localhost:4096" });
    await assert.rejects(client.runTurn({ id: "s", prompt: "run" }, { onEvent() {}, signal: controller.signal }), { name: "AbortError" });
    assert.match(submittedId!, /^[0-9a-f-]{36}$/);
    assert.equal(abortedId, submittedId);
  } finally { globalThis.fetch = saved; }
});

test("a mismatched response identity cannot be reported as completed", async () => {
  const saved = globalThis.fetch;
  globalThis.fetch = async input => requestUrl(input).endsWith("/capabilities")
    ? Response.json({ constraints: { scoped_turn_cancellation: true } })
    : new Response(completed, { headers: { "x-owo-turn-id": "different" } });
  try {
    await assert.rejects(createClient({ baseUrl: "http://localhost:4096" }).runTurn({ id: "s", prompt: "run", turn_id: id }, { onEvent() {} }), /identity_mismatch/);
  } finally { globalThis.fetch = saved; }
});

test("custom model connections are capability gated and forwarded intact", async () => {
  const saved = globalThis.fetch;
  const connection = { model: "fixture", base_url: "http://localhost:7777/v1",
    api_format: "openai" as const, api_key: "fixture-secret", temperature: 0, timeout_secs: 3 };
  let supported = false; let submissions = 0; let submitted: unknown;
  globalThis.fetch = async (input, init) => {
    if (requestUrl(input).endsWith("/capabilities")) {
      return Response.json({constraints:{custom_model_connection:supported,scoped_turn_cancellation:true}});
    }
    submissions++; submitted = JSON.parse(String(init?.body));
    return new Response(completed, {headers:{"x-owo-turn-id":id}});
  };
  try {
    const client = createClient({baseUrl:"http://localhost:4096"});
    await assert.rejects(client.runTurn({id:"s",prompt:"hi",turn_id:id,model_connection:connection},
      {onEvent(){}}), /model_connection\/unsupported/);
    assert.equal(submissions,0);
    supported = true;
    await client.runTurn({id:"s",prompt:"hi",turn_id:id,model_connection:connection},{onEvent(){}});
    assert.deepEqual(submitted,{prompt:"hi",turn_id:id,model_connection:connection});
  } finally {globalThis.fetch=saved;}
});
