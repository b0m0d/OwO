use crate::error::AgentError;
use crate::gateway::ChatMessage;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use chrono::Utc;
use owo_agent_protocol::FileDiff;
use owo_agent_protocol::SseEvent;
pub use owo_agent_protocol::TurnEventRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    /// None 表示文件原本不存在（回滚时删除）。
    #[serde(default)]
    pub original_b64: Option<String>,
    /// Agent 最近一次成功写入后的 SHA-256；撤销前必须匹配，防止覆盖用户后续修改。
    #[serde(default)]
    pub expected_after_sha256: Option<String>,
}

/// 普通 Agent 文件写入的最小变更集收据。
///
/// 内容本身仍由 `SnapshotEntry` 保存；这里保存本次执行窗口的身份、基线/结果哈希和
/// 状态，使 diff/revert 可以按收据消费，而不是只能按整个会话的路径集合猜测。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub receipt_id: String,
    pub tool: String,
    pub turn_id: String,
    pub changed_files: Vec<String>,
    /// 展示相对路径 → Session snapshot 的 canonical 绝对键；只用于内部恢复定位。
    #[serde(default)]
    pub snapshot_keys: HashMap<String, String>,
    pub before_hashes: HashMap<String, Option<String>>,
    pub after_hashes: HashMap<String, Option<String>>,
    pub diff_sha256: String,
    pub created_at: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace: PathBuf,
    pub model: String,
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub snapshots: HashMap<String, SnapshotEntry>,
    /// ToolHost 成功写入产生的收据；旧会话没有此字段时按空列表加载。
    #[serde(default)]
    pub execution_receipts: Vec<ExecutionReceipt>,
    pub created_at: String,
    pub updated_at: String,
    /// 父会话（由 fork 产生时）。
    #[serde(default)]
    pub parent_id: Option<String>,
    /// fork 时的消息下标（含）。
    #[serde(default)]
    pub fork_point: Option<usize>,
    /// 被 /rewind 截断的历史，可 /redo 恢复。
    #[serde(default)]
    pub redo_stack: Vec<Vec<ChatMessage>>,
    /// 被 /undo-msg 移除的消息，可 /redo-msg 恢复。
    #[serde(default)]
    pub message_redo_stack: Vec<Vec<ChatMessage>>,
    /// 用户可编辑的会话标题（None 时按首条用户消息自动显示）。
    #[serde(default)]
    pub title: Option<String>,
    /// 归档标记（默认列表可隐藏）。
    #[serde(default)]
    pub archived: bool,
    /// 置顶标记（列表优先）。
    #[serde(default)]
    pub pinned: bool,
    /// M4.2 会话级模型覆盖：`Some(model)` 固定请求模型（回合/压缩外的一切调用
    /// 都走该模型）；`None` 表示按 Provider 解析链（OPENAI_MODEL 热切换 → 启动
    /// 配置 → 内置默认）。`model` 字段仅为展示值，路由以本字段为准。
    #[serde(default)]
    pub model_override: Option<String>,
}

