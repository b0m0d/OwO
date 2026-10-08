import test from "node:test";
import assert from "node:assert/strict";
import recovery from "../core/model-recovery.js";
import { readFileSync } from "node:fs";

test("provider model-missing errors offer an in-session switch action", () => {
  assert.equal(recovery.shouldOfferModelSwitch('400 Bad Request: {"error":{"code":"1211","message":"\u6a21\u578b\u4e0d\u5b58\u5728"}}'), true);
  assert.equal(recovery.shouldOfferModelSwitch("\u627e\u4e0d\u5230\u6a21\u578b qwen3.8-max"), true);
  assert.equal(recovery.shouldOfferModelSwitch("model_not_found: qwen3.8-max"), true);
});

test("output-budget failures offer a direct path to raise the affected model limit", () => {
  assert.equal(recovery.shouldOfferOutputBudget("模型输出达到 max_tokens 上限（finish_reason=length）"), true);
  assert.equal(recovery.shouldOfferOutputBudget("stream finished with finish_reason = length"), true);
  assert.equal(recovery.shouldOfferOutputBudget("401 unauthorized"), false);
  assert.equal(recovery.shouldOfferOutputBudget("max_tokens 参数不受支持"), false);
});

test("unrelated request failures do not offer model-switch recovery", () => {
  assert.equal(recovery.shouldOfferModelSwitch("401 unauthorized"), false);
  assert.equal(recovery.shouldOfferModelSwitch("rate limit exceeded"), false);
});


test("desktop web loads recovery helper and attaches the switch action to model errors", () => {
  const index = readFileSync(new URL("../index.html", import.meta.url), "utf8");
  const app = readFileSync(new URL("../app.js", import.meta.url), "utf8");
  assert.ok(index.indexOf('<script src="core/model-recovery.js"></script>') < index.indexOf('<script src="app.js"></script>'));
  assert.match(app, /shouldOfferModelSwitch\(reason\)/);
  assert.match(app, /shouldOfferOutputBudget\(reason\)/);
  assert.match(app, /textContent = "调整输出上限…"/);
  assert.match(app, /action\.addEventListener\("click", \(\) => openModelMenu\(\)\)/);
});
