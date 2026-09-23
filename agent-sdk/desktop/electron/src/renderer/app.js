// OwO Agent 桌面工作台（Electron 渲染层）：组件化动态渲染。
//
// 设计要点（直接对应旧壳被投诉的每个 bug）：
//   1. **单一状态源**：所有界面状态在 `store` 里，视图由 store 推导；
//      路由只是一个字段 → 不存在"面板打开后关不掉"（关 = 把 route 改回去）。
//   2. **不做 DOM 搬动**：旧壳把 #settingsSection 在容器间搬来搬去，搬丢过、
//      被 replaceChildren 删过。这里没有任何跨容器搬节点，容器就是视图的一部分。
//   3. **配置全部文件驱动**：模型地址/名称/上下文/温度/输出上限… 读写 config.json，
//      界面只是文件的编辑器 + 「从文件重载」按钮；没有一处写死的模型清单。
//   4. 从磁盘加载：改这个文件后刷新（Ctrl+R）即可，不需要重新编译。
import { reactive, h, mount, renderMarkdown, escapeHtml } from "./lib/reactive.js";
import {
  setConnection,
  getConnection,
  get,
  post,
  del,
  streamTurn,
  health,
  request,
} from "./lib/api.js";

const store = reactive({
  coreState: { state: "starting" },
  connection: { port: 0, token: "" },
  route: "chat", // chat | model | permissions | diagnostics
  workspace: "",
  health: null,

  sessions: [],
  currentSessionId: null,
  messages: [],
  streaming: false,
  streamingText: "",
  abortController: null,
  pendingApproval: null,
  activity: "",
  input: "",
  notice: "",

  configPath: "",
  config: null,
  configDirty: false,
  configHint: "",
  modelModels: [],

  permissions: { profile: "", pending: [], grants: [] },
  audit: [],
  diagnostics: { requests: [], metrics: null },
});

// ---------- 与主进程协作 ----------

const api = window.owo || null;

function bindCoreState(state) {
  store.coreState = state || {};
  if (state && state.state === "ready" && state.port) {
    setConnection({ port: state.port, token: state.token || "", ready: true });
    store.connection = { port: state.port, token: state.token || "" };
    refreshAll();
  }
}

if (api) {
  api.onCoreState(bindCoreState);
  api.getCoreState().then(bindCoreState);
  api.getWorkspace().then((value) => {
    store.workspace = value || "";
  });
}

// ---------- 数据加载 ----------

async function refreshHealth() {
  store.health = await health();
}

async function refreshSessions() {
  try {
    const rows = await get("/sessions");
    store.sessions = Array.isArray(rows) ? rows : [];
    if (!store.currentSessionId && store.sessions.length) {
      await openSession(store.sessions[0].id);
    }
  } catch (error) {
    store.notice = `读取会话失败：${message(error)}`;
  }
}

async function refreshAll() {
  await refreshHealth();
  await refreshSessions();
  await loadConfig();
}

async function loadConfig() {
  if (!api) return;
  const result = await api.readConfig();
  store.configPath = result.path;
  store.config = result.config;
  store.configDirty = false;
  store.modelModels = (result.config.model && result.config.model.models) || [];
}

async function openSession(id) {
  store.currentSessionId = id;
  store.messages = [];
  try {
    const detail = await get(`/session/${id}`);
    store.messages = (detail.messages || []).filter((m) => m && m.content);
  } catch (error) {
    store.notice = `打开会话失败：${message(error)}`;
  }
}

async function createSession() {
  try {
    const created = await post("/session", {
      workspace: store.workspace,
      title: "新会话",
    });
    await refreshSessions();
    await openSession(created.id || created.session_id);
  } catch (error) {
    store.notice = `新建会话失败：${message(error)}`;
  }
}

