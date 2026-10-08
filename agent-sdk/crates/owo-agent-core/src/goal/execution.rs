//! 单步骤执行、执行目标路由、租约/取消收尾与步骤级验证。
//!
//! Goal DAG 调度器只负责派发与结果合并；具体 Worker 执行语义集中在本模块。

use super::{
    GoalBudget, HostCommandValidationV1, Worker, WorkerRegistry, WorkspaceCommandVerifier,
    CANCELLATION_CLEANUP_GRACE,
};
use crate::blackboard::Blackboard;
use crate::capability::{CapabilityWorkerRegistry, RouteDecision, WorkerRequirement};
use crate::critic::{review_loop, CriticConfig};
use crate::execution_target::{
    dispatch_disposition, select_binding, DispatchCancelRegistry, DispatchChannel,
    DispatchDisposition, FleetDispatchWorker, FleetProbe, LocalProcessProbe, TargetAvailability,
    WorkerBinding,
};
use crate::plan::StepSpec;
use crate::worker_pool::{PoolWorker, WorkerPool};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------- 并发执行辅助 ----------

/// Per-run admission ledger for worker executions. Reservations happen synchronously and
/// release the lock before any await, so parallel steps cannot oversubscribe global budgets.
pub(super) struct ExecutionBudget {
    usage: Mutex<(u32, u32)>,
    max_steps: u32,
    max_total_retries: u32,
}

impl ExecutionBudget {
    pub(super) fn new(steps_taken: u32, total_retries: u32, budget: GoalBudget) -> Self {
        Self {
            usage: Mutex::new((steps_taken, total_retries)),
            max_steps: budget.max_steps,
            max_total_retries: budget.max_total_retries,
        }
    }

    fn reserve_attempt(&self, is_retry: bool) -> Result<(), String> {
        let mut usage = self
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if usage.0 >= self.max_steps {
            return Err(format!(
                "预算熔断：步骤数已达上限 {}（steps_taken={}）",
                self.max_steps, usage.0
            ));
        }
        if is_retry && usage.1 >= self.max_total_retries {
            return Err(format!(
                "预算熔断：全局重试次数已达上限 {}（total_retries={}）",
                self.max_total_retries, usage.1
            ));
        }
        usage.0 = usage.0.saturating_add(1);
        if is_retry {
            usage.1 = usage.1.saturating_add(1);
        }
        Ok(())
    }
}

pub(super) enum StepExecutionError {
    Budget(String),
    Failed(String),
}

/// 步骤执行结果。
pub(super) enum StepResult {
    Ok {
        output: String,
        attempt_id: String,
        validation_receipts: Vec<crate::plan::ValidationReceiptV1>,
        epoch: u64,
    },
    FailedWithReceipts {
        error: String,
        attempt_id: String,
        validation_receipts: Vec<crate::plan::ValidationReceiptV1>,
        epoch: u64,
    },
    Retried {
        error: String,
    },
    Budget {
        reason: String,
    },
    /// 确定性失败：不重试、不参与 replan，原因直达目标终态。
    /// A2 语义：显式目标被拒绝（Reject）或需用户确认（AskUser）——
    /// 绝不允许静默改派，故直接以原因终止而非消耗重试预算。
    Fatal {
        reason: String,
    },
}

/// wave 内提前终止原因：Budget 沿用既有「预算熔断」终态表述；
/// Fatal 直接以原因作为目标终态错误。
#[derive(Debug)]
pub(super) enum StepStop {
    Budget(String),
    Fatal(String),
}

/// 单个步骤的并发执行产出（含尝试次数，供预算合并）。
pub(super) struct StepOutcome {
    pub(super) step_id: String,
    pub(super) attempts: u32,
    pub(super) result: StepResult,
}

/// 单步运行环境（预算 / abort / 可选 critic / 黑板 / worker pool / 能力路由 / 传输 / 租约，随步骤任务克隆）。
/// A2：另携带显式执行绑定（定向派发）与在飞远端任务取消登记表。
#[derive(Clone)]
pub(super) struct StepRuntime {
    pub(super) execution_budget: Arc<ExecutionBudget>,
    pub(super) aborted: Arc<std::sync::atomic::AtomicBool>,
    pub(super) critic: Option<CriticConfig>,
    pub(super) blackboard: Option<Blackboard>,
    pub(super) bb_writer: Option<String>,
    pub(super) use_worker_pool: bool,
    pub(super) worker_pool: Option<WorkerPool>,
    pub(super) capabilities: Option<CapabilityWorkerRegistry>,
    pub(super) capability_requirement: Option<WorkerRequirement>,
    pub(super) transport: Option<std::sync::Arc<dyn crate::fleet_transport::FleetTransport>>,
    pub(super) leases: Option<crate::lease::LeaseManager>,
    /// 显式执行绑定（空 = 未配置，走旧解析链）。
    pub(super) bindings: Vec<WorkerBinding>,
    /// 运行 id（默认 correlation 兜底 `{run_id}:{step_id}`）。
    pub(super) run_id: String,
    /// 在飞远端派发任务登记表（abort 收尾统一 cancel）。
    pub(super) cancels: DispatchCancelRegistry,
    pub(super) workspace_verification_root: Option<std::path::PathBuf>,
    pub(super) workspace_command_verifier: Option<WorkspaceCommandVerifier>,
    pub(super) attempt_admission: Option<super::AttemptAdmissionSender>,
}

