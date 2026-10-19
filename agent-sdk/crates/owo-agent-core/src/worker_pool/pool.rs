use crate::audit::AuditLog;
use crate::fleet::{
    new_correlation_id, AgentBus, AgentId, CorrelationId, SupervisionState, Supervisor,
    WorkerEvent, WorkerEventKind,
};
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use super::protocol::*;

/// worker 槽位（池内状态）。
struct WorkerSlot {
    spec: WorkerSpec,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pid: Option<u32>,
    supervisor: Supervisor,
    status: WorkerStatus,
    turns: u32,
    started_at: Option<Instant>,
    exited: bool,
    /// spawn 代数：每次（重新）spawn 递增；reader 消息带代数，过期消息被调度循环忽略。
    gen: u64,
    pending: HashMap<String, oneshot::Sender<Result<String, String>>>,
    ping: Option<oneshot::Sender<()>>,
    /// 非结构化行计数（协议纪律观测：持续增长说明子进程未走结构化协议）。
    bad_lines: u32,
    /// 最近一次非结构化行时间。
    last_bad_line_at: Option<Instant>,
}

impl WorkerSlot {
    fn started_deadline(&self) -> Option<Instant> {
        let secs = self.spec.budget.max_duration_secs;
        if secs == 0 {
            None
        } else {
            self.started_at.map(|t| t + Duration::from_secs(secs))
        }
    }
}

/// 池内部状态（actor 模型：调度循环独占写）。
struct PoolInner {
    workers: HashMap<WorkerId, WorkerSlot>,
    tx: mpsc::UnboundedSender<ChildOutcome>,
    bus: Option<AgentBus>,
    supervisor_agent: Option<AgentId>,
    audit: Option<Arc<Mutex<AuditLog>>>,
    events: VecDeque<WorkerEvent>,
    /// 可选租约管理器：worker 持有租约，submit 前 fencing 校验（epoch/token）。
    leases: Option<crate::lease::LeaseManager>,
}

/// worker 子进程池（Clone 共享同一池）。
#[derive(Clone)]
pub struct WorkerPool {
    inner: Arc<AsyncMutex<PoolInner>>,
}

impl fmt::Debug for WorkerPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Ok(inner) = self.inner.try_lock() {
            f.debug_map()
                .entries(inner.workers.iter().map(|(id, slot)| (id, &slot.status)))
                .finish()
        } else {
            f.write_str("WorkerPool { <locked> }")
        }
    }
}

impl Drop for WorkerPool {
    /// 安全网：池被丢弃时同步 kill 全部子进程（start_kill 为同步 API）。
    /// 调用方应先用 `shutdown`/`cancel_all` 收尾；本实现保证不残留孤儿进程。
    fn drop(&mut self) {
        // 仅当这是最后一个池克隆时兜底 kill（安全网）：Clone 共享同一 inner，
        // 若每次 Drop 都 kill，则 PoolWorker 等临时克隆被丢弃时会误杀存活子进程。
        if Arc::strong_count(&self.inner) > 1 {
            return;
        }
        if let Ok(mut inner) = self.inner.try_lock() {
            for slot in inner.workers.values_mut() {
                if let Some(child) = slot.child.as_mut() {
                    let _ = child.start_kill();
                }
            }
        }
    }
}

impl Default for WorkerPool {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerPool {
    /// 新建池（必须在 tokio runtime 内调用；内部启动调度循环任务）。
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(AsyncMutex::new(PoolInner {
            workers: HashMap::new(),
            tx,
            bus: None,
            supervisor_agent: None,
            audit: None,
            events: VecDeque::new(),
            leases: None,
        }));
        let dispatcher = Arc::clone(&inner);
        tokio::spawn(async move { Self::dispatch_loop(dispatcher, rx).await });
        Self { inner }
    }

    /// 挂接租约管理器：worker 持有租约（spawn 时 acquire，submit 前 fencing 校验，
    /// terminate/kill 时 release）。
    pub async fn attach_leases(&self, leases: crate::lease::LeaseManager) {
        let mut inner = self.inner.lock().await;
        inner.leases = Some(leases);
    }

    /// 注册进总线：崩溃/重启/熔断等事件发往 `supervisor_agent`（需先 register 该 agent）。
    pub async fn attach_bus(&self, bus: AgentBus, supervisor_agent: impl Into<AgentId>) {
        let mut inner = self.inner.lock().await;
        inner.bus = Some(bus);
        inner.supervisor_agent = Some(supervisor_agent.into());
    }

