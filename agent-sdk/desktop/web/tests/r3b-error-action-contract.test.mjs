import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// R3-B（指南 §3.4/§4.7 + §6.9）：错误码 → 恢复动作 的**契约单一来源**测试。
//
// 为什么必须在 node 层再钉一遍：§3.4 的八类故障终态由真机故障矩阵断言（L4），
// 但矩阵只能证明"这一台机器这一次的界面长这样"。指南 §7.3 要求每次假绿都补
// 负例；本文件把"错误码到动作"的映射冻结成不依赖桌面会话的 L2 契约：
//   1) UI 按稳定 code 渲染动作（不匹配中文文案判型）；
//   2) 八个契约码的动作集合逐项锁定；
//   3) 验收矩阵脚本里的 RequiredActions 必须与 UI 实际动作**同源**——
//      任一侧改文案而另一侧没改，这里就红（历史上正是这种漂移造成假通过）。
const here = fileURLToPath(new URL(".", import.meta.url));
const read = (relative) => readFileSync(join(here, relative), "utf8");

// 从视图源码里取出对象字面量并求值（只读数据表，不执行渲染路径）。
function readConstObject(source, name, fileLabel) {
  const start = source.indexOf(`const ${name} = {`);
  const startA = source.indexOf(`const ${name} = [`);
  const at = start >= 0 ? start : startA;
  assert.ok(at >= 0, `${fileLabel} 必须定义 ${name}（§3.4 契约的唯一事实源）`);
  const openChar = source[at + `const ${name} = `.length];
  const closeChar = openChar === "{" ? "}" : "]";
  let depth = 0;
  let end = -1;
  for (let i = at; i < source.length; i += 1) {
    if (source[i] === openChar) depth += 1;
    else if (source[i] === closeChar) {
      depth -= 1;
      if (depth === 0) { end = i + 1; break; }
    }
  }
  assert.ok(end > at, `${fileLabel} 的 ${name} 字面量必须闭合`);
  const literal = source.slice(at + `const ${name} = `.length, end);
  // 视图里 label 用了拼接与 esc，数据表本身是纯字面量——只接受字面量，不跑任意代码。
  assert.ok(!/function|require\(|import\(/.test(literal), `${fileLabel} 的 ${name} 只允许纯字面量`);
  // eslint-disable-next-line no-new-func
  return new Function(`return (${literal});`)();
}

const viewSource = read("../views/service-error.view.js");
const guideSource = read("../views/setup-guide.view.js");
const matrixSource = read("../../../scripts/verify-desktop-failure-matrix.ps1");

const presets = readConstObject(viewSource, "ERROR_ACTION_PRESETS", "service-error.view.js");
const defaultActions = readConstObject(viewSource, "DEFAULT_ACTIONS", "service-error.view.js");

// §3.4 契约表：错误码 → 必须提供的动作（稳定 ID + 用户可见文案）。
const CONTRACT = {
  "core/binary_missing": ["recheck_core", "open_diagnostics_location"],
  "core/handshake_timeout": ["terminate_retry"],
  "core/exited": ["retry_core", "open_diagnostics"],
  "core/identity_mismatch": ["reinstall_component"],
  "storage/not_writable": ["choose_data_directory", "retry_core"],
  "provider/not_configured": ["open_provider_settings", "test_connection"],
  "workspace/required": ["choose_workspace"],
  "core/spawn_failed": ["retry_core", "open_diagnostics"],
};

test("§3.4：错误卡按稳定码渲染契约动作（八个码逐项锁定，不多不少）", () => {
  for (const [code, ids] of Object.entries(CONTRACT)) {
    assert.ok(presets[code], `缺少错误码动作预设：${code}`);
    const actual = presets[code].map((a) => a.id);
    assert.deepEqual(actual, ids, `${code} 的动作 ID 漂移：期望 ${ids.join("/")} 实际 ${actual.join("/")}`);
    for (const action of presets[code]) {
      assert.ok(action.label && action.label.length > 0, `${code} 的动作 ${action.id} 必须有用户可见文案`);
      assert.ok(action.role, `${code} 的动作 ${action.id} 必须有 role（决定点击后调哪个出口）`);
    }
  }
});

test("§4.7：动作 ID 只用稳定集合，不得混入中文判型", () => {
  const allowed = new Set([
    "retry_core", "terminate_retry", "recheck_core", "open_diagnostics",
    "open_diagnostics_location", "open_provider_settings", "test_connection",
    "choose_workspace", "choose_data_directory", "reinstall_component",
  ]);
  for (const [code, list] of Object.entries(presets)) {
    for (const action of list) {
      assert.ok(allowed.has(action.id), `${code} 使用了未登记的 action id：${action.id}`);
    }
  }
  // 判型必须来自 code（不是文案 contains）；同时保留无码降级出口。
  assert.match(viewSource, /d\.errorCode/, "错误码来源必须是壳诊断的 errorCode");
  assert.match(viewSource, /ERROR_ACTION_PRESETS\[code\] \|\| DEFAULT_ACTIONS/, "未知码必须降级到默认三出口，不得空转");
  assert.ok(defaultActions.length >= 3, "默认动作出口不得为空（旧壳/未知故障仍可自救）");
});

test("§3.4：每个动作都真的接到出口（retry/logs/settings/data-dir/test/workspace）", () => {
  const roles = new Set();
  for (const list of Object.values(presets)) for (const a of list) roles.add(a.role);
  for (const role of ["retry", "logs", "settings", "data-dir", "test", "workspace"]) {
    assert.ok(roles.has(role), `契约里存在 role=${role} 但视图未提供处理分支`);
    assert.ok(viewSource.includes(`"${role}"`), `视图必须显式处理 role=${role}`);
  }
  // 终止并重试必须落到壳命令（否则"挂死的 core"杀不掉，留孤儿进程）。
  assert.match(viewSource, /shellInvoke\("retry_core_start"\)/, "retry 动作必须经壳 retry_core_start");
  assert.match(viewSource, /shellInvoke\("choose_data_directory"\)/, "存储错误必须能改选数据目录");
  assert.match(viewSource, /\/settings\/provider-test/, "测试连接必须走核心侧自诊断端点（不在前端猜）");
});

test("§3.4：引导页呈现 provider/not_configured 与两个契约动作", () => {
  assert.ok(guideSource.includes("provider/not_configured"), "引导页必须呈现稳定码（矩阵按码断言，不按中文）");
  assert.ok(guideSource.includes('data-action="open_provider_settings"'), "引导页缺少动作 open_provider_settings");
  assert.ok(guideSource.includes('data-action="test_connection"'), "引导页缺少动作 test_connection");
  // §4.4 表单规范：provider 选择必须是约束控件（radio），不得要求手输内部值。
  assert.match(guideSource, /name="provider-mode"/, "提供商选择必须是单选控件");
});

// 解析验收脚本的场景表（PowerShell 里写的就是"真机期望"），核对与 UI 同源。
// 场景块以 `},` 分隔（PowerShell 数组字面量），因此收口符是 `}` 本身。
function parseMatrixScenarios(text) {
  const scenarios = [];
  const blockRe = /\[pscustomobject\]@\{ Id = '([^']+)';([\s\S]*?)\}/g;
  let m;
  while ((m = blockRe.exec(text)) !== null) {
    const body = m[2];
    const list = (name) => {
      const mm = body.match(new RegExp(`${name} = @\\(([^)]*)\\)`));
      if (!mm) return [];
      return [...mm[1].matchAll(/'([^']+)'/g)].map((x) => x[1]);
    };
    scenarios.push({
      id: m[1],
      codes: list("ExpectedCodes"),
      actions: list("RequiredActions"),
      expect: (body.match(/Expect = '([^']+)'/) || [])[1] || "",
      budget: Number((body.match(/FinalBudgetSec = (\d+)/) || [])[1] || 0),
    });
  }
  return scenarios;
}

test("§6.9 单一事实源：故障矩阵的契约动作必须与 UI 实际动作同源（文案漂移即红）", () => {
  const scenarios = parseMatrixScenarios(matrixSource);
  assert.equal(scenarios.length, 8, `§3.4 要求八个场景，脚本里有 ${scenarios.length} 个`);
  for (const sc of scenarios) {
    assert.ok(sc.budget > 0, `${sc.id} 必须有最终态时限（§3.4 两层时限）`);
    if (sc.expect !== "error" || sc.codes.length === 0) continue;
    const code = sc.codes[0];
    const preset = presets[code];
    assert.ok(preset, `${sc.id} 期望错误码 ${code}，但 UI 没有该码的动作预设`);
    const labels = preset.map((a) => a.label);
    for (const needle of sc.actions) {
      assert.ok(
        labels.some((label) => label.includes(needle)),
        `矩阵要求动作「${needle}」但 ${code} 的 UI 动作是 ${labels.join("/")}——两侧文案已漂移`
      );
    }
  }
});

test("§3.4 时限：两层时限拆分后不得退回单一 45s 大帽（R3-BUG-06）", () => {
  const scenarios = parseMatrixScenarios(matrixSource);
  const byCode = {};
  for (const sc of scenarios) byCode[sc.id] = sc.budget;
  assert.equal(byCode["binary-missing"], 15, "缺失场景最终态时限必须是 15s");
  assert.equal(byCode["core-exit"], 15, "早退场景必须是 15s（不得烧成握手超时）");
  assert.equal(byCode["identity-mismatch"], 20, "身份不匹配必须是 20s");
  assert.equal(byCode["core-hang"], 45, "挂死必须是 45s");
  assert.equal(byCode["no-workspace"], 20, "业务引导必须是 20s");
  assert.equal(byCode["provider-unset"], 20, "provider 引导必须是 20s（不得再冒充 45s 超时）");
  assert.equal(byCode["data-dir-unwritable"], 20, "存储错误必须是 20s");
  assert.match(matrixSource, /10s 内界面可操作/, "必须保留 ≤10s 可操作门（第一层时限）");
});

test("§3.4 归因：provider 未配置与数据目录不可写不得落在 core/* 超时码上（R3-BUG-05）", () => {
  const scenarios = parseMatrixScenarios(matrixSource);
  const provider = scenarios.find((s) => s.id === "provider-unset");
  assert.deepEqual(provider.codes, ["provider/not_configured"], "provider 未配置必须归 provider 层");
  assert.equal(provider.expect, "guide", "provider 未配置的终态是配置引导，不是错误卡");
  const storage = scenarios.find((s) => s.id === "data-dir-unwritable");
  assert.deepEqual(storage.codes, ["storage/not_writable"], "数据目录不可写必须归 storage 层");
  const workspace = scenarios.find((s) => s.id === "no-workspace");
  assert.deepEqual(workspace.codes, ["workspace/required"], "未选工作区必须归 workspace 层");
});