impl Session {
    pub fn new(
        workspace: impl Into<PathBuf>,
        model: impl Into<String>,
        system_prompt: Option<String>,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            workspace: workspace.into(),
            model: model.into(),
            system_prompt,
            messages: Vec::new(),
            snapshots: HashMap::new(),
            execution_receipts: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            parent_id: None,
            fork_point: None,
            redo_stack: Vec::new(),
            message_redo_stack: Vec::new(),
            title: None,
            archived: false,
            pinned: false,
            model_override: None,
        }
    }

    /// 模型覆盖归一（M4.2）：trim；空串与 `"default"` 哨兵 = None（自动）。
    fn normalize_model_override(model: Option<String>) -> Option<String> {
        model
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != crate::gateway::MODEL_DEFAULT_SENTINEL)
    }

    /// 设置会话级模型覆盖（M4.2）；空串/纯空白/`"default"` 哨兵视为清除覆盖
    /// （回退 Provider 链）；哨兵不落库。
    pub fn with_model_override(mut self, model: Option<String>) -> Self {
        self.model_override = Self::normalize_model_override(model);
        self
    }

    /// 会话级模型路由设置（M4.2）：非空字符串固定请求模型；`None`、空串与
    /// `"default"` 哨兵一律清除覆盖（回退 Provider 解析链，哨兵不落库、不进请求体）。
    /// 固定时展示模型同步为固定值；清除时展示模型保持现状。
    pub fn set_model_override(&mut self, model: Option<String>) {
        self.model_override = Self::normalize_model_override(model);
        if let Some(value) = self.model_override.clone() {
            self.model = value;
        }
        self.updated_at = Utc::now().to_rfc3339();
    }

    /// 展示标题：优先自定义标题，否则取首条用户消息，最后回退为会话短 ID。
    pub fn display_title(&self) -> String {
        if let Some(title) = &self.title {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Some(first) = self
            .messages
            .iter()
            .find(|message| message.role == "user")
            .and_then(|message| message.content.as_deref())
        {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(40).collect();
            }
        }
        let short_id: String = self.id.chars().take(8).collect();
        format!("会话 {short_id}")
    }

    pub fn rename(&mut self, title: String) {
        self.title = Some(title);
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn set_archived(&mut self, archived: bool) {
        self.archived = archived;
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn set_pinned(&mut self, pinned: bool) {
        self.pinned = pinned;
        self.updated_at = Utc::now().to_rfc3339();
    }

    pub fn push(&mut self, message: ChatMessage) {
        self.messages.push(message);
        self.redo_stack.clear();
        self.message_redo_stack.clear();
        self.updated_at = Utc::now().to_rfc3339();
    }

    /// 当前会话改动 diff（相对工作区路径）。
    pub fn diff(&self) -> Vec<FileDiff> {
        let mut diffs = Vec::new();
        for (path, snapshot) in &self.snapshots {
            let original = snapshot.original_b64.as_ref().and_then(|encoded| {
                BASE64
                    .decode(encoded)
                    .ok()
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            });
            let current = std::fs::read(path)
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
            if original == current {
                continue;
            }
            diffs.push(FileDiff {
                path: relative_display(&self.workspace, Path::new(path)),
                before: original,
                after: current,
            });
        }
        diffs
    }

    /// 在工具成功写入后登记一个单文件执行收据。
    ///
    /// 返回 `None` 表示内容未发生变化；读取/解码错误则 fail closed，不生成可撤销收据。
    pub fn record_file_execution(
        &mut self,
        tool: &str,
        turn_id: &str,
        absolute_path: &Path,
    ) -> Result<Option<ExecutionReceipt>, AgentError> {
        let key = absolute_path.to_string_lossy().replace('\\', "/");
        let Some(snapshot) = self.snapshots.get(&key) else {
            return Ok(None);
        };
        let before = snapshot
            .original_b64
            .as_deref()
            .map(|encoded| {
                BASE64
                    .decode(encoded)
                    .map_err(|error| AgentError::Session(format!("快照解码失败：{error}")))
            })
            .transpose()?;
        let after = match std::fs::read(absolute_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(AgentError::Io(error)),
        };
        let before_hash = before.as_deref().map(crate::CasStore::hash_of);
        let after_hash = after.as_deref().map(crate::CasStore::hash_of);
        if before_hash == after_hash {
            return Ok(None);
        }
        let relative = relative_display(&self.workspace, absolute_path);
        let diff_sha256 = crate::CasStore::hash_of(
            serde_json::json!({
                "path": relative,
                "before": before_hash,
                "after": after_hash,
            })
            .to_string()
            .as_bytes(),
        );
        let receipt = ExecutionReceipt {
            receipt_id: format!("exec-{}", uuid::Uuid::new_v4()),
            tool: tool.to_string(),
            turn_id: turn_id.to_string(),
            changed_files: vec![relative.clone()],
            snapshot_keys: HashMap::from([(relative.clone(), key)]),
            before_hashes: HashMap::from([(relative.clone(), before_hash)]),
            after_hashes: HashMap::from([(relative, after_hash)]),
            diff_sha256,
            created_at: Utc::now().to_rfc3339(),
            status: "executed".to_string(),
        };
        self.execution_receipts.push(receipt.clone());
        self.updated_at = Utc::now().to_rfc3339();
        Ok(Some(receipt))
    }

    /// 按执行收据撤销；不传 ID 时消费最近一张尚未撤销的收据。
    /// 没有新式收据的旧会话回退到兼容的全快照撤销逻辑。
    pub async fn revert_receipt(
        &mut self,
        receipt_id: Option<&str>,
    ) -> Result<Vec<String>, AgentError> {
        let index = match receipt_id {
            Some(id) => self
                .execution_receipts
                .iter()
                .position(|receipt| receipt.receipt_id == id)
                .ok_or_else(|| AgentError::Session(format!("执行收据不存在：{id}")))?,
            None => match self
                .execution_receipts
                .iter()
                .rposition(|receipt| receipt.status != "reverted")
            {
                Some(index) => index,
                None => return self.revert_legacy().await,
            },
        };
        let receipt = self.execution_receipts[index].clone();
        if receipt.status == "reverted" {
            return Ok(Vec::new());
        }

        let mut plan = Vec::new();
        let mut conflicts = Vec::new();
        for relative in &receipt.changed_files {
            let key = receipt
                .snapshot_keys
                .get(relative)
                .cloned()
                .unwrap_or_else(|| {
                    self.workspace
                        .canonicalize()
                        .unwrap_or_else(|_| self.workspace.clone())
                        .join(relative)
                        .to_string_lossy()
                        .replace('\\', "/")
                });
            let target = PathBuf::from(&key);
            let Some(snapshot) = self.snapshots.get(&key) else {
                conflicts.push(relative.clone());
                continue;
            };
            let current = match std::fs::read(&target) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(AgentError::Io(error)),
            };
            let current_hash = current.as_deref().map(crate::CasStore::hash_of);
            let before_hash = receipt.before_hashes.get(relative).cloned().flatten();
            let after_hash = receipt.after_hashes.get(relative).cloned().flatten();
            if current_hash == before_hash {
                continue;
            }
            if current_hash != after_hash {
                conflicts.push(relative.clone());
                continue;
            }
            let original = snapshot
                .original_b64
                .as_deref()
                .map(|encoded| {
                    BASE64
                        .decode(encoded)
                        .map_err(|error| AgentError::Session(format!("快照解码失败：{error}")))
                })
                .transpose()?;
            plan.push((relative.clone(), target, key, original));
        }
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Err(AgentError::RevertConflict { paths: conflicts });
        }

        let mut restored = Vec::with_capacity(plan.len());
        for (relative, target, key, original) in plan {
            match original {
                Some(bytes) => {
                    if let Some(parent) = target.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&target, bytes).await?;
                }
                None => match tokio::fs::remove_file(&target).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(AgentError::Io(error)),
                },
            }
            self.snapshots.remove(&key);
            restored.push(relative);
        }
        self.execution_receipts[index].status = "reverted".to_string();
        self.updated_at = Utc::now().to_rfc3339();
        Ok(restored)
    }

    /// 回滚全部已快照的写操作，返回被恢复的路径。
    pub async fn revert(&mut self) -> Result<Vec<String>, AgentError> {
        if !self.execution_receipts.is_empty() {
            return self.revert_receipt(None).await;
        }
        self.revert_legacy().await
    }

    /// 旧快照格式的兼容撤销路径；新写入优先走 `revert_receipt`。
    async fn revert_legacy(&mut self) -> Result<Vec<String>, AgentError> {
        // 先完整预检，再开始写盘：任何文件被用户/外部进程改过时，整批撤销零副作用。
        let mut restore_plan = Vec::new();
        let mut conflicts = Vec::new();
        for (path, snapshot) in &self.snapshots {
            let target = PathBuf::from(path);
            let original = match &snapshot.original_b64 {
                Some(encoded) => Some(
                    BASE64
                        .decode(encoded)
                        .map_err(|e| AgentError::Session(format!("快照解码失败：{e}")))?,
                ),
                None => None,
            };
            let current = match std::fs::read(&target) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(AgentError::Io(error)),
            };
            if current == original {
                continue;
            }
            let matches_agent_write = match (
                snapshot.expected_after_sha256.as_deref(),
                current.as_deref(),
            ) {
                (Some(expected), Some(bytes)) => crate::CasStore::hash_of(bytes) == expected,
                _ => false,
            };
            if matches_agent_write {
                restore_plan.push((target, original));
            } else {
                conflicts.push(relative_display(&self.workspace, &target));
            }
        }
        if !conflicts.is_empty() {
            conflicts.sort();
            conflicts.dedup();
            return Err(AgentError::RevertConflict { paths: conflicts });
        }

        let mut restored = Vec::with_capacity(restore_plan.len());
        for (target, original) in restore_plan {
            match original {
                Some(bytes) => {
                    if let Some(parent) = target.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&target, bytes).await?;
                }
                None => match tokio::fs::remove_file(&target).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(AgentError::Io(error)),
                },
            }
            restored.push(relative_display(&self.workspace, &target));
        }
        self.snapshots.clear();
        self.execution_receipts.clear();
        self.updated_at = Utc::now().to_rfc3339();
        Ok(restored)
    }

    /// 在指定消息处派生一个子会话（继承历史，快照与 redo 栈清空）。
    pub fn fork(&self, message_index: usize) -> Session {
        let messages = if self.messages.is_empty() {
            Vec::new()
        } else {
            let end = message_index.min(self.messages.len() - 1);
            self.messages[..=end].to_vec()
        };
        let now = Utc::now().to_rfc3339();
        Session {
            id: uuid::Uuid::new_v4().to_string(),
            workspace: self.workspace.clone(),
            model: self.model.clone(),
            system_prompt: self.system_prompt.clone(),
            messages,
            snapshots: HashMap::new(),
            execution_receipts: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
            parent_id: Some(self.id.clone()),
            fork_point: Some(message_index),
            redo_stack: Vec::new(),
            message_redo_stack: Vec::new(),
            title: None,
            archived: false,
            pinned: false,
            // fork 继承父会话的模型覆盖（路由语义随历史一起派生）。
            model_override: self.model_override.clone(),
        }
    }

    /// 回退到仅保留前 `keep` 条消息，同时清空文件快照；返回被移除的历史。
    pub fn rewind(&mut self, keep: usize) -> Vec<ChatMessage> {
        if keep >= self.messages.len() {
            return Vec::new();
        }
        let removed = self.messages.split_off(keep);
        self.redo_stack.push(removed.clone());
        self.snapshots.clear();
        self.execution_receipts.clear();
        self.updated_at = Utc::now().to_rfc3339();
        removed
    }

    /// 恢复最近一次 rewind 移除的历史。
    pub fn redo(&mut self) -> Option<Vec<ChatMessage>> {
        let tail = self.redo_stack.pop()?;
        self.messages.extend(tail.iter().cloned());
        self.updated_at = Utc::now().to_rfc3339();
        Some(tail)
    }

    /// 移除最近 `count` 条消息（消息级撤销），压入可恢复栈。
    pub fn undo_message(&mut self, count: usize) -> Option<Vec<ChatMessage>> {
        let count = count.min(self.messages.len());
        if count == 0 {
            return None;
        }
        let split_at = self.messages.len() - count;
        let removed = self.messages.split_off(split_at);
        self.message_redo_stack.push(removed.clone());
        self.updated_at = Utc::now().to_rfc3339();
        Some(removed)
    }

    /// 恢复最近一次消息级撤销。
    pub fn redo_message(&mut self) -> Option<Vec<ChatMessage>> {
        let tail = self.message_redo_stack.pop()?;
        self.messages.extend(tail.iter().cloned());
        self.updated_at = Utc::now().to_rfc3339();
        Some(tail)
    }
}

