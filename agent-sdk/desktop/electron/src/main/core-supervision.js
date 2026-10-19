// 核心监管的纯逻辑（ADR-003 S2：M3 实例校验 / M4 代际守卫与退避重启 / M5 复用 / M6 配置校验）。
//
// 移植来源：`desktop/tauri/src-tauri/src/core_supervisor.rs`（握手行解析、实例身份
// 与 API 版本双重校验、ledger 标签）与 `core_runtime.rs`（代际守卫、重启、复用）的
// 语义；`provider.rs` 的 validate/parse。原 Rust 实现带单测，这里同样带单测——
// **本文件刻意不 import electron**，因此 `desktop/electron/tests/` 可直接 node --test。
"use strict";

const MODEL_OUTPUT_BUDGET = require("../../../web/core/model-output-budget.js");
const DEFAULT_MODEL_OUTPUT_TOKENS = MODEL_OUTPUT_BUDGET.DEFAULT;
const MAX_MODEL_OUTPUT_TOKENS = MODEL_OUTPUT_BUDGET.MAX;
const CORE_API_VERSION = "0.7";
// 桌面实例头名（服务端 auth_token.rs:41 同名常量）：壳拉起核心时注入实例身份，
// 此后所有取 token 的请求都必须带同一个值，否则被 instance_gate_allows 判为
// "另一个桌面实例" → 403 auth/instance_mismatch → 渲染层全线 401。
const DESKTOP_INSTANCE_HEADER = "x-owo-desktop-instance";

// ---------- 握手行解析（§4.2） ----------

// 约定：恰好一行 JSON，含 `"event":"core_ready"` 与合法的 port/pid。容忍噪声行。
function parseReadyLine(line) {
  const text = String(line == null ? "" : line).trim();
  if (!text.includes('"event":"core_ready"')) return null;
  let value;
  try {
    value = JSON.parse(text);
  } catch (_) {
    return null;
  }
  if (!value || typeof value !== "object") return null;
  const port = Number(value.port);
  const pid = Number(value.pid);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) return null;
  if (!Number.isInteger(pid) || pid <= 0) return null;
  return {
    port,
    pid,
    api_version: typeof value.api_version === "string" ? value.api_version : "",
    build_id: typeof value.build_id === "string" ? value.build_id : "",
    instance_id: typeof value.instance_id === "string" ? value.instance_id : "",
  };
}

// 启动期致命错误行：稳定码形如 `layer/name`（§2.4），缺码宁可落回超时也不静默通过。
function parseFatalLine(line) {
  const text = String(line == null ? "" : line).trim();
  if (!text.includes('"event":"core_fatal"')) return null;
  let value;
  try {
    value = JSON.parse(text);
  } catch (_) {
    return null;
  }
  if (!value || typeof value !== "object" || value.event !== "core_fatal") return null;
  const code = typeof value.code === "string" ? value.code : "";
  if (!code.includes("/")) return null;
  return { code, message: typeof value.message === "string" ? value.message : "" };
}

// ---------- /health 校验（M3：实例身份 + API 版本双重校验） ----------

// 只认自己拉起的核心：端口可能被上一次未清理干净（或别人）的核心占用。
function evaluateHealth(health, expected) {
  const want = expected || {};
  const apiVersion = want.apiVersion || CORE_API_VERSION;
  if (!health || typeof health !== "object") return { ok: false, reason: "no_response" };
  if (health.healthy !== true) return { ok: false, reason: "unhealthy" };
  const stage = health.stage;
  if (typeof stage === "string" && stage !== "" && stage !== "ready") {
    return { ok: false, reason: "starting", stage };
  }
  if (health.api_version !== apiVersion) return { ok: false, reason: "version_mismatch" };
  if (want.instanceId && health.instance_id !== want.instanceId) {
    return { ok: false, reason: "instance_mismatch" };
  }
  return { ok: true };
}

function healthReasonText(reason, expected) {
  switch (reason) {
    case "no_response":
      return "核心服务未响应健康检查";
    case "unhealthy":
      return "核心服务未报告 healthy=true";
    case "starting":
      return "核心服务尚未就绪";
    case "version_mismatch":
      return `核心服务 API 版本不兼容（期望 ${(expected && expected.apiVersion) || CORE_API_VERSION}）`;
    case "instance_mismatch":
      return "核心实例身份不匹配（该端口属于另一个核心实例）";
    default:
      return "核心服务健康检查未通过";
  }
}

