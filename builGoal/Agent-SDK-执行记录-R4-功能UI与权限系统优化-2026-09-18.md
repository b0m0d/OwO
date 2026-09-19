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

---

## R4.3 全量回归抓出的引导门回归（provider-unset 三条红）

### 1. 触发方式（不是读代码读出来的）

R4.2 收口后按用户口径自动串跑全量：`verify-desktop-cold-boot.ps1` → 43/43 绿；
`verify-desktop-failure-matrix.ps1` → **79/82**，三条红全在 `provider-unset`
（引导页不可见 / 缺契约动作 / 不呈现稳定码），取证
`docs/qa/evidence/r3-failure-matrix-20260919-014955/`。

### 2. 根因（两处，互相放大）

- **R4-BUG-08（引导门一次性快照）**：R3-C 门禁重列 sidecar 后，core 在**完全没有凭据**时
  不再以 `provider/not_configured` 退出，而是按 §3.4 的"正常运行"继续 ready，把稳定码
  推迟到模型调用时返回（`gateway.rs` 占位 Provider 设计）。而 `needsSetup()` 只在 boot()
  开头取一次壳快照：那一刻壳还在 `starting` → 判"不需要引导" → 用户直接进主界面，
  每次模型调用都挂。**§3.4 规定这一类的可操作终态是模型配置引导**，所以这是产品缺陷，
  不是断言写得严。
- **R4-BUG-09（壳与 core 两份凭据口径）**：`provider_status()` 里 `Unset` 一律
  `ready=false`，可 core 实际会用 `OPENAI_API_KEY` + 内置 BigModel 端点工作。
  这个分歧正是 R3-BUG-23"引导页顶掉健康主界面"的根，也堵住了"ready 之后复查一次"
  这条唯一能修 R4-BUG-08 的路。

### 3. 修法

- `provider.rs::provider_status`：判定收敛为**显式选择 > 环境凭据**，与 core 取凭据顺序
  一致；`Unset` + 环境有 key = 就绪（不再谎报未配置），无 key = 未就绪（引导判据成立）。
  两条路径各一条定向断言，且**改环境变量的用例用静态锁串行**（并行互踩的偶发红比不测更糟）。
- `app.js::needsSetup()`：壳侧未出终态时以 250 ms 节拍**有界等待**（`SETUP_GATE_SETTLE_MS=6000`，
  远在 §3.4 的 10 s 可操作时限内），只走 Tauri IPC **零 HTTP**（不污染 §8.2 首屏 ≤5 口径）；
  保留 `provider/not_configured` 稳定码短路；`failed` 终态直接放行错误卡，
  **不再复查提供商**（避免把 `core/exited`/`storage/not_writable` 的归因盖成引导页）。
- 状态条/引导页文案：`ready + provider=unset` 说"环境变量凭据 · 内置端点"并写明来源，
  两份视图真不一致时才用"壳侧未配置"。

### 4. 契约测试

- 新建 `desktop/web/tests/r4-setup-gate.test.mjs`（6 条）：用**大括号配平从 app.js
  抽出真实 `needsSetup` 函数体**在 VM 沙箱执行——这类缺陷全在取样时序上，
  源码字符串断言抓不住"什么时候会返回 false"。覆盖：starting×2→ready+未就绪=进引导、
  有环境凭据的健康启动=不进引导、`failed` 不进引导且零提供商复查、
  `no_workspace`/稳定码即时短路、永不收敛时**有界**返回、非壳环境不猜、函数体零 HTTP。
- `r4-status-bar.test.mjs` +1 条（内置端点兜底文案）；`api-client.test.mjs` 归一条款不变。
- 结果：web **363/363**；壳 **31/31**（`provider::tests::status_reflects_mode_and_key_presence`
  改写为 env 受控 + 串行）；`cargo fmt --all --check` 0、`clippy --all-targets -D warnings` 0
  （首轮抓到 `doc_lazy_continuation` 一条，已按建议补空行）。

