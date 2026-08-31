//! WorkSwarm S0 HTTP 路由（§8.5 / §9.0 S0）。
//!
//! 路由面：
//! - `POST /teams` 创建 single/team/swarmflow 运行（后台运行循环驱动阶段推进）；
//! - `GET /teams` 团队运行列表（含运行中标志）；
//! - `GET /teams/{id}` 成员、预算、状态（+ 任务视图 + 审计尾迹）；
//! - `GET /teams/{id}/tasks` TeamRun 的任务图（步骤 × 状态）；
//! - `GET /teams/{id}/events` 团队事件流 SSE（审计重放 + 状态轮询；`?format=json` 一次性快照，
//!   五期起快照含 `progress`——轮询降级也能看到当前步骤）；
//! - `GET /teams/{id}/metrics` TeamRun 指标（角色 span 聚合 / token·费用 / 最慢 Worker /
//!   失败·返工次数 / Artifact 版本数 / 预算状态；五期 · 第三路）；
//! - `GET /teams/{id}/diagnostic` 脱敏诊断导出（TeamRun/任务/产物/评审/交接/指标/审计；
//!   凭据类键值与令牌已脱敏、超长文本截断；五期 · 第三路）；
//! - `POST /teams/{id}/steer` continue/steer/replace/cancel/retry（R2：retry 局部重试 + 中断恢复）；
//! - `GET  /teams/{id}/change-sets` 团队 ChangeSet 列表 + 批准门控状态（八期 · 二路）；
//! - `GET  /change-sets/{id}` 单个 ChangeSet；
//! - `POST /change-sets/{id}/accept|reject|revert` 接受/拒绝/撤销（reject/revert
//!   安全恢复：用户改过的文件 409 + conflicted，不覆盖；八期 · 二路）；
//! - `GET /projects/{id}` Project Space 摘要；
//! - `GET /projects/{id}/artifacts` 版本化共享产物（ref 列表）；
//! - `GET /projects/{id}/workspace/changes` Worker 代码变更追踪（七期 · 二路：
//!   变更文件 + diff 摘要 + 逐步骤记录 + 白名单越界原因）；
//! - `POST /tasks/{id}/handoff` 结构化接力（team_id 在请求体）；
//! - `POST /tasks/{id}/human-result` Human 节点提交结果（team_id 在请求体）；
//! - `GET /teams/templates` 已采纳模板；
//! - `GET /teams/templates/proposals` 模板提案（只提案，不自动启用）；
//! - `POST /teams/templates/proposals/{proposal_id}/adopt` 采纳提案；
//! - `POST /teams/templates/proposals/{proposal_id}/reject` 拒绝提案（保留记录，可审计）。
//!
//! 五期（第三路）指标接线：`build_run_registry` 用 `MeasuredRoleWorker` 包装每个
//! `RoleWorker`（span 指标 JSONL 落盘 TeamRun 数据目录，重启可读）；`run_team_loop`
//! 在每阶段前做指标预算门（`TeamRun.budget` additive 支持 `max_cost_usd`/`max_wall_secs`，
//! 超限停止调度下一阶段并留审计）。指标/脱敏实现见子模块 [`workswarm_metrics`]。
//!
//! 七期（第二路）权限接线：`build_run_registry` 按角色画像（`WorkerProfile`）装配
//! 每个角色的工具注册表（注册表面即权限边界，模板 `budget_calls_per_role` → 真实
//! `max_turns`）；写角色经单写租约互斥，执行前后 git 快照 → 变更摘要/diff ref 落盘
//! （子模块 [`workspace_change_tracker`]；白名单越界 → `scope_violation` 步骤失败）；
//! 团队取消令牌经桥接任务置位共享 abort 标志，运行中 Worker 协作即时中断。
//!
//! S0 边界：产物经 CAS ref 传递（大对象不进响应体）；agent 角色经
//! `Agent::run_subagent` 模型驱动；内置 echo/sleep/fail worker 供测试与演示。

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::goal::{Worker, WorkerRegistry};
use owo_agent_core::permissions::AutoApprover;
use owo_agent_core::project_space_store::{ProjectSpaceStoreBackend, SqliteProjectSpaceStore};
use owo_agent_core::subagent::SubagentRunner;
use owo_agent_core::workswarm::{
    CreateTeamRequest, HandoffFields, RoleSpec, RoleWorker, SteerCommand, TeamCoordinator,
    TeamTemplateRegistry, WorkSwarmError,
};
use owo_agent_protocol::{TeamMode, TeamRunStatus};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

// 六期（第二路）：项目工作区绑定（真实目录 / 只读 / 写白名单）。
// 独立文件，经本模块 router 合并挂载（子模块可访问本模块私有项）。
#[path = "project_workspace_api.rs"]
pub(crate) mod project_workspace;

// 七期（第二路）：工作区变更追踪（Worker 执行前后 git 快照 + 写白名单校验 +
// 变更摘要/diff ref 落盘；经本模块 router 挂载读取面）。
#[path = "workspace_change_tracker.rs"]
pub(crate) mod workspace_change_tracker;

// 八期（第二路）：ChangeSet 审批、接受与安全撤销（独立文件，经本模块 router
// 合并挂载；子模块可访问本模块私有项：error_response 与 project_workspace）。
#[path = "change_set_api.rs"]
pub(crate) mod change_set_api;

// 七期（第二路）：角色画像（工具面/只读/写白名单/回合上限）驱动真实 Worker。
use owo_agent_core::worker_profile::{intersect_paths, ProfileSubagentRunner, WorkerProfile};

use owo_agent_server::AppState;

// 五期（第三路）：指标/预算/脱敏实现（#[path] 子模块声明 = 零 lib.rs 接线，
// 物理文件 crates/owo-agent-server/src/workswarm_metrics.rs；接线归第四路，
// 其后续若要在 crate 根登记 `pub mod workswarm_metrics;` 可平移，无语义差异）。
#[path = "workswarm_metrics.rs"]
pub mod workswarm_metrics;

// ---------------------------------------------------------------------------
// 状态（进程内单例协调器，懒初始化；目录 = data_root/workswarm）
// ---------------------------------------------------------------------------

/// WorkSwarm 服务端状态：`data_root/workswarm/{space.db, cas, templates, runs}`。
#[derive(Clone)]
pub struct WorkSwarmState {
    inner: Arc<Inner>,
}

struct Inner {
    base_dir: PathBuf,
    coordinator: std::sync::OnceLock<Arc<TeamCoordinator>>,
}

impl WorkSwarmState {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_dir,
                coordinator: std::sync::OnceLock::new(),
            }),
        }
    }

    /// 获取（并懒初始化）协调器。
    pub fn coordinator(&self) -> Result<Arc<TeamCoordinator>, WorkSwarmError> {
        if let Some(c) = self.inner.coordinator.get() {
            return Ok(c.clone());
        }
        let base = self.inner.base_dir.clone();
        std::fs::create_dir_all(&base)
            .map_err(|e| WorkSwarmError::Io(format!("创建 workswarm 目录失败：{e}")))?;
        let store_path = base.join("space.db");
        let store = SqliteProjectSpaceStore::open(&store_path).map_err(WorkSwarmError::Store)?;
        let cas = owo_agent_core::cas_store::CasStore::new(base.join("cas"))
            .map_err(|e| WorkSwarmError::Io(format!("CAS 初始化失败：{e}")))?;
        let templates = Arc::new(TeamTemplateRegistry::new(base.join("templates")));
        let audit = Arc::new(std::sync::Mutex::new(
            owo_agent_core::audit::AuditLog::default(),
        ));
        let mut coordinator = TeamCoordinator::new(
            Arc::new(store)
                as Arc<dyn owo_agent_core::project_space_store::ProjectSpaceStoreBackend>,
            templates,
            cas,
            base.join("runs"),
        );
        coordinator.attach_audit(audit);
        let coordinator = Arc::new(coordinator);
        // 并发首用：set 失败者取先到者（幂等）。
        let _ = self.inner.coordinator.set(coordinator.clone());
        // R2 重启恢复：启动时全量中断扫描（幂等；只落「interrupted」识别标记并
        // 把磁盘遗留的 Running 步骤转为可恢复状态——绝不在启动时静默重放任何写操作；
        // 恢复必须经用户显式 continue / retry 发起）。
        {
            let boot_coordinator = Arc::clone(&coordinator);
            tokio::spawn(async move {
                match boot_coordinator.detect_interrupted().await {
                    Ok(scan) if !scan.interrupted.is_empty() => tracing::info!(
                        teams = ?scan
                            .interrupted
                            .iter()
                            .map(|r| r.team_id.as_str())
                            .collect::<Vec<_>>(),
                        "workswarm 启动扫描：识别到中断运行（等待显式 continue/retry 恢复）"
                    ),
                    Ok(scan) if !scan.unreadable_states.is_empty() => tracing::warn!(
                        teams = ?scan.unreadable_states,
                        "workswarm 启动扫描：存在损坏的运行状态文件（已原样保留，未被覆盖；需人工修复）"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(%e, "workswarm 启动中断扫描失败"),
                }
            });
        }
        self.inner
            .coordinator
            .get()
            .cloned()
            .ok_or_else(|| WorkSwarmError::Run("协调器初始化失败".to_string()))
    }
}

// ---------------------------------------------------------------------------
// 内层 worker（agent = 模型驱动；echo/sleep/fail = 内置演示/测试）
// ---------------------------------------------------------------------------

