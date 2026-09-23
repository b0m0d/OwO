// Electron 主进程：OwO Agent 桌面壳（替代 Tauri 壳）。
//
// 为什么换：用户明确要求"别再用 Rust 壳、前端改成组件化动态渲染"。Electron 的
// 关键好处是**渲染层（Vue）从磁盘加载**——改 UI 只需刷新，不必重新编译，
// 这正好治住"改一次编译半天"这个痛点。
//
// 职责（与旧 Tauri 壳对齐，语义不变）：
//   1. 拉起核心服务 `owo-agent.exe serve --port 0`，从 stdout 的 core_ready 行取端口；
//   2. 读取 `%LOCALAPPDATA%\OwO\Agent\config.json`，把模型配置注入核心环境变量
//      （地址/模型名/上下文窗口/温度/超时…全部文件驱动，见 docs/model-config.md）；
//   3. 取一次 bearer token 交给渲染层（渲染层不再自己引导凭据）；
//   4. 提供 IPC：读/写配置、重载配置、重启核心；
//   5. 退出时优雅关闭核心（POST /server/shutdown，兜底 taskkill）。
//
// 不弹控制台：Windows 上以 `windowsHide: true` + `CREATE_NO_WINDOW` 语义启动核心。
const { app, BrowserWindow, ipcMain, shell, dialog } = require("electron");
const { spawn, execFile } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");
const http = require("node:http");

const CORE_API_VERSION = "0.7";

let mainWindow = null;
let core = null; // { proc, pid, port, token }
let coreState = { state: "starting" };

// ---------- 路径解析 ----------

function localAppData() {
  return process.env.LOCALAPPDATA || process.env.TEMP || os.tmpdir();
}

function dataRoot() {
  return path.join(localAppData(), "OwO", "Agent");
}

function configPath() {
  return process.env.OWO_CONFIG_FILE || path.join(dataRoot(), "config.json");
}

function workspacePath() {
  // 与旧壳同源：数据目录下 workspace.json 的 path 字段。
  try {
    const text = fs.readFileSync(path.join(dataRoot(), "workspace.json"), "utf8");
    const parsed = JSON.parse(text);
    if (parsed && typeof parsed.path === "string" && parsed.path) {
      return parsed.path;
    }
  } catch (_) {
    /* 没配置过就走下面的默认 */
  }
  return process.cwd();
}

function saveWorkspace(target) {
  const file = path.join(dataRoot(), "workspace.json");
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify({ path: target }));
  return target;
}

// 核心可执行文件：优先用同级目录（打包后），否则用仓库产物（开发时）。
function coreCandidates() {
  const here = path.dirname(app.getAppPath());
  const list = [
    // 显式指定（start.ps1 注入 / 部署脚本注入）优先。
    ...(process.env.OWO_CORE_EXE ? [process.env.OWO_CORE_EXE] : []),
    path.join(here, "owo-agent.exe"),
    path.join(process.cwd(), "owo-agent.exe"),
    path.join(__dirname, "..", "..", "..", "..", "target", "release", "owo-agent.exe"),
    path.join(__dirname, "..", "..", "..", "..", "dist", "OwO-Agent", "owo-agent.exe"),
  ];
  return list.filter((candidate) => fs.existsSync(candidate));
}

// ---------- 配置读写（唯一事实源：config.json） ----------

const DEFAULT_CONFIG = {
  version: 1,
  model: {
    provider: "unset",
    base_url: "",
    name: "",
    api_key: "",
    api_key_env: "OPENAI_API_KEY",
    context_window: null,
    max_output_tokens: null,
    temperature: null,
    timeout_secs: null,
    keep_recent: null,
    compaction: null,
    models: [],
  },
};

function readConfig() {
  try {
    const text = fs.readFileSync(configPath(), "utf8");
    const parsed = JSON.parse(text);
    return { ...DEFAULT_CONFIG, ...parsed, model: { ...DEFAULT_CONFIG.model, ...(parsed.model || {}) } };
  } catch (_) {
    return JSON.parse(JSON.stringify(DEFAULT_CONFIG));
  }
}

function writeConfig(config) {
  const file = configPath();
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const text = JSON.stringify({ version: 1, ...config }, null, 2) + "\n";
  const temp = `${file}.tmp`;
  fs.writeFileSync(temp, text);
  fs.renameSync(temp, file);
  // 可能含明文密钥：收紧为仅当前用户可读写（icacls，失败不影响功能）。
  try {
    execFile("icacls", [file, "/inheritance:r", "/grant:r", `${process.env.USERNAME}:F`], {
      windowsHide: true,
    }, () => {});
  } catch (_) {
    /* 忽略：权限收紧是加分项，不是前置条件 */
  }
}

