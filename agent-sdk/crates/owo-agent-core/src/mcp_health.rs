//! §10 MCP 运行期隔离：per-server 状态机 / 熔断 / 限流 / 幂等重试。
//!
//! - **状态机（§10.1）**：每个 MCP server 独立维护 Connected / Degraded /
//!   CircuitOpen；熔断只隔离故障 server，其余 server 与整个 Agent turn 不受拖累。
//! - **熔断（§10.2）**：连续失败达阈值 → 开路（调用快速失败，不触碰子进程）；
//!   冷却结束 → 半开放行单个探测，探测成功恢复、失败立即重新开路。
//! - **限流（§10.3）**：per-server 最小调用间隔（默认 0 = 关闭；配置后两次调用
//!   过密时快速失败，避免拖慢整轮）。
//! - **幂等重试（§10.3）**：仅 `EffectClass::Read` 工具失败后重试（写/执行/
//!   未知 effect 一律不重试，杜绝重复副作用）。

use crate::tool_effects::EffectClass;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// per-server 运行状态（§10.1 状态机，供观测/UI 消费）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    /// 正常。
    Connected,
    /// 连续失败未达熔断阈值（放行但观测）。
    Degraded,
    /// 熔断开路：拒绝调用直到冷却结束。
    CircuitOpen,
}

/// 熔断/限流/重试配置。
#[derive(Debug, Clone)]
pub struct McpHealthConfig {
    /// 连续失败阈值（达到即熔断开路）。
    pub failure_threshold: u32,
    /// 熔断冷却时长（超过后半开放行探测）。
    pub cooldown: Duration,
    /// per-server 两次调用的最小间隔（0 = 关闭限流）。
    pub min_call_interval: Duration,
    /// 只读工具失败后的幂等重试次数上限。
    pub read_retries: u32,
}

impl Default for McpHealthConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
            // 默认关闭：串行执行下同 server 天然不并发，避免改变既有时序。
            min_call_interval: Duration::ZERO,
            read_retries: 1,
        }
    }
}

#[derive(Debug, Default)]
struct ServerHealth {
    consecutive_failures: u32,
    total_failures: u64,
    /// §10 服务状态：总成功调用数（失败率分母）。
    total_success: u64,
    /// §10 服务状态：最近调用时延样本（毫秒；环形截断，供 p50/p95）。
    latency_samples: std::collections::VecDeque<u64>,
    /// §10 服务状态：历史熔断开路次数。
    circuit_open_count: u32,
    /// §10 服务状态：最近一次失败错误原文（截断存储）。
    last_error: String,
    circuit_opened_at: Option<Instant>,
    /// 半开探测中：单个失败即重新开路。
    half_open: bool,
    last_call_at: Option<Instant>,
}

/// 时延样本环形容量：只保留最近 32 次（内存有界）。
const LATENCY_SAMPLE_CAP: usize = 32;

/// 调用前检查失败原因（快速失败，不触碰 server 进程）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpCallBlocked {
    #[error("MCP server {server} 熔断开路中，剩余冷却 {remaining_ms}ms（§10.2）")]
    CircuitOpen { server: String, remaining_ms: u64 },
    #[error("MCP server {server} 被限流节流，需再等 {remaining_ms}ms（§10.3）")]
    RateLimited { server: String, remaining_ms: u64 },
}

/// §10.3：MCP 调用错误分类——决定只读工具是否值得幂等重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpErrorClass {
    /// 瞬态（超时/连接抖动/进程重启窗口）：可幂等重试。
    Transient,
    /// 永久（工具不存在/参数错误/权限拒绝）：重试无意义。
    Permanent,
}

/// 按错误文案关键词分类（MCP 错误为字符串；保守启发式：
/// 只有明确命中瞬态标记才判 Transient，其余一律 Permanent——宁可漏重试，
/// 不可对永久错误空转）。
pub fn classify_error(error: &str) -> McpErrorClass {
    let lowered = error.to_ascii_lowercase();
    const TRANSIENT_MARKERS: &[&str] = &[
        "timeout",
        "timed out",
        "超时",
        "connection",
        "connect",
        "reset",
        "eof",
        "broken pipe",
        "not running",
        "busy",
        "unavailable",
        "temporarily",
        "502",
        "503",
        "504",
        "重启",
        "中断",
    ];
    if TRANSIENT_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
    {
        McpErrorClass::Transient
    } else {
        McpErrorClass::Permanent
    }
}

/// per-server 健康跟踪器（§10.1–10.3；Agent 级共享，线程安全）。
#[derive(Debug)]
pub struct McpHealthTracker {
    config: McpHealthConfig,
    servers: Mutex<HashMap<String, ServerHealth>>,
}

