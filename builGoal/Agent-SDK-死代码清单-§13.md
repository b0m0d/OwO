# §13 死代码清单（扫描快照）

> 扫描日期：2026-09-10 ｜ 扫描范围：`agent-sdk/crates/` 全部 `.rs`（含 `src/` 与 `tests/`）
> 扫描模式：`#[allow(dead_code)]` 与 `#[allow(unused…)]` 变体（ripgrep 语法 `#\[allow\(dead_code\)\]|#\[allow\(unused`），另单独统计 `#![allow(dead_code)]` 内部属性。
> 方法：Grep 全量命中 → Read 逐处提取被标注项 → 对唯一项名做全 crates 同名引用统计（浅可达性检查，定义处与纯文档注释提及不计入引用）。
> 红线声明：本报告为只读分析产物，未修改任何源代码；不含 API 密钥或任何环境变量值。

---

## 总览

- **总命中数：71**（行内属性 `#[allow(...)]` 62 处 + 内部属性 `#![allow(...)]` 9 处）
- 其中 `#[allow(unused…)]` 变体仅 1 处（`owo-agent-server/src/lib.rs:61` 的 `#[allow(unused_imports)]`，作用于 `pub use` 再导出），其余均为 `dead_code`。
- 命中集中在 `owo-agent-server`（占 90%）；`owo-agent-protocol`、`owo-sim`、`owo-build-info` 三个 crate 零命中。

### crate 分布

| crate | 行内 `#[allow]` | `#![allow]` blanket | 合计 |
|---|---:|---:|---:|
| owo-agent-server | 56 | 8 | 64 |
| owo-agent-core | 3 | 1 | 4 |
| owo-agent-cli | 3 | 0 | 3 |
| owo-agent-protocol | 0 | 0 | 0 |
| owo-sim | 0 | 0 | 0 |
| owo-build-info | 0 | 0 | 0 |
| **合计** | **62** | **9** | **71** |

### 前 10 大文件排名（按命中数，行内+blanket 合计）

| # | 文件 | 命中数 |
|---|---|---:|
| 1 | owo-agent-server/src/observability_api.rs | 18 |
| 2 | owo-agent-server/src/usage.rs | 9 |
| 3 | owo-agent-server/src/event_stream.rs | 8（7 行内 + 1 blanket） |
| 4 | owo-agent-server/src/sse.rs | 5 |
| 5 | owo-agent-server/src/slo.rs | 4（3 行内 + 1 blanket） |
| 6 | owo-agent-server/src/fleet_api.rs | 3 |
| 7 | owo-agent-server/src/workflow_backend.rs | 3 |
| 8 | owo-agent-server/src/lib.rs | 2 |
| 9 | owo-agent-server/src/logging.rs | 2（1 行内 + 1 blanket） |
| 10 | owo-agent-cli/src/worker_child.rs | 2 |

> 尾部（各 1 处）：owo-agent-cli/tests/worker_child_tests.rs、owo-agent-core/tests/artifact_review_tests.rs、owo-agent-core/src/perception.rs、owo-agent-core/src/storage_crypto.rs、owo-agent-server/src/notes_api.rs、owo-agent-server/src/plugin_market_api.rs、owo-agent-server/tests/product_eval_api_tests.rs（2）、owo-agent-server/tests/usage_tests.rs。
> 仅含 blanket 的文件：owo-agent-server/src/{auth_token,error_codes,idempotency,rate_limit,team_api}.rs、owo-agent-core/src/sandbox.rs。

---

## crate 级 blanket allow（`#![allow(...)]` 内部属性，最高优先级清理对象）

共 **9 处**。说明：agent-sdk 的 `lib.rs` 本体（crate 根）**没有** blanket allow；以下 9 处均为**模块文件头部（或嵌套子模块内）的内部属性**——由于这些文件本身是所属 crate 的 `mod`，效果等同"整个模块 blanket 豁免 dead_code"，是清单口径下的最高优先级清理对象。模式源自 `team_api.rs`（"主控接线前整模块 allow，接线后保留无害"），并扩散到同批 `#[path]` 可独立编译模块。

