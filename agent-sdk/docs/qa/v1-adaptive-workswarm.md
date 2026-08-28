# 五期 QA 报告：自适应组队、角色指标、Artifact 返工与最终交付物（第四路）

日期：2026-08-28 · 负责：第四路（公共接线 / 产品 UI / 工作树收口） · 环境：Windows / 隔离链路 4099(服务端) + 4174(静态代理)

## 0. 结论

- 本路五期功能全部落地并实测：**契约 14/14、TS unit 9/9、web node --test 106/106、core 七套件 69/69、clippy --all-targets 干净、浏览器产品闭环 E2E ok=YES（0 页面异常 / 0 console 错误）**。
- **组队策略与返工链标记已全量落地**：第一路交付引擎（`team_strategy.rs`，engine/阈值/测试齐备）后在创建面集成上停滞 80+ 分钟，第四路按 AGENTS.md 协作规则备案接管收尾——`POST /teams` `strategy`（auto 判定/single/team 强制，缺省 auto）+ `strategy_decision` 暴露（mode/roles/parallelism/budget_calls_total/reasons）+ 返工登记 `supersedes_artifact_id` 写入与前版让位。E2E 实测：策略理由框渲染"单 Agent…单 Agent 足够"、时间线 v1"已被取代"、v2 批准切换 approved head。

## 1. 交付面（本路）

| 计划项 | 落地 | 实测证据 |
|---|---|---|
| ① 建队界面 自动选择/单Agent/多Agent | `#ws-strategy` select（auto/single/team，缺省 auto），提交体带 `strategy` | 降级冒烟 + E2E `createFormStrategy={present:true,value:"auto"}`；旧后端忽略该字段（服务端无 `deny_unknown_fields`）|
| ② 自动模式显示组队理由/角色数/预算 | 详情页「组队策略与角色指标」区 `strategyDecisionOf` 容错读取 `strategy_decision‖strategy`（reasons/reason/why、roles、budget_per_role、parallelism） | 服务端未暴露时显示空态提示（`strategyHint:true`）；字段到场即自动渲染（单测覆盖）|
| ③ 角色指标卡 | `metricsFromPayload` 双形状容错（冻结契约 worker 行 + 三路实弹 roles[]/workers[] 别名映射）+ `metricsCardsHtml` 汇总 chips + 预算余量/耗尽徽章 | E2E 实测：`cards:1`，sum=`总墙钟 679ms 总调用 0 估算费用 $0.0000 最慢 critic 失败 0 返工 0 产物版本 1` |
| ④ Artifact 版本时间线 / v1v2 差异 / 返工 / approved head / 交付物入口 | 时间线（`.owo-ws-timeline`）+ LCS 行级 diff（400 行封顶）+ `details.owo-ws-rework` 返工表单（懒加载历史回填最近 request_changes 意见）+ 交付物三桶视图 | E2E：rework→**v2 真实生成**；批准 v2 后产物态 `["superseded","approved"]`；交付物区「已批准 1 / 待评审 0 / 驳回取代 1 / 交付完成 / 返工中 1 + 交付清单 cas://sha256:…」|
| ⑤ 下载诊断信息（已脱敏） | `#ws-d-diag` 按钮 + Blob 下载容错 + 「已脱敏」标注 | E2E 实测 `GET /teams/{id}/diagnostic` 200，键 `team/tasks/artifacts/reviews/handoffs/metrics/metrics_file/audit_tail/redaction/generated_at`，泄漏扫描（sk-/OPENAI_API_KEY/Bearer token）=false |
| ⑥ ProductEval 面板同步服务端统计 | `serverStats` 优先识别 `report_statistics` 真实形状（modes[]+comparison→Wilson CI95/p50/p95/样本充分性/启用建议/四维差值），后回落旧 per_engine 形状 | 单测 4 条（modes 正/反序、comparison 启用位、样本不足回落）|
| ⑦ 工作树清理 | 本路文件清晰边界提交；排除 skills-main*；不覆盖他人未收口文件（第一路在途文件未纳入本轮提交）| 见 §5 |

## 2. 公共接线（本路独占 `server/lib.rs` + 契约 + TS）

