//! Surface 适配器与租约 fencing 环境注册表（从 desktop_env.rs 拆出）。

use super::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// TaskSurface 适配器：既有模拟面/真实面 → DesktopEnv（只读观测 + 动作注入）
// ---------------------------------------------------------------------------

/// 把既有 [`owo_agent_kernel::TaskSurface`] 适配为 DesktopEnv。
///
/// 能力边界（显式声明，不伪造）：
/// - `observe`：OCR 版面 → 场景元素；
/// - `step`：GUI click/type/key/launch 与 wait；
/// - `reset/snapshot/restore/inject_fault/judge`：真实面不具备 → [`EnvError::Unsupported`]。
pub struct SurfaceEnvAdapter<S> {
    surface: S,
    env_id: String,
    env_version: String,
    freshness: u64,
}

impl<S> SurfaceEnvAdapter<S>
where
    S: owo_agent_kernel::TaskSurface + Send,
{
    pub fn new(surface: S, env_id: impl Into<String>) -> Self {
        Self {
            surface,
            env_id: env_id.into(),
            env_version: format!("surface-{}", std::any::type_name::<S>()),
            freshness: 0,
        }
    }

    pub fn surface_mut(&mut self) -> &mut S {
        &mut self.surface
    }
}

#[async_trait]
impl<S> DesktopEnv for SurfaceEnvAdapter<S>
where
    S: owo_agent_kernel::TaskSurface + Send,
{
    fn env_id(&self) -> &str {
        &self.env_id
    }

    fn env_version(&self) -> &str {
        &self.env_version
    }

    async fn reset(&mut self, _task: TaskSeed) -> Result<WorldStateV1, EnvError> {
        Err(EnvError::Unsupported(
            "TaskSurface 适配器不支持 reset；训练请使用 SimDesktopEnv".into(),
        ))
    }

    async fn observe(&mut self) -> Result<WorldStateV1, EnvError> {
        self.freshness += 1;
        let app = self.surface.app();
        let ocr = self
            .surface
            .ocr()
            .await
            .map_err(|e| EnvError::App(format!("surface OCR 失败：{e}")))?;
        let lines = ocr
            .get("lines")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut elements = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let text = line.get("text").and_then(Value::as_str).unwrap_or_default();
            let x = line.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
            let y = line.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
            let width = line.get("width").and_then(Value::as_i64).unwrap_or(0) as i32;
            let height = line.get("height").and_then(Value::as_i64).unwrap_or(0) as i32;
            elements.push(SimElement {
                id: format!("ocr.line.{i}"),
                role: line
                    .get("role_hint")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    .to_string(),
                text: text.to_string(),
                x,
                y,
                width,
                height,
                enabled: true,
                visible: true,
            });
        }
        Ok(WorldStateV1 {
            env_id: self.env_id.clone(),
            env_version: self.env_version.clone(),
            snapshot_id: None,
            timestamp: now_rfc3339(),
            screenshot_ref: None,
            scene_graph: json!({ "elements": elements }),
            accessibility_tree_ref: None,
            foreground_app: app,
            window_stack: Vec::new(),
            structured_app_state: None,
            freshness: self.freshness,
            privacy_labels: vec!["surface".into()],
        })
    }

    async fn step(&mut self, action: GroundedAction) -> Result<StepResult, EnvError> {
        let started = Instant::now();
        let before = self.observe().await?;
        let outcome: Result<String, String> = match action.kind {
            ActionKind::Gui => {
                let op = action
                    .arguments
                    .get("op")
                    .and_then(Value::as_str)
                    .unwrap_or("click");
                match op {
                    "click" => {
                        let (x, y) = resolve_click_point(&action)?;
                        self.surface
                            .click(x, y)
                            .await
                            .map(|_| format!("点击 ({x},{y})"))
                    }
                    "type" => {
                        let text = action
                            .arguments
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| EnvError::InvalidAction("type 缺少 text 参数".into()))?;
                        self.surface
                            .type_text(text)
                            .await
                            .map(|_| format!("输入 {text:?}"))
                    }
                    "key" => {
                        let key = action
                            .arguments
                            .get("key")
                            .and_then(Value::as_str)
                            .ok_or_else(|| EnvError::InvalidAction("key 缺少 key 参数".into()))?;
                        self.surface.key(key).await.map(|_| format!("按键 {key:?}"))
                    }
                    "launch" => {
                        let target = action
                            .arguments
                            .get("target")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                EnvError::InvalidAction("launch 缺少 target 参数".into())
                            })?;
                        self.surface
                            .launch(target)
                            .await
                            .map(|_| format!("启动 {target:?}"))
                    }
                    other => return Err(EnvError::InvalidAction(format!("未知 GUI op：{other}"))),
                }
            }
            ActionKind::Wait => Ok("等待".to_string()),
            ActionKind::AskUser => Err("surface 适配器无人类节点".to_string()),
            ActionKind::Cli | ActionKind::Api => {
                return Err(EnvError::Unsupported(format!(
                    "surface 适配器不支持 {:?} 动作",
                    action.kind
                )));
            }
        };
        let after = self.observe().await?;
        let (verdict, error) = match outcome {
            Ok(summary) => (
                Verdict::Pass {
                    evidence: vec![summary],
                },
                None,
            ),
            Err(e) => (
                Verdict::Fail {
                    reason: e.clone(),
                    evidence: Vec::new(),
                },
                Some(EnvError::App(e)),
            ),
        };
        let before_elements: Vec<SimElement> = serde_json::from_value(
            before
                .scene_graph
                .get("elements")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
        .unwrap_or_default();
        let after_elements: Vec<SimElement> = serde_json::from_value(
            after
                .scene_graph
                .get("elements")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
        .unwrap_or_default();
        let delta = diff_observation(
            &before_elements,
            &after_elements,
            &before.window_stack,
            &after.window_stack,
        );
        Ok(StepResult {
            before_state_ref: sha256_hex(&before.scene_graph.to_string()),
            action,
            after_state_ref: sha256_hex(&after.scene_graph.to_string()),
            observed_delta: delta,
            verdict,
            reward_parts: RewardParts {
                progress: if error.is_none() { 1.0 } else { 0.0 },
                efficiency: if error.is_none() { 1.0 } else { 0.25 },
                safety: 1.0,
            },
            duration_ms: started.elapsed().as_millis() as u64,
            error,
            evidence_refs: Vec::new(),
        })
    }

    async fn snapshot(&mut self) -> Result<String, EnvError> {
        Err(EnvError::Unsupported("surface 适配器不支持快照".into()))
    }

    async fn restore(&mut self, _snapshot: String) -> Result<WorldStateV1, EnvError> {
        Err(EnvError::Unsupported("surface 适配器不支持恢复".into()))
    }

    async fn inject_fault(&mut self, _fault: FaultSpec) -> Result<(), EnvError> {
        Err(EnvError::Unsupported("surface 适配器不支持故障注入".into()))
    }

    async fn judge(&mut self, _success: SuccessSpec) -> Result<Verdict, EnvError> {
        Err(EnvError::Unsupported(
            "surface 适配器不支持程序化判分（真实面判分需 Verifier 与授权）".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// EnvRegistry：环境实例 + ControllerLease 单写租约
// ---------------------------------------------------------------------------

/// 环境写租约记录（§4.2 单环境单写作者）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvLeaseRecord {
    pub owner: String,
    pub token: String,
    pub epoch: u64,
    pub expires_at_ms: i64,
}

/// 租约凭证（写操作必须携带；不匹配即被 fencing 拒绝）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseProof {
    pub owner: String,
    pub token: String,
    pub epoch: u64,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 环境注册表：创建/克隆 S1 环境实例，并强制单写租约。
///
/// 读路径（observe/judge/snapshot）不要求租约；写路径（step/inject_fault/restore）
/// 必须携带与当前租约完全匹配的 [`LeaseProof`]，否则返回 [`EnvError::StaleLease`]。
#[derive(Clone)]
pub struct EnvRegistry {
    envs: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<SimDesktopEnv>>>>>,
    leases: Arc<std::sync::Mutex<HashMap<String, EnvLeaseRecord>>>,
    ttl_ms: i64,
}

impl Default for EnvRegistry {
    fn default() -> Self {
        Self::new(Duration::from_secs(30))
    }
}

impl EnvRegistry {
    pub fn new(ttl: Duration) -> Self {
        Self {
            envs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            leases: Arc::new(std::sync::Mutex::new(HashMap::new())),
            ttl_ms: ttl.as_millis() as i64,
        }
    }

    /// 创建并 reset 一个 S1 环境；返回 env_id。
    pub async fn create(
        &self,
        env_id: impl Into<String>,
        task: TaskSeed,
    ) -> Result<String, EnvError> {
        let env_id = env_id.into();
        let mut env = SimDesktopEnv::new(env_id.clone());
        env.reset_sync(task)?;
        self.envs
            .lock()
            .expect("envs lock poisoned")
            .insert(env_id.clone(), Arc::new(Mutex::new(env)));
        Ok(env_id)
    }

    /// 以既有环境的 TaskSeed 克隆一个并行实例（相同初始状态，互不串状态）。
    pub async fn clone_env(
        &self,
        source_env_id: &str,
        new_env_id: impl Into<String>,
    ) -> Result<String, EnvError> {
        let source = self.get(source_env_id)?;
        let new_env_id = new_env_id.into();
        let cloned = {
            let guard = source.lock().await;
            guard.clone_fresh(new_env_id.clone())?
        };
        self.envs
            .lock()
            .expect("envs lock poisoned")
            .insert(new_env_id.clone(), Arc::new(Mutex::new(cloned)));
        Ok(new_env_id)
    }

    pub fn get(&self, env_id: &str) -> Result<Arc<Mutex<SimDesktopEnv>>, EnvError> {
        self.envs
            .lock()
            .expect("envs lock poisoned")
            .get(env_id)
            .cloned()
            .ok_or_else(|| EnvError::App(format!("未知环境 {env_id}")))
    }

    pub fn env_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .envs
            .lock()
            .expect("envs lock poisoned")
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    /// 获取写租约：空闲或已过期 → 授予（epoch 递增）；被他人持有 → 拒绝。
    pub fn acquire_lease(&self, env_id: &str, owner: &str) -> Result<EnvLeaseRecord, EnvError> {
        self.get(env_id)?;
        let mut leases = self.leases.lock().expect("leases lock poisoned");
        let now = now_ms();
        let next_epoch = leases.get(env_id).map(|l| l.epoch + 1).unwrap_or(1);
        if let Some(existing) = leases.get(env_id) {
            if existing.expires_at_ms > now && existing.owner != owner {
                return Err(EnvError::StaleLease(format!(
                    "环境 {env_id} 写租约由 {} 持有（至 {}）",
                    existing.owner, existing.expires_at_ms
                )));
            }
        }
        let record = EnvLeaseRecord {
            owner: owner.to_string(),
            token: uuid::Uuid::new_v4().to_string(),
            epoch: next_epoch,
            expires_at_ms: now + self.ttl_ms,
        };
        leases.insert(env_id.to_string(), record.clone());
        Ok(record)
    }

    /// 续租：凭证必须匹配当前租约。
    pub fn renew_lease(
        &self,
        env_id: &str,
        proof: &LeaseProof,
    ) -> Result<EnvLeaseRecord, EnvError> {
        self.check_lease(env_id, proof)?;
        let mut leases = self.leases.lock().expect("leases lock poisoned");
        let record = leases.get_mut(env_id).expect("checked above");
        record.expires_at_ms = now_ms() + self.ttl_ms;
        Ok(record.clone())
    }

    /// 释放租约：凭证必须匹配当前租约。
    ///
    /// 释放不删除记录，而是置为过期——epoch 历史必须跨接管单调递增（fencing），
    /// 否则新接管无法用 epoch 区分旧控制者的写入。
    pub fn release_lease(&self, env_id: &str, proof: &LeaseProof) -> Result<(), EnvError> {
        self.check_lease(env_id, proof)?;
        let mut leases = self.leases.lock().expect("leases lock poisoned");
        if let Some(record) = leases.get_mut(env_id) {
            record.expires_at_ms = 0;
        }
        Ok(())
    }

    /// 当前未过期的活跃租约（若存在）。
    pub fn active_lease(&self, env_id: &str) -> Option<EnvLeaseRecord> {
        self.leases
            .lock()
            .expect("leases lock poisoned")
            .get(env_id)
            .filter(|l| l.expires_at_ms > now_ms())
            .cloned()
    }

    fn check_lease(&self, env_id: &str, proof: &LeaseProof) -> Result<EnvLeaseRecord, EnvError> {
        let leases = self.leases.lock().expect("leases lock poisoned");
        let Some(record) = leases.get(env_id) else {
            return Err(EnvError::StaleLease(format!("环境 {env_id} 无活跃租约")));
        };
        if record.owner != proof.owner || record.token != proof.token || record.epoch != proof.epoch
        {
            return Err(EnvError::StaleLease(format!(
                "环境 {env_id} 租约凭证不匹配（当前 epoch {}，请求 epoch {}）",
                record.epoch, proof.epoch
            )));
        }
        if record.expires_at_ms <= now_ms() {
            return Err(EnvError::StaleLease(format!("环境 {env_id} 租约已过期")));
        }
        Ok(record.clone())
    }

    /// 写路径：租约校验通过后执行一步动作。
    pub async fn step(
        &self,
        env_id: &str,
        proof: &LeaseProof,
        action: GroundedAction,
    ) -> Result<StepResult, EnvError> {
        self.check_lease(env_id, proof)?;
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.step_sync(action)
    }

    /// 写路径：租约校验通过后注入故障。
    pub async fn inject_fault(
        &self,
        env_id: &str,
        proof: &LeaseProof,
        fault: FaultSpec,
    ) -> Result<(), EnvError> {
        self.check_lease(env_id, proof)?;
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.inject_fault_sync(fault)
    }

    /// 写路径：租约校验通过后恢复快照。
    pub async fn restore(
        &self,
        env_id: &str,
        proof: &LeaseProof,
        snapshot: String,
    ) -> Result<WorldStateV1, EnvError> {
        self.check_lease(env_id, proof)?;
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.restore_sync(snapshot)
    }

    /// 写路径：租约校验通过后按新任务种子重新复位环境（清除全部状态与快照）。
    ///
    /// 复位会改变环境状态，属于单写者纪律范围：必须携带有效租约凭证，
    /// 与 step / inject_fault / restore 相同的 fencing 语义。
    pub async fn reset(
        &self,
        env_id: &str,
        proof: &LeaseProof,
        task: TaskSeed,
    ) -> Result<WorldStateV1, EnvError> {
        self.check_lease(env_id, proof)?;
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.reset_sync(task)
    }

    // 读路径：不要求租约（只读不改状态）。

    pub async fn observe(&self, env_id: &str) -> Result<WorldStateV1, EnvError> {
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.observe_sync()
    }

    pub async fn judge(&self, env_id: &str, spec: SuccessSpec) -> Result<Verdict, EnvError> {
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.judge_sync(spec)
    }

    pub async fn snapshot(&self, env_id: &str) -> Result<String, EnvError> {
        let env = self.get(env_id)?;
        let mut guard = env.lock().await;
        guard.snapshot_sync()
    }
}
