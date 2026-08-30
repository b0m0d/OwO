# 七期 QA 报告：真实 Worker 能力落地——输出契约执行器、角色权限与取消、Artifact 校验交付、公共契约与 UI（第四路收口汇总）

## 0. 结论

七期四路功能全部落地并完成工程收口。门禁全绿（真实退出码）：

| 门禁项 | 结果 |
|---|---|
| `cargo check`（core / server / protocol / cli） | ✅ 0 |
| `cargo fmt --check`（各路自报文件 + 收口复检 core/server） | ✅ 0 |
| `cargo clippy -p owo-agent-server`（lib / lib-test） | ✅ 0 告警 |
| `cargo clippy -p owo-agent-core`（lib / lib-test） | ✅ 0 告警 |
| 路由契约 `route_contract_tests` | ✅ **14/14** |
| core 全套（含新定向测试） | ✅ lib **359/359**、contract_worker **11/11**、artifact_pipeline **6/6**、product_eval_workswarm 4/4 |
| server 定向测试 | ✅ artifact_delivery_api **5/5**、project_workspace 全绿、workspace_change_tracker 单测全绿 |
| TS typecheck / unit | ✅ 0 / **12/12**（schema.d.ts 再生成 + 七期类型级契约块） |
| desktop/web 面板测试（5 文件） | ✅ **163/163**（action-center 17 / workswarm 55 / launcher 29 / artifact-review / product-eval） |
| 真实模型冒烟 ①代码任务 | ✅（二路执行，详见 §3.1） |
| 真实模型冒烟 ②研究/结构化 | ✅（收口补跑，详见 §3.2） |
| `git diff --check` | ✅ 0（简报所列 3 处格式问题复核时已不存在：AGENTS-COORD.md 全文无行尾空格、workswarm.panel.test.mjs 以单换行结尾；收口时逐字节复检确认） |

## 1. 交付面（四路汇总）

### 1.1 第一路：生产 Worker 输出契约执行器
- `crates/owo-agent-core/src/contract_worker.rs`（新建）：`enforce_worker_output_contract` 三态解析 + 角色复验，定向修复至多一次、修复后 producer/critic 双复验；`ContractSubagentRunner` 字段集与 `SubagentRunner` 相同，`SubagentRunner::run` 纯委托。
- 自由文本交付路径彻底关闭；失败前缀冻结 `output_contract_invalid:`（UI 容错 `failure_code`/`error` 双口径）。
- ProductEval `EvalAgentWorker` 与生产 `SubagentRunner` 复用同一执行器（stats 口径不变，`bump_output_repairs`）。
- 定向测试 `tests/contract_worker_tests.rs` 11/11；core lib 343→359 无回退。

