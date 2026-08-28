// R13:S0 编排层（WorkSwarm 最小协同闭环）
//! WorkSwarm 编排内核（Part 6 / S0：§6.1 编排内核、§6.6 Project Space、§6.7 团队经验、§9.0 完成标准）。
//!
//! 复用既有原语，不新建调度器：
//! - 任务图调度复用 [`crate::goal::GoalRunner`]：按「阶段」执行（已完成步骤永不重跑）；
//! - 产物经 [`crate::cas_store::CasStore`] 内容寻址落盘，**以 ref 传递**（版本化 Artifact）；
//! - ProjectSpace / Artifact / DecisionRecord / HandoffRecord / TeamRun 持久化复用
//!   [`crate::project_space_store`]；
//! - 跨成员消息经 [`crate::fleet::AgentBus`]（correlation_id 贯通父子，A3 语义）；
//! - 审计经 [`crate::audit::AuditLog`]（关键动作全部落审计，含 correlation_id）。
//!
//! S0 范围（本模块实现）：
//! - 组队：`single | team | swarmflow`；模板优先（[`TeamTemplateRegistry`]），动态组队 ≤5 个 Agent；
//! - 接力：步骤完成 → 版本化 Artifact + 结构化 [`HandoffRecord`]（context slice，A3）；
//! - 人节点：运行任务开「门闩」等待（cancel 可中断），结果录入后自动唤醒下游；
//! - steer：`continue / steer / replace / cancel / retry`——只改未完成节点，已完成产物永不丢失；
//!   变更必须留下 [`DecisionRecord`]（结论不留在聊天里）；
//! - 成功收尾：生成交付清单 + [`TeamTemplateProposal`]（**只提案，不自动启用**；
//!   用户采纳后才进 TeamTemplateRegistry）。
//!
//! 并发模型：
//! - 每个 team 一个运行任务（server 侧 tokio 任务）顺序驱动阶段循环；
//! - 磁盘状态变更临界区由 per-team 异步锁串行化（阶段边界 / steer / handoff / human-result）;
//! - `run-active` 标志只在阶段执行期间为真；人节点等待窗口为「暂停」，
//!   该窗口内 steer 直接改盘（下一阶段读取最新状态），阶段执行中 steer → Conflict(409)。

use async_trait::async_trait;
use owo_agent_protocol::{
    Artifact, ArtifactClassification, DecisionRecord, HandoffRecord, MemberHealth, ProjectSpace,
    ProjectSpaceStatus, ReviewState, RuntimeBinding, TeamMember, TeamMode, TeamRun, TeamRunStatus,
    TeamTemplate, TeamTemplateProposal, TeamTemplateProposalStatus, TeamTemplateRole,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::audit::AuditLog;
use crate::cas_store::CasStore;
use crate::fleet::{new_correlation_id, AgentBus, MessageKind, OverflowPolicy};
use crate::goal::{
    Goal, GoalBudget, GoalRunState, GoalRunner, GoalStatus, RunnerConfig, Worker, WorkerRegistry,
};
use crate::plan::{verify_output, Plan, StepSpec, StepStatus, VerificationSpec};
use crate::project_space_store::{ProjectSpaceStoreBackend, ProjectSpaceStoreError};

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// WorkSwarm 编排错误（server 层映射：NotFound→404 / Conflict→409 / Validation→400 / 其余→500）。
#[derive(Debug, thiserror::Error)]
pub enum WorkSwarmError {
    #[error("校验失败：{0}")]
    Validation(String),
    #[error("状态冲突：{0}")]
    Conflict(String),
    #[error("未找到：{0}")]
    NotFound(String),
    #[error("存储错误：{0}")]
    Store(#[from] ProjectSpaceStoreError),
    #[error("运行错误：{0}")]
    Run(String),
    #[error("序列化错误：{0}")]
    Serialization(String),
    #[error("IO 错误：{0}")]
    Io(String),
    /// 运行状态文件损坏（明确失败；原文件一律保留，禁止覆盖成新状态）。
    #[error("运行状态损坏：{0}")]
    CorruptState(String),
}

impl From<serde_json::Error> for WorkSwarmError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

pub type WorkSwarmResult<T> = std::result::Result<T, WorkSwarmError>;

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------

fn now_ts() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 文本预览（交接摘要/活动流用）。
fn preview(text: &str, max: usize) -> String {
    let t = text.trim();
    let take: String = t.chars().take(max).collect();
    if t.chars().count() > max {
        format!("{take}…")
    } else {
        take.to_string()
    }
}

/// 角色 → 产物分类（§6.6 Artifact.kind 语义）。
fn role_kind(role: &str) -> &str {
    match role {
        "planner" => "plan",
        "researcher" => "research",
        "builder" => "document",
        "controller" | "verifier" => "verification",
        "critic" => "review",
        "leader" => "final",
        "coordinator" => "coordination",
        other => other,
    }
}

/// 验证断言字符串 → VerificationSpec（`non_empty` / `contains:x` / `equals:x` / 其他=custom）。
fn parse_verify(s: &str) -> VerificationSpec {
    if s == "non_empty" {
        VerificationSpec::OutputNonEmpty
    } else if let Some(x) = s.strip_prefix("contains:") {
        VerificationSpec::OutputContains(x.to_string())
    } else if let Some(x) = s.strip_prefix("equals:") {
        VerificationSpec::OutputEquals(x.to_string())
    } else {
        VerificationSpec::Custom(s.to_string())
    }
}

fn is_critic_role(role: &str) -> bool {
    role == "critic"
}

// ---------------------------------------------------------------------------
// 角色规格 / 运行元数据
// ---------------------------------------------------------------------------

/// 角色规格（组队输入；模板角色的运行时展开）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleSpec {
    pub role: String,
    /// 承担者种类：`agent` | `human` | `worker`。
    pub assignee: String,
    /// 内层 worker 名（agent 角色缺省 `agent` 模型驱动；human 角色 = user_id）。
    pub worker: Option<String>,
    /// 上游角色（任务 DAG 边）。
    pub depends_on: Vec<String>,
    pub handoff_contract: Option<String>,
    /// 验证断言（字符串形式，见 [`parse_verify`]）。
    pub verify: Option<String>,
    /// 附加步骤输入（透传给内层 worker：echo 的 text / agent 的 prompt 覆盖等）。
    pub extra_input: Value,
}

impl RoleSpec {
    pub fn agent(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            assignee: "agent".to_string(),
            worker: None,
            depends_on: Vec::new(),
            handoff_contract: None,
            verify: None,
            extra_input: Value::Null,
        }
    }
}

impl From<TeamTemplateRole> for RoleSpec {
    fn from(r: TeamTemplateRole) -> Self {
        Self {
            role: r.role,
            assignee: r.assignee,
            worker: r.worker,
            depends_on: r.depends_on,
            handoff_contract: r.handoff_contract,
            verify: r.verify,
            extra_input: Value::Null,
        }
    }
}

/// 运行元数据（sidecar 文件 `<run_dir>/<team_id>-meta.json`）：
/// correlation_id + 角色规格（worker 构建的唯一来源；replace 在此生效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub team_id: String,
    pub correlation_id: String,
    pub roles: Vec<RoleSpec>,
}

impl RunMeta {
    fn file_path(run_dir: &Path, team_id: &str) -> PathBuf {
        run_dir.join(format!("{team_id}-meta.json"))
    }

    fn save(&self, run_dir: &Path) -> WorkSwarmResult<()> {
        std::fs::create_dir_all(run_dir).map_err(|e| WorkSwarmError::Io(e.to_string()))?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::file_path(run_dir, &self.team_id), json)
            .map_err(|e| WorkSwarmError::Io(e.to_string()))
    }

    fn load(run_dir: &Path, team_id: &str) -> WorkSwarmResult<Self> {
        let path = Self::file_path(run_dir, team_id);
        let raw = std::fs::read_to_string(&path)
            .map_err(|_| WorkSwarmError::NotFound(format!("运行元数据 {team_id} 不存在")))?;
        serde_json::from_str(&raw).map_err(WorkSwarmError::from)
    }
}

/// 默认接力样例角色（§9.0：planner → builder → critic → leader）。
pub fn default_relay_roles() -> Vec<RoleSpec> {
    let mut planner = RoleSpec::agent("planner");
    planner.handoff_contract = Some(
        "把目标拆解为可执行的建设方案：明确产物要求、验收要点与风险，输出方案大纲（非代码）。"
            .to_string(),
    );
    planner.verify = Some("non_empty".to_string());
    let mut builder = RoleSpec::agent("builder");
    builder.depends_on = vec!["planner".to_string()];
    builder.handoff_contract = Some(
        "依据上游方案（planner 产物）产出主交付物草稿；输出产物正文本身，不要解释过程。"
            .to_string(),
    );
    builder.verify = Some("non_empty".to_string());
    let mut critic = RoleSpec::agent("critic");
    critic.depends_on = vec!["builder".to_string()];
    critic.handoff_contract = Some(
        "只读评审上游草稿（不修改原文）：检查完整性、一致性与风险，输出 JSON {\"approved\":bool,\"score\":0-100,\"comments\":[..]}。"
            .to_string(),
    );
    critic.verify = Some("non_empty".to_string());
    let mut leader = RoleSpec::agent("leader");
    leader.depends_on = vec!["critic".to_string()];
    leader.handoff_contract =
        Some("最终裁决：综合上游草稿与评审意见采纳或修正，输出最终交付物与交付清单。".to_string());
    leader.verify = Some("non_empty".to_string());
    vec![planner, builder, critic, leader]
}

// ---------------------------------------------------------------------------
// 取消令牌（跨阶段取消传播；A3：fan-out/运行支持父任务取消）
// ---------------------------------------------------------------------------

