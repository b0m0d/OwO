(function (root) {
  "use strict";
  function effectiveDefaultModel(settings) {
    const value = settings && settings.runtime && settings.runtime.model
      ? settings.runtime.model
      : settings && settings.model;
    return String(value || "").trim();
  }
  function buildSessionModelRequest(model) {
    const value = String(model || "").trim();
    return { model: value || null };
  }
  function buildCreateSessionRequest(workspace, explicitModel) {
    const request = { workspace: String(workspace || "").trim() };
    const model = String(explicitModel || "").trim();
    if (model) request.model = model;
    return request;
  }
  function createSessionModelUpdateQueue(update) {
    if (typeof update !== "function") throw new TypeError("update must be a function");
    const tails = new Map();
    const revisions = new Map();
    function enqueue(sessionId, model) {
      const id = String(sessionId || "").trim();
      if (!id) return Promise.reject(new Error("sessionId is required"));
      const revision = (revisions.get(id) || 0) + 1;
      revisions.set(id, revision);
      const previous = tails.get(id) || Promise.resolve();
      const task = previous.catch(() => undefined).then(() =>
        update(id, buildSessionModelRequest(model))
      );
      tails.set(id, task);
      return task.then(
        () => ({ revision, latest: revisions.get(id) === revision, error: null }),
        (error) => ({ revision, latest: revisions.get(id) === revision, error })
      ).finally(() => {
        if (tails.get(id) === task) tails.delete(id);
      });
    }
    return Object.freeze({ enqueue });
  }
  function normalizeProviderId(provider) {
    const id = String(provider || "").trim().toLowerCase();
    if (id === "qwen" || id === "aliyun" || id === "alibaba") return "dashscope";
    if (id === "zhipu" || id === "glm") return "bigmodel";
    return id;
  }
  function findProviderPreset(provider, presets, baseUrl) {
    const list = Array.isArray(presets) ? presets : [];
    const normalized = normalizeProviderId(provider);
    const byId = normalized && list.find((preset) => normalizeProviderId(preset && preset.id) === normalized);
    if (byId) return byId;
    const strip = (url) => String(url || "").trim().toLowerCase().replace(/^https?:\/\//, "");
    const endpoint = strip(baseUrl);
    if (!endpoint) return null;
    return list.find((preset) => {
      const candidate = strip(preset && preset.baseUrl);
      return candidate && endpoint.includes(candidate);
    }) || null;
  }
  function providerModelOptionState(model, supportedModels, currentModel, custom) {
    const supported = Array.isArray(supportedModels) ? supportedModels.map(String) : [];
    if (custom || supported.length === 0) return { hidden: false, disabled: false };
    const compatible = supported.includes(String(model || ""));
    const isCurrent = String(model || "") === String(currentModel || "");
    return { hidden: !compatible && !isCurrent, disabled: !compatible && isCurrent };
  }
  function providerModelCompatibility(model, supportedModels, custom) {
    const value = String(model || "").trim();
    const supported = Array.isArray(supportedModels) ? supportedModels.map(String) : [];
    if (!value || custom || supported.length === 0) return { known: false, compatible: true };
    return { known: true, compatible: supported.includes(value) };
  }
  const api = {
    effectiveDefaultModel,
    buildSessionModelRequest,
    buildCreateSessionRequest,
    createSessionModelUpdateQueue,
    providerModelOptionState,
    providerModelCompatibility,
    normalizeProviderId,
    findProviderPreset,
  };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.OwoModelRouting = api;
})(typeof window !== "undefined" ? window : globalThis);
