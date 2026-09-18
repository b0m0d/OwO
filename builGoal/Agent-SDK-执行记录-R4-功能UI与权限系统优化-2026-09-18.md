# Agent-SDK R4 执行记录 · 功能使用、UI 与权限系统优化（2026-09-18 起）

> 对应指南：`builGoal/Agent-SDK-后续任务实施指南-2026-09-18.md` §4（R4）。
> 基线：R3 已收口（§3.7 全量门禁 exit 0，见 R3 记录 §R3.7）。
> 纪律：本文件只写**实跑出来的数字**；未跑的不写"已完成"，跑失败的原样留档。

---

## R4.1 §4.6 诊断请求台账 + §4.3 全局状态条（首批交付）

### 1. 本批唯一目标

把 R1–R3 已经建好、但**界面完全没接**的两块能力变成用户能用到的东西：

- §4.6：`GET /diagnostics/requests` 安全请求台账接入「设置与诊断」页（此前前端对该
  端点**零引用**，只有验收脚本在读它）；
- §4.3：主窗口顶部统一状态条（后台 / 工作区 / 模型 / 权限 / 当前任务），
  且"不能只是不可交互的文字灯"。

### 2. 精确修改文件

新增：

- `agent-sdk/desktop/web/views/diagnostics-ledger.view.js`（§4.6 台账视图：计数、
  慢请求 Top 8、按 route_template 的 P50/P95、来源分布、重启近似、SSE、脱敏导出包）
- `agent-sdk/desktop/web/views/status-bar.view.js`（§4.3 五段状态条）
- `agent-sdk/desktop/web/tests/r4-diagnostics-ledger.test.mjs`（10 条契约）
- `agent-sdk/desktop/web/tests/r4-status-bar.test.mjs`（11 条契约）
- `agent-sdk/scripts/verify-desktop-r4-ui.ps1`（§4.10 真机 UI 验收，31 断言）
- `agent-sdk/docs/qa/evidence/r4-desktop-ui-<stamp>/`（DOM 事实 + 三档截图 + 报告）

修改：

- `agent-sdk/desktop/web/index.html`：台账容器（`#diagnosticsLedger`）与状态条容器
  （`#globalStatusBar`）、两个视图脚本引用
- `agent-sdk/desktop/web/app.js`：`refreshDiagnosticsLedger()`（只随设置路由触发）、
  `initGlobalStatusBar()`（boot 早期、水合之前）、`owo:statusbar-navigate` 接收方、
  `state.lastTurnOutcome` 字段
- `agent-sdk/desktop/web/app-domain.js`：回合结束写 `lastTurnOutcome`
  （completed / cancelled / failed）
- `agent-sdk/desktop/web/style.css`：状态条样式 + 1180/860 两档收紧 + `:focus-visible`
- `agent-sdk/scripts/desktop-acceptance-common.ps1`：新增 `New-OwoAcceptanceRun` /
  `Start-OwoAcceptanceShell` 共享运行骨架

### 3. 缺陷、根因与修法（全部由真机/契约跑出来，非阅读猜测）

| 编号 | 现象 | 根因 | 修法 |
|---|---|---|---|
| R4-BUG-01 | 状态条工作区段在真机显示「未选择」，而壳明明带着工作区跑起来了 | 全新 WebView2 存储下 `localStorage["owo.workspace"]` 是空的，状态条只读前端 `state.workspaceRoot`（它只在用户手输/选目录时才被写） | 改成**壳 IPC `get_workspace` 权威水合**（5 s TTL，零 HTTP），前端 state 降为即时反馈；`unmount()` 同时清空缓存——同一进程连跑多场景时留着上一轮工作区就是假绿 |
| R4-BUG-02 | 台账首帧把「0 条 / 环形容量 0」当事实播报 | `load()` 先渲染骨架，骨架沿用真实渲染模板，把"还没取到"写成 0；`0` 与「未知」不可区分 | 骨架态**不输出任何数量结论**（只说"正在读取…"），失败态单独渲染（保留重试出口 + 壳侧事实），不再静默停在 loading |
| R4-BUG-03 | 验收脚本把骨架误判成"已加载"，四条台账断言在空数据上空转通过 | 判据用了 `returned 非空`——0 也是非空 | 判据换成 `cap=512`：cap 只可能来自服务端响应，与被测事实同源 |
| R4-BUG-04（自查设计问题） | 「一键导出」直接触发浏览器下载，会把文件写进**真实用户**下载目录 | 验收环境只重定向了 `LOCALAPPDATA/APPDATA/TEMP`，下载目录由 `USERPROFILE` 派生，重定向管不到 | 拆成三出口：生成（只在页面展开脱敏包）/ 复制（剪贴板，失败降级为页面展开）/ 下载（显式落盘）。验收脚本只点生成 |

