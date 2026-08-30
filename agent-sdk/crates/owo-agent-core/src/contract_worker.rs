//! 生产 Worker 输出契约执行器（七期一路）：真实 Agent Worker 两路的共享契约层。
//!
//! 统一生产 `SubagentRunner`（`agent.rs`/`tools.rs` 的 subagent 工具、`workswarm_api.rs`
//! 的 Agent 执行后端）与 ProductEval `EvalAgentWorker`（`product_eval/workswarm_executor.rs`）
//! 的输出契约执行：**最终结果必须是合法的 `WorkerOutputV1` 契约 JSON，自由文本交付路径彻底关闭**。
//!
//! 七期契约冻结口径：
//! 1. 先经 [`parse_worker_output`] 解析，再按角色规则校验（`read_only` 参数是两路的
//!    critic 角色代理：`true` == critic 角色，禁带 artifact；`false` == producer 角色，
//!    done 必须携带 artifact）；
//! 2. 不合规只做**至多一次**定向修复（一次直接 provider 调用），修复后 producer/critic
//!    **双复验**才接受（旧 EvalAgentWorker 只复验 critic，共享执行器更严格）；
//! 3. 仍不合规 → `Err`，错误前缀冻结为 `output_contract_invalid:`（小写下划线失败码，
//!    与 `failure_code` 字段同一口径，UI 对 `error` 字段前缀双容错），禁止无限重试。
//!
//! 成功时返回**契约合规的 JSON 本体**（引擎重解析后按 Artifact/Handoff 版本化登记，
//! Producer 正文只取自 `artifact.content`）；连「无最终文本」的兜底文本也走契约执行，
//! 不豁免。

use crate::agent::{Agent, AgentConfig, TurnEvent};
use crate::gateway::{ChatMessage, ModelOutput, ModelProvider};
use crate::permissions::{Approver, Policy};
use crate::session::Session;
use crate::subagent::MAX_SUBAGENT_DEPTH;
use crate::tools::ToolRegistry;
use crate::workswarm_output::{
    contract_repair_prompt, contract_system_prompt, parse_worker_output, strip_code_fences,
    WorkerOutputParse, WorkerOutputV1,
};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 契约执行结果（两路共享）。
#[derive(Debug, Clone)]
pub struct ContractEnforcementResult {
    /// 契约合规本体（`WorkerOutputV1` JSON；直接返回给引擎/调用方）。
    pub text: String,
    /// 已消耗的定向修复次数（0 = 首轮即合规；1 = 修复一次后接受）。
    /// 调用方（如 EvalAgentWorker stats）据此计 model_calls/output_repairs 计数。
    pub repairs: u32,
}

/// 契约执行失败（唯一的一次定向修复已消耗）。
#[derive(Debug, Clone)]
pub struct ContractEnforcementError {
    /// 失败原因；前缀冻结为 `output_contract_invalid:`。
    pub message: String,
    /// 已消耗的定向修复次数（失败路径恒为 1）。
    pub repairs: u32,
}

/// 角色规则校验（producer / critic）。
fn validate_by_role(output: &WorkerOutputV1, is_critic: bool) -> Result<(), String> {
    if is_critic {
        output.validate_critic()
    } else {
        output.validate()
    }
}

