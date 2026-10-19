/* Minimal history-backed route state for the desktop shell. */
(function (global) {
  "use strict";
  class OwoRouter {
    constructor(routes, onChange) {
      this.routes = routes || {};
      this.onChange = onChange || function () {};
      this.current = null;
      global.addEventListener("popstate", () => {
        this.go(this.read(), true);
      });
    }
    read() {
      const value = new URLSearchParams(global.location.search).get("page");
      return this.routes[value] ? value : "chat";
    }
    start() { return this.go(this.read(), true); }
    go(route, replace) {
      const next = this.routes[route] ? route : "chat";
      this.current = next;
      const url = next === "chat" ? global.location.pathname : global.location.pathname + "?page=" + encodeURIComponent(next);
      global.history[replace ? "replaceState" : "pushState"]({ page: next }, "", url);
      this.onChange(next);
      return next;
    }
  }
  global.OwoRouter = OwoRouter;
})(typeof window !== "undefined" ? window : globalThis);
