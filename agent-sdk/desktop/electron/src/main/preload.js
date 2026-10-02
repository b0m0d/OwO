// 预加载：把主进程能力以最小面暴露给渲染层（contextIsolation 保持开启）。
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("owo", {
  getCoreState: () => ipcRenderer.invoke("core:get"),
  restartCore: () => ipcRenderer.invoke("core:restart"),
  readConfig: () => ipcRenderer.invoke("config:read"),
  writeConfig: (config) => ipcRenderer.invoke("config:write", config),
  applyConfig: (config) => ipcRenderer.invoke("config:apply", config),
  revealConfig: () => ipcRenderer.invoke("config:reveal"),
  getWorkspace: () => ipcRenderer.invoke("workspace:get"),
  chooseWorkspace: () => ipcRenderer.invoke("workspace:choose"),
  openExternal: (url) => ipcRenderer.invoke("app:openExternal", url),
  onCoreState: (handler) => {
    const listener = (_event, state) => handler(state);
    ipcRenderer.on("core:state", listener);
    return () => ipcRenderer.removeListener("core:state", listener);
  },
});

// ADR-003：Tauri 兼容桥。
//
// desktop/web 对壳的调用面是 `window.__TAURI_INTERNALS__.invoke(command, args)`
// （部分代码也认 `window.__TAURI__.core.invoke`），共 14 个命令。壳由 Tauri 换成
// Electron 后这个全局对象消失，会让「模型配置保存」「原生目录选择器」「诊断页
// 重启核心/打开日志」全部静默降级成"非桌面环境"，而界面看起来毫无异常。
//
// 这里把同一形状暴露出来、后端接到主进程的 shell:invoke，命令实现见
// src/main/shell-commands.js —— desktop/web 因此一行都不用改，UI 与功能都与旧壳一致。
const tauriInvoke = (command, args) => ipcRenderer.invoke("shell:invoke", command, args);
contextBridge.exposeInMainWorld("__TAURI_INTERNALS__", { invoke: tauriInvoke });
contextBridge.exposeInMainWorld("__TAURI__", { core: { invoke: tauriInvoke } });
