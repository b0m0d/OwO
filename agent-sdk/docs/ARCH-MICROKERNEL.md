# Agent SDK 微内核化重构：边界、顺序与证据

> 施工依据：`builGoal/Agent-SDK-后续任务实施指南-2026-09-18.md` §2（目标架构）、§3（迁移映射）、
> §4（目标仓库结构）、§9（分阶段计划）、§10（Rust 资源红线）。
> 本文记录**每一步微内核拆分**的边界、验收与可复现证据；事实优先级同指南：
> 当前源码与真实运行证据 > 本文件 > 执行记录 > 历史规划。
> 分支：`agent-frame`。

---

## 0. 拆分的判据（为什么这样拆，而不是"看见大文件就切"）

指南 §0.1 的结论是：不做"每个模块一个微服务"，而是

> **一个权威 Agent Daemon + 一个受信工具宿主 + 按需原生 Worker + 多个薄客户端 + 进程内插件系统。**

而 §2.4 又列出**绝对不能拆散的事务边界**（消息顺序、turn 终态、权限判定与执行与审计、
写前快照/写入/diff/revert、会话库迁移、壳对子进程的生命周期）。所以拆分必须按
**事务边界**而不是按文件大小或命名。

一个候选边界只有同时满足下列条件，才允许成为微内核：

1. **自包含**：候选内的模块 `crate::` 引用**不逃出候选集合**（逃出即需先搬运或做依赖倒置）；
2. **入边可 re-export 满足**：外部模块引用候选内部条目时，只要候选被旧 crate
   重新 `pub use`，调用方**零改动**；
3. **无环**：候选借用的外部模块不得反过来引用候选（双向边 = 真环，必须拆到无环或用
   内核 trait 倒置）；
4. **可独立验收**：拆分后候选 crate 能自己编译、自己跑测试，且整个 workspace 仍能
   编译与运行。

**核心手法（本轮确立，后续每步复用）**：把文件 `git mv` 到新 crate，并在旧 crate 的
`lib.rs` 保留**同名别名模块 + 顶层 `pub use`**：

```rust
// crates/owo-agent-core/src/lib.rs
pub use owo_agent_kernel::{
    audit, capability, cas_store, credentials, deadline, error, injection, lease, platform,
    storage_crypto, whitelist,
};
```

这样两类既有路径同时继续有效：

* crate 内相对路径 `crate::audit::AuditLog`（拆分前 25 个 core 源文件使用）；
* 外部路径 `owo_agent_core::audit::AuditLog`、`owo_agent_core::AgentError`
  （`owo-agent-server`、`owo-agent-cli`、36 个 core 集成测试使用）。

于是"每拆一个微内核后仍能完整运行"不是靠多跑一遍全量测试撞运气，而是**由类型系统保证**：
调用方一行没改，编译器立刻能证明接口面没变。

---

## 1. 拆分顺序（按顺序执行，每步必须独立验收）

顺序依据两个量：**出边数**（候选必须借用外部多少模块，需要先搬运或倒置）与
**入边数**（外部有多少模块依赖候选，可用 re-export 免费满足）。入边多而出边少 =
最安全的先拆。

| 步骤 | 微内核 crate | 内容 | 出边 | 入边 | 状态 |
|---|---|---|---:|---:|---|
| **M0** | `owo-agent-kernel` | error/platform/capability/audit/credentials/cas_store/storage_crypto/whitelist/injection/lease/deadline（11 模块 6,604 行）+ 并入 `tool_args` | **0** | 25 个 core 源文件 | ✅ 已完成 |
| **M1** | `devtools/product-eval`（独立 workspace）+ `owo-agent-eval-facade`（门面） | product_eval / eval / dataset_builder / product_eval_workswarm + 6 个集成测试（约 8.6k 行） | 7（必须依赖 core） | **0** | ✅ 已完成，见 §3 |
| **M2** | `owo-agent-extensions` | notes / automation / change_set / change_set_store / cloud_exec（4,493 行） | **0** | **0** | ✅ 已完成，见 §5 |
| **M3** | `owo-agent-tool-safety` | sandbox + audit_chain（2,694 行） | **0** | `mcp` / `plugin` / `tools` + 4 个集成测试 | ✅ 已完成，见 §6 |
| **M4** | 下一个候选见 §7（SCC 数据已给出唯一可切方向） | — | — | — | 待执行 |

### 实测耦合数据（用于选序，不是估计）

`scratch-eval-runs/dep-graph-clean.json` 由注释/字符串剥离后的 `crate::` 扫描生成，
下表是四个候选边界的**直接**边（不含传递闭包，传递闭包会把 91 模块连成一片，没有决策价值）：

| 候选边界 | 模块数 | 出边 | 入边 | 说明 |
|---|---:|---|---|---|
| `owo-agent-kernel` | 11 | **无** | 25 文件 | 唯一零出边的成规模集合 → 第一步 |
| Perception（OCR/STT/UIA/ONNX） | 15 | 5：`computer_use`、`learn`、`memory`、`settings`、`transition` | 7 文件 | 出边全为**双向**（真环）；且 `computer_use` 反向引用 perception 达 30 处 |
| Policy/Executor（Tool Host 内核） | 8 | 8：`accessibility`、`audit_chain`、`learn`、`locate`、`mcp`、`ocr`、`scene`、`tools` | 16 文件 | 入边最多（指南 §13 要求"Tool Host 权限不可绕过"）；8 条出边中 6 条双向 |
| WorkSwarm（团队编排） | 18 | 15 | 6 文件 | 出边最多；指南 §9 定档为"暂停新增、可选加载" |
| ProductEval（开发工具） | 3 | 7 | **0 文件** | 唯一零入边候选；`eval`/`dataset_builder` 反向被 `workflow`/`action_program` 引用 |
| Fleet（远程节点/云执行） | 5 | 4 | 6 文件 | 与 WorkSwarm 互相引用（`goal`↔`fleet`） |