### 5. 真机复验（顺序：重列 sidecar → 重建壳 → 跑验收）

- `stage-desktop-sidecar.ps1` → `cargo build`（strict `-j 1`）→ 壳 mtime 02:11 > 最新 web 02:07
  （内嵌前端新鲜度门通过）。
- `verify-desktop-failure-matrix.ps1 -Only provider-unset`：**10/10 全绿**
  （`r3-failure-matrix-20260919-021110`），引导页 419 字、`provider/not_configured` 上屏、
  工作区卡显示"选择目录… 由原生目录选择器设定"（§4.4 在故障场景同样成立）。
- 全量链（矩阵 10 场景 + 冷启动含隐藏期 + R4 UI）串跑结果见本节末续写
  （同一轮自动跑，日志 `docs/qa/logs/r4-full-chain-*.log`）。

### 6. 这一轮暴露的流程问题（备案，不掩盖）

- R3-C 门禁"全量一轮"重列了 sidecar，但**故障矩阵没有随之复跑**（只在 R3.6 的旧核上绿过）；
  壳/核世代一换，UI 终态判据就可能失效。以后凡是**重列 sidecar** 的提交，
  §8.3 故障矩阵必须与 §8.2 冷启动一起跑（本轮起写进 R4 退出条件）。
- `Invoke-CiCargo -LogFile` 传相对路径会按 pwsh 工具的 cwd（不是 `-Cwd`）解析而报
  `DirectoryNotFoundException`——日志写不出等于自断取证；后续一律绝对路径。
- 背景任务用 `Out-File`/管道抓不到 `Write-Host` 的 PASS/FAIL 行（信息流不进管道），
  本轮改用 `Start-Transcript`，验收输出才真正留痕。

---

## R4.4 设计定档 · §4.5 统一权限中心（先定契约，再动 Rust）

> 本节是**设计**，不是完成声明。实现前的现状盘点（逐条带证据位，经只读盘点复核）：

### 1. 现状（为什么这一节必须先写设计）

| 指南 §4.5 要求 | 服务端现状 | 结论 |
|---|---|---|
| 当前权限预设（只读/工作区编辑/受控执行/自定义） | 单枚举 `PermissionProfile`（`permissions.rs`），`GET/POST /permissions`（`lib.rs:522-531`）已可读写 | 有，但只有一维 |
| 文件系统/命令/网络**三维度实际范围** | 无此拆分：只有 profile + `settings.workspace_write` 两个松字段展开 | **缺**，前端映射不出来 |
| 待审批请求列表 | `AppState.pending_approvals`（`lib.rs:130-131`，`oneshot::Sender<Decision> + PermissionRequest`）**没有任何列路由**，只能从回合 SSE 流里看到 | **缺关键读取面** |
| 审批动作（拒绝/仅本次/本任务/工作区长期） | `POST /session/{id}/permission/{request_id}`（`lib.rs:369`，`turn_api.rs:366`）；scope 字面量是 `once/session/one_hour/always_readonly`（`grant_store.rs:42-50`），拒绝走 `allow:false` 不是 scope | 有，但**口径与 §4.5.2 不一致**，且必须先知道 session id |
| 已授权范围与有效期 | `GET /permissions/grants` 有；但 `GrantStore` 是**纯内存**（`lib.rs:221`，无表无文件） | "工作区长期"重启即蒸发 = **假承诺** |
| 撤销入口 | `POST /permissions/grants/revoke` 逐条撤销有；无会话/工作区级联 | 部分 |
| 最近审批历史 | 审计事件里有 `permission` 类事件，无专用读取面 | 需要查询面 |
| 完全访问风险拆解 | 无 | **缺** |
| 前端 | `desktop/web` **从未调用过 `/permissions`**；权限页不存在（状态条权限段落设置页，`app.js` 里有注释） | 整页待建 |