impl StepRuntime {
    pub(super) async fn admit_attempt(
        &self,
        step_id: &str,
        attempt_id: &str,
        is_retry: bool,
    ) -> Result<(), String> {
        self.execution_budget.reserve_attempt(is_retry)?;
        if let Some(admission) = &self.attempt_admission {
            admission
                .admit(step_id.to_string(), attempt_id.to_string(), is_retry)
                .await?;
        }
        Ok(())
    }
}

/// 显式绑定解析结果（A2 定向派发与旧解析链的分界）。
enum WorkerResolution {
    /// 命中显式绑定并装配完成（含生效绑定视图，供 `_dispatch` 注入与预算封顶）。
    Directed(Arc<dyn Worker>, WorkerBinding),
    /// 未配置绑定时走旧解析链命中（行为完全兼容）。
    Legacy(Arc<dyn Worker>),
    /// 绑定命中但目标不可用：等待 / 询问 / 拒绝（绝不改派其他目标）。
    Disposition(DispatchDisposition),
    /// 旧解析链未命中（保持既有可重试错误语义与文案）。
    Unresolved(String),
}

/// A2 统一派发解析：
/// 1) 先解析显式绑定（step id 匹配 > worker 名匹配）；命中即严格定向，
///    目标探测不足时产出 Wait/AskUser/Reject 裁定，不进入任何回退路径；
/// 2) 未配置绑定时保持旧行为兼容（registry → 池 → 能力路由 → transport）。
async fn resolve_execution(
    workers: &WorkerRegistry,
    step: &StepSpec,
    rt: &StepRuntime,
) -> WorkerResolution {
    if let Some(mut binding_view) = select_binding(&rt.bindings, &step.id, &step.worker).cloned() {
        // 按 step id 命中的绑定：执行者引用取 plan 步骤声明的 worker 名，
        // target/能力/权限/预算/关联信息仍来自绑定本身。
        if binding_view.worker == step.id {
            binding_view.worker = step.worker.clone();
        }
        let availability = probe_target_availability(workers, step, rt).await;
        return match dispatch_disposition(&binding_view, &availability) {
            ready @ DispatchDisposition::Ready(_) => WorkerResolution::Directed(
                build_channel_worker(workers, rt, step, ready, &binding_view),
                binding_view,
            ),
            disposition => WorkerResolution::Disposition(disposition),
        };
    }
    match legacy_resolve_worker(workers, step, rt).await {
        Ok(Some(worker)) => WorkerResolution::Legacy(worker),
        Ok(None) => WorkerResolution::Unresolved(format!("worker 未注册：{}", step.worker)),
        Err(reason) => WorkerResolution::Unresolved(reason),
    }
}

/// 三类目标的可用性探测（无远程接口细节；远端仅判断是否配置了传输）。
async fn probe_target_availability(
    workers: &WorkerRegistry,
    step: &StepSpec,
    rt: &StepRuntime,
) -> TargetAvailability {
    TargetAvailability {
        in_process_ready: workers.get(&step.worker).is_some(),
        local_process: match (&rt.use_worker_pool, &rt.worker_pool) {
            (false, _) => LocalProcessProbe::NotConfigured,
            (true, None) => LocalProcessProbe::NotConfigured,
            (true, Some(pool)) => {
                if pool.contains(&step.worker).await {
                    LocalProcessProbe::Ready
                } else {
                    LocalProcessProbe::WorkerMissing
                }
            }
        },
        fleet: match &rt.transport {
            None => FleetProbe::NotConfigured,
            Some(_) => FleetProbe::Ready,
        },
    }
}

/// 把 Ready 裁定装配为可执行的通道 worker。
fn build_channel_worker(
    workers: &WorkerRegistry,
    rt: &StepRuntime,
    step: &StepSpec,
    ready: DispatchDisposition,
    binding: &WorkerBinding,
) -> Arc<dyn Worker> {
    let resolved = match ready {
        DispatchDisposition::Ready(resolved) => resolved,
        other => unreachable!("非 Ready 裁定不应装配通道：{other:?}"),
    };
    let correlation_fallback = format!("{}:{}", rt.run_id, step.id);
    match resolved.channel {
        DispatchChannel::Registry(name) => {
            // 探测已确认命中；unwrap_or_else 仅防御编程错误。
            workers
                .get(&name)
                .unwrap_or_else(|| panic!("定向派发内部错误：registry 探测通过但未找到 {name}"))
        }
        DispatchChannel::Pool(id) => {
            let pool = rt
                .worker_pool
                .as_ref()
                .unwrap_or_else(|| panic!("定向派发内部错误：池探测通过但 WorkerPool 缺失"));
            Arc::new(PoolWorker::new(pool.clone(), id))
        }
        DispatchChannel::Fleet { node_id, .. } => {
            let transport =
                rt.transport.as_ref().map(Arc::clone).unwrap_or_else(|| {
                    panic!("定向派发内部错误：传输探测通过但 FleetTransport 缺失")
                });
            Arc::new(FleetDispatchWorker::from_binding(
                resolved.binding.worker.clone(),
                node_id,
                binding,
                resolved
                    .binding
                    .effective_correlation_id(&correlation_fallback),
                transport,
                rt.cancels.clone(),
            ))
        }
    }
}

