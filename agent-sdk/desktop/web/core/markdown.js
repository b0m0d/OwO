// OwO Agent 轻量 Markdown / LaTeX 渲染器（自 app.js 机械提取，单一职责：文本 → 安全 HTML）。
//
// 无外部依赖、不访问会话/网络状态；app.js 经 window.OwoMarkdown 解构复用原函数名。
// 安全边界：所有插值先 HTML 转义；链接仅放行 http/https/mailto 与相对路径。
(function (root) {
  "use strict";

// ---------- 轻量 Markdown 渲染（对标 Codex 桌面：代码块/标题/列表/表格/行内样式） ----------

function escapeHtml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function escapeAttribute(text) {
  return escapeHtml(text).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

function safeMarkdownHref(raw) {
  const href = raw.replace(/&amp;/g, "&").trim();
  if (!href || /^(?:javascript|data|vbscript):/i.test(href)) return "";
  try {
    const url = new URL(href, window.location.href);
    if (["http:", "https:", "mailto:"].includes(url.protocol)) return url.href;
  } catch (_) {
    // 无法解析的链接按普通文本显示。
  }
  return /^(?:\.|\/|#)/.test(href) ? href : "";
}

// ---------- 轻量 LaTeX 可读化（推理模型的输出习惯） ----------
//
// 接入深度思考（deepseek-reasoner / glm-z1 系）后暴露的新问题：这类模型写数学
// 结论时习惯用 LaTeX（`\frac{1}{6}`、`\times`、`\approx`），轻量 Markdown 渲染器
// 不认 LaTeX，原样吐给用户——一屏 `$\frac{24}{7}$` 看起来就像乱码。
//
// 这里不做完整 KaTeX（体积大、与 md 代码块冲突多），只做**可读化**：
// 分隔符内的公式转成 `a/(b)` 形式并用等宽样式标出，分隔符外的裸符号做等价替换。
// 代码块不经过这里（flushCode 直接转义），所以不会误伤代码。

// 占位符用 NUL (NUL 不会出现在正常文本里，行内规则也不会跨它匹配)
const TEX_MARK = "\u0000";

const TEX_SYMBOLS = [
  ["\\times", "×"],
  ["\\cdot", "·"],
  ["\\div", "÷"],
  ["\\approx", "≈"],
  ["\\neq", "≠"],
  ["\\leq", "≤"],
  ["\\geq", "≥"],
  ["\\le", "≤"],
  ["\\ge", "≥"],
  ["\\pm", "±"],
  ["\\mp", "∓"],
  ["\\rightarrow", "→"],
  ["\\Rightarrow", "⇒"],
  ["\\to", "→"],
  ["\\ldots", "…"],
  ["\\cdots", "…"],
  ["\\infty", "∞"],
  ["\\pi", "π"],
  ["\\alpha", "α"],
  ["\\beta", "β"],
  ["\\gamma", "γ"],
  ["\\theta", "θ"],
  ["\\lambda", "λ"],
  ["\\mu", "μ"],
  ["\\sigma", "σ"],
  ["\\Delta", "Δ"],
  ["\\%", "%"],
];

/// LaTeX 片段 → 可读文本（分数/根号做结构化简写，其余符号等价替换）。
function texToReadable(tex) {
  let out = String(tex || "");
  out = out.replace(/\\[dt]?frac\s*\{([^{}]*)\}\s*\{([^{}]*)\}/g, "($1)/($2)");
  out = out.replace(/\\[dt]?frac\s*(\d)\s*(\d)/g, "($1)/($2)");
  out = out.replace(/\\sqrt\s*\{([^{}]*)\}/g, "√($1)");
  out = out.replace(/\\sqrt\s*(\d)/g, "√$1");
  for (const [command, symbol] of TEX_SYMBOLS) {
    out = out.split(command).join(symbol);
  }
  out = out.replace(/\\(?:left|right|displaystyle|text|mathrm|mathbf|operatorname|mbox)\b/g, "");
  out = out.replace(/\\\\/g, " ");
  out = out.replace(/[{}]/g, "");
  return out.replace(/\s+/g, " ").trim();
}

/// 公式表按"每次渲染"累积：占位符在整篇 html 拼好后统一还原，
/// 这样行内渲染（标题/列表/表格/段落）无需各自关心公式。
let TEX_STORE = [];

/// 把 `\(…\)` / `$$…$$` / `$…$` 的公式抠成占位符（只对非代码行调用）。
function extractTex(text) {
  const hold = (match, body) => {
    TEX_STORE.push(texToReadable(body));
    return `${TEX_MARK}${TEX_STORE.length - 1}${TEX_MARK}`;
  };
  let out = String(text || "");
  out = out.replace(/\\\[([\s\S]+?)\\\]/g, hold);
  out = out.replace(/\$\$([\s\S]+?)\$\$/g, hold);
  out = out.replace(/\$([^$\n]{1,400}?)\$/g, hold);
  out = out.replace(/\\\(([\s\S]+?)\\\)/g, hold);
  return out;
}

/// 占位符还原：公式渲染成等宽的 `<code class="md-tex">`。
function restoreTex(html, store) {
  if (!store.length) return html;
  return html.replace(
    new RegExp(`${TEX_MARK}(\\d+)${TEX_MARK}`, "g"),
    (match, index) => {
      const body = store[Number(index)];
      if (!body) return match;
      return `<code class="md-tex">${escapeHtml(body)}</code>`;
    }
  );
}

function inlineMarkdown(text) {
  let out = escapeHtml(text);
  out = out.replace(/`([^`]+)`/g, "<code>$1</code>");
  out = out.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  out = out.replace(/\*([^*]+)\*/g, "<em>$1</em>");
  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (match, label, rawHref) => {
    const href = safeMarkdownHref(rawHref);
    return href
      ? `<a href="${escapeAttribute(href)}" target="_blank" rel="noopener">${label}</a>`
      : label;
  });
  // 分隔符外的裸 LaTeX（模型常常不加 $…$）：符号表 + 花括号边界明确的 frac/sqrt。
  // 取舍说明：行内代码里的 \frac 也会被化简（概率极低——没人会在代码片段里写
  // 公式），换取正文中裸公式可读，这个交换是划算的。
  out = out.replace(/\\[dt]?frac\s*\{([^{}]*)\}\s*\{([^{}]*)\}/g, "($1)/($2)");
  out = out.replace(/\\[dt]?frac\s*(\d)\s*(\d)/g, "($1)/($2)");
  out = out.replace(/\\sqrt\s*\{([^{}]*)\}/g, "√($1)");
  for (const [command, symbol] of TEX_SYMBOLS) {
    out = out.split(command).join(symbol);
  }
  return out;
}

// Markdown 表格按分隔行识别，保留空单元格和转义/代码中的竖线。
function splitMarkdownTableRow(line) {
  const source = String(line || "").trim();
  if (!source.includes("|")) return null;
  const cells = [];
  let cell = "";
  let codeTicks = 0;
  for (let index = 0; index < source.length; index++) {
    const char = source[index];
    if (char === "\\" && source[index + 1] === "|") {
      cell += "|";
      index += 1;
      continue;
    }
    if (char === "`") {
      let end = index + 1;
      while (source[end] === "`") end += 1;
      const run = end - index;
      if (!codeTicks) codeTicks = run;
      else if (run === codeTicks) codeTicks = 0;
      cell += source.slice(index, end);
      index = end - 1;
      continue;
    }
    if (char === "|" && !codeTicks) {
      cells.push(cell.trim());
      cell = "";
      continue;
    }
    cell += char;
  }
  cells.push(cell.trim());
  if (source.startsWith("|")) cells.shift();
  if (source.endsWith("|") && !source.endsWith("\\|")) cells.pop();
  return cells;
}

function isMarkdownTableSeparator(cells) {
  return Array.isArray(cells) && cells.length >= 2 && cells.every((cell) => /^:?-{3,}:?$/.test(cell));
}

function markdownTableAlignment(separator) {
  const left = separator.startsWith(":");
  const right = separator.endsWith(":");
  return left && right ? "center" : right ? "right" : "left";
}

// 把 markdown 文本渲染为 HTML。代码块保留原样（pre/code），行内元素转义。
// 公式可读化只对**非代码行**做占位（代码块里的 $…$ 是代码不是公式），
// 整篇 html 拼好后再统一还原占位符。
function renderMarkdown(text) {
  if (!text) return "";
  TEX_STORE = [];
  const lines = text.split("\n");
  const html = [];
  let inCode = false;
  let codeLang = "";
  let codeFenceChar = "";
  let codeFenceLength = 0;
  let codeLines = [];
  let inList = "";
  let inQuote = false;
  let inTable = false;
  let tableHeader = null;
  let tableAlign = null;

  const flushCode = () => {
    if (codeLines.length) {
      html.push(
        `<pre class="md-code"><div class="md-code-head"><span>${escapeHtml(codeLang || "code")}</span><button class="md-copy" data-code="${encodeURIComponent(codeLines.join("\n"))}">复制</button></div><code>${escapeHtml(codeLines.join("\n"))}</code></pre>`
      );
      codeLines = [];
    }
    inCode = false;
    codeLang = "";
    codeFenceChar = "";
    codeFenceLength = 0;
  };
  const flushList = () => {
    if (inList) {
      html.push("</" + inList + ">");
      inList = "";
    }
  };
  const flushQuote = () => {
    if (inQuote) {
      html.push("</blockquote>");
      inQuote = false;
    }
  };
  const flushTable = () => {
    if (inTable) {
      html.push("</table>");
      inTable = false;
    }
    tableHeader = null;
    tableAlign = null;
  };

  for (let lineIndex = 0; lineIndex < lines.length; lineIndex++) {
    const rawLine = lines[lineIndex];
    const fence = rawLine.match(/^\s*(`{3,}|~{3,})(.*)$/);
    if (fence) {
      const marker = fence[1];
      const info = fence[2] || "";
      if (inCode) {
        const closes = marker[0] === codeFenceChar && marker.length >= codeFenceLength && /^\s*$/.test(info);
        if (closes) {
          flushCode();
          continue;
        }
      } else {
        flushList();
        flushQuote();
        flushTable();
        inCode = true;
        codeFenceChar = marker[0];
        codeFenceLength = marker.length;
        codeLang = (info.trim().split(/\s+/, 1)[0] || "").replace(/[^\w.+#-]/g, "");
        continue;
      }
    }
    if (inCode) {
      codeLines.push(rawLine);
      continue;
    }
    const line = extractTex(rawLine);
    if (/^\s*$/.test(line)) {
      flushList();
      flushQuote();
      flushTable();
      html.push("");
      continue;
    }
    const quote = line.match(/^\s*>\s?(.*)$/);
    if (quote) {
      flushList();
      flushTable();
      if (!inQuote) {
        html.push('<blockquote class="md-quote">');
        inQuote = true;
      }
      html.push('<div class="md-p">' + inlineMarkdown(quote[1]) + "</div>");
      continue;
    }
    flushQuote();
    const heading = line.match(/^(#{1,4})\s+(.*)$/);
    if (heading) {
      flushList();
      flushTable();
      const level = heading[1].length;
      html.push(`<h${level} class="md-h${level}">${inlineMarkdown(heading[2])}</h${level}>`);
      continue;
    }
    const hr = line.match(/^\s*(-{3,}|\*{3,})\s*$/);
    if (hr) {
      flushList();
      flushTable();
      html.push('<hr class="md-hr">');
      continue;
    }
    const unorderedItem = line.match(/^\s*[-*+]\s+(.*)$/);
    const orderedItem = line.match(/^\s*\d+\.\s+(.*)$/);
    const listItem = unorderedItem || orderedItem;
    if (listItem) {
      flushTable();
      const listTag = orderedItem ? "ol" : "ul";
      if (inList && inList !== listTag) flushList();
      if (!inList) {
        html.push("<" + listTag + ' class="md-list">');
        inList = listTag;
      }
      html.push("<li>" + inlineMarkdown(listItem[1]) + "</li>");
      continue;
    }
    flushList();
    const rowCells = splitMarkdownTableRow(line);
    if (inTable) {
      if (rowCells && rowCells.length) {
        const paddedCells = rowCells.slice(0, tableHeader.length);
        while (paddedCells.length < tableHeader.length) paddedCells.push("");
        html.push("<tr>");
        paddedCells.forEach((cell, index) => {
          html.push(`<td style="text-align:${tableAlign[index] || "left"}">${inlineMarkdown(cell)}</td>`);
        });
        html.push("</tr>");
        continue;
      }
      flushTable();
    }
    const separatorCells = splitMarkdownTableRow(lines[lineIndex + 1] || "");
    if (rowCells && rowCells.length >= 2 && isMarkdownTableSeparator(separatorCells) && separatorCells.length === rowCells.length) {
      tableHeader = rowCells;
      tableAlign = separatorCells.map(markdownTableAlignment);
      inTable = true;
      html.push('<table class="md-table"><thead><tr>');
      tableHeader.forEach((cell, index) => {
        html.push(`<th style="text-align:${tableAlign[index]}">${inlineMarkdown(cell)}</th>`);
      });
      html.push("</tr></thead><tbody>");
      lineIndex += 1;
      continue;
    }
    html.push(`<div class="md-p">${inlineMarkdown(line)}</div>`);
  }
  flushCode();
  flushList();
  flushQuote();
  flushTable();
  return restoreTex(html.join("\n"), TEX_STORE);
}

  root.OwoMarkdown = Object.freeze({
    escapeHtml,
    escapeAttribute,
    safeMarkdownHref,
    texToReadable,
    extractTex,
    restoreTex,
    inlineMarkdown,
    splitMarkdownTableRow,
    isMarkdownTableSeparator,
    markdownTableAlignment,
    renderMarkdown,
  });
})(typeof window !== "undefined" ? window : globalThis);