| # | 文件 | 行 | 作用范围 | 模块注释自述 |
|---|---|---:|---|---|
| B1 | owo-agent-server/src/team_api.rs | 17 | 整个 team_api 模块 | 模式源头："router 由主控合并进 build_router 前模块内部结构暂未被 lib 引用，故模块级 allow（接线后自动消除，保留无害）" |
| B2 | owo-agent-server/src/auth_token.rs | 17 | 整个 auth_token 模块 | lib 经 build_router 使用 require_auth/bootstrap；#[path] 测试目标内中间件未被调用，避免 clippy -D warnings 误报 |
| B3 | owo-agent-server/src/error_codes.rs | 15 | 整个 error_codes 模块 | lib 当前仅登记模块（无路由引用），全部符号由 error_codes_tests 以 #[path] 独立编译使用；"后续接入错误面后移除" |
| B4 | owo-agent-server/src/event_stream.rs | 23 | 整个 event_stream 模块 | lib 目标仅引用 router/hub；其余符号属"测试面符号" |
| B5 | owo-agent-server/src/idempotency.rs | 15 | 整个 idempotency 模块 | lib 当前仅登记模块（无路由引用），"后续接入幂等端点后移除" |
| B6 | owo-agent-server/src/logging.rs | 22 | 整个 logging 模块 | lib 目标仅引用 TraceId/emit/Level；Redactor/safe_field/sanitize_json 属"测试面符号" |
| B7 | owo-agent-server/src/rate_limit.rs | 17 | 整个 rate_limit 模块 | lib 经 build_router 使用 enforce_rate_limit；#[path] 测试目标内中间件未被调用 |
| B8 | owo-agent-server/src/slo.rs | 23 | 整个 slo 模块 | lib 目标仅引用 report_global（经 observability_api 探针注册）；其余属"测试面符号" |
| B9 | owo-agent-core/src/sandbox.rs | 1015 | `#[cfg(target_os="windows")] pub(crate) mod win`（FFI 层子模块） | Windows 裸 FFI 层（Job Object/令牌/AppContainer/管道），同处还有 `non_camel_case_types`、`clippy::upper_case_acronyms`；平台绑定惯例，风险最低 |

> 评估提示：B1/B3/B4/B5/B6/B8 这 6 个模块的自述是"lib 仅登记/仅引用少数符号"——blanket 会同时豁免**真正无人调用的符号**与**测试面符号**，二者不可区分，属于掩蔽性最强的清理对象（详见明细表与其后的分批建议）。B2/B7 的豁免动机是 #[path] 测试目标编译（`-D warnings`），若要收窄需为测试目标单独提供 allow 或调整接线。

---

## 明细表

"定义外引用数"= 全 crates 同名文本引用 − 定义处 − 纯文档注释提及；标注（测试）= 引用位于 `tests/` 测试目标；标注（内部）= 引用位于同模块其它函数体内。浅检查基于同名 grep，同名多义处已注明"需人工复核"。

### owo-agent-cli

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/worker_child.rs | 127 | `partition_credential_keys` | fn (pub) | 1（测试 worker_child_tests.rs:339） | 测试可达、lib 内死；注释声明"A2 正式白名单路径接手前显式保留"，属有意保留 |
| src/worker_child.rs | 149 | `build_child_command` | fn (pub) | 1（测试 worker_child_tests.rs:364） | 测试可达、lib 内死；契约测试 + A2 适配层语义锁定，属有意保留 |
| tests/worker_child_tests.rs | 21 | `mod worker_child`（`#[path]` 整模块挂载） | mod | 多（测试目标内多处） | 测试目标级 blanket：测试只用部分公开函数，余下入口由二进制侧使用；惯例可保留 |

### owo-agent-core

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/perception.rs | 171 | `CaptureFrame.bytes` | 字段 | 0 读（仅 2 处构造时写入 :245/:321） | **疑似死字段**：截图字节仅存环形缓冲从不读取（OCR 在写入前已用局部变量完成），既掩盖数据流动也常驻内存，建议优先人工复核（删除或补充读取路径） |
| src/storage_crypto.rs | 64 | `StorageCryptoError::Unsupported` | 枚举变体 | 2（`encrypt_blob`/`decrypt_blob` 的 `#[cfg(not(windows))]` 分支内构造 :91/:105） | 条件性死代码：Windows 构建下该变体不构造故触发 lint；非 Windows 构建下实际使用。**保留合理**（显式不静默降级契约） |
| src/sandbox.rs | 1015 | `mod win`（FFI 层） | blanket（嵌套子模块内部属性） | — | 平台 FFI 绑定层整层 allow，惯例可接受；低优先级，可不处理 |
| tests/artifact_review_tests.rs | 67 | `ReviewStateForTest` | enum（测试 crate） | 22（全部在本测试文件内） | 测试可达；且 5 个变体均被构造与映射（:44-48），**allow 疑似已过期可移除**，需人工复核 |

