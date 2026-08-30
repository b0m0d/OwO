//! 子代理：主 Agent 派生的嵌套会话（explore 只读 / subagent 通用）。
//!
//! 七期一路（契约化）：`SubagentRunner::run` 委托给
//! [`crate::contract_worker::ContractSubagentRunner`]——子代理输出必须通过 `WorkerOutputV1`
//! 契约执行（定向修复至多一次，失败 `output_contract_invalid`），返回值为契约合规的
//! JSON 本体而非自由文本。`read_only` 参数是角色代理（`read_only` == critic 角色）。
//!
//! 注意：`SubagentRunner` 不新增/不改名任何字段——`agent.rs`/`tools.rs`/
//! `workswarm_api.rs` 里的结构体字面量不受影响。

use crate::gateway::ModelProvider;
use crate::permissions::Approver;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

pub const MAX_SUBAGENT_DEPTH: usize = 2;

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
        };
        let result = runner.run(&workspace, "x", true).await;
        assert!(result.unwrap_err().contains("深度超限"));
    }
}
