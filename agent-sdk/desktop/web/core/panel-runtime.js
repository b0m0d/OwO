// A feature-panel mount is an isolated failure boundary. One broken tool page must not
// abort navigation or leave the shared panel slot blank.
(function (root) {
  "use strict";

  function layoutFlags(children, wideFlags) {
    const isSectionHeading = (child) =>
      /^H[2-4]$/.test(child.tagName || "") ||
      Boolean(child.classList && child.classList.contains("sub"));
    return children.map((child, index) => Boolean(
      wideFlags[index] || (index > 0 && isSectionHeading(children[index - 1]))
    ));
  }

  function mount(panel, host, helpers, onError) {
    let result;
    try {
      result = panel.mount(host, helpers);
    } catch (error) {
      onError(error);
      return null;
    }
    if (result && typeof result.then === "function") {
      return Promise.resolve(result).catch((error) => {
        onError(error);
        return null;
      });
    }
    return result;
  }

  root.OwoPanelRuntime = Object.freeze({ mount, layoutFlags });
})(window);
