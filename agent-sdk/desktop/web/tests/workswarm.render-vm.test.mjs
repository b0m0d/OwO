// ============================================================================
// WorkSwarm render.js 视图模型（VM）契约测试 —— desktop/web/tests/workswarm.render-vm.test.mjs
//
// 运行：node tests/workswarm.render-vm.test.mjs（在 agent-sdk/desktop/web 下）
// 依赖：仅 node:test / node:assert / node 内置模块；无 DOM、无网络。
//
// 覆盖（四/九期 changeSet + artifact 行视图模型化拆分后的纯函数契约）：
//   - approvalBlockBanner(approvalBlock)：门控横幅只由参数驱动，无 state 依赖；
//   - changeSetsHtml(list, vm)：空态 / 待审批动作 / busy 提交锁 / conflicted
//     动作与冲突清单 / 门控横幅置顶；
//   - artifactRowHtml(a, chain, vm)：评审表单与 busy 锁、门控阻断仅禁批准、
//     返工表单仅限链内最新 Draft/Rejected（引用判定）、reviewFlash 存续。
// 这些函数在面板中经薄包装（state 切片 → vm）以原签名（list）/（a, chain）暴露，
// 本文件直接构造 vm 校验纯函数本体，防止拆分回退成对 state 的隐式依赖。
// ============================================================================
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));
const R = require(join(here, "../panels/workswarm/render.js"));

const cs = (id, over = {}) => ({
  change_set_id: id,
  role: "writer",
  step_id: "s1",
  status: "pending_review",
  changed_files: ["src/calc.rs"],
  conflicts: [],
  created_at: "2025-01-02T03:04:05Z",
  ...over,
});

const art = (over = {}) => ({
  artifact_id: "a1",
  kind: "doc",
  version: 1,
  producer: "m-writer",
  review_state: "PendingReview",
  created_at: "2025-01-01T00:00:00Z",
  ...over,
});

const vm0 = () => ({ reviewBusy: {}, reworkBusy: {}, reviewFlash: null, approvalBlock: null });

test("approvalBlockBanner：仅由 approvalBlock 参数驱动（无 state 依赖）", () => {
  assert.equal(R.approvalBlockBanner(null), "");
  assert.equal(R.approvalBlockBanner(undefined), "");
  assert.equal(R.approvalBlockBanner({ blocked: false }), "");
  const h = R.approvalBlockBanner({ blocked: true, reason: "存在待审批的 ChangeSet" });
  assert.match(h, /data-cs-approval-block="1"/);
  assert.match(h, /存在待审批的 ChangeSet/);
  // 缺 reason 时给默认文案
  assert.match(R.approvalBlockBanner({ blocked: true }), /先接受或拒绝后才能批准 Artifact/);
});

test("changeSetsHtml：空态与待审批动作（接受/拒绝/撤销 + busy 提交锁）", () => {
  assert.match(R.changeSetsHtml([], vm0()), /暂无 ChangeSet/);
  assert.match(R.changeSetsHtml(null, vm0()), /暂无 ChangeSet/);
  const h = R.changeSetsHtml([cs("cs1")], vm0());
  assert.match(h, /data-cs-act="accept"/);
  assert.match(h, /data-cs-act="reject"/);
  assert.match(h, /data-cs-act="revert"/);
  assert.match(h, /data-cs-id="cs1"/);
  // 非待审批状态：不渲染动作按钮
  const done = R.changeSetsHtml([cs("cs2", { status: "accepted" })], vm0());
  assert.ok(!done.includes('data-cs-act="accept"'));
  // busy 提交锁 → disabled；结果区展示
  const locked = R.changeSetsHtml([cs("cs1")], {
    results: { cs1: { ok: true, text: "已接受。" } },
    busy: { "cs1:accept": true },
    approvalBlock: null,
  });
  assert.match(locked, /disabled/);
  assert.match(locked, /已接受。/);
});

