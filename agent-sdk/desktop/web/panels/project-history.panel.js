// ============================================================================
// 项目与运行历史面板 —— desktop/web/panels/project-history.panel.js
//
// 八期第四路：把历史 TeamRun 变成可检索、可复用的工作档案。全部数据来自
// **既有路由**，不新增服务端接口：
//   GET  /teams                        {teams:[TeamRun & {active,interrupted}]}
//                                      （TeamRun 序列化：team_id/mode/status/
//                                       template_id?/project_space_id?/created_at/
//                                       updated_at/budget/strategy_decision?）
//   GET  /teams/{id}                   {team,tasks,interrupted,audit_tail}
//                                      （复跑预填：objective 未来 additive 优先，
//                                       回退 audit_tail「目标 …」事件启发式提取）
//   GET  /projects/{pid}/deliverables  {approved[],pending_review[],...}
//   GET  /projects/{pid}/workspace     {root,read_only,write_allowed_paths,...}
//                                      （六期冻结；未绑定 404 → 复跑不带 workspace）
//   POST /teams                        （复跑：同目标 + 同模板 + 同工作区）
//
// 容错原则：所有字段缺省 []/""/null；详情/交付物/工作区请求失败各自降级为空并记
// 错误行，不阻塞列表渲染；workspace 404 视为「未绑定」静默处理。
//
// Node 兼容：globalThis 回退 + module.exports 导出 _test 纯逻辑挂钩（浏览器零差异）。
// ============================================================================
(function () {
  "use strict";

  var win = typeof window !== "undefined" ? window : globalThis;
  win.OwoPanels = win.OwoPanels || {};

  win.OwoPanels["project-history"] = (function () {
    var ID = "project-history";
    var MAX_ROWS = 50;       // 过滤后列表渲染上限
    var MAX_DETAIL = 12;     // 团队详情（任务数/objective）并发上限
    var MAX_PROJECTS = 8;    // 项目交付物并发上限

    // ---------- helpers（优先 app.js 注入，缺失时自建回退，与 action-center 同款） ----------
    var H = {};
    var rootEl = null;
    var tokenPromise = null;

    function defaultToken() {
      if (!tokenPromise) {
        tokenPromise = fetch(H.baseUrl + "/auth/token").then(function (r) {
          if (!r.ok) throw new Error("token 引导失败（HTTP " + r.status + "）");
          return r.json().then(function (d) {
            var t = d && d.token;
            if (!t) throw new Error("token 引导响应缺少 token");
            return t;
          });
        }).catch(function (e) {
          tokenPromise = null;
          throw e;
        });
      }
      return tokenPromise;
    }

    function httpFinish(r) {
      if (!r.ok) {
        return r.text().then(function (b) {
          throw new Error(r.status + ": " + b);
        });
      }
      if (r.status === 204) return null;
      return r.json();
    }

    function defaultGet(path) {
      return defaultToken().then(function (tok) {
        return fetch(H.baseUrl + path, {
          headers: { "Authorization": "Bearer " + tok, "Accept": "application/json" },
        }).then(httpFinish);
      });
    }

    function defaultPost(path, body) {
      return defaultToken().then(function (tok) {
        return fetch(H.baseUrl + path, {
          method: "POST",
          headers: {
            "Authorization": "Bearer " + tok,
            "Content-Type": "application/json",
            "Accept": "application/json",
          },
          body: JSON.stringify(body || {}),
        }).then(httpFinish);
      });
    }

    function defaultEsc(s) {
      return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
        return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
      });
    }

    function defaultFriendlyError(e) {
      var msg = String((e && e.message) || e || "");
      if (/^404/.test(msg)) return "资源不存在（404）。";
      if (/^400/.test(msg)) return "请求被拒绝（400）：" + msg.slice(5, 160);
      if (/^401|^403/.test(msg)) return "无权访问（401/403）。请检查凭据与权限范围。";
      if (/^409/.test(msg)) return "状态冲突（409）：当前状态不允许该操作。" + msg.slice(4, 160);
      if (/Failed to fetch|NetworkError/i.test(msg)) return "网络错误：无法连接服务端，请确认服务已启动。";
      return msg || "未知错误";
    }

    function el(sel) {
      if (!rootEl) return null;
      return rootEl.querySelector(String(sel || ""));
    }

    function esc(s) {
      return (H.esc || defaultEsc)(s);
    }

    function friendly(e) {
      return (H.friendlyError && H.friendlyError(e)) || defaultFriendlyError(e);
    }

    // ---------- 状态 ----------
    var state = {
      loading: false,
      loadedOnce: false,
      teams: [],           // GET /teams 原始列表
      details: {},         // team_id -> 详情 payload（失败不写入）
      detailErrors: {},    // team_id -> 错误
      deliverables: {},    // pid -> { approved, pending, error }（404 → 空且 error=""）
      workspaces: {},      // pid -> 绑定 payload（404 → null）
      filters: { status: "", template: "", project: "", q: "" },
      rerunBusy: {},       // team_id -> true
      rerunResults: {},    // team_id -> { ok, text, new_team_id? }
      errors: [],
      lastLoadedAt: "",
    };

    // ---------- 纯逻辑层（Node 测试挂钩覆盖） ----------
    // 团队状态归一：列表是 snake_case（serde），创建响应/SSE 是 Debug 形式——统一小写
    // 归一化（与 action-center/workswarm 同款）。
    function normStatus(s) {
      var x = String(s == null ? "" : s).toLowerCase();
      if (x === "awaitinghuman") return "awaiting_human";
      return x;
    }

    var STATUS_OPTIONS = [
      { value: "", label: "全部状态" },
      { value: "running", label: "运行中" },
      { value: "awaiting_human", label: "等待人工" },
      { value: "created", label: "已创建" },
      { value: "succeeded", label: "已完成" },
      { value: "failed", label: "失败" },
      { value: "cancelled", label: "已取消" },
    ];

    var STATUS_CN = {
      created: "已创建",
      running: "运行中",
      awaiting_human: "等待人工",
      succeeded: "已完成",
      failed: "失败",
      cancelled: "已取消",
    };

    // 耗时：created_at/updated_at（ISO rfc3339）→ "1m 24s" 人类可读；缺失/不可解析 → ""。
    function durationText(createdAt, updatedAt) {
      var a = Date.parse(String(createdAt == null ? "" : createdAt));
      var b = Date.parse(String(updatedAt == null ? "" : updatedAt));
      if (!isFinite(a) || !isFinite(b) || b < a) return "";
      var s = Math.round((b - a) / 1000);
      if (s < 60) return s + "s";
      var m = Math.floor(s / 60);
      var rest = s % 60;
      if (m < 60) return m + "m " + rest + "s";
      var h = Math.floor(m / 60);
      return h + "h " + (m % 60) + "m";
    }

    // 计划调用预算：strategy_decision.budget_calls_total（八期一路 additive 容错）。
    function plannedCalls(team) {
      var sd = team && team.strategy_decision;
      var n = sd && sd.budget_calls_total;
      return typeof n === "number" && n > 0 ? n : null;
    }

    // 任务计数（详情侧）：tasks 视图按状态归类；详情缺失 → null。
    function taskCounts(detail) {
      var tasks = (detail && detail.tasks) || [];
      if (!tasks.length) return null;
      var done = 0;
      var failed = 0;
      tasks.forEach(function (t) {
        var st = normStatus(t && t.status);
        if (st === "succeeded") done += 1;
        if (st === "failed" || st === "aborted") failed += 1;
      });
      return { total: tasks.length, done: done, failed: failed };
    }

    // 过滤选项：模板（template_id 缺失 = 动态组队）、项目（project_space_id 缺失按
    // "proj-"+team_id 派生，与 action-center 同款）。返回 [{value,label}]，value=""
    // 表示不过滤。
    function optionList(teams, keyOf, labelOf) {
      var seen = {};
      var out = [];
      (teams || []).forEach(function (t) {
        if (!t) return;
        var v = keyOf(t);
        if (v == null || seen[v]) return;
        seen[v] = true;
        out.push({ value: v, label: labelOf ? labelOf(t, v) : v });
      });
      return out;
    }

    function projectKeyOf(team) {
      if (!team) return "";
      return team.project_space_id || "proj-" + team.team_id;
    }

    function templateOptions(teams) {
      var opts = optionList(teams, function (t) {
        return t.template_id ? String(t.template_id) : "";
      }, function (t, v) {
        return v || "动态组队（无模板）";
      });
      opts.sort(function (a, b) { return a.label.localeCompare(b.label); });
      return opts;
    }

    function projectOptions(teams) {
      var opts = optionList(teams, projectKeyOf, function (t, v) { return v; });
      opts.sort(function (a, b) { return a.value.localeCompare(b.value); });
      return opts;
    }

    // 过滤判定：status 归一化相等（空 = 全部）、template 精确（"" = 动态组队桶）、
    // project 精确、q 大小写不敏感匹配 team_id。
    function matchesFilters(team, f) {
      if (!team || !team.team_id) return false;
      f = f || {};
      if (f.status && normStatus(team.status) !== f.status) return false;
      if (f.template != null && f.template !== "") {
        if (String(team.template_id || "") !== f.template) return false;
      }
      if (f.project && projectKeyOf(team) !== f.project) return false;
      if (f.q) {
        var q = String(f.q).toLowerCase();
        if (String(team.team_id || "").toLowerCase().indexOf(q) < 0) return false;
      }
      return true;
    }

    // 可见列表：过滤 + created_at 降序（历史视角最新在前）+ 截断。
    function visibleTeams(teams, f, cap) {
      var list = (teams || []).filter(function (t) { return matchesFilters(t, f); });
      list.sort(function (a, b) {
        return String(b.created_at || "").localeCompare(String(a.created_at || ""));
      });
      return list.slice(0, cap == null ? MAX_ROWS : cap);
    }

    // 复跑目标预填：detail.objective（未来 additive，优先）→ audit_tail 中最早一条
    // 「目标 …」事件 → null（用户手工输入）。audit_tail 为最新在前，取最后一条匹配。
    function objectiveFromDetail(detail) {
      if (!detail) return null;
      var direct = detail.objective;
      if (typeof direct === "string" && direct.trim()) return direct.trim();
      var tail = detail.audit_tail || [];
      var found = null;
      for (var i = 0; i < tail.length; i++) {
        var d = String((tail[i] && tail[i].detail) || "");
        if (d.indexOf("目标") === 0) found = d.slice(2).trim();
      }
      return found || null;
    }

    // 复跑请求体（Launcher buildCreateBody 同口径）：objective 必填；mode 沿用；
    // template_id 仅历史存在时带上；workspace 仅当前仍绑定时带上（六期冻结形状）。
    function rerunBody(detail, workspace, objectiveOverride) {
      var team = (detail && detail.team) || {};
      var objective = String(objectiveOverride == null ? "" : objectiveOverride).trim() ||
        objectiveFromDetail(detail) || "";
      if (!objective) return { error: "missing_objective" };
      var body = {
        objective: objective,
        mode: team.mode ? String(team.mode).toLowerCase() : "team",
      };
      if (team.template_id) body.template_id = String(team.template_id);
      if (workspace && workspace.root) {
        body.workspace = {
          root: String(workspace.root),
          read_only: workspace.read_only !== false,
        };
        if (Array.isArray(workspace.write_allowed_paths) && workspace.write_allowed_paths.length) {
          body.workspace.write_allowed_paths = workspace.write_allowed_paths.slice();
        }
        if (typeof workspace.tree_depth === "number" && workspace.tree_depth > 0) {
          body.workspace.tree_depth = workspace.tree_depth;
        }
      }
      return body;
    }

    // ---------- 渲染（纯 HTML 构建器） ----------
    function statusBadge(status) {
      var st = normStatus(status);
      var cn = STATUS_CN[st] || (st || "未知");
      return '<span class="ph-st ph-st-' + esc(st || "unknown") + '">' + esc(cn) + "</span>";
    }

    function rowHtml(team, extras) {
      extras = extras || {};
      var tid = String(team.team_id || "");
      var st = state.rerunResults[tid];
      var busy = state.rerunBusy[tid];
      var calls = plannedCalls(team);
      var dur = durationText(team.created_at, team.updated_at);
      var tc = taskCounts(extras.detail);
      var dlv = extras.deliverables;
      var dlvText = dlv
        ? "批准 " + (dlv.approved || 0) + " · 待评审 " + (dlv.pending || 0)
        : "";
      return (
        '<div class="ph-row" data-ph-row="' + esc(tid) + '">' +
        '<div class="ph-row-head">' +
        '<code class="ph-tid">' + esc(tid) + "</code>" +
        statusBadge(team.status) +
        '<span class="ph-mode">' + esc(String(team.mode || "").toLowerCase() || "team") + "</span>" +
        (team.template_id
          ? '<span class="ph-tpl">' + esc(String(team.template_id)) + "</span>"
          : '<span class="ph-tpl ph-tpl-none">动态组队</span>') +
        "</div>" +
        '<div class="ph-row-meta hint">' +
        "<span>创建 " + esc(String(team.created_at || "").replace("T", " ").slice(0, 19)) + "</span>" +
        (dur ? "<span>耗时 " + esc(dur) + "</span>" : "") +
        (calls != null ? "<span>预算 " + calls + " 次调用</span>" : "") +
        (tc ? "<span>任务 " + tc.done + "/" + tc.total + (tc.failed ? "（失败 " + tc.failed + "）" : "") + "</span>" : "") +
        (dlvText ? "<span>交付物：" + esc(dlvText) + "</span>" : "") +
        (extras.objective ? '<span class="ph-obj" title="' + esc(extras.objective) + '">' + esc(extras.objective.slice(0, 80)) + "</span>" : "") +
        "</div>" +
        '<div class="ph-row-actions">' +
        '<button type="button" class="owo-ac-mini" data-ph-open="' + esc(tid) + '">打开详情</button>' +
        '<button type="button" class="owo-ac-mini ph-rerun" data-ph-rerun="' + esc(tid) + '"' +
        (busy ? " disabled" : "") + ">同配置复跑</button>" +
        '<span class="hint" data-ph-ws="' + esc(tid) + '">' +
        (extras.workspace === null ? "工作区未绑定（复跑不带目录）" : "") +
        "</span>" +
        "</div>" +
        '<div class="ph-result" data-ph-result="' + esc(tid) + '">' +
        (st ? esc(st.text) : "") +
        "</div>" +
        "</div>"
      );
    }

    function listHtml(teams, st) {
      st = st || state;
      var rows = visibleTeams(teams, st.filters, MAX_ROWS);
      if (!rows.length) {
        return '<div class="ph-empty">没有匹配的历史运行（调整过滤条件或刷新）。</div>';
      }
      return rows
        .map(function (t) {
          var tid = String(t.team_id || "");
          return rowHtml(t, {
            detail: st.details[tid],
            deliverables: (function () {
              var d = st.deliverables[projectKeyOf(t)];
              return d && !d.error ? d : null;
            })(),
            workspace: st.workspaces[projectKeyOf(t)],
            objective: (function () {
              var d = st.details[tid];
              return d ? objectiveFromDetail(d) : null;
            })(),
          });
        })
        .join("");
    }

    function filtersHtml(st) {
      st = st || state;
      var f = st.filters;
      function opt(list, cur) {
        return list.map(function (o) {
          return '<option value="' + esc(o.value) + '"' + (o.value === cur ? " selected" : "") + ">" +
            esc(o.label) + "</option>";
        }).join("");
      }
      return (
        '<div class="ph-filters">' +
        '<select id="ph-f-status" aria-label="按状态过滤">' + opt(STATUS_OPTIONS, f.status) + "</select>" +
        '<select id="ph-f-template" aria-label="按模板过滤">' +
        '<option value="">全部模板</option>' + opt(templateOptions(st.teams), f.template) + "</select>" +
        '<select id="ph-f-project" aria-label="按项目过滤">' +
        '<option value="">全部项目</option>' + opt(projectOptions(st.teams), f.project) + "</select>" +
        '<input id="ph-f-q" type="search" placeholder="按 team_id 搜索" value="' + esc(f.q) + '" />' +
        "</div>"
      );
    }

    function shellHtml() {
      return (
        '<div class="ph">' +
        '<div class="ph-head">' +
        "<h2>项目与运行历史</h2>" +
        '<span class="hint">客户端聚合既有路由（无新增接口）：/teams · /teams/{id} · deliverables · workspace · POST /teams（复跑）</span>' +
        '<button type="button" id="ph-refresh" class="owo-ac-mini">刷新</button>' +
        '<span id="ph-meta" class="hint" aria-live="polite"></span>' +
        "</div>" +
        '<div id="ph-errors" aria-live="polite"></div>' +
        '<div id="ph-filters-box"></div>' +
        '<div id="ph-list"><div class="ph-empty">正在加载历史运行…</div></div>' +
        "</div>"
      );
    }

    // ---------- 行为层 ----------
    function paintMeta(text) {
      var m = el("#ph-meta");
      if (m) m.textContent = text;
    }

    function paintErrors() {
      var box = el("#ph-errors");
      if (!box) return;
      var lines = state.errors.slice();
      Object.keys(state.detailErrors || {}).forEach(function (tid) {
        lines.push("团队 " + tid + " 详情加载失败：" + state.detailErrors[tid]);
      });
      Object.keys(state.deliverables || {}).forEach(function (pid) {
        var d = state.deliverables[pid];
        if (d && d.error) lines.push("项目 " + pid + " 交付物加载失败：" + d.error);
      });
      box.innerHTML = lines.map(function (x) { return '<div class="owo-ac-err">' + esc(x) + "</div>"; }).join("");
    }

    function renderFilters() {
      var box = el("#ph-filters-box");
      if (box) box.innerHTML = filtersHtml(state);
    }

    function render() {
      if (!rootEl) return;
      var box = el("#ph-list");
      if (box) box.innerHTML = listHtml(state.teams, state);
      paintErrors();
      var total = (state.teams || []).length;
      var shown = visibleTeams(state.teams, state.filters, MAX_ROWS).length;
      paintMeta(
        state.loading
          ? "加载中…"
          : "共 " + total + " 次运行 · 显示 " + shown + " 条" +
            (state.lastLoadedAt ? " · 更新于 " + state.lastLoadedAt : "")
      );
    }

    function paintRerun(tid) {
      var box = el("#ph-list");
      if (!box) return;
      var res = state.rerunResults[tid];
      var div = box.querySelector('[data-ph-result="' + cssEsc(tid) + '"]');
      if (div) {
        div.textContent = res ? res.text : "";
        div.className = "ph-result" + (res && res.ok ? " ok" : "") + (res && !res.ok ? " bad" : "");
      }
      if (!state.rerunBusy[tid]) {
        var btn = box.querySelector('[data-ph-rerun="' + cssEsc(tid) + '"]');
        if (btn) {
          btn.disabled = false;
          btn.textContent = "同配置复跑";
        }
      }
    }

    function cssEsc(s) {
      return String(s == null ? "" : s).replace(/"/g, '\\"');
    }

    // 详情 + 交付物 + 工作区（可见行的补充信息；全部容错降级）。
    function enrichVisible(rows) {
      var detailTargets = rows.slice(0, MAX_DETAIL);
      var projects = (function () {
        var seen = {};
        var out = [];
        for (var i = 0; i < rows.length && out.length < MAX_PROJECTS; i++) {
          var pid = projectKeyOf(rows[i]);
          if (pid && !seen[pid]) {
            seen[pid] = true;
            out.push({ pid: pid, team_id: rows[i].team_id });
          }
        }
        return out;
      })();

      var detailJobs = detailTargets.map(function (t) {
        return Promise.resolve()
          .then(function () { return H.get("/teams/" + encodeURIComponent(t.team_id)); })
          .then(function (d) { state.details[t.team_id] = d; })
          .catch(function (e) {
            state.detailErrors[t.team_id] = friendly(e);
          });
      });

      var projectJobs = projects.map(function (p) {
        var dlv = H.get("/projects/" + encodeURIComponent(p.pid) + "/deliverables")
          .then(function (d) {
            state.deliverables[p.pid] = {
              approved: (d.approved || []).length,
              pending: (d.pending_review || []).length,
            };
          })
          .catch(function (e) {
            var is404 = /^404/.test(String((e && e.message) || e || ""));
            state.deliverables[p.pid] = { approved: 0, pending: 0, error: is404 ? "" : friendly(e) };
          });
        var ws = H.get("/projects/" + encodeURIComponent(p.pid) + "/workspace")
          .then(function (w) { state.workspaces[p.pid] = w; })
          .catch(function (e) {
            // 404 = 未绑定工作区 → null（复跑不带目录）；其余错误也降级为 null 但不阻塞。
            state.workspaces[p.pid] = null;
            void e;
          });
        return Promise.all([dlv, ws]);
      });

      return Promise.all(detailJobs.concat(projectJobs));
    }

    function load() {
      if (state.loading) return Promise.resolve();
      state.loading = true;
      state.errors = [];
      render();
      return Promise.resolve()
        .then(function () { return H.get("/teams"); })
        .then(function (d) {
          state.teams = (d && d.teams) || [];
          state.loading = false;
          state.loadedOnce = true;
          state.lastLoadedAt = new Date().toLocaleTimeString();
          renderFilters();
          render();
          var rows = visibleTeams(state.teams, state.filters, MAX_ROWS);
          return enrichVisible(rows).then(function () {
            state.lastLoadedAt = new Date().toLocaleTimeString();
            render();
          });
        })
        .catch(function (e) {
          state.errors.push("团队列表加载失败：" + friendly(e));
          state.loading = false;
          state.loadedOnce = true;
          render();
        });
    }

    // 复跑：详情（objective 预填）→ 工作区绑定 → POST /teams → 结果行 + 可深链。
    function startRerun(teamId) {
      var tid = String(teamId || "");
      if (!tid || state.rerunBusy[tid]) return Promise.resolve();
      state.rerunBusy[tid] = true;
      delete state.rerunResults[tid];
      paintRerun(tid);
      var detail = state.details[tid];
      var pid = null;
      var team = (state.teams || []).filter(function (t) { return t.team_id === tid; })[0];
      if (team) pid = projectKeyOf(team);
      var ensureDetail = detail
        ? Promise.resolve(detail)
        : Promise.resolve()
            .then(function () { return H.get("/teams/" + encodeURIComponent(tid)); })
            .then(function (d) {
              state.details[tid] = d;
              return d;
            });
      return ensureDetail
        .then(function (d) {
          var ws = pid ? state.workspaces[pid] : null;
          if (ws === undefined) ws = null;
          var body = rerunBody(d, ws);
          if (body && body.error === "missing_objective") {
            state.rerunResults[tid] = {
              ok: false,
              text: "无法自动提取原目标（审计尾迹未含「目标 …」事件）——请在 WorkSwarm 详情确认目标后手工重跑。",
            };
            return null;
          }
          return H.post("/teams", body).then(function (created) {
            var newId = created && created.team_id;
            state.rerunResults[tid] = {
              ok: true,
              text: "已按同配置创建新运行" + (newId ? "：" + newId : "") + "。",
              new_team_id: newId || null,
            };
          });
        })
        .catch(function (e) {
          state.rerunResults[tid] = { ok: false, text: "复跑失败：" + friendly(e) };
        })
        .then(function () {
          state.rerunBusy[tid] = false;
          paintRerun(tid);
        });
    }

    function gotoWorkswarm(teamId) {
      var id = String(teamId || "");
      if (!id) return;
      try {
        if (win.OwoPanels && win.OwoPanels.workswarm && win.OwoPanels.workswarm.open) {
          // 侧导航切到 workswarm 面板（app.js 挂载机制），再深链详情。
          var nav = document.querySelector('#panelNav button[data-panel="workswarm"]');
          if (nav) nav.click();
          win.OwoPanels.workswarm.open(id);
          return;
        }
      } catch (_e) { /* 深链失败静默：详情可手工打开 */ }
    }

    function openRerunPrompt(tid) {
      var newId = (state.rerunResults[tid] || {}).new_team_id;
      if (newId) gotoWorkswarm(newId);
    }

    function onListClick(ev) {
      var t = ev.target;
      if (!t || !t.closest) return;
      var openBtn = t.closest("[data-ph-open]");
      if (openBtn) {
        gotoWorkswarm(openBtn.getAttribute("data-ph-open") || "");
        return;
      }
      var rerunBtn = t.closest("[data-ph-rerun]");
      if (rerunBtn) {
        startRerun(rerunBtn.getAttribute("data-ph-rerun") || "");
        return;
      }
      var result = t.closest("[data-ph-result]");
      if (result && result.textContent.indexOf("已按同配置创建") === 0) {
        openRerunPrompt(result.getAttribute("data-ph-result") || "");
      }
    }

    function onFilterChange(ev) {
      var t = ev.target;
      if (!t || !t.id) return;
      if (t.id === "ph-f-status") state.filters.status = t.value;
      else if (t.id === "ph-f-template") state.filters.template = t.value;
      else if (t.id === "ph-f-project") state.filters.project = t.value;
      else if (t.id === "ph-f-q") state.filters.q = t.value;
      else return;
      render();
      var rows = visibleTeams(state.teams, state.filters, MAX_ROWS);
      enrichVisible(rows).then(render);
    }

    function mount(root, helpers) {
      rootEl = root;
      H = helpers || {};
      H.baseUrl = H.baseUrl || (win.OwoPanels && win.OwoPanels.baseUrl) || "";
      if (!H.get) H.get = defaultGet;
      if (!H.post) H.post = defaultPost;
      if (!H.esc) H.esc = defaultEsc;
      root.innerHTML = shellHtml();
      var box = el("#ph-list");
      if (box) box.addEventListener("click", onListClick);
      var fbox = el("#ph-filters-box");
      if (fbox) fbox.addEventListener("change", onFilterChange);
      var q = el("#ph-f-q");
      if (q) q.addEventListener("input", onFilterChange);
      var refresh = el("#ph-refresh");
      if (refresh) refresh.onclick = function () { load(); };
      load();
    }

    // ---------- 测试挂钩 ----------
    var TEST_API = {
      state: state,
      normStatus: normStatus,
      durationText: durationText,
      plannedCalls: plannedCalls,
      taskCounts: taskCounts,
      templateOptions: templateOptions,
      projectOptions: projectOptions,
      projectKeyOf: projectKeyOf,
      matchesFilters: matchesFilters,
      visibleTeams: visibleTeams,
      objectiveFromDetail: objectiveFromDetail,
      rerunBody: rerunBody,
      rowHtml: rowHtml,
      listHtml: listHtml,
      filtersHtml: filtersHtml,
      shellHtml: shellHtml,
      render: render,
      load: load,
      startRerun: startRerun,
      gotoWorkswarm: gotoWorkswarm,
      getTransport: function () {
        return { get: H.get, post: H.post };
      },
      setTransport: function (t) {
        if (t && t.get) H.get = t.get;
        if (t && t.post) H.post = t.post;
      },
      setRoot: function (r) {
        rootEl = r;
      },
    };

    return {
      id: ID,
      title: "项目与运行历史",
      mount: mount,
      _test: TEST_API,
    };
  })();
})();

// Node（CommonJS）测试环境导出；浏览器无 module 定义，此分支不生效、零运行时差异。
if (typeof module !== "undefined" && module.exports) {
  var __phWin = typeof window !== "undefined" ? window : globalThis;
  module.exports = __phWin.OwoPanels["project-history"];
}