### owo-agent-server —— lib.rs / fleet_api.rs / plugin_market_api.rs / notes_api.rs

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/lib.rs | 61 | `pub use fleet_api::{fleet_hub, router_with_hub as fleet_router_with_hub, FleetHub}`（`#[allow(unused_imports)]`） | re-export | `fleet_hub`→goal_api.rs:958/999 + 测试 3 处；`fleet_router_with_hub`→goal_api_tests.rs:14/1019/1188 | **可能可移除**：再导出已被 lib 内（goal_api）与测试实际使用，`pub use` 本身不触发 unused_imports；allow 疑似接线完成前的遗留，需人工复核 |
| src/lib.rs | 5814 | `CONTRACT_RFC_LOG` | const | 0 | 疑似死代码：契约 RFC 登记以 const 形式存在但无读者；建议改为模块级文档注释或 `#[doc)]`，或保留但加说明 |
| src/fleet_api.rs | 242 | `FleetHub.cas` | 字段 (pub) | 0（全仓 `.cas` 命中均属 WorkswarmCoordinator/ChangeTracker 等同名物，需同名人工复核已做） | 疑似死字段，但注释显式声明保留（"R13 起节点侧经 HttpTransport 接线产物写入，保留字段维持控制面'内容寻址产物'契约"）→ 有意保留，建议改为 `#[doc(hidden)]` 或加 TODO 锚点 |
| src/fleet_api.rs | 296 | `fleet_hub` | fn (pub) | lib 内 5（router 内部 :370、goal_api.rs:958/999、lib.rs:62 再导出）+ 测试 3 | **可能可达——allow 疑似过期**：goal_api 已接线（A2 目标复用控制面），建议复核后移除 allow |
| src/fleet_api.rs | 368 | `router` | fn (pub) | 1（lib.rs:549 build_router 已 `.merge(fleet_api::router(...))`） | **可能可达——allow 疑似过期**：路由已挂载，注释"待主控在 build_router merge"已过时，建议复核后移除 allow |
| src/plugin_market_api.rs | 151 | `SeedEntry` | struct | 1（`SeedBody.entries` :147） | struct 本身可达；allow 实为豁免其**仅反序列化不读取的协议保留字段**（seed 表单/未来服务端使用）→ 字段级死代码，保留合理 |
| src/notes_api.rs | 994 | `block_text_debug` | fn | 0 | 疑似死代码：自称"测试辅助"但测试目标亦无引用，可直接删除候选 |

### owo-agent-server —— observability_api.rs（18 处，最重灾区）