### 4. 协议/持久化/CLI/UI 变化

- **零协议变更**：台账页纯消费既有 `GET /diagnostics/requests`（六字段白名单）与
  `GET /metrics/overview`（`sse.active_connections/total_connections/lagged_total`）。
- **零持久化变更**；无新 CLI 面。
- UI 新增两块（状态条常驻顶部；台账在设置路由内）。
- 口径说明（写进页面，不藏文档里）：服务端 `aggregates.business` 含 SSE，页面把
  `events` 单列、`业务请求 = business − events`，四类可互相加总核对；
  「最近一次 core 重启」在壳未上报重启计数前以 `/auth/token` 引导次数**近似**，页面明写。
- §4.6 禁止回显清单在**渲染与导出两个出口**都过同一套脱敏函数
  （`maskLocalPath` / `maskSecretValue` / `stripQuery` / `instancePrefix`）：
  盘符路径、UNC、`/Users`、`/home`、`Bearer`/`Basic`/`token=`/`api_key`、
  JWT 形态串、query 一律不落页面也不落包；`logPath` 只导出"能否打开"。

### 5. 权限、秘密、网络、不可恢复操作影响

- 状态条与台账**都不新增 HTTP**：状态条只读壳 IPC 快照与前端运行态，
  台账只随「设置」路由按需拉（`BOOT_LAZY_TASKS` / `BOOT_HYDRATE_TASKS` 未动，
  并有契约测试守着）。真机首屏口径复测见 §6。
- 不写入任何持久化文件；「下载诊断包」是唯一落盘动作，且必须显式点击。
- 台账/导出包不含密钥、配对秘密、用户输入全文（§4.6），实测负例见 §6。

### 6. 实际运行的测试与结果

- 桌面 web 契约：**341/341 通过**（本批新增 21 条：台账 10 + 状态条 11）。
  关键条目：五段 key 与顺序、每段真 `BUTTON` 且可聚焦、稳定码→用户术语
  （8 个码逐条）、工作区段不回显绝对路径、四类计数互斥可核对、
  P50/P95 最近秩法、骨架不报数、导出包结构齐备、禁止回显负例（渲染 + 导出两处）、
  台账不进首屏清单。
- 真机 `verify-desktop-r4-ui.ps1`（真实壳 + 真实 WebView2 + 真实 sidecar + CDP +
  三档截图像素验真）：**28/31（首轮，`r4-desktop-ui-20260919-002518`）**，
  三红即 §3 的 R4-BUG-01/02/03；修复后复跑结果见本节末（本文件由同会话续写）。
- 首屏回归（同一轮真机）：**业务请求 4 条 ≤5**、无同路由重复、
  事件连接唯一——证明状态条 1 s 定时器与台账没污染 §8.2 口径。
- 门禁 `cargo fmt --check` / `clippy -D warnings` / workspace 测试本轮未触发
  （零 Rust 产品改动）；台账与状态条都未改服务端。

### 7. 集成证据

`docs/qa/evidence/r4-desktop-ui-20260919-002518/`：`r4-ui-report.json`（31 条逐项
pass/fail）、`status-bar.json`、`status-bar-click-workspace.json`、`ledger-page.json`、
`ledger-export.json`、`ledger-boot.json`、`ui-first-screen.json`、
`r4-ui-{900x600,1280x720,1920x1080}.png`（三档窗口截图，像素验真通过）。
修复后的复跑证据目录同前缀、不同 stamp，两版并存不覆盖。

