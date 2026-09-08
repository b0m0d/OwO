// WorkSwarm domain helpers shared by the browser panel and Node regression tests.
// Keep this module DOM-free: it owns state normalization and run-summary rules only.
(function () {
  "use strict";

  var TEAM_STATUS_CN = {
    created: "已创建",
    running: "运行中",
    awaiting_human: "等待人节点",
    succeeded: "已完成",
    failed: "失败",
    cancelled: "已取消",
    stopping: "正在停止",
    stopped: "已停止",
  };
  var STEP_STATUS_CN = {
    pending: "等待",
    ready: "就绪",
    running: "运行中",
    succeeded: "完成",
    failed: "失败",
    aborted: "中止",
  };
  var MODE_CN = { single: "单节点", team: "接力（默认）", swarmflow: "DAG 流程" };
  var HEALTH_CN = { active: "正常", degraded: "降级", offline: "离线", fused: "熔断" };
  var PROPOSAL_STATUS_CN = { proposed: "待采纳", adopted: "已采纳", rejected: "已拒绝" };
  var TERMINAL = { succeeded: true, failed: true, cancelled: true };
  var STEP_TERMINAL = { succeeded: true, failed: true, aborted: true };
  var STEP_DEAD = { failed: true, aborted: true };
  var STEP_RETRYABLE = { failed: true, aborted: true };

  function normStatus(status) {
    var normalized = String(status || "").toLowerCase();
    return normalized === "awaitinghuman" ? "awaiting_human" : normalized;
  }

  function isDeadStatus(status) {
    return !!STEP_DEAD[normStatus(status)];
  }

  function taskBlocked(task, byId) {
    if (!task || STEP_TERMINAL[normStatus(task.status)]) return false;
    return (task.depends_on || []).some(function (dependency) {
      return byId[dependency] && isDeadStatus(byId[dependency].status);
    });
  }

  function isTerminalTeam(status) {
    return !!TERMINAL[normStatus(status)];
  }

  function isRetryableStep(task) {
    return !!task && !!STEP_RETRYABLE[normStatus(task.status)];
  }

  function retryableTasks(tasks) {
    return (tasks || []).filter(isRetryableStep);
  }

  function shouldShowRetry(tasks, teamStatus) {
    var status = normStatus(teamStatus);
    return status !== "succeeded" && status !== "cancelled" && retryableTasks(tasks).length > 0;
  }

  function buildRetryBody(stepId, note) {
    var normalizedId = String(stepId == null ? "" : stepId).trim();
    var normalizedNote = note == null ? "" : String(note).trim();
    return {
      command: "retry",
      step_id: normalizedId,
      note: normalizedNote || ("重试此节点：" + normalizedId),
    };
  }

  function computeRunSummary(input) {
    var team = (input && input.team) || {};
    var tasks = (input && input.tasks) || [];
    var byId = {};
    tasks.forEach(function (task) { byId[task.task_id] = task; });
    var counts = { total: tasks.length, succeeded: 0, failed: 0, waiting: 0, running: 0, blocked: 0 };
    var totalAttempts = 0;
    var failedStep = null;
    var runningStep = null;
    tasks.forEach(function (task) {
      var status = normStatus(task.status);
      var attempts = Number(task.attempts || 0);
      totalAttempts += attempts;
      if (status === "succeeded") counts.succeeded++;
      else if (status === "failed" || status === "aborted") {
        counts.failed++;
        if (!failedStep) failedStep = {
          task_id: task.task_id,
          role: task.role || task.worker || task.task_id,
          attempts: attempts,
          error: task.error || "",
        };
      } else if (status === "running") {
        counts.running++;
        if (!runningStep) runningStep = task;
      } else counts.waiting++;
      if (taskBlocked(task, byId)) counts.blocked++;
    });
    var interrupted = !!(input && input.interrupted);
    var rawStatus = team.status != null ? String(team.status) : "";
    var status = normStatus(rawStatus);
    var interruptedView = interrupted && !isTerminalTeam(status);
    var phase;
    if (interruptedView) phase = "已中断，可恢复";
    else if (status === "awaiting_human") phase = "等待人节点结果";
    else if (runningStep) phase = "执行：" + (runningStep.role || runningStep.worker || runningStep.task_id);
    else if (status === "succeeded") phase = "全部步骤已完成";
    else if (counts.failed > 0) phase = "已停止：存在失败步骤";
    else if (status === "running") phase = counts.total ? "调度中" : "等待任务图生成";
    else if (status === "created") phase = counts.total ? "准备启动" : "尚未开始";
    else if (status === "cancelled") phase = "已取消";
    else if (status === "stopping") phase = "正在停止（Worker 退出中）";
    else if (status === "stopped") phase = "已停止";
    else if (status === "failed") phase = "已失败";
    else phase = "—";
    return {
      statusKey: interruptedView ? "interrupted" : status,
      statusLabel: interruptedView ? "已中断（可恢复）" : TEAM_STATUS_CN[status] || rawStatus || "未知",
      phase: phase,
      counts: counts,
      totalAttempts: totalAttempts,
      failedStep: failedStep,
      artifactCount: input && input.artifactCount != null ? input.artifactCount : null,
      canRetry: shouldShowRetry(tasks, status),
    };
  }

  var api = {
    TEAM_STATUS_CN: TEAM_STATUS_CN,
    STEP_STATUS_CN: STEP_STATUS_CN,
    MODE_CN: MODE_CN,
    HEALTH_CN: HEALTH_CN,
    PROPOSAL_STATUS_CN: PROPOSAL_STATUS_CN,
    STEP_TERMINAL: STEP_TERMINAL,
    normStatus: normStatus,
    isDeadStatus: isDeadStatus,
    taskBlocked: taskBlocked,
    isTerminalTeam: isTerminalTeam,
    isRetryableStep: isRetryableStep,
    retryableTasks: retryableTasks,
    shouldShowRetry: shouldShowRetry,
    buildRetryBody: buildRetryBody,
    computeRunSummary: computeRunSummary,
  };
  var win = typeof window !== "undefined" ? window : globalThis;
  win.OwoWorkswarmDomain = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
