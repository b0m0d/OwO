// 核心 HTTP 客户端 + SSE：所有请求都带 bearer token，端口来自主进程（动态端口）。
//
// 与旧壳的差别：旧壳把 token/端口分散在 api-client.js 的多处状态里，401 之后要
// "整体重查壳连接"才能恢复（复杂且出过 bug）。这里收敛成一处：连接信息只在
// `connection` 对象里，401 就换一次 token 再重试一次，仍失败就上报给上层显示。
let connection = { port: 0, token: "", ready: false };
let unauthorizedHandler = null;

export function setConnection(next) {
  connection = { ...connection, ...next };
}

export function getConnection() {
  return connection;
}

export function onUnauthorized(handler) {
  unauthorizedHandler = handler;
}

function url(path) {
  return `http://127.0.0.1:${connection.port}${path}`;
}

function headers(extra = {}) {
  const result = { ...extra };
  if (connection.token) result.Authorization = `Bearer ${connection.token}`;
  return result;
}

async function refreshToken() {
  const response = await fetch(url("/auth/token"));
  if (!response.ok) throw new Error(`token 引导失败（HTTP ${response.status}）`);
  const data = await response.json();
  connection.token = data.token || "";
  return connection.token;
}

export async function request(path, options = {}, retry = true) {
  if (!connection.port) throw new Error("本地核心服务尚未就绪");
  const response = await fetch(url(path), {
    ...options,
    headers: headers(options.headers || {}),
  });
  if (response.status === 401 && retry) {
    try {
      await refreshToken();
      if (unauthorizedHandler) unauthorizedHandler();
    } catch (_) {
      /* 交给下面统一报错 */
    }
    return request(path, options, false);
  }
  if (!response.ok) {
    const detail = await response.text().catch(() => "");
    throw new Error(`${response.status}: ${detail.slice(0, 200) || response.statusText}`);
  }
  const contentType = response.headers.get("content-type") || "";
  if (contentType.includes("application/json")) return response.json();
  return response.text();
}

export const get = (path) => request(path);

export const post = (path, body) =>
  request(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body || {}) });

export const del = (path) => request(path, { method: "DELETE" });

/// 流式回合：`POST /session/{id}/turn` 返回 SSE，逐事件回调。
/// 旧壳这里是自定义 stream 解析器；这里直接用 fetch + ReadableStream，逻辑只有一处。
export async function streamTurn(sessionId, payload, onEvent, signal) {
  const response = await fetch(url(`/session/${sessionId}/turn`), {
    method: "POST",
    headers: headers({ "Content-Type": "application/json", Accept: "text/event-stream" }),
    body: JSON.stringify(payload),
    signal,
  });
  if (!response.ok) {
    const detail = await response.text().catch(() => "");
    throw new Error(`${response.status}: ${detail.slice(0, 200)}`);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let index;
    while ((index = buffer.indexOf("\n\n")) >= 0) {
      const raw = buffer.slice(0, index);
      buffer = buffer.slice(index + 2);
      const event = parseSse(raw);
      if (event) onEvent(event);
    }
  }
  if (buffer.trim()) {
    const event = parseSse(buffer);
    if (event) onEvent(event);
  }
}

function parseSse(raw) {
  const lines = raw.split(/\r?\n/);
  let eventName = "message";
  const dataLines = [];
  for (const line of lines) {
    if (line.startsWith("event:")) eventName = line.slice(6).trim();
    else if (line.startsWith("data:")) dataLines.push(line.slice(5).trim());
  }
  if (!dataLines.length) return null;
  const data = dataLines.join("\n");
  try {
    return { event: eventName, data: JSON.parse(data) };
  } catch (_) {
    return { event: eventName, data };
  }
}

/// 健康检查（用于状态显示与重连判定）。
export async function health() {
  if (!connection.port) return null;
  try {
    return await request("/health", {}, false);
  } catch (_) {
    return null;
  }
}