### 8. 未完成项（不用"基本完成"替代）

- **§4.5 统一权限中心未做**：`GET/POST /permissions`、`/permissions/grants(/revoke)` 已存在，
  但 §4.5.1 要的"文件系统/命令/网络三维度实际范围 + 待审批 + 审批历史 + 撤销入口 +
  完全访问风险拆解"需要**服务端新端点**（pending approvals 目前只有 per-workflow-run 视图，
  无全局列表），属 Rust 改动，尚未动工。
- **§4.4 文件夹选择器未接线**：壳只注册了 `choose_data_directory`（rfd），
  **没有 `choose_project_directory`**；工作区目前仍是"手输完整路径 + datalist"，
  与 §4.4「文件夹＝Tauri 原生目录选择器，禁止要求手输完整路径」不符。这是 R4-4 的硬缺口。
- **§4.2 五类导航未对齐**：现 rail 是 任务/项目/团队/产物/设置，缺"工具与权限"，
  "产物"在 §4.2 表里应归 工作区/自动化与团队；状态条权限段暂时落设置页（代码里已标
  `TODO(R4-1)` 的位置是指南约定，不是 TODO 注释）。
- 状态条「权限」段在权限中心落地前显示「点按查看」，**不显示猜的档位**。
- 台账「最近一次 core 重启」是近似口径：壳侧 `generation` 计数器已存在但没进
  `get_core_state` 载荷，接上它需要一次壳改动（连同 `choose_project_directory` 一起做）。
- §4.10 其余真机项未覆盖：键盘 Tab 全链走查（本轮只验了状态条段可聚焦）、
  审批四动作（§4.5.2）、空/加载/错误/大数据四种状态逐页走查。

> 上表最后两条中的「§4.4 文件夹选择器未接线」与「台账近似口径」已在 **R4.2** 收口；
> 其余（§4.5 权限中心、§4.2 五类导航、键盘全链走查）仍是未完成项，见 R4.2 §8。

---

## R4.2 §4.4 原生目录选择器 + §4.6 重启权威口径 + 模型段真相收口（同会话续做）

### 1. 本批唯一目标

把 R4.1 §8 里"必须动 Rust 才能收"的两条硬缺口一次做完，并顺手收掉真机截图抓到的
模型段自相矛盾（R4-BUG-05）：

1. **§4.4 文件夹 = Tauri 原生目录选择器**（此前工作区是"手输完整路径 + datalist"，
   直接违反指南"禁止做法：要求手输完整路径"）；
2. **§4.6「最近一次 core 重启」改用壳侧权威计数**（此前只能用 `/auth/token` 引导次数近似）。

### 2. 精确修改文件

| 文件 | 本批职责 |
|---|---|
| `desktop/tauri/src-tauri/src/core_runtime.rs` | `generation()` 访问器语义定档（换代 vs 同代 attempt）+ `state_for_test()` 测试注入口 + 2 条新单测 |
| `desktop/tauri/src-tauri/src/commands.rs` | `runtime_state_value` 注入 `generation`；ready 描述符也带 `generation`；抽 `workspace_receipt()` 统一两个入口的回执形状；新增 `choose_project_directory`（rfd 原生选择器）；4 条 payload 契约单测 |
| `desktop/tauri/src-tauri/src/main.rs` | 注册 `choose_project_directory` |
| `desktop/web/core/folder-picker.js` | **新建** §4.4 选择器封装：选定/取消/失败三终态 + 浏览器降级显式禁用 |
| `desktop/web/core/api-client.js` | `buildCoreDiagnostics` 透出 `generation`/`attempt`（未上报归 `null`，不补 0） |
| `desktop/web/views/diagnostics-ledger.view.js` | 重启段用权威口径；无权威值才允许近似文案；auth 卡片口径修正；导出包加 `shell_generation`/`shell_attempt` |
| `desktop/web/views/status-bar.view.js` | 模型段两份真相处理（R4-BUG-05） |
| `desktop/web/views/setup-guide.view.js` | 引导页浏览按钮走原生选择器；"可粘贴绝对路径"文案仅在非桌面分支 |
| `desktop/web/index.html` | 工作区字段改 `.owo-field-row` + `#chooseWorkspace` + `#workspacePickerHint`；引入 `core/folder-picker.js` |
| `desktop/web/app.js` | `applyWorkspacePicked()`（写状态 + 复位连接 + 重绘状态条 + `recover()`）与选择器接线 |
| `desktop/web/style.css` | `.owo-field-row` 字段行样式（含只读态） |
| `desktop/web/tests/r4-folder-picker.test.mjs` | **新建** 8 条 §4.4 契约测试 |
| `desktop/web/tests/r4-diagnostics-ledger.test.mjs` | 追加 3 条（权威口径 / 旧壳近似 / auth 卡片文案） |
| `desktop/web/tests/api-client.test.mjs` | 追加 1 条归一测试；2 处整对象断言补 `generation`/`attempt` 字段 |
| `scripts/verify-desktop-r4-ui.ps1` | 31 → **37** 条断言：新增 §4.4 三条 + §4.6 权威口径一条，导出包键位补 `shell_*` |

