use axum::http::StatusCode;
use axum::Json;
use owo_agent_core::list_traces;
use owo_agent_core::load_trace;
use owo_agent_core::trace::TraceRecord;
use owo_agent_core::TurnEvent;
use owo_agent_server::AppState;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
pub(crate) type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// 加载 traces 目录全部记录（不可用路径静默跳过）。
pub(crate) fn load_all_traces(state: &AppState) -> Vec<TraceRecord> {
    list_traces(&state.traces_dir)
        .iter()
        .filter_map(|path| load_trace(path).ok())
        .collect()
}

/// 工具调度延迟样本上限（环形丢弃最旧）。
const TOOL_SAMPLES_CAP: usize = 1000;

/// 运行时指标注册表（R6）：由接线方（event_stream 接线、工具调度层）调用
/// `record_*` 填充；查询端空数据一律返回 null/0，不 panic。
pub struct RuntimeMetrics {
    pub(crate) tool_durations_ms: Mutex<Vec<u64>>,
    pub(crate) sse_active: AtomicU64,
    pub(crate) sse_total_connections: AtomicU64,
    pub(crate) sse_lagged: AtomicU64,
    pub(crate) queue_depth: AtomicU64,
    pub(crate) events_published: AtomicU64,
    pub(crate) events_dropped: AtomicU64,
    pub(crate) turn_sse_slow_consumers: AtomicU64,
    pub(crate) turn_sse_disconnects: AtomicU64,
}

impl Default for RuntimeMetrics {
    fn default() -> Self {
        Self {
            tool_durations_ms: Mutex::new(Vec::with_capacity(TOOL_SAMPLES_CAP)),
            sse_active: AtomicU64::new(0),
            sse_total_connections: AtomicU64::new(0),
            sse_lagged: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            events_published: AtomicU64::new(0),
            events_dropped: AtomicU64::new(0),
            turn_sse_slow_consumers: AtomicU64::new(0),
            turn_sse_disconnects: AtomicU64::new(0),
        }
    }
}

static RUNTIME: Mutex<Option<RuntimeMetrics>> = Mutex::new(None);

pub(crate) fn with_runtime<T>(f: impl FnOnce(&RuntimeMetrics) -> T) -> T {
    let mut guard = RUNTIME.lock().unwrap_or_else(|e| e.into_inner());
    let metrics = guard.get_or_insert_with(RuntimeMetrics::default);
    f(metrics)
}

/// 记录一次工具调度耗时（ms）。超上限时丢弃最旧样本。
pub fn record_tool_duration_ms(duration_ms: u64) {
    with_runtime(|metrics| {
        let mut samples = metrics
            .tool_durations_ms
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        samples.push(duration_ms);
        while samples.len() > TOOL_SAMPLES_CAP {
            samples.remove(0);
        }
    });
}

/// 记录 SSE 连接增减（open=+1 / close=-1）。
#[allow(dead_code)] // 生产数据面已由 R7 MetricsSample 桥（lib.rs set_metrics_observer → ingest_metrics_sample）覆盖，直接调用会双重计数；保留仅供 observability_tests 以 #[path] 独立编译播种状态。
pub fn record_sse_connection(delta: i64) {
    with_runtime(|metrics| {
        if delta > 0 {
            metrics
                .sse_total_connections
                .fetch_add(delta as u64, Ordering::Relaxed);
        }
        let active = if delta < 0 {
            metrics
                .sse_active
                .load(Ordering::Relaxed)
                .saturating_sub(delta.unsigned_abs())
        } else {
            metrics.sse_active.load(Ordering::Relaxed) + delta as u64
        };
        metrics.sse_active.store(active, Ordering::Relaxed);
    });
}

/// 记录订阅队列深度（当前值快照）。
#[allow(dead_code)] // 生产数据面已由 R7 MetricsSample 桥覆盖（queue_depth 随样本快照更新），直接调用会双重计数；保留仅供 observability_tests 以 #[path] 独立编译播种状态。
pub fn record_queue_depth(depth: u64) {
    with_runtime(|metrics| metrics.queue_depth.store(depth, Ordering::Relaxed));
}

/// 记录事件流发布/丢弃计数。
#[allow(dead_code)] // 生产数据面已由 R7 MetricsSample 桥覆盖（published/dropped 随样本累加），直接调用会双重计数；保留仅供 observability_tests 以 #[path] 独立编译播种状态。
pub fn record_events(published: u64, dropped: u64) {
    with_runtime(|metrics| {
        metrics
            .events_published
            .fetch_add(published, Ordering::Relaxed);
        metrics.events_dropped.fetch_add(dropped, Ordering::Relaxed);
    });
}

