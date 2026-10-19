/* Bounded readiness probe used before hydrating the desktop surfaces. */
(function (global) {
  "use strict";
  class ServiceReadiness {
    constructor(client) { this.client = client; }
    async probe() {
      const health = await this.client.get("/health", { public: true });
      if (!health || health.healthy !== true) throw new Error("核心服务未报告 healthy");
      return health;
    }
    async wait(timeoutMs) {
      const deadline = Date.now() + (timeoutMs || 10000);
      let delay = 100;
      let lastError = null;
      while (Date.now() < deadline) {
        try { return await this.probe(); }
        catch (error) { lastError = error; }
        await new Promise((resolve) => setTimeout(resolve, Math.min(delay, Math.max(0, deadline - Date.now()))));
        delay = Math.min(delay * 2, 800);
      }
      const error = new Error("核心服务在限定时间内未就绪");
      error.cause = lastError;
      throw error;
    }
  }
  global.OwoServiceReadiness = ServiceReadiness;
})(typeof window !== "undefined" ? window : globalThis);