/// 真实 Agent 子代理 worker（name="agent"）：prompt → 子代理执行。
///
/// 五期（第三路）：与 `Agent::run_subagent` 同口径（顶层 depth=0、
/// 子代理 max_turns 上限 12），但 Provider 支持 [`workswarm_metrics::MeasuredProvider`]
/// 计数装饰器注入——`model_calls` 由 `MeasuredRoleWorker` 逐 span 精确统计
/// （每次模型调用恰好经过 `complete`/`complete_stream` 其一）。
/// 七期（第二路）：角色画像驱动——工具注册表按 `WorkerProfile` 装配（注册表面即
/// 权限边界）、回合上限取模板预算、写面为「角色 ∩ 绑定」交集白名单。
pub struct AgentSubagentWorker {
    agent: Arc<owo_agent_core::Agent>,
    workspace: PathBuf,
    /// 指标层注入的 per-span 模型调用计数（None = 不计数，行为不变）。
    model_calls: Option<Arc<AtomicU64>>,
    /// 六期（第二路）：项目工作区绑定作用域（None = 全局工作区，行为不变）。
    /// 绑定后：运行目录 = 绑定根；只读绑定强制 read_only；写白名单经审批器强制。
    workspace_scope: Option<project_workspace::WorkspaceScope>,
    /// 七期（第二路）：角色画像（工具面/只读/回合上限；None = 防御分支退回通用执行器）。
    profile: Option<WorkerProfile>,
    /// 七期（第二路）：critic 角色代理（服务端口径：`role == "critic"` 字面量；
    /// 引擎注入的 read_only 只覆盖 critic，其余内置角色都是 producer，画像另管只读面）。
    is_critic: bool,
    /// 七期（第二路）：团队取消桥共享标志（None = 本地标志，行为退化为不可中断）。
    cancel_flag: Option<Arc<AtomicBool>>,
    /// 七期（第二路）：最终写白名单（角色 ∩ 绑定交集；空 = 工作区内可写）。
    write_allowed: Vec<PathBuf>,
}

#[async_trait]
impl Worker for AgentSubagentWorker {
    fn name(&self) -> &str {
        "agent"
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "agent 步骤缺少 prompt 参数".to_string())?;
        let input_read_only = input
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        // 只读三层叠加（七期 · 二路）：步骤输入 → 团队绑定只读（团队级上限）→
        // 角色画像只读（角色级上限）。任一只读即只读。
        let read_only = input_read_only
            || self
                .workspace_scope
                .as_ref()
                .map(|s| s.read_only)
                .unwrap_or(false)
            || self.profile.as_ref().map(|p| p.read_only).unwrap_or(false);
        if std::env::var("OPENAI_API_KEY")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(
                "缺少 OPENAI_API_KEY，agent 角色无法调用模型（请配置凭据后重试）".to_string(),
            );
        }
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                std::env::var("OWO_AGENT_MODEL")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "gpt-4.1-mini".to_string());
        // 指标计数注入：MeasuredProvider 包装共享 provider（计数仅对本 span 生效）。
        let provider: Arc<dyn ModelProvider> = match &self.model_calls {
            Some(counter) => Arc::new(workswarm_metrics::MeasuredProvider::new(
                self.agent.provider(),
                Arc::clone(counter),
            )),
            None => self.agent.provider(),
        };
        // 审批器（七期 · 二路）：绑定作用域 → 白名单审批器，白名单取「角色 ∩ 绑定」
        // 交集（角色白名单空 = 绑定原样）；未绑定保持 AutoApprover 原行为。
        let scope = self.workspace_scope.as_ref();
        let allow_writes = !read_only;
        let workspace_approver;
        let fallback_approver = AutoApprover { allow: read_only };
        let approver: &dyn owo_agent_core::permissions::Approver = match scope {
            Some(s) => {
                let allowed = if self.write_allowed.is_empty() {
                    s.allowed.clone()
                } else {
                    self.write_allowed.clone()
                };
                workspace_approver = project_workspace::WorkspaceScopeApprover {
                    allow_writes,
                    root: s.root.clone(),
                    allowed,
                };
                &workspace_approver
            }
            None => &fallback_approver,
        };
        // 中断标志（七期 · 二路）：优先团队取消桥共享标志（cancel → 即时置位，
        // run_turn 协作式中断）；None = 本地标志（行为退化为不可中断）。
        let local_abort = AtomicBool::new(false);
        let abort: &AtomicBool = match &self.cancel_flag {
            Some(flag) => flag,
            None => &local_abort,
        };
        // 七期（第二路）：画像执行器——注册表面即权限边界 + 模板预算回合上限；
        // 无画像（防御分支，现网构建路径恒有画像）退回通用执行器保持旧行为。
        let output = match &self.profile {
            Some(profile) => {
                let runner = ProfileSubagentRunner {
                    provider,
                    approver,
                    abort,
                    depth: 0,
                    model,
                    is_critic: self.is_critic,
                    write_allowed: self.write_allowed.clone(),
                    profile: profile.clone(),
                };
                runner.run(&self.workspace, prompt).await
            }
            None => {
                let runner = SubagentRunner {
                    provider,
                    approver,
                    abort,
                    depth: 0,
                    max_turns: 12,
                    model,
                };
                runner.run(&self.workspace, prompt, read_only).await
            }
        }
        .map_err(|e| format!("agent 子代理执行失败：{e}"))?;
        Ok(output)
    }
}

struct EchoWorker;

#[async_trait]
impl Worker for EchoWorker {
    fn name(&self) -> &str {
        "echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string()))
    }
}

struct SleepWorker;

#[async_trait]
impl Worker for SleepWorker {
    fn name(&self) -> &str {
        "sleep"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let ms = input
            .get("ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(600_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("slept {ms}ms"))
    }
}

struct FailWorker;

#[async_trait]
impl Worker for FailWorker {
    fn name(&self) -> &str {
        "fail"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Err(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "fail worker 注入失败".to_string()))
    }
}

/// 追踪型 worker 包装（七期 · 二路）：插在内层 worker 与 `RoleWorker` 之间——
/// 1) 写角色先取单写租约（同一工作区同时只允许一个写角色在执行；读角色不参与）；
/// 2) 拿到租约后采集前快照（等待租约的时间不计入变更窗口——否则会把其他写角色
///    正在进行的变更记到本步骤头上）；
/// 3) 执行后采集后快照，登记「变更文件 + diff 摘要 + diff ref」，并对窗口内新增
///    变更做写白名单校验——越界转 `scope_violation` 步骤失败（引擎按失败处理，
///    不登记成功 Artifact）。
///
/// 仅写角色包装追踪（`tracking`/`lease` 均为 None 时纯透传）：读角色没有写工具
/// 不会改文件，而并行读角色的快照窗口会误捕写角色的变更。
///
/// 八期（二路）：执行前对允许路径做内容基线快照（进 CAS）；成功路径自动生成
/// `ChangeSet`（pending_review，等人工 accept/reject/revert；未接受时该团队代码
/// Artifact 不得成为最终 approved head——门控函数见 `change_set_store`）。
struct TrackedRoleWorker {
    inner: Arc<dyn Worker>,
    /// 单写租约（None = 只读角色，不参与租约）。
    lease: Option<Arc<tokio::sync::Mutex<()>>>,
    /// 变更追踪配置（None = 不追踪，纯透传）。
    tracking: Option<workspace_change_tracker::Tracker>,
}

#[async_trait]
impl Worker for TrackedRoleWorker {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let Some(tracking) = &self.tracking else {
            return self.inner.run(input).await;
        };
        // 单写租约：写角色串行化（等待发生在前快照之前——窗口只覆盖本步骤）。
        // 十期·四路 R1：租约覆盖「前快照 → 执行 → 后快照 → 变更登记」全程——
        // 变更登记（record + ChangeSet upsert）完成前不释放：并发写步骤若在
        // 本步骤后快照前插窗口，会把本步骤的变更误算进它的前基线，反之亦然。
        let _lease = match &self.lease {
            Some(lease) => Some(lease.lock().await),
            None => None,
        };
        let pre = workspace_change_tracker::GitSnapshot::snapshot(&tracking.root).await;
        // 八期（二路）：执行前对允许路径做内容基线快照（进 CAS）——ChangeSet
        // reject/revert 的恢复依据；快照尽力而为（超限/读取失败的文件基线记为
        // 「未知」，之后被改只能走 conflicted 人工路径，绝不误删）。
        let base = owo_agent_core::change_set::snapshot_allowed_paths(
            &tracking.root,
            &tracking.allowed,
            &tracking.cas,
        )
        .await;
        // 九期（一路）：执行前已脏文件的内容哈希——合并变更检测的第二基线。
        // 允许路径内的文件直接复用 base.entries（同一份数据，避免重复读盘）；
        // 之外（或基线未覆盖）的执行前脏文件现场读哈希。
        let mut pre_dirty_hashes: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for path in pre.dirty_paths() {
            if let Some(hash) = base.entries.get(&path) {
                pre_dirty_hashes.insert(path, Some(hash.clone()));
            } else {
                let hash = workspace_change_tracker::content_hash(&tracking.root, &path);
                pre_dirty_hashes.insert(path, hash);
            }
        }
        // 执行（成败、超时、取消都以 result 承接——变更收尾对四种结局一视同仁）。
        let result = self.inner.run(input).await;
        // ---- 变更收尾：成功/失败/超时/取消都必须完成 ----
        let post = workspace_change_tracker::GitSnapshot::snapshot(&tracking.root).await;
        // 十期·四路 R1：非 git 目录（含 git 快照不可用）改用内容哈希检测——
        // 执行后对同一组允许路径再采内容快照，与执行前 CAS 基线逐文件比哈希
        // （新增/修改/删除），不依赖 git。
        let (changed, detection) = if pre.git && post.git {
            (
                workspace_change_tracker::merge_changed_files(
                    &pre,
                    &post,
                    &pre_dirty_hashes,
                    &tracking.root,
                ),
                "git 窗口+内容哈希",
            )
        } else {
            let post_base = owo_agent_core::change_set::snapshot_allowed_paths(
                &tracking.root,
                &tracking.allowed,
                &tracking.cas,
            )
            .await;
            (
                owo_agent_core::change_set::merge_content_snapshots(&base, &post_base),
                "内容哈希（非 git）",
            )
        };
        let violation =
            workspace_change_tracker::check_whitelist(&changed, &tracking.root, &tracking.allowed)
                .err();
        let step = input
            .get("_workswarm")
            .and_then(|meta| meta.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let record = match tracking
            .record(&step, &post, &changed, violation.as_deref(), Some(&base))
            .await
        {
            Ok(record) => Some(record),
            Err(error) => {
                tracing::warn!(
                    team_id = %tracking.team_id,
                    role = %tracking.role,
                    %error,
                    "工作区变更记录落盘失败（旁路数据，不阻断步骤）"
                );
                None
            }
        };
        if let Some(violation) = violation {
            // 越界：变更已留痕（record 含 violation 字段）但步骤失败——引擎按失败
            // 处理，不登记成功 Artifact；不生成可审批 ChangeSet（越界写入不可审查
            // 为合法工作区改动，恢复基线只会制造更大的不可控面）。
            return Err(violation);
        }
        // 九期（一路）：无实际变更 → 不创建空 pending ChangeSet（此前会生成
        // changed_files 为空的空壳待办，人工审批无法操作），只记审计留痕。
        if changed.is_empty() {
            if let Some(audit) = &tracking.audit {
                if let Ok(mut audit) = audit.lock() {
                    audit.record(
                        &tracking.team_id,
                        "change_set.no_change",
                        Some(format!("workswarm/{}", tracking.team_id)),
                        Some(true),
                        format!(
                            "步骤 {} 无实际工作区变更（检测：{detection}），跳过 ChangeSet 生成",
                            step
                        ),
                    );
                }
            }
            return result;
        }
        // 十期·四路 R1：无论步骤成败都生成 ChangeSet（pending_review，等人工
        // accept/reject/revert）——「写入后失败」「写入中取消」留下的残留修改
        // 同样可审查、可恢复；未接受时该团队代码 Artifact 不得成为最终 approved
        // head。记录/落盘失败不回滚步骤（变更本身已在工作区/内容快照中），留
        // error 审计与日志。
        let change_set = owo_agent_core::change_set::build_change_set(
            &tracking.team_id,
            &step,
            &tracking.role,
            &base,
            &changed,
            &tracking.root,
            record.as_ref().and_then(|record| record.diff_ref.clone()),
        );
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&tracking.run_dir);
        match store.save_upsert(&change_set) {
            Ok(()) => {
                if let Some(audit) = &tracking.audit {
                    if let Ok(mut audit) = audit.lock() {
                        audit.record(
                            &tracking.team_id,
                            "change_set.created",
                            Some(format!("workswarm/{}", tracking.team_id)),
                            Some(true),
                            format!(
                                "{} 生成（步骤 {}，检测：{detection}，文件：{}）",
                                change_set.change_set_id,
                                step,
                                changed.join(", ")
                            ),
                        );
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    team_id = %tracking.team_id,
                    role = %tracking.role,
                    error = %error,
                    "ChangeSet 落盘失败（变更已在工作区追踪中，但审批闭环缺失）"
                );
            }
        }
        // 变更收尾完成（租约在此作用域末尾释放）。返回执行原结果：成功回成功，
        // 失败/超时/取消回原错误——步骤状态由引擎依结果判定，变更记录独立留存。
        result
    }
}

