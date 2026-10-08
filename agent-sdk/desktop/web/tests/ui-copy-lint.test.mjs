import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// §8.2 UI 文案 lint：用户可见文案（含中文的字符串字面量）不得出现 API path、
// HTTP method、路由模板、内部动作名与内部字段名。
// 豁免：① 纯代码行/注释行（开发者可读面）；② title 属性（§8.2 技术详情折叠位，
// 允许保留原始 route/status/trace id）；③ 不含中文的字符串（数据值/选择器/端点）。
// 行级豁免：确需保留时在行内写 `ui-copy-lint:allow` 并注明理由。

const here = fileURLToPath(new URL(".", import.meta.url));
const panelsDir = join(here, "../panels");

// 开发者模式面板（§8.1：原始 JSON/DSL/内部 id 的合法场所）不参与文案 lint。
const DEV_PANELS = new Set([
  "eval.panel.js",
  "fleet.panel.js",
  "observability.panel.js",
  "plugin-market.panel.js",
]);

const panels = readdirSync(panelsDir)
  .filter((name) => name.endsWith(".js") && !DEV_PANELS.has(name))
  .sort();
const sources = [
  ["../app.js", readFileSync(join(here, "../app.js"), "utf8")],
  ...panels.map((name) => [
    `../panels/${name}`,
    readFileSync(join(panelsDir, name), "utf8"),
  ]),
];

// title 属性视为"技术详情"折叠位；注释行是开发者面——先剔除。
const stripTechnicalDetail = (source) =>
  source
    .replace(/title=\\"[^"]*\\"/g, 'title=""')
    .replace(/title="[^"]*"/g, 'title=""')
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");

// 提取含中文的字符串字面量（用户文案判定：本产品用户文案均为中文）。
function cjkStrings(line) {
  const found = [];
  const tick = String.fromCharCode(96);
  for (let index = 0; index < line.length;) {
    const quote = line[index];
    if (quote !== "\"" && quote !== "'" && quote !== tick) {
      index += 1;
      continue;
    }
    index += 1;
    let value = "";
    while (index < line.length) {
      const char = line[index];
      if (char === "\\") {
        if (index + 1 < line.length) value += char + line[index + 1];
        index += 2;
        continue;
      }
      if (char === quote) {
        index += 1;
        break;
      }
      value += char;
      index += 1;
    }
    if (/[\u4e00-\u9fff]/.test(value)) found.push(value);
  }
  return found;
}

const BANNED = [
  [/\b(GET|POST|PUT|DELETE) \//, "HTTP 方法+路径"],
  [/\/[a-z][a-z0-9-]*\/\{[a-z_]+\}/, "路由模板"],
  [/steer\(/, "内部动作名"],
  [
    /\b(awaiting_human|pending_review|write_lease|output_artifact_refs|completed_summary|open_issues|evidence_refs|suggested_next_actions|known_risks|permission_level|last_compaction)\b/,
    "内部字段/状态名",
  ],
];

function collectUserCopyViolations(source, fileName) {
  const violations = [];
  const cleaned = stripTechnicalDetail(source);
  const lines = cleaned.split("\n");
  lines.forEach((line, index) => {
    if (line.includes("ui-copy-lint:allow")) return;
    for (const copy of cjkStrings(line)) {
      for (const [pattern, label] of BANNED) {
        if (pattern.test(copy)) {
          violations.push(
            `${fileName}:${index + 1} ${label} → ${copy.trim().slice(0, 110)}`,
          );
        }
      }
    }
  });
  return violations;
}

test("UI copy lexer scans quoted and template strings in linear time", () => {
  const cjk = String.fromCharCode(0x6a21, 0x578b);
  const tick = String.fromCharCode(96);
  const templateContents = cjk + ":" + String.fromCharCode(36) + "{name}";
  const line = 'const a = "prefix ' + cjk + '"; const b = ' + tick + templateContents + tick + ';';
  assert.deepEqual(cjkStrings(line), ["prefix " + cjk, templateContents]);
});

test("§8.2 UI 文案 lint：普通界面不得泄漏 API 路径、HTTP 方法与内部字段名", () => {
  const violations = sources.flatMap(([file, source]) =>
    collectUserCopyViolations(source, file),
  );
  assert.deepEqual(
    violations,
    [],
    `发现 ${violations.length} 处文案泄漏（修复或用 title= 折叠为技术详情）：\n${violations.join("\n")}`,
  );
});

test("§8.2 开发者面板豁免清单必须与真实文件一致", () => {
  for (const name of DEV_PANELS) {
    assert.ok(
      readdirSync(panelsDir).includes(name),
      `豁免清单中的开发者面板不存在：${name}`,
    );
  }
});