/// 旧解析链（保留原文案与顺序）：registry 优先（进程内语义）；feature flag 开启时
/// 回退到 worker pool 子进程；未命中且配置传输时经 transport 提交（跨机铺路）；
/// 步骤显式声明 `_cap` 时按能力路由选 worker。仅在**未配置**显式绑定时启用。
async fn legacy_resolve_worker(
    workers: &WorkerRegistry,
    step: &StepSpec,
    rt: &StepRuntime,
) -> Result<Option<Arc<dyn Worker>>, String> {
    let name = &step.worker;
    if let Some(worker) = workers.get(name) {
        return Ok(Some(worker));
    }
    if rt.use_worker_pool {
        if let Some(pool) = &rt.worker_pool {
            if pool.contains(name).await {
                return Ok(Some(Arc::new(PoolWorker::new(
                    pool.clone(),
                    name.to_string(),
                ))));
            }
        }
    }
    // 能力路由（仅当步骤显式声明能力需求 `_cap`，或 runner 配置了全局需求基线时启用；
    // 此时 worker 名仅为提示，按能力匹配选择执行者）。
    let requirement = step_requirement(step, rt);
    if let Some(req) = requirement {
        if let Some(reg) = &rt.capabilities {
            match reg.route(&req) {
                RouteDecision::Pick(id) => {
                    if let Some(pool) = &rt.worker_pool {
                        if pool.contains(&id).await {
                            return Ok(Some(Arc::new(PoolWorker::new(pool.clone(), id))));
                        }
                    }
                    if let Some(transport) = &rt.transport {
                        return Ok(Some(Arc::new(
                            crate::fleet_transport::TransportWorker::new(transport.clone(), id),
                        )));
                    }
                    return Err(format!("能力路由选中 worker {id}，但池/传输均未注册"));
                }
                RouteDecision::Degrade { worker, missing } => {
                    tracing::warn!(
                        worker = %worker,
                        missing = ?missing,
                        "能力路由降级：缺失 {}",
                        missing.join(", ")
                    );
                    if let Some(pool) = &rt.worker_pool {
                        if pool.contains(&worker).await {
                            return Ok(Some(Arc::new(PoolWorker::new(pool.clone(), worker))));
                        }
                    }
                    if let Some(transport) = &rt.transport {
                        return Ok(Some(Arc::new(
                            crate::fleet_transport::TransportWorker::new(transport.clone(), worker),
                        )));
                    }
                    return Err(format!(
                        "能力路由降级选中 worker {worker}，但池/传输均未注册（缺失：{}）",
                        missing.join(", ")
                    ));
                }
                RouteDecision::Reject { reasons } => {
                    return Err(format!("能力不满足，无可用 worker：{}", reasons.join("；")));
                }
            }
        }
    }
    // 未注册 worker 名且配置了传输：经 transport 提交（失败/恢复沿用总线持久化）。
    if let Some(transport) = &rt.transport {
        return Ok(Some(Arc::new(
            crate::fleet_transport::TransportWorker::new(transport.clone(), name.to_string()),
        )));
    }
    Ok(None)
}

/// 步骤能力需求：步骤 `_cap` 显式声明优先；否则用 runner 全局基线；默认空需求视为未启用。
fn step_requirement(step: &StepSpec, rt: &StepRuntime) -> Option<WorkerRequirement> {
    if let Some(cap) = step.input.get("_cap") {
        if let Ok(req) = serde_json::from_value::<WorkerRequirement>(cap.clone()) {
            return Some(req);
        }
    }
    rt.capability_requirement
        .clone()
        .filter(|r| *r != WorkerRequirement::default())
}

/// 步骤租约 RAII：步骤结束（成功/失败/预算/abort/取消）自动释放租约，
/// 防 `goal:<step_id>` 租约表泄漏（孤儿持有者）。token 匹配才释放。
struct StepLeaseGuard {
    leases: Option<crate::lease::LeaseManager>,
    holder: String,
    token: Option<String>,
}

impl Drop for StepLeaseGuard {
    fn drop(&mut self) {
        if let (Some(leases), Some(token)) = (&self.leases, &self.token) {
            let _ = leases.release(&self.holder, token);
        }
    }
}

/// 定向裁定 → 统一步骤结果映射：
/// Wait 按可重试错误参与既有重试/失败语义；AskUser 与 Reject 为确定性失败
/// （不消耗重试预算、不参与 replan，原因直达目标终态）。
fn disposition_outcome(step: &StepSpec, disposition: DispatchDisposition) -> StepOutcome {
    let result = match disposition {
        DispatchDisposition::Ready(_) => unreachable!("Ready 裁定不会进入失败映射"),
        DispatchDisposition::Wait { reason } => StepResult::Retried {
            error: format!("目标暂不可用（wait）：{reason}"),
        },
        DispatchDisposition::AskUser { prompt } => StepResult::Fatal {
            reason: format!("需用户确认：{prompt}"),
        },
        DispatchDisposition::Reject { reason } => StepResult::Fatal {
            reason: format!("目标拒绝：{reason}"),
        },
    };
    StepOutcome {
        step_id: step.id.clone(),
        attempts: 0,
        result,
    }
}