// ---------- 核心：环境变量注入 ----------

function coreEnv(config, extra = {}) {
  const model = config.model || {};
  const env = {
    ...process.env,
    OWO_AGENT_DATA: path.join(dataRoot(), "data"),
    OWO_DESKTOP_RELEASE: "1",
    ...extra,
  };
  const provider = String(model.provider || "unset");
  if (provider !== "unset") {
    if (model.base_url) env.OPENAI_BASE_URL = model.base_url;
    if (model.name) env.OPENAI_MODEL = model.name;
    // 上下文与采样参数：**未配置就不注入**，核心保留自己的默认值。
    const numeric = {
      OWO_MODEL_CONTEXT_WINDOW: model.context_window,
      OWO_MODEL_MAX_OUTPUT_TOKENS: model.max_output_tokens,
      OWO_MODEL_TEMPERATURE: model.temperature,
      OWO_MODEL_TIMEOUT_SECS: model.timeout_secs,
      OWO_AGENT_KEEP_RECENT: model.keep_recent,
    };
    for (const [key, value] of Object.entries(numeric)) {
      if (value !== null && value !== undefined && String(value).trim() !== "" && Number(value) > 0) {
        env[key] = String(value);
      }
    }
    if (model.compaction === true) env.OWO_AGENT_COMPACTION = "1";
    if (model.compaction === false) env.OWO_AGENT_COMPACTION = "0";
  }
  // 凭据：文件里的 key 优先，其次 api_key_env 指向的环境变量，再退 OPENAI_API_KEY。
  const fileKey = typeof model.api_key === "string" ? model.api_key.trim() : "";
  if (fileKey) {
    env.OPENAI_API_KEY = fileKey;
  } else {
    const envName = String(model.api_key_env || "OPENAI_API_KEY").trim() || "OPENAI_API_KEY";
    const fromEnv = process.env[envName] || process.env.OPENAI_API_KEY || "";
    if (fromEnv) env.OPENAI_API_KEY = fromEnv;
  }
  return env;
}

// ---------- 核心：启动 / 就绪 / 关闭 ----------

function httpGet(port, urlPath, headers = {}) {
  return new Promise((resolve, reject) => {
    const request = http.request(
      { host: "127.0.0.1", port, path: urlPath, method: "GET", headers, timeout: 5000 },
      (response) => {
        let body = "";
        response.on("data", (chunk) => (body += chunk));
        response.on("end", () => resolve({ status: response.statusCode, body }));
      }
    );
    request.on("timeout", () => request.destroy(new Error("timeout")));
    request.on("error", reject);
    request.end();
  });
}

function httpPost(port, urlPath, headers = {}, payload = null) {
  return new Promise((resolve, reject) => {
    const data = payload ? JSON.stringify(payload) : "";
    const request = http.request(
      {
        host: "127.0.0.1",
        port,
        path: urlPath,
        method: "POST",
        timeout: 8000,
        headers: {
          ...headers,
          ...(data ? { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(data) } : {}),
        },
      },
      (response) => {
        let body = "";
        response.on("data", (chunk) => (body += chunk));
        response.on("end", () => resolve({ status: response.statusCode, body }));
      }
    );
    request.on("timeout", () => request.destroy(new Error("timeout")));
    request.on("error", reject);
    request.end(data);
  });
}

function killCore() {
  if (!core || !core.proc) return;
  const { proc, port, token } = core;
  core = null;
  // 先请它优雅退出；给 1.5s，再强杀兜底（与旧壳同语义）。
  const finish = () => {
    try {
      proc.kill();
    } catch (_) {
      /* 已退出 */
    }
    try {
      execFile("taskkill", ["/PID", String(proc.pid), "/T", "/F"], { windowsHide: true }, () => {});
    } catch (_) {
      /* 忽略 */
    }
  };
  if (port && token) {
    httpPost(port, "/server/shutdown", { Authorization: `Bearer ${token}` }, { confirm: true })
      .then(() => setTimeout(finish, 1500))
      .catch(() => finish());
  } else {
    finish();
  }
}

