// §4.4 表单控件规范：**文件夹必须走 Tauri 原生目录选择器，禁止要求手输完整路径**。
//
// 为什么单独立一个模块：桌面版里"路径"不是文本，是壳侧的一次受控操作
// （`choose_project_directory` 会校验目录、持久化到数据目录并受控重启核心）。
// 把输入框留在页面上让用户敲盘符，既违反 §4.4，也绕过了壳侧校验——
// 真机上见过用户把项目选到 `C:\Users\<user>` 这种整盘根目录。
//
// 降级口径（诚实优先）：纯浏览器（dev 模式，无 `__TAURI_INTERNALS__`）拿不到
// 原生选择器，此时**显式声明**"浏览器模式无原生选择器，仅开发用"，
// 并把按钮禁用而不是伪装成功；不得让桌面验收看到一条无声的降级。
(function (global) {
  "use strict";

  const PICK_COMMAND = "choose_project_directory";

  /** Tauri IPC owner（与 api-client / status-bar 同一套探测，不复制第二份逻辑）。 */
  function invokeOwner(scope) {
    const root = scope || global;
    if (!root) return null;
    const internals = root.__TAURI_INTERNALS__;
    const publicCore = root.__TAURI__ && root.__TAURI__.core;
    const owner = publicCore && typeof publicCore.invoke === "function" ? publicCore : internals;
    return owner && typeof owner.invoke === "function" ? owner : null;
  }

  function isNativeAvailable(scope) {
    return Boolean(invokeOwner(scope));
  }

  /**
   * 调一次原生目录选择器。返回统一结论，**取消不是错误**：
   * `{ok:true, workspace}` / `{ok:false, canceled:true}` / `{ok:false, error}`。
   */
  function pick(scope) {
    const owner = invokeOwner(scope);
    if (!owner) {
      return Promise.resolve({ ok: false, unavailable: true, error: "当前不是桌面壳环境，原生目录选择器不可用" });
    }
    return Promise.resolve(owner.invoke(PICK_COMMAND, {})).then(
      (result) => normalize(result),
      (error) => ({ ok: false, error: String((error && error.message) || error) }),
    );
  }

  function normalize(result) {
    if (!result || typeof result !== "object") return { ok: false, error: "选择器无响应" };
    if (result.canceled) return { ok: false, canceled: true };
    if (result.ok && result.workspace) {
      return {
        ok: true,
        workspace: String(result.workspace),
        state: result.state ? String(result.state) : null,
        generation: Number.isFinite(Number(result.generation)) ? Number(result.generation) : null,
      };
    }
    return { ok: false, error: String(result.error || "设置工作区失败") };
  }

  function aliasFor(path) {
    const display = global.OwoWorkspaceDisplay || (global.window && global.window.OwoWorkspaceDisplay);
    if (display && typeof display.alias === "function") {
      try {
        return display.alias(path);
      } catch (_) {
        /* 别名失败退回完整显示，由 title 兜底 */
      }
    }
    return path;
  }

  /**
   * 把一个"路径输入框 + 浏览按钮"改造成原生选择器入口。
   *
   * options:
   * - `onPicked(workspace, result)` 选定并成功落壳侧后回调（调用方负责写 state/localStorage）
   * - `onCanceled()` 取消（合法终态：界面回到原值，不得报错）
   * - `onError(message)` 失败
   * - `title` 原生选择器标题（仅用于按钮 title 说明）
   *
   * 返回 `{ native }` 供调用方决定是否隐藏"手输路径"这条降级文案。
   */
  function attach(input, button, options) {
    const opts = options || {};
    const native = isNativeAvailable(global);
    if (!input || !button) return { native: native };
    if (native) {
      // 只读 = 路径不再是自由文本（§4.4 禁止做法：要求手输完整路径）。
      input.readOnly = true;
      input.setAttribute("aria-readonly", "true");
      input.dataset.owoFolderPicker = "native";
      input.placeholder = "由原生目录选择器设定";
      input.title = opts.title || "桌面版通过原生目录选择器设置项目工作区，无需手输完整路径";
      button.type = "button";
      button.textContent = "选择目录…";
      button.disabled = false;
      button.dataset.owoFolderPicker = "native";
      button.addEventListener("click", function () {
        if (button.disabled) return;
        button.disabled = true;
        pick(global).then(
          (result) => {
            if (result.ok) {
              input.value = aliasFor(result.workspace);
              input.title = "当前项目：" + result.workspace;
              input.dataset.path = result.workspace;
              if (typeof opts.onPicked === "function") opts.onPicked(result.workspace, result);
            } else if (result.canceled) {
              if (typeof opts.onCanceled === "function") opts.onCanceled();
            } else if (typeof opts.onError === "function") {
              opts.onError(result.error || "选择失败", result);
            }
          },
          (error) => {
            if (typeof opts.onError === "function") opts.onError(String((error && error.message) || error));
          },
        ).finally(function () {
          button.disabled = false;
        });
      });
    } else {
      // 浏览器 dev 模式：保留手输（开发便利），但必须显式说明这不是桌面路径。
      button.type = "button";
      button.disabled = true;
      button.textContent = "选择目录…";
      button.dataset.owoFolderPicker = "browser";
      button.title = "浏览器开发模式没有原生目录选择器；桌面版由壳侧原生选择器设定（§4.4）";
    }
    return { native: native };
  }

  global.OwoFolderPicker = {
    PICK_COMMAND: PICK_COMMAND,
    invokeOwner: invokeOwner,
    isNativeAvailable: isNativeAvailable,
    pick: pick,
    normalize: normalize,
    attach: attach,
    aliasFor: aliasFor,
  };
})(typeof window !== "undefined" ? window : globalThis);