    /// 附加审计日志：每次生命周期事件写一条 `worker.<kind>` 记录。
    pub async fn attach_audit(&self, log: Arc<Mutex<AuditLog>>) {
        let mut inner = self.inner.lock().await;
        inner.audit = Some(log);
    }

    /// spawn 一个 worker：启动子进程 + ready 握手（结构化协议就绪）后返回。
    pub async fn spawn(&self, spec: WorkerSpec) -> Result<WorkerId, PoolError> {
        let id = spec.id.clone();
        self.spawn_inner(spec, id.clone(), true).await?;
        self.emit(
            &id,
            WorkerEventKind::Started,
            "worker 已启动".to_string(),
            new_correlation_id(),
        )
        .await;
        Ok(id)
    }

    pub async fn contains(&self, id: &str) -> bool {
        self.inner.lock().await.workers.contains_key(id)
    }

    pub async fn worker_count(&self) -> usize {
        self.inner.lock().await.workers.len()
    }

    pub async fn workers(&self) -> Vec<WorkerId> {
        self.inner.lock().await.workers.keys().cloned().collect()
    }

    pub async fn status(&self, id: &str) -> Option<WorkerStatus> {
        self.inner
            .lock()
            .await
            .workers
            .get(id)
            .map(|s| s.status.clone())
    }

    pub async fn pid(&self, id: &str) -> Option<u32> {
        self.inner.lock().await.workers.get(id).and_then(|s| s.pid)
    }

    /// 最近事件（上限 EVENT_CAP 条；总线/审计之外的本地视图）。
    pub async fn events(&self) -> Vec<WorkerEvent> {
        self.inner.lock().await.events.iter().cloned().collect()
    }

