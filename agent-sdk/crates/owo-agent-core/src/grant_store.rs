//! §5.4 有作用域、可撤销的授权记忆（Grant）。
//!
//! 用户审批时可选「此会话 / 此项目一小时 / 始终允许此只读动作」生成 Grant；
//! Grant 命中、拒绝（scope 不符）与过期都必须写审计（由调用方在 agent 层记录），
//! 并可在设置中逐条撤销。
//!
//! 安全边界：
//! - 破坏性/注入/越界请求不生成 Grant（approval 入口层把关）；
//! - Grant 只放宽「原本 Ask」的操作，永不越过 Policy 的 deny 规则与越界拒绝；
//! - 参数指纹命中才放行：同一工具同一参数摘要才可以免问，参数变化重新审批。
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::permissions::PermissionRequest;

/// 临时授权的范围与有效期选择（审批卡选项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    /// 仅本次：不生成 Grant，仅对当前请求放行（现状语义）。
    Once,
    /// 此会话：进程内有效（约 8 小时，进程退出即失效）。
    Session,
    /// 此项目一小时。
    OneHour,
    /// 始终允许此只读动作（无到期；仅限宿主验证只读级工具）。
    AlwaysReadOnly,
}

impl GrantScope {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantScope::Once => "once",
            GrantScope::Session => "session",
            GrantScope::OneHour => "one_hour",
            GrantScope::AlwaysReadOnly => "always_readonly",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "once" => Some(Self::Once),
            "session" => Some(Self::Session),
            "one_hour" => Some(Self::OneHour),
            "always_readonly" => Some(Self::AlwaysReadOnly),
            _ => None,
        }
    }
}

/// 一条可撤销的临时授权（§5.4 契约字段；密钥不落盘）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub grant_id: String,
    pub tool_id: String,
    pub workspace_id: String,
    pub path_scope: Option<PathBuf>,
    pub host_scope: Option<String>,
    pub argument_fingerprint: Option<String>,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub remaining_uses: Option<u32>,
}

impl Grant {
    #[cfg(test)]
    pub fn test(
        tool_id: &str,
        workspace_id: &str,
        path_scope: Option<PathBuf>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            grant_id: uuid::Uuid::new_v4().to_string(),
            tool_id: tool_id.to_string(),
            workspace_id: workspace_id.to_string(),
            path_scope,
            host_scope: None,
            argument_fingerprint: None,
            created_at: Utc::now(),
            expires_at,
            remaining_uses: Some(10),
        }
    }
}

/// 请求是否命中 Grant 的作用域（工具/工作区/路径/主机/参数指纹/有效期）。
pub fn grant_matches(grant: &Grant, request: &PermissionRequest, workspace_id: &str) -> bool {
    if grant.tool_id != request.tool {
        return false;
    }
    if grant.workspace_id != workspace_id {
        return false;
    }
    if let Some(expires_at) = grant.expires_at {
        if expires_at < Utc::now() {
            return false;
        }
    }
    if let Some(path_scope) = &grant.path_scope {
        let path = request
            .args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let scope_text = path_scope.to_string_lossy();
        if !path.starts_with(scope_text.as_ref()) {
            return false;
        }
        if let Some(file) = request.args.get("file").and_then(serde_json::Value::as_str) {
            if !file.starts_with(scope_text.as_ref()) {
                return false;
            }
        }
    }
    if let Some(host_scope) = &grant.host_scope {
        let url = request
            .args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if url.is_empty() || !url.contains(host_scope.as_str()) {
            return false;
        }
    }
    if let Some(fingerprint) = &grant.argument_fingerprint {
        if fingerprint_args(&request.args).as_deref() != Some(fingerprint.as_str()) {
            return false;
        }
    }
    true
}