async function sendMessage(explicitText) {
  const content = String(explicitText !== undefined ? explicitText : store.input).trim();
  if (!content || store.streaming) return;
  if (!store.currentSessionId) {
    await createSession();
    if (!store.currentSessionId) return;
  }
  store.input = "";
  store.messages = [...store.messages, { role: "user", content }];
  store.streaming = true;
  store.streamingText = "";
  store.abortController = new AbortController();
  const sessionId = store.currentSessionId;
  try {
    await streamTurn(
      sessionId,
      // 字段名以核心契约为准：POST /session/{id}/turn 的 body 是 { prompt, ... }。
      // （曾经写成 message → 核心 422 `missing field prompt`；契约在
      //  crates/owo-agent-server/src/turn_api.rs）
      { prompt: content, stream: true },
      (event) => handleTurnEvent(event),
      store.abortController.signal
    );
  } catch (error) {
    if (error && error.name !== "AbortError") {
      store.notice = `回合失败：${message(error)}`;
    }
  } finally {
    store.streaming = false;
    store.abortController = null;
    store.activity = "";
    if (store.streamingText) {
      store.messages = [...store.messages, { role: "assistant", content: store.streamingText }];
      store.streamingText = "";
    }
    refreshSessions();
  }
}

/// 处理回合事件：事件名与字段名**逐一对照核心契约**（crates/owo-agent-server/src/turn_api.rs
/// `to_event` 与 crates/owo-agent-protocol/src/lib.rs `SseEvent`），不再靠猜：
///   progress{message} / tool_use{id,tool,args} / tool_result{id,tool,ok,error}
///   permission_request{request_id,tool,args,reason,level,risk_note,explain}
///   token_delta{delta} / final{text} / compaction{summary}
function handleTurnEvent(event) {
  const data = event.data || {};
  switch (event.event) {
    case "token_delta":
      store.streamingText += data.delta || "";
      break;
    case "tool_use":
      store.activity = `调用工具：${data.tool || "未知"}`;
      break;
    case "tool_result":
      store.activity = `${data.ok ? "✔" : "✘"} ${data.tool || "工具"}${data.error ? "：" + data.error : ""}`;
      break;
    case "progress":
      store.activity = data.message || "";
      break;
    case "compaction":
      store.activity = `上下文已压缩：${(data.summary || "").slice(0, 80)}`;
      break;
    case "permission_request":
      // 关键：审批必须能在界面里完成，否则"直接对话"走不通（工具一被拦就卡住）。
      store.pendingApproval = {
        requestId: data.request_id,
        tool: data.tool,
        reason: data.reason,
        level: data.level || "",
        riskNote: data.risk_note || "",
        args: data.redacted_args || data.args || {},
        explain: data.explain || null,
      };
      break;
    case "final":
      store.streamingText = data.text || store.streamingText;
      break;
    default:
      break;
  }
}

/// 响应审批：`POST /session/{id}/permission/{request_id}` { allow, scope }。
async function respondApproval(allow, scope) {
  const approval = store.pendingApproval;
  if (!approval || !store.currentSessionId) return;
  try {
    await post(`/session/${store.currentSessionId}/permission/${approval.requestId}`, {
      allow,
      scope: scope || "once",
    });
    store.activity = allow ? `已允许（${scope || "once"}）：${approval.tool}` : `已拒绝：${approval.tool}`;
  } catch (error) {
    store.notice = `审批响应失败：${message(error)}`;
  } finally {
    store.pendingApproval = null;
  }
}

/// 从输入框读取内容并发送，然后清空输入框（清空是 DOM 操作，不触发整树重建）。
function submitFromDom(textarea) {
  const value = textarea && typeof textarea.value === "string" ? textarea.value : "";
  if (!value.trim()) return;
  if (textarea) textarea.value = "";
  sendMessage(value);
}

async function abortTurn() {
  if (store.abortController) store.abortController.abort();
  if (store.currentSessionId) {
    try {
      await post(`/session/${store.currentSessionId}/abort`, {});
    } catch (_) {
      /* 中止失败不影响界面 */
    }
  }
  store.streaming = false;
}

// ---------- 模型配置（全部写进 config.json） ----------

function fieldValue(path) {
  const model = (store.config && store.config.model) || {};
  return model[path] === null || model[path] === undefined ? "" : model[path];
}

function patchModel(patch) {
  if (!store.config) return;
  store.config = {
    ...store.config,
    model: { ...store.config.model, ...patch },
  };
  store.configDirty = true;
}

