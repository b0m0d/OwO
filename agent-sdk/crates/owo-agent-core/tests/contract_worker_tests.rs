//! 七期一路定向测试：生产 Worker 输出契约执行器（`contract_worker`）。
//!
//! 覆盖：`enforce_worker_output_contract` 的 producer/critic 双角色规则、
//! 定向修复至多一次、修复后双复验、围栏剥离、失败前缀冻结（`output_contract_invalid:`），
//! 以及 `SubagentRunner` 端到端（生产路径）自由文本交付路径彻底关闭。

use async_trait::async_trait;
use owo_agent_core::contract_worker::{
    enforce_worker_output_contract, ContractEnforcementError, ContractSubagentRunner,
};
use owo_agent_core::gateway::{ChatMessage, ModelOutput, ModelProvider};
use owo_agent_core::permissions::AutoApprover;
use owo_agent_core::subagent::SubagentRunner;
use owo_agent_core::tools::ToolSpec;
use owo_agent_core::workswarm_output::{parse_worker_output, strip_code_fences, WorkerOutputParse};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// 合法的 producer 契约输出（done + 非空 artifact.content）。
const PRODUCER_OK: &str = r###"{"status":"done","summary":"交付完成","artifact":{"kind":"document","format":"markdown","content":"## 交付正文\n关键结论 A。"},"evidence":[],"open_issues":[]}"###;

/// 合法的 critic 契约输出（评审结论放 summary，禁止 artifact）。
const CRITIC_OK: &str = r#"{"status":"done","summary":"{\"approved\":true,\"score\":90}","evidence":[],"open_issues":[]}"#;

/// critic 越权：携带 artifact（首轮违例，应触发定向修复）。
const CRITIC_WITH_ARTIFACT: &str = r#"{"status":"done","summary":"评审完成","artifact":{"kind":"review","format":"json","content":"{\"score\":60}"},"evidence":[],"open_issues":[]}"#;

/// 自由文本（Legacy：不是契约 JSON，不能登记为交付物）。
const FREE_TEXT: &str = "任务完成：我已经修好了 bug 并通过了测试。";

/// 脚本化 provider：按序返回预设输出；耗尽后报错。
struct ScriptedProvider {
    outputs: Mutex<VecDeque<String>>,
    calls: AtomicU32,
}

impl ScriptedProvider {
    fn new(outputs: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.iter().map(|s| s.to_string()).collect()),
            calls: AtomicU32::new(0),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let next = self
            .outputs
            .lock()
            .expect("provider 锁中毒")
            .pop_front()
            .ok_or_else(|| "脚本输出耗尽".to_string())?;
        Ok(ModelOutput::Text(next))
    }
}

fn provider_arc(provider: &Arc<ScriptedProvider>) -> Arc<dyn ModelProvider> {
    // 显式绑定以触发 Arc<具体类型> → Arc<dyn ModelProvider> 的 unsizing 强制转换。
    let arc: Arc<ScriptedProvider> = Arc::clone(provider);
    let dyn_arc: Arc<dyn ModelProvider> = arc;
    dyn_arc
}

#[tokio::test]
async fn producer_valid_json_passes_without_repair() {
    let provider = ScriptedProvider::new(&[]);
    let enforced = enforce_worker_output_contract(&provider_arc(&provider), PRODUCER_OK, false)
        .await
        .expect("合法 producer 输出应直接通过");
    assert_eq!(enforced.repairs, 0, "首轮即合规不应消耗修复");
    assert_eq!(enforced.text, PRODUCER_OK, "合规输出应原样返回（不重生成）");
    assert_eq!(provider.calls(), 0, "合规路径不应发起 provider 调用");
}

