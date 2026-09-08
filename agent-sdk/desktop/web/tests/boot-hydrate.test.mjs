import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const app = readFileSync(join(here, "../app.js"), "utf8");
const index = readFileSync(join(here, "../index.html"), "utf8");

test("首屏水合收敛：健康检查先行，业务面板 ≤5 并发", () => {
  assert.match(
    app,
    /async function hydrateShell\(\) \{\s*await refreshHealth\(\);\s*return window\.OwoRecovery\.runWithConcurrency\(BOOT_HYDRATE_TASKS, 5\);\s*\}/,
    "水合必须健康先行并以 ≤5 并发执行"
  );
  const bootTasks = app.match(/^\s*const BOOT_HYDRATE_TASKS = \[([\s\S]*?)\];/m);
  assert.ok(bootTasks, "BOOT_HYDRATE_TASKS 必须存在");
  for (const name of [
    "refreshSessions", "refreshSkills", "refreshPlugins", "refreshPackages",
    "refreshSuggestions", "refreshAutomations", "refreshReminders", "refreshSettings",
    "refreshUsage", "refreshServerStatus", "refreshAudit", "refreshWhitelist",
    "refreshPerception", "refreshLearn", "refreshObservations", "refreshSkillHealth",
    "refreshProjectRules", "refreshMcp", "refreshTraces", "refreshComputerTasks",
  ]) {
    assert.match(bootTasks[1], new RegExp(`\\b${name},`), `首屏水合不得遗漏 ${name}`);
  }
  assert.match(index, /<script src="core\/recovery\.js"><\/script>/, "恢复/并发原语必须随壳加载");
});

test("后台刷新：防重入 + 页面隐藏暂停 + 路由可见性标注", () => {
  assert.match(app, /let running = false;.*?防重入/s, "刷新必须防重入，避免慢请求叠轮次");
  assert.match(app, /document\.visibilityState === "hidden"/, "页面隐藏时必须暂停后台刷新");
  assert.match(app, /function routeActive\(routes\)/, "刷新必须按路由可见性过滤");
  const plans = app.match(/\{ refresh: \w+, intervalMs: \d+, routes: (?:"[^"]*"|\*) \}/g) || [];
  assert.equal(plans.length, 17, "每个刷新计划都必须带可见性标注");
  assert.ok(plans.some((plan) => plan.includes('routes: "*"')), "健康检查必须全路由运行");
  assert.ok(plans.filter((plan) => plan.includes('"notChat"')).length === 3, "设置区三个刷新器只在路由页运行");
});

test("单飞恢复：定时驱动与手动重试共用同一控制器", () => {
  assert.match(app, /window\.OwoRecovery\.createRecoveryController/, "恢复必须经单飞控制器");
  assert.match(app, /if \(activeRecovery\) return activeRecovery\.trigger\(\);/, "recover() 必须复用控制器合并触发");
  assert.match(app, /window\.owoRecoverService = \(\) => recovery\.trigger\(\);/, "外部重试入口必须合并到控制器");
  assert.match(app, /recovery\.trigger\(\)\.catch\(\(\) => \{\}\)\.then\(/, "定时驱动不得产生未处理拒绝");
  assert.match(app, /resetCoreConnection/, "恢复前必须重查核心连接（动态端口/实例可能变化）");
});