/// 参数摘要指纹：对 args 的稳定 JSON 排序序列化后取 sha256 十六进制。
/// 指纹相同才命中 Grant（同一工具同一参数），避免「授权过一会却放行了别的参数」。
pub fn fingerprint_args(args: &serde_json::Value) -> Option<String> {
    use sha2::{Digest, Sha256};
    let canonical = canonicalize(args)?;
    let serialized = serde_json::to_vec(&canonical).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&serialized);
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// 稳定规范化：对象键排序（BTreeMap 天然排序），数组保持顺序。
fn canonicalize(value: &serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                out.insert(key.clone(), canonicalize(value)?);
            }
            Some(serde_json::Value::Object(out))
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(canonicalize(item)?);
            }
            Some(serde_json::Value::Array(out))
        }
        other => Some(other.clone()),
    }
}

/// Grant 存储：按 tool_id 索引，保证线程安全；容量有上限（防止无限膨胀）。
pub struct GrantStore {
    inner: RwLock<BTreeMap<String, Vec<Grant>>>,
    /// 单工具最多持有 grant 数（超出时丢弃最旧的）。
    per_tool_cap: usize,
}

impl Default for GrantStore {
    fn default() -> Self {
        Self {
            inner: RwLock::new(BTreeMap::new()),
            per_tool_cap: 16,
        }
    }
}

impl GrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, grant: Grant) {
        if let Ok(mut inner) = self.inner.write() {
            let bucket = inner.entry(grant.tool_id.clone()).or_default();
            // 容量守卫：先淘汰过期/用尽的，再超出则丢弃最旧。
            bucket.retain(|existing| {
                !existing_expired(existing) && existing.remaining_uses != Some(0)
            });
            if bucket.len() >= self.per_tool_cap {
                bucket.remove(0);
            }
            bucket.push(grant);
        }
    }

    /// 命中并消费：返回匹配的 Grant（复制），同时递减 remaining_uses。
    /// 未命中 / 命中但已过期 / 参数指纹不符 → None。
    pub fn consume(&self, request: &PermissionRequest, workspace_id: &str) -> Option<Grant> {
        let mut hit = None;
        if let Ok(mut inner) = self.inner.write() {
            if let Some(bucket) = inner.get_mut(&request.tool) {
                bucket.retain(|existing| !existing_expired(existing));
                if let Some(index) = bucket.iter().position(|grant| {
                    grant_matches(grant, request, workspace_id) && grant.remaining_uses != Some(0)
                }) {
                    let mut grant = bucket[index].clone();
                    if let Some(uses) = grant.remaining_uses.as_mut() {
                        *uses = uses.saturating_sub(1);
                    }
                    if grant.remaining_uses == Some(0) {
                        bucket.remove(index);
                    } else {
                        bucket[index] = grant.clone();
                    }
                    hit = Some(grant);
                }
            }
        }
        hit
    }

    /// 是否可能命中（不消费；诊断/审计用）：任一 grant 的参数指纹与请求一致。
    pub fn peek(&self, request: &PermissionRequest, workspace_id: &str) -> bool {
        self.consume_peek_only(request, workspace_id)
    }

    fn consume_peek_only(&self, request: &PermissionRequest, workspace_id: &str) -> bool {
        if let Ok(inner) = self.inner.read() {
            if let Some(bucket) = inner.get(&request.tool) {
                return bucket
                    .iter()
                    .any(|grant| grant_matches(grant, request, workspace_id));
            }
        }
        false
    }

    /// 撤销单条授权；返回是否确实撤销。
    pub fn revoke(&self, grant_id: &str) -> bool {
        if let Ok(mut inner) = self.inner.write() {
            for bucket in inner.values_mut() {
                if let Some(index) = bucket.iter().position(|grant| grant.grant_id == grant_id) {
                    bucket.remove(index);
                    return true;
                }
            }
        }
        false
    }

    /// 撤销某工具全部授权（工具卸载时清理）。
    pub fn revoke_tool(&self, tool_id: &str) -> usize {
        if let Ok(mut inner) = self.inner.write() {
            let removed = inner
                .remove(tool_id)
                .map(|bucket| bucket.len())
                .unwrap_or(0);
            return removed;
        }
        0
    }

    /// 全部未过期 grant 快照（设置页展示；可逐条撤销）。
    pub fn list(&self) -> Vec<Grant> {
        if let Ok(inner) = self.inner.read() {
            let mut out: Vec<Grant> = inner
                .values()
                .flatten()
                .filter(|grant| !existing_expired(grant))
                .cloned()
                .collect();
            out.sort_by_key(|grant| grant.created_at);
            return out;
        }
        Vec::new()
    }

    /// 清理过期条目；返回清理数量。
    pub fn prune_expired(&self) -> usize {
        if let Ok(mut inner) = self.inner.write() {
            let mut removed = 0;
            for bucket in inner.values_mut() {
                let before = bucket.len();
                bucket.retain(|grant| !existing_expired(grant));
                removed += before - bucket.len();
            }
            return removed;
        }
        0
    }

    /// 由审批选项生成 Grant（scope=Once 返回 None，不生成）。
    pub fn grant_from_scope(
        &self,
        request: &PermissionRequest,
        workspace_id: &str,
        scope: GrantScope,
    ) -> Option<Grant> {
        if scope == GrantScope::Once {
            return None;
        }
        let path_scope = request
            .args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .map(|path| {
                if path.is_dir() {
                    path
                } else {
                    path.parent().map(PathBuf::from).unwrap_or(path)
                }
            });
        let host_scope = request
            .args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .and_then(host_of);
        Some(Grant {
            grant_id: uuid::Uuid::new_v4().to_string(),
            tool_id: request.tool.clone(),
            workspace_id: workspace_id.to_string(),
            path_scope,
            host_scope,
            argument_fingerprint: fingerprint_args(&request.args),
            created_at: Utc::now(),
            expires_at: match scope {
                GrantScope::Once => None,
                GrantScope::Session => Some(Utc::now() + Duration::hours(8)),
                GrantScope::OneHour => Some(Utc::now() + Duration::hours(1)),
                GrantScope::AlwaysReadOnly => None,
            },
            remaining_uses: match scope {
                GrantScope::Once => None,
                GrantScope::Session => Some(50),
                GrantScope::OneHour => Some(50),
                GrantScope::AlwaysReadOnly => None,
            },
        })
    }
}

