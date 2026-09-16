// §12.3 CLI 拆分批次三：eval 子命令域（自 main.rs 机械外移，零行为变化）。

use crate::support::resolve_model;
use async_trait::async_trait;
use clap::Args;
use owo_agent_core::{builtin_suite, eval_suite_path, run_suite};
use owo_agent_core::{ChatMessage, ModelOutput, ModelProvider, OpenAiCompatibleConfig, ToolSpec};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct EvalArgs {
    /// 自定义套件 JSON 路径（缺省使用内置 demo 套件）
    #[arg(long)]
    suite: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
}

pub(crate) async fn run_eval(args: EvalArgs) -> Result<(), Box<dyn std::error::Error>> {
    let model = resolve_model(args.model, None);
    let mut config = OpenAiCompatibleConfig::from_env()?;
    config.model = model.clone();
    // R9：模型网关韧性（重试/熔断/failover）。
    let provider = std::sync::Arc::new(owo_agent_core::gateway::ResilientProvider::from_config(
        config,
    )?);
    let suite = match args.suite {
        Some(path) => {
            eval_suite_path(&path).ok_or_else(|| format!("评估套件解析失败：{}", path.display()))?
        }
        None => builtin_suite(),
    };
    println!(
        "运行评估套件：{}（{} 个用例）",
        suite.name,
        suite.cases.len()
    );
    let report = run_suite(provider, &model, &suite).await;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub(crate) struct StubProvider;

#[async_trait]
impl ModelProvider for StubProvider {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _tools: &[ToolSpec],
    ) -> Result<ModelOutput, String> {
        Err("bench stub".to_string())
    }
}