/// 团队运行取消令牌（watch 语义；运行任务每阶段/每次人节点等待都监听）。
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: tokio::sync::watch::Sender<bool>,
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::watch::channel(false);
        Self { tx }
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    pub fn rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.tx.subscribe()
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// 等待取消（值变 true 返回；发送端关闭返回 false）。
pub async fn wait_cancel(token: &CancelToken) -> bool {
    let mut rx = token.rx();
    if *rx.borrow() {
        return true;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 团队模板注册表（§6.1 / §6.7）
// ---------------------------------------------------------------------------

/// 团队模板注册表：`templates/`（已采纳）+ `proposals/`（提案，只提案不自动启用）。
#[derive(Debug, Clone)]
pub struct TeamTemplateRegistry {
    dir: PathBuf,
}

impl TeamTemplateRegistry {
    pub fn new(root: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(root.join("templates"));
        let _ = std::fs::create_dir_all(root.join("proposals"));
        Self { dir: root }
    }

    // -- 模板 --

    pub fn save_template(&self, t: &TeamTemplate) -> std::io::Result<()> {
        let dir = self.dir.join("templates");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("{}.json", t.template_id)),
            serde_json::to_string_pretty(t)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
        )
    }

    pub fn get_template(&self, id: &str) -> Option<TeamTemplate> {
        let path = self.dir.join("templates").join(format!("{id}.json"));
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn list_templates(&self) -> Vec<TeamTemplate> {
        let dir = self.dir.join("templates");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Ok(raw) = std::fs::read_to_string(p) {
                    if let Ok(t) = serde_json::from_str::<TeamTemplate>(&raw) {
                        out.push(t);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.template_id.cmp(&b.template_id));
        out
    }

    /// 模板优先匹配（§6.1）：同形态模板中，适用条件任一关键词命中 objective 即命中；
    /// S0 采用简单子串启发（关键词 = applicability 按分隔符切段，长度 ≥2）。
    pub fn find_match(&self, mode: TeamMode, objective: &str) -> Option<TeamTemplate> {
        let objective_lower = objective.to_lowercase();
        let templates = self.list_templates();
        let mut matched: Vec<&TeamTemplate> = templates
            .iter()
            .filter(|t| t.mode == mode)
            .filter(|t| {
                let tokens: Vec<String> = t
                    .applicability
                    .split([' ', '，', ',', '、', '/', '\n', '\t'])
                    .map(|s| s.to_string())
                    .filter(|s| s.chars().count() >= 2)
                    .collect();
                tokens
                    .iter()
                    .any(|tok| objective_lower.contains(&tok.to_lowercase()))
            })
            .collect();
        matched.sort_by(|a, b| b.created_at.cmp(&a.created_at)); // 最近创建的优先
        matched.into_iter().next().cloned()
    }

    // -- 提案（只提案，不自动启用） --

    pub fn save_proposal(&self, p: &TeamTemplateProposal) -> std::io::Result<()> {
        let dir = self.dir.join("proposals");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("{}.json", p.proposal_id)),
            serde_json::to_string_pretty(p)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?,
        )
    }

    pub fn get_proposal(&self, id: &str) -> Option<TeamTemplateProposal> {
        let path = self.dir.join("proposals").join(format!("{id}.json"));
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn list_proposals(&self) -> Vec<TeamTemplateProposal> {
        let dir = self.dir.join("proposals");
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Ok(raw) = std::fs::read_to_string(p) {
                    if let Ok(t) = serde_json::from_str::<TeamTemplateProposal>(&raw) {
                        out.push(t);
                    }
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    /// 采纳提案 → 进入模板注册表（幂等；已采纳直接返回模板；已拒绝报错）。
    pub fn adopt_proposal(&self, proposal_id: &str) -> Result<TeamTemplate, String> {
        let mut proposal = self
            .get_proposal(proposal_id)
            .ok_or_else(|| format!("提案 {proposal_id} 不存在"))?;
        match proposal.status {
            TeamTemplateProposalStatus::Adopted => {
                // 幂等：模板应已在注册表；缺失时自愈重建。
                if let Some(t) = self.get_template(&proposal.template.template_id) {
                    return Ok(t);
                }
                self.save_template(&proposal.template)
                    .map_err(|e| format!("模板落盘失败：{e}"))?;
                Ok(proposal.template)
            }
            TeamTemplateProposalStatus::Rejected => {
                Err(format!("提案 {proposal_id} 已被拒绝，不能采纳"))
            }
            TeamTemplateProposalStatus::Proposed => {
                self.save_template(&proposal.template)
                    .map_err(|e| format!("模板落盘失败：{e}"))?;
                proposal.status = TeamTemplateProposalStatus::Adopted;
                self.save_proposal(&proposal)
                    .map_err(|e| format!("提案更新失败：{e}"))?;
                Ok(proposal.template)
            }
        }
    }

    /// 拒绝提案（保留记录，可审计）。
    pub fn reject_proposal(&self, proposal_id: &str) -> Result<(), String> {
        let mut proposal = self
            .get_proposal(proposal_id)
            .ok_or_else(|| format!("提案 {proposal_id} 不存在"))?;
        if proposal.status == TeamTemplateProposalStatus::Adopted {
            return Err(format!("提案 {proposal_id} 已采纳，不能拒绝"));
        }
        proposal.status = TeamTemplateProposalStatus::Rejected;
        self.save_proposal(&proposal).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// 编排协调器
// ---------------------------------------------------------------------------

/// 阶段结果（server 运行循环据此推进）。
#[derive(Debug, Clone)]
pub enum PhaseOutcome {
    /// 运行已进入终态（无需再推进）。
    Finished,
    /// 本阶段 agent 批次完成；后续仍有就绪步骤 → 运行循环继续。
    MoreReady,
    /// 等待人节点；运行任务开门闩等待结果（结果录入后自动唤醒下游）。
    AwaitingHuman { waits: Vec<HumanWait> },
    /// 全部步骤成功；可收尾（交付清单 + 模板提案）。
    Done,
    /// 运行失败（team 已置 Failed，产物保留）。
    Failed,
    /// 运行被取消（team 已置 Cancelled，产物保留）。
    Aborted,
}

/// 人节点等待项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HumanWait {
    pub step_id: String,
    pub member_id: String,
    pub user_id: String,
    pub role: String,
}

/// 组队请求。
#[derive(Debug, Clone)]
pub struct CreateTeamRequest {
    pub goal_id: Option<String>,
    pub objective: String,
    pub mode: TeamMode,
    /// 显式指定模板（swarmflow 必填；team 可选；single 忽略）。
    pub template_id: Option<String>,
    /// 动态角色（空 = 按模式取模板默认 / 内置默认接力）。
    pub roles: Vec<RoleSpec>,
    pub budget: Value,
    pub human_policy: Option<String>,
}

impl CreateTeamRequest {
    pub fn new(objective: impl Into<String>, mode: TeamMode) -> Self {
        Self {
            goal_id: None,
            objective: objective.into(),
            mode,
            template_id: None,
            roles: Vec::new(),
            budget: Value::Null,
            human_policy: None,
        }
    }
}

/// steer 指令（§6.1：continue | steer | replace | cancel；R2 追加：retry）。
///
/// S0 语义：只改未完成节点，已完成产物永不丢失；变更必须留下 DecisionRecord。
#[derive(Debug, Clone)]
pub enum SteerCommand {
    /// 失败/取消后重跑：重置未完成步骤（已完成不重跑），清除失败/取消现场。
    Continue,
    /// 修改未完成节点：`step_id` 为空 = 全部未完成节点；`new_input` 合并进步骤输入。
    Steer {
        step_id: Option<String>,
        new_input: Option<Value>,
        note: String,
    },
    /// 更换成员承担者（agent 换 worker / 人节点换用户）：仅影响该成员未完成步骤。
    Replace {
        role: String,
        new_worker: Option<String>,
        new_user_id: Option<String>,
        note: String,
    },
    /// 取消运行（传播到运行中阶段；已完成产物保留）。
    Cancel,
    /// 局部重试（R2）：仅允许指定一个 Failed/Aborted/中断中的步骤；
    /// 只重置目标步骤及其尚未完成的下游节点——已成功步骤、已有 Artifact、
    /// Handoff 与 DecisionRecord 一律不动。重复发送同一 retry 不产生额外副作用。
    Retry { step_id: String, note: String },
}

/// 进程中断识别记录（sidecar 文件 `<run_dir>/<team_id>-interrupted.json`）。
///
/// 进程重启后磁盘状态仍为 Running、且当前无活动运行/运行循环时，
/// 由 [`TeamCoordinator::detect_interrupted`] 识别为 interrupted 并落盘本记录：
/// - 原 Running 步骤转为可恢复状态（Aborted，带中断说明）；
/// - 通过 `continue` 或 `retry` 显式恢复（禁止启动时静默重复执行写操作）；
/// - 恢复成功后本记录被清除。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptedRun {
    pub team_id: String,
    /// 识别时间（RFC3339）。
    pub detected_at: String,
    /// 中断时正在执行的步骤（原 record.status == Running）。
    pub interrupted_steps: Vec<String>,
    pub reason: String,
}

/// 全量中断扫描报告（[`TeamCoordinator::detect_interrupted`]）。
#[derive(Debug, Clone, Default)]
pub struct InterruptionScan {
    /// 本次新识别的中断团队（已识别过的不再重复报告）。
    pub interrupted: Vec<InterruptedRun>,
    /// 状态文件无法解析的团队（原样保留、未覆盖；需人工修复后才能恢复操作）。
    pub unreadable_states: Vec<String>,
}

/// 手动交接输入（§8.5 POST /tasks/{id}/handoff）。
#[derive(Debug, Clone, Default)]
pub struct HandoffFields {
    /// 目标成员（缺省 `*` = 任意下游）。
    pub to_member: Option<String>,
    pub completed_summary: String,
    pub open_issues: Vec<String>,
    pub output_artifact_refs: Vec<String>,
    pub evidence_refs: Vec<String>,
    pub suggested_next_actions: Vec<String>,
    pub known_risks: Vec<String>,
}

/// 进度视图中的执行中步骤（SSE `progress` 事件用）。
#[derive(Debug, Clone, Serialize)]
pub struct ProgressStep {
    pub step_id: String,
    /// 角色名（成员名去掉 `m-` 前缀）。
    pub worker: String,
    pub status: String,
    pub attempts: u32,
    pub started_at: String,
}

/// 步骤计数（进度视图；`aborted` 为附加口径，前四项与既定契约一致）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProgressCounts {
    pub pending: u32,
    pub running: u32,
    pub succeeded: u32,
    pub failed: u32,
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub aborted: u32,
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

/// 团队实时进度快照（`seq` 单调递增；变化即代表有状态转移）。
#[derive(Debug, Clone, Serialize)]
pub struct TeamProgress {
    pub seq: u64,
    pub team_id: String,
    pub status: String,
    pub active: bool,
    pub current_steps: Vec<ProgressStep>,
    pub counts: ProgressCounts,
    pub updated_at: String,
}

/// 阶段领取记录（进程内；进度视图 current_steps 的数据源）。
#[derive(Debug, Clone)]
struct PhaseClaim {
    epoch: u64,
    steps: Vec<ProgressStep>,
}

/// WorkSwarm 协调器：组队、阶段执行、接力注册、人节点、steer、模板提案。
///
/// 共享方式：`Arc<TeamCoordinator>`（内部状态均带锁；Clone 成本 = 若干 Arc）。
#[derive(Clone)]
pub struct TeamCoordinator {
    store: Arc<dyn ProjectSpaceStoreBackend>,
    templates: Arc<TeamTemplateRegistry>,
    cas: CasStore,
    bus: AgentBus,
    audit: Option<Arc<Mutex<AuditLog>>>,
    run_dir: PathBuf,
    /// 动态团队 Agent 成员上限（§6.1：默认不超过 5 个 Agent）。
    max_agent_members: usize,
    /// per-team 状态锁（磁盘状态变更临界区串行化）。
    team_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// per-team 运行中标志（阶段执行期间为真；人节点等待窗口为假 = 暂停）。
    run_flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// per-team 运行循环存活标志（server 运行循环进程内声明；重启后为空 =
    /// 磁盘 Running 但无循环 → 可识别为 interrupted）。
    loop_alive: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// per-team 取消令牌。
    cancels: Arc<Mutex<HashMap<String, Arc<CancelToken>>>>,
    /// per-team 阶段代次（进程内单调）：领取阶段读取，cancel/retry/replace 等转向时 +1；
    /// 旧阶段的合并与产物回传凭代次校验，过期即丢弃（只记审计，不改状态）。
    phase_epochs: Arc<Mutex<HashMap<String, u64>>>,
    /// 当前阶段领取信息（进度视图 current_steps 数据源；每团队至多一条）。
    phase_claims: Arc<Mutex<HashMap<String, PhaseClaim>>>,
    /// per-team 进度事件序号（进程内单调；状态转移时 +1）。
    progress_seqs: Arc<Mutex<HashMap<String, u64>>>,
}

impl TeamCoordinator {
    pub fn new(
        store: Arc<dyn ProjectSpaceStoreBackend>,
        templates: Arc<TeamTemplateRegistry>,
        cas: CasStore,
        run_dir: PathBuf,
    ) -> Self {
        Self {
            store,
            templates,
            cas,
            bus: AgentBus::new(),
            audit: None,
            run_dir,
            max_agent_members: 5,
            team_locks: Arc::new(Mutex::new(HashMap::new())),
            run_flags: Arc::new(Mutex::new(HashMap::new())),
            loop_alive: Arc::new(Mutex::new(HashMap::new())),
            cancels: Arc::new(Mutex::new(HashMap::new())),
            phase_epochs: Arc::new(Mutex::new(HashMap::new())),
            phase_claims: Arc::new(Mutex::new(HashMap::new())),
            progress_seqs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn attach_audit(&mut self, log: Arc<Mutex<AuditLog>>) {
        self.audit = Some(log);
    }

    pub fn bus(&self) -> &AgentBus {
        &self.bus
    }

    pub fn templates(&self) -> &Arc<TeamTemplateRegistry> {
        &self.templates
    }

    pub fn cas(&self) -> &CasStore {
        &self.cas
    }

    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    // -- 内部基础 --

    fn team_lock(&self, team_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.team_locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(team_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn run_flag(&self, team_id: &str) -> Arc<AtomicBool> {
        let mut map = self.run_flags.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(team_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    pub fn cancel_token(&self, team_id: &str) -> Arc<CancelToken> {
        let mut map = self.cancels.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(team_id.to_string())
            .or_insert_with(|| Arc::new(CancelToken::new()))
            .clone()
    }

    fn set_run_active(&self, team_id: &str, active: bool) {
        self.run_flag(team_id).store(active, Ordering::SeqCst);
    }

    pub fn is_run_active(&self, team_id: &str) -> bool {
        self.run_flag(team_id).load(Ordering::SeqCst)
    }

    fn loop_flag(&self, team_id: &str) -> Arc<AtomicBool> {
        let mut map = self.loop_alive.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(team_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    }

    /// 声明/撤销进程内运行循环存活（server 运行循环入口调用；重启后自然为空）。
    pub fn set_loop_alive(&self, team_id: &str, alive: bool) {
        self.loop_flag(team_id).store(alive, Ordering::SeqCst);
    }

    /// 该团队在当前进程中是否有存活的运行循环（磁盘 Running 但此值为假 → 中断候选）。
    pub fn is_loop_alive(&self, team_id: &str) -> bool {
        self.loop_flag(team_id).load(Ordering::SeqCst)
    }

    // -- 阶段代次 / 实时进度（长任务响应性支撑） --

    /// 当前阶段代次（未领取过 = 0；cancel/retry/replace 等转向时 +1）。
    fn phase_epoch(&self, team_id: &str) -> u64 {
        self.phase_epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(team_id)
            .copied()
            .unwrap_or(0)
    }

    /// 阶段代次 +1（转向操作接管现场时调用；返回新代次）。
    fn bump_phase_epoch(&self, team_id: &str) -> u64 {
        let mut map = self.phase_epochs.lock().unwrap_or_else(|e| e.into_inner());
        let next = map.entry(team_id.to_string()).or_insert(0);
        *next = next.wrapping_add(1);
        *next
    }

    /// 登记阶段领取（claim 后调用；进度视图据此外显 current_steps）。
    fn note_phase_claim(&self, team_id: &str, claim: PhaseClaim) {
        self.phase_claims
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(team_id.to_string(), claim);
    }

    /// 清除阶段领取（仅当代次仍匹配；防误清新阶段的领取）。
    fn clear_phase_claim(&self, team_id: &str, epoch: u64) {
        let mut map = self.phase_claims.lock().unwrap_or_else(|e| e.into_inner());
        if map.get(team_id).map(|c| c.epoch) == Some(epoch) {
            map.remove(team_id);
        }
    }

    /// 进度序号 +1（每次状态转移调用；返回新序号）。
    fn advance_progress(&self, team_id: &str) -> u64 {
        let mut map = self.progress_seqs.lock().unwrap_or_else(|e| e.into_inner());
        let seq = map.entry(team_id.to_string()).or_insert(0);
        *seq = seq.wrapping_add(1);
        *seq
    }

    /// 当前进度序号（只读；订阅方据此判断是否有新进度）。
    pub fn progress_seq(&self, team_id: &str) -> u64 {
        self.progress_seqs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(team_id)
            .copied()
            .unwrap_or(0)
    }

    /// 实时进度快照（**不取 team_lock**：长 Worker 执行期间随时可安全调用）。
    ///
    /// - `current_steps` 来自阶段领取记录（代次匹配时）；
    /// - 计数：pending = Pending+Ready，running = Running，failed = Failed+Aborted。
    pub async fn progress_snapshot(&self, team_id: &str) -> WorkSwarmResult<TeamProgress> {
        let team = self
            .store
            .get_team_run(team_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })?;
        let state = self.load_goal_state(team_id)?;
        let mut counts = ProgressCounts::default();
        for record in state.records.values() {
            match record.status {
                StepStatus::Pending | StepStatus::Ready => counts.pending += 1,
                StepStatus::Running => counts.running += 1,
                StepStatus::Succeeded => counts.succeeded += 1,
                StepStatus::Failed => counts.failed += 1,
                StepStatus::Aborted => counts.aborted += 1,
            }
        }
        let claim = self
            .phase_claims
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(team_id)
            .cloned();
        let current_steps = match claim {
            Some(claim) if claim.epoch == self.phase_epoch(team_id) => claim.steps,
            _ => Vec::new(),
        };
        Ok(TeamProgress {
            seq: self.progress_seq(team_id),
            team_id: team_id.to_string(),
            status: format!("{:?}", team.status),
            active: self.is_run_active(team_id),
            current_steps,
            counts,
            updated_at: now_ts(),
        })
    }

    fn audit(&self, team_id: &str, event: &str, detail: String) {
        if let Some(log) = &self.audit {
            if let Ok(mut log) = log.lock() {
                log.record(
                    team_id,
                    event,
                    Some(format!("workswarm/{team_id}")),
                    None,
                    detail,
                );
            }
        }
    }

    async fn load_bundle(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<(TeamRun, ProjectSpace, GoalRunState)> {
        let team = self
            .store
            .get_team_run(team_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })?;
        let project_space_id = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run(format!("团队 {team_id} 缺少 project_space_id")))?;
        let space = self
            .store
            .get_project_space(&project_space_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("项目空间 {project_space_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })?;
        let state = self.load_goal_state(team_id)?;
        Ok((team, space, state))
    }

    /// 读取运行状态（带损坏分类）：
    /// - 文件缺失 → NotFound；
    /// - 解析失败 → CorruptState（原文件保留；明确失败而非静默重建）；
    /// - 其余 IO 错误 → Io。
    fn load_goal_state(&self, team_id: &str) -> WorkSwarmResult<GoalRunState> {
        let path = self.run_dir.join(format!("{team_id}.json"));
        if !path.exists() {
            return Err(WorkSwarmError::NotFound(format!(
                "运行状态 {team_id} 不存在（{}）",
                path.display()
            )));
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| WorkSwarmError::Io(format!("读取 {} 失败：{e}", path.display())))?;
        serde_json::from_str(&raw).map_err(|e| {
            WorkSwarmError::CorruptState(format!(
                "运行状态文件损坏（原文件已保留，未被覆盖）：{}：{e}",
                path.display()
            ))
        })
    }

    fn load_state(&self, team_id: &str) -> WorkSwarmResult<GoalRunState> {
        self.load_goal_state(team_id)
    }

    /// 读取运行状态（HTTP 任务视图 / 诊断用）。
    pub fn load_run_state(&self, team_id: &str) -> WorkSwarmResult<GoalRunState> {
        self.load_state(team_id)
    }

    /// 读取运行元数据（HTTP 层构建 worker 注册表用）。
    pub fn load_run_meta(&self, team_id: &str) -> WorkSwarmResult<RunMeta> {
        RunMeta::load(&self.run_dir, team_id)
    }

    /// 列出全部团队运行（HTTP 列表视图）。
    pub async fn list_team_runs(&self) -> WorkSwarmResult<Vec<TeamRun>> {
        self.store
            .list_team_runs()
            .await
            .map_err(WorkSwarmError::Store)
    }

    /// 读取 TeamRun（不存在 → NotFound）。
    pub async fn get_team_run(&self, team_id: &str) -> WorkSwarmResult<TeamRun> {
        self.store.get_team_run(team_id).await.map_err(|e| match e {
            ProjectSpaceStoreError::NotFound(_) => {
                WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
            }
            other => WorkSwarmError::Store(other),
        })
    }

    /// 读取 Project Space（不存在 → NotFound）。
    pub async fn get_project_space(
        &self,
        project_id: &str,
    ) -> WorkSwarmResult<owo_agent_protocol::ProjectSpace> {
        self.store
            .get_project_space(project_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("项目空间 {project_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })
    }

    /// 列出项目空间的版本化产物（按创建时间）。
    pub async fn list_artifacts(
        &self,
        space: &owo_agent_protocol::ProjectSpace,
    ) -> WorkSwarmResult<Vec<Artifact>> {
        self.store
            .list_artifacts_by_project(&space.project_id)
            .await
            .map_err(WorkSwarmError::Store)
    }

    /// 按团队列出全部结构化 Handoff（评测适配器/诊断用）。
    pub async fn list_handoffs(&self, team_id: &str) -> WorkSwarmResult<Vec<HandoffRecord>> {
        let (_team, space, _state) = self.load_bundle(team_id).await?;
        self.store
            .list_handoffs_by_project(&space.project_id)
            .await
            .map_err(WorkSwarmError::Store)
    }

    /// `cas://sha256:{hash}` → 文本内容（CAS 命中时返回 Some）。
    ///
    /// 评测适配器据此把**版本化 Artifact 的内容**复制进评测沙盒，
    /// 保证最终结果来自 ProjectSpace/CAS 而非某次模型回复的内存值。
    pub fn resolve_content_text(&self, content_ref: &str) -> Option<String> {
        let hash = content_ref.strip_prefix("cas://sha256:")?;
        self.cas.get_text(hash)
    }

    /// 审计日志（S0 可见性：team.* 关键动作尾迹）。
    pub fn audit_log(&self) -> Option<Arc<Mutex<crate::audit::AuditLog>>> {
        self.audit.clone()
    }

    fn persist_state(&self, state: &GoalRunState) -> WorkSwarmResult<()> {
        // 损坏保护：目标状态文件已存在且无法解析时拒绝写入——
        // 任何路径都不得把损坏文件静默覆盖成"新状态"（R2：明确失败并保留原文件）。
        let path = self.run_dir.join(format!("{}.json", state.run_id));
        if path.exists() {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if serde_json::from_str::<GoalRunState>(&raw).is_err() {
                    return Err(WorkSwarmError::CorruptState(format!(
                        "拒绝覆盖损坏的运行状态文件（请人工修复或移除后再试）：{}",
                        path.display()
                    )));
                }
            }
        }
        state
            .persist(&self.run_dir)
            .map(|_| ())
            .map_err(WorkSwarmError::Run)
    }

    fn step_deps_satisfied(state: &GoalRunState, step: &StepSpec) -> bool {
        step.depends_on.iter().all(|d| {
            state
                .records
                .get(d)
                .map(|r| r.status == StepStatus::Succeeded)
                .unwrap_or(false)
        })
    }

    fn ready_steps(state: &GoalRunState) -> Vec<StepSpec> {
        state
            .plan
            .steps
            .iter()
            .filter(|s| {
                let rec = &state.records[&s.id];
                rec.status.can_resume() && Self::step_deps_satisfied(state, s)
            })
            .cloned()
            .collect()
    }

    fn all_succeeded(state: &GoalRunState) -> bool {
        state
            .plan
            .steps
            .iter()
            .all(|s| state.records[&s.id].status == StepStatus::Succeeded)
    }

    fn role_spec_of_member<'a>(
        meta: &'a RunMeta,
        member_id: &'a str,
    ) -> WorkSwarmResult<&'a RoleSpec> {
        meta.roles
            .iter()
            .find(|r| format!("m-{}", r.role) == member_id)
            .ok_or_else(|| {
                WorkSwarmError::Run(format!("成员 {member_id} 无对应角色规格（元数据不一致）"))
            })
    }

    // -- 组队（§6.1：模板优先 + 动态组队 ≤5 Agent） --

    /// 创建团队运行：成员/角色/assignee 绑定 + 任务图 + ProjectSpace + TeamRun。
    pub async fn create_team_run(&self, req: &CreateTeamRequest) -> WorkSwarmResult<TeamRun> {
        let objective = req.objective.trim();
        if objective.is_empty() {
            return Err(WorkSwarmError::Validation("objective 不能为空".to_string()));
        }
        // 角色来源：显式 roles（可搭配显式模板记录来源）> 模板（指定/匹配）> 模式默认。
        let (roles, template_id): (Vec<RoleSpec>, Option<String>) = if !req.roles.is_empty() {
            if let Some(id) = &req.template_id {
                self.templates
                    .get_template(id)
                    .ok_or_else(|| WorkSwarmError::NotFound(format!("模板 {id} 不存在")))?;
            }
            (req.roles.clone(), req.template_id.clone())
        } else {
            let tpl = match &req.template_id {
                Some(id) => Some(
                    self.templates
                        .get_template(id)
                        .ok_or_else(|| WorkSwarmError::NotFound(format!("模板 {id} 不存在")))?,
                ),
                None if req.mode == TeamMode::Swarmflow => {
                    let m = self
                        .templates
                        .find_match(req.mode, objective)
                        .ok_or_else(|| {
                            WorkSwarmError::Validation(
                                "swarmflow 模式必须基于版本化模板（无匹配模板；请先提供 template_id 或 roles）"
                                    .to_string(),
                            )
                        })?;
                    Some(m)
                }
                None => self.templates.find_match(req.mode, objective),
            };
            match tpl {
                Some(t) => (
                    t.roles.iter().cloned().map(RoleSpec::from).collect(),
                    Some(t.template_id),
                ),
                None => {
                    if req.mode == TeamMode::Single {
                        // single：单一角色短任务。
                        (vec![RoleSpec::agent("runner")], None)
                    } else {
                        // team 动态组队：默认接力样例。
                        (default_relay_roles(), None)
                    }
                }
            }
        };

        // 约束：角色唯一；Agent 成员 ≤ max_agent_members。
        let mut seen = std::collections::HashSet::new();
        let mut agent_count = 0usize;
        for r in &roles {
            if r.role.trim().is_empty() {
                return Err(WorkSwarmError::Validation("角色名不能为空".to_string()));
            }
            if !seen.insert(r.role.clone()) {
                return Err(WorkSwarmError::Validation(format!("角色重复：{}", r.role)));
            }
            if r.assignee == "agent" {
                agent_count += 1;
            }
        }
        if agent_count > self.max_agent_members {
            return Err(WorkSwarmError::Validation(format!(
                "动态团队 Agent 成员超过上限（{agent_count} > {}）",
                self.max_agent_members
            )));
        }
        // swarmflow 且显式给了非模板角色 → 仍允许（模板角色 + 补充），此处不额外限制。
        if roles.is_empty() {
            return Err(WorkSwarmError::Validation(
                "团队至少需要一个角色".to_string(),
            ));
        }

        let team_id = format!("team-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let project_id = format!("proj-{team_id}");
        let correlation_id = new_correlation_id();

        // 角色规格（worker 缺省：agent 角色 = "agent" 模型驱动；human = user_id）。
        let specs: Vec<RoleSpec> = roles
            .into_iter()
            .map(|mut r| {
                if r.assignee.is_empty() {
                    r.assignee = "agent".to_string();
                }
                if r.worker.is_none() && r.assignee == "agent" {
                    r.worker = Some("agent".to_string());
                }
                r
            })
            .collect();

        // 成员（agent/human/worker 运行时绑定）。
        let mut members = Vec::new();
        for spec in &specs {
            let member_id = format!("m-{}", spec.role);
            let binding = match spec.assignee.as_str() {
                "human" => RuntimeBinding::Human {
                    user_id: spec.worker.clone().unwrap_or_else(|| "user".to_string()),
                },
                "worker" => RuntimeBinding::Worker {
                    worker_name: spec.worker.clone().unwrap_or_else(|| spec.role.clone()),
                },
                _ => RuntimeBinding::Agent {
                    agent_id: format!("{team_id}:{member_id}"),
                },
            };
            let read_only = is_critic_role(&spec.role);
            self.bus.register(member_id.clone(), 64).await;
            members.push(TeamMember {
                member_id: member_id.clone(),
                role: spec.role.clone(),
                runtime_binding: binding,
                capabilities: vec![spec.role.clone()],
                tool_scope: if read_only {
                    vec!["read".to_string()]
                } else {
                    vec!["read".to_string(), "write".to_string()]
                },
                read_scope: vec!["project_space".to_string(), "artifacts".to_string()],
                write_scope: if read_only {
                    Vec::new()
                } else {
                    vec!["artifacts".to_string()]
                },
                budget: Value::Null,
                handoff_contract: spec.handoff_contract.clone(),
                health: MemberHealth::Active,
            });
        }

        // 任务图（步骤 ↔ 成员 绑定；worker 名 = member_id，由 run 注册表按名派发角色 worker）。
        let mut plan = Plan::new(format!("{team_id}-plan"), team_id.clone());
        plan.description = format!("WorkSwarm 团队 {team_id}：{objective}");
        for spec in &specs {
            let step_id = format!("s-{}", spec.role);
            let member_id = format!("m-{}", spec.role);
            let mut input = spec.extra_input.clone();
            if !input.is_object() {
                input = json!({});
            }
            if let Some(obj) = input.as_object_mut() {
                obj.insert("objective".to_string(), json!(objective));
                obj.insert(
                    "_workswarm".to_string(),
                    json!({ "team_id": team_id, "member_id": member_id, "step_id": step_id }),
                );
            }
            let step = StepSpec {
                id: step_id.clone(),
                depends_on: spec.depends_on.iter().map(|d| format!("s-{d}")).collect(),
                parallel: true,
                worker: member_id,
                input,
                verify: spec.verify.as_ref().map(|v| parse_verify(v)),
                retries: 0,
            };
            plan.add_step(step);
        }
        plan.validate()
            .map_err(|e| WorkSwarmError::Validation(format!("任务图非法：{e}")))?;

        // 目标（预算映射到 GoalBudget）。
        let mut goal = Goal::new(team_id.clone(), objective.to_string());
        goal.budget = parse_goal_budget(&req.budget);
        let mut state = GoalRunState::new(goal, plan);
        state.run_id = team_id.clone();
        state.persist(&self.run_dir).map_err(WorkSwarmError::Run)?;

        // ProjectSpace（统一事实空间：任务/产物/决策/交接/活动）。
        let now = now_ts();
        let space = ProjectSpace {
            project_id: project_id.clone(),
            goal_id: Some(team_id.clone()),
            team_id: Some(team_id.clone()),
            tasks: state.plan.steps.iter().map(|s| s.id.clone()).collect(),
            artifacts: Vec::new(),
            decisions: Vec::new(),
            approvals: Vec::new(),
            discussions: Vec::new(),
            activity_stream: vec![format!(
                "{now} team.created（mode={}，{} 个成员，来源={}）",
                match req.mode {
                    TeamMode::Single => "single",
                    TeamMode::Team => "team",
                    TeamMode::Swarmflow => "swarmflow",
                },
                members.len(),
                template_id
                    .as_deref()
                    .map(|t| format!("template:{t}"))
                    .unwrap_or_else(|| "dynamic".to_string()),
            )],
            delivery_manifest_ref: None,
            status: ProjectSpaceStatus::Active,
            version: 1,
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        self.store.save_project_space(&space).await?;

        // TeamRun（Leader 产出的结构化协作计划）。
        let team = TeamRun {
            team_id: team_id.clone(),
            goal_id: req.goal_id.clone().or_else(|| Some(team_id.clone())),
            mode: req.mode,
            members: members.clone(),
            task_graph_ref: Some(format!("{team_id}-plan")),
            project_space_id: Some(project_id),
            template_id: template_id.clone(),
            shared_context_refs: Vec::new(),
            budget: req.budget.clone(),
            human_policy: req.human_policy.clone(),
            status: TeamRunStatus::Created,
            created_at: now.clone(),
            updated_at: now,
        };
        self.store.save_team_run(&team).await?;

        // 运行元数据（correlation + 角色规格 sidecar）。
        RunMeta {
            team_id: team_id.clone(),
            correlation_id,
            roles: specs,
        }
        .save(&self.run_dir)?;

        self.audit(
            &team_id,
            "team.created",
            format!(
                "组队：mode={} 成员={} 来源={}（correlation 已建立）",
                match team.mode {
                    TeamMode::Single => "single",
                    TeamMode::Team => "team",
                    TeamMode::Swarmflow => "swarmflow",
                },
                members.len(),
                template_id
                    .clone()
                    .map(|t| format!("template:{t}"))
                    .unwrap_or_else(|| "dynamic".to_string()),
            ),
        );
        Ok(team)
    }

    // -- 运行阶段（server 运行循环驱动） --

    /// 执行一个阶段（当前就绪的 agent 批次）。人节点不进入 runner（由运行任务开门闩等待）。
    ///
    /// 每阶段结束后把子状态合并回完整状态并落盘（steer/replace/human 的改盘变更在
    /// 下一阶段重新加载时生效）。
    pub async fn run_phase(
        &self,
        team_id: &str,
        registry: &WorkerRegistry,
    ) -> WorkSwarmResult<PhaseOutcome> {
        // ---- 阶段 A（短锁）：领取 ready 步骤、标记 Running、持久化 ----
        // 锁只覆盖领取与落盘，Worker/模型执行的整段时间**不持锁**，
        // GET 详情 / 任务图 / 产物 / SSE 等读路径不再被长 Worker 阻塞。
        struct PhaseClaimPlan {
            epoch: u64,
            sub_state: GoalRunState,
            claimed: Vec<ProgressStep>,
            meta: RunMeta,
        }
        let claim: PhaseClaimPlan = {
            let lock = self.team_lock(team_id);
            let _guard = lock.lock().await;

            let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
            if team.status.is_terminal() {
                return Ok(PhaseOutcome::Finished);
            }
            let meta = RunMeta::load(&self.run_dir, team_id)?;

            let ready = Self::ready_steps(&state);
            let is_human_step = |s: &StepSpec, meta: &RunMeta| -> bool {
                Self::role_spec_of_member(meta, &s.worker)
                    .map(|r| r.assignee == "human")
                    .unwrap_or(false)
            };
            let agent_steps: Vec<StepSpec> = ready
                .iter()
                .filter(|s| !is_human_step(s, &meta))
                .cloned()
                .collect();
            let human_steps: Vec<StepSpec> = ready
                .iter()
                .filter(|s| is_human_step(s, &meta))
                .cloned()
                .collect();

            // 无就绪：全部完成 → Done；否则死锁（上游失败等）→ Failed。
            if agent_steps.is_empty() && human_steps.is_empty() {
                if Self::all_succeeded(&state) {
                    return Ok(PhaseOutcome::Done);
                }
                self.fail_run_internal(
                    team_id,
                    &mut team,
                    &mut state,
                    "死锁：存在未完成步骤但无就绪步骤（检查上游失败依赖）",
                )
                .await?;
                return Ok(PhaseOutcome::Failed);
            }
            if agent_steps.is_empty() {
                // 仅人节点就绪：进入门闩（无 Worker 执行，无长锁窗口）。
                let waits = self.build_human_waits(team_id, &team, &state, &meta, &human_steps);
                self.mark_awaiting_human(team_id, &mut team, &waits).await?;
                self.persist_state(&state)?;
                return Ok(PhaseOutcome::AwaitingHuman { waits });
            }

            let epoch = self.phase_epoch(team_id);
            self.set_run_active(team_id, true);
            // 磁盘状态 → Running（R2 恢复底座）：进程若在本阶段内崩溃，
            // 磁盘留下 Running 且无活动循环 → 重启后被识别为 interrupted。
            self.mark_team_running(&mut team).await?;
            let started_at = now_ts();
            let mut claimed: Vec<ProgressStep> = Vec::with_capacity(agent_steps.len());
            for step in &agent_steps {
                if let Some(record) = state.records.get_mut(&step.id) {
                    record.status = StepStatus::Running;
                    let role = worker_role(&step.worker).unwrap_or_else(|| step.worker.clone());
                    claimed.push(ProgressStep {
                        step_id: step.id.clone(),
                        worker: role,
                        status: "Running".to_string(),
                        attempts: record.attempts.saturating_add(1),
                        started_at: started_at.clone(),
                    });
                }
            }
            self.persist_state(&state)?;
            self.note_phase_claim(
                team_id,
                PhaseClaim {
                    epoch,
                    steps: claimed.clone(),
                },
            );
            self.advance_progress(team_id);

            // 子计划 = 已完成步骤 + 本批 agent 步骤（人节点/未来步骤不在子计划内）。
            // 领取步在完整状态中标记为 Running（可观测/可恢复），子计划内转换回
            // Pending 供 GoalRunner 执行；合并时以终态覆盖。
            let batch_ids: std::collections::HashSet<&str> =
                agent_steps.iter().map(|s| s.id.as_str()).collect();
            let sub_ids: HashSet<String> = state
                .plan
                .steps
                .iter()
                .filter(|s| {
                    batch_ids.contains(s.id.as_str())
                        || state.records[&s.id].status == StepStatus::Succeeded
                })
                .map(|s| s.id.clone())
                .collect();
            let sub_steps: Vec<StepSpec> = state
                .plan
                .steps
                .iter()
                .filter(|s| sub_ids.contains(&s.id))
                .cloned()
                .map(|mut step| {
                    if batch_ids.contains(step.id.as_str()) {
                        // 领取代次注入步骤输入：RoleWorker 回传产物时凭此校验，
                        // 过期阶段的回传在 register_step_output_checked 被拒收。
                        if let Some(obj) = step.input.as_object_mut() {
                            let workswarm = obj
                                .entry("_workswarm".to_string())
                                .or_insert_with(|| json!({}));
                            if let Some(workswarm_obj) = workswarm.as_object_mut() {
                                workswarm_obj.insert("phase_epoch".to_string(), json!(epoch));
                            }
                        }
                    }
                    step
                })
                .collect();
            let sub_records: BTreeMap<String, _> = state
                .records
                .iter()
                .filter(|(k, _)| sub_ids.contains(*k))
                .map(|(k, v)| {
                    let mut record = v.clone();
                    if batch_ids.contains(k.as_str()) && record.status == StepStatus::Running {
                        record.status = StepStatus::Pending;
                    }
                    (k.clone(), record)
                })
                .collect();
            let mut sub_state = GoalRunState {
                run_id: state.run_id.clone(),
                goal: state.goal.clone(),
                plan: Plan {
                    id: state.plan.id.clone(),
                    goal_id: state.plan.goal_id.clone(),
                    description: format!("{}（阶段子计划）", state.plan.description),
                    steps: sub_steps,
                    created_at: now_ts(),
                },
                records: sub_records,
                steps_taken: 0,
                total_retries: 0,
                replan_count: 0,
                started_at: now_ts(),
                events: Vec::new(),
                aborted: false,
            };
            if let Err(e) = sub_state.plan.validate() {
                self.set_run_active(team_id, false);
                self.clear_phase_claim(team_id, epoch);
                return Err(WorkSwarmError::Run(format!("阶段子计划非法：{e}")));
            }
            // 子目标状态：强制可运行（整体 goal 已终态时上面会 Finished；这里处理 Failed 后 continue 的场景）。
            if sub_state.goal.status.is_terminal() {
                sub_state.goal.transition(GoalStatus::Running);
                sub_state.goal.error = None;
            }
            PhaseClaimPlan {
                epoch,
                sub_state,
                claimed,
                meta,
            }
        }; // —— 阶段 A 结束：锁已释放 ——

        // ---- 阶段 B（无锁）：Worker/模型执行 ----
        let config = RunnerConfig {
            max_parallel: 4,
            persist_dir: None,   // 阶段结束由协调器合并完整状态后统一落盘
            allow_replan: false, // 团队运行失败 = 显式失败（由 continue 决定重试）
            ..Default::default()
        };
        let mut runner = GoalRunner::from_state(claim.sub_state, config);
        if let Some(audit) = &self.audit {
            runner.attach_audit(Arc::clone(audit));
        }
        let cancel = self.cancel_token(team_id);
        let result = tokio::select! {
            r = runner.run(registry) => r,
            _ = wait_cancel(&cancel) => {
                runner.abort();
                Ok(GoalStatus::Aborted)
            }
        };

        // ---- 阶段 C（短锁）：校验代次后合并结果 ----
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let current_epoch = self.phase_epoch(team_id);
        if current_epoch != claim.epoch {
            // 过期阶段：cancel/retry/replace 已接管现场。旧结果只记审计——
            // 不创建 Artifact、不合并记录、不改终态（新阶段会重新领取执行）。
            self.clear_phase_claim(team_id, claim.epoch);
            self.set_run_active(team_id, false);
            self.advance_progress(team_id);
            self.audit(
                team_id,
                "team.phase.stale_drop",
                format!(
                    "阶段 epoch={} 结果丢弃（当前 epoch={}；步骤 {:?}；cancel/retry/replace 已接管）",
                    claim.epoch,
                    current_epoch,
                    claim
                        .claimed
                        .iter()
                        .map(|step| step.step_id.as_str())
                        .collect::<Vec<_>>()
                ),
            );
            let team = self
                .store
                .get_team_run(team_id)
                .await
                .map_err(|e| match e {
                    ProjectSpaceStoreError::NotFound(_) => {
                        WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
                    }
                    other => WorkSwarmError::Store(other),
                })?;
            return Ok(match team.status {
                TeamRunStatus::Cancelled => PhaseOutcome::Aborted,
                TeamRunStatus::Failed | TeamRunStatus::Succeeded => PhaseOutcome::Finished,
                _ if cancel.is_cancelled() => PhaseOutcome::Aborted,
                _ => PhaseOutcome::MoreReady,
            });
        }

        let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
        // 合并子状态 → 完整状态（记录 + 计数器增量），落盘。
        self.merge_phase_into_full(&mut state, &runner.state);
        let meta = claim.meta;
        let is_human_step = |s: &StepSpec, meta: &RunMeta| -> bool {
            Self::role_spec_of_member(meta, &s.worker)
                .map(|r| r.assignee == "human")
                .unwrap_or(false)
        };
        match result {
            Ok(GoalStatus::Succeeded) => {
                self.set_run_active(team_id, false);
                self.persist_state(&state)?;
            }
            Ok(GoalStatus::Aborted) => {
                self.set_run_active(team_id, false);
                self.advance_progress(team_id);
                self.clear_phase_claim(team_id, claim.epoch);
                self.cancel_run_internal(team_id, &mut team, &mut state, "调度器 abort")
                    .await?;
                return Ok(PhaseOutcome::Aborted);
            }
            Ok(GoalStatus::Failed) => {
                self.set_run_active(team_id, false);
                let reason = state
                    .goal
                    .error
                    .clone()
                    .unwrap_or_else(|| "步骤失败".to_string());
                self.persist_state(&state)?;
                self.advance_progress(team_id);
                self.clear_phase_claim(team_id, claim.epoch);
                self.fail_run_internal(team_id, &mut team, &mut state, &reason)
                    .await?;
                return Ok(PhaseOutcome::Failed);
            }
            Ok(_) => {
                self.set_run_active(team_id, false);
                self.persist_state(&state)?;
            }
            Err(e) => {
                self.set_run_active(team_id, false);
                self.persist_state(&state)?;
                self.advance_progress(team_id);
                self.clear_phase_claim(team_id, claim.epoch);
                self.fail_run_internal(team_id, &mut team, &mut state, &format!("执行异常：{e}"))
                    .await?;
                return Ok(PhaseOutcome::Failed);
            }
        }
        self.advance_progress(team_id);
        self.clear_phase_claim(team_id, claim.epoch);

        // 批次后重评就绪（基于落盘前的最新内存状态）。
        let ready = Self::ready_steps(&state);
        let agent_ready = ready.iter().filter(|s| !is_human_step(s, &meta)).count();
        let human_ready: Vec<StepSpec> = ready
            .iter()
            .filter(|s| is_human_step(s, &meta))
            .cloned()
            .collect();
        if agent_ready > 0 {
            return Ok(PhaseOutcome::MoreReady);
        }
        if !human_ready.is_empty() {
            let waits = self.build_human_waits(team_id, &team, &state, &meta, &human_ready);
            self.mark_awaiting_human(team_id, &mut team, &waits).await?;
            self.persist_state(&state)?;
            return Ok(PhaseOutcome::AwaitingHuman { waits });
        }
        if Self::all_succeeded(&state) {
            return Ok(PhaseOutcome::Done);
        }
        self.set_run_active(team_id, false);
        self.persist_state(&state)?;
        self.fail_run_internal(
            team_id,
            &mut team,
            &mut state,
            "死锁：存在未完成步骤但无就绪步骤",
        )
        .await?;
        Ok(PhaseOutcome::Failed)
    }

    fn build_human_waits(
        &self,
        _team_id: &str,
        team: &TeamRun,
        _state: &GoalRunState,
        _meta: &RunMeta,
        human_steps: &[StepSpec],
    ) -> Vec<HumanWait> {
        human_steps
            .iter()
            .filter_map(|s| {
                let member = team.members.iter().find(|m| m.member_id == s.worker)?;
                let user_id = match &member.runtime_binding {
                    RuntimeBinding::Human { user_id } => user_id.clone(),
                    _ => "user".to_string(),
                };
                Some(HumanWait {
                    step_id: s.id.clone(),
                    member_id: member.member_id.clone(),
                    user_id,
                    role: member.role.clone(),
                })
            })
            .collect()
    }

    async fn mark_awaiting_human(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        waits: &[HumanWait],
    ) -> WorkSwarmResult<()> {
        let detail = waits
            .iter()
            .map(|w| format!("{} 等待 {}", w.step_id, w.user_id))
            .collect::<Vec<_>>()
            .join("；");
        team.status = TeamRunStatus::AwaitingHuman;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        self.space_activity(team_id, &format!("team.awaiting_human：{detail}"))
            .await?;
        self.audit(team_id, "team.awaiting_human", detail);
        Ok(())
    }

    // -- R2：中断识别与恢复（进程重启闭环） --

    /// 阶段开批：磁盘 TeamRun → Running（已为 Running 时不重复写盘）。
    ///
    /// 进程在阶段执行期间崩溃时磁盘保留 Running 且无活动循环，
    /// 重启后据此识别为 interrupted（见 [`Self::detect_interrupted`]）。
    async fn mark_team_running(&self, team: &mut TeamRun) -> WorkSwarmResult<()> {
        if team.status == TeamRunStatus::Running {
            return Ok(());
        }
        team.status = TeamRunStatus::Running;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        Ok(())
    }

    fn interrupted_marker_path(&self, team_id: &str) -> PathBuf {
        self.run_dir.join(format!("{team_id}-interrupted.json"))
    }

    fn save_interrupted_marker(&self, run: &InterruptedRun) -> WorkSwarmResult<()> {
        let json = serde_json::to_string_pretty(run)?;
        std::fs::write(self.interrupted_marker_path(&run.team_id), json)
            .map_err(|e| WorkSwarmError::Io(e.to_string()))
    }

    /// 读取中断识别记录（无记录 → None）。
    pub fn load_interrupted(&self, team_id: &str) -> Option<InterruptedRun> {
        let raw = std::fs::read_to_string(self.interrupted_marker_path(team_id)).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// 该团队当前是否被标记为「运行中断，可显式恢复」（continue/retry 恢复后自动清除）。
    pub fn is_interrupted(&self, team_id: &str) -> bool {
        self.load_interrupted(team_id).is_some()
    }

    fn clear_interrupted_marker(&self, team_id: &str) {
        let _ = std::fs::remove_file(self.interrupted_marker_path(team_id));
    }

    /// 中断候选判定：磁盘状态 Running、且本进程既没有活动阶段也没有存活运行循环。
    fn is_interrupt_candidate(&self, team_id: &str, status: TeamRunStatus) -> bool {
        status == TeamRunStatus::Running
            && !self.is_run_active(team_id)
            && !self.is_loop_alive(team_id)
    }

    /// 中断标记核心步骤（**不取锁**；调用方必须已持有 team_lock，避免重入死锁）。
    ///
    /// 语义（R2 冻结）：
    /// - 原记录状态为 Running 的步骤转为 Aborted（可恢复），error 注明「进程中断」；
    /// - 已成功步骤 / 已有 Artifact / Handoff / DecisionRecord 一律不动（禁止静默重放写操作）；
    /// - 仅落 sidecar 标记 + 状态文件标记位——不触发任何步骤执行。
    async fn mark_interrupted_inner(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<Option<InterruptedRun>> {
        if self.load_interrupted(team_id).is_some() {
            return Ok(None); // 已识别过，幂等跳过。
        }
        let (mut team, _space, mut state) = self.load_bundle(team_id).await?;
        if !self.is_interrupt_candidate(team_id, team.status) {
            return Ok(None);
        }
        let steps: Vec<String> = state
            .records
            .values()
            .filter(|r| r.status == StepStatus::Running)
            .map(|r| r.step_id.clone())
            .collect();
        // Running 步骤转为可恢复状态（Aborted）；其余记录不触碰。
        for r in state.records.values_mut() {
            if r.status == StepStatus::Running {
                r.status = StepStatus::Aborted;
                r.error = Some("进程重启：执行被中断（可 continue / retry 显式恢复）".to_string());
            }
        }
        self.persist_state(&state)?;
        let record = InterruptedRun {
            team_id: team_id.to_string(),
            detected_at: now_ts(),
            interrupted_steps: steps.clone(),
            reason: "process_restart".to_string(),
        };
        self.save_interrupted_marker(&record)?;
        let detail = if steps.is_empty() {
            "无正在执行的步骤".to_string()
        } else {
            format!("中断步骤：{}", steps.join(", "))
        };
        team.updated_at = now_ts();
        self.store.save_team_run(&team).await?;
        self.space_activity(
            team_id,
            &format!(
                "team.interrupted：运行在进程重启后识别为中断（{detail}；未自动重放任何写操作）"
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.interrupted",
            format!("进程重启中断识别：{detail}"),
        );
        Ok(Some(record))
    }

    /// 单团队中断标记（apply_steer 路径专用：**调用方已持有 team_lock**）。
    async fn mark_interrupted_if_applicable(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<Option<InterruptedRun>> {
        self.mark_interrupted_inner(team_id).await
    }

    /// 全量扫描：把「磁盘 Running 但无活动运行」的团队识别为 interrupted。
    ///
    /// 启动/首次访问时调用一次即可；幂等（已识别团队不再重复报告）。
    /// 返回新识别列表 + 状态文件无法解析的团队清单（原样保留、需人工修复）。
    pub async fn detect_interrupted(&self) -> WorkSwarmResult<InterruptionScan> {
        let teams = self.list_team_runs().await?;
        let mut scan = InterruptionScan::default();
        for team in teams {
            match self.detect_interrupted_for(&team.team_id).await {
                Ok(Some(record)) => scan.interrupted.push(record),
                Ok(None) => {}
                Err(WorkSwarmError::CorruptState(_)) => {
                    scan.unreadable_states.push(team.team_id.clone());
                }
                Err(WorkSwarmError::NotFound(_)) => {} // 尚无运行状态文件（未开跑）
                Err(e) => return Err(e),
            }
        }
        Ok(scan)
    }

    /// 单团队版 [`Self::detect_interrupted`]（HTTP 详情视图按需调用；幂等）。
    pub async fn detect_interrupted_for(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<Option<InterruptedRun>> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        self.mark_interrupted_inner(team_id).await
    }

    /// 目标步骤的下游闭包（传递闭包）：plan 中经由 depends_on 可达的步骤里，
    /// 尚未成功（status != Succeeded）的部分。retry 只重置该闭包。
    fn downstream_reset_closure(state: &GoalRunState, target_step_id: &str) -> Vec<String> {
        let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
        for s in &state.plan.steps {
            for d in &s.depends_on {
                dependents
                    .entry(d.as_str())
                    .or_default()
                    .push(s.id.as_str());
            }
        }
        let mut closure = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut queue: Vec<&str> = vec![target_step_id];
        while let Some(cur) = queue.pop() {
            let Some(next) = dependents.get(cur) else {
                continue;
            };
            for &n in next {
                if !visited.insert(n.to_string()) {
                    continue;
                }
                if let Some(rec) = state.records.get(n) {
                    if rec.status != StepStatus::Succeeded {
                        closure.push(n.to_string());
                        queue.push(n);
                    }
                }
            }
        }
        closure.sort();
        closure
    }

    /// 合并阶段子状态到完整状态（仅阶段内步骤记录 + 计数器增量；已完成步骤记录不被覆盖）。
    fn merge_phase_into_full(&self, full: &mut GoalRunState, sub: &GoalRunState) {
        let sub_ids: HashSet<String> = sub.plan.steps.iter().map(|s| s.id.clone()).collect();
        for (id, sr) in &sub.records {
            if !sub_ids.contains(id) {
                continue;
            }
            if let Some(fr) = full.records.get_mut(id) {
                if fr.status == StepStatus::Succeeded {
                    continue; // 已完成永不回退
                }
                *fr = sr.clone();
            }
        }
        full.steps_taken = full.steps_taken.saturating_add(sub.steps_taken);
        full.total_retries = full.total_retries.saturating_add(sub.total_retries);
        if sub.goal.error.is_some() {
            full.goal.error = sub.goal.error.clone();
        }
    }

    /// 失败收尾（team → Failed；产物保留；成员 Degraded）。
    async fn fail_run_internal(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
        reason: &str,
    ) -> WorkSwarmResult<()> {
        if !state.goal.status.is_terminal() || state.goal.status == GoalStatus::Aborted {
            state.goal.transition(GoalStatus::Failed);
        }
        state.goal.error = Some(reason.to_string());
        self.persist_state(state)?;
        let failed_members: Vec<String> = state
            .records
            .values()
            .filter(|r| matches!(r.status, StepStatus::Failed | StepStatus::Aborted))
            .filter_map(|r| {
                state
                    .plan
                    .steps
                    .iter()
                    .find(|s| s.id == r.step_id)
                    .map(|s| s.worker.clone())
            })
            .collect();
        team.status = TeamRunStatus::Failed;
        team.updated_at = now_ts();
        for m in &mut team.members {
            if failed_members.contains(&m.member_id) {
                m.health = MemberHealth::Degraded;
            }
        }
        self.store.save_team_run(team).await?;
        self.space_activity(team_id, &format!("team.failed：{reason}（已完成产物保留）"))
            .await?;
        self.audit(team_id, "team.failed", format!("失败：{reason}"));
        // 终态落定：过期中断标记不再有意义。
        self.clear_interrupted_marker(team_id);
        Ok(())
    }

    /// 取消收尾（team → Cancelled；未完成步骤 Aborted；已完成产物保留）。
    async fn cancel_run_internal(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
        reason: &str,
    ) -> WorkSwarmResult<()> {
        state.aborted = true;
        for r in state.records.values_mut() {
            if !r.status.is_terminal() {
                r.status = StepStatus::Aborted;
            }
        }
        if !state.goal.status.is_terminal() {
            state.goal.transition(GoalStatus::Aborted);
        }
        self.persist_state(state)?;
        team.status = TeamRunStatus::Cancelled;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        self.space_activity(
            team_id,
            &format!("team.cancelled：{reason}（已完成产物保留）"),
        )
        .await?;
        self.audit(team_id, "team.cancelled", format!("取消：{reason}"));
        // 终态落定：过期中断标记不再有意义。
        self.clear_interrupted_marker(team_id);
        Ok(())
    }

    async fn space_activity(&self, team_id: &str, msg: &str) -> WorkSwarmResult<()> {
        let team = self.store.get_team_run(team_id).await?;
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run(format!("团队 {team_id} 缺少 project_space_id")))?;
        let mut space = self.store.get_project_space(&pid).await?;
        space.activity_stream.push(msg.to_string());
        if space.activity_stream.len() > 200 {
            let drain = space.activity_stream.len() - 200;
            space.activity_stream.drain(..drain);
        }
        space.version += 1;
        space.updated_at = now_ts();
        self.store.save_project_space(&space).await?;
        Ok(())
    }

    // -- 收尾：交付清单 + 模板提案（§6.7：只提案，不自动启用） --

    /// 成功收尾：交付清单（CAS ref）+ ProjectSpace Completed + 模板提案。
    pub async fn finalize_success(&self, team_id: &str) -> WorkSwarmResult<TeamRun> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (mut team, mut space, mut state) = self.load_bundle(team_id).await?;
        if !Self::all_succeeded(&state) {
            return Err(WorkSwarmError::Conflict(
                "存在未完成步骤，不能收尾".to_string(),
            ));
        }
        state.goal.transition(GoalStatus::Succeeded);
        self.persist_state(&state)?;

        team.status = TeamRunStatus::Succeeded;
        team.updated_at = now_ts();
        self.store.save_team_run(&team).await?;

        let mut final_artifacts: Vec<Value> = Vec::new();
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                final_artifacts.push(json!({
                    "artifact_id": a.artifact_id,
                    "kind": a.kind,
                    "version": a.version,
                    "content_ref": a.content_ref,
                    "producer": a.producer,
                }));
            }
        }
        let manifest = json!({
            "team_id": team_id,
            "objective": state.goal.objective,
            "artifacts": final_artifacts,
            "created_at": now_ts(),
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let manifest_hash = self
            .cas
            .put(&manifest_bytes)
            .map_err(|e| WorkSwarmError::Run(format!("交付清单 CAS 落盘失败：{e}")))?;
        space.status = ProjectSpaceStatus::Completed;
        space.delivery_manifest_ref = Some(format!("cas://sha256:{manifest_hash}"));
        space.version += 1;
        space.updated_at = now_ts();
        space.activity_stream.push(format!(
            "{} team.succeeded：交付 {} 项产物",
            now_ts(),
            final_artifacts.len()
        ));
        self.store.save_project_space(&space).await?;

        // 模板提案（single 不产生：单角色无团队经验可沉淀）。
        if team.mode != TeamMode::Single {
            let meta = RunMeta::load(&self.run_dir, team_id)?;
            let proposal =
                self.build_template_proposal(team_id, &team, &state, &meta, &final_artifacts);
            self.templates
                .save_proposal(&proposal)
                .map_err(|e| WorkSwarmError::Io(format!("模板提案落盘失败：{e}")))?;
            self.space_activity(
                team_id,
                &format!(
                    "template.proposed：{}（只提案，未自动启用；采纳后进入模板注册表）",
                    proposal.proposal_id
                ),
            )
            .await?;
            self.audit(
                team_id,
                "team.template_proposed",
                format!("模板提案 {}（来源运行 {}）", proposal.proposal_id, team_id),
            );
        }
        self.audit(
            team_id,
            "team.succeeded",
            format!("目标达成：{}", state.goal.objective),
        );
        // 终态落定：过期中断标记不再有意义。
        self.clear_interrupted_marker(team_id);
        Ok(team)
    }

    fn build_template_proposal(
        &self,
        team_id: &str,
        team: &TeamRun,
        state: &GoalRunState,
        meta: &RunMeta,
        final_artifacts: &[Value],
    ) -> TeamTemplateProposal {
        let roles: Vec<TeamTemplateRole> = meta
            .roles
            .iter()
            .map(|r| TeamTemplateRole {
                role: r.role.clone(),
                assignee: r.assignee.clone(),
                worker: r.worker.clone(),
                depends_on: r.depends_on.clone(),
                handoff_contract: r.handoff_contract.clone(),
                verify: r.verify.clone(),
            })
            .collect();
        let template = TeamTemplate {
            template_id: format!("tpl-{team_id}"),
            name: preview(&state.goal.objective, 60),
            mode: team.mode,
            roles,
            applicability: preview(&state.goal.objective, 200),
            source_team_id: Some(team_id.to_string()),
            created_at: now_ts(),
        };
        TeamTemplateProposal {
            proposal_id: format!("prop-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            template,
            source_team_id: team_id.to_string(),
            evidence: final_artifacts
                .iter()
                .filter_map(|a| {
                    a.get("artifact_id")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .collect(),
            status: TeamTemplateProposalStatus::Proposed,
            created_at: now_ts(),
        }
    }

    // -- 产物注册 / 接力（A3：handoff 使用结构化 context slice） --

    /// 步骤完成 → 版本化 Artifact（CAS ref）+ HandoffRecord + 项目空间更新 + 总线消息 + 审计。
    ///
    /// 由 [`RoleWorker`] 在 worker 成功后调用；人节点结果经 [`Self::record_human_result`]。
    pub async fn register_step_output(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &str,
    ) -> WorkSwarmResult<Artifact> {
        self.register_step_output_checked(team_id, member_id, role, step_id, output, None)
            .await
    }

    /// 带阶段代次校验的产物登记：`phase_epoch` 与当前代次不一致（cancel/retry/
    /// replace 已接管现场）时，**只记审计事件，不创建 Artifact、不改状态**。
    ///
    /// `phase_epoch = None` 为兼容入口（人节点/诊断路径），跳过代次校验。
    pub async fn register_step_output_checked(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
        step_id: &str,
        output: &str,
        phase_epoch: Option<u64>,
    ) -> WorkSwarmResult<Artifact> {
        if let Some(epoch) = phase_epoch {
            let current = self.phase_epoch(team_id);
            if current != epoch {
                self.audit(
                    team_id,
                    "team.phase.stale_drop",
                    format!(
                        "过期阶段产物回传丢弃：member={member_id} step={step_id} epoch={epoch}（当前 {current}）"
                    ),
                );
                return Err(WorkSwarmError::Conflict(format!(
                    "阶段已过期（epoch {epoch} < {current}）：回传结果已丢弃（cancel/retry/replace 已接管）"
                )));
            }
        }
        let (_team, space, state) = self.load_bundle(team_id).await?;
        let project_id = space.project_id.clone();
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let correlation = meta.correlation_id.clone();

        let version = self.next_artifact_version(&space, role).await?;
        let hash = self
            .cas
            .put(output.as_bytes())
            .map_err(|e| WorkSwarmError::Run(format!("产物 CAS 落盘失败：{e}")))?;
        let content_ref = format!("cas://sha256:{hash}");
        let kind = role_kind(role).to_string();

        // 来源引用：直接上游的最新产物（ref 传递）。
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
        let mut source_refs: Vec<String> = Vec::new();
        for dep in &step.depends_on {
            let Some(dep_step) = state
                .plan
                .steps
                .iter()
                .find(|s| s.id.as_str() == dep.as_str())
            else {
                continue;
            };
            let Some(dep_role) = worker_role(&dep_step.worker) else {
                continue;
            };
            if let Some(a) = self.latest_artifact_for_role(&space, &dep_role).await {
                source_refs.push(a.artifact_id);
            }
        }

        let artifact = Artifact {
            artifact_id: format!("{team_id}:{role}:v{version}"),
            kind: kind.clone(),
            version,
            producer: member_id.to_string(),
            content_ref: content_ref.clone(),
            schema_ref: None,
            source_refs,
            classification: ArtifactClassification::Private,
            review_state: if is_critic_role(role) {
                ReviewState::PendingReview
            } else {
                ReviewState::Draft
            },
            // 步骤产物首版登记不指向旧版本；重跑版本链由评审闭环（R3）维护。
            supersedes_artifact_id: None,
            created_at: now_ts(),
        };
        self.store.save_artifact(&artifact, &project_id).await?;

        // 交接（结构化 context slice 的摘要视图；完整内容在 CAS，下游按 ref 读取）。
        let downstream: Vec<&StepSpec> = state
            .plan
            .steps
            .iter()
            .filter(|s| s.depends_on.iter().any(|d| d == step_id))
            .collect();
        let to_member = downstream
            .first()
            .map(|d| d.worker.clone())
            .unwrap_or_else(|| "*".to_string());
        let (open_issues, known_risks) = Self::parse_optional_json_lists(output);
        let handoff = HandoffRecord {
            handoff_id: format!("{team_id}:{step_id}:v{version}"),
            from_member: member_id.to_string(),
            to_member: to_member.clone(),
            completed_summary: preview(output, 500),
            open_issues,
            output_artifact_refs: vec![artifact.artifact_id.clone()],
            evidence_refs: vec![content_ref],
            suggested_next_actions: downstream
                .iter()
                .filter_map(|d| {
                    let r = worker_role(&d.worker)?;
                    Some(format!("{r}：{}", self.contract_of(&meta, &r)))
                })
                .collect(),
            known_risks,
            created_at: now_ts(),
        };
        self.store.save_handoff(&handoff, &project_id).await?;

        let mut new_space = space;
        new_space.artifacts.push(artifact.artifact_id.clone());
        new_space.version += 1;
        new_space.updated_at = now_ts();
        new_space.activity_stream.push(format!(
            "{} step.completed {step_id} → {}",
            now_ts(),
            artifact.artifact_id
        ));
        if new_space.activity_stream.len() > 200 {
            let drain = new_space.activity_stream.len() - 200;
            new_space.activity_stream.drain(..drain);
        }
        self.store.save_project_space(&new_space).await?;

        // 总线：交接消息（correlation_id 贯通；关键消息溢出拒绝不丢弃）。
        for d in &downstream {
            let _ = self
                .bus
                .send(
                    member_id,
                    &d.worker,
                    MessageKind::Task,
                    correlation.clone(),
                    serde_json::to_value(&handoff).unwrap_or(Value::Null),
                    OverflowPolicy::Reject,
                )
                .await;
        }
        let artifact_id = artifact.artifact_id.clone();
        self.audit(
            team_id,
            "team.handoff",
            format!(
                "{member_id}({role}) 交付 {artifact_id} → {to_member}（correlation={correlation}）"
            ),
        );
        Ok(artifact)
    }

    fn contract_of(&self, meta: &RunMeta, role: &str) -> String {
        meta.roles
            .iter()
            .find(|r| r.role == role)
            .and_then(|r| r.handoff_contract.clone())
            .unwrap_or_else(|| "按角色职责继续".to_string())
    }

    async fn next_artifact_version(
        &self,
        space: &ProjectSpace,
        role: &str,
    ) -> WorkSwarmResult<u32> {
        let kind = role_kind(role);
        let mut max = 0u32;
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                if a.kind == kind && a.version > max {
                    max = a.version;
                }
            }
        }
        Ok(max + 1)
    }

    async fn latest_artifact_for_role(&self, space: &ProjectSpace, role: &str) -> Option<Artifact> {
        let kind = role_kind(role);
        let mut best: Option<Artifact> = None;
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                if a.kind == kind && best.as_ref().is_none_or(|b| a.version > b.version) {
                    best = Some(a);
                }
            }
        }
        best
    }

    // -- 上下文切片（handoff 的运行时视图；A3 结构化 context slice） --

    /// 为 (member, step) 组装结构化上下文切片：
    /// `{ team_id, objective, role, handoff_contract, upstream: [{role, artifact_id, version, content, review_state}] }`。
    pub async fn assemble_context_slice(
        &self,
        team_id: &str,
        member_id: &str,
        step_id: &str,
    ) -> WorkSwarmResult<Value> {
        let (_team, space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let spec = Self::role_spec_of_member(&meta, member_id)?;
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("步骤 {step_id} 不存在")))?;
        let mut upstream = Vec::new();
        for dep in &step.depends_on {
            let dep_step = match state
                .plan
                .steps
                .iter()
                .find(|s| s.id.as_str() == dep.as_str())
            {
                Some(s) => s,
                None => continue,
            };
            let Some(dep_role) = worker_role(&dep_step.worker) else {
                continue;
            };
            if let Some(a) = self.latest_artifact_for_role(&space, &dep_role).await {
                let content = self.cas_content_text(&a.content_ref);
                upstream.push(json!({
                    "role": dep_role,
                    "artifact_id": a.artifact_id,
                    "version": a.version,
                    "content": content,
                    "review_state": format!("{:?}", a.review_state),
                }));
            }
        }
        Ok(json!({
            "team_id": team_id,
            "objective_text": state.goal.objective,
            "role": spec.role,
            "member_id": member_id,
            "handoff_contract": spec.handoff_contract,
            "upstream": upstream,
        }))
    }

    fn cas_content_text(&self, content_ref: &str) -> String {
        content_ref
            .strip_prefix("cas://sha256:")
            .and_then(|h| self.cas.get_text(h))
            .unwrap_or_default()
    }

    /// 组装内层 worker 输入：agent 角色注入 prompt（critic 只读）；内置 worker 注入 text。
    pub fn build_enriched_input(ctx: &Value, input: &Value, worker_kind: &str) -> Value {
        let mut out = if input.is_object() {
            input.clone()
        } else {
            json!({})
        };
        let Some(obj) = out.as_object_mut() else {
            return out;
        };
        if worker_kind == "agent" {
            let has_prompt = obj
                .get("prompt")
                .and_then(Value::as_str)
                .map(|p| !p.trim().is_empty())
                .unwrap_or(false);
            if !has_prompt {
                obj.insert("prompt".to_string(), json!(Self::build_role_prompt(ctx)));
            }
            let role = ctx.get("role").and_then(Value::as_str).unwrap_or("");
            obj.insert("read_only".to_string(), json!(is_critic_role(role)));
        } else if obj.get("text").map(Value::is_null).unwrap_or(true) {
            // 内置 worker（echo 等）：text 承载上下文切片 → 接力链在产物内容中可见。
            obj.insert("text".to_string(), json!(ctx.to_string()));
        }
        out
    }

    /// 角色 prompt（agent worker）：角色 + 目标 + 交接契约 + 上游产物（ref + 截断内容）。
    fn build_role_prompt(ctx: &Value) -> String {
        let objective = ctx
            .get("objective_text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let role = ctx.get("role").and_then(Value::as_str).unwrap_or("member");
        let contract = ctx
            .get("handoff_contract")
            .and_then(Value::as_str)
            .unwrap_or("按角色职责交付产物");
        let mut upstream_blocks = Vec::new();
        if let Some(items) = ctx.get("upstream").and_then(Value::as_array) {
            for item in items {
                let r = item.get("role").and_then(Value::as_str).unwrap_or("?");
                let id = item
                    .get("artifact_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let v = item.get("version").and_then(Value::as_u64).unwrap_or(0);
                let content = item.get("content").and_then(Value::as_str).unwrap_or("");
                let content = preview(content, 4000);
                upstream_blocks.push(format!("### {r} v{v}（{id}）\n{content}"));
            }
        }
        let upstream = if upstream_blocks.is_empty() {
            "（无上游产物；你是首个执行者）".to_string()
        } else {
            upstream_blocks.join("\n\n")
        };
        let critic_hint = if is_critic_role(role) {
            "\n你是只读评审者：不得要求修改或覆盖上游产物，只输出评审结论。"
        } else {
            ""
        };
        format!(
            "# 角色：{role}\n你是 WorkSwarm 团队中的 {role} 成员，与团队成员围绕共享项目空间协作。\n\
## 团队目标\n{objective}\n\n\
## 你的交接契约\n{contract}\n\n\
## 上游产物（来自 Project Space 的版本化共享产物，按 ref 传递）\n{upstream}\n\n\
## 要求\n直接输出你的产物正文（不要输出解释过程）。{critic_hint}"
        )
    }

    /// 输出中可选的结构化字段（`{"open_issues":[..],"known_risks":[..]}`；非对象 → 空）。
    fn parse_optional_json_lists(output: &str) -> (Vec<String>, Vec<String>) {
        let Ok(v) = serde_json::from_str::<Value>(output) else {
            return (Vec::new(), Vec::new());
        };
        if !v.is_object() {
            return (Vec::new(), Vec::new());
        }
        let strings = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        (strings("open_issues"), strings("known_risks"))
    }

    // -- 人节点（结果录入 → 产物 + 状态推进；运行任务自动唤醒下游） --

    /// 录入人节点结果：校验（人节点、未完成、verify）→ 版本化 Artifact + Handoff + 步骤置 Succeeded。
    pub async fn record_human_result(
        &self,
        team_id: &str,
        step_id: &str,
        result: &str,
    ) -> WorkSwarmResult<Artifact> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (team, _space, mut state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        if team.status.is_terminal() {
            return Err(WorkSwarmError::Conflict(format!(
                "运行已终结（{:?}），不能录入人节点结果",
                team.status
            )));
        }
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {step_id} 不存在")))?;
        let spec = Self::role_spec_of_member(&meta, &step.worker)?;
        if spec.assignee != "human" {
            return Err(WorkSwarmError::Validation(format!(
                "任务 {step_id} 不是人节点（assignee={}",
                spec.assignee
            )));
        }
        let record = &state.records[step_id];
        if record.status.is_terminal() {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {step_id} 已终结（{:?}），不能重复录入",
                record.status
            )));
        }
        if let Some(v) = &spec.verify {
            verify_output(&parse_verify(v), result)
                .map_err(|e| WorkSwarmError::Validation(format!("人节点结果未通过验证：{e}")))?;
        }
        if result.trim().is_empty() {
            return Err(WorkSwarmError::Validation("人节点结果不能为空".to_string()));
        }
        // 产物注册（复用步骤产物通道；producer = 人成员）。
        let artifact = self
            .register_step_output(team_id, &step.worker, &spec.role, step_id, result)
            .await?;
        // 步骤置 Succeeded（运行任务据此自动唤醒下游）。
        if let Some(r) = state.records.get_mut(step_id) {
            if !r.status.is_terminal() || r.status == StepStatus::Aborted {
                r.status = StepStatus::Succeeded;
                r.output = Some(result.to_string());
                r.error = None;
            }
        }
        self.persist_state(&state)?;
        self.space_activity(
            team_id,
            &format!(
                "team.human_result：{} 已录入 {}（等待运行循环唤醒下游）",
                step_id, artifact.artifact_id
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.human_result",
            format!("{} 录入人节点结果 → {}", step_id, artifact.artifact_id),
        );
        Ok(artifact)
    }

    // -- 显式交接（§8.5 POST /tasks/{id}/handoff） --

    /// 手动提交结构化交接（成员完成交付后显式登记；校验：本人、已完成）。
    pub async fn submit_handoff(
        &self,
        team_id: &str,
        task_id: &str,
        from_member: &str,
        fields: &HandoffFields,
    ) -> WorkSwarmResult<HandoffRecord> {
        let lock = self.team_lock(team_id);
        let _guard = lock.lock().await;
        let (team, _space, state) = self.load_bundle(team_id).await?;
        let meta = RunMeta::load(&self.run_dir, team_id)?;
        let correlation = meta.correlation_id.clone();
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == task_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {task_id} 不存在")))?;
        if step.worker != from_member {
            return Err(WorkSwarmError::Validation(format!(
                "只有任务 {} 的承担者 {} 可以交接（收到 from_member={from_member}）",
                task_id, step.worker
            )));
        }
        let record = &state.records[task_id];
        if record.status != StepStatus::Succeeded {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {task_id} 未完成（{:?}），不能交接",
                record.status
            )));
        }
        let role = worker_role(&step.worker).unwrap_or_default();
        let source_artifact = self
            .latest_artifact_for_role_by_producer(team_id, from_member, &role)
            .await?;
        let handoff = HandoffRecord {
            handoff_id: format!("{team_id}:{task_id}:manual:{}", now_ms()),
            from_member: from_member.to_string(),
            to_member: fields.to_member.clone().unwrap_or_else(|| "*".to_string()),
            completed_summary: fields.completed_summary.clone(),
            open_issues: fields.open_issues.clone(),
            output_artifact_refs: if fields.output_artifact_refs.is_empty() {
                source_artifact.map(|a| vec![a]).unwrap_or_default()
            } else {
                fields.output_artifact_refs.clone()
            },
            evidence_refs: fields.evidence_refs.clone(),
            suggested_next_actions: fields.suggested_next_actions.clone(),
            known_risks: fields.known_risks.clone(),
            created_at: now_ts(),
        };
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        self.store.save_handoff(&handoff, &pid).await?;
        let _ = self
            .bus
            .send(
                from_member,
                &handoff.to_member,
                MessageKind::Task,
                correlation.clone(),
                serde_json::to_value(&handoff).unwrap_or(Value::Null),
                OverflowPolicy::Reject,
            )
            .await;
        self.space_activity(
            team_id,
            &format!(
                "handoff.manual：{from_member} 显式交接 {}",
                handoff.handoff_id
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.handoff.manual",
            format!("{from_member} 手动交接任务 {task_id}（correlation={correlation}）"),
        );
        Ok(handoff)
    }

    async fn latest_artifact_for_role_by_producer(
        &self,
        team_id: &str,
        member_id: &str,
        role: &str,
    ) -> WorkSwarmResult<Option<String>> {
        let team = self.store.get_team_run(team_id).await?;
        let pid = team
            .project_space_id
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        let space = self.store.get_project_space(&pid).await?;
        let kind = role_kind(role);
        let mut best: Option<Artifact> = None;
        for id in &space.artifacts {
            if let Ok(a) = self.store.get_artifact(id).await {
                if a.kind == kind
                    && a.producer == member_id
                    && best.as_ref().is_none_or(|b| a.version > b.version)
                {
                    best = Some(a);
                }
            }
        }
        Ok(best.map(|a| a.artifact_id))
    }

    // -- steer（continue / steer / replace / cancel / retry；只改未完成节点） --

    /// 应用 steer 指令。
    ///
    /// 并发语义（单写者）：
    /// - `Cancel`：**立即**置位取消令牌（无锁；进行中的阶段 select 立即感知并自行收尾），
    ///   随后取锁收尾（若阶段已先行收尾则幂等跳过）——因此 cancel 永远可达成，
    ///   且不会与运行中阶段竞争写状态。
    /// - `Continue / Steer / Replace / Retry`：前置无锁检查运行标志，运行中 → 立即 Conflict
    ///   （409 语义：待阶段结束后重试，或先 cancel）；随后取锁复检（阶段在检查后启动的窗口也被覆盖）。
    ///   人节点等待窗口（暂停）允许改盘，下一阶段读取最新状态。
    /// - R2 中断恢复：`Continue / Retry` 进入前会先就地识别「磁盘 Running 但无活动运行」的
    ///   中断残留（幂等标记 + Running 步骤转 Aborted 可恢复），随后按显式指令恢复；
    ///   绝不静默重放任何步骤执行。
    pub async fn apply_steer(&self, team_id: &str, cmd: &SteerCommand) -> WorkSwarmResult<TeamRun> {
        match cmd {
            SteerCommand::Cancel => {
                let cancel = self.cancel_token(team_id);
                cancel.cancel(); // 立即（无锁）：进行中的阶段 select 立即感知
                                 // 代次立即失效：旧阶段的合并与产物回传即刻被拒（阶段 C / 回传校验）。
                self.bump_phase_epoch(team_id);
                self.advance_progress(team_id);
                let lock = self.team_lock(team_id);
                let _guard = lock.lock().await; // 运行中阶段会先完成取消收尾（同一把锁）
                let (mut team, mut state) = {
                    let (t, _s, st) = self.load_bundle(team_id).await?;
                    (t, st)
                };
                if !team.status.is_terminal() {
                    self.cancel_run_internal(
                        team_id,
                        &mut team,
                        &mut state,
                        "steer cancel（运行取消）",
                    )
                    .await?;
                }
                Ok(team)
            }
            _ => {
                if self.is_run_active(team_id) {
                    return Err(WorkSwarmError::Conflict(
                        "运行正在执行中，该 steer 不可用（待阶段结束后重试，或先 cancel）"
                            .to_string(),
                    ));
                }
                let lock = self.team_lock(team_id);
                let _guard = lock.lock().await;
                if self.is_run_active(team_id) {
                    return Err(WorkSwarmError::Conflict(
                        "运行正在执行中，该 steer 不可用（待阶段结束后重试，或先 cancel）"
                            .to_string(),
                    ));
                }
                // 转向接管现场：阶段代次 +1（此刻无在飞阶段，防御性使旧回传即刻失效）。
                self.bump_phase_epoch(team_id);
                self.advance_progress(team_id);
                // R2：continue/retry 前先就地识别「磁盘 Running 但无活动运行」的中断残留
                // （幂等；不满足条件时是空操作）。恢复仍必须显式发起——这里只是把
                // 中断遗留的 Running 步骤转成可恢复状态并落识别标记。
                if matches!(cmd, SteerCommand::Continue | SteerCommand::Retry { .. }) {
                    self.mark_interrupted_if_applicable(team_id).await?;
                    let (mut team, mut state) = {
                        let (t, _s, st) = self.load_bundle(team_id).await?;
                        (t, st)
                    };
                    return match cmd {
                        SteerCommand::Continue => {
                            self.steer_continue(team_id, &mut team, &mut state).await
                        }
                        SteerCommand::Retry { step_id, note } => {
                            self.steer_retry(team_id, &mut team, &mut state, step_id, note)
                                .await
                        }
                        _ => unreachable!("matches! 已过滤"),
                    };
                }
                let (mut team, mut state) = {
                    let (t, _s, st) = self.load_bundle(team_id).await?;
                    (t, st)
                };
                match cmd {
                    SteerCommand::Cancel => {
                        // 防御：正常走上面的分支（cancel 不取前置检查）。
                        let (mut t2, mut st2) = {
                            let (t, _s, st) = self.load_bundle(team_id).await?;
                            (t, st)
                        };
                        self.cancel_run_internal(team_id, &mut t2, &mut st2, "steer cancel")
                            .await?;
                        Ok(t2)
                    }
                    SteerCommand::Continue => {
                        self.steer_continue(team_id, &mut team, &mut state).await
                    }
                    SteerCommand::Steer {
                        step_id,
                        new_input,
                        note,
                    } => {
                        self.steer_nodes(team_id, &mut team, &mut state, step_id, new_input, note)
                            .await
                    }
                    SteerCommand::Replace {
                        role,
                        new_worker,
                        new_user_id,
                        note,
                    } => {
                        self.replace_member(
                            team_id,
                            &mut team,
                            role,
                            new_worker.as_deref(),
                            new_user_id.as_deref(),
                            note,
                        )
                        .await
                    }
                    SteerCommand::Retry { step_id, note } => {
                        self.steer_retry(team_id, &mut team, &mut state, step_id, note)
                            .await
                    }
                }
            }
        }
    }

    /// continue 内部逻辑（调用方已完成运行中检查与加锁）。
    async fn steer_continue(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
    ) -> WorkSwarmResult<TeamRun> {
        {
            match team.status {
                TeamRunStatus::Failed | TeamRunStatus::Cancelled => {}
                TeamRunStatus::Created => {}
                // R2：磁盘 Running 但无活动阶段 = 中断遗留或阶段间暂停窗口，
                // 允许显式 continue 恢复（前置检查已确认非运行中）。
                TeamRunStatus::Running if !self.is_run_active(team_id) => {}
                _ => {
                    return Err(WorkSwarmError::Conflict(format!(
                        "当前状态 {:?} 不可 continue（仅 Failed/Cancelled/Created/中断遗留）",
                        team.status
                    )))
                }
            }
            // 重置未完成步骤（已完成永不重跑）；清除失败/取消现场。
            for r in state.records.values_mut() {
                if !r.status.is_terminal() || r.status == StepStatus::Aborted {
                    r.status = StepStatus::Pending;
                    r.attempts = 0;
                    r.output = None;
                    r.error = None;
                }
            }
            state.aborted = false;
            if state.goal.status.is_terminal() {
                state.goal.transition(GoalStatus::Pending);
            }
            state.goal.error = None;
            team.status = TeamRunStatus::Created;
            team.updated_at = now_ts();
            for m in &mut team.members {
                if m.health == MemberHealth::Degraded {
                    m.health = MemberHealth::Active;
                }
            }
            self.store.save_team_run(&*team).await?;
            self.persist_state(&*state)?;
            // 恢复成功：清除中断识别标记。
            self.clear_interrupted_marker(team_id);
            self.space_activity(
                team_id,
                "team.steer.continue：重置未完成步骤（已完成产物保留）",
            )
            .await?;
            self.audit(
                team_id,
                "team.steer.continue",
                "continue：重置未完成步骤并重跑".to_string(),
            );
            Ok(team.clone())
        }
    }

    async fn steer_nodes(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
        step_id: &Option<String>,
        new_input: &Option<Value>,
        note: &str,
    ) -> WorkSwarmResult<TeamRun> {
        // 运行中检查由 apply_steer 前置统一处理（无锁快速路径 + 锁内复检）。
        // R2：磁盘 Running 但无活动阶段（阶段间暂停窗口 / 中断遗留）同样放行——
        // 与旧行为一致：人节点等待/批次间隙允许修改未完成节点。
        match team.status {
            TeamRunStatus::Created
            | TeamRunStatus::Failed
            | TeamRunStatus::Cancelled
            | TeamRunStatus::AwaitingHuman => {}
            TeamRunStatus::Running if !self.is_run_active(team_id) => {}
            _ => {
                return Err(WorkSwarmError::Conflict(format!(
                    "当前状态 {:?} 不可 steer",
                    team.status
                )))
            }
        }
        let note = if note.trim().is_empty() {
            "steer".to_string()
        } else {
            note.trim().to_string()
        };
        let targets: Vec<StepSpec> = match step_id {
            Some(id) => {
                let step = state
                    .plan
                    .steps
                    .iter()
                    .find(|s| &s.id == id)
                    .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {id} 不存在")))?;
                let record = &state.records[&step.id];
                if record.status.is_terminal() && record.status != StepStatus::Aborted {
                    return Err(WorkSwarmError::Conflict(format!(
                        "任务 {} 已完成（{:?}），不能 steer——已完成成果不受新指令影响",
                        step.id, record.status
                    )));
                }
                vec![step.clone()]
            }
            None => state
                .plan
                .steps
                .iter()
                .filter(|s| {
                    let r = &state.records[&s.id];
                    !r.status.is_terminal() || r.status == StepStatus::Aborted
                })
                .cloned()
                .collect(),
        };
        if targets.is_empty() {
            return Err(WorkSwarmError::Conflict(
                "没有可 steer 的未完成节点（全部已完成）".to_string(),
            ));
        }
        let affected: Vec<String> = targets.iter().map(|s| s.id.clone()).collect();
        for step in &mut state.plan.steps {
            if !targets.iter().any(|t| t.id == step.id) {
                continue;
            }
            if let Some(new_input) = new_input {
                if new_input.is_object() {
                    if let (Some(a), Some(b)) = (step.input.as_object_mut(), new_input.as_object())
                    {
                        for (k, v) in b {
                            a.insert(k.clone(), v.clone());
                        }
                    } else {
                        step.input = new_input.clone();
                    }
                } else {
                    step.input = new_input.clone();
                }
            }
            // 重新注入 _workswarm 标记（防被 new_input 覆盖）。
            if let Some(obj) = step.input.as_object_mut() {
                obj.insert(
                    "_workswarm".to_string(),
                    json!({
                        "team_id": team_id,
                        "member_id": step.worker,
                        "step_id": step.id,
                    }),
                );
            }
        }
        // 变更必须留 DecisionRecord（结论不留在聊天里）。
        let decision = DecisionRecord {
            decision_id: format!("{team_id}:steer:{}", now_ms()),
            proposer: "user".to_string(),
            choice: format!("steer：{note}（备选：维持原计划继续）"),
            affected_refs: affected.clone(),
            rationale: "steer：只修改未完成节点，已完成产物保留".to_string(),
            created_at: now_ts(),
        };
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        self.store.save_decision(&decision, &pid).await?;
        team.updated_at = now_ts();
        if let Some(space_id) = &team.project_space_id {
            if let Ok(mut space) = self.store.get_project_space(space_id).await {
                space.decisions.push(decision.decision_id.clone());
                space.version += 1;
                let _ = self.store.save_project_space(&space).await;
            }
        }
        self.persist_state(state)?;
        self.store.save_team_run(team).await?;
        self.space_activity(
            team_id,
            &format!("team.steer：{} 影响 {} 个未完成节点", note, affected.len()),
        )
        .await?;
        self.audit(
            team_id,
            "team.steer",
            format!("steer：{note}；影响节点：{}", affected.join(", ")),
        );
        Ok(team.clone())
    }

    /// retry 内部逻辑（R2 局部重试；调用方已完成运行中检查、加锁与中断就地识别）。
    ///
    /// 契约：
    /// - 目标步骤必须处于 Failed / Aborted（中断识别会把遗留 Running 转 Aborted）；
    ///   已成功（Succeeded）目标 → Conflict——重复发送同一 retry 不产生额外副作用；
    /// - 只重置目标步骤及其**尚未成功**的下游闭包；其余状态（包括并行分支的失败）
    ///   保持原样，由用户逐个显式处理；
    /// - 已成功步骤执行次数不增加；已有 Artifact 版本/CAS ref、Handoff、DecisionRecord 不删除不重跑；
    /// - 所有校验先于任何持久化发生（全部拒绝路径零写副作用）；
    /// - 变更留下 DecisionRecord（结论不留在聊天里），恢复后重启运行循环（server 侧负责 spawn）。
    async fn steer_retry(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        state: &mut GoalRunState,
        step_id: &str,
        note: &str,
    ) -> WorkSwarmResult<TeamRun> {
        let note = if note.trim().is_empty() {
            "retry".to_string()
        } else {
            note.trim().to_string()
        };
        let step = state
            .plan
            .steps
            .iter()
            .find(|s| s.id == step_id)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("任务 {step_id} 不存在")))?
            .clone();
        let target_status = state
            .records
            .get(&step.id)
            .map(|r| r.status)
            .ok_or_else(|| {
                WorkSwarmError::Run(format!("任务 {} 缺少执行记录（状态不一致）", step.id))
            })?;
        // 目标校验先于团队状态闸门：重复发送同一 retry（目标已成功/已重置）必须
        // 返回明确的目标级冲突，而不是笼统的状态冲突——且全部拒绝路径零写副作用。
        if target_status == StepStatus::Succeeded {
            return Err(WorkSwarmError::Conflict(format!(
                "任务 {} 已成功，不能 retry——重复发送同一 retry 不产生额外副作用",
                step.id
            )));
        }
        if !matches!(target_status, StepStatus::Failed | StepStatus::Aborted) {
            return Err(WorkSwarmError::Conflict(format!(
                "仅 Failed/Aborted/中断中的步骤可重试（任务 {} 当前 {:?}）",
                step.id, target_status
            )));
        }
        // 团队状态闸门：只拦「真正运行中」；Created/Failed/Cancelled/AwaitingHuman、
        // 中断遗留与暂停窗口（Running 且无活动阶段）均放行。
        if team.status == TeamRunStatus::Running && self.is_run_active(team_id) {
            return Err(WorkSwarmError::Conflict(format!(
                "当前状态 {:?} 不可 retry（运行中）",
                team.status
            )));
        }

        // ---- 校验全部通过，开始变更 ----
        let downstream = Self::downstream_reset_closure(state, &step.id);
        let mut affected = vec![step.id.clone()];
        affected.extend(downstream.iter().cloned());
        let reset_ids: std::collections::HashSet<&str> =
            affected.iter().map(String::as_str).collect();
        for r in state.records.values_mut() {
            if reset_ids.contains(r.step_id.as_str()) && r.status != StepStatus::Succeeded {
                r.status = StepStatus::Pending;
                r.attempts = 0;
                r.output = None;
                r.error = None;
            }
        }
        state.aborted = false;
        if state.goal.status.is_terminal() {
            state.goal.transition(GoalStatus::Pending);
        }
        state.goal.error = None;

        // 受影响成员恢复健康（Degraded → Active；只动受影响成员）。
        let affected_members: HashSet<String> = state
            .plan
            .steps
            .iter()
            .filter(|s| reset_ids.contains(s.id.as_str()))
            .map(|s| s.worker.clone())
            .collect();
        for m in &mut team.members {
            if m.health == MemberHealth::Degraded && affected_members.contains(&m.member_id) {
                m.health = MemberHealth::Active;
            }
        }
        team.status = TeamRunStatus::Created;
        team.updated_at = now_ts();

        // 变更留痕：DecisionRecord（affected_refs = 目标 + 未完成下游闭包）。
        let decision = DecisionRecord {
            decision_id: format!("{team_id}:retry:{}", now_ms()),
            proposer: "user".to_string(),
            choice: format!(
                "retry：{note}（目标 {} 及未完成下游共 {} 个节点）",
                step.id,
                affected.len()
            ),
            affected_refs: affected.clone(),
            rationale: "retry：仅重置目标步骤及未完成下游；已成功步骤与既有产物/交接/决策不动"
                .to_string(),
            created_at: now_ts(),
        };
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        self.store.save_decision(&decision, &pid).await?;
        if let Some(space_id) = &team.project_space_id {
            if let Ok(mut space) = self.store.get_project_space(space_id).await {
                space.decisions.push(decision.decision_id.clone());
                space.version += 1;
                let _ = self.store.save_project_space(&space).await;
            }
        }

        self.persist_state(state)?;
        self.store.save_team_run(team).await?;
        // 恢复成功：清除中断识别标记。
        self.clear_interrupted_marker(team_id);
        self.space_activity(
            team_id,
            &format!(
                "team.steer.retry：{} 影响 {} 个节点（其余成功产物保持不变）",
                note,
                affected.len()
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.steer.retry",
            format!("retry：{note}；影响节点：{}", affected.join(", ")),
        );
        Ok(team.clone())
    }

    async fn replace_member(
        &self,
        team_id: &str,
        team: &mut TeamRun,
        role: &str,
        new_worker: Option<&str>,
        new_user_id: Option<&str>,
        note: &str,
    ) -> WorkSwarmResult<TeamRun> {
        // 运行中检查由 apply_steer 前置统一处理（无锁快速路径 + 锁内复检）。
        // R2：Running 且无活动阶段（暂停窗口 / 中断遗留）同样放行（与 steer 一致）。
        match team.status {
            TeamRunStatus::Created
            | TeamRunStatus::Failed
            | TeamRunStatus::Cancelled
            | TeamRunStatus::AwaitingHuman => {}
            TeamRunStatus::Running if !self.is_run_active(team_id) => {}
            _ => {
                return Err(WorkSwarmError::Conflict(format!(
                    "当前状态 {:?} 不可 replace",
                    team.status
                )))
            }
        }
        let member = team
            .members
            .iter_mut()
            .find(|m| m.role == role)
            .ok_or_else(|| WorkSwarmError::NotFound(format!("角色 {role} 的成员不存在")))?;
        let note = if note.trim().is_empty() {
            "replace".to_string()
        } else {
            note.trim().to_string()
        };
        let old_binding = format!("{:?}", member.runtime_binding);
        match (new_worker, new_user_id) {
            (Some(w), Some(u)) if !w.is_empty() && !u.is_empty() => {
                return Err(WorkSwarmError::Validation(
                    "new_worker 与 new_user_id 只能提供一个".to_string(),
                ));
            }
            (Some(w), _) if !w.trim().is_empty() => {
                member.runtime_binding = RuntimeBinding::Agent {
                    agent_id: format!("{team_id}:{}:replaced", member.member_id),
                };
                member.health = MemberHealth::Active;
            }
            (None, Some(u)) if !u.trim().is_empty() => {
                member.runtime_binding = RuntimeBinding::Human {
                    user_id: u.to_string(),
                };
                member.health = MemberHealth::Active;
            }
            _ => {
                return Err(WorkSwarmError::Validation(
                    "replace 需要 new_worker 或 new_user_id 之一".to_string(),
                ))
            }
        }
        // 角色规格 sidecar 同步（运行循环每阶段据此重建 worker 注册表 → 新阶段生效）。
        let mut meta = RunMeta::load(&self.run_dir, team_id)?;
        let spec = meta
            .roles
            .iter_mut()
            .find(|r| r.role == role)
            .ok_or_else(|| {
                WorkSwarmError::Run(format!("角色 {role} 无角色规格（元数据不一致）"))
            })?;
        match (new_worker, new_user_id) {
            (Some(w), _) => {
                spec.assignee = "agent".to_string();
                spec.worker = Some(w.trim().to_string());
            }
            (None, Some(u)) => {
                spec.assignee = "human".to_string();
                spec.worker = Some(u.trim().to_string());
            }
            _ => {}
        }
        meta.save(&self.run_dir)?;
        // 只有未完成步骤换人（已完成产物保持原承担者记录 = 历史事实）。
        let affected: Vec<String> = self
            .load_state(team_id)
            .map(|s| {
                s.plan
                    .steps
                    .iter()
                    .filter(|st| {
                        st.worker == member.member_id && !s.records[&st.id].status.is_terminal()
                    })
                    .map(|st| st.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let decision = DecisionRecord {
            decision_id: format!("{team_id}:replace:{}", now_ms()),
            proposer: "user".to_string(),
            choice: format!("replace {role}：{note}（原绑定 {old_binding}）"),
            affected_refs: affected.clone(),
            rationale: "replace：仅未完成节点换人，已完成产物保留原承担者".to_string(),
            created_at: now_ts(),
        };
        let pid = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run("缺少 project_space_id".to_string()))?;
        self.store.save_decision(&decision, &pid).await?;
        team.updated_at = now_ts();
        self.store.save_team_run(team).await?;
        self.space_activity(
            team_id,
            &format!(
                "team.replace：{} 更换承担者（{} 个未完成节点受影响）",
                role,
                affected.len()
            ),
        )
        .await?;
        self.audit(
            team_id,
            "team.replace",
            format!("{role} 换人：{note}；影响节点：{}", affected.join(", ")),
        );
        Ok(team.clone())
    }
}

// ---------------------------------------------------------------------------
// 角色 worker（worker 注册表层：内层 worker + 完成即登记产物/交接）
// ---------------------------------------------------------------------------

/// 角色 worker：步骤完成后自动登记版本化 Artifact + HandoffRecord（产物经 ref 传递）。
pub struct RoleWorker {
    coordinator: Arc<TeamCoordinator>,
    team_id: String,
    member_id: String,
    role: String,
    inner: Arc<dyn Worker>,
}

impl RoleWorker {
    pub fn new(
        coordinator: Arc<TeamCoordinator>,
        team_id: String,
        member_id: String,
        role: String,
        inner: Arc<dyn Worker>,
    ) -> Self {
        Self {
            coordinator,
            team_id,
            member_id,
            role,
            inner,
        }
    }
}

#[async_trait]
impl Worker for RoleWorker {
    fn name(&self) -> &str {
        self.member_id.as_str()
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let step_id = input
            .get("_workswarm")
            .and_then(|w| w.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // 领取代次：cancel/retry/replace 接管现场后，旧阶段回传凭此被拒收。
        let phase_epoch = input
            .get("_workswarm")
            .and_then(|w| w.get("phase_epoch"))
            .and_then(Value::as_u64);
        let ctx = match self
            .coordinator
            .assemble_context_slice(&self.team_id, &self.member_id, &step_id)
            .await
        {
            Ok(c) => c,
            Err(e) => return Err(format!("上下文切片组装失败：{e}")),
        };
        let worker_kind = self.inner.name().to_string();
        let enriched = TeamCoordinator::build_enriched_input(&ctx, input, &worker_kind);
        let out = self.inner.run(&enriched).await?;
        if let Err(e) = self
            .coordinator
            .register_step_output_checked(
                &self.team_id,
                &self.member_id,
                &self.role,
                &step_id,
                &out,
                phase_epoch,
            )
            .await
        {
            return Err(format!("产物登记失败：{e}"));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

/// 成员 worker 名 → 角色（worker 名约定为 `m-{role}`）。
fn worker_role(worker: &str) -> Option<String> {
    worker.strip_prefix("m-").map(str::to_string)
}

/// 预算 JSON → GoalBudget（缺省用默认值）。
fn parse_goal_budget(budget: &Value) -> GoalBudget {
    let mut b = GoalBudget::default();
    if let Some(obj) = budget.as_object() {
        if let Some(v) = obj.get("max_steps").and_then(Value::as_u64) {
            b.max_steps = v as u32;
        }
        if let Some(v) = obj.get("max_retries_per_step").and_then(Value::as_u64) {
            b.max_retries_per_step = v as u32;
        }
        if let Some(v) = obj.get("max_total_retries").and_then(Value::as_u64) {
            b.max_total_retries = v as u32;
        }
        if let Some(v) = obj.get("max_replans").and_then(Value::as_u64) {
            b.max_replans = v as u32;
        }
        if let Some(v) = obj.get("max_duration_secs").and_then(Value::as_u64) {
            b.max_duration_secs = v;
        }
    }
    b
}

// ---------------------------------------------------------------------------
// 模块内单测（契约测试见 tests/workswarm_tests.rs）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use owo_agent_protocol::TeamTemplate;

    #[test]
    fn default_relay_roles_form_valid_dag() {
        let roles = default_relay_roles();
        assert_eq!(roles.len(), 4);
        let mut plan = Plan::new("p", "g");
        for r in &roles {
            let step = StepSpec {
                id: format!("s-{}", r.role),
                depends_on: r.depends_on.iter().map(|d| format!("s-{d}")).collect(),
                parallel: true,
                worker: format!("m-{}", r.role),
                input: Value::Null,
                verify: r.verify.as_ref().map(|v| parse_verify(v)),
                retries: 0,
            };
            plan.add_step(step);
        }
        plan.validate().unwrap();
    }

    #[test]
    fn parse_verify_kinds() {
        assert!(matches!(
            parse_verify("non_empty"),
            VerificationSpec::OutputNonEmpty
        ));
        assert!(matches!(
            parse_verify("contains:ok"),
            VerificationSpec::OutputContains(_)
        ));
        assert!(matches!(
            parse_verify("equals:x"),
            VerificationSpec::OutputEquals(_)
        ));
        assert!(matches!(
            parse_verify("whatever"),
            VerificationSpec::Custom(_)
        ));
    }

    #[test]
    fn role_kind_mapping() {
        assert_eq!(role_kind("planner"), "plan");
        assert_eq!(role_kind("builder"), "document");
        assert_eq!(role_kind("critic"), "review");
        assert_eq!(role_kind("leader"), "final");
        assert_eq!(role_kind("custom-role"), "custom-role");
    }

    #[test]
    fn template_adopt_is_idempotent_and_reject_blocks() {
        let dir = std::env::temp_dir().join(format!("owo-workswarm-test-{}", now_ms()));
        let reg = TeamTemplateRegistry::new(dir.clone());
        let template = TeamTemplate {
            template_id: "tpl-x".into(),
            name: "test".into(),
            mode: TeamMode::Team,
            roles: Vec::new(),
            applicability: "浏览器 任务".into(),
            source_team_id: Some("team-x".into()),
            created_at: now_ts(),
        };
        let proposal = TeamTemplateProposal {
            proposal_id: "prop-x".into(),
            template: template.clone(),
            source_team_id: "team-x".into(),
            evidence: vec!["art-1".into()],
            status: TeamTemplateProposalStatus::Proposed,
            created_at: now_ts(),
        };
        reg.save_proposal(&proposal).unwrap();
        // 提案不影响注册表（只提案，不自动启用）。
        assert!(reg.get_template("tpl-x").is_none());
        // 采纳 → 进注册表。
        let adopted = reg.adopt_proposal("prop-x").unwrap();
        assert_eq!(adopted.template_id, "tpl-x");
        assert!(reg.get_template("tpl-x").is_some());
        // 幂等。
        let again = reg.adopt_proposal("prop-x").unwrap();
        assert_eq!(again.template_id, "tpl-x");
        // 已采纳不可拒绝。
        assert!(reg.reject_proposal("prop-x").is_err());
        // 匹配：applicability 关键词命中。
        assert!(reg
            .find_match(TeamMode::Team, "完成浏览器表单任务")
            .is_some());
        assert!(reg
            .find_match(TeamMode::Single, "完成浏览器表单任务")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enriched_input_agent_and_echo_paths() {
        let ctx = json!({
            "team_id": "t", "objective_text": "O", "role": "critic",
            "handoff_contract": "C", "upstream": [{"role":"builder","artifact_id":"a1","version":1,"content":"BODY"}]
        });
        // agent：注入 prompt + read_only（critic 只读）。
        let v = TeamCoordinator::build_enriched_input(&ctx, &json!({}), "agent");
        assert!(v["prompt"].as_str().unwrap().contains("# 角色：critic"));
        assert_eq!(v["read_only"], true);
        // echo：text 承载上下文切片。
        let v = TeamCoordinator::build_enriched_input(&ctx, &json!({}), "echo");
        assert!(v["text"].as_str().unwrap().contains("BODY"));
    }
}