/// 独立执行一个步骤的尝试循环（worker 调用 + 验证断言 + 可选 critic/黑板；不触碰 runner 状态）。
/// Worker 与 critic 作者调用前共享 ExecutionBudget 做并发准入，避免本进程的任务并行越额；
/// 预留目前是运行内存账本，持久 Goal/Team 计数仍在结果/进度合并时更新，不能视为崩溃安全的 attempt WAL。
///
/// 步骤 input 可选约定（多 Agent P0 编排原语，`_` 前缀键）：
/// - `"_critic": { "rounds": N }`：输出经只读 critic 评审，意见回流 worker 重跑，最多 N 轮。
/// - `"_bb": { "read": ["key"], "write": "key" }`：读取黑板中间结果（`{{bb:key}}` 占位符替换）与写回。
pub(super) async fn run_step_attempts(
    workers: WorkerRegistry,
    step: StepSpec,
    rt: StepRuntime,
    attempt_id: String,
    epoch: u64,
) -> StepOutcome {
    // A2：显式绑定优先（严格定向），未配置时走旧解析链。
    let mut directed_binding: Option<WorkerBinding> = None;
    let worker = match resolve_execution(&workers, &step, &rt).await {
        WorkerResolution::Directed(worker, binding) => {
            directed_binding = Some(binding);
            worker
        }
        WorkerResolution::Legacy(worker) => worker,
        WorkerResolution::Unresolved(error) => {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts: 0,
                result: StepResult::Retried { error },
            }
        }
        WorkerResolution::Disposition(disposition) => {
            return disposition_outcome(&step, disposition);
        }
    };
    let prepared_input = match prepare_step_input(&step.input, &rt.blackboard).await {
        Ok(input) => input,
        Err(e) => {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts: 0,
                result: StepResult::Retried { error: e },
            }
        }
    };
    // 定向绑定把派发上下文注入 `_dispatch`（correlation / CAS 引用 / node 对下游可见）。
    let input = match &directed_binding {
        Some(binding) => {
            let correlation =
                binding.effective_correlation_id(&format!("{}:{}", rt.run_id, step.id));
            let mut injected = prepared_input;
            binding.inject_dispatch_context(&mut injected, &correlation);
            injected
        }
        None => prepared_input,
    };
    // 租约：步骤任务持有（fencing 语义；写结果前校验 epoch/token，防分区双写）。
    // RAII guard：任何返回路径自动 release（防租约表孤儿持有者泄漏）。
    let step_lease = match &rt.leases {
        Some(leases) => {
            let holder = format!("goal:{}", step.id);
            match leases.acquire(&holder) {
                Ok(lease) => Some(lease),
                Err(e) => {
                    return StepOutcome {
                        step_id: step.id.clone(),
                        attempts: 0,
                        result: StepResult::Retried {
                            error: format!("步骤租约获取失败：{e}"),
                        },
                    }
                }
            }
        }
        None => None,
    };
    let _step_lease_guard = step_lease.as_ref().map(|lease| StepLeaseGuard {
        leases: rt.leases.clone(),
        holder: lease.holder.clone(),
        token: Some(lease.token.clone()),
    });
    let critic_rounds = step
        .input
        .get("_critic")
        .and_then(|v| v.get("rounds"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    // 尝试上限：绑定预算 > 0 时对 plan 声明的 retries+1 取 min；时长预算 0 = 不限。
    let base_max_attempts = step.retries.saturating_add(1);
    let max_attempts = directed_binding
        .as_ref()
        .map(|b| b.cap_attempts(base_max_attempts))
        .unwrap_or(base_max_attempts);
    let duration_cap_secs = directed_binding
        .as_ref()
        .map(|b| b.budget.max_duration_secs)
        .unwrap_or(0);
    let started_at = std::time::Instant::now();
    let mut attempts = 0u32;
    let mut validation_receipts = Vec::new();
    while attempts < max_attempts {
        if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result: StepResult::Retried {
                    error: "调度器已 abort".to_string(),
                },
            };
        }
        // 绑定级时长预算（三类目标统一语义；超限按可重试错误退出）。
        if duration_cap_secs > 0 && started_at.elapsed().as_secs() >= duration_cap_secs {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result: StepResult::Retried {
                    error: format!("绑定预算耗尽：时长超过 {duration_cap_secs}s"),
                },
            };
        }
        if let Err(reason) = rt.admit_attempt(&step.id, &attempt_id, attempts > 0).await {
            let result = if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                StepResult::Retried {
                    error: "调度器在执行准入期间收到 abort".to_string(),
                }
            } else {
                StepResult::Budget { reason }
            };
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result,
            };
        }
        // Abort may arrive while waiting for the durable host acknowledgement.
        // The reservation remains charged, but no Worker call may start afterward.
        if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
            return StepOutcome {
                step_id: step.id.clone(),
                attempts,
                result: StepResult::Retried {
                    error: "调度器在执行准入确认期间收到 abort".to_string(),
                },
            };
        }
        attempts = attempts.saturating_add(1);
        // 取消传播：在飞步骤任务随 abort 标志即时终止（池路径经 cancel_all 传播到子进程）。
        match run_worker_cancellable(&worker, &input, &rt).await {
            Ok(output) => {
                // fencing 写校验：租约失效（过期/重连/分区）时拒绝写入结果，
                // 按血缘重算（replan 重置）而非重复写。
                if let (Some(leases), Some(lease)) = (&rt.leases, &step_lease) {
                    if let Err(e) = leases.verify_write(&lease.holder, &lease.token, lease.epoch) {
                        return StepOutcome {
                            step_id: step.id.clone(),
                            attempts,
                            result: StepResult::Retried {
                                error: format!("fencing 拒绝写入：{e}"),
                            },
                        };
                    }
                }
                // 可选 critic 评审：先得到最终候选，再对最终字节执行验收。
                let candidate = if critic_rounds > 0 {
                    match run_step_critic(
                        &worker,
                        &input,
                        &output,
                        &step,
                        &attempt_id,
                        &rt,
                        critic_rounds,
                        &mut attempts,
                    )
                    .await
                    {
                        Ok(out) => out,
                        Err(StepExecutionError::Budget(reason)) => {
                            return StepOutcome {
                                step_id: step.id.clone(),
                                attempts,
                                result: StepResult::Budget { reason },
                            }
                        }
                        Err(StepExecutionError::Failed(error)) => {
                            return StepOutcome {
                                step_id: step.id.clone(),
                                attempts,
                                result: if validation_receipts.is_empty() {
                                    StepResult::Retried { error }
                                } else {
                                    StepResult::FailedWithReceipts {
                                        error,
                                        attempt_id,
                                        validation_receipts,
                                        epoch,
                                    }
                                },
                            }
                        }
                    }
                } else {
                    output
                };
                let (receipts, validation_error) =
                    verify_step_output(&step, &rt, &input, &attempt_id, epoch, &candidate);
                validation_receipts.extend(receipts);
                if let Some(error) = validation_error {
                    if attempts >= max_attempts {
                        return StepOutcome {
                            step_id: step.id.clone(),
                            attempts,
                            result: StepResult::FailedWithReceipts {
                                error,
                                attempt_id,
                                validation_receipts,
                                epoch,
                            },
                        };
                    }
                    continue;
                }
                return finish_step(
                    &step,
                    &rt,
                    candidate,
                    attempts,
                    attempt_id,
                    validation_receipts,
                    epoch,
                )
                .await;
            }
            Err(e) => {
                if attempts >= max_attempts {
                    return StepOutcome {
                        step_id: step.id.clone(),
                        attempts,
                        result: if validation_receipts.is_empty() {
                            StepResult::Retried { error: e }
                        } else {
                            StepResult::FailedWithReceipts {
                                error: e,
                                attempt_id,
                                validation_receipts,
                                epoch,
                            }
                        },
                    };
                }
            }
        }
    }
    StepOutcome {
        step_id: step.id.clone(),
        attempts,
        result: StepResult::Retried {
            error: "未知错误".to_string(),
        },
    }
}