// ---------- 轮询与退避（M4） ----------

const HEALTH_POLL_MS = [100, 200, 400, 800];
const RESTART_BACKOFF_MS = [500, 1000, 2000, 4000];

function pollDelayMs(attempt) {
  const index = Math.max(0, Math.min(Number(attempt) || 0, HEALTH_POLL_MS.length - 1));
  return HEALTH_POLL_MS[index];
}

function nextBackoffMs(attempt) {
  const index = Math.max(0, Math.min(Number(attempt) || 0, RESTART_BACKOFF_MS.length - 1));
  return RESTART_BACKOFF_MS[index];
}

// 代际守卫是核心：监管循环是异步的，过期那一轮的回调不得操作新核心。
function shouldAutoRestart(state) {
  const s = state || {};
  if (Number(s.generation) !== Number(s.currentGeneration)) return false; // 已被新一轮取代
  if (s.shuttingDown === true) return false;
  if (s.userInitiated === true) return false; // 用户主动重启不算失败，不进自动重试
  return (Number(s.attempts) || 0) < (Number(s.maxAttempts) || 0);
}

// 核心可服务健康检查不等于桌面 API 已可用：Bearer token 引导也必须成功，
// 否则 UI 会显示“服务已连接”但所有会话请求都 401/403。
function authenticationState(status, token) {
  if (Number(status) === 200 && typeof token === "string" && token.trim().length > 0) {
    return { state: "ready" };
  }
  const httpStatus = Number(status);
  return {
    state: "failed",
    errorCode: "core/authentication_failed",
    message: httpStatus > 0
      ? `核心已启动，但桌面认证握手失败（HTTP ${httpStatus}）。请重启后台并查看诊断日志。`
      : "核心已启动，但没有取得桌面认证凭据。请重启后台并查看诊断日志。",
  };
}

// ---------- 复用已存活核心（M5） ----------

// `<data_root>/runtime/daemon.json` 的端口字段（字段名按服务端实现做防御式取值）。
function discoveryPort(descriptor) {
  if (!descriptor || typeof descriptor !== "object") return null;
  const raw =
    descriptor.port !== undefined ? descriptor.port : descriptor.http_port;
  const port = Number(raw);
  if (!Number.isInteger(port) || port <= 0 || port > 65535) return null;
  return port;
}

// ---------- 配置校验（M6，自 provider.rs 移植） ----------

const PROVIDER_ALIASES = {
  bigmodel: "bigmodel",
  zhipu: "bigmodel",
  glm: "bigmodel",
  cloud: "bigmodel",
  openai: "openai",
  deepseek: "deepseek",
  dashscope: "dashscope",
  qwen: "dashscope",
  aliyun: "dashscope",
  ollama: "ollama",
  custom: "custom",
  self: "custom",
  selfhosted: "custom",
  "openai-compatible": "custom",
  unset: "unset",
  none: "unset",
  "": "unset",
};

function normalizeProvider(value) {
  const key = String(value == null ? "" : value).trim().toLowerCase();
  if (Object.prototype.hasOwnProperty.call(PROVIDER_ALIASES, key)) return PROVIDER_ALIASES[key];
  return null;
}

function isLocalProvider(value) {
  return normalizeProvider(value) === "ollama";
}

const NUMERIC_RULES = {
  context_window: [1, 10000000],
  max_output_tokens: [1, MAX_MODEL_OUTPUT_TOKENS],
  timeout_secs: [1, 3600],
  keep_recent: [1, 100000],
};