/// 按角色 worker 名解析内层 worker（"agent"/缺省 = 模型驱动）。
///
/// 五期（第三路）：agent 角色接收 `model_calls` 计数器（MeasuredProvider 注入；
/// 仅指标用途，不影响执行行为）。
/// 六期（第二路）：`scope` 为团队工作区绑定（None = 全局工作区，行为不变）。
/// 七期（第二路）：agent 角色带角色画像（工具面/只读/回合上限）+ critic 代理 +
/// 团队取消桥标志 + 「角色 ∩ 绑定」写白名单交集（内置 echo/sleep/fail 不受影响）。
#[allow(clippy::too_many_arguments)]
fn inner_worker_for(
    state: &AppState,
    worker_name: Option<&str>,
    model_calls: Option<&Arc<AtomicU64>>,
    scope: Option<&project_workspace::WorkspaceScope>,
    profile: &WorkerProfile,
    is_critic: bool,
    cancel_flag: &Arc<AtomicBool>,
    write_allowed: Vec<PathBuf>,
) -> Option<Arc<dyn Worker>> {
    match worker_name.map(str::trim).filter(|w| !w.is_empty()) {
        Some("echo") => Some(Arc::new(EchoWorker)),
        Some("sleep") => Some(Arc::new(SleepWorker)),
        Some("fail") => Some(Arc::new(FailWorker)),
        _ => {
            // 绑定后：Worker 实际运行目录 = 项目绑定目录。
            let workspace = scope
                .map(|s| s.root.clone())
                .unwrap_or_else(|| state.workspace.clone());
            let workspace_scope = scope.cloned();
            Some(Arc::new(AgentSubagentWorker {
                agent: Arc::clone(&state.agent),
                workspace,
                model_calls: model_calls.cloned(),
                workspace_scope,
                profile: Some(profile.clone()),
                is_critic,
                cancel_flag: Some(Arc::clone(cancel_flag)),
                write_allowed,
            }))
        }
    }
}

/// 构建团队运行 worker 注册表（成员名 → MeasuredRoleWorker(RoleWorker(Tracked(inner)))）。
///
/// 五期（第三路）：每个角色 worker 外层包一层 [`workswarm_metrics::MeasuredRoleWorker`]——
/// span 级起止/墙钟/终态/失败原因/尝试序数/输出 Artifact + model_calls/token/费用，
/// 指标 JSONL 落盘 TeamRun 数据目录（`<run_dir>/<team_id>-metrics.jsonl`，重启可读）。
/// 包装在 RoleWorker 之外：span 覆盖「上下文切片组装 → 执行 → 产物登记」全窗口。
/// 失败返回 None（运行任务记录后退出）。
///
/// 七期（第二路）：
/// - 角色画像：模板 `budget_calls_per_role` → 真实 `max_turns`；`WorkerProfile::for_role`
///   决定每个角色实际可见工具面（注册表面即权限边界，不靠审批事后拒绝）；
/// - 单写租约 + 变更追踪：写角色包 `TrackedRoleWorker`（同一工作区同时只允许一个
///   写角色；执行前后 git 快照 → 变更摘要/diff ref 落盘 → 白名单越界 `scope_violation`）；
/// - 取消桥：`cancel_flag` 由 run_team_loop 的令牌监听任务置位，Worker 协作中断。
async fn build_run_registry(
    coordinator: &Arc<TeamCoordinator>,
    state: &AppState,
    team_id: &str,
    cancel_flag: &Arc<AtomicBool>,
) -> Option<WorkerRegistry> {
    let meta = coordinator.load_run_meta(team_id).ok()?;
    // 六期（第二路）：团队工作区绑定（cancel/retry/resume 后循环按迭代重读——
    // 绑定生命周期独立于运行状态，恢复后继续生效）。
    let scope = project_workspace::load_binding(coordinator.run_dir(), team_id).map(|b| b.scope());
    // 七期（第二路）：模板角色预算 → 真实 max_turns（无模板 / 未知角色 → 缺省 12）。
    let budgets: Vec<owo_agent_core::builtin_team_templates::RoleBudget> = coordinator
        .get_team_run(team_id)
        .await
        .ok()
        .and_then(|run| {
            run.template_id
                .as_deref()
                .and_then(owo_agent_core::builtin_team_templates::descriptor)
        })
        .map(|descriptor| descriptor.budget_calls_per_role)
        .unwrap_or_default();
    let journal = workswarm_metrics::MetricsJournal::for_team(coordinator.run_dir(), team_id);
    // 单写租约（每次重建注册表新发一份：同一轮注册表内的写角色互斥）。
    let write_lease = Arc::new(tokio::sync::Mutex::new(()));
    let registry = WorkerRegistry::new();
    for r in &meta.roles {
        let member_id = format!("m-{}", r.role);
        let worker_kind = r
            .worker
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .unwrap_or("agent")
            .to_string();
        let model_calls = (worker_kind == "agent").then(|| Arc::new(AtomicU64::new(0)));
        // 七期（第二路）：角色画像（模板预算 → 回合上限；工具面/只读按角色族）。
        let budget_calls = budgets
            .iter()
            .find(|budget| budget.role == r.role)
            .map(|budget| budget.budget_calls)
            .unwrap_or(0);
        let profile = WorkerProfile::for_role(&r.role, budget_calls);
        let is_critic = r.role == "critic";
        let is_writer = profile.is_writer();
        // 最终写面 = 角色白名单 ∩ 团队绑定白名单（角色白名单空 = 交由绑定决定；
        // 两侧都空 = 工作区内可写，仍受审批约束）。
        let tracking_root = scope
            .as_ref()
            .map(|s| s.root.clone())
            .unwrap_or_else(|| state.workspace.clone());
        let scope_allowed: Vec<PathBuf> = scope
            .as_ref()
            .map(|s| s.allowed.clone())
            .unwrap_or_default();
        let profile_allowed: Vec<PathBuf> = profile
            .write_allowed_paths
            .iter()
            .map(|relative| tracking_root.join(relative))
            .collect();
        let write_allowed = intersect_paths(&profile_allowed, &scope_allowed);
        let inner = inner_worker_for(
            state,
            r.worker.as_deref(),
            model_calls.as_ref(),
            scope.as_ref(),
            &profile,
            is_critic,
            cancel_flag,
            write_allowed.clone(),
        )?;
        // 追踪 + 租约只作用于写角色（读角色没有写工具不会改文件；并行读角色的
        // 快照窗口会误捕写角色的变更）。
        let tracking = is_writer.then(|| workspace_change_tracker::Tracker {
            root: tracking_root,
            run_dir: coordinator.run_dir().to_path_buf(),
            team_id: team_id.to_string(),
            role: r.role.clone(),
            allowed: write_allowed.clone(),
            // 八期（二路）：ChangeSet 基线快照进团队 CAS + 生成留痕审计。
            cas: coordinator.cas().clone(),
            audit: coordinator.audit_log(),
        });
        let inner: Arc<dyn Worker> = Arc::new(TrackedRoleWorker {
            inner,
            lease: is_writer.then(|| Arc::clone(&write_lease)),
            tracking,
        });
        let role_worker = Arc::new(RoleWorker::new(
            Arc::clone(coordinator),
            team_id.to_string(),
            member_id.clone(),
            r.role.clone(),
            inner,
        ));
        let provider = (worker_kind == "agent").then(|| state.agent.provider());
        registry.register(Arc::new(workswarm_metrics::MeasuredRoleWorker::new(
            role_worker,
            Arc::clone(coordinator),
            journal.clone(),
            team_id.to_string(),
            member_id,
            r.role.clone(),
            worker_kind,
            provider,
            model_calls,
        )));
    }
    Some(registry)
}

