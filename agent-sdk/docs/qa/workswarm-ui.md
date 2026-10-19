# WorkSwarm UI 二轮优化 QA 报告（V1-R2 联动）

- 日期：2026-08-27
- 责任：四路并行日二期 · 第四路（WorkSwarm UI 与现有前端代码优化）
- 改动文件：`desktop/web/panels/workswarm.panel.js`、`desktop/web/style.css`（新增第 16 节，全部 `.owo-ws-*` 作用域）
- 新增文件：`desktop/web/tests/workswarm.panel.test.mjs`、本报告
- 方法：**全部结论来自当日实测**——Node 单测（node:test）、`node --check` 语法门禁、真实 owo-agent-server + 无头 Chrome（puppeteer-core 直连本机 Chrome，1280×720 / 1440×900 / 1920×1080 三分辨率）交互驱动；未用历史绿灯代替。

## 1. 测试环境（真实链路）

| 项 | 值 |
|---|---|
| 服务 | `owo-agent serve --port 4096`（debug 构建，exit 0） |
| 数据目录 | 独立 `OWO_AGENT_DATA`（scratch，不入库）；凭据仅注入进程环境变量且为占位值（红线：不写入任何文件）；全部使用内置 `echo/sleep/fail` worker，**零模型调用** |
| 页面宿主 | 本地静态+反代（127.0.0.1:4173 → `desktop/web` + API 4096，流式转发兼容 SSE） |
| 浏览器 | 无头 Chrome（本机安装版）+ puppeteer-core，逐分辨率新开页面 |
| 种子数据 | 经真实 HTTP API 创建 3 个团队：`fail` 失败队、`echo` 链成功队、`sleep` 中断队（运行中强杀服务进程模拟崩溃重启） |

## 2. 静态门禁（实测退出码）

| 门禁 | 命令 | 结果 |
|---|---|---|
| 语法 | `node --check panels/workswarm.panel.js` | **通过（exit 0）** |
| 单测 | `node --test tests/workswarm.panel.test.mjs` | **16/16 全绿**（状态归一化、运行摘要计算、重试门控矩阵、冻结 retry 请求体形状、DAG 重试按钮可见性、详情骨架（摘要/中断横幅/审计 tabindex）、双击去重、终态拒绝、interrupted 流入、style.css 第 16 节守卫） |

## 3. 三分辨率横向溢出（无头 Chrome 实测 `scrollWidth vs clientWidth`）

| 分辨率 | documentElement (scroll/client) | body (scroll/client) | 整体横向滚动 | 团队列表渲染 |
|---|---|---|---|---|
| 1280×720 | 1280 / 1280 | 1280 / 1280 | **无** | 正常（表头+团队行） |
| 1440×900 | 1440 / 1440 | 1440 / 1440 | **无** | 正常 |
| 1920×1080 | 1920 / 1920 | 1920 / 1920 | **无** | 正常 |

配套防溢出改造（style.css 第 16 节，均有单测断言在场）：`.owo-ws-inline` 补齐 flex-wrap（面板此前误依赖外壳 `.inline`，窄主栏长表单行溢出的根因）；表格统一 `.owo-ws-tablewrap` 横向滚动包裹（`min-width:560px` 内滚、区块不破格）；`≤1280px` 断点收窄成员/模板/统计卡网格最小宽度；长文本 `.owo-ws-ellip`（单行省略+完整 `title` tooltip）+ `.owo-ws-mono` 等宽引用字体，应用于成员 ID/交接契约/能力、DAG 节点错误、摘要失败原因、产物 artifact_id、详情 meta 行。

## 4. 主要交互实测（1280×720 无头驱动，POST 经网络层计数）

### 4.1 运行摘要（失败团队，真实跑失败）
- 状态徽标 `失败`；活动阶段 `已停止：存在失败步骤`。
- 统计卡实测值：成功 0 · 失败/中止 1 · 等待 0 · 运行中 0 · 阻塞 0 · 累计尝试 1 · 产物 0（产物计数由产物接口加载后回流）。
- 失败步骤行：`当前失败步骤：builder（s-builder · 已尝试 1 次）`+ 错误摘要（完整内容进 title tooltip）+「↻ 重试此节点」。
- 成功团队对照：`成功 2 · 产物 2`，badge `已成功`。

