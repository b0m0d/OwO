/* 自动化面板：新建任务（间隔/每天/单次 × 提醒/跑任务）+ 任务列表（启停/执行记录/删除）+ 提醒。
 * 由工具视图的静态卡片迁移而来（此前 index.html 手写表单 + app.js 零散函数，
 * 与扩展面板体系割裂）；迁入后走统一网格排版与分区目录。
 * 纯脚本 IIFE，注册 window.OwoPanels.automations；helpers 缺失时自建 fetch（防御性降级）。
 */
window.OwoPanels = window.OwoPanels || {};
window.OwoPanels.automations = (function () {
  "use strict";

  var id = "automations";

  function defaultHelpers() {
    var baseUrl = (window.OwoPanels && window.OwoPanels.baseUrl) || "";
    function get(path) {
      return fetch(baseUrl + path).then(function (r) {
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
        return fetch(baseUrl + path, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body || {}),
        }).then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return r.status === 204 ? null : r.json();
        });
      },
      call: function (path, options) {
        return fetch(baseUrl + path, options || {}).then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return r.status === 204 ? null : r.json().catch(function () { return null; });
        });
      },
      del: function (path) {
        return fetch(baseUrl + path, { method: "DELETE" }).then(function (r) {
          if (!r.ok) throw new Error("HTTP " + r.status);
          return null;
        });
      },
      esc: esc,
      friendlyError: friendlyError,
    };
  }

  var H = defaultHelpers();
  // 轮询定时器：仅面板挂载期间运行（元素被卸载即自停，重新挂载时重启）。
  var pollTimer = null;

  function el(domId) {
    return document.getElementById(domId);
  }

  function setStatus(text, isError) {
    var status = el("owo-aut-status");
    if (!status) return;
    status.textContent = text || "";
    status.style.color = isError ? "var(--red)" : "var(--text-3)";
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
      '<div class="tool-field"><label for="owo-aut-value">触发值</label><input id="owo-aut-value" placeholder="60 / 09:00 / 2026-08-12T12:00:00Z" required></div>' +
      '<div class="tool-field"><label for="owo-aut-action">动作</label><select id="owo-aut-action">' +
      // 「跑任务」（run_prompt）需引擎执行器支持，上游 08f6d82 暂未回移——先只提供提醒。
      '<option value="reminder">提醒（到点推送一条话）</option>' +
      "</select></div>" +
      '<div class="tool-field tool-field-full"><label for="owo-aut-content">内容</label><input id="owo-aut-content" placeholder="提醒文案 / 任务指令（如：检查 git 状态并总结）" required></div>' +
      '<div class="tool-actions tool-actions-end tool-field-full"><button type="submit" class="primary">创建任务</button></div>' +
      "</form>" +
      '<div id="owo-aut-status"></div>' +
      '<div class="sub">任务列表</div>' +
      '<ul id="owo-aut-list" class="list"><li class="sub">加载中…</li></ul>' +
      '<div class="sub">提醒</div>' +
      '<div class="tool-actions"><button id="owo-aut-clear">清除提醒</button></div>' +
      '<ul id="owo-aut-reminders" class="list"><li class="sub">加载中…</li></ul>' +
      "</div>" +
      "</section>"
    );
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

  /// 动作类型可读化（提醒 / 跑任务）。
  function describeAction(action) {
    if (!action || typeof action !== "object") return "提醒";
    if (action.kind === "run_prompt") return "跑任务";
    if (action.kind === "reminder") return "提醒";
    return String(action.kind || "");
  }

  function refresh() {
    H.get("/automations")
      .then(function (tasks) {
        renderTasks(Array.isArray(tasks) ? tasks : []);
      })
      .catch(function (error) {
        var list = el("owo-aut-list");
        if (list) list.innerHTML = '<li class="sub">' + H.esc(H.friendlyError(error)) + "</li>";
      });
    H.get("/automations/reminders")
      .then(function (reminders) {
        renderReminders(Array.isArray(reminders) ? reminders : []);
      })
      .catch(function (error) {
        var list = el("owo-aut-reminders");
        if (list) list.innerHTML = '<li class="sub">' + H.esc(H.friendlyError(error)) + "</li>";
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
      toggleBtn.textContent = task.enabled ? "停用" : "启用";
      toggleBtn.addEventListener("click", async function (event) {
        event.stopPropagation();
        await H.call("/automations/" + task.id + "/toggle", { method: "POST" }).catch(function () {});
        refresh();
      });

      // A8-1：展开执行记录（次日可查「跑没跑、结果如何」）。
      var runsBtn = document.createElement("button");
      runsBtn.textContent = "记录";
      runsBtn.addEventListener("click", async function (event) {
        event.stopPropagation();
        var existing = li.querySelector(".automation-runs");
        if (existing) {
          existing.remove();
          runsBtn.textContent = "记录";
          return;
        }
        var box = document.createElement("ul");
        box.className = "list automation-runs";
        box.innerHTML = '<li class="sub">加载中…</li>';
        li.appendChild(box);
        runsBtn.textContent = "收起";
        try {
          var runs = await H.get(
            "/automations/runs?task_id=" + encodeURIComponent(task.id) + "&limit=10"
          );
          box.innerHTML = "";
          if (!Array.isArray(runs) || !runs.length) {
            box.innerHTML = '<li class="sub">尚无执行记录（到点触发后可见）</li>';
            return;
          }
          runs.forEach(function (run) {
            var item = document.createElement("li");
            var when = String(run.at || "").replace("T", " ").slice(0, 16);
            var verdict = run.status === "ok" ? "✓ 成功" : "✗ 失败";
            item.innerHTML =
              "<strong>" + H.esc(verdict) + " · " + H.esc(when) + '</strong><span class="sub">' +
              H.esc(String(run.output || "（无输出）").slice(0, 400)) + "</span>";
            box.appendChild(item);
          });
        } catch (error) {
          box.innerHTML = '<li class="sub">' + H.esc(H.friendlyError(error)) + "</li>";
        }
      });

      var deleteBtn = document.createElement("button");
      deleteBtn.textContent = "删除";
      deleteBtn.addEventListener("click", async function (event) {
        event.stopPropagation();
        await H.del("/automations/" + task.id).catch(function () {});
        refresh();
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
    var name = (el("owo-aut-name").value || "").trim();
    var kind = el("owo-aut-kind").value;
    var value = (el("owo-aut-value").value || "").trim();
    var actionKind = el("owo-aut-action").value;
    var content = (el("owo-aut-content").value || "").trim();
    if (!name || !value || !content) return;
    var schedule;
    if (kind === "interval") {
      var everySecs = parseInt(value, 10);
      if (!Number.isFinite(everySecs) || everySecs <= 0) {
        setStatus("间隔需为正整数（秒）", true);
        return;
      }
      schedule = { kind: "interval", every_secs: everySecs };
    } else if (kind === "daily") {
      schedule = { kind: "daily", time: value };
    } else {
      // 后端 OneShot 的 serde tag 是 `one_shot`（此前发的 `oneshot` 会反序列化失败）。
      schedule = { kind: "one_shot", at: value };
    }
    var action =
      actionKind === "prompt"
        ? { kind: "reminder", text: content }
        : { kind: "reminder", text: content };
    try {
      await H.post("/automations", { name: name, schedule: schedule, action: action });
      el("owo-aut-name").value = "";
      el("owo-aut-value").value = "";
      el("owo-aut-content").value = "";
      setStatus("已创建：" + name, false);
      refresh();
    } catch (error) {
      setStatus("创建自动化失败：" + (error && error.message ? error.message : error), true);
    }
  }

  function mount(root, helpers) {
    if (helpers) H = helpers;
    root.innerHTML = nav();
    el("owo-aut-form").addEventListener("submit", createTask);
    el("owo-aut-clear").addEventListener("click", async function () {
      try {
        await H.call("/automations/reminders/clear", { method: "POST" });
        setStatus("已清除提醒", false);
        refresh();
      } catch (error) {
        setStatus("清除提醒失败：" + (error && error.message ? error.message : error), true);
      }
    });
    refresh();
    startPolling();
  }

  function startPolling() {
    if (pollTimer) return;
    // 提醒到点由服务端推送进列表，这里轮询刷新保持可见（原全局 5s/10s 轮询迁入面板，
    // 仅挂载期间运行——面板被替换后元素消失即自停）。
    pollTimer = setInterval(function () {
      if (!el("owo-aut-list")) {
        clearInterval(pollTimer);
        pollTimer = null;
        return;
      }
      refresh();
    }, 5000);
  }

  return {
    id: id,
    title: "自动化",
    nav: nav,
    mount: mount,
    refresh: refresh,
  };
})();
