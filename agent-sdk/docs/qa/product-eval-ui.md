# ProductEval 桌面 UI 与外壳缺陷优化 QA 报告（V1 三日 · 第三路）

- 日期：2026-08-28
- 责任：四路并行日三期 · 第三路（ProductEval 桌面 UI 与外壳缺陷优化）
- 修改文件：`desktop/web/panels/eval.panel.js`（重写为产品评测中心）、`desktop/web/panels/notes.panel.js`（挂载空节点修复）、`desktop/web/style.css`（新增第 17 节，`.owo-pe-*` 作用域）、`desktop/web/index.html`（favicon 声明）
- 新增文件：`desktop/web/favicon.svg`、`desktop/web/tests/product-eval.panel.test.mjs`、本报告
- 方法：**全部结论来自当日实测**——Node 单测（node:test 26 条）、`node --check` 语法门禁、**真实 owo-agent-server**（本日构建，`serve --port 4096`，数据目录独立 scratch、凭据仅注入进程环境变量且为占位值，红线未破）+ 4173 静态反代宿主 + 无头 Chrome（puppeteer-core 直连本机 Chrome，1280×720 / 1440×900 / 1920×1080 三分辨率交互驱动）。未用历史绿灯或 mock 代替（评测后端即第四路本日真实 `/product-eval/*` 实现）。

## 1. 面板能力（重写后）

| 区块 | 内容 |
|---|---|
| 运行配置 | suite（默认 v1）、reference/live、单 Agent / WorkSwarm 复选、重复次数（1–50 钳制）、类别过滤（代码/研究/文档）、仅运行 case、启动/取消按钮 |
| 运行进度 | 六态徽标（queued/running/interrupted/cancelled/completed/failed）、进度条（done/total 百分比）、当前任务（progress.current 缺席时回落 report.pending[0].case_id）、单 Agent / WorkSwarm 双引擎并排指标卡（成功率/平均耗时/模型调用/Token/费用） |
| 逐 case 对比 | 任务 × 类别 × 单 Agent × WorkSwarm × 质量检查（"通过/总数"）× 失败步骤 × Artifact refs（一键复制）× 可展开错误明细（word-break 折行、限高滚动） |
| 历史运行 | run_id/状态徽标/suite·execution·modes，点击行加载详情；键盘可达（tabindex + 焦点环） |
| 轮询 | 2 秒间隔拉取详情；面板切走（DOM 脱离文档）立即停轮询；终态自动停；切换运行后晚到响应经 seq 守卫作废 |

对接冻结契约：`POST /product-eval/runs`（202 异步受理）、`GET /product-eval/runs`、`GET /product-eval/runs/{id}`、`POST /product-eval/runs/{id}/cancel`（幂等）。详情形状按第四路实际实现对接：进度键 `progress.done`；逐格结果 `report.runs[]`（`key.agent_mode` serde 为 `single/multi`，面板归一 `multi→workswarm`）；引擎聚合由 `report.runs` 现算（成功率/均耗时/调用/Token/费用），`report` 为 null（首格未完成）时显示 "—" 不炸。

## 2. 静态门禁（实测退出码）

| 门禁 | 命令 | 结果 |
|---|---|---|
| 语法 | `node --check` 全部 desktop/web JS（app.js + 12 个面板） | **全部通过（exit 0）** |
| 新增单测 | `node --test tests/product-eval.panel.test.mjs` | **26/26 全绿**：六态状态机、启动请求体规范化（含缺省/脏值/次数钳制）、`progress.done` 与 `completed` 双键兼容、`casesFromDetail` 归一（multi 归一/检查计数/失败步骤/refs 去重）、engineAgg 过滤聚合、渲染守卫（对比表/复制按钮/失败步骤/可展开错误/XSS 转义/空态）、行为（同 tick 三连击只发一次 POST、400 不选中运行、终态拒发取消、取消后一个轮询周期内呈现 cancelled、切走即停且零请求、终态自动停、seq 晚到作废、列表两种形态）、错误文案映射、notes 修复与 favicon/style 第 17 节守卫 |
| 回归 | `node --test tests/workswarm.panel.test.mjs` | **16/16 全绿（无回归）** |
| 汇总 | 两文件同跑 | **42/42，exit 0** |

## 3. 真实浏览器实测（无头 Chrome × 真实服务端，25/25 全过）

服务端：本日 `cargo build -p owo-agent-cli` 构建，`serve --port 4096 --workspace agent-sdk`，`OWO_AGENT_DATA=scratch-pe-ui/data`，`OPENAI_API_KEY` 仅占位值注入进程环境；页面经 4173（静态 desktop/web + API 反代）加载。评测以 **reference 模式**实跑（v1 套件全量 10 case × 双引擎，零模型调用、零费用），启动→running→completed 全链路真实发生。