### 3. 缺陷、根因与修法

- **R4-BUG-05（模型段自相矛盾，真机截图抓到）**：`后台 可用` 与 `模型 unset · 未配置` 同屏。
  根因：密钥经环境变量注入 sidecar 时，壳的 `get_provider_status` 读不到自己的配置文件
  （它只描述"壳侧配置文件视图"，不代表 core 不可用）。修法：设置页水合成功后把 core
  实际生效的 provider/model 回灌状态条（`OwoStatusBar.reportModel`）；只有壳视图时
  降级为 muted 并写明"壳侧配置视图与 core 不一致"，**不再标黄**。
- **R4-BUG-06（`null` 被当成 0）**：`Number.isFinite(Number(null)) === true`，
  旧壳未上报时台账会显示"第 0 代"——把"未知"伪装成了事实。修法：三处判定统一先排除
  `null`/`undefined`（`api-client.js` 两处、台账渲染与导出各一处），并各加一条契约测试。
- **R4-BUG-06（同代崩溃被记成换代的风险）**：`retry()` 会换代，崩溃自动重启只累加
  `attempt`。这条语义在指南 §4.6 没有明说，但台账一旦把两者混称就会误导排障。
  修法：`generation_counts_manual_retry_only` 单测把语义钉死（`start()` 不得改代际），
  UI 侧分列"第 N 代 · 当代自动重启 M 次"。
- **测试侧自伤**：`cargo test` 首轮 `error[E0428]` 重复定义同名测试——同一轮里我用两个
  相邻锚点各插了一次同一个测试（第一次 edit 因参数写错未生效，误以为没落地）。已删重。
  教训：插入代码块后必须 `Select-String` 复查锚点唯一性，不能凭记忆判断是否落盘。

### 4. 协议/持久化/CLI/UI 变化

- **壳 IPC 契约新增**：`choose_project_directory() -> {ok,workspace,state,generation}
  | {ok:false,canceled:true} | {ok:false,error}`；`set_workspace` 回执从
  `{ok,workspace,state}` 扩为同一形状（前端只需一套判定）。
- `get_core_state` / `retry_core_start` / `get_core_connection`（ready 分支）载荷新增
  `generation`（数值，恒存在）。**取消不是错误**：canceled 走独立分支。
- 持久化：无新增文件；工作区仍写数据目录 `workspace.json`（`set_workspace` 同一校验路径）。
- UI：侧栏工作区输入框在壳内 `readOnly=true` + `aria-readonly`；引导页文案改为"由原生
  目录选择器设定"。

### 5. 权限、秘密、网络、不可恢复操作影响

- 原生选择器只在**用户主动点击**时弹系统对话框；自动化验收**不去点它**——
  Win32 模态框不属于本窗口客户区，CDP 与截图都覆盖不到，去点只会把无人值守跑批卡死。
  本轮因此只断言"接线就绪"（只读态 + 按钮指向壳命令 + 文案不引导手输），
  真实点选/取消的交互留给人工走查（记在 §8，不含混）。
