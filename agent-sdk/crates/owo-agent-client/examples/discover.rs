//! 客户端发现/连接的最小可运行示例（P1 证据）：
//!   `cargo run -p owo-agent-client --example discover -- <data_root>`
//!
//! 读取 `<data_root>/runtime/daemon.json` → 校验 pid → 带 token 请求 `/health`。
//! 用于人工/脚本验证"客户端确实连到同一 Daemon"。

use owo_agent_client::connect;
use owo_agent_client::discovery::resolve_data_root;

fn main() {
    let data_root = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(resolve_data_root);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("创建 tokio runtime 失败：{error}");
            std::process::exit(2);
        }
    };
    let exit = runtime.block_on(async {
        match connect(&data_root, None).await {
            Ok(client) => {
                let descriptor = client.descriptor().cloned();
                match client.health().await {
                    Ok(health) => {
                        println!(
                            "connected base_url={} healthy={} api_version={} build_id={} pid={:?} port={:?}",
                            client.base_url(),
                            health.healthy,
                            health.api_version,
                            health.build_id,
                            descriptor.as_ref().map(|d| d.pid),
                            descriptor.as_ref().map(|d| d.port),
                        );
                        match client.list_sessions().await {
                            Ok(sessions) => {
                                println!("sessions={}", sessions.len());
                                for session in &sessions {
                                    println!("session id={} workspace={}", session.id, session.workspace);
                                }
                            }
                            Err(error) => eprintln!("list_sessions 失败：{error}"),
                        }
                        0
                    }
                    Err(error) => {
                        eprintln!("health 请求失败：{error}");
                        1
                    }
                }
            }
            Err(error) => {
                eprintln!("发现/连接 Daemon 失败：{error}");
                1
            }
        }
    });
    std::process::exit(exit);
}
