/* 权限中心 · 网络层（§4.5 / §4.8）。
 *
 * 这个文件只回答一个问题：**"权限中心的每个动作，打到哪个端点、带什么载荷"**。
 * 所有路径集中在下面的常量区，是本模块唯一知道 URL 的地方；请求体在这里定型，
 * controller 与 view 都不再拼路径。
 *
 * 边界（§4.8）：零 DOM、零定时器、零直接网络调用——传输函数由宿主注入
 * （`fetchJson(path, opts)`，生产环境即 core/api-client.js 的封装），
 * 因此本模块可脱离浏览器做契约测试。
 */
(function (global) {
  "use strict";

  // ---- 端点常量（全部集中在此，改动只需看这一处）----
  const OVERVIEW_PATH = "/permissions/overview";
  const SPEC_PATH = "/permissions/spec";
  const REVOKE_PATH = "/permissions/grants/revoke";
  /** 审批响应沿用既有会话端点（与聊天页审批条同一响应通道，向后兼容）。 */
  const APPROVAL_PATH = (sessionId, requestId) =>
    "/session/" + encodeURIComponent(String(sessionId)) + "/permission/" + encodeURIComponent(String(requestId));

  const JSON_HEADERS = { "Content-Type": "application/json", Accept: "application/json" };

  function isPlainObject(value) {
    return Boolean(value) && typeof value === "object" && !Array.isArray(value);
  }

  function text(value) {
    return value == null ? "" : String(value);
  }

  /** 注入缺省值：宿主没给 fetchJson 时经统一 API 客户端取（仍不裸请求）。 */
  function resolveFetchJson(injected) {
    if (typeof injected === "function") return injected;
    const client = global.OwoApi;
    if (client && typeof client.request === "function") {
      return (path, options) => client.request(path, options || {});
    }
    return null;
  }

  function missingTransport() {
    const error = new Error("API 客户端未就绪");
    error.name = "TransportMissing";
    return error;
  }

  /**
   * 完全访问三要素里的两个（confirm + duration_secs）缺一就不发请求：
   * 服务端对二者缺失回 400，前端必须自己先拦住，避免"点了没反应"变成一次网络往返。
   */
  function fullAccessGuard(spec, options) {
    const unrestricted = isPlainObject(spec) && (text(spec.command) === "unrestricted" || text(spec.network) === "unrestricted");
    if (!unrestricted) return [];
    const problems = [];
    if (options && options.confirm === true) {
      /* confirm 已给出 */
    } else {
      problems.push("完全访问必须先二次确认（缺少确认标记）。");
    }
    const secs = options ? Number(options.durationSecs != null ? options.durationSecs : options.duration_secs) : NaN;
    if (!Number.isFinite(secs) || secs <= 0) {
      problems.push("完全访问必须选择有效时长（缺少时长参数）。");
    }
    return problems;
  }

  function create(transport) {
    const fetchJson = resolveFetchJson(transport && transport.fetchJson);

    function requireTransport() {
      if (!fetchJson) throw missingTransport();
      return fetchJson;
    }

    return {
      PATHS: { overview: OVERVIEW_PATH, spec: SPEC_PATH, revoke: REVOKE_PATH, approval: APPROVAL_PATH },

      /** 权限概览：档位 + 展开矩阵 + 待审批 + 已授权 + 最近决定 + 完全访问状态。 */
      overview() {
        return Promise.resolve().then(() => requireTransport()(OVERVIEW_PATH, { method: "GET" }));
      },

      /**
       * 提交结构化配置。完全访问（command/network 任一 unrestricted）必须带
       * `{confirm:true, durationSecs:N}`，否则**本地拒绝、不发请求**。
       */
      submitSpec(spec, options) {
        const opts = options || {};
        const blocked = fullAccessGuard(spec, opts);
        if (blocked.length) {
          return Promise.resolve({ ok: false, blocked: blocked, error: { code: "validation/local", message: blocked.join(" ") } });
        }
        const body = { spec: spec };
        const unrestricted = isPlainObject(spec) && (text(spec.command) === "unrestricted" || text(spec.network) === "unrestricted");
        if (unrestricted) {
          body.confirm = true;
          body.duration_secs = Number(opts.durationSecs != null ? opts.durationSecs : opts.duration_secs);
        }
        return Promise.resolve().then(() => requireTransport()(SPEC_PATH, { method: "POST", headers: JSON_HEADERS, body: JSON.stringify(body) }));
      },

      /** 一步关闭完全访问：提交一份收紧后的配置，不需要确认要素。 */
      disableFullAccess(spec) {
        return this.submitSpec(spec, {});
      },

      /** 撤销授权：载荷已由 domain.revokePayload 归一为三种粒度之一。 */
      revoke(payload) {
        if (!isPlainObject(payload) || !payload.grant_id && !payload.tool_id && payload.all !== true) {
          return Promise.resolve({ ok: false, error: { code: "validation/local", message: "撤销载荷缺少粒度标识，已阻止发送。" } });
        }
        return Promise.resolve().then(() => requireTransport()(REVOKE_PATH, { method: "POST", headers: JSON_HEADERS, body: JSON.stringify(payload) }));
      },

      /**
       * 审批响应（四动作：拒绝 / 仅本次 / 本任务 / 工作区长期）。
       * scope 字面量是 `once|task|workspace`，与会话审批条旧字面量互不影响。
       */
      respondApproval(sessionId, requestId, decision) {
        const value = isPlainObject(decision) ? decision : {};
        if (!text(sessionId) || !text(requestId)) {
          return Promise.resolve({ ok: false, error: { code: "validation/local", message: "审批缺少会话或请求标识，已阻止发送。" } });
        }
        const body = { allow: value.allow === true };
        if (body.allow) body.scope = text(value.scope);
        return Promise.resolve().then(() => requireTransport()(APPROVAL_PATH(sessionId, requestId), {
          method: "POST",
          headers: JSON_HEADERS,
          body: JSON.stringify(body),
        }));
      },
    };
  }

  global.OwoPermissionsApi = {
    OVERVIEW_PATH: OVERVIEW_PATH,
    SPEC_PATH: SPEC_PATH,
    REVOKE_PATH: REVOKE_PATH,
    APPROVAL_PATH: APPROVAL_PATH,
    fullAccessGuard: fullAccessGuard,
    create: create,
  };
})(typeof window !== "undefined" ? window : globalThis);

// Node（CommonJS）测试环境导出；浏览器无 module 定义，此分支不生效。
if (typeof module !== "undefined" && module.exports) {
  module.exports = (typeof window !== "undefined" ? window : globalThis).OwoPermissionsApi;
}