// 只挡结构性错误（类型/取值域）；凭据缺失只告警不阻断——避免把用户锁在门外。
// `options.envHas(name)` 由调用方注入（主进程传 process.env 探测），纯函数不直接读环境；
// 不传则不告警（宁可漏报，也不在拿不到环境信息时误报）。
function validateConfig(config, options) {
  const errors = [];
  const warnings = [];
  if (!config || typeof config !== "object") {
    return { ok: false, errors: ["配置不是对象"], warnings };
  }
  if (config.version !== undefined && Number(config.version) !== 1) {
    errors.push("version 必须为 1");
  }
  const model = config.model;
  if (!model || typeof model !== "object") {
    errors.push("缺少 model 段");
    return { ok: false, errors, warnings };
  }
  const provider = typeof model.provider === "string" ? model.provider.trim() : "";
  if (!provider) {
    errors.push("provider 不能为空");
  } else if (!normalizeProvider(provider)) {
    errors.push(`未知 provider：${provider}`);
  }
  for (const key of Object.keys(NUMERIC_RULES)) {
    const raw = model[key];
    if (raw === null || raw === undefined || raw === "") continue;
    const value = Number(raw);
    const [min, max] = NUMERIC_RULES[key];
    if (!Number.isFinite(value) || !Number.isInteger(value) || value < min || value > max) {
      errors.push(`${key} 超出范围（${min}~${max}）`);
    }
  }
  if (model.temperature !== null && model.temperature !== undefined && model.temperature !== "") {
    const temperature = Number(model.temperature);
    if (!Number.isFinite(temperature) || temperature < 0 || temperature > 2) {
      errors.push("temperature 必须在 0~2 之间");
    }
  }
  if (
    model.compaction !== undefined &&
    model.compaction !== null &&
    typeof model.compaction !== "boolean"
  ) {
    errors.push("compaction 必须为布尔值");
  }
  const canonical = normalizeProvider(provider);
  const envHas = options && typeof options.envHas === "function" ? options.envHas : null;
  if (canonical && canonical !== "unset" && !isLocalProvider(provider) && envHas) {
    const fileKey = typeof model.api_key === "string" ? model.api_key.trim() : "";
    const envName = String(model.api_key_env || "OPENAI_API_KEY").trim() || "OPENAI_API_KEY";
    if (!fileKey && !envHas(envName)) {
      warnings.push(`未配置凭据：api_key 为空且环境变量 ${envName} 未设置`);
    }
  }
  return { ok: errors.length === 0, errors, warnings };
}

// ---------- 日志写入的容错封装 ----------

/**
 * 包一层"永不抛"的写入函数。
 *
 * 背景（真实崩溃）：壳把核心 stdout 转发到自身 stdout（`[core] …`），并落盘到
 * core.log。这两处写入都发生在核心进程的 `'data'` 回调里 —— 一旦抛出，异常无人
 * 接管，Electron 主进程直接挂掉，表现为「A JavaScript error occurred in the main
 * process」，窗口直接消失。已实测的触发条件：
 *   * stdout 对端消失 → `EPIPE: broken pipe, write`（管道被 `head` 之类截断、
 *     重定向目标提前退出、CI 里跑壳）；
 *   * 日志流句柄失效 / 磁盘满 / 只读文件系统 → `write` 同步抛错。
 *
 * 日志只是诊断手段，写不进去绝不能影响壳的可用性，所以两条写入路径都必须过这里。
 *
 * @param {(text: string) => any} write 底层写入（process.stdout.write / stream.write）
 * @returns {(text: string) => boolean} 同签名写入；返回是否真的写成功
 */
function safeLogWrite(write) {
  return function safeWrite(text) {
    if (typeof write !== "function") return false;
    try {
      write(text);
      return true;
    } catch (_) {
      return false; // 丢这一行，不影响调用方继续跑
    }
  };
}

// Writable streams report broken pipes asynchronously through an error event;
// safeLogWrite only catches synchronous throws, so stdout needs a listener too.
function safeStreamLogWrite(stream) {
  let available = Boolean(stream && typeof stream.write === "function");
  if (stream && typeof stream.on === "function") {
    stream.on("error", () => {
      available = false;
    });
  }
  return function safeStreamWrite(text) {
    if (!available) return false;
    try {
      stream.write(text);
      return true;
    } catch (_) {
      available = false;
      return false;
    }
  };
}

module.exports = {
  CORE_API_VERSION,
  DEFAULT_MODEL_OUTPUT_TOKENS,
  MAX_MODEL_OUTPUT_TOKENS,
  HEALTH_POLL_MS,
  RESTART_BACKOFF_MS,
  // 桌面实例头名：必须与服务端 auth_token.rs::DESKTOP_INSTANCE_HEADER 逐字一致，
  // 否则注入实例身份的核心会认为"不是同一个桌面实例"→ 403 instance_mismatch。
  DESKTOP_INSTANCE_HEADER,
  safeLogWrite,
  safeStreamLogWrite,
  parseReadyLine,
  parseFatalLine,
  evaluateHealth,
  authenticationState,
  healthReasonText,
  pollDelayMs,
  nextBackoffMs,
  shouldAutoRestart,
  discoveryPort,
  normalizeProvider,
  isLocalProvider,
  validateConfig,
};