**三条硬结论**（决定了"纯前端做不了 §4.5"）：
① 无全局待审批列表；② 无三维度实际范围；③ grants 不持久化，"长期允许"是空头承诺。

### 2. 目标契约（新增面，全部走既有鉴权与 ledger 标签）

- `GET /permissions/overview` → 
  `{ profile, dimensions: { filesystem, command, network, persistence, scopes[] },
     pending: [{request_id, session_id, tool, level, risk_note, args_summary, destructive, requested_at}],
     grants: [{grant_id, tool_id, scope, path_scope, host_scope, expires_at, remaining_uses}],
     decisions: [{at, tool, decision, scope, session_id}] }`
  - `dimensions` 由**服务端**从 profile + 设置 + grant 展开成 §4.5.3 词表；
    表达不下的真实规则一律回 `custom` + 原始规则清单，**不得为了好看而误映射**。
  - `args_summary` 走 `redact_args`（只暴露键名/类型/长度），路径只给必要范围。
- `POST /permissions/decide` → `{request_id, decision: deny|once|task|workspace}`：
  权限中心不关心 session id（服务端经 `pending_approval_sessions` 反查）；
  `task|workspace` 生成对应 Grant，`deny` 不落 Grant。
- `POST /permissions/revoke` → `{level: grant|session|workspace, grant_id?, session_id?}`。
- Grant 持久化：`GrantStore` 落 `data_root/grants.json`（tmp→rename 原子写，
  读回时丢弃过期/用尽项），否则"工作区长期允许"重启失效属**不可接受的静默退化**。
- 词表对齐：`once→once`、`task→session`、`workspace→always_readonly|one_hour`（按动作类别），
  对外统一暴露 §4.5.2 四词，旧 scope 字面量在 wire 上保留兼容读取。

### 3. 落地顺序与验证天花板

1. Rust：`permission_center_api.rs`（读面 + 决定 + 撤销）+ `grant_store` 持久化
   → `server/lib.rs` 接线 → `tests/route_contract_tests.rs` 同步（AGENTS.md 红线）
   → openapi 快照 + `clients/ts` 再生成。
2. 前端：`desktop/web/permissions/{domain,api,controller,render}.js`（§4.8 四件套）
   + `permissions` 路由 + rail 增「工具与权限」（§4.2）+ 状态条权限段改指真页。
3. 验收：`verify-desktop-r4-ui.ps1` 增权限中心段（§4.10）：
   拒绝 / 仅本次 / 本任务 / 工作区长期四动作各一条真机断言 + 撤销后再查必须消失 +
   完全访问必须出现范围/时长/风险三要素与二次确认；重启壳后长期 grant 仍在（持久化端到端）。

**已知风险**：`server/lib.rs` 是 2049 行聚合文件（R6 待拆），本轮只做加法接线不改结构；
`pending_approvals` 持锁跨 `.await` 会造成回合停滞——新增读面必须**先克隆摘要再放锁**。

## R4.5 落地 · §4.5 统一权限中心（判定链接入 + 前端四分页面）

### 1. 落地清单