- 选定工作区是**受控重启**动作（壳侧 `retry()` 换代）：前端必须 `resetCoreConnection()`
  作废旧端口与旧 token，再走 `recover()`；不做这步会拿陈旧 bearer 打新实例。
- 台账导出包仍不含绝对路径（`log_available` 布尔化）；`shell_generation` 是纯计数，无敏感性。

### 6. 实际运行的测试与结果

- 桌面 web 契约：**356/356 通过**（341 → 356，本批 +15 条：选择器 8 + 台账 3 + 归一 1 + 既有断言修正 3 处）。
- 壳侧单测 `desktop/tauri/src-tauri`：**31 passed / 0 failed**（27 → 31，+4 条 payload 契约；
  含 `generation_counts_manual_retry_only`、`bearer_token_is_none_until_shell_provisions_it`）。
  `cargo fmt --all --check` exit=0；`cargo clippy --all-targets --locked -- -D warnings` exit=0；
  全程 §2.4 strict 档（`-j 1` / `--test-threads=1`），经 `Invoke-CiCargo -Cwd <src-tauri> -LogFile …`。
- 壳重建：`cargo build --locked -j 1` exit=0（19 s，19.2 MB exe），内嵌前端新鲜度门通过
  （壳 01:37 > 最新 web 01:28）。
- **真机 `verify-desktop-r4-ui.ps1`：37/37 全绿**（`docs/qa/evidence/r4-desktop-ui-20260919-013945/`），
  关键事实：`readOnly=True aria=true mode=native`；`command=choose_project_directory`；
  `restart="第 0 代"`（权威字段贯通，未退化为"壳未上报"）；`bootstraps=1`；
  首屏 `web_business=4 ≤5`、无同路由重复；台账 `returned=12 cap=512 rows=21`；
  导出包 4263 字节且禁止项负例全过；三档窗口截图像素验真（900×600 / 1280×720 / 1707×1067）。
- 本轮**未**跑 R3-C 门禁全量与 §8.2/§8.3 冷启动+故障矩阵：放在本批 Rust/UI 收口后一次性
  串跑（用户已选"修完立刻自动跑全量"），结果记在 R4.3 或 R3 记录续写节。

### 7. 集成证据

- `docs/qa/evidence/r4-desktop-ui-20260919-013945/`：`r4-ui-report.json`（37 条逐项）、
  `folder-picker.json`（新增）、`ledger-page.json`、`ledger-export.json`、`ledger-boot.json`、
  `status-bar.json`、`status-bar-after-settings.json`、`ui-first-screen.json`、
  三张 `r4-ui-*.png`。
- `docs/qa/logs/r4-shell-{clippy,test,build,fmtcheck}-*.log`：§2.4 逐行留痕（真实退出码，未用 `--quiet`）。

### 8. 未完成项（不用"基本完成"替代）

- **§4.5 统一权限中心仍未动工**（R4 最大缺口）。已确认服务端缺三块事实：
  ① 全局待审批列表（现在只有 per-run 视图）；② profile 三维度实际范围（现在只有
  `PermissionMode` 三档 + `workspace_write` 布尔，无 `filesystem/command/network` 拆分）；
  ③ 持久化授权撤销只有 `DELETE /permissions/grant?pattern=`，无"工作区/会话"级联撤销。
  指南 §4.5.1 的字段口径**不可能**在纯前端映射出来 → 必须动 Rust（新端点 + 路由契约测试）。
- 原生选择器的**真实点选/取消**未做无人值守验证（原因见 §5）；需人工走查一次并在 §4.11 打勾。
- **§4.2 五类导航未对齐**：现 rail 仍是 任务/项目/团队/产物/设置，缺"工具与权限"；
  状态条权限段仍落设置页。
- 台账 `generation` 在验收里恒为"第 0 代"（无人点重试/换目录），
  "换代后口径跟着变"只有单测覆盖，**没有**真机端到端覆盖。
- §4.10 其余真机项：键盘 Tab 全链走查、审批四动作（§4.5.2）、空/加载/错误/大数据逐页走查。
