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
  const api = Object.freeze({ shouldOfferModelSwitch, shouldOfferOutputBudget });
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.OwoModelRecovery = api;
})(typeof window !== "undefined" ? window : globalThis);
