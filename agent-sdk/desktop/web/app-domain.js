// §12.3 任务 12：app.js 域面板层（自 app.js 机械外移，零行为变化）。
// 经典脚本全局词法环境共享（同 shell/ 先例）：载入顺序在 app.js 之前，
// 头部状态 helpers（$ / esc / api / addMessage 等）在运行期经闭包引用。

// ---------- R8 存储与恢复（/storage/* + /server/status） ----------

async function refreshServerStatus() {
  try {
    const status = await api("/server/status");
    const gate = status.shutdown_gate || {};
    const storage = status.storage || {};
    const parts = [
      `并发回合：${gate.active_turns ?? 0}/${gate.max_concurrent_turns ?? "?"}`,
      gate.shutting_down ? "服务关闭中" : "运行中",
    ];
    if (storage.read_only) {
      parts.push(`⚠️ 存储只读降级：${storage.migration_warning || "迁移失败"}`);
    } else if (storage.migration_warning) {
      parts.push(`提示：${storage.migration_warning}`);
    }
    $("serverStatusPanel").textContent = parts.join(" ｜ ");
  } catch (error) {
    $("serverStatusPanel").textContent = friendlyError(error);
  }
}

async function storageBackup() {
  try {
    const result = await api("/storage/backup", { method: "POST", body: "{}" });
    $("storageResult").textContent =
      `备份完成：${(result.size_bytes / 1024 / 1024).toFixed(2)} MB` +
      `\n保存于：${result.saved_to}` +
      `\n可用「恢复…」选择该 zip 恢复（恢复前会自动再备份）`;
    refreshServerStatus();
  } catch (error) {
    $("storageResult").textContent = friendlyError(error);
  }
}

async function storageExport() {
  try {
    const data = await api("/storage/export", { method: "POST", body: "{}" });
    const counts = data.counts || {};
    const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `owo-export-${new Date().toISOString().slice(0, 19).replace(/[:T]/g, "-")}.json`;
    link.click();
    URL.revokeObjectURL(url);
    $("storageResult").textContent =
      `导出完成：${counts.sessions ?? 0} 会话 / ${counts.audit ?? 0} 审计 / ` +
      `${counts.notes ?? 0} 笔记 / ${counts.skills ?? 0} 技能 / ${counts.workflows ?? 0} 工作流（标准 JSON 已下载）`;
  } catch (error) {
    $("storageResult").textContent = friendlyError(error);
  }
}

async function storageRestore(file) {
  if (!file) return;
  const archive_b64 = await new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const raw = reader.result;
      const bytes = typeof raw === "string" ? atob(raw.split(",")[1] || "") : raw;
      resolve(bytes);
    };
    reader.onerror = () => reject(new Error("读取备份文件失败"));
    reader.readAsDataURL(file);
  });
  if (!confirm("恢复会覆盖当前 settings/notes/skills/workflows，且 index.db 需重启生效（恢复前自动备份）。确认继续？")) return;
  try {
    const result = await api("/storage/restore", {
      method: "POST",
      body: JSON.stringify({ archive_b64 }),
    });
    $("storageResult").textContent =
      `恢复完成：${result.restored.length} 项已还原，${result.staged.length} 项暂存（index.db 重启后生效）` +
      `\n恢复前自动备份：${result.pre_backup}` +
      (result.restart_required ? "\n⚠️ 请重启核心服务使 index.db 生效" : "");
  } catch (error) {
    $("storageResult").textContent = friendlyError(error);
  }
}

async function storageClear() {
  if (!confirm("将清空全部会话/审计/笔记/记忆/自动化（技能与工作流保留）。\n再次输入 CLEAR_ALL 确认：")) return;
  const token = prompt("输入 CLEAR_ALL 以二次确认：", "");
  if (token !== "CLEAR_ALL") return;
  try {
    const result = await api("/storage/clear", {
      method: "POST",
      body: JSON.stringify({ confirm: "CLEAR_ALL" }),
    });
    $("storageResult").textContent =
      `已清空：${(result.cleared || []).join("、")}\n完整性校验：${result.integrity}`;
    refreshSessions(null);
    refreshAudit();
    refreshServerStatus();
  } catch (error) {
    $("storageResult").textContent = friendlyError(error);
  }
}

async function executePackage(pkg) {
  let variables = {};
  if (pkg.variables && pkg.variables.length) {
    const raw = prompt(`为技能包填写变量（JSON，如 {"value":"小李"}）：`, "{}");
    if (raw === null) return;
    try {
      variables = JSON.parse(raw);
    } catch (_) {
      addMessage("error", "变量 JSON 解析失败");
      return;
    }
  }
  if (!confirm(`确认执行技能包 ${pkg.name}？首次执行需要审批。`)) return;
  let highRiskAck = false;
  if (pkg.sensitivity === "high") {
    if (!confirm(`⚠ ${pkg.name} 是高敏感技能包（可能操作支付/验证码等场景），再次确认执行？`)) return;
    highRiskAck = true;
  }
  try {
    const report = await api("/learn/execute-package", {
      method: "POST",
      body: JSON.stringify({ name: pkg.name, variables, confirm: true, high_risk_ack: highRiskAck }),
    });
    if (report.ok) {
      addMessage("system", `技能包 ${pkg.name} 执行成功（${report.steps.length} 步）`);
    } else {
      addMessage("error", `技能包 ${pkg.name} 执行失败：${report.error || ""}`);
    }
    for (const step of report.steps) {
      addMessage("tool", `${step.status.toUpperCase()} ${step.node_id}（${step.action}）：${step.detail || "ok"}`, "执行步骤");
    }
  } catch (error) {
    addMessage("error", `执行失败：${error.message}`);
  }
}

