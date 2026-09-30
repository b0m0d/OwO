use super::bus::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// 等待图环检测：返回首个环（首尾同一节点），无环返回 `None`。
pub fn detect_cycle(edges: &[(AgentId, AgentId)]) -> Option<Vec<AgentId>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    fn visit(
        node: &str,
        adj: &HashMap<String, Vec<String>>,
        color: &mut HashMap<String, Color>,
        stack: &mut Vec<String>,
        cycle: &mut Vec<AgentId>,
    ) -> bool {
        color.insert(node.to_string(), Color::Gray);
        stack.push(node.to_string());
        if let Some(nexts) = adj.get(node) {
            for next in nexts {
                let c = color.get(next).copied().unwrap_or(Color::White);
                if c == Color::White {
                    if visit(next, adj, color, stack, cycle) {
                        return true;
                    }
                } else if c == Color::Gray {
                    let start = stack.iter().position(|n| n == next).unwrap_or(0);
                    cycle.extend(stack[start..].iter().cloned());
                    cycle.push(next.clone());
                    return true;
                }
            }
        }
        stack.pop();
        color.insert(node.to_string(), Color::Black);
        false
    }

    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for (from, to) in edges {
        adj.entry(from.clone()).or_default().push(to.clone());
    }
    let keys: Vec<String> = adj.keys().cloned().collect();
    let mut color = HashMap::new();
    let mut stack = Vec::new();
    let mut cycle = Vec::new();
    for node in keys {
        if color.get(&node).copied().unwrap_or(Color::White) == Color::White
            && visit(&node, &adj, &mut color, &mut stack, &mut cycle)
        {
            return Some(cycle);
        }
    }
    None
}

/// 等待图边：`waiter` 正在等待 `waited` 完成（handoff 推广：任意 agent 间等待）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitEdge {
    pub waiter: AgentId,
    pub waited: AgentId,
    /// 等待超时（None = 无限等待，靠仲裁与整体预算兜底）。
    pub timeout: Option<Duration>,
}

impl WaitEdge {
    pub fn new(waiter: impl Into<AgentId>, waited: impl Into<AgentId>) -> Self {
        Self {
            waiter: waiter.into(),
            waited: waited.into(),
            timeout: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// 等待图环检测：把 `plan.rs` 的 DAG 环检测推广为 agent 间等待图检测。
/// 返回构成环的 agent 序列（首尾同一节点）；无环返回 `None`。
pub fn detect_wait_cycle(edges: &[WaitEdge]) -> Option<Vec<AgentId>> {
    let pairs: Vec<(AgentId, AgentId)> = edges
        .iter()
        .map(|e| (e.waiter.clone(), e.waited.clone()))
        .collect();
    detect_cycle(&pairs)
}

/// 环仲裁：按优先级取消最低优先分支（priority 值越大优先级越低；缺省视为最低）。
/// 并列取字典序最大者，保证确定性。
pub fn arbitrate_wait_cycle(cycle: &[AgentId], priority: &HashMap<AgentId, u32>) -> AgentId {
    cycle
        .iter()
        .filter(|a| !a.is_empty())
        .max_by(|a, b| {
            let pa = priority.get(*a).copied().unwrap_or(u32::MAX);
            let pb = priority.get(*b).copied().unwrap_or(u32::MAX);
            pa.cmp(&pb).then_with(|| a.cmp(b))
        })
        .cloned()
        .unwrap_or_else(|| "unknown".to_string())
}

/// 等待图仲裁决议：取消哪个分支以解开死锁。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitResolution {
    pub cancel: AgentId,
    pub cycle: Vec<AgentId>,
    pub reason: String,
}

/// 可维护的等待图：登记边与优先级，周期扫描环并给出仲裁决议。
#[derive(Debug, Clone, Default)]
pub struct WaitGraph {
    edges: Vec<WaitEdge>,
    priority: HashMap<AgentId, u32>,
}

impl WaitGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(
        &mut self,
        waiter: impl Into<AgentId>,
        waited: impl Into<AgentId>,
        timeout: Option<Duration>,
    ) {
        self.edges.push(WaitEdge {
            waiter: waiter.into(),
            waited: waited.into(),
            timeout,
        });
    }

    pub fn set_priority(&mut self, agent: impl Into<AgentId>, priority: u32) {
        self.priority.insert(agent.into(), priority);
    }

    pub fn edges(&self) -> &[WaitEdge] {
        &self.edges
    }

    /// 周期扫描：发现环返回环路径。
    pub fn cycle(&self) -> Option<Vec<AgentId>> {
        detect_wait_cycle(&self.edges)
    }

    /// 仲裁：发现环即按优先级取消低优先分支（带超时等待的边优先由超时处理）。
    pub fn resolve(&self) -> Option<WaitResolution> {
        let cycle = self.cycle()?;
        let cancel = arbitrate_wait_cycle(&cycle, &self.priority);
        Some(WaitResolution {
            reason: format!(
                "agent 等待图死锁：取消低优先分支 {cancel}（环：{}）",
                cycle.join(" → ")
            ),
            cycle,
            cancel,
        })
    }
}
