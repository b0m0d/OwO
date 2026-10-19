// §12.3 CLI 拆分批次二：plugins 子命令域（自 main.rs 机械外移，零行为变化）。
// 签名/扫描/回滚由 core PluginManager 提供；远端拉取走 HTTP 面。

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct PluginArgs {
    #[command(subcommand)]
    action: PluginAction,
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// 数据目录（默认 OWO_AGENT_DATA 或 %LOCALAPPDATA%\OwO\Agent）
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// 远端市场 URL（预留；当前实现为本地离线模式）
    #[arg(long)]
    url: Option<String>,
    /// 离线模式（默认 true；不联网）
    #[arg(long)]
    offline: bool,
}
#[derive(Subcommand)]
pub(crate) enum PluginAction {
    /// 列出本地插件目录
    Catalog,
    /// 校验插件目录（签名/扫描/版本）
    Check { dir: PathBuf },
    /// 校验插件目录（与 check 相同）
    Verify { dir: PathBuf },
    /// 安装插件目录
    Install { dir: PathBuf },
    /// 更新已安装插件（id 为已安装插件 id）
    Update { id: String, dir: PathBuf },
    /// 卸载插件
    Uninstall { id: String },
}
/// 插件市场治理（本地离线模式）：catalog/check/install/update/uninstall/verify。
/// 签名/扫描/回滚由 core PluginManager 提供；远端拉取走 HTTP 面（POST /plugins/market/refresh）。
pub(crate) fn run_plugin(args: PluginArgs) -> Result<(), Box<dyn std::error::Error>> {
    use owo_agent_core::plugin::{
        discover_plugins, scan_plugin_for_risks, PluginManager, PluginManifest,
    };

    let workspace = args.workspace;
    let data_root = if let Some(dir) = args.data_dir {
        dir
    } else if let Ok(env_dir) = std::env::var("OWO_AGENT_DATA") {
        std::path::PathBuf::from(env_dir)
    } else if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        std::path::PathBuf::from(local).join("OwO").join("Agent")
    } else {
        std::path::PathBuf::from("data")
    };
    let app_version = env!("CARGO_PKG_VERSION").to_string();
    let _ = &args.url; // 远端 URL 预留：当前为本地离线模式（--offline 恒真）。
    let _ = args.offline;

    match args.action {
        PluginAction::Catalog => {
            let plugins = discover_plugins(&workspace, &data_root);
            println!(
                "本地插件目录（workspace={} data={}）：",
                workspace.display(),
                data_root.display()
            );
            for (path, manifest) in &plugins {
                let base = path.parent().unwrap_or(path);
                let manifest_content = std::fs::read_to_string(path).unwrap_or_default();
                let entry_content = manifest
                    .entry
                    .as_ref()
                    .and_then(|entry| std::fs::read_to_string(base.join(entry)).ok());
                let risks = scan_plugin_for_risks(
                    &manifest_content,
                    entry_content.as_deref(),
                    &manifest.network_allowlist,
                );
                let risk = if risks.is_empty() { "clean" } else { "RISK" };
                println!(
                    "  {} v{}（{}）[{}]{}",
                    manifest.id,
                    manifest.version,
                    manifest.name,
                    risk,
                    if risks.is_empty() {
                        String::new()
                    } else {
                        format!("：{}", risks.join("；"))
                    }
                );
            }
            if plugins.is_empty() {
                println!("  （无插件）");
            }
        }
        PluginAction::Check { dir } | PluginAction::Verify { dir } => {
            let manager = PluginManager::new(data_root.clone(), app_version);
            match manager.verify_plugin_dir(&dir) {
                Ok(report) => {
                    println!(
                        "校验通过：{} v{}（{:?}）",
                        report.id, report.version, report.state
                    );
                    for line in report.audit {
                        println!("  {line}");
                    }
                }
                Err(error) => {
                    println!("校验失败：{error}");
                    std::process::exit(1);
                }
            }
        }
        PluginAction::Install { dir } => {
            let manager = PluginManager::new(data_root.clone(), app_version);
            match manager.install(&dir) {
                Ok(report) => {
                    println!(
                        "安装完成：{} v{}（{:?}）",
                        report.id, report.version, report.state
                    );
                    for line in report.audit {
                        println!("  {line}");
                    }
                }
                Err(error) => {
                    println!("安装失败：{error}");
                    std::process::exit(1);
                }
            }
        }
        PluginAction::Update { id, dir } => {
            let manager = PluginManager::new(data_root.clone(), app_version);
            let backup = data_root.join("plugins").join("backups");
            match manager.update(&dir, &backup) {
                Ok(report) => {
                    println!("更新完成：{id} → v{}（{:?}）", report.version, report.state);
                    for line in report.audit {
                        println!("  {line}");
                    }
                }
                Err(error) => {
                    println!("更新失败（已回滚或旧版保留）：{error}");
                    std::process::exit(1);
                }
            }
        }
        PluginAction::Uninstall { id } => {
            let manager = PluginManager::new(data_root.clone(), app_version);
            match manager.uninstall(&id) {
                Ok(audit_lines) => {
                    println!("已卸载 {id}");
                    for line in audit_lines {
                        println!("  {line}");
                    }
                }
                Err(error) => {
                    println!("卸载失败：{error}");
                    std::process::exit(1);
                }
            }
        }
    }
    let _ = PluginManifest::load; // 类型引用保活（防未使用告警变体依赖）。
    Ok(())
}
