// R3-B §3.4 终态收敛契约（"过程态不得被当成结论"的机器化版本）。
//
// 真机矩阵里 core-hang / data-dir-unwritable / provider-unset 三条反复红的共同根因
// 不是"产品没报错"，而是界面在 t≈10s 拿着一份 starting 快照就把卡面**定型**：
//   · 挂死场景 45s 才出稳定码 → 卡面永远停在默认三出口；
//   · 无密钥场景 core 以 provider/not_configured 退出 → needsSetup 只认 ready+status
//     一条路径，于是规定终态（模型配置引导）被渲染成通用错误卡。
// 本测试只读源码结构，不起浏览器：把"必须跟查到终态"钉成契约，防止有人再把
// 一次性渲染改回去。
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const WEB = join(dirname(fileURLToPath(import.meta.url)), "..");
const view = readFileSync(join(WEB, "views", "service-error.view.js"), "utf8");
const guide = readFileSync(join(WEB, "views", "setup-guide.view.js"), "utf8");
const app = readFileSync(join(WEB, "app.js"), "utf8");

test("错误卡必须跟查到稳定码：有界预算 + 只在壳里挂 + 换码即重绘且自终止", () => {
  assert.match(view, /function startTerminalFollowUp\(/, "必须有终态跟查");
  assert.match(view, /if \(!code && hasInvoke\) startTerminalFollowUp\(root, error, retry\);/,
    "只在「当前无码」且「运行在桌面壳」时挂跟查（浏览器直连不空转）");
  assert.match(view, /Date\.now\(\) \+ 45000/, "跟查预算必须对齐 §3.4 最长最终时限 45s");
  // 三条护栏：预算到期要停、被新卡取代要停、重绘后不得递归再挂。
  assert.match(view, /if \(Date\.now\(\) >= deadline\) return;/, "预算用尽必须停，不许永久轮询");
  assert.match(view, /seq !== terminalWatchSeq \|\| !root \|\| !root\.isConnected/, "卡被取代/离屏必须停");
  assert.match(view, /terminalWatchSeq = seq \+ 1;[\s\S]{0,160}renderOwoServiceError\(root, error, retry\)/,
    "拿到码后重绘本卡，并且认领序号使旧跟查失效");
  assert.match(view, /ensureCoreConnection\(\)/, "跟查走 api-client 单飞重查（非裸 setInterval 各查各的）");
});

test("跟查不得引入 HTTP：只允许壳 IPC 重查（§8.2 首屏请求预算另有时限约束）", () => {
  const watcher = /function startTerminalFollowUp\([\s\S]*?\n  \}/.exec(view);
  assert.ok(watcher, "跟查函数应存在");
  assert.doesNotMatch(watcher[0], /fetch\(|XMLHttpRequest|\.get\(|\.post\(/,
    "跟查里不得出现 HTTP 调用：终态码来自壳，不来自服务端轮询");
});

test("needsSetup 必须把 provider/not_configured 认成引导页（不是错误卡）", () => {
  const body = /async function needsSetup\(\)\s*\{([\s\S]*?)\n\}/.exec(app);
  assert.ok(body, "needsSetup 应存在");
  assert.match(body[1], /connection\.state === "no_workspace"/, "工作区缺失 → 引导");
  assert.match(body[1], /connection\.errorCode === "provider\/not_configured"/,
    "core 以 provider/not_configured 失败退出时，规定终态仍是模型配置引导");
  assert.match(body[1], /get_provider_status/, "core ready 时按壳的提供商就绪态分流");
});

test("引导页在未配置态必须自带稳定码（成功/失败两条回调路径都覆盖）", () => {
  assert.match(guide, /const shellCode = diagnostics[\s\S]*?provider\/not_configured/,
    "引导页要读壳带回来的稳定码");
  assert.match(guide, /if \(state\.ready && !providerUnset\)/,
    "壳已判未配置时，不得因 status.ready 竞态把页面渲染成就绪态");
  const notReady = /status\.innerHTML = text \+ '[\s\S]*?provider\/not_configured[\s\S]*?\n      \}/.exec(guide);
  assert.ok(notReady, "未就绪分支必须内联稳定码");
  assert.match(guide, /\.catch\(function \(\)\s*\{[\s\S]{0,400}provider\/not_configured/,
    "读状态失败也要给归因码，不得静默降级成一句提示");
});