| 文件 | 干了什么 |
| --- | --- |
| `crates/owo-agent-core/src/permission_spec.rs`（新） | §4.5.3 四维词表与结构化 profile：`FilesystemScope`/`RuleScope`/`PersistenceScope` + `PermissionSpec`，`from_profile` 如实投影（投影不出的落 `custom`，不硬编）、`nearest_profile` **只用于"是否等价只读"一个判断**、`expand` 出四维规则、`extra_denial` 只收紧不放宽、`risk_notes` 完全访问风险四条、`dimension_of` 归类（`browser_*`→network、`run_command`/`shell.`/`command:`→command、文件类→filesystem；UI 注入类不归本模块，留给档位与审批链） |
| `crates/owo-agent-core/src/permissions.rs` | `Policy` 持 `spec: Arc<Mutex<Option<PermissionSpec>>>`；`decision()` 在 **Read 放行与 grant 命中之前**过收紧层；`set_spec`/`spec`/`clear_spec`；5 条新单测（收紧优先于 grant、filesystem=none 连读也拒、无 spec 行为逐条不变、只读上界、收紧不动档位） |
| `crates/owo-agent-core/src/grant_store.rs` | `GrantScope` 增 `Task`/`Workspace`（旧四值字面量与语义不变，`parse` 向前兼容）+ `label()`/`persists()`；**长期授权落盘** `<data_root>/grants.json`（只写"无到期且无次数上限"的授权，tmp→rename 原子写，坏文件改名 `*.json.bad` 保留现场后按空启动）；`revoke_workspace` 级联；`revoke`/`revoke_tool`/`prune_expired` 成功后同步刷新落盘；`Grant.scope` 字段（展示用，不参与判定）；6 条新单测 |
| `crates/owo-agent-core/src/settings.rs` | `Settings.permission_spec: Option<PermissionSpec>`（结构化配置跨重启） |
| `crates/owo-agent-server/src/settings_api.rs` | `GET /permissions/overview`（服务端展开 `dimensions`/`expanded` + 全局 `pending` + `grants` + `recent_decisions` + `full_access.risk_notes` + `scope_literals`）；`POST /permissions/spec`（字面量校验、scopes 必须工作区相对、完全访问三要素、只读模式拒绝保存更宽配置）；`grants_revoke` 扩为三粒度（`grant_id`/`tool_id`/`all`，空 body 400）；`grant_rows` 统一三处列表形状 |
| `crates/owo-agent-server/src/lib.rs` | 两条新路由 + OpenAPI 登记（新增 `permission_spec_schema()` 单一词表来源）；`AppState::new` 用 `GrantStore::persisting(data_root/grants.json)` 并在档位**之后**恢复 spec；`POST /settings` 同步 spec（缺省即清除，不留两套真相） |
| `crates/owo-agent-server/tests/route_contract_tests.rs` | `sample_body` 与路由-事件矩阵各登记一处（`/permissions/spec` → `Settings` 领域失效） |
| `crates/owo-agent-server/tests/permissions_center_api_tests.rs`（新） | 7 条 HTTP 契约：维度服务端展开、提交落盘与 source 翻转、完全访问三要素与 8h 上限、越界/空 custom 拒绝、只读上界、三粒度撤销、跨 AppState 重建的长期授权 |
| `desktop/web/permissions/{domain,api,controller,view}.js`（新） | §4.8 四分：domain 纯函数与词表、api 唯一持路径且传输经注入、controller 三态 + 提交前校验 + 双确认 + 撤销后复查 + dispose、view 注册 `OwoPanels.permissions` 与事件委托 |
| `desktop/web/{app.js,index.html,style.css}` | `ROUTE_META.permissions` + rail「权限」入口 + 状态条权限段从"降级到设置页"改指真页 + 渲染后回灌 `OwoStatusBar.reportPermission` |
| `desktop/web/tests/r4-permissions.test.mjs`（新） | 37 条：域函数、api 路径与 body、controller loading/error/empty、审批四动作、三粒度撤销、假控件防护、接线（路由/rail/脚本顺序/降级分支）、四条分层红线 |
| `desktop/web/tests/panels-lint.test.mjs` | `groups` 显式登记 `permissions` 目录（新前端目录不登记就红） |
| `scripts/verify-desktop-r4-ui.ps1` | 新增 §4.5 段 11 条真机断言（见 §5） |
| `clients/ts/openapi.json` + `src/schema.d.ts` | `regenerate-openapi-snapshot.ps1 -Build` 再生成（273 paths，typecheck 通过） |

### 2. 三条语义红线是怎么被机器守住的（不是"写在注释里"）