// ---------------------------------------------------------------------------
// 后台运行循环（阶段驱动 + 人节点门闩）
// ---------------------------------------------------------------------------

/// 运行循环存活守卫（R2）：进程内声明「该团队有活动运行循环」——
/// 中断识别据此区分「循环存活（含批次间隙/人节点门闩）」与「重启遗留」。
/// Drop 兜底撤销（覆盖所有 return 路径与 panic 展开栈）。
struct LoopAliveGuard {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
}

impl LoopAliveGuard {
    fn new(coordinator: &Arc<TeamCoordinator>, team_id: &str) -> Self {
        let guard = Self {
            coordinator: Arc::clone(coordinator),
            team_id: team_id.to_string(),
        };
        guard.coordinator.set_loop_alive(team_id, true);
        guard
    }
}

impl Drop for LoopAliveGuard {
    fn drop(&mut self) {
        self.coordinator.set_loop_alive(&self.team_id, false);
    }
}

/// 五期（第三路）：指标预算门——累计指标超过任务预算（TeamRun.budget additive
/// `max_cost_usd` / `max_wall_secs`；此前未知键被忽略，与 GoalBudget 步数/重试
/// 熔断正交互补）即停止调度下一阶段：审计 `team.budget_exhausted`（含明确原因），
/// 团队显式转 Cancelled（不静默挂起，也不伪装成用户取消——原因可在
/// `/teams/{id}/metrics` 的 `budget.reason` 与审计尾迹复查）。
/// 检查失败按「继续调度」处理（可用性优先；run_phase 自身会显式失败）。
/// 返回 true 表示已停止（调用方应立即退出运行循环）。
async fn stop_if_budget_exhausted(coordinator: &Arc<TeamCoordinator>, team_id: &str) -> bool {
    match workswarm_metrics::team_budget_exhaustion(coordinator, team_id).await {
        Ok(Some(reason)) => {
            tracing::warn!(team_id = %team_id, %reason, "workswarm 指标超预算，停止调度下一阶段");
            if let Some(log) = coordinator.audit_log() {
                if let Ok(mut audit) = log.lock() {
                    audit.record(
                        team_id,
                        "team.budget_exhausted",
                        Some(format!("workswarm/{team_id}")),
                        Some(false),
                        reason,
                    );
                }
            }
            if let Err(e) = coordinator
                .apply_steer(team_id, &SteerCommand::Cancel)
                .await
            {
                tracing::error!(team_id = %team_id, %e, "预算停止：团队取消收尾失败");
            }
            true
        }
        Ok(None) => false,
        Err(e) => {
            tracing::warn!(team_id = %team_id, %e, "指标预算门检查失败（忽略并继续调度）");
            false
        }
    }
}

