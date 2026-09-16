// §12.3 CLI 拆分批次一：audit 子命令域（自 main.rs 机械外移，零行为变化）。
// 密钥解析/链校验完全自含；测试模块随域同迁（super:: 语义不变）。

use clap::{Args, Subcommand};

/// `owo-agent audit`：verify|export（复用 core audit_chain::run_audit_cli）。
#[derive(Args)]
pub(crate) struct AuditArgs {
    #[command(subcommand)]
    action: AuditAction,
    /// 审计链 HMAC 密钥（hex 字符串；缺省读 OWO_AUDIT_KEY 环境变量）
    #[arg(long)]
    key: Option<String>,
    /// 审计链密钥文件路径（hex 文本；与 --key 二选一）
    #[arg(long)]
    key_file: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum AuditAction {
    /// 校验导出文件的链完整性（检出任意篡改）
    Verify { path: String },
    /// 把已导出审计文件另存为 out（可离线分发校验）
    Export { path: String, out: String },
}

/// 解析审计链密钥：--key → OWO_AUDIT_KEY → --key-file。缺省明确报错（不 panic）。
pub(crate) fn audit_key(args: &AuditArgs) -> Result<Vec<u8>, String> {
    let hex = if let Some(key) = &args.key {
        Some(key.clone())
    } else if let Ok(env) = std::env::var("OWO_AUDIT_KEY") {
        if env.trim().is_empty() {
            None
        } else {
            Some(env)
        }
    } else {
        None
    };
    let hex = match hex {
        Some(hex) => hex,
        None => {
            if let Some(path) = &args.key_file {
                let content =
                    std::fs::read_to_string(path).map_err(|e| format!("读取密钥文件失败：{e}"))?;
                content.trim().to_string()
            } else {
                return Err(
                    "缺少审计链密钥：请用 --key <hex> 或设置 OWO_AUDIT_KEY 环境变量".to_string(),
                );
            }
        }
    };
    let hex = hex.trim();
    if hex.len() % 2 != 0 {
        return Err("密钥必须为偶数长度 hex 字符串".to_string());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| format!("密钥不是合法 hex：{e}"))
        })
        .collect()
}

/// `owo-agent audit verify|export` 命令入口。
pub(crate) fn run_audit_cmd(args: AuditArgs) -> Result<(), Box<dyn std::error::Error>> {
    use owo_agent_core::audit_chain::{run_audit_cli, AuditCliCommand};
    let key = audit_key(&args)?;
    let command = match args.action {
        AuditAction::Verify { path } => AuditCliCommand::Verify { path },
        AuditAction::Export { path, out } => AuditCliCommand::Export { path, out },
    };
    let outcome = run_audit_cli(&command, &key)?;
    match outcome {
        owo_agent_core::audit_chain::AuditCliOutcome::VerifyOk { records, anchors } => {
            println!("审计链校验通过：{records} 条记录 / {anchors} 个锚点");
        }
        owo_agent_core::audit_chain::AuditCliOutcome::Exported { out, records } => {
            println!("审计导出完成：{out}（{records} 条记录，含链锚点）");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod audit_cli_tests {
    use super::audit_key;
    use super::AuditAction;
    use super::AuditArgs;

    fn args(action: AuditAction, key: Option<String>, key_file: Option<String>) -> AuditArgs {
        AuditArgs {
            action,
            key,
            key_file,
        }
    }

    #[test]
    fn audit_key_from_flag_hex_decodes() {
        let a = args(
            AuditAction::Verify { path: "x".into() },
            Some("00ff10ab".into()),
            None,
        );
        assert_eq!(audit_key(&a).unwrap(), vec![0x00, 0xff, 0x10, 0xab]);
    }

    #[test]
    fn audit_key_missing_is_explicit_error() {
        let a = args(AuditAction::Verify { path: "x".into() }, None, None);
        let err = audit_key(&a).unwrap_err();
        assert!(err.contains("OWO_AUDIT_KEY"), "缺密钥应明确报错：{err}");
    }

    #[test]
    fn audit_key_odd_hex_rejected() {
        let a = args(
            AuditAction::Verify { path: "x".into() },
            Some("abc".into()),
            None,
        );
        assert!(audit_key(&a).is_err());
    }
}
