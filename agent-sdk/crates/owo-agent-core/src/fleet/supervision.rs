use serde::{Deserialize, Serialize};

/// 监督重启策略（OTP 映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RestartPolicy {
    OneForOne,
    RestForOne,
    OneForAll,
}

/// 监督规则：崩溃计数、退避基数与重启策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartRule {
    pub max_restarts: u32,
    pub base_backoff_secs: u64,
    pub policy: RestartPolicy,
}

impl Default for RestartRule {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            base_backoff_secs: 1,
            policy: RestartPolicy::OneForOne,
        }
    }
}

/// 指数退避（对齐 `cloud_exec::backoff_delay`：base·2^n，封顶 60s）。
pub fn backoff_secs(base_secs: u64, attempts: u32) -> u64 {
    base_secs.saturating_mul(1u64 << attempts.min(6)).min(60)
}

/// 监督状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisionState {
    Healthy,
    Restarting { attempts: u32, next_retry_secs: u64 },
    Fused { attempts: u32 },
}

/// 单 worker 监督器（崩溃计数 + 退避 + 熔断）。
#[derive(Debug, Clone)]
pub struct Supervisor {
    rule: RestartRule,
    attempts: u32,
}

impl Supervisor {
    pub fn new(rule: RestartRule) -> Self {
        Self { rule, attempts: 0 }
    }

    pub fn rule(&self) -> RestartRule {
        self.rule
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// 健康运行后复位崩溃计数。
    pub fn mark_healthy(&mut self) {
        self.attempts = 0;
    }

    /// 崩溃上报：返回监督状态（重启待退避，或熔断）。
    pub fn on_crash(&mut self) -> SupervisionState {
        self.attempts += 1;
        if self.attempts > self.rule.max_restarts {
            SupervisionState::Fused {
                attempts: self.attempts,
            }
        } else {
            SupervisionState::Restarting {
                attempts: self.attempts,
                next_retry_secs: backoff_secs(
                    self.rule.base_backoff_secs,
                    self.attempts.saturating_sub(1),
                ),
            }
        }
    }
}
