import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { createRequire } from "node:module";
const nav = createRequire(import.meta.url)("../core/workbench-navigation.js");
const read = file => readFileSync(new URL("../" + file, import.meta.url), "utf8");

test("sidebar primary action starts a conversation instead of opening the task wizard", () => {
  const index = read("index.html");
  const button = index.match(/<button[^>]*id="sidebarNewChat"[^>]*>[\s\S]*?<\/button>/)?.[0] || "";
  assert.match(button, /新建对话/);
  assert.doesNotMatch(button, /data-workbench-panel/);
  assert.match(read("app.js"), /\$\("sidebarNewChat"\)\?\.addEventListener\("click", startNewConversation\)/);
});

test("workbench navigation gives each in-panel route one home", () => {
  const sections = nav.sections();
  assert.deepEqual(sections[0].entries.map(item => item.id), ["home","project-history","evidence","action-center"]);
  assert.deepEqual(sections.slice(1).map(group => group.label), ["资料与记忆","工具与权限","运行与诊断","实验与评测"]);
  const ids = sections.flatMap(group => group.entries.map(item => item.id));
  assert.equal(new Set(ids).size, ids.length);
  assert.equal(ids.length, 17);
  // 团队协作、三个自动化页面与帮助页是一级入口 / 独立页面，不重复出现在工作台内部导航。
  const outsideWorkbench = new Set(["project-launcher","workswarm","automations","workflow","custom-workflows","about"]);
  assert.deepEqual([...ids].sort(), nav.entries.filter(item => !outsideWorkbench.has(item.id)).map(item => item.id).sort());
  assert.deepEqual(nav.sections(["notes","permissions"]).flatMap(group => group.entries.map(item => item.id)), ["notes","permissions"]);
});

test("left rail has one workbench hub, keeps team, and promotes automation", () => {
  const index = read("index.html");
  const rail = index.match(/<nav class="workbench-shortcuts"[\s\S]*?<\/nav>/)?.[0] || "";
  assert.match(rail, /data-workbench-panel="home"/);
  for (const id of ["project-history", "evidence", "action-center"]) {
    assert.doesNotMatch(rail, new RegExp(`data-workbench-panel="${id}"`), `${id} 不应再占用一级入口`);
  }
  for (const id of ["workswarm", "automations", "workflow", "custom-workflows"]) {
    assert.match(rail, new RegExp(`data-workbench-panel="${id}"`), `${id} 应保留一级入口`);
  }
  assert.doesNotMatch(index, /id="codexToolsEntry"/);
  assert.doesNotMatch(index, /更多功能/);
  assert.match(index, /id="panelSearch"/);
});

test("search reveals folded destinations and restores user disclosure after clearing", () => {
  const buttons = ["notes","permissions","eval"].map(id => ({ dataset: { panel: id }, hidden: false }));
  const sections = buttons.map((button,i) => ({ dataset: {}, open: i === 0, hidden: false, querySelectorAll: () => [button] }));
  const root = { querySelectorAll: selector => selector.startsWith("button") ? buttons : sections };
  const source = read("app-domain.js").match(/function filterPanelNavigation\(nav, query\) \{[\s\S]*?\n\}/)[0];
  const filter = runInNewContext(source + "\nfilterPanelNavigation;", { window: { OwoWorkbenchNavigation: nav }, PANEL_ORDER: buttons.map(b => b.dataset.panel), Set, String, Array });
  filter(root, "权限");
  assert.equal(sections[1].open, true);
  assert.equal(buttons[1].hidden, false);
  assert.equal(sections[0].hidden, true);
  filter(root, "无匹配任务");
  assert.ok(sections.every(section => section.hidden));
  filter(root, "");
  assert.ok(buttons.every(button => !button.hidden));
  assert.equal(sections[0].open, true);
  assert.equal(sections[1].open, false);
  assert.ok(sections.every(section => !section.hidden));
});

test("empty-session entrance survives administrative notices but yields to work, failures and running turns", () => {
  const state = { sessionId: "empty" };
  const hidden = new Set();
  const views = { empty: { querySelector: () => null }, worked: { querySelector: () => ({}) }, failed: { querySelector: () => ({}) } };
  let running = null;
  const empty = { classList: { toggle: (name,on) => on ? hidden.add(name) : hidden.delete(name) } };
  const messages = { querySelector: selector => views[selector.match(/data-sid="([^"]+)"/)[1]] };
  const source = read("app.js").match(/function syncSessionWelcome\(\) \{[\s\S]*?\n\}/)[0];
  const sync = runInNewContext(source + "\nsyncSessionWelcome;", { state, $: id => id === "emptyState" ? empty : messages, currentTurn: () => running, Boolean });
  sync(); assert.equal(hidden.has("hidden"), false);
  state.sessionId = "worked"; sync(); assert.equal(hidden.has("hidden"), true);
  state.sessionId = "empty"; sync(); assert.equal(hidden.has("hidden"), false);
  running = {}; sync(); assert.equal(hidden.has("hidden"), true);
  running = null; state.sessionId = "failed"; sync(); assert.equal(hidden.has("hidden"), true);
  state.sessionId = null; sync(); assert.equal(hidden.has("hidden"), false);
});