/// 执行 worker 且响应 abort 传播：abort 标志置位时**先通知停止，再做有界清理**，
/// 不直接丢弃 worker Future 而跳过其变更收尾（十期·四路 R2）：
///
/// 1. 池路径经 `cancel_all` 把取消传播到子进程（submit 以 Cancelled 立即可见）；
/// 2. 远端派发统一 cancel（防 transport 残留 pending）；
/// 3. 协作式 worker（`TrackedRoleWorker` 等）在回合边界检查取消后快速返回——
///    其 Future 内部已完成 后快照/变更登记/ChangeSet 收尾；
/// 4. 超过 [`CANCELLATION_CLEANUP_GRACE`] 仍未返回的任务才被强制终止（本函数
///    返回 Err 丢弃 Future；进程树由沙箱 Job kill-on-close / 子进程终止兜底）。
pub(super) async fn run_worker_cancellable(
    worker: &Arc<dyn Worker>,
    input: &serde_json::Value,
    rt: &StepRuntime,
) -> Result<String, String> {
    let aborted = Arc::clone(&rt.aborted);
    let run = worker.run(input);
    tokio::pin!(run);
    tokio::select! {
        out = &mut run => out,
        _ = wait_aborted(aborted) => {
            if let Some(pool) = &rt.worker_pool {
                let _ = pool.cancel_all().await;
            }
            // A2：abort 即时取消在飞的远端派发任务（防残留 pending）。
            rt.cancels.cancel_all().await;
            tracing::debug!(rt.run_id, "调度器已 abort：等待 worker 有界清理（含变更收尾）");
            match tokio::time::timeout(CANCELLATION_CLEANUP_GRACE, &mut run).await {
                Ok(out) => out,
                Err(_) => {
                    tracing::warn!(
                        rt.run_id,
                        "worker 清理超时（{}s），强制终止（进程树由沙箱终止）",
                        CANCELLATION_CLEANUP_GRACE.as_secs()
                    );
                    Err("调度器已 abort（清理超时，强制终止）".to_string())
                }
            }
        }
    }
}