结论：**M0 之后不存在"下一个零出边大边界"**。候选取舍见 §6 的取舍记录，原则是
"先把编译器与启动链的痛点搬走，且优先选择能用 re-export 满足入边的一侧"。

---

## 2. M0：`owo-agent-kernel`（已完成）

### 2.1 边界

**放进来**（被多个运行边界共用、且不绑定业务状态机的稳定原语）：

| 模块 | 行数 | 归属理由 |
|---|---:|---|
| `platform.rs` | 720 | 前台应用/窗口枚举/截图/剪贴板，各运行边界的公共底座 |
| `storage_crypto.rs` | 888 | DPAPI/DEK/AEAD 信封，Daemon 与 Tool Host 都要用 |
| `credentials.rs` | 599 | 凭据解析与 Windows 凭据管理器封装 |
| `capability.rs` | 561 | 能力卡与路由判定（能力协商原语，非 Agent 业务） |
| `deadline.rs` | 258 | 阶段预算与转/轮次 waterfall（可观测性与多 Agent 共用） |
| `lease.rs` | 270 | 租约管理器 |
| `whitelist.rs` | 222 | 应用分层白名单 |
| `injection.rs` | 195 | 提示注入净化（跨边界安全工具函数） |
| `cas_store.rs` | 174 | 内容寻址存储（快照/diff/revert 的底座） |
| `audit.rs` | 37 | 审计条目（§2.4 第 3 条事务边界的一半） |
| `error.rs` | 17 | 共享错误类型 |

**明确不放进来**：Agent loop / Provider 网关 / 工具注册表 / 会话状态机（留在 core，
按 §9 A2/A4/A5 继续外迁）；OCR/STT/ONNX/Sherpa（未来 Perception Worker）；任何 HTTP 路由。

依赖方向固定为 `owo-agent-kernel ← owo-agent-core ← owo-agent-server ← owo-agent-cli`。

### 2.2 拆分中真实修好的三个缺陷（不是顺手改，是拆分暴露出来的）

1. **`owo-agent-kernel` 缺 `Win32_UI_Input_KeyboardAndMouse` feature**
   —— `platform::activate_window` 使用 `keybd_event`/`VK_MENU`。旧 core 的
   `windows-sys` 开了该 feature，所以 core 内联编译时看不出来；一旦独立成 crate 就
   报 `E0432`。**这正是"独立 crate 才能暴露的隐式耦合"**：原 manifest 里的 feature
   列表实际是为整个 core 服务的，谁真用它并不清楚。已在 kernel manifest 显式声明，
   并把"不得引入 `Media_Ocr`/`UI_Accessibility`/`Graphics_Imaging`"写成注释约束。

2. **集成测试用 `#[path = "../src/storage_crypto.rs"]` 复制编译源文件**
   —— `crates/owo-agent-core/tests/os_sandbox_integration_tests.rs` 原先把
   `storage_crypto.rs` 再编译一份做 DPAPI 冒烟。文件搬走后该路径失效；即使不搬走，
   它验证的也**不是真正链接进二进制的那份实现**。已改为
   `use owo_agent_kernel::storage_crypto;` + `::*`，从此验证真实产物。

3. **`scripts/ci-shared.ps1` 会掩盖 cargo 的真实退出码**（红线 10 的漏洞）
   —— `Invoke-CiCargo` 靠 `$global:LASTEXITCODE` 收口，但 `$LASTEXITCODE` 只在原生
   命令结束时由 PowerShell 自动写入，经函数调用后丢失；再叠加
   `Assert-CiBuildIdle`/`New-CiFailureState` 的布尔返回值混进输出流，调用方会拿到
   `@($true, 101)` 并退化成 exit 1。实测表现：`cargo check` 因缺 feature 失败（101），
   脚本却报 `exit=1` 且没有任何理由，看起来像"门禁说不出为什么红"。
   已修：新增 `-PassThru` 返回**纯标量**退出码；`$null = Assert-CiBuildIdle`、
   `$null = New-CiFailureState` 丢弃泄漏值。负例已验证（见 §2.4）。

### 2.3 改动文件

| 文件 | 变更 |
|---|---|
| `Cargo.toml`（workspace） | 新增成员 `crates/owo-agent-kernel`、新增 workspace 依赖 |
| `Cargo.lock` | 新增 `owo-agent-kernel` 包与其依赖闭包（无新增外部 crate） |
| `crates/owo-agent-kernel/Cargo.toml`、`src/lib.rs` | 新 crate 清单与边界文档 |
| `crates/owo-agent-core/src/{audit,capability,cas_store,credentials,deadline,error,injection,lease,platform,storage_crypto,whitelist}.rs` | `git mv` 到 kernel（git 识别为 R 重命名，历史保留） |
| `crates/owo-agent-core/Cargo.toml` | 新增 `owo-agent-kernel.workspace = true` |
| `crates/owo-agent-core/src/lib.rs` | 删除 11 个 `pub mod`，加内核兼容层别名 + 注释 |
| `crates/owo-agent-core/tests/os_sandbox_integration_tests.rs` | 改引真实 crate，弃用 `#[path]` |
| `scripts/ci-shared.ps1` | `-PassThru` 真实退出码修复（红线 10） |
| `scripts/mk-check.ps1` | 新增：本重构专用的 `--workspace --all-targets` 门禁入口 |
| `scripts/mk-smoke.ps1` | 新增：运行态验收（真实进程 + 真实 HTTP + 真实落盘 + 无孤儿进程） |
| `docs/ARCH-MICROKERNEL.md` | 新增：本文（边界、顺序、证据、复用清单） |

