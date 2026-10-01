# Agent-SDK 远端 engine 取优合并记录（2026-10-01）

远端 `lingxi/engine`（force-update 至 `be6298f`）与本仓库 `agent-frame` 的取优合并收口记录。
原则：**以本仓库为主**；功能重合时取更完整/更安全一侧；本地是超集或等价实现的，记录理由后不回移。

## 一、已合并（本记录周期，按提交顺序）

| 提交 | 内容 |
|---|---|
| `773a218` | 桌宠 `/pet` 静态路由 + 全局 no-store（be6298f 取优） |
| `9036891` | `ToolResult.preview` 全链路（core → SSE → CLI） |
| `fa9e8dd` | 步数耗尽收尾总结 + 空回答兜底摘要 |
| `1594bdb` | `multi_edit` 批量替换工具（原子 + Write 级策略） |
| `cd65280` | `fan_out_subagents` 并行只读子代理（含取消桥） |
| `c2c7307` | `git_status` / `git_diff` / `git_log` 只读工具 |
| `ff64788` | hooks（A2-1）：5 类生命周期事件 + settings/agent/server/CLI 全链路 |
| `1a0fe7c` | `ReasoningDelta` / `PlanUpdate` / `TurnStats` 事件全链路（provider 流式重构） |
| `3312b76` | 脏历史归一 + CJK token 估算 + A4-3 预算硬裁剪兜底 |
| `69b9e50` | `/activity`、桌宠显隐 `/desktop/pet[/report]`、`/approvals/pending`、`/automations/runs` |
| `7c6f811` | `/fs/pick-directory` 原生选目录、`/fs/open` 工作区内开文件 |
| `cdd0b10` | ask_user 全链路：question 通道 + 工具 + SSE 事件 + `/session/{id}/answer/{question_id}` |
| `3a98e1a` | Anthropic 原生 provider（A1-1）+ `build_model_http_client`（NO_PROXY，A1-4）+ `OWO_PROVIDER` 选择 |
| `1c08d35` | MCP resources/prompts 泛化工具全链路（A2-2）+ `crypto_contract.rs` 存储加密契约测试（P0-1） |
| 本次 | 本记录文档 |

## 二、判定“本地超集/等价，不回移”的清单（附理由）

| 远端项 | 本地替代 | 理由 |
|---|---|---|
| `permissions.rs`（core 简易权限表） | `owo-agent-policy`（GrantStore/profile/spec） | 本地按“工具+参数指纹+scope+过期”授权，且审批与主 Agent 分离；远端规则表是更弱的同功能面 |
| `/permissions/rules`、`/permissions/rules/remove` 路由 | `/permissions/grants` 等本地路由 | 同上；远端规则语义（glob + 过期）无法映射到指纹授权而不失真，不提供假适配层 |
| `plan_tools.rs`（update_plan / SessionPlan） | `todo` 工具 + `PlanUpdate` 事件 | 本地 todo 已接通 PlanUpdate 全链路（CLI/TUI/SSE），另立 plan 状态是重复状态源 |
| `web_tools.rs` | `tools.rs` 内 `web_fetch` / `web_search` | 本地实现已覆盖且带宿主校验 |
| `loop_guard.rs` | agent 执行循环内联守卫 | 本地已覆盖重复调用/步数/空转拦截，落文件会拆散守卫逻辑 |
| `tokenizer.rs`（tiktoken-rs） | core CJK 感知启发式估算 | 避免引入新原生依赖；估算精度经 `text_token_estimate` 单测与预算兜底验证 |
| `eval_tests.rs` | `devtools/product-eval/tests/eval_tests.rs` | 已逐用例同源合并 |
| `loop_guard_tests.rs` / `plan_tool_tests.rs` / `permission_rules.rs` / `tokenizer_accuracy.rs` / `tool_efficiency_tests.rs` | 对应功能本地测试 | 测试对象是被判定不回移的项；保留会测不存在/已替代的 API |
| `cli/main.rs`（+668）单体命令表 | `crates/owo-agent-cli/src/commands/*` | 本地命令面已完整（turn/serve/serve-ime/repl/tui/init/eval/bench/cloud/plugin/audit/backup/doctor + daemon/capabilities/worker/product-eval 超集），差异是结构不是功能 |
| `cli/markdown.rs` | 本地增量 `MarkdownStream`（Block/Style 状态机） | 本地为“边到边显”流式渲染（长段落不缓冲），远端为整行渲染；本地实现更优 |
| `desktop/web/app.js`、`panels/*`、`config/provider-presets.js` | `desktop/web/app-domain.js` + 本地 core/panels 分层 | 本地桌面为独立分层架构，整体替换会推翻本地前端；功能面由本地面板承接 |
| `desktop/web/assets/pet/skins/**`（48 个文件，28 MB 二进制） | 无 | 皮肤消费者是仓库外的 overlay app（`apps/overlay/ui/pet`，本仓库与远端分支均不存在）；本地 UI 不消费皮肤，暂不引入 28 MB 二进制资产 |

## 三、合并收口指标（2026-10-01）

- HTTP 路由面：远端独有仅剩 `/permissions/rules`、`/permissions/rules/remove`（见上表，判定不回移）；本地注册 285 条 vs 远端 127 条。
- 核心/服务端 Rust 面：远端模块与工具结构体已被本地全集覆盖（本地为超集，如 ApplyPatch/Todo/WebFetch/WebSearch/ReadImage/shell 输出与终止等）。
- 回归基线（最近一轮）：core lib 235、kernel 36 lib + 11 crypto_contract、mcp 15、cli bins 42、policy 63、route_contract 36、observability 27、cloud_sse 6、turn_api 4、protocol 2、extensions 22；`cargo fmt --all --check` 干净，改动 crate `clippy --all-targets` 干净。
