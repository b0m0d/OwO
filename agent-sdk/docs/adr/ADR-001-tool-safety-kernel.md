# ADR-001：抽出 Tool-Safety 内核（`audit_chain` + `sandbox`）并打断 2-模块环

> 状态：**已决定，待实施**（M3）
> 日期：2026-09-19
> 依据：`builGoal/Agent-SDK-后续任务实施指南-2026-09-18.md` §2.2（受信执行内核）、
> §2.4（不可拆散的事务边界）、§9 A4（抽取 Tool Host）、§13（Tool Host 权限不可绕过）；
> 事实基线：`docs/ARCH-MICROKERNEL.md` §5.1 的 SCC 分析。
> 前置：M0（`owo-agent-kernel`）、M1（`devtools/product-eval` + `eval-facade`）、
> M2（`owo-agent-extensions`）均已完成并验收。

---

## 1. 背景与问题

`docs/ARCH-MICROKERNEL.md` §5.1 用 Tarjan 强连通分量把 `owo-agent-core` 的 76 个模块
压成 46 个分量。结论是：**M0/M1/M2 已经把 core 里所有"零入边 + 零出边"的模块切完了**
（M2 切走最后 5 个，4,493 行）。此后每一步都必须做**依赖倒置或整组搬迁**，不能再指望
"搬文件 + re-export"零代价推进。

在剩下的分量里，唯一一个**零出边**的成规模分量是：

```
[8] audit_chain(506 行) + sandbox(2,188 行) = 2,694 行
    出边（对 core 其它模块）：0
    对内核的依赖：audit_chain → credentials, storage_crypto；sandbox → 无
    入边：mcp、plugin
```

它正对应指南要隔离的**受信执行边界**：

* §2.2：「Policy 与 Executor 必须在同一受信边界，不能让主 Agent 自批自执行」；
* §13 完成定义：「Tool Host 权限不可绕过」；
* §2.4 第 3 条：权限判定、capability、执行和审计收据必须同属一个拥有者。

因此把 `sandbox`（OS 级执行沙箱：Job Object / AppContainer / 资源上限 / kill-on-close）
与 `audit_chain`（防篡改审计链）放在同一个 crate，是**方向正确且必须**的一步。

## 2. 唯一阻塞点（实测）

两者**互相引用**，构成一个 2 模块真环：

```rust
// crates/owo-agent-core/src/audit_chain.rs:14
use crate::sandbox::SandboxAuditLog;

// crates/owo-agent-core/src/sandbox.rs:637
chain: &mut crate::audit_chain::AuditChain,
```

* `audit_chain::append_sandbox_log(&mut self, log: &SandboxAuditLog, actor: &str) -> usize`
  —— 链**知道**沙箱类型（把 `SandboxAuditEvent` 按 `sandbox.<kind>` 写成审计记录）；
* `sandbox` 内部某处把 `SandboxAuditLog` 汇入 `AuditChain` —— 沙箱**知道**链。

在同一个 crate 里这完全合法，一旦分 crate 就必然成环。这与 M1 的
"core ↔ devtools"环不同：那是**包之间**的环，可以用独立 workspace + 门面解决；
这里是**两个模块之间的概念环**，只能靠接口倒置消除。

## 3. 候选方案

### 方案 A（推荐）：沙箱审计汇出改为注入式 sink，`audit_chain` 不再认识沙箱类型

把"把沙箱事件写成审计记录"的职责从 `audit_chain` 挪到调用点（core 侧）：

```rust
// 新 crate：sandbox 侧只暴露数据与一个中性汇出口
pub trait SandboxAuditSink {
    fn record(&mut self, event: &SandboxAuditEvent, actor: &str) -> usize;
}
// sandbox 自己不再 import audit_chain；core 侧提供适配器
impl SandboxAuditSink for AuditChain { /* 原来的 append_sandbox_log 逻辑搬到这里 */ }
```

* 优点：环彻底消失；`sandbox` 与 `audit_chain` 之间不再有概念依赖；不扩大内核 API；
  审计记录格式（`sandbox.<kind>`、`tool = sandbox:<name>`）保持字节级不变，
  指南 §5.3 的 `policy.db` / 审计收据语义不受影响。
* 代价：`append_sandbox_log` 从 `AuditChain` 的固有方法变成 core 侧适配器的实现；
  需要改动 `audit_chain` 的少量调用点与 `audit_chain_tests` 的断言位置。
* 风险：低。审计链的**验证**逻辑（HMAC 分段链、anchor、verify）完全不动，只动"谁负责
  把沙箱事件转成 AuditRecord"。

