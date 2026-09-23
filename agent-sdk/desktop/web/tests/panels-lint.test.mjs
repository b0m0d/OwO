// §8.2 任务 7 尾：全前端 JS 面 lint（walks all classic scripts loaded by index.html）。
// 断言：语法可编译、UTF-8 无 BOM、无 eval/new Function、无 console.log、
//       无硬编码本机 URL（一律经 core/api-client.js）、*.panel.js 必含 OwoPanels 注册。
// 运行：node --test tests/panels-lint.test.mjs（或 node tests/panels-lint.test.mjs）。
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { test } from "node:test";

const here = dirname(fileURLToPath(import.meta.url));
const web = dirname(here);

const groups = [
  { dir: "core", pattern: (name) => name.endsWith(".js") },
  { dir: "shell", pattern: (name) => name.endsWith(".js") },
  { dir: "views", pattern: (name) => name.endsWith(".js") },
  { dir: "panels", pattern: (name) => name.endsWith(".js") },
  { dir: "panels/workswarm", pattern: (name) => name.endsWith(".js") },
  // §4.8 权限中心四分（domain/api/controller/render）与 panels 同等对待：
  // 新增前端目录必须显式登记进 lint 面，否则 index.html 挂了脚本也没人查。
  { dir: "permissions", pattern: (name) => name.endsWith(".js") },
  // R10：模型服务提供方预设（含本地 Ollama 端点）。登记进 lint 面是刻意的：
  // 这个文件是唯一允许出现"本机 URL 字面量"的地方，规则必须继续盯着它。
  { dir: "config", pattern: (name) => name.endsWith(".js") },
];

function listScripts() {
  const files = [];
  for (const group of groups) {
    const abs = join(web, group.dir);
    if (!existsSync(abs)) continue;
    for (const name of readdirSync(abs)) {
      if (group.pattern(name)) files.push({ rel: `${group.dir}/${name}`, abs: join(abs, name) });
    }
  }
  files.push({ rel: "app-domain.js", abs: join(web, "app-domain.js") });
  files.push({ rel: "app.js", abs: join(web, "app.js") });
  files.push({ rel: "index.html", abs: join(web, "index.html"), html: true });
  return files;
}

const scripts = listScripts();

test("walks the full frontend surface", () => {
  // 前端面完整性：面板数、shell、core、app.js 至少各 1（防止目录被误移后 lint 静默通过）。
  const panelCount = scripts.filter((f) => f.rel.startsWith("panels/")).length;
  if (panelCount < 15) throw new Error(`panels 数异常：${panelCount}（应 ≥15）`);
  for (const must of ["core/api-client.js", "shell/connection.js", "app.js"]) {
    if (!scripts.some((f) => f.rel === must)) throw new Error(`缺少 ${must}`);
  }
});

for (const file of scripts) {
  test(`lint ${file.rel}`, () => {
    const buf = readFileSync(file.abs);
    // UTF-8 无 BOM
    if (buf[0] === 0xef && buf[1] === 0xbb && buf[2] === 0xbf) {
      throw new Error("文件含 UTF-8 BOM（仓库规范禁止）");
    }
    const src = buf.toString("utf8");
    if (file.html) {
      // index.html：仅断言脚本引用都在 lint 覆盖面内（避免漏挂新脚本）。
      const refs = [...src.matchAll(/<script src="([^"]+)"><\/script>/g)].map((m) => m[1]);
      const covered = new Set(scripts.map((f) => f.rel));
      for (const ref of refs) {
        if (!covered.has(ref)) throw new Error(`index.html 引用的脚本未被 lint 覆盖：${ref}`);
      }
      return;
    }
    // 语法可编译（经典脚本语义；不执行）
    new vm.Script(src, { filename: file.rel });
    // 危险面
    if (/\beval\s*\(/.test(src)) throw new Error("禁止 eval(");
    if (/new\s+Function\s*\(/.test(src)) throw new Error("禁止 new Function(");
    if (/console\.log\s*\(/.test(src)) throw new Error("禁止 console.log（用 H/stderr 通道或移除）");
    // 硬编码本机 URL：仅放行 ① 仓库约定的服务端缺省端口回退（4096 主控 / 4098 面板代端口）、
    // ② api-client 的动态端口拼接（字面量以 "http://127.0.0.1:" 结尾，端口来自连接注入）；
    // 其余 localhost URL 视为错误。
    const localUrl = /https?:\/\/(127\.0\.0\.1|localhost)(:\d+)?/g;
    for (const match of src.matchAll(localUrl)) {
      const literal = match[0];
      if (/^https?:\/\/(127\.0\.0\.1|localhost):(4096|4098)$/.test(literal)) continue;
      const after = src.slice(match.index + literal.length, match.index + literal.length + 2);
      if (after.startsWith(":")) continue; // "http://127.0.0.1:" + port 动态拼接
      throw new Error(`硬编码本机 URL（仅允许缺省端口回退或动态端口拼接）：${literal}`);
    }
    // 面板契约：*.panel.js 必须注册 OwoPanels
    if (/panels\/.+\.panel\.js$/.test(file.rel) && !/OwoPanels\s*[\[.]/.test(src)) {
      throw new Error("面板未注册 OwoPanels（契约见 §8.2）");
    }
  });
}
