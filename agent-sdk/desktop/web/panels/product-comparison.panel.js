/* Product comparison lifecycle. The existing eval panel remains an engineering suite. */
(function (global) {
  "use strict";
  const ACTIVE = new Set(["queued", "running"]);
  const STATES = new Set(["queued", "running", "completed", "failed", "cancelled", "interrupted"]);
  const LABELS = { queued: "排队中", running: "执行中", completed: "已结束", failed: "运行失败", cancelled: "已取消", interrupted: "服务重启后中断" };
  const esc = value => String(value ?? "").replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  function normalizeConfig(config) {
    const repetitions = Number(config.repetitions);
    if (!Number.isInteger(repetitions) || repetitions < 1 || repetitions > 20) throw Error("重复次数必须是 1 到 20 的整数");
    if (!["reference", "live"].includes(config.execution)) throw Error("请选择参考回放或真实模型");
    if (config.category && !["code", "research", "document"].includes(config.category)) throw Error("任务分类无效");
    return { suite: "v1", execution: config.execution, modes: ["single", "workswarm"], repetitions,
      category: config.category || null, only: String(config.only || "").trim() || null };
  }
  function summarize(report) {
    const result = { single: null, team: null, paired: 0, missing: 0, duplicates: 0,
      invalid: 0, speedRatio: null, tokenRatio: null, toolRatio: null, comparable: false, reason: "尚未取得报告" };
    if (!report || !Array.isArray(report.runs)) return result;
    const groups = { single: [], multi: [] }, pairs = new Map();
    for (const run of report.runs) {
      const mode = run.key?.agent_mode;
      if (!groups[mode] || typeof run.key?.case_id !== "string" || !run.key.case_id.trim() || !Number.isInteger(run.key.repetition) || run.key.repetition < 0 || !["passed", "failed", "error", "timeout", "cancelled"].includes(run.status)) { result.invalid++; continue; }
      groups[mode].push(run);
      const key = JSON.stringify([run.key.case_id, run.key.repetition]);
      const pair = pairs.get(key) || {};
      if (pair[mode]) result.duplicates++;
      pair[mode] = run; pairs.set(key, pair);
    }
    const aggregate = runs => {
      if (!runs.length) return null;
      const knownTokens = runs.every(run => Number.isFinite(run.total_tokens) && run.total_tokens >= 0);
      const knownWall = runs.every(run => Number.isFinite(run.wall_ms) && run.wall_ms >= 0);
      const knownChecks = runs.every(run => Number.isInteger(run.checker_total) && run.checker_total > 0 && Number.isInteger(run.checker_passed) && run.checker_passed >= 0 && run.checker_passed <= run.checker_total);
      const knownCalls = runs.every(run => Number.isInteger(run.model_calls) && run.model_calls >= 0);
      const observedTools = runs.map(run => Number.isInteger(run.tool_calls) && run.tool_calls >= 0 ? run.tool_calls :
        Array.isArray(run.tool_log) && run.tool_log.length > 0 ? run.tool_log.length : null);
      const knownTools = observedTools.every(value => value !== null);
      return { attempted: runs.length, passed: runs.filter(run => run.status === "passed").length,
        meanWallMs: knownWall ? runs.reduce((sum, run) => sum + run.wall_ms, 0) / runs.length : null,
        totalTokens: knownTokens ? runs.reduce((sum, run) => sum + run.total_tokens, 0) : null,
        modelCalls: knownCalls ? runs.reduce((sum, run) => sum + run.model_calls, 0) : null,
        toolCalls: knownTools ? observedTools.reduce((sum, value) => sum + value, 0) : null,
        checkerPassed: knownChecks ? runs.reduce((sum, run) => sum + run.checker_passed, 0) : null,
        checkerTotal: knownChecks ? runs.reduce((sum, run) => sum + run.checker_total, 0) : null };
    };
    result.single = aggregate(groups.single); result.team = aggregate(groups.multi);
    const models = new Set(report.runs.map(run => run.model).filter(Boolean));
    for (const pair of pairs.values()) {
      if (pair.single && pair.multi) result.paired++;
      else result.missing++;
    }
    const fingerprints = ["suite_hash", "run_contract_sha256", "evaluator_binary_sha256", "provider_endpoint_sha256"];
    if (report.execution !== "live") result.reason = "参考回放仅检查评测链路，不能证明 Team 的真实能力或速度";
    else if (result.invalid || result.missing || result.duplicates || !result.paired || (report.pending || []).length) result.reason = "配对样本不完整或存在重复，保留失败与缺席记录";
    else if (!fingerprints.every(key => typeof report[key] === "string" && report[key].trim())) result.reason = "缺少任务、执行器或端点身份，当前仅展示诊断指标";
    else if (!report.model || models.size !== 1 || !models.has(report.model) || report.runs.some(run => !run.model)) result.reason = "两侧模型身份缺失或不一致";
    else {
      result.comparable = true;
      result.reason = "同一报告内样本完整配对；指标用于比较，综合优势仍需预先约定门槛和重复实验";
      if (result.single.meanWallMs > 0 && result.team.meanWallMs != null) result.speedRatio = result.team.meanWallMs / result.single.meanWallMs;
      if (result.single.totalTokens > 0 && result.team.totalTokens != null) result.tokenRatio = result.team.totalTokens / result.single.totalTokens;
      if (result.single.toolCalls > 0 && result.team.toolCalls != null) result.toolRatio = result.team.toolCalls / result.single.toolCalls;
    }
    return result;
  }
  function createController(api, options = {}) {
    const schedule = options.setTimeout || global.setTimeout, clear = options.clearTimeout || global.clearTimeout;
    const emit = options.onChange || (() => {}), visible = options.visible || (() => !global.document?.hidden);
    const state = { runs: [], runId: null, detail: null, error: "", submitting: false, cancelling: false, loading: false };
    let active = true, sequence = 0, listSequence = 0, timer = null, readAbort = null, failures = 0;
    const publish = () => { if (active) emit(state); };
    function setDetail(detail) {
      state.detail = detail;
      state.runs = state.runs.map(run => run.run_id === detail.run_id
        ? { ...run, status: detail.status, progress: detail.progress } : run);
    }
    const path = id => "/product-eval/runs/" + encodeURIComponent(id);
    const stop = () => { if (timer !== null) clear(timer); timer = null; };
    function queuePoll() {
      stop();
      if (!active || !state.runId || (state.detail && !ACTIVE.has(state.detail.status))) return;
      timer = schedule(async () => {
        timer = null;
        if (!active) return;
        if (visible()) await readSelected();
        else queuePoll();
      }, Math.min(15000, 1500 * (1 + failures)));
    }
    async function readSelected() {
      if (!active || !state.runId || state.cancelling) return;
      const id = state.runId, version = sequence;
      readAbort?.abort(); const controller = new AbortController(); readAbort = controller;
      state.loading = true; publish();
      try {
        const detail = await api.get(path(id), { signal: controller.signal });
        if (!active || version !== sequence || controller.signal.aborted) return;
        if (!detail || detail.run_id !== id || !STATES.has(detail.status)) throw Error("服务返回的评测运行身份或状态无效");
        setDetail(detail); state.error = ""; failures = 0;
      } catch (error) {
        if (!active || version !== sequence || controller.signal.aborted) return;
        state.error = error.message || String(error); failures++;
      } finally {
        if (active && version === sequence && !controller.signal.aborted) {
          state.loading = false; publish(); queuePoll();
        }
      }
    }
    async function refreshList() {
      const version = ++listSequence;
      try {
        const response = await api.get("/product-eval/runs");
        if (active && version === listSequence) { state.runs = response.runs || []; publish(); }
      } catch (error) {
        if (active && version === listSequence) { state.error = error.message || String(error); publish(); }
      }
    }
    const controller = {
      state,
      refreshList,
      async select(id) {
        if (!active) return;
        stop(); readAbort?.abort(); sequence++;
        state.runId = id; state.detail = null; state.error = ""; state.cancelling = false; failures = 0;
        await readSelected();
      },
      async start(config) {
        if (!active || state.submitting) return;
        const body = normalizeConfig(config), version = sequence;
        state.submitting = true; state.error = ""; publish();
        try {
          const created = await api.post("/product-eval/runs", body);
          if (!created?.run_id || !STATES.has(created.status)) throw Error("服务没有返回有效运行身份");
          if (active && version === sequence) await controller.select(created.run_id);
          if (active) await refreshList();
          return created;
        } catch (error) {
          if (active) { state.error = error.message || String(error); publish(); }
          throw error;
        } finally { state.submitting = false; publish(); }
      },
      async cancel() {
        if (!active || state.cancelling || !ACTIVE.has(state.detail?.status)) return;
        stop(); readAbort?.abort(); sequence++;
        const id = state.runId, version = sequence;
        state.loading = false;
        state.cancelling = true; publish();
        try {
          const result = await api.post(path(id) + "/cancel", {});
          if (active && version === sequence) {
            if (result?.run_id !== id || !STATES.has(result.status)) throw Error("取消响应身份或状态无效");
            stop(); setDetail({ ...state.detail, status: result.status }); publish(); queuePoll();
          }
        } catch (error) {
          if (active && version === sequence) { state.error = error.message || String(error); publish(); }
          throw error;
        } finally { if (active && version === sequence) { state.cancelling = false; publish(); queuePoll(); } }
      },
      refresh: readSelected,
      dispose() { active = false; sequence++; listSequence++; stop(); readAbort?.abort(); },
    };
    return controller;
  }
  function reportHtml(detail, sampleLimit = 50) {
    if (!detail) return '<p class="sub">选择历史运行或创建一次对比。</p>';
    const summary = summarize(detail.report);
    const metric = (value, suffix = "") => value == null ? "未知" : esc(Number(value).toFixed(suffix === " 秒" ? 2 : 0)) + suffix;
    const row = (name, key, transform = value => value) => '<tr><th>' + name + '</th>' +
      ["single", "team"].map(mode => '<td>' + metric(summary[mode] ? transform(summary[mode][key]) : null, key === "meanWallMs" ? " 秒" : "") + '</td>').join("") + '</tr>';
    let html = '<h3>' + esc(LABELS[detail.status] || detail.status) + '</h3><p>' + esc(detail.execution === "live" ? "真实模型" : "参考回放") + ' · 模型：' + esc(detail.model || detail.report?.model || "尚未取得") + '</p>';
    if (detail.progress) html += '<p>已记录样本：' + esc(detail.progress.done ?? "?") + ' / ' + esc(detail.progress.total ?? "?") + '</p>';
    if (detail.status === "cancelled") html += '<p class="sub">取消已提交；正在执行的样本可能仍在收尾，已有记录保留。</p>';
    if (detail.status === "interrupted") html += '<p class="sub">服务重启后不会自动重跑模型，可查看保留的报告或创建新运行。</p>';
    if (detail.error) html += '<p class="owo-pe-error">' + esc(detail.error) + '</p>';
    html += '<p role="status">' + esc(summary.reason) + '</p><div class="owo-pe-table"><table><thead><tr><th>指标</th><th>Single</th><th>Team</th></tr></thead><tbody>' +
      row("已尝试样本", "attempted") + row("通过样本", "passed") + row("平均耗时", "meanWallMs", value => value == null ? null : value / 1000) +
      row("模型调用", "modelCalls") + row("工具调用", "toolCalls") + row("Token 总量", "totalTokens") + row("通过检查数", "checkerPassed") + row("检查总数", "checkerTotal") +
      '</tbody></table></div><p>完整配对 ' + summary.paired + ' · 缺席 ' + summary.missing + ' · 重复 ' + summary.duplicates + '</p>';
    if (summary.speedRatio != null) html += '<p>Team / Single 耗时：' + summary.speedRatio.toFixed(2) + ' 倍</p>';
    if (summary.tokenRatio != null) html += '<p>Team / Single Token：' + summary.tokenRatio.toFixed(2) + ' 倍</p>';
    if (summary.toolRatio != null) html += '<p>Team / Single 工具调用：' + summary.toolRatio.toFixed(2) + ' 倍</p>';
    const comparison = detail.report?.comparison;
    if (comparison) {
      const verdict = comparison.enabled ? "建议启用 Team" : "暂不自动启用 Team";
      const rules = [...(comparison.alignment_guardrails || []), ...(comparison.quality_guardrails || []), ...(comparison.resource_guardrails || []), ...(comparison.rules || [])];
      html += '<section class="owo-pe-verdict"><h4>共享启用判定：' + esc(verdict) + '</h4><p>' +
        esc(comparison.sample_sufficient ? "样本量达到判定门槛" : "样本量不足，结论仅作观察") +
        '</p><ul>' + rules.map(rule => '<li>' + (rule.satisfied ? "通过" : "未通过") + ' · ' +
        esc(rule.name) + '：' + esc(rule.detail) + '</li>').join("") + '</ul></section>';
    }
    const runs = detail.report?.runs || [];
    if (runs.length) html += '<details><summary>样本、检查与交付记录</summary><p>显示 ' + Math.min(sampleLimit, runs.length) + ' / ' + runs.length + ' 条；汇总仍覆盖全部记录。</p>' + runs.slice(0, sampleLimit).map(run => '<article class="owo-pe-sample"><strong>' +
      esc(run.key?.case_id) + ' · ' + esc(run.key?.agent_mode === "multi" ? "Team" : "Single") + ' · 第 ' + esc(Number.isInteger(run.key?.repetition) ? run.key.repetition + 1 : "?") + ' 轮</strong><p>' +
      esc(run.status) + ' · ' + metric(run.wall_ms == null ? null : run.wall_ms / 1000, " 秒") +
      ' · 模型调用 ' + metric(run.model_calls) + ' · 工具调用 ' + metric(run.tool_calls == null ? (Array.isArray(run.tool_log) && run.tool_log.length ? run.tool_log.length : null) : run.tool_calls) + '</p>' +
      (run.error ? '<p class="owo-pe-error">' + esc(run.error) + '</p>' : '') +
      '<p>失败检查：' + esc((run.failed_steps || []).join("；") || "无记录") + '</p><p>交付引用：' + esc((run.artifact_refs || []).join("；") || "无记录") +
      '</p></article>').join("") + (runs.length > sampleLimit ? '<button type="button" class="owo-pe-more">再显示 50 条</button>' : '') + '</details>';
    return html;
  }
  let current = null;
  const panel = {
    id: "product-eval", title: "Single / Team 对比",
    nav() { return '<section data-panel="product-eval" class="owo-pe-panel"><h2>任务交付对比</h2><p class="sub">两种模式使用注册任务集、相同重复次数与检查器。真实模型使用宿主当前配置，运行会消耗模型额度。</p>' +
      '<form class="owo-pe-config"><label>执行方式<select name="execution"><option value="reference">参考回放（不调用模型）</option><option value="live">真实模型</option></select></label>' +
      '<label>每任务重复次数<input name="repetitions" type="number" min="1" max="20" value="1" required></label>' +
      '<label>分类<select name="category"><option value="">全部</option><option value="code">代码</option><option value="research">研究</option><option value="document">文档</option></select></label>' +
      '<label>任务名称筛选<input name="only" placeholder="留空则运行所选分类"></label><button class="owo-pe-start" type="submit">创建对比</button></form>' +
      '<div class="owo-pe-actions"><button type="button" class="owo-pe-list-refresh">刷新历史</button><button type="button" class="owo-pe-detail-refresh">刷新所选运行</button><button type="button" class="owo-pe-cancel" disabled>取消运行</button></div>' +
      '<p class="owo-pe-error" role="alert"></p><div class="owo-pe-history"></div><div class="owo-pe-report"></div></section>'; },
    mount(root, helpers = {}) {
      panel.dispose(); root.innerHTML = panel.nav();
      const section = root.querySelector(".owo-pe-panel"), form = section.querySelector("form");
      let sampleLimit = 50, renderedRun = null;
      const api = { get: (path, options) => helpers.get ? helpers.get(path, options) : global.OwoApi.get(path, options),
        post: (path, body) => helpers.post ? helpers.post(path, body) : global.OwoApi.post(path, body) };
      const controller = createController(api, { visible: () => section.isConnected && !global.document.hidden,
        onChange(state) {
          if (!section.isConnected) return;
          if (renderedRun !== state.runId) { renderedRun = state.runId; sampleLimit = 50; }
          section.querySelector(".owo-pe-start").disabled = state.submitting;
          section.querySelector(".owo-pe-cancel").disabled = state.cancelling || !ACTIVE.has(state.detail?.status);
          section.querySelector(".owo-pe-error").textContent = state.error;
          section.querySelector(".owo-pe-history").innerHTML = state.runs.map(run => '<button type="button" data-run-id="' + esc(run.run_id) + '">' + esc(run.run_id) + ' · ' + esc(LABELS[run.status] || run.status) + '</button>').join("") || '<p class="sub">暂无历史运行</p>';
          section.querySelector(".owo-pe-report").innerHTML = reportHtml(state.detail, sampleLimit);
        } });
      current = controller;
      form.addEventListener("submit", event => {
        event.preventDefault();
        const values = Object.fromEntries(new FormData(form));
        controller.start(values).catch(error => { section.querySelector(".owo-pe-error").textContent = error.message || String(error); });
      });
      section.querySelector(".owo-pe-cancel").addEventListener("click", () => controller.cancel().catch(() => {}));
      section.querySelector(".owo-pe-list-refresh").addEventListener("click", () => controller.refreshList());
      section.querySelector(".owo-pe-detail-refresh").addEventListener("click", () => controller.refresh());
      section.querySelector(".owo-pe-history").addEventListener("click", event => {
        const button = event.target.closest("[data-run-id]");
        if (button) controller.select(button.dataset.runId);
      });
      section.querySelector(".owo-pe-report").addEventListener("click", event => {
        if (event.target.closest(".owo-pe-more")) {
          sampleLimit += 50;
          section.querySelector(".owo-pe-report").innerHTML = reportHtml(controller.state.detail, sampleLimit);
          const details = section.querySelector(".owo-pe-report details");
          if (details) details.open = true;
        }
      });
      controller.refreshList();
    },
    dispose() { current?.dispose(); current = null; },
    refresh() { return current?.refresh(); },
  };
  if (global.document) { global.OwoPanels = global.OwoPanels || {}; global.OwoPanels[panel.id] = panel; }
  const exported = { normalizeConfig, summarize, createController, reportHtml, panel };
  if (typeof module !== "undefined" && module.exports) module.exports = exported;
  else global.OwoProductComparison = Object.freeze(exported);
})(typeof window !== "undefined" ? window : globalThis);
