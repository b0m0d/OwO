//! 子代理：主 Agent 派生的嵌套会话（explore 只读 / subagent 通用）。
//!
//! 七期一路（契约化）：`SubagentRunner::run` 委托给
//! [`crate::contract_worker::ContractSubagentRunner`]——子代理输出必须通过 `WorkerOutputV1`
//! 契约执行（定向修复至多一次，失败 `output_contract_invalid`），返回值为契约合规的
//! JSON 本体而非自由文本。`read_only` 参数是角色代理（`read_only` == critic 角色）。
//!
//! 注意：`SubagentRunner` 不新增/不改名任何字段——`agent.rs`/`tools.rs`/
//! `workswarm_api.rs` 里的结构体字面量不受影响。

use crate::agent::{Agent, AgentConfig};
use crate::gateway::ModelProvider;
use crate::permissions::{Approver, AutoApprover, Policy};
use crate::session::Session;
use crate::tools::ToolRegistry;
use crate::TurnEvent;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub const MAX_SUBAGENT_DEPTH: usize = 2;

/// 只读子代理系统提示（单任务 explore 与 fan-out 共用）。
pub(crate) const READ_ONLY_SUBAGENT_PROMPT: &str =
    "你是只读探索子代理：只能读取/搜索工作区文件，禁止写入或执行命令；\
     调查完成后用简洁中文汇报发现。";

/// fan-out 工具的注入通道（A5-1 取优合并自远端 engine）：与 [`SubagentRunner`]
/// 不同，它是 **owned 数据** 且要求 `'static`（[`crate::fleet::fan_out_cfg`] 的
/// 闭包约束），因此单独成型。
#[derive(Clone)]
pub struct FanOutRunner {
    pub provider: Arc<dyn ModelProvider>,
    pub workspace: PathBuf,
    pub model: String,
    pub depth: usize,
    pub max_turns: usize,
}

/// A5-1 并行 fan-out：同时派出多个**只读**子代理，返回部分成功报告。
///
/// - 并发上限 / 单任务超时 / 整体时长预算 / 取消传播由
///   [`crate::fleet::FanOutConfig`] 控制（`fan_out_cfg` 已具备完整仲裁语义）；
/// - **只读限定**：多子代理并行写同一工作区存在冲突风险，写类任务请串行走
///   `subagent`；只读策略同时天然禁掉网络/执行类工具（Policy::read_only 拒非 Read）；
/// - 每个子代理独立会话、独立失败（单失败不影响其余，结果按输入顺序返回）。
pub async fn fan_out_subagents(
    provider: Arc<dyn ModelProvider>,
    workspace: PathBuf,
    model: String,
    depth: usize,
    max_turns: usize,
    prompts: Vec<String>,
    mut out_config: crate::fleet::FanOutConfig,
) -> Result<crate::fleet::FanOutReport, String> {
    if depth >= MAX_SUBAGENT_DEPTH {
        return Err(format!("子代理深度超限（最多 {MAX_SUBAGENT_DEPTH} 层）"));
    }
    if prompts.is_empty() {
        return Err("fan-out 任务列表为空".to_string());
    }
    // 取消标志：调用方未提供时内建一个——主回合取消经工具层桥接到该标志，
    // fan_out_cfg 停止调度新子任务并 abort 在飞者（已成功结果保留）。
    let cancelled = out_config
        .cancelled
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    out_config.cancelled = Some(Arc::clone(&cancelled));
    let workers: Vec<String> = (0..prompts.len())
        .map(|index| format!("subagent-{index}"))
        .collect();
    let prompts = Arc::new(prompts);
    let report =
        crate::fleet::fan_out_cfg(&workers, out_config, "subagent-fanout", move |worker| {
            let provider = Arc::clone(&provider);
            let workspace = workspace.clone();
            let model = model.clone();
            let prompts = Arc::clone(&prompts);
            let cancelled = Arc::clone(&cancelled);
            async move {
                let index: usize = worker
                    .strip_prefix("subagent-")
                    .and_then(|value| value.parse().ok())
                    .ok_or_else(|| format!("worker 名解析失败：{worker}"))?;
                let prompt = prompts
                    .get(index)
                    .cloned()
                    .ok_or_else(|| format!("任务下标越界：{index}"))?;
                let policy = Policy::read_only(workspace.clone());
                let registry = ToolRegistry::read_only();
                let config = AgentConfig {
                    max_turns: max_turns.min(12),
                    subagent_depth: depth + 1,
                    ..Default::default()
                };
                let agent = Agent::new(provider, registry, policy, config);
                let mut session = Session::new(
                    &workspace,
                    model,
                    Some(READ_ONLY_SUBAGENT_PROMPT.to_string()),
                );
                let approver = AutoApprover { allow: true };
                let mut on_event = |_event: &TurnEvent| {};
                let outcome = agent
                    .run_turn(
                        &mut session,
                        &prompt,
                        &approver,
                        cancelled.as_ref(),
                        &mut on_event,
                    )
                    .await
                    .map_err(|error| format!("子代理失败：{error}"))?;
                Ok(outcome
                    .final_text
                    .unwrap_or_else(|| format!("（无最终文本，共 {} 步）", outcome.steps)))
            }
        })
        .await;
    Ok(report)
}