/// 团队运行循环：run_phase 阶段推进；人节点等待窗口内轮询（结果落盘即唤醒；cancel 即终止）。
///
/// 每轮外层迭代重建 worker 注册表（steer/replace 修改角色规格后新阶段生效）。
pub(crate) async fn run_team_loop(
    state: Arc<AppState>,
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
) {
    let _alive = LoopAliveGuard::new(&coordinator, &team_id);
    // 七期（第二路）：团队取消令牌 → 运行中 Worker 的即时中断桥。`wait_cancel`
    // 只在真实取消（令牌值变 true）时置位共享 abort 标志（Worker 在回合边界协作
    // 中断）；发送端随团队收尾关闭返回 false，不置位（运行已结束，无需中断）。
    // 桥存活期 = 团队运行循环存活期（令牌由协调器持有，跨注册表重建共用一份标志）。
    let cancel_flag = Arc::new(AtomicBool::new(false));
    {
        let token = coordinator.cancel_token(&team_id);
        let flag = Arc::clone(&cancel_flag);
        tokio::spawn(async move {
            if owo_agent_core::wait_cancel(&token).await {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                tracing::info!("workswarm 取消桥：运行中 Worker 中断标志已置位");
            }
        });
    }
    let mut backoff = Duration::from_secs(1);
    loop {
        // 五期（第三路）：指标预算门（外层调度点；门闩内调度点见下方 latch 循环）。
        if stop_if_budget_exhausted(&coordinator, &team_id).await {
            return;
        }
        let Some(registry) = build_run_registry(&coordinator, &state, &team_id, &cancel_flag).await
        else {
            tracing::error!(team_id = %team_id, "workswarm 运行循环：worker 注册表构建失败，运行终止");
            return;
        };
        match coordinator.run_phase(&team_id, &registry).await {
            Err(e) => {
                // 存储/IO 异常：退避重试，避免热循环打爆磁盘。
                tracing::warn!(team_id = %team_id, %e, "run_phase 失败，退避重试");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Ok(outcome) => match outcome {
                owo_agent_core::PhaseOutcome::MoreReady => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
                owo_agent_core::PhaseOutcome::Done => {
                    if let Err(e) = coordinator.finalize_success(&team_id).await {
                        tracing::error!(team_id = %team_id, %e, "workswarm 收尾失败");
                    }
                    return;
                }
                owo_agent_core::PhaseOutcome::Failed
                | owo_agent_core::PhaseOutcome::Aborted
                | owo_agent_core::PhaseOutcome::Finished => return,
                owo_agent_core::PhaseOutcome::AwaitingHuman { waits } => {
                    tracing::info!(team_id = %team_id, ?waits, "团队等待人节点，进入门闩等待");
                    // 门闩：人结果录入（落盘）→ 下一次 run_phase 自动唤醒；
                    // cancel → 立即取消收尾。200ms 轮询（S0 无推送通道，保持最小实现）。
                    let cancel = coordinator.cancel_token(&team_id);
                    loop {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        if cancel.is_cancelled() {
                            if let Err(e) = coordinator
                                .apply_steer(&team_id, &SteerCommand::Cancel)
                                .await
                            {
                                tracing::error!(team_id = %team_id, %e, "取消收尾失败");
                            }
                            return;
                        }
                        // 五期（第三路）：门闩内调度点同样过指标预算门（人结果落盘
                        // 唤醒的下一阶段在此受控，否则会绕过外层门直接执行）。
                        if stop_if_budget_exhausted(&coordinator, &team_id).await {
                            return;
                        }
                        match coordinator.run_phase(&team_id, &registry).await {
                            Ok(owo_agent_core::PhaseOutcome::AwaitingHuman { .. }) => continue,
                            Ok(owo_agent_core::PhaseOutcome::Done) => {
                                let _ = coordinator.finalize_success(&team_id).await;
                                return;
                            }
                            Ok(owo_agent_core::PhaseOutcome::MoreReady) => break, // 外层循环重建注册表继续
                            Ok(_) => return,                                      // 终态
                            Err(e) => {
                                tracing::warn!(team_id = %team_id, %e, "门闩等待中 run_phase 失败，退避");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 请求模型
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateTeamHttpRequest {
    goal_id: Option<String>,
    objective: String,
    mode: Option<String>,
    template_id: Option<String>,
    #[serde(default)]
    roles: Vec<RoleSpec>,
    #[serde(default)]
    budget: Value,
    human_policy: Option<String>,
    /// 五期：组队策略 auto|single|team（缺省 auto；未知值 → 400）。
    #[serde(default)]
    strategy: Option<String>,
    /// 六期（第二路）：可选项目工作区绑定（root 必须已存在；缺省只读）。
    #[serde(default)]
    workspace: Option<project_workspace::WorkspaceSpec>,
}

#[derive(Debug, Deserialize)]
struct SteerHttpRequest {
    /// continue | steer | replace | cancel | retry
    command: String,
    #[serde(default)]
    step_id: Option<String>,
    #[serde(default)]
    new_input: Option<Value>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    new_worker: Option<String>,
    #[serde(default)]
    new_user_id: Option<String>,
}

impl SteerHttpRequest {
    fn into_command(self) -> Result<SteerCommand, String> {
        match self.command.as_str() {
            "continue" => Ok(SteerCommand::Continue),
            "cancel" => Ok(SteerCommand::Cancel),
            "steer" => Ok(SteerCommand::Steer {
                step_id: self.step_id,
                new_input: self.new_input,
                note: self.note,
            }),
            // R2 冻结契约：POST /teams/{id}/steer {"command":"retry","step_id":"builder","note":"…"}
            // 缺少/空 step_id → 400（Validation）。
            "retry" => {
                let step_id = self
                    .step_id
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| "retry 需要 step_id 字段".to_string())?;
                Ok(SteerCommand::Retry {
                    step_id,
                    note: self.note,
                })
            }
            "replace" => {
                let role = self
                    .role
                    .filter(|r| !r.trim().is_empty())
                    .ok_or_else(|| "replace 需要 role 字段".to_string())?;
                Ok(SteerCommand::Replace {
                    role,
                    new_worker: self.new_worker,
                    new_user_id: self.new_user_id,
                    note: self.note,
                })
            }
            other => Err(format!(
                "未知 steer 指令：{other}（支持 continue/retry/steer/replace/cancel）"
            )),
        }
    }
}

#[derive(Debug, Deserialize)]
struct HandoffHttpRequest {
    team_id: String,
    from_member: String,
    #[serde(default)]
    to_member: Option<String>,
    #[serde(default)]
    completed_summary: String,
    #[serde(default)]
    open_issues: Vec<String>,
    #[serde(default)]
    output_artifact_refs: Vec<String>,
    #[serde(default)]
    evidence_refs: Vec<String>,
    #[serde(default)]
    suggested_next_actions: Vec<String>,
    #[serde(default)]
    known_risks: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HumanResultHttpRequest {
    team_id: String,
    result: String,
}

// ---------------------------------------------------------------------------
// 响应辅助
// ---------------------------------------------------------------------------

fn error_response(e: &WorkSwarmError) -> (StatusCode, Json<Value>) {
    let (code, msg) = match e {
        WorkSwarmError::Validation(m) => (StatusCode::BAD_REQUEST, m.clone()),
        WorkSwarmError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
        WorkSwarmError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
        // R2：状态文件损坏 → 明确 500 失败；原文件已保留、未被覆盖（消息中注明），
        // 不返回可重试语义——需人工修复后才能继续。
        WorkSwarmError::CorruptState(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    };
    (code, Json(json!({ "error": msg })))
}

fn task_view(state: &owo_agent_core::goal::GoalRunState) -> Vec<Value> {
    state
        .plan
        .steps
        .iter()
        .map(|s| {
            let rec = state.records.get(&s.id);
            json!({
                "task_id": s.id,
                "worker": s.worker,
                "role": s.worker.strip_prefix("m-").unwrap_or(&s.worker),
                "depends_on": s.depends_on,
                "status": format!("{:?}", rec.map(|r| r.status).unwrap_or(
                    owo_agent_core::plan::StepStatus::Pending
                )),
                "attempts": rec.map(|r| r.attempts).unwrap_or(0),
                "error": rec.and_then(|r| r.error.clone()),
            })
        })
        .collect()
}

/// 团队审计尾迹（最近 20 条；S0 可见性：关键动作可回看）。
fn audit_tail(coordinator: &Arc<TeamCoordinator>, team_id: &str) -> Vec<Value> {
    let Some(log) = coordinator.audit_log() else {
        return Vec::new();
    };
    let Ok(entries) = log.lock() else {
        return Vec::new();
    };
    entries
        .entries
        .iter()
        .filter(|e| e.session_id == team_id)
        .rev()
        .take(20)
        .map(|e| json!({ "ts": e.ts, "event": e.event, "tool": e.tool, "detail": e.detail }))
        .collect()
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/teams", post(create_team).get(list_teams))
        .route("/teams/{id}", get(get_team))
        .route("/teams/{id}/tasks", get(get_team_tasks))
        .route("/teams/{id}/events", get(team_events))
        .route("/teams/{id}/metrics", get(team_metrics))
        .route("/teams/{id}/diagnostic", get(team_diagnostic))
        .route("/teams/{id}/steer", post(steer_team))
        // 八期（第二路）：ChangeSet 审批、接受与安全撤销。
        .route(
            "/teams/{id}/change-sets",
            get(change_set_api::list_team_change_sets),
        )
        .route("/change-sets/{id}", get(change_set_api::get_change_set))
        .route(
            "/change-sets/{id}/accept",
            post(change_set_api::accept_change_set),
        )
        .route(
            "/change-sets/{id}/reject",
            post(change_set_api::reject_change_set),
        )
        .route(
            "/change-sets/{id}/revert",
            post(change_set_api::revert_change_set),
        )
        .route("/projects/{id}", get(get_project_space))
        .route("/projects/{id}/artifacts", get(list_artifacts))
        // 六期（第二路）：项目工作区绑定（真实目录 / 只读 / 写白名单 / 树 / git 状态）。
        .route(
            "/projects/{id}/workspace",
            get(project_workspace::get_workspace).put(project_workspace::put_workspace),
        )
        .route(
            "/projects/{id}/workspace/tree",
            get(project_workspace::get_workspace_tree),
        )
        .route(
            "/projects/{id}/workspace/git-status",
            get(project_workspace::get_workspace_git_status),
        )
        // 七期（第二路）：Worker 代码变更追踪（变更文件 + diff 摘要 + 逐步骤记录）。
        .route(
            "/projects/{id}/workspace/changes",
            get(project_workspace::get_workspace_changes),
        )
        .route("/tasks/{id}/handoff", post(submit_handoff))
        .route("/tasks/{id}/human-result", post(submit_human_result))
        .route("/teams/templates", get(list_templates))
        .route("/teams/templates/proposals", get(list_proposals))
        .route(
            "/teams/templates/proposals/{proposal_id}/adopt",
            post(adopt_proposal),
        )
        .route(
            "/teams/templates/proposals/{proposal_id}/reject",
            post(reject_proposal),
        )
        .with_state(state)
}

// ---------------- handlers ----------------

/// POST /teams：创建运行（202）+ 后台运行循环。
async fn create_team(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateTeamHttpRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let mode = match req.mode.as_deref() {
        Some("single") => TeamMode::Single,
        Some("team") | None => TeamMode::Team,
        Some("swarmflow") => TeamMode::Swarmflow,
        Some(other) => {
            return Err(error_response(&WorkSwarmError::Validation(format!(
                "未知 mode：{other}（支持 single/team/swarmflow）"
            ))));
        }
    };
    let strategy = match req.strategy.as_deref() {
        None | Some("") => None,
        Some(raw) => match owo_agent_core::team_strategy::TeamSelectionMode::parse(raw) {
            Ok(s) => Some(s),
            Err(msg) => {
                return Err(error_response(&WorkSwarmError::Validation(msg)));
            }
        },
    };
    // 六期（第二路）：工作区绑定先校验（不依赖团队存在；路径非法 → 400，不建队）。
    let workspace_binding = match &req.workspace {
        Some(spec) => Some(
            project_workspace::validate_workspace_spec(spec)
                .map_err(|msg| (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))))?,
        ),
        None => None,
    };
    let req = CreateTeamRequest {
        goal_id: req.goal_id,
        objective: req.objective,
        mode,
        template_id: req.template_id,
        roles: req.roles,
        budget: req.budget,
        human_policy: req.human_policy,
        strategy,
    };
    let team = coordinator
        .create_team_run(&req)
        .await
        .map_err(|e| error_response(&e))?;
    let team_id = team.team_id.clone();
    // 绑定在运行循环启动前落盘（首迭代 build_run_registry 即生效）。
    let mut bound_workspace = false;
    if let Some(mut binding) = workspace_binding {
        binding.team_id = team_id.clone();
        binding.project_id = team.project_space_id.clone().unwrap_or_default();
        project_workspace::save_binding(coordinator.run_dir(), &binding).map_err(|msg| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": msg })),
            )
        })?;
        bound_workspace = true;
    }
    // 后台运行循环（单写者：阶段边界串行化；人节点等待 = 门闩）。
    tokio::spawn(run_team_loop(state, coordinator, team_id.clone()));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "team_id": team_id,
            "project_space_id": team.project_space_id,
            "mode": format!("{:?}", team.mode),
            "template_id": team.template_id,
            "members": team.members,
            "status": format!("{:?}", team.status),
            "strategy_decision": team.strategy_decision,
            "workspace_bound": bound_workspace,
        })),
    ))
}

/// GET /teams：团队运行列表。
///
/// `active` = 运行循环正在执行阶段（人节点等待窗口 / 终态为 false = 暂停）。
async fn list_teams(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let runs = coordinator
        .list_team_runs()
        .await
        .map_err(|e| error_response(&e))?;
    let mut items = Vec::with_capacity(runs.len());
    for team in runs {
        let mut item = serde_json::to_value(&team).unwrap_or_else(|_| json!({}));
        if let Some(obj) = item.as_object_mut() {
            obj.insert(
                "active".to_string(),
                json!(coordinator.is_run_active(&team.team_id)),
            );
            // R2：中断标记（磁盘 Running 但无活动运行，等待显式恢复）。
            obj.insert(
                "interrupted".to_string(),
                json!(coordinator.is_interrupted(&team.team_id)),
            );
        }
        items.push(item);
    }
    Ok(Json(json!({ "teams": items })))
}

/// GET /teams/{id}：成员、预算、状态 + 任务视图 + 审计尾迹。
///
/// R2：响应含 `interrupted` 标记（磁盘 Running 但无活动运行 → 已被识别为中断、
/// 等待显式 continue/retry 恢复）。损坏的状态文件在此明确 500（原文件保留）。
async fn get_team(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    // 请求时先做一次单团队中断识别（幂等；CorruptState 在下方 load_run_state 明确报错）。
    // 响应性（R3）：运行中/循环存活的团队跳过识别——该路径取 team 锁，
    // 长 Worker 阶段绝不等待（运行中的团队按定义不可能是中断残留）。
    if !coordinator.is_run_active(&id) && !coordinator.is_loop_alive(&id) {
        let _ = coordinator.detect_interrupted_for(&id).await;
    }
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let run_state = coordinator
        .load_run_state(&id)
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({
        "team": team,
        "interrupted": coordinator.is_interrupted(&id),
        "tasks": task_view(&run_state),
        "audit_tail": audit_tail(&coordinator, &id),
    })))
}

/// GET /teams/{id}/tasks：任务图（步骤 × 状态）。
async fn get_team_tasks(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let run_state = coordinator
        .load_run_state(&id)
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({
        "team_id": team.team_id,
        "tasks": task_view(&run_state),
    })))
}

/// `GET /teams/{id}/events` 查询参数（`?format=json` 一次性快照，供轮询/契约测试）。
#[derive(Debug, Clone, Default, Deserialize)]
struct TeamEventsQuery {
    #[serde(default)]
    format: Option<String>,
}

