/* §3.1/§3.2/§3.3/§3.4 事件失效网络（重构）：
 * - 唯一连接 /events/stream，经 api-client.openEventStream（Bearer fetch-stream）。
 *   不再使用匿名 EventSource：它无法携带 Authorization 头，也不允许把 token 放进 URL。
 * - 域版本对账：版本 ≤ 本地 → duplicateInvalidations（丢弃）；
 *   防抖窗口内同域多次失效 → coalescedInvalidations（合并）；
 *   实际执行刷新 → eventRefreshes；降级轮询 tick → pollFallbackRefreshes；
 *   隐藏窗口发生的业务刷新 → hiddenWindowRefreshes（验收目标 0，仅为 canary）。
 * - 生命周期：start/stop 幂等；stop 取消 reader（AbortController）与全部 pending timer；
 *   连续失败 ≥2 次 → Degraded（单一 pollFallback 全局调度器），恢复后自动关闭轮询。
 * - 断线指数退避重连（base×2^n，封顶 maxBackoffMs），重连带 Last-Event-ID 续传。
 * 计数全部在本状态机内部完成，业务 handler 不参与统计（§3.3）。
 */
(function (global) {
  "use strict";

  const INVALIDATE_EVENT = "invalidate";
  const DEFAULT_DEBOUNCE_MS = 300;
  const DEFAULT_RECONNECT_BASE_MS = 1000;
  const DEFAULT_MAX_BACKOFF_MS = 30000;
  const DEFAULT_POLL_INTERVAL_MS = 15000;
  const DEGRADE_AFTER_FAILURES = 2;

  function visibility() {
    return (typeof document !== "undefined" && document.visibilityState) || "visible";
  }

  /**
   * 域失效订阅器。
   * @param {{
   *   baseUrl?: string,
   *   path?: string,
   *   openStream?: (path: string, opts: {lastEventId?: number, signal: AbortSignal,
   *     onEvent: (frame: {event: string, data: string, id?: string}) => void,
   *     onOpen?: (status: number) => void}) => Promise<void>,
   *   debounceMs?: number,
   *   reconnectBaseMs?: number,
   *   maxBackoffMs?: number,
   *   pollIntervalMs?: number,
   *   now?: () => number,
   * }} options
   */
  function createDomainInvalidator(options) {
    const opts = options || {};
    const debounceMs = Number.isFinite(opts.debounceMs) ? opts.debounceMs : DEFAULT_DEBOUNCE_MS;
    const reconnectBaseMs = Number.isFinite(opts.reconnectBaseMs) ? opts.reconnectBaseMs : DEFAULT_RECONNECT_BASE_MS;
    const maxBackoffMs = Number.isFinite(opts.maxBackoffMs) ? opts.maxBackoffMs : DEFAULT_MAX_BACKOFF_MS;
    const pollIntervalMs = Number.isFinite(opts.pollIntervalMs) ? opts.pollIntervalMs : DEFAULT_POLL_INTERVAL_MS;
    const now = typeof opts.now === "function" ? opts.now : () => Date.now();
    const openStream = typeof opts.openStream === "function" ? opts.openStream : null;
    const streamPath = opts.path || "/events/stream";
    const fullUrl = String(opts.baseUrl || "").replace(/\/$/, "") + streamPath;

    let state = "stopped"; // stopped | connecting | live | degraded
    let controller = null; // AbortController（当前连接）
    let reconnectTimer = null;
    let reconnectAttempts = 0;
    let consecutiveFailures = 0;
    let pollTimer = null;
    let pollFn = null;
    let lastEventId = null;

    const handlers = new Map(); // domain -> Array<(version) => void>
    const versions = new Map(); // domain -> 本地已消费版本
    const pending = new Map(); // domain -> {timer, version}（trailing 防抖）
    const deferred = new Map(); // domain -> 隐藏期间未刷版本（可见时补刷）

    const metrics = {
      eventRefreshes: 0,
      coalescedInvalidations: 0,
      duplicateInvalidations: 0,
      pollFallbackRefreshes: 0,
      hiddenWindowRefreshes: 0,
    };

    function setState(next) {
      state = next;
    }

    function flushDomain(domain, version) {
      pending.delete(domain);
      if (visibility() === "hidden") {
        // 计划执行时窗口已隐藏：转 deferred（可见时补刷），不产生业务请求。
        deferred.set(domain, version);
        return;
      }
      metrics.eventRefreshes += 1;
      const list = handlers.get(domain) || [];
      for (const handler of list) {
        try {
          handler(version);
        } catch (_) {
          // 单个域处理器失败不影响其它域。
        }
      }
    }

    function flushDeferred() {
      if (visibility() === "hidden") return;
      for (const [domain, version] of deferred) {
        deferred.delete(domain);
        metrics.eventRefreshes += 1;
        const list = handlers.get(domain) || [];
        for (const handler of list) {
          try {
            handler(version);
          } catch (_) {
            // 忽略单点失败。
          }
        }
      }
    }

    function onVisibilityChange() {
      if (visibility() === "visible") flushDeferred();
    }

    function consume(domain, version) {
      const previous = versions.get(domain) || 0;
      if (version <= previous) {
        // §3.3：版本不大于本地 → 重复失效，丢弃并计数（不刷新）。
        metrics.duplicateInvalidations += 1;
        return;
      }
      versions.set(domain, version);
      if (debounceMs <= 0) {
        // 测试/低延迟模式：同步刷新。
        flushDomain(domain, version);
        return;
      }
      if (visibility() === "hidden") {
        // §3.4 验收：隐藏窗口不产生核心业务请求（版本保留，可见时补刷）。
        deferred.set(domain, version);
        return;
      }
      // trailing 防抖：同域短窗口内的多次失效只保留最后一次调度，
      // 窗口结束用最新版本刷一次——「一次状态变化只触发一次刷新」。
      const slot = pending.get(domain);
      if (slot) {
        clearTimeout(slot.timer);
        metrics.coalescedInvalidations += 1;
      }
      const timer = setTimeout(() => flushDomain(domain, version), debounceMs);
      pending.set(domain, { timer, version });
    }

    function handleFrame(frame) {
      if (frame && frame.id != null && frame.id !== "") {
        const parsedId = Number(frame.id);
        if (Number.isFinite(parsedId)) lastEventId = parsedId;
      }
      if (!frame || frame.event !== INVALIDATE_EVENT) return; // 进度/审批/心跳等帧不在此消费
      try {
        const outer = JSON.parse(frame.data);
        const payload = typeof outer.data === "string" ? JSON.parse(outer.data) : outer.data;
        if (payload && typeof payload.domain === "string" && typeof payload.version === "number") {
          consume(payload.domain, payload.version);
        }
      } catch (_) {
        // 无效帧忽略（心跳注释帧等）。
      }
    }

    function stopPollFallback() {
      if (pollTimer) {
        clearInterval(pollTimer);
        pollTimer = null;
      }
    }

    function startPollFallback() {
      if (pollTimer || typeof pollFn !== "function") return;
      // §3.4：Degraded 后只启用这一个全局兜底调度器；恢复后关闭。
      const tick = () => {
        metrics.pollFallbackRefreshes += 1;
        try {
          pollFn();
        } catch (_) {
          // 兜底失败等待下一 tick。
        }
      };
      tick();
      pollTimer = setInterval(tick, pollIntervalMs);
    }

    function markLive() {
      consecutiveFailures = 0;
      reconnectAttempts = 0;
      if (state !== "live") {
        setState("live");
        stopPollFallback();
        flushDeferred();
      }
    }

    function connect() {
      if (state === "stopped") return;
      if (!openStream) {
        // 无带认证流客户端（异常装配）：直接进入 Degraded，仅靠兜底调度。
        setState("degraded");
        startPollFallback();
        return;
      }
      setState(state === "degraded" ? "degraded" : "connecting");
      controller = typeof AbortController !== "undefined" ? new AbortController() : null;
      const attempt = controller;
      const promise = openStream(fullUrl, {
        lastEventId: lastEventId || undefined,
        signal: controller ? controller.signal : undefined,
        onEvent: handleFrame,
        onOpen: () => {
          if (controller === attempt) markLive();
        },
      });
      promise
        .then(() => {
          // 流正常结束（服务端关闭）：视为一次失败，按退避重连。
          if (controller !== attempt || state === "stopped") return;
          scheduleReconnect("流结束");
        })
        .catch((error) => {
          if (controller !== attempt || state === "stopped") return;
          if (error && error.name === "AbortError") return; // stop() 主动取消
          scheduleReconnect(String((error && error.message) || error));
        });
    }

    function scheduleReconnect(_reason) {
      consecutiveFailures += 1;
      if (consecutiveFailures >= DEGRADE_AFTER_FAILURES && state !== "degraded") {
        setState("degraded");
        startPollFallback();
      }
      const delay = Math.min(reconnectBaseMs * Math.pow(2, reconnectAttempts), maxBackoffMs);
      reconnectAttempts += 1;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        connect();
      }, delay);
    }

    return {
      /** 注册域处理器；返回解绑函数（面板销毁时调用）。 */
      on(domain, handler) {
        if (typeof handler !== "function") throw new TypeError("handler 必须是函数");
        if (!handlers.has(domain)) handlers.set(domain, []);
        handlers.get(domain).push(handler);
        return () => {
          const list = handlers.get(domain) || [];
          const index = list.indexOf(handler);
          if (index >= 0) list.splice(index, 1);
          if (list.length === 0) handlers.delete(domain);
        };
      },
      /** 注册 Degraded 兜底调度器（全局唯一；仅降级窗口内被调用）。 */
      setPollFallback(fn) {
        pollFn = typeof fn === "function" ? fn : null;
        if (state === "degraded") {
          stopPollFallback();
          startPollFallback();
        }
      },
      /** 建立单一带认证事件流连接（幂等）。 */
      start() {
        if (state !== "stopped") return;
        setState("connecting");
        if (typeof document !== "undefined" && document.addEventListener) {
          document.addEventListener("visibilitychange", onVisibilityChange);
        }
        connect();
      },
      /** 切断连接：取消 reader 与全部 pending timer（§3.4 生命周期闭环）。 */
      stop() {
        if (state === "stopped") return;
        setState("stopped");
        if (reconnectTimer) {
          clearTimeout(reconnectTimer);
          reconnectTimer = null;
        }
        stopPollFallback();
        for (const slot of pending.values()) clearTimeout(slot.timer);
        pending.clear();
        deferred.clear();
        if (typeof document !== "undefined" && document.removeEventListener) {
          document.removeEventListener("visibilitychange", onVisibilityChange);
        }
        if (controller) {
          controller.abort();
          controller = null;
        }
      },
      /** 当前已消费的最高版本（对账/调试）。 */
      versionOf(domain) {
        return versions.get(domain) || 0;
      },
      /** 连接状态：stopped | connecting | live | degraded。 */
      state() {
        return state;
      },
      /** §3.3 指标快照（诊断页只读，读取不改变计数）。 */
      snapshot() {
        return Object.assign({}, metrics, {
          state,
          reconnectAttempts,
          lastEventId,
        });
      },
      /** 测试钩子：直接注入一帧（绕过网络）。 */
      _emitFrame(frame) {
        handleFrame(frame);
      },
    };
  }

  global.OwoInvalidation = { createDomainInvalidator, INVALIDATE_EVENT };
  if (typeof module !== "undefined" && module.exports) {
    module.exports = global.OwoInvalidation;
  }
})(typeof window !== "undefined" ? window : globalThis);