async function saveConfig() {
  if (!api) return;
  const result = await api.applyConfig(store.config);
  store.configHint = result && result.ok ? "已保存并重启核心（新配置已生效）" : "保存失败";
  await loadConfig();
  bindCoreState(await api.getCoreState());
}

async function reloadConfig() {
  if (!api) return;
  await loadConfig();
  store.configHint = "已从 config.json 重新读取；点「保存并重启核心」让它生效";
}

async function restartCore() {
  if (!api) return;
  store.configHint = "正在重启核心…";
  bindCoreState(await api.restartCore());
  store.configHint = "核心已重启";
}

async function chooseWorkspace() {
  if (!api) return;
  const result = await api.chooseWorkspace();
  if (result && result.ok) {
    store.workspace = result.workspace;
    bindCoreState(await api.getCoreState());
  }
}

// ---------- 权限 ----------

async function refreshPermissions() {
  try {
    const overview = await get("/permissions/overview");
    store.permissions = {
      profile: overview.profile || "",
      pending: overview.pending_approvals || overview.pending || [],
      grants: overview.grants || [],
    };
  } catch (error) {
    store.notice = `读取权限失败：${message(error)}`;
  }
}

async function refreshAudit() {
  try {
    const rows = await get("/audit?limit=50");
    store.audit = Array.isArray(rows) ? rows : rows.entries || [];
  } catch (_) {
    store.audit = [];
  }
}

async function refreshDiagnostics() {
  try {
    const ledger = await get("/diagnostics/requests");
    store.diagnostics = { requests: ledger.requests || [], metrics: ledger.metrics || null };
  } catch (error) {
    store.notice = `读取诊断失败：${message(error)}`;
  }
}

// ---------- 视图 ----------

function message(error) {
  return String((error && error.message) || error || "未知错误");
}

function statusLabel() {
  const state = store.coreState || {};
  if (state.state === "ready") return { text: "已连接", tone: "ok" };
  if (state.state === "starting") return { text: "启动中…", tone: "warn" };
  if (state.state === "failed" || state.state === "exited") {
    return { text: `未连接（${state.errorCode || "失败"}）`, tone: "bad" };
  }
  return { text: "未连接", tone: "warn" };
}

function topBar() {
  const status = statusLabel();
  const model = (store.config && store.config.model) || {};
  return h(
    "header",
    { class: "topbar" },
    h("div", { class: "brand" }, h("span", { class: "logo", html: "O" }), h("h1", {}, "OwO Agent"), h("span", { class: "tag" }, "本地优先")),
    h(
      "div",
      { class: "topbar-mid" },
      h("span", { class: `pill ${status.tone}` }, status.text),
      h("span", { class: "pill" }, `工作区 ${store.workspace ? short(store.workspace) : "未选择"}`),
      h("span", { class: "pill" }, `模型 ${model.name || "未配置"}`),
      model.context_window ? h("span", { class: "pill" }, `上下文 ${model.context_window}`) : null
    ),
    h(
      "div",
      { class: "topbar-actions" },
      h("button", { class: "btn", onClick: chooseWorkspace }, "选择工作区"),
      h("button", { class: "btn", onClick: restartCore }, "重连核心"),
      h("button", { class: "btn", onClick: () => (store.route = "model") }, "模型设置")
    )
  );
}

