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
use crate::subagent::{TurnEventSink, MAX_SUBAGENT_DEPTH};
use crate::tools::ToolRegistry;
use crate::workswarm_output::{
    contract_repair_prompt, contract_system_prompt, parse_worker_output, strip_code_fences,
    WorkerOutputParse, WorkerOutputV1,
};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 子代理回合预算（按任务复杂度自适应，而非固定 12 轮）。
///
/// 依据：委派提示词的长度/结构是"任务复杂度"的可用代理指标——一句话委派
/// （"看看 X"）给基础轮数即可；带验收标准、多步骤、多文件的委派需要更多轮。
/// 读角色（探索）天然比写角色（实现+验证）轮数少。
///
/// 计算：基础值（只读 10 / 通用 16）+ 每 200 字符 +1，上限（只读 24 / 通用 40），
/// 最终再与调用方配置的 `max_turns` 取小（配置是硬上限）。
pub fn adaptive_subagent_turns(prompt: &str, read_only: bool) -> usize {
    let base: usize = if read_only { 10 } else { 16 };
    let cap: usize = if read_only { 24 } else { 40 };
    let extra = prompt.chars().count() / 200;
    base.saturating_add(extra).min(cap)
}

/// 子代理自动复核开关（默认开；`OWO_SUBAGENT_REVIEW=0|false` 关闭）。
///
/// 关掉的原因通常是成本/延迟（复核 = 一次额外子代理回合）；质量优先时保持默认开。
pub fn subagent_review_enabled() -> bool {
    std::env::var("OWO_SUBAGENT_REVIEW")
        .map(|value| !(value == "0" || value.eq_ignore_ascii_case("false")))
        .unwrap_or(true)
}

/// 父侧自动复核提示词：独立 critic 逐条核对验收标准与证据（只读）。
///
/// 这是"子代理草草了事"的质量门：委派返回后由**另一个**子代理独立核对，
/// 而不是让交付者自己宣布完成。
pub fn review_prompt(task: &str, output: &str) -> String {
    format!(
        "你是独立复核子代理（critic）：只读核对上游交付是否**真正**满足委派任务的验收标准。\n\
         【委派任务与验收标准】\n{task}\n\n\
         【上游交付（契约 JSON）】\n{output}\n\n\
         复核要求：\n\
         1. 逐条核对任务里的验收标准（缺标准时按「任务字面要求是否被逐字满足」判定）；\n\
         2. 核对证据是否支撑结论（无证据的完成声明一律视为未完成）；\n\
         3. 不通过时给出**可执行**的返工清单（缺什么、改哪里、怎么验证）。\n\
         输出按你的输出契约：summary 里放 {{\"approved\":true|false,\"score\":0-100,\"comments\":[\"…\"],\"rework\":[\"…\"]}}。"
    )
}

/// 返工提示词：把复核意见变成下一轮交付的硬要求（一次返工，不无限重试）。
pub fn rework_prompt(task: &str, previous: &str, review: &str) -> String {
    format!(
        "你上一轮的交付**未通过独立复核**，现在按复核意见返工一次（只有这一次机会）。\n\
         【原始任务与验收标准】\n{task}\n\n\
         【上一轮交付】\n{previous}\n\n\
         【复核意见（必须逐条落实）】\n{review}\n\n\
         返工要求：只做复核指出的缺口；完成后按输出契约提交（artifact.content 放最终交付正文，\
         evidence 里写清每条验收标准的证据）。"
    )
}

/// 复核结论解析：从 critic 契约输出里取 `"approved"` 布尔。
///
/// 兼容模型把结论 JSON 放进 `summary` 字符串的写法（契约规定如此），也兼容
/// 裸 JSON 片段；解析不到返回 `None`（调用方按"无法判定"处理，不误判为通过）。
pub fn critic_approved(review_text: &str) -> Option<bool> {
    let text = strip_code_fences(review_text);
    let haystack = match parse_worker_output(&text) {
        WorkerOutputParse::Parsed(output) => output.summary,
        _ => text.clone(),
    };
    let mut search_from = 0usize;
    while let Some(index) = haystack[search_from..].find("\"approved\"") {
        let after = &haystack[search_from + index + "\"approved\"".len()..];
        let after = after.trim_start();
        let after = after.strip_prefix(':').unwrap_or(after).trim_start();
        if after.starts_with("true") {
            return Some(true);
        }
        if after.starts_with("false") {
            return Some(false);
        }
        search_from += index + "\"approved\"".len();
    }
    None
}