#[tokio::test]
async fn producer_free_text_repaired_once() {
    // 首轮文本经参数进入执行器；队列里只有「定向修复」的 provider 应答。
    let provider = ScriptedProvider::new(&[PRODUCER_OK]);
    let enforced = enforce_worker_output_contract(&provider_arc(&provider), FREE_TEXT, false)
        .await
        .expect("自由文本应经一次定向修复转为契约 JSON");
    assert_eq!(enforced.repairs, 1, "恰好一次修复");
    assert_eq!(enforced.text, PRODUCER_OK, "修复结果应被接受");
    assert_eq!(provider.calls(), 1, "修复 = 恰好一次 provider 调用");
    match parse_worker_output(&enforced.text) {
        WorkerOutputParse::Parsed(output) => {
            output
                .validate()
                .expect("修复后 producer 输出应通过 validate");
        }
        other => panic!("修复结果不是 Parsed：{other:?}"),
    }
}

#[tokio::test]
async fn producer_broken_json_repaired_once() {
    let provider = ScriptedProvider::new(&[PRODUCER_OK]);
    let enforced =
        enforce_worker_output_contract(&provider_arc(&provider), r#"{"status":"done"#, false)
            .await
            .expect("坏 JSON 应经一次定向修复转为契约 JSON");
    assert_eq!(enforced.repairs, 1);
    assert_eq!(enforced.text, PRODUCER_OK);
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn critic_with_artifact_repaired_and_revalidated() {
    let provider = ScriptedProvider::new(&[CRITIC_OK]);
    let enforced =
        enforce_worker_output_contract(&provider_arc(&provider), CRITIC_WITH_ARTIFACT, true)
            .await
            .expect("critic 越权 artifact 应经一次定向修复");
    assert_eq!(enforced.repairs, 1);
    assert_eq!(enforced.text, CRITIC_OK);
    // 双复验：修复结果仍按 critic 角色规则校验（禁带 artifact + 结论非空）。
    match parse_worker_output(&enforced.text) {
        WorkerOutputParse::Parsed(output) => {
            assert!(
                output.artifact.is_none(),
                "critic 修复结果不得携带 artifact"
            );
            output
                .validate_critic()
                .expect("critic 修复结果应通过 validate_critic");
        }
        other => panic!("修复结果不是 Parsed：{other:?}"),
    }
}

#[tokio::test]
async fn critic_repair_output_fences_are_stripped() {
    let fenced = format!("```json\n{CRITIC_OK}\n```");
    let provider = ScriptedProvider::new(&[&fenced]);
    let enforced =
        enforce_worker_output_contract(&provider_arc(&provider), CRITIC_WITH_ARTIFACT, true)
            .await
            .expect("带围栏的修复输出应剥离围栏后接受");
    assert_eq!(enforced.repairs, 1);
    assert_eq!(enforced.text, CRITIC_OK, "围栏应被剥离为 JSON 本体");
    // 独立验证 strip_code_fences 的围栏语义（语言标记行 + 成对围栏）。
    assert_eq!(strip_code_fences(&fenced), CRITIC_OK);
}

#[tokio::test]
async fn enforce_failure_carries_frozen_prefix_and_repairs_count() {
    let provider = ScriptedProvider::new(&["还是自由文本"]);
    // 四路集成微修：clippy err_expect（.err().expect(..) → expect_err(..)）。
    let error = enforce_worker_output_contract(&provider_arc(&provider), FREE_TEXT, false)
        .await
        .expect_err("修复后仍不合规应失败");
    assert!(
        error.message.starts_with("output_contract_invalid:"),
        "失败前缀必须冻结为 output_contract_invalid:（UI failure_code 口径），实际：{}",
        error.message
    );
    assert!(error.message.contains("定向修复一次后仍不符合契约"));
    assert_eq!(
        error.repairs, 1,
        "失败路径也应计 1 次修复（与六期 stats 基线一致）"
    );
    assert_eq!(provider.calls(), 1, "修复上限 1 次：不应有第二次修复调用");
}

#[tokio::test]
async fn subagent_end_to_end_producer() {
    let workspace = std::env::temp_dir();
    let provider = ScriptedProvider::new(&[PRODUCER_OK]);
    let abort = AtomicBool::new(false);
    let approver = AutoApprover { allow: true };
    let runner = SubagentRunner {
        provider: provider_arc(&provider),
        approver: &approver,
        abort: &abort,
        depth: 0,
        max_turns: 3,
        model: "mock".to_string(),
    };
    let text = runner
        .run(&workspace, "调查并交付修复", false)
        .await
        .expect("producer 子代理端到端应成功");
    assert_eq!(text, PRODUCER_OK, "返回值应为契约合规 JSON 本体");
    match parse_worker_output(&text) {
        WorkerOutputParse::Parsed(output) => {
            output.validate().expect("端到端产物应通过 producer 校验");
            assert!(
                output
                    .artifact
                    .as_ref()
                    .expect("producer 必须携带 artifact")
                    .content
                    .contains("交付正文"),
                "Producer 正文只取自 artifact.content"
            );
        }
        other => panic!("端到端产物不是 Parsed：{other:?}"),
    }
    assert_eq!(
        provider.calls(),
        1,
        "合规路径：1 次 Agent 回合调用，0 次修复调用"
    );
}

#[tokio::test]
async fn subagent_end_to_end_critic() {
    let workspace = std::env::temp_dir();
    let provider = ScriptedProvider::new(&[CRITIC_OK]);
    let abort = AtomicBool::new(false);
    let approver = AutoApprover { allow: true };
    let runner = SubagentRunner {
        provider: provider_arc(&provider),
        approver: &approver,
        abort: &abort,
        depth: 0,
        max_turns: 3,
        model: "mock".to_string(),
    };
    let text = runner
        .run(&workspace, "评审上游交付物", true)
        .await
        .expect("critic 子代理端到端应成功");
    assert_eq!(text, CRITIC_OK);
    match parse_worker_output(&text) {
        WorkerOutputParse::Parsed(output) => {
            output
                .validate_critic()
                .expect("端到端 critic 产物应通过 validate_critic");
        }
        other => panic!("端到端 critic 产物不是 Parsed：{other:?}"),
    }
}

#[tokio::test]
async fn subagent_free_text_path_is_closed() {
    let workspace = std::env::temp_dir();
    let provider = ScriptedProvider::new(&[FREE_TEXT, "还是自由文本"]);
    let abort = AtomicBool::new(false);
    let approver = AutoApprover { allow: true };
    let runner = SubagentRunner {
        provider: provider_arc(&provider),
        approver: &approver,
        abort: &abort,
        depth: 0,
        max_turns: 3,
        model: "mock".to_string(),
    };
    let error = runner
        .run(&workspace, "调查并交付修复", false)
        .await
        .expect_err("自由文本交付路径必须关闭（不得登记为交付物）");
    assert!(
        error.starts_with("output_contract_invalid:"),
        "生产路径失败前缀冻结：{error}"
    );
    assert_eq!(
        provider.calls(),
        2,
        "1 次 Agent 回合 + 1 次定向修复，且无第二次修复"
    );
}

// ContractSubagentRunner 的字段构造形态回归（与 SubagentRunner 字段集一致，
// 确保共享执行器可直接承载生产路径的全部字段）。
#[tokio::test]
async fn contract_runner_accepts_explicit_fields() {
    let workspace = std::env::temp_dir();
    let provider = ScriptedProvider::new(&[CRITIC_OK]);
    let abort = AtomicBool::new(false);
    let approver = AutoApprover { allow: true };
    let runner = ContractSubagentRunner {
        provider: provider_arc(&provider),
        approver: &approver,
        abort: &abort,
        depth: 0,
        max_turns: 3,
        model: "mock".to_string(),
    };
    let text = runner
        .run(&workspace, "评审", true)
        .await
        .expect("ContractSubagentRunner 直接构造应可用");
    assert_eq!(text, CRITIC_OK);
}

// ContractEnforcementError 的 Debug 派生回归（日志/断言路径不 panic）。
#[test]
fn enforcement_error_debug_is_stable() {
    let error = ContractEnforcementError {
        message: "output_contract_invalid: 定向修复一次后仍不符合契约：示例违例".to_string(),
        repairs: 1,
    };
    let debug = format!("{error:?}");
    assert!(debug.contains("output_contract_invalid"));
    assert!(debug.contains("1"));
}