**调用方零改动**：`owo-agent-server`（101 文件）、`owo-agent-cli`（21 文件）、
36 个 core 集成测试、Tauri 壳均未修改。

### 2.4 验收证据（可复现）

| 验收项 | 命令 | 结果 | 证据 |
|---|---|---|---|
| **拆分前基线**：workspace 全目标编译 | `scripts/mk-check.ps1 -Tag baseline` | **exit=0**，189 s | `docs/qa/logs/mk-baseline-*.log` |
| 拆分后 workspace 全目标编译 | `scripts/mk-check.ps1 -Tag m0-kernel2` | **exit=0**，52 s | `docs/qa/logs/mk-m0-kernel2-*.log` |
| 内核 crate 自测（独立编译 + 36 单测） | `cargo test -p owo-agent-kernel --locked` | **36 passed / 0 failed** | `docs/qa/logs/mk-kernel-tests-*.log` |
| 内核依赖闭包**不含**重原生依赖与上层 crate | `cargo tree -p owo-agent-kernel --edges normal` | `ort`/`sherpa`/`ndarray`/`rusqlite`/`axum`/`tokio`/`owo-agent-core`/`owo-agent-server` 出现次数**全部为 0**；闭包只剩 crypto/serde/chrono/uuid/windows-sys | `docs/qa/logs/mk-kernel-tree.log` |
| 全量 core 测试（含 36 个集成测试文件） | `cargo test -p owo-agent-core --locked` | **exit=0**，363 s（1 个 `--ignored` 真模型用例按设计跳过） | `docs/qa/logs/mk-core-tests-*.log` |
| **运行态验收**（真实进程 + 真实 HTTP + 真实落盘） | `scripts/mk-smoke.ps1 -Tag m0-kernel` | **13/13 PASS**，ok=True | `docs/qa/evidence/mk-smoke-m0-kernel-*/report.json` |
| `cargo fmt --all -- --check` | 经 `Invoke-CiCargo` | **exit=0** | 会话记录 |
| 退出码不被掩盖（负例） | `Invoke-CiCargo -Arguments @('check','--workspace') -Cwd <不存在的目录> -PassThru` | 返回纯标量 `Int32 1`，非数组 | 会话记录（§2.2 第 3 条） |
| 所有改动文件编码 | 逐文件 `UTF8Encoding(throwOnInvalid:true)` 解码 + mojibake 标记扫描 | 23 个文本文件全部合法 UTF-8，无 mojibake；3 个 `.ps1` 均带 BOM | 会话记录 |

运行态验收的 13 项覆盖（`scripts/mk-smoke.ps1`，退出码 0/1）：

```text
[PASS] daemon.core_ready            core_ready 行已输出（端口 55054，api_version 0.7）
[PASS] http.health_public           /health 200 healthy=True stage=ready
[PASS] http.unauthenticated_401     无 token 的 GET /sessions -> 401（权限面未被拆分削弱）
[PASS] data.auth_token_written      auth/token 已落盘（不打印内容）
[PASS] http.sessions_list           带 token -> 200
[PASS] http.server_status_readable  storage.read_only=False，无迁移告警
[PASS] session.create               POST /session -> 200，真实会话 id
[PASS] session.read_back            GET /session/{id} -> 200
[PASS] session.persisted_in_list    列表中读回 1 条
[PASS] data.index_db                index.db（+ WAL/SHM）已建立
[PASS] data.server_pid              server.pid 已建立
[PASS] audit.event_emitted          server_start 审计事件落盘（kernel::audit 参与运行）
[PASS] process.no_orphan            停止后无残留进程
```

> 资源合规：所有 cargo 调用均经 `scripts/ci-shared.ps1` 的 `Invoke-CiCargo`
> （完整 workspace 档 `-j 1`/`--test-threads=1`，定向档 `-j 2`），并在启动前通过
> `Resolve-OwoOrtEnv` 注入 ORT、执行内存门与磁盘门。本轮实测磁盘事件：
> 构建前 T: 仅剩 **24.7 GB**（低于 §10 红线 5 的 20 GB 全量档要求），按 AGENTS.md
> 允许清单清理 `target/**/incremental` 与 `target/**/*.pdb` 后回收 **25.9 GB**，
> 盘余 **50.6 GB** 才启动构建。

---

## 3. M1：`devtools/product-eval` + `owo-agent-eval-facade`（已完成）

### 3.1 边界与依赖方向

ProductEval 底座（`product_eval` 3,317 行 + `eval` + `dataset_builder` +
`product_eval_workswarm` + 6 个集成测试，约 8.6k 行）整体迁出 core：

```text
server / cli ──► owo-agent-eval-facade ──► devtools/product-eval ──► owo-agent-core
                                                  ▲
                                                  └─ workspace 成员（独立解析，不回流）
core 对 devtools 的依赖 = 0
```

* `devtools/product-eval/` 是**独立 workspace**（自带 `Cargo.lock` 与 `target/`），
  对齐指南 §9 对它的定位：开发时加载、不进生产默认运行时、不拖累用户启动与 Rust 编译。
