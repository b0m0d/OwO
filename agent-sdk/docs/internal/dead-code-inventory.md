# 死代码清单（§13.1）——逐处明细与批次台账

> 对应审计：`builGoal/Agent-SDK-全项目问题审计与技术重构方案-2026-09-05.md` §13.1（P2：死代码与仓库卫生）。
> 扫描快照：2026-09-11 03:05 首扫，03:20 复扫校准（批次三/六落地后）。本仓库正由多 agent 并行执行 §13 批次，行号以本快照为准；后续批次摘除 allow 后请回填本表。
> 扫描范围：`agent-sdk/` 全部 `.rs`（`crates/*/src`、`crates/*/tests`、`desktop/tauri/src-tauri/src`）。
> 扫描模式：`#[allow(dead_code)]`、`#![allow(dead_code)]`、`#[cfg_attr(..., allow(dead_code))]`、`#![expect(dead_code)]` 及含 `dead_code` 的复合 blanket。
> 完整 71 处历史快照（含逐项引用统计与初步结论）见 `builGoal/Agent-SDK-死代码清单-§13.md`；本文件是仓库内**逐处明细 + 批次台账**的权威记录。
> 红线声明：本文档为只读分析产物（批次执行记录除外），不含 API 密钥或任何环境变量值。

## 判定流程（引自审计 §13.1，共 5 步）

1. 用 `rg`、Rust 编译器、route 表、CLI 子命令和前端入口联合确定可达性。
2. 仅测试使用但仍有价值的 helper 移到 `#[cfg(test)]`。
3. 规划功能但无入口的代码删除或移入明确 feature，不允许长期靠 allow 保存。
4. 兼容分支必须写明支持到哪个版本和删除日期。
5. 每批只处理一个领域，删除后运行 workspace check、strict clippy、route contract、前端测试和相应 E2E。

目标不是清零，而是**每一处都有明确、可审查的理由；生产模块中无说明的 blanket allow 必须清零**。

### 结论分类口径

| 标记 | 含义 | 处置方向 |
|---|---|---|
| **A** | 生产可达但被豁免（allow 疑似过期/注释失实） | 摘除 allow 或补真实引用，列入下一批，clippy 全目标实测裁决 |
| **B** | 仅测试使用 | 移入 `#[cfg(test)]`，或豁免注明 "test-only"（`cfg_attr(not(test))` 为范例形态） |
| **C** | 规划功能保留 | 必须注明 feature/入口计划与兼容要求，不允许无限期搁置 |
| **D** | 可删除候选（低风险） | 下一批直接删除并跑门禁 |
| **保留（有说明）** | allow/expect 行自带注释说明理由，且理由经本表复核属实 | 摘录理由留档；理由失实的升级为 A 并标注 ⚠ |

### 本仓库实测沉淀的两条机制判据（批次一~四，clippy -D warnings 实测）

1. **`#[path]` 双目标差异**：同一源文件既编进 lib 目标又经 `#[path] mod` 编进独立测试目标时，"lib 内可达"与"测试目标内可达"是两套判据——"allow 疑似过期"必须经 **全编译目标 clippy** 裁决；`expect` 按单目标期望判定，双目标差异场景必有未满足侧，只能用 `allow`。
2. **`#[derive]` 不参与 dead_code 构造判定**：字段/变体"被映射或读取"≠"被构造"。

---

## 一、扫描口径与存量对账

| 口径 | 数量 | 说明 |
|---|---:|---|
| 任务下发的存量清单（§13.1 建档时点） | 66 | `crates/` 内 `allow(dead_code)` + `cfg_attr(not(test), allow(dead_code))` |
| − 建档时点前已摘除 | −9 | 批次五：perception.rs `CaptureFrame.bytes` 死字段删除（1 处，字段注释记录结论并指向本文件）；批次三：usage.rs R10 持久化组接线后摘除（8 处：4 方法 + 2 常量 + persist_to/load_from） |
| − 批次七（A 组） | −1 | `subscribe_with_capacity` allow 摘除（双目标均活，clippy 实证） |
| − 批次八（logging 组） | −6 | 文件日志/脱敏面接线后摘除（Redactor/safe_field/sanitize_json/init/close/audit_event；Trace/Debug 预留 2 处保留） |
| − 批次九（observability 组） | −1 | `record_tool_duration_ms` 接线后摘除（turn_api ToolStart/ToolResult 配对计时） |
| **= 主口径现存（§三 明细表）** | **49** | 全部逐处给出结论，无抽样 |
| + 变体豁免（§三.5 附录） | +4 | `expect(dead_code)` 2 处（team_api、slo）、复合 blanket 1 处（storage_crypto `mod win_dpapi`）、desktop/tauri 1 处（批次七删除 `spawn_watch_thread` 后余 1） |
| **= 全仓现存豁免点合计** | **53** | |