function short(path) {
  const parts = String(path).split(/[\\/]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : path;
}

function rail() {
  const items = [
    { key: "chat", icon: "☰", label: "任务" },
    { key: "model", icon: "✦", label: "模型" },
    { key: "permissions", icon: "⛨", label: "权限" },
    { key: "diagnostics", icon: "◎", label: "诊断" },
  ];
  return h(
    "nav",
    { class: "rail" },
    items.map((item) =>
      h(
        "button",
        {
          class: `rail-btn ${store.route === item.key ? "active" : ""}`,
          onClick: () => {
            store.route = item.key; // 关面板 = 改状态；不存在"关不掉"
            if (item.key === "permissions") refreshPermissions();
            if (item.key === "diagnostics") refreshDiagnostics();
          },
        },
        h("span", { class: "rail-icon" }, item.icon),
        h("span", { class: "rail-label" }, item.label)
      )
    )
  );
}

function sessionList() {
  return h(
    "section",
    { class: "panel" },
    h(
      "div",
      { class: "panel-head" },
      h("h2", {}, "会话"),
      h("button", { class: "btn primary small", onClick: createSession }, "＋ 新建")
    ),
    h(
      "ul",
      { class: "list", "data-scroll-key": "sessions" },
      store.sessions.length
        ? store.sessions.map((session) =>
            h(
              "li",
              {
                class: `session ${session.id === store.currentSessionId ? "active" : ""}`,
                onClick: () => openSession(session.id),
              },
              h("div", { class: "session-title" }, session.title || session.id.slice(0, 12)),
              h("div", { class: "session-meta" }, `${session.model || ""} ｜ ${(session.updated_at || "").slice(0, 16).replace("T", " ")}`)
            )
          )
        : h("li", { class: "muted" }, "暂无会话")
    )
  );
}

function chatView() {
  return h(
    "div",
    { class: "chat" },
    h(
      "div",
      { class: "messages", "data-scroll-key": "messages" },
      store.messages.length === 0 && !store.streaming
        ? h(
            "div",
            { class: "empty" },
            h("h3", {}, "开始一个新任务"),
            h("p", {}, "在下面输入任务目标，Agent 会在当前工作区内工作；写入与执行需要你批准。")
          )
        : store.messages.map((item) =>
            h("div", {
              class: `msg ${item.role === "user" ? "user" : "assistant"}`,
              html: renderMarkdown(item.content || ""),
            })
          ),
      store.streaming
        ? h(
            "div",
            { class: "msg assistant streaming" },
            h("div", { html: renderMarkdown(store.streamingText || "…") }),
            h("div", { class: "streaming-hint" }, "生成中…")
          )
        : null
    ),
    store.pendingApproval ? approvalCard() : null,
    store.activity ? h("div", { class: "activity" }, store.activity) : null,
    h(
      "form",
      {
        class: "composer",
        onSubmit: (event) => {
          event.preventDefault();
          submitFromDom(document.querySelector(".composer-input"));
        },
      },
      // ⚠ 非受控输入：值是 DOM 自己的，不在每敲一字时写 store。
      // 早先的受控写法（onInput → store.input）会触发整树重建，把输入框销毁重建、
      // 焦点丢失 —— 真实表现就是"输入框根本没法输入"。只有在发送时读一次 DOM。
      h("textarea", {
        class: "composer-input",
        rows: 3,
        placeholder: "输入任务，Enter 发送（Shift+Enter 换行）",
        onKeydown: (event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            submitFromDom(event.target);
          }
        },
      }),
      h(
        "div",
        { class: "composer-actions" },
        store.streaming
          ? h("button", { type: "button", class: "btn danger", onClick: abortTurn }, "⏹ 停止")
          : h("span", { class: "muted" }, "Enter 发送"),
        h("button", { type: "submit", class: "btn primary" }, "发送")
      )
    )
  );
}

/// 审批卡：核心发 permission_request 事件时出现；必须能在界面里点掉，
/// 否则工具调用会一直挂着（旧壳就是"点了没反应"的体验）。
function approvalCard() {
  const approval = store.pendingApproval;
  return h(
    "div",
    { class: "approval-card" },
    h(
      "div",
      { class: "approval-head" },
      h("strong", {}, `需要授权：${approval.tool}`),
      approval.level ? h("span", { class: "pill warn" }, approval.level) : null
    ),
    h("div", { class: "approval-reason" }, approval.reason || ""),
    approval.riskNote ? h("div", { class: "approval-risk" }, approval.riskNote) : null,
    h("pre", { class: "approval-args" }, JSON.stringify(approval.args || {}, null, 2)),
    h(
      "div",
      { class: "approval-actions" },
      h("button", { class: "btn primary small", onClick: () => respondApproval(true, "once") }, "允许一次"),
      h("button", { class: "btn small", onClick: () => respondApproval(true, "session") }, "此会话允许"),
      // 只读动作才给长期授权（破坏性操作不该有"永远允许"）。
      approval.level === "read"
        ? h("button", { class: "btn small", onClick: () => respondApproval(true, "always_readonly") }, "始终允许只读")
        : null,
      h("button", { class: "btn danger small", onClick: () => respondApproval(false, "deny") }, "拒绝")
    )
  );
}