模块头自述："lib 目标内无引用，仅供接线方与 observability_tests 以 #[path] 独立编译调用"。经查 `lib.rs:315-323` 的接线函数已实际注册 5 个探针/桥接，**其中 5 个 allow 已过期**：

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/observability_api.rs | 83 | `record_tool_duration_ms` | fn (pub) | 4（测试） | 测试可达；lib 未接线（工具耗时埋点缺接线方），保留待接线 |
| src/observability_api.rs | 98 | `record_sse_connection` | fn (pub) | 4（测试） | 测试可达；lib 未接线，保留待接线 |
| src/observability_api.rs | 119 | `record_queue_depth` | fn (pub) | 2（测试） | 测试可达；lib 未接线，保留待接线 |
| src/observability_api.rs | 125 | `record_events` | fn (pub) | 2（测试） | 测试可达；lib 未接线，保留待接线 |
| src/observability_api.rs | 136 | `record_event_lagged` | fn (pub) | **0** | **疑似死代码**：同族函数均有测试引用，唯它全无引用；接线或删除二选一 |
| src/observability_api.rs | 147 | `ingest_metrics_sample` | fn (pub) | 7（lib.rs:315 已接线 + 测试 6） | **可能可达——allow 疑似过期**（R7 指标桥接已在主控接线函数中调用） |
| src/observability_api.rs | 183 | `reset_runtime_metrics_for_test` | fn (pub) | 20（测试） | 测试可达（跨测试隔离必需） |
| src/observability_api.rs | 356 | `register_slo_report_probe` | fn (pub) | 6（lib.rs:317 已接线 + 测试 5） | **可能可达——allow 疑似过期** |
| src/observability_api.rs | 363 | `reset_slo_report_probe_for_test` | fn (pub) | 8（测试） | 测试可达 |
| src/observability_api.rs | 697 | `register_usage_probe` | fn (pub) | 2（lib.rs:321 已接线 + 测试 1） | **可能可达——allow 疑似过期** |
| src/observability_api.rs | 704 | `reset_usage_probe_for_test` | fn (pub) | 3（测试） | 测试可达 |
| src/observability_api.rs | 723 | `set_telemetry_enabled` | fn (pub) | **0** | **疑似死代码**：R10 遥测开关无任何接线方/测试调用 |
| src/observability_api.rs | 733 | `record_telemetry_counter` | fn (pub) | **0** | **疑似死代码**：遥测计数无调用方 |
| src/observability_api.rs | 744 | `record_telemetry_error` | fn (pub) | **0** | **疑似死代码**：错误码分布记录无调用方 |
| src/observability_api.rs | 818 | `register_slo_alerts_probe` | fn (pub) | 1（lib.rs:322 已接线） | **可能可达——allow 疑似过期** |
| src/observability_api.rs | 825 | `reset_slo_alerts_probe_for_test` | fn (pub) | **0** | **疑似死代码**：对应探针已接线但其 reset 连测试都没用 |
| src/observability_api.rs | 837 | `register_slo_period_probe` | fn (pub) | 1（lib.rs:323 已接线） | **可能可达——allow 疑似过期** |
| src/observability_api.rs | 844 | `reset_slo_period_probe_for_test` | fn (pub) | **0** | **疑似死代码**：同上 |

### owo-agent-server —— event_stream.rs / sse.rs（SSE 族，存在同名词跨模块）

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/event_stream.rs | 176 | `EventStreamHub::last_delivered` | fn (method) | 0（同名 `last_delivered` 为字段，:136/:151/:170/:178/:410 均为字段访问，非本方法） | 疑似死方法：自述"SSE 断线续传接线方使用"但 lib/测试均未调用；续传接线后启用 |
| src/event_stream.rs | 253 | `reset_metrics_observer_for_test` | fn (pub) | 15（测试 event_stream_tests/observability_tests） | 测试可达 |
| src/event_stream.rs | 402 | `subscribe_with_capacity` | fn (method) | 10（内部 `subscribe_after` :398 + 测试 9） | 可达（lib 内即有调用），allow 仅为压测小容量参数保留 |
| src/event_stream.rs | 606 | `reset_hub_for_test` | fn (pub) | 2（测试） | 测试可达；**与 sse.rs:229 同名不同物，需人工复核** |
| src/event_stream.rs | 628 | `sse_frame_text` | fn (pub) | 1（测试 event_stream_tests.rs:258） | 测试可达；**与 sse.rs:172 同名不同物，需人工复核** |
| src/event_stream.rs | 716 | `sse_response_ok` | fn (pub) | 1（测试 event_stream_tests.rs:247） | 测试可达；**与 sse.rs:235 同名不同物，需人工复核** |
| src/event_stream.rs | 727 | `_type_probe` | fn | 0 | 有意保留的编译占位探针（保障 #[path] 独立编译时 IntoResponse 路径类型完整）；惯例可保留 |
| src/sse.rs | 82 | `CloudSseHub::history` | fn (method) | 1（测试 cloud_sse_tests.rs:234） | 测试可达；lib 内死 |
| src/sse.rs | 171 | `sse_frame_text` | fn (pub) | 1（测试 cloud_sse_tests.rs:234） | 测试可达；与 event_stream.rs:628 同名，需人工复核 |
| src/sse.rs | 228 | `reset_hub_for_test` | fn (pub) | 4（测试 cloud_sse_tests） | 测试可达；与 event_stream.rs:607 同名，需人工复核 |
| src/sse.rs | 234 | `sse_response_ok` | fn (pub) | 2（测试 cloud_sse_tests） | 测试可达；与 event_stream.rs:717 同名，需人工复核 |
| src/sse.rs | 245 | `_type_probe` | fn | 0 | 编译占位探针，有意保留 |