fn existing_expired(grant: &Grant) -> bool {
    grant
        .expires_at
        .map(|expires| expires < Utc::now())
        .unwrap_or(false)
}

/// 提取 URL 的 host 段用于授权作用域（不解析外部请求；仅做前缀匹配基准）。
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', ':', '?', '#']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

// 统一导出 Arc 包装（Policy 内持有）。
pub type SharedGrantStore = Arc<GrantStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Level;
    use serde_json::{json, Value};

    fn request(tool: &str, args: Value, level: Level) -> PermissionRequest {
        PermissionRequest::new(tool, args, level, "test")
    }

    #[test]
    fn fingerprint_is_stable_across_key_order() {
        let left = json!({ "path": "a/b", "recursive": true });
        let right = json!({ "recursive": true, "path": "a/b" });
        assert_eq!(fingerprint_args(&left), fingerprint_args(&right));
        assert!(
            fingerprint_args(&left).unwrap().len() == 64,
            "sha256 hex 长度"
        );
    }

    #[test]
    fn grant_hits_same_tool_workspace_and_fingerprint() {
        let store = GrantStore::new();
        let req = request("read_file", json!({ "path": "a/b.txt" }), Level::Read);
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::OneHour)
            .expect("one_hour 生成 grant");
        store.insert(grant);
        assert!(store.consume(&req, "ws-1").is_some(), "同参命中");
        // 参数变化 → 指纹不符 → 未命中。
        let other = request("read_file", json!({ "path": "secret.txt" }), Level::Read);
        assert!(store.consume(&other, "ws-1").is_none());
    }

    #[test]
    fn grant_respects_workspace_scope() {
        let store = GrantStore::new();
        let req = request("write_file", json!({ "path": "a.txt" }), Level::Write);
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::Session)
            .unwrap();
        store.insert(grant);
        assert!(store.consume(&req, "ws-2").is_none(), "异工作区不可复用");
        assert!(store.consume(&req, "ws-1").is_some());
    }

    #[test]
    fn consumed_uses_decrement_and_exhaust() {
        let store = GrantStore::new();
        let mut req = request("run_command", json!({ "command": "ls" }), Level::Execute);
        let mut grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::Session)
            .unwrap();
        grant.remaining_uses = Some(2);
        store.insert(grant);
        assert!(store.consume(&req, "ws-1").is_some());
        assert!(store.consume(&req, "ws-1").is_some());
        req.args = json!({ "command": "ls" });
        assert!(store.consume(&req, "ws-1").is_none(), "用尽后不再放行");
    }

    #[test]
    fn expired_grant_is_not_consumed() {
        let store = GrantStore::new();
        let req = request("read_file", json!({ "path": "a.txt" }), Level::Read);
        let mut grant = Grant::test(
            "read_file",
            "ws-1",
            Some(PathBuf::from("a")),
            Some(Utc::now() - Duration::seconds(1)),
        );
        grant.expires_at = Some(Utc::now() - Duration::seconds(1));
        store.insert(grant);
        assert!(store.consume(&req, "ws-1").is_none(), "过期不命中");
        assert_eq!(store.list().len(), 0, "过期 grant 不进入列表");
    }

    #[test]
    fn revoke_removes_grant() {
        let store = GrantStore::new();
        let req = request("read_file", json!({ "path": "a.txt" }), Level::Read);
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::AlwaysReadOnly)
            .unwrap();
        let id = grant.grant_id.clone();
        store.insert(grant);
        assert!(store.revoke(&id), "撤销必须返回 true");
        assert!(!store.revoke(&id), "重复撤销返回 false");
        assert!(store.consume(&req, "ws-1").is_none());
    }

    #[test]
    fn always_read_only_grant_has_no_expiry_and_no_uses_limit() {
        let store = GrantStore::new();
        let req = request("read_file", json!({ "path": "a.txt" }), Level::Read);
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::AlwaysReadOnly)
            .unwrap();
        assert_eq!(grant.expires_at, None);
        assert_eq!(grant.remaining_uses, None);
    }

    #[test]
    fn once_scope_creates_no_grant() {
        let store = GrantStore::new();
        let req = request("read_file", json!({ "path": "a.txt" }), Level::Read);
        assert!(store
            .grant_from_scope(&req, "ws-1", GrantScope::Once)
            .is_none());
    }

    #[test]
    fn host_scope_matches_url_prefix() {
        let store = GrantStore::new();
        let req = request(
            "browser_navigate",
            json!({ "url": "https://example.com/page" }),
            Level::Execute,
        );
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::Session)
            .unwrap();
        assert_eq!(grant.host_scope.as_deref(), Some("example.com"));
        let same = request(
            "browser_navigate",
            json!({ "url": "https://example.com/other" }),
            Level::Execute,
        );
        assert!(
            store.consume(&same, "ws-1").is_none(),
            "同 host 不同参数（指纹不同）不命中"
        );
    }

    #[test]
    fn grant_scope_parse_roundtrip() {
        assert_eq!(GrantScope::parse("once"), Some(GrantScope::Once));
        assert_eq!(GrantScope::parse("session"), Some(GrantScope::Session));
        assert_eq!(GrantScope::parse("one_hour"), Some(GrantScope::OneHour));
        assert_eq!(
            GrantScope::parse("always_readonly"),
            Some(GrantScope::AlwaysReadOnly)
        );
        assert_eq!(GrantScope::parse("forever"), None);
        assert_eq!(GrantScope::Once.as_str(), "once");
    }

    #[test]
    fn per_tool_capacity_is_bounded() {
        let store = GrantStore::new();
        for index in 0..40 {
            let req = request(
                "read_file",
                json!({ "path": format!("a{index}.txt") }),
                Level::Read,
            );
            let grant = store
                .grant_from_scope(&req, "ws-1", GrantScope::OneHour)
                .unwrap();
            store.insert(grant);
        }
        assert!(
            store.list().len() <= 16,
            "容量必须有上限：{}",
            store.list().len()
        );
    }
}
