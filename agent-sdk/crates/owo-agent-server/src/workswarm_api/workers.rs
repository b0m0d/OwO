use super::{project_workspace, workspace_change_tracker, workswarm_metrics};
use async_trait::async_trait;
use owo_agent_core::gateway::ModelProvider;
use owo_agent_core::goal::Worker;
use owo_agent_core::permissions::AutoApprover;
use owo_agent_core::subagent::SubagentRunner;
use owo_agent_core::worker_profile::{ProfileSubagentRunner, WorkerProfile};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::Duration;

/// 真实 Agent 子代理 worker（name="agent"）：prompt → 子代理执行。
///
/// 五期（第三路）：与 `Agent::run_subagent` 同口径（顶层 depth=0、
/// 子代理 max_turns 上限 12），但 Provider 支持 [`workswarm_metrics::MeasuredProvider`]
/// 计数装饰器注入——`model_calls` 由 `MeasuredRoleWorker` 逐 span 精确统计
/// （每次模型调用恰好经过 `complete`/`complete_stream` 其一）。
/// 七期（第二路）：角色画像驱动——工具注册表按 `WorkerProfile` 装配（注册表面即
/// 权限边界）、回合上限取模板预算、写面为「角色 ∩ 绑定」交集白名单。
pub struct AgentSubagentWorker {
    pub(super) agent: Arc<owo_agent_core::Agent>,
    pub(super) workspace: PathBuf,
    /// 指标层注入的 per-span 模型调用计数（None = 不计数，行为不变）。
    pub(super) model_calls: Option<Arc<AtomicU64>>,
    /// 六期（第二路）：项目工作区绑定作用域（None = 全局工作区，行为不变）。
    /// 绑定后：运行目录 = 绑定根；只读绑定强制 read_only；写白名单经审批器强制。
    pub(super) workspace_scope: Option<project_workspace::WorkspaceScope>,
    /// 七期（第二路）：角色画像（工具面/只读/回合上限；None = 防御分支退回通用执行器）。
    pub(super) profile: Option<WorkerProfile>,
    /// 七期（第二路）：critic 角色代理（服务端口径：`role == "critic"` 字面量；
    /// 引擎注入的 read_only 只覆盖 critic，其余内置角色都是 producer，画像另管只读面）。
    pub(super) is_critic: bool,
    /// 七期（第二路）：团队取消桥共享标志（None = 本地标志，行为退化为不可中断）。
    pub(super) cancel_flag: Option<Arc<AtomicBool>>,
    /// 七期（第二路）：最终写白名单（角色 ∩ 绑定交集；空 = 工作区内可写）。
    pub(super) write_allowed: Vec<PathBuf>,
}

#[async_trait]
impl Worker for AgentSubagentWorker {
    fn name(&self) -> &str {
        "agent"
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let prompt = input
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "agent 步骤缺少 prompt 参数".to_string())?;
        let input_read_only = input
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        // 只读三层叠加（七期 · 二路）：步骤输入 → 团队绑定只读（团队级上限）→
        // 角色画像只读（角色级上限）。任一只读即只读。
        let read_only = input_read_only
            || self
                .workspace_scope
                .as_ref()
                .map(|s| s.read_only)
                .unwrap_or(false)
            || self.profile.as_ref().map(|p| p.read_only).unwrap_or(false);
        if std::env::var("OPENAI_API_KEY")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            return Err(
                "缺少 OPENAI_API_KEY，agent 角色无法调用模型（请配置凭据后重试）".to_string(),
            );
        }
        let model = input
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                std::env::var("OWO_AGENT_MODEL")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| "gpt-4.1-mini".to_string());
        // 指标计数注入：MeasuredProvider 包装共享 provider（计数仅对本 span 生效）。
        let provider: Arc<dyn ModelProvider> = match &self.model_calls {
            Some(counter) => Arc::new(workswarm_metrics::MeasuredProvider::new(
                self.agent.provider(),
                Arc::clone(counter),
            )),
            None => self.agent.provider(),
        };
        // 审批器（七期 · 二路）：绑定作用域 → 白名单审批器，白名单取「角色 ∩ 绑定」
        // 交集（角色白名单空 = 绑定原样）；未绑定保持 AutoApprover 原行为。
        let scope = self.workspace_scope.as_ref();
        let allow_writes = !read_only;
        let workspace_approver;
        let fallback_approver = AutoApprover { allow: read_only };
        let approver: &dyn owo_agent_core::permissions::Approver = match scope {
            Some(s) => {
                let allowed = if self.write_allowed.is_empty() {
                    s.allowed.clone()
                } else {
                    self.write_allowed.clone()
                };
                workspace_approver = project_workspace::WorkspaceScopeApprover {
                    allow_writes,
                    root: s.root.clone(),
                    allowed,
                };
                &workspace_approver
            }
            None => &fallback_approver,
        };
        // 中断标志（七期 · 二路）：优先团队取消桥共享标志（cancel → 即时置位，
        // run_turn 协作式中断）；None = 本地标志（行为退化为不可中断）。
        let local_abort = AtomicBool::new(false);
        let abort: &AtomicBool = match &self.cancel_flag {
            Some(flag) => flag,
            None => &local_abort,
        };
        // 七期（第二路）：画像执行器——注册表面即权限边界 + 模板预算回合上限；
        // 无画像（防御分支，现网构建路径恒有画像）退回通用执行器保持旧行为。
        let output = match &self.profile {
            Some(profile) => {
                let runner = ProfileSubagentRunner {
                    provider,
                    approver,
                    abort,
                    depth: 0,
                    model,
                    is_critic: self.is_critic,
                    write_allowed: self.write_allowed.clone(),
                    profile: profile.clone(),
                };
                runner.run(&self.workspace, prompt).await
            }
            None => {
                let runner = SubagentRunner {
                    provider,
                    approver,
                    abort,
                    depth: 0,
                    max_turns: 12,
                    model,
                };
                runner.run(&self.workspace, prompt, read_only).await
            }
        }
        .map_err(|e| format!("agent 子代理执行失败：{e}"))?;
        Ok(output)
    }
}

