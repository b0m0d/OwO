/* Pure projections of host facts. No HTTP, synthetic stages, or independent task store. */
(function (root) {
  "use strict";
  function category(session, facts = {}) {
    const pending = (facts.approvals || []).some(item => item.sessionId === session.id);
    const active = facts.activeTurns?.get(session.id);
    if (pending || (facts.questionSessionId === session.id) || /^(waiting|asking)$/.test(active?.phase || "")) return "waiting";
    if (active || session.active_turn_id || session.facts?.outcome === "running") return "running";
    return session.mode === "chat" ? "chat" : "history";
  }
  function matches(session, filter, facts) {
    if (!filter) return true;
    if (filter === "all") return session.mode !== "chat";
    return category(session, facts) === filter;
  }
  function toolKind(tool) {
    if (/^(run_command|exec_command|shell|terminal|execute_command)$/.test(tool || "")) return "terminal";
    if (/^(write_file|edit_file|apply_patch|delete_file)$/.test(tool || "")) return "write";
    if (/^(read_file|list_files|list_directory|list_dir|search|grep|glob|search_files)$/.test(tool || "")) return "read";
    if (/plan/.test(tool || "")) return "plan";
    return "tool";
  }
  function completion(facts = {}) {
    const record = facts.lastCompletionRecord;
    const labels = { response_complete: "回答完成", candidate: "候选待验收", accepted: "验收通过", unverified: "尚未验证", blocked: "执行受阻", aborted: "已取消" };
    const status = record?.status || facts.lastCompletionStatus;
    const receipts = Array.isArray(record?.evidence_receipt_ids) ? record.evidence_receipt_ids.filter(x => typeof x === "string" && x) : [];
    const label = status === "accepted" && (!record || !receipts.length) ? "回答完成 · 未验证" : labels[status];
    return { label: label || (facts.outcome === "failed" ? "执行失败" : facts.outcome === "cancelled" ? "已取消" : "尚无完成记录"),
      hint: receipts.length ? `宿主记录包含 ${receipts.length} 项验收回执，请在成果页核对来源。` : "尚无验收回执；请检查实际输出与验证依据。" };
  }
  const api = { category, matches, toolKind, completion };
  root.OwoTaskWorkbench = Object.freeze(api);
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})(typeof window !== "undefined" ? window : globalThis);
