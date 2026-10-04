use super::workers::*;
use super::write_lease::{manager_for_workspace, WriteLease, WriteScope};
use super::{project_workspace, workspace_change_tracker, workswarm_metrics};
use owo_agent_core::goal::{Worker, WorkerRegistry};
use owo_agent_core::worker_profile::{intersect_paths, WorkerProfile};
use owo_agent_core::workswarm::{RoleWorker, SteerCommand, TeamCoordinator};
use owo_agent_server::AppState;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;

async fn fail_pending_rework_tasks(coordinator: &TeamCoordinator, team_id: &str, reason: &str) {
    let Ok(team) = coordinator.get_team_run(team_id).await else {
        return;
    };
    let Some(project_id) = team.project_space_id.as_deref() else {
        return;
    };
    let Ok(mut space) = coordinator.store().get_project_space(project_id).await else {
        return;
    };
    let mut changed = false;
    for task in &mut space.rework_tasks {
        if task.team_id == team_id
            && matches!(
                task.status,
                owo_agent_protocol::ArtifactReworkStatus::Dispatching
                    | owo_agent_protocol::ArtifactReworkStatus::Requested
            )
        {
            task.status = owo_agent_protocol::ArtifactReworkStatus::Failed;
            task.error = reason.to_string();
            changed = true;
        }
    }
    if changed {
        space.version += 1;
        space.updated_at = chrono::Utc::now().to_rfc3339();
        if let Err(error) = coordinator.store().save_project_space(&space).await {
            tracing::error!(team_id = %team_id, %error, "返工终态回写失败");
        }
    }
}

/// 声明写范围与团队绑定无交集时的不可达写白名单哨兵（工具/审批层据此拒绝一切写入）。
const NO_WRITE_SCOPE_MARKER: &str = ".owo-no-write-scope";

/// 按角色 worker 名解析内层 worker（"agent"/缺省 = 模型驱动）。
///
/// 五期（第三路）：agent 角色接收 `model_calls` 计数器（MeasuredProvider 注入；
/// 仅指标用途，不影响执行行为）。
/// 六期（第二路）：`scope` 为团队工作区绑定（None = 全局工作区，行为不变）。
/// 七期（第二路）：agent 角色带角色画像（工具面/只读/回合上限）+ critic 代理 +
/// 团队取消桥标志 + 「角色 ∩ 绑定」写白名单交集（内置 echo/sleep/fail 不受影响）。
#[allow(clippy::too_many_arguments)]
pub(super) fn inner_worker_for(
    state: &AppState,
    worker_name: Option<&str>,
    model_calls: Option<&Arc<AtomicU64>>,
    request_usage: Option<&Arc<workswarm_metrics::RequestUsageCollector>>,
    team_request_budget: Option<&Arc<workswarm_metrics::TeamModelRequestBudget>>,
    scope: Option<&project_workspace::WorkspaceScope>,
    profile: &WorkerProfile,
    is_critic: bool,
    cancel_flag: &Arc<AtomicBool>,
    write_allowed: Vec<PathBuf>,
    coordinator: &Arc<TeamCoordinator>,
    team_id: &str,
    parent_session_id: Option<&str>,
    role: &str,
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
                request_usage: request_usage.cloned(),
                team_request_budget: team_request_budget.cloned(),
                workspace_scope,
                profile: Some(profile.clone()),
                is_critic,
                cancel_flag: Some(Arc::clone(cancel_flag)),
                write_allowed,
                coordinator: Arc::clone(coordinator),
                session_store: Arc::clone(&state.store),
                parent_session_id: parent_session_id.map(str::to_string),
                team_id: team_id.to_string(),
                role: role.to_string(),
            }))
        }
    }
}