### 1.2 第二路：角色工具权限、预算、取消与代码变更跟踪
- `crates/owo-agent-core/src/worker_profile.rs`（新建）：`WorkerProfile`（visible_tools/read_only/write_allowed_paths/max_turns/can_use_browser/can_run_command），按角色映射（analyzer/reviewer 只读；implementer/finalizer 受控读写；researcher 浏览器无写）；`ToolRegistry` 按角色裁剪注册；`budget_calls_per_role` → 真实 `max_turns`。
- 单写租约（write_lease，同一工作区同时仅一个写角色）；TeamRun cancel 经取消令牌桥接到运行中的 Worker（冒烟 ②见 §3.1：244ms 转停止）；写角色执行前后 Git 快照 → `crates/owo-agent-server/src/workspace_change_tracker.rs`（新建）落盘 `changed_files/diff_summary/diff_ref/violation`；白名单外变更判 `scope_violation`。
- 修复 verbatim `\\?\` 前缀混用（canonicalize 与绑定侧 simplify 不一致导致白名单内写入被误拒）——双侧 simplify + 回归测试。
- 修复写角色假交付：写角色画像提示词强制 `write_file` 落盘 + 最后一回合只出契约 JSON，预算成为真实硬上限。
- 新路由 `GET /projects/{id}/workspace/changes`（operationId `projectWorkspaceChanges`，四路完成 openapi/契约/TS 三方同步）。

### 1.3 第三路：Artifact 校验、证据链与下载交付
- `crates/owo-agent-core/src/artifact_pipeline.rs`（新建）：登记前格式门控——json 可解析且禁 Markdown 围栏 / csv 表头+行列数一致 / research ≥1 有效证据（URL 或文件引用）/ markdown 拒 TBD 与空模板；未过门控 `artifact_invalid:` 拒收 + 审计 + 零落盘。
- 登记面 additive 落盘 `format/media_type/file_name/sha256/size_bytes/evidence_refs/open_issues/validation/handoff`；Worker handoff 写入真实 HandoffRecord。
- 三路由：`GET /artifacts/{id}/content`（`?raw=true` 原始下载流）、`GET /artifacts/{id}/metadata`、`GET /projects/{id}/delivery-manifest`——经 `artifact_review_api.rs` 内 `#[path]` 子模块挂载（server/lib.rs 零改动，挂载点回执见 AGENTS-COORD 七期留言区）。
- 定向测试 `tests/artifact_delivery_api_tests.rs` 5/5；收口期修复其 649 行 E0308（泛型 `Arc::clone` 参数位不做 unsize 强转 → 按仓库惯例 `as Arc<dyn ProjectSpaceStoreBackend>` 显式强转）。

### 1.4 第四路：公共契约、执行详情与「待我处理」UI
- 新面板 `desktop/web/panels/action-center.panel.js`（+测试 17/17）：四类待办纯客户端聚合既有路由——等待 Human 结果 / 待评审+校验未通过产物 / 可重试失败步骤 / 写租约持有；retry 提交锁 + 成功重聚合 + 深链；产物 404 静默、详情失败降级。
- WorkSwarm 详情七期区：WorkerProfile 表（角色×工具×预算）、写租约三态、文件变更+diff 折叠预览、产物校验徽标/证据/Handoff/sha256、Artifact 下载（按 format 推断扩展名）、交付清单下载、`stopping/stopped`「正在停止/已停止」全链路（状态中文/门控/横幅/摘要/CSS）。
- Launcher 步骤⑥预览升级「实际执行权限」（`rolePermissions`：只读工作区全员只读、评审角色恒只读、可写按路径 scoped/留空 denied、浏览器按角色画像）。
- TS：`schema.d.ts` 再生成 + `client.unit.test.ts` 七期类型级契约块（team200 三新字段 + required 不变守卫、artifacts 四新字段、3 条交付路由 + `projectWorkspaceChanges` 的 200/404 双态）。
- `style.css` 第 21 节 Action Center 样式 + 模块 CSS 补停止态徽标与 rv-* 评审徽标配色。

## 2. 公共契约与路由面（收口终态）

七期路由面共 5 条新增（全部经模块 router 自挂载或既有 merge，lib.rs 装配零新增 merge）：

| 路由 | operationId | 来源 |
|---|---|---|
| `GET /projects/{id}/workspace/changes` | `projectWorkspaceChanges` | 二路 |
| `GET /artifacts/{id}/content` | `artifactContent` | 三路 |
| `GET /artifacts/{id}/metadata` | `artifactMetadata` | 三路 |
| `GET /projects/{id}/delivery-manifest` | `projectDeliveryManifest` | 三路 |
| `/teams/{id}` 200 additive | —（worker_profiles/write_lease/changes，双路径容错） | 二路 |

- `/teams/{id}` openapi 行在收口期发现 3 个多余闭括号（字段被拼到 `schema.properties` 之外），按 HEAD 原行重建并经 JSON.parse 结构验证（字段回到 properties 内、`required` 仍为 `[team,interrupted,tasks,audit_tail]`）。
- 快照 `clients/ts/openapi.json`（gitignored 本地工件，250 路径）与 served spec 双向等价；契约测试 14/14。