* `crates/owo-agent-eval-facade` 是 workspace 成员里的薄门面（11 行代码 + 边界文档），
  只做 `pub use owo_agent_product_eval::*;`，让 server/cli 继续用熟悉路径拿评测面。
* **core 不再持有任何评测面**：`pub mod product_eval` / `pub use product_eval::*` /
  `#[path = "product_eval/workswarm_executor.rs"]` 全部删除。

### 3.2 为什么不能用 optional dependency + feature（三条路都实测撞环）

| 尝试 | 结果 |
|---|---|
| product-eval 作为 workspace 成员 + core `optional` 依赖 + 成员写 `default-features = false` | Cargo 警告该开关被忽略（须写在 workspace 定义处），随后 `cyclic package dependency` |
| 把 `default-features = false` 写到 workspace 定义处 | server/cli 需要评测面 → 打开 core 的 `product-eval` feature；**feature 是并集**，devtool 那条 core 边被重新点亮 → 再次成环 |
| devtool 移出 `crates/`、加 workspace `exclude` | path 依赖仍被解析进同一个 package 实例 → 第三次成环 |
| **最终**：devtool 成为独立 workspace + core 零依赖 + 门面 crate 承接消费方 | ✅ 成立，且方向更正确（受信运行时不依赖开发工具） |

### 3.3 拆分暴露并修好的四个缺陷

1. **`workswarm_executor` 从未真正成为模块**：core 用
   `#[path = "product_eval/workswarm_executor.rs"] pub mod product_eval_workswarm;`
   把它挂在 crate 根。搬到新 crate 后 `pub use product_eval::workswarm_executor` 直接
   `E0432`（`no workswarm_executor in product_eval`）。已改为 `product_eval` 的正式子模块。
2. **`required_string` 是 core 内的死代码**：它是 `owo-agent-core::tools` 的
   `pub(crate)`，core 内部**零调用**，唯一真实使用者是开发工具包。已迁到内核
   `owo_agent_kernel::tool_args::required_string`，core 侧改为 `use owo_agent_kernel::required_string;`
   （涉及 `tools.rs` 7 处、`computer_use.rs` 9 处调用点）。**这正是"独立 crate 才能暴露的
   隐式耦合"**：一个 `pub(crate)` 助手把内核原语寄生在 core 里，同时暴露了 core 用不到的
   宽度。
3. **两个 core 集成测试依赖开发工具面**：`transition_tests.rs` / `world_model_tests.rs`
   原先 `use owo_agent_core::dataset_builder::…`。core 已不持有它，改为通过
   **dev-dependency** 引门面（正常依赖仍为零；Cargo 的环检测不覆盖 dev 边）。
4. **测试里写死的仓库相对深度会随目录搬迁失效**：
   `CARGO_MANIFEST_DIR/../../evals/v1/suite.json` 在 `crates/owo-agent-core/tests`
   下是对的，搬到 `devtools/product-eval/tests` 后少了（后来多了）一层。已按最终位置
   校正为 `../../evals/...`，并实测 `Test-Path` 通过。

### 3.4 验收证据（可复现）

| 验收项 | 命令 | 结果 | 证据 |
|---|---|---|---|
| workspace 全目标编译（含门面与 server/cli 重接线） | `scripts/mk-check.ps1 -Tag m1 -WithDevtool` | workspace **exit=0**（163 s）、devtool **exit=0**（32 s） | `docs/qa/logs/mk-m1-*.log` |
| **全量 core 测试**（461 单测 + 36 集成测试文件） | `cargo test -p owo-agent-core --locked` | **exit=0**，330 s | `docs/qa/logs/mk-m1-core-tests-*.log` |
| **全量 server 测试**（63 单测 + 40 个集成测试文件，含 `product_eval_api_tests`、`eval_gate_tests`、`route_contract_tests`） | `cargo test -p owo-agent-server --locked` | **exit=0**，326 s（首轮被 §2.4 红线 7 内存门以 137 中止，清内存后复跑通过） | `docs/qa/logs/mk-m1-server-tests2-*.log` |
| **迁走的 ProductEval 测试在独立 workspace 全绿** | `cargo test --manifest-path devtools/product-eval/Cargo.toml` | **66 passed / 0 failed**（1 个真模型用例按设计 `--ignored`） | `docs/qa/logs/mk-m1-devtool-tests2-*.log` |
| **运行态**：ProductEval 路由真实走通门面 | `scripts/mk-smoke.ps1 -Tag m1-product-eval3` | **15/15 PASS**；`POST /product-eval/runs` → 202 + `run_id`，轮询到 `completed`，进度 **20/20**，`metrics.runs_total=20` | `docs/qa/evidence/mk-smoke-m1-product-eval3-*/report.json` |
| `cargo fmt --all`（两个 workspace） | 经 `Invoke-CiCargo` | 均 exit=0 | 会话记录 |

运行态验收追加的两项（相对 M0 的 13 项）：

```text
[PASS] product_eval.create_run     status=202 run_id=eval-089acbadd3114a759c63f9c39747eac9
[PASS] product_eval.run_completed  status=completed progress=20/20 metrics.runs_total=20
```

> 踩坑记录：ProductEval 运行态门**不能另起第二个服务实例**——`serve.rs` 用 pid 文件做
> 单实例闸门，实测第二次启动直接报“检测到运行中的服务（pid=…）：请先停止该进程再启动”。
> 现在改为单实例 + 把仓库 `evals/` 以目录联接挂进隔离工作区。

### 3.5 改动文件

