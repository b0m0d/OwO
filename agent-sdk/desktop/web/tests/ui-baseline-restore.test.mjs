import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { runInNewContext } from "node:vm";
const read = file => readFileSync(new URL("../" + file, import.meta.url), "utf8").replace(/\r\n/g, "\n");
const BASELINE_HASH = "34794e3d983aa907ed4ee515a3d732ccc319250598cbf896569a3305bb4ea2bf";
const CURRENT_PREFIX_HASH = "5cfd5691d8c1b492c4529aebbb1db38bda04f8fe9ccd83a0affda27e306fd5fb";
// 这两个哈希都对应同一套冻结结构/样式：后者只多了 2026-11-02 的 logo 与
// UI 精修增量（单独 CSS 文件、不改主结构）。哈希是回退保险，不是禁止增量。
const ALLOWED_PREFIX_HASHES = new Set([BASELINE_HASH, CURRENT_PREFIX_HASH]);
test("welcome, rail, top bar and composer keep the a01b1bf1 visual baseline", () => {
  const html = read("index.html");
  const rawPrefix = html.slice(0, html.indexOf('  <aside id="right"'));
  const prefix = rawPrefix.replace('  <link rel="stylesheet" href="task-details.css?v=20261101-restore">\n', '');
  const hash = createHash("sha256").update(prefix).digest("hex");
  assert.ok(ALLOWED_PREFIX_HASHES.has(hash), `unexpected frozen home prefix hash: ${hash}`);
  assert.doesNotMatch(prefix, /agent-workspace\.css|task-navigation|composerOptions|taskListHeading/);
  if (hash === CURRENT_PREFIX_HASH) {
    assert.match(rawPrefix, /brand-logo\.css/);
    assert.match(rawPrefix, /ui-refine\.css/);
    assert.equal(rawPrefix.includes('src="assets/owo-logo-64.png?v=20261102-transparent"'), false);
  }
});
test("retained project filters classify real running and waiting tasks outside the welcome page", () => {
  const root = {};
  runInNewContext(read("core/task-workbench.js"), root);
  runInNewContext(read("panels/home.panel.js"), root);
  const filter = root.OwoPanels.home._test.filterSessions;
  const sessions = [{id:"run",facts:{outcome:"running"}},{id:"wait"},{id:"history"},{id:"chat",mode:"chat"}];
  const facts = {approvals:[{sessionId:"wait"}]};
  assert.equal(filter(sessions,"",facts), sessions);
  assert.deepEqual(Array.from(filter(sessions,"running",facts), s=>s.id), ["run"]);
  assert.deepEqual(Array.from(filter(sessions,"waiting",facts), s=>s.id), ["wait"]);
  assert.deepEqual(Array.from(filter(sessions,"history",facts), s=>s.id), ["history"]);
  assert.deepEqual(Array.from(filter(sessions,"chat",facts), s=>s.id), ["chat"]);
});

test("project filter changes neither source sessions nor the sidebar and is inert after leaving the panel", async () => {
  const root = { OwoSessionFacts:{fromDetail:()=>({outcome:"completed"})} };
  runInNewContext(read("core/task-workbench.js"), root);
  runInNewContext(read("panels/home.panel.js"), root);
  const panel = root.OwoPanels.home;
  const sessions = [{id:"task",title:"Task source"},{id:"chat",title:"Chat source",mode:"chat"}];
  const host={innerHTML:"",scrollTop:0,setAttribute(){},querySelectorAll:()=>[],contains:()=>false};
  let requests=0;
  await panel.mount(host,{esc:String,snapshot:()=>({sessionsKnown:true,sessions}),get:async path=>{
    requests++; return path==="/teams"?{teams:[]}:path==="/human/inbox"?{items:[]}:{};
  }});
  assert.match(host.innerHTML,/Task source/); assert.match(host.innerHTML,/Chat source/);
  const beforeRequests=requests, change=host.onchange;
  change({target:{matches:()=>true,value:"chat"}});
  assert.doesNotMatch(host.innerHTML,/Task source/); assert.match(host.innerHTML,/Chat source/);
  assert.equal(requests,beforeRequests); assert.equal(sessions.length,2);
  const filtered=host.innerHTML; panel.dispose();
  change({target:{matches:()=>true,value:""}});
  assert.equal(host.innerHTML,filtered); assert.equal(host.onchange,null);
});
