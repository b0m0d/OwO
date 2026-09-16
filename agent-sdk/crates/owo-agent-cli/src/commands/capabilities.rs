// §12.3 CLI 拆分批次三：capabilities 子命令域（自 main.rs 机械外移，零行为变化）。

use crate::ui_output::OutputMode;

/// `owo-agent capabilities`：直接读取 server 注册表（§8.3 单一来源），离线免鉴权。
/// §11 输出模式：human（分组列表）/ plain（TSV 行）/ jsonl（单行 JSON 目录）。
pub(crate) fn run_capabilities_cmd(output: OutputMode) -> Result<(), Box<dyn std::error::Error>> {
    use owo_agent_server::capabilities::{CapabilityMaturity, CAPABILITIES};

    let stable = CAPABILITIES
        .iter()
        .filter(|capability| capability.maturity == CapabilityMaturity::Stable)
        .count();
    let beta = CAPABILITIES
        .iter()
        .filter(|capability| capability.maturity == CapabilityMaturity::Beta)
        .count();
    let experimental = CAPABILITIES
        .iter()
        .filter(|capability| capability.maturity == CapabilityMaturity::Experimental)
        .count();

    match output {
        OutputMode::Jsonl => {
            // 目录不是回合流协议事件，按单行 JSON 对象输出（不与 turn 流混用）。
            let catalog = serde_json::json!({
                "type": "capabilities",
                "count": CAPABILITIES.len(),
                "maturity": { "stable": stable, "beta": beta, "experimental": experimental },
                "capabilities": CAPABILITIES.iter().map(|capability| serde_json::json!({
                    "id": capability.id.as_str(),
                    "user_name": capability.user_name,
                    "summary": capability.summary,
                    "maturity": capability.maturity.as_str(),
                    "advanced": capability.advanced,
                    "entrypoints": capability.entrypoints.iter().map(|entrypoint|
                        serde_json::json!({
                            "kind": entrypoint.kind,
                            "target": entrypoint.target,
                            "label": entrypoint.label,
                        })
                    ).collect::<Vec<serde_json::Value>>(),
                    "required_effects": capability.required_effects.iter()
                        .map(|class| class.label()).collect::<Vec<&'static str>>(),
                    "dependencies": capability.dependencies.iter()
                        .map(|id| id.as_str()).collect::<Vec<&'static str>>(),
                })).collect::<Vec<serde_json::Value>>(),
            });
            println!("{catalog}");
        }
        OutputMode::Plain => {
            println!("id\tmaturity\tadvanced\tuser_name");
            for capability in CAPABILITIES {
                println!(
                    "{}\t{}\t{}\t{}",
                    capability.id.as_str(),
                    capability.maturity.as_str(),
                    capability.advanced,
                    capability.user_name
                );
            }
        }
        OutputMode::Human => {
            println!(
                "OwO Agent 功能目录（§8.3）：共 {} 项能力（Stable {stable} / Beta {beta} / Experimental {experimental}）",
                CAPABILITIES.len()
            );
            for capability in CAPABILITIES {
                let advanced = if capability.advanced {
                    "（高级）"
                } else {
                    ""
                };
                println!(
                    "\n[{}] {} ({}){advanced}\n  {}",
                    capability.maturity.as_str(),
                    capability.user_name,
                    capability.id.as_str(),
                    capability.summary
                );
                for entrypoint in capability.entrypoints {
                    println!(
                        "  - {:<4} {} · {}",
                        entrypoint.kind, entrypoint.target, entrypoint.label
                    );
                }
            }
        }
    }
    Ok(())
}