/// 输出契约执行器——生产 Agent 与 ProductEval Worker 的**唯一**共享入口。
///
/// - `is_critic`：角色代理（两路 `read_only` 语义对齐：true == critic 角色）；
/// - 首轮不合规时把**具体违例原因**带进修复提示词（定向修复，不是重发完整指令）；
/// - 修复后 producer/critic 双复验，合法才接受；
/// - `repairs` 计数在 Ok/Err 两侧都返回（调用方按它计 stats，失败也计——与六期基线一致）。
pub async fn enforce_worker_output_contract(
    provider: &Arc<dyn ModelProvider>,
    text: &str,
    is_critic: bool,
) -> Result<ContractEnforcementResult, ContractEnforcementError> {
    let first_violation = match parse_worker_output(text) {
        WorkerOutputParse::Parsed(output) => match validate_by_role(&output, is_critic) {
            Ok(()) => {
                return Ok(ContractEnforcementResult {
                    text: text.to_string(),
                    repairs: 0,
                })
            }
            Err(violation) => violation,
        },
        WorkerOutputParse::Invalid { error } => error,
        WorkerOutputParse::Legacy => {
            "输出不是 WorkerOutputV1 契约 JSON（自由文本不能登记为交付物）".to_string()
        }
    };

    // 至多一次定向修复（一次直接 provider 调用；不计入 Agent turn，由调用方按
    // 返回的 repairs 计 stats）。
    let messages = [ChatMessage {
        role: "user".to_string(),
        content: Some(contract_repair_prompt(is_critic, &first_violation, text)),
        tool_calls: None,
        tool_call_id: None,
    }];
    let repaired = match provider.complete(&messages, &[]).await {
        Ok(ModelOutput::Text(text)) => strip_code_fences(&text),
        _ => String::new(),
    };
    let violation = match parse_worker_output(&repaired) {
        WorkerOutputParse::Parsed(output) => match validate_by_role(&output, is_critic) {
            Ok(()) => {
                return Ok(ContractEnforcementResult {
                    text: repaired,
                    repairs: 1,
                })
            }
            Err(violation) => violation,
        },
        WorkerOutputParse::Invalid { error } => error,
        WorkerOutputParse::Legacy => {
            "定向修复后仍是自由文本（自由文本不能登记为交付物）".to_string()
        }
    };
    Err(ContractEnforcementError {
        message: format!("output_contract_invalid: 定向修复一次后仍不符合契约：{violation}"),
        repairs: 1,
    })
}

/// 契约化子代理执行器：承载完整 subagent 回合循环 + 输出契约执行。
///
/// `SubagentRunner::run` 委托到这里（七期一路）。字段集与 `SubagentRunner` 完全一致
/// （不新增/不改名任何字段——`agent.rs`/`tools.rs`/`workswarm_api.rs` 里的结构体
/// 字面量不受影响），区别只在返回的是**契约校验后的 JSON 本体**而非自由文本。
pub struct ContractSubagentRunner<'a> {
    pub provider: Arc<dyn ModelProvider>,
    pub approver: &'a dyn Approver,
    pub abort: &'a AtomicBool,
    pub depth: usize,
    pub max_turns: usize,
    pub model: String,
}

impl ContractSubagentRunner<'_> {
    /// 运行一个只读或通用子代理会话，返回契约校验后的 JSON 本体。
    ///
    /// `read_only` 是角色代理：true = critic 角色（只读探索，不交付，禁带 artifact）；
    /// false = producer 角色（通用子代理，done 必须携带 artifact）。
    pub async fn run(
        &self,
        workspace: &Path,
        prompt: &str,
        read_only: bool,
    ) -> Result<String, String> {
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Err(format!("子代理深度超限（最多 {MAX_SUBAGENT_DEPTH} 层）"));
        }
        let policy = if read_only {
            Policy::read_only(workspace.to_path_buf())
        } else {
            Policy::new(workspace.to_path_buf())
        };
        let registry = if read_only {
            ToolRegistry::read_only()
        } else {
            ToolRegistry::new()
        };
        let config = AgentConfig {
            max_turns: self.max_turns.min(12),
            subagent_depth: self.depth + 1,
            ..Default::default()
        };
        let agent = Agent::new(Arc::clone(&self.provider), registry, policy, config);
        let base_prompt = if read_only {
            "你是只读探索子代理：只能读取/搜索工作区文件，禁止写入或执行命令；调查完成后用简洁中文汇报发现。\n"
        } else {
            "你是通用子代理：独立完成委派任务，工具调用仍需审批，完成后汇报结果。\n"
        };
        // 输出契约（V1）：system prompt 追加契约条款，让模型首轮即可按
        // WorkerOutputV1 JSON 输出；不合规时共享执行器最多定向修复一次。
        let system_prompt = format!("{base_prompt}{}", contract_system_prompt(read_only));
        let mut session = Session::new(workspace, self.model.clone(), Some(system_prompt));
        let mut on_event = |_event: &TurnEvent| {};
        let outcome = agent
            .run_turn(
                &mut session,
                prompt,
                self.approver,
                self.abort,
                &mut on_event,
            )
            .await
            .map_err(|error| format!("子代理执行失败：{error}"))?;
        // 连兜底文本（无最终文本）也走契约执行——自由文本路径不豁免
        // （至多修复一次，否则 output_contract_invalid）。
        let text = outcome
            .final_text
            .unwrap_or_else(|| format!("（子代理无最终文本，共 {} 步）", outcome.steps));
        match enforce_worker_output_contract(&self.provider, &text, read_only).await {
            Ok(result) => Ok(result.text),
            Err(error) => Err(error.message),
        }
    }
}
