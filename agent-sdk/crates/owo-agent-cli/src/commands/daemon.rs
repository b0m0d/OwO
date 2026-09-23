// P1（指南 §2.3/§5.4）：`daemon status|stop` —— 经 discovery + 唯一共享客户端，
// 不本地构造 Agent / 打开 SQLite / 连 MCP（由 tests/turn_path_guard_tests.rs 守卫）。

use crate::support::data_root;
use clap::{Args, Subcommand};
use owo_agent_client::discovery::{process_alive, DaemonDiscovery};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Args)]
pub(crate) struct DaemonArgs {
    #[command(subcommand)]
    action: DaemonAction,
    /// 数据根（缺省用户级数据目录 / OWO_AGENT_DATA）。
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// 显示当前 Daemon 状态（pid/port/api/build/instance）
    Status,
    /// 优雅停止当前 Daemon（二次确认；等待进程退出）
    Stop,
}

pub(crate) async fn run_daemon_cmd(args: DaemonArgs) -> Result<(), Box<dyn std::error::Error>> {
    let root = data_root(args.data_dir.clone());
    match args.action {
        DaemonAction::Status => {
            match DaemonDiscovery::read(&root) {
                Ok(discovery) => {
                    let descriptor = &discovery.descriptor;
                    println!(
                        "Daemon 运行中：pid={} port={} api={} build={} instance='{}' data_root={} started_at={}",
                        descriptor.pid,
                        descriptor.port,
                        descriptor.api_version,
                        descriptor.build_id,
                        descriptor.instance_id,
                        descriptor.data_root,
                        descriptor.started_at,
                    );
                    println!("base_url={}", discovery.base_url());
                }
                Err(error) => {
                    println!("Daemon 未运行：{error}");
                    println!("数据根：{}", root.display());
                }
            }
            Ok(())
        }
        DaemonAction::Stop => {
            let client = match owo_agent_client::connect(&root, None).await {
                Ok(client) => client,
                Err(error) => {
                    println!("Daemon 未运行，无需停止：{error}");
                    return Ok(());
                }
            };
            let pid = client.descriptor().map(|descriptor| descriptor.pid);
            client.request_shutdown(true).await?;
            if let Some(pid) = pid {
                let deadline = Instant::now() + Duration::from_secs(30);
                while Instant::now() < deadline && process_alive(pid) {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                if process_alive(pid) {
                    return Err(format!("Daemon（pid={pid}）在 30s 内未退出").into());
                }
            }
            println!("Daemon 已优雅停止");
            Ok(())
        }
    }
}
