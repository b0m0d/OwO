(function (root) {
  "use strict";

  function createSessionStartGate() {
    var pending = null;
    function run(create) {
      if (typeof create !== "function") {
        return Promise.reject(new TypeError("create must be a function"));
      }
      if (pending) return pending;
      var task = Promise.resolve().then(create);
      pending = task.finally(function () {
        pending = null;
      });
      return pending;
    }
    return Object.freeze({ run: run });
  }

  var api = { createSessionStartGate: createSessionStartGate };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.OwoSessionStart = api;
})(typeof window !== "undefined" ? window : globalThis);