---

## 二、汇总统计

### 主口径（49 处，crates/ 内 allow + cfg_attr；按批次七/八/九台账行对账）

| 结论 | 数量 | 占比 |
|---|---:|---:|
| A 生产可达但被豁免（注释失实，疑似过期） | 0 | 0% |
| B 仅测试使用（含"#[path] 测试播种"形态） | 20 | 41% |
| C 规划功能保留 | 4 | 8% |
| D 可删除候选 | 0 | 0% |
| 保留（有说明） | 25 | 51% |
| **合计** | **49** | 100% |

### 全仓（53 处，含附录变体）

| 结论 | A | B | C | D | 保留（有说明） | 合计 |
|---|---:|---:|---:|---:|---:|---:|
| 数量 | 0 | 20 | 4 | 0 | 29 | 53 |

生产模块中"无说明 blanket allow"现状：**0 处**——现存 6 处 blanket（auth_token、error_codes、event_stream、idempotency、rate_limit、sandbox `mod win`）与 2 处 expect（team_api、slo）全部自带说明注释（其中 error_codes 的说明滞后于接线现状，见明细 ⚠ 行）。

---

## 三、明细表

列说明：**生产引用** = lib/bin 目标内真实调用（文件:行）；**测试引用** = `tests/` 集成测试或 `#[cfg(test)]` 单测；**入口** = route 注册 / CLI 子命令 / 前端 fetch。引用关系基于本快照全仓同名 grep + 人工排歧（同名多义处已注明"同名不同物"）；"疑似过期"结论以 clippy 全目标实测为准。

### 三.1 owo-agent-cli（3 处）

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `partition_credential_keys` | src/worker_child.rs:127 | 无（bin 侧 dispatch 不调用） | worker_child_tests.rs:339 | CLI 无入口（A2 白名单路径规划中） | 保留（有说明） | 行注释："宿主侧防线由契约测试锁定；正式白名单路径（A2）接手前显式保留"。A2 落地时同步摘除 |
| `build_child_command` | src/worker_child.rs:149 | 无（宿主演示路径经 WorkerPool 组装命令） | worker_child_tests.rs:364 | CLI 无入口（A2 执行目标适配层规划中） | 保留（有说明） | doc 注释："本函数为契约测试与 A2 执行目标适配层保留（语义锁定：零继承 + 固定 flags）" |
| `mod worker_child`（`#[path]` 整模块挂载） | tests/worker_child_tests.rs:21 | —（测试 crate） | 测试目标内多处 | — | 保留（有说明） | 测试目标级 blanket：测试只用部分公开函数，其余入口（dispatch/run_child 等）由二进制侧使用；注释自述非死区 |

### 三.2 owo-agent-core（4 处）

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `StorageCryptoError::Unsupported` 变体 | src/storage_crypto.rs:64 | 条件性：`encrypt_blob`/`decrypt_blob` 的 `#[cfg(not(windows))]` 分支构造（:91/:105） | 无专项测试 | — | 保留（有说明） | Windows 构建下该变体不构造故触发 lint；非 Windows 构建实际使用。显式"不静默降级"契约（DPAPI 仅 Windows），不可删 |
| `mod win`（Windows 裸 FFI 层 blanket） | src/sandbox.rs:1015 | 部分 FFI 绑定被本模块 Windows 实现调用；其余为 Windows SDK 结构/常量镜像（逐绑定待核） | 模块内含 repr(C) 尺寸断言测试 | — | 保留（有说明） | 平台绑定惯例：结构布局对齐 Windows SDK（kernel32/advapi32/ntdll），同处豁免 `non_camel_case_types`/`upper_case_acronyms`；整层收窄成本高于收益，低优先级 |
| `mod win_dpapi`（DPAPI FFI 层，复合 blanket） | src/storage_crypto.rs:518 `#![allow(clippy::upper_case_acronyms, dead_code)]` | protect/unprotect 被加密路径调用；部分绑定/常量为 SDK 镜像 | — | — | 保留（有说明） | 平台 FFI 惯例（与 sandbox `mod win` 同款）；计入附录口径 |
| `ReviewStateForTest`（测试 crate 枚举） | tests/artifact_review_tests.rs:69 | —（测试 crate） | 本测试文件内映射/比较 | — | 保留（有说明） | 行注释（批次一复核）："此 allow 非过期——枚举变体未全部被构造（映射不构成构造），移除会触发 dead_code；保留并在此记录复核结论" |

