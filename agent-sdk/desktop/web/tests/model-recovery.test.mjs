import test from "node:test";
import assert from "node:assert/strict";
import recovery from "../core/model-recovery.js";
import { readFileSync } from "node:fs";

test("provider model-missing errors offer an in-session switch action", () => {
  assert.equal(recovery.shouldOfferModelSwitch('400 Bad Request: {"error":{"code":"1211","message":"\u6a21\u578b\u4e0d\u5b58\u5728"}}'), true);
  assert.equal(recovery.shouldOfferModelSwitch("\u627e\u4e0d\u5230\u6a21\u578b qwen3.8-max"), true);
  assert.equal(recovery.shouldOfferModelSwitch("model_not_found: qwen3.8-max"), true);
});

test("provider failures show an actionable summary and retain raw JSON for diagnostics", () => {
  const raw = '模型网关全部失败：模型返回 400 Bad Request: {"error":{"code":"1211","message":"模型不存在，请检查模型代码。"}} · 结果未验证';
  const result = recovery.summarizeTurnFailure(raw);
  assert.equal(result.category, "model");
  assert.match(result.message, /模型代码与服务商/);
  assert.doesNotMatch(result.message, /1211|Bad Request/);
  assert.equal(result.detail, raw);
  assert.equal(result.providerCode, "1211");
  assert.match(recovery.summarizeTurnFailure("401 Unauthorized").message, /API 密钥/);
  assert.match(recovery.summarizeTurnFailure("429 rate limit").message, /限流或额度不足/);
  assert.match(recovery.summarizeTurnFailure("503 Service Unavailable").message, /暂时不可用/);
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
  const domain = readFileSync(new URL("../app-domain.js", import.meta.url), "utf8");
  assert.ok(index.indexOf('<script src="core/model-recovery.js"></script>') < index.indexOf('<script src="app.js"></script>'));
  assert.match(app, /shouldOfferModelSwitch\(reason\)/);
  assert.match(app, /shouldOfferOutputBudget\(reason\)/);
  assert.match(app, /summary\.textContent = "技术详情"/);
  assert.match(app, /raw\.textContent = presentation\.detail/);
  assert.match(app, /textContent = "调整输出上限…"/);
  assert.match(app, /function addHistoricalTurnFailure\(storedText\)/);
  assert.match(app, /function attachTurnFailureActions\(failure, reason, presentation\)/);
  assert.match(domain, /addHistoricalTurnFailure\(storedContent\)/);
  assert.match(app, /action\.addEventListener\("click", \(\) => openModelMenu\(\)\)/);
});
