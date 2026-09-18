//! §8.1（R3）开发诊断安全请求记录（request ledger）。
//!
//! 目的：真实桌面冷启动/错误恢复验收（§8.2/§8.3）需要**服务端权威**的
//! HTTP 事实——「首次可输入前真实 HTTP 总数 ≤5」「窗口隐藏 5 分钟业务
//! 请求为 0」「core 重启后只重新引导一次」全部以本 ledger 为准，浏览器
//! 侧计数或模拟不再作为证据。
//!
//! 硬约束（方案 §8.1，字段白名单即契约）：每条记录**只允许**
//! `method / route_template / started_at / duration_ms / status / source`
//! 六字段。**禁止**记录 Authorization、配对密钥、查询串、请求体、响应体、
//! 完整私人路径。route_template 取 axum `MatchedPath`（路由模板，参数段
//! 恒为 `{id}` 形态，不携带真实 id）；未匹配路由（fallback 静态资产服务）
//! **不进 ledger**——静态资产不计入业务请求，避免污染 ≤5 口径。
//!
//! 存储：data_root 键控注册表（与 notes_api/memory_graph 同一并行测试安全
//! 模式，不允许给 AppState 加字段）；环形缓冲上限 [`LEDGER_CAP`]，
//! 溢出丢最旧（total 仍单调累计）。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

/// 环形缓冲上限（溢出丢最旧；total 不丢）。
pub const LEDGER_CAP: usize = 512;

/// 客户端自报来源头名（web/shell/cli；值经 [`sanitize_source`] 白名单过滤，
/// CORS 放行清单与中间件共用此常量）。
pub const CLIENT_HEADER: &str = "x-owo-client";

/// 单条请求记录——六字段白名单之外的任何数据禁止进入本结构。
#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub route_template: String,
    /// RFC3339（毫秒精度，本地时钟 UTC）。
    pub started_at: String,
    pub duration_ms: u64,
    pub status: u16,
    /// 客户端自报来源（`x-owo-client` 头，已消毒）：web/shell/cli/other。
    pub source: String,
}

impl RequestRecord {
    /// 契约形状（wire 与测试共用唯一渲染点）。
    pub fn to_json(&self) -> Value {
        json!({
            "method": self.method,
            "route_template": self.route_template,
            "started_at": self.started_at,
            "duration_ms": self.duration_ms,
            "status": self.status,
            "source": self.source,
        })
    }
}

#[derive(Default)]
struct Ledger {
    records: VecDeque<RequestRecord>,
    total: u64,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<Ledger>>>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Ledger>>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ledger_for(data_root: &Path) -> Arc<Mutex<Ledger>> {
    let mut map = registry().lock().unwrap_or_else(|p| p.into_inner());
    map.entry(data_root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(Ledger::default())))
        .clone()
}

/// 由 trace_id_middleware 在响应完成后调用（最外层，401/429 也被记录）。
pub fn record(data_root: &Path, rec: RequestRecord) {
    let ledger = ledger_for(data_root);
    let mut guard = ledger.lock().unwrap_or_else(|p| p.into_inner());
    guard.total += 1;
    if guard.records.len() >= LEDGER_CAP {
        guard.records.pop_front();
    }
    guard.records.push_back(rec);
}

/// `x-owo-client` 头消毒：仅 [a-z0-9_-]{1,32}，不合规 → "other"。
/// （头的存在性本身可选；未知/异常值绝不透传原文进 ledger。）
pub fn sanitize_source(raw: Option<&str>) -> String {
    match raw {
        Some(v)
            if !v.is_empty()
                && v.len() <= 32
                && v.chars().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
                }) =>
        {
            v.to_string()
        }
        _ => "other".to_string(),
    }
}

