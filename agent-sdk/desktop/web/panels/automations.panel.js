/* 自动化面板：新建任务（间隔/每天/单次 × 提醒/跑任务）+ 任务列表（启停/执行记录/删除）+ 提醒。
 * 由工具视图的静态卡片迁移而来（此前 index.html 手写表单 + app.js 零散函数，
 * 与扩展面板体系割裂）；迁入后走统一网格排版与分区目录。
 * 纯脚本 IIFE，注册 window.OwoPanels.automations；helpers 缺失时复用统一 ApiClient。
 */
window.OwoPanels = window.OwoPanels || {};
window.OwoPanels.automations = (function () {
  "use strict";

  var id = "automations";

  function defaultHelpers() {
    var baseUrl = (window.OwoPanels && window.OwoPanels.baseUrl) || "";
    function get(path) {
      return window.OwoApi.stream(path).then(function (r) {
        if (!r.ok) throw new Error("HTTP " + r.status);
        return r.json();
      });
    }
    function esc(s) {
      return String(s == null ? "" : s)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;");
    }
    function friendlyError(e) {
      return "操作失败：" + (e && e.message ? e.message : String(e));
    }
    return {
      baseUrl: baseUrl,
      get: get,
      post: function (path, body) {
        return window.OwoApi.stream(path, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body || {}),
        }).then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return r.status === 204 ? null : r.json();
        });
      },
      call: function (path, options) {
        return window.OwoApi.stream(path, options || {}).then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return r.status === 204 ? null : r.json().catch(function () { return null; });
        });
      },
      del: function (path) {
        return window.OwoApi.stream(path, { method: "DELETE" }).then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return null;
        });
      },
      confirm: function (options) {
        return Promise.resolve(window.confirm(options.message || ""));
      },
      notify: function (message) {
        window.alert(message);
      },
      esc: esc,
      friendlyError: friendlyError,
    };
  }

  var H = defaultHelpers();
  var panelGeneration = 0;
  var taskRefreshGeneration = 0;
  var reminderRefreshGeneration = 0;
  var creatingTask = false;
  var activeCreateButton = null;
  var clearingReminders = false;
  var taskToggleBusy = Object.create(null);
  var taskDeleteBusy = Object.create(null);
  var panelMounted = false;

  function el(domId) {
    return document.getElementById(domId);
  }

  function setStatus(text, isError) {
    var status = el("owo-aut-status");
    if (!status) return;
    status.textContent = text || "";
    status.style.color = isError ? "var(--red)" : "var(--text-3)";
  }

  function setButtonBusy(button, busy, idleText, busyText) {
    if (!button) return;
    button.disabled = !!busy;
    if (busy) {
      button.setAttribute("aria-busy", "true");
      button.textContent = busyText || idleText;
    } else {
      button.removeAttribute("aria-busy");
      button.textContent = idleText;
    }
  }

  function explainActionError(action, error) {
    var detail = H.friendlyError ? H.friendlyError(error) : (error && error.message) || String(error);
    var message = action + "失败：" + detail;
    setStatus(message, true);
    if (H.notify) H.notify(message, "error");
  }

  function nav() {
    return (
      '<section data-panel="' +
      id +
      '">' +
      "<style>" +
      "#owo-aut-status{min-height:16px;font-size:12px;color:var(--text-3)}" +
      ".owo-aut-task{display:flex;flex-direction:column;gap:4px}" +
      ".owo-aut-task-actions{display:flex;gap:8px;flex-wrap:wrap}" +
      ".owo-aut-load-error{display:flex;align-items:center;justify-content:space-between;gap:12px;list-style:none;border:1px solid var(--red-soft);border-radius:8px;padding:10px 12px;background:var(--red-soft);color:var(--red)}" +
      ".owo-aut-retry{flex:none;border:1px solid currentColor;border-radius:6px;padding:5px 10px;background:var(--surface);color:inherit;cursor:pointer}" +
      ".owo-aut-runs-error{display:flex;align-items:center;justify-content:space-between;gap:8px;list-style:none;border:1px solid var(--red-soft);border-radius:8px;padding:8px 10px;background:var(--red-soft);color:var(--red)}" +
      ".owo-aut-runs-error button{flex:none;border:1px solid currentColor;border-radius:6px;padding:4px 9px;background:var(--surface);color:inherit;cursor:pointer}" +
      "</style>" +
      '<div class="stack">' +
      '<div class="sub">新建任务</div>' +
      '<div class="owo-mtr-row"><span class="sub">按「间隔 / 每天 / 单次」触发提醒或任务，到点后在会话中推送；适合周期性检查、定时汇总这类重复动作。</span></div>' +
      '<form id="owo-aut-form" class="tool-form-grid">' +
      '<div class="tool-field"><label for="owo-aut-name">任务名</label><input id="owo-aut-name" placeholder="如 每小时提醒" required></div>' +
      '<div class="tool-field"><label for="owo-aut-kind">触发方式</label><select id="owo-aut-kind">' +
      '<option value="interval">间隔（秒）</option>' +
      '<option value="daily">每天（HH:MM）</option>' +
      '<option value="oneshot">单次（RFC3339）</option>' +
      "</select></div>" +
      '<div class="tool-field" data-schedule-group="interval"><label for="owo-aut-interval">间隔秒数</label><input id="owo-aut-interval" type="number" min="1" step="1" inputmode="numeric" required></div>' +
      '<div class="tool-field" data-schedule-group="daily" hidden><label for="owo-aut-daily">每天时间</label><input id="owo-aut-daily" type="time" step="60" disabled></div>' +
      '<div class="tool-field" data-schedule-group="oneshot" hidden><label for="owo-aut-oneshot">执行时间（本地）</label><input id="owo-aut-oneshot" type="datetime-local" step="60" disabled></div>' +
      '<div class="tool-field"><label for="owo-aut-action">动作</label><select id="owo-aut-action">' +
      '<option value="reminder">提醒（到点推送一条话）</option>' +
      '<option value="run_prompt">跑只读 Agent 任务</option>' +
      "</select></div>" +
      '<div class="tool-field tool-field-full"><label id="owo-aut-content-label" for="owo-aut-content">提醒内容</label><input id="owo-aut-content" placeholder="到点显示的提醒文案" required><div id="owo-aut-action-hint" class="sub">提醒只会推送到工作台，不会调用模型。</div></div>' +
      '<div class="tool-actions tool-actions-end tool-field-full"><button type="submit" class="primary" data-core-action>创建任务</button></div>' +
      "</form>" +
      '<p class="sub" data-core-action-hint>连接并授权本地核心后可创建自动化任务。</p>' +
      '<div id="owo-aut-status"></div>' +
      '<div class="sub">任务列表</div>' +
      '<ul id="owo-aut-list" class="list"><li class="sub" role="status">加载中…</li></ul>' +
      '<div class="sub">提醒</div>' +
      '<div class="tool-actions"><button id="owo-aut-clear" data-core-action>清除提醒</button></div>' +
      '<p class="sub" data-core-action-hint>连接并授权本地核心后可管理工作台提醒。</p>' +
      '<ul id="owo-aut-reminders" class="list"><li class="sub" role="status">加载中…</li></ul>' +
      "</div>" +
      "</section>"
    );
  }

  function syncScheduleFields() {
    var kindControl = el("owo-aut-kind");
    var selected = kindControl ? kindControl.value : "interval";
    ["interval", "daily", "oneshot"].forEach(function (kind) {
      var group = document.querySelector('[data-schedule-group="' + kind + '"]');
      var input = el(kind === "interval" ? "owo-aut-interval" : kind === "daily" ? "owo-aut-daily" : "owo-aut-oneshot");
      var active = kind === selected;
      if (group) group.hidden = !active;
      if (input) {
        input.disabled = !active;
        input.required = active;
      }
    });
  }

  function formatOffset(offsetMinutes) {
    if (!Number.isInteger(offsetMinutes) || Math.abs(offsetMinutes) > 14 * 60) {
      throw new Error("本地时区偏移无效");
    }
    var sign = offsetMinutes <= 0 ? "+" : "-";
    var absolute = Math.abs(offsetMinutes);
    return sign + String(Math.floor(absolute / 60)).padStart(2, "0") + ":" +
      String(absolute % 60).padStart(2, "0");
  }

  function buildSchedule(kind, value, offsetMinutes) {
    var rawValue = String(value == null ? "" : value).trim();
    if (kind === "interval") {
      if (!/^\d+$/.test(rawValue)) throw new Error("间隔需为正整数（秒）");
      var seconds = Number(rawValue);
      if (!Number.isSafeInteger(seconds) || seconds <= 0) throw new Error("间隔需为正整数（秒）");
      return { kind: "interval", every_secs: seconds };
    }
    if (kind === "daily") {
      if (!/^(?:[01]\d|2[0-3]):[0-5]\d$/.test(rawValue)) throw new Error("每天时间需使用 HH:MM 格式");
      return { kind: "daily", time: rawValue };
    }
    if (kind === "oneshot") {
      if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(rawValue)) {
        throw new Error("请选择有效的本地日期和时间");
      }
      var localDate = new Date(rawValue);
      if (!Number.isFinite(localDate.getTime()) ||
          localDate.getFullYear() !== Number(rawValue.slice(0, 4)) ||
          localDate.getMonth() + 1 !== Number(rawValue.slice(5, 7)) ||
          localDate.getDate() !== Number(rawValue.slice(8, 10)) ||
          localDate.getHours() !== Number(rawValue.slice(11, 13)) ||
          localDate.getMinutes() !== Number(rawValue.slice(14, 16))) {
        throw new Error("请选择有效的本地日期和时间");
      }
      var offset = offsetMinutes == null ? localDate.getTimezoneOffset() : offsetMinutes;
      return { kind: "one_shot", at: rawValue + ":00" + formatOffset(offset) };
    }
    throw new Error("未知的触发方式");
  }

  /// 触发方式可读化。
  function describeSchedule(schedule) {
    if (!schedule || typeof schedule !== "object") return "";
    if (schedule.kind === "interval") return "每 " + schedule.every_secs + " 秒";
    if (schedule.kind === "daily") return "每天 " + (schedule.time || "");
    if (schedule.kind === "one_shot") {
      return "单次 " + String(schedule.at || "").replace("T", " ").slice(0, 16);
    }
    return JSON.stringify(schedule);
  }

  /// 动作类型可读化（提醒 / 跑只读任务）。
  function describeAction(action) {
    if (!action || typeof action !== "object") return "提醒";
    if (action.kind === "run_prompt") return "跑只读 Agent 任务";
    if (action.kind === "reminder") return "提醒";
    return String(action.kind || "");
  }

  function buildAction(kind, value) {
    var content = String(value == null ? "" : value).trim();
    if (!content) throw new Error("内容不能为空");
    if (kind === "reminder") return { kind: "reminder", text: content };
    if (kind === "run_prompt") return { kind: "run_prompt", prompt: content };
    throw new Error("未知的自动化动作");
  }

  function syncActionFields() {
    var action = el("owo-aut-action");
    var label = el("owo-aut-content-label");
    var content = el("owo-aut-content");
    var hint = el("owo-aut-action-hint");
    if (!action || !content) return;
    var runPrompt = action.value === "run_prompt";
    if (label) label.textContent = runPrompt ? "Agent 提示词（只读）" : "提醒内容";
    content.placeholder = runPrompt
      ? "描述要查询、巡检或总结的内容"
      : "到点显示的提醒文案";
    if (hint) hint.textContent = runPrompt
      ? "到点后调用 Agent；定时任务无人值守，只读模式，不会修改文件或执行命令。"
      : "提醒只会推送到工作台，不会调用模型。";
  }

  function renderListLoadError(list, error, retryKind) {
    if (!list) return;
    var detail = H.friendlyError ? H.friendlyError(error) : (error && error.message) || String(error);
    var message = retryKind === "tasks" ? "自动化任务加载失败：" : "提醒加载失败：";
    list.innerHTML =
      '<li class="owo-aut-load-error" role="alert"><span>' +
      H.esc(message + detail) +
      '</span><button type="button" class="owo-aut-retry" data-aut-retry="' +
      retryKind +
      '">重试</button></li>';
  }

  function refreshTasks() {
    var request = ++taskRefreshGeneration;
    var owner = panelGeneration;
    var list = el("owo-aut-list");
    if (list) list.innerHTML = '<li class="sub" role="status">正在加载自动化任务…</li>';
    return H.get("/automations")
      .then(function (tasks) {
        if (request !== taskRefreshGeneration || owner !== panelGeneration) return;
        renderTasks(Array.isArray(tasks) ? tasks : []);
      })
      .catch(function (error) {
        if (request !== taskRefreshGeneration || owner !== panelGeneration) return;
        renderListLoadError(el("owo-aut-list"), error, "tasks");
      });
  }

  function refreshReminders() {
    var request = ++reminderRefreshGeneration;
    var owner = panelGeneration;
    var list = el("owo-aut-reminders");
    if (list) list.innerHTML = '<li class="sub" role="status">正在加载提醒…</li>';
    return H.get("/automations/reminders")
      .then(function (reminders) {
        if (request !== reminderRefreshGeneration || owner !== panelGeneration) return;
        renderReminders(Array.isArray(reminders) ? reminders : []);
      })
      .catch(function (error) {
        if (request !== reminderRefreshGeneration || owner !== panelGeneration) return;
        renderListLoadError(el("owo-aut-reminders"), error, "reminders");
      });
  }

  function refresh() {
    refreshTasks();
    refreshReminders();
  }

  function bindListRetry(listId, retryKind) {
    var list = el(listId);
    if (!list) return;
    list.addEventListener("click", function (event) {
      var button = event.target && event.target.closest
        ? event.target.closest('[data-aut-retry="' + retryKind + '"]')
        : null;
      if (!button) return;
      event.preventDefault();
      if (retryKind === "tasks") refreshTasks();
      else refreshReminders();
    });
  }

  function renderTasks(tasks) {
    var list = el("owo-aut-list");
    if (!list) return;
    list.innerHTML = "";
    if (!tasks.length) {
      list.innerHTML = '<li class="sub">暂无自动化任务</li>';
      return;
    }
    tasks.forEach(function (task) {
      var li = document.createElement("li");
      li.className = "owo-aut-task";
      li.innerHTML =
        "<strong>" + H.esc(task.name) + '</strong><span class="sub">' + H.esc(describeSchedule(task.schedule)) +
        " ｜ " + H.esc(describeAction(task.action)) + " ｜ " + (task.enabled ? "启用" : "停用") + "</span>";

      var toggleBtn = document.createElement("button");
      toggleBtn.setAttribute("data-core-action", "true");
      var toggleKey = String(task.id);
      var idleText = task.enabled ? "停用" : "启用";
      setButtonBusy(toggleBtn, !!taskToggleBusy[toggleKey], idleText, "处理中…");
      toggleBtn.addEventListener("click", async function (event) {
        event.stopPropagation();
        if (taskToggleBusy[toggleKey]) return;
        taskToggleBusy[toggleKey] = true;
        var owner = panelGeneration;
        setButtonBusy(toggleBtn, true, idleText, "处理中…");
        try {
          await H.call("/automations/" + encodeURIComponent(task.id) + "/toggle", { method: "POST" });
          if (owner === panelGeneration) setStatus((task.enabled ? "已停用：" : "已启用：") + task.name, false);
        } catch (error) {
          if (owner === panelGeneration) explainActionError("自动化启停", error);
        } finally {
          delete taskToggleBusy[toggleKey];
          setButtonBusy(toggleBtn, false, idleText);
          // 重新挂载时列表可能在请求期间显示了忙碌按钮；用服务端状态刷新当前页。
          if (panelMounted) refresh();
        }
      });

      // A8-1：展开执行记录（次日可查「跑没跑、结果如何」）。
      var runsBtn = document.createElement("button");
      runsBtn.textContent = "记录";
      var runsGeneration = 0;
      var activeRunsBox = null;
      async function loadRuns(box) {
        var request = ++runsGeneration;
        var owner = panelGeneration;
        activeRunsBox = box;
        box.innerHTML = '<li class="sub" role="status">正在加载执行记录…</li>';
        try {
          var runs = await H.get(
            "/automations/runs?task_id=" + encodeURIComponent(task.id) + "&limit=10"
          );
          if (request !== runsGeneration || activeRunsBox !== box || owner !== panelGeneration) return;
          box.innerHTML = "";
          if (!Array.isArray(runs) || !runs.length) {
            box.innerHTML = '<li class="sub">尚无执行记录（到点触发后可见）</li>';
            return;
          }
          runs.forEach(function (run) {
            var item = document.createElement("li");
            var when = String(run.at || "").replace("T", " ").slice(0, 16);
            var verdict = run.status === "ok" ? "✓ 成功" : run.status === "skipped" ? "↷ 已跳过" : "✗ 失败";
            item.innerHTML =
              "<strong>" + H.esc(verdict) + " · " + H.esc(when) + '</strong><span class="sub">' +
              H.esc(String(run.output || "（无输出）").slice(0, 400)) + "</span>";
            box.appendChild(item);
          });
        } catch (error) {
          if (request !== runsGeneration || activeRunsBox !== box || owner !== panelGeneration) return;
          box.innerHTML = "";
          var errorItem = document.createElement("li");
          errorItem.className = "owo-aut-runs-error";
          errorItem.setAttribute("role", "alert");
          var errorText = document.createElement("span");
          errorText.textContent = "读取执行记录失败：" + (H.friendlyError ? H.friendlyError(error) : error.message || String(error));
          var retry = document.createElement("button");
          retry.type = "button";
          retry.textContent = "重试";
          retry.setAttribute("aria-label", "重新加载执行记录");
          retry.addEventListener("click", function () { loadRuns(box); });
          errorItem.appendChild(errorText);
          errorItem.appendChild(retry);
          box.appendChild(errorItem);
        }
      }
      runsBtn.addEventListener("click", function (event) {
        event.stopPropagation();
        var existing = li.querySelector(".automation-runs");
        if (existing) {
          runsGeneration++;
          activeRunsBox = null;
          existing.remove();
          runsBtn.textContent = "记录";
          return;
        }
        var box = document.createElement("ul");
        box.className = "list automation-runs";
        li.appendChild(box);
        runsBtn.textContent = "收起";
        loadRuns(box);
      });

      var deleteBtn = document.createElement("button");
      deleteBtn.setAttribute("data-core-action", "true");
      var deleteKey = String(task.id);
      setButtonBusy(deleteBtn, !!taskDeleteBusy[deleteKey], "删除", "处理中…");
      deleteBtn.addEventListener("click", async function (event) {
        event.stopPropagation();
        if (taskDeleteBusy[deleteKey]) return;
        taskDeleteBusy[deleteKey] = true;
        var owner = panelGeneration;
        setButtonBusy(deleteBtn, true, "删除", "等待确认…");
        try {
          var confirmed = await H.confirm({
            title: "删除自动化任务",
            message: "确定删除“" + task.name + "”？此操作无法撤销。",
            confirmText: "删除任务",
            kind: "danger",
          });
          if (!confirmed) return;
          setButtonBusy(deleteBtn, true, "删除", "删除中…");
          await H.del("/automations/" + encodeURIComponent(task.id));
          if (owner === panelGeneration) setStatus("已删除：" + task.name, false);
        } catch (error) {
          if (owner === panelGeneration) explainActionError("删除自动化", error);
        } finally {
          delete taskDeleteBusy[deleteKey];
          setButtonBusy(deleteBtn, false, "删除");
          if (panelMounted) refreshTasks();
        }
      });

      var actions = document.createElement("div");
      actions.className = "owo-aut-task-actions";
      actions.appendChild(toggleBtn);
      actions.appendChild(runsBtn);
      actions.appendChild(deleteBtn);
      li.appendChild(actions);
      list.appendChild(li);
    });
  }

  function renderReminders(reminders) {
    var list = el("owo-aut-reminders");
    if (!list) return;
    list.innerHTML = "";
    if (!reminders.length) {
      list.innerHTML = '<li class="sub">暂无提醒</li>';
      return;
    }
    reminders.forEach(function (text) {
      var li = document.createElement("li");
      li.textContent = "⏰ " + text;
      list.appendChild(li);
    });
  }

  async function createTask(event) {
    event.preventDefault();
    if (creatingTask) return;
    creatingTask = true;
    var owner = panelGeneration;
    var form = el("owo-aut-form");
    var submitButton = event.submitter || (form && form.querySelector('button[type="submit"]'));
    activeCreateButton = submitButton;
    setButtonBusy(submitButton, true, "创建任务", "正在创建…");
    try {
      var name = (el("owo-aut-name").value || "").trim();
      var kind = el("owo-aut-kind").value;
      var valueId = kind === "interval" ? "owo-aut-interval" : kind === "daily" ? "owo-aut-daily" : "owo-aut-oneshot";
      var value = (el(valueId).value || "").trim();
      var actionKind = el("owo-aut-action").value;
      var content = (el("owo-aut-content").value || "").trim();
      if (!name || !content) return;
      var schedule;
      try {
        schedule = buildSchedule(kind, value);
      } catch (error) {
        setStatus(error.message, true);
        el(valueId).focus();
        return;
      }
      var action;
      try {
        action = buildAction(actionKind, content);
      } catch (error) {
        setStatus(error.message, true);
        el("owo-aut-content").focus();
        return;
      }
      try {
        await H.post("/automations", { name: name, schedule: schedule, action: action });
        if (owner !== panelGeneration) return;
        el("owo-aut-name").value = "";
        el(valueId).value = "";
        el("owo-aut-content").value = "";
        setStatus("已创建：" + name, false);
        refresh();
      } catch (error) {
        if (owner === panelGeneration) setStatus("创建自动化失败：" + (error && error.message ? error.message : error), true);
      }
    } finally {
      creatingTask = false;
      var buttonToRestore = activeCreateButton || submitButton;
      activeCreateButton = null;
      setButtonBusy(buttonToRestore, false, "创建任务");
    }
  }

  function dispose() {
    panelMounted = false;
    panelGeneration += 1;
    taskRefreshGeneration += 1;
    reminderRefreshGeneration += 1;
    // 创建请求不会因页面切换而取消；保留锁直到原 POST 真正结束，避免重进页面重复创建。
    clearingReminders = false;
  }

  function mount(root, helpers) {
    dispose();
    if (helpers) H = helpers;
    panelMounted = true;
    root.innerHTML = nav();
    var form = el("owo-aut-form");
    var submitButton = form && form.querySelector('button[type="submit"]');
    if (creatingTask) {
      activeCreateButton = submitButton;
      setButtonBusy(submitButton, true, "创建任务", "正在创建…");
    }
    form.addEventListener("submit", createTask);
    bindListRetry("owo-aut-list", "tasks");
    bindListRetry("owo-aut-reminders", "reminders");
    el("owo-aut-kind").addEventListener("change", syncScheduleFields);
    el("owo-aut-action").addEventListener("change", syncActionFields);
    syncScheduleFields();
    syncActionFields();
    el("owo-aut-clear").addEventListener("click", async function () {
      var button = el("owo-aut-clear");
      if (clearingReminders) return;
      clearingReminders = true;
      var owner = panelGeneration;
      setButtonBusy(button, true, "清除提醒", "正在清除…");
      try {
        await H.call("/automations/reminders/clear", { method: "POST" });
        if (owner !== panelGeneration) return;
        setStatus("已清除提醒", false);
        refresh();
      } catch (error) {
        if (owner === panelGeneration) explainActionError("清除提醒", error);
      } finally {
        if (owner === panelGeneration) {
          clearingReminders = false;
          setButtonBusy(button, false, "清除提醒");
        }
      }
    });
    refresh();
  }

  return {
    id: id,
    title: "自动化",
    nav: nav,
    mount: mount,
    refresh: refresh,
    dispose: dispose,
    _test: { buildSchedule: buildSchedule, buildAction: buildAction, createTask: createTask },
  };
})();