async function startCore() {
  killCore();
  const candidates = coreCandidates();
  if (!candidates.length) {
    coreState = { state: "failed", errorCode: "core/binary_missing", message: "找不到核心服务 owo-agent.exe" };
    notifyState();
    return coreState;
  }
  const exe = candidates[0];
  const config = readConfig();
  const workspace = workspacePath();
  coreState = { state: "starting", executable: exe, configPath: configPath(), workspace };
  notifyState();

  const proc = spawn(exe, ["serve", "--port", "0", "--workspace", workspace], {
    cwd: path.dirname(exe),
    env: coreEnv(config),
    windowsHide: true, // 不弹控制台窗口
    stdio: ["ignore", "pipe", "pipe"],
  });

  const handleLine = async (line) => {
    const text = String(line || "").trim();
    if (!text) return;
    if (core && core.log) core.log(text);
    try {
      const parsed = JSON.parse(text);
      if (parsed.event === "core_ready" && parsed.port) {
        const port = Number(parsed.port);
        let token = "";
        try {
          const auth = await httpGet(port, "/auth/token");
          token = JSON.parse(auth.body).token || "";
        } catch (_) {
          /* token 拿不到时渲染层会退回 /auth/token 引导 */
        }
        core = { proc, port, token, pid: proc.pid, version: parsed.api_version, buildId: parsed.build_id, log: core && core.log };
        coreState = {
          state: "ready",
          port,
          token,
          pid: proc.pid,
          apiVersion: parsed.api_version,
          buildId: parsed.build_id,
          executable: exe,
          configPath: configPath(),
          workspace,
        };
        notifyState();
      } else if (parsed.event === "core_fatal") {
        coreState = { state: "failed", errorCode: parsed.code, message: parsed.message };
        notifyState();
      }
    } catch (_) {
      /* 非 JSON 行：仅进日志 */
    }
  };

  let buffer = "";
  proc.stdout.on("data", (chunk) => {
    buffer += chunk.toString("utf8");
    let index;
    while ((index = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      handleLine(line);
    }
  });
  proc.stderr.on("data", (chunk) => handleLine(chunk.toString("utf8")));
  proc.on("exit", (code) => {
    if (coreState.state === "ready") {
      coreState = { state: "exited", errorCode: "core/exited", message: `核心已退出（code=${code}）` };
    } else if (coreState.state === "starting") {
      coreState = { state: "failed", errorCode: "core/exited", message: `核心启动失败（code=${code}）` };
    }
    notifyState();
  });
  core = { proc, log: (line) => process.stdout.write(`[core] ${line}\n`) };
  return coreState;
}

function notifyState() {
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send("core:state", coreState);
  }
}

// ---------- 窗口 ----------

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1360,
    height: 880,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: "#f6f7f9",
    title: "OwO Agent 工作台",
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  mainWindow.loadFile(path.join(__dirname, "..", "renderer", "index.html"));
  if (process.argv.includes("--dev")) {
    mainWindow.webContents.openDevTools({ mode: "detach" });
  }
}

// ---------- IPC ----------

ipcMain.handle("core:get", () => coreState);
ipcMain.handle("core:restart", async () => {
  await startCore();
  return coreState;
});
ipcMain.handle("config:read", () => ({ path: configPath(), config: readConfig() }));
ipcMain.handle("config:write", (_event, config) => {
  writeConfig(config);
  return { ok: true, path: configPath() };
});
// 保存配置并重启核心：改文件/界面保存走同一条路，避免两份真相。
ipcMain.handle("config:apply", async (_event, config) => {
  writeConfig(config);
  await startCore();
  return { ok: true, path: configPath(), state: coreState };
});
ipcMain.handle("config:reveal", () => {
  const file = configPath();
  if (!fs.existsSync(file)) {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    writeConfig(readConfig());
  }
  shell.showItemInFolder(file);
  return { ok: true, path: file };
});
ipcMain.handle("workspace:get", () => workspacePath());
ipcMain.handle("workspace:choose", async () => {
  const result = await dialog.showOpenDialog(mainWindow, {
    title: "选择项目工作区",
    properties: ["openDirectory"],
  });
  if (result.canceled || !result.filePaths.length) return { ok: false, canceled: true };
  const target = saveWorkspace(result.filePaths[0]);
  await startCore();
  return { ok: true, workspace: target };
});
ipcMain.handle("app:openExternal", (_event, url) => {
  if (/^https?:\/\//.test(String(url))) shell.openExternal(url);
  return { ok: true };
});

// ---------- 生命周期 ----------

app.whenReady().then(async () => {
  createWindow();
  await startCore();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  // 关窗即退出（并把核心一起收干净）；要常驻可以改成托盘模式。
  killCore();
  app.quit();
});

app.on("before-quit", () => killCore());
process.on("exit", () => killCore());