#[cfg(test)]
mod review_tests {
    use super::{critic_approved, review_prompt, rework_prompt};

    #[test]
    fn critic_verdict_parsed_from_summary_and_bare_json() {
        // 契约写法：结论 JSON 放在 summary 字符串里。
        let wrapped = r#"{"status":"done","summary":"{\"approved\":false,\"score\":40,\"comments\":[\"缺证据\"]}","evidence":[],"open_issues":[]}"#;
        assert_eq!(critic_approved(wrapped), Some(false));
        let ok = r#"{"status":"done","summary":"{\"approved\":true,\"score\":92}","evidence":[],"open_issues":[]}"#;
        assert_eq!(critic_approved(ok), Some(true));
        // 裸 JSON / 围栏包裹也兼容。
        assert_eq!(
            critic_approved("```json\n{\"approved\": true}\n```"),
            Some(true)
        );
        // 解析不到 → None（调用方不得当成通过）。
        assert_eq!(critic_approved("看起来没问题"), None);
        assert_eq!(
            critic_approved(r#"{"status":"done","summary":"完成"}"#),
            None
        );
    }

    #[test]
    fn review_and_rework_prompts_carry_task_and_findings() {
        let review = review_prompt("写 hello.txt（验收：内容为 hi）", "{\"status\":\"done\"}");
        assert!(review.contains("写 hello.txt（验收：内容为 hi）"));
        assert!(review.contains("approved"));
        let rework = rework_prompt("写 hello.txt", "上一轮交付", "缺证据：没有 read_file 回读");
        assert!(rework.contains("未通过独立复核"));
        assert!(rework.contains("缺证据：没有 read_file 回读"));
        assert!(rework.contains("只有这一次机会"));
    }
}
#[cfg(test)]
mod budget_tests {
    use super::adaptive_subagent_turns;