### owo-agent-server —— slo.rs / usage.rs（R9/R10 数据面，"待主控接线"组）

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/slo.rs | 314 | `reset_global_for_test` | fn (pub) | 9（slo_tests 4 + observability_tests 5） | 测试可达；**与 usage.rs:418 同名不同物，需人工复核** |
| src/slo.rs | 508 | `reset_alert_listener_for_test` | fn (pub) | **0** | **疑似死代码**：告警监听器的测试 reset 无任何调用（listener 本体接线/测试面不完整） |
| src/slo.rs | 733 | `reset_alert_registry_for_test` | fn (pub) | **0** | **疑似死代码**：同上，告警注册表测试 reset 无调用 |
| src/usage.rs | 206 | `UsageStore::push_record` | fn (method) | 1（内部 `load_from` :502） | 未接线组：随 `load_from` 一起等主控启动恢复接线；测试经 usage_tests 间接覆盖 |
| src/usage.rs | 216 | `UsageStore::budgets_snapshot` | fn (method) | 1（内部 `persist_to` :474） | 未接线组：随 `persist_to` 接线 |
| src/usage.rs | 228 | `UsageStore::restore_budgets` | fn (method) | 1（内部 `load_from` :511） | 未接线组：随 `load_from` 接线 |
| src/usage.rs | 240 | `UsageStore::force_hard_stop` | fn (method) | 1（内部 `load_from` :514） | 未接线组：随 `load_from` 接线 |
| src/usage.rs | 417 | `reset_global_for_test` | fn (pub) | 5（测试 usage_tests） | 测试可达；与 slo.rs:315 同名，需人工复核 |
| src/usage.rs | 457 | `USAGE_SNAPSHOT_FILE` | const (pub) | 2（内部 persist_to/load_from） | 未接线组：随持久化接线 |
| src/usage.rs | 461 | `USAGE_SNAPSHOT_VERSION` | const (pub) | 1（内部 persist_to） | 未接线组：随持久化接线 |
| src/usage.rs | 466 | `persist_to` | fn (pub) | 1（测试 usage_tests.rs:91） | 测试可达；模块头自述"R10 持久化完成，**待主控接线**"（定时落盘/优雅关闭）→ 优先接线而非删除 |
| src/usage.rs | 487 | `load_from` | fn (pub) | 1（测试 usage_tests.rs:93） | 测试可达；待主控启动恢复接线；**注意 core 的 memory.rs:120 / observe.rs:48 存在同名 `load_from`（不同物），同名需人工复核** |

### owo-agent-server —— workflow_backend.rs / logging.rs / 测试 crate

| 文件 | 行 | 项 | 类型 | 定义外引用数 | 初步结论 |
|---|---:|---|---|---:|---|
| src/workflow_backend.rs | 353 | `ServerActionBackend.act_stub` | 字段 (pub) | lib 内 1 处读（:466），但 lib 目标内**无写入路径**（写入仅经 `with_act_stub`，测试专用） | 条件性死写：allow 在 `with_act_stub` 未进 lib 前必要，保留合理；接线后随 :366 一并清理 |
| src/workflow_backend.rs | 366 | `ServerActionBackend::with_act_stub` | fn (method) | 2（测试 workflow_api_tests.rs:1035/1044） | 测试可达、lib 内死；测试桩 API（`#[path]` 使用） |
| src/workflow_backend.rs | 571 | `write_workspace_file` | fn (pub) | **0** | **疑似死代码**：注释自称"测试与后端共用"，但全仓（含测试）无引用；删除候选，需人工复核 |
| src/logging.rs | 339 | `_probe_format` | fn | 0 | 编译占位探针（std::fmt::Write 路径类型完整，供 #[path] 独立编译）；有意保留 |
| tests/product_eval_api_tests.rs | 83 | `TestEnv.hub` | 字段（测试 crate） | 0 | 保活字段（Router/HUB 生命周期与 tempdir 绑定），allow 惯例合理 |
| tests/product_eval_api_tests.rs | 86 | `TestEnv.runs_root` | 字段（测试 crate） | 0 | 保活字段（tempdir drop 顺序依赖），allow 惯例合理 |
| tests/usage_tests.rs | 15 | `mod usage`（`#[path]` 整模块挂载） | mod | 多（测试目标） | 测试目标级 blanket：测试只覆盖 UsageStore/预算子集，路由面在 lib 接线；注释已自证非死区，惯例可保留 |