## 3. 真实模型冒烟（glm-5.3-flash，凭据仅经用户级环境变量注入）

### 3.1 冒烟①：代码任务（二路执行）
`code-change-v1` 模板 + 绑定白名单 `src/out`：三角色全 Succeeded；implementer 以 `write_file` 真实修复 `src/calc.rs`（`a-b`→`a+b`）并写出报告；`GET /projects/{id}/workspace/changes` 返回 `changed_files` + `diff_summary="1 file changed, 1 insertion(+), 1 deletion(-)"` + `diff_ref` 补丁；无越界；各角色调用数全部 ≤ 预算。附带验证：运行中 cancel → **244ms** 转 cancelled（要求 ≤2s）、全角色 Aborted、产物 0。

### 3.2 冒烟②：研究/结构化任务（收口补跑）
```
cargo run -q -p owo-agent-cli -- product-eval run --exec workswarm --agents multi --reps 1 --only research-source-compare --fresh
```
- 结果：**Passed 1/1（100%）**，wall 56,741ms，模型调用 7 次（自适应预算 budget_calls_total=7，全部 ≤ 预算），tokens 8,091。
- 自适应组队判定：`mode=team roles="researcher+leader" budget=7`（auto 判定，未启用多余角色）。
- 产物管线实弹验证（`scratch-eval-runs/product-eval/workswarm-teams/.../projects.db` 内登记记录）：
  - `validation: {"format":"markdown","valid":true}`——research→markdown 交付通过格式门控；
  - `evidence_refs: ["sources/source_a.md (…)", "sources/source_b.md (…)"]`——研究证据链真实落盘（两处源文件引用）；
  - `sha256` / `size_bytes` / `handoff` 全部落盘；`open_issues: []`。
- 最终交付：`final_artifact_ref = team-…:leader:v1`（producer 链 approved head 语义）。

### 3.3 证据留痕
- 冒烟输出目录：`scratch-eval-runs/product-eval/`（gitignored，不入提交）。
- 报告：`scratch-eval-runs/product-eval/report.json`；journal：`state.jsonl`。

## 4. 提交边界（按功能边界分批）

1. `feat(core)`：输出契约执行器 + 角色权限/预算/取消 + artifact 管线（含各自定向测试）。
2. `feat(server)`：workspace 变更跟踪/新路由 + artifact 交付三路由（含定向测试与修复）。
3. `feat(protocol)`：七期 additive 字段。
4. `feat(clients)`：openapi 快照同步产物 schema.d.ts + TS 类型级契约测试。
5. `feat(web)`：Action Center 面板 + WorkSwarm 七期详情区 + Launcher 实际执行权限 + 测试守卫。
6. `docs(qa)`：本报告 + AGENTS-COORD 七期登记/留言。

排除项（不入提交）：`scratch-eval-runs/**`、`scratch-ws-ui/**`、运行期数据库（`*.db`/`space.db`）、CAS 缓存、补丁缓存、日志；全部位于 gitignore 规则内（提交前逐一核对 `git status`）。

## 5. 已知边界

- `transition_tests.rs` 存量 clippy 告警（16 条 Default 字段赋值风格）为历史文件、本轮无人触碰，未清（避免无主文件搅动）。
- `usage.rs` 死代码类告警为 HEAD 既有（integration test 目标），本轮零新增。
- Action Center 的四类聚合仍为前端临时聚合；八期三路将以 `/human/inbox` 持久化收编。
- TeamRun observation.json 摘要未携带七期 additive 字段（legacy 摘要结构）；权威数据以 space.db / 变更端点 / metadata 路由为准。
- 真实模型冒烟样本 n=1/类，置信区间仅供参考（V1-R2「多 Agent 稳定优于单 Agent」量化门槛未在本轮评估范围）。
