/* §6.1 恢复与并发原语：单飞恢复控制器 + 受限并发执行器。
 * 供 app.js 的启动水合与断连恢复复用；纯逻辑、可独立单测。
 */
(function (global) {
  "use strict";

  // 恢复退避序列（毫秒）：首次立即，其后逐步拉长，封顶 5s。
  const RECOVERY_BACKOFF_MS = [0, 500, 1000, 2000, 5000];

  /**
   * 单飞恢复控制器：任意来源（定时驱动、手动重试、UI 卡片）并发触发时
   * 只允许一次恢复流程在跑，其余触发合并到同一次执行；失败后按退避延迟
   * 下一次触发，成功后重置退避。
   * @param {() => Promise<void>} attemptRecovery 恢复动作（抛错视为失败）
   * @param {{ setTimeoutFn?: Function, clearTimeoutFn?: Function }} [scheduler]
   *   可选计时器注入（重构方案 3.2）：默认真实 setTimeout/clearTimeout；
   *   测试可传 fake scheduler 验证退避序列，避免依赖真实时钟与真实等待。
   */
  function createRecoveryController(attemptRecovery, scheduler) {
    if (typeof attemptRecovery !== "function") throw new TypeError("attemptRecovery 必须是函数");
    const scheduleTimer = (scheduler && typeof scheduler.setTimeoutFn === "function")
      ? scheduler.setTimeoutFn
      : setTimeout;
    const cancelTimer = (scheduler && typeof scheduler.clearTimeoutFn === "function")
      ? scheduler.clearTimeoutFn
      : clearTimeout;
    let inFlight = null;
    let attempt = 0;
    let timer = null;
    const execute = async () => {
      try {
        await attemptRecovery();
        attempt = 0; // 成功：重置退避
      } finally {
        inFlight = null;
      }
    };
    return {
      /** 触发恢复；恢复进行中或处于退避等待时合并到当前流程。 */
      trigger() {
        if (inFlight) return inFlight;
        const delay = RECOVERY_BACKOFF_MS[Math.min(attempt, RECOVERY_BACKOFF_MS.length - 1)];
        attempt += 1;
        inFlight = new Promise((resolve, reject) => {
          timer = scheduleTimer(() => {
            timer = null;
            execute().then(resolve, reject);
          }, delay);
        });
        return inFlight;
      },
      /** 取消待执行的退避计时（不中断已在跑的恢复）。 */
      cancel() {
        if (timer !== null) {
          cancelTimer(timer);
          timer = null;
        }
        inFlight = null;
      },
      /** 当前是否在恢复流程（含退避等待窗口）中。 */
      get pending() {
        return inFlight !== null;
      },
      /** 已累计的失败次数（成功后归零）。 */
      get attempts() {
        return attempt;
      },
    };
  }

  /**
   * 受限并发执行任务列表：任意任务抛错不中断整体，仅计数。
   * @param {Array<() => Promise<void>>} tasks
   * @param {number} limit 并发上限
   * @returns {Promise<{failures: number, total: number}>}
   */
  async function runWithConcurrency(tasks, limit) {
    const list = Array.isArray(tasks) ? tasks : [];
    if (list.length === 0) return { failures: 0, total: 0 };
    const width = Math.max(1, Math.min(limit || 1, list.length));
    let index = 0;
    let failures = 0;
    const worker = async () => {
      while (index < list.length) {
        const task = list[index];
        index += 1;
        try {
          await task();
        } catch (_) {
          failures += 1; // 单个面板失败不拖垮整体水合
        }
      }
    };
    await Promise.all(Array.from({ length: width }, worker));
    return { failures, total: list.length };
  }

  global.OwoRecovery = { createRecoveryController, runWithConcurrency, RECOVERY_BACKOFF_MS };
  if (typeof module !== "undefined" && module.exports) {
    module.exports = global.OwoRecovery;
  }
})(typeof window !== "undefined" ? window : globalThis);
