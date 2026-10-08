/* One bounded, demand-timed refresh scheduler for the active workbench entry. */
(function (global) {
  "use strict";

  function createScheduler(plans, options = {}) {
    const doc = options.document || global.document;
    const now = options.now || Date.now;
    const schedule = options.setTimeout || global.setTimeout;
    const cancel = options.clearTimeout || global.clearTimeout;
    const limit = Math.max(1, options.concurrency || 3);
    const jobs = plans.map(plan => ({ ...plan, next: 0, pending: false }));
    let active = false, timer = null, running = 0;
    const queue = [];
    const stats = { started: 0, failed: 0, hiddenSkipped: 0, peakConcurrent: 0 };
    const isHidden = typeof options.isHidden === "function"
      ? options.isHidden
      : () => doc.hidden || doc.body?.classList.contains("desktop-background");
    const visible = () => !isHidden();

    function scheduleNextTick() {
      if (timer !== null) cancel(timer);
      timer = null;
      if (!active || !visible()) return;
      let nextDue = Infinity;
      for (const job of jobs) {
        if (!job.pending) nextDue = Math.min(nextDue, job.next);
      }
      if (!Number.isFinite(nextDue)) return;
      timer = schedule(tick, Math.max(0, nextDue - now()));
    }

    function pump() {
      while (active && running < limit && queue.length) {
        const job = queue.shift();
        if (!visible()) { job.pending = false; stats.hiddenSkipped++; continue; }
        running++; stats.started++; stats.peakConcurrent = Math.max(stats.peakConcurrent, running);
        Promise.resolve().then(() => job.refresh()).catch(() => { stats.failed++; }).finally(() => {
          running--; job.pending = false; pump(); scheduleNextTick();
        });
      }
    }

    function tick() {
      timer = null;
      if (!active || !visible()) return;
      const time = now();
      for (const job of jobs) {
        if (job.pending || time < job.next) continue;
        job.next = time + job.intervalMs; job.pending = true; queue.push(job);
      }
      pump();
      scheduleNextTick();
    }

    function wake() {
      if (!active || !visible()) return;
      if (timer !== null) cancel(timer);
      timer = null;
      tick();
    }

    return {
      start() {
        if (active) return;
        active = true;
        for (const job of jobs) job.next = now() + job.intervalMs;
        doc.addEventListener("visibilitychange", wake);
        scheduleNextTick();
      },
      stop() {
        active = false;
        if (timer !== null) cancel(timer);
        timer = null;
        for (const job of queue) job.pending = false;
        queue.length = 0;
        doc.removeEventListener("visibilitychange", wake);
      },
      wake,
      get stats() { return { ...stats, running, queued: queue.length }; },
    };
  }

  global.OwoRefresh = Object.freeze({ createScheduler });
})(typeof window !== "undefined" ? window : globalThis);