/// GET /teams/{id}/events：团队事件流（SSE：审计重放 + 状态轮询，终态后结束）。
///
/// 团队不存在 → 404（先于开流，保持与 `/teams/{id}` 一致的语义）。
async fn team_events(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<TeamEventsQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    if query.format.as_deref() == Some("json") {
        // 五期（第三路）：快照补 `progress`（seq/counts/current_steps）——轮询降级
        // 不再完全依赖 SSE 帧也能看到当前步骤（快照不可得时为 null，additive 字段）。
        let progress = coordinator
            .progress_snapshot(&id)
            .await
            .ok()
            .and_then(|p| serde_json::to_value(&p).ok());
        return Ok(Json(json!({
            "team_id": team.team_id,
            "status": format!("{:?}", team.status),
            "active": coordinator.is_run_active(&id),
            "interrupted": coordinator.is_interrupted(&id),
            "progress": progress,
            "audit": audit_tail(&coordinator, &id),
        }))
        .into_response());
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Event, Infallible>>();
    let stream_coordinator = Arc::clone(&coordinator);
    let team_id = team.team_id.clone();
    tokio::spawn(async move {
        team_event_stream(stream_coordinator, team_id, tx).await;
    });
    Ok(Sse::new(UnboundedReceiverStream::new(rx)).into_response())
}

/// SSE 流任务：先重放审计尾迹（最近 50 条，旧→新），再 500ms 轮询新增审计 + 状态变化；
/// 团队进入终态（并补发终帧）后结束。客户端断开（发送失败）即退出。
async fn team_event_stream(
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<Result<Event, Infallible>>,
) {
    let emit = |frame: Value| -> bool {
        tx.send(Ok(Event::default().data(frame.to_string())))
            .is_ok()
    };
    if !emit(json!({ "type": "open", "team_id": team_id })) {
        return;
    }

    // 进度帧（R3）：订阅即发当前快照（客户端立即拿到 current_steps/counts/seq），
    // 此后仅当代次变化才发（seq 单调递增；客户端以 seq 去重/断线续传）。
    let mut last_progress_seq: Option<u64> = None;
    if let Ok(progress) = coordinator.progress_snapshot(&team_id).await {
        last_progress_seq = Some(progress.seq);
        if !emit(json!({ "type": "progress", "progress": progress })) {
            return;
        }
    }

    // 该团队当前的审计条目（session_id = team_id）。
    let team_entries = || -> Vec<owo_agent_core::audit::AuditEntry> {
        let Some(log) = coordinator.audit_log() else {
            return Vec::new();
        };
        let Ok(guard) = log.lock() else {
            return Vec::new();
        };
        guard
            .entries
            .iter()
            .filter(|e| e.session_id == team_id)
            .cloned()
            .collect()
    };

    // 历史重放（最近 50 条）。
    let replay = team_entries();
    let mut seen = replay.len();
    for entry in replay.iter().rev().take(50).rev() {
        if !emit(json!({
            "type": "audit",
            "ts": entry.ts,
            "event": entry.event,
            "detail": entry.detail,
        })) {
            return;
        }
    }

    let mut last_status: Option<TeamRunStatus> = None;
    loop {
        let Some(team) = coordinator.get_team_run(&team_id).await.ok() else {
            return;
        };
        // 新增审计条目（重放点之后）。
        let entries = team_entries();
        for entry in entries.iter().skip(seen) {
            if !emit(json!({
                "type": "audit",
                "ts": entry.ts,
                "event": entry.event,
                "detail": entry.detail,
            })) {
                return;
            }
        }
        seen = entries.len();
        // 状态帧（变化即发；首次必发）。
        if last_status != Some(team.status) {
            last_status = Some(team.status);
            if !emit(json!({
                "type": "state",
                "status": format!("{:?}", team.status),
                "active": coordinator.is_run_active(&team_id),
                "interrupted": coordinator.is_interrupted(&team_id),
            })) {
                return;
            }
        }
        // 进度帧（seq 变化即发；步骤开始/完成/失败/取消都会推进 coordinator 侧序号）。
        if let Ok(progress) = coordinator.progress_snapshot(&team_id).await {
            if last_progress_seq != Some(progress.seq) {
                last_progress_seq = Some(progress.seq);
                if !emit(json!({ "type": "progress", "progress": progress })) {
                    return;
                }
            }
        }
        if team.status.is_terminal() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// GET /teams/{id}/metrics：TeamRun 指标汇总（五期 · 第三路）。
///
/// 数据源 = TeamRun 数据目录的 `metrics.jsonl`（`MeasuredRoleWorker` 追加落盘），
/// 每次请求从文件聚合——无进程内账本，重启后仍可读取。响应：
/// `summary`（span 数/成败/返工/总墙钟/调用·token·费用/最慢 Worker/Artifact 版本数）+
/// `roles`（按角色聚合）+ `workers`（span 明细）+ `budget`（预算状态与耗尽原因）。
async fn team_metrics(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    // 团队不存在 → 404（与 /teams/{id} 语义一致）。
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let journal = workswarm_metrics::MetricsJournal::for_team(coordinator.run_dir(), &id);
    let records = journal.read_records();
    let mut payload = workswarm_metrics::aggregate_metrics(&id, &records, &team.budget);
    // 数据源路径（可观测：UI/运维可直接定位 TeamRun 数据目录里的指标文件）。
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "metrics_file".to_string(),
            json!(journal.path().display().to_string()),
        );
    }
    Ok(Json(payload))
}

/// GET /teams/{id}/diagnostic：脱敏诊断导出（五期 · 第三路）。
///
/// 汇集 TeamRun / 任务视图 / 产物（含 CAS 内容预览）/ 评审记录 / 交接 / 指标 /
/// 审计尾迹，统一经脱敏（凭据类键值与令牌 → `[REDACTED]`、超长文本截断），
/// 供「下载诊断信息」。评审记录复用 WorkSwarm `space.db` 独立连接（打开失败
/// 不阻塞其余诊断面，reviews 缺席即其信号）。
async fn team_diagnostic(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let team = coordinator
        .get_team_run(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let run_state = coordinator
        .load_run_state(&id)
        .map_err(|e| error_response(&e))?;

    // 任务视图（错误信息等自由文本脱敏）。
    let tasks: Vec<Value> = task_view(&run_state)
        .iter()
        .map(workswarm_metrics::sanitize_value)
        .collect();

    // 产物 + 评审记录（评审存储打开失败 → 跳过，不阻塞导出）。
    let review_store =
        SqliteProjectSpaceStore::open(&state.data_root.join("workswarm").join("space.db")).ok();
    let mut artifacts_json: Vec<Value> = Vec::new();
    let mut reviews_json: Vec<Value> = Vec::new();
    if let Some(space_id) = team.project_space_id.as_deref() {
        if let Ok(space) = coordinator.get_project_space(space_id).await {
            if let Ok(artifacts) = coordinator.list_artifacts(&space).await {
                for a in artifacts {
                    let preview = coordinator
                        .cas()
                        .get_text(a.content_ref.strip_prefix("cas://sha256:").unwrap_or(""))
                        .map(|c| {
                            let head: String = c.chars().take(200).collect();
                            workswarm_metrics::sanitize_text(&head)
                        })
                        .unwrap_or_default();
                    artifacts_json.push(json!({
                        "artifact_id": a.artifact_id,
                        "kind": a.kind,
                        "version": a.version,
                        "producer": a.producer,
                        "content_ref": a.content_ref,
                        "review_state": format!("{:?}", a.review_state),
                        "supersedes_artifact_id": a.supersedes_artifact_id,
                        "created_at": a.created_at,
                        "preview": preview,
                    }));
                    if let Some(store) = review_store.as_ref() {
                        if let Ok(records) = store.list_artifact_reviews(&a.artifact_id).await {
                            for r in records {
                                let value = serde_json::to_value(&r).unwrap_or_else(|_| json!({}));
                                reviews_json.push(workswarm_metrics::sanitize_value(&value));
                            }
                        }
                    }
                }
            }
        }
    }

    // 交接记录（completed_summary 等自由文本脱敏）。
    let handoffs_json: Vec<Value> = coordinator
        .list_handoffs(&id)
        .await
        .unwrap_or_default()
        .iter()
        .map(|h| workswarm_metrics::sanitize_value(&serde_json::to_value(h).unwrap_or(json!({}))))
        .collect();

    // 审计尾迹（最近 200 条，detail 脱敏）。
    let audit_tail_json: Vec<Value> = match coordinator.audit_log() {
        Some(log) => match log.lock() {
            Ok(entries) => entries
                .entries
                .iter()
                .filter(|e| e.session_id == id)
                .rev()
                .take(200)
                .map(|e| {
                    json!({
                        "ts": e.ts,
                        "event": e.event,
                        "tool": e.tool,
                        "detail": workswarm_metrics::sanitize_text(&e.detail),
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };

    // 指标（与 /teams/{id}/metrics 同一聚合口径）。
    let journal = workswarm_metrics::MetricsJournal::for_team(coordinator.run_dir(), &id);
    let metrics = workswarm_metrics::aggregate_metrics(&id, &journal.read_records(), &team.budget);
    let metrics_file = journal.path().display().to_string();

    // TeamRun 本体（budget 等自由 JSON 脱敏）+ 运行标志。
    let mut team_value = serde_json::to_value(&team).unwrap_or_else(|_| json!({}));
    team_value = workswarm_metrics::sanitize_value(&team_value);
    if let Some(obj) = team_value.as_object_mut() {
        obj.insert("active".to_string(), json!(coordinator.is_run_active(&id)));
        obj.insert(
            "interrupted".to_string(),
            json!(coordinator.is_interrupted(&id)),
        );
    }

    Ok(Json(json!({
        "team_id": team.team_id,
        "generated_at": workswarm_metrics::rfc3339(),
        "team": team_value,
        "tasks": tasks,
        "artifacts": artifacts_json,
        "reviews": reviews_json,
        "handoffs": handoffs_json,
        "metrics": metrics,
        "metrics_file": metrics_file,
        "audit_tail": audit_tail_json,
        "redaction": {
            "applied": true,
            "note": "凭据类键值与令牌已替换为 [REDACTED]；超长文本截断（*_tokens 为用量计数，不属凭据）"
        },
    })))
}

/// POST /teams/{id}/steer：continue/retry/steer/replace/cancel。
async fn steer_team(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<SteerHttpRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let cmd = req
        .into_command()
        .map_err(|m| (StatusCode::BAD_REQUEST, Json(json!({ "error": m }))))?;
    // continue / retry 成功后都需要重新启动团队运行循环（R2：retry 恢复同样重启循环；
    // 运行循环每轮自行重建 worker 注册表，构建失败会在循环内记录并退出）。
    let restarts_loop = matches!(cmd, SteerCommand::Continue | SteerCommand::Retry { .. });
    let team = coordinator
        .apply_steer(&id, &cmd)
        .await
        .map_err(|e| error_response(&e))?;
    if restarts_loop && !coordinator.is_run_active(&id) {
        let state2 = state.clone();
        let coordinator2 = Arc::clone(&coordinator);
        tokio::spawn(run_team_loop(state2, coordinator2, id.clone()));
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "team_id": team.team_id,
            "status": format!("{:?}", team.status),
            "interrupted": coordinator.is_interrupted(&team.team_id),
        })),
    ))
}

/// GET /projects/{id}：Project Space 摘要。
async fn get_project_space(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let space = coordinator
        .get_project_space(&id)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({ "project": space })))
}

/// GET /projects/{id}/artifacts：版本化共享产物（含 CAS 内容解析）。
async fn list_artifacts(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    // 空间不存在 → 404（区分"空项目"与"不存在"）。
    let space = coordinator
        .get_project_space(&id)
        .await
        .map_err(|e| error_response(&e))?;
    let artifacts = coordinator
        .list_artifacts(&space)
        .await
        .map_err(|e| error_response(&e))?;
    let mut items = Vec::with_capacity(artifacts.len());
    for a in artifacts {
        let content_preview = coordinator
            .cas()
            .get_text(a.content_ref.strip_prefix("cas://sha256:").unwrap_or(""))
            .map(|c| c.chars().take(200).collect::<String>())
            .unwrap_or_default();
        items.push(json!({
            "artifact_id": a.artifact_id,
            "kind": a.kind,
            "version": a.version,
            "producer": a.producer,
            "content_ref": a.content_ref,
            "source_refs": a.source_refs,
            "review_state": format!("{:?}", a.review_state),
            // 五期：版本链链接（返工重跑登记时指向前版）——前端版本时间线/
            // v1v2 差异按此字段合并链（null = 首版/无链接）。
            "supersedes_artifact_id": a.supersedes_artifact_id,
            "created_at": a.created_at,
            "preview": content_preview,
        }));
    }
    Ok(Json(json!({ "project_id": id, "artifacts": items })))
}

/// POST /tasks/{id}/handoff：结构化接力（team_id 在请求体；任务 id 按团队命名空间解析）。
async fn submit_handoff(
    State(state): State<Arc<AppState>>,
    AxumPath(task_id): AxumPath<String>,
    Json(req): Json<HandoffHttpRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let fields = HandoffFields {
        to_member: req.to_member,
        completed_summary: req.completed_summary,
        open_issues: req.open_issues,
        output_artifact_refs: req.output_artifact_refs,
        evidence_refs: req.evidence_refs,
        suggested_next_actions: req.suggested_next_actions,
        known_risks: req.known_risks,
    };
    let handoff = coordinator
        .submit_handoff(&req.team_id, &task_id, &req.from_member, &fields)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({ "handoff": handoff })))
}