| # | 实测项 | 结果 |
|---|---|---|
| 1 | 1280×720 横向溢出（documentElement/body scrollWidth vs clientWidth） | ✅ 无（1280/1280） |
| 2 | 1440×900 横向溢出 | ✅ 无（1440/1440） |
| 3 | 1920×1080 横向溢出 | ✅ 无（1920/1920） |
| 4–6 | 三分辨率页面控制台错误 / pageerror | ✅ 均为 0 |
| 7 | `/favicon.ico` 404 噪声 | ✅ 已消除——页面仅请求 `favicon.svg`（index.html 显式声明），无 .ico 请求 |
| 8–9 | notes 挂载竞态（页面加载后立即切走面板，/notes 回包晚到） | ✅ 无 alert/dialog、无 pageerror（修复前此处弹 "Cannot set properties of null"） |
| 10 | 启动按钮可见可点（展开侧栏「扩展面板」抽屉后） | ✅ |
| 11 | **连续点击只创建一个运行**（同一 tick 三连击） | ✅ `POST /product-eval/runs` **仅 1 次**（网络层计数） |
| 12 | 创建后自动选中运行（runId 回填） | ✅ |
| 13 | 挂载期间 2s 轮询在途（pollTimer=true, status=running） | ✅ |
| 14 | 终态自动停轮询（completed → pollTimer=false） | ✅ |
| 15 | 面板切走即停轮询（守卫分支） | ✅ |
| 16 | 历史行点击重新选中、详情恢复 | ✅ |
| 17 | 运行中取消按钮可用 | ✅ |
| 18 | **取消后一个轮询周期内呈现 cancelled** | ✅ 实测 **232ms**（远小于 2s 周期） |
| 19 | 终态后取消按钮禁用 | ✅ |
| 20 | 终态再点取消不发送请求（幂等双保险） | ✅ 0 次 cancel POST |
| 21 | 非法套件（nope-suite）→ 400 友好呈现 | ✅ 「请求被拒绝（400）：请检查套件名 / 次数 / 过滤参数」 |
| 22 | reference 全量跑至 completed | ✅ 60/60（reps=3 运行） |
| 23 | 逐 case 行渲染 | ✅ 10 行（v1 全量 case） |
| 24 | Artifact 复制按钮在场 | ✅ 10 个（`data-pe-copy`） |
| 25 | 双引擎指标卡渲染（单 Agent / WorkSwarm） | ✅ |

渲染内容抽查（已完成运行，DOM innerText）：
- 摘要：`已完成 | run: eval-d5d5… | v1 · reference | 单 Agent + WorkSwarm | 进度 60/60（100%）`，双引擎卡各含 成功率 100.0% / 平均耗时 / 模型调用 / Token / 费用（reference 执行器无 token 数据时显示 "—"，符合口径）。
- 首个 case 行：`code-repo-audit | code | passed 0.0s | passed 0.0s | 3/3 通过 | — | out/audit.md [复制] | —`——单 vs 多并列、检查计数、Artifact 复制齐备。

## 4. 缺陷修复明细（外壳两条预存问题，昨日 QA 归档本路）

1. **notes.panel.js 挂载访问不存在节点**：`refresh()` 异步回包与 `renderDetail` 直接 `rootEl().querySelector(...)`，面板被切走后 `rootEl()` 返回 null 即抛 "Cannot set properties of null"。修复：回包后 null 守卫静默放弃；`renderDetail` 头部守卫；导出/删除/双击改标题/保存四处点击路径统一 `currentDetailId()`/`detailOf()` 空节点安全读取。实测竞态场景零弹窗零报错。
2. **`/favicon.ico` 404 控制台噪声**：新增 `favicon.svg`（品牌蓝底 + 圆环 O + 中心点），`index.html` 头部显式 `<link rel="icon" type="image/svg+xml">` + `alternate icon`。实测页面仅请求 favicon.svg，无 .ico 回退请求。

## 5. 已知边界（如实记录）

- reference 执行器不产生 token/费用数据，指标卡显示 "—"；live 模式（真实 Provider）下同位置将展示真实累计值。live 实跑不在本路验收口径（无凭据红线内不做真实计费调用）。
- 本地 reference 服务受理极快（单次 POST 往返 < 数十 ms），「连点」守卫验证为同一 tick 三连击（在途锁窗口）；若两次点击间隔大于服务端往返，将合法创建第二个运行（产品语义允许，非误触）。
- 长错误通过单行省略 + title tooltip + `<details>` 展开区三层治理；展开区 `word-break:break-all` + 限高 180px 滚动（style.css 第 17 节，含单测断言）。

## 6. 截图存档

`scratch-pe-ui/shots/`（gitignored）：`config-{1280x720,1440x900,1920x1080}.png`、`switched-away-notes.png`、`progress-running-1280x720.png`、`cancelled-1280x720.png`、`result-completed-1280x720.png`。

## 7. 完工要求对照

| 要求 | 结果 |
|---|---|
| 三分辨率无整体横向溢出 | ✅（第 3 节 #1–3 实测） |
| 启动按钮连续点击只创建一个运行 | ✅（#11，网络层计数 1 次 POST） |
| 取消后 UI 一次轮询周期内显示 cancelled | ✅（#18，实测 232ms < 2s） |
| 单/多 Agent 指标和失败任务可展开查看 | ✅（引擎卡并列 + 错误 `<details>` 展开，#23/25） |
| Artifact ref 可复制，长错误不撑破布局 | ✅（#24 + 第 17 节省略/折行守卫） |
| `node --check` 与新增 Node 测试全绿 | ✅（全部 JS exit 0；26/26；回归 16/16） |
| 浏览器控制台无 favicon 404 与 notes 初始化错误 | ✅（#4–6、#7、#8–9） |
| 不修改 Rust、OpenAPI、TS SDK 和 WorkSwarm 核心文件 | ✅（仅动本路 4 个修改文件 + 3 个新增文件；`desktop/web/app.js` 的未提交改动为昨日第四路面板工作遗留，本路未触碰） |
