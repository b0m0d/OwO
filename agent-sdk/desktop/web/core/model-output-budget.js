// Shared output-token bounds for the browser settings UI and Electron host.
(function (root, factory) {
  "use strict";
  const limits = factory();
  if (typeof module === "object" && module.exports) module.exports = limits;
  if (root) root.OwoModelOutputBudget = limits;
})(typeof globalThis !== "undefined" ? globalThis : this, function () {
  "use strict";
  return Object.freeze({ DEFAULT: 32000, MAX: 1000000 });
});