/// 嵌套回合事件出口：把子代理回合的事件**即时**转交给父回合的 `on_event`。
///
/// 为什么必须即时（不能缓冲）：子代理的审批请求（`TurnEvent::PermissionRequest`）
/// 要经由父回合的流到达客户端；若缓冲到子代理结束才回放，子代理正阻塞在
/// `approver.decide()` 上等一个永远不会到达的决定 → 任务卡死到审批超时。
pub type TurnEventSink<'a> = Arc<dyn Fn(&TurnEvent) + Send + Sync + 'a>;

/// 子代理执行器：启动一个只读或完整子会话并运行一轮 `Agent::run_turn`。
///
/// `read_only` 参数是角色代理（`read_only` == critic 角色）：
/// - true  → 只读工具 + 只读策略（探索/调查），critic 角色，不交付 artifact；
/// - false → 完整工具 + 完整策略（实现/修改），producer 角色，done 必须携带 artifact。
///
/// 运行后输出走共享契约执行器（定向修复至多一次，失败 `output_contract_invalid:{reason}`）；
/// 成功时返回契约合规的 JSON 本体。
pub struct SubagentRunner<'a> {
    pub provider: Arc<dyn ModelProvider>,
    pub approver: &'a dyn Approver,
    pub abort: &'a AtomicBool,
    pub depth: usize,
    pub max_turns: usize,
    pub model: String,
    /// 嵌套回合事件出口（`None` = 不可见，用于无交互通道的后台路径）。
    pub events: Option<TurnEventSink<'a>>,
}

