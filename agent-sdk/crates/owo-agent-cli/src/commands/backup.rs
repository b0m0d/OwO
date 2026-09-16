// §12.3 CLI 拆分批次一：backup 子命令域（自 main.rs 机械外移，零行为变化）。
// 复用服务端 backup.rs 打包逻辑；数据目录经 crate::support::ensure_data_root。

use clap::Args;

use crate::support::ensure_data_root;

/// `owo-agent backup`：本地一键备份（同 HTTP POST /storage/backup 的打包逻辑）。
#[derive(Args)]
pub(crate) struct BackupArgs {
    /// 输出 zip 路径（缺省 <data>/backups/backup-<时间戳>.zip）
    #[arg(long)]
    out: Option<std::path::PathBuf>,
}

/// `owo-agent backup`：复用服务端 backup.rs 打包逻辑，本地一键备份。
pub(crate) fn run_backup_cmd(args: BackupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?;
    let root = ensure_data_root(None, &workspace);
    let zip_bytes = owo_agent_server::backup::build_backup_zip(&root, &workspace)?;
    let out = match args.out {
        Some(path) => path,
        None => {
            let dir = root.join("backups");
            std::fs::create_dir_all(&dir)?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default();
            dir.join(format!("backup-{stamp}.zip"))
        }
    };
    std::fs::write(&out, &zip_bytes)?;
    println!("备份完成：{}（{} 字节）", out.display(), zip_bytes.len());
    Ok(())
}
