# 六期 QA 报告 · 第四路 Project Launcher UI 与公共集成

> 任务：把工作区、模板、自适应组队和交付物串成普通用户可直接使用的启动流程。
> 集成顺序固定：一路输出契约 → 二路工作区 → 三路模板 → 四路公共契约与 UI 收口（第四路独占共享契约文件）。
> 状态：✅ 收口（本文由第四路维护，记录 UI/接线实测与三路形状对齐记录）

## 交付面

### 1. 新面板 `desktop/web/panels/project-launcher.panel.js`（七步流程）

① 任务目标 → ② 绑定项目目录 → ③ 只读/写入范围（允许路径 + 目录树深度）→ ④ 组队模式
（auto/single/team，缺省 auto）→ ⑤ 模板（仅已安装可选；候选一键安装，幂等）→
⑥ 预览（角色/依赖/预算/权限，双来源：模板条目或组队模式说明）→ ⑦ 创建并直达 TeamRun 详情
（经 `window.OwoPanels.workswarm.open()`，不重复挂载面板）。

- 纯函数拆分（`_test` 导出）：`parseWritePaths` / `pathIsSafe`（客户端即拒 `..` 与盘符转义）/
  `validateState` / `buildCreateBody`（冻结契约请求体：`strategy` + `workspace{root,read_only,
  write_allowed_paths,tree_depth}` + `template_id`；single/team 策略显式带 `mode`，auto 不带）/
  `normTemplate`（目录双形状归一）/ `previewFromTemplate` / `previewFromStrategy` /
  `permissionsSummary` / `buildPreview` / `templateOptionsHtml` / `catalogHtml` / `viewHtml`。
- 行为守卫：创建提交锁（快速双击零重复请求）、校验失败零请求、错误 `aria-live`、
  `syncStateFromDom` 元素缺失时保留 state（Node 测试零 DOM 可跑全流程）。

### 2. WorkSwarm 详情补充（六期）

- **工作区与模板区**（新增 section，位于组队策略区之前）：绑定 root（`code` 样式防溢出）、
  读写模式与允许路径、目录树深度；「目录树」「Git 状态」按钮实拉
  `GET /projects/{pid}/workspace/tree|git-status` 并渲染（扁平树按路径层级缩进、
  porcelain 行解析为 path+state 徽标）；「刷新工作区」重拉绑定。
- **使用的模板及版本**：`templateBoxHtml` 展示 template_id 与名称；版本经模板目录懒加载解析
  （双形状容错），动态组队显示"由组队策略判定"。
- **输出契约失败原因**：`failureCodeLabel`/`failureBadgeHtml`/`failureSummaryHtml` ——
  `output_contract_invalid`（输出契约无效）/`artifact_missing`（缺少交付物）/`scope_violation`
  （越权访问）三码徽章，`failure_code` 字段优先、error 前缀兜底。
- **最终交付物入口**：沿用五期 `#ws-dlv-toggle`（已验证在场）。
- 面板新增公开方法 `open(teamId)`（Launcher 直达详情入口）。

### 3. 公共接线（第四路独占文件）

- `crates/owo-agent-server/src/lib.rs`：`mod team_template_catalog_api` +
  `build_router` merge `team_template_catalog_api::team_template_catalog_router`（二路四条
  workspace 路由经其 `#[path]` 模块在 `workswarm_api::router` 内自挂载）；openapi 六条目
  （PUT/GET workspace、tree、git-status、catalog、install）+ `POST /teams` 请求体
  `workspace` 字段。
- `crates/owo-agent-server/tests/route_contract_tests.rs`：`sample_body` +2
  （PUT workspace 最小体、install `{}`）、`resource_404_ok` +4（workspace/tree/git-status/
  install；catalog GET 恒 200 不入 404 表）。
- `clients/ts/*`：快照再生成 + `schema.d.ts` + `client.unit.test.ts` 六期契约类型级断言
  （TS 10/10）。
- `desktop/web/app.js`（PANEL_ORDER + project-launcher）、`index.html`（+1 script）、
  `style.css`（第 20 节：Launcher 布局/预览/目录卡/工作区树/失败徽标/窄栏断点）。

## 三路形状偏差与对齐记录（留言区冻结契约 vs 实现）

