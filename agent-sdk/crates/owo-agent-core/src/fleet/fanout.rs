use super::bus::*;
use crate::capability::CapabilityMatch;
use crate::experience_store::{Attribution, ExperienceStore, Outcome};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinSet;

/// fan-out 单 worker 终态（部分成功仲裁的依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanOutStatus {
    /// 成功产出。
    Succeeded,
    /// worker 明确失败（确定性错误，可单独重试）。
    #[default]
    Failed,
    /// 单 worker 超时（可单独重试）。
    TimedOut,
    /// 取消传播（调用方取消，不自动重试）。
    Cancelled,
    /// 整体预算硬停（时长维度）。
    Aborted,
    /// 能力不满足/降级（未调度；确定性，不自动重试）。
    Unfit,
}

/// fan-out 单 worker 结果（按输入顺序返回）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanOutOutcome {
    pub worker: AgentId,
    pub ok: bool,
    #[serde(default)]
    pub status: FanOutStatus,
    pub error: Option<String>,
    pub output: Option<String>,
}

impl FanOutOutcome {
    pub fn success(worker: impl Into<AgentId>, output: impl Into<String>) -> Self {
        Self {
            worker: worker.into(),
            ok: true,
            status: FanOutStatus::Succeeded,
            error: None,
            output: Some(output.into()),
        }
    }

    pub fn failure(worker: impl Into<AgentId>, error: impl Into<String>) -> Self {
        Self {
            worker: worker.into(),
            ok: false,
            status: FanOutStatus::Failed,
            error: Some(error.into()),
            output: None,
        }
    }

    pub fn with_status(mut self, status: FanOutStatus) -> Self {
        self.ok = status == FanOutStatus::Succeeded;
        self.status = status;
        self
    }
}

/// fan-out 调度配置：并行度、预算、单 worker 超时与取消传播。
#[derive(Debug, Clone)]
pub struct FanOutConfig {
    pub max_parallel: usize,
    pub budget: Budget,
    /// 单 worker 超时（None = 不超时，仅受整体预算约束）。
    pub per_worker_timeout: Option<Duration>,
    /// 取消标志：置位后不再启动新 worker，在飞 worker 被 abort，未完成者标记 Cancelled。
    pub cancelled: Option<Arc<AtomicBool>>,
    /// 能力注册表（可选）：需求不满足/降级的 worker 标记 [`FanOutStatus::Unfit`] 且不调度。
    pub capabilities: Option<crate::capability::CapabilityWorkerRegistry>,
    /// 能力需求（`capabilities` 提供时生效；worker 逐一评估）。
    pub requirement: Option<crate::capability::WorkerRequirement>,
    /// 经验库（可选）：每个终态结果以 `correlation_id:worker` 幂等写入。
    pub experience: Option<crate::experience_store::ExperienceStore>,
}

impl Default for FanOutConfig {
    fn default() -> Self {
        Self {
            max_parallel: 4,
            budget: Budget::default(),
            per_worker_timeout: None,
            cancelled: None,
            capabilities: None,
            requirement: None,
            experience: None,
        }
    }
}

/// fan-out 汇总报告（部分成功仲裁 + 可单独重试视图）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FanOutReport {
    pub correlation_id: CorrelationId,
    /// 按输入顺序的结果。
    pub outcomes: Vec<FanOutOutcome>,
}

impl FanOutReport {
    pub fn outcome(&self, worker: &str) -> Option<&FanOutOutcome> {
        self.outcomes.iter().find(|o| o.worker == worker)
    }

    pub fn succeeded(&self) -> Vec<&FanOutOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.status == FanOutStatus::Succeeded)
            .collect()
    }

    pub fn failed(&self) -> Vec<&FanOutOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.status != FanOutStatus::Succeeded)
            .collect()
    }

    /// 可单独重试的子任务（明确失败或超时；取消/预算中止由调用方决策，不自动重试）。
    pub fn retryable(&self) -> Vec<&FanOutOutcome> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, FanOutStatus::Failed | FanOutStatus::TimedOut))
            .collect()
    }

    /// 部分成功：已成功结果保留，未成功者是否为空。
    pub fn all_succeeded(&self) -> bool {
        self.failed().is_empty()
    }
}

