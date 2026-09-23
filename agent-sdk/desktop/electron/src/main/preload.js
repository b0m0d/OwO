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
