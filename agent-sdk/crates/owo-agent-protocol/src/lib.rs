//! Agent SDK 公开线协议（v1 契约，HTTP JSON + SSE 事件）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// WorkSwarm 协同 DTO（§6.1 / §6.6）
// ---------------------------------------------------------------------------

/// 团队运行模式（对应技术文档 §6.1 三种形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamMode {
    /// 单一角色、短任务。
    Single,
    /// 角色明确、需要接力或评审。
    Team,
    /// 高频稳定流程、版本化模板固化。
    Swarmflow,
}

/// 团队成员运行时绑定（agent / human / worker）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeBinding {
    Agent { agent_id: String },
    Human { user_id: String },
    Worker { worker_name: String },
}

/// 成员健康状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberHealth {
    Active,
    Degraded,
    Offline,
    Fused,
}

/// 团队成员（§6.1 TeamMember）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    pub member_id: String,
    pub role: String,
    pub runtime_binding: RuntimeBinding,
    /// 能力声明（工具/感知/模型等）。
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 工具作用域白名单。
    #[serde(default)]
    pub tool_scope: Vec<String>,
    /// 可读资源范围。
    #[serde(default)]
    pub read_scope: Vec<String>,
    /// 可写资源范围。
    #[serde(default)]
    pub write_scope: Vec<String>,
    /// 预算限制（JSON 自由结构，由调度层解释）。
    #[serde(default)]
    pub budget: Value,
    /// 交接契约描述（下游期望的产物格式/语义）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_contract: Option<String>,
    #[serde(default = "default_member_health")]
    pub health: MemberHealth,
}

fn default_member_health() -> MemberHealth {
    MemberHealth::Active
}

/// 团队运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRunStatus {
    Created,
    Running,
    AwaitingHuman,
    Succeeded,
    Failed,
    Cancelled,
}

impl TeamRunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// 团队运行（§6.1 TeamRun）。
///
/// Leader 产出的结构化协作计划，不是自然语言指令。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRun {
    pub team_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    pub mode: TeamMode,
    pub members: Vec<TeamMember>,
    /// 任务图引用（RunGraph id 或内联 DAG；具体载体由编排层决定）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_graph_ref: Option<String>,
    /// 关联 Project Space。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_space_id: Option<String>,
    /// 团队组建来源：命中的 TeamTemplate id（None = 动态组队）。
    /// 溯源字段：让用户看到「团队为什么被创建」（§9.0 S0 完成标准）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// 共享上下文引用（CAS / artifact refs）。
    #[serde(default)]
    pub shared_context_refs: Vec<String>,
    /// 全局预算（JSON 自由结构）。
    #[serde(default)]
    pub budget: Value,
    /// 人机协作策略（如 human_approval_required / auto_continue）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_policy: Option<String>,
    pub status: TeamRunStatus,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// 产物分类（§6.6 Artifact）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactClassification {
    Public,
    Private,
    Sensitive,
}

/// 产物评审状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Draft,
    PendingReview,
    Approved,
    Rejected,
    Superseded,
}

/// 共享产物（§6.6 Artifact）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub artifact_id: String,
    /// 产物类型（document / code / dataset / review / decision 等）。
    pub kind: String,
    pub version: u32,
    /// 生产者 member_id。
    pub producer: String,
    /// 内容引用（CAS hash / 文件路径 / URL）。
    pub content_ref: String,
    /// Schema 引用（可选，用于结构化校验）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_ref: Option<String>,
    /// 来源引用（trace / artifact / note ids）。
    #[serde(default)]
    pub source_refs: Vec<String>,
    #[serde(default = "default_artifact_classification")]
    pub classification: ArtifactClassification,
    #[serde(default = "default_review_state")]
    pub review_state: ReviewState,
    /// 版本链：本版本所取代的上一版 artifact_id（首版为 None）。
    /// 评审闭环（R2）用于把同 kind 的历次产出串成可追溯链。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_artifact_id: Option<String>,
    pub created_at: String,
}