/// fan-out 增强版：超时 + 取消传播 + 部分成功仲裁。
///
/// - 超时：`config.per_worker_timeout` 到点 abort 该 worker（tokio timeout 取消 future）。
/// - 取消：`config.cancelled` 置位后，未启动者直接 Cancelled，在飞者 abort，已成功结果保留。
/// - 预算：时长维度整体硬停（与 [`Budget::exceeded`] 语义一致），未完成者 Aborted。
/// - 结果按输入顺序返回；已成功结果不受后续取消/超时影响（部分成功保留）。
pub async fn fan_out_cfg<F, Fut>(
    workers: &[AgentId],
    config: FanOutConfig,
    correlation_id: impl Into<CorrelationId>,
    run: F,
) -> FanOutReport
where
    F: Fn(AgentId) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
{
    // 先落成具体类型：后续 clone 进 'static 任务不受泛型生命周期约束。
    let correlation_id: CorrelationId = correlation_id.into();
    let max_parallel = config.max_parallel.max(1);
    let run = Arc::new(run);
    let start = Instant::now();
    let mut set: JoinSet<(AgentId, Result<String, String>, bool)> = JoinSet::new();
    let mut outcomes: HashMap<AgentId, FanOutOutcome> = HashMap::new();
    let mut next = 0usize;
    let mut aborted_by_budget = false;
    let mut cancelled_by_flag = false;
    let mut panic_seen = false;

    loop {
        let cancelled = config
            .cancelled
            .as_ref()
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(false);
        let budget_hit = config.budget.exceeded(0, 0, start.elapsed());
        if budget_hit {
            aborted_by_budget = true;
        }
        if cancelled {
            cancelled_by_flag = true;
        }
        if aborted_by_budget || cancelled_by_flag {
            if !set.is_empty() {
                set.abort_all();
            }
            while set.join_next().await.is_some() {}
            break;
        }
        while set.len() < max_parallel && next < workers.len() {
            let worker = workers[next].clone();
            next += 1;
            // 能力过滤：需求不满足/降级的 worker 明确标记 Unfit，不调度。
            if let (Some(reg), Some(req)) = (&config.capabilities, &config.requirement) {
                let unfit = match reg.evaluate_worker(&worker, req) {
                    None => Some(format!("worker {worker} 未注册能力卡")),
                    Some(CapabilityMatch::Full) => None,
                    Some(CapabilityMatch::Partial { missing }) => Some(format!(
                        "worker {worker} 能力降级（缺失 {}）",
                        missing.join(", ")
                    )),
                    Some(CapabilityMatch::Unfit { reasons }) => Some(format!(
                        "worker {worker} 能力不满足：{}",
                        reasons.join("；")
                    )),
                };
                if let Some(reason) = unfit {
                    if let Some(reg) = &config.capabilities {
                        reg.mark_health(&worker, false);
                    }
                    let outcome = FanOutOutcome::failure(worker.clone(), reason)
                        .with_status(FanOutStatus::Unfit);
                    if let Some(exp) = &config.experience {
                        let corr: CorrelationId = correlation_id.clone();
                        record_fanout_experience(exp, corr, &outcome);
                    }
                    outcomes.insert(worker, outcome);
                    continue;
                }
            }
            let task_worker = worker.clone();
            let run = Arc::clone(&run);
            let per_timeout = config.per_worker_timeout;
            let exp = config.experience.clone();
            let caps = config.capabilities.clone();
            let corr: CorrelationId = correlation_id.clone();
            set.spawn(async move {
                let result = match per_timeout {
                    Some(timeout) => {
                        match tokio::time::timeout(timeout, run(task_worker.clone())).await {
                            Ok(result) => (result, false),
                            Err(_) => (Err("worker timed out".to_string()), true),
                        }
                    }
                    None => {
                        let id = task_worker.clone();
                        (run(id).await, false)
                    }
                };
                if let Some(exp) = exp {
                    let mut outcome = match &result.0 {
                        Ok(output) => FanOutOutcome::success(task_worker.clone(), output.clone()),
                        Err(err) => FanOutOutcome::failure(task_worker.clone(), err.clone()),
                    };
                    if result.1 {
                        outcome = outcome.with_status(FanOutStatus::TimedOut);
                    }
                    record_fanout_experience(&exp, corr, &outcome);
                }
                if let Some(reg) = &caps {
                    reg.mark_health(&task_worker, result.0.is_ok());
                }
                (task_worker, result.0, result.1)
            });
        }
        if set.is_empty() {
            break;
        }
        match set.join_next().await {
            Some(Ok((worker, result, timed_out))) => {
                let mut outcome = match result {
                    Ok(output) => FanOutOutcome::success(worker, output),
                    Err(err) => FanOutOutcome::failure(worker, err),
                };
                if timed_out {
                    outcome = outcome.with_status(FanOutStatus::TimedOut);
                }
                outcomes.insert(outcome.worker.clone(), outcome);
            }
            Some(Err(_)) => panic_seen = true,
            None => break,
        }
    }

    let report = workers
        .iter()
        .map(|worker| {
            if let Some(outcome) = outcomes.remove(worker) {
                outcome
            } else if aborted_by_budget {
                FanOutOutcome::failure(worker, "budget exceeded: task aborted")
                    .with_status(FanOutStatus::Aborted)
            } else if cancelled_by_flag {
                FanOutOutcome::failure(worker, "cancelled by caller")
                    .with_status(FanOutStatus::Cancelled)
            } else if panic_seen {
                FanOutOutcome::failure(worker, "worker panicked or join error")
            } else {
                FanOutOutcome::failure(worker, "worker did not complete")
            }
        })
        .collect();
    FanOutReport {
        correlation_id,
        outcomes: report,
    }
}

