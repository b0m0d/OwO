use owo_agent_core::change_set_store::{ChangeSetStatus, ChangeSetStore};
use owo_agent_core::StepStatus;
use owo_agent_protocol::{ReviewState, RuntimeBinding, TeamRun, TeamRunStatus};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use owo_agent_server::AppState;

use super::super::human_inbox_store::{
    is_valid_kind, open_shared, InboxItemDraft, KIND_ARTIFACT_REVIEW, KIND_CHANGE_SET,
    KIND_HUMAN_RESULT, KIND_STEP_RETRY,
};
use super::{MAX_SCAN_PROJECTS, MAX_SCAN_TEAMS};
pub(super) fn inbox_store(
    state: &AppState,
) -> Arc<super::super::human_inbox_store::HumanInboxStore> {
    // 与 space.db 同目录：data_root/workswarm/human-inbox.json。
    let path = state.data_root.join("workswarm").join("human-inbox.json");
    open_shared(&path)
}

// ---------------------------------------------------------------------------
// live 扫描（领域对象仍由各自存储持有；此处只产出草稿 + 详情）
// ---------------------------------------------------------------------------

/// 任务视图元素（与 workswarm_api::task_view 同口径的最小子集）。
pub(super) struct TaskView<'a> {
    step_id: &'a str,
    worker: &'a str,
    role: &'a str,
    status: StepStatus,
    attempts: u32,
    error: Option<&'a str>,
}

pub(super) fn task_views<'a>(state: &'a owo_agent_core::GoalRunState) -> Vec<TaskView<'a>> {
    state
        .plan
        .steps
        .iter()
        .map(|s| {
            let record = state.records.get(&s.id);
            TaskView {
                step_id: &s.id,
                worker: &s.worker,
                role: s.worker.strip_prefix("m-").unwrap_or(&s.worker),
                status: record.map(|r| r.status).unwrap_or(StepStatus::Pending),
                attempts: record.map(|r| r.attempts).unwrap_or(0),
                error: record.and_then(|r| r.error.as_deref()),
            }
        })
        .collect()
}

/// 团队扫描：人节点待录入（human_result）+ 失败步骤待重试（step_retry）。
/// 九期：团队清单由 [`collect_drafts`] 统一拉取传入；occurrence = 当前 attempts。
pub(super) async fn scan_team_drafts(
    coordinator: &Arc<owo_agent_core::TeamCoordinator>,
    runs: &[TeamRun],
    drafts: &mut Vec<(InboxItemDraft, Value)>,
) {
    for team in runs.iter().take(MAX_SCAN_TEAMS) {
        let team_id = team.team_id.clone();
        let status: TeamRunStatus = team.status;
        // 重试类：failed 团队保留（正是 retry 的合法场景）；succeeded/cancelled 排除。
        let retry_eligible = !matches!(status, TeamRunStatus::Succeeded | TeamRunStatus::Cancelled);
        // 人节点：任何非终态团队都可能存在未完成人节点步骤。
        if status.is_terminal() && !retry_eligible {
            continue;
        }
        let run_state = match coordinator.load_run_state(&team_id) {
            Ok(s) => s,
            Err(_) => continue, // 状态缺失/损坏 → 跳过该团队（详情页有明确报错）
        };
        // 人成员集合（member_id → role）。
        let mut human_roles: BTreeMap<&str, &str> = BTreeMap::new();
        for member in &team.members {
            if matches!(member.runtime_binding, RuntimeBinding::Human { .. }) {
                human_roles.insert(member.member_id.as_str(), member.role.as_str());
            }
        }
        let interrupted = coordinator.is_interrupted(&team_id);
        for t in task_views(&run_state) {
            let is_human_step = human_roles.contains_key(t.worker);
            if is_human_step
                && !matches!(
                    t.status,
                    StepStatus::Succeeded | StepStatus::Failed | StepStatus::Aborted
                )
            {
                drafts.push((
                    InboxItemDraft {
                        kind: KIND_HUMAN_RESULT.to_string(),
                        team_id: team_id.clone(),
                        project_id: team.project_space_id.clone(),
                        target_id: t.step_id.to_string(),
                        occurrence: t.attempts.to_string(),
                        summary: format!(
                            "人节点结果待录入：{}（状态 {}）",
                            t.role,
                            format!("{:?}", t.status).to_lowercase()
                        ),
                    },
                    json!({
                        "step_id": t.step_id,
                        "role": t.role,
                        "step_status": format!("{:?}", t.status),
                        "team_status": format!("{status:?}"),
                    }),
                ));
            }
            if retry_eligible && matches!(t.status, StepStatus::Failed | StepStatus::Aborted) {
                drafts.push((
                    InboxItemDraft {
                        kind: KIND_STEP_RETRY.to_string(),
                        team_id: team_id.clone(),
                        project_id: team.project_space_id.clone(),
                        target_id: t.step_id.to_string(),
                        occurrence: t.attempts.to_string(),
                        summary: format!(
                            "失败步骤待重试：{}（第 {} 次尝试{}）",
                            t.role,
                            t.attempts,
                            if interrupted {
                                "，团队已中断"
                            } else {
                                ""
                            }
                        ),
                    },
                    json!({
                        "step_id": t.step_id,
                        "role": t.role,
                        "attempts": t.attempts,
                        "error": t.error,
                        "team_status": format!("{status:?}"),
                        "interrupted": interrupted,
                    }),
                ));
            }
        }
    }
}