impl McpHealthTracker {
    pub fn new(config: McpHealthConfig) -> Self {
        Self {
            config,
            servers: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, ServerHealth>> {
        self.servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 调用前检查：熔断开路 / 限流窗口内 → `Err`；通过则登记本次调用时刻。
    /// 冷却结束的 server 在此进入半开状态（放行探测）。
    pub fn check_call(&self, server: &str) -> Result<(), McpCallBlocked> {
        let mut servers = self.lock();
        let health = servers.entry(server.to_string()).or_default();
        if let Some(opened_at) = health.circuit_opened_at {
            let elapsed = opened_at.elapsed();
            if elapsed < self.config.cooldown {
                return Err(McpCallBlocked::CircuitOpen {
                    server: server.to_string(),
                    remaining_ms: (self.config.cooldown - elapsed).as_millis() as u64,
                });
            }
            // 冷却结束 → 半开：放行单个探测，失败即重新开路。
            health.circuit_opened_at = None;
            health.half_open = true;
            health.consecutive_failures = 0;
        }
        if self.config.min_call_interval > Duration::ZERO {
            if let Some(last) = health.last_call_at {
                let elapsed = last.elapsed();
                if elapsed < self.config.min_call_interval {
                    return Err(McpCallBlocked::RateLimited {
                        server: server.to_string(),
                        remaining_ms: (self.config.min_call_interval - elapsed).as_millis() as u64,
                    });
                }
            }
        }
        health.last_call_at = Some(Instant::now());
        Ok(())
    }

    /// 记录成功：清零计数、关闭断路（半开探测成功 → 恢复 Connected）。
    pub fn record_success(&self, server: &str) {
        self.record_success_with(server, None);
    }

    /// 记录成功并携带本次调用时延（§10 服务状态：p50/p95 数据源）。
    pub fn record_success_with(&self, server: &str, elapsed: Option<Duration>) {
        let mut servers = self.lock();
        let health = servers.entry(server.to_string()).or_default();
        health.consecutive_failures = 0;
        health.half_open = false;
        health.circuit_opened_at = None;
        health.total_success += 1;
        record_latency(&mut health.latency_samples, elapsed);
    }

    /// 记录失败：累计连续失败；达阈值（或半开探测失败）→ 熔断开路。
    /// 返回本次是否触发了熔断。
    pub fn record_failure(&self, server: &str) -> bool {
        self.record_failure_with(server, None, "")
    }

    /// 记录失败并携带本次调用时延与错误原文（§10 服务状态：失败率/最近原因）。
    pub fn record_failure_with(
        &self,
        server: &str,
        elapsed: Option<Duration>,
        error: &str,
    ) -> bool {
        let mut servers = self.lock();
        let health = servers.entry(server.to_string()).or_default();
        health.consecutive_failures += 1;
        health.total_failures += 1;
        health.last_error = trunc_error(error);
        record_latency(&mut health.latency_samples, elapsed);
        if health.circuit_opened_at.is_some() {
            return false; // 已开路（并发记账窗口），不重复触发。
        }
        if health.half_open || health.consecutive_failures >= self.config.failure_threshold {
            health.half_open = false;
            health.circuit_opened_at = Some(Instant::now());
            health.circuit_open_count += 1;
            return true;
        }
        false
    }

    /// 幂等重试上限：仅只读工具重试（§10.3）；写/执行/未知 effect 一律 0。
    pub fn retries_for(&self, effect_class: Option<&EffectClass>) -> u32 {
        match effect_class {
            Some(EffectClass::Read) => self.config.read_retries,
            _ => 0,
        }
    }

    /// 观测快照：按 server 名称排序（§10 可观测面；经 /mcp/health 暴露）。
    pub fn snapshot(&self) -> Vec<McpServerHealthSnapshot> {
        let servers = self.lock();
        let mut rows: Vec<McpServerHealthSnapshot> = servers
            .iter()
            .map(|(name, health)| {
                let state = if health.circuit_opened_at.is_some() {
                    McpServerState::CircuitOpen
                } else if health.consecutive_failures > 0 {
                    McpServerState::Degraded
                } else {
                    McpServerState::Connected
                };
                let total_calls = health.total_success + health.total_failures;
                let failure_rate = if total_calls > 0 {
                    health.total_failures as f64 / total_calls as f64
                } else {
                    0.0
                };
                let (p50_ms, p95_ms) = percentiles(&health.latency_samples);
                McpServerHealthSnapshot {
                    server: name.clone(),
                    state,
                    consecutive_failures: health.consecutive_failures,
                    total_failures: health.total_failures,
                    // §10 服务状态扩展：失败率 / 分位延迟 / 熔断次数 / 最近错误。
                    total_calls,
                    failure_rate,
                    p50_ms,
                    p95_ms,
                    circuit_open_count: health.circuit_open_count,
                    last_error: health.last_error.clone(),
                }
            })
            .collect();
        rows.sort_by(|a, b| a.server.cmp(&b.server));
        rows
    }
}

/// 记录一次时延样本（环形截断；None = 未测量，跳过）。
fn record_latency(samples: &mut std::collections::VecDeque<u64>, elapsed: Option<Duration>) {
    if let Some(elapsed) = elapsed {
        if samples.len() >= LATENCY_SAMPLE_CAP {
            samples.pop_front();
        }
        samples.push_back(elapsed.as_millis() as u64);
    }
}

/// 最近错误截断（快照只保留摘要，避免错误正文撑爆观测面）。
fn trunc_error(error: &str) -> String {
    const MAX: usize = 200;
    let error = error.trim();
    if error.chars().count() <= MAX {
        error.to_string()
    } else {
        let mut out: String = error.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

/// p50 / p95（毫秒；最近最近邻秩，样本空时为 None）。
fn percentiles(samples: &std::collections::VecDeque<u64>) -> (Option<u64>, Option<u64>) {
    if samples.is_empty() {
        return (None, None);
    }
    let mut sorted: Vec<u64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let pick = |quantile: f64| -> u64 {
        let rank = ((sorted.len() as f64) * quantile).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    };
    (Some(pick(0.5)), Some(pick(0.95)))
}

/// 观测快照行（§10 可观测面；序列化给 server/UI 消费）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerHealthSnapshot {
    pub server: String,
    pub state: McpServerState,
    pub consecutive_failures: u32,
    pub total_failures: u64,
    /// §10 服务状态：总调用数（成功 + 失败）。
    #[serde(default)]
    pub total_calls: u64,
    /// §10 服务状态：失败率（总失败 / 总调用；无调用为 0.0）。
    #[serde(default)]
    pub failure_rate: f64,
    /// §10 服务状态：最近调用时延 p50（毫秒；无样本为 None）。
    #[serde(default)]
    pub p50_ms: Option<u64>,
    /// §10 服务状态：最近调用时延 p95（毫秒；无样本为 None）。
    #[serde(default)]
    pub p95_ms: Option<u64>,
    /// §10 服务状态：历史熔断开路次数。
    #[serde(default)]
    pub circuit_open_count: u32,
    /// §10 服务状态：最近一次失败错误原文（截断摘要）。
    #[serde(default)]
    pub last_error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(config: McpHealthConfig) -> McpHealthTracker {
        McpHealthTracker::new(config)
    }

    #[test]
    fn breaker_opens_after_threshold_and_fails_fast() {
        let health = tracker(McpHealthConfig::default());
        for _ in 0..(McpHealthConfig::default().failure_threshold - 1) {
            health.record_failure("srv");
        }
        assert!(health.check_call("srv").is_ok(), "未达阈值前应放行");
        let opened = health.record_failure("srv");
        assert!(opened, "第 3 次连续失败应触发熔断");
        let blocked = health.check_call("srv").unwrap_err();
        assert!(
            matches!(blocked, McpCallBlocked::CircuitOpen { .. }),
            "开路期间必须快速失败：{blocked}"
        );
        assert!(blocked.to_string().contains("熔断"));
    }

    #[test]
    fn half_open_probe_succeeds_or_reopens_immediately() {
        let health = tracker(McpHealthConfig {
            cooldown: Duration::from_millis(20),
            ..McpHealthConfig::default()
        });
        for _ in 0..3 {
            health.record_failure("srv");
        }
        assert!(health.check_call("srv").is_err());
        std::thread::sleep(Duration::from_millis(30));
        assert!(health.check_call("srv").is_ok(), "冷却结束后应半开放行探测");
        // 半开探测失败 → 立即重新开路（不等阈值重算）。
        let reopened = health.record_failure("srv");
        assert!(reopened, "半开探测失败必须立即重新开路");
        assert!(health.check_call("srv").is_err());
    }

    #[test]
    fn success_resets_consecutive_failures() {
        let health = tracker(McpHealthConfig::default());
        for _ in 0..2 {
            health.record_failure("srv");
        }
        health.record_success("srv");
        for _ in 0..2 {
            health.record_failure("srv");
        }
        assert!(health.check_call("srv").is_ok(), "成功清零后不应误熔断");
        let rows = health.snapshot();
        assert_eq!(rows.len(), 1, "快照应有一行");
        assert_eq!(rows[0].server, "srv");
        assert_eq!(rows[0].state, McpServerState::Degraded);
        assert_eq!(rows[0].consecutive_failures, 2, "连续失败应反映 Degraded");
        assert_eq!(rows[0].total_failures, 4, "总失败应累计（不清零）");
    }

    #[test]
    fn rate_limit_throttles_burst_when_configured() {
        let health = tracker(McpHealthConfig {
            min_call_interval: Duration::from_millis(40),
            ..McpHealthConfig::default()
        });
        assert!(health.check_call("srv").is_ok());
        let blocked = health.check_call("srv").unwrap_err();
        assert!(
            matches!(blocked, McpCallBlocked::RateLimited { .. }),
            "窗口内第二次调用应被节流：{blocked}"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(health.check_call("srv").is_ok(), "窗口过后应放行");
    }

    #[test]
    fn only_read_effects_are_retryable() {
        let health = tracker(McpHealthConfig::default());
        assert_eq!(
            health.retries_for(Some(&EffectClass::Read)),
            McpHealthConfig::default().read_retries,
            "只读工具可幂等重试"
        );
        assert_eq!(
            health.retries_for(Some(&EffectClass::Write)),
            0,
            "写工具不重试"
        );
        assert_eq!(
            health.retries_for(Some(&EffectClass::Execute)),
            0,
            "执行工具不重试"
        );
        assert_eq!(health.retries_for(None), 0, "未知 effect 不重试");
    }

    #[test]
    fn default_config_keeps_rate_limit_off() {
        // 默认关闭限流：连续调用不受节流（既有测试/时序零破坏）。
        let health = tracker(McpHealthConfig::default());
        for _ in 0..5 {
            assert!(health.check_call("srv").is_ok());
        }
    }

    /// §10 服务状态：时延分位（p50/p95）、失败率、熔断次数与最近错误进入快照；
    /// 并冻结状态机语义：未达阈值 Degraded 计数 0，恰在阈值触发一次开路，
    /// 开路窗口内失败不重复计次（默认 failure_threshold=3）。
    #[test]
    fn snapshot_carries_latency_percentiles_failure_rate_and_last_error() {
        let health = tracker(McpHealthConfig::default());
        // 5 成功（1..=5ms）。
        for ms in 1..=5u64 {
            health.record_success_with("srv", Some(Duration::from_millis(ms)));
        }
        // 未达阈值（2 连续失败 < 3）：Degraded 放行观察，不计熔断。
        for _ in 0..2 {
            health.record_failure_with(
                "srv",
                Some(Duration::from_millis(20)),
                "request timeout after 30s",
            );
        }
        let row = &health.snapshot()[0];
        assert_eq!(row.total_calls, 7);
        assert_eq!(row.consecutive_failures, 2);
        assert_eq!(row.circuit_open_count, 0, "未达阈值不应计熔断");
        assert_eq!(row.state, McpServerState::Degraded, "未达阈值应为 Degraded");
        // 第 3 个连续失败 → 恰达阈值，触发一次开路（§10.2）。
        health.record_failure_with(
            "srv",
            Some(Duration::from_millis(20)),
            "request timeout after 30s",
        );
        let row = &health.snapshot()[0];
        assert_eq!(row.total_calls, 8);
        assert!(
            (row.failure_rate - 3.0 / 8.0).abs() < 1e-9,
            "失败率应为 3/8"
        );
        assert_eq!(row.p95_ms, Some(20), "p95 应落在最慢样本 20ms");
        assert!(
            row.p50_ms.is_some_and(|v| (1..=5).contains(&v)),
            "p50 应在成功样本带内"
        );
        assert_eq!(row.circuit_open_count, 1, "阈值触发应恰好计一次开路");
        assert_eq!(row.state, McpServerState::CircuitOpen);
        assert!(row.last_error.contains("timeout"), "最近错误应入快照");
        // 开路窗口内继续失败：状态保持 CircuitOpen，但不重复计开路次数。
        for _ in 0..3 {
            health.record_failure_with("srv", Some(Duration::from_millis(20)), "boom");
        }
        let row = &health.snapshot()[0];
        assert_eq!(row.circuit_open_count, 1, "开路窗口内失败不得重复触发");
        assert_eq!(row.state, McpServerState::CircuitOpen);
    }

    #[test]
    fn error_classification_is_conservative() {
        // 瞬态：超时/连接/重启窗口。
        assert_eq!(
            classify_error("MCP 工具 f 失败：request timeout after 30s"),
            McpErrorClass::Transient
        );
        assert_eq!(
            classify_error("connection reset by peer"),
            McpErrorClass::Transient
        );
        assert_eq!(
            classify_error("server 进程已重启"),
            McpErrorClass::Transient
        );
        // 永久：工具缺失/参数/权限——重试无意义。
        assert_eq!(classify_error("未知工具：nope"), McpErrorClass::Permanent);
        assert_eq!(
            classify_error("invalid arguments"),
            McpErrorClass::Permanent
        );
        assert_eq!(
            classify_error("permission denied"),
            McpErrorClass::Permanent
        );
        // 未知文案保守判永久（宁可漏重试，不空转）。
        assert_eq!(
            classify_error("some weird failure"),
            McpErrorClass::Permanent
        );
    }
}