/// 轮询 abort 标志（20ms 粒度；预算/取消响应的最低时延）。
async fn wait_aborted(flag: Arc<std::sync::atomic::AtomicBool>) {
    loop {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A step receipt only satisfies the active requirement when every identity field
/// still matches the accepted attempt, output, validator version, and arguments.
pub(super) fn step_validation_receipt_matches(
    step_id: &str,
    attempt_id: &str,
    epoch: u64,
    output_sha256: &str,
    requirement: &crate::plan::VerificationRequirementV1,
    receipt: &crate::plan::ValidationReceiptV1,
) -> bool {
    let arguments_sha256 = crate::cas_store::CasStore::hash_of(
        &serde_json::to_vec(&requirement.arguments).unwrap_or_default(),
    );
    receipt.task_id == step_id
        && receipt.attempt_id == attempt_id
        && receipt.epoch == epoch
        && receipt.requirement_id == requirement.requirement_id
        && receipt.validator_id == requirement.validator_id
        && receipt.validator_version
            == requirement
                .validator_version
                .as_deref()
                .unwrap_or("unknown")
        && receipt.arguments_sha256 == arguments_sha256
        && receipt
            .subject_sha256
            .get("step-output")
            .map(String::as_str)
            == Some(output_sha256)
}

pub(super) fn validate_host_command_validation(
    requirement: &crate::plan::VerificationRequirementV1,
    result: &HostCommandValidationV1,
) -> Result<(), &'static str> {
    let crate::plan::VerificationScopeV1::WorkspacePaths { relative_paths } = &requirement.scope
    else {
        return Err("宿主命令回执范围不是 WorkspacePaths");
    };
    let expected_subjects = relative_paths
        .iter()
        .map(|path| format!("workspace-path:{path}"))
        .collect::<std::collections::BTreeSet<_>>();
    if expected_subjects.len() != relative_paths.len()
        || result.subject_sha256.len() != expected_subjects.len()
        || result
            .subject_sha256
            .keys()
            .any(|key| !expected_subjects.contains(key))
    {
        return Err("宿主命令回执的源码路径证据与声明范围不一致");
    }
    if result
        .subject_sha256
        .values()
        .any(|hash| hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err("宿主命令回执包含非法源码 SHA-256");
    }
    let Some(evidence_ref) = result.evidence_ref.as_deref() else {
        return Err("宿主命令回执缺少命令结果证据引用");
    };
    let Some(command_hash) = evidence_ref.strip_prefix("command-result:sha256:") else {
        return Err("宿主命令回执证据引用格式无效");
    };
    if command_hash.len() != 64 || !command_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("宿主命令回执证据引用缺少有效 SHA-256");
    }
    Ok(())
}