fn worker_kind_uses_agent_provider(worker_kind: &str) -> bool {
    !matches!(worker_kind, "echo" | "sleep" | "fail")
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
/// - 范围写租约 + 变更追踪：写角色包 `TrackedRoleWorker`（未声明写范围 = 工作区级
///   全局互斥；声明 `write_paths` 且互不重叠 = 并发落盘；执行前后 git 快照 →
///   变更摘要/diff ref 落盘 → 白名单越界 `scope_violation`）；
/// - 取消桥：`cancel_flag` 由 run_team_loop 的令牌监听任务置位，Worker 协作中断。
pub(super) async fn build_run_registry(
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
    let team_run = coordinator.get_team_run(team_id).await.ok();
    let parent_session_id = team_run.as_ref().and_then(|run| {
        run.shared_context_refs.iter().find_map(|reference| {
            let hash = reference.strip_prefix("cas://sha256:")?;
            let snapshot = coordinator.cas().get_text(hash)?;
            serde_json::from_str::<serde_json::Value>(&snapshot)
                .ok()?
                .get("source_session_id")?
                .as_str()
                .map(str::to_string)
        })
    });
    let budgets: Vec<owo_agent_core::builtin_team_templates::RoleBudget> = team_run
        .as_ref()
        .and_then(|run| {
            run.template_id
                .as_deref()
                .and_then(owo_agent_core::builtin_team_templates::descriptor)
        })
        .map(|descriptor| descriptor.budget_calls_per_role)
        .unwrap_or_default();
    let journal = workswarm_metrics::MetricsJournal::for_team(coordinator.run_dir(), team_id);
    let team_request_budget = match team_run
        .as_ref()
        .and_then(|run| run.budget.get("max_model_calls"))
    {
        Some(value) => {
            let limit = value.as_u64()?;
            Some(Arc::new(
                workswarm_metrics::TeamModelRequestBudget::new(
                    limit,
                    workswarm_metrics::RequestReservationJournal::for_team(
                        coordinator.run_dir(),
                        team_id,
                    ),
                )
                .ok()?,
            ))
        }
        None => None,
    };
    // 范围写租约按实际工作区共享：跨调度阶段和不同 TeamRun 仍能互斥重叠写面。
    // 未声明范围的写角色全局互斥；声明写范围且互不重叠的写角色可并发落盘。
    let tracking_root = scope
        .as_ref()
        .map(|s| s.root.as_path())
        .unwrap_or(state.workspace.as_path());
    let write_lease_manager = manager_for_workspace(tracking_root);
    let usage_tracker = Arc::new(workswarm_metrics::UsageAttributionTracker::default());
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
        let model_calls = worker_kind_uses_agent_provider(&worker_kind)
            .then(|| Arc::new(AtomicU64::new(0)));
        let request_usage = worker_kind_uses_agent_provider(&worker_kind)
            .then(|| Arc::new(workswarm_metrics::RequestUsageCollector::default()));
        // 角色画像：显式 write_paths 是写能力声明；并行 TaskGraph writer 槽位
        // 以可写工具面启动，再由每个任务的 host-validated capability scope 收窄。
        let budget_calls = budgets
            .iter()
            .find(|budget| budget.role == r.role)
            .map(|budget| budget.budget_calls)
            .unwrap_or(0);
        let profile = WorkerProfile::for_team_role(
            &r.role,
            &r.capabilities,
            budget_calls,
            !r.write_paths.is_empty(),
            meta.parallel && owo_agent_core::worker_profile::is_parallel_writer_name(&r.role),
        );
        let profile = if meta.template_id.as_deref()
            == Some(owo_agent_core::builtin_team_templates::FULLSTACK_WEB_V1)
            && matches!(r.role.as_str(), "w1" | "w2")
        {
            profile.without_commands()
        } else {
            profile
        };
        let is_critic = r.is_reviewer();
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
        // 十一期（二路）：角色级写范围优先（RoleSpec.write_paths，相对工作区根）；
        // 未声明沿用画像白名单（当前内置画像恒为空 = 工作区级）。
        let role_allowed: Vec<PathBuf> = if r.write_paths.is_empty() {
            profile
                .write_allowed_paths
                .iter()
                .map(|relative| tracking_root.join(relative))
                .collect()
        } else {
            r.write_paths
                .iter()
                .map(|relative| tracking_root.join(relative))
                .collect()
        };
        // 角色 ∩ 绑定：两侧都非空且无交集 = 该角色在此绑定下不可写任何文件
        // （哨兵路径保证工具/审批/租约三层一致拒绝；空白的「未约束」语义不被复用）。
        let write_allowed = if role_allowed.is_empty() {
            scope_allowed.clone()
        } else if scope_allowed.is_empty() {
            role_allowed
        } else {
            let intersection = intersect_paths(&role_allowed, &scope_allowed);
            if intersection.is_empty() {
                vec![tracking_root.join(NO_WRITE_SCOPE_MARKER)]
            } else {
                intersection
            }
        };
        let lease_waits =
            is_writer.then(|| Arc::new(workswarm_metrics::LeaseWaitTracker::default()));
        let lease = is_writer.then(|| {
            // 空写面 = 工作区级（全局互斥）；声明写面 = 范围租约（不重叠可并发）。
            let scope = if write_allowed.is_empty() {
                WriteScope::global()
            } else {
                WriteScope::from_paths(&write_allowed)
            };
            WriteLease::new(Arc::clone(&write_lease_manager), scope)
        });
        let inner = inner_worker_for(
            state,
            r.worker.as_deref(),
            model_calls.as_ref(),
            request_usage.as_ref(),
            team_request_budget.as_ref(),
            scope.as_ref(),
            &profile,
            is_critic,
            cancel_flag,
            write_allowed.clone(),
            coordinator,
            team_id,
            parent_session_id.as_deref(),
            &r.role,
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
            lease,
            lease_waits: lease_waits.clone(),
            tracking,
        });
        let role_worker = Arc::new(RoleWorker::new_with_capabilities(
            Arc::clone(coordinator),
            team_id.to_string(),
            member_id.clone(),
            r.role.clone(),
            r.capabilities.clone(),
            inner,
        ));
        let provider = worker_kind_uses_agent_provider(&worker_kind).then(|| state.agent.provider());
        registry.register(Arc::new(
            workswarm_metrics::MeasuredRoleWorker::new_with_usage_tracker(
                role_worker,
                Arc::clone(coordinator),
                journal.clone(),
                team_id.to_string(),
                member_id,
                r.role.clone(),
                worker_kind,
                provider,
                model_calls,
                Arc::clone(&usage_tracker),
                request_usage,
                lease_waits,
            ),
        ));
    }
    Some(registry)
}

