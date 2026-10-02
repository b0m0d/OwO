// core-supervision 契约测试（ADR-003 S2 / M10）。
// 对应 tauri 壳 core_supervisor.rs 内的同名断言，壳换成 JS 后这些契约必须继续成立：
//   * 握手行解析不得放过非法 port/pid；
//   * 实例身份 + API 版本双重校验（防连到别的核心）；
//   * 代际守卫：过期监管循环不得再重启；
//   * 配置校验只挡结构性错误，凭据缺失只告警。
import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const sup = require("../src/main/core-supervision.js");

test("解析标准 core_ready 行", () => {
  const value = sup.parseReadyLine(
    '{"event":"core_ready","pid":1234,"port":6071,"api_version":"0.7","build_id":"abc","instance_id":"a1b2"}'
  );
  assert.ok(value, "标准 ready 行应可解析");
  assert.equal(value.port, 6071);
  assert.equal(value.pid, 1234);
  assert.equal(value.instance_id, "a1b2");
});

test("ready 行容忍噪声并拒绝非法取值", () => {
  assert.equal(sup.parseReadyLine("tracing: log line"), null);
  assert.equal(sup.parseReadyLine('{"event":"other"}'), null);
  assert.equal(sup.parseReadyLine('{"event":"core_ready","pid":1,"port":0}'), null, "port=0 无效");
  assert.equal(sup.parseReadyLine('{"event":"core_ready","pid":1,"port":99999}'), null, "port 越界");
  assert.equal(sup.parseReadyLine('{"event":"core_ready","port":8080}'), null, "缺 pid");
  assert.equal(sup.parseReadyLine('not json with "event":"core_ready"'), null);
});

test("core_fatal 只接受 layer/name 形状的稳定码", () => {
  const value = sup.parseFatalLine(
    '{"event":"core_fatal","code":"storage/not_writable","message":"数据目录不可写"}'
  );
  assert.ok(value);
  assert.equal(value.code, "storage/not_writable");
  assert.equal(sup.parseFatalLine("tracing: 普通日志行"), null);
  assert.equal(sup.parseFatalLine('{"event":"core_ready","pid":1,"port":9}'), null);
  assert.equal(sup.parseFatalLine('{"event":"core_fatal","code":"broken","message":"x"}'), null, "非 layer/name");
  assert.equal(sup.parseFatalLine('{"event":"core_fatal","message":"无码"}'), null, "缺 code 不得静默通过");
});

test("健康检查：版本一致且身份一致才放行", () => {
  assert.equal(sup.evaluateHealth({ healthy: true, api_version: "0.7", instance_id: "i1" }, { instanceId: "i1" }).ok, true);
});

test("健康检查：拒绝旧版本核心", () => {
  const verdict = sup.evaluateHealth({ healthy: true, api_version: "0.6", instance_id: "i1" }, { instanceId: "i1" });
  assert.equal(verdict.ok, false);
  assert.equal(verdict.reason, "version_mismatch");
  assert.match(sup.healthReasonText(verdict.reason, { apiVersion: "0.7" }), /不兼容/);
});

test("健康检查：拒绝未 healthy 与启动中", () => {
  assert.equal(sup.evaluateHealth({ healthy: false, api_version: "0.7" }, {}).reason, "unhealthy");
  assert.equal(sup.evaluateHealth({ healthy: true, api_version: "0.7", stage: "booting" }, {}).reason, "starting");
  assert.equal(sup.evaluateHealth(null, {}).reason, "no_response");
});

test("健康检查：端口被别的核心占用时按实例身份拒绝", () => {
  const verdict = sup.evaluateHealth(
    { healthy: true, api_version: "0.7", instance_id: "other-instance" },
    { instanceId: "mine" }
  );
  assert.equal(verdict.ok, false);
  assert.equal(verdict.reason, "instance_mismatch");
  assert.match(sup.healthReasonText(verdict.reason), /另一个核心实例/);
});