1. **范围必须服务端给出**。`/permissions/overview` 的 `dimensions[].effective/summary/source`
   来自 `PermissionSpec::expand()`；前端 domain 层对缺项只补 `synthesized: true` 的占位行
   （稳骨架、不算事实），控制器空态判据显式排除占位行。真机断言要求四维**每维都有非空
   生效值**且 `source ∈ {profile, spec}`，任何一环退化成前端自造都会红。
2. **维度只能收紧**。`extra_denial` 永不调用 `Decision::Allow`；`decision()` 里它排在
   Read 放行与 grant 命中之前，`dimension_deny_beats_grant_hit` 用"同一策略同一 grant，
   只把命令维度改成 deny → 必须从 Allow 翻成 Deny"钉住顺序；档位侧 `set_spec` 只允许把
   档位**推到只读**，绝不反推放宽（否则 `AutoReview` 会被降级）。
3. **完全访问必须范围 + 时长 + 风险**。缺 `confirm` 或缺 `duration_secs` 都是 400
   （`confirmation/required`），时长硬上限 8 小时（超了 `validation/failed`），
   风险清单由 `risk_notes()` 四条给出；真机断言点「申请完全访问」后必须同时出现
   ≥4 条风险与 ≥2 个时长选项，取消后卡片收起且**不发任何请求**。

### 3. 本轮抓出的实现缺陷（都是测试/真机抓的，不是读代码读出来的）