| 文件 | 变更 |
|---|---|
| `Cargo.toml`（workspace） | 新增成员 `crates/owo-agent-eval-facade`；新增 `exclude = ["devtools/product-eval"]` 及环依赖说明 |
| `crates/owo-agent-eval-facade/` | 新增门面 crate（`pub use owo_agent_product_eval::*`） |
| `devtools/product-eval/` | 新增独立 workspace（`product_eval` / `eval` / `dataset_builder` / `workswarm_executor` + 6 个集成测试 + `.gitignore`） |
| `crates/owo-agent-core/{Cargo.toml,src/lib.rs}` | 移除评测面与 `product-eval` feature；新增 dev-dependency 门面；删除 `required_string` 死代码 |
| `crates/owo-agent-core/src/{tools,computer_use}.rs` | 改用 `owo_agent_kernel::required_string` |
| `crates/owo-agent-core/tests/{transition,world_model}_tests.rs` | 改引门面 |
| `crates/owo-agent-server/{Cargo.toml,src/{lib,eval_api,eval_gate,product_eval_api,desktop_world_api}.rs,tests/product_eval_api_tests.rs}` | 评测面来源改为 `owo_agent_eval_facade`（20 处） |
| `crates/owo-agent-cli/{Cargo.toml,src/{commands/eval.rs,product_eval_cmd.rs}}` | 同上 |
| `crates/owo-agent-kernel/src/{lib.rs,tool_args.rs}` | 新增 `tool_args` 模块（`required_string`） |
| `scripts/mk-check.ps1` | 新增 `-WithDevtool`：把被排除的独立 workspace 纳入同一轮验证 |
| `scripts/mk-smoke.ps1` | 新增 ProductEval 运行态门（路由 → 门面 → 开发工具全链） |
| `docs/ARCH-MICROKERNEL.md` | 本文 §5 |

---


---

## 4. 复用清单（M1 及以后每一步都照这个模板执行）

```text
1. 选边界：算出边/入边（注释剥离后的 crate:: 扫描，脚本见 scratch-eval-runs/dep-graph-clean.json 生成方式）
2. 查可见性：候选内是否有 pub(crate) 条目被外部使用（有则先升为 pub 或改内聚）
3. 查宏：宏定义无法跨 crate 用 `crate::` 访问，候选内有 macro_rules! 就要先规划
4. git mv：保留历史；不要复制
5. 写新 crate：Cargo.toml（依赖最小化 + 边界注释）、src/lib.rs（模块 + 顶层 re-export 1:1 对齐）
6. 旧 crate：删 pub mod，加同名别名模块 + pub use
7. cargo fmt --all
8. 生成锁文件：cargo metadata --format-version 1（离线失败时先跑一次不带 --locked 的构建）
9. 验收：新 crate 单测 + workspace --all-targets 编译 + 运行态冒烟（`scripts/mk-smoke.ps1`）
10. 证据落盘（`docs/qa/logs/`、`docs/qa/evidence/`）并在本文追加一行
```

### 反复出现的坑（已踩）

| 坑 | 症状 | 处置 |
|---|---|---|
| feature 隐式依赖旧 crate 的 manifest | 独立后 `E0432`/`E0433` | 新 crate 显式声明自己真正用到的 feature；在 manifest 写"不得引入"约束 |
| 测试用 `#[path]` 复制源文件 | 文件搬走后编译失败；且验的不是真实实现 | 改为引用新 crate |
| `$LASTEXITCODE` 经函数调用丢失 | 门禁报 1 而 cargo 实际是 101 | `Invoke-CiCargo -PassThru` 返回标量退出码 |
| PowerShell 函数返回值混入输出流 | `-PassThru` 返回 `@($true, 101)` | 调用处 `$null = ...` 丢弃；`-PassThru` 再做标量兜底 |
| `.ps1` 编辑后丢 UTF-8 BOM | `ci-gate -Step utf8` 会红 | 每次改完 `.ps1` 立即复查前三字节 `239,187,191` |
| 无中间目录的相对日志路径 | `StreamWriter` 抛 `DirectoryNotFoundException` | 传绝对路径 |
| **开发工具与运行时互相依赖** | `cyclic package dependency: <crate> depends on itself` | devtool 必须是**独立 workspace**；运行时对它的依赖为零；消费方经薄门面 crate 接入（详见 §3.2） |
| `Exclude` 挡不住 path 依赖 | 已 exclude 仍报环 | exclude 只挡自动成员，不挡 `path =` 引用；真正的隔离要靠独立 workspace + 零反向依赖 |
| workspace 级 `default-features = false` 也挡不住 | 已关默认 feature 仍报环 | Cargo feature 是**并集**，任一成员打开就重新点亮；不要指望用 feature 关掉一条会成环的边 |
| 测试写死仓库相对深度 | 目录搬迁后 `suite.json 不存在` | 用最终位置校正 `CARGO_MANIFEST_DIR` 的相对层数，并在验收前用 `Test-Path` 实测解析结果 |
| 服务端单实例闸门 | 第二次 `serve` 直接退出，无 `core_ready` | 同一轮验收只用**一个**服务实例；需要套件可见时把 `evals/` 联进隔离工作区 |
| 验收门自己写错契约导致的假红 | `POST /notes` 返回 201 但断言 200；`GET /notes` 断言裸数组但实际是 `{count, notes}` | 写运行态断言前先读服务端 handler 与请求/响应结构，不要凭直觉 |
| 用"入边/出边计数"选边界 | 反复撞 `cyclic package dependency` | 改用 **Tarjan SCC + 分量 DAG**：只有"零出边 + 零入边"的分量才能零代价切走（见 §5.1） |
| 把"模块互相引用"直接当成"必须接口倒置" | 多写一堆不必要的 sink/trait | 先问**这条边搬迁后是否跨越 crate 边界**：① 列出出边 ② 判断目标是否与它**同迁** ③ 只有"不同迁且目标反向引用它"的边才是真环。M3 实测 `audit_chain ↔ sandbox` 同迁一 crate → 零倒置（见 §6.1） |
| 构建卷空间不足 | `LNK1318 非意外的 PDB 错误: LIMIT`（看着像编译错误） | 先清 `target/**/incremental`、`target/**/*.pdb`（M0–M3 共回收约 67 GB） |