test("changeSetsHtml（九期）：conflicted 提供可重试动作 + 冲突清单；门控横幅置顶", () => {
  const h = R.changeSetsHtml(
    [cs("cs1", { status: "conflicted", conflicts: ["src/calc.rs"] })],
    { results: {}, busy: {}, approvalBlock: { blocked: true, reason: "冲突未处理" } }
  );
  assert.match(h, /data-cs-act="accept"/);
  assert.match(h, /data-cs-conflicts="cs1"/);
  assert.match(h, /冲突文件/);
  assert.match(h, /冲突未处理/);
  // 横幅在列表区顶部（banner 早于行出现）
  assert.ok(h.indexOf("data-cs-approval-block") < h.indexOf("owo-ws-chg-row"));
});

test("artifactRowHtml：评审表单 + busy/门控对批准按钮的锁定语义", () => {
  const a = art();
  const chain = { items: [a], approvedHead: null };
  const row = R.artifactRowHtml(a, chain, vm0());
  assert.match(row, /data-art-act="approve"/);
  assert.match(row, /data-art-act="request_changes"/);
  assert.match(row, /data-art-act="reject"/);
  assert.ok(!/disabled/.test(row), "无 busy/门控时批准不禁用");
  // reviewBusy → 三个评审按钮全锁
  const busyRow = R.artifactRowHtml(a, chain, { ...vm0(), reviewBusy: { a1: true } });
  assert.ok(/disabled/.test(busyRow));
  // 门控阻断 → 仅批准禁用并给出阻断说明，要求修改/驳回不受影响
  const blockRow = R.artifactRowHtml(a, chain, {
    ...vm0(),
    approvalBlock: { blocked: true, reason: "ChangeSet 待审批" },
  });
  assert.match(blockRow, /data-art-approve-blocked="a1"/);
  assert.match(blockRow, /title="ChangeSet 未处理：批准被门控阻断"/);
  const approveTag = blockRow.match(/data-art-act="approve"[^>]*/)[0];
  assert.match(approveTag, /disabled/);
  const requestTag = blockRow.match(/data-art-act="request_changes"[^>]*/)[0];
  assert.ok(!/disabled/.test(requestTag), "要求修改不受门控影响");
});

test("artifactRowHtml：reviewFlash 存续只渲染对应产物行", () => {
  const a = art();
  const chain = { items: [a] };
  assert.ok(!/评审已提交/.test(R.artifactRowHtml(a, chain, vm0())));
  const okRow = R.artifactRowHtml(a, chain, { ...vm0(), reviewFlash: { artifactId: "a1", ok: true, text: "评审已提交：批准（记录不可变）" } });
  assert.match(okRow, /评审已提交：批准（记录不可变）/);
  assert.match(okRow, /owo-ws-review-result sub ok/);
  const badRow = R.artifactRowHtml(a, chain, { ...vm0(), reviewFlash: { artifactId: "a1", ok: false, text: "版本已更新" } });
  assert.match(badRow, /owo-ws-review-result sub bad/);
  // flash 属于其他产物 → 不影响本行
  const other = R.artifactRowHtml(a, chain, { ...vm0(), reviewFlash: { artifactId: "a9", ok: true, text: "别的产物" } });
  assert.ok(!other.includes("别的产物"));
});

test("artifactRowHtml：返工表单仅限链内最新 Draft/Rejected（isHead 引用判定）", () => {
  // 链内最新 Draft → 有返工表单
  const head = art({ review_state: "Draft" });
  const headRow = R.artifactRowHtml(head, { items: [head] }, vm0());
  assert.match(headRow, /data-rework-go/);
  // Rejected 同样提供
  const rej = art({ review_state: "Rejected" });
  assert.match(R.artifactRowHtml(rej, { items: [rej] }, vm0()), /data-rework-go/);
  // 非最新（链中有更新版本）→ 无返工表单
  const older = art({ review_state: "Draft" });
  const newer = art({ artifact_id: "a2", version: 2, review_state: "PendingReview" });
  const mid = R.artifactRowHtml(older, { items: [older, newer] }, vm0());
  assert.ok(!mid.includes("data-rework-go"), "非链内最新 Draft 不应出现返工表单");
  // reworkBusy → 表单提交按钮锁
  const busyRow = R.artifactRowHtml(head, { items: [head] }, { ...vm0(), reworkBusy: { a1: true } });
  assert.match(busyRow, /data-rework-go="a1" disabled/);
});