function modelView() {
  const model = (store.config && store.config.model) || {};
  const presets = [
    { label: "智谱 BigModel", provider: "bigmodel", base_url: "https://open.bigmodel.cn/api/paas/v4", name: "glm-5.3-flash", context_window: 128000 },
    { label: "OpenAI", provider: "openai", base_url: "https://api.openai.com/v1", name: "gpt-4o-mini", context_window: 128000 },
    { label: "DeepSeek", provider: "deepseek", base_url: "https://api.deepseek.com/v1", name: "deepseek-chat", context_window: 64000 },
    { label: "阿里 DashScope", provider: "dashscope", base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1", name: "qwen-plus", context_window: 128000 },
    { label: "本地 Ollama", provider: "ollama", base_url: "http://127.0.0.1:11434/v1", name: "qwen2.5:3b", context_window: 32000 },
  ];

  const field = (label, key, props = {}) =>
    h(
      "label",
      { class: "field" },
      h("span", { class: "field-label" }, label),
      h("input", {
        class: "input",
        value: fieldValue(key),
        onInput: (event) => patchModel({ [key]: event.target.value === "" ? null : event.target.value }),
        ...props,
      })
    );

  return h(
    "div",
    { class: "page" },
    h(
      "section",
      { class: "panel" },
      h(
        "div",
        { class: "panel-head" },
        h("h2", {}, "模型"),
        h("span", { class: "muted" }, store.configDirty ? "有未保存修改" : "与 config.json 一致")
      ),
      h(
        "p",
        { class: "hint" },
        "全部配置写在 ",
        h("code", {}, store.configPath || "%LOCALAPPDATA%\\OwO\\Agent\\config.json"),
        " —— 地址、模型名、上下文窗口、温度、输出上限都可直接改文件；改完点「从文件重载」或「保存并重启核心」。"
      ),
      h(
        "div",
        { class: "presets" },
        presets.map((preset) =>
          h(
            "button",
            {
              class: "btn small",
              onClick: () => {
                patchModel({
                  provider: preset.provider,
                  base_url: preset.base_url,
                  name: preset.name,
                  context_window: preset.context_window,
                });
                store.configHint = `已套用「${preset.label}」预设（只是起点，可任意改）`;
              },
            },
            preset.label
          )
        ),
        h(
          "button",
          {
            class: "btn small",
            onClick: () => patchModel({ provider: "custom", base_url: (model.base_url || ""), name: model.name || "" }),
          },
          "自定义 / 自建"
        )
      ),
      h(
        "div",
        { class: "grid" },
        h(
          "label",
          { class: "field" },
          h("span", { class: "field-label" }, "服务提供方"),
          h(
            "select",
            {
              class: "input",
              onChange: (event) => patchModel({ provider: event.target.value }),
            },
            ["bigmodel", "openai", "deepseek", "dashscope", "ollama", "custom", "unset"].map((value) =>
              h("option", { value, selected: model.provider === value }, value)
            )
          )
        ),
        field("接口地址（base_url）", "base_url", { placeholder: "https://…/v1" }),
        field("模型名称", "name", { placeholder: "任意模型名，可自由输入", list: "owo-model-suggestions" }),
        h("datalist", { id: "owo-model-suggestions" }, (store.modelModels || []).map((name) => h("option", { value: name }))),
        field("API Key（留空则读环境变量）", "api_key", { type: "password", placeholder: "sk-…" }),
        field("密钥环境变量名", "api_key_env", { placeholder: "OPENAI_API_KEY" }),
        field("上下文窗口（token）", "context_window", { type: "number", placeholder: "留空 = 核心默认 60000" }),
        field("单次最大输出（token）", "max_output_tokens", { type: "number", placeholder: "留空 = 由模型决定" }),
        field("温度（0–2）", "temperature", { type: "number", step: "0.1", placeholder: "留空 = 核心默认" }),
        field("请求超时（秒）", "timeout_secs", { type: "number", placeholder: "留空 = 核心默认" }),
        field("保留最近消息条数", "keep_recent", { type: "number", placeholder: "留空 = 核心默认 20" })
      ),
      h(
        "div",
        { class: "actions" },
        h("button", { class: "btn primary", onClick: saveConfig }, "保存并重启核心"),
        h("button", { class: "btn", onClick: reloadConfig }, "从文件重载"),
        h("button", { class: "btn", onClick: () => api && api.revealConfig() }, "定位配置文件"),
        store.configHint ? h("span", { class: "hint" }, store.configHint) : null
      )
    )
  );
}

function permissionsView() {
  const pending = store.permissions.pending || [];
  return h(
    "div",
    { class: "page" },
    h(
      "section",
      { class: "panel" },
      h("div", { class: "panel-head" }, h("h2", {}, "权限中心"), h("span", { class: "muted" }, `档位：${store.permissions.profile || "未知"}`)),
      h("p", { class: "hint" }, "默认拒绝：写入、执行、注入等动作必须逐次批准。被拒绝的动作不会执行。"),
      pending.length
        ? pending.map((item) =>
            h(
              "div",
              { class: "approval" },
              h("div", { class: "approval-title" }, `${item.tool_id || item.tool || "工具"} ${item.reason ? "· " + item.reason : ""}`),
              h(
                "div",
                { class: "approval-actions" },
                h(
                  "button",
                  {
                    class: "btn primary small",
                    onClick: async () => {
                      await post(`/permission/${item.id || item.request_id}`, { allow: true, scope: "once" });
                      refreshPermissions();
                    },
                  },
                  "允许一次"
                ),
                h(
                  "button",
                  {
                    class: "btn danger small",
                    onClick: async () => {
                      await post(`/permission/${item.id || item.request_id}`, { allow: false, scope: "deny" });
                      refreshPermissions();
                    },
                  },
                  "拒绝"
                )
              )
            )
          )
        : h("p", { class: "muted" }, "当前没有待审批请求。")
    )
  );
}

function diagnosticsView() {
  const metrics = store.diagnostics.metrics || {};
  return h(
    "div",
    { class: "page" },
    h(
      "section",
      { class: "panel" },
      h("div", { class: "panel-head" }, h("h2", {}, "诊断")),
      h(
        "pre",
        { class: "code-block" },
        JSON.stringify(
          {
            核心状态: store.coreState,
            端口: getConnection().port,
            健康: store.health,
            请求指标: metrics,
            配置路径: store.configPath,
            工作区: store.workspace,
          },
          null,
          2
        )
      ),
      h(
        "div",
        { class: "actions" },
        h("button", { class: "btn", onClick: refreshDiagnostics }, "刷新"),
        h("button", { class: "btn", onClick: refreshAudit }, "刷新审计"),
      ),
      h(
        "ul",
        { class: "list" },
        (store.audit || []).slice(0, 20).map((entry) =>
          h("li", { class: "audit-row" }, `${entry.ts || ""} ${entry.target || ""} ${entry.action || entry.msg || ""}`)
        )
      )
    )
  );
}

function mainArea() {
  if (store.route === "model") return modelView();
  if (store.route === "permissions") return permissionsView();
  if (store.route === "diagnostics") return diagnosticsView();
  return chatView();
}

function view() {
  return h(
    "div",
    { class: "shell" },
    topBar(),
    h(
      "div",
      { class: "body" },
      rail(),
      h("aside", { class: "sidebar" }, sessionList()),
      h("main", { class: "content" }, mainArea())
    ),
    store.notice
      ? h(
          "div",
          { class: "toast", onClick: () => (store.notice = "") },
          store.notice,
          h("span", { class: "toast-close" }, " ✕")
        )
      : null
  );
}

// 首屏 + 定时健康检查（每 10s 一次，仅本地环回请求）。
mount(store, view);
setInterval(refreshHealth, 10000);

// Ctrl+R 刷新（改了 renderer 里的文件后立刻能看到效果，不用重新编译）。
window.addEventListener("keydown", (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "r") {
    event.preventDefault();
    location.reload();
  }
});

// 暴露给排障：控制台里 `owoStore` 可看当前状态。
window.owoStore = store;
window.owoApi = { get, post, del, request, refreshAll };
