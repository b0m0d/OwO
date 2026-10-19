# WorkSwarm 实时进度 + Artifact 评审 UI QA 报告（V1 四期联动）

- 日期：2026-08-28
- 责任：四路并行日四期 · 第四路（实时进度与 Artifact 评审 UI）
- 改动文件：`desktop/web/panels/workswarm.panel.js`、`desktop/web/panels/eval.panel.js`、`desktop/web/style.css`（新增第 18 节，`.owo-ws-*`/`.owo-pe-*` 作用域）
- 新增文件：`desktop/web/tests/artifact-review.panel.test.mjs`、本报告
- 方法：**全部结论来自当日实测**——Node 单测（node:test）、`node --check` 语法门禁、隔离真实链路（owo-agent-server:4099 + 静态/反代宿主:4174 → 无头 Chrome puppeteer-core 交互驱动）；未用历史绿灯代替。

## 1. 测试环境（真实链路，与共享端口隔离）

| 项 | 值 |
|---|---|
| 服务 | `owo-agent serve --port 4099`（debug 构建；`OWO_AGENT_DATA` 指向 gitignored scratch `data4`，与其它路隔离） |
| 凭据 | 服务器启动要求 `OPENAI_API_KEY` 在环境变量（按 AGENTS.md 标准注入法自用户级注入进程；本路 QA 全程仅用内置 `echo/sleep` worker，**零模型调用、零费用、密钥零回显**） |
| 页面宿主 | 本地静态+反代（127.0.0.1:4174 → `desktop/web` + API 4099，流式转发兼容 SSE） |
| 浏览器 | 无头 Chrome（本机安装版）+ puppeteer-core，1280×720 主测 + 三分辨率复测 |
| 种子数据 | 经真实 HTTP API 创建：echo 链 ×1（产物链）、sleep 90s ×1（实时进度）、critic 队 ×3（critic 产物天生 `PendingReview`，供三动作）、sleep 25s ×1（取消）、人类节点队 ×3（human-result 产物为 `draft`） |

## 2. 静态门禁（实测）

| 门禁 | 命令 | 结果 |
|---|---|---|
| 语法 | `node --check panels/*.js`（13 文件） | **通过（exit 0）** |
| 单测 | `node --test tests/*.test.mjs` | **85/85 全绿**（workswarm 27：进度 seq 单调守卫/嵌套帧分发/耗时视图/取消中/样式守卫；artifact-review 22：版本链分组/徽标映射/请求体冻结契约/生产者自批拦截/409·403·网络文案/忙锁/flash 存续；product-eval 36：Wilson CI 边界（0 样本/全成功/全失败/半数/非法输入）、p50/p95 序统计、服务端统计优先归一、启用阈值判定、样本不足标注） |

## 3. 实时进度区（第二路 progress 事件，真实 SSE 实测）

- **fetch 流式 SSE**：面板升级为可携带 Bearer 的 fetch ReadableStream 解析（`data:` 帧解析，多行 data 拼接），实测连接后指示灯 `owo-ws-live on`；裸 EventSource（无法带凭据）保留为回退，均失败再降级 2.5s 轮询。
- 90s sleep 团队运行中：进度区显示 `seq #1`（单调）、计数徽标（等待 0 · 运行 1 · 完成 0 · 失败 0）、运行中步骤行高亮 + `第 1 次尝试` + **已运行 6s→8s 滴答**（1s tick，仅存在运行中步骤时活跃，步骤终态自动停表）。
- seq 守卫：旧/重复 seq 跳过（断线重连、轮询快照去重）；`lastProgressSeq` 仅在切换团队时清零。嵌套帧 `{type:"progress",progress:{…}}`（第二路实现）与扁平形状（计划原形）均兼容，单测锁定。

## 4. 长任务响应性（第二路三阶段锁的 UI 侧实测）

| 指标 | 实测值（90s sleep 执行中） |
|---|---|
| 页面 JS 主线程 | rAF 循环 30 帧连续可用（164–172ms/30 帧），无冻结 |
| `GET /teams/{id}` 延迟 | **5–13ms（HTTP 200）**——二期实测同场景被阻塞至阶段边界，本期第二路短锁拆分后详情即时返回 |
| 取消（cancelme 队，25s sleep） | 确认弹窗接受后 1.2s 轮询窗口内「取消中…」徽标即时出现；终态 `已取消` 徽标自动清除；重复/终态取消由服务端幂等保证 |

