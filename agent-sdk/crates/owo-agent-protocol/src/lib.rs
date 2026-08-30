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
    /// 五期：组队策略决策（TeamPlan 序列化：mode/requested/roles/parallelism/
    /// budget_calls_total/json_repair/reasons）。`auto` 判定理由供 UI 直接渲染；
    /// 旧记录反序列化为 None（additive，不破坏既有 wire）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy_decision: Option<Value>,
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
    // -- 七期（第三路）Artifact 交付扩展（全部 additive：旧记录 serde 缺省反序列化） --
    /// 所属团队运行（七期 additive；旧记录缺省为空）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub team_id: String,
    /// 内容格式（七期：text|markdown|json|csv|research；旧记录缺省 "text"）。
    #[serde(default = "default_artifact_format")]
    pub format: String,
    /// 下载交付 media type（七期；旧记录缺省 "text/plain"）。
    #[serde(default = "default_artifact_media_type")]
    pub media_type: String,
    /// 下载交付文件名（七期；旧记录缺省为空）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file_name: String,
    /// 内容 SHA-256（七期；与 content_ref 的 CAS 哈希一致。旧记录缺省为空，
    /// 服务端可由 content_ref 回退解析）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// 内容字节数（七期；旧记录缺省 0，服务端可由 CAS 内容回退计算）。
    #[serde(default)]
    pub size_bytes: u64,
    /// 证据引用链（七期：Worker 证据来源列表；旧记录缺省为空）。
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    /// 未解决问题（七期：Worker 如实上报；旧记录缺省为空）。
    #[serde(default)]
    pub open_issues: Vec<String>,
    /// 格式校验结果（七期：登记前门控产物；legacy 记录为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ArtifactValidation>,
    /// Worker 交接说明原文（七期：WorkerOutputV1.handoff；旧记录为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

fn default_artifact_format() -> String {
    "text".to_string()
}

fn default_artifact_media_type() -> String {
    "text/plain".to_string()
}

/// Artifact 格式校验结果（七期 · 第三路：Artifact 校验与下载交付）。
///
/// 登记前门控：JSON 必须可解析且不得裹 Markdown 围栏；CSV 需表头 + 列数一致；
/// research 需至少一条有效证据（文件引用或 URL）；markdown 拒绝 TBD/空模板占位。
/// 校验未通过的产物**不登记**（不进版本链、不进 PendingReview）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactValidation {
    /// 被校验的格式（json|csv|research|markdown|text|…）。
    pub format: String,
    /// 是否通过。
    pub valid: bool,
    /// 失败原因（valid=true 时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
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
    /// 生产该产物的任务步骤（`s-{role}`；返工定位用。旧记录缺省为空）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub step_id: String,
    /// 生产者 member_id（冗余锚点：评审记录自足，不依赖产物表回查）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub producer_member_id: String,
    pub created_at: String,
}

/// 返工任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactReworkStatus {
    /// 已受理：步骤已重置并注入返工指令，等待重跑产出新版本。
    Requested,
    /// 新版本已登记（由版本链/评审闭环体现；任务记录保留供追溯）。
    Completed,
    /// 重跑失败（记录失败原因；可再次发起返工）。
    Failed,
}

/// Artifact 返工任务（V1 五期 · 第二路）。
///
/// 同一 `review_id` 至多一个返工任务（幂等）；重复请求返回原任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReworkTask {
    pub rework_id: String,
    /// 被返工的产物（vN）。
    pub artifact_id: String,
    pub artifact_version: u32,
    /// 触发返工的 request_changes 评审（幂等键）。
    pub review_id: String,
    pub team_id: String,
    pub project_id: String,
    /// 生产步骤（重置目标；下游未完成节点一并重置）。
    pub step_id: String,
    /// 评审意见驱动的返工指令（注入步骤输入 `rework.instruction`）。
    pub instruction: String,
    pub idempotency_key: String,
    #[serde(default = "default_rework_status")]
    pub status: ArtifactReworkStatus,
    /// 返工产生的新版本 artifact_id（完成后回填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reworked_artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub created_at: String,
}

fn default_rework_status() -> ArtifactReworkStatus {
    ArtifactReworkStatus::Requested
}

// ---------------------------------------------------------------------------
// ChangeSet（八期 · 二路）：可审查/可接受/可拒绝/可撤销的代码变更闭环
// ---------------------------------------------------------------------------

