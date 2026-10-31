// 2026-10-31 桌宠 / 托盘 / 模型菜单口径契约。
//
// 用户要求：托盘只保留「显示桌面 / 退出」；桌宠置顶、可聊天、拖动不再"卡住"、
// 能从桌宠打开工作区；用户菜单里有桌宠开关。这些行为横跨 main / preload / pet / app
// 四处，行为测试之外再加一层接线守卫，避免下一次改 UI 时悄悄丢回来。
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const electron = join(here, "..", "src", "main");
const main = readFileSync(join(electron, "main.js"), "utf8");
const preload = readFileSync(join(electron, "preload.js"), "utf8");
const petFeed = readFileSync(join(electron, "pet-feed.js"), "utf8");
const web = join(here, "..", "..", "web");
const app = readFileSync(join(web, "app.js"), "utf8");
const pet = readFileSync(join(web, "pet", "pet.js"), "utf8");
const petHtml = readFileSync(join(web, "pet", "index.html"), "utf8");
const theme = readFileSync(join(web, "chatgpt-theme.css"), "utf8");
const refine = readFileSync(join(web, "codex-refine.css"), "utf8");
const petCss = readFileSync(join(web, "pet", "pet.css"), "utf8");

test("托盘菜单只剩显示桌面与退出", () => {
  const block = /function refreshTrayMenu\(\) \{[\s\S]*?\n\}/.exec(main)?.[0];
  assert.ok(block, "找不到 refreshTrayMenu");
  assert.match(block, /label: "显示桌面"/);
  assert.match(block, /label: "退出"/);
  assert.doesNotMatch(block, /显示 \/ 隐藏桌宠|打开配置目录|开机自启|label: stateText/);
  // 状态仍保留在 tooltip，不占菜单项。
  assert.match(block, /setToolTip\(`OwO Agent · \$\{stateText\}`\)/);
});