/// 项目扫描：PendingReview 产物（artifact_review）。occurrence 恒 "1"
/// （artifact_id 全局唯一且含 team 前缀与版本号）。
pub(super) async fn scan_review_drafts(
    coordinator: &Arc<owo_agent_core::TeamCoordinator>,
    runs: &[TeamRun],
    drafts: &mut Vec<(InboxItemDraft, Value)>,
) {
    let mut project_to_team: BTreeMap<&str, &str> = BTreeMap::new();
    let mut projects: Vec<&str> = Vec::new();
    for team in runs {
        if let Some(pid) = team.project_space_id.as_deref() {
            if !projects.contains(&pid) {
                projects.push(pid);
                project_to_team.insert(pid, team.team_id.as_str());
            }
        }
    }
    for pid in projects.into_iter().take(MAX_SCAN_PROJECTS) {
        // 产物元数据直接走 store（space.artifacts 是 id 列表；此处需 review_state 等字段）。
        let artifacts = match coordinator.store().list_artifacts_by_project(pid).await {
            Ok(a) => a,
            Err(_) => continue,
        };
        for artifact in &artifacts {
            if artifact.review_state != ReviewState::PendingReview {
                continue;
            }
            let team_id = if artifact.team_id.is_empty() {
                project_to_team
                    .get(pid)
                    .map(|s| (*s).to_string())
                    .unwrap_or_default()
            } else {
                artifact.team_id.clone()
            };
            drafts.push((
                InboxItemDraft {
                    kind: KIND_ARTIFACT_REVIEW.to_string(),
                    team_id,
                    project_id: Some(pid.to_string()),
                    target_id: artifact.artifact_id.clone(),
                    occurrence: "1".to_string(),
                    summary: format!("待评审：{} v{}（{}）", artifact.kind, artifact.version, pid),
                },
                json!({
                    "artifact_id": artifact.artifact_id,
                    "version": artifact.version,
                    "kind": artifact.kind,
                    "format": artifact.format,
                    "review_state": format!("{:?}", artifact.review_state),
                }),
            ));
        }
    }
}