### 4.2 重试此节点（第三路冻结契约）
- 快速**三连击**仅产生 **1 次** `POST /teams/team-d34c95d0/steer`（网络层计数；提交锁在点击入口同步生效，第二/三击被 `data-busy` 丢弃）。
- 请求体即冻结契约：`{"command":"retry","step_id":"s-builder","note":"…"}`（单测锁定键集 `command,note,step_id`）。
- 响应后：结果区输出「[重试] s-builder 已受理：仅重置该节点及其未完成下游（已成功步骤、Artifact、Handoff、DecisionRecord 保持不变），运行循环已重启」；按钮解锁并恢复文案（`lockCleared=true`）。
- 重试入口可见性：仅 Failed/Aborted 节点渲染（DAG 节点内 + 摘要行各一，实测 `retryTargets=["s-builder","s-builder"]`）；**终态成功团队 0 个重试入口**（实测 `retryBtns=0`）且门控横幅在场（`gateShown=true`），与核心闸门（目标非 Failed/Aborted → 409）双保险。

### 4.3 重启中断态（强杀服务进程 → 重启）
- 服务端实测：重启后详情接口返回 `interrupted=true` 且磁盘状态保持 `Running`（识别为中断，未自动重放写操作）。
- UI 实测：**中断横幅显示**（`role="alert"`：「⚠ 运行已中断，可恢复 …可用『继续（continue）』恢复运行，或对失败/被中断节点点『重试此节点』」）；状态徽标变为琥珀 `st-interrupted`「已中断（可恢复）」；活动指示 `◦ 已中断`（不再是正常执行中的 `●`）；摘要阶段 `已中断，可恢复`。
- 中断遗留步骤为 Pending 时**不**提供 retry 按钮（契约只允许 Failed/Aborted 目标），恢复走横幅指引的 continue——实测 `POST /steer {"command":"continue"}` → 200、`interrupted=false`、运行循环重启，睡眠步骤真实执行至完成（`status=Succeeded, attempts=1`）。
- 服务端语义佐证（同日 API 实测）：retry 缺 `step_id` → **400**；对已成功步骤重复发送同一 retry → **409**（无额外副作用）；与 UI 的入口可见性/禁用规则构成双保险。

### 4.4 无障碍与状态统一
- 摘要容器 `aria-live="polite"`；中断横幅 `role="alert"`；审计区 `role="log"` + `tabindex="0"`（键盘可达，`:focus-visible` 焦点环由第 16 节提供）并保持独立限高滚动。
- loading / empty / failed / terminal 状态统一由 `stateBox` 输出，摘要区在数据到达前同样呈现 loading 态；终态门控横幅、按钮禁用与 title 解释沿用并扩展到重试按钮。

## 5. 控制台与网络

- **WorkSwarm 面板自身：零控制台错误、零页面异常（pageerror）、零失败请求**（两阶段实测累计 6 次页面加载）。
- 外壳预存问题（非本路口径，如实记录，建议归第一路/主控跟踪）：
  1. `GET /favicon.ico → 404`：web 目录本无 favicon 资产，每页一条资源加载错误；
  2. `notes.panel.js:99` 在面板自动挂载初始化时弹 `alert("列表加载失败：Cannot set properties of null (setting 'innerHTML')")`——预存 shell/notes 缺陷（初始化时序访问空元素），与 WorkSwarm 面板无关。
- 服务端协调器锁预存行为：长步骤（sleep 90s）执行期间 `GET /teams/{id}` 会被阻塞至阶段边界；面板的异步加载 + 2.5s 轮询兜底可容忍，不在本路修复范围。

## 6. 截图存档

`scratch-ws-ui/shots/`（gitignored）：`list-{1280x720,1440x900,1920x1080}.png`、`detail-failed-1280x720.png`、`detail-after-retry-1280x720.png`、`detail-ok-1280x720.png`、`detail-interrupted-1280x720.png`。

## 7. 完工要求对照

| 要求 | 结果 |
|---|---|
| 三分辨率无整体横向滚动 | ✅（第 3 节实测） |
| 列表/详情/DAG/Artifact/Human 节点/retry 可操作 | ✅（列表 5 行渲染、详情/DAG/产物/人节点表单挂载、retry 真实 POST 成功） |
| 快速双击只产生一次请求 | ✅（三连击 → 1 次 POST，网络层计数） |
| 终态团队不能执行无效 steer/retry | ✅（成功团队 0 重试入口 + 门控横幅；终态 continue/steer/cancel 原有禁用逻辑保留） |
| 浏览器控制台零错误 | ✅（WorkSwarm 面板自身零错误；两条外壳预存噪声见第 5 节，已归档非本路） |
| `node --check` 与新增 Node 前端测试全绿 | ✅（exit 0；16/16） |
| QA 报告记录三个分辨率、主要交互和实际结果 | ✅（本报告） |
| 不修改任何 Rust、OpenAPI、TS SDK 和公共接线文件 | ✅（仅动本路 2 个修改文件 + 2 个新增文件） |