/// POST /tasks/{id}/human-result：人节点提交结果（落盘后运行循环自动唤醒下游）。
async fn submit_human_result(
    State(state): State<Arc<AppState>>,
    AxumPath(task_id): AxumPath<String>,
    Json(req): Json<HumanResultHttpRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let artifact = coordinator
        .record_human_result(&req.team_id, &task_id, &req.result)
        .await
        .map_err(|e| error_response(&e))?;
    Ok(Json(json!({ "artifact": artifact })))
}

/// GET /teams/templates：已采纳团队模板。
async fn list_templates(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let templates = coordinator.templates().list_templates();
    Ok(Json(json!({ "templates": templates })))
}

/// GET /teams/templates/proposals：模板提案列表（只提案，不自动启用）。
async fn list_proposals(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let proposals = coordinator.templates().list_proposals();
    Ok(Json(json!({ "proposals": proposals })))
}

/// POST /teams/templates/proposals/{proposal_id}/adopt：采纳 → 进入模板注册表（幂等）。
async fn adopt_proposal(
    State(state): State<Arc<AppState>>,
    AxumPath(proposal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    let template = coordinator
        .templates()
        .adopt_proposal(&proposal_id)
        .map_err(|m| {
            let code = if m.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(json!({ "error": m })))
        })?;
    Ok(Json(json!({ "template": template })))
}

/// POST /teams/templates/proposals/{proposal_id}/reject：拒绝提案（保留记录，可审计；已采纳 → 400）。
async fn reject_proposal(
    State(state): State<Arc<AppState>>,
    AxumPath(proposal_id): AxumPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let coordinator = state
        .workswarm
        .coordinator()
        .map_err(|e| error_response(&e))?;
    coordinator
        .templates()
        .reject_proposal(&proposal_id)
        .map_err(|m| {
            let code = if m.contains("不存在") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(json!({ "error": m })))
        })?;
    Ok(Json(json!({
        "proposal_id": proposal_id,
        "status": "rejected",
    })))
}

