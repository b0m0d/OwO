// ============================================================================
// WorkSwarm（团队编排）面板 —— desktop/web/panels/workswarm.panel.js
//
// 纯脚本 IIFE，注册 window.OwoPanels.workswarm。
// app.js 的 panelHelpers() 缺失时自建 get/post/esc/friendlyError 回退
// （与 fleet / goal 面板一致的防御式写法）。
//
// API 以 owo-agent-server（workswarm_api.rs）实际路由为准，不发明字段：
//   POST /teams                          创建团队 {objective, mode?, template_id?, roles[]} → 202
//   GET  /teams                          列表 {teams:[TeamRun + active]}
//   GET  /teams/{id}                     详情 {team, tasks:task_view[], audit_tail[]}
//   GET  /teams/{id}/tasks               {team_id, tasks:task_view[]}
//   GET  /teams/{id}/events              SSE：open / state{status,active} / audit{ts,event,detail}；
//                                        ?format=json 一次性快照 {team_id,status,active,audit}
//   POST /teams/{id}/steer               {command: continue|steer|replace|cancel, step_id?, new_input?, note?, role?, new_worker?, new_user_id?}
//   GET  /projects/{pid}/artifacts       产物 {project_id, artifacts[]}
//   POST /tasks/{id}/handoff             交接记录 {team_id, from_member, to_member?, completed_summary, ...}
//   POST /tasks/{id}/human-result        人节点结果 {team_id, result} → {artifact}
//   GET  /teams/templates                已采纳模板 {templates[]}
//   GET  /teams/templates/proposals      模板提案 {proposals[]}
//   POST /teams/templates/proposals/{pid}/adopt    采纳 → {template}
//   POST /teams/templates/proposals/{pid}/reject   拒绝 → {proposal_id, status}
//
// 说明：/teams/* 为受保护路由，仅识别 Authorization: Bearer 头；浏览器
// EventSource 无法携带请求头，SSE 若出错（如 401）本面板自动降级为 2.5s 轮询
// （GET /teams/{id}/events?format=json + GET /teams/{id}），审计事件按
// ts|event|detail 去重，两种通道可无缝拼接。
//
// 修复/体验记录：
// - el() 同时接受裸 ID 与 "#selector"。历史版本对 "#id" 再拼一次 "#"，生成
//   非法选择器 "##id"，querySelector 抛 SyntaxError 使首次挂载中断 → 面板空白。
// - 写操作按钮（创建/steer/cancel/换员/人节点提交/模板采纳拒绝/各刷新）
//   均带提交锁（data-busy + disabled + 文案变化），杜绝双击重复提交。
// - 团队终态时禁用 continue/steer/replace/cancel 与人节点结果提交并给出
//   门控横幅；离开详情视图立即停止 SSE/轮询，不留幽灵请求。
// - 列表/详情/成员/任务 DAG/产物/模板/提案具备「加载中 / 空 / 失败+重试」三态。
// - 结果区与实时通道标注 aria-live；键盘焦点可见；审计区独立限高滚动；
//   状态徽标 / 阻塞计数 / 实时通道使用分级视觉层级。
//
// 二轮（V1-R2 联动，2026-08-27）：
// - 团队详情顶部新增「运行摘要」：状态/活动阶段、成功/失败/等待/运行/阻塞计数、
//   当前失败步骤（含尝试次数与错误摘要）、累计尝试、产物数量。
// - Failed/Aborted 节点与摘要行提供「重试此节点」，走第三路冻结契约
//   POST /teams/{id}/steer {"command":"retry","step_id":...,"note":...}；
//   提交锁防双击重复请求，终态成功/取消团队不渲染重试入口。
// - 重启遗留中断态（服务端 interrupted 标记，列表/详情/SSE/轮询四通道一致接线）
//   显示「运行已中断，可恢复」，不再呈现为正常执行中。
// - 视觉层级与长文本治理：摘要统计卡、成员/DAG/产物/审计省略+完整 tooltip、
//   表格横向滚动包裹；防溢出与重试/中断样式在 style.css 第 16 节（.owo-ws-* 作用域）。
// - Node 兼容：window 回退 globalThis + module.exports 导出 _test 纯逻辑挂钩，
//   供 tests/workswarm.panel.test.mjs 做结构/行为断言（浏览器零差异）。
// ============================================================================
(function () {
  "use strict";

  // Node 测试环境兼容：window 未定义时回退 globalThis（浏览器行为不变）。
  // 文件末尾按需导出 module.exports，供 tests/workswarm.panel.test.mjs 使用。
  var win = typeof window !== "undefined" ? window : globalThis;

  win.OwoPanels = win.OwoPanels || {};

  win.OwoPanels.workswarm = (function () {
    var ID = "workswarm";

    // ---------- helpers（优先 app.js 注入，缺失时自建回退） ----------
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

    function esc(s) {
      return H.esc ? H.esc(s) : defaultEsc(s);
    }

    // ---------- 状态 ----------
    var state = {
      view: "list", // create | list | templates | detail
      teams: [],
      current: null, // 当前详情 team_id
      team: null,
      tasks: [],
      audit: [], // 最新在前
      auditKeys: {},
      active: false,
      teamStatus: "", // 最新状态（可能是 Debug 形式，展示前统一归一化）
      interrupted: false, // 服务端中断标记：磁盘 Running 但无活动运行（R2）
      artifactCount: null, // 产物数量（loadArtifacts 成功后回填；null=未知）
      streamNote: "",
      liveMode: "off", // sse | poll | off
      es: null,
      pollTimer: null,
      pollNoteShown: false,
      refreshDebounce: null,
      templates: [],
      proposals: [],
      humanTask: "",
      // —— 四期：实时进度（第二路 progress 事件）——
      progress: null, // 最新 progress 事件 {seq,status,active,current_steps[],counts{},updated_at}
      lastProgressSeq: 0, // 单调递增守卫：断线重连/轮询快照里的旧 seq 一律跳过（不重置于重连）
      progressTimer: null, // 1s tick：刷新"已运行时间"（仅在存在 Running 步骤时活跃）
      cancelling: false, // 用户点了取消：立即置位（不等服务端），终态/确认后清除
      reviewBusy: {}, // artifact_id -> true（评审提交进行中，按钮锁定）
      artifacts: [], // 最近一次 loadArtifacts 的产物数组（findArtifactById / 评审提交依赖）
      reviewResult: null, // 最近一次评审提交结果（测试与 DOM 提示共用）
      reviewFlash: null, // {artifactId, ok, text} 评审结果闪存：产物区重载后仍显示
    };

    // ---------- 常量与工具 ----------
    var TEAM_STATUS_CN = {
      created: "已创建",
      running: "运行中",
      awaiting_human: "等待人节点",
      succeeded: "已成功",
      failed: "失败",
      cancelled: "已取消",
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
    // "死上游"= 失败/中止（下游不会再被运行）。历史版本错用含 succeeded 的
    // STEP_TERMINAL 判定，导致"上游刚完成、下游等待运行"的任务被误标为
    // 已阻塞、依赖边被误涂红。
    var STEP_DEAD = { failed: true, aborted: true };

    function isDeadStatus(s) {
      return !!STEP_DEAD[normStatus(s)];
    }

    function taskBlocked(t, byId) {
      if (!t) return false;
      var st = normStatus(t.status);
      if (STEP_TERMINAL[st]) return false;
      return (t.depends_on || []).some(function (d) {
        return byId[d] && isDeadStatus(byId[d].status);
      });
    }

    // 团队状态在 TeamRun 里是 snake_case（serde），创建响应/SSE state 帧是
    // Debug 形式（Created/Running/AwaitingHuman/...）——统一小写归一化。
    function normStatus(s) {
      var x = String(s || "").toLowerCase();
      if (x === "awaitinghuman") return "awaiting_human";
      return x;
    }

    function isTerminalTeam(s) {
      return !!TERMINAL[normStatus(s)];
    }

    // ---------- 二轮纯逻辑层（Node 测试挂钩覆盖） ----------
    // 可重试步骤：与核心 steer_retry 的目标闸门一致——仅 Failed / Aborted
    // （中断识别会把遗留 Running 转 Aborted，因此天然落在同一集合）。
    var STEP_RETRYABLE = { failed: true, aborted: true };

    function isRetryableStep(t) {
      return !!t && !!STEP_RETRYABLE[normStatus(t.status)];
    }

    function retryableTasks(tasks) {
      return (tasks || []).filter(isRetryableStep);
    }

    // 重试入口是否可见：团队 succeeded/cancelled 一律不提供（终态无效操作）；
    // 其余状态只要存在 Failed/Aborted 节点即提供（运行中节点级 409 由服务端把关，
    // 前端不隐藏——否则中断/暂停窗口下的合法恢复会被误藏）。
    function shouldShowRetry(tasks, teamStatus) {
      var st = normStatus(teamStatus);
      if (st === "succeeded" || st === "cancelled") return false;
      return retryableTasks(tasks).length > 0;
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

    // 运行摘要（纯函数）：状态/阶段、任务计数、失败步骤、累计尝试、产物数、重试可见性。
    // 输入 {team, tasks, artifactCount, interrupted, active}，输出展示模型。
    function computeRunSummary(input) {
      var team = (input && input.team) || {};
      var tasks = (input && input.tasks) || [];
      var byId = {};
      tasks.forEach(function (t) {
        byId[t.task_id] = t;
      });
      var counts = { total: tasks.length, succeeded: 0, failed: 0, waiting: 0, running: 0, blocked: 0 };
      var totalAttempts = 0;
      var failedStep = null;
      var runningStep = null;
      tasks.forEach(function (t) {
        var st = normStatus(t.status);
        var at = Number(t.attempts || 0);
        totalAttempts += at;
        if (st === "succeeded") counts.succeeded++;
        else if (st === "failed" || st === "aborted") {
          counts.failed++;
          if (!failedStep)
            failedStep = {
              task_id: t.task_id,
              role: t.role || t.worker || t.task_id,
              attempts: at,
              error: t.error || "",
            };
        } else if (st === "running") {
          counts.running++;
          if (!runningStep) runningStep = t;
        } else counts.waiting++; // pending / ready / 未知
        if (taskBlocked(t, byId)) counts.blocked++;
      });
      var interrupted = !!(input && input.interrupted);
      var rawStatus = team.status != null ? String(team.status) : "";
      var st = normStatus(rawStatus);
      var terminal = isTerminalTeam(st);
      var interruptedView = interrupted && !terminal;
      var phase;
      if (interruptedView) phase = "已中断，可恢复";
      else if (st === "awaiting_human") phase = "等待人节点结果";
      else if (runningStep) phase = "执行：" + (runningStep.role || runningStep.worker || runningStep.task_id);
      else if (st === "succeeded") phase = "全部步骤已完成";
      else if (counts.failed > 0) phase = "已停止：存在失败步骤";
      else if (st === "running") phase = counts.total ? "调度中" : "等待任务图生成";
      else if (st === "created") phase = counts.total ? "准备启动" : "尚未开始";
      else if (st === "cancelled") phase = "已取消";
      else if (st === "failed") phase = "已失败";
      else phase = "—";
      return {
        statusKey: interruptedView ? "interrupted" : st,
        statusLabel: interruptedView ? "已中断（可恢复）" : TEAM_STATUS_CN[st] || rawStatus || "未知",
        phase: phase,
        counts: counts,
        totalAttempts: totalAttempts,
        failedStep: failedStep,
        artifactCount: input && input.artifactCount != null ? input.artifactCount : null,
        canRetry: shouldShowRetry(tasks, st),
      };
    }

    // ---------- 三态（加载/空/失败）、提交锁、终态门控、重试委托 ----------
    // 生成统一的加载中 / 失败状态片段；retryKind 注册到重试委托。
    function stateBox(kind, msg, retryKind) {
      var cls = kind === "error" ? "hint err" : kind === "loading" ? "hint owo-ws-loading" : "hint";
      var icon = kind === "error" ? "⚠ " : kind === "loading" ? "⏳ " : "";
      var html = '<div class="' + cls + '">' + icon + esc(msg) + "</div>";
      if (retryKind)
        html +=
          '<div style="margin-top:4px"><button type="button" class="owo-ws-mini" data-retry="' +
          esc(retryKind) +
          '">↻ 重试</button></div>';
      return html;
    }

    function retryAction(kind) {
      if (kind === "teams") loadTeams();
      else if (kind === "templates") loadTemplates();
      else if (kind === "proposals") loadProposals();
      else if (kind === "artifacts") loadArtifacts();
      else if (kind === "tasks") refreshTasksOnly();
      else if (kind === "detail" && state.current) loadDetail(state.current);
    }

    // —— 节点重试（第三路冻结契约） ——
    // submitRetry 返回 Promise（成功/失败都展示结果）；终态成功/取消团队直接拒绝。
    function submitRetry(stepId) {
      if (!state.current) return Promise.resolve();
      var st = normStatus(currentTeamStatus());
      if (st === "succeeded" || st === "cancelled") {
        show(
          "ws-act-result",
          "err",
          "团队已进入终态（" + (TEAM_STATUS_CN[st] || st) + "），重试不可用；如需重跑请新建团队"
        );
        return Promise.resolve();
      }
      var body = buildRetryBody(stepId);
      return H.post("/teams/" + encodeURIComponent(state.current) + "/steer", body)
        .then(function (d) {
          state.interrupted = !!(d && d.interrupted); // retry 恢复会清除中断标记
          var dst = normStatus(d && d.status);
          show(
            "ws-act-result",
            "ok",
            "[重试] " +
              stepId +
              " 已受理：仅重置该节点及其未完成下游（已成功步骤、Artifact、Handoff、DecisionRecord 保持不变），运行循环已重启。团队状态：" +
              (TEAM_STATUS_CN[dst] || (d && d.status) || "—")
          );
          syncDetail();
          refreshTasksOnly();
        })
        .catch(function (e) {
          show("ws-act-result", "err", explainError(e, "重试节点 " + stepId));
        });
    }

    // 重试按钮点击入口（事件委托调用；测试挂钩直接调用）：
    // 入口处同步置提交锁——快速双击的第二击读到 data-busy=1 即被丢弃。
    function handleRetryClick(btn) {
      if (!btn || btn.disabled || btn.getAttribute("data-busy") === "1") return false;
      var stepId = btn.getAttribute("data-ws-retry");
      if (stepId == null || String(stepId) === "") return false;
      lockBtn(btn, "重试中…");
      var p = submitRetry(String(stepId));
      if (p && typeof p.then === "function") {
        p.then(
          function () {
            unlockBtn(btn);
          },
          function () {
            unlockBtn(btn);
          }
        );
      } else {
        unlockBtn(btn);
      }
      return true;
    }

    // 重试按钮走事件委托：绑定在面板 section 上（随 innerHTML 重建一起销毁），
    // 绝不能绑在 rootEl 上——app.js 用同一个 #panelRoot 反复挂载各面板，
    // 挂在根上的监听器会在多次切换后累积并污染其他面板。
    function bindDelegates(section) {
      if (!section || !section.addEventListener) return;
      section.addEventListener("click", function (ev) {
        var t = ev.target;
        if (!t || !t.getAttribute) return;
        var r = t.getAttribute("data-retry");
        if (r != null && !t.disabled) {
          ev.preventDefault();
          retryAction(String(r));
          return;
        }
        var rt = t.getAttribute("data-ws-retry");
        if (rt != null) {
          ev.preventDefault();
          handleRetryClick(t); // 忙/禁用守卫与提交锁在入口内
        }
      });
    }

    function lockBtn(btn, busyLabel) {
      if (!btn) return;
      btn.setAttribute("data-busy", "1");
      btn.setAttribute("aria-busy", "true");
      btn.__idleText = btn.textContent;
      btn.disabled = true;
      if (busyLabel) btn.textContent = busyLabel;
    }

    function unlockBtn(btn) {
      if (!btn) return;
      btn.removeAttribute("data-busy");
      btn.removeAttribute("aria-busy");
      btn.disabled = false;
      if (btn.__idleText != null) btn.textContent = btn.__idleText;
      applyGating(); // 解锁后按最新状态重新应用终态门控
    }

    // 绑定带提交锁的按钮：makePromise 返回待执行的 Promise；
    // 返回空值表示校验未通过或用户取消（不进入提交态）。双击/连点被丢弃。
    function bindLockedButton(btn, makePromise, busyLabel) {
      if (!btn) return;
      btn.addEventListener("click", function () {
        if (btn.disabled || btn.getAttribute("data-busy") === "1") return;
        var p;
        try {
          p = makePromise();
        } catch (e) {
          // 校验函数内部已展示错误，这里兜底避免未捕获异常
          if (typeof console !== "undefined" && console.error) console.error("[workswarm] 按钮处理异常:", e);
          return;
        }
        if (!p || typeof p.then !== "function") return;
        lockBtn(btn, busyLabel);
        p.then(
          function () {
            unlockBtn(btn);
          },
          function () {
            unlockBtn(btn);
          }
        );
      });
    }

    // 终态团队不再允许的操作按钮；终态时禁用并解释原因。
    var TEAM_ACTION_BTNS = ["ws-act-continue", "ws-act-cancel", "ws-act-steer-go", "ws-act-replace-go"];

    function currentTeamStatus() {
      return state.teamStatus || ((state.team && state.team.status) || "");
    }

    function applyGating() {
      var term = isTerminalTeam(currentTeamStatus());
      TEAM_ACTION_BTNS.forEach(function (id) {
        var b = el(id);
        if (!b) return;
        if (b.getAttribute("data-busy") === "1") return; // 提交中的锁优先
        b.disabled = term;
        b.title = term ? "团队已进入终态，该操作不可用" : "";
      });
      var hGo = el("ws-h-go"); // 人节点结果提交：终态禁用；没有未完成的人节点任务也禁用
      if (hGo && hGo.getAttribute("data-busy") !== "1") {
        var noHuman = humanTasks().length === 0;
        hGo.disabled = term || noHuman;
        hGo.title = term
          ? "团队已进入终态，人节点结果提交不可用"
          : noHuman
            ? "当前没有未完成的人节点任务"
            : "";
      }
      var xGo = el("ws-x-go"); // 任务交接：终态禁用
      if (xGo && xGo.getAttribute("data-busy") !== "1") {
        xGo.disabled = term;
        xGo.title = term ? "团队已进入终态，交接不可用" : "";
      }
      // 节点重试按钮：仅 succeeded / cancelled 终态隐藏为禁用（failed 终态正是
      // retry 的合法场景）；提交中的锁优先。渲染层已按 shouldShowRetry 控制可见性。
      var noRetry = (function () {
        var s = normStatus(currentTeamStatus());
        return s === "succeeded" || s === "cancelled";
      })();
      var rbtns = rootEl ? rootEl.querySelectorAll("[data-ws-retry]") : [];
      for (var ri = 0; ri < rbtns.length; ri++) {
        var rb = rbtns[ri];
        if (rb.getAttribute("data-busy") === "1") continue;
        rb.disabled = noRetry;
        rb.title = noRetry
          ? "团队已成功/已取消，无可重试节点"
          : "重置此节点及其未完成下游；已成功步骤、产物与交接保持不变";
      }
      var gate = el("ws-d-gate");
      if (gate) gate.style.display = term ? "" : "none";
    }

    // 结果区与实时通道标注 aria-live，供读屏器播报状态变化。
    function markLiveRegions() {
      ["ws-create-result", "ws-act-result", "ws-h-result", "ws-x-result"].forEach(function (id) {
        var n = el(id);
        if (n) {
          n.setAttribute("aria-live", "polite");
          n.setAttribute("role", "status");
        }
      });
      var s = el("ws-d-stream");
      if (s) s.setAttribute("aria-live", "polite");
      var dErr = el("ws-d-error");
      if (dErr) dErr.setAttribute("aria-live", "assertive");
      var badge = el("ws-d-status");
      if (badge) badge.setAttribute("aria-live", "polite");
    }

    // 同时接受裸 ID（"ws-main"）与 "#selector"（"#ws-main"）。
    // 历史 bug：调用方传 "#id" 时这里会拼出 "##id"，querySelector 直接抛
    // SyntaxError，renderView()/mount() 随之中断，面板首次打开即空白。
    // 统一归一化后查询；对残留的非法选择器兜底返回 null 而不是抛异常。
    function el(sel) {
      if (!rootEl || sel == null) return null;
      var q = String(sel);
      if (!q) return null;
      if (q.charAt(0) !== "#") q = "#" + q;
      try {
        return rootEl.querySelector(q);
      } catch (e) {
        return null;
      }
    }

    function short(s, n) {
      s = String(s == null ? "" : s);
      return s.length > n ? s.slice(0, n) + "…" : s;
    }

    function splitCsv(input) {
      var v = input && input.value;
      if (!v) return [];
      return v
        .split(",")
        .map(function (x) {
          return x.trim();
        })
        .filter(Boolean);
    }

    // show() 的 id 约定为裸 ID（如 "ws-act-result"），交由 el() 补 "#"，
    // 避免出现 "##id"；保留元素原有的 "sub" 修饰类不被覆盖。
    function show(id, kind, text) {
      var box = el(id);
      if (!box) return;
      box.textContent = text || "—";
      box.className =
        "owo-ws-result" +
        (box.classList.contains("sub") ? " sub" : "") +
        (kind === "ok" ? " ok" : kind === "err" ? " err" : "");
    }

    // 可操作的错误文案：解析 api() 抛出的 "NNN: body"，4xx 给出具体指引。
    // 原始响应体超长时截断（保留 full 参数回传），避免整段 HTML/日志刷屏。
    function explainError(err, ctx) {
      var msg = String((err && err.message) || err || "");
      var m = msg.match(/^(\d{3}):/);
      var extra = ctx ? "（" + ctx + "）" : "";
      if (m) {
        var st = Number(m[1]);
        var raw = msg.slice(m[0].length).trim();
        var detail = "";
        try {
          var o = JSON.parse(raw);
          if (o && (o.error || o.message)) detail = String(o.error || o.message);
        } catch (e) {
          detail = raw;
        }
        if (/OPENAI_API_KEY/i.test(detail)) {
          detail += "。提示：请为 owo-agent-server 设置 OPENAI_API_KEY 环境变量后重启服务";
        }
        detail = short(detail || raw, 240);
        switch (st) {
          case 400:
            return "请求被拒绝（400）：" + (detail || "参数不合法") + extra;
          case 401:
            return "认证失败（401）：访问令牌可能已失效，请刷新页面重新获取后重试" + extra;
          case 403:
            return "权限不足（403）：" + (detail || "当前令牌无权执行该操作，请联系管理员或检查角色配置") + extra;
          case 404:
            return "资源不存在（404）：" + (detail || "") + "。请检查团队/任务 ID 是否正确，或刷新团队列表" + extra;
          case 409:
            return "状态冲突（409）：" + (detail || "") + "。团队可能已终结或正在运行/等待人工，无法重复该操作；可先刷新状态再决定" + extra;
          case 500:
            return "服务内部错误（500）：" + (detail || "") + "。请稍后重试；若持续出现请查看 owo-agent-server 日志" + extra;
          case 502:
          case 503:
          case 504:
            return "服务暂不可用（HTTP " + st + "）：" + (detail || "") + "。请确认服务进程存活、端口未被占用后再重试" + extra;
          default:
            return "服务端错误（HTTP " + st + "）：" + (detail || "") + extra;
        }
      }
      if (msg === "Failed to fetch" || /network|CORS|load failed/i.test(msg)) {
        return "无法连接 owo-agent-server（" + (H.baseUrl || "http://127.0.0.1:4096") + "）：请确认服务已启动（如 cd agent-sdk && cargo run -p owo-agent-server），并检查端口与防火墙设置" + extra;
      }
      if (H.friendlyError) {
        try {
          return H.friendlyError(err, { resource: true });
        } catch (e2) {
          /* 回退原文 */
        }
      }
      // 原写法 msg || "未知错误" + extra 有优先级问题：有 msg 时会丢掉上下文。
      return (msg || "未知错误") + extra;
    }

    // ---------- 审计事件（去重） ----------
    function addAudit(entry) {
      if (!entry || !entry.event) return;
      var key = [entry.ts, entry.event, entry.detail].join("|");
      if (state.auditKeys[key]) return;
      state.auditKeys[key] = true;
      state.audit.unshift(entry);
      if (state.audit.length > 300) {
        var dropped = state.audit.pop();
        delete state.auditKeys[[dropped.ts, dropped.event, dropped.detail].join("|")];
      }
    }

    function paintAudit() {
      var box = el("#ws-d-audit");
      if (!box) return;
      if (!state.audit.length) {
        box.innerHTML = '<div class="hint">暂无审计事件</div>';
        return;
      }
      box.innerHTML = state.audit
        .map(function (e) {
          return (
            '<div class="owo-ws-audit-row">' +
            '<span class="owo-ws-audit-ts">' + esc(e.ts) + "</span>" +
            '<span class="owo-ws-audit-ev">' + esc(e.event) + "</span>" +
            '<span class="owo-ws-audit-det" title="' + esc(e.detail) + '">' + esc(short(e.detail, 200)) + "</span>" +
            "</div>"
          );
        })
        .join("");
    }

    // ==================== 四期：实时进度（第二路 progress 事件） ====================
    // progress 事件形状（协作计划冻结）：
    // { seq, team_id, status, active, current_steps:[{step_id,worker,status,attempts,started_at}],
    //   counts:{pending,running,succeeded,failed}, updated_at }
    var PROGRESS_STEP_CN = {
      pending: "等待",
      ready: "就绪",
      running: "运行中",
      succeeded: "完成",
      failed: "失败",
      aborted: "中止",
      cancelled: "已取消",
    };

    // 应用一条 progress 事件：seq 单调递增守卫 —— 断线重连后的重复推送、
    // 轮询快照里的旧事件一律跳过（返回 false），保证"重复事件不得重复渲染"。
    // lastProgressSeq 不因重连/轮询切换而重置；仅在切换团队（loadDetail）时清零。
    function applyProgress(evt) {
      if (!evt || typeof evt !== "object") return false;
      var seq = Number(evt.seq);
      if (!isFinite(seq)) return false; // 无有效 seq 的形状一律不采纳
      if (seq <= state.lastProgressSeq) return false; // 旧/重复事件
      state.lastProgressSeq = seq;
      var steps = Array.isArray(evt.current_steps) ? evt.current_steps : [];
      state.progress = {
        seq: seq,
        status: String(evt.status || ""),
        active: !!evt.active,
        current_steps: steps
          .map(function (s) {
            s = s || {};
            return {
              step_id: String(s.step_id || ""),
              worker: String(s.worker || ""),
              status: String(s.status || ""),
              attempts: Number(s.attempts) || 0,
              started_at: String(s.started_at || ""),
            };
          })
          .filter(function (s) {
            return s.step_id;
          }),
        counts: {
          pending: Number(evt.counts && evt.counts.pending) || 0,
          running: Number(evt.counts && evt.counts.running) || 0,
          succeeded: Number(evt.counts && evt.counts.succeeded) || 0,
          failed: Number(evt.counts && evt.counts.failed) || 0,
        },
        updated_at: String(evt.updated_at || ""),
      };
      if (state.progress.status) {
        state.teamStatus = state.progress.status; // progress 是最新状态源
        state.active = state.progress.active;
      }
      return true;
    }

    function fmtElapsed(ms) {
      if (!isFinite(ms) || ms == null || ms < 0) return "—";
      var s = Math.floor(ms / 1000);
      if (s < 60) return s + "s";
      var m = Math.floor(s / 60);
      var rs = s % 60;
      if (m < 60) return m + "m" + (rs ? rs + "s" : "");
      var h = Math.floor(m / 60);
      return h + "h" + (m % 60) + "m";
    }

    // 进度视图模型（纯函数）：耗时基于 started_at 与 nowMs；rows 只收有 step_id 的步骤。
    // 无 progress 快照但用户已点取消：仍返回最小视图，让"取消中"徽标立即可见。
    function computeProgressView(nowMs) {
      var p = state.progress;
      if (!p) {
        if (!state.cancelling) return null;
        return { seq: null, counts: { pending: 0, running: 0, succeeded: 0, failed: 0 }, updated_at: "", rows: [], cancelling: true, active: false };
      }
      var now = typeof nowMs === "number" && isFinite(nowMs) ? nowMs : Date.now();
      var rows = (p.current_steps || []).map(function (s) {
        var started = Date.parse(s.started_at);
        var elapsed = isFinite(started) ? Math.max(0, now - started) : null;
        var st = normStatus(s.status);
        return {
          step_id: s.step_id,
          worker: s.worker,
          status: st,
          statusCn: PROGRESS_STEP_CN[st] || s.status,
          attempts: s.attempts,
          elapsedMs: elapsed,
          running: st === "running",
        };
      });
      return {
        seq: p.seq,
        counts: p.counts,
        updated_at: p.updated_at,
        rows: rows,
        cancelling: state.cancelling,
        active: p.active,
      };
    }

    function progressCountChip(label, n, cls) {
      return '<span class="owo-ws-prog-count' + (cls ? " " + cls : "") + '">' + esc(label) + " <b>" + esc(n) + "</b></span>";
    }

    function renderProgress(vm) {
      if (!vm) return '<div class="hint owo-ws-prog-empty">暂无实时进度事件（等待 progress 推送…）</div>';
      var c = vm.counts || {};
      var html =
        '<div class="owo-ws-prog-head">' +
        (vm.seq != null ? '<span class="owo-ws-prog-seq" title="最新 progress 事件序号（单调递增，断线恢复依据）">seq #' + esc(vm.seq) + "</span>" : "") +
        progressCountChip("等待", c.pending || 0, "off") +
        progressCountChip("运行", c.running || 0, "run") +
        progressCountChip("完成", c.succeeded || 0, "ok") +
        progressCountChip("失败", c.failed || 0, "bad") +
        (vm.cancelling ? '<span class="owo-ws-badge st-interrupted owo-ws-prog-cancelling">取消中…（已下发，等待执行器停止）</span>' : "") +
        "</div>";
      if (!vm.rows.length) {
        return html + '<div class="hint">当前无活动步骤</div>';
      }
      html += '<div class="owo-ws-prog-steps">';
      for (var i = 0; i < vm.rows.length; i++) {
        var r = vm.rows[i];
        html +=
          '<div class="owo-ws-prog-step' + (r.running ? " run" : "") + '">' +
          '<span class="owo-ws-mono owo-ws-ellip" title="' + esc(r.step_id) + '">' + esc(r.worker || r.step_id) + "</span>" +
          '<span class="owo-ws-badge">' + esc(r.statusCn) + "</span>" +
          '<span class="hint">第 ' + esc(r.attempts) + " 次尝试</span>" +
          '<span class="owo-ws-prog-elapsed" title="自 started_at 起的已运行时间">' +
          (r.running ? "已运行 " : "耗时 ") + esc(r.elapsedMs == null ? "—" : fmtElapsed(r.elapsedMs)) +
          "</span>" +
          "</div>";
      }
      html += "</div>";
      return html;
    }

    function paintProgress() {
      var box = el("#ws-d-progress");
      if (!box) return;
      var vm = computeProgressView(Date.now());
      box.innerHTML = renderProgress(vm);
      var hasRunning = vm && vm.rows.some(function (r) {
        return r.running;
      });
      if (hasRunning) ensureProgressTimer();
      else stopProgressTimer();
    }

    // 1s tick：仅在存在 Running 步骤时刷新"已运行时间"；无活动步骤自动停止。
    function ensureProgressTimer() {
      if (state.progressTimer) return;
      state.progressTimer = setInterval(function () {
        if (el("#ws-d-progress")) paintProgress();
        else stopProgressTimer(); // 视图已切走
      }, 1000);
    }

    function stopProgressTimer() {
      if (state.progressTimer) {
        clearInterval(state.progressTimer);
        state.progressTimer = null;
      }
    }

    // ==================== 四期：Artifact 版本链与评审（第三路 review API） ====================
    // 评审接口（协作计划冻结）：
    //   POST /artifacts/{id}/review  body {team_id, decision, reviewer, comment, expected_version, idempotency_key}
    //   GET  /artifacts/{id}/history → 不可变 ArtifactReviewRecord 列表
    var REVIEW_CN = { draft: "草稿（返工中）", pendingreview: "待评审", approved: "已批准", changesrequested: "要求修改", rejected: "已驳回", superseded: "已被取代" };
    var REVIEW_CLS = { draft: "off", pendingreview: "warn", approved: "ok", changesrequested: "warn", rejected: "bad", superseded: "off" };
    var REVIEW_DECISIONS = ["approve", "request_changes", "reject"];
    var DECISION_CN = { approve: "批准", request_changes: "要求修改", reject: "驳回" };

    function normReviewState(s) {
      return String(s == null ? "" : s).toLowerCase().replace(/[_\s-]/g, "");
    }

    function reviewBadgeHtml(st) {
      var k = normReviewState(st);
      var cls = REVIEW_CLS[k] || "off";
      return '<span class="owo-ws-badge rv-' + cls + '" data-art-state="' + esc(k || "unknown") + '">' + esc(REVIEW_CN[k] || String(st || "—")) + "</span>";
    }

    function isReviewable(a) {
      return !!a && normReviewState(a.review_state) === "pendingreview" && !state.reviewBusy[String(a.artifact_id)];
    }

    // 版本链分组（纯函数）：supersedes_artifact_id 指向链内既有产物则续链；
    // 链内按 version 升序；链头（items 末位）为最新版本。
    function groupArtifactChain(arts) {
      var list = (arts || []).filter(function (a) {
        return a && a.artifact_id != null;
      });
      var byId = {};
      list.forEach(function (a) {
        byId[String(a.artifact_id)] = a;
      });
      list.sort(function (x, y) {
        return String(x.created_at).localeCompare(String(y.created_at));
      });
      var chains = [];
      var chainOf = {};
      list.forEach(function (a) {
        var id = String(a.artifact_id);
        var sup = a.supersedes_artifact_id == null ? "" : String(a.supersedes_artifact_id);
        if (sup && byId[sup] && chainOf[sup] != null) {
          var c = chains[chainOf[sup]];
          c.items.push(a);
          chainOf[id] = chainOf[sup];
        } else {
          chainOf[id] = chains.length;
          chains.push({ items: [a] });
        }
      });
      chains.forEach(function (c) {
        c.items.sort(function (x, y) {
          return (Number(x.version) || 0) - (Number(y.version) || 0) || String(x.created_at).localeCompare(String(y.created_at));
        });
        var approved = null;
        c.items.forEach(function (a) {
          if (normReviewState(a.review_state) === "approved") approved = a; // 取最高版本（链已升序）
        });
        c.approvedHead = approved;
      });
      return chains;
    }

    // 评审请求体（纯函数）：expected_version 乐观并发控制 + 幂等键；生产者禁止自行批准。
    function buildReviewBody(input) {
      var a = input.artifact || {};
      var decision = String(input.decision || "");
      if (REVIEW_DECISIONS.indexOf(decision) < 0) throw new Error("未知评审动作：" + decision);
      var reviewer = String(input.reviewer || "").trim();
      if (!reviewer) throw new Error("请先填写评审者（critic 或 human 用户名）");
      if (decision === "approve") {
        var producer = String(a.producer || "");
        var producerRole = producer.replace(/^m-/, "");
        if (reviewer === producer || reviewer === producerRole) {
          throw new Error("生产者不能自行批准自己的产物（approve 需 Human 策略授权的 critic/human 执行）");
        }
      }
      return {
        team_id: input.teamId || state.current || "",
        decision: decision,
        reviewer: reviewer,
        comment: String(input.comment == null ? "" : input.comment),
        expected_version: Number(a.version) || 0,
        idempotency_key: input.idempotencyKey || aidemKey(a, decision, reviewer),
      };
    }

    // 幂等键：不同意图的提交生成新键（时间戳 + 进程内序号，同毫秒两次提交也互异）；
    // 同键重复提交由服务端保证零副作用。
    var aidemSeq = 0;
    function aidemKey(a, decision, reviewer) {
      aidemSeq += 1;
      return [String((a && a.artifact_id) || ""), (a && a.version) || 0, decision, reviewer, Date.now(), aidemSeq].join(":");
    }

    // 评审错误文案：409/403 用计划规定的可操作提示，其余退回 explainError。
    // 传输层把状态码嵌在 message 头部（"409: {...}"），此处一并识别。
    function explainReviewError(err) {
      var st = err && err.status;
      if (!st && err && typeof err.message === "string") {
        var m = /^(\d{3}):/.exec(err.message);
        if (m) st = Number(m[1]);
      }
      if (st === 409) return "版本已更新，请刷新后重试（你提交的 expected_version 已过期，可能有更新的评审或版本）";
      if (st === 403) return "无评审权限（403）：生产者不能自行批准自己的产物；请使用已授权的 critic/human 身份重试";
      return explainError(err, "评审提交");
    }

    // 提交评审（transport 可注入，Node 测试直接调用）。结果与错误同时记录到
    // state.reviewResult，供 DOM 层与测试读取；按钮锁定由 reviewBusy 保证。
    function submitArtifactReview(opts) {
      var aid = String((opts && opts.artifactId) || "");
      var a = findArtifactById(aid);
      if (!a) return Promise.reject(new Error("产物不存在或列表已刷新，请刷新后重试"));
      var body;
      try {
        body = buildReviewBody({
          artifact: a,
          decision: opts && opts.decision,
          reviewer: opts && opts.reviewer,
          comment: opts && opts.comment,
          idempotencyKey: opts && opts.idempotencyKey,
          teamId: opts && opts.teamId,
        });
      } catch (e) {
        return Promise.reject(e);
      }
      if (state.reviewBusy[aid]) return Promise.reject(new Error("该产物已有评审提交进行中，请等待完成"));
      state.reviewBusy[aid] = true;
      return H.post("/artifacts/" + encodeURIComponent(aid) + "/review", body).then(
        function (resp) {
          delete state.reviewBusy[aid];
          state.reviewResult = { ok: true, artifactId: aid, decision: body.decision, reviewer: body.reviewer, resp: resp || null };
          state.reviewFlash = { artifactId: aid, ok: true, text: "评审已提交：" + (DECISION_CN[body.decision] || body.decision) + "（记录不可变）" };
          return resp;
        },
        function (e) {
          delete state.reviewBusy[aid];
          state.reviewResult = { ok: false, artifactId: aid, decision: body.decision, reviewer: body.reviewer, error: explainReviewError(e) };
          state.reviewFlash = { artifactId: aid, ok: false, text: explainReviewError(e) };
          throw e;
        }
      );
    }

    function findArtifactById(aid) {
      var list = state.artifacts || [];
      for (var i = 0; i < list.length; i++) {
        if (String(list[i].artifact_id) === String(aid)) return list[i];
      }
      return null;
    }

    // 评审历史（懒加载）：GET /artifacts/{id}/history → 不可变记录列表。
    function artifactHistoryHtml(records) {
      if (!records || !records.length) return '<div class="hint">暂无评审记录</div>';
      return records
        .map(function (r) {
          var dec = String((r && r.decision) || "");
          var cn = { approve: "批准", request_changes: "要求修改", reject: "驳回" }[dec] || dec;
          return (
            '<div class="owo-ws-review-rec">' +
            reviewBadgeHtml(dec === "approve" ? "approved" : dec === "request_changes" ? "changes_requested" : "rejected") +
            "<b>" + esc(r.reviewer || "—") + "</b>" +
            '<span class="owo-ws-ellip" title="' + esc(r.comment || "") + '">' + esc(r.comment || "（无评语）") + "</span>" +
            '<span class="hint">' + esc(r.created_at || "") + "</span>" +
            "</div>"
          );
        })
        .join("");
    }

    function loadArtifactHistory(aid) {
      var box = el("#ws-d-artifacts");
      if (!box) return Promise.resolve();
      var target = null;
      var boxes = box.querySelectorAll("[data-art-history-box]");
      for (var i = 0; i < boxes.length; i++) {
        if (boxes[i].getAttribute("data-art-history-box") === String(aid)) target = boxes[i];
      }
      if (!target) return Promise.resolve();
      target.innerHTML = '<div class="hint">加载评审历史…</div>';
      return H.get("/artifacts/" + encodeURIComponent(aid) + "/history")
        .then(function (d) {
          // 第三路响应：reviews[]（评审记录）+ supersedes/superseded_by/approved_head
          var recs = (d && (d.reviews || d.records || d.history)) || [];
          target.innerHTML = artifactHistoryHtml(recs);
        })
        .catch(function (e) {
          target.innerHTML = stateBox("error", explainError(e, "评审历史"), "history-" + aid);
        });
    }

    // 产物行（链内）：版本徽标 + 评审状态 + 产出者 + 取代关系 + 预览 + 评审表单 + 历史。
    function artifactRowHtml(a, chain) {
      var aid = String(a.artifact_id == null ? "" : a.artifact_id);
      var busy = !!state.reviewBusy[aid];
      var isHead = chain && chain.items[chain.items.length - 1] === a;
      var sup = a.supersedes_artifact_id == null ? "" : String(a.supersedes_artifact_id);
      var supVer = "";
      if (sup) {
        for (var i = 0; i < (chain ? chain.items : []).length; i++) {
          if (String(chain.items[i].artifact_id) === sup) supVer = "v" + chain.items[i].version;
        }
      }
      var formHtml = "";
      if (normReviewState(a.review_state) === "pendingreview") {
        formHtml =
          '<details class="owo-ws-review"' + (busy ? ' data-busy="1"' : "") + ">" +
          '<summary>评审此版本（批准 / 要求修改 / 驳回）</summary>' +
          '<div class="owo-ws-review-form">' +
          '<input class="owo-ws-review-reviewer" placeholder="评审者：critic 或 human 用户名（生产者不能自行批准）">' +
          '<textarea class="owo-ws-review-comment" rows="2" placeholder="评语（随不可变评审记录保存）"></textarea>' +
          '<div class="owo-ws-review-actions">' +
          '<button type="button" class="owo-ws-review-act ok" data-art-act="approve" data-art-id="' + esc(aid) + '"' + (busy ? " disabled" : "") + ">批准</button>" +
          '<button type="button" class="owo-ws-review-act warn" data-art-act="request_changes" data-art-id="' + esc(aid) + '"' + (busy ? " disabled" : "") + ">要求修改</button>" +
          '<button type="button" class="owo-ws-review-act bad" data-art-act="reject" data-art-id="' + esc(aid) + '"' + (busy ? " disabled" : "") + ">驳回</button>" +
          "</div>" +
          "</div></details>";
      }
      // 评审结果行（行级，独立于表单）：状态迁移后表单可能消失，但 flash 提示仍在。
      var flash = state.reviewFlash && String(state.reviewFlash.artifactId) === aid ? state.reviewFlash : null;
      var resultHtml =
        '<div class="owo-ws-review-result sub' + (flash && !flash.ok ? " bad" : flash ? " ok" : "") + '" data-art-result="' + esc(aid) + '" aria-live="polite">' +
        (flash ? esc(flash.text) : "") +
        "</div>";
      return (
        '<div class="owo-ws-art-row' + (isHead ? " head" : "") + '" data-art-row="' + esc(aid) + '">' +
        '<div class="owo-ws-art-line">' +
        '<span class="owo-ws-mono">v' + esc(a.version) + (isHead ? "（最新）" : "") + "</span>" +
        reviewBadgeHtml(a.review_state) +
        '<span class="hint">产出者 ' + esc(roleOfProducer(a.producer)) + "</span>" +
        (supVer ? '<span class="hint">取代 ' + esc(supVer) + "</span>" : "") +
        '<span class="hint owo-ws-ellip" title="' + esc(a.created_at || "") + '">' + esc(a.created_at || "") + "</span>" +
        "</div>" +
        (a.preview != null
          ? '<details class="owo-ws-art-preview"><summary>预览</summary><pre>' + esc(a.preview || "（空）") + "</pre></details>"
          : "") +
        formHtml +
        resultHtml +
        '<div class="owo-ws-art-histline">' +
        '<button type="button" class="owo-ws-mini" data-art-history="' + esc(aid) + '">评审历史</button>' +
        '<span class="owo-ws-art-history" data-art-history-box="' + esc(aid) + '"></span>' +
        "</div>" +
        "</div>"
      );
    }

    function roleOfProducer(p) {
      return String(p || "").replace(/^m-/, "");
    }

    function renderArtifactsChains(chains) {
      if (!chains || !chains.length) return "";
      return chains
        .map(function (c) {
          var kind = (c.items[0] && c.items[0].kind) || "—";
          var headHtml = c.approvedHead
            ? '当前 approved head：<span class="owo-ws-badge rv-ok">v' + esc(c.approvedHead.version) + "</span>"
            : '<span class="hint">无已批准版本（approved head 未建立）</span>';
          return (
            '<div class="owo-ws-art-chain">' +
            '<div class="owo-ws-art-chainhead"><b>' + esc(kind) + "</b> 版本链（" + c.items.length + " 个版本）· " + headHtml + "</div>" +
            c.items
              .map(function (a) {
                return artifactRowHtml(a, c);
              })
              .join("") +
            "</div>"
          );
        })
        .join("");
    }

    // ---------- 成员 / 任务 ----------
    function membersByMId() {
      var m = {};
      ((state.team && state.team.members) || []).forEach(function (x) {
        if (x.member_id) m[x.member_id] = x;
      });
      return m;
    }

    function findTask(id) {
      for (var i = 0; i < state.tasks.length; i++) {
        if (state.tasks[i].task_id === id) return state.tasks[i];
      }
      return null;
    }

    function paintMembers() {
      var box = el("#ws-d-members");
      if (!box) return;
      var members = (state.team && state.team.members) || [];
      if (!members.length) {
        box.innerHTML = '<div class="hint">暂无成员记录（团队可能尚未开始运行）</div>';
        return;
      }
      box.innerHTML = members
        .map(function (m) {
          var b = m.runtime_binding || {};
          var kind = b.kind || "";
          var who =
            kind === "agent"
              ? "Agent " + (b.agent_id || "")
              : kind === "human"
                ? "人工 " + (b.user_id || "")
                : "Worker " + (b.worker_name || "");
          var caps = (m.capabilities || []).join(", ");
          var extra =
            m.handoff_contract != null ||
            m.budget != null ||
            (m.read_scope || []).length ||
            (m.write_scope || []).length;
          return (
            '<div class="owo-ws-member">' +
            '<div class="owo-ws-member-head"><b>' + esc(m.role) + "</b>" +
            '<span class="chip c-' + esc(kind) + '">' + esc(kind) + "</span>" +
            '<span class="chip h-' + esc(m.health) + '">' + (HEALTH_CN[m.health] || esc(m.health)) + "</span>" +
            "</div>" +
            '<div class="hint owo-ws-ellip" title="' + esc(m.member_id + " · " + who) + '">' +
            '<span class="owo-ws-mono">' + esc(m.member_id) + "</span> · " + esc(who) + "</div>" +
            (m.handoff_contract
              ? '<div class="hint owo-ws-ellip" title="交接契约：' + esc(m.handoff_contract) + '">交接契约：' + esc(m.handoff_contract) + "</div>"
              : "") +
            (caps ? '<div class="hint owo-ws-ellip" title="能力：' + esc(caps) + '">能力：' + esc(caps) + "</div>" : "") +
            (extra
              ? "<details class=\"hint\"><summary>范围 / 预算</summary><pre class=\"hint\">" +
                esc(
                  JSON.stringify({
                    read_scope: m.read_scope || [],
                    write_scope: m.write_scope || [],
                    budget: m.budget != null ? m.budget : null,
                  })
                ) +
                "</pre></details>"
              : "") +
            "</div>"
          );
        })
        .join("");
    }

    // 任务 DAG：按依赖层级分列，SVG + foreignObject 渲染节点。
    // teamStatus 用于节点级「重试此节点」按钮的可见性（succeeded/cancelled 终态不出按钮）。
    function dagSvg(tasks, teamStatus) {
      var showRetry = shouldShowRetry(tasks, teamStatus || "");
      var byId = {};
      tasks.forEach(function (t) {
        byId[t.task_id] = t;
      });
      var levelCache = {};
      function level(id) {
        if (levelCache[id] != null) return levelCache[id];
        var t = byId[id];
        if (!t) return 0;
        levelCache[id] = 0; // 环保护
        var lv = 0;
        var deps = t.depends_on || [];
        for (var i = 0; i < deps.length; i++) {
          if (byId[deps[i]]) {
            var d = level(deps[i]) + 1;
            if (d > lv) lv = d;
          }
        }
        levelCache[id] = lv;
        return lv;
      }
      var maxL = 0;
      tasks.forEach(function (t) {
        var l = level(t.task_id);
        if (l > maxL) maxL = l;
      });
      var cols = {};
      tasks.forEach(function (t) {
        var l = level(t.task_id);
        (cols[l] = cols[l] || []).push(t);
      });
      var NW = 210;
      var NH = 100;
      var GX = 60;
      var GY = 40;
      var PAD = 12;
      var rows = 0;
      for (var l = 0; l <= maxL; l++) rows = Math.max(rows, (cols[l] || []).length);
      var W = PAD * 2 + (maxL + 1) * NW + maxL * GX;
      var Hh = PAD * 2 + Math.max(rows, 1) * NH + Math.max(rows - 1, 0) * GY;
      var pos = {};
      for (var l2 = 0; l2 <= maxL; l2++) {
        (cols[l2] || []).forEach(function (t, i) {
          pos[t.task_id] = { x: PAD + l2 * (NW + GX), y: PAD + i * (NH + GY) };
        });
      }
      var edges = "";
      tasks.forEach(function (t) {
        (t.depends_on || []).forEach(function (dep) {
          if (!byId[dep] || !pos[dep] || !pos[t.task_id]) return;
          var dead = isDeadStatus(byId[dep].status);
          var s = pos[dep];
          var e2 = pos[t.task_id];
          edges +=
            '<line x1="' + s.x + NW + '" y1="' + (s.y + NH / 2) + '" x2="' + (e2.x - 5) + '" y2="' + (e2.y + NH / 2) + '" marker-end="url(#ws-arrow' + (dead ? "-dead" : "") + ')"' + (dead ? ' class="dead"' : "") + "></line>";
        });
      });
      var nodes = "";
      tasks.forEach(function (t) {
        var p = pos[t.task_id];
        if (!p) return;
        var st = normStatus(t.status);
        var blocked = taskBlocked(t, byId);
        var inner =
          '<div class="owo-ws-node st-' + st + (blocked ? " blocked" : "") + '">' +
          '<div class="owo-ws-node-line"><b>' + esc(t.role || t.worker || t.task_id) + "</b>" +
          '<span class="chip">' + (STEP_STATUS_CN[st] || esc(t.status)) + (blocked ? " · 已阻塞" : "") + "</span></div>" +
          '<div class="hint">worker ' + esc(t.worker || "—") + (t.attempts ? " · 第 " + t.attempts + " 次" : "") + "</div>" +
          (t.error
            ? '<div class="owo-ws-node-err" title="' + esc(t.error) + '">' + esc(short(t.error, 70)) + "</div>"
            : '<div class="hint">&nbsp;</div>') +
          // R2 冻结契约入口：仅 Failed/Aborted 节点给出「重试此节点」；
          // 提交体由 buildRetryBody 构造，提交锁/去重在 handleRetryClick。
          (showRetry && isRetryableStep(t)
            ? '<div class="owo-ws-node-act"><button type="button" class="owo-ws-mini owo-ws-retry" data-ws-retry="' +
              esc(t.task_id) +
              '" aria-label="重试此节点 ' + esc(t.role || t.worker || t.task_id) + '">↻ 重试此节点</button></div>'
            : "") +
          "</div>";
        nodes +=
          '<foreignObject x="' + p.x + '" y="' + p.y + '" width="' + NW + '" height="' + NH + '">' + inner + "</foreignObject>";
      });
      return (
        '<svg class="owo-ws-dag" width="' + W + '" height="' + Hh + '">' +
        "<defs>" +
        '<marker id="ws-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="#8899aa"></path></marker>' +
        '<marker id="ws-arrow-dead" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="#e05555"></path></marker>' +
        "</defs>" +
        "<g>" + edges + "</g>" +
        nodes +
        "</svg>"
      );
    }

    function paintDag() {
      var box = el("ws-d-dag");
      if (!box) return;
      var tasks = state.tasks || [];
      if (!tasks.length) {
        box.innerHTML = '<div class="hint">暂无任务记录（团队可能尚未开始运行，或任务图尚未生成）</div>';
        return;
      }
      var byId = {};
      tasks.forEach(function (t) {
        byId[t.task_id] = t;
      });
      var doneN = 0;
      var failN = 0;
      var blockedN = 0;
      var runningN = 0;
      tasks.forEach(function (t) {
        var st = normStatus(t.status);
        if (st === "succeeded") doneN++;
        else if (st === "failed" || st === "aborted") failN++;
        else if (st === "running") runningN++;
        if (taskBlocked(t, byId)) blockedN++;
      });
      var sum =
        '<div class="owo-ws-dagsum">共 ' + tasks.length +
        " 项 · 完成 " + doneN +
        (runningN ? ' · <b class="run">运行中 ' + runningN + "</b>" : "") +
        (failN ? ' · <b class="blocked">失败/中止 ' + failN + "</b>" : "") +
        (blockedN ? ' · <b class="blocked">阻塞 ' + blockedN + "</b>" : "") +
        "</div>";
      try {
        box.innerHTML =
          '<div class="hint" style="margin:0 0 6px">图例：虚线框=未开始 · 实线框=进行中/已终结 · 红色边框=已阻塞（上游失败/中止，不会再运行）· 红色边=上游已死 · 琥珀按钮=可重试此节点（仅重置自身与未完成下游）</div>' +
          sum +
          '<div class="owo-ws-dagwrap">' + dagSvg(tasks, currentTeamStatus()) + "</div>";
      } catch (e) {
        var rows = tasks
          .map(function (t) {
            return (
              "<tr>" +
              "<td>" + esc(t.task_id) + "</td><td>" + esc(t.role || "") + "</td><td>" + esc(t.worker || "") + "</td>" +
              '<td class="hint">' + esc((t.depends_on || []).join(", ")) + "</td><td>" + esc(t.status) + "</td>" +
              "<td>" + esc(t.attempts) + "</td><td class=\"hint\">" + esc(t.error || "") + "</td></tr>"
            );
          })
          .join("");
        box.innerHTML =
          '<div class="hint">DAG 渲染失败（' + esc(e && e.message) + "），回退为列表：</div>" +
          '<div class="owo-ws-tablewrap"><table class="owo-ws-table"><tr><th>task_id</th><th>角色</th><th>worker</th><th>依赖</th><th>状态</th><th>尝试</th><th>错误</th></tr>' +
          rows +
          "</table></div>";
      }
    }

    // 中断态显示：服务端 interrupted 标记（磁盘 Running 但无活动运行）。
    // 需求红线——中断的团队不得继续显示为正常执行中。
    function interruptedView() {
      return !!state.interrupted && !isTerminalTeam(currentTeamStatus());
    }

    function paintInterrupted() {
      var box = el("ws-d-interrupted");
      if (box) box.style.display = interruptedView() ? "" : "none";
    }

    // 运行摘要（详情页顶部）：状态/阶段、成功/失败/等待/运行/阻塞、失败步骤、
    // 累计尝试、产物数；失败步骤行内联「重试此节点」。
    function paintRunSummary() {
      var box = el("ws-d-summary");
      if (!box) return;
      if (!state.team) {
        box.innerHTML = stateBox("loading", "正在汇总运行状态…");
        return;
      }
      var s = computeRunSummary({
        team: state.team,
        tasks: state.tasks,
        artifactCount: state.artifactCount,
        interrupted: state.interrupted,
        active: state.active,
      });
      var badge =
        '<span class="owo-ws-badge st-' + esc(s.statusKey) + '">' + esc(s.statusLabel) + "</span>" +
        '<span class="chip">' + esc(s.phase) + "</span>";
      var stats = [
        { label: "成功", v: s.counts.succeeded, k: s.counts.succeeded ? "ok" : "" },
        { label: "失败/中止", v: s.counts.failed, k: s.counts.failed ? "bad" : "" },
        { label: "等待", v: s.counts.waiting, k: "" },
        { label: "运行中", v: s.counts.running, k: s.counts.running ? "run" : "" },
        { label: "阻塞", v: s.counts.blocked, k: s.counts.blocked ? "bad" : "" },
        { label: "累计尝试", v: s.totalAttempts, k: "" },
        { label: "产物", v: s.artifactCount == null ? "—" : s.artifactCount, k: "" },
      ];
      var cells = stats
        .map(function (x) {
          return (
            '<div class="owo-ws-stat"><small>' + esc(x.label) + '</small><b class="' + x.k + '">' + esc(x.v) + "</b></div>"
          );
        })
        .join("");
      var line = "";
      if (s.failedStep) {
        var head =
          "当前失败步骤：<b>" + esc(s.failedStep.role) + "</b>（" + esc(s.failedStep.task_id) +
          " · 已尝试 " + s.failedStep.attempts + " 次）";
        var err = s.failedStep.error
          ? '<span class="owo-ws-node-err owo-ws-ellip" title="' + esc(s.failedStep.error) + '">' +
            esc(short(s.failedStep.error, 80)) + "</span>"
          : "";
        if (s.canRetry) {
          line =
            '<div class="owo-ws-sumline">' + head + err +
            '<button type="button" class="owo-ws-mini owo-ws-retry" data-ws-retry="' + esc(s.failedStep.task_id) +
            '" aria-label="重试此节点 ' + esc(s.failedStep.role) + '">↻ 重试此节点</button></div>';
        } else {
          line = '<div class="owo-ws-sumline">' + head + err + '<span class="hint">团队已终态，重试不可用</span></div>';
        }
      } else {
        line = '<div class="owo-ws-sumline hint">当前无失败步骤' + (s.counts.total ? "" : "（任务图尚未生成）") + "</div>";
      }
      box.innerHTML = '<div class="owo-ws-sumline owo-ws-sum-head">' + badge + "</div>" +
        '<div class="owo-ws-statgrid">' + cells + "</div>" + line;
    }

    function paintStatusLive() {
      var t = state.team || {};
      var raw = state.teamStatus || (t.status != null ? t.status : "");
      var st = normStatus(raw);
      var iv = interruptedView();
      var badge = el("#ws-d-status");
      if (badge) {
        if (iv) {
          badge.className = "owo-ws-badge st-interrupted";
          badge.textContent = "已中断（可恢复）";
          badge.title = "进程重启遗留的运行态：进度已保留，可用 continue / retry 显式恢复";
        } else {
          badge.className = "owo-ws-badge st-" + (st || "unknown");
          badge.textContent = TEAM_STATUS_CN[st] || raw || "未知";
          badge.title = "";
        }
      }
      var act = el("#ws-d-active");
      if (act) {
        act.textContent = iv ? "◦ 已中断" : state.active ? "● 执行中" : "○ 未执行";
        act.className = "owo-ws-active" + (iv ? " interrupted" : state.active ? " on" : "");
      }
      var meta = el("#ws-d-meta");
      if (meta && t && t.team_id) {
        var metaText =
          "模式 " + (MODE_CN[normStatus(t.mode)] || t.mode || "—") +
          " · 项目空间 " + (t.project_space_id || "—") +
          " · 模板 " + (t.template_id || "—") +
          " · 创建 " + (t.created_at || "—") +
          " · 更新 " + (t.updated_at || "—");
        meta.textContent = metaText;
        meta.title = metaText; // 长路径/长 ID：完整 tooltip
      }
      // 状态变化（含 SSE/轮询推送）实时重算终态门控。
      applyGating();
    }

    function paintDetailLive() {
      paintStatusLive();
      paintRunSummary();
      paintInterrupted();
      paintMembers();
      paintDag();
      paintAudit();
      paintProgress();
      paintHumanSelect();
      paintHandoffSelect();
      paintFromSelect();
    }

    // ---------- 人节点 / 交接 下拉 ----------
    function humanTasks() {
      var mBy = membersByMId();
      var out = [];
      (state.tasks || []).forEach(function (t) {
        var m = mBy[t.worker];
        if (m && m.runtime_binding && m.runtime_binding.kind === "human" && !STEP_TERMINAL[normStatus(t.status)]) {
          out.push(t);
        }
      });
      return out;
    }

    function paintHumanSelect() {
      var sel = el("#ws-h-task");
      if (!sel) return;
      var prev = state.humanTask || "";
      var list = humanTasks();
      sel.innerHTML = list.length
        ? list
            .map(function (t) {
              return (
                '<option value="' + esc(t.task_id) + '">' +
                esc(t.role || t.task_id) +
                "（" +
                (STEP_STATUS_CN[normStatus(t.status)] || t.status) +
                "）</option>"
              );
            })
            .join("")
        : '<option value="">暂无未完成的人节点任务</option>';
      if (list.some(function (t) {
        return t.task_id === prev;
      }))
        sel.value = prev;
      state.humanTask = sel.value;
    }

    function paintHandoffSelect() {
      var sel = el("#ws-x-task");
      if (!sel) return;
      var prev = sel.value;
      var tasks = state.tasks || [];
      sel.innerHTML = tasks.length
        ? tasks
            .map(function (t) {
              return (
                '<option value="' + esc(t.task_id) + '">' +
                esc(t.role || t.task_id) +
                "（" +
                (STEP_STATUS_CN[normStatus(t.status)] || t.status) +
                "）</option>"
              );
            })
            .join("")
        : '<option value="">暂无任务</option>';
      if (tasks.some(function (t) {
        return t.task_id === prev;
      }))
        sel.value = prev;
    }

    function paintFromSelect() {
      var xTask = el("#ws-x-task");
      var xFrom = el("#ws-x-from");
      if (!xTask || !xFrom) return;
      var t = findTask(xTask.value);
      xFrom.innerHTML = t
        ? '<option value="' + esc(t.worker) + '">' + esc(t.worker) + "（任务执行者）</option>"
        : '<option value="">—</option>';
    }

    function populateSteerSelect() {
      var sel = el("#ws-act-steer-step");
      if (!sel) return;
      var tasks = state.tasks || [];
      sel.innerHTML =
        '<option value="">全部未完成任务（默认）</option>' +
        tasks
          .map(function (t) {
            return (
              '<option value="' + esc(t.task_id) + '">' +
              esc(t.role || t.task_id) +
              " · " +
              (STEP_STATUS_CN[normStatus(t.status)] || t.status || "") +
              "</option>"
            );
          })
          .join("");
    }

    function populateReplaceRole() {
      var sel = el("#ws-act-replace-role");
      if (!sel) return;
      var ms = (state.team && state.team.members) || [];
      sel.innerHTML = ms.length
        ? ms
            .map(function (m) {
              return '<option value="' + esc(m.role) + '">' + esc(m.role) + "（" + esc(m.member_id) + "）</option>";
            })
            .join("")
        : '<option value="">（暂无成员）</option>';
    }

    function populateSelects() {
      populateSteerSelect();
      populateReplaceRole();
      paintHumanSelect();
      paintHandoffSelect();
      paintFromSelect();
    }

    // ---------- 产物 ----------
    function loadArtifacts() {
      var box = el("#ws-d-artifacts");
      if (!box) return Promise.resolve();
      var t = state.team;
      var pid = (t && t.project_space_id) || (state.current ? "proj-" + state.current : "");
      if (!pid) {
        box.innerHTML = '<div class="hint">项目空间尚不可用</div>';
        return Promise.resolve();
      }
      box.innerHTML = '<div class="hint">project_id：' + esc(pid) + "</div>" + stateBox("loading", "正在加载产物…");
      return H.get("/projects/" + encodeURIComponent(pid) + "/artifacts")
        .then(function (d) {
          if (el("#ws-d-artifacts") !== box) return; // 视图已切走
          var arts = (d && d.artifacts) || [];
          state.artifacts = arts;
          state.artifactCount = arts.length; // 运行摘要「产物」计数
          paintRunSummary();
          if (!arts.length) {
            box.innerHTML = '<div class="hint">project_id：' + esc(pid) + '</div><div class="hint">暂无产物（任务产出后会出现在这里）</div>';
            return;
          }
          var chains = groupArtifactChain(arts);
          box.innerHTML =
            '<div class="hint">project_id：' + esc(pid) + " · 共 " + arts.length + " 个产物 · " + chains.length + " 条版本链</div>" +
            renderArtifactsChains(chains);
          bindArtifactReviewHandlers(box);
        })
        .catch(function (e) {
          if (el("#ws-d-artifacts") !== box) return;
          box.innerHTML =
            '<div class="hint">project_id：' + esc(pid) + "</div>" +
            stateBox("error", explainError(e, "产物加载"), "artifacts");
        });
    }

    // 产物区事件委托：评审动作 / 评审历史懒加载。box 每次 loadArtifacts 只换
    // innerHTML，委托绑定标记在 box 元素自身上，元素随 renderDetail 重建时重绑。
    function bindArtifactReviewHandlers(box) {
      if (!box || box.getAttribute("data-review-bound") === "1") return;
      box.setAttribute("data-review-bound", "1");
      box.addEventListener("click", function (ev) {
        var target = ev.target;
        if (!target || !target.closest) return;
        var actBtn = target.closest("[data-art-act]");
        if (actBtn) {
          var aid = actBtn.getAttribute("data-art-id") || "";
          var act = actBtn.getAttribute("data-art-act") || "";
          var form = actBtn.closest(".owo-ws-review-form");
          var reviewerEl = form ? form.querySelector(".owo-ws-review-reviewer") : null;
          var commentEl = form ? form.querySelector(".owo-ws-review-comment") : null;
          var resultEl = form ? form.querySelector("[data-art-result]") : null;
          // 锁定该产物全部评审按钮（提交期间防重复）
          var allBtns = box.querySelectorAll('[data-art-act][data-art-id="' + aid.replace(/"/g, '\\"') + '"]');
          for (var i = 0; i < allBtns.length; i++) allBtns[i].disabled = true;
          if (actBtn.setAttribute) actBtn.setAttribute("data-busy", "1");
          submitArtifactReview({
            artifactId: aid,
            decision: act,
            reviewer: reviewerEl ? reviewerEl.value : "",
            comment: commentEl ? commentEl.value : "",
          }).then(
            function () {
              if (resultEl) resultEl.textContent = "评审已提交：" + (DECISION_CN[act] || act) + "（记录不可变）";
              loadArtifacts(); // 重取产物/链头状态
            },
            function (e) {
              for (var j = 0; j < allBtns.length; j++) allBtns[j].disabled = false;
              if (actBtn.removeAttribute) actBtn.removeAttribute("data-busy");
              if (resultEl) {
                resultEl.textContent = explainReviewError(e);
                resultEl.className = "owo-ws-review-result sub bad";
              }
            }
          );
          return;
        }
        var histBtn = target.closest("[data-art-history]");
        if (histBtn) {
          loadArtifactHistory(histBtn.getAttribute("data-art-history"));
        }
      });
    }

    // ---------- 详情数据 ----------
    var detailSeq = 0; // 每次进入/重载详情递增；用于丢弃陈旧响应

    function loadDetail(teamId) {
      state.current = teamId;
      var seq = ++detailSeq;
      var idEl = el("#ws-d-id");
      if (idEl) idEl.textContent = teamId;
      var errBox = el("#ws-d-error");
      if (errBox) errBox.innerHTML = "";
      H.get("/teams/" + encodeURIComponent(teamId))
        .then(function (d) {
          if (seq !== detailSeq || state.current !== teamId || state.view !== "detail") return; // 已切走
          state.team = d.team || null;
          state.tasks = d.tasks || [];
          state.interrupted = !!d.interrupted;
          state.audit = [];
          state.auditKeys = {};
          state.progress = null; // 换团队：进度状态清零（seq 守卫随之重置）
          state.lastProgressSeq = 0;
          state.cancelling = false;
          state.artifacts = [];
          state.reviewBusy = {};
          state.reviewResult = null;
          state.reviewFlash = null;
          stopProgressTimer();
          var tail = (d.audit_tail || []).slice();
          tail.sort(function (a, b) {
            return String(a.ts).localeCompare(String(b.ts));
          });
          tail.forEach(addAudit);
          populateSelects();
          paintDetailLive();
          loadArtifacts();
          connectEvents(teamId);
        })
        .catch(function (e) {
          if (seq !== detailSeq || state.current !== teamId || state.view !== "detail") return;
          var eb = el("#ws-d-error");
          if (eb) eb.innerHTML = stateBox("error", explainError(e, "团队详情加载"), "detail");
        });
    }

    function syncDetail() {
      if (!state.current) return Promise.resolve();
      var seq = detailSeq;
      return H.get("/teams/" + encodeURIComponent(state.current))
        .then(function (d) {
          if (seq !== detailSeq) return;
          if (d.team && d.team.team_id !== state.current) return; // 已切到其他团队
          state.team = d.team || state.team;
          state.tasks = d.tasks || [];
          state.interrupted = !!d.interrupted;
          (d.audit_tail || []).forEach(addAudit);
          populateSelects();
          paintDetailLive();
        })
        .catch(function () {
          /* 轮询会兜底 */
        });
    }

    function refreshTasksOnly() {
      if (!state.current) return Promise.resolve();
      var seq = detailSeq;
      return H.get("/teams/" + encodeURIComponent(state.current) + "/tasks")
        .then(function (d) {
          if (seq !== detailSeq) return;
          state.tasks = d.tasks || [];
          paintDag();
          paintRunSummary();
          populateSteerSelect();
          paintHumanSelect();
          paintHandoffSelect();
          paintFromSelect();
        })
        .catch(function (e) {
          if (seq !== detailSeq) return;
          var box = el("#ws-d-dag");
          if (box) box.innerHTML = stateBox("error", explainError(e, "任务刷新"), "tasks");
        });
    }

    // ---------- 实时通道：SSE + 轮询兜底 ----------
    // 通道指示：SSE=绿、轮询降级=琥珀、停止=灰；文字本身承载详细原因。
    function paintStreamNote() {
      var box = el("#ws-d-stream");
      if (!box) return;
      box.textContent = state.streamNote || "";
      var cls =
        state.liveMode === "sse" ? "owo-ws-live on" : state.liveMode === "poll" ? "owo-ws-live poll" : "owo-ws-live off";
      box.className = cls;
      box.title = state.streamNote || "";
    }

    function stopLive(note) {
      if (state.es) {
        try {
          if (typeof state.es.abort === "function") state.es.abort(); // fetch 流式 SSE 的 AbortController
          else state.es.close();
        } catch (e) {
          /* ignore */
        }
        state.es = null;
      }
      if (state.pollTimer) {
        clearInterval(state.pollTimer);
        state.pollTimer = null;
      }
      if (state.refreshDebounce) {
        clearTimeout(state.refreshDebounce);
        state.refreshDebounce = null;
      }
      stopProgressTimer();
      state.pollNoteShown = false;
      state.liveMode = "off";
      if (note) {
        state.streamNote = note;
        paintStreamNote();
      }
    }

    function pollOnce() {
      var id = state.current;
      if (!id) return;
      var seq = detailSeq;
      H.get("/teams/" + encodeURIComponent(id) + "/events?format=json")
        .then(function (snap) {
          if (state.current !== id || seq !== detailSeq) return;
          state.active = !!snap.active;
          state.teamStatus = snap.status;
          state.interrupted = !!snap.interrupted;
          (snap.audit || []).forEach(addAudit);
          if (snap.progress && applyProgress(snap.progress)) {
            paintStatusLive();
            paintProgress();
          }
          paintStatusLive();
          paintAudit();
          if (isTerminalTeam(snap.status)) {
            state.cancelling = false; // 终态确认：清除"取消中"
            stopLive("已结束：团队进入终态（" + String(snap.status) + "）");
          }
        })
        .catch(function (e) {
          if (!state.pollNoteShown) {
            state.pollNoteShown = true;
            state.streamNote = "轮询模式（服务暂不可达：" + short(String(e.message || e), 60) + "，将继续重试）";
            paintStreamNote();
          }
        });
      H.get("/teams/" + encodeURIComponent(id))
        .then(function (d) {
          if (state.current !== id || seq !== detailSeq) return;
          state.team = d.team || state.team;
          state.tasks = d.tasks || [];
          (d.audit_tail || []).forEach(addAudit);
          paintDetailLive();
        })
        .catch(function () {
          /* 快照接口已给出状态提示 */
        });
    }

    function startPolling(note) {
      if (state.es) {
        try {
          state.es.close();
        } catch (e) {
          /* ignore */
        }
        state.es = null;
      }
      if (state.pollTimer) clearInterval(state.pollTimer);
      state.liveMode = "poll";
      state.streamNote = note;
      paintStreamNote();
      state.pollTimer = setInterval(pollOnce, 2500);
      pollOnce();
    }

    function scheduleTasks() {
      if (state.refreshDebounce) clearTimeout(state.refreshDebounce);
      state.refreshDebounce = setTimeout(function () {
        state.refreshDebounce = null;
        refreshTasksOnly();
      }, 800);
    }

    // 事件帧统一分发（EventSource 与 fetch 流式 SSE 共用）。progress 帧兼容两种
    // 形状：嵌套 {type:"progress", progress:{...}}（第二路实现）与扁平（计划原形）。
    function handleEventFrame(f) {
      if (!f || !f.type) return;
      if (f.type === "audit") {
        addAudit(f);
        paintAudit();
        scheduleTasks();
      } else if (f.type === "progress") {
        var payload = f.progress && typeof f.progress === "object" ? f.progress : f;
        if (applyProgress(payload)) {
          paintStatusLive();
          paintRunSummary();
          paintProgress();
          scheduleTasks(); // 步骤状态变化 → 防抖刷新 DAG/任务表
        }
      } else if (f.type === "state") {
        state.active = !!f.active;
        state.teamStatus = f.status;
        state.interrupted = !!f.interrupted;
        paintStatusLive();
        paintRunSummary();
        paintInterrupted();
        if (isTerminalTeam(f.status)) {
          state.cancelling = false; // 终态确认：清除"取消中"
          stopLive("已结束：团队进入终态（" + String(f.status) + "）");
        }
      }
    }

    // fetch 流式 SSE：与 EventSource 等价的 text/event-stream 解析，但可携带
    // Authorization 头（受保护路由必需）。逐帧解析 data: 行；多行 data 按规范拼接。
    function connectFetchSse(teamId, note) {
      var ctrl = typeof AbortController !== "undefined" ? new AbortController() : null;
      state.es = ctrl;
      defaultToken()
        .then(function (tok) {
          if (state.es !== ctrl) return null; // 已被取代
          return fetch(H.baseUrl + "/teams/" + encodeURIComponent(teamId) + "/events", {
            headers: { "Authorization": "Bearer " + tok, "Accept": "text/event-stream" },
            signal: ctrl ? ctrl.signal : undefined,
          }).then(function (resp) {
            if (!resp.ok || !resp.body || typeof resp.body.getReader !== "function") {
              throw new Error("SSE HTTP " + resp.status);
            }
            state.liveMode = "sse";
            state.streamNote = note || "SSE 已连接（fetch 流式，实时推送）";
            paintStreamNote();
            var reader = resp.body.getReader();
            var decoder = new TextDecoder();
            var buf = "";
            function pump() {
              return reader.read().then(function (chunk) {
                if (state.es !== ctrl) return; // 已被取代/停止
                if (chunk.done) {
                  startPolling("SSE 流结束，已切换 2.5s 轮询");
                  return;
                }
                buf += decoder.decode(chunk.value, { stream: true });
                var idx;
                while ((idx = buf.indexOf("\n\n")) >= 0) {
                  var frame = buf.slice(0, idx);
                  buf = buf.slice(idx + 2);
                  var dataLines = frame.split("\n").filter(function (l) {
                    return l.indexOf("data:") === 0;
                  });
                  if (!dataLines.length) continue;
                  var payload = dataLines.map(function (l) { return l.slice(5).replace(/^ /, ""); }).join("\n");
                  var f;
                  try {
                    f = JSON.parse(payload);
                  } catch (e) {
                    continue;
                  }
                  handleEventFrame(f);
                }
                return pump();
              });
            }
            return pump();
          });
        })
        .catch(function (e) {
          if (state.es !== ctrl) return; // 已被新连接/停流取代
          startPolling("SSE 不可用（" + short(String((e && e.message) || e), 60) + "），已切换 2.5s 轮询");
        });
    }

    function connectEvents(teamId) {
      stopLive();
      state.pollNoteShown = false;
      // 首选 fetch 流式 SSE（可带 Bearer，progress/audit/state 实时推送）；
      // 不支持 ReadableStream 的环境退回裸 EventSource（大概率 401 → 轮询兜底）。
      if (typeof fetch === "function" && win.ReadableStream !== undefined) {
        state.liveMode = "sse";
        state.streamNote = "正在连接 SSE（fetch 流式）…";
        paintStreamNote();
        connectFetchSse(teamId, "SSE 已连接（fetch 流式，实时推送）");
        return;
      }
      if (typeof EventSource === "undefined") {
        startPolling("浏览器不支持 EventSource，使用 2.5s 轮询");
        return;
      }
      state.liveMode = "sse";
      state.streamNote = "正在连接 SSE…";
      paintStreamNote();
      var es;
      try {
        es = new EventSource(H.baseUrl + "/teams/" + encodeURIComponent(teamId) + "/events");
      } catch (e) {
        startPolling("EventSource 创建失败，使用 2.5s 轮询");
        return;
      }
      state.es = es;
      var got = false;
      es.onopen = function () {
        if (state.es !== es) return;
        got = true;
        state.streamNote = "SSE 已连接（实时推送）";
        paintStreamNote();
      };
      es.onmessage = function (ev) {
        if (state.es !== es) return;
        var f;
        try {
          f = JSON.parse(ev.data);
        } catch (e) {
          return;
        }
        handleEventFrame(f);
      };
      es.onerror = function () {
        if (state.es !== es) return;
        // /teams/* 需 Bearer 头，EventSource 无法携带（401 → 连接失败），
        // 或连接中断：统一降级为可鉴权的 2.5s 轮询，审计自动去重。
        startPolling(
          got
            ? "SSE 中断，已切换 2.5s 轮询"
            : "SSE 不可用（受保护路由需 Bearer 令牌，浏览器 EventSource 无法携带），已切换 2.5s 轮询"
        );
      };
    }

    // ---------- 视图：创建团队 ----------
    function roleRowHtml(role) {
      role = role || {};
      return (
        '<div class="owo-ws-role-row">' +
        '<input class="owo-ws-role-name" placeholder="角色，如 planner" size="12" value="' + esc(role.role || "") + '">' +
        '<select class="owo-ws-role-assignee">' +
        '<option value="agent"' + (role.assignee === "agent" ? " selected" : "") + ">agent</option>" +
        '<option value="human"' + (role.assignee === "human" ? " selected" : "") + ">human</option>" +
        '<option value="worker"' + (role.assignee === "worker" ? " selected" : "") + ">worker</option>" +
        "</select>" +
        '<input class="owo-ws-role-worker" placeholder="worker：agent/echo/sleep/fail（human=用户 id）" size="18" value="' + esc(role.worker || "") + '">' +
        '<input class="owo-ws-role-deps" placeholder="上游角色（逗号分隔）" size="16" value="' + esc((role.depends_on || []).join(",")) + '">' +
        '<button class="owo-ws-role-del" type="button" title="删除该角色">✕</button>' +
        "</div>"
      );
    }

    function addRoleRow(role) {
      var box = el("#ws-roles");
      if (!box) return;
      box.insertAdjacentHTML("beforeend", roleRowHtml(role || {}));
    }

    function collectRoles() {
      var box = el("#ws-roles");
      if (!box) return [];
      var rows = box.querySelectorAll(".owo-ws-role-row");
      var out = [];
      for (var i = 0; i < rows.length; i++) {
        var r = rows[i];
        var role = r.querySelector(".owo-ws-role-name").value.trim();
        if (!role) continue;
        var item = {
          role: role,
          assignee: r.querySelector(".owo-ws-role-assignee").value,
        };
        var w = r.querySelector(".owo-ws-role-worker").value.trim();
        if (w) item.worker = w;
        var deps = r
          .querySelector(".owo-ws-role-deps")
          .value.split(",")
          .map(function (x) {
            return x.trim();
          })
          .filter(Boolean);
        if (deps.length) item.depends_on = deps;
        out.push(item);
      }
      return out;
    }

    function fillCreateTemplateSelect() {
      var sel = el("#ws-template");
      if (!sel) return;
      var prev = sel.value;
      sel.innerHTML =
        '<option value="">（动态组队：按上方角色规格）</option>' +
        state.templates
          .map(function (t) {
            return '<option value="' + esc(t.template_id) + '">' + esc(t.name) + "（" + esc(normStatus(t.mode)) + "）</option>";
          })
          .join("");
      if (state.templates.some(function (t) {
        return t.template_id === prev;
      }))
        sel.value = prev;
    }

    function prefillFromTemplate(tplId) {
      var t = null;
      for (var i = 0; i < state.templates.length; i++) {
        if (state.templates[i].template_id === tplId) t = state.templates[i];
      }
      if (!t) return;
      var modeSel = el("#ws-mode");
      if (modeSel) modeSel.value = normStatus(t.mode);
      var box = el("#ws-roles");
      if (!box) return;
      box.innerHTML = "";
      (t.roles || []).forEach(function (r) {
        addRoleRow(r);
      });
      if (!(t.roles || []).length) addRoleRow();
    }

    // 返回 Promise（成功/失败都在内部展示结果并 resolve）；校验失败返回 null，
    // 由 bindLockedButton 据此决定是否进入提交锁。
    function doCreate() {
      if (!state || state.view !== "create") return null;
      var objEl = el("#ws-obj");
      var objective = objEl ? objEl.value.trim() : "";
      if (!objective) {
        show("ws-create-result", "err", "objective 不能为空：请填写团队目标");
        return null;
      }
      var modeEl = el("#ws-mode");
      var tplEl = el("#ws-template");
      var mode = modeEl ? modeEl.value : "team";
      var tpl = tplEl ? tplEl.value : "";
      var body = { objective: objective, mode: mode };
      if (tpl && mode !== "single") body.template_id = tpl;
      var roles = collectRoles();
      if (roles.length) body.roles = roles;
      show("ws-create-result", "", "创建中…");
      return H.post("/teams", body)
        .then(function (d) {
          show(
            "ws-create-result",
            "ok",
            "团队已创建：" +
              (d.team_id || "（响应缺少 team_id，请到团队列表查看）") +
              "（状态 " +
              (TEAM_STATUS_CN[normStatus(d.status)] || d.status || "—") +
              "，HTTP 202 已 spawn 运行）。正在打开团队详情…"
          );
          if (!d.team_id) return; // 缺 team_id 不做跳转，避免打开 /teams/undefined
          // 跳转前的提示窗口内保持按钮禁用，避免双击重复创建团队
          var goBtn = el("#ws-create-go");
          if (goBtn) {
            goBtn.setAttribute("data-busy", "1");
            goBtn.disabled = true;
          }
          setTimeout(function () {
            unlockBtn(goBtn);
            openTeam(d.team_id);
          }, 400);
        })
        .catch(function (e) {
          show("ws-create-result", "err", explainError(e, "创建团队 POST /teams"));
        });
    }

    function renderCreate() {
      return (
        '<div class="owo-ws-sec">' +
        '<h3>创建团队 <span class="hint">POST /teams —— 创建后立即 spawn 运行（HTTP 202）</span></h3>' +
        '<label class="hint">目标 objective（必填，不能为空）</label>' +
        '<textarea id="ws-obj" rows="3" spellcheck="false" placeholder="例如：设计并实现一个文本 diff 的 CLI 工具，并给出单元测试"></textarea>' +
        '<div class="owo-ws-inline">' +
        "<label>模式 mode</label>" +
        '<select id="ws-mode">' +
        '<option value="team">team — 默认接力（planner → builder → critic → leader）</option>' +
        '<option value="single">single — 单节点（忽略模板与角色规格）</option>' +
        '<option value="swarmflow">swarmflow — DAG 流程（需模板角色或自定义角色规格）</option>' +
        "</select>" +
        "<label>模板</label>" +
        '<select id="ws-template"><option value="">（动态组队：按下方角色规格）</option></select>' +
        "</div>" +
        '<div><label class="hint">角色规格 roles（可选；留空 = 所选模式的内置默认流程；swarmflow 留空将按 4 角色接力执行）</label>' +
        '<div id="ws-roles" style="display:flex;flex-direction:column;gap:6px"></div>' +
        '<div class="owo-ws-inline" style="margin-top:6px"><button id="ws-role-add" class="owo-ws-mini" type="button">＋ 添加角色</button><span class="hint">depends_on 为逗号分隔的上游角色；assignee=human 时 worker 填用户 id</span></div></div>' +
        '<div class="owo-ws-inline"><button id="ws-create-go" class="primary">创建并启动</button><span class="hint">agent 类 worker：agent（模型驱动，需 OPENAI_API_KEY）/ echo（回显测试）/ sleep / fail</span></div>' +
        '<pre class="owo-ws-result sub" id="ws-create-result">—</pre>' +
        "</div>"
      );
    }

    function bindCreate() {
      var addBtn = el("#ws-role-add");
      if (addBtn) addBtn.onclick = function () {
        addRoleRow();
      };
      var rolesBox = el("#ws-roles");
      if (rolesBox)
        rolesBox.addEventListener("click", function (ev) {
          if (ev.target && ev.target.classList.contains("owo-ws-role-del")) ev.target.closest(".owo-ws-role-row").remove();
        });
      var tpl = el("#ws-template");
      if (tpl)
        tpl.onchange = function () {
          prefillFromTemplate(tpl.value);
        };
      bindLockedButton(el("#ws-create-go"), doCreate, "创建中…");
      addRoleRow();
      H.get("/teams/templates")
        .then(function (d) {
          state.templates = (d && d.templates) || [];
          fillCreateTemplateSelect();
        })
        .catch(function (e) {
          var s = el("#ws-template");
          if (s)
            s.insertAdjacentHTML(
              "beforeend",
              '<option value="" disabled>（模板加载失败：' +
                esc(short(explainError(e, "模板列表"), 120)) +
                "。可切走再切回本页签重试；不影响手工填写角色规格）</option>"
            );
        });
    }

    // ---------- 视图：团队列表 ----------
    function paintTeams() {
      var body = el("#ws-list-body");
      if (!body) return;
      var t = state.teams;
      if (!t.length) {
        body.innerHTML = '<div class="hint">暂无团队。切换到"创建团队"页签创建第一个团队。</div>';
        return;
      }
      body.innerHTML =
        '<table class="owo-ws-table"><tr><th>team_id</th><th>模式</th><th>状态</th><th>执行</th><th>成员</th><th>项目空间</th><th>创建</th><th>更新</th><th></th></tr>' +
        t
          .map(function (x) {
            var st = normStatus(x.status);
            var listBadge =
              x.interrupted && !isTerminalTeam(st)
                ? '<span class="owo-ws-badge st-interrupted" title="进程重启遗留的运行态：进度已保留，可用 continue / retry 显式恢复">已中断（可恢复）</span>'
                : '<span class="owo-ws-badge st-' + st + '">' + (TEAM_STATUS_CN[st] || esc(x.status)) + "</span>";
            return (
              "<tr>" +
              '<td class="hint">' + esc(x.team_id) + "</td>" +
              "<td>" + esc(normStatus(x.mode)) + "</td>" +
              "<td>" + listBadge + "</td>" +
              '<td class="' + (x.active ? "owo-ws-active on" : "hint") + '">' + (x.active ? "●" : "○") + "</td>" +
              "<td>" + ((x.members || []).length) + "</td>" +
              '<td class="hint">' + esc(x.project_space_id || "—") + "</td>" +
              '<td class="hint">' + esc(x.created_at) + "</td>" +
              '<td class="hint">' + esc(x.updated_at) + "</td>" +
              '<td><button class="owo-ws-mini" data-team="' + esc(x.team_id) + '">详情</button></td>' +
              "</tr>"
            );
          })
          .join("") +
        "</table>";
      var rows = body.querySelectorAll("button[data-team]");
      for (var i = 0; i < rows.length; i++) {
        rows[i].onclick = function () {
          openTeam(this.getAttribute("data-team"));
        };
      }
    }

    function loadTeams() {
      var body = el("#ws-list-body");
      if (body && !body.querySelector(".owo-ws-table")) {
        body.innerHTML = stateBox("loading", "正在加载团队列表…");
      }
      return H.get("/teams")
        .then(function (d) {
          state.teams = (d && d.teams) || [];
          paintTeams();
          // 空态文案由 paintTeams 输出；这里无需额外处理
        })
        .catch(function (e) {
          var b2 = el("#ws-list-body");
          if (b2) b2.innerHTML = stateBox("error", explainError(e, "团队列表 GET /teams"), "teams");
        });
    }

    function renderList() {
      return (
        '<div class="owo-ws-sec">' +
        '<h3>团队列表 <span class="hint">GET /teams</span> <button class="owo-ws-mini" id="ws-list-refresh">刷新</button></h3>' +
        '<div id="ws-list-body">' + stateBox("loading", "正在加载团队列表…") + "</div>" +
        "</div>"
      );
    }

    function bindList() {
      bindLockedButton(el("#ws-list-refresh"), loadTeams, "刷新中…");
      loadTeams();
    }

    // ---------- 视图：模板与提案 ----------
    function roleChips(roles) {
      if (!roles || !roles.length) return '<span class="hint">（无角色）</span>';
      return roles
        .map(function (r) {
          return (
            '<span class="chip">' +
            esc(r.role) +
            "（" +
            esc(r.assignee) +
            (r.worker ? ":" + esc(r.worker) : "") +
            (r.depends_on && r.depends_on.length ? " ← " + esc(r.depends_on.join(",")) : "") +
            "）</span>"
          );
        })
        .join(" ");
    }

    function tplCard(t) {
      return (
        '<div class="owo-ws-tpl-card">' +
        '<div class="owo-ws-member-head"><b>' + esc(t.name) + "</b>" +
        '<span class="chip">' + esc(normStatus(t.mode)) + "</span>" +
        '<span class="hint">' + esc(t.template_id) + "</span></div>" +
        '<div class="hint">适用条件：' + esc(t.applicability || "—") + "</div>" +
        '<div class="hint">' + roleChips(t.roles) + "</div>" +
        '<div class="hint">创建 ' + esc(t.created_at || "—") + (t.source_team_id ? " · 来源团队 " + esc(t.source_team_id) : "") + "</div>" +
        '<div class="owo-ws-inline"><button class="owo-ws-mini" data-usetpl="' + esc(t.template_id) + '">用此模板创建团队</button></div>' +
        "</div>"
      );
    }

    function propCard(p) {
      return (
        '<div class="owo-ws-tpl-card">' +
        '<div class="owo-ws-member-head"><b>' + esc(p.template.name) + "</b>" +
        '<span class="chip">' + esc(normStatus(p.template.mode)) + "</span>" +
        '<span class="chip owo-ws-prop-' + esc(p.status) + '">' +
        (PROPOSAL_STATUS_CN[p.status] || esc(p.status)) +
        "</span></div>" +
        '<div class="hint">提案 ' + esc(p.proposal_id) + " · 来源团队 " + esc(p.source_team_id) + " · 生成 " + esc(p.created_at) + "</div>" +
        '<div class="hint">证据：' +
        (p.evidence && p.evidence.length
          ? p.evidence
              .map(function (x) {
                return '<span class="chip">' + esc(x) + "</span>";
              })
              .join(" ")
          : "—") +
        "</div>" +
        '<div class="hint">' + roleChips(p.template.roles) + "</div>" +
        (p.status === "proposed"
          ? '<div class="owo-ws-inline"><button class="primary" data-adopt="' + esc(p.proposal_id) + '">采纳</button><button class="danger" data-reject="' + esc(p.proposal_id) + '">拒绝</button></div>'
          : "") +
        '<pre class="owo-ws-result sub" data-proj-result="' + esc(p.proposal_id) + '">—</pre>' +
        "</div>"
      );
    }

    function paintTemplates() {
      var box = el("#ws-tpl-list");
      if (!box) return;
      box.innerHTML = state.templates.length
        ? state.templates.map(tplCard).join("")
        : '<div class="hint">暂无已采纳模板。先运行并验证一个团队，待系统生成模板提案后在右侧"采纳"。</div>';
      var btns = box.querySelectorAll("button[data-usetpl]");
      for (var i = 0; i < btns.length; i++) {
        btns[i].onclick = function () {
          var tplId = this.getAttribute("data-usetpl");
          state.view = "create";
          renderView();
          var sel = el("#ws-template");
          if (sel) sel.value = tplId;
          prefillFromTemplate(tplId);
        };
      }
    }

    function paintProposals() {
      var box = el("#ws-prop-list");
      if (!box) return;
      box.innerHTML = state.proposals.length
        ? state.proposals.map(propCard).join("")
        : '<div class="hint">暂无模板提案。团队成功完成一次运行后，系统会据此生成模板提案（只提案，不自动启用）。</div>';
      var btns = box.querySelectorAll("button[data-adopt], button[data-reject]");
      for (var i = 0; i < btns.length; i++) {
        (function (btn) {
          btn.onclick = function () {
            if (btn.disabled || btn.getAttribute("data-busy") === "1") return;
            var adopt = btn.hasAttribute("data-adopt");
            var attr = adopt ? "data-adopt" : "data-reject";
            var pid = btn.getAttribute(attr);
            var pre = box.querySelector('pre[data-proj-result="' + pid + '"]');
            // 提交期间同时锁定同一张卡的「采纳/拒绝」两个按钮，防重复与并发操作
            var mate = box.querySelector((adopt ? 'button[data-reject="' : 'button[data-adopt="') + pid + '"]');
            var p = adopt ? adoptProposal(pid, pre) : rejectProposal(pid, pre); // 确认取消 → null
            if (!p || typeof p.then !== "function") return;
            lockBtn(btn, adopt ? "采纳中…" : "拒绝中…");
            if (mate) mate.disabled = true;
            p.then(
              function () {
                unlockBtn(btn);
                if (mate && !mate.getAttribute("data-busy")) mate.disabled = false;
              },
              function () {
                unlockBtn(btn);
                if (mate && !mate.getAttribute("data-busy")) mate.disabled = false;
              }
            );
          };
        })(btns[i]);
      }
    }

    function loadTemplates() {
      var box = el("#ws-tpl-list");
      if (box && !box.querySelector(".owo-ws-tpl-card")) {
        box.innerHTML = stateBox("loading", "正在加载已采纳模板…");
      }
      return H.get("/teams/templates")
        .then(function (d) {
          state.templates = (d && d.templates) || [];
          paintTemplates();
          fillCreateTemplateSelect();
        })
        .catch(function (e) {
          var b2 = el("#ws-tpl-list");
          if (b2) b2.innerHTML = stateBox("error", explainError(e, "模板列表 GET /teams/templates"), "templates");
        });
    }

    function loadProposals() {
      var box = el("#ws-prop-list");
      if (box && !box.querySelector(".owo-ws-tpl-card")) {
        box.innerHTML = stateBox("loading", "正在加载模板提案…");
      }
      return H.get("/teams/templates/proposals")
        .then(function (d) {
          state.proposals = (d && d.proposals) || [];
          paintProposals();
        })
        .catch(function (e) {
          var b2 = el("#ws-prop-list");
          if (b2) b2.innerHTML = stateBox("error", explainError(e, "提案列表 GET /teams/templates/proposals"), "proposals");
        });
    }

    // 采纳/拒绝：成功与失败都在提案卡内的结果区展示；返回 Promise 供提交锁。
    function adoptProposal(pid, pre) {
      return H.post("/teams/templates/proposals/" + encodeURIComponent(pid) + "/adopt", {})
        .then(function (d) {
          if (pre) {
            pre.textContent = "已采纳 → 模板 " + (d.template ? d.template.template_id + " · " + d.template.name : "（见左侧列表）");
            pre.className = "owo-ws-result ok";
          }
          loadTemplates();
          loadProposals();
        })
        .catch(function (e) {
          if (pre) {
            pre.textContent = explainError(e, "采纳提案 " + pid);
            pre.className = "owo-ws-result err";
          }
        });
    }

    function rejectProposal(pid, pre) {
      if (!win.confirm("拒绝该模板提案？提案记录会保留以供审计，且无法再被采纳。")) return null;
      return H.post("/teams/templates/proposals/" + encodeURIComponent(pid) + "/reject", {})
        .then(function (d) {
          if (pre) {
            pre.textContent = "已拒绝提案 " + (d.proposal_id || pid) + "（状态 " + (d.status || "rejected") + "，记录保留可审计）";
            pre.className = "owo-ws-result ok";
          }
          loadProposals();
        })
        .catch(function (e) {
          if (pre) {
            pre.textContent = explainError(e, "拒绝提案 " + pid);
            pre.className = "owo-ws-result err";
          }
        });
    }

    function renderTemplates() {
      return (
        '<div class="owo-ws-sec">' +
        '<h3>已采纳模板 <span class="hint">GET /teams/templates —— 采纳后可在创建团队时选用</span> <button class="owo-ws-mini" id="ws-tpl-refresh">刷新</button></h3>' +
        '<div class="owo-ws-tpl-grid" id="ws-tpl-list">' + stateBox("loading", "正在加载已采纳模板…") + "</div>" +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>模板提案 <span class="hint">GET /teams/templates/proposals —— 只提案不自动启用，需人工采纳/拒绝</span> <button class="owo-ws-mini" id="ws-prop-refresh">刷新</button></h3>' +
        '<div class="owo-ws-tpl-grid" id="ws-prop-list">' + stateBox("loading", "正在加载模板提案…") + "</div>" +
        "</div>"
      );
    }

    function bindTemplates() {
      bindLockedButton(el("#ws-tpl-refresh"), loadTemplates, "刷新中…");
      bindLockedButton(el("#ws-prop-refresh"), loadProposals, "刷新中…");
      loadTemplates();
      loadProposals();
    }

    // ---------- 视图：团队详情 ----------
    function toggleForm(id, btnId) {
      var f = el(id); // 调用方传裸 ID，避免 "##id"
      var b = el(btnId);
      if (!f) return;
      f.style.display = f.style.display === "none" ? "" : "none";
      if (b) b.textContent = b.textContent.replace(/[▴▾]$/, f.style.display === "none" ? "▾" : "▴");
    }

    // 返回 Promise；校验失败/被终态拦截返回 null。
    function doSteerPost(body, label) {
      if (!state.current) return null;
      if (isTerminalTeam(currentTeamStatus())) {
        show("ws-act-result", "err", "团队已进入终态（" + (TEAM_STATUS_CN[normStatus(currentTeamStatus())] || currentTeamStatus()) + "），操作已停用。如需重新运行请新建团队");
        return null;
      }
      return H.post("/teams/" + encodeURIComponent(state.current) + "/steer", body)
        .then(function (d) {
          var st = normStatus(d && d.status);
          show("ws-act-result", "ok", "[" + label + "] 已提交，团队状态：" + (TEAM_STATUS_CN[st] || (d && d.status) || "—"));
          syncDetail();
        })
        .catch(function (e) {
          show("ws-act-result", "err", explainError(e, label + " POST /teams/{id}/steer"));
        });
    }

    function doSteer() {
      if (!state || state.view !== "detail") return null;
      var stepEl = el("#ws-act-steer-step");
      var noteEl = el("#ws-act-steer-note");
      var rawEl = el("#ws-act-steer-input");
      var step = stepEl ? stepEl.value : "";
      var note = noteEl ? noteEl.value.trim() : "";
      if (!note) {
        show("ws-act-result", "err", "steer 需要 note（转向指令内容），请填写");
        return null;
      }
      var rawIn = rawEl ? rawEl.value.trim() : "";
      var body = { command: "steer", note: note };
      if (step) body.step_id = step;
      if (rawIn) {
        var v;
        try {
          v = JSON.parse(rawIn);
        } catch (e) {
          show("ws-act-result", "err", 'new_input 必须是合法 JSON（例如 {"prompt": "..."}）');
          return null;
        }
        body.new_input = v;
      }
      return doSteerPost(body, "转向");
    }

    function doReplace() {
      if (!state || state.view !== "detail") return null;
      var roleEl = el("#ws-act-replace-role");
      var role = roleEl ? roleEl.value : "";
      if (!role) {
        show("ws-act-result", "err", "replace 需要 role 字段：请先选择角色");
        return null;
      }
      var body = { command: "replace", role: role };
      var w = el("#ws-act-replace-worker");
      var u = el("#ws-act-replace-user");
      var n = el("#ws-act-replace-note");
      if (w && w.value.trim()) body.new_worker = w.value.trim();
      if (u && u.value.trim()) body.new_user_id = u.value.trim();
      if (n && n.value.trim()) body.note = n.value.trim();
      return doSteerPost(body, "换员");
    }

    function doHumanResult() {
      if (!state || state.view !== "detail" || !state.current) return null;
      var tidEl = el("#ws-h-task");
      var textEl = el("#ws-h-text");
      var tid = tidEl ? tidEl.value : "";
      var result = textEl ? textEl.value.trim() : "";
      if (isTerminalTeam(currentTeamStatus())) {
        show("ws-h-result", "err", "团队已进入终态，人节点结果提交已停用");
        return null;
      }
      if (!tid) {
        show("ws-h-result", "err", "请选择一个未完成的人节点任务（当前没有可提交的任务）");
        return null;
      }
      if (!result) {
        show("ws-h-result", "err", "人节点结果不能为空：请填写结果内容");
        return null;
      }
      return H.post("/tasks/" + encodeURIComponent(tid) + "/human-result", { team_id: state.current, result: result })
        .then(function (d) {
          show(
            "ws-h-result",
            "ok",
            "人节点结果已提交 → 产物 " +
              (d.artifact && d.artifact.artifact_id ? d.artifact.artifact_id : "（见审计）") +
              "（评审状态 " +
              (d.artifact && d.artifact.review_state ? d.artifact.review_state : "—") +
              "），下游任务将自动继续。"
          );
          if (textEl) textEl.value = "";
          syncDetail();
        })
        .catch(function (e) {
          show("ws-h-result", "err", explainError(e, "人节点结果 POST /tasks/{id}/human-result"));
        });
    }

    function doHandoff() {
      if (!state || state.view !== "detail" || !state.current) return null;
      var tidEl = el("#ws-x-task");
      var fromEl = el("#ws-x-from");
      var summaryEl = el("#ws-x-summary");
      var toEl = el("#ws-x-to");
      var tid = tidEl ? tidEl.value : "";
      var from = fromEl ? fromEl.value : "";
      var summary = summaryEl ? summaryEl.value.trim() : "";
      if (isTerminalTeam(currentTeamStatus())) {
        show("ws-x-result", "err", "团队已进入终态，任务交接已停用");
        return null;
      }
      if (!tid) {
        show("ws-x-result", "err", "请选择要交接的任务");
        return null;
      }
      if (!from) {
        show("ws-x-result", "err", "from_member 不能为空（必须为任务执行者 m-&lt;角色&gt;）");
        return null;
      }
      if (!summary) {
        show("ws-x-result", "err", "completed_summary 不能为空：请描述已完成的内容");
        return null;
      }
      var body = {
        team_id: state.current,
        from_member: from,
        completed_summary: summary,
        open_issues: splitCsv(el("#ws-x-issues")),
        output_artifact_refs: splitCsv(el("#ws-x-arts")),
        evidence_refs: splitCsv(el("#ws-x-evid")),
        suggested_next_actions: splitCsv(el("#ws-x-next")),
        known_risks: splitCsv(el("#ws-x-risks")),
      };
      var to = toEl ? toEl.value.trim() : "";
      if (to) body.to_member = to;
      return H.post("/tasks/" + encodeURIComponent(tid) + "/handoff", body)
        .then(function (d) {
          show(
            "ws-x-result",
            "ok",
            "交接已提交 → 交接记录 " +
              (d.handoff && d.handoff.handoff_id ? d.handoff.handoff_id : "（见审计）") +
              "。下游任务可据此继续。"
          );
          if (summaryEl) summaryEl.value = "";
          syncDetail();
        })
        .catch(function (e) {
          show("ws-x-result", "err", explainError(e, "交接 POST /tasks/{id}/handoff"));
        });
    }

    function renderDetail() {
      return (
        '<div class="owo-ws-d-head">' +
        '<button class="owo-ws-mini" id="ws-d-back">← 团队列表</button>' +
        '<span class="owo-ws-d-id" id="ws-d-id">…</span>' +
        '<span class="owo-ws-badge" id="ws-d-status">加载中</span>' +
        '<span class="owo-ws-active" id="ws-d-active"></span>' +
        '<span class="hint" id="ws-d-meta">正在加载团队详情…</span>' +
        "</div>" +
        '<div id="ws-d-main" style="display:flex;flex-direction:column;gap:10px">' +
        '<pre class="owo-ws-result sub" id="ws-d-error"></pre>' +
        '<div class="owo-ws-sec">' +
        '<h3>运行摘要 <span class="hint">状态 / 活动阶段 / 任务计数 / 失败步骤 / 尝试 / 产物</span></h3>' +
        '<div id="ws-d-summary" class="owo-ws-sumbox" aria-live="polite">' + stateBox("loading", "正在汇总运行状态…") + "</div>" +
        "</div>" +
        '<div class="owo-ws-interrupted" id="ws-d-interrupted" role="alert">⚠ 运行已中断，可恢复' +
        '<span class="owo-ws-int-detail">检测到上次进程未正常收尾遗留的运行态：任务进度已保留，未自动重放任何写操作。可用「继续（continue）」恢复运行，或对失败/被中断节点点「重试此节点」。</span></div>' +
        '<div class="owo-ws-gate" id="ws-d-gate">⛔ 团队已进入终态：运行操作与人节点结果提交已停用（仍可查看任务、产物与审计）。</div>' +
        '<div class="owo-ws-sec">' +
        '<h3>运行操作 <span class="hint">POST /teams/{id}/steer —— continue / retry（失败节点按钮）/ steer / replace / cancel</span></h3>' +
        '<div class="owo-ws-inline">' +
        '<button id="ws-act-continue" class="owo-ws-mini">继续（continue）</button>' +
        '<button id="ws-act-steer-toggle" class="owo-ws-mini">转向（steer）▾</button>' +
        '<button id="ws-act-replace-toggle" class="owo-ws-mini">换员（replace）▾</button>' +
        '<button id="ws-act-cancel" class="danger">取消（cancel）</button>' +
        "</div>" +
        '<div class="owo-ws-form" id="ws-act-steer-form" style="display:none">' +
        '<div class="owo-ws-inline"><label>目标任务 step_id</label><select id="ws-act-steer-step"></select>' +
        '<label>指令 note</label><input id="ws-act-steer-note" placeholder="转向指令（必填）" size="32"></div>' +
        '<label class="hint">新输入 new_input（可选 JSON，覆盖该任务输入）</label>' +
        '<textarea id="ws-act-steer-input" rows="2" spellcheck="false" placeholder="{&quot;prompt&quot;: &quot;改为…&quot;}"></textarea>' +
        '<div class="owo-ws-inline"><button id="ws-act-steer-go" class="primary">发送转向</button></div>' +
        "</div>" +
        '<div class="owo-ws-form" id="ws-act-replace-form" style="display:none">' +
        '<div class="owo-ws-inline"><label>角色 role</label><select id="ws-act-replace-role"></select>' +
        '<label>新 worker</label><input id="ws-act-replace-worker" placeholder="agent / echo / sleep / fail" size="14">' +
        '<label>新用户 new_user_id</label><input id="ws-act-replace-user" placeholder="人节点换人时填写" size="12">' +
        '<label>note</label><input id="ws-act-replace-note" placeholder="换员原因（可选）" size="20"></div>' +
        '<div class="owo-ws-inline"><button id="ws-act-replace-go" class="primary">提交换员</button></div>' +
        '<p class="hint" style="margin:0">新 worker 自该角色任务的下一轮尝试生效；人节点可用 new_user_id 更换承接用户。</p>' +
        "</div>" +
        '<pre class="owo-ws-result sub" id="ws-act-result">—</pre>' +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>成员与职责 <span class="hint">GET /teams/{id} · members / runtime_binding / handoff_contract</span></h3>' +
        '<div class="owo-ws-member-grid" id="ws-d-members">' + stateBox("loading", "正在加载成员与职责…") + "</div>" +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>任务 DAG <span class="hint">红色边框 = 已阻塞（上游失败/中止，不会再运行）</span> <button class="owo-ws-mini" id="ws-dag-refresh">刷新任务</button></h3>' +
        '<div id="ws-d-dag">' + stateBox("loading", "正在加载任务…") + "</div>" +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>人节点结果 <span class="hint">POST /tasks/{id}/human-result —— 团队处于"等待人节点"时由人工提交结果，下游任务自动继续</span></h3>' +
        '<div class="owo-ws-inline">' +
        "<label>人节点任务</label><select id=\"ws-h-task\"></select>" +
        "</div>" +
        '<label class="hint">结果内容 result（必填；将存为产物并注入下游任务输入）</label>' +
        '<textarea id="ws-h-text" rows="3" spellcheck="false" placeholder="人工完成的产物/结论，例如：验收意见 + 修订要求"></textarea>' +
        '<div class="owo-ws-inline"><button id="ws-h-go" class="primary">提交结果</button><span class="hint" id="ws-h-gate-note"></span></div>' +
        '<pre class="owo-ws-result sub" id="ws-h-result">—</pre>' +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>任务交接 <span class="hint">POST /tasks/{id}/handoff —— 记录"谁完成了什么、留下什么问题、下一步建议"</span></h3>' +
        '<div class="owo-ws-inline">' +
        "<label>任务</label><select id=\"ws-x-task\"></select>" +
        "<label>from_member</label><select id=\"ws-x-from\"></select>" +
        '<label>to_member（可选）</label><input id="ws-x-to" placeholder="m-&#60;角色&#62;，留空 = 交由团队/下游自动" size="16">' +
        "</div>" +
        '<label class="hint">completed_summary 完成摘要（必填）</label>' +
        '<textarea id="ws-x-summary" rows="2" spellcheck="false"></textarea>' +
        '<details class="hint"><summary>可选明细（逗号分隔多项）</summary>' +
        '<div style="display:flex;flex-direction:column;gap:4px;margin:6px 0">' +
        '<label>open_issues 遗留问题 <input id="ws-x-issues" size="60"></label>' +
        '<label>output_artifact_refs 产物引用 <input id="ws-x-arts" size="60"></label>' +
        '<label>evidence_refs 证据引用 <input id="ws-x-evid" size="60"></label>' +
        '<label>suggested_next_actions 下一步建议 <input id="ws-x-next" size="60"></label>' +
        '<label>known_risks 已知风险 <input id="ws-x-risks" size="60"></label>' +
        "</div></details>" +
        '<div class="owo-ws-inline"><button id="ws-x-go" class="primary">提交交接</button></div>' +
        '<pre class="owo-ws-result sub" id="ws-x-result">—</pre>' +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>实时进度 <span class="hint">progress 事件（步骤开始/完成/失败/取消，seq 单调递增；断线恢复后旧 seq 自动跳过）</span></h3>' +
        '<div id="ws-d-progress" class="owo-ws-prog" aria-live="polite"><div class="hint">等待进度数据…</div></div>' +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>产物 <span class="hint">GET /projects/{pid}/artifacts —— CAS 引用，版本链 + 评审闭环</span> <button class="owo-ws-mini" id="ws-art-refresh">刷新产物</button></h3>' +
        '<div id="ws-d-artifacts">' + stateBox("loading", "正在加载产物…") + "</div>" +
        "</div>" +
        '<div class="owo-ws-sec">' +
        '<h3>审计事件（实时） <span class="hint">GET /teams/{id}/events</span> <span class="owo-ws-live off" id="ws-d-stream"></span></h3>' +
        '<div class="owo-ws-audit" id="ws-d-audit" role="log" aria-label="审计事件流（可聚焦后用方向键滚动）" tabindex="0">' + stateBox("loading", "正在连接审计通道…") + "</div>" +
        "</div>" +
        "</div>"
      );
    }

    function bindDetail() {
      var back = el("#ws-d-back");
      if (back)
        back.onclick = function () {
          stopLive();
          state.view = "list";
          renderView();
        };
      // 运行操作：continue / steer / replace / cancel 全部带提交锁；
      // toggle 按钮仅展开表单，不涉及请求，不锁。
      bindLockedButton(el("#ws-act-continue"), function () {
        return doSteerPost({ command: "continue", note: "继续" }, "继续");
      }, "提交中…");
      var stT = el("#ws-act-steer-toggle");
      if (stT)
        stT.onclick = function () {
          toggleForm("ws-act-steer-form", "ws-act-steer-toggle");
        };
      bindLockedButton(el("#ws-act-steer-go"), doSteer, "发送中…");
      var rpT = el("#ws-act-replace-toggle");
      if (rpT)
        rpT.onclick = function () {
          toggleForm("ws-act-replace-form", "ws-act-replace-toggle");
        };
      bindLockedButton(el("#ws-act-replace-go"), doReplace, "提交中…");
      bindLockedButton(el("#ws-act-cancel"), function () {
        if (!win.confirm("取消该团队运行？未完成的任务将被中止，运行无法恢复。")) return null;
        state.cancelling = true; // 立即反馈"取消中"，不等服务端往返
        paintProgress();
        var p = doSteerPost({ command: "cancel", note: "用户取消" }, "取消");
        // 下发失败（网络/权限）：撤销"取消中"标记，让用户可重试
        if (p && typeof p.catch === "function") {
          p = p.catch(function (e) {
            state.cancelling = false;
            paintProgress();
            throw e;
          });
        }
        return p;
      }, "取消中…");
      bindLockedButton(el("#ws-h-go"), doHumanResult, "提交中…");
      bindLockedButton(el("#ws-x-go"), doHandoff, "提交中…");
      var xTask = el("#ws-x-task");
      if (xTask)
        xTask.onchange = function () {
          paintFromSelect();
        };
      bindLockedButton(el("#ws-dag-refresh"), refreshTasksOnly, "刷新中…");
      bindLockedButton(el("#ws-art-refresh"), loadArtifacts, "刷新中…");
      markLiveRegions();
      applyGating(); // 以当前已知状态初始化按钮可用性（数据到达后再刷新）
    }

    function openTeam(id) {
      if (!id || typeof id !== "string") return; // 防 /teams/undefined
      stopLive();
      state.view = "detail";
      state.current = id;
      state.team = null;
      state.tasks = [];
      state.audit = [];
      state.auditKeys = {};
      state.active = false;
      state.teamStatus = "";
      state.interrupted = false;
      state.artifactCount = null;
      state.humanTask = "";
      state.progress = null;
      state.lastProgressSeq = 0;
      state.cancelling = false;
      state.artifacts = [];
      state.reviewBusy = {};
      state.reviewResult = null;
      state.reviewFlash = null;
      stopProgressTimer();
      renderView();
    }

    // ---------- 视图调度 ----------
    function paintTabs() {
      if (!rootEl) return;
      var activeView = state.view === "detail" ? "list" : state.view;
      var tabs = rootEl.querySelectorAll(".owo-ws-tab");
      for (var i = 0; i < tabs.length; i++) {
        var b = tabs[i];
        var on = b.getAttribute("data-view") === activeView;
        if (on && !b.classList.contains("active")) b.classList.add("active");
        if (!on && b.classList.contains("active")) b.classList.remove("active");
      }
    }

    function renderView() {
      var main = el("#ws-main");
      if (!main) return;
      try {
        paintTabs();
        // 离开详情视图必须停掉 SSE/轮询，避免在创建页/列表页留下幽灵请求
        if (state.view !== "detail") stopLive();
        if (state.view === "create") {
          main.innerHTML = renderCreate();
          bindCreate();
        } else if (state.view === "templates") {
          main.innerHTML = renderTemplates();
          bindTemplates();
        } else if (state.view === "detail" && state.current) {
          main.innerHTML = renderDetail();
          bindDetail();
          loadDetail(state.current);
        } else {
          // detail 但没有 current：回退列表，防止渲染 /teams/undefined 页面
          if (state.view === "detail") state.view = "list";
          paintTabs();
          main.innerHTML = renderList();
          bindList();
        }
        markLiveRegions();
      } catch (e) {
        // 渲染兜底：宁可显示错误也不要整块空白
        if (typeof console !== "undefined" && console.error) console.error("[workswarm] 视图渲染异常:", e);
        main.innerHTML =
          '<div class="owo-ws-sec"><h3>面板渲染异常</h3><div class="hint err">' +
          esc((e && e.message) || String(e)) +
          '</div><div class="hint" style="margin-top:6px">请切换到其他页签（如"团队列表"）后重试；若持续出现，请联系维护者并提供控制台报错。</div></div>';
      }
    }

    function bindTabs() {
      var tabs = rootEl.querySelectorAll(".owo-ws-tab");
      for (var i = 0; i < tabs.length; i++) {
        (function (b) {
          b.addEventListener("click", function () {
            state.view = b.getAttribute("data-view");
            renderView();
          });
        })(tabs[i]);
      }
    }

    // ---------- 样式（全部 .owo-ws-* 前缀） ----------
    // 注意：<style> 注入是全局生效的。旧版这里存在裸选择器（.hint/.chip/
    // button.primary/select,input,textarea…），会把外壳的全局表单样式整个
    // 覆盖掉 —— 现已全部收敛到 .owo-ws-panel 作用域下。
    // 颜色一律走 style.css 的设计令牌（--surface/--muted/--green…），
    // 深浅主题自动跟随外壳切换；fallback 仅保证面板被单独打开时可用。
    var CSS =
      "section.owo-ws-panel{display:flex;flex-direction:column;gap:10px;padding:4px;min-width:0;}" +
      ".owo-ws-tabs{display:flex;gap:6px;flex-wrap:wrap;}" +
      ".owo-ws-tab{background:transparent;border:1px solid var(--border,#e2e6ee);color:var(--muted,#5b6577);padding:5px 13px;border-radius:999px;cursor:pointer;font-size:13px;font-family:inherit;" +
      "transition:background-color .15s var(--ease-out,ease-out),color .15s var(--ease-out,ease-out),border-color .15s var(--ease-out,ease-out);}" +
      "@media(hover:hover){.owo-ws-tab:hover:not(.active){background:var(--hover,#ebEEF5);color:var(--text,#1d2433);}}" +
      // 激活页签与外壳 Rail 一致的着色药丸语言
      ".owo-ws-tab.active{background:var(--active-tint,#e7eeff);border-color:transparent;color:var(--accent,#2563eb);font-weight:600;}" +
      ".owo-ws-main{display:flex;flex-direction:column;gap:10px;min-width:0;}" +
      ".owo-ws-sec{border:1px solid var(--border,#e2e6ee);border-radius:12px;background:var(--surface,#fff);box-shadow:var(--shadow-sm,0 1px 2px rgba(18,25,40,.05));padding:12px 14px;display:flex;flex-direction:column;gap:8px;min-width:0;}" +
      ".owo-ws-sec h3{margin:0;font-size:14px;display:flex;gap:8px;align-items:center;flex-wrap:wrap;}" +
      // 以下通用类均限定在面板作用域内，不再污染全局
      ".owo-ws-panel .hint{font-size:12px;color:var(--muted,#5b6577);}" +
      ".owo-ws-panel .hint.err{color:var(--red,#cd3f35);}" +
      "pre.owo-ws-result{white-space:pre-wrap;word-break:break-word;font-size:12px;margin:0;max-height:180px;overflow:auto;font-family:inherit;border:1px solid var(--soft-border,#edf0f5);border-radius:8px;background:var(--raised,#f1f3f8);padding:7px 9px;}" +
      ".owo-ws-result.ok{color:var(--green,#14855a);border-color:var(--green-line,rgba(20,133,90,.3));}" +
      ".owo-ws-result.err{color:var(--red,#cd3f35);border-color:var(--red-line,rgba(207,63,53,.32));background:var(--red-soft,rgba(207,63,53,.1));}" +
      ".owo-ws-result.sub{color:var(--text,#1d2433);background:var(--surface,#fff);}" +
      ".owo-ws-panel button.owo-ws-mini{background:transparent;border:1px solid var(--border,#e2e6ee);color:var(--text,#1d2433);padding:3px 9px;border-radius:7px;cursor:pointer;font-size:11px;font-family:inherit;" +
      "transition:border-color .15s var(--ease-out,ease-out),background-color .15s var(--ease-out,ease-out),transform .15s var(--ease-out,ease-out);}" +
      "@media(hover:hover){.owo-ws-panel button.owo-ws-mini:hover:not([disabled]){border-color:var(--accent,#2563eb);background:var(--accent-soft,rgba(37,99,235,.09));}}" +
      ".owo-ws-panel button.primary{background:var(--accent,#2563eb);border:1px solid var(--accent,#2563eb);color:var(--accent-ink,#fff);padding:5px 13px;border-radius:8px;cursor:pointer;font-size:12px;font-family:inherit;font-weight:600;}" +
      "@media(hover:hover){.owo-ws-panel button.primary:hover:not([disabled]){background:var(--accent-hover,#1e56cf);border-color:var(--accent-hover,#1e56cf);}}" +
      ".owo-ws-panel button.danger{background:transparent;border:1px solid var(--red-line,rgba(207,63,53,.32));color:var(--red,#cd3f35);padding:5px 13px;border-radius:8px;cursor:pointer;font-size:12px;font-family:inherit;}" +
      "@media(hover:hover){.owo-ws-panel button.danger:hover:not([disabled]){background:var(--red-soft,rgba(207,63,53,.1));}}" +
      ".owo-ws-panel select,.owo-ws-panel input,.owo-ws-panel textarea{width:auto;background:var(--surface,#fff);border:1px solid var(--border,#e2e6ee);color:inherit;border-radius:7px;padding:4px 8px;font-size:12px;font-family:inherit;max-width:100%;}" +
      ".owo-ws-table{width:100%;border-collapse:collapse;font-size:12px;}" +
      ".owo-ws-table th,.owo-ws-table td{border-bottom:1px solid var(--soft-border,#edf0f5);padding:5px 7px;text-align:left;vertical-align:top;}" +
      ".owo-ws-table th{color:var(--muted,#5b6577);font-weight:600;white-space:nowrap;}" +
      ".owo-ws-badge{display:inline-block;padding:1px 9px;border-radius:10px;font-size:11px;background:var(--raised,#f1f3f8);color:var(--muted,#5b6577);font-variant-numeric:tabular-nums;}" +
      // 团队状态徽标：语义色平铺到柔和底色上，深浅主题同源
      ".owo-ws-badge.st-created{background:var(--active-tint,#e7eeff);color:var(--accent,#2563eb);}" +
      ".owo-ws-badge.st-running,.owo-ws-badge.st-succeeded{background:var(--green-soft,rgba(20,133,90,.11));color:var(--green,#14855a);}" +
      ".owo-ws-badge.st-awaiting_human{background:var(--yellow-soft,rgba(199,138,26,.13));color:var(--yellow,#a06a04);}" +
      ".owo-ws-badge.st-failed{background:var(--red-soft,rgba(207,63,53,.1));color:var(--red,#cd3f35);}" +
      ".owo-ws-active{font-size:12px;color:var(--faint,#8a93a5);font-variant-numeric:tabular-nums;}" +
      ".owo-ws-active.on{color:var(--green,#14855a);font-weight:600;}" +
      ".owo-ws-member-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(270px,1fr));gap:8px;}" +
      ".owo-ws-member{border:1px solid var(--border,#e2e6ee);border-radius:10px;padding:9px;display:flex;flex-direction:column;gap:4px;font-size:12px;background:var(--surface,#fff);}" +
      ".owo-ws-member-head{display:flex;gap:6px;align-items:center;flex-wrap:wrap;}" +
      ".owo-ws-panel .chip{display:inline-block;padding:0 8px;border-radius:9px;font-size:11px;background:var(--raised,#f1f3f8);color:var(--muted,#5b6577);}" +
      ".owo-ws-panel .chip.h-active{color:var(--green,#14855a);}" +
      ".owo-ws-panel .chip.h-degraded{color:var(--yellow,#a06a04);}" +
      ".owo-ws-panel .chip.h-offline,.owo-ws-panel .chip.h-fused{color:var(--red,#cd3f35);}" +
      // 提案状态徽标（替代旧的行内 style 三元硬编码）
      ".owo-ws-panel .chip.owo-ws-prop-proposed{color:var(--yellow,#a06a04);background:var(--yellow-soft,rgba(199,138,26,.13));}" +
      ".owo-ws-panel .chip.owo-ws-prop-adopted{color:var(--green,#14855a);background:var(--green-soft,rgba(20,133,90,.11));}" +
      ".owo-ws-panel .chip.owo-ws-prop-rejected{color:var(--red,#cd3f35);background:var(--red-soft,rgba(207,63,53,.1));}" +
      ".owo-ws-dagwrap{overflow:auto;border:1px solid var(--soft-border,#edf0f5);border-radius:10px;max-height:560px;background:var(--raised,#f1f3f8);}" +
      ".owo-ws-dag{display:block;}" +
      ".owo-ws-dag line{stroke:var(--scroll-thumb,rgba(122,132,152,.38));stroke-width:1.5;}" +
      ".owo-ws-dag line.dead{stroke:var(--red,#cd3f35);}" +
      ".owo-ws-node{box-sizing:border-box;width:100%;height:100%;border:1px solid var(--border,#e2e6ee);border-radius:9px;padding:6px 8px;font-size:12px;background:var(--surface,#fff);display:flex;flex-direction:column;gap:2px;overflow:hidden;}" +
      ".owo-ws-node.st-running{border-color:var(--accent,#2563eb);box-shadow:0 0 0 1px var(--accent-soft,rgba(37,99,235,.09)) inset;}" +
      ".owo-ws-node.st-succeeded{border-color:var(--green-line,rgba(20,133,90,.3));}" +
      ".owo-ws-node.st-failed,.owo-ws-node.st-aborted{border-color:var(--red-line,rgba(207,63,53,.32));}" +
      ".owo-ws-node.st-pending,.owo-ws-node.st-ready{border-style:dashed;}" +
      ".owo-ws-node.blocked{border:2px solid var(--red,#cd3f35);}" +
      ".owo-ws-node-line{display:flex;justify-content:space-between;gap:6px;align-items:center;}" +
      ".owo-ws-node-line b{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}" +
      ".owo-ws-node-err{color:var(--red,#cd3f35);font-size:11px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;}" +
      ".owo-ws-audit{max-height:340px;overflow:auto;border:1px solid var(--soft-border,#edf0f5);border-radius:10px;padding:6px 10px;font-size:12px;background:var(--raised,#f1f3f8);}" +
      ".owo-ws-audit-row{display:flex;gap:8px;padding:2px 0;border-bottom:1px solid var(--soft-border,#edf0f5);}" +
      ".owo-ws-audit-ts{color:var(--faint,#8a93a5);white-space:nowrap;font-family:Consolas,ui-monospace,monospace;font-variant-numeric:tabular-nums;}" +
      ".owo-ws-audit-ev{color:var(--accent,#2563eb);white-space:nowrap;min-width:110px;}" +
      ".owo-ws-audit-det{color:var(--text,#1d2433);word-break:break-all;opacity:.85;}" +
      ".owo-ws-role-row{display:flex;gap:6px;flex-wrap:wrap;align-items:center;}" +
      ".owo-ws-role-del{background:transparent;border:1px solid var(--red-line,rgba(207,63,53,.32));color:var(--red,#cd3f35);border-radius:6px;cursor:pointer;font-size:11px;padding:1px 7px;font-family:inherit;}" +
      ".owo-ws-d-head{display:flex;gap:10px;align-items:center;flex-wrap:wrap;}" +
      ".owo-ws-d-id{font-family:Consolas,ui-monospace,monospace;font-size:14px;font-weight:700;}" +
      ".owo-ws-tpl-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(340px,1fr));gap:10px;}" +
      ".owo-ws-tpl-card{border:1px solid var(--border,#e2e6ee);border-radius:11px;padding:11px;display:flex;flex-direction:column;gap:6px;font-size:12px;background:var(--surface,#fff);}" +
      ".owo-ws-form{border:1px dashed var(--border,#e2e6ee);border-radius:8px;padding:8px;display:flex;flex-direction:column;gap:6px;}" +
      // 终态门控横幅：琥珀语义色的柔和应用
      ".owo-ws-gate{display:none;border:1px solid var(--yellow-line,rgba(160,106,4,.34));background:var(--yellow-soft,rgba(199,138,26,.13));color:var(--yellow,#a06a04);border-radius:9px;padding:7px 11px;font-size:12px;font-weight:600;}" +
      // 实时通道指示：SSE=绿 / 轮询降级=琥珀 / 停止=灰
      ".owo-ws-live{font-size:12px;color:var(--faint,#8a93a5);}" +
      ".owo-ws-live.on{color:var(--green,#14855a);}" +
      ".owo-ws-live.poll{color:var(--yellow,#a06a04);}" +
      ".owo-ws-live.off{color:var(--faint,#8a93a5);}" +
      // 任务统计摘要行（阻塞标红，运行中标强调色）
      ".owo-ws-dagsum{font-size:12px;color:var(--muted,#5b6577);margin:0 0 6px;font-variant-numeric:tabular-nums;}" +
      ".owo-ws-dagsum b.blocked{color:var(--red,#cd3f35);}" +
      ".owo-ws-dagsum b.run{color:var(--accent,#2563eb);}" +
      // 加载中状态轻微弱化，便于与空态区分（焦点/禁用反馈由全局原语统一提供）
      ".owo-ws-loading{opacity:.75;}" +
      "";

    function nav() {
      return (
        '<section data-panel="workswarm" class="owo-ws-panel">' +
        "<style>" + CSS + "</style>" +
        '<div class="owo-ws-tabs">' +
        '<button class="owo-ws-tab" data-view="create">创建团队</button>' +
        '<button class="owo-ws-tab" data-view="list">团队列表</button>' +
        '<button class="owo-ws-tab" data-view="templates">模板与提案</button>' +
        "</div>" +
        '<div class="owo-ws-main" id="ws-main"></div>' +
        "</section>"
      );
    }

    function mount(root, helpers) {
      rootEl = root;
      H = helpers || {};
      H.baseUrl = H.baseUrl || (win.OwoPanels && win.OwoPanels.baseUrl) || "";
      H.get = H.get || defaultGet;
      H.post = H.post || defaultPost;
      H.esc = H.esc || defaultEsc;
      stopLive();
      detailSeq++; // 上一次详情流水的响应全部作废
      state.view = "list";
      state.current = null;
      root.innerHTML = nav();
      // 重试按钮的事件委托绑在本次渲染出的 section 上：
      // innerHTML 重建时旧监听器随之销毁，重复挂载不会累积监听器。
      var section = root.querySelector("section.owo-ws-panel");
      if (section) bindDelegates(section);
      bindTabs();
      renderView();
    }

    function refresh() {
      if (!rootEl) return;
      renderView();
    }

    // —— 测试挂钩（tests/workswarm.panel.test.mjs 专用；浏览器运行时不读取） ——
    // 只暴露纯逻辑与字符串构建器；state 引用供测试预置输入（不触发 DOM）。
    var TEST_API = {
      normStatus: normStatus,
      isTerminalTeam: isTerminalTeam,
      taskBlocked: taskBlocked,
      isRetryableStep: isRetryableStep,
      retryableTasks: retryableTasks,
      shouldShowRetry: shouldShowRetry,
      buildRetryBody: buildRetryBody,
      computeRunSummary: computeRunSummary,
      explainError: explainError,
      dagSvg: dagSvg,
      renderDetail: renderDetail,
      renderList: renderList,
      handleRetryClick: handleRetryClick,
      submitRetry: submitRetry,
      syncDetail: syncDetail,
      // —— 四期挂钩：实时进度 + Artifact 评审 ——
      applyProgress: applyProgress,
      handleEventFrame: handleEventFrame,
      computeProgressView: computeProgressView,
      renderProgress: renderProgress,
      fmtElapsed: fmtElapsed,
      paintProgress: paintProgress,
      stopProgressTimer: stopProgressTimer,
      normReviewState: normReviewState,
      reviewBadgeHtml: reviewBadgeHtml,
      isReviewable: isReviewable,
      groupArtifactChain: groupArtifactChain,
      buildReviewBody: buildReviewBody,
      explainReviewError: explainReviewError,
      submitArtifactReview: submitArtifactReview,
      artifactHistoryHtml: artifactHistoryHtml,
      artifactRowHtml: artifactRowHtml,
      renderArtifactsChains: renderArtifactsChains,
      loadArtifactHistory: loadArtifactHistory,
      findArtifactById: findArtifactById,
      css: function () {
        return CSS;
      },
      state: state,
      // 测试注入传输层（H 在 mount 时可能被整体替换，故经访问器读写）
      getTransport: function () {
        return { get: H.get, post: H.post };
      },
      setTransport: function (t) {
        if (t && t.get) H.get = t.get;
        if (t && t.post) H.post = t.post;
      },
    };

    return {
      id: ID,
      title: "WorkSwarm（团队编排）",
      nav: nav,
      mount: mount,
      refresh: refresh,
      _test: TEST_API,
    };
  })();
})();

// Node（CommonJS）测试环境导出；浏览器无 module 定义，此分支不生效、零运行时差异。
if (typeof module !== "undefined" && module.exports) {
  var __wswWin = typeof window !== "undefined" ? window : globalThis;
  module.exports = __wswWin.OwoPanels.workswarm;
}