/// Record a turn SSE client whose bounded queue overflowed and caused the turn to abort.
pub fn record_turn_sse_slow_consumer() {
    with_runtime(|metrics| {
        metrics
            .turn_sse_slow_consumers
            .fetch_add(1, Ordering::Relaxed);
    });
}

/// Record a turn SSE client disconnect observed by the event producer.
pub fn record_turn_sse_disconnect() {
    with_runtime(|metrics| {
        metrics.turn_sse_disconnects.fetch_add(1, Ordering::Relaxed);
    });
}

/// 消费 event_stream 指标钩子样本（R7 桥接）：
/// 解析 `event_stream::MetricsSample::to_json()` 快照并更新运行时注册表。
/// 与 event_stream 解耦（双方互不引用类型），主控接线：`event_stream::set_metrics_observer(closure)`，
/// closure 内 `observability_api::ingest_metrics_sample(&sample.to_json())`。
pub fn ingest_metrics_sample(sample: &Value) {
    with_runtime(|metrics| {
        if let Some(v) = sample.get("conn_opened").and_then(|v| v.as_u64()) {
            metrics
                .sse_total_connections
                .fetch_add(v, Ordering::Relaxed);
        }
        if let Some(v) = sample.get("active_connections").and_then(|v| v.as_u64()) {
            metrics.sse_active.store(v, Ordering::Relaxed);
        }
        if let Some(v) = sample.get("published").and_then(|v| v.as_u64()) {
            metrics.events_published.fetch_add(v, Ordering::Relaxed);
        }
        let dropped_mergeable = sample
            .get("dropped_mergeable")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let dropped_critical = sample
            .get("dropped_critical")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let dropped = dropped_mergeable + dropped_critical;
        if dropped > 0 {
            metrics.events_dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        if let Some(v) = sample.get("lagged").and_then(|v| v.as_u64()) {
            metrics.sse_lagged.fetch_add(v, Ordering::Relaxed);
        }
        if let Some(v) = sample.get("queue_depth").and_then(|v| v.as_u64()) {
            metrics.queue_depth.store(v, Ordering::Relaxed);
        }
    });
}

/// 仅供测试：重置运行时指标注册表（进程内跨测试隔离）。
#[allow(dead_code)] // 仅供 observability_tests 以 #[path] 独立编译调用；lib 目标内无引用。
pub fn reset_runtime_metrics_for_test() {
    *RUNTIME.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

#[cfg(test)]
#[allow(dead_code)] // turn_api 单元测试在库内目标中调用；observability_tests 以路径复用时不调用。
pub(crate) fn turn_sse_counts_for_test() -> (u64, u64) {
    with_runtime(|metrics| {
        (
            metrics.turn_sse_slow_consumers.load(Ordering::Relaxed),
            metrics.turn_sse_disconnects.load(Ordering::Relaxed),
        )
    })
}

/// 有序样本的百分位（0.0-1.0）。空样本返回 None。
pub(crate) fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let index = (((sorted.len() - 1) as f64) * p).round() as usize;
    Some(sorted[index])
}

/// 从 TraceRecord 事件流聚合工具调用与失败。
pub(crate) fn aggregate_events(
    traces: &[TraceRecord],
) -> (usize, usize, HashMap<String, (usize, usize)>) {
    let mut tool_calls = 0usize;
    let mut failures = 0usize;
    let mut tools: HashMap<String, (usize, usize)> = HashMap::new(); // tool -> (calls, fails)
    for trace in traces {
        for event in &trace.events {
            match event {
                TurnEvent::ToolStart { tool, .. } => {
                    tool_calls += 1;
                    tools.entry(tool.clone()).or_insert((0, 0)).0 += 1;
                }
                TurnEvent::ToolResult {
                    tool, ok: false, ..
                } => {
                    failures += 1;
                    tools.entry(tool.clone()).or_insert((0, 0)).1 += 1;
                }
                _ => {}
            }
        }
    }
    (tool_calls, failures, tools)
}

/// 从内存审计面统计审批：approvals_total = 全部审批请求，denied = 其中拒绝数。
pub(crate) fn approval_stats(state: &AppState) -> (usize, usize) {
    let mut approved = 0usize;
    let mut denied = 0usize;
    if let Ok(audit) = state.agent.audit_log().lock() {
        for entry in &audit.entries {
            if entry.event.contains("permission") || entry.tool.as_deref() == Some("approver") {
                match entry.approved {
                    Some(true) => approved += 1,
                    Some(false) => denied += 1,
                    None => {}
                }
            }
        }
    }
    (approved + denied, denied)
}