fn relative_display(workspace: &Path, path: &Path) -> String {
    let normalize = |value: &Path| -> String {
        let raw = value.to_string_lossy().replace('\\', "/");
        raw.strip_prefix("//?/").unwrap_or(&raw).to_string()
    };
    let workspace = normalize(workspace);
    let path = normalize(path);
    path.strip_prefix(&workspace)
        .map(|relative| relative.trim_start_matches('/').to_string())
        .unwrap_or(path)
}

pub trait SessionStore: Send + Sync {
    fn create(
        &self,
        workspace: &Path,
        model: &str,
        system_prompt: Option<&str>,
    ) -> Result<Session, AgentError>;
    fn load(&self, id: &str) -> Result<Session, AgentError>;
    fn save(&self, session: &Session) -> Result<(), AgentError>;
    /// Persist one turn event and allocate the next sequence number for its session.
    /// Non-durable stores must opt in explicitly rather than pretending to support replay.
    fn append_turn_event(
        &self,
        session_id: &str,
        turn_id: &str,
        payload: &SseEvent,
    ) -> Result<TurnEventRecord, AgentError> {
        let _ = (session_id, turn_id, payload);
        Err(AgentError::Session("当前存储不支持持久化回合事件".into()))
    }
    /// Read a bounded page of persisted events after a session-scoped sequence cursor.
    fn turn_events_after(
        &self,
        session_id: &str,
        turn_id: Option<&str>,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<TurnEventRecord>, AgentError> {
        let _ = (session_id, turn_id, after_seq, limit);
        Err(AgentError::Session("当前存储不支持回合事件回放".into()))
    }
    /// 列出全部会话 ID（按更新时间倒序）。
    fn list(&self) -> Vec<String> {
        Vec::new()
    }
    /// 持久化审计记录（默认 no-op；SQLite 存储落库）。
    fn append_audit(&self, entries: &[crate::audit::AuditEntry]) -> Result<(), AgentError> {
        let _ = entries;
        Ok(())
    }
    /// 最近 N 条审计记录（默认空；SQLite 存储返回持久化记录）。
    fn recent_audit(&self, limit: usize) -> Vec<crate::audit::AuditEntry> {
        let _ = limit;
        Vec::new()
    }

    /// 分页/过滤/搜索审计（默认退化为 recent_audit；SQLite 存储提供完整实现）。
    fn query_audit(
        &self,
        query: &crate::sqlite_store::AuditQuery,
    ) -> (Vec<crate::audit::AuditEntry>, usize) {
        let entries = self.recent_audit(query.limit.max(1));
        let total = entries.len();
        (entries, total)
    }

    /// 清空会话与审计（R8 存储运维；默认不支持，SQLite 存储提供实现）。
    fn clear(&self) -> Result<(), AgentError> {
        Err(AgentError::Session("当前存储不支持清空".into()))
    }

    /// 存储是否处于只读降级（R8：迁移失败后的安全状态；默认否）。
    fn is_read_only(&self) -> bool {
        false
    }

    /// 只读降级原因/迁移警告（R8；默认无）。
    fn migration_warning(&self) -> Option<String> {
        None
    }
}

/// M1 会话存储：JSON 文件（后续迁移 SQLite）。
/// R9：可选加密模式——落盘经 storage_crypto 文件信封加密（`<id>.json.owo-crypt`），
/// 读取优先解密；明文 `.json` 保持兼容（既有存储不受影响）。
pub struct JsonSessionStore {
    root: PathBuf,
    encrypted: bool,
}

impl JsonSessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            encrypted: false,
        }
    }

    /// 加密模式：会话落盘经 DPAPI 信封加密（非 Windows 下 save 会显式失败）。
    pub fn new_encrypted(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            encrypted: true,
        }
    }

    fn plain_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn encrypted_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json.owo-crypt"))
    }

    fn path(&self, id: &str) -> PathBuf {
        if self.encrypted {
            self.encrypted_path(id)
        } else {
            self.plain_path(id)
        }
    }
}