fn default_artifact_classification() -> ArtifactClassification {
    ArtifactClassification::Private
}

fn default_review_state() -> ReviewState {
    ReviewState::Draft
}

/// 评审决定（§6.6 Artifact 评审闭环，V1-R2）。
///
/// - `approve`：批准（进入 approved head 候选）；
/// - `request_changes`：要求修改（产物回到 Draft 供返工出新版）；
/// - `reject`：驳回（终态拒绝，留审计）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactReviewDecision {
    Approve,
    RequestChanges,
    Reject,
}

/// 不可变评审记录（§6.6 Artifact 评审闭环，V1-R2）。
///
/// 一旦落盘永不修改/删除：评审历史只能追加（append-only）。
/// 同一 `idempotency_key` 重复提交不产生第二条记录（幂等回放）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReviewRecord {
    pub review_id: String,
    /// 被评审的产物。
    pub artifact_id: String,
    /// 被评审的产物版本（与 `expected_version` 乐观并发目标对应）。
    pub artifact_version: u32,
    /// 产物所属团队运行。
    pub team_id: String,
    pub decision: ArtifactReviewDecision,
    /// 评审者（member_id / user_id / 角色名，如 "critic" / "human:u1"）。
    pub reviewer: String,
    /// 评语（可空）。
    #[serde(default)]
    pub comment: String,
    /// 幂等键（存储层唯一约束；同键重放返回既有记录）。
    pub idempotency_key: String,
    /// 评审时的产物内容引用（取证锚点：证明评审的是这份内容）。
    #[serde(default)]
    pub content_ref: String,
    pub created_at: String,
}

/// 决策记录（§6.6 DecisionRecord）。
///
/// 每项会改变任务路线的结论必须写成 DecisionRecord，不能只留在聊天中。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub decision_id: String,
    /// 提出者 member_id 或 user_id。
    pub proposer: String,
    /// 选择/结论摘要。
    pub choice: String,
    /// 理由与依据。
    pub rationale: String,
    /// 受影响的任务/资产 ID。
    #[serde(default)]
    pub affected_refs: Vec<String>,
    pub created_at: String,
}

/// 交接记录（§6.6 HandoffRecord）。
///
/// Agent 交接任务时必须填写的结构化信息，下游从 Project Space 读取而非猜测。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffRecord {
    pub handoff_id: String,
    /// 来源 member_id。
    pub from_member: String,
    /// 目标 member_id（或 * 表示任意下游）。
    pub to_member: String,
    /// 已完成内容摘要。
    pub completed_summary: String,
    /// 未解决问题。
    #[serde(default)]
    pub open_issues: Vec<String>,
    /// 输出产物引用。
    #[serde(default)]
    pub output_artifact_refs: Vec<String>,
    /// 证据引用（trace / snapshot / assertion）。
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// 建议下游动作。
    #[serde(default)]
    pub suggested_next_actions: Vec<String>,
    /// 已知风险。
    #[serde(default)]
    pub known_risks: Vec<String>,
    pub created_at: String,
}

/// 项目空间状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSpaceStatus {
    Active,
    Archived,
    Completed,
}

/// 项目空间（§6.6 ProjectSpace）。
///
/// WorkSwarm 式协同的核心：统一的任务、产物、决策、审批和活动流工作空间。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSpace {
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    /// 任务 ID 列表（完整任务图由 task_graph_ref 承载）。
    #[serde(default)]
    pub tasks: Vec<String>,
    /// 产物 ID 列表。
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// 决策 ID 列表。
    #[serde(default)]
    pub decisions: Vec<String>,
    /// 审批 ID 列表。
    #[serde(default)]
    pub approvals: Vec<String>,
    /// 讨论消息 ID 列表（讨论允许自然语言，但结论须写 DecisionRecord）。
    #[serde(default)]
    pub discussions: Vec<String>,
    /// 活动流条目 ID（按时间排序）。
    #[serde(default)]
    pub activity_stream: Vec<String>,
    /// 交付清单引用（最终产物集合）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_manifest_ref: Option<String>,
    pub version: u32,
    #[serde(default = "default_project_status")]
    pub status: ProjectSpaceStatus,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn default_project_status() -> ProjectSpaceStatus {
    ProjectSpaceStatus::Active
}