/// 步骤成功收尾：可选黑板写回 + 组装成功结果。
pub(super) fn verify_step_output(
    step: &StepSpec,
    rt: &StepRuntime,
    input: &serde_json::Value,
    attempt_id: &str,
    epoch: u64,
    output: &str,
) -> (Vec<crate::plan::ValidationReceiptV1>, Option<String>) {
    let plan = step.verification_plan.clone().or_else(|| {
        step.verify.as_ref().map(|spec| {
            crate::verification::plan_for_specs(
                &format!("verify-step-{}", step.id),
                std::slice::from_ref(spec),
            )
        })
    });
    let Some(plan) = plan else {
        return (Vec::new(), None);
    };
    let input_bytes = serde_json::to_vec(input).unwrap_or_default();
    let input_sha256 = crate::cas_store::CasStore::hash_of(&input_bytes);
    let output_sha256 = crate::cas_store::CasStore::hash_of(output.as_bytes());
    let mut receipts = Vec::with_capacity(plan.requirements.len());
    let mut failures = Vec::new();
    let mut workspace_validation = crate::verification::WorkspaceValidationBatch::new(
        rt.workspace_verification_root.as_deref(),
    );
    for requirement in &plan.requirements {
        let started_at = chrono::Utc::now().to_rfc3339();
        let (verdict, detail, workspace_subjects, command_evidence_ref) =
            if requirement.validator_id == "workspace-command-success-v1" {
                let resources = &requirement.resources;
                let supported = requirement.validator_version.as_deref() == Some("1")
                    && matches!(
                        &requirement.scope,
                        crate::plan::VerificationScopeV1::WorkspacePaths { .. }
                    )
                    && crate::verification::workspace_validator_arguments_supported(
                        &requirement.validator_id,
                        &requirement.arguments,
                    )
                    && resources.cpu_slots == 1
                    && (8..=128).contains(&resources.memory_mb)
                    && !resources.exclusive_workspace
                    && (1..=30_000).contains(&resources.timeout_ms);
                if !supported {
                    (
                        crate::plan::ValidationVerdictV1::Unsupported,
                        Some("workspace-command-success-v1 不符合宿主注册契约".to_string()),
                        std::collections::BTreeMap::new(),
                        None,
                    )
                } else if let Some(verifier) = &rt.workspace_command_verifier {
                    let mut host_result = verifier(&step.id, attempt_id, requirement);
                    if host_result.verdict == crate::plan::ValidationVerdictV1::Passed {
                        if let Err(reason) =
                            validate_host_command_validation(requirement, &host_result)
                        {
                            host_result.verdict = crate::plan::ValidationVerdictV1::Unverified;
                            host_result.detail = Some(reason.to_string());
                            host_result.subject_sha256.clear();
                            host_result.evidence_ref = None;
                        }
                    }
                    (
                        host_result.verdict,
                        host_result.detail,
                        host_result.subject_sha256,
                        host_result.evidence_ref,
                    )
                } else {
                    let (verdict, detail, subjects) =
                        workspace_validation.execute_registered(requirement, output);
                    (verdict, detail, subjects, None)
                }
            } else {
                let (verdict, detail, subjects) =
                    workspace_validation.execute_registered(requirement, output);
                (verdict, detail, subjects, None)
            };
        let arguments = serde_json::to_vec(&requirement.arguments).unwrap_or_default();
        let validator_version = requirement
            .validator_version
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let arguments_sha256 = crate::cas_store::CasStore::hash_of(&arguments);
        let receipt_id_seed = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            rt.run_id,
            step.id,
            attempt_id,
            epoch,
            requirement.requirement_id,
            requirement.validator_id,
            validator_version,
            arguments_sha256,
            output_sha256,
            workspace_subjects
                .iter()
                .map(|(path, hash)| format!("{path}:{hash}"))
                .collect::<Vec<_>>()
                .join("|")
        );
        receipts.push(crate::plan::ValidationReceiptV1 {
            receipt_id: crate::cas_store::CasStore::hash_of(receipt_id_seed.as_bytes()),
            task_id: step.id.clone(),
            attempt_id: attempt_id.to_string(),
            epoch,
            requirement_id: requirement.requirement_id.clone(),
            validator_id: requirement.validator_id.clone(),
            validator_version,
            arguments_sha256,
            input_sha256: input_sha256.clone(),
            environment_id: format!("owo-agent-core/{}", env!("CARGO_PKG_VERSION")),
            changeset_sha256: None,
            detail: detail.clone(),
            subject_sha256: {
                let mut subjects = std::collections::HashMap::from([(
                    "step-output".to_string(),
                    output_sha256.clone(),
                )]);
                subjects.extend(workspace_subjects);
                subjects
            },
            verdict,
            evidence_refs: {
                let mut refs = vec![format!(
                    "goal-step://{}/{}/{}",
                    rt.run_id, step.id, attempt_id
                )];
                if let Some(reference) = command_evidence_ref {
                    refs.push(reference);
                }
                refs
            },
            review_result: None,
            started_at,
            completed_at: chrono::Utc::now().to_rfc3339(),
        });
        if requirement.required && verdict != crate::plan::ValidationVerdictV1::Passed {
            failures.push(format!(
                "{}: {}",
                requirement.requirement_id,
                detail.as_deref().unwrap_or("验证器未返回通过")
            ));
        }
    }
    let mut final_snapshot = crate::workspace_snapshot::WorkspaceSnapshotBatch::new(
        rt.workspace_verification_root.as_deref(),
    );
    for receipt in &mut receipts {
        if receipt.verdict == crate::plan::ValidationVerdictV1::Passed
            && !final_snapshot.subjects_match(&receipt.subject_sha256)
        {
            receipt.verdict = crate::plan::ValidationVerdictV1::Stale;
            receipt.detail = Some("步骤最终工作区偏离验证快照".into());
            receipt.completed_at = chrono::Utc::now().to_rfc3339();
            if plan.requirements.iter().any(|requirement| {
                requirement.required && requirement.requirement_id == receipt.requirement_id
            }) {
                failures.push(format!(
                    "{}: 步骤最终工作区偏离验证快照",
                    receipt.requirement_id
                ));
            }
        }
    }
    (
        receipts,
        (!failures.is_empty()).then(|| failures.join("；")),
    )
}

/// 步骤成功收尾：可选黑板写回 + 组装成功结果。
async fn finish_step(
    step: &StepSpec,
    rt: &StepRuntime,
    output: String,
    attempts: u32,
    attempt_id: String,
    validation_receipts: Vec<crate::plan::ValidationReceiptV1>,
    epoch: u64,
) -> StepOutcome {
    if let (Some(bb), Some(writer)) = (&rt.blackboard, &rt.bb_writer) {
        if let Some(key) = step
            .input
            .get("_bb")
            .and_then(|v| v.get("write"))
            .and_then(|v| v.as_str())
        {
            if let Err(e) = bb
                .write(writer, key, serde_json::Value::String(output.clone()))
                .await
            {
                return StepOutcome {
                    step_id: step.id.clone(),
                    attempts,
                    result: StepResult::FailedWithReceipts {
                        error: format!("blackboard 写入失败：{e}"),
                        attempt_id,
                        validation_receipts,
                        epoch,
                    },
                };
            }
        }
    }
    StepOutcome {
        step_id: step.id.clone(),
        attempts,
        result: StepResult::Ok {
            output,
            attempt_id,
            validation_receipts,
            epoch,
        },
    }
}

