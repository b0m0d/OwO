import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const web = join(dirname(fileURLToPath(import.meta.url)), "..");
const app = readFileSync(join(web, "app.js"), "utf8");

function extractFunction(source, name) {
  const start = source.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `missing ${name}`);
  let depth = 0;
  let opened = false;
  for (let i = start; i < source.length; i += 1) {
    if (source[i] === "{") { depth += 1; opened = true; }
    else if (source[i] === "}") {
      depth -= 1;
      if (opened && depth === 0) return source.slice(start, i + 1);
    }
  }
  throw new Error(`unclosed ${name}`);
}

class NodeStub {
  constructor(tag) { this.tag = tag; this.children = []; this.listeners = {}; }
  setAttribute(name, value) { this[name] = value; }
  append(...nodes) { this.children.push(...nodes); }
  appendChild(node) { this.children.push(node); }
  addEventListener(name, fn) { this.listeners[name] = fn; }
}

function renderWith(renderer) {
  const root = new NodeStub("main");
  root.hidden = true;
  root.replaceChildren = (...nodes) => { root.children = nodes; };
  const classes = new Set();
  let reloads = 0;
  const sandbox = {
    document: {
      getElementById: (id) => id === "setupRoot" ? root : null,
      createElement: (tag) => new NodeStub(tag),
      body: { classList: { add: (name) => classes.add(name) } },
    },
    window: {
      renderOwoSetupGuide: renderer,
      __owoSetupDiagnostics: {},
      OwoApi: { resetCoreConnection() {} },
      location: { reload: () => { reloads += 1; } },
    },
  };
  vm.runInNewContext(`${extractFunction(app, "renderSetupGuide")}
__result = renderSetupGuide();`, sandbox);
  return { result: sandbox.__result, root, classes, reloads: () => reloads };
}

test("missing setup view renders a visible alert and a reload action", () => {
  const ui = renderWith(undefined);
  assert.equal(ui.result, false);
  assert.equal(ui.root.hidden, false);
  assert.ok(ui.classes.has("setup-required"));
  const card = ui.root.children[0];
  assert.equal(card.role, "alert");
  assert.equal(card.children[0].textContent, "首次配置暂时无法显示");
  assert.equal(card.children[2].textContent, "重新加载");
  card.children[2].listeners.click();
  assert.equal(ui.reloads(), 1);
});

test("setup view initialization errors render the same actionable fallback", () => {
  const ui = renderWith(() => { throw new Error("renderer failure"); });
  assert.equal(ui.result, false);
  assert.equal(ui.root.hidden, false);
  assert.equal(ui.root.children[0].role, "alert");
  assert.match(ui.root.children[0].children[1].textContent, /启动失败/);
  ui.root.children[0].children[2].listeners.click();
  assert.equal(ui.reloads(), 1);
});
