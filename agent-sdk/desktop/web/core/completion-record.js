(function (root) {
  "use strict";
  const statusLabels = {
    response_complete: "回答完成",
    candidate: "候选待验收",
    accepted: "验收通过",
    unverified: "未验证",
    blocked: "存在阻断",
    aborted: "已取消",
  };
  const short = (value, limit) => Array.from(String(value || "")).slice(0, limit).join("");
  function summarize(record) {
    if (!record || typeof record !== "object") return null;
    const taskId = typeof record.task_id === "string" ? record.task_id : "";
    const attemptId = typeof record.attempt_id === "string" ? record.attempt_id : "";
    const receipts = Array.isArray(record.evidence_receipt_ids)
      ? record.evidence_receipt_ids.filter((id) => typeof id === "string" && id.length > 0)
      : [];
    const candidate = typeof record.candidate_version_sha256 === "string"
      ? record.candidate_version_sha256
      : "";
    const status = statusLabels[record.status] || "状态未知";
    const details = [];
    if (taskId) details.push(["任务", taskId]);
    if (attemptId) details.push(["尝试", attemptId]);
    const shownReceipts = receipts.slice(0, 8);
    const receiptText = shownReceipts.join("、") + (receipts.length > shownReceipts.length ? ` 等 ${receipts.length} 项` : "");
    details.push(["验收回执", receiptText || "无"]);
    if (candidate) details.push(["候选 SHA-256", candidate]);
    const compact = [
      `完成记录：${status}`,
      `回执 ${receipts.length} 项`,
      candidate ? `候选 ${short(candidate, 12)}…` : "",
    ].filter(Boolean).join(" · ");
    return { status, compact, details };
  }
  root.OwoCompletionRecord = Object.freeze({ summarize });
})(globalThis);
