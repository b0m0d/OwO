import { test } from "node:test";
import assert from "node:assert/strict";
import recoveryModule from "../core/recovery.js";

const { createRecoveryController, runWithConcurrency } = recoveryModule;

// fake scheduler（重构方案 3.2）：退避类断言用注入计时器，禁止依赖真实时钟与真实等待。
function createFakeScheduler() {
  const pending = [];
  let nextId = 1;
  return {
    setTimeoutFn(cb, delay) {
      const id = nextId++;
      pending.push({ id, delay, cb });
      return id;
    },
    clearTimeoutFn(id) {
      const idx = pending.findIndex((t) => t.id === id);
      if (idx >= 0) pending.splice(idx, 1);
    },
    get delays() {
      return pending.map((t) => t.delay);
    },
    fireAll() {
      const batch = pending.splice(0, pending.length);
      for (const t of batch) t.cb();
    },
  };
}

test("单飞恢复：并发触发合并为一次执行", async () => {
  let executions = 0;
  const controller = createRecoveryController(async () => {
    executions += 1;
    await new Promise((resolve) => setTimeout(resolve, 30));
  });
  const first = controller.trigger();
  const second = controller.trigger();
  const third = controller.trigger();
  assert.equal(controller.pending, true, "恢复进行中应标记 pending");
  await Promise.all([first, second, third]);
  assert.equal(executions, 1, "并发触发必须合并到同一次恢复");
  assert.equal(controller.pending, false);
  assert.equal(controller.attempts, 0, "成功后失败计数应重置");
});

test("恢复失败按退避序列重试，成功后重置退避（注入 fake scheduler）", async () => {
  let calls = 0;
  const scheduler = createFakeScheduler();
  const controller = createRecoveryController(async () => {
    calls += 1;
    if (calls < 3) throw new Error("not ready");
  }, scheduler);
  const first = controller.trigger();
  assert.deepEqual(scheduler.delays, [0], "首次触发立即执行（0ms 退避）");
  scheduler.fireAll();
  await assert.rejects(first, /not ready/);
  assert.equal(controller.attempts, 1);
  const second = controller.trigger();
  assert.deepEqual(scheduler.delays, [500], "第二次触发应等待 500ms 退避");
  scheduler.fireAll();
  await assert.rejects(second, /not ready/);
  assert.equal(controller.attempts, 2);
  const third = controller.trigger();
  assert.deepEqual(scheduler.delays, [1000], "第三次触发应等待 1000ms 退避");
  scheduler.fireAll();
  await third; // 第三次执行成功
  assert.equal(calls, 3);
  assert.equal(controller.attempts, 0, "成功后退避必须归零");
});

test("恢复动作抛错时 trigger 返回的 promise 以同错误拒绝", async () => {
  const controller = createRecoveryController(async () => {
    throw new Error("boom");
  });
  await assert.rejects(controller.trigger(), /boom/);
});

test("受限并发：并发不超过上限，任务失败不中断整体", async () => {
  let active = 0;
  let maxActive = 0;
  const makeTask = (value) => async () => {
    active += 1;
    maxActive = Math.max(maxActive, active);
    await new Promise((resolve) => setTimeout(resolve, 5));
    active -= 1;
    if (value % 3 === 0) throw new Error("boom");
  };
  const tasks = Array.from({ length: 20 }, (_, index) => makeTask(index));
  const result = await runWithConcurrency(tasks, 5);
  assert.equal(result.total, 20);
  assert.equal(result.failures, 7, "失败任务应计数：0,3,6,9,12,15,18");
  assert.ok(maxActive <= 5, `并发不得超过 5，实际峰值 ${maxActive}`);
  assert.equal(active, 0, "所有任务必须真正跑完");
});

test("受限并发：空任务列表与宽度钳制", async () => {
  assert.deepEqual(await runWithConcurrency([], 5), { failures: 0, total: 0 });
  let runs = 0;
  const result = await runWithConcurrency([() => { runs += 1; }], 99);
  assert.equal(result.total, 1);
  assert.equal(runs, 1);
});
