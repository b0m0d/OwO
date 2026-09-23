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
      .filter((record) => record && record.turn_id === turnId && Number(record.seq) > cursor)
      .sort((left, right) => Number(left.seq) - Number(right.seq));
  }

  global.OwoTurnSse = Object.freeze({ parseBlock, replayPath, eventsAfterCursor });
})(typeof window !== "undefined" ? window : globalThis);
