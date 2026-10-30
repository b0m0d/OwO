import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const read = (relative) => readFileSync(join(here, relative), "utf8");

test("Electron 注入 bearer 后立即放开桌面动作门", () => {
  const api = read("../core/api-client.js");
  assert.match(
    api,
    /this\.injectedToken = connection\.token;[\s\S]*this\.token = connection\.token;[\s\S]*this\.emit\(true, \{ reason: "token" \}\)/,
    "壳注入 token 也必须发出已授权事件，否则 data-core-action 会一直 disabled"
  );
});

test("桌面快捷键包含 Ctrl+L 聚焦输入框", () => {
  const app = read("../app.js");
  assert.match(app, /key === "l"[\s\S]*\$\("prompt"\)\.focus\(\)/);
});

test("桌宠清单中的每个模型资源都实际存在", () => {
  const index = JSON.parse(read("../pet/assets/skins/index.json"));
  for (const skin of index.skins || []) {
    if (skin.kind !== "gltf") continue;
    assert.ok(skin.model, `${skin.id} 缺少模型文件名`);
    const modelPath = join(here, "../pet/assets/skins", skin.id, skin.model);
    assert.ok(existsSync(modelPath), `${skin.id} 引用了不存在的 ${skin.model}`);
  }
});