/// ChangeSet 扫描（九期 · 二路拆分）：直读 `ChangeSetStore::list_all()` 跨团队
/// 全量，与团队运行状态扫描完全解耦——不受 TeamRun succeeded/cancelled、
/// 24 团队扫描上限、run state 加载失败、团队非运行态影响；只要有未处理
/// ChangeSet（pending_review/conflicted）就出现在「待我处理」。occurrence 恒 "1"
/// （change_set_id 全局唯一，含 team/步骤/毫秒；conflicted 仅由未决定 ChangeSet
/// 进入，故同一 ChangeSet 恒对应同一待办）。
pub(super) fn scan_change_set_drafts(
    coordinator: &Arc<owo_agent_core::TeamCoordinator>,
    project_by_team: &BTreeMap<String, Option<String>>,
    drafts: &mut Vec<(InboxItemDraft, Value)>,
) {
    let cs_store = ChangeSetStore::new(coordinator.run_dir());
    let Ok(records) = cs_store.list_all() else {
        return; // 变更存储不可用 → 该来源降级为空（其余来源不受影响）
    };
    for cs in records.iter().filter(|c| {
        matches!(
            c.status,
            ChangeSetStatus::PendingReview | ChangeSetStatus::Conflicted
        )
    }) {
        let summary = if cs.status == ChangeSetStatus::Conflicted {
            format!(
                "ChangeSet 存在冲突：{}（步骤 {}，{} 个文件；处理后可重试接受/拒绝）",
                cs.role,
                cs.step_id,
                cs.changed_files.len()
            )
        } else {
            format!(
                "ChangeSet 待审批：{}（步骤 {}，{} 个文件）",
                cs.role,
                cs.step_id,
                cs.changed_files.len()
            )
        };
        drafts.push((
            InboxItemDraft {
                kind: KIND_CHANGE_SET.to_string(),
                team_id: cs.team_id.clone(),
                project_id: project_by_team.get(&cs.team_id).cloned().flatten(),
                target_id: cs.change_set_id.clone(),
                occurrence: "1".to_string(),
                summary,
            },
            json!({
                "change_set_id": cs.change_set_id,
                "step_id": cs.step_id,
                "role": cs.role,
                "changed_files": cs.changed_files,
                "diff_ref": cs.diff_ref,
                "status": format!("{:?}", cs.status),
                "conflicts": cs.conflicts,
            }),
        ));
    }
}

/// 全量扫描：四类来源合一（团队任务/失败步骤 + ChangeSet + 产物评审）。
/// 团队清单只拉取一次（best-effort）：列表失败 → 团队/产物扫描降级为空，
/// ChangeSet 扫描仍独立工作（九期拆分的核心目标）。
pub(super) async fn collect_drafts(state: &AppState) -> Vec<(InboxItemDraft, Value)> {
    let mut drafts: Vec<(InboxItemDraft, Value)> = Vec::new();
    if let Ok(coordinator) = state.workswarm.coordinator() {
        let runs = coordinator.list_team_runs().await.unwrap_or_default();
        let project_by_team: BTreeMap<String, Option<String>> = runs
            .iter()
            .map(|t| (t.team_id.clone(), t.project_space_id.clone()))
            .collect();
        scan_team_drafts(&coordinator, &runs, &mut drafts).await;
        scan_review_drafts(&coordinator, &runs, &mut drafts).await;
        scan_change_set_drafts(&coordinator, &project_by_team, &mut drafts);
    }
    drafts
}

/// 扫描并把全部 live 候选登记进覆盖层（list/claim/resolve/release 共用前置）。
/// 单项操作前也执行全量登记：保证客户端未先 GET 列表时 claim/resolve 仍可达。
pub(super) async fn ensure_all_items(
    state: &AppState,
) -> Arc<super::super::human_inbox_store::HumanInboxStore> {
    let store = inbox_store(state);
    for (draft, _) in collect_drafts(state).await {
        if is_valid_kind(&draft.kind) {
            store.ensure_item(&draft);
        }
    }
    store
}

// ---------------------------------------------------------------------------
// handlers
// ---------------------------------------------------------------------------
