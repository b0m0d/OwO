import { test } from "node:test";
import assert from "node:assert/strict";
import "../core/completion-record.js";

const api = globalThis.OwoCompletionRecord;

test("completion evidence summary keeps host decision and candidate binding visible", () => {
  const result = api.summarize({
    status: "accepted",
    task_id: "task-1",
    attempt_id: "attempt-2",
    evidence_receipt_ids: ["workspace-accepted", "validation-accepted"],
    candidate_version_sha256: "0123456789abcdef0123456789abcdef",
  });
  assert.equal(result.status, "验收通过");
  assert.match(result.compact, /回执 2 项/);
  assert.match(result.compact, /候选 0123456789ab…/);
  assert.deepEqual(result.details, [
    ["任务", "task-1"],
    ["尝试", "attempt-2"],
    ["验收回执", "workspace-accepted、validation-accepted"],
    ["候选 SHA-256", "0123456789abcdef0123456789abcdef"],
  ]);
});

test("missing durable record stays absent and malformed receipt entries are ignored", () => {
  assert.equal(api.summarize(null), null);
  assert.equal(api.summarize(undefined), null);
  const result = api.summarize({ status: "unknown", evidence_receipt_ids: ["id", 3, null] });
  assert.equal(result.status, "状态未知");
  assert.equal(result.compact, "完成记录：状态未知 · 回执 1 项");
  assert.deepEqual(result.details, [["验收回执", "id"]]);
});


test("expanded receipt details stay bounded while preserving the total count", () => {
  const receipts = Array.from({ length: 12 }, (_, index) => `receipt-${index}`);
  const result = api.summarize({ status: "accepted", evidence_receipt_ids: receipts });
  assert.match(result.compact, /回执 12 项/);
  assert.match(result.details[0][1], /等 12 项/);
  assert.equal((result.details[0][1].match(/receipt-/g) || []).length, 8);
});