- `openapi_spec` 新增四条路由：`POST /artifacts/{id}/rework`、`GET /projects/{id}/deliverables`、`GET /teams/{id}/metrics`、`GET /teams/{id}/diagnostic`；同轮修正四期以来松散对象 schema（`artifact`/`approved_head` 等）缺 `additionalProperties` 导致 TS 生成 `Record<string,never>` 的类型污染（day-4 单测因此转红→修复后全绿）。
- `route_contract_tests.rs`：`sample_body`（rework 冻结契约体）+ `resource_404_ok`（四条新路由占位资源 404 语义）。**route contract 14/14**（含 spec↔快照↔router 三方一致性）。
- 快照/类型链路：活服 `GET /openapi.json` → `clients/ts/openapi.json`（873,507B）→ openapi-typescript 再生成 `schema.d.ts` → TS typecheck 0 → **TS unit 9/9**（新增「五期契约」类型级断言：rework 体/交付物三桶/metrics workers+summary/diagnostic 200 在场；响应键存在性用 `Present<R>` never-消解断言，避免把类型当值）。
- 一/二/三路路由均自行挂载（二路经 `artifact_review_api::router`、三路 `#[path]` 微注册），lib.rs 零冲突。

## 3. 实弹形状容错（与第三路收口留言映射逐项一致）

- metrics：`workers[]`（member_id/wall_ms/attempt/outcome/error/artifact{}）+ `roles[]` 汇总 + `summary`（wall_window_ms/model_calls/cost_usd/failed_spans/rework_count/slowest_worker{role}）+ `budget{exceeded,reason}` → UI 映射 `duration_ms=wall_ms、tokens=prompt/completion_tokens、est_cost=cost_usd、attempts=attempt、terminal=outcome、failures=failed_spans、reworks=rework_count、wall_clock=wall_window_ms、total_calls=model_calls、total_cost=cost_usd`；`slowest_worker` 对象提 `.role`；`artifact` 对象提取 `artifact_id`。
- deliverables：`pending_review` 键并入待评审桶、`rejected_or_superseded` 可为 null、`complete/delivery_manifest_ref/rework_tasks` 增强渲染。
- 新增 2 条单测以实测响应为 fixture（团队 metrics live shape / deliverables live shape）。**web 106/106**。

## 4. 浏览器产品闭环 E2E（qa5-e2e.mjs，puppeteer-core + 真 Chrome）

流程：auto 建队（echo critic）→ 运行出产物 → 指标刷新（实弹卡）→ 评审「要求修改」（v1→Draft）→ 展开返工表单（懒加载回填 review_id + 意见预填）→ 提交返工 → **轮询到 v2（PendingReview）** → 批准 v2 → **v1 superseded + v2 approved** → 最终交付物（approved v2 + manifest + 交付完成徽标）→ 下载脱敏诊断（200 + 泄漏扫描 false）。

结果：`ok=YES`、`pageErrors=0`、`consoleErrors=0`；截图存 `scratch-ws-ui/shots5/`（metrics/request-changes/v2/approved/deliverables/diagnostic）。

降级冒烟（qa5-degrade.mjs，旧后端无新路由）：策略选择器在场、指标空态、交付物/诊断 404 容错（带重试/文案）、零页面异常——新 UI 对未接线后端安全。

E2E 调试中修复的脚本级问题（非产品缺陷）：固定 sleep 改显式等待；返工预填触发器应为 `summary` click（面板委托挂在 SUMMARY click 上，真实用户点击路径无恙）。

## 5. 提交边界

- 本轮提交（第四路名下）：`desktop/web/panels/{workswarm,eval}.panel.js`、`desktop/web/style.css`、`desktop/web/tests/{workswarm,product-eval}.panel.test.mjs`、`owo-agent-server/src/lib.rs`、`owo-agent-server/tests/route_contract_tests.rs`、`clients/ts/src/schema.d.ts`、`clients/ts/tests/client.unit.test.ts`、本报告、`AGENTS-COORD.md`。
- 不纳入：第一路在途文件（`team_strategy` 创建面接入、rework supersedes 标记）、skills-main*；scratch-ws-ui/ 与 data5/shots5 均忽略。
- 一路落地后的追加动作（已备案）：重建 → 重快照/schema → 重跑契约/TS/web/E2E（补策略理由框与合并链时间线断言）→ 随全量收口提交。

## 6. 已知边界

1. ~~`strategy_decision` 暴露与 `supersedes_artifact_id` 写入在第一路（在途）~~ **已由第四路接管集成落地并 E2E 实测**（见 §0）；裁剪口径：显式 single 强制单角色；auto 判定 single 仅在「未显式给角色且未命中已采纳模板」时裁剪，用户编排与模板复用始终尊重。
2. 并发批次下 token/费用为共享 provider 快照差值近似（三路 `attribution_note` 口径），UI 原样展示不换算。
3. 无模型用量 span 的 token 三字段为 null（区别于 0），指标卡显示「—」。
4. auto 判定的 `strategy_decision.roles` 为引擎计划角色名（producer），单角色裁剪保留用户首角色绑定（如 critic）——计划角色名与实际成员名在该场景下可不同，duty/预算语义一致。