| 项 | 冻结契约 | 实际实现 | 处置 |
|---|---|---|---|
| 模板目录条目 | 顶层 `template_id/version/title/category` | 嵌套 `template.template_id/template.name`，无 version | UI `normTemplate` 双形状归一；spec 描述改实测形状 |
| 模板角色依赖 | `edges[]` | `template.roles[].depends_on` | 归一折算为边（预览显示依赖计数） |
| 模板预算 | `budget_calls_total` | `budget_calls_per_role[]` | 求和展示（如 3+5+3=11） |
| 安装响应 | `{template_id,version,installed,replayed}` | `{installed, already_installed, template, auto_match, budget_hint}` | UI 双键判定幂等（`replayed‖already_installed`） |
| GET workspace | 顶层绑定对象 | `{workspace:{...}}` 包装 | UI 解包 + spec 同步 |
| git-status | `{is_git_repo,branch,clean,entries:[{path,state}]}` | `{git,porcelain,entries:[" M path"...]}` | UI 双形状解析 porcelain 行；spec/TS 同步 |
| TeamRun.workspace 回显 | 创建/详情 additive 透出 | 详情未透出（绑定落盘可查） | UI 经 `GET /projects/{pid}/workspace` 回退拉取（实测可用）；偏差已在此备案 |

## 集成修复（第四路收尾授权，均留痕）

1. 一路 `tests/product_eval_workswarm_tests.rs` 为两份同内容拼接（`//!` 内嵌注释块落文件中部
   E0753 + 重复定义）→ 去重为单份并补尾部闭合（353 行）。
2. 一路 `src/product_eval/workswarm_executor.rs` cfg(test) 两测试仍用旧自由文本脚本 →
   换 WorkerOutputV1 契约信封（与其独立测试文件同款写法）+ `ForceTeam` 配置 +
   断言对齐新语义（空输出 → 契约定向修复 `output_repairs=1`、attempts=1、`retries_used=0`、
   修复不重跑 critic/leader）。
3. 收尾期 `tests/usage_tests.rs` 两例长期红（三路四期已登记"已知环境/全局态问题"，与各路
   改动无交集，本路仅测试端修复）：① `summary_aggregates` 断言 `cost_usd > 0` 恒假——
   summary 舍入到 3 位小数而默认单价 0.002 $/Mtok 下 150 token = 3e-7 恒为 0.0，测试内
   显式 `set_price_per_mtok(50.0)`；② `persist_load` 与前者共享 `global()` 全局态，并行
   reset 互踩（观察到"恢复 2 条"）——加静态互斥串行化。两例均不触碰 `usage.rs` 生产代码，
   修复后 usage_tests 6/6 连续两遍稳定。

## 门禁实测（收口时点）

- route contract **14/14**（六期五条新路径入遍历，404 语义正确）
- **server 全套件 33 套件全绿（exit 0）**：workswarm_api **16/16**、project_workspace_api **3/3**、
  team_template_catalog_api **5/5**、usage_tests **6/6**（收尾期修复后）、slo_tests **14/14**
  （收尾期修复后）等；最终一轮全量重跑确认
- core：lib **343/343**（含 workswarm_output 模块单测）、workswarm_tests **13/13**、
  product_eval_workswarm_tests **4/4**、team_strategy **12/12**、artifact_review 16、
  product_eval_statistics 13、recovery/responsiveness 全绿（全套 `cargo test -p
  owo-agent-core` exit 0）
- TS：typecheck 0 错、unit **10/10**；web：**132/132**（Launcher 26 项）
- fmt：`cargo fmt --check` 两 crate 0 差异；clippy exit 0，残余告警均为历史遗留
  （transition_tests/goal_api_tests/`#[path]` 独立编译固有死代码等，非六期引入）
- **浏览器完整闭环 ok=YES（0 页面异常 / 0 console 错误）**：
  Launcher 目录 4 模板 → 安装 code-change-v1（幂等实测：二次 `already_installed` 不覆盖）→
  模板预览（3 角色 code_analyzer→implementer→reviewer、预算 11 次）→
  auto + workspace 绑定建队 → 直达详情 → 工作区框（root/只读/深度）→
  模板名回显 → 目录树（📁📄 层级缩进）→ Git 状态（porcelain 23 项变更渲染）→
  失败原因区 + 交付物入口在场。截图 `scratch-ws-ui/shots6/launcher-detail.png`。

## 已知边界

1. TeamRun 详情响应未回显 `workspace` 字段（一路交接记录口径）——UI 经
   `GET /projects/{pid}/workspace` 回退拉取，功能完整；后续如需透出为 additive 变更。
2. 未安装模板在选择器中禁用但目录卡片仍展示（候选语义：展示≠自动参与匹配，
   `find_match` 只匹配已安装，由三路实现保证）。
3. Launcher 建队后的真实 Worker 运行目录等于绑定目录由二路实现保证（其
   `load_binding`/`WorkspaceScope` 在 AgentSubagentWorker 启动时生效），本路验证到
   绑定回显与树/git 读取层。
4. git-status 无 branch 字段（porcelain 口径），UI 不渲染分支行；非 Git 目录
   `git:false` + 空 entries 优雅显示"干净"。