---

## 下一步建议（按 crate 分批清理顺序）

**原则**：每批改动后跑 `cargo check` + `cargo clippy -- -D warnings` + `cargo fmt --check`，涉及路由面的批次同步 `tests/route_contract_tests.rs`；先移除"已接线却仍 allow"的过期豁免（零行为风险），再处置真死代码（需产品决策），最后收窄 blanket。

1. **第一批（server，零风险：清理过期 allow，不改符号）**
   - `observability_api.rs` 5 处：ingest_metrics_sample / register_slo_report_probe / register_usage_probe / register_slo_alerts_probe / register_slo_period_probe —— lib.rs:315-323 接线函数已在调用，删除 allow 前以 clippy 验证。
   - `fleet_api.rs` 2 处：fleet_hub / router —— lib.rs:549 已挂载、goal_api 已复用。
   - `lib.rs:61` 的 `#[allow(unused_imports)]` —— 再导出已被 goal_api 与测试使用。
   - `core/tests/artifact_review_tests.rs:67` —— 测试内全变体使用，allow 疑似过期。
2. **第二批（server，真死代码候选：接线或删除二选一）**
   - observability_api 遥测组：set_telemetry_enabled / record_telemetry_counter / record_telemetry_error / record_event_lagged（0 引用）；reset_slo_alerts_probe_for_test / reset_slo_period_probe_for_test（探针已接线但 reset 无用）。
   - slo.rs：reset_alert_listener_for_test / reset_alert_registry_for_test。
   - notes_api.rs：block_text_debug；workflow_backend.rs：write_workspace_file；lib.rs：CONTRACT_RFC_LOG（改文档注释）；event_stream.rs：last_delivered 方法（SSE 续传接线决策）。
3. **第三批（server，R10 持久化组：优先接线而非删除）**
   - usage.rs persist_to/load_from 及其内部依赖（push_record / budgets_snapshot / restore_budgets / force_hard_stop / USAGE_SNAPSHOT_FILE / USAGE_SNAPSHOT_VERSION）——模块头明确"待主控接线"，建议在主控接线任务（定时落盘 + 启动恢复）落地后整组摘除 allow。
4. **第四批（server，blanket 收窄：难度最高）**
   - 8 个模块级 `#![allow(dead_code)]`（team_api 模式）逐模块处置：优先完成路由/接线（error_codes、idempotency 目前 lib 仅登记模块），接线后整块移除；无法立即接线的模块（logging 的 Redactor 系、event_stream/slo 的测试面符号）把 blanket 收窄为 item 级 allow 并逐条挂 TODO 锚点，使"真死"与"测试面"可被工具区分。
5. **第五批（core/cli，小批量）**
   - core/perception.rs `CaptureFrame.bytes`：仅写不读且常驻内存，建议人工复核（截断存储或补读取路径）——这是清单中唯一疑似"逻辑浪费"型死字段。
   - cli/worker_child.rs 两处：A2 白名单路径接手时同步摘除（注释已锁定语义）。
6. **长期保留项（不建议动）**：sandbox.rs `mod win` FFI blanket（B9）、storage_crypto `Unsupported`（cfg 条件性）、TestEnv 保活字段、plugin_market_api `SeedEntry` 协议保留字段、`_type_probe`/`_probe_format` 编译占位、两个 `#[path]` 测试目标 blanket。

---

### 方法局限（提示性结论声明）

- 浅可达性检查基于同名文本 grep：宏展开产物、方法与字段同名（如 `last_delivered`、`cas`、`history`、`router`、`load_from`、`reset_global_for_test`、`sse_frame_text`）可能互相干扰，凡同名多义处均已标注"需人工复核"，不作为删除依据。
- "定义外引用数"把 `tests/` 引用单独归类为"测试可达"，避免把测试目标使用误判为生产可达。
- 本次未运行 `cargo`/编译器 lint 交叉验证（只读约束），"allow 疑似过期"类结论以 clippy 实测为准。

---