| 编号 | 症状 | 根因 | 处置 |
| --- | --- | --- | --- |
| R4-BUG-10 | 提交 spec 会顺手放宽档位 | 最初写成 `set_profile(spec.nearest_profile())`，而 `AutoReview` 在若干面比 `Workspace` 更严，反推必然降级 | 收紧层承担减法；档位只在"等价只读"时被推到 `ReadOnly`，其余一律不动（`spec_tightens_without_moving_the_profile_dial`） |
| R4-BUG-11 | 只读模式下可保存更宽的结构化配置 | `forces_read_only = nearest==ReadOnly \|\| current==ReadOnly` 让冲突判据 `is_read_only && !forces_read_only` 恒假——**闸门是死代码** | 拆成两件事：上界（只读态拒绝非只读等价 spec，400）与收紧（等价只读才推档位）。由 `read_only_mode_rejects_loosening_spec` 首先抓到 |
| R4-BUG-12 | 权限页永远不显示空态，且缺维度时像有配置 | domain 为稳骨架把四维补成 4 行，控制器用 `!dimensions.length` 判空 → 恒非空 | 占位行打 `synthesized` 标记，空态只认权威行 |
| R4-BUG-13 | HTTP 200 + `{"ok":false,"error":{code}}` 时稳定错误码消失 | 不可用分支自造 `new Error("…缺少档位与维度矩阵")`，`normalizeError` 又从内层对象找 `.error` | 传整个 envelope 给 `normalizeError`，`fail()` 增 `codeOverride`；有码用服务端原文，无码才自述 |
| R4-BUG-14 | 两处"假绿"测试 | ① 委托测试没 `setController(stub)` 就断言派发（`onAction` 早退返回 undefined，被误读成"返回值就是 undefined"）；② 跨 Realm 对象用 `deepEqual`、把 `typeof client.request` 能力探测数成调用点 | ①补绑桩并断言 `calls[0]`；②经 `plain()` 往返 + 只数 `client.request(` |
| R4-BUG-15 | 后台整链"跑完 exit=1"但什么都没发生 | 后台命令里嵌套调用 `pwsh -File`，该主机 PATH 无 `pwsh`（PowerShell 7 未安装），脚本从未起跑 | 改用调用运算符 `&` 直接跑；教训：**后台任务起跑 ≠ 断言通过**，必须看日志里的真实步骤行 |
| R4-BUG-16 | 故障矩阵 4 场景 + 冷启动全红，报 `Cannot find path 'psdrive'`（`failed_stage=inject`，看起来像 sidecar 注入的产品回归） | 给 `Stage-OwoDesktopSidecar` 新加 §2.4 磁盘门时裸调 `Assert-CiDiskGate`——它在成功时**向管道吐一个状态对象**，于是本应"返回单个元数据"的函数返回了两个对象，调用方 `$staged.source` 撞到状态对象的 `source='psdrive'` 字段，`Copy-Item` 当场炸 | 按仓库既有写法 `$null = Assert-CiDiskGate …` 吞输出；实测函数输出恢复单对象且 `source` 正确；并在 `test-ci-shared-resource-policy.ps1` 加 2 条断言把这个"输出污染"类永久锁住（selftest 29 → **31/31**）。教训：**门禁本身也可能是假红的源头**，加门必须同时检查它的输出面 |
| R4-BUG-17 | 「申请完全访问」按钮在默认配置下**静默无事发生**（真机第一轮被读成"确认卡没出现"的断言失败） | `requestFullAccess()` 遇到不含不受限维度的草稿直接 `return`，既不提示也不改状态——一个能点但什么都不做的控件 | 改为显式解释文案（"当前配置不含不受限的命令或网络维度…"），并清掉上一条提示；新增单测同时覆盖"无需确认要给原因"与"含不受限维度必须真的开卡"。教训：**断言失败先怀疑自己有没有走真路径**，验收脚本原来用默认草稿点按钮，等于从没走过这条产品分支 |
| R4-BUG-18 | 验收脚本自身两处口径错误（同时具备假阳与假阴能力） | ① 维度表按 `td[0..3]` 取"键/生效/来源/摘要"，实际视图用 `<th>` 承载维度名 → 四列全部错位；② 用**整页** `.owo-perm-risk li` 计数当"确认卡三要素"，而页面常驻风险预览使 `risks=4` 恒成立 → 卡没开也能通过 | ①改为 `<th>`+`td[0..2]`，并把服务端事实（`GET /permissions/overview` 的 `dimensions[].key/summary/source`、`expanded=4`、`full_access.risk_notes≥4`、`grants_persisted=true`）与 DOM 渲染**分成两组断言**，各按各自口径校验（含中文标签与 `来源未标注` 占位识别）；②`riskItems/durationOptions/confirmHasScope` 一律限定在 `[data-perm-confirm]` 卡内计数。教训：**渲染层断言必须绑定作用域**，全局 `querySelectorAll` 计数的"通过"往往测的是别的元素 |
| R4-BUG-19 | 状态条断言抓到瞬时态：`后台段反映核心就绪` 报 `backend="检查中"`（同一次运行稍后又显示 `可用/ok`） | 等待循环只要"五段齐全"就取样，而后端段的 ready→可用 折叠晚于骨架首帧；断言因此依赖运气（同一脚本前几轮恰好绿过） | 等待条件收紧为"后端段落定到可用或 24 s 超时"，并在断言详情里回显 `settle=` 落定与否——真坏时仍会失败，不把断言改成永真。教训：**验收脚本的等待条件必须是要断言的那个事实**，否则就是在采样竞态 |

### 4. 过程与工具事实（备案）

- 派给子代理的前端权限中心任务**两次**在同一处被上游模型流空闲超时打断（第二次 43 ms 即失败）。
  磁盘上已有其 ~71 KB 产物与 41 条测试，主线接手后修掉 7 条红（其中 2 条是它踩坏的既有守卫：
  `api-client` 的"只有 core 能碰网络"被**注释里的 `fetch()` 字样**误报、`panels-lint` 因新目录
  未登记而红）。结论：产物落盘不等于完成，且这类"跨文件面接线"任务在流不稳定时不如主线直做。
- 全量 Rust 验证：`cargo fmt --check`、`clippy -p owo-agent-core -p owo-agent-server --all-targets -D warnings`
  均 exit 0；core lib **503/503**、center-api 7/7、permissions-profile 5/5、route-contract 23/23。
  Web 全量 **404/404**（§4.5 前端落地时）→ 修 R4-BUG-17 后补 1 条死控件回归，现为 **405/405**。
  全程 §2.4 normal 档（`-j 2` / `--test-threads=2`），`-LogFile` 一律绝对路径。
