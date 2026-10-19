# Agent SDK P2.2 增量施工记录：SSE 有界队列与持久断线恢复

依据：`builGoal/Agent-SDK-后续任务实施指南-2026-09-18.md` §4.3、§8 P2.2。

## 范围与源码身份

- 基线 `HEAD`: `fc8251d42aa192715fa9f3208585802fa87d88ef`。
- 工作树：dirty；本记录只描述 P2.2 相关服务端、协议、存储、客户端与桌面端增量，不代表整个工作树已验收。
- `crates/owo-agent-server/src/turn_api.rs` SHA256：`D9C07EB02079890AB7ADC80B49483F51E010DC7D1178B012D2DAA0770DFA0D4F`。
- 本次没有生成/验收发布二进制；下列测试证明当前工作树的 Server 测试目标通过，不替代 P0/P6 产物身份验收。

## 本次完成

1. turn SSE 的无界 `mpsc` 替换为容量 128、序列化数据上限 1 MiB 的有界事件队列。
2. 队列积压达到容量一半后，合并相邻 `TokenDelta`，保持文本顺序；慢消费者持续追不上时停止接收新事件并设置 turn abort。
3. turn 收尾并排空已排队事件后，慢消费者可收到稳定码 `turn/sse_slow_consumer`；已断开的消费者不再接收事件并触发 abort。
4. 指标新增 `turn_sse_slow_consumers` / `turn_sse_disconnects`，同时导出 runtime JSON 与 Prometheus。
5. SQLite schema v3 持久化 turn 事件；同一 session 使用原子单调序号，支持 `turn_id + after_seq` 有界补拉。
6. 重放页明确区分 `active/completed/failed/interrupted`；活动身份按 session+turn 匹配，服务重启或另一回合启动不会误报为仍活动。
7. turn SSE 返回 `x-owo-turn-id`，CORS 向 WebView 暴露该头；OpenAPI 与 TypeScript 契约快照已同步。
8. Rust 共享客户端和桌面端在 SSE 结束/断开后自动补拉；重放到 final 正常完成，服务端未留终态则报中断并保留部分输出。

## 当前验证（真实退出码）

本节记录的是 2026-09-22 本次 P2.2 验证时的历史运行配置：当时 Cargo 输出临时放在 C 盘 target。用户随后明确要求 C 盘缓存清理、后续直接在 T 盘构建；该 C 盘 target 已按用户要求清理。之后的构建均先执行 `Resolve-OwoOrtEnv`，经 `Invoke-CiCargo -PassThru`，清除进程级 `CARGO_TARGET_DIR` 后使用 T 盘默认 `agent-sdk/target`，定向档 `-j 2`、测试线程 2。

| 检查 | 结果 |
|---|---|
| `cargo fmt --all -- --check` | exit 0 |
| `cargo test -p owo-agent-server --lib --locked` | 71/71 通过（含队列 4 项） |
| `cargo test -p owo-agent-server --test route_contract_tests --locked` | 初验 27/27；加入 Provider 断流、审批写入闭环后 30/30；2026-09-22 再加入慢消费者 + Provider 断流组合故障后 31/31（含 CORS、终态、OpenAPI、断点补拉、Session diff/revert）；本轮复验 31/31 |
| `cargo clippy -p owo-agent-server --test route_contract_tests --locked -- -D warnings` | exit 0（新增压力测试所在目标无 Clippy warning） |
| `cargo test -p owo-agent-client --test client_contract_tests --locked` | 10/10 通过（含 EOF 恢复与 interrupted 失败语义） |
| `cargo test -p owo-agent-core --lib sqlite_store::tests --locked` | 9/9 通过（含跨重开持久序号） |
| `cargo clippy -p owo-agent-server --lib --locked -- -D warnings` | exit 0 |
| `node --test desktop/web/tests/*.test.mjs` | 410/410 通过；新 SSE helper 测试 4/4 |
| `node --check`（`app-domain.js` 与 `turn-sse.js`） | exit 0 |

覆盖项：慢消费者队列容量、字节上限、增量合并顺序、过载终态、接收端断开、持久序号、turn 过滤、重放分页、终态分类、服务重启中断，以及桌面/Rust 客户端恢复。loopback TCP 慢读探针：客户端收到 SSE headers 后不消费 body，测试观察到慢消费者指标增长、Provider 在 1024 个增量上限前被取消；随后关闭连接，按 turn ID 补拉到 `failed` 状态并恢复已经生成的部分 token。2026-09-22 新增组合用例，在不读取 SSE body 时注入超过队列字节上限的增量 burst 并让 Provider 返回断流错误；检查慢消费者指标、Provider 故障注入完成、失败终态、部分 delta 与失败进度的持久回放。2026-09-22 又新增 4 连接并发慢读用例：四个 HTTP 响应同时只读取 headers、不消费 body，分别触发慢消费者计数，并逐个按 turn ID 补拉 `failed` 终态和部分增量。审批写入集成测试使用 AutoReview 策略，通过真实 Axum handlers 请求 once、写入文件、读取 diff 并执行 Session revert；它不是独立 Daemon/真实 Desktop 验收。当前完整 `route_contract_tests` 为 32/32，目标 Clippy `-D warnings` 通过。测试未测量长时内存曲线或长 soak。CORS 测试确认 WebView 可见 turn ID。尚未对真实 Tauri 桌面窗口执行断网/恢复截图验收；本轮 Playwright 截图仅针对设置页能力开关。

## 明确未完成（因此 P2.2 仍为部分完成）

- 已做单连接慢 HTTP 压力探针、4 连接并发慢读、Provider 中途断流持久回放及两者组合用例，但未做长时 soak、持续内存曲线；因此不能声称 SSE 稳定性 SLO 或 P2 整体完成。
- `turn_events` 当前没有保留期/清理策略，事件长期增长需要后续容量设计。
- 每个事件现按序同步提交 SQLite；功能正确性已测，但其对高频 token delta 首 token/吞吐的影响尚未基准测量。
- 桌面协议 helper 与恢复分支有 Node 契约覆盖，尚缺真实桌面窗口的逐步断网/恢复可视验收。

## 资源门状态

历史资源快照：T 盘余量 4.96 GiB、C 盘余量 333.40 GiB。此后依用户指示，先用 Cargo 清理 T 盘项目构建缓存（70,747 个文件、约 73.1 GiB），并清理 C 盘临时 target；不是通过文件系统直接删除活动构建产物。后续构建已在 T 盘默认 target 运行。2026-09-22 05:46 实时余量 **29.55 GB**；定向构建门要求至少 6 GB，完整 workspace/重链档要求至少 20 GB，当前无需清理。继续构建前仍须实时复查磁盘门；用户已授权空间不足时执行项目范围内的 `cargo clean`，不因此放宽构建并发/内存/磁盘门。
