#!/usr/bin/env node
// ============================================================================
// §8.2 UI 文案 lint —— 普通路由用户可见文案禁止泄漏实现细节
//
// 规则（源自 builGoal/Agent-SDK-全项目问题审计与技术重构方案-2026-09-05.md §8.2）：
//   禁止示例：/teams/{id}、POST、steer(retry)、output_artifact_refs
//   用户文案：团队详情暂不可用、重新执行失败步骤、关联产物
//   技术详情：保留原始 route、status、trace id 和服务端错误
//
// 扫描对象：desktop/web/panels/**/*.js 中的字符串字面量（用户可见 HTML 文案、
// 提示、错误信息）。仅匹配字符串字面量内的内容；请求体键名（代码标识符，
// 如 body.output_artifact_refs）不属于用户文案，不在扫描范围。
//
// 豁免：开发者模式门控分块（data-dev + .owo-dev-block）允许出现原始字段名，
// 在对应行尾添加 `ui-lint:allow` 注释标记即可豁免该行。
//
// 用法：node tools/ui-copy-lint.mjs   （退出码 0 = 通过；1 = 存在泄漏）
// ============================================================================
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, dirname, relative } from "node:path";
import { fileURLToPath } from "node:url";

const panelsDir = join(dirname(fileURLToPath(import.meta.url)), "..", "desktop", "web", "panels");

// ---------- 禁止模式（用户可见字符串中出现即违规） ----------
const DENY = [
  { re: /字段：[A-Za-z_]/, why: "“字段：xxx”提示直接暴露内部字段名" },
  { re: /<label>[a-z][a-z0-9]*_[a-z0-9_]*</, why: "label 可见文本使用 snake_case 字段名" },
  {
    re: /\b(output_artifact_refs|evidence_refs|from_member|to_member|completed_summary|open_issues|suggested_next_actions|known_risks|handoff_contract|new_user_id|approval_required|depends_on|max_retries)\b/,
    why: "内部字段名泄漏进用户文案",
  },
  { re: /\bsteer\((retry|continue|replace|cancel)\)/, why: "内部动作名以 steer(xxx) 形式出现在文案" },
  { re: /[“”（](POST|PUT|DELETE|PATCH)\b/, why: "HTTP method 出现在中文文案中" },
];

function listJsFiles(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const st = statSync(p);
    if (st.isDirectory()) out.push(...listJsFiles(p));
    else if (name.endsWith(".js")) out.push(p);
  }
  return out;
}

const files = listJsFiles(panelsDir);
let violations = 0;

for (const file of files) {
  const rel = relative(process.cwd(), file).split("\\").join("/");
  const lines = readFileSync(file, "utf8").split(/\r?\n/);
  lines.forEach((line, idx) => {
    if (line.includes("ui-lint:allow")) return; // 开发者分块豁免
    // 提取本行字符串字面量（单/双引号；不含跨行字面量——文案均为单行）
    const literals = line.match(/'(?:[^'\\]|\\.)*'|"(?:[^"\\]|\\.)*"/g) || [];
    for (const lit of literals) {
      for (const { re, why } of DENY) {
        const m = lit.match(re);
        if (m) {
          violations++;
          console.log(
            `${rel}:${idx + 1}: ${why}（命中：${m[0]}）\n    ${line.trim().slice(0, 120)}`
          );
        }
      }
    }
  });
}

if (violations > 0) {
  console.log(`\nui-copy-lint：${violations} 处文案泄漏（修复或加行内 ui-lint:allow 豁免）`);
  process.exit(1);
}
console.log("ui-copy-lint：通过（普通路由文案零泄漏）");