pub(super) struct EchoWorker;

#[async_trait]
impl Worker for EchoWorker {
    fn name(&self) -> &str {
        "echo"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Ok(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| input.to_string()))
    }
}

pub(super) struct SleepWorker;

#[async_trait]
impl Worker for SleepWorker {
    fn name(&self) -> &str {
        "sleep"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        let ms = input
            .get("ms")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(600_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(format!("slept {ms}ms"))
    }
}

pub(super) struct FailWorker;

#[async_trait]
impl Worker for FailWorker {
    fn name(&self) -> &str {
        "fail"
    }
    async fn run(&self, input: &Value) -> Result<String, String> {
        Err(input
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "fail worker 注入失败".to_string()))
    }
}

/// 追踪型 worker 包装（七期 · 二路）：插在内层 worker 与 `RoleWorker` 之间——
/// 1) 写角色先取单写租约（同一工作区同时只允许一个写角色在执行；读角色不参与）；
/// 2) 拿到租约后采集前快照（等待租约的时间不计入变更窗口——否则会把其他写角色
///    正在进行的变更记到本步骤头上）；
/// 3) 执行后采集后快照，登记「变更文件 + diff 摘要 + diff ref」，并对窗口内新增
///    变更做写白名单校验——越界转 `scope_violation` 步骤失败（引擎按失败处理，
///    不登记成功 Artifact）。
///
/// 仅写角色包装追踪（`tracking`/`lease` 均为 None 时纯透传）：读角色没有写工具
/// 不会改文件，而并行读角色的快照窗口会误捕写角色的变更。
///
/// 八期（二路）：执行前对允许路径做内容基线快照（进 CAS）；成功路径自动生成
/// `ChangeSet`（pending_review，等人工 accept/reject/revert；未接受时该团队代码
/// Artifact 不得成为最终 approved head——门控函数见 `change_set_store`）。
pub(super) struct TrackedRoleWorker {
    pub(super) inner: Arc<dyn Worker>,
    /// 单写租约（None = 只读角色，不参与租约）。
    pub(super) lease: Option<Arc<tokio::sync::Mutex<()>>>,
    /// 变更追踪配置（None = 不追踪，纯透传）。
    pub(super) tracking: Option<workspace_change_tracker::Tracker>,
}