/// 聚合口径（验收断言直接消费，调用方不再各自重算）：
/// - `health`：/health 探测（壳就绪轮询与诊断页允许，不计业务）；
/// - `auth_token`：/auth/token 引导（token 交换）；
/// - `business`：其余全部路由（首屏 ≤5 与隐藏零业务都以此为分子）。
pub fn aggregates(records: &[RequestRecord]) -> Value {
    let health = records
        .iter()
        .filter(|r| r.route_template == "/health")
        .count();
    let auth = records
        .iter()
        .filter(|r| r.route_template == "/auth/token")
        .count();
    json!({
        "health": health,
        "auth_token": auth,
        "business": records.len().saturating_sub(health + auth),
    })
}

pub async fn requests_report(
    State(state): State<Arc<crate::AppState>>,
    Query(q): Query<ReportQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(200).clamp(1, LEDGER_CAP);
    let ledger = ledger_for(&state.data_root);
    let (total, window) = {
        let guard = ledger.lock().unwrap_or_else(|p| p.into_inner());
        let skip = guard.records.len().saturating_sub(limit);
        (
            guard.total,
            guard.records.iter().skip(skip).cloned().collect::<Vec<_>>(),
        )
    };
    // 聚合基于窗口内记录（验收脚本以 limit 拉全窗口，语义=「窗口内口径」）。
    Json(json!({
        "total": total,
        "returned": window.len(),
        "cap": LEDGER_CAP,
        "aggregates": aggregates(&window),
        "records": window.iter().map(RequestRecord::to_json).collect::<Vec<_>>(),
    }))
}

#[derive(serde::Deserialize)]
pub struct ReportQuery {
    pub limit: Option<usize>,
}

pub fn router(state: Arc<crate::AppState>) -> Router {
    Router::new()
        .route("/diagnostics/requests", get(requests_report))
        .with_state(state)
}

/// 测试隔离：清空指定 data_root 的 ledger（环形与 total 归零）。
pub fn reset_for_test(data_root: &Path) {
    let ledger = ledger_for(data_root);
    let mut guard = ledger.lock().unwrap_or_else(|p| p.into_inner());
    guard.records.clear();
    guard.total = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(template: &str) -> RequestRecord {
        RequestRecord {
            method: "GET".into(),
            route_template: template.into(),
            started_at: "2026-09-17T00:00:00.000Z".into(),
            duration_ms: 1,
            status: 200,
            source: "test".into(),
        }
    }

    #[test]
    fn record_shape_is_exactly_six_whitelisted_fields() {
        let v = rec("/health").to_json();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "duration_ms",
                "method",
                "route_template",
                "source",
                "started_at",
                "status"
            ]
        );
    }

    #[test]
    fn ring_caps_and_keeps_newest() {
        let dir = std::env::temp_dir().join(format!("owo-ledger-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        reset_for_test(&dir);
        for i in 0..(LEDGER_CAP + 40) {
            record(&dir, rec(&format!("/x/{i}")));
        }
        let ledger = ledger_for(&dir);
        let guard = ledger.lock().unwrap();
        assert_eq!(guard.records.len(), LEDGER_CAP, "环形上限");
        assert_eq!(guard.total, (LEDGER_CAP + 40) as u64, "total 单调累计");
        assert_eq!(
            guard.records.back().unwrap().route_template,
            format!("/x/{}", LEDGER_CAP + 39),
            "丢最旧保最新"
        );
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_sanitization_rejects_raw_pass_through() {
        assert_eq!(sanitize_source(Some("web")), "web");
        assert_eq!(sanitize_source(Some("cli-x")), "cli-x");
        assert_eq!(sanitize_source(None), "other");
        assert_eq!(sanitize_source(Some("")), "other");
        assert_eq!(
            sanitize_source(Some("Bearer s3cr3t")),
            "other",
            "含空格大写→拒"
        );
        assert_eq!(sanitize_source(Some("x".repeat(33).as_str())), "other");
    }

    #[test]
    fn aggregates_split_health_and_auth_from_business() {
        let recs = vec![
            rec("/health"),
            rec("/health"),
            rec("/auth/token"),
            rec("/sessions"),
            rec("/sessions/{id}"),
        ];
        assert_eq!(
            aggregates(&recs),
            json!({"health": 2, "auth_token": 1, "business": 2})
        );
    }
}