### 三.3 owo-agent-server —— src（48 处）

**模块级 blanket（5 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| 整模块（AuthToken/require_auth/auth_token_bootstrap 等） | src/auth_token.rs:17 | lib 全链路可达：lib.rs:229（`AuthToken::load_or_create`）、:347（`/auth/token` 路由）、:639（require_auth 中间件）、:721（配对头）、:1481；ops_api.rs:46-48 | auth_token_tests（#[path] 目标内中间件等未被测试调用） | route: `GET /auth/token`（引导）+ 全局鉴权中间件 | 保留（有说明） | 行注释（第四批 clippy 全目标实测）：lib 目标零死亡、expect 不成立；#[path] 双目标差异场景 allow 为唯一正确形态（同 event_stream.rs） |
| 整模块（ErrorCode/from_code/lookup/code 等） | src/error_codes.rs:15 | **部分接线**：usage.rs:675（`ErrorCode::from_code`）、lib.rs:1823-1835（`api_error_response` 签名与字段读取）、lib.rs:1834（遥测错误码打点） | error_codes_tests、production_readiness_tests（#[path] 全用） | route: 统一错误响应体 `{error:{code,...}}`（HTTP 错误面） | 保留（有说明）⚠ | 行注释"lib 目标当前仅登记模块（无路由引用）"**已滞后**（lib 已部分接线）；`http_status()`/`retry_after()`/`to_json()`/`code()` 是否仍 lib 死需 clippy 实测，收窄复核列入下一批 |
| 整模块（IdempotencyRegistry 等） | src/idempotency.rs:15 | 无（lib.rs:48 仅登记模块，无路由引用） | idempotency_tests、production_readiness_tests（#[path] 全用） | route: 无（幂等端点未接入） | C | 行注释："幂等端点接入后随测试目标一并复核"。规划入口=请求级幂等中间件/端点；接入时同步 `tests/route_contract_tests.rs` |
| 整模块（enforce_rate_limit/RateLimitConfig 等） | src/rate_limit.rs:17 | lib 可达：lib.rs:643（enforce_rate_limit 中间件） | rate_limit_tests（#[path] 目标内中间件未调用） | route: 全局中间件（/command、/subagent/run、/team/import 等敏感面） | 保留（有说明） | 同 auth_token：第四批实测，双目标差异场景 allow 为唯一正确形态 |
| 整模块（EventStreamHub/router/StreamEvent 等） | src/event_stream.rs:25 | lib 全链路可达（router 经 build_router merge，hub 全局单例） | event_stream_tests/observability_tests（#[path] 目标内 KIND_*、publish_alert、InvalidateDomain 等未用） | route: `/events/stream`（SSE） | 保留（有说明） | 行注释（第四批实测）：allow 在 lib 内零触发、仅豁免测试目标局部死亡；任一目标必有未满足期望，expect 不适用，blanket 为双目标差异唯一正确形态 |

**event_stream.rs（7 处 item 级）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `EventStreamHub::last_delivered()` | :178 | 0 引用（全仓无调用） | 0 引用 | route: 无（Last-Event-ID 续传未接线） | C | 行注释："接线方在 SSE 断线续传时使用；当前 lib 目标与测试内无引用"。规划入口=SSE 断线续传（`subscribe_after` 重放已备）；续传方案若调整则转 D |
| `reset_metrics_observer_for_test()` | :255 | 无 | event_stream_tests:382-525、observability_tests:596/629 | — | B | 行注释：仅供 event_stream_tests / observability_tests 以 #[path] 独立编译使用（test-only） |
| `EventStreamHub::subscribe_with_capacity()` | :404 | **lib 可达**：同模块 `subscribe_after`:400 调用（生产订阅路径） | event_stream_tests:139/162/198-199/216/432/456/486/506 | route: 间接（/events/stream 订阅） | **A** | 行注释"仅供 event_stream_tests…lib 目标内无引用"**与代码矛盾**（同模块 subscribe_after 构成真实引用）；allow 疑似过期，待 clippy 全目标实测后摘除 |
| `reset_hub_for_test()` | :608 | 无 | event_stream_tests:232、observability_tests:595 | — | B | test-only（与 sse.rs:228 同名不同物） |
| `sse_frame_text()` | :630 | 无 | event_stream_tests:258 | — | B | test-only（SSE 帧格式断言；与 sse.rs:171 同名不同物） |
| `sse_response_ok()` | :718 | 无 | event_stream_tests:247 | — | B | test-only（与 sse.rs:234 同名不同物） |
| `_type_probe()` | :729 | 0 引用（有意保留） | 0 | — | 保留（有说明） | 编译占位：保障 #[path] 独立编译时 IntoResponse 路径类型完整 |