    /// 心跳：ping → pong（结构化协议活性探测）。
    pub async fn ping(&self, id: &str) -> Result<(), PoolError> {
        let rx = {
            let mut inner = self.inner.lock().await;
            let slot = inner
                .workers
                .get_mut(id)
                .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?;
            if slot.exited {
                return Err(PoolError::NotReady(id.to_string()));
            }
            let (tx, rx) = oneshot::channel();
            // 先注册 pong 期望，再写 ping：防"子进程极快回包但父进程尚未登记"的竞态。
            slot.ping = Some(tx);
            let write = async {
                let stdin = slot
                    .stdin
                    .as_mut()
                    .ok_or_else(|| PoolError::NotReady(id.to_string()))?;
                stdin
                    .write_all(ping_line().as_bytes())
                    .await
                    .map_err(|e| PoolError::Io(e.to_string()))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| PoolError::Io(e.to_string()))?;
                Ok::<(), PoolError>(())
            };
            // 写入也纳入超时：子进程不读 stdin 时不得让写侧挂死整个池。
            match tokio::time::timeout(PING_TIMEOUT, write).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    slot.ping = None;
                    return Err(e);
                }
                Err(_) => {
                    slot.ping = None;
                    return Err(PoolError::Timeout(format!("worker {id} 心跳写入超时")));
                }
            }
            rx
        };
        tokio::time::timeout(PING_TIMEOUT, rx)
            .await
            .map_err(|_| PoolError::Timeout(format!("worker {id} 心跳超时")))?
            .map_err(|_| PoolError::Protocol(format!("worker {id} pong 通道异常")))?;
        Ok(())
    }

    /// 健康检查：ping 失败/已退出 → 走崩溃自愈（指数退避重启 / 熔断）。
    pub async fn check_health(&self, id: &str) -> Result<WorkerStatus, PoolError> {
        let exited = self
            .inner
            .lock()
            .await
            .workers
            .get(id)
            .map(|s| s.exited)
            .unwrap_or(false);
        let alive = if exited {
            false
        } else {
            self.ping(id).await.is_ok()
        };
        if alive {
            Ok(WorkerStatus::Running)
        } else {
            self.handle_crash(id, "心跳/退出检测").await
        }
    }

    /// 提交结构化任务：发送 task 消息并等待结果（时长预算到期中止并 kill）。
    pub async fn submit(&self, id: &str, input: &serde_json::Value) -> Result<String, PoolError> {
        let (rx, correlation_id) = {
            let mut inner = self.inner.lock().await;
            // 租约 fencing 校验：worker 租约失效（过期/重连/分区只读）时拒绝提交。
            // 先取租约（不可变借用）再取 slot（可变借用），避免借用冲突。
            if let Some(leases) = inner.leases.clone() {
                match leases.lease(id) {
                    Some(lease) => {
                        if let Err(e) =
                            leases.verify_write(&lease.holder, &lease.token, lease.epoch)
                        {
                            return Err(PoolError::Io(format!("worker {id} fencing 拒绝：{e}")));
                        }
                    }
                    None => {
                        return Err(PoolError::Io(format!("worker {id} 无租约，拒绝提交")));
                    }
                }
            }
            let slot = inner
                .workers
                .get_mut(id)
                .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?;
            if matches!(slot.status, WorkerStatus::Fused { .. }) {
                return Err(PoolError::Fused(id.to_string()));
            }
            if slot.status == WorkerStatus::Stopped {
                return Err(PoolError::Stopped(id.to_string()));
            }
            if slot.exited {
                return Err(PoolError::NotReady(id.to_string()));
            }
            let max_turns = slot.spec.budget.max_turns;
            if max_turns > 0 && slot.turns >= max_turns {
                return Err(PoolError::BudgetTurns {
                    worker: id.to_string(),
                    max_turns,
                });
            }
            let task_id = uuid::Uuid::new_v4().to_string();
            let correlation_id = new_correlation_id();
            let line = task_line(&task_id, &correlation_id, input);
            let (tx, rx) = oneshot::channel();
            // 先登记 pending 再写 stdin：防"子进程极快回包但父进程尚未登记"的竞态丢结果。
            slot.pending.insert(task_id.clone(), tx);
            let write = async {
                let stdin = slot
                    .stdin
                    .as_mut()
                    .ok_or_else(|| PoolError::NotReady(id.to_string()))?;
                stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| PoolError::Io(e.to_string()))?;
                stdin
                    .flush()
                    .await
                    .map_err(|e| PoolError::Io(e.to_string()))?;
                Ok::<(), PoolError>(())
            };
            if let Err(e) = write.await {
                slot.pending.remove(&task_id);
                return Err(e);
            }
            slot.turns = slot.turns.saturating_add(1);
            (rx, correlation_id)
        };

        let deadline = {
            let inner = self.inner.lock().await;
            inner
                .workers
                .get(id)
                .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?
                .started_deadline()
        };
        let result = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    None
                } else {
                    tokio::time::timeout(deadline - now, rx).await.ok()
                }
            }
            None => Some(rx.await),
        };
        match result {
            Some(Ok(Ok(output))) => {
                self.mark_healthy(id).await;
                Ok(output)
            }
            Some(Ok(Err(err))) if err == CANCELLED_MARKER => {
                Err(PoolError::Cancelled(id.to_string()))
            }
            Some(Ok(Err(err))) => Err(PoolError::WorkerFailed(err)),
            Some(Err(_)) => Err(PoolError::NotReady(id.to_string())),
            None => {
                let reason = "worker 时长预算到期".to_string();
                self.abort_budget(id, &reason, &correlation_id).await;
                Err(PoolError::BudgetDuration {
                    worker: id.to_string(),
                    reason,
                })
            }
        }
    }

    /// 取消该 worker 的全部待处理任务（取消传播：pending 立即以 cancelled 解决；
    /// 子进程侧由 cancel 消息通知，阻塞中的任务随后 kill 清理）。返回取消数。
    pub async fn cancel_pending(&self, id: &str) -> Result<usize, PoolError> {
        let count = {
            let mut inner = self.inner.lock().await;
            let slot = inner
                .workers
                .get_mut(id)
                .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?;
            let count = slot.pending.len();
            let lines: Vec<String> = slot
                .pending
                .keys()
                .map(|task_id| cancel_line(task_id))
                .collect();
            if let Some(stdin) = slot.stdin.as_mut() {
                for line in &lines {
                    let _ = stdin.write_all(line.as_bytes()).await;
                    let _ = stdin.flush().await;
                }
            }
            for (_, tx) in slot.pending.drain() {
                let _ = tx.send(Err(CANCELLED_MARKER.to_string()));
            }
            count
        };
        if count > 0 {
            self.emit(
                id,
                WorkerEventKind::Cancelled,
                format!("取消 {count} 个待处理任务"),
                new_correlation_id(),
            )
            .await;
        }
        Ok(count)
    }

    /// 取消全部 worker 的待处理任务。
    pub async fn cancel_all(&self) -> usize {
        let ids = self.workers().await;
        let mut total = 0usize;
        for id in ids {
            if let Ok(n) = self.cancel_pending(&id).await {
                total += n;
            }
        }
        total
    }

    /// kill 单个 worker（终止子进程并回收；状态 Stopped）。
    pub async fn kill(&self, id: &str) -> Result<(), PoolError> {
        self.terminate(id, "worker 被 kill".to_string()).await;
        {
            let mut inner = self.inner.lock().await;
            let slot = inner
                .workers
                .get_mut(id)
                .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?;
            slot.status = WorkerStatus::Stopped;
        }
        self.emit(
            id,
            WorkerEventKind::Stopped,
            "kill".to_string(),
            new_correlation_id(),
        )
        .await;
        Ok(())
    }

    /// 手动重启（终止 + 立即按原规格拉起，不做退避；自动退避在 [`Self::check_health`]）。
    pub async fn restart(&self, id: &str) -> Result<WorkerStatus, PoolError> {
        self.terminate(id, "手动重启".to_string()).await;
        self.respawn(id).await?;
        Ok(WorkerStatus::Running)
    }

    /// 关闭全部 worker：发送 shutdown、kill、回收、标记 Stopped。
    pub async fn shutdown(&self) {
        let ids = self.workers().await;
        for id in ids {
            // 尽力发送 shutdown 协议消息。
            {
                let mut inner = self.inner.lock().await;
                if let Some(slot) = inner.workers.get_mut(&id) {
                    if let Some(stdin) = slot.stdin.as_mut() {
                        let _ = stdin.write_all(shutdown_line().as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                }
            }
            self.terminate(&id, "shutdown".to_string()).await;
            {
                let mut inner = self.inner.lock().await;
                if let Some(slot) = inner.workers.get_mut(&id) {
                    slot.status = WorkerStatus::Stopped;
                }
            }
            self.emit(
                &id,
                WorkerEventKind::Stopped,
                "shutdown".to_string(),
                new_correlation_id(),
            )
            .await;
        }
    }

    // ---------- 内部实现 ----------

    async fn emit(
        &self,
        worker: &str,
        kind: WorkerEventKind,
        detail: String,
        correlation_id: CorrelationId,
    ) {
        let event = WorkerEvent::new(worker, kind, detail, correlation_id);
        let (bus, supervisor, audit) = {
            let mut inner = self.inner.lock().await;
            inner.events.push_back(event.clone());
            if inner.events.len() > EVENT_CAP {
                inner.events.pop_front();
            }
            (
                inner.bus.clone(),
                inner.supervisor_agent.clone(),
                inner.audit.clone(),
            )
        };
        if let (Some(bus), Some(supervisor)) = (bus, supervisor) {
            let _ = bus.send_worker_event(worker, supervisor, &event).await;
        }
        if let Some(audit) = audit {
            if let Ok(mut log) = audit.lock() {
                log.record(
                    "worker_pool",
                    &format!("worker.{}", kind.label()),
                    Some("worker_pool".to_string()),
                    None,
                    format!("{}: {}", event.worker, event.detail),
                );
            }
        }
    }

    /// 终止旧子进程并解决所有 pending（不改变状态；由调用方设置状态与事件）。
    /// stdin/stdout 生命周期：先取走 stdin 并丢弃（子进程侧读到 EOF 走协议退出），
    /// 再 kill 兜底回收，最后 wait 收尸，保证无孤儿。
    async fn terminate(&self, id: &str, detail: String) {
        let (child, stdin) = {
            let mut inner = self.inner.lock().await;
            // 租约释放：worker 终止（kill/重启/崩溃自愈）即释放租约，迁移语义。
            if let Some(leases) = &inner.leases {
                if let Some(lease) = leases.lease(id) {
                    let _ = leases.release(id, &lease.token);
                }
            }
            if let Some(slot) = inner.workers.get_mut(id) {
                slot.exited = true;
                // 丢弃 ping 期望：退出时心跳必须失败（不得误报 pong 成功）。
                slot.ping.take();
                for (_, tx) in slot.pending.drain() {
                    let _ = tx.send(Err(detail.clone()));
                }
                (slot.child.take(), slot.stdin.take())
            } else {
                (None, None)
            }
        };
        // 先关闭 stdin（优雅路径：子进程协议在 EOF 时正常退出），再 kill 兜底。
        drop(stdin);
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// 崩溃自愈：终止 → 指数退避 → 重启；重启即崩则继续退避；连续失败超限 → 熔断。
    async fn handle_crash(&self, id: &str, reason: &str) -> Result<WorkerStatus, PoolError> {
        loop {
            self.terminate(id, format!("崩溃：{reason}")).await;
            let state = {
                let mut inner = self.inner.lock().await;
                let slot = inner
                    .workers
                    .get_mut(id)
                    .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?;
                slot.supervisor.on_crash()
            };
            match state {
                SupervisionState::Restarting {
                    attempts,
                    next_retry_secs,
                } => {
                    self.emit(
                        id,
                        WorkerEventKind::Crashed,
                        format!("{reason}（attempt {attempts}）"),
                        new_correlation_id(),
                    )
                    .await;
                    if next_retry_secs > 0 {
                        tokio::time::sleep(Duration::from_secs(next_retry_secs)).await;
                    }
                    match self.respawn(id).await {
                        Ok(()) => {
                            self.emit(
                                id,
                                WorkerEventKind::Restarted,
                                format!("attempt {attempts}，退避 {next_retry_secs}s 后重启"),
                                new_correlation_id(),
                            )
                            .await;
                            return Ok(WorkerStatus::Running);
                        }
                        Err(_) => continue, // 重启即崩 → 继续退避/熔断
                    }
                }
                SupervisionState::Fused { attempts } => {
                    {
                        let mut inner = self.inner.lock().await;
                        if let Some(slot) = inner.workers.get_mut(id) {
                            slot.status = WorkerStatus::Fused { attempts };
                        }
                    }
                    self.emit(
                        id,
                        WorkerEventKind::Fused,
                        format!("连续 {attempts} 次失败，熔断"),
                        new_correlation_id(),
                    )
                    .await;
                    return Err(PoolError::Fused(id.to_string()));
                }
                SupervisionState::Healthy => return Ok(WorkerStatus::Running),
            }
        }
    }

    /// 预算中止：kill 子进程 + 解决 pending + 事件 + 状态 Stopped。
    async fn abort_budget(&self, id: &str, reason: &str, correlation_id: &CorrelationId) {
        self.terminate(id, reason.to_string()).await;
        {
            let mut inner = self.inner.lock().await;
            if let Some(slot) = inner.workers.get_mut(id) {
                slot.status = WorkerStatus::Stopped;
            }
        }
        self.emit(
            id,
            WorkerEventKind::BudgetAborted,
            reason.to_string(),
            correlation_id.clone(),
        )
        .await;
    }

    async fn mark_healthy(&self, id: &str) {
        let mut inner = self.inner.lock().await;
        if let Some(slot) = inner.workers.get_mut(id) {
            slot.supervisor.mark_healthy();
        }
    }

    async fn respawn(&self, id: &str) -> Result<(), PoolError> {
        let spec = {
            let inner = self.inner.lock().await;
            inner
                .workers
                .get(id)
                .ok_or_else(|| PoolError::UnknownWorker(id.to_string()))?
                .spec
                .clone()
        };
        self.spawn_inner(spec, id.to_string(), false).await
    }

    /// 启动子进程 + ready 握手。`reset_supervisor=true` 时为全新 worker（重置崩溃计数）。
    async fn spawn_inner(
        &self,
        spec: WorkerSpec,
        id: WorkerId,
        reset_supervisor: bool,
    ) -> Result<(), PoolError> {
        // 若已存在存活子进程（重复 spawn 同 id），先终止。
        self.terminate(&id, "重新 spawn".to_string()).await;

        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args);
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        // 真白名单：先清空继承环境，再只注入 env_whitelist 声明的变量。
        // 不清空则子进程继承宿主全部环境（含 OPENAI_API_KEY 等凭据），白名单形同虚设。
        cmd.env_clear();
        for (key, value) in &spec.env_whitelist {
            cmd.env(key, value);
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| PoolError::Spawn(format!("{}: {e}", spec.command.display())))?;
        let pid = child.id();
        // 管道取用失败必须 kill 已 spawn 的进程，不得泄漏孤儿。
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(PoolError::Spawn("stdin 不可用".to_string()));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                drop(stdin);
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(PoolError::Spawn("stdout 不可用".to_string()));
            }
        };

        let gen = {
            let mut inner = self.inner.lock().await;
            let slot = inner
                .workers
                .entry(id.clone())
                .or_insert_with(|| WorkerSlot {
                    spec: spec.clone(),
                    child: None,
                    stdin: None,
                    pid: None,
                    supervisor: Supervisor::new(spec.restart_rule),
                    status: WorkerStatus::Starting,
                    turns: 0,
                    started_at: None,
                    exited: false,
                    gen: 0,
                    pending: HashMap::new(),
                    ping: None,
                    bad_lines: 0,
                    last_bad_line_at: None,
                });
            if reset_supervisor {
                slot.supervisor = Supervisor::new(spec.restart_rule);
            }
            slot.gen = slot.gen.saturating_add(1);
            slot.child = Some(child);
            slot.stdin = Some(stdin);
            slot.pid = pid;
            slot.status = WorkerStatus::Starting;
            slot.exited = false;
            slot.turns = 0;
            slot.started_at = None;
            let gen = slot.gen;
            Self::spawn_reader(id.clone(), gen, stdout, inner.tx.clone());
            gen
        };

        // ready 握手轮询。dispatch 按序处理 Ready（status=Running）与 Exited（exited=true），
        // 因此先查 Running 再查 exited：瞬时退出（ready 后立即 exit）的 child 不算启动失败。
        // 过期代（旧 child）的 Exited 由 gen 过滤，不会污染本代状态。
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            {
                let inner = self.inner.lock().await;
                let slot = inner
                    .workers
                    .get(&id)
                    .ok_or_else(|| PoolError::UnknownWorker(id.clone()))?;
                if slot.status == WorkerStatus::Running && slot.gen == gen {
                    break;
                }
                if slot.exited {
                    return Err(PoolError::Spawn(format!(
                        "worker {id} 启动后立即退出（协议未就绪）"
                    )));
                }
            }
            if Instant::now() >= deadline {
                // ready 超时：子进程可能仍存活，必须终止回收，不得泄漏孤儿。
                self.terminate(&id, "ready 超时".to_string()).await;
                return Err(PoolError::Spawn(format!("worker {id} ready 超时")));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        // worker 租约：spawn/重启成功即持有（submit 前 fencing 校验；终止时释放）。
        {
            let inner = self.inner.lock().await;
            if let Some(leases) = &inner.leases {
                if let Err(e) = leases.acquire(&id) {
                    self.terminate(&id, "租约获取失败".to_string()).await;
                    return Err(PoolError::Spawn(format!("worker {id} 租约获取失败：{e}")));
                }
            }
        }
        Ok(())
    }

    /// 子进程 stdout 读取任务：逐行解析结构化消息送入调度循环；EOF 上报 Exited。
    /// 消息携带 spawn 代数（gen），调度循环据此丢弃过期代（旧 child）的消息。
    fn spawn_reader(
        worker: WorkerId,
        gen: u64,
        stdout: ChildStdout,
        tx: mpsc::UnboundedSender<ChildOutcome>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let outcome = match parse_child_line(&line) {
                            Ok(msg) => outcome_from_msg(&worker, gen, msg),
                            Err(_) => ChildOutcome::BadLine {
                                worker: worker.clone(),
                                gen,
                            },
                        };
                        if tx.send(outcome).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let _ = tx.send(ChildOutcome::Exited { worker, gen });
        })
    }

    /// 调度循环：独占处理子进程回报（ready/pong/result/exit/badline）。
    async fn dispatch_loop(
        inner: Arc<AsyncMutex<PoolInner>>,
        mut rx: mpsc::UnboundedReceiver<ChildOutcome>,
    ) {
        while let Some(outcome) = rx.recv().await {
            let mut inner = inner.lock().await;
            match outcome {
                ChildOutcome::Ready { worker, gen } => {
                    if let Some(slot) = inner.workers.get_mut(&worker) {
                        if slot.gen != gen {
                            continue; // 过期代消息（旧 child 的 ready），忽略
                        }
                        if matches!(
                            slot.status,
                            WorkerStatus::Starting | WorkerStatus::Restarting { .. }
                        ) {
                            slot.status = WorkerStatus::Running;
                            slot.exited = false;
                            slot.started_at = Some(Instant::now());
                        }
                    }
                }
                ChildOutcome::Pong { worker, gen } => {
                    if let Some(slot) = inner.workers.get_mut(&worker) {
                        if slot.gen != gen {
                            continue;
                        }
                        if let Some(tx) = slot.ping.take() {
                            let _ = tx.send(());
                        }
                    }
                }
                ChildOutcome::Result {
                    worker,
                    gen,
                    task_id,
                    result,
                } => {
                    if let Some(slot) = inner.workers.get_mut(&worker) {
                        // 旧代结果的 task_id 与当代 UUID 不冲突，但按代过滤更严谨。
                        if slot.gen != gen {
                            continue;
                        }
                        if let Some(tx) = slot.pending.remove(&task_id) {
                            let _ = tx.send(result);
                        }
                    }
                }
                ChildOutcome::Exited { worker, gen } => {
                    if let Some(slot) = inner.workers.get_mut(&worker) {
                        if slot.gen != gen {
                            continue;
                        }
                        slot.exited = true;
                        for (_, tx) in slot.pending.drain() {
                            let _ = tx.send(Err("worker 已退出".to_string()));
                        }
                        // 丢弃 ping 期望：退出时心跳必须失败（不得误报 pong 成功）。
                        slot.ping.take();
                    }
                }
                // 非结构化行：忽略（不解释、不传递），协议严格性体现在"只认 JSON 行"。
                ChildOutcome::BadLine { worker, gen } => {
                    if let Some(slot) = inner.workers.get_mut(&worker) {
                        if slot.gen != gen {
                            continue;
                        }
                        slot.bad_lines += 1;
                        slot.last_bad_line_at = Some(std::time::Instant::now());
                    }
                }
            }
        }
    }
}