## 5. M2：`owo-agent-extensions`（已完成）

### 5.1 用强连通分量（SCC）找"真正能切"的边界

M1 的教训是：靠"入边/出边计数"选边界会反复撞环。M2 改用**结构性判据**——对
`owo-agent-core` 的 76 个模块依赖图（注释/字符串剥离后的真实 `crate::` 引用）跑
Tarjan 强连通分量，把图压缩成分量 DAG。结论：

* 共 **46 个分量**，其中三个是真实环团：
  * **[16] 18 模块 / 16,502 行**：`agent`、`tools`、`permissions`、`session`、`gateway`、
    `executor`、`computer_use`、`learn`、`mcp_health`、`settings`、`sqlite_store`、
    `subagent`、`contract_worker`、`autoreview`、`grant_store`、`permission_spec`、
    `tool_effects`、`schema_budget`；
  * **[26] 8 模块 / 7,015 行**：`fleet`、`goal`、`bus_store`、`execution_target`、
    `worker_pool`、`remote_step`、`fleet_transport`、`fleet_node_protocol`；
  * **[28] 4 模块 / 6,700 行**：`workswarm`、`team_strategy`、`team_prompt`、
    `builtin_team_templates`。
* **完全可分离集合（零出边 + 零入边）只有 5 个模块、4,493 行**：
  `notes`(1,449)、`cloud_exec`(1,316)、`change_set`(838)、`change_set_store`(552)、
  `automation`(338)。

这就是 M2 的对象。判据很硬：**零出边** ⇒ 不依赖 core 任何模块（可独立编译）；
**零入边** ⇒ core 内部无人引用（只需别名 re-export，调用方零改动）；两者同时成立
⇒ **不可能形成 crate 环**。

> 方法可复用：依赖图的生成方式见 §4 第 1 步；SCC 脚本是一次性分析工具，结论已固化
> 在本文，不需要每次重跑。

### 5.2 边界与验收

* 依赖方向：`owo-agent-core ──► owo-agent-extensions ──► owo-agent-kernel`，且 extensions
  **不依赖 core / server / ONNX / Sherpa**（`cargo tree` 实测：这些名字在依赖闭包中
  出现 0 次）。
* core 侧只做一件事：`pub use owo_agent_extensions::{automation, change_set,
  change_set_store, cloud_exec, notes};`（同名别名模块），原有逐条 `pub use` 继续工作。
* **服务端与 CLI 一行未改**（`git status` 实测：M2 只动了 `Cargo.toml`、`Cargo.lock`、
  core 的 `Cargo.toml`/`lib.rs` 与 5 个 `git mv`）——这是"re-export 透明"的直接证据。

关键裁决点（写进 crate 文档，避免后人搬一半）：指南 §2.4 要求"文件写前快照、写入、
diff 和 revert"保持在同一拥有者内。本 crate **只承载快照与恢复的状态机/存储**
（`change_set` / `change_set_store`），真正的写入仍由 core 的 `executor`/`tools` 执行；
迁到 Tool Host（§9 A4）时必须整体复核。

| 验收项 | 命令 | 结果 | 证据 |
|---|---|---|---|
| workspace 全目标编译 | `check --workspace --all-targets`（经 `Invoke-CiCargo -j 1`） | **exit=0**，159 s | `docs/qa/logs/mk-m2-check-*.log` |
| 全量 core 测试 | `cargo test -p owo-agent-core --locked` | **exit=0**，328 s（439 单测 + 36 集成测试文件；比 M1 少 22 个单测，因为随 5 个模块迁走） | `docs/qa/logs/mk-m2-core-tests-*.log` |
| **全量 server 测试**（迁移模块的真实消费者：`notes_api_tests`、`change_set_api_tests`、`human_inbox_api_tests`、`cloud_sse_tests`） | `cargo test -p owo-agent-server --locked -j 1 -- --test-threads=1` | **exit=0**，512 s（63 单测 + 40 集成测试文件全绿；首轮被 §2.4 红线 7 内存门以 137 中止，清内存 + 降并发后复跑通过，未绕过门） | `docs/qa/logs/mk-m2-server-tests2-*.log` |
| 依赖闭包不含 core/server/ONNX | `cargo tree -p owo-agent-extensions` | `owo-agent-core`/`owo-agent-server`/`ort`/`sherpa`/`ndarray` 出现 **0 次** | `docs/qa/logs/mk-m2-tree.log` |
| **运行态**（新增扩展路由门） | `scripts/mk-smoke.ps1 -Tag m2-final` | **18/18 PASS**；`POST /notes` → 201 + id，`GET /notes` → `count=1` 命中，`GET /automations` → 200 | `docs/qa/evidence/mk-smoke-m2-final-*/report.json` |
| `cargo fmt --all` | 经 `Invoke-CiCargo` | exit=0 | 会话记录 |

运行态新增的三项：

