import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

const read = file => readFileSync(new URL("../" + file, import.meta.url), "utf8");

function load(name) {
  const source = read("app-domain.js").match(new RegExp(`function ${name}\\([^)]*\\) \\{[\\s\\S]*?\\n\\}`))[0];
  return runInNewContext(source + `\n${name};`, { String, Array, Set });
}

test("conversation titles strip request prefixes and keep 8-14 readable characters", () => {
  const derive = load("deriveConversationTitle");
  assert.equal(derive("帮我分析一下这个项目的整体结构"), "分析一下这个项目的整体结构");
  assert.equal(derive("请解释什么是向量数据库"), "解释什么是向量数据库");
  assert.equal(derive("  审查最近的改动，\n并给出风险点  "), "审查最近的改动");
  assert.equal(derive("你好，今天天气怎么样"), "你好，今天天气怎么样");
  assert.equal(derive(""), "");
  const long = derive("请帮我梳理一下这个仓库里所有和桌宠窗口相关的代码以及它们之间的调用关系");
  assert.ok(Array.from(long).length <= 14, long);
  assert.doesNotMatch(long, /^(帮我|请|麻烦|我想|想请你|能不能|可以|给我|帮忙)/);
});

test("sessions without a real title are treated as unnamed", () => {
  const hasAuto = load("owoSessionHasAutoTitle");
  const display = load("owoSessionDisplayTitle");
  assert.equal(hasAuto({ title: "" }), true);
  assert.equal(hasAuto({ title: "会话 4f9c1a" }), true);
  assert.equal(hasAuto({ id: "7d1c9e0a-1234", title: "7d1c9e0a" }), true);
  assert.equal(hasAuto({ title: "分析项目结构" }), false);
  assert.equal(display({ title: "" }), "新对话");
  assert.equal(display({ title: "会话 4f9c1a" }), "新对话");
  assert.equal(display({ title: "分析项目结构" }), "分析项目结构");
});