async function refreshSuggestions() {
  try {
    const suggestions = await api("/proactive/suggestions");
    const list = $("suggestionList");
    list.innerHTML = "";
    for (const suggestion of suggestions) {
      const li = document.createElement("li");
      li.innerHTML = `<strong>${esc(suggestion.app_id)}</strong><span class="sub">${esc(suggestion.summary)}</span><span class="sub">${esc(suggestion.sequence.join(" → "))}</span>`;
      const actions = ["learn", "execute_once", "ignore", "mute_forever"];
      const labels = { learn: "学习", execute: "执行一次", ignore: "忽略", mute: "静默" };
      for (const action of actions) {
        const button = document.createElement("button");
        button.textContent = labels[action];
        button.addEventListener("click", async () => {
          try {
            const result = await api("/proactive/decide", {
              method: "POST",
              body: JSON.stringify({ suggestion_id: suggestion.id, action }),
            });
            await refreshSuggestions();
            if (action === "learn" && result && result.package) {
              addMessage(
                "system",
                `建议已沉淀为技能包 ${result.package.name}（变量：${(result.package.variables || []).join(",") || "无"}）`
              );
              await refreshPackages();
            }
          } catch (error) {
            addMessage("error", `建议处理失败：${error.message}`);
          }
        });
        li.appendChild(button);
      }
      list.appendChild(li);
    }
    if (!suggestions.length) list.innerHTML = '<li class="sub">暂无建议</li>';
  } catch (error) {
    $("suggestionList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function refreshAudit() {
  try {
    const params = new URLSearchParams({ limit: "50" });
    const eventFilter = $("auditEvent").value.trim();
    const toolFilter = $("auditTool").value.trim();
    const qFilter = $("auditQ").value.trim();
    if (eventFilter) params.set("event", eventFilter);
    if (toolFilter) params.set("tool", toolFilter);
    if (qFilter) params.set("q", qFilter);
    const entries = await api(`/audit?${params.toString()}`);
    const list = $("auditList");
    list.innerHTML = "";
    for (const entry of entries) {
      const li = document.createElement("li");
      const ok = entry.approved === undefined ? "" : entry.approved ? "✅" : "⛔";
      const tool = entry.tool ? ` [${esc(entry.tool)}]` : "";
      li.innerHTML = `<span class="sub">${esc((entry.ts || "").slice(0, 19).replace("T", " "))} ${esc(entry.event)} ${ok}${tool}</span><span class="sub">${esc(entry.detail || "")}</span>`;
      list.appendChild(li);
    }
    if (!entries.length) list.innerHTML = '<li class="sub">暂无审计记录</li>';
  } catch (error) {
    $("auditList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

// ---------- 会话 / 任务 ----------

async function refreshSessions(selectId) {
  try {
    await refreshSessionsImpl(selectId);
  } catch (error) {
    $("sessionList").innerHTML = `<li class="sub">会话读取失败：${esc(error.message || error)}</li>`;
  }
}

async function refreshSessionsImpl(selectId) {
  const sessions = await api("/sessions");
  const showArchived = $("showArchived").checked;
  const visible = sessions.filter((session) => showArchived || !session.archived);
  const visibleIds = new Set(visible.map((session) => session.id));
  const byParent = new Map();
  for (const session of visible) {
    const key = session.parent_id || "";
    if (!byParent.has(key)) byParent.set(key, []);
    byParent.get(key).push(session);
  }
  const roots = visible.filter(
    (session) => !session.parent_id || !visibleIds.has(session.parent_id)
  );
  const list = $("sessionList");
  list.innerHTML = "";
  const renderSession = (session, depth) => {
    const li = document.createElement("li");
    if (session.id === selectId) {
      li.className = "active";
      state.sessionId = session.id;
    }
    const badges = [];
    if (session.pinned) badges.push("📌");
    if (session.archived) badges.push("🗄");
    const updated = (session.updated_at || session.created_at || "").slice(0, 19).replace("T", " ");
    li.innerHTML = `
      <div style="margin-left:${depth * 14}px">
        <strong>${esc(session.title || session.id.slice(0, 12))} ${badges.join(" ")}</strong>
        <span class="sub">${esc(session.model)} ｜ ${esc(updated)}</span>
        <div class="inline">
          <button data-act="open">继续</button>
          <button data-act="rename">重命名</button>
          <button data-act="pin">${session.pinned ? "取消置顶" : "置顶"}</button>
          <button data-act="archive">${session.archived ? "取消归档" : "归档"}</button>
          <button data-act="fork">fork</button>
          <button data-act="rewind">回退</button>
          <button data-act="redo">重做</button>
        </div>
      </div>`;
    for (const button of li.querySelectorAll("button")) {
      button.addEventListener("click", async (event) => {
        event.stopPropagation();
        const act = button.dataset.act;
        try {
          if (act === "open") {
            await selectSession(session.id);
            return;
          }
          if (act === "rename") {
            const title = window.prompt("新标题：", session.title || "");
            if (title === null) return;
            await api(`/session/${session.id}/rename`, {
              method: "POST",
              body: JSON.stringify({ title }),
            });
          } else if (act === "pin") {
            await api(`/session/${session.id}/pin`, {
              method: "POST",
              body: JSON.stringify({ pinned: !session.pinned }),
            });
          } else if (act === "archive") {
            await api(`/session/${session.id}/archive`, {
              method: "POST",
              body: JSON.stringify({ archived: !session.archived }),
            });
          } else if (act === "fork") {
            const raw = window.prompt("在第几条消息处分叉（0 起，留空=最后一条）", "");
            if (raw === null) return;
            const parsed = parseInt(raw, 10);
            const message_index = Number.isFinite(parsed) ? parsed : 999999;
            const child = await api(`/session/${session.id}/fork`, {
              method: "POST",
              body: JSON.stringify({ message_index }),
            });
            addMessage("system", `已 fork 子会话 ${child.id}（可在列表中选择）`);
          } else if (act === "rewind") {
            const raw = window.prompt("保留前几条消息？", "");
            if (raw === null) return;
            await api(`/session/${session.id}/rewind`, {
              method: "POST",
              body: JSON.stringify({ keep: parseInt(raw, 10) || 0 }),
            });
          } else if (act === "redo") {
            await api(`/session/${session.id}/redo`, { method: "POST" });
          }
          await refreshSessions(selectId || state.sessionId);
        } catch (error) {
          addMessage("system", `操作失败：${friendlyError(error, { resource: true })}`);
        }
      });
    }
    li.addEventListener("click", () => selectSession(session.id));
    list.appendChild(li);
    for (const child of byParent.get(session.id) || []) {
      renderSession(child, depth + 1);
    }
  };
  for (const session of roots) renderSession(session, 0);
  if (!list.children.length) list.innerHTML = '<li class="sub">暂无会话</li>';
}

async function selectSession(id) {
  const selectionVersion = ++state.selectionVersion;
  state.sessionId = id;
  state.attachments = [];
  renderAttachmentChips();
  try {
    const detail = await api(`/session/${id}`);
    if (selectionVersion !== state.selectionVersion) return;
    $("messages").innerHTML = "";
    for (const message of detail.messages || []) {
      if (!message.content) continue;
      addMessage(message.role === "user" ? "user" : "assistant", message.content);
    }
    addMessage("system", `已恢复会话：${detail.title || id.slice(0, 12)}`);
  } catch (error) {
    if (selectionVersion !== state.selectionVersion) return;
    $("messages").innerHTML = "";
    addMessage("system", `会话加载失败：${friendlyError(error, { resource: true })}`);
  }
  await refreshSessions(id);
  await refreshDiff(id);
  await refreshSessionContext(id);
}

// ---------- 会话上下文仪表（v0.5.7，对标 Codex 上下文状态显示） ----------

async function refreshSessionContext(sessionId) {
  const bar = $("contextBar");
  if (!sessionId) {
    bar.classList.add("hidden");
    return;
  }
  try {
    const ctx = await api(`/session/${sessionId}/context`);
    const fill = $("contextFill");
    const ratio = ctx.token_budget > 0 ? ctx.estimated_tokens / ctx.token_budget : 0;
    fill.style.width = `${Math.min(100, Math.round(ratio * 100))}%`;
    fill.className = ratio > 1 ? "over" : ratio > 0.8 ? "warn" : "";
    const rulesBadge = ctx.rules_injected ? "规则注入 ✅" : "无规则";
    const compactionBadge = ctx.last_compaction ? "已压缩" : "未压缩";
    $("contextLabel").textContent =
      `上下文 ${ctx.messages} 条 ｜ 估算 ${ctx.estimated_tokens}/${ctx.token_budget} tokens` +
      ` ｜ ${rulesBadge} ｜ ${compactionBadge}` +
      (ctx.compaction_enabled ? "" : "（压缩关闭）");
    const compactionSummary = ctx.last_compaction || "";
    bar.title = compactionSummary
      ? `最近压缩摘要：\n${compactionSummary.slice(0, 300)}`
      : "会话上下文状态";
    bar.classList.remove("hidden");
  } catch (error) {
    $("contextLabel").textContent = friendlyError(error, { resource: true });
    bar.title = "会话上下文状态";
    bar.classList.remove("hidden");
  }
}

async function newSession() {
  const workspace = state.workspaceRoot || localStorage.getItem("owo.workspace") || "";
  if (!workspace) {
    alert("请先在项目页选择工作区");
    return;
  }
  state.workspaceRoot = workspace;
  localStorage.setItem("owo.workspace", workspace);
  const session = await api("/session", {
    method: "POST",
    body: JSON.stringify({ workspace }),
  });
  await selectSession(session.id);
  addMessage("system", `已创建会话 ${session.id}`);
}

// ---------- 对话（SSE） ----------

function parseSseBlock(block) {
  let event = "message";
  let data = "";
  for (const line of block.split("\n")) {
    if (line.startsWith("event:")) event = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  return { event, data };
}

async function sendPrompt() {
  if (state.reading || !state.sessionId) {
    if (!state.sessionId) addMessage("system", "请先新建或选择一个会话");
    return;
  }
  const prompt = $("prompt").value.trim();
  if (!prompt) return;
  $("prompt").value = "";
  addMessage("user", prompt);
  const attachments = state.attachments.map((attachment) => attachment.id);
  if (attachments.length) {
    addMessage("system", `附带 ${attachments.length} 个附件`);
  }
  const streaming = addMessage("assistant", "");
  let assistantText = "";
  let finished = false;
  let reader = null;

  state.reading = true;
  state.abortController = new AbortController();
  $("abortBtn").disabled = false;
  try {
    const response = await apiClient.stream(`/session/${state.sessionId}/turn`, {
      method: "POST",
      json: { prompt, attachments },
      signal: state.abortController.signal,
    });
    if (!response.body) throw new Error("服务未返回流式响应");
    reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";

    const handleBlock = (block) => {
      const { event, data } = parseSseBlock(block);
      if (!data) return;
      let payload;
      try {
        payload = JSON.parse(data);
      } catch (_) {
        return;
      }
      switch (event) {
        case "token_delta":
          assistantText += payload.delta || "";
          streaming.innerHTML = renderMarkdown(assistantText);
          bindCopyButtons(streaming);
          $("messages").scrollTop = $("messages").scrollHeight;
          break;
        case "progress":
          addMessage("system", `[${payload.message || "处理中"}]`);
          break;
        case "tool_use":
          addMessage("tool", `▶ ${payload.tool}`, "工具调用");
          break;
        case "tool_result":
          addMessage(
            "tool",
            payload.ok ? `✔ ${payload.tool}` : `✘ ${payload.tool}：${payload.error || ""}`,
            "工具结果",
          );
          break;
        case "permission_request":
          showApproval(payload);
          break;
        case "final":
          assistantText = payload.text || assistantText;
          streaming.innerHTML = renderMarkdown(assistantText);
          bindCopyButtons(streaming);
          finished = true;
          break;
        case "compaction":
          addMessage("system", `上下文已压缩：${payload.summary}`);
          break;
      }
    };

    const consumeBlocks = (text, flush) => {
      const blocks = text.split(/\r?\n\r?\n/);
      const remainder = flush ? "" : blocks.pop() || "";
      for (const block of blocks) handleBlock(block);
      return remainder;
    };

    while (true) {
      const { done, value } = await reader.read();
      if (done) {
        buffer += decoder.decode();
        if (buffer.trim()) consumeBlocks(buffer, true);
        buffer = "";
        break;
      }
      buffer += decoder.decode(value, { stream: true });
      buffer = consumeBlocks(buffer, false);
    }
    if (!finished && !assistantText) streaming.remove();
    hideApproval();
    // §4.3 状态条判据：正常收尾=已完成（中断/失败在 catch 里各自归类）。
    state.lastTurnOutcome = finished ? "completed" : "failed";
    state.attachments = [];
    renderAttachmentChips();
    await refreshSessions(state.sessionId);
    await refreshDiff(state.sessionId);
    await refreshSessionContext(state.sessionId);
  } catch (error) {
    if (!assistantText) streaming.remove();
    if (error.name !== "AbortError") {
      state.lastTurnOutcome = "failed";
      addMessage("error", `回合失败：${error.message}`);
    } else {
      state.lastTurnOutcome = "cancelled";
      addMessage("system", "已中断回合");
    }
  } finally {
    reader?.releaseLock();
    hideApproval();
    state.reading = false;
    state.abortController = null;
    $("abortBtn").disabled = true;
  }
}

function renderAttachmentChips() {
  const container = $("attachmentChips");
  container.innerHTML = "";
  for (const attachment of state.attachments) {
    const chip = document.createElement("span");
    chip.className = "chip";
    chip.textContent = `${attachment.name} ×`;
    chip.title = attachment.mime || "attachment";
    chip.addEventListener("click", () => {
      state.attachments = state.attachments.filter(
        (item) => item.id !== attachment.id
      );
      renderAttachmentChips();
    });
    container.appendChild(chip);
  }
}

async function uploadAttachments(files) {
  if (!state.sessionId) {
    addMessage("system", "请先新建或选择一个会话，再添加附件");
    return;
  }
  for (const file of files) {
    try {
      const dataUrl = await new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(reader.result);
        reader.onerror = () => reject(reader.error);
        reader.readAsDataURL(file);
      });
      const comma = String(dataUrl).indexOf(",");
      const dataB64 = comma >= 0 ? String(dataUrl).slice(comma + 1) : String(dataUrl);
      const uploaded = await api(`/session/${state.sessionId}/attachments`, {
        method: "POST",
        body: JSON.stringify({
          name: file.name,
          mime: file.type || "application/octet-stream",
          data_b64: dataB64,
        }),
      });
      state.attachments.push({ id: uploaded.id, name: uploaded.name, mime: uploaded.mime });
      renderAttachmentChips();
    } catch (error) {
      addMessage("system", `附件上传失败 ${file.name}：${error.message || error}`);
    }
  }
  $("attachmentInput").value = "";
}

// ---------- 审批条 ----------

function showApproval(payload) {
  state.pendingApproval = payload.request_id;
  state.pendingTool = payload.tool;
  state.pendingReason = payload.reason || "";
  $("approvalExplain").textContent = describeApproval(payload);
  $("approvalText").textContent = `需要审批：${payload.tool}（${payload.reason || ""}）`;
  // §5.5：原始 JSON 只在开发者详情中展开；破坏性操作不提供「始终允许」。
  const rawJson = document.getElementById("approvalRawJson");
  if (rawJson) rawJson.textContent = JSON.stringify(payload.args ?? {}, null, 2);
  const destructive = payload.level !== "read" || ["desktop_type", "desktop_click", "desktop_key", "desktop_shortcut", "clipboard", "text.inject", "desktop_launch", "desktop_activate", "desktop_scroll"].includes(payload.tool);
  const alwaysBtn = document.getElementById("alwaysAllowBtn");
  if (alwaysBtn) alwaysBtn.classList.toggle("hidden", destructive || payload.tool !== "read_file" && payload.tool !== "list_dir" && payload.tool !== "search_files" && payload.tool !== "screen_ocr");
  const rawDetails = document.getElementById("approvalRaw");
  if (rawDetails && destructive) rawDetails.open = false;
  $("approvalBar").classList.remove("hidden");
}

// §5.5：可解释展示——“将做什么 / 影响哪里 / 能否撤销”。
function describeApproval(payload) {
  const tool = payload.tool || "";
  const args = payload.args || {};
  const redacted = payload.redactedArgs || args;
  const path = typeof args.path === "string" ? args.path : "";
  const command = typeof args.command === "string" ? args.command : "";
  const url = typeof args.url === "string" ? args.url : "";
  const parts = [];
  if (tool === "write_file") {
    parts.push(`将写入：${path || "?"}`);
    parts.push("影响：目标文件；可通过 diff/回滚还原");
    parts.push("能否撤销：可以");
  } else if (tool === "read_file") {
    parts.push(`将读取：${path || "?"}`);
    parts.push("影响：只读，不修改工作区");
    parts.push("能否撤销：无需撤销");
  } else if (tool === "run_command") {
    parts.push(`将执行命令：${command || "?"}`);
    parts.push("影响：工作区内运行，可能产生改动");
    parts.push("能否撤销：视命令而定，可用 diff/快照还原");
  } else if (tool.startsWith("browser_") && url) {
    parts.push(`将联网访问：${url}`);
    parts.push("影响：向该域名发送请求（可能上传数据）");
    parts.push("能否撤销：联网动作本身不可撤销");
  } else {
    const summary = JSON.stringify(redacted, null, 2);
    parts.push(`参数：${summary}`);
    parts.push("影响：约见工具风险说明");
  }
  if (payload.riskNote) parts.push(`风险：${payload.riskNote}`);
  parts.push(`是否可撤销：${payload.undoable === false ? "否" : "视情况"}`);
  return parts.join("\n");
}

function hideApproval() {
  state.pendingApproval = null;
  state.pendingTool = null;
  state.pendingReason = null;
  $("approvalBar").classList.add("hidden");
}

async function respondApproval(scope) {
  if (!state.pendingApproval) return;
  const requestId = state.pendingApproval;
  const allow = scope !== "deny";
  hideApproval();
  try {
    const body = { allow };
    if (scope !== "once" && scope !== "deny") body.scope = scope;
    await api(`/session/${state.sessionId}/permission/${requestId}`, {
      method: "POST",
      body: JSON.stringify(body),
    });
    if (allow) {
      const label = { once: "本次", session: "此会话", one_hour: "此项目一小时", always_readonly: "始终允许此只读动作" }[scope] || "本次";
      addMessage("system", scope === "once" ? "已允许该操作" : `已允许该操作（${label}）`);
    } else {
      addMessage("system", "已拒绝该操作");
    }
  } catch (error) {
    addMessage("error", `审批失败：${error.message}`);
  }
}

// ---------- diff 审阅 ----------

async function refreshDiff(sessionId) {
  const list = $("diffList");
  list.innerHTML = "";
  if (!sessionId) return;
  try {
    const diffs = await api(`/session/${sessionId}/diff`);
    for (const diff of diffs) {
      const li = document.createElement("li");
      li.className = "diff-item";
      const changed = diff.before != null && diff.after != null ? "修改" : diff.after != null ? "新增" : "删除";
      const marker = changed === "删除" ? "🗑" : changed === "新增" ? "➕" : "✏️";
      li.innerHTML = `<strong>${marker} ${esc(diff.path)}</strong><span class="sub">${changed}</span>`;
      const body = document.createElement("pre");
      body.className = "diff-body hidden";
      body.textContent = diffText(diff);
      li.appendChild(body);
      li.addEventListener("click", (event) => {
        if (event.target.tagName === "BUTTON") return;
        body.classList.toggle("hidden");
      });
      list.appendChild(li);
    }
    if (!diffs.length) list.innerHTML = '<li class="sub">暂无改动</li>';
  } catch (_) {
    list.innerHTML = '<li class="sub">无会话或读取失败</li>';
  }
}

// 生成行级 diff 文本（统一格式，参照 git diff 风格）。
function diffText(diff) {
  const beforeLines = diff.before != null ? diff.before.split("\n") : [];
  const afterLines = diff.after != null ? diff.after.split("\n") : [];
  const lines = [];
  const maxLen = Math.max(beforeLines.length, afterLines.length);
  for (let index = 0; index < maxLen; index++) {
    const before = index < beforeLines.length ? beforeLines[index] : null;
    const after = index < afterLines.length ? afterLines[index] : null;
    if (before === null) lines.push(`+ ${after}`);
    else if (after === null) lines.push(`- ${before}`);
    else if (before !== after) {
      lines.push(`- ${before}`);
      lines.push(`+ ${after}`);
    } else {
      lines.push(`  ${before}`);
    }
  }
  return lines.join("\n");
}

async function revertAll() {
  if (!state.sessionId) return;
  if (!confirm("确定回滚当前会话全部写操作？")) return;
  await api(`/session/${state.sessionId}/revert`, { method: "POST" });
  await refreshDiff(state.sessionId);
  addMessage("system", "已回滚全部改动");
}

// ---------- 技能中心 ----------

async function refreshSkills() {
  try {
    const skills = await api("/skills");
    const list = $("skillList");
    list.innerHTML = "";
    for (const skill of skills) {
      const li = document.createElement("li");
      const enabled = skill.enabled !== false;
      li.innerHTML = `<strong>${esc(skill.name)} ${enabled ? "" : "🔇"}</strong><span class="sub">${esc(skill.description || "")}</span>`;
      const actions = document.createElement("div");
      actions.className = "inline";
      const toggle = document.createElement("button");
      toggle.textContent = enabled ? "禁用" : "启用";
      toggle.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          await api(`/skills/${encodeURIComponent(skill.name)}/enabled`, {
            method: "POST",
            body: JSON.stringify({ enabled: !enabled }),
          });
          await refreshSkills();
          addMessage("system", `技能 ${skill.name} 已${enabled ? "禁用" : "启用"}（即时生效）`);
        } catch (error) {
          addMessage("system", `操作失败：${friendlyError(error, { resource: true })}`);
        }
      });
      const view = document.createElement("button");
      view.textContent = "查看";
      view.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          const detail = await api(`/skills/${encodeURIComponent(skill.name)}`);
          $("skillDetail").textContent = detail.content || "";
          addMessage("system", `技能 ${skill.name}：${detail.path}`);
        } catch (error) {
          addMessage("system", `查看失败：${friendlyError(error, { resource: true })}`);
        }
      });
      const edit = document.createElement("button");
      edit.textContent = "编辑";
      edit.addEventListener("click", async (event) => {
        event.stopPropagation();
        try {
          const detail = await api(`/skills/${encodeURIComponent(skill.name)}`);
          const content = window.prompt(
            `编辑 ${skill.name} 的 SKILL.md：`,
            detail.content || ""
          );
          if (content === null) return;
          await api(`/skills/${encodeURIComponent(skill.name)}`, {
            method: "POST",
            body: JSON.stringify({ content }),
          });
          addMessage("system", `技能 ${skill.name} 已保存（注册表内技能重启核心服务后生效）`);
        } catch (error) {
          addMessage("system", `编辑失败：${error.message || error}`);
        }
      });
      actions.appendChild(toggle);
      actions.appendChild(view);
      actions.appendChild(edit);
      li.appendChild(actions);
      list.appendChild(li);
    }
    if (!skills.length) list.innerHTML = '<li class="sub">暂无技能</li>';
  } catch (error) {
    $("skillList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

// ---------- 情景记忆 ----------

async function refreshObservations() {
  try {
    const data = await api("/memory/observations?limit=30");
    const list = $("observationList");
    list.innerHTML = "";
    for (const observation of data.observations || []) {
      const li = document.createElement("li");
      li.innerHTML =
        `<strong>${esc(observation.app_id)}｜${esc(observation.kind)}</strong>` +
        `<span class="sub">${esc(observation.summary)}</span>` +
        `<span class="sub">${esc((observation.ts || "").slice(0, 19).replace("T", " "))}</span>`;
      list.appendChild(li);
    }
    if (!(data.observations || []).length) list.innerHTML = '<li class="sub">暂无观察记录</li>';
  } catch (error) {
    $("observationList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function recallMemory() {
  const query = $("recallQ").value.trim();
  if (!query) return;
  try {
    const data = await api(`/memory/recall?q=${encodeURIComponent(query)}&top_k=8`);
    const list = $("recallList");
    list.innerHTML = "";
    for (const hit of data.hits || []) {
      const li = document.createElement("li");
      const score = hit.confidence != null ? `（${(hit.confidence * 100).toFixed(0)}%）` : "";
      li.innerHTML =
        `<strong>${esc(hit.app_id || "")} ${score}</strong>` +
        `<span class="sub">${esc(hit.summary || "")}</span>` +
        `<span class="sub">${esc((hit.ts || "").slice(0, 19).replace("T", " "))}</span>`;
      list.appendChild(li);
    }
    if (!(data.hits || []).length) list.innerHTML = '<li class="sub">无匹配结果</li>';
  } catch (error) {
    $("recallList").innerHTML = `<li class="sub">检索失败：${esc(error.message)}</li>`;
  }
}

// ---------- 技能健康度 ----------

async function refreshSkillHealth() {
  try {
    const data = await api("/skills/health");
    const container = $("healthList");
    container.innerHTML = "";
    const skills = data.skills || [];
    for (const skill of skills) {
      const row = document.createElement("div");
      row.className = "health-row";
      const stateBadge = skill.state === "active" ? "✅" : skill.state === "degraded" ? "⚠️" : "⛔";
      row.innerHTML =
        `<span>${stateBadge} <strong>${esc(skill.name)}</strong> ` +
        `${esc(skill.state)}（${skill.successes}/${skill.attempts}，成功率 ${(skill.success_rate * 100).toFixed(0)}%）` +
        ` ｜ 连续失败 ${skill.consecutive_failures} ｜ 模板命中 ${(skill.template_hit_rate * 100).toFixed(0)}%</span>`;
      container.appendChild(row);
    }
    if (!skills.length) container.textContent = "暂无技能健康度数据";
  } catch (error) {
    $("healthList").textContent = friendlyError(error);
  }
}

// ---------- Eval 评估 ----------

async function runEval() {
  const suiteId = $("evalSuite").value;
  const button = $("evalRunBtn");
  button.disabled = true;
  button.textContent = "运行中…";
  $("evalReport").textContent = "评估运行中（调用真实模型，请稍候）…";
  try {
    const report = await api("/eval/run", {
      method: "POST",
      body: JSON.stringify({ suite_id: suiteId }),
    });
    const lines = [
      `套件：${report.suite}`,
      `通过率：${report.passed}/${report.total}（${(report.pass_rate * 100).toFixed(1)}%）`,
      `总耗时：${(report.total_duration_ms / 1000).toFixed(1)}s`,
      "",
    ];
    for (const caseResult of report.cases || []) {
      lines.push(
        `${caseResult.passed ? "✅" : "❌"} ${caseResult.name}（${(caseResult.duration_ms / 1000).toFixed(1)}s，${caseResult.steps} 步）${caseResult.error ? `：${caseResult.error}` : ""}`
      );
    }
    $("evalReport").textContent = lines.join("\n");
  } catch (error) {
    $("evalReport").textContent = `评估失败：${error.message}`;
  } finally {
    button.disabled = false;
    button.textContent = "运行";
  }
}

// ---------- Traces 可观测（v0.5.6） ----------

async function refreshTraces() {
  try {
    const data = await api("/traces");
    const list = $("traceList");
    list.innerHTML = "";
    const traces = data.traces || [];
    for (let index = 0; index < traces.length; index++) {
      const trace = traces[index];
      const li = document.createElement("li");
      const final = trace.has_final ? "✅" : "—";
      const usage = trace.usage && trace.usage.total_tokens ? ` ｜ ${trace.usage.total_tokens} tokens` : "";
      li.innerHTML =
        `<strong>${index} ${esc(trace.prompt_preview || trace.prompt || "")}</strong>` +
        `<span class="sub">${final} ${(trace.duration_ms / 1000).toFixed(1)}s ｜ ${trace.steps} 步 ｜ ${esc(trace.model)}${usage}</span>` +
        `<span class="sub">${esc((trace.started_at || "").slice(0, 19).replace("T", " "))}</span>`;
      li.addEventListener("click", () => showTrace(index));
      list.appendChild(li);
    }
    if (!traces.length) list.innerHTML = '<li class="sub">暂无轨迹（完成回合后自动记录）</li>';
  } catch (error) {
    $("traceList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function showTrace(index) {
  try {
    const trace = await api(`/traces/${index}`);
    const lines = [
      `prompt：${trace.prompt}`,
      `model：${trace.model} ｜ ${trace.duration_ms}ms ｜ ${trace.steps} 步`,
      `final：${trace.final_text || "（无）"}`,
      `usage：${JSON.stringify(trace.usage || {})}`,
      "",
      "── 事件回放 ──",
    ];
    for (const event of trace.events || []) {
      const type = event.type || "?";
      if (type === "model_call") lines.push("· 模型调用");
      else if (type === "token_delta") lines.push(`· token：${(event.delta || "").slice(0, 60)}`);
      else if (type === "tool_start") lines.push(`▶ 工具开始：${event.tool}`);
      else if (type === "tool_result") lines.push(`✔ 工具结果：${event.tool}${event.ok ? "" : "（失败）"}`);
      else if (type === "permission_request") lines.push(`⛔ 审批请求：${event.tool}（${event.reason || ""}）`);
      else if (type === "compaction") lines.push(`📦 上下文压缩：${event.summary}`);
      else if (type === "final") lines.push(`✔ 最终：${(event.text || "").slice(0, 120)}`);
      else lines.push(`· ${type}`);
    }
    $("traceReplay").textContent = lines.join("\n");
  } catch (error) {
    $("traceReplay").textContent = `回放失败：${friendlyError(error, { resource: true })}`;
  }
}

async function exportSession(format) {
  if (!state.sessionId) return;
  try {
    const blob = await apiClient.download(`/session/${state.sessionId}/export/${format}`);
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = `session-${state.sessionId.slice(0, 8)}.${format === "html" ? "html" : "md"}`;
    link.click();
    URL.revokeObjectURL(url);
    addMessage("system", `已导出会话为 ${format.toUpperCase()}`);
  } catch (error) {
    addMessage("error", `导出失败：${friendlyError(error, { resource: true })}`);
  }
}

// ---------- 子代理 / 项目规则 / MCP 管理（v0.5.5，对标 Codex @explore/@subagent、AGENTS.md、/mcp） ----------

async function runSubagent() {
  const prompt = $("subagentPrompt").value.trim();
  if (!prompt) {
    addMessage("system", "请输入子代理任务");
    return;
  }
  const readOnly = $("subagentMode").value === "read_only";
  const button = $("subagentRunBtn");
  button.disabled = true;
  button.textContent = "运行中…";
  $("subagentResult").textContent = "子代理执行中（调用真实模型，请稍候）…";
  try {
    const result = await api("/subagent/run", {
      method: "POST",
      body: JSON.stringify({ prompt, read_only: readOnly }),
    });
    $("subagentResult").textContent =
      `${result.read_only ? "只读探索" : "通用子代理"}完成（${(result.duration_ms / 1000).toFixed(1)}s）：\n\n${result.text}`;
  } catch (error) {
    $("subagentResult").textContent = `子代理失败：${error.message}`;
  } finally {
    button.disabled = false;
    button.textContent = "运行子代理";
  }
}

async function refreshProjectRules() {
  try {
    const data = await api("/project/rules");
    const info = $("projectRulesInfo");
    info.innerHTML = "";
    for (const rule of data.rules || []) {
      const badge = rule.exists ? (rule.injected ? "✅ 注入" : "⚠️ 未注入") : "— 不存在";
      const div = document.createElement("div");
      div.textContent = `${rule.name}：${badge}`;
      info.appendChild(div);
      if (rule.name === "AGENTS.md") {
        const editor = $("agentsEditor");
        if (!editor.dataset.seeded) {
          editor.value = rule.content || "";
          editor.dataset.seeded = "1";
        }
      }
    }
  } catch (error) {
    $("projectRulesInfo").textContent = friendlyError(error);
  }
}

async function saveAgentsRules() {
  const content = $("agentsEditor").value;
  try {
    const result = await api("/project/rules", {
      method: "POST",
      body: JSON.stringify({ content }),
    });
    addMessage("system", `已保存 AGENTS.md（${result.chars} 字符），下次会话注入生效`);
    await refreshProjectRules();
  } catch (error) {
    addMessage("error", `保存失败：${error.message}`);
  }
}

async function generateAgentsTemplate() {
  try {
    const result = await api("/project/rules/template", { method: "POST" });
    addMessage("system", `已生成 AGENTS.md 模板（${result.chars} 字符）`);
    $("agentsEditor").dataset.seeded = "0";
    await refreshProjectRules();
  } catch (error) {
    const msg = String((error && error.message) || error || "");
    if (msg.startsWith("409")) addMessage("error", "AGENTS.md 已存在，未生成模板（幂等）");
    else addMessage("error", `生成失败：${friendlyError(error)}`);
  }
}

// 任务 10 尾：MCP UI——健康状态/查看错误/重连/禁用（进程级启停不持久化，客户端跟踪状态）。
const mcpDisabledServers = new Set();
const mcpToolCounts = new Map();

async function refreshMcp() {
  try {
    const data = await api("/mcp");
    // 健康快照可选：失败时列表仍可用（仅缺状态行）。
    let health = { servers: [] };
    try {
      health = await api("/mcp/health");
    } catch (_) {}
    const healthByServer = new Map((health.servers || []).map((s) => [s.server, s]));
    const list = $("mcpList");
    list.innerHTML = "";
    for (const server of data.servers || []) {
      const li = document.createElement("li");
      const target = server.transport === "http" ? server.url : server.command;
      const h = healthByServer.get(server.name);
      const stateChip = h
        ? { Connected: '<b style="color:#2e7d32">正常</b>', Degraded: '<b style="color:#ef6c00">降级</b>', CircuitOpen: '<b style="color:#c62828">熔断</b>' }[h.state] || esc(String(h.state))
        : '<span class="sub">未知</span>';
      const errLine =
        h && h.last_error
          ? `<div class="sub" title="${esc(h.last_error)}">最近错误：${esc(h.last_error)}</div>`
          : "";
      const statsLine = h
        ? `<span class="sub">调用 ${h.total_calls ?? 0} ｜ 失败率 ${Math.round((h.failure_rate || 0) * 100)}%${h.p95_ms != null ? ` ｜ p95 ${h.p95_ms} ms` : ""}${h.circuit_open_count ? ` ｜ 熔断 ${h.circuit_open_count} 次` : ""}</span>`
        : "";
      li.innerHTML = `<strong>${esc(server.name)}</strong><span class="sub">${esc(server.transport)} ｜ ${esc(target || "")}${server.args && server.args.length ? " " + esc(server.args.join(" ")) : ""}</span>${statsLine}${errLine}`;
      const reconnectBtn = document.createElement("button");
      reconnectBtn.textContent = "重连";
      reconnectBtn.addEventListener("click", async () => {
        try {
          const result = await api("/mcp/reconnect", {
            method: "POST",
            body: JSON.stringify({ name: server.name }),
          });
          const prev = mcpToolCounts.get(server.name);
          const delta = prev == null ? `共 ${result.tools} 个工具` : `工具 ${prev} → ${result.tools}`;
          mcpToolCounts.set(server.name, result.tools);
          addMessage("system", `已重连 MCP 服务器 ${server.name}（${delta}）`);
          await refreshMcp();
        } catch (error) {
          addMessage("error", `重连失败：${friendlyError(error, { resource: true })}`);
        }
      });
      const toggleBtn = document.createElement("button");
      const disabledNow = mcpDisabledServers.has(server.name);
      toggleBtn.textContent = disabledNow ? "启用" : "禁用";
      toggleBtn.addEventListener("click", async () => {
        try {
          await api("/mcp/enabled", {
            method: "POST",
            body: JSON.stringify({ name: server.name, enabled: disabledNow }),
          });
          if (disabledNow) mcpDisabledServers.delete(server.name);
          else mcpDisabledServers.add(server.name);
          addMessage("system", `已${disabledNow ? "启用" : "禁用"} MCP 服务器 ${server.name} 的工具（进程级，重启后恢复）`);
          await refreshMcp();
        } catch (error) {
          addMessage("error", `切换失败：${friendlyError(error, { resource: true })}`);
        }
      });
      li.appendChild(reconnectBtn);
      li.appendChild(toggleBtn);
      const removeBtn = document.createElement("button");
      removeBtn.textContent = "移除";
      removeBtn.addEventListener("click", async () => {
        try {
          await api("/mcp/remove", {
            method: "POST",
            body: JSON.stringify({ name: server.name }),
          });
          await refreshMcp();
          addMessage("system", `已移除 MCP 服务器 ${server.name}`);
        } catch (error) {
          addMessage("error", `移除失败：${friendlyError(error, { resource: true })}`);
        }
      });
      li.appendChild(removeBtn);
      list.appendChild(li);
    }
    if (!(data.servers || []).length) list.innerHTML = '<li class="sub">暂无 MCP 服务器</li>';
  } catch (error) {
    $("mcpList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function addMcpServer() {
  const name = $("mcpName").value.trim();
  const transport = $("mcpTransport").value;
  const command = $("mcpCommand").value.trim();
  const url = $("mcpUrl").value.trim();
  if (!name) return;
  if (transport === "stdio" && !command) {
    addMessage("error", "stdio 传输需要填写启动命令");
    return;
  }
  if (transport === "http" && !url) {
    addMessage("error", "http 传输需要填写 URL");
    return;
  }
  try {
    const result = await api("/mcp/add", {
      method: "POST",
      body: JSON.stringify({ name, transport, command, url: url || null }),
    });
    addMessage("system", `MCP 服务器 ${name} 已连接（${result.tools} 个工具）`);
    $("mcpName").value = "";
    $("mcpCommand").value = "";
    $("mcpUrl").value = "";
    await refreshMcp();
  } catch (error) {
    addMessage("error", `添加失败：${error.message}`);
  }
}

// ---------- 白名单 ----------

async function refreshWhitelist() {
  try {
    const entries = await api("/whitelist");
    const list = $("whitelistList");
    list.innerHTML = "";
    // §12-13 约束控件：datalist 候选来自当前白名单 + 能力注册表，可搜索选择。
    const datalist = $("wlAppCandidates");
    if (datalist) {
      datalist.innerHTML = "";
      const seen = new Set();
      for (const entry of entries) {
        if (!seen.has(entry.app_id)) {
          seen.add(entry.app_id);
          const option = document.createElement("option");
          option.value = entry.app_id;
          datalist.appendChild(option);
        }
      }
      for (const app of chipOptionsCatalog().apps) {
        if (!seen.has(app.value)) {
          seen.add(app.value);
          const option = document.createElement("option");
          option.value = app.value;
          datalist.appendChild(option);
        }
      }
    }
    for (const entry of entries) {
      const li = document.createElement("li");
      li.innerHTML = `<strong>${esc(entry.name)}</strong><span class="sub">${esc(entry.app_id)} ｜ ${esc(entry.tier)} ｜ 操作:${entry.auto_ops_allowed ? "开" : "关"}</span>`;
      li.addEventListener("dblclick", async () => {
        try {
          await api("/whitelist/manage", {
            method: "POST",
            body: JSON.stringify({ action: "remove", app_id: entry.app_id }),
          });
          await refreshWhitelist();
        } catch (error) {
          addMessage("error", `白名单删除失败：${error.message || error}`);
        }
      });
      list.appendChild(li);
    }
  } catch (error) {
    $("whitelistList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

// ---------- Computer-use 任务级审批（P2，文档 7.3 语义：批准目标应用+描述+最长时长+允许动作） ----------

async function refreshComputerTasks() {
  try {
    const data = await api("/computer-use/tasks");
    const list = $("computerTaskList");
    list.innerHTML = "";
    const tasks = data.tasks || [];
    for (const task of tasks) {
      const li = document.createElement("li");
      const status = String(task.state || "unknown").toLowerCase();
      const elapsed = task.elapsed_ms != null ? `${Math.round(task.elapsed_ms / 1000)}s / ` : "";
      const cap = task.max_duration_ms != null ? `${Math.round(task.max_duration_ms / 1000)}s` : "无上限";
      li.innerHTML =
        `<strong>${esc(task.target_app || task.app || "")}：${esc(task.description || "")}</strong>` +
        `<span class="sub">${esc(status)} ｜ ${elapsed}${cap} ｜ 动作 ${esc((task.allowed_actions || []).join(",")) || "全部"}</span>`;
      if (status.startsWith("pending")) {
        const approve = document.createElement("button");
        approve.textContent = "批准";
        approve.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/approve`, { method: "POST" });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `批准失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(approve);
        const deny = document.createElement("button");
        deny.textContent = "拒绝";
        deny.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/reject`, { method: "POST" });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `拒绝失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(deny);
        const cancel = document.createElement("button");
        cancel.textContent = "取消";
        cancel.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/cancel`, { method: "POST" });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `取消失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(cancel);
      } else if (status.startsWith("running")) {
        const pause = document.createElement("button");
        pause.textContent = "暂停";
        pause.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/pause`, { method: "POST" });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `暂停失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(pause);
        const abort = document.createElement("button");
        abort.textContent = "终止";
        abort.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/cancel`, { method: "POST" });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `终止失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(abort);
      } else if (status.startsWith("paused")) {
        const resume = document.createElement("button");
        resume.textContent = "恢复";
        resume.addEventListener("click", async () => {
          try {
            await api(`/computer-use/task/${encodeURIComponent(task.id)}/resume`, { method: "POST" });
            await refreshComputerTasks();
          } catch (error) {
            addMessage("error", `恢复失败：${friendlyError(error)}`);
          }
        });
        li.appendChild(resume);
      }
      list.appendChild(li);
    }
    if (!tasks.length) list.innerHTML = '<li class="sub">暂无 computer-use 任务</li>';
  } catch (error) {
    $("computerTaskList").innerHTML = `<li class="sub">${esc(friendlyError(error))}</li>`;
  }
}

async function createComputerTask() {
  const targetApp = $("cuApp").value.trim();
  const description = $("cuDesc").value.trim();
  if (!targetApp || !description) {
    addMessage("system", "请填写目标应用与任务描述");
    return;
  }
  const maxDurationMs = (parseInt($("cuMaxDur").value, 10) || 120) * 1000;
  const allowedActions = getSelectedChips("cuActionsChips");
  try {
    const result = await api("/computer-use/task", {
      method: "POST",
      body: JSON.stringify({
        target_app: targetApp,
        description,
        max_duration_ms: maxDurationMs,
        allowed_actions: allowedActions,
      }),
    });
    addMessage("system", `已创建 computer-use 任务（${result.id}），等待任务级审批`);
    $("cuApp").value = "";
    $("cuDesc").value = "";
    $("cuMaxDur").value = "";
    setSelectedChips("cuActionsChips", []);
    await refreshComputerTasks();
  } catch (error) {
    addMessage("error", `创建失败：${friendlyError(error)}`);
  }
}

// ---------- 扩展面板（第四轮：notes / plugin-market / workflow / goal；第五轮：team / eval / observability / memory / command） ----------

// 挂载顺序（与 index.html 的 script 引入顺序一致）。七期：action-center（待我处理）
// 置于首位——日常工作台作为默认落地面板。
const PANEL_ORDER = [
  "capabilities",
  "action-center",
  "notes",
  "plugin-market",
  "workflow",
  "goal",
  "team",
  "eval",
  "observability",
  "memory",
  "command",
  "fleet",
  "workswarm",
  "project-launcher",
  "project-history",
];

function panelHelpers(root = $("panelRoot")) {
  return {
    baseUrl: API_BASE,
    root,
    get(path) {
      return api(path);
    },
    post(path, body) {
      return api(path, { method: "POST", body: JSON.stringify(body || {}) });
    },
    put(path, body) {
      return api(path, { method: "PUT", body: JSON.stringify(body || {}) });
    },
    delete(path) {
      return api(path, { method: "DELETE" });
    },
    stream(path, options = {}) {
      return apiClient.stream(path, options);
    },
    esc,
    friendlyError,
    renderMarkdown,
  };
}

let serviceReady = false;

function mountPanel(id, targetRoot = $("panelRoot")) {
  const panel = window.OwoPanels && window.OwoPanels[id];
  if (!panel || !targetRoot) return;
  for (const button of document.querySelectorAll("#panelNav button")) {
    button.classList.toggle("active", button.dataset.panel === id);
  }
  panel.mount(targetRoot, panelHelpers(targetRoot));
}

function initPanels() {
  const nav = $("panelNav");
  if (!nav) return;
  window.OwoPanels = window.OwoPanels || {};
  window.OwoPanels.baseUrl = API_BASE;
  nav.innerHTML = "";
  for (const id of PANEL_ORDER) {
    const panel = window.OwoPanels[id];
    if (!panel) continue;
    const button = document.createElement("button");
    button.textContent = panel.title || id;
    button.dataset.panel = id;
    button.addEventListener("click", () => mountPanel(id));
    nav.appendChild(button);
  }
  // 首屏只准备工具导航，不在服务就绪前隐式挂载面板。
  // 具体面板由工具抽屉或一级路由按需挂载，避免启动阶段并发触发请求。
}
