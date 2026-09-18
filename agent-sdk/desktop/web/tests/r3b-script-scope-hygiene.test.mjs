// R3-B 作用域卫生契约（§3.4 业务引导终态的地基）。
//
// 背景（实测缺陷，别再靠肉眼）：视图模块统一写成 `(function (global) { … })(window)`
// 形式，形参名就叫 `global`；而 app.js / core/*.js 是**顶层脚本**，作用域里根本没有
// `global` 标识符。上一版 renderSetupGuide() 里写了 `global.__owoCoreDiagnostics`，
// 在 `"use strict"` 下抛 ReferenceError——此时 `content.replaceChildren()` 已经把
// 主区清空，于是 no-workspace 场景呈现"标题有了、正文空白"，矩阵三条断言连环失败，
// 而且**任何单测都抓不到**（浏览器里才炸）。
//
// 本测试把这条规则变成机器可检查的契约：
//   1) 顶层脚本（未被 IIFE 包裹）禁止出现裸 `global.` 引用；
//   2) 引导页渲染必须有异常兜底：抛错时回落错误卡，绝不允许白屏；
//   3) 引导/错误两条终态通道都必须真的往 #routeContent 里写东西。
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

function listScripts(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    if (entry === "tests" || entry === "node_modules" || entry.startsWith(".")) continue;
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) out.push(...listScripts(full));
    else if (entry.endsWith(".js")) out.push(full);
  }
  return out;
}

// IIFE 形式的模块（视图/core）自带 `global` 形参，属于安全写法。
const IIFE_WRAPPED = /^\s*\(function\s*\(\s*global\s*[,)]/m;

test("契约1：顶层脚本不得引用裸 global（ReferenceError 会清空白屏）", () => {
  const offenders = [];
  for (const file of listScripts(WEB_ROOT)) {
    const text = readFileSync(file, "utf8");
    if (IIFE_WRAPPED.test(text)) continue;
    // 允许注释里出现（文档会说"global 形参"），只看非注释行。
    const codeOnly = text
      .split(/\r?\n/)
      .filter((line) => !/^\s*(\/\/|\*|\/\*)/.test(line))
      .join("\n");
    if (/\bglobal\./.test(codeOnly)) {
      const rel = file.slice(WEB_ROOT.length + 1).replace(/\\/g, "/");
      offenders.push(rel);
    }
  }
  assert.deepEqual(
    offenders,
    [],
    `以下顶层脚本引用了不存在的 global：${offenders.join(", ")}（顶层脚本请用 window）`
  );
});

test("契约2：renderSetupGuide 必须有异常兜底，不允许主区留空", () => {
  const app = readFileSync(join(WEB_ROOT, "app.js"), "utf8");
  const body = /function renderSetupGuide\(\)\s*\{([\s\S]*?)\n\}/.exec(app);
  assert.ok(body, "app.js 必须定义 renderSetupGuide()");
  const code = body[1];
  assert.doesNotMatch(code, /\bglobal\./, "引导渲染里不得出现裸 global（历史缺陷）");
  assert.match(code, /try\s*\{/, "引导页渲染必须包在 try 里");
  assert.match(code, /catch\s*\(/, "必须有 catch：引导抛错不能变成白屏");
  assert.match(code, /renderOwoServiceError/, "catch 必须回落错误卡，给出可操作出口");
  assert.match(code, /if \(!window\.renderOwoSetupGuide\)/, "视图未加载时也必须回落，不得静默返回");
});

test("契约3：壳连接快照缺失时仍走引导判定（provider 分流不被跳过）", () => {
  const app = readFileSync(join(WEB_ROOT, "app.js"), "utf8");
  const body = /async function needsSetup\(\)\s*\{([\s\S]*?)\n\}/.exec(app);
  assert.ok(body, "app.js 必须定义 needsSetup()");
  const code = body[1];
  assert.match(code, /tauriInvokeOwner/, "非桌面环境必须直接判 false（不越权猜工作区）");
  assert.match(code, /state === "no_workspace"/, "no_workspace 是引导页第一入口");
  assert.match(code, /get_provider_status/, "provider 未配置必须由壳 IPC 判定后分流引导页");
  assert.doesNotMatch(code, /fetch\(|\/health/, "needsSetup 不得走 HTTP（§8.2 冷启预算：只允许 1 次 health）");
});