/// 池 worker 适配：让 `goal.rs` 的 `Worker` trait 走子进程执行。
pub struct PoolWorker {
    pool: WorkerPool,
    worker_id: WorkerId,
}

impl fmt::Debug for PoolWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolWorker")
            .field("worker_id", &self.worker_id)
            .finish()
    }
}

impl PoolWorker {
    pub fn new(pool: WorkerPool, worker_id: impl Into<WorkerId>) -> Self {
        Self {
            pool,
            worker_id: worker_id.into(),
        }
    }
}

#[async_trait]
impl crate::goal::Worker for PoolWorker {
    fn name(&self) -> &str {
        &self.worker_id
    }

    async fn run(&self, input: &serde_json::Value) -> Result<String, String> {
        self.pool
            .submit(&self.worker_id, input)
            .await
            .map_err(|e| e.to_string())
    }
}

/// 子进程侧协议入口（测试二进制 / 真实 worker 宿主均可复用）。
pub mod child {
    use serde_json::Value;
    use std::io::{BufRead, Write};

    /// 子进程协议主循环：启动即上报 `ready`，然后从 stdin 读 JSON 行、向 stdout 写 JSON 行
    /// （stderr 供人读诊断）。
    /// - `task`：调用 `handler(input)`，结果回写 `result`（ok/error 字段）。
    /// - `ping`：回 `pong`。
    /// - `shutdown` / EOF：正常退出（exit 0）。
    /// - handler 内可用 `std::process::exit(n)` 模拟崩溃。
    pub fn run_child_protocol<F>(mut handler: F) -> !
    where
        F: FnMut(&Value) -> Result<String, String>,
    {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        // ready 握手：告知父进程结构化协议已经就绪
        let _ = writeln!(out, "{{\"type\":\"ready\"}}");
        let _ = out.flush();
        let mut line = String::new();
        loop {
            line.clear();
            let read = match stdin.lock().read_line(&mut line) {
                Ok(n) => n,
                Err(_) => break,
            };
            if read == 0 {
                break;
            }
            let msg: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match msg.get("cmd").and_then(Value::as_str) {
                Some("shutdown") => break,
                Some("ping") => {
                    let _ = writeln!(out, "{{\"type\":\"pong\"}}");
                }
                Some("cancel") => {}
                Some("task") => {
                    let task_id = msg
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let input = msg.get("input").cloned().unwrap_or(Value::Null);
                    match handler(&input) {
                        Ok(output) => {
                            let _ = writeln!(
                                out,
                                "{{\"type\":\"result\",\"task_id\":{},\"ok\":true,\"output\":{}}}",
                                serde_json::json!(task_id),
                                serde_json::json!(output)
                            );
                        }
                        Err(error) => {
                            let _ = writeln!(
                                out,
                                "{{\"type\":\"result\",\"task_id\":{},\"ok\":false,\"error\":{}}}",
                                serde_json::json!(task_id),
                                serde_json::json!(error)
                            );
                        }
                    }
                }
                _ => {}
            }
            let _ = out.flush();
        }
        std::process::exit(0);
    }
}
