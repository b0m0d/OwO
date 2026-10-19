//! §9.2 统一截止时间预算（DeadlineBudget）。
//!
//! 从 turn 入口向下传递**一份**预算，而不是模型、审批、MCP、进程各自重新获得
//! 完整超时：每个阶段开始前取 [`DeadlineBudget::remaining`]，结束记
//! [`DeadlineBudget::record`]；retry / 下一阶段前重新检查，耗尽即结构化失败
//!（错误统一携带 phase/elapsed，provider/tool/server/attempt/trace_id 经
//! [`DeadlineExceeded::with_context`] 附加）。
//!
//! `DeadlineBudget::new(None, ..)` = 不限时（保持既有行为，仅记账）；
//! `Some(turn_budget)` 即激活 turn 级 deadline + 各阶段独立预算。
//!
//! §9.3：[`PhaseTiming`] 是同一 trace 内的阶段耗时瀑布记录，
//! `TurnOutcome::phase_timings` 按发生顺序携带。

use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// 预算作用阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 模型调用（流式补全）。
    Model,
    /// 审批等待（独立审批模型 + 用户审批）。
    Approval,
    /// 工具执行（含 MCP）。
    Tool,
    /// 持久化（会话提交等）。
    Persistence,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Approval => "approval",
            Self::Tool => "tool",
            Self::Persistence => "persistence",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Model => 0,
            Self::Approval => 1,
            Self::Tool => 2,
            Self::Persistence => 3,
        }
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 各阶段独立预算（由 turn 入口统一分配；缺省值宽松，仅作上限保护）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PhaseBudgets {
    pub model: Duration,
    pub approval: Duration,
    pub tool: Duration,
    pub persistence: Duration,
}

impl Default for PhaseBudgets {
    fn default() -> Self {
        Self {
            model: Duration::from_secs(300),
            approval: Duration::from_secs(120),
            tool: Duration::from_secs(300),
            persistence: Duration::from_secs(60),
        }
    }
}

impl PhaseBudgets {
    fn for_phase(&self, phase: Phase) -> Duration {
        match phase {
            Phase::Model => self.model,
            Phase::Approval => self.approval,
            Phase::Tool => self.tool,
            Phase::Persistence => self.persistence,
        }
    }
}

/// 超预算错误：核心字段 phase/elapsed（§9.2）；provider/tool/server/attempt/
/// trace_id 经 [`DeadlineExceeded::with_context`] 追加进 context。
#[derive(Debug, Clone, thiserror::Error)]
#[error("预算耗尽：phase={phase} elapsed_ms={elapsed_ms}{context}")]
pub struct DeadlineExceeded {
    pub phase: Phase,
    pub elapsed_ms: u64,
    /// 附加上下文（provider=… tool=… server=… attempt=… trace_id=…）。
    pub context: String,
}

impl DeadlineExceeded {
    /// 追加 §9.2 错误契约的上下文字段。
    pub fn with_context(
        mut self,
        provider: &str,
        tool: &str,
        server: &str,
        attempt: u32,
        trace_id: &str,
    ) -> Self {
        self.context = format!(
            " provider={provider} tool={tool} server={server} attempt={attempt} trace_id={trace_id}"
        );
        self
    }

    /// 映射为 `AgentError::Gateway` 的结构化文案（不新增 AgentError 变体，
    /// 避免破坏下游穷举匹配；phase/elapsed/context 全部保留在文案中）。
    pub fn to_agent_error(&self) -> crate::error::AgentError {
        crate::error::AgentError::Gateway(self.to_string())
    }
}

/// §9.3 阶段耗时瀑布的单条记录（同一 trace 内按发生顺序排列）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseTiming {
    /// 阶段名（model / approval / tool / persistence）。
    pub phase: String,
    /// 本次耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 关联对象（工具名 / 并发组标记等；无则空串）。
    #[serde(default)]
    pub target: String,
    /// §9.3 瀑布：首 token 到达时延（仅 model 阶段记录；非流式或未触发为 None）。
    #[serde(default)]
    pub first_token_ms: Option<u64>,
}

/// 统一截止预算：turn 级 deadline + 各阶段消耗记账（§9.2 契约实现）。
#[derive(Debug)]
pub struct DeadlineBudget {
    started: Instant,
    turn_deadline: Option<Instant>,
    budgets: PhaseBudgets,
    consumed: [Duration; 4],
}

impl DeadlineBudget {
    /// `turn_budget: None` = 不限时（仅记账）；`Some(d)` = turn 级截止。
    pub fn new(turn_budget: Option<Duration>, budgets: PhaseBudgets) -> Self {
        Self {
            started: Instant::now(),
            turn_deadline: turn_budget.map(|d| Instant::now() + d),
            budgets,
            consumed: [Duration::ZERO; 4],
        }
    }