/// 把 fan-out 终态结果幂等写入经验库（correlation_id = `fan-out:correlation_id:worker`）。
pub(super) fn record_fanout_experience(
    exp: &ExperienceStore,
    correlation_id: CorrelationId,
    o: &FanOutOutcome,
) {
    let outcome = match o.status {
        FanOutStatus::Succeeded => Outcome::Success,
        FanOutStatus::Cancelled => Outcome::Cancelled,
        FanOutStatus::Aborted | FanOutStatus::Unfit => Outcome::Aborted,
        FanOutStatus::Failed | FanOutStatus::TimedOut => Outcome::Failure,
    };
    let attribution = Attribution {
        goal_id: None,
        plan_id: None,
        step_id: None,
        input_keys: Vec::new(),
        error: o.error.clone(),
    };
    let _ = exp.record_worker_outcome(
        format!("fan-out:{correlation_id}:{}", o.worker),
        o.worker.clone(),
        outcome,
        attribution,
    );
}

/// 数据并行 fan-out：`max_parallel` 限流 + 预算硬停（时长维度），结果按输入顺序返回。
///
/// 预算的轮次/步数维度由 worker 自身循环检查（本函数只负责调度与时长预算）。
/// worker 闭包不应 panic；若发生，本函数保守地将未完成 worker 标记为失败。
/// 需要超时/取消/部分成功仲裁时使用 [`fan_out_cfg`]。
pub async fn fan_out<F, Fut>(
    workers: &[AgentId],
    max_parallel: usize,
    budget: Budget,
    run: F,
) -> Vec<FanOutOutcome>
where
    F: Fn(AgentId) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String, String>> + Send + 'static,
{
    fan_out_cfg(
        workers,
        FanOutConfig {
            max_parallel,
            budget,
            ..Default::default()
        },
        "fan-out",
        run,
    )
    .await
    .outcomes
}
