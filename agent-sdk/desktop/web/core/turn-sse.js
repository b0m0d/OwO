/* Durable turn-stream parsing and replay helpers shared by the desktop shell. */
(function (global) {
  "use strict";

  function parseBlock(block) {
    let event = "message";
    let id = null;
    const data = [];
    for (const line of String(block || "").split(/\r?\n/)) {
      if (line.startsWith("event:")) event = line.slice(6).replace(/^ /, "");
      else if (line.startsWith("id:")) id = line.slice(3).replace(/^ /, "");
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
    }
    return { event, id, data: data.join("\n") };
  }

  function replayPath(sessionId, turnId, afterSeq, limit = 256) {
    const query = new URLSearchParams({
      turn_id: String(turnId),
      after_seq: String(afterSeq),
      limit: String(limit),
    });
    return `/session/${encodeURIComponent(sessionId)}/turn/events?${query}`;
  }

  function eventsAfterCursor(page, turnId, afterSeq) {
    const cursor = Number(afterSeq) || 0;
    return (page && Array.isArray(page.events) ? page.events : [])
      .filter((record) => record && record.turn_id === turnId && Number.isSafeInteger(Number(record.seq)) && Number(record.seq) > cursor)
      .sort((left, right) => Number(left.seq) - Number(right.seq));
  }


  const statuses = new Set(["response_complete", "candidate", "accepted", "unverified", "blocked", "aborted"]);
  function streamError(message, status = "unverified") {
    const error = new Error(message); error.name = "TurnStreamError"; error.completionStatus = status; return error;
  }
  function createCompletion() {
    let final = null, stats = null, failure = null;
    return {
      get terminal() { return Boolean(stats || failure); },
      observe(event, payload) {
        if (event === "final") final = payload;
        if (event === "turn_stats") {
          if (!statuses.has(payload.completion_status)) throw streamError("回合缺少有效的宿主完成状态");
          stats = payload;
        }
        if (event === "turn_failed") failure = payload;
      },
      finish() {
        if (failure) throw streamError(failure.message || "回合执行失败", failure.completion_status || "unverified");
        if (!stats) throw streamError("连接结束但缺少宿主完成记录；回答和改动尚未确认");
        if (!final) throw streamError("宿主已结束回合，但缺少最终回答");
        return { completionStatus: stats.completion_status, finalText: final.text || "", stats };
      },
    };
  }
  function delay(ms, signal) {
    return new Promise((resolve, reject) => {
      const abort = () => {
        clearTimeout(timer); signal?.removeEventListener("abort", abort);
        const error = new Error("Aborted"); error.name = "AbortError"; reject(error);
      };
      const timer = setTimeout(() => { signal?.removeEventListener("abort", abort); resolve(); }, ms);
      if (signal?.aborted) abort();
      else signal?.addEventListener("abort", abort, { once: true });
    });
  }
  async function consumeResponse(response, { onEvent, replay, signal, wait = delay }) {
    if (!response.body) throw streamError("服务未返回流式响应");
    const completion = createCompletion(), turnId = response.headers.get("x-owo-turn-id");
    let cursor = 0, buffer = "", streamEnded = false;
    const decoder = new TextDecoder(), encoder = new TextEncoder(), reader = response.body.getReader();
    const dispatch = (event, payload, id) => {
      const sequence = Number(id);
      if (id != null && Number.isSafeInteger(sequence) && sequence > 0 && sequence <= cursor) return;
      if (!payload || typeof payload !== "object") throw streamError("无效的回合事件");
      const type = event === "message" ? payload.type : event;
      completion.observe(type, payload);
      onEvent(type, payload);
      if (id != null && Number.isSafeInteger(sequence) && sequence > cursor) cursor = sequence;
    };
    const consume = (flush) => {
      const blocks = buffer.split(/\r?\n\r?\n/);
      buffer = flush ? "" : blocks.pop() || "";
      for (const block of blocks) {
        const { event, id, data } = parseBlock(block);
        if (!data || data === "[DONE]") continue;
        if (encoder.encode(data).length > 1024 * 1024) throw streamError("回合事件超过 1 MiB 上限");
        let payload;
        try { payload = JSON.parse(data); } catch (_) { throw streamError("回合事件格式无效"); }
        dispatch(event, payload, id);
      }
      if (buffer.length > 256 * 1024 && encoder.encode(buffer).length > 1024 * 1024) throw streamError("回合事件超过 1 MiB 上限");
    };
    try {
      while (true) {
        let chunk;
        try { chunk = await reader.read(); }
        catch (error) {
          if (signal?.aborted || error.name === "AbortError" || (!turnId && !completion.terminal)) throw error;
          break; // Recover transport failures only; protocol and callback errors propagate.
        }
        if (chunk.done) { streamEnded = true; buffer += decoder.decode(); consume(true); break; }
        buffer += decoder.decode(chunk.value, { stream: true }); consume(false);
      }
    } finally {
      if (!streamEnded) await reader.cancel().catch(() => undefined);
      reader.releaseLock();
    }
    let emptyPages = 0;
    while (!completion.terminal && turnId && replay) {
      if (signal?.aborted) { const error = new Error("Aborted"); error.name = "AbortError"; throw error; }
      const page = await replay(turnId, cursor, signal);
      let advanced = false;
      for (const record of eventsAfterCursor(page, turnId, cursor)) {
        if (Number(record.seq) <= cursor) continue;
        dispatch(record.payload?.type, record.payload, record.seq); advanced = true;
      }
      if (completion.terminal) break;
      if (advanced) { emptyPages = 0; continue; } // Drain completed turns spanning multiple pages.
      if (!page?.active) throw streamError(page?.state === "interrupted"
        ? "回合在写入完成结果前中断；已保留部分回答" : "重放结束但缺少宿主完成记录；结果尚未确认");
      await wait(Math.min(2500, 200 * (++emptyPages)), signal);
    }
    return completion.finish();
  }

  global.OwoTurnSse = Object.freeze({ parseBlock, replayPath, eventsAfterCursor, createCompletion, consumeResponse });
})(typeof window !== "undefined" ? window : globalThis);