/// 步骤内 critic 评审循环：输出经只读门禁评审，意见回流 worker 重跑。
/// 通过或轮数耗尽返回最终草稿；未通过返回 Err（视为步骤失败）。
async fn run_step_critic(
    worker: &Arc<dyn Worker>,
    input: &serde_json::Value,
    initial_output: &str,
    step: &StepSpec,
    attempt_id: &str,
    rt: &StepRuntime,
    max_rounds: u32,
    attempts: &mut u32,
) -> Result<String, StepExecutionError> {
    let config = rt.critic.as_ref().ok_or_else(|| {
        StepExecutionError::Failed("步骤声明 _critic 但 runner 未 attach critic".to_string())
    })?;
    let context = serde_json::json!({
        "step": step.id,
        "worker": step.worker,
        "correlation_id": step.id,
    });
    let revision_attempts = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let budget_failure = Arc::new(Mutex::new(None::<String>));
    let author = {
        let worker = Arc::clone(worker);
        let input = input.clone();
        let rt = rt.clone();
        let revision_attempts = Arc::clone(&revision_attempts);
        let budget_failure = Arc::clone(&budget_failure);
        let attempt_admission = rt.attempt_admission.clone();
        let step_id = step.id.clone();
        let attempt_id = attempt_id.to_string();
        move |_draft: String, feedback: Vec<String>| {
            let worker = Arc::clone(&worker);
            let input = inject_feedback(&input, &feedback);
            let rt = rt.clone();
            let revision_attempts = Arc::clone(&revision_attempts);
            let budget_failure = Arc::clone(&budget_failure);
            let attempt_admission = attempt_admission.clone();
            let step_id = step_id.clone();
            let attempt_id = attempt_id.clone();
            async move {
                if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("调度器已 abort".to_string());
                }
                if let Err(reason) = rt.execution_budget.reserve_attempt(true) {
                    if let Ok(mut failure) = budget_failure.lock() {
                        *failure = Some(reason.clone());
                    }
                    return Err(reason);
                }
                if let Some(admission) = &attempt_admission {
                    if let Err(reason) = admission.admit(step_id, attempt_id, true).await {
                        if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                            return Err("调度器已 abort".to_string());
                        }
                        if let Ok(mut failure) = budget_failure.lock() {
                            *failure = Some(reason.clone());
                        }
                        return Err(reason);
                    }
                }
                if rt.aborted.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err("调度器在 critic 执行准入确认期间收到 abort".to_string());
                }
                revision_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                worker.run(&input).await
            }
        }
    };
    let outcome = review_loop(config, &context, initial_output.to_string(), author).await;
    *attempts =
        attempts.saturating_add(revision_attempts.load(std::sync::atomic::Ordering::SeqCst));
    let budget_failure = budget_failure
        .lock()
        .ok()
        .and_then(|mut failure| failure.take());
    if let Some(reason) = budget_failure {
        return Err(StepExecutionError::Budget(reason));
    }
    let outcome = outcome.map_err(StepExecutionError::Failed)?;
    if outcome.approved {
        Ok(outcome.final_draft)
    } else {
        Err(StepExecutionError::Failed(format!(
            "critic 评审未通过（{max_rounds} 轮）：score={}",
            outcome.history.last().map(|r| r.verdict.score).unwrap_or(0)
        )))
    }
}

/// 把评审意见注入 input 的 `_critic_feedback` 键（worker 可读取并据此修订）。
fn inject_feedback(input: &serde_json::Value, feedback: &[String]) -> serde_json::Value {
    let mut cloned = input.clone();
    if let Some(obj) = cloned.as_object_mut() {
        obj.insert(
            "_critic_feedback".to_string(),
            serde_json::Value::Array(
                feedback
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    cloned
}

/// 步骤 input 预处理：`_bb.read` 声明的黑板键读取并替换 `{{bb:key}}` 占位符。
async fn prepare_step_input(
    input: &serde_json::Value,
    blackboard: &Option<Blackboard>,
) -> Result<serde_json::Value, String> {
    let Some(read_keys) = input
        .get("_bb")
        .and_then(|v| v.get("read"))
        .and_then(|v| v.as_array())
    else {
        return Ok(input.clone());
    };
    let bb = blackboard
        .as_ref()
        .ok_or_else(|| "步骤声明 _bb.read 但 runner 未 attach blackboard".to_string())?;
    let mut values: Vec<(String, String)> = Vec::new();
    for key in read_keys {
        let key = key
            .as_str()
            .ok_or_else(|| "步骤声明 _bb.read 必须为字符串键数组".to_string())?;
        let value = bb
            .read(key)
            .await
            .map_err(|e| format!("blackboard 读取失败：{e}"))?;
        let text = match value {
            serde_json::Value::String(s) => s,
            other => serde_json::to_string(&other).map_err(|e| format!("黑板值序列化失败：{e}"))?,
        };
        values.push((key.to_string(), text));
    }
    Ok(replace_tokens(input, &values))
}

/// 递归替换字符串叶子中的 `{{bb:key}}` 占位符。
fn replace_tokens(value: &serde_json::Value, values: &[(String, String)]) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let mut out = s.clone();
            for (key, text) in values {
                out = out.replace(&format!("{{{{bb:{key}}}}}"), text);
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|v| replace_tokens(v, values)).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), replace_tokens(v, values)))
                .collect(),
        ),
        other => other.clone(),
    }
}