```text
[PASS] extensions.notes_create       status=201 id=b61dc9c8-2287-48ee-b742-52603b99f6ab
[PASS] extensions.notes_list         count=1 命中=1（notes 已迁至 owo-agent-extensions）
[PASS] extensions.automations_list   status=200 body=[]
```

> 写这两项时踩了两个契约坑（已修正断言）：`create_note` 的契约状态码是 **201**（CREATED）
> 而不是 200；`list_notes` 返回 `{count, notes:[...]}` **对象信封**而不是裸数组。
> 这类"门自己写错、看起来像功能坏了"的假红，正是 §4 要求先查契约再写断言的原因。

### 5.3 改动文件

| 文件 | 变更 |
|---|---|
| `Cargo.toml`（workspace） | 新增成员 `crates/owo-agent-extensions` + workspace 依赖 |
| `crates/owo-agent-extensions/{Cargo.toml,src/lib.rs}` | 新 crate（边界文档 + 五个模块 + glob 再导出） |
| `crates/owo-agent-core/src/{notes,cloud_exec,change_set,change_set_store,automation}.rs` | `git mv` 到新 crate（其中 2 个文件仅把 `crate::cas_store`/`crate::audit` 改指内核） |
| `crates/owo-agent-core/{Cargo.toml,src/lib.rs}` | 新增依赖 + 别名 re-export |
| `scripts/mk-smoke.ps1` | 新增扩展路由运行态门 |
| `crates/owo-agent-server`、`crates/owo-agent-cli` | **未改动** |



---

## 6. M3：`owo-agent-tool-safety`（已完成）

设计与决定见 **`docs/adr/ADR-001-tool-safety-kernel.md`**；本节是摘要 + 与 ADR 的差异。

### 6.1 结果：比 ADR 原计划简单一半

ADR 原计划「先用注入式 `SandboxAuditSink` 打断环、再整体搬迁」。实测发现**前半步不必要**：

* `audit_chain` 与 `sandbox` 的相互引用**只发生在这一对内部**
  （`audit_chain.rs:14 use crate::sandbox::SandboxAuditLog`；
  `sandbox.rs:637 chain: &mut crate::audit_chain::AuditChain`）；
* 两个模块是**一起**搬进同一个新 crate 的 → 该边不再跨越任何 crate 边界 → 无需倒置。

实际实施 = `git mv` 两个模块 + core 保留别名 re-export，**零接口改动、零调用方改动**。
代价面：`audit_chain.rs` 只改 3 处内核引用（`crate::credentials` / `crate::storage_crypto` ×2
→ `owo_agent_kernel::*`），`sandbox.rs` **一行未改**。

### 6.2 由此修正的判断规则（已写入 §4 复用清单）

原 ADR 把"两个模块互相引用"直接等同于"必须接口倒置"，漏问了一个更基本的问题：
**这条边在搬迁后是否会跨越 crate 边界？** 正确顺序是：

```text
① 列出该模块的全部出边；
② 判断这些目标是否与它「同迁」；
③ 只有「不同迁、且目标反向引用它」的边，才是必须倒置的真环。
```

### 6.3 额外发现：tool-safety 连 Windows 绑定依赖都不需要

`sandbox` 的 Windows 部分是**裸 FFI**（`extern "system"` + `#[link(name =
"kernel32"/"advapi32"/"ntdll")]`，共 3 个 extern 块），**完全不使用 `windows` /
`windows-sys`**（core 里用这两个 crate 的其实是 `executor.rs`(31 处) / `ocr.rs`(4) /
`accessibility.rs`(4)）。因此新 crate 的依赖闭包只有
`owo-agent-kernel` + serde / serde_json / chrono / uuid / sha2 / thiserror —— 对指南 §10
「普通 Agent 改动不触发原生重链」是直接利好。

### 6.4 验收证据

| 验收项 | 命令 | 结果 | 证据 |
|---|---|---|---|
| workspace 全目标编译 | `check --workspace --all-targets`（`-j 1`） | **exit=0**，157 s | `docs/qa/logs/mk-m3-check-*.log` |
| 全量 core 测试（含全部安全契约） | `cargo test -p owo-agent-core --locked` | **exit=0**，320 s；`sandbox_tests`(26)、`os_sandbox_integration_tests`(25)、`production_security_contract_tests`(12) 与 audit_chain 篡改矩阵全绿 | `docs/qa/logs/mk-m3-core-tests-*.log` |
| 全量 server 测试 | `cargo test -p owo-agent-server --locked -j 1 -- --test-threads=1` | **exit=0**，995 s（63 单测 + 40 集成测试文件全绿） | `docs/qa/logs/mk-m3-server-tests-*.log` |
| 依赖闭包 | `cargo tree -p owo-agent-tool-safety` | `owo-agent-core`/`owo-agent-server`/`sherpa`/`ndarray`/`rusqlite`/`windows` 均 **0 次**；`owo-agent-*` 只有 kernel 与自身 | `docs/qa/logs/mk-m3-tree.log` |
| 调用方零改动 | `git status` | `mcp.rs` / `plugin.rs` / `tools.rs` / server / cli **未改动** | 会话记录 |
| 运行态 | `scripts/mk-smoke.ps1 -Tag m3-tool-safety` | **18/18 PASS**（含审计事件落盘、无孤儿进程） | `docs/qa/evidence/mk-smoke-m3-tool-safety-*/report.json` |
| `cargo fmt --all` | 经 `Invoke-CiCargo` | exit=0 | 会话记录 |


## 7. 后续候选与取舍记录