#[async_trait]
impl Worker for TrackedRoleWorker {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn run(&self, input: &Value) -> Result<String, String> {
        let Some(tracking) = &self.tracking else {
            return self.inner.run(input).await;
        };
        // 单写租约：写角色串行化（等待发生在前快照之前——窗口只覆盖本步骤）。
        // 十期·四路 R1：租约覆盖「前快照 → 执行 → 后快照 → 变更登记」全程——
        // 变更登记（record + ChangeSet upsert）完成前不释放：并发写步骤若在
        // 本步骤后快照前插窗口，会把本步骤的变更误算进它的前基线，反之亦然。
        let _lease = match &self.lease {
            Some(lease) => Some(lease.lock().await),
            None => None,
        };
        let pre = workspace_change_tracker::GitSnapshot::snapshot(&tracking.root).await;
        // 八期（二路）：执行前对允许路径做内容基线快照（进 CAS）——ChangeSet
        // reject/revert 的恢复依据；快照尽力而为（超限/读取失败的文件基线记为
        // 「未知」，之后被改只能走 conflicted 人工路径，绝不误删）。
        let base = owo_agent_core::change_set::snapshot_allowed_paths(
            &tracking.root,
            &tracking.allowed,
            &tracking.cas,
        )
        .await;
        // 九期（一路）：执行前已脏文件的内容哈希——合并变更检测的第二基线。
        // 允许路径内的文件直接复用 base.entries（同一份数据，避免重复读盘）；
        // 之外（或基线未覆盖）的执行前脏文件现场读哈希。
        let mut pre_dirty_hashes: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        for path in pre.dirty_paths() {
            if let Some(hash) = base.entries.get(&path) {
                pre_dirty_hashes.insert(path, Some(hash.clone()));
            } else {
                let hash = workspace_change_tracker::content_hash(&tracking.root, &path);
                pre_dirty_hashes.insert(path, hash);
            }
        }
        // 执行（成败、超时、取消都以 result 承接——变更收尾对四种结局一视同仁）。
        let result = self.inner.run(input).await;
        // ---- 变更收尾：成功/失败/超时/取消都必须完成 ----
        let post = workspace_change_tracker::GitSnapshot::snapshot(&tracking.root).await;
        // 十期·四路 R1：非 git 目录（含 git 快照不可用）改用内容哈希检测——
        // 执行后对同一组允许路径再采内容快照，与执行前 CAS 基线逐文件比哈希
        // （新增/修改/删除），不依赖 git。
        let (changed, detection) = if pre.git && post.git {
            (
                workspace_change_tracker::merge_changed_files(
                    &pre,
                    &post,
                    &pre_dirty_hashes,
                    &tracking.root,
                ),
                "git 窗口+内容哈希",
            )
        } else {
            let post_base = owo_agent_core::change_set::snapshot_allowed_paths(
                &tracking.root,
                &tracking.allowed,
                &tracking.cas,
            )
            .await;
            (
                owo_agent_core::change_set::merge_content_snapshots(&base, &post_base),
                "内容哈希（非 git）",
            )
        };
        let violation =
            workspace_change_tracker::check_whitelist(&changed, &tracking.root, &tracking.allowed)
                .err();
        let step = input
            .get("_workswarm")
            .and_then(|meta| meta.get("step_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let record = match tracking
            .record(&step, &post, &changed, violation.as_deref(), Some(&base))
            .await
        {
            Ok(record) => Some(record),
            Err(error) => {
                tracing::warn!(
                    team_id = %tracking.team_id,
                    role = %tracking.role,
                    %error,
                    "工作区变更记录落盘失败（旁路数据，不阻断步骤）"
                );
                None
            }
        };
        if let Some(violation) = violation {
            // 越界：变更已留痕（record 含 violation 字段）但步骤失败——引擎按失败
            // 处理，不登记成功 Artifact；不生成可审批 ChangeSet（越界写入不可审查
            // 为合法工作区改动，恢复基线只会制造更大的不可控面）。
            return Err(violation);
        }
        // 九期（一路）：无实际变更 → 不创建空 pending ChangeSet（此前会生成
        // changed_files 为空的空壳待办，人工审批无法操作），只记审计留痕。
        if changed.is_empty() {
            if let Some(audit) = &tracking.audit {
                if let Ok(mut audit) = audit.lock() {
                    audit.record(
                        &tracking.team_id,
                        "change_set.no_change",
                        Some(format!("workswarm/{}", tracking.team_id)),
                        Some(true),
                        format!(
                            "步骤 {} 无实际工作区变更（检测：{detection}），跳过 ChangeSet 生成",
                            step
                        ),
                    );
                }
            }
            return result;
        }
        // 十期·四路 R1：无论步骤成败都生成 ChangeSet（pending_review，等人工
        // accept/reject/revert）——「写入后失败」「写入中取消」留下的残留修改
        // 同样可审查、可恢复；未接受时该团队代码 Artifact 不得成为最终 approved
        // head。记录/落盘失败不回滚步骤（变更本身已在工作区/内容快照中），留
        // error 审计与日志。
        let change_set = owo_agent_core::change_set::build_change_set(
            &tracking.team_id,
            &step,
            &tracking.role,
            &base,
            &changed,
            &tracking.root,
            record.as_ref().and_then(|record| record.diff_ref.clone()),
        );
        let store = owo_agent_core::change_set_store::ChangeSetStore::new(&tracking.run_dir);
        match store.save_upsert(&change_set) {
            Ok(()) => {
                if let Some(audit) = &tracking.audit {
                    if let Ok(mut audit) = audit.lock() {
                        audit.record(
                            &tracking.team_id,
                            "change_set.created",
                            Some(format!("workswarm/{}", tracking.team_id)),
                            Some(true),
                            format!(
                                "{} 生成（步骤 {}，检测：{detection}，文件：{}）",
                                change_set.change_set_id,
                                step,
                                changed.join(", ")
                            ),
                        );
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    team_id = %tracking.team_id,
                    role = %tracking.role,
                    error = %error,
                    "ChangeSet 落盘失败（变更已在工作区追踪中，但审批闭环缺失）"
                );
            }
        }
        // 变更收尾完成（租约在此作用域末尾释放）。返回执行原结果：成功回成功，
        // 失败/超时/取消回原错误——步骤状态由引擎依结果判定，变更记录独立留存。
        result
    }
}