// ---------------------------------------------------------------------------
// 九期（一路）：TrackedRoleWorker 合并检测 / 空 ChangeSet 守卫（真实 git 仓库）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tracked_worker_tests {
    use super::*;
    use std::path::Path;

    /// 唯一临时目录。
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "owo-tracked-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 建一个带初始提交的真实 git 仓库，src/a.rs 处于「执行前已脏」状态。
    /// 返回 (root, head_content, pre_agent_content)。
    fn git_repo_with_pre_dirty_file(tag: &str) -> (PathBuf, &'static str, &'static str) {
        let root = unique_dir(tag);
        let run = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git 可执行");
            assert!(
                output.status.success(),
                "git {args:?} 失败：{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"fn a() {} // HEAD\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        // 用户先手改（执行前已脏）。
        std::fs::write(root.join("src/a.rs"), b"fn a() {} // user dirty\n").unwrap();
        (root, "fn a() {} // HEAD\n", "fn a() {} // user dirty\n")
    }

    fn tracking_for(root: &Path, dir: &Path) -> workspace_change_tracker::Tracker {
        workspace_change_tracker::Tracker {
            root: root.to_path_buf(),
            run_dir: dir.join("run"),
            team_id: "t1".to_string(),
            role: "implementer".to_string(),
            allowed: Vec::new(),
            cas: owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap(),
            audit: None,
        }
    }

    /// 模拟 Agent 写文件的测试 worker。
    struct WriteWorker(PathBuf, &'static str);

    #[async_trait]
    impl Worker for WriteWorker {
        fn name(&self) -> &str {
            "agent"
        }
        async fn run(&self, _input: &Value) -> Result<String, String> {
            std::fs::write(&self.0, self.1).unwrap();
            Ok("ok".to_string())
        }
    }

    fn step_input(step_id: &str) -> Value {
        json!({ "prompt": "p", "_workswarm": { "step_id": step_id } })
    }

    /// 核心完工要求：执行前已经修改过的文件，Agent 再次修改后必须出现在
    /// changed_files；基线哈希 = 执行前内容（reject 恢复到执行前状态，不是 HEAD）；
    /// 有真实修改时 diff_ref 非空。
    #[tokio::test]
    async fn pre_dirty_file_modified_again_enters_changed_files() {
        let dir = unique_dir("dirty");
        let (root, head, pre_agent) = git_repo_with_pre_dirty_file("dirty");
        let tracking = tracking_for(&root, &dir);
        let worker = TrackedRoleWorker {
            inner: Arc::new(WriteWorker(
                root.join("src/a.rs"),
                "fn a() { /* agent fix */ }\n",
            )),
            lease: None,
            tracking: Some(tracking),
        };
        let output = worker.run(&step_input("s-impl")).await;
        assert!(output.is_ok(), "{output:?}");

        let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
            .await
            .unwrap();
        let record = records.last().expect("应有变更记录");
        assert!(
            record.changed_files.contains(&"src/a.rs".to_string()),
            "执行前已脏 + Agent 再次修改 → 必须在 changed_files：{:?}",
            record.changed_files
        );
        assert!(record.diff_ref.is_some(), "有真实修改时 diff_ref 非空");

        // ChangeSet：changed_files 含该文件；基线 = 执行前内容（≠ HEAD 内容）。
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
        let sets = store.list_for_team("t1").unwrap();
        assert_eq!(sets.len(), 1, "恰好一个 ChangeSet");
        let set = &sets[0];
        assert!(set.changed_files.contains(&"src/a.rs".to_string()));
        let base_hash = set
            .base_hashes
            .iter()
            .find(|h| h.path == "src/a.rs")
            .expect("基线含该文件");
        assert_eq!(
            base_hash.sha256.as_deref(),
            Some(owo_agent_core::cas_store::CasStore::hash_of(pre_agent.as_bytes()).as_str()),
            "基线必须是执行前内容，不是 Git HEAD（{head:?}）"
        );
        // 恢复 → 执行前脏内容回来（reject 语义的核心）。
        let report = owo_agent_core::change_set::restore_change_set(
            &root,
            set,
            &tracking_for(&root, &dir).cas,
        )
        .await;
        assert!(report.conflicts.is_empty(), "{report:?}");
        assert_eq!(
            std::fs::read(root.join("src/a.rs")).unwrap(),
            pre_agent.as_bytes()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空变更：不创建 ChangeSet（列表为空），记录 changed_files 为空、diff_ref None。
    #[tokio::test]
    async fn no_change_creates_no_change_set() {
        let dir = unique_dir("nochange");
        let (root, _head, _pre) = git_repo_with_pre_dirty_file("nochange");
        // EchoWorker 无副作用：窗口内没有任何新变更。
        let worker = TrackedRoleWorker {
            inner: Arc::new(EchoWorker) as Arc<dyn Worker>,
            lease: None,
            tracking: Some(tracking_for(&root, &dir)),
        };
        let output = worker.run(&step_input("s-echo")).await;
        assert!(output.is_ok(), "{output:?}");
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
        assert!(
            store.list_for_team("t1").unwrap().is_empty(),
            "无实际变更不得创建空 ChangeSet"
        );
        let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
            .await
            .unwrap();
        let record = records.last().unwrap();
        assert!(
            record.changed_files.is_empty(),
            "{:?}",
            record.changed_files
        );
        assert!(record.diff_ref.is_none(), "无变更不得产生 diff 引用");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 十期 · 四路 R1：ChangeSet 异常闭环——成功/失败/超时/取消都必须完成
    // 变更收尾（前快照→执行→后快照→变更登记），任何结局都不得丢变更、留
    // 不可解释的修改或孤儿记录。
    // -----------------------------------------------------------------------

    /// 按 `input.ops` 顺序执行文件操作的测试 worker（写角色落盘的等效模拟）：
    /// - {"action":"write","path":…,"content":…} → 写/覆写文件；
    /// - {"action":"delete","path":…} → 删除文件；
    /// - {"action":"sleep","ms":…} → 延展窗口（扩大与并发方交叉的概率）；
    /// - {"action":"fail","message":…} → 执行失败（模拟失败/取消结局）。
    struct FileOpsWorker;

    #[async_trait]
    impl Worker for FileOpsWorker {
        fn name(&self) -> &str {
            "fileops"
        }
        async fn run(&self, input: &Value) -> Result<String, String> {
            let root = input
                .get("root")
                .and_then(Value::as_str)
                .ok_or_else(|| "fileops 缺 root".to_string())?;
            let ops = input
                .get("ops")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for op in &ops {
                let action = op.get("action").and_then(Value::as_str).unwrap_or("");
                match action {
                    "write" | "delete" => {
                        let path = op
                            .get("path")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "fileops 缺 path".to_string())?;
                        let target = Path::new(root).join(path);
                        if action == "write" {
                            if let Some(parent) = target.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let content = op.get("content").and_then(Value::as_str).unwrap_or("");
                            std::fs::write(&target, content)
                                .map_err(|e| format!("写 {path} 失败：{e}"))?;
                        } else {
                            std::fs::remove_file(&target)
                                .map_err(|e| format!("删 {path} 失败：{e}"))?;
                        }
                    }
                    "sleep" => {
                        let ms = op.get("ms").and_then(Value::as_u64).unwrap_or(0);
                        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    }
                    "fail" => {
                        return Err(op
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("fileops 注入失败")
                            .to_string());
                    }
                    other => return Err(format!("未知文件操作：{other:?}")),
                }
            }
            Ok("done".to_string())
        }
    }

    /// fileops 专用输入（root + ops；root 供 worker 解析相对路径）。
    fn fileops_input(root: &Path, ops: Value, step_id: &str) -> Value {
        json!({
            "root": root.to_str().unwrap(),
            "prompt": "p",
            "ops": ops,
            "_workswarm": { "step_id": step_id },
        })
    }

    /// 场景 1（写入后失败）：Worker 已真实落盘后步骤失败——仍生成 ChangeSet
    /// （残留修改可审查、可恢复），记录与审计齐全；不生成空壳 ChangeSet 也不丢记录。
    #[tokio::test]
    async fn write_then_fail_still_generates_change_set() {
        let dir = unique_dir("wfail");
        let (root, _head, pre_agent) = git_repo_with_pre_dirty_file("wfail");
        let worker = TrackedRoleWorker {
            inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
            lease: None,
            tracking: Some(tracking_for(&root, &dir)),
        };
        let input = fileops_input(
            &root,
            json!([
                { "action": "write", "path": "src/a.rs",
                  "content": "fn a() { /* agent fix */ }\n" },
                { "action": "fail", "message": "写后失败（注入）" },
            ]),
            "s-impl",
        );
        let output = worker.run(&input).await;
        assert!(output.is_err(), "步骤应失败：{output:?}");
        // 变更收尾必须完成：ChangeSet 已生成且基底 = 执行前内容（不是 HEAD）。
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
        let sets = store.list_for_team("t1").unwrap();
        assert_eq!(sets.len(), 1, "写后失败仍应恰好生成一个 ChangeSet");
        let set = &sets[0];
        assert!(
            set.changed_files.contains(&"src/a.rs".to_string()),
            "{:?}",
            set.changed_files
        );
        assert_eq!(
            set.status,
            owo_agent_protocol::ChangeSetStatus::PendingReview
        );
        let base_hash = set
            .base_hashes
            .iter()
            .find(|h| h.path == "src/a.rs")
            .expect("基底含该文件");
        assert_eq!(
            base_hash.sha256.as_deref(),
            Some(owo_agent_core::cas_store::CasStore::hash_of(pre_agent.as_bytes()).as_str()),
            "失败路径基底仍必须是执行前内容，不混入用户已有修改"
        );
        // 变更记录同样落盘（diff 摘要面）。
        let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
            .await
            .unwrap();
        let record = records.last().expect("应有变更记录");
        assert!(record.changed_files.contains(&"src/a.rs".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 场景 2（写入中取消）：Worker 写入后被取消（协作中断返回已取消）——
    /// 残留修改同样完成变更收尾：ChangeSet 生成、可恢复。
    #[tokio::test]
    async fn cancelled_during_write_still_closes_out() {
        let dir = unique_dir("wcancel");
        let (root, _head, _pre_agent) = git_repo_with_pre_dirty_file("wcancel");
        let worker = TrackedRoleWorker {
            inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
            lease: None,
            tracking: Some(tracking_for(&root, &dir)),
        };
        let input = fileops_input(
            &root,
            json!([
                { "action": "write", "path": "src/cancel.txt", "content": "half\n" },
                { "action": "fail", "message": "已取消" },
            ]),
            "s-impl",
        );
        let output = worker.run(&input).await;
        assert!(output.is_err(), "取消路径应返回原错误：{output:?}");
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
        let sets = store.list_for_team("t1").unwrap();
        assert_eq!(sets.len(), 1, "取消也应完成收尾生成 ChangeSet");
        assert!(
            sets[0]
                .changed_files
                .contains(&"src/cancel.txt".to_string()),
            "{:?}",
            sets[0].changed_files
        );
        // 新建文件基线 = 执行前不存在（恢复即删除）：sha256 为 None 且 content_available。
        let base_hash = sets[0]
            .base_hashes
            .iter()
            .find(|h| h.path == "src/cancel.txt")
            .expect("基底含该文件");
        assert!(
            base_hash.sha256.is_none() && base_hash.content_available,
            "新建文件基线应为「执行前不存在」：{base_hash:?}"
        );
        // 同时确认取消路径的窗口隔离：已有脏文件 a.rs 本轮未触碰 → 不进入变更窗口
        // （窗口 = 执行前后差集，用户已有修改与取消残留区分开）。
        assert!(
            !sets[0].changed_files.iter().any(|p| p == "src/a.rs"),
            "窗口外文件不得进入变更集：{:?}",
            sets[0].changed_files
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 场景 3（非 Git 目录）：内容哈希检测增/改/删——不依赖 git 也能识别
    /// 新建、修改、删除三类变更并生成 ChangeSet（diff 走退化摘要）。
    #[tokio::test]
    async fn non_git_dir_content_hash_detects_new_modify_delete() {
        let dir = unique_dir("nongit");
        let root = dir.join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/mod.txt"), b"v0\n").unwrap();
        std::fs::write(root.join("src/del.txt"), b"x\n").unwrap();
        let tracking = tracking_for(&root, &dir);
        let worker = TrackedRoleWorker {
            inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
            lease: None,
            tracking: Some(tracking),
        };
        let input = fileops_input(
            &root,
            json!([
                { "action": "write", "path": "src/new.txt", "content": "new\n" },
                { "action": "write", "path": "src/mod.txt", "content": "v1\n" },
                { "action": "delete", "path": "src/del.txt" },
            ]),
            "s-impl",
        );
        let output = worker.run(&input).await;
        assert!(output.is_ok(), "{output:?}");
        let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
            .await
            .unwrap();
        let record = records.last().expect("应有变更记录");
        for expected in ["src/new.txt", "src/mod.txt", "src/del.txt"] {
            assert!(
                record.changed_files.contains(&expected.to_string()),
                "非 git 内容哈希应检出 {expected}（实际 {:#?}）",
                record.changed_files
            );
        }
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
        let sets = store.list_for_team("t1").unwrap();
        assert_eq!(sets.len(), 1);
        for expected in ["src/new.txt", "src/mod.txt", "src/del.txt"] {
            assert!(
                sets[0].changed_files.contains(&expected.to_string()),
                "{:?}",
                sets[0].changed_files
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 场景 4（连续两个写 Worker 共享同一租约）：租约覆盖「前快照→执行→后快照→
    /// 变更登记」全程——后一个写步骤的前基线不得混入前一个步骤的变更（各自 ChangeSet
    /// 只含自己的文件），且两个步骤的收尾都完整落盘。
    #[tokio::test]
    async fn two_write_workers_sharing_lease_keep_change_sets_isolated() {
        let dir = unique_dir("wlease");
        let (root, _head, _pre) = git_repo_with_pre_dirty_file("wlease");
        let lease = Arc::new(tokio::sync::Mutex::new(()));
        let mk = |role: &str, dir: &Path| -> TrackedRoleWorker {
            TrackedRoleWorker {
                inner: Arc::new(FileOpsWorker) as Arc<dyn Worker>,
                lease: Some(Arc::clone(&lease)),
                tracking: Some(workspace_change_tracker::Tracker {
                    root: root.clone(),
                    run_dir: dir.join("run"),
                    team_id: "t1".to_string(),
                    role: role.to_string(),
                    allowed: Vec::new(),
                    cas: owo_agent_core::cas_store::CasStore::new(dir.join("cas")).unwrap(),
                    audit: None,
                }),
            }
        };
        // 并发执行：若租约未覆盖收尾，第二个的前快照会误捕第一个的写入。
        let w1 = mk("w1", &dir);
        let w2 = mk("w2", &dir);
        let input1 = fileops_input(
            &root,
            json!([
                { "action": "write", "path": "src/one.txt", "content": "one\n" },
                { "action": "sleep", "ms": 60 },
            ]),
            "s-one",
        );
        let input2 = fileops_input(
            &root,
            json!([
                { "action": "write", "path": "src/two.txt", "content": "two\n" },
                { "action": "sleep", "ms": 60 },
            ]),
            "s-two",
        );
        let (r1, r2) = tokio::join!(w1.run(&input1), w2.run(&input2));
        assert!(r1.is_ok(), "{r1:?}");
        assert!(r2.is_ok(), "{r2:?}");

        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&dir.join("run"));
        let sets = store.list_for_team("t1").unwrap();
        assert_eq!(
            sets.len(),
            2,
            "两个写步骤各应生成自己的 ChangeSet：{sets:#?}"
        );
        let one = sets.iter().find(|s| s.step_id == "s-one").expect("s-one");
        let two = sets.iter().find(|s| s.step_id == "s-two").expect("s-two");
        assert_eq!(
            one.changed_files,
            vec!["src/one.txt".to_string()],
            "{:?}",
            one.changed_files
        );
        assert_eq!(
            two.changed_files,
            vec!["src/two.txt".to_string()],
            "{:?}",
            two.changed_files
        );
        // 变更记录同样两笔、各自窗口独立。
        let records = workspace_change_tracker::load_records(&dir.join("run"), "t1")
            .await
            .unwrap();
        assert_eq!(records.len(), 2, "两笔收尾记录：{records:#?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
