// 极简响应式渲染内核（Vue 风格：状态 → 声明式视图，无构建步骤即可用）。
//
// 为什么不直接上 Vue 运行时：Electron 的渲染层要能从磁盘直接加载、不需要打包器
// （用户的核心诉求是"改 UI 别再来一轮编译"）。这里用 60 行实现同样的心智模型：
//   const store = reactive({ ... })   // 状态
//   h(tag, props, ...children)        // 声明式元素
//   mount(store, () => h(...))        // 状态变 → 整棵视图重渲染
//
// 关键约束（正是旧壳 bug 的根源，这里从结构上避免）：
//   * 唯一数据源在 store 里，视图永远由 store 推导 —— 不存在"DOM 改了但状态没改"；
//   * 路由是 store.route 的一个取值，切换路由 = 改状态，视图自然重建；
//   * 没有"把 DOM 节点在容器之间搬来搬去"的写法（旧壳的 #settingsSection 就被搬丢过）。
let activeStore = null;

export function reactive(initial) {
  const listeners = new Set();
  let drawing = false;
  const store = new Proxy(initial, {
    set(target, key, value) {
      if (target[key] === value) return true;
      target[key] = value;
      // 渲染期间再改状态不再递归触发（下一次事件会带上最新值）。
      if (!drawing) listeners.forEach((listener) => listener());
      return true;
    },
    deleteProperty(target, key) {
      delete target[key];
      if (!drawing) listeners.forEach((listener) => listener());
      return true;
    },
  });
  store.__onChange = (listener) => {
    listeners.add(listener);
    return () => listeners.delete(listener);
  };
  // 供外部（诊断脚本/调试）强制触发一次重渲染：直接改字段可能因"值相同"被跳过。
  store.__forceRender = () => listeners.forEach((listener) => listener());
  store.__setDrawing = (value) => {
    drawing = value;
  };
  activeStore = store;
  return store;
}

/// 批量更新：一次事件里改多个字段只渲染一次（避免闪烁与重复请求）。
/// 用法：用浏览器原生的 microtask 合并即可，这里保留入口以便将来扩展。
export function batch(fn) {
  return fn();
}

export function h(tag, props, ...children) {
  const element = document.createElement(tag);
  if (props) {
    for (const [key, value] of Object.entries(props)) {
      if (value === null || value === undefined || value === false) continue;
      if (key === "class") element.className = value;
      else if (key === "style" && typeof value === "object") Object.assign(element.style, value);
      else if (key === "dataset" && typeof value === "object") Object.assign(element.dataset, value);
      else if (key.startsWith("on") && typeof value === "function") {
        element.addEventListener(key.slice(2).toLowerCase(), value);
      } else if (key === "html") element.innerHTML = value;
      else if (key === "ref" && typeof value === "function") value(element);
      else if (value === true) element.setAttribute(key, "");
      else element.setAttribute(key, String(value));
    }
  }
  append(element, children);
  return element;
}

function append(parent, children) {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) append(parent, child);
    else if (child instanceof Node) parent.appendChild(child);
    else parent.appendChild(document.createTextNode(String(child)));
  }
}

export function text(value) {
  return document.createTextNode(value === null || value === undefined ? "" : String(value));
}

/// 挂载：状态每次变化就重建视图（桌面应用规模下足够快，且不会有状态漂移）。
export function mount(store, render) {
  const root = document.getElementById("app");
  let scheduled = false;
  const draw = () => {
    scheduled = false;
    store.__setDrawing(true);
    try {
      const scrollState = captureScroll(root);
      const focusState = captureFocus(root);
      // ⚠ 整棵重建必须避开"用户正在输入的元素"：否则每敲一个字符都会把 textarea
      // 销毁重建 → 焦点丢失 → 表现成"输入框根本没法输入"（真实故障）。
      // 做法：把焦点元素的引用挪到新旧树的同一位置（节点级复用），并在重建后恢复。
      const preserved = reuseFocusedNode(root, focusState);
      root.replaceChildren(render());
      restoreFocusedNode(root, preserved, focusState);
      restoreScroll(root, scrollState);
    } finally {
      store.__setDrawing(false);
    }
  };
  store.__onChange(() => {
    if (scheduled) return;
    scheduled = true;
    requestAnimationFrame(draw);
  });
  draw();
}