/// ChangeSet 文件哈希条目。
///
/// `base_hashes` 侧三态（恢复语义）：
///
/// - `(Some(hash), content_available=true)`：基线内容在 CAS，可自动恢复；
/// - `(None, content_available=true)`：执行前不存在（新建文件，恢复即删除）；
/// - `(_, content_available=false)`：基线不可用/未知（超限/读取失败/快照不完整），
///   恢复按冲突处理（409 + conflicted，绝不误删）。
///
/// `result_hashes` 侧恒为 `content_available=false`（结果内容以磁盘为准，恢复用基线）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeSetFileHash {
    /// 相对工作区根路径（`/` 分隔，与 git porcelain / 变更追踪口径一致）。
    pub path: String,
    /// 内容 SHA-256（十六进制）；`None` = 该状态下文件不存在。
    #[serde(default)]
    pub sha256: Option<String>,
    /// 基线内容是否可自动恢复（见结构体文档三态；结果侧恒 false）。
    #[serde(default = "default_content_available")]
    pub content_available: bool,
}

fn default_content_available() -> bool {
    true
}

/// ChangeSet 状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSetStatus {
    /// 待人工审查（写 Worker 成功落盘变更后自动进入）。
    PendingReview,
    /// 已接受：保留文件现状，该团队代码 Artifact 允许批准为最终 approved head。
    Accepted,
    /// 已拒绝：该 ChangeSet 修改的文件已恢复到基线。
    Rejected,
    /// 已撤销：同拒绝（语义为「撤销变更」，文件恢复到基线）。
    Reverted,
    /// 冲突：恢复前检测到用户改过文件（当前哈希 ≠ 结果哈希 ≠ 基线哈希），
    /// 不覆盖用户新内容；用户处理后可重试 accept/reject/revert。
    Conflicted,
}

/// ChangeSet 决定记录（幂等 + 审计锚点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeSetDecision {
    /// accept / reject / revert。
    pub action: String,
    /// 幂等键：同动作重放零副作用（返回现状，`replayed: true`）。
    pub idempotency_key: String,
    pub decided_at: String,
    /// 备注（可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// ChangeSet：一次写角色执行的「工作区变更集合」（八期 · 二路）。
///
/// 由服务端 `TrackedRoleWorker` 在写角色成功路径自动生成：
/// 执行前对允许路径做内容基线快照（进 CAS）→ 执行 → 窗口内 git 变更差集 →
/// ChangeSet 落盘（`<run_dir>/<team_id>-change-sets.json`）。reject/revert 只恢复
/// 该 ChangeSet 修改的文件；未接受（pending_review/conflicted）时该团队代码
/// Artifact 可评审但不能成为最终 approved head。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeSet {
    pub change_set_id: String,
    pub team_id: String,
    /// 产生该变更集合的步骤（`s-{role}`）。
    pub step_id: String,
    /// 产生该变更集合的角色。
    pub role: String,
    /// 执行前基线（仅本次变更涉及的文件；三态见 [`ChangeSetFileHash`]）。
    pub base_hashes: Vec<ChangeSetFileHash>,
    /// 执行后结果哈希（与 changed_files 一一对应）。
    pub result_hashes: Vec<ChangeSetFileHash>,
    /// 本次执行窗口内新增变更的文件（相对路径）。
    pub changed_files: Vec<String>,
    /// diff 补丁引用（`<team_id>-changes/<file>.patch`，相对 run_dir；可空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_ref: Option<String>,
    pub status: ChangeSetStatus,
    pub created_at: String,
    /// 决定记录（accept/reject/revert 各至多一条；幂等重放零副作用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<ChangeSetDecision>,
    /// 最近一次冲突文件清单（conflicted 时非空；落决定后清空）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
}

impl ChangeSet {
    /// 状态中文名（UI/审计展示）。
    pub fn status_label(&self) -> &'static str {
        match self.status {
            ChangeSetStatus::PendingReview => "待评审",
            ChangeSetStatus::Accepted => "已接受",
            ChangeSetStatus::Rejected => "已拒绝",
            ChangeSetStatus::Reverted => "已撤销",
            ChangeSetStatus::Conflicted => "冲突",
        }
    }
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
    /// Worker 交接说明原文（七期 · 第三路：WorkerOutputV1.handoff 逐字落盘，
    /// 不再依赖自动摘要/反解析；旧记录为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_note: Option<String>,
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
    /// Artifact 返工任务（按 review_id 幂等；随空间 JSON 持久化）。
    #[serde(default)]
    pub rework_tasks: Vec<ArtifactReworkTask>,
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