## 5. Artifact 评审闭环（第三路 review API，真实 POST 实测）

| 动作 | 队列 | 实测结果 |
|---|---|---|
| 批准 approve | critic1（产物 `team-…:critic:v1`） | 200/201：徽标 `已批准`，链头显示 **当前 approved head：v1**；评审历史懒加载渲染 1 条不可变记录（决策徽标+评审者+评语+时间戳） |
| 要求修改 request_changes | critic2 | 成功：徽标迁移 `草稿（返工中）`（服务端 PendingReview→Draft 语义），链头「无已批准版本」如实显示 |
| 驳回 reject | critic3 | 成功：徽标 `已驳回` |
| 409 乐观并发 | critic1（已批准 v1） | 人为污染客户端 expected_version=0 → 服务端真实 **409**（版本冲突：产物当前为 v1，提交基于 v0）→ UI 提示计划规定文案「**版本已更新，请刷新后重试**（你提交的 expected_version 已过期…）」 |
| 生产者自批 | — | 客户端前置拦截（reviewer==producer 时 approve 抛「生产者不能自行批准自己的产物」）；服务端 403 语义由第三路测试锁定，UI 403 文案在场（单测） |
| 忙锁 | — | 提交进行中重复提交被拒（`reviewBusy` 锁 + 按钮 disabled），成功/失败后解锁 |
| 评审表单可见性 | — | 仅 `PendingReview` 产物渲染表单与三按钮；Draft/Approved/Rejected/Superseded 无评审入口 |

评审结果提示为 **state 驱动 flash**：提交成功/失败后即使产物区因刷新重渲染（批准后表单消失），提示仍在产物行级保留，失败提示带红色语义类。

## 6. eval 统计判读（第一路统计口径的 UI 消费）

- 真实 reference 运行（`eval-8ed9…`，202→completed）选中后统计区渲染：
  - 单 Agent / WorkSwarm 各一行：`n=10`、成功率 `100.0%`、**CI95 [72.2%, 100.0%]**（Wilson）、p50/p95；
  - 两行均带**样本不足徽标**（n<30）+ 显式说明条「样本不足：当前样本量 n<30，置信区间偏宽、差值与启用建议仅供观察，不构成上线依据」；
  - 差值行（多 − 单）：成功率 pp / p50 耗时 % / 模型调用 / Token / 费用；
  - 启用建议按冻结阈值（成功率 +5% / p50 耗时 −30%）判定，未达阈值如实显示「暂不建议启用」，并标注**判定来源：客户端推导**；服务端 statistics 字段在场时优先采用并标注来源（宽容归一，单测覆盖 `{lo,hi}`/`[lo,hi]`/verdict 形状）。

## 7. 三分辨率横向溢出（无头 Chrome 实测 `scrollWidth vs clientWidth`）

| 分辨率 | documentElement (scroll/client) | 整体横向滚动 | 实测视图 |
|---|---|---|---|
| 1280×720 | 1280 / 1280 | **无** | WorkSwarm 详情（产物链+评审表单）+ eval 面板 |
| 1440×900 | 1440 / 1440 | **无** | 同上 |
| 1920×1080 | 1920 / 1080 对应 1920 / 1920 | **无** | 同上 |

## 8. 控制台与网络

- **pageErrors：0**（全部页面加载与交互累计）。
- console 错误仅 **1 条**：409 并发探针的浏览器自动资源日志（有意注入的冲突请求，非面板缺陷）；其余交互**零控制台错误、零失败请求**。
- 已知边界（如实记录）：① 版本链 live 实测为单件链——第三路 rework 流程（request_changes → 重做 → v2 supersedes v1）的链合并写入方尚未落地，链分组/断链/approved head 选取由 22 条 Node 单测覆盖，后端落地后无需 UI 改动即自动生效；② `GET /teams/{id}/events?format=json` 快照暂不含 progress（仅 SSE 帧携带），轮询降级模式下进度区显示最近一次快照/事件，SSE 恢复后自动续推。

## 9. 截图存档

`scratch-ws-ui/shots4/`（gitignored）：`qa-progress-live.png`、`qa-responsiveness.png`、`qa-cancel.png`、`qa-approve.png`、`qa-request-changes.png`、`qa-reject.png`、`qa-409.png`、`qa-eval-stats.png`、`qa-detail-{1280x720,1440x900,1920x1080}.png`。