// ---------------------------------------------------------------------------
// WorkSwarm 团队模板（§6.1 TeamTemplateRegistry / §6.7 团队经验）
// ---------------------------------------------------------------------------

/// 团队模板中的角色定义（角色组合 + 依赖 + 交付契约，不含历史敏感上下文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTemplateRole {
    pub role: String,
    /// 承担者种类：`agent` | `human` | `worker`。
    pub assignee: String,
    /// 建议 worker 名（agent 角色默认 `agent` 模型驱动；其余为内置 worker 名）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// 依赖的上游角色（任务 DAG 边）。
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// 交接契约（下游期望的产物格式/语义）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_contract: Option<String>,
    /// 验证断言（`non_empty` / `contains:<text>` / `equals:<text>`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
}

/// 团队模板：已验证的角色组合、Swarmflow 与适用条件（§6.1）。
///
/// 只接受经用户采纳的提案进入注册表（§6.7）；复用任务图与交付契约，
/// 不复制历史敏感上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTemplate {
    pub template_id: String,
    pub name: String,
    pub mode: TeamMode,
    pub roles: Vec<TeamTemplateRole>,
    /// 适用条件描述（模板优先匹配的语义依据）。
    pub applicability: String,
    /// 模板来源团队（溯源）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_team_id: Option<String>,
    pub created_at: String,
}

/// 团队模板提案状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamTemplateProposalStatus {
    /// 已生成，等待用户采纳（默认）。
    Proposed,
    /// 用户已采纳，进入 TeamTemplateRegistry。
    Adopted,
    /// 用户已拒绝（保留记录，可审计）。
    Rejected,
}

fn default_proposal_status() -> TeamTemplateProposalStatus {
    TeamTemplateProposalStatus::Proposed
}

/// 团队模板提案（§6.7）：从一次已验证运行生成，**只做提案，不自动启用**；
/// 至少经过一次用户采纳后才进入 TeamTemplateRegistry。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamTemplateProposal {
    pub proposal_id: String,
    pub template: TeamTemplate,
    /// 产生该提案的运行（team_id）。
    pub source_team_id: String,
    /// 证据引用（最终产物 artifact ids）。
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default = "default_proposal_status")]
    pub status: TeamTemplateProposalStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub workspace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub workspace: String,
    pub model: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub fork_point: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRequest {
    pub prompt: String,
    /// 附件 ID（由 `POST /session/{id}/attachments` 返回；发送时注入路径上下文）。
    #[serde(default)]
    pub attachments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkRequest {
    pub message_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindRequest {
    pub keep: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunRequest {
    pub suite_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionResponse {
    pub allow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub healthy: bool,
    pub version: String,
    pub auto_approve: bool,
}

/// SSE 事件协议版本（R10：所有 SSE 事件帧 data 统一携带 `v` 字段）。
/// 变更策略：破坏性事件结构变更 → 递增版本并登记 RFC 注释（弃用期 ≥2 个 minor）。
pub const SSE_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseEvent {
    Progress {
        message: String,
    },
    ToolUse {
        id: String,
        tool: String,
        args: Value,
    },
    ToolResult {
        id: String,
        tool: String,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    PermissionRequest {
        request_id: String,
        tool: String,
        args: Value,
        reason: String,
    },
    Final {
        text: String,
    },
    TokenDelta {
        delta: String,
    },
    Compaction {
        summary: String,
    },
}
