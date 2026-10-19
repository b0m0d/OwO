(function (root) {
  "use strict";

  function createObserverScope(Observer = root.ResizeObserver) {
    const observers = new Set();
    return {
      observe(target, callback) {
        if (typeof Observer !== "function") return null;
        const observer = new Observer(callback);
        try {
          observer.observe(target);
          observers.add(observer);
          return observer;
        } catch (error) {
          observer.disconnect();
          throw error;
        }
      },
      clear() {
        for (const observer of observers) observer.disconnect();
        observers.clear();
      },
      get size() {
        return observers.size;
      },
    };
  }

  const api = { createObserverScope };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.OwoObserverScope = api;
})(typeof window !== "undefined" ? window : globalThis);