- 提交：`dcf3f7b`（服务端地基）、`585d178`（前端四分页面）。

### 5. 真机整链（顺序即契约：重建 core → 重建壳 → 三套验收 → 壳单测）

世代链本轮首次被**脚本硬门**保护：`stage-desktop-sidecar.ps1` 在"产物已存在时不重建"，
提交后直接 stage 会把上一代 core 塞进 `binaries/`，壳运行期判 `core/identity_mismatch`，
于是整条真机链以错误的理由全红（实测第一轮就这样烧掉 30 分钟）。现在 stage 后立刻核对
`staged commit == HEAD`，不等就 throw（`-AllowStaleIdentity` 仅供排障复现旧产物）。

| 轮次 | 世代核对 | 结果 | 结论 |
| --- | --- | --- | --- |
| 第一轮 `r45-matrix-20260919-131822` | 壳构建期即警告 core=`2f838bc` vs HEAD=`585d178` | 主动终止（未产出可信数字） | 世代门生效前的问题，作废处理正确（世代硬门因此补进 `stage-desktop-sidecar.ps1`） |
| 第二轮 `r45-matrix-20260919-132205` | `same_generation=True` ✅ | 矩阵 45/49：4 场景 `Cannot find path 'psdrive'` | 全红原因＝R4-BUG-16（门禁自身输出污染），非产品回归；冷启动同因中止 |
| 第三轮（修复 R4-BUG-16 后重投） | `same_generation=True` ✅ | 矩阵 **82/82**、冷启动 **43/43**、壳单测 **31/31**；R4 UI **46/48** | 唯一两条红都在验收脚本侧（R4-BUG-18），产品面首轮真机事实全部正确：`present=True / profile="workspace（工作区编辑）" / dims=4 / overview_calls=1 delta=1 / 隐藏 5 min 零新增请求` |
| 第四轮（修 R4-BUG-17/18/19，web 资产重建进壳） | 世代不变 `commit=585d178 dirty=true`，随包 core sha256 `7DC82D6456FB…` | **四套全绿**：R4 UI **52/52**（`r4-desktop-ui-20260919-r45final`）、故障矩阵 **82/82**（`r45-matrix-final`，末条 `hash_before=7DC82D6456FB hash_after=7DC82D6456FB` 证真实 core 未被改动）、冷启动 **43/43**（`r45-coldboot-final`：首屏业务请求 4、零 `/auth/token` 回落、事件流唯一、隐藏 5 min 新增 0 请求、重启后旧 bearer 401/新 bearer 200）、壳单测 **31/31**（`docs/qa/logs/r45-shell-test-final.log`，§2.4 strict `-j 1`） | §4.5 全链在真机上通过：服务端事实与 DOM 渲染分开断言；完全访问走真路径（默认草稿→给解释，改成不受限→开卡，`risks=4 durations=4`）；整页只发 1 次 overview（台账差值＝1）。矩阵里 `core-hang` 终态仍在 **44 s / 45 s** 预算内（余量 1 s，与 R3 的 44.2 s 同量级）——该项按指南交给 R5 基线定档，不在本轮私调时限 |

第四轮的关键事实（不写结论只写读数）：维度表 `文件系统=工作区内读写/档位展开 命令执行=白名单内允许/档位展开 网络访问=白名单内允许/档位展开 授权有效期=本任务/档位展开`；
确认卡正文含 `"filesystem":"workspace_write","command":"unrestricted","network":"unrestricted"` + 四档时长（10 分钟/1 小时/4 小时/8 小时）+ 四条风险原文；
`grants_persisted=True` 证明"工作区长期"这一格这次不是勾了就算——它真的落到 `grants.json`；
取消确认后 `[data-perm-confirm]` 消失且台账 `web_business` 差值仍为 1（整个走查零泄漏请求）。