test("退避序列有上界", () => {
  assert.equal(sup.pollDelayMs(0), 100);
  assert.equal(sup.pollDelayMs(99), 800, "轮询延迟封顶 800ms");
  assert.equal(sup.nextBackoffMs(0), 500);
  assert.equal(sup.nextBackoffMs(99), 4000, "重启退避封顶 4s");
});

test("代际守卫：过期那一轮不得再重启", () => {
  const base = { attempts: 0, maxAttempts: 3, shuttingDown: false, userInitiated: false };
  assert.equal(sup.shouldAutoRestart({ ...base, generation: 1, currentGeneration: 1 }), true);
  assert.equal(sup.shouldAutoRestart({ ...base, generation: 1, currentGeneration: 2 }), false, "代际已过期");
  assert.equal(sup.shouldAutoRestart({ ...base, generation: 1, currentGeneration: 1, attempts: 3 }), false, "超过重试上限");
  assert.equal(sup.shouldAutoRestart({ ...base, generation: 1, currentGeneration: 1, shuttingDown: true }), false, "退出中");
});

test("用户主动重启不计入自动重试预算", () => {
  assert.equal(sup.shouldAutoRestart({ generation: 1, currentGeneration: 1, attempts: 0, maxAttempts: 3, userInitiated: true }), false);
});

test("发现文件端口取值：防御式且拒绝非法", () => {
  assert.equal(sup.discoveryPort({ port: 4096 }), 4096);
  assert.equal(sup.discoveryPort({ http_port: 4097 }), 4097);
  assert.equal(sup.discoveryPort({}), null);
  assert.equal(sup.discoveryPort(null), null);
  assert.equal(sup.discoveryPort({ port: 70000 }), null);
  assert.equal(sup.discoveryPort({ port: "abc" }), null);
});

test("provider 别名归一与本地判定", () => {
  assert.equal(sup.normalizeProvider("glm"), "bigmodel");
  assert.equal(sup.normalizeProvider("qwen"), "dashscope");
  assert.equal(sup.normalizeProvider("openai-compatible"), "custom");
  assert.equal(sup.normalizeProvider("unset"), "unset");
  assert.equal(sup.normalizeProvider("火星"), null);
  assert.equal(sup.isLocalProvider("ollama"), true);
  assert.equal(sup.isLocalProvider("openai"), false);
});

test("配置校验：结构性错误被拦下", () => {
  assert.equal(sup.validateConfig({}).ok, false);
  assert.equal(sup.validateConfig({ version: 1, model: {} }).errors.length > 0, true, "缺 provider");
  const bad = sup.validateConfig({ version: 1, model: { provider: "openai", temperature: 9 } });
  assert.equal(bad.ok, false);
  assert.match(bad.errors.join(";"), /temperature/);
  const range = sup.validateConfig({ version: 1, model: { provider: "openai", context_window: -1 } });
  assert.equal(range.ok, false);
});

test("配置校验：合法配置放行，缺凭据只告警不阻断", () => {
  const ok = sup.validateConfig({
    version: 1,
    model: { provider: "bigmodel", name: "glm-5.3-flash", temperature: 0.7 },
  });
  assert.equal(ok.ok, true, `合法配置应放行，实际错误：${ok.errors.join(";")}`);
  // 凭据缺失：只告警、不阻断，且必须由注入的环境探测来判定（纯函数不读 process.env）
  const warn = sup.validateConfig({ version: 1, model: { provider: "openai" } }, { envHas: () => false });
  assert.equal(warn.ok, true, "缺凭据不得阻断保存");
  assert.ok(warn.warnings.length > 0, "环境变量确实没有时应告警");
  assert.match(warn.warnings.join(";"), /OPENAI_API_KEY/);
  const quiet = sup.validateConfig({ version: 1, model: { provider: "openai" } }, { envHas: () => true });
  assert.equal(quiet.warnings.length, 0, "环境变量存在时不得误报");
});