test("桌宠窗口置顶且不抢焦点", () => {
  const windows = [...main.matchAll(/alwaysOnTop: ?["']?([^,'"\n}]+)["']?/g)].map((m) => m[1].trim());
  assert.ok(windows.length >= 2, "主桌宠与角色桌宠都要显式声明置顶");
  assert.ok(windows.every((value) => value === "screen-saver"), `置顶档位必须一致：${windows.join(", ")}`);
  assert.match(main, /focusable: false/);
});

test("聊天期间窗口才可聚焦，关闭后交还键盘", () => {
  // 常驻不可聚焦是桌宠不抢键盘的前提；但聊天输入框必须拿到系统焦点才能打字。
  assert.match(petFeed, /pet:actor:chat-focus/);
  assert.match(petFeed, /w\.setFocusable\(true\)[\s\S]{0,80}w\.focus\(\)/);
  assert.match(petFeed, /w\.blur\(\);w\.setFocusable\(false\)/);
  assert.match(preload, /setChatFocus: open => ipcRenderer\.invoke\("pet:actor:chat-focus"/);
  assert.match(pet, /shellBridge\.setChatFocus\(open\)/);
  assert.match(petCss, /\.chat-panel header button \{[^}]*width: 28px/);
  assert.match(pet, /event\.key !== "Escape"/);
});

test("命中区内临时去掉不可激活样式，否则 Windows 会吞掉 mousedown", () => {
  // WS_EX_NOACTIVATE 下 Chromium 只收到 mouseup，单击/双击/点 × 都时灵时不灵。
  // 进入命中区必须临时可聚焦，但不能主动抢焦点；移出后交还键盘。
  const handler = /ipcMain\.handle\('pet:actor:hover',[^\n]*/.exec(petFeed)?.[0];
  assert.ok(handler, "找不到 pet:actor:hover");
  assert.match(handler, /w\.setFocusable\(true\)/);
  assert.doesNotMatch(handler, /w\.focus\(\)/, "悬停不得主动抢焦点");
  assert.match(handler, /if\(w\.isFocused\(\)\)w\.blur\(\)/);
  assert.match(preload, /setHover: inside => ipcRenderer\.invoke\("pet:actor:hover"/);
  assert.match(pet, /shellBridge\.setHover\(inside\)/);
  // 点击会激活窗口，互动一结束必须把键盘交还；聊天期间由主进程忽略。
  assert.match(petFeed, /pet:actor:blur/);
  assert.match(preload, /releaseFocus: \(\) => ipcRenderer\.invoke\("pet:actor:blur"\)/);
  assert.match(pet, /function releaseKeyboard\(\)/);
  assert.match(pet, /releaseKeyboard\(\)/);
});

test("桌宠打开工作台时带上当前工作区，并订阅聚焦事件", () => {
  assert.match(main, /ipcMain\.handle\("pet:workbench"[\s\S]{0,500}pet:focus-workspace/);
  assert.match(preload, /onPetFocusWorkspace/);
  assert.match(preload, /ipcRenderer\.on\("pet:focus-workspace"/);
  assert.match(app, /onPetFocusWorkspace/);
  assert.match(app, /openWorkspaceMenu\(\$\("composerProjectBtn"\)\)/);
});

test("用户菜单提供桌宠开关并复用现有切换逻辑", () => {
  assert.match(app, /label: "桌面桌宠"/);
  assert.match(app, /toggleDesktopPet\(\)/);
  // 只应有一份实现：用户菜单调用现有切换函数，而不是另写显隐逻辑。
  assert.equal([...app.matchAll(/function toggleDesktopPet\(\)/g)].length, 1);
});

test("桌宠聊天复用统一 API、流式回合和完成确认，不另起运行时", () => {
  assert.match(petHtml, /\/core\/turn-sse\.js/);
  assert.match(pet, /OwoTurnSse\.consumeResponse/);
  assert.match(pet, /client\.stream\("\/session\/"/);
  assert.match(pet, /api\("\/session", \{ method: "POST"/);
  assert.doesNotMatch(pet, /new Agent\(|createAgent\(|createSessionRuntime/);
});

test("拖动默认移动窗口，Shift 拖动才旋转 3D 形象", () => {
  const move = /pet\.addEventListener\("pointermove"[\s\S]*?\n\}\);/.exec(pet)?.[0];
  assert.ok(move);
  assert.match(move, /down\.shift/);
  assert.match(move, /moveBy/);
  // 默认分支必须是移动窗口，旋转只在 Shift 分支里。
  assert.match(move, /if \(down\.shift[\s\S]{0,200}rotate[\s\S]{0,300}else if[\s\S]{0,100}moveBy/);
});

test("模型菜单是紧凑单面板，思考强度就在同一面板里", () => {
  // 单面板：搜索 → 模型列表 → 思考强度 → 管理模型，不再有第二层飞出面板。
  assert.match(refine, /\.composer-menu\.composer-menu-model \{[^}]*width: min\(332px/);
  assert.match(refine, /\.composer-model-effort \{/);
  assert.match(app, /data-effort-segment/);
  assert.doesNotMatch(app, /data-model-effort-panel/);
  assert.doesNotMatch(app, /悬停或聚焦模型可调整思考强度/);
});

test("桌宠聊天带当前自定义模型连接，但拿不到密钥", () => {
  assert.match(preload, /chatConnection: model => ipcRenderer\.invoke\("pet:chat:connection"/);
  const handler = /ipcMain\.handle\("pet:chat:connection"[\s\S]*?\n\}\);/.exec(main)?.[0];
  assert.ok(handler);
  assert.match(handler, /api_key_env/);
  assert.doesNotMatch(handler, /sealed|secret\(/);
  assert.match(pet, /shellBridge\?\.chatConnection\?\.\(model\)/);
  assert.match(pet, /model_connection: connection/);
});

test("桌宠聊天面板占满小窗口，形象退场且标题栏可拖动", () => {
  assert.match(petCss, /\.chat-panel \{[^}]*inset: 6px/);
  assert.match(petCss, /body\.chatting #pet/);
  assert.match(pet, /document\.body\.classList\.toggle\("chatting", open\)/);
  assert.match(pet, /chatHeader\?\.addEventListener\("pointermove"/);
});

test("透明区域穿透且边缘定位排除模型周围的透明留白", () => {
  assert.match(main, /PET_CONTENT_INSETS/);
  assert.match(main, /area\.x - PET_CONTENT_INSETS\.left/);
  assert.match(main, /PET_SIZE\.width \+ PET_CONTENT_INSETS\.right/);
  assert.match(main, /setIgnoreMouseEvents\(true, \{ forward: true \}\)/);
  assert.match(preload, /pet:actor:ignore-mouse/);
  assert.match(petHtml, /id="pet-hitbox"/);
  assert.match(petCss, /\.pet-hitbox \{/);
  assert.match(pet, /document\.addEventListener\("mousemove", updateMousePassthrough\)/);
});

test("右键菜单由系统弹出且形象收进子菜单，不再使用窗口内 DOM 菜单", () => {
  assert.match(preload, /pet:actor:menu/);
  assert.match(pet, /label: "更换形象"/);
  assert.match(pet, /submenu: skinIndex\.skins\.map/);
  assert.doesNotMatch(petHtml, /id="skin-menu"/);
  assert.doesNotMatch(petCss, /\.skin-menu/);
});