## 批次一执行结果（2026-09-10，clippy -D warnings 实测回填）

按本报告"第一批"清单执行，**7 处候选实测结果：4 处成功移除，3 处经门禁纠错保留**。核心方法论结论：**"allow 疑似过期"必须经 clippy 实测裁决——#[path] 独立编译测试目标内的死亡与 lib 目标接线与否是两套判据**。

| 项 | 结果 | 说明 |
|---|---|---|
| observability_api `ingest_metrics_sample`（:147） | ✅ 已移除 | lib.rs:315 接线真实存在 |
| observability_api `register_slo_report_probe`（:356） | ✅ 已移除 | lib.rs:317 接线真实存在 |
| observability_api `register_usage_probe`（:697） | ✅ 已移除 | lib.rs:321 接线真实存在 |
| lib.rs:63 `#[allow(unused_imports)]`（fleet 再导出） | ✅ 已移除 | `pub use` 公开面不触发 unused_imports |
| fleet_api `fleet_hub`（:296） | ❌ 保留（注释已修正） | **#[path] 测试目标 fleet_api_tests 内无 lib 接线 → 目标内真死**；与 B2/B7 同机制，报告"疑似过期"判定有误 |
| fleet_api `router`（:368） | ❌ 保留（注释已修正） | 同上（测试目标内无挂载方） |
| artifact_review_tests `ReviewStateForTest`（:67） | ❌ 保留（已加复核注释） | 变体未全部被构造（映射不构成构造），dead_code 实测触发；报告":44-48 均被构造"判定有误 |

给后续批次的修正判据：
1. 判断"allow 过期"前先确认**该符号所在 crate 的所有编译目标**（lib + 每个 #[path] 测试目标）内是否均有引用——只有全目标可达才可移除。
2. `#[derive]` 不参与 dead_code 构造判定；"字段/变体被映射或读取"≠"被构造"。
3. B2/B7 模式（allow 动机 = #[path] 测试目标）在 fleet_api 同样成立，后续批次对含 #[path] 测试目标的模块应直接按 B2/B7 处置，不再作为"过期候选"。

---

## 批次二执行结果（2026-09-10，clippy 实测回填）

按本报告"第二批"执行。**8 处符号删除 + 1 处常量转文档 + 2 处 allow 恢复；净移除 allow 10 处**（批次一 4 + 批次二 8 − 恢复 2）。

| 项 | 处置 | 门禁结果 |
|---|---|---|
| observability_api `record_event_lagged` | 删除 | ✅ 全目标绿 |
| observability_api `reset_slo_alerts_probe_for_test` / `reset_slo_period_probe_for_test` | 删除（0 引用确认） | ✅ |
| observability_api `register_slo_alerts_probe` / `register_slo_period_probe` 过期 allow | 移除→**恢复** | ❌ 移除后 observability_tests 目标内 dead_code（仅 lib 接线、测试不调用）——B2/B7 判据第 3 次验证；注释已改为准确动机 |
| slo.rs `reset_alert_listener_for_test` / `reset_alert_registry_for_test` | 删除 | ✅ |
| notes_api.rs `block_text_debug` | 删除 + 级联清理 `block_text`/`BlockId` 导入 | ✅ |
| workflow_backend.rs `write_workspace_file` | 删除 + 级联清理 `std::path::Path` 导入 | ✅ |
| lib.rs `CONTRACT_RFC_LOG` | 常量删除，RFC 内容并入 `DEPRECATED_ROUTES` 文档注释 | ✅ |
| 遥测三件套（set_telemetry_enabled/record_telemetry_counter/record_telemetry_error） | **暂缓** | 删除牵连 `telemetry_snapshot`（静态量 TELEMETRY_COUNTERS/ERROR_CODES 仍被快照聚合读取）——需整体产品决策（整个遥测子面接线或删除），不适合机械批次 |

批次二方法论补充：删除符号后必须检查**同文件未用导入级联**（rustc 的 unused_imports 会以错误级触发）；#[path] 测试目标的 dead_code 判定独立于 lib 目标，删除前后都要全目标 clippy。

---

## 批次四执行结果（2026-09-10，blanket 收窄 + 机制结论回填）

server 9 处 blanket 的处置全部经 clippy 全目标实测：