### 方案 B：把 `SandboxAuditEvent` / `SandboxEventKind` 等类型下沉到 `owo-agent-kernel`

* 优点：改动更机械（只是移类型），环消失。
* 缺点：内核开始承载"沙箱"概念，与 §2.2 的边界划分相悖——内核应当只放"被多个边界
  共用的稳定原语"，而不是"某个执行器的数据类型"。**否决。**

### 方案 C：整组搬迁 + 让 core 依赖新 crate（`owo-agent-tool-safety`）

* 即：先做方案 A 消除环，再把 `sandbox` + `audit_chain` 一起 `git mv` 到新 crate，
  core 添加别名 re-export（与 M0/M2 同手法），入边 `mcp`/`plugin` 由 re-export 满足。
* 这是方案 A 的执行形态，不是替代方案。**采纳。**

## 4. 决定

采用 **A + C**：

1. 先用注入式 sink 打断 `audit_chain ↔ sandbox` 环（方案 A）；
2. 再按 M0/M2 的既有手法把两者整体迁到新 crate `owo-agent-tool-safety`
   （依赖 `owo-agent-kernel`；反向依赖为零）；
3. core 保留同名别名模块 + 顶层 `pub use`，因此 `mcp`、`plugin` 与所有集成测试
   **零改动**；
4. 新 crate 是否最终演进为指南 §4 的独立 `services/tool-host` workspace，
   留到 §9 A4（Tool Host 抽取）再决定——本 ADR 只负责把**受信执行内核的边界**立起来。

### 命名的取舍

暂命名为 `owo-agent-tool-safety` 而不是直接叫 `owo-agent-tool-host`：按指南 §4，
Tool Host 是一个**独立 workspace 的进程**（含 policy/grant/approval/executor/mcp-host
四组 crate）。当前迁移只是把"沙箱执行 + 审计收据"这两块从 core 拿出来，
尚未包含 policy/grant/approval，故不用 `tool-host` 之名，避免名不副实。

## 5. 验收标准（实施时逐条落证据）

| # | 标准 | 判据 |
|---|---|---|
| 1 | 环已消除 | `audit_chain.rs` 中不再出现 `sandbox`（注释除外）；`sandbox.rs` 中不再出现 `audit_chain` |
| 2 | 新 crate 零反向依赖 | `cargo tree -p owo-agent-tool-safety` 中 `owo-agent-core`/`owo-agent-server` 出现 0 次 |
| 3 | 调用方零改动 | `git status` 显示 `owo-agent-server`、`owo-agent-cli` 未改动 |
| 4 | 编译 | `check --workspace --all-targets` exit=0（经 `Invoke-CiCargo`，完整档 `-j 1`） |
| 5 | 安全契约不回归 | `cargo test -p owo-agent-core --locked` exit=0，且 `sandbox_tests`、`os_sandbox_integration_tests`、`production_security_contract_tests`、`audit_chain_tests` 全绿 |
| 6 | 审计链语义不变 | 审计记录格式与 `audit_chain_tests` 的篡改检出矩阵不变；`verify` 仍能检出改字段/删记录/重排/伪造插入/篡改锚点 |
| 7 | 运行态 | `scripts/mk-smoke.ps1` 全绿（含 `audit.event_emitted` 与 no-orphan） |
| 8 | 资源合规 | 全部 cargo 经 `Invoke-CiCargo`；必要时按允许清单清理 `incremental`/`*.pdb` |

## 6. 影响与不做的事

* **不做**：本次不动 `permissions`/`permission_spec`/`grant_store`/`tool_effects`
  （它们与 `agent`/`tools` 同处 18 模块环团 `[16]`，属 §9 A4 后续步骤）。
* **不做**：不把沙箱执行改成跨进程。当前仍是同进程受信内核；跨进程 RPC
  （指南 §5.2 的 length-prefixed JSON-RPC）是 §9 A4 的课题。
* **必须复核**：`change_set` / `change_set_store`（M2 已迁到 `owo-agent-extensions`）
  与 `sandbox` 共同承担 §2.4 第 4 条"写前快照 / 写入 / diff / revert"边界。
  迁完 M3 后要明确：**快照与恢复的状态机在 extensions，执行的隔离与审计在 tool-safety，
  实际文件写入仍在 core 的 executor/tools** —— 三者的交接点必须有测试覆盖，
  否则就是"把一个事务边界切成了三份"。这是 M3 之后第一个要补的契约测试。
