import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const here = fileURLToPath(new URL(".", import.meta.url));
const app = readFileSync(join(here, "../app.js"), "utf8");
const index = readFileSync(join(here, "../index.html"), "utf8");

test("§12-14 首屏收敛：健康检查先行 + 4 个无兜底水合，总请求 ≤5", () => {
  assert.match(
    app,
    /async function hydrateShell\(\) \{\s*await refreshHealth\(\);\s*return window\.OwoRecovery\.runWithConcurrency\(BOOT_HYDRATE_TASKS, 5\);\s*\}/,
    "水合必须健康先行并以 ≤5 并发执行"
  );
  const bootTasks = app.match(/^\s*const BOOT_HYDRATE_TASKS = \[([\s\S]*?)\];/m);
  assert.ok(bootTasks, "BOOT_HYDRATE_TASKS 必须存在");
  for (const name of [
    "refreshSessions", "refreshSkills", "refreshWhitelist", "refreshProjectRules",
  ]) {
    assert.match(bootTasks[1], new RegExp(`\\b${name},`), `首屏水合必须保留 ${name}`);
  }
  // 其余面板刷新器必须被懒加载并全部由 REFRESH_PLANS 定时兜底，防止双遗漏。
  const lazyTasks = app.match(/^\s*const BOOT_LAZY_TASKS = \[([\s\S]*?)\];/m);
  assert.ok(lazyTasks, "BOOT_LAZY_TASKS 必须存在");
  const plans = app.match(/\{ refresh: \w+, intervalMs: \d+, routes: (?:"[^"]*"|\*) \}/g) || [];
  for (const name of [
    "refreshPlugins", "refreshPackages", "refreshSuggestions", "refreshAutomations",
    "refreshReminders", "refreshSettings", "refreshUsage", "refreshServerStatus",
    "refreshAudit", "refreshPerception", "refreshLearn", "refreshObservations",
    "refreshSkillHealth", "refreshMcp", "refreshTraces", "refreshComputerTasks",
  ]) {
    assert.match(lazyTasks[1], new RegExp(`\\b${name},`), `懒加载集合必须包含 ${name}`);
    assert.ok(
      plans.some((plan) => plan.includes(`refresh: ${name},`)),
      `懒加载刷新器 ${name} 必须由 REFRESH_PLANS 兜底`
    );
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

test("§3.1 事件流 Bearer 认证 + 域失效事件驱动 + 低频兜底 + 单一连接", () => {
  assert.match(index, /<script src="core\/events\.js"><\/script>/, "事件失效网络必须随壳加载");
  const eventsJs = readFileSync(join(here, "../core/events.js"), "utf8");
  assert.match(eventsJs, /function createDomainInvalidator\(options\)/, "必须提供域失效订阅器");
  assert.match(eventsJs, /version <= previous/, "旧版本事件必须被忽略（防重复刷新）");
  assert.match(eventsJs, /debounceMs/, "同域短窗口必须防抖合并");
  assert.match(eventsJs, /openStream/, "必须经带认证的 fetch-stream（openStream）订阅");
  assert.doesNotMatch(eventsJs, /new EventSource\(/, "禁止匿名 EventSource（无法携带 Authorization 头）");
  // §3.3 指标在状态机内部计数，业务 handler 不再累加。
  assert.match(eventsJs, /eventRefreshes/, "必须统计实际事件刷新数");
  assert.match(eventsJs, /coalescedInvalidations/, "必须统计防抖合并数");
  assert.match(eventsJs, /duplicateInvalidations/, "必须统计重复失效丢弃数");
  assert.match(eventsJs, /pollFallbackRefreshes/, "必须统计降级轮询次数");
  assert.match(eventsJs, /hiddenWindowRefreshes/, "必须统计隐藏窗口业务刷新（验收目标 0）");
  assert.doesNotMatch(app, /REQUEST_STATS\.duplicateRefreshes/, "业务 handler 不得再累加旧指标");
  // §3.4 生命周期：base 变化重建、连接断开停流、卸载清理、Degraded 单一兜底调度。
  assert.match(app, /function startInvalidation\(\)/, "必须提供事件失效启动入口");
  assert.match(app, /function stopInvalidation\(\)/, "必须提供事件失效停止入口");
  assert.match(app, /Object\.entries\(INVALIDATE_HANDLERS\)/, "启动时须逐一注册处理器");
  // R3（§8.2）：幂等键必须含实例身份——core 重启后端口可能被系统复用，只比 base
  // 会留下上一代进程的订阅器与 Last-Event-ID 游标（新事件版本号落在游标之下被丢弃）。
  assert.match(app, /invalidationKey === key/, "core 连接描述变化必须重建事件流");
  assert.match(app, /base \+ "#" \+ \(apiClient\.coreInstanceId \|\| ""\)/, "幂等键必须包含实例身份");
  assert.doesNotMatch(app, /invalidationBase === base/, "不得退回只比 base 的旧口径");
  assert.match(app, /beforeunload[\s\S]*?stopInvalidation\(\)/, "页面卸载必须停流");
  assert.match(app, /markConnectionUnavailable\(\);\s*stopInvalidation\(\);/, "core 断开必须停流");
  assert.match(app, /setPollFallback/, "必须注册 Degraded 单一兜底调度器");
  // R3（§8.2）真实桌面实测缺陷的结构性防回潮断言：
  // ① 订阅器只交路径给宿主（base 由 api-client 单点组装，杜绝双前缀）；
  // ② 进入 Degraded 不得立即冲刷全部领域（首屏零风暴）。
  assert.match(eventsJs, /openStream\(streamPath,/, "openStream 必须收到路径而非绝对 URL");
  assert.doesNotMatch(eventsJs, /openStream\(fullUrl/, "禁止把 baseUrl 预拼的绝对 URL 交给宿主");
  const degradeBlock = eventsJs.match(/function startPollFallback\(\)[\s\S]*?\n    \}/);
  assert.ok(degradeBlock, "必须存在 Degraded 兜底调度器");
  assert.doesNotMatch(degradeBlock[0], /\n\s*tick\(\);/, "兜底首 tick 必须等一个完整周期");
  // §3.1 api-client：带认证流式请求 + 401 单次刷新。
  const apiClient = readFileSync(join(here, "../core/api-client.js"), "utf8");
  assert.match(apiClient, /async openEventStream\(/, "api-client 必须提供带认证流式入口");
  assert.match(apiClient, /Last-Event-ID/, "续传必须走 Last-Event-ID 头");
  assert.match(apiClient, /token 不允许进入 URL|Authorization.*Bearer/s, "必须用 Authorization 头携带 token");
  // 轮询降频：被事件覆盖的 chat 域兜底间隔不短于 5 分钟，health 保持 30 秒心跳。
  const plans = app.match(/\{ refresh: (\w+), intervalMs: (\d+), routes: ([^}]+) \}/g) || [];
  const healthPlan = plans.find((p) => p.includes("refreshHealth"));
  assert.match(healthPlan, /intervalMs: 30000/, "health 保留 30 秒心跳");
  for (const domain of ["refreshLearn", "refreshPlugins", "refreshPackages", "refreshAutomations", "refreshMcp", "refreshTraces", "refreshComputerTasks"]) {
    const plan = plans.find((p) => p.includes(domain));
    assert.ok(plan, `${domain} 必须有兜底计划`);
    assert.match(plan, /intervalMs: 600000/, `${domain} 必须降为 10 分钟低频兜底`);
  }
});

test("单飞恢复：定时驱动与手动重试共用同一控制器", () => {
  assert.match(app, /window\.OwoRecovery\.createRecoveryController/, "恢复必须经单飞控制器");
  assert.match(app, /if \(activeRecovery\) return activeRecovery\.trigger\(\);/, "recover() 必须复用控制器合并触发");
  assert.match(app, /window\.owoRecoverService = \(\) => recovery\.trigger\(\);/, "外部重试入口必须合并到控制器");
  assert.match(app, /recovery\.trigger\(\)\.catch\(\(\) => \{\}\)\.then\(/, "定时驱动不得产生未处理拒绝");
  assert.match(app, /resetCoreConnection/, "恢复前必须重查核心连接（动态端口/实例可能变化）");
});

test("§8.2 桌面后台态：守卫不得只看 document.visibilityState", () => {
  const connection = readFileSync(join(here, "../shell/connection.js"), "utf8");
  // 壳注入入口 + 统一口径 + 三个守卫点全部改走 uiHidden()。
  assert.match(app, /window\.owoSetBackground = function \(hidden\)/, "必须暴露壳注入入口");
  assert.match(app, /shellBackgroundHidden = Boolean\(hidden\)/, "注入值必须归一为布尔");
  assert.match(app, /if \(running \|\| uiHidden\(\)\) return;/, "后台刷新守卫必须用 uiHidden()");
  assert.match(app, /invalidator\.setPollFallback\(\(\) => \{\s*if \(uiHidden\(\)\) return;/, "兜底轮询守卫必须用 uiHidden()");
  assert.match(
    app,
    /if \(!shellBackgroundHidden && invalidator[\s\S]{0,120}onVisibility\(\)/,
    "唤回后必须补刷隐藏期间攒下的域失效"
  );
  assert.match(connection, /uiHidden\(\)[\s\S]{0,80}document\.visibilityState === "hidden"/, "隐藏期请求计数与守卫同口径");
});

test("§8.2 第5条：运行期重握手后必须按新代际重建事件订阅与请求通道", () => {
  // 只在"启动失败"分支挂恢复控制器是不够的：core 在运行期被壳重启时端口与
  // bearer 同时换代，旧端口只会网络失败。客户端两层都得接上。
  const connection = readFileSync(join(here, "../shell/connection.js"), "utf8");
  const client = readFileSync(join(here, "../core/api-client.js"), "utf8");
  assert.match(
    connection,
    /owo:connection[\s\S]{0,520}startInvalidation\(\)/,
    "ready 事件必须重建失效订阅器（幂等键含实例身份，同代际早退）"
  );
  assert.match(
    client,
    /catch \(error\) \{[\s\S]{0,260}handleNetworkFailure\(allowRetry\)[\s\S]{0,160}return execute\(false\)/,
    "fetch 网络失败分支必须触发重查连接并重试一次"
  );
  assert.match(
    client,
    /handleNetworkFailure\(allowRetry\)[\s\S]{0,700}resetCoreConnection\(\)/,
    "重查必须整体失效注入 token 与缓存描述符（不能只清 this.token）"
  );
  assert.match(client, /REHANDSHAKE_COOLDOWN_MS/, "重查必须有冷却窗口（防风暴放大）");
});
