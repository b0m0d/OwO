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
use std::path::{Path, PathBuf};
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
    /// §4.5.2「本任务」：审批卡四动作之一。
    ///
    /// 实现上等同于 `Session`（8h / 50 次，**不落盘**）。之所以单列一个变体
    /// 而不是直接复用 `Session`：字面量要和指南与前端对齐，且未来若给 Grant
    /// 加 run/task id，只需改这一处的有效期，不必再动前端与契约。
    Task,
    /// §4.5.2「工作区长期」：无到期、跨进程保留，只能在权限中心显式撤销。
    /// 与 `AlwaysReadOnly` 的区别是它不预设"只读"——因此 `Workspace::as_str()`
    /// 出现在授权列表里时，前端必须连同 `tool_id` 与参数指纹一起展示。
    Workspace,
}

impl GrantScope {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantScope::Once => "once",
            GrantScope::Session => "session",
            GrantScope::OneHour => "one_hour",
            GrantScope::AlwaysReadOnly => "always_readonly",
            GrantScope::Task => "task",
            GrantScope::Workspace => "workspace",
        }
    }

    /// 人类可读标签（审批卡与权限中心共用；口径见指南 §4.5.2）。
    pub fn label(self) -> &'static str {
        match self {
            GrantScope::Once => "仅本次",
            GrantScope::Session | GrantScope::Task => "本任务",
            GrantScope::OneHour => "一小时内",
            GrantScope::AlwaysReadOnly => "只读长期",
            GrantScope::Workspace => "工作区长期",
        }
    }

    /// 是否会跨进程重启保留（§4.5.2「工作区长期」的唯一落盘判据）。
    pub fn persists(self) -> bool {
        matches!(self, GrantScope::AlwaysReadOnly | GrantScope::Workspace)
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "once" => Some(Self::Once),
            "session" => Some(Self::Session),
            "one_hour" => Some(Self::OneHour),
            "always_readonly" => Some(Self::AlwaysReadOnly),
            "task" => Some(Self::Task),
            "workspace" => Some(Self::Workspace),
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
    /// §4.5.2 生成该授权时的审批选项字面量（`GrantScope::as_str`）。
    ///
    /// 只为展示与撤销而存在——判定仍然只看工具/工作区/路径/主机/参数指纹，
    /// 不让标签参与安全决策。旧数据缺此字段按 `None` 处理（不猜范围）。
    #[serde(default)]
    pub scope: Option<String>,
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
            scope: None,
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
    /// §4.5.2「工作区长期」落盘路径；`None` = 纯内存（测试与嵌入式用法零副作用）。
    persist_path: RwLock<Option<PathBuf>>,
}

impl Default for GrantStore {
    fn default() -> Self {
        Self {
            inner: RwLock::new(BTreeMap::new()),
            per_tool_cap: 16,
            persist_path: RwLock::new(None),
        }
    }
}

/// 落盘文件（`<data_root>/grants.json`）。版本不认识的条目一律不加载。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GrantsFile {
    version: u32,
    grants: Vec<Grant>,
}

/// 当前落盘格式版本。
const GRANTS_FILE_VERSION: u32 = 1;
/// 落盘条目上限：长期授权本应是少数，超限直接不写（保留既有条目，绝不静默丢新的）。
const GRANTS_PERSIST_CAP: usize = 128;

/// 是否属于"跨进程长期"授权：无到期且无次数上限。
///
/// 判据刻意只看有效期字段、不看 `scope` 标签——老数据没有标签也能被正确对待，
/// 而"本任务/此会话"这类有期限授权就算被写进文件也一定会在加载时过期作废，
/// 双保险不依赖任何一侧的自觉。
fn is_long_lived(grant: &Grant) -> bool {
    grant.expires_at.is_none() && grant.remaining_uses.is_none()
}

