(function (root) {
  "use strict";

  function shouldOfferModelSwitch(message) {
    const text = String(message || "");
    return /(?:\u6a21\u578b\u4e0d\u5b58\u5728|\u627e\u4e0d\u5230\u6a21\u578b|\u6a21\u578b\u4e0d\u53ef\u7528)/i.test(text) ||
      /"code"\s*:\s*"?1211"?/i.test(text) ||
      /\bmodel(?:[_\s-]+name)?(?:[_\s-]+not[_\s-]+found|[_\s-]+does[_\s-]+not[_\s-]+exist|[_\s-]+is[_\s-]+unavailable)\b/i.test(text);
  }

  function shouldOfferOutputBudget(message) {
    const text = String(message || "");
    return /finish_reason\s*=\s*length/i.test(text) || /(?:输出|达到)\s*max_tokens\s*上限/i.test(text);
  }

  function firstErrorObject(text) {
    const source = String(text || "");
    for (let start = source.indexOf("{"); start >= 0; start = source.indexOf("{", start + 1)) {
      let depth = 0;
      let inString = false;
      let escaped = false;
      for (let index = start; index < source.length; index += 1) {
        const char = source[index];
        if (inString) {
          if (escaped) escaped = false;
          else if (char === "\\") escaped = true;
          else if (char === '"') inString = false;
          continue;
        }
        if (char === '"') inString = true;
        else if (char === "{") depth += 1;
        else if (char === "}") {
          depth -= 1;
          if (depth === 0) {
            try { return JSON.parse(source.slice(start, index + 1)); } catch (_) { break; }
          }
        }
      }
    }
    return null;
  }

  function summarizeTurnFailure(message) {
    const detail = String(message || "");
    const payload = firstErrorObject(detail);
    const providerMessage = payload && payload.error && typeof payload.error.message === "string"
      ? payload.error.message : "";
    const providerCode = payload && payload.error && payload.error.code != null
      ? String(payload.error.code) : "";
    const statusMatch = detail.match(/(?:HTTP\s*)?([45]\d\d)\b/i);
    const status = statusMatch ? Number(statusMatch[1]) : 0;
    let category = "raw";
    let summary = detail;

    if (shouldOfferModelSwitch(detail)) {
      category = "model";
      summary = "当前模型在所选服务商中不可用或名称不匹配。请检查“设置 → 配置”的模型代码与服务商，或切换到已配置的模型后重试。";
    } else if (shouldOfferOutputBudget(detail)) {
      category = "output_budget";
      summary = "回答达到当前模型的输出上限，已生成内容已保留但本回合未完成。可提高该模型的输出上限，或拆分任务后继续。";
    } else if (status === 401 || status === 403) {
      category = "authentication";
      summary = "模型服务拒绝了认证。请检查当前服务商的 API 密钥、端点和账户权限后重试。";
    } else if (status === 429) {
      category = "rate_limit";
      summary = "模型服务暂时限流或额度不足。请检查服务商配额，稍后重试或切换模型。";
    } else if (status === 400) {
      category = "bad_request";
      summary = "模型服务拒绝了请求（HTTP 400）。请检查模型名称、端点及该模型支持的参数；展开技术详情查看服务商原因。";
    } else if (status >= 500) {
      category = "provider_unavailable";
      summary = "模型服务暂时不可用（HTTP " + status + "）。请稍后重试，或检查服务商状态。";
    } else if (/timeout|timed out|ECONN|ENOTFOUND|Failed to fetch|NetworkError|无法连接/i.test(detail)) {
      category = "network";
      summary = "连接模型服务失败。请检查网络、代理和 API 端点后重试。";
    } else if (providerMessage || providerCode) {
      category = "provider_error";
      summary = providerMessage || ("模型服务返回错误代码 " + providerCode + "。展开技术详情查看完整信息。");
    }
    return { category, message: summary, detail, providerMessage, providerCode };
  }

  const api = Object.freeze({ shouldOfferModelSwitch, shouldOfferOutputBudget, firstErrorObject, summarizeTurnFailure });
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.OwoModelRecovery = api;
})(typeof window !== "undefined" ? window : globalThis);