impl SessionStore for JsonSessionStore {
    fn create(
        &self,
        workspace: &Path,
        model: &str,
        system_prompt: Option<&str>,
    ) -> Result<Session, AgentError> {
        let session = Session::new(workspace, model, system_prompt.map(str::to_string));
        self.save(&session)?;
        Ok(session)
    }

    fn load(&self, id: &str) -> Result<Session, AgentError> {
        // 加密形态优先，明文兜底（读取解密；加密文件损坏 → 显式错误，不静默回退明文）。
        let encrypted_path = self.encrypted_path(id);
        if encrypted_path.exists() {
            let content = crate::storage_crypto::decrypt_file_envelope(&encrypted_path)
                .map_err(|error| AgentError::Session(format!("会话 {id} 解密失败：{error}")))?;
            return Ok(serde_json::from_slice(&content)?);
        }
        let content = std::fs::read_to_string(self.plain_path(id))
            .map_err(|e| AgentError::Session(format!("会话 {id} 读取失败：{e}")))?;
        Ok(serde_json::from_str(&content)?)
    }

    fn save(&self, session: &Session) -> Result<(), AgentError> {
        std::fs::create_dir_all(&self.root)?;
        let target = self.path(&session.id);
        let content = serde_json::to_vec_pretty(session)?;
        if self.encrypted {
            crate::storage_crypto::encrypt_file_envelope(&target, &content).map_err(|error| {
                AgentError::Session(format!("会话 {} 落盘加密失败：{error}", session.id))
            })?;
            return Ok(());
        }
        let tmp = self.root.join(format!("{}.tmp", session.id));
        std::fs::write(&tmp, content)?;
        if let Err(rename_error) = std::fs::rename(&tmp, &target) {
            // Windows 不允许 rename 覆盖已有文件；保留临时文件写入语义，
            // 在目标存在时执行一次兼容替换。
            if !target.exists() {
                return Err(rename_error.into());
            }
            std::fs::remove_file(&target)?;
            std::fs::rename(&tmp, &target)?;
        }
        Ok(())
    }

