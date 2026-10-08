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

test("Electron 桌宠请求必须使用当前核心 bearer token", async () => {
  const { readFileSync } = await import("node:fs");
  const { dirname, join } = await import("node:path");
  const { fileURLToPath } = await import("node:url");
  const mainPath = join(dirname(fileURLToPath(import.meta.url)), "../src/main/main.js");
  const source = readFileSync(mainPath, "utf8");
  assert.match(source, /function coreAuthorizationHeaders\(\)[\s\S]*?Bearer.*coreState\.token/);
  assert.match(source, /httpPost\(coreState\.port, "\/desktop\/pet", coreAuthorizationHeaders\(\)/);
  assert.match(source, /httpGet\(coreState\.port, "\/desktop\/pet", coreAuthorizationHeaders\(\)/);
});

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

test("模型默认输出上限按可支持范围拒绝超过 32k 的值", () => {
  const result = sup.validateConfig({
    version: 1,
    model: { provider: "bigmodel", max_output_tokens: 32001 },
  }, {});
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((item) => item.includes("max_output_tokens")));
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

// ---------- 日志写入容错（回归：EPIPE 崩主进程） ----------
//
// 这条契约来自一次真实崩溃：壳把核心日志裸写 process.stdout，stdout 对端消失
// （管道被 `head` 截断 / 重定向目标提前退出 / CI 里跑）时 write 抛 EPIPE，
// 而调用点在核心 'data' 回调里 → 未捕获异常掀翻 Electron 主进程，窗口直接消失。

test("safeLogWrite：写入正常时透传并报告成功", () => {
  const seen = [];
  const write = sup.safeLogWrite((text) => {
    seen.push(text);
  });
  assert.equal(write("[core] hello\n"), true);
  assert.equal(write("[core] world\n"), true);
  assert.deepEqual(seen, ["[core] hello\n", "[core] world\n"]);
});

test("safeLogWrite：EPIPE 等写入异常被吞掉，不向上抛", () => {
  // 复刻崩溃现场：write 同步抛 EPIPE。
  const boom = () => {
    const err = new Error("write EPIPE");
    err.code = "EPIPE";
    throw err;
  };
  const write = sup.safeLogWrite(boom);
  assert.doesNotThrow(() => write("[core] x\n"), "写入异常不得向上抛（否则主进程崩）");
  assert.equal(write("[core] x\n"), false, "应报告失败而不是抛错");
});

test("safeStreamLogWrite：异步 EPIPE 后停止写入且不崩溃", () => {
  let onError = null;
  const seen = [];
  const stream = {
    on(event, handler) {
      if (event === "error") onError = handler;
    },
    write(text) {
      seen.push(text);
    },
  };
  const write = sup.safeStreamLogWrite(stream);
  assert.equal(write("ready\n"), true);
  const error = new Error("broken pipe");
  error.code = "EPIPE";
  onError(error);
  assert.doesNotThrow(() => write("after-close\n"));
  assert.equal(write("after-close\n"), false);
  assert.deepEqual(seen, ["ready\n"]);
});

test("safeLogWrite：底层非函数 / 流已关闭时静默降级", () => {
  assert.doesNotThrow(() => sup.safeLogWrite(null)("x"), "写入器缺失应静默");
  assert.equal(sup.safeLogWrite(null)("x"), false);
  // stream 已 end() 后再 write 会抛 ERR_STREAM_WRITE_AFTER_END
  const closed = sup.safeLogWrite(() => {
    throw new Error("write after end");
  });
  assert.equal(closed("x"), false);
  // 反复调用不累积状态（自动重启会反复建流）
  const flaky = sup.safeLogWrite((text) => {
    if (text.includes("boom")) throw new Error("disk full");
  });
  for (let i = 0; i < 50; i += 1) flaky("ok\n");
  assert.equal(flaky("boom\n"), false);
  assert.equal(flaky("ok\n"), true, "单次失败后仍应继续可用");
});

// ---------- 桌面实例头（回归：instance_mismatch 403） ----------
//
// 这条契约来自一次真实故障：壳拉起核心后，主进程与渲染层取 token 全部 403
// （auth/instance_mismatch/not_retryable「桌面实例身份不匹配」），随后所有业务请求
// 401，界面表现为"服务未连接"+"列表加载失败：Failed to fetch"。
// 根因：ADR-003 S6/M13 移植时只搬了配对头 x-owo-desktop-pairing，漏了实例头
// x-owo-desktop-instance（服务端 auth_token.rs::DESKTOP_INSTANCE_HEADER）。
// 门控在 instance_gate_allows：注入了实例身份时，只允许同一实例的引导请求取 token。

test("实例头常量与服务端口头名一致", () => {
  // 服务端 auth_token.rs:41 `pub const DESKTOP_INSTANCE_HEADER: &str = "x-owo-desktop-instance"`
  assert.equal(sup.DESKTOP_INSTANCE_HEADER, "x-owo-desktop-instance");
});

test("实例头语义：身份已知才带，接管外部核心必须清空", () => {
  // 复刻 shellHeaders 的实例头决策：仅当本壳确实注入过 OWO_DESKTOP_INSTANCE_ID 才带。
  // 接管已存活核心（M5 adopt）时那个核心属于别的实例，带上必然被 instance_gate_allows 拒。
  const headerFor = (instanceId) => (instanceId ? { [sup.DESKTOP_INSTANCE_HEADER]: instanceId } : {});
  const own = headerFor("shell-abc");
  assert.equal(own[sup.DESKTOP_INSTANCE_HEADER], "shell-abc", "自己拉起的核心必须带头");
  assert.deepEqual(headerFor(""), {}, "接管外部核心时不得带头");
  assert.deepEqual(headerFor(null), {}, "身份未知时不得带头");
});