/// 记录当前焦点位置（用元素路径表达，便于在新树里找回同一位置）。
function captureFocus(root) {
  const active = document.activeElement;
  if (!active || active === document.body || !root.contains(active)) return null;
  const path = [];
  let node = active;
  while (node && node !== root) {
    const parent = node.parentElement;
    if (!parent) return null;
    path.unshift(Array.prototype.indexOf.call(parent.children, node));
    node = parent;
  }
  return {
    path,
    tag: active.tagName,
    start: typeof active.selectionStart === "number" ? active.selectionStart : null,
    end: typeof active.selectionEnd === "number" ? active.selectionEnd : null,
    value: typeof active.value === "string" ? active.value : null,
  };
}

function nodeAtPath(root, path) {
  let node = root;
  for (const index of path) {
    if (!node || !node.children || index >= node.children.length) return null;
    node = node.children[index];
  }
  return node;
}

/// 在重建前把焦点节点从旧树里摘出来（保留其 DOM 身份与已输入的值）。
function reuseFocusedNode(root, focusState) {
  if (!focusState) return null;
  const node = nodeAtPath(root, focusState.path);
  if (!node || node.tagName !== focusState.tag) return null;
  node.remove();
  return node;
}

/// 重建后把同一个节点放回它的位置并恢复焦点与光标（不打断输入）。
function restoreFocusedNode(root, node, focusState) {
  if (!node || !focusState) return;
  const path = focusState.path.slice(0, -1);
  const parent = path.length ? nodeAtPath(root, path) : root;
  const index = focusState.path[focusState.path.length - 1];
  if (!parent) return;
  const before = parent.children[index] || null;
  parent.insertBefore(node, before);
  if (typeof node.focus === "function") {
    node.focus({ preventScroll: true });
    if (focusState.start !== null && typeof node.setSelectionRange === "function") {
      try {
        node.setSelectionRange(focusState.start, focusState.end === null ? focusState.start : focusState.end);
      } catch (_) {
        /* 某些 input 类型不支持 selection，忽略 */
      }
    }
  }
}

// 重建视图会丢滚动位置：这里按元素标识记一下（聊天区、列表区）。
function captureScroll(root) {
  const snapshot = {};
  for (const node of root.querySelectorAll("[data-scroll-key]")) {
    snapshot[node.dataset.scrollKey] = { top: node.scrollTop, height: node.scrollHeight };
  }
  return snapshot;
}

function restoreScroll(root, snapshot) {
  for (const node of root.querySelectorAll("[data-scroll-key]")) {
    const saved = snapshot[node.dataset.scrollKey];
    if (!saved) continue;
    // 只有原本贴在底部时才继续贴底（用户上翻查看历史时不打扰）。
    const atBottom = saved.height - saved.top <= node.clientHeight + 40;
    node.scrollTop = atBottom ? node.scrollHeight : saved.top;
  }
}

export function escapeHtml(value) {
  return String(value === null || value === undefined ? "" : value).replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[ch]);
}

/// 轻量 markdown → HTML（只做安全子集：代码块/行内代码/粗体/换行）。
export function renderMarkdown(source) {
  const escaped = escapeHtml(source);
  const blocks = escaped.split(/```/);
  return blocks
    .map((block, index) => {
      if (index % 2 === 1) {
        const body = block.replace(/^[a-zA-Z0-9+-]*\n/, "");
        return `<pre class="md-code"><code>${body}</code></pre>`;
      }
      return block
        .replace(/`([^`]+)`/g, "<code>$1</code>")
        .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
        .replace(/^### (.*)$/gm, "<h4>$1</h4>")
        .replace(/^## (.*)$/gm, "<h3>$1</h3>")
        .replace(/\n/g, "<br>");
    })
    .join("");
}
