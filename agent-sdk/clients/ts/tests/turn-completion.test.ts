import test from "node:test";
import assert from "node:assert/strict";
import {createClient, TurnFailure} from "../src/index.js";

async function withSse(text: string, check: (client: ReturnType<typeof createClient>) => Promise<void>) {
 const original = globalThis.fetch;
 globalThis.fetch = async () => new Response(text);
 try { await check(createClient({baseUrl:"http://localhost:4096"})); }
 finally { globalThis.fetch = original; }
}
const final = 'event: final\ndata: {"text":"中文回答"}\n\n';
test("final alone cannot resolve runTurn as delivery", async () => {
 await withSse(final, async client => {
  await assert.rejects(client.runTurn({id:"session",prompt:"test"},{onEvent:()=>{}}), /宿主完成记录/);
 });
});
test("host save failure is typed and wins over final text", async () => {
 await withSse(final+'data: {"type":"turn_failed","message":"save failed","completion_status":"unverified"}\n\n', async client => {
  await assert.rejects(client.runTurn({id:"session",prompt:"test"},{onEvent:()=>{}}), e=>e instanceof TurnFailure && e.completionStatus==="unverified");
 });
});
test("failed attempts preserve host completion evidence in the terminal event", async () => {
 const record={task_id:"single-turn:turn-1",attempt_id:"turn-1",status:"aborted",evidence_receipt_ids:["receipt-1"],candidate_version_sha256:"sha256:candidate",decided_at:"2026-10-05T00:00:00Z"};
 await withSse(final+`data: ${JSON.stringify({type:"turn_failed",message:"cancelled",completion_status:"aborted",completion_record:record})}\n\n`, async client => {
  const events: Array<Record<string, unknown>>=[];
  await assert.rejects(client.runTurn({id:"session",prompt:"test"},{onEvent:event=>events.push(event)}), e=>e instanceof TurnFailure && e.completionStatus==="aborted");
  assert.deepEqual(events[1].completion_record,record);
 });
});
test("candidate and multiline named stats remain explicit", async () => {
 await withSse(final+'event: turn_stats\ndata: {"completion_status":\ndata: "candidate"}', async client => {
  const result=await client.runTurn({id:"session",prompt:"test"},{onEvent:()=>{}});
  assert.equal(result.finalText,"中文回答");assert.equal(result.completionStatus,"candidate");
 });
});
test("consumer errors propagate rather than being swallowed as JSON errors", async () => {
 await withSse(final, async client => {
  await assert.rejects(client.runTurn({id:"session",prompt:"test"},{onEvent:()=>{throw new Error("consumer failed");}}),/consumer failed/);
 });
});
test("malformed and oversized payloads are rejected", async () => {
 for(const text of ['data: {broken}\n\n','data: '+'x'.repeat(1024*1024+1)]) {
  await withSse(text, async client => { await assert.rejects(client.runTurn({id:"session",prompt:"test"},{onEvent:()=>{}}),/格式无效|1 MiB/); });
 }
});
test("runTurn replays durable events after an SSE disconnect and deduplicates by sequence", async () => {
 const originalFetch = globalThis.fetch;
 const urls: string[] = [];
 const delivered: string[] = [];
 globalThis.fetch = async (input, init) => {
  // openapi-fetch 的 replay 调用传入 Request 对象；URL 需从 Request.url 取。
  const url = input instanceof Request ? input.url : String(input); urls.push(url);
  if (url.endsWith("/turn")) {
   let sent=false;
   const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
     if (!sent) {
      sent=true;
      controller.enqueue(new TextEncoder().encode('id: 1\nevent: final\ndata: {"text":"中文回答"}\n\n'));
     } else {
      controller.error(new Error("socket closed"));
     }
    },
   });
   return new Response(stream, {headers:{"x-owo-turn-id":"turn-1"}});
  }
  return Response.json({
   events:[
    {turn_id:"turn-1",seq:1,payload:{type:"final",text:"重复回答"}},
    {turn_id:"turn-1",seq:2,payload:{type:"turn_stats",completion_status:"accepted",total_tokens:12}},
   ],
   active:false,state:"completed",next_after_seq:2,
  });
 };
 try {
  const client=createClient({baseUrl:"http://localhost:4096",headers:{Authorization:"Bearer replay"}});
  const result=await client.runTurn({id:"session",prompt:"继续"},{onEvent:event=>delivered.push(event.type)});
  assert.equal(result.finalText,"中文回答");
  assert.equal(result.completionStatus,"accepted");
  assert.deepEqual(delivered,["final","turn_stats"]);
  assert.match(urls[1],/after_seq=1/);
 } finally { globalThis.fetch=originalFetch; }
});

test("runTurn cancels an unfinished SSE reader when a consumer callback fails", async () => {
 const originalFetch=globalThis.fetch;
 let cancelled=false;
 globalThis.fetch=async()=>new Response(new ReadableStream<Uint8Array>({
  start(controller){controller.enqueue(new TextEncoder().encode(final));},
  cancel(){cancelled=true;},
 }));
 try {
  const client=createClient({baseUrl:"http://localhost:4096"});
  await assert.rejects(client.runTurn({id:"session",prompt:"test"},{onEvent:()=>{throw new Error("consumer failed");}}),/consumer failed/);
  assert.equal(cancelled,true);
 } finally { globalThis.fetch=originalFetch; }
});
