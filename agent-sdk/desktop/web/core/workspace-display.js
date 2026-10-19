/* Keep real roots for execution, but use a safe human label in the shell. */
(function (global) {
  "use strict";
  function alias(root) {
    const value = String(root || "").replace(/[\\/]+$/, "");
    if (!value) return "未选择项目";
    const parts = value.split(/[\\/]/).filter(Boolean);
    return parts[parts.length - 1] || "本地项目";
  }
  function masked(root) {
    return String(root || "")
      .replace(/^([A-Za-z]):[\\/]+Users[\\/][^\\/]+/i, "$1:\\…\\用户")
      .replace(/([\\/]Users[\\/])[^\\/]+/i, "$1…\\用户");
  }
  global.OwoWorkspaceDisplay = { alias, masked };
})(typeof window !== "undefined" ? window : globalThis);