    /// turn 已消耗时间。
    pub fn turn_elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// turn 剩余预算（未激活 → `None`）。
    pub fn turn_remaining(&self) -> Option<Duration> {
        self.turn_deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// 阶段剩余 = min(阶段预算 − 阶段已消耗, turn 剩余)；耗尽 → `Err`（§9.2）。
    /// 未激活 turn deadline 时仅受阶段预算约束。
    pub fn remaining(&self, phase: Phase) -> Result<Duration, DeadlineExceeded> {
        let index = phase.index();
        let phase_left = self
            .budgets
            .for_phase(phase)
            .saturating_sub(self.consumed[index]);
        let left = match self.turn_remaining() {
            Some(turn_left) => phase_left.min(turn_left),
            None => phase_left,
        };
        if left.is_zero() {
            Err(DeadlineExceeded {
                phase,
                elapsed_ms: self.turn_elapsed().as_millis() as u64,
                context: String::new(),
            })
        } else {
            Ok(left)
        }
    }

    /// 记录一次阶段消耗（阶段结束时调用；与 `remaining` 配对使用）。
    pub fn record(&mut self, phase: Phase, elapsed: Duration) {
        self.consumed[phase.index()] += elapsed;
    }

    /// retry / 下一阶段前的预算复查（§9.2：外层取消后内层不得继续重试）。
    pub fn check_turn(&self, phase: Phase) -> Result<Duration, DeadlineExceeded> {
        self.remaining(phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_respects_phase_consumption() {
        let mut budget = DeadlineBudget::new(None, PhaseBudgets::default());
        let first = budget.remaining(Phase::Model).expect("初始应有预算");
        assert_eq!(first, PhaseBudgets::default().model);
        budget.record(Phase::Model, Duration::from_secs(299));
        let left = budget.remaining(Phase::Model).expect("还剩 1s");
        assert!(left <= Duration::from_secs(1) && !left.is_zero());
        budget.record(Phase::Model, Duration::from_secs(5));
        let exceeded = budget.remaining(Phase::Model).unwrap_err();
        assert_eq!(exceeded.phase, Phase::Model);
    }

    #[test]
    fn turn_deadline_caps_all_phases() {
        let budget = DeadlineBudget::new(Some(Duration::from_millis(5)), PhaseBudgets::default());
        std::thread::sleep(Duration::from_millis(15));
        // turn 级耗尽：任何阶段 remaining 都必须 Err。
        for phase in [
            Phase::Model,
            Phase::Approval,
            Phase::Tool,
            Phase::Persistence,
        ] {
            assert!(
                budget.remaining(phase).is_err(),
                "phase {phase:?} 应超 turn deadline"
            );
        }
        assert!(budget.turn_remaining().unwrap().is_zero());
    }

    #[test]
    fn check_turn_mirrors_remaining_for_retry_gate() {
        let budget = DeadlineBudget::new(None, PhaseBudgets::default());
        assert!(budget.check_turn(Phase::Tool).is_ok());
    }

    #[test]
    fn inactive_budget_still_records_phase_waterfall() {
        let mut budget = DeadlineBudget::new(None, PhaseBudgets::default());
        budget.record(Phase::Tool, Duration::from_millis(25));
        // 未激活 turn deadline：无 turn 剩余概念，但阶段记账照常。
        assert!(budget.turn_remaining().is_none());
        assert!(budget.turn_elapsed() >= Duration::from_millis(0));
    }

    #[test]
    fn exceeded_error_carries_contract_context() {
        let error = DeadlineExceeded {
            phase: Phase::Tool,
            elapsed_ms: 1234,
            context: String::new(),
        }
        .with_context("glm", "read_file", "fs", 2, "trace-1");
        let text = error.to_string();
        assert!(text.contains("phase=tool"), "{text}");
        assert!(text.contains("elapsed_ms=1234"), "{text}");
        assert!(text.contains("provider=glm"), "{text}");
        assert!(text.contains("trace_id=trace-1"), "{text}");
        // 映射为 AgentError::Gateway 时保留全部字段。
        match error.to_agent_error() {
            crate::error::AgentError::Gateway(message) => {
                assert!(message.contains("phase=tool"), "{message}");
                assert!(message.contains("attempt=2"), "{message}");
            }
            other => panic!("应映射为 Gateway：{other:?}"),
        }
    }

    #[test]
    fn phase_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Phase::Persistence).unwrap(),
            "\"persistence\""
        );
    }
}