    #[test]
    fn adaptive_budget_scales_with_prompt_and_role() {
        // 一句话委派：基础轮数（只读 10 / 通用 16）。
        assert_eq!(adaptive_subagent_turns("看看 X", true), 10);
        assert_eq!(adaptive_subagent_turns("看看 X", false), 16);
        // 长委派（带验收标准/多步骤）：每 200 字符 +1。
        let long = "需求：".to_string() + &"x".repeat(600);
        assert_eq!(adaptive_subagent_turns(&long, false), 19);
        // 上限：只读 24 / 通用 40（防"委派一段超长文本换来无限轮"）。
        let huge = "x".repeat(20_000);
        assert_eq!(adaptive_subagent_turns(&huge, true), 24);
        assert_eq!(adaptive_subagent_turns(&huge, false), 40);
        // 调用方配置是硬上限：min 在调用点做（此处只验函数本体不超过上限）。
        assert!(adaptive_subagent_turns(&huge, false) <= 40);
    }
}
/// 契约执行结果（两路共享）。
#[derive(Debug, Clone)]
pub struct ContractEnforcementResult {
    /// 契约合规本体（`WorkerOutputV1` JSON；直接返回给引擎/调用方）。
    pub text: String,
    /// 已消耗的定向修复次数（0 = 首轮即合规；1 = 修复一次后接受）。
    /// 调用方（如 EvalAgentWorker stats）据此计 model_calls/output_repairs 计数。
    pub repairs: u32,
    /// Usage attributable to the optional contract-repair request.
    pub usage: Option<crate::gateway::TokenUsage>,
}

/// 契约执行失败（唯一的一次定向修复已消耗）。
#[derive(Debug, Clone)]
pub struct ContractEnforcementError {
    /// 失败原因；前缀冻结为 `output_contract_invalid:`。
    pub message: String,
    /// 已消耗的定向修复次数（失败路径恒为 1）。
    pub repairs: u32,
    /// Usage attributable to the repair attempt, including invalid repaired output.
    pub usage: Option<crate::gateway::TokenUsage>,
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
    enforce_worker_output_contract_with_model(provider, None, text, is_critic).await
}

/// Same output-contract path with an explicit request model, used by task-scoped workers
/// so a repair request stays on the same model route as the work it repairs.
pub async fn enforce_worker_output_contract_with_model(
    provider: &Arc<dyn ModelProvider>,
    model: Option<&str>,
    text: &str,
    is_critic: bool,
) -> Result<ContractEnforcementResult, ContractEnforcementError> {
    let first_violation = match parse_worker_output(text) {
        WorkerOutputParse::Parsed(output) => match validate_by_role(&output, is_critic) {
            Ok(()) => {
                return Ok(ContractEnforcementResult {
                    text: text.to_string(),
                    repairs: 0,
                    usage: None,
                })
            }
            Err(violation) => violation,
        },
        WorkerOutputParse::Invalid { error } => error,
        WorkerOutputParse::Legacy => {
            "输出不是 WorkerOutputV1 契约 JSON（自由文本不能登记为交付物）".to_string()
        }
    };

    let messages = [ChatMessage {
        role: "user".to_string(),
        content: Some(contract_repair_prompt(is_critic, &first_violation, text)),
        tool_calls: None,
        tool_call_id: None,
        images: Vec::new(),
    }];
    let (repaired, repair_usage) = match provider
        .complete_with_model_observed(model, &messages, &[])
        .await
    {
        Ok(observed) => {
            let text = match observed.output {
                ModelOutput::Text(text) => strip_code_fences(&text),
                _ => String::new(),
            };
            (text, observed.metadata.usage)
        }
        Err(_) => (String::new(), None),
    };
    let violation = match parse_worker_output(&repaired) {
        WorkerOutputParse::Parsed(output) => match validate_by_role(&output, is_critic) {
            Ok(()) => {
                return Ok(ContractEnforcementResult {
                    text: repaired,
                    repairs: 1,
                    usage: repair_usage,
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
        usage: repair_usage,
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
    /// 嵌套回合事件出口（`None` = 不转发，后台路径）。
    pub events: Option<TurnEventSink<'a>>,
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
            max_turns: if self.max_turns == 0 {
                adaptive_subagent_turns(prompt, read_only)
                    .min(crate::subagent::MAX_SUBAGENT_TURNS)
            } else {
                adaptive_subagent_turns(prompt, read_only)
                    .min(self.max_turns)
                    .min(crate::subagent::MAX_SUBAGENT_TURNS)
            },
            max_tool_calls_per_turn: crate::agent::DEFAULT_BOUNDED_TOOL_CALL_CAP,
            subagent_depth: self.depth + 1,
            ..Default::default()
        };
        let agent = Agent::new(Arc::clone(&self.provider), registry, policy, config);
        let base_prompt = if read_only {
            "你是只读探索子代理：只能读取/搜索工作区文件，禁止写入或执行命令；调查完成后用简洁中文汇报发现。\n"
        } else {
            "你是通用子代理：独立完成委派任务，工具调用仍需审批，完成后汇报结果。\n\
             **验收纪律**：先明确任务里的验收标准；每一项结论都要有证据（读到的文件路径、命令输出、\n\
             测试结果），写进 evidence 字段；无证据的完成声明会被独立复核判为未完成。\n"
        };
        // 输出契约（V1）：system prompt 追加契约条款，让模型首轮即可按
        // WorkerOutputV1 JSON 输出；不合规时共享执行器最多定向修复一次。
        let system_prompt = format!("{base_prompt}{}", contract_system_prompt(read_only));
        // M4.2：调用方给出的模型显式进入请求体（非空且非 `"default"` 哨兵即固定）；
        // 空串/哨兵表示自动——回退 Provider 解析链（OPENAI_MODEL 热切换 → 启动配置 → 内置默认）。
        let mut session = Session::new(workspace, self.model.clone(), Some(system_prompt))
            .with_model_override(Some(self.model.clone()));
        // 嵌套事件转发：工具进度 + 审批请求即时到达父回合的客户端。
        // 过滤：子代理的 TokenDelta/Final/Compaction 不外发——否则子代理的流式
        // 文本会混进父代理的回答（工具事件用 `sub:` 前缀标明来源，id 保持配对）。
        let sink = self.events.clone();
        let mut on_event = |event: &TurnEvent| {
            let Some(sink) = sink.as_ref() else {
                return;
            };
            match event {
                TurnEvent::PermissionRequest(_) | TurnEvent::ModelCall => sink(event),
                TurnEvent::ToolStart {
                    id,
                    tool,
                    args_preview,
                } => sink(&TurnEvent::ToolStart {
                    id: id.clone(),
                    tool: format!("sub:{tool}"),
                    args_preview: args_preview.clone(),
                }),
                TurnEvent::ToolResult {
                    id,
                    tool,
                    ok,
                    error,
                    preview,
                } => sink(&TurnEvent::ToolResult {
                    id: id.clone(),
                    tool: format!("sub:{tool}"),
                    ok: *ok,
                    error: error.clone(),
                    preview: preview.clone(),
                }),
                _ => {}
            }
        };
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
        if outcome.reached_model_turn_limit {
            return Err("worker_turn_budget_exhausted:模型在任务预算内未自行给出最终答复".to_string());
        }
        // 连兜底文本（无最终文本）也走契约执行——自由文本路径不豁免
        // （至多修复一次，否则 output_contract_invalid）。
        let text = outcome
            .final_text
            .unwrap_or_else(|| format!("（子代理无最终文本，共 {} 步）", outcome.steps));
        match enforce_worker_output_contract_with_model(
            &self.provider,
            Some(&self.model),
            &text,
            read_only,
        )
        .await
        {
            Ok(result) => Ok(result.text),
            Err(error) => Err(error.message),
        }
    }
}

#[cfg(test)]
mod enforcement_usage_tests {
    use super::enforce_worker_output_contract_with_model;
    use crate::gateway::{
        ChatMessage, ModelCallMetadata, ModelOutput, ModelProvider, ObservedModelOutput, TokenUsage,
    };
    use crate::tools::ToolSpec;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct RepairUsageProvider {
        requested_model: Mutex<Option<String>>,
    }

    #[async_trait]
    impl ModelProvider for RepairUsageProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Ok(ModelOutput::Text(String::new()))
        }

        async fn complete_with_model_observed(
            &self,
            model: Option<&str>,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ObservedModelOutput, String> {
            *self.requested_model.lock().unwrap() = model.map(str::to_string);
            Ok(ObservedModelOutput {
                output: ModelOutput::Text(
                    r#"{"status":"done","summary":"fixed","artifact":{"kind":"note","format":"text","content":"accepted"},"evidence":[],"open_issues":[]}"#.to_string(),
                ),
                metadata: ModelCallMetadata {
                    usage: Some(TokenUsage {
                        prompt_tokens: 11,
                        completion_tokens: 5,
                        total_tokens: 16,
                    }),
                    ..Default::default()
                },
            })
        }
    }

    #[tokio::test]
    async fn repair_usage_and_model_are_retained() {
        let provider = Arc::new(RepairUsageProvider {
            requested_model: Mutex::new(None),
        });
        let provider_dyn: Arc<dyn ModelProvider> = provider.clone();
        let result = enforce_worker_output_contract_with_model(
            &provider_dyn,
            Some("worker-model"),
            "not a contract",
            false,
        )
        .await
        .unwrap();

        assert_eq!(result.repairs, 1);
        assert_eq!(result.usage.unwrap().total_tokens, 16);
        assert_eq!(
            provider.requested_model.lock().unwrap().as_deref(),
            Some("worker-model")
        );
    }
}