impl GrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// §4.5.2 开一个带落盘的存储：先加载长期授权（顺带清过期），再挂上路径。
    /// 文件不存在是正常态（首次启动）；JSON 损坏则改名为 `*.bad` 保留现场后按空启动。
    pub fn persisting(path: impl Into<PathBuf>) -> Self {
        let store = Self::new();
        let path = path.into();
        store.load_from(&path);
        if let Ok(mut current) = store.persist_path.write() {
            *current = Some(path);
        }
        store
    }

    /// 落盘路径（权限中心展示"长期授权是否会跨重启保留"）。
    pub fn persist_path(&self) -> Option<PathBuf> {
        self.persist_path
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    fn load_from(&self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return; // 首次启动：没有文件不是错误。
        };
        let parsed = match serde_json::from_str::<GrantsFile>(&text) {
            Ok(file) if file.version == GRANTS_FILE_VERSION => file,
            Ok(file) => {
                Self::quarantine(path, &format!("不支持的 grants 版本 {}", file.version));
                return;
            }
            Err(_) => {
                Self::quarantine(path, "grants.json 无法解析");
                return;
            }
        };
        let mut loaded = 0usize;
        for grant in parsed.grants {
            if existing_expired(&grant) {
                continue; // 重启即清过期，不落进内存再等惰性清理。
            }
            self.insert_memory(grant);
            loaded += 1;
        }
        if loaded > 0 {
            tracing::info!("§4.5.2 已从磁盘恢复 {loaded} 条长期授权");
        }
    }

    /// 坏文件改名保留（`grants.json.bad`），便于排障时看清"授权为何消失了"。
    fn quarantine(path: &Path, reason: &str) {
        let bad = path.with_extension("json.bad");
        match std::fs::rename(path, &bad) {
            Ok(()) => tracing::warn!("{reason}，已改名保留 {} 后按空启动", bad.display()),
            Err(error) => tracing::warn!("{reason}，且改名失败（{error}），本次按空启动"),
        }
    }

    /// 内存写入（不含落盘副作用）：insert 与加载共用，避免加载时反向触发写盘。
    fn insert_memory(&self, grant: Grant) {
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

    /// 把当前长期授权原子写回磁盘（tmp → rename）。
    ///
    /// 失败只记 warn，绝不向上抛：权限判定不该因为"记住授权失败"而中断，
    /// 但也不能静默——否则用户以为勾了长期、重启后却发现每次都要审批。
    fn persist_now(&self) {
        let Some(path) = self.persist_path() else {
            return;
        };
        let mut long_lived: Vec<Grant> = self.list().into_iter().filter(is_long_lived).collect();
        if long_lived.len() > GRANTS_PERSIST_CAP {
            tracing::warn!(
                "长期授权 {} 条超过落盘上限 {}，本次不写入（请撤销部分授权）",
                long_lived.len(),
                GRANTS_PERSIST_CAP
            );
            return;
        }
        long_lived.sort_by(|a, b| a.grant_id.cmp(&b.grant_id));
        let file = GrantsFile {
            version: GRANTS_FILE_VERSION,
            grants: long_lived,
        };
        let tmp = path.with_extension("json.tmp");
        // 整体写成一个小闭包：任何一步失败都要清掉半成品，且不会抛给调用方
        // （权限判定不能因为"记住授权失败"而中断）。
        let write = || -> std::io::Result<()> {
            std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            std::fs::rename(&tmp, &path)
        };
        if let Err(error) = write() {
            tracing::warn!("长期授权落盘失败（{}）：{error}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn insert(&self, grant: Grant) {
        let long_lived = is_long_lived(&grant);
        self.insert_memory(grant);
        // 只有会跨重启的授权需要落盘；有期限的写进去也会在加载时作废，
        // 因此不写，避免每次审批都产生磁盘副作用。
        if long_lived {
            self.persist_now();
        }
    }

    /// §4.5.2 按工作区批量撤销（"撤销本工作区全部长期授权"）。返回撤销条数。
    pub fn revoke_workspace(&self, workspace_id: &str) -> usize {
        let mut removed = 0;
        if let Ok(mut inner) = self.inner.write() {
            for bucket in inner.values_mut() {
                let before = bucket.len();
                bucket.retain(|grant| grant.workspace_id != workspace_id);
                removed += before - bucket.len();
            }
            inner.retain(|_, bucket| !bucket.is_empty());
        }
        if removed > 0 {
            self.persist_now();
        }
        removed
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

    /// 撤销单条授权；返回是否确实撤销（成功后同步刷新落盘文件）。
    pub fn revoke(&self, grant_id: &str) -> bool {
        let mut removed = false;
        if let Ok(mut inner) = self.inner.write() {
            for bucket in inner.values_mut() {
                if let Some(index) = bucket.iter().position(|grant| grant.grant_id == grant_id) {
                    bucket.remove(index);
                    removed = true;
                    break;
                }
            }
            inner.retain(|_, bucket| !bucket.is_empty());
        }
        if removed {
            self.persist_now();
        }
        removed
    }

    /// 撤销某工具全部授权（工具卸载/权限中心"按工具撤销"时清理）。
    pub fn revoke_tool(&self, tool_id: &str) -> usize {
        let removed = {
            if let Ok(mut inner) = self.inner.write() {
                inner
                    .remove(tool_id)
                    .map(|bucket| bucket.len())
                    .unwrap_or(0)
            } else {
                0
            }
        };
        if removed > 0 {
            // 必须在写锁释放之后再落盘：persist_now 内部要拿读锁。
            self.persist_now();
        }
        removed
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
        let removed = {
            if let Ok(mut inner) = self.inner.write() {
                let mut removed = 0;
                for bucket in inner.values_mut() {
                    let before = bucket.len();
                    bucket.retain(|grant| !existing_expired(grant));
                    removed += before - bucket.len();
                }
                inner.retain(|_, bucket| !bucket.is_empty());
                removed
            } else {
                0
            }
        };
        if removed > 0 {
            self.persist_now();
        }
        removed
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
                GrantScope::Task => Some(Utc::now() + Duration::hours(8)),
                GrantScope::Workspace => None,
            },
            remaining_uses: match scope {
                GrantScope::Once => None,
                GrantScope::Session => Some(50),
                GrantScope::OneHour => Some(50),
                GrantScope::AlwaysReadOnly => None,
                GrantScope::Task => Some(50),
                GrantScope::Workspace => None,
            },
            scope: Some(scope.as_str().to_string()),
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

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("owo-grants-{tag}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn task_scope_is_session_lifetime_but_never_persisted() {
        // §4.5.2「本任务」= 进程内会话级：字面量对齐指南，有效期对齐 Session，
        // 关键是它不得写进 grants.json——重启后还认得"本任务"就是假语义。
        let path = temp_path("task");
        let store = GrantStore::persisting(&path);
        let req = request("write_file", json!({ "path": "a.txt" }), Level::Write);
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::Task)
            .expect("task 生成 grant");
        let uses = grant.remaining_uses;
        let expiry = grant.expires_at;
        store.insert(grant);
        assert_eq!(uses, Some(50), "本任务授权沿用会话级次数");
        assert!(expiry.is_some(), "本任务授权必须有到期时间");
        assert!(store.consume(&req, "ws-1").is_some(), "当次进程内命中免问");
        assert!(
            !path.exists(),
            "有期限授权不该产生落盘副作用（文件存在说明跨重启会被恢复）"
        );
    }

    #[test]
    fn workspace_scope_survives_restart() {
        // 这是"工作区长期"能进权限中心的唯一前提：不落盘就是假承诺。
        let path = temp_path("workspace");
        let store = GrantStore::persisting(&path);
        let req = request("read_file", json!({ "path": "a.txt" }), Level::Read);
        let grant = store
            .grant_from_scope(&req, "ws-1", GrantScope::Workspace)
            .expect("workspace 生成 grant");
        let id = grant.grant_id.clone();
        store.insert(grant);
        assert!(path.exists(), "长期授权必须写盘");
        drop(store);

        let revived = GrantStore::persisting(&path);
        assert_eq!(revived.list().len(), 1, "重启后恢复且只有一条");
        assert_eq!(revived.list()[0].grant_id, id, "同一张授权，不是新发的");
        assert_eq!(
            revived.list()[0].scope.as_deref(),
            Some("workspace"),
            "范围标签随授权一起恢复，权限中心才能如实展示"
        );
        assert!(
            revived.consume(&req, "ws-1").is_some(),
            "恢复出来的长期授权要真的能免问"
        );
        // 只删自己写的那一个文件：path.parent() 是 %TEMP% 本身，绝不可整体删除。
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn corrupt_grants_file_is_quarantined() {
        // 坏文件不能把 server 启动卡住，也不能静默吞掉授权——改名保留现场。
        let path = temp_path("corrupt");
        std::fs::write(&path, b"{ this is not json").expect("写入坏文件");
        let store = GrantStore::persisting(&path);
        assert!(store.list().is_empty(), "坏文件按空启动");
        assert!(
            path.with_extension("json.bad").exists(),
            "原文必须保留为 *.json.bad 供排障"
        );
    }

    #[test]
    fn revoke_tool_and_workspace_cascades_are_scoped() {
        let store = GrantStore::new();
        for (tool, workspace) in [
            ("read_file", "ws-1"),
            ("read_file", "ws-2"),
            ("write_file", "ws-1"),
        ] {
            let req = request(tool, json!({ "path": "a.txt" }), Level::Read);
            let grant = store
                .grant_from_scope(&req, workspace, GrantScope::Workspace)
                .unwrap();
            store.insert(grant);
        }
        assert_eq!(store.list().len(), 3);
        assert_eq!(
            store.revoke_tool("read_file"),
            2,
            "按工具撤销跨工作区（工具本身没了）"
        );
        assert_eq!(store.list().len(), 1, "另一工具的授权不受牵连");
        assert_eq!(
            store.revoke_workspace("ws-1"),
            1,
            "按工作区撤销只清本工作区"
        );
        assert!(store.list().is_empty());
        assert_eq!(store.revoke_workspace("ws-1"), 0, "重复撤销是幂等的");
    }

    #[test]
    fn new_scope_literals_and_labels() {
        assert_eq!(GrantScope::parse("task"), Some(GrantScope::Task));
        assert_eq!(GrantScope::parse("workspace"), Some(GrantScope::Workspace));
        assert_eq!(GrantScope::Task.as_str(), "task");
        assert_eq!(GrantScope::Workspace.label(), "工作区长期");
        assert_eq!(GrantScope::Task.label(), "本任务", "与审批卡四动作同词");
        assert!(GrantScope::Workspace.persists());
        assert!(!GrantScope::Task.persists());
        assert!(!GrantScope::Session.persists());
        // 旧客户端字面量必须继续可用（词表切换不能丢授权意图）。
        assert_eq!(
            GrantScope::parse("always_readonly"),
            Some(GrantScope::AlwaysReadOnly)
        );
        assert_eq!(GrantScope::parse("forever"), None, "未知识别仍是 None");
    }
}
