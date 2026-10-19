// ============================================================================
// WorkSwarm 交接对话 / 引用选择器渲染（HTML 视图构建）
//   desktop/web/panels/workswarm/render_conversation.js
//
// 从 render.js 拆出的两个末端纯 HTML 构建器：
//   - teamConversationHtml：按发言人/时间顺序渲染已持久化的团队交接消息；
//   - refPickerHtml：结构化引用选择器（已选行 + 搜索候选 + allowFree 输入）。
//
// esc/short 由调用方经 ctx 注入（render.js 的 bindEsc/bindShort 可变绑定），
// 保持浏览器（H.esc DOM 实现）与 Node 单测（defaultEsc 正则实现）行为一致。
//
// 加载约定与 domain/format/render 相同：
//   - 浏览器由 index.html 预加载，挂 win.OwoWorkswarmRenderConversation；
//   - Node 单测 require 同目录模块（module.exports 导出）。
// ============================================================================
(function () {
  "use strict";

  var win = typeof window !== "undefined" ? window : globalThis;

  /// §8.1 引用选择器（artifact/evidence/member 引用）：已选结构化行（可移除）
  /// + 搜索过滤后的候选按钮（可点击添加）。allowFree 时支持输入任意引用
  /// （如证据链接/描述）后点「添加」成为一行。
  /// opts = {kind, rows:[string], options:[{value,label}], query, allowFree}
  function refPickerHtml(ctx, opts) {
    var esc = ctx.esc;
    var short = ctx.short;
    var kind = String((opts && opts.kind) || "");
    var rows = (opts && opts.rows) || [];
    var options = (opts && opts.options) || [];
    var query = (opts && opts.query) == null ? "" : String(opts.query);
    var allowFree = !!(opts && opts.allowFree);
    var rowsHtml = rows.length
      ? rows.map(function (r) {
          var v = String(r);
          return (
            '<span class="owo-ws-refrow"><span class="owo-ws-mono" title="' + esc(v) + '">' +
            esc(short(v)) + "</span>" +
            '<button type="button" class="owo-ws-mini" data-core-action data-ws-ref-del="' + esc(kind + ":" + v) +
            '" title="移除该引用">×</button></span>'
          );
        }).join(" ")
      : '<span class="hint">暂无引用行</span>';
    var sug = options
      .map(function (o) {
        return (
          '<button type="button" class="owo-ws-mini" data-core-action data-ws-ref-add="' + esc(kind + ":" + (o && o.value)) +
          '" title="' + esc((o && o.label) || (o && o.value) || "") + '">' +
          esc(short((o && o.label) || (o && o.value) || "")) + "</button>"
        );
      })
      .join(" ");
    return (
      '<div class="owo-ws-refpick" data-ws-refpick="' + esc(kind) + '">' +
      '<div class="owo-ws-refrows">' + rowsHtml + "</div>" +
      '<div class="owo-ws-inline">' +
      '<input class="owo-ws-refq" data-ws-ref-q="' + esc(kind) + '" size="34" placeholder="' +
      (allowFree ? "搜索候选或输入引用后点添加" : "搜索候选引用") + '" value="' + esc(query) + '">' +
      (allowFree ? '<button type="button" class="owo-ws-mini" data-ws-ref-free="' + esc(kind) + '">添加</button>' : "") +
      "</div>" +
      (sug ? '<div class="owo-ws-refsug">' + sug + "</div>" : '<div class="hint">无匹配候选</div>') +
      "</div>"
    );
  }

  function teamConversationHtml(ctx, handoffs, members, markdown, errorText) {
    var esc = ctx.esc;
    if (errorText) {
      return '<div class="owo-ws-chat-state bad" role="alert">' + esc(errorText) + "</div>";
    }
    if (handoffs === null) {
      return '<div class="owo-ws-chat-state hint" aria-live="polite">正在读取团队交接记录…</div>';
    }
    if (!Array.isArray(handoffs) || !handoffs.length) {
      return '<div class="owo-ws-chat-state hint">暂无已保存的交接消息。逐轮内部对话只有在服务端持久化后才能展示。</div>';
    }
    var memberMap = {};
    (Array.isArray(members) ? members : []).forEach(function (m) {
      if (m && m.member_id) memberMap[String(m.member_id)] = m;
    });
    var rows = handoffs.slice().sort(function (a, b) {
      return String((a && a.created_at) || "").localeCompare(String((b && b.created_at) || ""));
    });
    return '<div class="owo-ws-chat-log" role="log" aria-label="团队交接对话">' +
      rows.map(function (h, index) {
        h = h || {};
        var senderId = String(h.from_member || "");
        var receiverId = String(h.to_member || "");
        var member = memberMap[senderId] || {};
        var role = String(member.role || senderId || "团队成员");
        var initial = role.trim().slice(0, 1) || "团";
        var slug = role.toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "member";
        var summary = String(h.completed_summary || "");
        var body = typeof markdown === "function"
          ? markdown(summary)
          : '<div class="md-p">' + esc(summary).replace(/\n/g, "<br>") + "</div>";
        var receiver = receiverId
          ? '<span class="owo-ws-chat-to">发给 ' + esc((memberMap[receiverId] && memberMap[receiverId].role) || receiverId) + "</span>"
          : "";
        var extras = "";
        [
          ["遗留问题", h.open_issues],
          ["下一步", h.suggested_next_actions],
          ["已知风险", h.known_risks],
        ].forEach(function (pair) {
          var values = Array.isArray(pair[1]) ? pair[1].filter(Boolean) : [];
          if (!values.length) return;
          extras += '<div class="owo-ws-chat-extra"><b>' + pair[0] + "</b><ul>" +
            values.map(function (value) { return "<li>" + esc(value) + "</li>"; }).join("") +
            "</ul></div>";
        });
        var refs = Array.isArray(h.output_artifact_refs) ? h.output_artifact_refs.filter(Boolean) : [];
        return '<article class="owo-ws-chat-row speaker-' + esc(slug) + '" data-handoff-index="' + index + '">' +
          '<div class="owo-ws-chat-avatar" aria-hidden="true">' + esc(initial) + "</div>" +
          '<div class="owo-ws-chat-card"><header class="owo-ws-chat-head"><b>' + esc(role) + "</b>" +
          '<span class="owo-ws-chat-id">' + esc(senderId) + "</span>" + receiver +
          '<time>' + esc(h.created_at || "") + "</time></header>" +
          '<div class="owo-ws-chat-body">' + body + "</div>" + extras +
          (refs.length ? '<div class="owo-ws-chat-refs">关联产物：' + refs.map(esc).join(" · ") + "</div>" : "") +
          "</div></article>";
      }).join("") + "</div>";
  }

  var api = {
    refPickerHtml: refPickerHtml,
    teamConversationHtml: teamConversationHtml,
  };

  win.OwoWorkswarmRenderConversation = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})();