// ---------------------------------------------------------------------------
// 后台运行循环（阶段驱动 + 人节点门闩）
// ---------------------------------------------------------------------------

/// 运行循环存活守卫（R2）：进程内声明「该团队有活动运行循环」——
/// 中断识别据此区分「循环存活（含批次间隙/人节点门闩）」与「重启遗留」。
/// Drop 兜底撤销（覆盖所有 return 路径与 panic 展开栈）。
pub(super) struct LoopAliveGuard {
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
pub(super) async fn stop_if_budget_exhausted(
    coordinator: &Arc<TeamCoordinator>,
    team_id: &str,
) -> bool {
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
            fail_pending_rework_tasks(
                coordinator,
                team_id,
                "团队预算耗尽，返工未产出新版本",
            )
            .await;
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
    let verification_root = project_workspace::load_binding(coordinator.run_dir(), &team_id)
        .map(|binding| binding.scope().root)
        .unwrap_or_else(|| state.workspace.clone());
    if let Err(error) = coordinator.bind_verification_workspace(&team_id, &verification_root) {
        tracing::warn!(team_id = %team_id, %error, "workswarm 验证工作区未绑定；WorkspacePaths 要求将在 DeliveryGate 中失败关闭");
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
            fail_pending_rework_tasks(&coordinator, &team_id, "团队运行器初始化失败，返工未产出新版本").await;
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
                    let _delivery_lease =
                        super::acquire_workspace_delivery_lease(&verification_root).await;
                    if let Err(e) = coordinator.finalize_success(&team_id).await {
                        tracing::error!(team_id = %team_id, %e, "workswarm 收尾失败");
                    }
                    return;
                }
                owo_agent_core::PhaseOutcome::Failed
                | owo_agent_core::PhaseOutcome::Aborted
                | owo_agent_core::PhaseOutcome::Finished => {
                    fail_pending_rework_tasks(
                        &coordinator,
                        &team_id,
                        "团队执行在返工产出新版本前终止",
                    )
                    .await;
                    return;
                }
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
                            fail_pending_rework_tasks(
                                &coordinator,
                                &team_id,
                                "团队已取消，返工未产出新版本",
                            )
                            .await;
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
                                let _delivery_lease =
                                    super::acquire_workspace_delivery_lease(&verification_root)
                                        .await;
                                let _ = coordinator.finalize_success(&team_id).await;
                                return;
                            }
                            Ok(owo_agent_core::PhaseOutcome::MoreReady) => break, // 外层循环重建注册表继续
                            Ok(_) => {
                                fail_pending_rework_tasks(
                        &coordinator,
                        &team_id,
                        "团队执行在返工产出新版本前终止",
                    )
                    .await;
                                return;
                            } // 终态
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


#[cfg(test)]
mod worker_kind_budget_tests {
    use super::worker_kind_uses_agent_provider;

    #[test]
    fn every_fallback_agent_worker_is_metered_and_budgeted() {
        assert!(worker_kind_uses_agent_provider("agent"));
        assert!(worker_kind_uses_agent_provider("custom-agent"));
        assert!(!worker_kind_uses_agent_provider("echo"));
        assert!(!worker_kind_uses_agent_provider("sleep"));
        assert!(!worker_kind_uses_agent_provider("fail"));
    }
}