**fleet_api.rs（3 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `FleetHub.cas` 字段 | :242 | 无读取（仅构造时写入） | fleet_api_tests 构造时赋值 | route: /fleet/*（控制面） | 保留（有说明） | 行注释：R12 节点显式驱动阶段尚未消费 CAS；R13 起节点侧经 HttpTransport 接线产物写入，保留字段维持控制面"内容寻址产物"契约。兼容要求=R13 协议字段，删除需契约评审 |
| `fleet_hub()` | :297 | lib 可达：lib.rs:88 再导出、goal_api.rs:958/999、router():372 | fleet_api_tests（#[path] 目标内无 lib 接线 → 目标内真死） | route: /fleet/*（经 build_router） | 保留（有说明） | 行注释（批次一 clippy 实测保留）：lib.rs build_router 与 goal_api 接线均使用本函数；allow 仅为 #[path] 测试目标豁免 |
| `router()` | :370 | lib 可达：lib.rs:631 `.merge(fleet_api::router(state.clone()))` | fleet_api_tests（目标内无挂载方） | route: /fleet/* 全组 | 保留（有说明） | 行注释（批次一实测保留）：路由已挂载；allow 仅为 #[path] 测试目标豁免 |

**human_inbox_store.rs（2 处，cfg_attr 范例形态）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `HumanInboxStore::len()` | :430（`#[cfg_attr(not(test), allow(dead_code))]`） | 无（lib 生产路径未调用） | 本文件 #[cfg(test)] 单测（:494/:518/:530/:548/:554/:740） | route: 无（自省 API，未暴露） | B | 行注释："resolved 记录数（自检/统计；当前仅测试消费，保留为公开自省 API）"。cfg_attr 按编译目标收窄豁免面，为 test-only 类的范例形态 |
| `HumanInboxStore::is_empty()` | :435（同上） | 无 | 本文件 #[cfg(test)] 单测（:732） | 同上 | B | 同上（内部调 `len()`） |

**logging.rs（9 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `Level::Trace` 变体 | :34 | 0 构造 | 0（tests/ 无 logging 引用） | — | C | 行注释："待接线（文件日志/详细诊断级别启用时使用）"。规划入口=R10 文件日志/诊断级别配置；`as_str()` 内的 match 不构成构造 |
| `Level::Debug` 变体 | :37 | 0 构造 | 0 | — | C | 同上 |
| `Redactor` | :58 | 传递性死：仅被 `safe_field`/`sanitize_json` 调用，二者 0 调用方 | 0 | — | C ⚠ | 行注释称"测试面：#[path] 独立编译目标使用"，但当前**不存在 logging 测试目标**（tests/ 全域无引用）——注释滞后；规划入口=R10 脱敏日志面（与 audit_event 接线联动） |
| `init_file_logging()` | :207 | 0 引用 | 0 | — | C | 行注释："R10 文件日志初始化（待主控接线：serve 启动时可选落盘 + 大小轮转）" |
| `close_file_logging()` | :226 | 0 引用 | 0 | — | C | 行注释："R10 文件日志关闭（待主控接线：与 init_file_logging 配对）"——优雅关闭时调用 |
| `audit_event()` | :289 | 0 引用 | 0 | — | C | 行注释："审计可观测面日志（待主控接线：关键审计动作联动日志面）"；HMAC 审计链在 core `audit_chain`，此处为可观测面 |
| `safe_field()` | :320 | 传递性死（仅 audit_event:298 调用） | 0 | — | C | 行注释："audit_event 联动接线前 lib 无调用方" |
| `sanitize_json()` | :327 | 0 引用 | 0 | — | C ⚠ | 行注释称测试面但无测试目标（滞后）；规划入口=R10 诊断输出脱敏 |
| `_probe_format()` | :355 | 0 引用（有意保留） | 0 | — | 保留（有说明） | 编译占位：保障 std::fmt::Write 路径类型完整（供 #[path] 独立编译） |

**observability_api.rs（12 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `record_tool_duration_ms()` | :83 | **已接线（批次九）**：turn_api.rs `on_event` 闭包以 ToolStart/ToolResult id 配对计时 | observability_tests:347/667-669 | route: 数据面经 `/metrics/runtime` 暴露 | 摘除 | allow 已摘（lib 与 #[path] 测试目标两侧均有真实引用）；双目标判据下无未满足侧 |
| `record_sse_connection()` | :98 | 无——生产数据面已由 R7 MetricsSample 桥覆盖（lib.rs:326 `set_metrics_observer` → `ingest_metrics_sample`），直接调用会双重计数 | observability_tests:390-399/670 | 同上 | B | allow 注释已按"桥接取代"改写（批次九）；保留仅供 #[path] 测试播种状态 |
| `record_queue_depth()` | :119 | 无——同上（queue_depth 随样本快照更新） | observability_tests:392/671 | 同上 | B | 同上（批次九） |
| `record_events()` | :125 | 无——同上（published/dropped 随样本累加） | observability_tests:411-412 | 同上 | B | 同上（批次九） |
| `reset_runtime_metrics_for_test()` | :174 | 无 | observability_tests 20+ 处 | — | B | test-only（进程内跨测试隔离必需） |
| `reset_slo_report_probe_for_test()` | :353 | 无 | observability_tests:483-763 | — | B | test-only |
| `reset_usage_probe_for_test()` | :693 | 无 | observability_tests:713/771/802 | — | B | test-only |
| `set_telemetry_enabled()` | :715 | lib 可达：lib.rs:1617（`apply_telemetry_setting`）← settings_api.rs:248（POST /settings）+ cli serve 启动应用（批次六） | observability_tests 目标内不调用 | route: `/settings`（写）+ `/metrics/telemetry/status`（读） | 保留（有说明） | doc 注释（批次六升级）："已接线…#[path] observability_tests 目标内不调用该函数——双目标差异场景保留 allow（expect 按单目标判定，必有未满足侧）" |
| `record_telemetry_counter()` | :726 | lib 可达：turn_api.rs:167（回合入口打 "turn" 计数，批次六） | observability_tests 目标内不调用 | route: /command 回合链路 | 保留（有说明） | doc 注释："已接线：turn_api 回合入口打 'turn'；默认关时零开销；#[path] 目标内不调用——双目标差异场景保留 allow" |
| `record_telemetry_error()` | :739 | lib 可达：lib.rs:1834（`api_error_response` 统一错误出口打点，批次六） | 同上 | route: 全部错误响应 | 保留（有说明） | doc 注释："已接线：lib api_error_response 错误响应统一出口打点；默认关时零开销"（同双目标差异判据） |
| `register_slo_alerts_probe()` | :814 | lib 可达：lib.rs:334 | observability_tests 目标未调用 | route: `/metrics/slo`（告警面） | 保留（有说明） | 行注释：lib.rs 接线函数已调用；observability_tests 目标内无 lib 接线亦无测试调用，故保留 allow（双目标差异，批次二实测恢复） |
| `register_slo_period_probe()` | :827 | lib 可达：lib.rs:335 | 同上 | route: `/metrics/slo/report?days=` | 保留（有说明） | 同上 |

**plugin_market_api.rs（1 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `SeedEntry` | :151 | 结构可达：`SeedBody.entries`:147（seed 请求体反序列化） | 无 | route: `/plugins/market/seed` | 保留（有说明） | 行注释："协议保留字段（seed 表单/未来服务端使用）"。allow 实为豁免其仅反序列化不读取的 `#[serde(default)]` 字段；删除属协议破坏性变更，需走弃用流程 |

**slo.rs（1 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `reset_global_for_test()` | :316 | 无 | slo_tests:163-186、observability_tests:484-577 | — | B | 行注释：仅供 slo_tests / observability_tests 以 #[path] 独立编译使用（test-only；与 usage.rs:419 同名不同物；跨测试隔离必需） |

**sse.rs（5 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `CloudSseHub::history()` | :82 | 无（生产历史重放经 `subscribe` 返回值，cloud_task_events:180；本读取器方法未被调用） | cloud_sse_tests:105-255（多处） | route: `/cloud/tasks/{id}/events` | B | 行注释：仅供 cloud_sse_tests 以 #[path] 独立编译使用；lib 目标内无引用（test-only） |
| `sse_frame_text()` | :171 | 无 | cloud_sse_tests:234 | — | B | test-only（与 event_stream.rs:630 同名不同物） |
| `reset_hub_for_test()` | :228 | 无 | cloud_sse_tests:88-209 | — | B | test-only |
| `sse_response_ok()` | :234 | 无 | cloud_sse_tests:204/230 | — | B | test-only |
| `_type_probe()` | :245 | 0 引用（有意保留） | 0 | — | 保留（有说明） | 编译占位：保障 IntoResponse 路径在独立编译测试中类型完整 |

**usage.rs（1 处；R10 持久化组 8 处 allow 已由批次三接线后摘除）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `reset_global_for_test()` | :419 | 无 | usage_tests:55-103 | — | B | 行注释：仅供测试/接线方以 #[path] 独立编译使用（test-only；与 slo.rs:316 同名不同物；跨测试隔离必需） |

**workflow_backend.rs（2 处）**

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `ServerActionBackend.act_stub` 字段 | :352 | lib 内 act 分派有读取路径，lib 无写入路径（写入仅经 `with_act_stub`，测试专用） | workflow_api_tests:1035-1044 | route: /workflow 执行面（桩注入仅测试） | B | 行注释："测试桩 API（测试 crate 经 #[path] 使用，lib 目标未直接调用）"。安全语义=测试不触真实桌面执行；接真实桩入口后随 :365 一并清理 |
| `ServerActionBackend::with_act_stub()` | :365 | 无 | workflow_api_tests:1035/1044 | — | B | 同上（测试桩注入构造器） |

### 三.4 owo-agent-server —— tests（3 处）

| 符号 | allow 行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| `TestEnv.hub` 字段 | tests/product_eval_api_tests.rs:83 | —（测试 crate） | 0 读取（保活） | — | 保留（有说明） | 行注释："持有所有权保活（Router/HUB 生命周期与 tempdir 绑定），测试体不读取" |
| `TestEnv.runs_root` 字段 | tests/product_eval_api_tests.rs:86 | — | 0 读取（保活） | — | 保留（有说明） | 行注释："同上：保活 runs_root 目录句柄语义（tempdir drop 顺序依赖）" |
| `mod usage`（`#[path]` 整模块挂载） | tests/usage_tests.rs:15 | — | 测试目标内多处 | — | 保留（有说明） | 行注释：本测试只覆盖 UsageStore/预算熔断子集；路由/HTTP 处理器在主 crate lib.rs build_router 内接线（`.merge(usage::usage_router(...))`），此处并非死区 |

### 三.5 附录：变体豁免（5 处，计入全仓 62 口径）

| 符号 | 属性行 | 生产引用 | 测试引用 | 入口 | 结论 | 理由/兼容要求 |
|---|---|---|---|---|---|---|
| 整模块 team_api | src/team_api.rs:18 `#![expect(dead_code)]` | router 已由 lib.rs build_router merge；模块内仍有死符号 | team_api_tests（#[path]） | route: /team/export、/team/review、/team/import、/team/versions、/team/audit | 保留（有说明） | 行注释（第四批实测）：blanket→expect——接线/使用完成后 expect 失效即告警，-D warnings 下强制清理本属性 |
| 整模块 slo | src/slo.rs:25 `#![expect(dead_code)]` | lib 仅 report_global 可达（经 observability_api 探针注册，lib.rs:329） | slo_tests/observability_tests（#[path]） | route: /metrics/slo、/metrics/slo/report | 保留（有说明） | 行注释：lib 目标仅引用 report_global，其余符号为测试面/待接线组；expect=接线后强制清理契约 |
| （win_dpapi 复合 blanket） | src/storage_crypto.rs:518 | 见 §三.2 | — | — | （计入 §三.2） | — |
| `_assert_core_api_version_in_scope()` | desktop/tauri/src-tauri/src/main.rs:330 | 0 引用（有意保留） | — | Tauri 壳 | 保留（有说明） | 行注释：CORE_API_VERSION 供后续 get_core_connection 比对；当前经 commands 返回 ready.apiVersion，此函数为编译期占位保持常量在作用域内 |
| `spawn_watch_thread()` | desktop/tauri/src-tauri/src/core_runtime.rs:636 | 0 引用（全仓仅定义处） | — | — | **D** | 行注释："已迁入 ChildGeneration::spawned 的空壳占位：外部历史调用点若仍引用可平滑编译"。当前全仓无引用，兼容假设已失效，确认桌面壳无外部调用后可删（低风险） |

---

## 四、批次执行状态（并行批次台账，保留自原工作台账记录）

| 批次 | 范围 | 状态 | 结果 |
|---|---|---|---|
| 一 | server 过期 allow（零风险） | ✅ 完成 | 4 处移除（ingest_metrics_sample / register_slo_report_probe / register_usage_probe / lib.rs fleet 再导出 unused_imports）；3 处经门禁纠错保留（fleet_hub/router——`#[path]` 测试目标内真死；ReviewStateForTest——derive 映射不构成构造） |
| 二 | server 真死代码（接线或删除） | ✅ 完成 | 删除 8 符号 + CONTRACT_RFC_LOG 转文档注释；遥测子面暂缓（牵连 telemetry_snapshot，需整体产品决策）；register_slo_alerts/period_probe 的 allow 移除后实测恢复（仅 lib 接线、测试目标不调用） |
| 四 | blanket 收窄 | ✅ 完成 | B6 logging 收窄为 item 级；B8 slo / B1 team_api 改 `#![expect(dead_code)]`（接线后强制清理）；B4 event_stream / B2 auth_token / B7 rate_limit / B3 error_codes / B5 idempotency 保留 allow（`#[path]` 双目标差异场景 blanket 是唯一正确形态） |
| 三 | server usage.rs R10 持久化组接线 | ✅ 完成 | `restore_usage_snapshot`（启动恢复）+ `start_usage_persistence_loop`（每小时定时落盘）+ `persist_usage_snapshot`（优雅关闭）三面接线完成；摘除 allow（4 方法 + 2 常量 + persist_to/load_from），全目标 clippy 验证通过 |
| 五 | core/cli 小批量 | ✅ 完成 | `perception.rs CaptureFrame.bytes` 死字段删除（写入后零读取，且违背 L2"用后即毁"隐私语义）；worker_child 两处为 A2 白名单路径绑定的有意保留（不动） |
| 六 | observability_api 遥测子面接线 | ✅ 完成 | **选择接线而非删除**：Settings 新增 `telemetry_enabled`（serde default，POST /settings 自动回写）；cli serve 启动应用（`apply_telemetry_setting`）+ settings_update 即时生效；打点两点位（turn_api 回合入口 "turn" 计数、lib `api_error_response` 错误码分布）——默认关时早退零开销；暴露面 `/metrics/telemetry/status`（既有路由含数据字典）不变。三函数 allow 保留（#[path] 测试目标不调用——双目标差异判据），注释升级为"已接线"状态 |
| 七 | 独立复查批（本文档 §六建议 1-3 的执行） | ✅ 完成 | A 类唯一项 `subscribe_with_capacity` allow 摘除（lib 经 subscribe_after:400 真实调用 + event_stream_tests 9 处——双目标均活，workspace clippy 实证；原注释"lib 目标内无引用"与代码矛盾）；D 类唯一项 desktop `spawn_watch_thread` 空壳删除（全仓 0 引用；独立 crate `cargo check` 绿）；⚠ 注释纠偏 3 处完成（error_codes.rs blanket 现状改写——lib 经 api_error_response 实际引用，blanket 豁免的是未构造错误码常量；logging.rs Redactor/safe_field/sanitize_json 由失实的"测试面使用"改为 C 类"R10 脱敏日志面规划入口"）。allow 主口径 57→56 |
| 八 | logging R10 文件日志/脱敏面接线（§六建议 4 的 logging 组） | ✅ 完成 | `init_server_file_logging`（serve 启动落盘 `logs/server.jsonl`，8MB×5 轮转）+ `close_server_file_logging`（优雅关闭配对）；emit 文件落盘路径新增脱敏边界——结构字段与 msg（第一方静态描述契约）保持原样，用户供给 fields 经 `sanitize_json` 包装按字段名走 Redactor 策略（未知字段保守哈希），stderr 保持完整原文；`audit_event` 三站点消费（自动化触发、服务启动/关闭生命周期，经 `logging_lifecycle_audit`）。摘除 allow 6 处（Redactor/safe_field/sanitize_json/init_file_logging/close_file_logging/audit_event）；Level::Trace/Debug 预留诊断级别保留 allow（2 处）。无 logging #[path] 测试目标（单目标判据）。allow 主口径 56→50 |
| 九 | observability 埋点组接线（§六建议 4 的 observability 组） | ✅ 完成 | **架构裁决先行**：复查发现 SSE/队列/事件三项的生产数据面已由 R7 MetricsSample 桥覆盖（lib.rs:326 `set_metrics_observer` → `ingest_metrics_sample` 活接线），直接调用 record_sse_connection/record_queue_depth/record_events 会双重计数——原"接入点=连接开/关回调"结论作废，三函数定性为"#[path] 测试播种工具"，allow 注释按"桥接取代"改写（保留，非摘除）。唯一真实待接线项 `record_tool_duration_ms` 在 turn_api.rs `on_event` 以 ToolStart/ToolResult id 配对计时接线（core TurnEvent 无 duration 字段，服务端配对为零语义侵入方案）；摘除其 allow（lib 与 observability_tests 双目标均活——双目标判据无未满足侧）。allow 主口径 50→49 |

## 五、剩余未决项（需产品决策或后续接线，不允许无说明长期搁置）

| 项 | 现状 | 决策待办 |
|---|---|---|
| event_stream `last_delivered` 方法 | 测试外 0 引用；自述供 SSE 断线续传接线 | SSE 续传接线时启用，否则删除 |
| workflow_backend `with_act_stub` / `act_stub` | 测试桩 API（`#[path]` 使用）；lib 写入路径缺位 | 接线后一并清理（注释已锚定） |
| cli worker_child 两函数 | A2 白名单路径接手前显式保留（契约测试语义锁定） | A2 落地时同步摘除 |
| core sandbox `mod win` FFI blanket | 平台绑定惯例 | 长期保留（B9） |
| storage_crypto `Unsupported` / plugin_market `SeedEntry` / `_type_probe` 系 / TestEnv 保活字段 | cfg 条件性 / 协议保留 / 编译占位 / 惯例 | 长期保留 |

## 六、下一批处理建议（按结论分组，每批一个领域 + 全目标门禁）

> 2026-09-10 批次七已执行本节 1-3 项（A 组摘除、注释纠偏、D 组删除）；批次八已执行第 4 项的 logging 组（文件日志/脱敏面 8 处中 6 处摘除 + Trace/Debug 预留保留）；批次九已执行第 4 项的 observability 组（record_tool_duration_ms 接线摘除；SSE/队列/事件三项经 R7 桥架构裁决定性为测试播种工具）。4 项余下子项与 5 项保持待办。

1. ~~**A 组（1 处，零行为风险，优先）**：`event_stream.rs:404 subscribe_with_capacity`~~ ✅ 批次七完成。
2. **⚠ 注释纠偏（不删 allow，只改注释）**：`error_codes.rs:15`（lib 已部分接线）、`logging.rs:58/:327`（所称 #[path] 测试目标已不存在）。
3. **D 组（1 处）**：`desktop core_runtime.rs:636 spawn_watch_thread`——全仓 0 引用，删除 + desktop 壳 `cargo check`。
4. **C 组按领域接线**（接线后随批摘除 allow）：logging R10 文件日志组（init/close/audit_event/safe_field/Redactor 簇 + Level::Trace/Debug，9 处中 8 处）；observability 埋点组（record_tool_duration_ms/record_sse_connection/record_queue_depth/record_events，接入后转双目标差异形态）；SSE 断线续传（last_delivered）；幂等端点（idempotency blanket，接入时同步 route_contract_tests）。
5. **长期保留项（不建议动）**：sandbox `mod win` / storage_crypto `mod win_dpapi`（平台 FFI 惯例）；`_type_probe`×2 / `_probe_format` / `_assert_core_api_version_in_scope`（编译占位）；`#[path]` 测试目标 blanket（worker_child_tests:21、usage_tests:15）；TestEnv 保活字段；`SeedEntry` 协议保留字段；`FleetHub.cas`（R13 契约）；`StorageCryptoError::Unsupported`（条件性 cfg）。

每批完成后回填本表：更新"allow 行"存在性与结论，并在文首快照时间处追加批次日期。

## 七、方法局限（诚实声明）

- 可达性判定基于本快照全仓同名 grep + 上下文人工排歧；方法与字段同名（`sse_frame_text`、`reset_hub_for_test`、`sse_response_ok`、`reset_global_for_test`、`history`、`load_from`）均已在表内注明"同名不同物"。
- `sandbox.rs:1015` FFI 层"部分绑定被调用"未逐绑定核对，标注待核；`error_codes.rs` 剩余死亡面（`http_status()`/`retry_after()`/`to_json()`/`code()`）未跑 clippy，标注待核。
- 本清单主体未运行 `cargo` 命令交叉验证（只读约束）；所有"A 疑似过期"结论须经 clippy 全目标（lib + 各 #[path] 测试目标）实测后方可动手，参见于批次一/二的纠错记录。
- 扫描期间仓库正被并行修改（perception.rs、usage.rs、observability_api.rs 在扫描窗口内被批次三/五/六更新，已按复扫校准）；若行号再度漂移，以符号名 + 文件定位为准。

## 附：§13.2 源码边界收纳记录（保留自原工作台账）

- 36 项未跟踪源码已 `git add` 暂存纳入审查范围（27 个 .rs：任务 12 域模块全集、deadline/mcp_health/schema_budget/ui_output、owo-build-info 整 crate；capabilities.panel.js、events.js、4 个 web 测试、tools/）。提交组织遵 §16.1 四独立提交计划，不混大提交。
- 剩余未跟踪：builGoal/docs 用户文档（不动）；`log/cuttle.png`（用户材料，建议移至 artifacts/dev/ 或删除由用户决定）。