impl SubagentRunner<'_> {
    /// 在只读或完整模式下运行一个子会话，返回契约校验后的 JSON 本体。
    ///
    /// 委托 [`crate::contract_worker::ContractSubagentRunner`]（七期一路共享契约执行器）：
    /// 输出必须是合法 `WorkerOutputV1`（producer 携带 artifact，critic 不携带），
    /// 定向修复至多一次，禁止无限重试；兜底文本（无最终文本）同样不豁免。
    pub async fn run(
        &self,
        workspace: &Path,
        prompt: &str,
        read_only: bool,
    ) -> Result<String, String> {
        let contract = crate::contract_worker::ContractSubagentRunner {
            provider: Arc::clone(&self.provider),
            approver: self.approver,
            abort: self.abort,
            depth: self.depth,
            max_turns: self.max_turns,
            model: self.model.clone(),
            events: self.events.clone(),
        };
        contract.run(workspace, prompt, read_only).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{ChatMessage, ModelOutput, ModelProvider};
    use crate::permissions::AutoApprover;
    use crate::tools::ToolSpec;
    use async_trait::async_trait;

    struct FixedProvider;

    #[async_trait]
    impl ModelProvider for FixedProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Ok(ModelOutput::Text("ok".to_string()))
        }
    }

    #[tokio::test]
    async fn depth_limit_blocks_nested_run() {
        let workspace = std::env::temp_dir();
        let runner = SubagentRunner {
            provider: Arc::new(FixedProvider),
            approver: &AutoApprover { allow: true },
            abort: &AtomicBool::new(false),
            depth: MAX_SUBAGENT_DEPTH,
            max_turns: 5,
            model: "mock".to_string(),
            events: None,
        };
        let result = runner.run(&workspace, "x", true).await;
        assert!(result.unwrap_err().contains("深度超限"));
    }

    /// 可编排 mock（取优合并自远端 engine）：按 prompt 内容决定 sleep 时长与是否失败；
    /// 记录峰值并发。
    struct ScriptedFanoutProvider {
        delay_ms: u64,
        peak: Arc<std::sync::atomic::AtomicUsize>,
        inflight: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for ScriptedFanoutProvider {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            use std::sync::atomic::Ordering;
            let current = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);
            // 只匹配最后一条消息（当前用户 prompt）——system prompt 文本不可作为
            // 编排信号（否则「失败」等词会误伤全部子代理）。
            let prompt = messages
                .last()
                .and_then(|message| message.content.clone())
                .unwrap_or_default();
            if prompt.contains("慢") {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            self.inflight.fetch_sub(1, Ordering::SeqCst);
            if prompt.contains("失败") {
                return Err("scripted failure".to_string());
            }
            Ok(ModelOutput::Text(format!("结论：{prompt}")))
        }
    }

    fn fanout_provider(
        delay_ms: u64,
    ) -> (Arc<dyn ModelProvider>, Arc<std::sync::atomic::AtomicUsize>) {
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = Arc::new(ScriptedFanoutProvider {
            delay_ms,
            peak: Arc::clone(&peak),
            inflight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        (provider, peak)
    }

    fn fanout_config(max_parallel: usize) -> crate::fleet::FanOutConfig {
        crate::fleet::FanOutConfig {
            max_parallel,
            budget: crate::fleet::Budget {
                max_duration_secs: 60,
                ..Default::default()
            },
            per_worker_timeout: Some(std::time::Duration::from_secs(10)),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn fan_out_runs_subagents_in_parallel_and_isolates_failure() {
        let (provider, peak) = fanout_provider(300);
        let prompts = vec![
            "慢任务一".to_string(),
            "慢任务二".to_string(),
            "失败任务".to_string(),
        ];
        let start = std::time::Instant::now();
        let report = fan_out_subagents(
            provider,
            std::env::temp_dir(),
            "mock".to_string(),
            0,
            4,
            prompts,
            fanout_config(3),
        )
        .await
        .unwrap();
        let elapsed = start.elapsed();
        // 失败隔离：2 成功 1 失败，顺序与输入一致。
        assert_eq!(report.succeeded().len(), 2, "{report:?}");
        assert_eq!(report.failed().len(), 1, "{report:?}");
        assert_eq!(report.outcomes[0].worker, "subagent-0");
        assert_eq!(
            report.outcomes[2].status,
            crate::fleet::FanOutStatus::Failed
        );
        // 并行：3 个 300ms 任务串行需 ~900ms，并发（上限 3）应明显更快。
        assert!(
            elapsed < std::time::Duration::from_millis(750),
            "应并行执行，实际耗时 {elapsed:?}"
        );
        assert!(
            peak.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "并发度未体现"
        );
    }

    #[tokio::test]
    async fn fan_out_respects_max_parallel() {
        let (provider, peak) = fanout_provider(200);
        let prompts = vec!["慢A".to_string(), "慢B".to_string(), "慢C".to_string()];
        let report = fan_out_subagents(
            provider,
            std::env::temp_dir(),
            "mock".to_string(),
            0,
            4,
            prompts,
            fanout_config(1),
        )
        .await
        .unwrap();
        assert_eq!(report.succeeded().len(), 3);
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "max_parallel=1 应串行"
        );
    }

    #[tokio::test]
    async fn fan_out_rejects_depth_overflow_and_empty_tasks() {
        let (provider, _) = fanout_provider(10);
        let error = fan_out_subagents(
            Arc::clone(&provider),
            std::env::temp_dir(),
            "mock".to_string(),
            MAX_SUBAGENT_DEPTH,
            4,
            vec!["a".to_string(), "b".to_string()],
            fanout_config(2),
        )
        .await
        .unwrap_err();
        assert!(error.contains("深度超限"), "{error}");
        let error = fan_out_subagents(
            provider,
            std::env::temp_dir(),
            "mock".to_string(),
            0,
            4,
            Vec::new(),
            fanout_config(2),
        )
        .await
        .unwrap_err();
        assert!(error.contains("任务列表为空"), "{error}");
    }
}