M0–M3 已把 core 里"能结构性地零代价切下来"的部分用完：**§5.1 的 SCC 分析证明，整个 core
只有那 7 个模块满足零入边 + 零出边（M2 切 5 个、M3 切 2 个）**。因此 M4 起必须做
**真正的依赖倒置或整组搬迁**，不能再指望"搬文件 + re-export"零代价推进。

### M4 优先级 0（不是新 crate，而是补契约缺口）

**三方事务边界契约测试**——M3 把它留成了最大未闭合风险点（见 `docs/adr/ADR-001-tool-safety-kernel.md` §7.5）：
`change_set*`（extensions，快照/恢复状态机）＋ `sandbox`（tool-safety，执行隔离与审计收据）
＋ core 的 `executor`/`tools`（实际写入）共同承担指南 §2.4 第 4 条，但三者交接点**没有任何
专门测试**。必须先补：

```text
① 被沙箱拒绝的命令 → 工作区零文件变更 + 必须留下审计收据；
② 被接受的命令 → 变更必须能被 change_set 捕获，且 revert 能精确还原；
③ 拒绝/失败路径不得产生"半写"（写了但没有 change_set 记录）。
```

### M4 候选方向（都要先做倒置或整组搬迁）

下面按"收益 ÷ 代价"排序，全部依据 §5.1 的分量数据：

* **候选 A：Perception Worker**（指南 §9 A3，SLO 收益最大，代价也最大）
* **候选 B：Tool主机其余部分**（policy / grant / approval，指南 §9 A4）
* **候选 C：Fleet / WorkSwarm / Workflow**（指南 §1.3 已定档"暂停新增、可选加载"）
* **候选 D：`desktop_env`**（2,442 行，唯一出边 `computer_use`，入边 2）

### 候选 A：Perception Worker（指南 §9 A3，SLO 收益最大，但最贵）

* 收益：把 `ort`、`sherpa-onnx`、`ndarray`、Media_Ocr/UIA 从基础链搬走，
  直接兑现 §10 的"普通 Agent 改动不触发 ONNX 编译"。
* 代价：5 条出边全是**双向边**（真环）：
  * `computer_use` ↔ perception：`desktop_env`→`computer_use` 1 处 vs
    `computer_use`→perception 30 处；
  * `learn` ↔ `observe`：`observe`→`learn` 9 处；
  * `memory` ↔ `observe`：`memory`→`observe` 1 处；
  * `transition` ↔ `world_model`：`world_model`→`transition` 1 处；
  * `settings` ↔ `stt`：`stt`→`settings` 1 处。
* 处置：要么把 15 个模块整体搬（出边数从 5 降到 0，代价是 crate 变大且 `computer_use`
  这类"执行"能力会被错误地搬进感知 worker，违反 §3 的"动作执行和感知拆开"），
  要么在内核里放 `PerceptionProbe`/`TransitionSink` 之类的 trait 做倒置。
  **这是一次真正的架构决策，必须先写 ADR 再动手。**

### 候选 B：ProductEval devtool（入边为 0）

* 收益：入边 **0**，`eval`/`dataset_builder` 指向外部；`product_eval` 本身占 3,317 行，
  是 §1.1 点名的四个大文件之一。
* 代价：出边 7 条（`agent`、`gateway`、`session`、`tools`、`permissions`、
  `desktop_env`、`transition`），即新 crate 必须**依赖 `owo-agent-core`**。
* 关键约束：core 里还有 `#[path = "product_eval/workswarm_executor.rs"] pub mod
  product_eval_workswarm;`，且 `action_program`/`workflow` 反向引用 `eval`。
  为避免 core→eval 的环，必须用 **optional dependency + feature**（默认关闭）：

  ```toml
  [features]
  default = []
  product-eval = ["dep:owo-agent-product-eval"]
  ```

  这样默认构建把评测完全移出编译与启动链（正是指南 §9 对 ProductEval 的要求），
  而 CLI 的 `eval` 子命令与服务端 `product_eval_api` 显式开启该 feature。

### 候选 C：Tool Host 内核（Policy/Executor）

* 收益：指南 §2.2 的核心受信边界（`permissions`/`permission_spec`/`grant_store`/
  `sandbox`/`executor`/`change_set*`/`tool_effects`，8 模块 8,007 行），入边 16 个文件
  全部可由 re-export 满足。
* 代价：出边 8 条，其中 `tools`↔policy、`mcp`↔sandbox、`audit_chain`↔sandbox、
  `learn`↔executor、`ocr`/`scene`/`locate`/`accessibility`（executor 用）都是双向边。
  按 §9 A4 的迁移顺序（只读文件工具 → 写入+diff/revert → command/process →
  permission/grant/approval → MCP → browser/desktop action）逐段切，是**多步工作**，
  不适合作为 M1 的单个步骤。

### 候选 D：Fleet / WorkSwarm

指南 §1.3 已把它们定档为"暂停新增功能、降为 Advanced/Experimental、可选加载"。
它们的出边最多（15 条），且互相引用。**在 §9 A2（统一 Daemon）落地前动它们，只会
把环搬来搬去**；建议排到 A2/A7 之后。

### M1 实际选择：B（ProductEval）

零入边 + 一步可验收，实际执行结果见 §5。**结论：环依赖比预估严重**——原来以为
"optional dependency + feature" 足够，实际三条路都不成立，最终靠"独立 workspace +
薄门面 crate"才落地。这条经验已写入 §4 的坑表。

### M2 建议

按"收益 ÷ 风险"排序：**A（Perception，SLO 收益最大，需先写 ADR）**
→ C（Tool Host，按 §9 A4 分多步）→ D（推迟到 A2 之后）。

---