| blanket | 处置 | 实测结论 |
|---|---|---|
| B6 logging.rs | **收窄为 7 个 item 级 allow** | 模块头自述"lib 仅用 TraceId/emit/Level"不完整——clippy 暴露出 Trace/Debug 变体、init/close_file_logging、audit_event、safe_field 共 5 项 lib 内亦死（R10 文件日志组**待主控接线**，同 usage 持久化组判据，挂"待接线"注释） |
| B8 slo.rs | **blanket → `#![expect(dead_code)]`** | 55 符号整组待接线，逐项 item 化噪声大；expect 在 lib 与测试目标均满足（每目标内确有死亡符号）；接线完成后 expect 失效即告警，-D 下强制清理 |
| B4 event_stream.rs | **blanket 保留（注释升级）** | 两方案实测均否决：lib 目标全符号可达（移除→测试目标死）→ expect 不满足；测试目标内部分符号死（expect→lib 目标不满足）。**#[path] 双目标差异场景 blanket 是唯一正确形态** |
| B3 error_codes.rs / B5 idempotency.rs | **allow 保留（注释升级）** | lib 全死 + #[path] 测试目标全活的镜像场景，expect 同样不适用（error_codes_tests 内全活实测证伪） |
| B1 team_api / B2 auth_token / B7 rate_limit | 见下行 | 已复核 |
| B1 team_api.rs | **blanket → `#![expect(dead_code)]`** | router 已 merge（B1"待接线"注释过时）；clippy 实测 lib 与测试目标内均有死符号，expect 两目标成立，接线/使用后强制清理 |
| B2 auth_token.rs / B7 rate_limit.rs | **allow 保留（注释升级）** | clippy 实测 lib 内零死符号（expect 不成立）+ 测试目标内中间件死——#[path] 双目标差异场景，与 event_stream 同判据 |

**机制结论（供后续所有批次复用）**：
1. `#![expect(dead_code)]` 适用于"整组待接线、每目标内确有死亡符号"的模块——它把 allow 从静默豁免升级为**接线后强制清理**的契约；
2. `#[path]` 双目标差异（lib 活/测试死或反之）只能用 allow——expect 按单目标期望判定，双目标场景必有未满足侧；
3. blanket 的注释必须写明**在哪一目标内豁免什么**，否则下一次复核仍要重新实测。

---

## 批次三 + 批次五执行结果（2026-09-10，clippy 全目标实测回填）

| 批次 | 处置 | 门禁结果 |
|---|---|---|
| 批次三：usage.rs R10 持久化组接线 | 主控三面接线完成：`restore_usage_snapshot`（启动恢复）+ `start_usage_persistence_loop`（每小时定时落盘，spawn_blocking 包裹）+ `persist_usage_snapshot`（优雅关闭，flush_audit 后调用）；接线点在 cli serve 生命周期区（与 start_automation_loop 同批派生） | ✅ 摘除 allow 9 处（push_record / budgets_snapshot / restore_budgets / force_hard_stop / USAGE_SNAPSHOT_FILE / USAGE_SNAPSHOT_VERSION / persist_to / load_from），lib 与 #[path] 测试目标双绿 |
| 批次五：core/perception.rs `CaptureFrame.bytes` | **删除**（写入后全仓零读取——OCR 在构造前用局部变量完成；字段保留使截图字节常驻环形缓冲，违背"仅内存、用后即毁"隐私语义）；两处构造点同步移除，`begin_capture_bytes` 显式 `drop(bytes)` | ✅ |
| 批次五：cli worker_child 两函数 | 保留（A2 白名单路径绑定，注释已锁定） | — |

模块头"待主控接线"注释已同步更新为接线完成状态。仓库内权威台账落在 `agent-sdk/docs/internal/dead-code-inventory.md`（批次记录 + 剩余未决项 + 机制结论）；本文件保留为 71 处明细快照。剩余待决策项收敛为：event_stream `last_delivered`（SSE 续传接线决策）一项。

> **批次六补记（同日）**：遥测子面选择**接线而非删除**——Settings `telemetry_enabled` 字段 + 启动/设置变更双点应用 + turn 入口与错误出口两点位打点；`/metrics/telemetry/status` 既有暴露面不变。三函数 allow 经双目标实测后保留（#[path] 测试目标不调用），注释升级为"已接线"。allow 总量 66→57。
