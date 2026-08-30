// ============================================================================
// Action Center（待我处理）面板 —— desktop/web/panels/action-center.panel.js
//
// 七期第四路：把四类"需要人工处理"的事项聚合为一个日常工作台。全部数据来自
// **既有路由**的客户端聚合，不新增任何服务端接口：
//   GET  /teams                          {teams:[{team_id,status,mode,interrupted,
//                                         active,members[],project_space_id,...}]}
//   GET  /teams/{id}                     {team,tasks,interrupted,audit_tail,
//                                         changes?,write_lease?,worker_profiles?}
//                                        （七期新字段为可选+可空，UI 双路径容错读取）
//   GET  /projects/{pid}/artifacts       {artifacts:[{artifact_id,version,kind,format,
//                                         review_state,validation?,team_id?,...}]}
//   POST /teams/{id}/steer               {command:"retry",step_id,note} —— 第三路冻结契约
//
// 四类待办（聚合口径，与 WorkSwarm 面板闸门一致）：
//   ① 等待 Human 结果：团队 awaiting_human，或存在人节点（runtime_binding.kind=human）
//      且步骤未到终态（succeeded/failed/aborted）的任务；
//   ② 待评审产物：review_state 归一化后为 pendingreview；以及 validation.valid===false
//      的校验未通过产物（未过校验不进入 PendingReview，同样需要人工关注）；
//   ③ 失败步骤（可重试）：Failed/Aborted 步骤，团队状态非 succeeded/cancelled
//      （与核心 steer_retry 目标闸门一致）；
//   ④ 写租约持有：write_lease 非空且未释放（单写租约，同一工作区同时仅一个写角色）。
//
// 容错原则：所有字段缺省 []/false/null；详情/产物请求失败各自降级为空并记错误行，
// 不阻塞其他类目；产物 404 视为"暂无产物"静默处理。
//
// Node 兼容：globalThis 回退 + module.exports 导出 _test 纯逻辑挂钩（浏览器零差异），
// 供 tests/action-center.panel.test.mjs 断言。
// ============================================================================
(function () {
  "use strict";

  var win = typeof window !== "undefined" ? window : globalThis;
  win.OwoPanels = win.OwoPanels || {};

  win.OwoPanels["action-center"] = (function () {
    var ID = "action-center";
    var MAX_DETAIL = 12;   // 团队详情并发上限（候选裁剪）
    var MAX_PROJECTS = 8;  // 项目产物并发上限

    // ---------- helpers（优先 app.js 注入，缺失时自建回退，与 workswarm/launcher 同款） ----------
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
      teams: [],            // GET /teams 原始列表
      details: {},          // team_id -> 详情 payload（加载失败不写入）
      detailErrors: {},     // team_id -> 错误消息
      artifacts: {},        // pid -> { items, team_id, error }（404 → items=[] 且 error=""）
      items: { human: [], review: [], failed: [], lease: [] },
      errors: [],           // 顶层请求错误备注
      retryBusy: {},        // key(teamId/stepId) -> true
      retryResults: {},     // key -> { ok, text }
      lastLoadedAt: "",
    };

    // ---------- 纯逻辑层（Node 测试挂钩覆盖） ----------
    var STEP_TERMINAL = { succeeded: true, failed: true, aborted: true };
    var STEP_RETRYABLE = { failed: true, aborted: true };

    // 团队状态归一：TeamRun 里是 snake_case（serde），创建响应/SSE state 帧是
    // Debug 形式（AwaitingHuman/...）——统一小写归一化（与 WorkSwarm 同款）。
    function normStatus(s) {
      var x = String(s == null ? "" : s).toLowerCase();
      if (x === "awaitinghuman") return "awaiting_human";
      return x;
    }

    var TEAM_NO_RETRY = { succeeded: true, cancelled: true };

    function isRetryableStep(t) {
      return !!t && !!STEP_RETRYABLE[normStatus(t.status)];
    }

    // 候选团队：可能存在"待我处理"项的团队。succeeded/cancelled 一律排除（终态且
    // 不可重试）；failed 保留（存在可 retry 的失败步骤）；其余状态保留。
    // 排序：awaiting_human > running > failed > 其他；同类内 updated_at 降序；截断。
    function candidateTeams(teams, cap) {
      var rank = { awaiting_human: 0, running: 1, failed: 2 };
      var list = (teams || []).filter(function (t) {
        if (!t || t.team_id == null) return false;
        var st = normStatus(t.status);
        return !TEAM_NO_RETRY[st];
      });
      list.sort(function (a, b) {
        var ra = rank[normStatus(a.status)];
        var rb = rank[normStatus(b.status)];
        if (ra !== rb) return (ra == null ? 3 : ra) - (rb == null ? 3 : rb);
        return String(b.updated_at || "").localeCompare(String(a.updated_at || ""));
      });
      return list.slice(0, cap == null ? MAX_DETAIL : cap);
    }

    // 项目空间 id：team.project_space_id 优先，缺省 "proj-"+team_id
    // （与 WorkSwarm loadArtifacts 的派生规则同款）；去重后截断。
    function projectIdsOf(teams, cap) {
      var seen = {};
      var out = [];
      (teams || []).forEach(function (t) {
        if (!t || t.team_id == null) return;
        var pid = t.project_space_id || "proj-" + t.team_id;
        if (pid && !seen[pid]) {
          seen[pid] = true;
          out.push({ pid: pid, team_id: String(t.team_id) });
        }
      });
      return out.slice(0, cap == null ? MAX_PROJECTS : cap);
    }

    // 人节点任务（与 WorkSwarm humanTasks 同闸门）：成员 runtime_binding.kind ===
    // "human" 且步骤未到终态。member 以 task.worker 对应 member_id。
    function humanTasksOf(detail) {
      var team = (detail && (detail.team || detail)) || {};
      var tasks = (detail && detail.tasks) || [];
      var byId = {};
      (team.members || []).forEach(function (m) {
        if (m && m.member_id) byId[m.member_id] = m;
      });
      return tasks.filter(function (t) {
        var m = t && byId[t.worker];
        return !!(m && m.runtime_binding && m.runtime_binding.kind === "human" && !STEP_TERMINAL[normStatus(t.status)]);
      });
    }

    // ① 等待 Human 结果：团队 awaiting_human，或存在未完成人节点任务。
    //    输出按团队聚合为一条（深链目标本来就是团队详情）。
    function humanItemsOf(detail) {
      var team = (detail && detail.team) || {};
      var st = normStatus(team.status);
      var hts = humanTasksOf(detail);
      if (st !== "awaiting_human" && !hts.length) return [];
      return [{
        kind: "human",
        team_id: String(team.team_id || ""),
        team_status: st,
        awaiting_team: st === "awaiting_human",
        tasks: hts.map(function (t) {
          return {
            task_id: String(t.task_id || ""),
            role: String(t.role || t.task_id || ""),
            status: normStatus(t.status),
          };
        }),
      }];
    }

    // ③ 失败步骤（可重试）：Failed/Aborted 步骤；团队 succeeded/cancelled 不提供
    // （与 shouldShowRetry 闸门一致）。error/failure_code 双字段容错。
    function failedItemsOf(detail) {
      var team = (detail && detail.team) || {};
      var st = normStatus(team.status);
      if (TEAM_NO_RETRY[st]) return [];
      var tasks = (detail && detail.tasks) || [];
      return tasks.filter(isRetryableStep).map(function (t) {
        return {
          kind: "failed",
          team_id: String(team.team_id || ""),
          team_status: st,
          step_id: String(t.task_id || ""),
          role: String(t.role || t.worker || t.task_id || ""),
          attempts: Number(t.attempts || 0),
          error: String(t.error || t.failure_code || ""),
          interrupted: !!(detail && detail.interrupted),
        };
      });
    }

    // ④ 写租约：七期新字段，双路径容错读取（详情顶层或 team 对象内）。
    function leaseOf(detail) {
      if (!detail) return null;
      var lease = detail.write_lease;
      if (!lease && detail.team && typeof detail.team === "object") lease = detail.team.write_lease;
      return lease && typeof lease === "object" ? lease : null;
    }

    function leaseItemsOf(detail) {
      var team = (detail && detail.team) || {};
      var lease = leaseOf(detail);
      if (!lease || lease.released_at_ms != null) return []; // 已释放 → 不再需要处理
      return [{
        kind: "lease",
        team_id: String(team.team_id || ""),
        holder_role: String(lease.holder_role || "—"),
        holder_step_id: String(lease.holder_step_id || "—"),
        acquired_at_ms: lease.acquired_at_ms == null ? null : Number(lease.acquired_at_ms),
      }];
    }

    // 评审状态归一（与 WorkSwarm normReviewState 同款）。
    function normReviewState(s) {
      return String(s == null ? "" : s).toLowerCase().replace(/[_\s-]/g, "");
    }

    // ② 待评审产物（单项目聚合）：pendingreview，或 validation.valid===false
    //    （校验未通过不进入 PendingReview，但同样需要人工处理）。
    //    entry = { items, team_id, error }；产物自带 team_id 优先，缺省回落到 pid 所属团队。
    function reviewItemsOf(pid, entry) {
      var fallbackTeam = entry && entry.team_id ? String(entry.team_id) : "";
      return ((entry && entry.items) || []).filter(function (a) {
        if (!a || a.artifact_id == null) return false;
        if (normReviewState(a.review_state) === "pendingreview") return true;
        var v = a.validation;
        return !!(v && typeof v === "object" && v.valid === false);
      }).map(function (a) {
        var v = a.validation && typeof a.validation === "object" ? a.validation : null;
        return {
          kind: "review",
          project_id: String(pid || ""),
          artifact_id: String(a.artifact_id),
          version: a.version == null ? "" : String(a.version),
          review_state: normReviewState(a.review_state),
          kind_name: String(a.kind || ""),
          format: String(a.format || ""),
          team_id: a.team_id == null ? fallbackTeam : String(a.team_id),
          validation_failed: !!(v && v.valid === false),
          validation_reason: v && v.reason != null ? String(v.reason) : "",
        };
      });
    }

    // 汇总（纯函数）：对 state 形状的输入做四类聚合；详情/产物加载失败的条目跳过
    // （错误已在 detailErrors/artifacts[].error 中记录，渲染层单独展示）。
    function aggregate(st) {
      var details = (st && st.details) || {};
      var human = [];
      var failed = [];
      var lease = [];
      Object.keys(details).forEach(function (tid) {
        var d = details[tid];
        if (!d) return;
        human = human.concat(humanItemsOf(d));
        failed = failed.concat(failedItemsOf(d));
        lease = lease.concat(leaseItemsOf(d));
      });
      var review = [];
      var arts = (st && st.artifacts) || {};
      Object.keys(arts).forEach(function (pid) {
        var entry = arts[pid];
        if (entry && !entry.error) review = review.concat(reviewItemsOf(pid, entry));
      });
      return { human: human, review: review, failed: failed, lease: lease };
    }

    // 第三路冻结契约：POST /teams/{id}/steer {"command":"retry","step_id":"...","note":"..."}
    // note 可省略（服务端缺省 "retry"）；这里给确定性中文缺省，便于审计可读。
    function buildRetryBody(stepId, note) {
      var sid = String(stepId == null ? "" : stepId).trim();
      var n = note == null ? "" : String(note).trim();
      return {
        command: "retry",
        step_id: sid,
        note: n || ("重试此节点：" + sid),
      };
    }

    // ---------- 渲染（纯 HTML 构建器，render 只负责赋值与绑定） ----------
    function fmtWhen(ms) {
      var n = Number(ms);
      if (!isFinite(n) || n <= 0) return "";
      try {
        return new Date(n).toLocaleString();
      } catch (e) {
        return String(ms);
      }
    }

    var SEC_DEFS = [
      { key: "human", no: "①", title: "等待 Human 结果", hint: "团队 awaiting_human / 人节点任务未完成", empty: "暂无等待人工结果的任务" },
      { key: "review", no: "②", title: "待评审产物", hint: "pending_review + 校验未通过（GET /projects/{pid}/artifacts）", empty: "暂无待评审产物" },
      { key: "failed", no: "③", title: "失败步骤（可重试）", hint: "Failed/Aborted · POST /teams/{id}/steer retry", empty: "暂无可重试的失败步骤" },
      { key: "lease", no: "④", title: "写租约持有", hint: "write_lease 未释放（单写租约）", empty: "当前无团队持有写租约" },
    ];

    function itemHtml(item, busy, results) {
      var b = busy || {};
      var r = results || {};
      if (item.kind === "human") {
        var tasks = item.tasks || [];
        return (
          '<div class="owo-ac-item" data-kind="human">' +
          '<div class="owo-ac-line">' +
          '<span class="owo-ac-badge warn">等待人节点</span>' +
          "<b>" + esc(item.team_id || "—") + "</b>" +
          '<span class="hint">' + (item.awaiting_team ? "团队 awaiting_human · " : "") + tasks.length + " 个未完成人节点</span>" +
          "</div>" +
          (tasks.length
            ? '<div class="owo-ac-sub hint">' + tasks.map(function (t) {
                return esc(t.role || t.task_id) + "（" + esc(t.status) + "）";
              }).join("、") + "</div>"
            : "") +
          '<div class="owo-ac-actions">' +
          '<button type="button" class="owo-ac-mini primary" data-ac-goto="' + esc(item.team_id) + '">进入团队提交结果</button>' +
          '<span class="hint">在团队详情「人节点结果」提交（POST /tasks/{id}/human-result）</span>' +
          "</div></div>"
        );
      }
      if (item.kind === "review") {
        var badge = item.validation_failed
          ? '<span class="owo-ac-badge bad">校验未通过</span>'
          : '<span class="owo-ac-badge ok">待评审</span>';
        var what = (item.kind_name || item.format || "artifact") + (item.version ? " · v" + esc(item.version) : "");
        return (
          '<div class="owo-ac-item" data-kind="review">' +
          '<div class="owo-ac-line">' + badge +
          "<b>" + esc(item.artifact_id) + "</b>" +
          '<span class="hint">' + what + " · 项目 " + esc(item.project_id) + "</span>" +
          "</div>" +
          (item.validation_failed
            ? '<div class="owo-ac-sub bad">校验：' + esc(item.validation_reason || "格式校验未通过") + "</div>"
            : "") +
          '<div class="owo-ac-actions">' +
          '<button type="button" class="owo-ac-mini primary" data-ac-goto-artifact data-team="' + esc(item.team_id) + '">进入评审</button>' +
          "</div></div>"
        );
      }
      if (item.kind === "failed") {
        var key = String(item.team_id) + "/" + String(item.step_id);
        var isBusy = !!b[key];
        var res = r[key];
        return (
          '<div class="owo-ac-item" data-kind="failed">' +
          '<div class="owo-ac-line">' +
          '<span class="owo-ac-badge bad">失败步骤</span>' +
          "<b>" + esc(item.step_id) + "</b>" +
          '<span class="hint">' + esc(item.role || "—") + " · 第 " + esc(item.attempts) + " 次尝试 · 团队 " + esc(item.team_id) + (item.interrupted ? " · 已中断" : "") + "</span>" +
          "</div>" +
          (item.error ? '<div class="owo-ac-sub bad">' + esc(item.error) + "</div>" : "") +
          '<div class="owo-ac-actions">' +
          '<button type="button" class="owo-ac-mini" data-ac-retry data-ac-key="' + esc(key) + '" data-team="' + esc(item.team_id) + '" data-step="' + esc(item.step_id) + '"' + (isBusy ? " disabled" : "") + ">" + (isBusy ? "重试中…" : "重试") + "</button>" +
          '<button type="button" class="owo-ac-mini" data-ac-goto="' + esc(item.team_id) + '">进入团队</button>' +
          "</div>" +
          '<div class="owo-ac-result' + (res ? (res.ok ? " ok" : " bad") : "") + '" data-ac-result="' + esc(key) + '" aria-live="polite">' + (res ? esc(res.text) : "") + "</div>" +
          "</div>"
        );
      }
      if (item.kind === "lease") {
        return (
          '<div class="owo-ac-item" data-kind="lease">' +
          '<div class="owo-ac-line">' +
          '<span class="owo-ac-badge info">写租约持有中</span>' +
          "<b>" + esc(item.team_id || "—") + "</b>" +
          '<span class="hint">' + esc(item.holder_role) + " · 步骤 " + esc(item.holder_step_id) + (item.acquired_at_ms ? " · " + esc(fmtWhen(item.acquired_at_ms)) + " 起" : "") + "</span>" +
          "</div>" +
          '<div class="owo-ac-actions">' +
          '<button type="button" class="owo-ac-mini" data-ac-goto="' + esc(item.team_id) + '">进入团队</button>' +
          "</div></div>"
        );
      }
      return "";
    }

    function sectionsHtml(items, st) {
      items = items || {};
      var busy = (st && st.retryBusy) || {};
      var results = (st && st.retryResults) || {};
      return SEC_DEFS.map(function (def) {
        var list = items[def.key] || [];
        var body = list.length
          ? list.map(function (item) { return itemHtml(item, busy, results); }).join("")
          : '<div class="owo-ac-empty">' + esc(def.empty) + "</div>";
        return (
          '<section class="owo-ac-sec" data-ac-sec="' + def.key + '">' +
          "<h3>" + def.no + " " + esc(def.title) +
          ' <span class="owo-ac-count' + (list.length ? " has" : "") + '">' + list.length + "</span>" +
          ' <span class="hint">' + esc(def.hint) + "</span></h3>" +
          '<div class="owo-ac-items">' + body + "</div>" +
          "</section>"
        );
      }).join("");
    }

    function shellHtml() {
      return (
        '<div class="owo-ac">' +
        '<div class="owo-ac-head">' +
        "<h2>待我处理</h2>" +
        '<span class="hint">客户端聚合既有路由（无新增接口）：/teams · /teams/{id} · /projects/{pid}/artifacts · steer(retry)</span>' +
        '<button type="button" id="ac-refresh" class="owo-ac-mini">刷新</button>' +
        '<span id="ac-meta" class="hint" aria-live="polite"></span>' +
        "</div>" +
        '<div id="ac-errors" aria-live="polite"></div>' +
        '<div id="ac-sections"><div class="owo-ac-empty">正在聚合待办…</div></div>' +
        "</div>"
      );
    }

    // ---------- 行为层 ----------
    function totalOf(items) {
      items = items || {};
      return (items.human || []).length + (items.review || []).length + (items.failed || []).length + (items.lease || []).length;
    }

    function paintMeta(text) {
      var m = el("#ac-meta");
      if (m) m.textContent = text;
    }

    function paintErrors() {
      var box = el("#ac-errors");
      if (!box) return;
      var lines = state.errors.slice();
      Object.keys(state.detailErrors || {}).forEach(function (tid) {
        lines.push("团队 " + tid + " 详情加载失败：" + state.detailErrors[tid]);
      });
      Object.keys(state.artifacts || {}).forEach(function (pid) {
        var entry = state.artifacts[pid];
        if (entry && entry.error) lines.push("项目 " + pid + " 产物加载失败：" + entry.error);
      });
      box.innerHTML = lines.map(function (x) { return '<div class="owo-ac-err">' + esc(x) + "</div>"; }).join("");
    }

    function render() {
      if (!rootEl) return;
      var box = el("#ac-sections");
      if (box) box.innerHTML = sectionsHtml(state.items, state);
      paintErrors();
      paintMeta(
        state.loading
          ? "加载中…"
          : "共 " + totalOf(state.items) + " 项待办" + (state.lastLoadedAt ? " · 更新于 " + state.lastLoadedAt : "")
      );
    }

    // 提交锁 + 结果回写（按钮/结果区由 sectionsHtml 重建，直接按 data 属性定位）。
    function paintRetry(key) {
      var box = el("#ac-sections");
      if (!box) return;
      var res = state.retryResults[key];
      var div = box.querySelector('[data-ac-result="' + key.replace(/"/g, '\\"') + '"]');
      if (div) {
        div.textContent = res ? res.text : "";
        div.className = "owo-ac-result" + (res ? (res.ok ? " ok" : " bad") : "");
      }
      if (!state.retryBusy[key]) {
        var btn = box.querySelector('[data-ac-retry][data-ac-key="' + key.replace(/"/g, '\\"') + '"]');
        if (btn) {
          btn.disabled = false;
          btn.textContent = "重试";
        }
      }
    }

    function submitRetry(teamId, stepId) {
      var key = String(teamId) + "/" + String(stepId);
      if (state.retryBusy[key]) return Promise.resolve();
      state.retryBusy[key] = true;
      delete state.retryResults[key];
      paintRetry(key);
      var btn = el('#ac-sections [data-ac-retry][data-ac-key="' + key.replace(/"/g, '\\"') + '"]');
      if (btn) {
        btn.disabled = true;
        btn.textContent = "重试中…";
      }
      return H.post("/teams/" + encodeURIComponent(teamId) + "/steer", buildRetryBody(stepId))
        .then(function () {
          state.retryResults[key] = { ok: true, text: "重试指令已发送（command=retry），等待步骤重新调度。" };
        })
        .catch(function (e) {
          state.retryResults[key] = { ok: false, text: "重试失败：" + friendly(e) };
        })
        .then(function () {
          delete state.retryBusy[key];
          paintRetry(key);
          // 成功后重载聚合：失败步骤应随团队状态推进而消失。
          if (state.retryResults[key] && state.retryResults[key].ok) return load();
        });
    }

    // 深链：经导航按钮切到 WorkSwarm 面板，再用公开 open() 直达团队详情
    // （与 Project Launcher gotoTeam 同款）。teamId 为空时仅落在列表页。
    function gotoWorkswarm(teamId) {
      var doc = typeof document !== "undefined" ? document : null;
      var btn = doc ? doc.querySelector('#panelNav button[data-panel="workswarm"]') : null;
      if (btn) btn.click();
      var ws = win.OwoPanels && win.OwoPanels.workswarm;
      if (ws && typeof ws.open === "function" && teamId) {
        // open 内部有异步加载；给挂载一帧时间。
        setTimeout(function () {
          try {
            ws.open(teamId);
          } catch (e) {
            /* 面板打开失败不阻塞本面板 */
          }
        }, 60);
      }
    }

    function load() {
      if (state.loading) return Promise.resolve();
      state.loading = true;
      state.errors = [];
      paintMeta("加载中…");
      return H.get("/teams")
        .then(function (d) {
          state.teams = (d && d.teams) || [];
          var details = {};
          var detailErrors = {};
          return Promise.all(
            candidateTeams(state.teams).map(function (t) {
              return H.get("/teams/" + encodeURIComponent(t.team_id)).then(
                function (dd) {
                  details[String(t.team_id)] = dd || {};
                },
                function (e) {
                  detailErrors[String(t.team_id)] = friendly(e);
                }
              );
            })
          ).then(function () {
            state.details = details;
            state.detailErrors = detailErrors;
            var artifacts = {};
            return Promise.all(
              projectIdsOf(state.teams).map(function (p) {
                return H.get("/projects/" + encodeURIComponent(p.pid) + "/artifacts").then(
                  function (ad) {
                    artifacts[p.pid] = { items: (ad && ad.artifacts) || [], team_id: p.team_id, error: "" };
                  },
                  function (e) {
                    var msg = friendly(e);
                    var is404 = /^404/.test(String((e && e.message) || e || ""));
                    // 项目尚无产物（404）→ 静默视为空；其余错误记录展示。
                    artifacts[p.pid] = { items: [], team_id: p.team_id, error: is404 ? "" : msg };
                  }
                );
              })
            ).then(function () {
              state.artifacts = artifacts;
              finishLoad();
            });
          });
        })
        .catch(function (e) {
          state.errors.push("团队列表加载失败：" + friendly(e));
          finishLoad();
        });
    }

    function finishLoad() {
      state.loading = false;
      state.loadedOnce = true;
      state.items = aggregate(state);
      state.lastLoadedAt = new Date().toLocaleTimeString();
      render();
    }

    function onSectionClick(ev) {
      var t = ev.target;
      if (!t || !t.closest) return;
      var retryBtn = t.closest("[data-ac-retry]");
      if (retryBtn) {
        submitRetry(retryBtn.getAttribute("data-team") || "", retryBtn.getAttribute("data-step") || "");
        return;
      }
      var goBtn = t.closest("[data-ac-goto]");
      if (goBtn) {
        gotoWorkswarm(goBtn.getAttribute("data-ac-goto") || "");
        return;
      }
      var artBtn = t.closest("[data-ac-goto-artifact]");
      if (artBtn) gotoWorkswarm(artBtn.getAttribute("data-team") || "");
    }

    function mount(root, helpers) {
      rootEl = root;
      H = helpers || {};
      H.baseUrl = H.baseUrl || (win.OwoPanels && win.OwoPanels.baseUrl) || "";
      if (!H.get) H.get = defaultGet;
      if (!H.post) H.post = defaultPost;
      if (!H.esc) H.esc = defaultEsc;
      root.innerHTML = shellHtml();
      var box = el("#ac-sections");
      // 事件委托绑在持久容器 #ac-sections 上：innerHTML 重建子树不丢监听，
      // 重复挂载也不会累积（容器元素随 root.innerHTML 整体重建）。
      if (box) box.addEventListener("click", onSectionClick);
      var refresh = el("#ac-refresh");
      if (refresh) refresh.onclick = function () { load(); };
      load();
    }

    // ---------- 测试挂钩 ----------
    var TEST_API = {
      state: state,
      normStatus: normStatus,
      isRetryableStep: isRetryableStep,
      candidateTeams: candidateTeams,
      projectIdsOf: projectIdsOf,
      humanTasksOf: humanTasksOf,
      humanItemsOf: humanItemsOf,
      failedItemsOf: failedItemsOf,
      leaseOf: leaseOf,
      leaseItemsOf: leaseItemsOf,
      normReviewState: normReviewState,
      reviewItemsOf: reviewItemsOf,
      aggregate: aggregate,
      buildRetryBody: buildRetryBody,
      itemHtml: itemHtml,
      sectionsHtml: sectionsHtml,
      shellHtml: shellHtml,
      render: render,
      load: load,
      submitRetry: submitRetry,
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
      title: "待我处理",
      mount: mount,
      _test: TEST_API,
    };
  })();
})();

// Node（CommonJS）测试环境导出；浏览器无 module 定义，此分支不生效、零运行时差异。
if (typeof module !== "undefined" && module.exports) {
  var __acWin = typeof window !== "undefined" ? window : globalThis;
  module.exports = __acWin.OwoPanels["action-center"];
}
