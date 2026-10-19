// §12.3 shell 拆分批次一：轻量 Markdown 渲染（自 app.js 机械外移，零行为变化）。
// 纯函数；panels 经 app.js 注入的 H.renderMarkdown 消费，本文件自身无依赖。

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
  return out;
}

// 把 markdown 文本渲染为 HTML。代码块保留原样（pre/code），行内元素转义。
function renderMarkdown(text) {
  if (!text) return "";
  const lines = text.split("\n");
  const html = [];
  let inCode = false;
  let codeLang = "";
  let codeLines = [];
  let inList = false;
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
  };
  const flushList = () => {
    if (inList) {
      html.push("</ul>");
      inList = false;
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

  for (const line of lines) {
    const fence = line.match(/^```(\w*)\s*$/);
    if (fence) {
      if (inCode) flushCode();
      else {
        flushList();
        flushTable();
        inCode = true;
        codeLang = fence[1];
      }
      continue;
    }
    if (inCode) {
      codeLines.push(line);
      continue;
    }
    if (/^\s*$/.test(line)) {
      flushList();
      flushTable();
      html.push("");
      continue;
    }
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
    const li = line.match(/^\s*[-*+]\s+(.*)$/) || line.match(/^\s*\d+\.\s+(.*)$/);
    if (li) {
      flushTable();
      if (!inList) {
        html.push("<ul class=\"md-list\">");
        inList = true;
      }
      html.push(`<li>${inlineMarkdown(li[1])}</li>`);
      continue;
    }
    flushList();
    const tableLine = line.match(/^\|?\s*(.*?)\s*\|?$/);
    const cells = line.split("|").slice(1, -1);
    const allCells = line.split("|").filter((cell) => cell.trim() !== "");
    if (allCells.length > 1 && !tableHeader) {
      tableHeader = allCells.map((cell) => cell.trim());
      inTable = true;
      html.push('<table class="md-table"><thead><tr>');
      for (const cell of tableHeader) {
        html.push(`<th>${inlineMarkdown(cell)}</th>`);
      }
      html.push("</tr></thead><tbody>");
      continue;
    }
    if (inTable) {
      if (tableHeader && allCells.every((cell) => /^:?-{2,}:?$/.test(cell.trim()))) {
        tableAlign = allCells.map((cell) => cell.trim());
        continue;
      }
      if (allCells.length) {
        html.push("<tr>");
        for (let index = 0; index < allCells.length; index++) {
          html.push(`<td>${inlineMarkdown(allCells[index])}</td>`);
        }
        html.push("</tr>");
        continue;
      }
      flushTable();
      tableHeader = null;
    }
    html.push(`<div class="md-p">${inlineMarkdown(line)}</div>`);
  }
  flushCode();
  flushList();
  flushTable();
  return html.join("\n");
}
