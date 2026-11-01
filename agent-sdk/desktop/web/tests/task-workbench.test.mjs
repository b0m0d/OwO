import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
const workbench = createRequire(import.meta.url)("../core/task-workbench.js");
const read = name => readFileSync(new URL("../" + name, import.meta.url), "utf8");

test("background approval belongs to its own task, and waiting takes precedence over running", () => {
  const a = {id:"a", mode:"agent"}, b = {id:"b", mode:"agent"};
  const facts = {activeTurns:new Map([["a",{phase:"tool"}],["b",{phase:"tool"}]]), approvals:[{sessionId:"b"}]};
  assert.equal(workbench.category(a, facts), "running");
  assert.equal(workbench.category(b, facts), "waiting");
  assert.equal(workbench.matches(a, "waiting", facts), false);
  assert.equal(workbench.category(a, {...facts, questionSessionId:"a"}), "waiting");
});
test("restored active host turn stays running; idle history never implies successful completion", () => {
  assert.equal(workbench.category({id:"s",active_turn_id:"host-turn"}), "running");
  assert.equal(workbench.category({id:"s"}), "history");
  assert.equal(workbench.completion({outcome:"completed"}).label, "尚无完成记录");
});
test("ordinary chat has its own section while an active chat remains discoverable under running", () => {
  const chat={id:"c",mode:"chat"};
  assert.equal(workbench.matches(chat,"all"),false);
  assert.equal(workbench.matches(chat,"chat"),true);
  assert.equal(workbench.matches(chat,"history"),false);
  assert.equal(workbench.matches(chat,"running",{activeTurns:new Map([["c",{}]])}),true);
});
test("accepted text alone cannot fabricate host acceptance or evidence", () => {
  assert.equal(workbench.completion({lastCompletionStatus:"accepted"}).label,"回答完成 · 未验证");
  assert.equal(workbench.completion({lastCompletionRecord:{status:"accepted",evidence_receipt_ids:[]}}).label,"回答完成 · 未验证");
  assert.equal(workbench.completion({lastCompletionRecord:{status:"accepted",evidence_receipt_ids:["receipt-1"]}}).label,"验收通过");
  assert.equal(workbench.completion({outcome:"failed"}).label,"执行失败");
  assert.equal(workbench.completion({lastCompletionStatus:"aborted"}).label,"已取消");
});
test("actual tool identifiers distinguish terminal, file operations and unknown tools", () => {
  assert.equal(workbench.toolKind("run_command"),"terminal");
  assert.equal(workbench.toolKind("edit_file"),"write");
  assert.equal(workbench.toolKind("list_dir"),"read");
  assert.equal(workbench.toolKind("some_plugin"),"tool");
});
test("inspector tabs support keyboard navigation and keep only the selected panel visible", () => {
  const panels=new Map(["progress","changes","results"].map(x=>["inspector"+x,{hidden:false}]));
  const tabs=[...panels.keys()].map((id,i)=>({dataset:{inspectorTab:["progress","changes","results"][i]},attrs:{"aria-controls":id},setAttribute(k,v){this.attrs[k]=v;},getAttribute(k){return this.attrs[k];},focus(){this.focused=true;}}));
  const source=read("app.js").match(/function selectInspectorTab\(tab, focus = false\) \{[\s\S]*?\n\}/)[0];
  const select=runInNewContext(source+"\nselectInspectorTab;",{document:{querySelectorAll:()=>tabs},$:id=>panels.get(id),String});
  select("changes",true);
  assert.equal(tabs[1].attrs["aria-selected"],"true");
  assert.equal(tabs[1].focused,true);
  assert.equal(tabs[0].tabIndex,-1);
  assert.equal(panels.get("inspectorchanges").hidden,false);
  assert.equal(panels.get("inspectorprogress").hidden,true);
});
