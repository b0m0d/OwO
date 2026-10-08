(function (root) {
  "use strict";

  function createWorkspaceSelectionController(persist, apply) {
    if (typeof persist !== "function") throw new TypeError("persist must be a function");
    if (typeof apply !== "function") throw new TypeError("apply must be a function");
    var revision = 0;
    var tail = Promise.resolve();

    function begin() {
      revision += 1;
      return revision;
    }

    function isCurrent(token) {
      return token === revision;
    }

    function select(target, token) {
      var path = String(target || "").trim();
      if (!path) return Promise.reject(new Error("workspace path is required"));
      var requestRevision = token == null ? begin() : token;
      var task = tail.catch(function () { return undefined; }).then(async function () {
        if (!isCurrent(requestRevision)) {
          return { revision: requestRevision, latest: false, result: null };
        }
        var result = await persist(path);
        var latest = isCurrent(requestRevision);
        if (latest) apply(path, result);
        return { revision: requestRevision, latest: latest, result: result };
      });
      tail = task.then(function () { return undefined; }, function () { return undefined; });
      return task;
    }

    return Object.freeze({ begin: begin, isCurrent: isCurrent, select: select });
  }

  function projectCreationFailureMessage(stage, error) {
    var detail = String((error && error.message) || error || "未知错误");
    if (stage === "activate") {
      return "项目文件夹已创建，但工作区切换失败。可从工作区菜单的最近目录重试。原因：" + detail;
    }
    if (stage === "session") {
      return "项目文件夹已创建并切换为当前工作区，但新建会话失败。目录仍可使用，请点击“新对话”重试。原因：" + detail;
    }
    return "新建项目文件夹失败：" + detail;
  }

  var api = {
    createWorkspaceSelectionController: createWorkspaceSelectionController,
    projectCreationFailureMessage: projectCreationFailureMessage,
  };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.OwoWorkspaceRouting = api;
})(typeof window !== "undefined" ? window : globalThis);