    fn list(&self) -> Vec<String> {
        let mut sessions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_suffix(".json.owo-crypt") {
                    sessions.push(id.to_string());
                } else if let Some(id) = name.strip_suffix(".json") {
                    sessions.push(id.to_string());
                }
            }
        }
        sessions.sort_by(|a, b| {
            let ta = self
                .load(a)
                .map(|s| s.updated_at.clone())
                .unwrap_or_default();
            let tb = self
                .load(b)
                .map(|s| s.updated_at.clone())
                .unwrap_or_default();
            tb.cmp(&ta)
        });
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ChatMessage;

    #[test]
    fn session_store_round_trip_and_list() {
        let root =
            std::env::temp_dir().join(format!("owo-session-store-test-{}", uuid::Uuid::new_v4()));
        let store = JsonSessionStore::new(&root);
        let session = store
            .create(std::path::Path::new("."), "mock", None)
            .unwrap();
        store.save(&session).unwrap();
        assert_eq!(store.list().len(), 1);
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.id, session.id);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// M4.2：`model_override` JSON 存储往返 + 旧文件（缺该字段）兼容 + fork 继承。
    #[test]
    fn model_override_roundtrip_legacy_compat_and_fork_inherits() {
        let root = std::env::temp_dir().join(format!(
            "owo-session-override-test-{}",
            uuid::Uuid::new_v4()
        ));
        let store = JsonSessionStore::new(&root);
        let session = store
            .create(std::path::Path::new("."), "glm-5.3-flash", None)
            .unwrap()
            .with_model_override(Some("vision-pro".to_string()));
        store.save(&session).unwrap();
        let loaded = store.load(&session.id).unwrap();
        assert_eq!(loaded.model_override.as_deref(), Some("vision-pro"));
        // 展示模型与路由覆盖解耦：覆盖不改展示值。
        assert_eq!(loaded.model, "glm-5.3-flash");
        // 空串覆盖 = 清除（回退 Provider 解析链）；"default" 哨兵同样归一为自动，
        // 且绝不作为展示/存储值（M4.2 哨兵不泄漏）。
        let cleared = loaded.clone().with_model_override(Some("  ".to_string()));
        assert_eq!(cleared.model_override, None);
        let sentinel = loaded
            .clone()
            .with_model_override(Some("default".to_string()));
        assert_eq!(sentinel.model_override, None);
        let mut pinned = loaded.clone();
        pinned.set_model_override(Some("wire-y".to_string()));
        assert_eq!(pinned.model_override.as_deref(), Some("wire-y"));
        assert_eq!(pinned.model, "wire-y", "固定时展示同步");
        pinned.set_model_override(Some("default".to_string()));
        assert_eq!(pinned.model_override, None, "哨兵必须清除覆盖");
        assert_eq!(pinned.model, "wire-y", "清除不改展示（不猜测缺省值）");
        // fork 继承覆盖（路由语义随历史派生）。
        assert_eq!(loaded.fork(0).model_override.as_deref(), Some("vision-pro"));
        // 旧格式文件（无 model_override 字段）必须照常加载为 None。
        let legacy = store
            .create(std::path::Path::new("."), "old-model", None)
            .unwrap();
        let mut raw: serde_json::Value = serde_json::to_value(&legacy).expect("会话应可序列化");
        raw.as_object_mut()
            .expect("会话是对象")
            .remove("model_override");
        std::fs::write(store.plain_path(&legacy.id), raw.to_string()).unwrap();
        assert_eq!(store.load(&legacy.id).unwrap().model_override, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fork_creates_child_with_history() {
        let mut session = Session::new(".", "mock", None);
        session.push(ChatMessage::user("a".to_string()));
        session.push(ChatMessage::assistant_text("b".to_string()));
        session.push(ChatMessage::user("c".to_string()));

        let child = session.fork(1);
        assert_eq!(child.messages.len(), 2);
        assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(child.fork_point, Some(1));
        assert!(child.snapshots.is_empty());
        assert!(child.redo_stack.is_empty());
    }

    #[test]
    fn fork_on_empty_session_does_not_panic() {
        let session = Session::new(".", "mock", None);
        let child = session.fork(999999);
        assert!(child.messages.is_empty());
        assert_eq!(child.parent_id.as_deref(), Some(session.id.as_str()));
    }

    #[test]
    fn rewind_and_redo_round_trip() {
        let mut session = Session::new(".", "mock", None);
        for index in 0..5 {
            session.push(ChatMessage::user(format!("m{index}")));
        }
        let removed = session.rewind(2);
        assert_eq!(removed.len(), 3);
        assert_eq!(session.messages.len(), 2);

        let restored = session.redo().expect("存在可恢复历史");
        assert_eq!(restored.len(), 3);
        assert_eq!(session.messages.len(), 5);
        assert!(session.redo().is_none());
    }

    #[tokio::test]
    async fn rewind_and_revert_restores_files_before_truncating_history() {
        let workspace =
            std::env::temp_dir().join(format!("owo-session-rewind-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("changed.txt");
        std::fs::write(&path, "after").unwrap();

        let mut session = Session::new(&workspace, "mock", None);
        session.push(ChatMessage::user("first".to_string()));
        session.push(ChatMessage::assistant_text("reply".to_string()));
        session.snapshots.insert(
            path.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                original_b64: Some(BASE64.encode("before")),
                expected_after_sha256: Some(crate::CasStore::hash_of(b"after")),
            },
        );

        session.revert().await.unwrap();
        let removed = session.rewind(1);

        assert_eq!(removed.len(), 1);
        assert_eq!(session.messages.len(), 1);
        assert!(session.snapshots.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn execution_receipt_revert_is_scoped_and_persistable() {
        let workspace = std::env::temp_dir().join(format!(
            "owo-session-execution-receipt-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("receipt.txt");
        std::fs::write(&path, "before").unwrap();

        let mut session = Session::new(&workspace, "mock", None);
        session.snapshots.insert(
            path.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                original_b64: Some(BASE64.encode("before")),
                expected_after_sha256: Some(crate::CasStore::hash_of(b"after")),
            },
        );
        std::fs::write(&path, "after").unwrap();
        let receipt = session
            .record_file_execution("write_file", "turn-1", &path)
            .unwrap()
            .expect("内容变化必须产生执行收据");
        assert_eq!(receipt.changed_files, vec!["receipt.txt"]);
        assert_eq!(session.execution_receipts.len(), 1);
        let serialized = serde_json::to_value(&session).unwrap();
        assert!(serialized.get("execution_receipts").is_some());

        let restored = session
            .revert_receipt(Some(&receipt.receipt_id))
            .await
            .unwrap();
        assert_eq!(restored, vec!["receipt.txt"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
        assert_eq!(session.execution_receipts[0].status, "reverted");
        assert!(session.snapshots.is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn revert_conflict_preflight_prevents_partial_overwrite() {
        let workspace = std::env::temp_dir().join(format!(
            "owo-session-revert-conflict-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let user_changed = workspace.join("a-user-edited.txt");
        let agent_written = workspace.join("b-agent-written.txt");
        std::fs::write(&user_changed, "user edit").unwrap();
        std::fs::write(&agent_written, "agent version").unwrap();

        let mut session = Session::new(&workspace, "mock", None);
        session.snapshots.insert(
            user_changed.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                original_b64: Some(BASE64.encode("before a")),
                expected_after_sha256: Some(crate::CasStore::hash_of(b"agent version")),
            },
        );
        session.snapshots.insert(
            agent_written.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                original_b64: Some(BASE64.encode("before b")),
                expected_after_sha256: Some(crate::CasStore::hash_of(b"agent version")),
            },
        );

        let error = session
            .revert()
            .await
            .expect_err("外部修改必须阻止整批撤销");
        match error {
            AgentError::RevertConflict { paths } => {
                assert_eq!(paths, vec!["a-user-edited.txt".to_string()])
            }
            other => panic!("应返回结构化撤销冲突，实际：{other}"),
        }
        assert_eq!(std::fs::read_to_string(&user_changed).unwrap(), "user edit");
        assert_eq!(
            std::fs::read_to_string(&agent_written).unwrap(),
            "agent version",
            "预检发现任意冲突时，不得先回滚其他文件"
        );
        assert_eq!(session.snapshots.len(), 2, "冲突时保留快照以便用户处理");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn legacy_snapshot_without_write_hash_fails_closed() {
        let workspace = std::env::temp_dir().join(format!(
            "owo-session-revert-legacy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("legacy.txt");
        std::fs::write(&path, "possibly user-edited").unwrap();
        let mut session = Session::new(&workspace, "mock", None);
        session.snapshots.insert(
            path.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                original_b64: Some(BASE64.encode("before")),
                expected_after_sha256: None,
            },
        );

        assert!(matches!(
            session.revert().await,
            Err(AgentError::RevertConflict { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "possibly user-edited"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn rewind_does_not_change_files_when_keep_is_current_length() {
        let workspace =
            std::env::temp_dir().join(format!("owo-session-rewind-noop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = workspace.join("changed.txt");
        std::fs::write(&path, "after").unwrap();

        let mut session = Session::new(&workspace, "mock", None);
        session.push(ChatMessage::user("first".to_string()));
        session.snapshots.insert(
            path.to_string_lossy().replace('\\', "/"),
            SnapshotEntry {
                original_b64: Some(BASE64.encode("before")),
                expected_after_sha256: Some(crate::CasStore::hash_of(b"after")),
            },
        );

        let removed = session.rewind(1);

        assert!(removed.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
        assert!(!session.snapshots.is_empty());
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn message_undo_and_redo_round_trip() {
        let mut session = Session::new(".", "mock", None);
        for index in 0..4 {
            session.push(ChatMessage::user(format!("m{index}")));
        }
        let removed = session.undo_message(2).expect("存在可撤销消息");
        assert_eq!(removed.len(), 2);
        assert_eq!(session.messages.len(), 2);
        assert!(session.undo_message(0).is_none());

        let restored = session.redo_message().expect("存在可恢复消息");
        assert_eq!(restored.len(), 2);
        assert_eq!(session.messages.len(), 4);
        assert!(session.redo_message().is_none());
    }

    #[test]
    fn pushing_new_history_invalidates_both_redo_stacks() {
        let mut session = Session::new(".", "mock", None);
        for index in 0..3 {
            session.push(ChatMessage::user(format!("m{index}")));
        }
        session.rewind(1);
        session.undo_message(1);

        session.push(ChatMessage::user("new branch".to_string()));

        assert!(session.redo().is_none());
        assert!(session.redo_message().is_none());
    }

    #[test]
    fn title_archive_pin_round_trip() {
        let mut session = Session::new(".", "mock", None);
        session.push(ChatMessage::user("给 parseConfig 补测试".to_string()));
        assert_eq!(session.display_title(), "给 parseConfig 补测试");
        session.rename("我的任务".to_string());
        assert_eq!(session.display_title(), "我的任务");
        session.set_pinned(true);
        session.set_archived(true);
        assert!(session.pinned);
        assert!(session.archived);
        let child = session.fork(0);
        assert!(child.title.is_none());
        assert!(!child.pinned);
        assert!(!child.archived);
        assert_eq!(child.display_title(), "给 parseConfig 补测试");
    }
}
