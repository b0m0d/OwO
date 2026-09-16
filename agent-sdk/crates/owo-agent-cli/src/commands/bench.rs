// §12.3 CLI 拆分批次三：bench 子命令域（自 main.rs 机械外移，零行为变化）。

use clap::Args;
use std::sync::Arc;

use crate::commands::eval::StubProvider;
use owo_agent_core::{Agent, AgentConfig, Policy, SqliteSessionStore, ToolRegistry};

#[derive(Args)]
pub(crate) struct BenchArgs {
    #[arg(long, default_value_t = 200)]
    requests: usize,
}

pub(crate) async fn run_bench(args: BenchArgs) -> Result<(), Box<dyn std::error::Error>> {
    let provider = std::sync::Arc::new(StubProvider);
    let registry = ToolRegistry::new();
    let policy = Policy::new(std::env::temp_dir());
    let agent = Agent::new(provider, registry, policy, AgentConfig::default());
    let root = std::env::temp_dir().join(format!("owo-bench-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    let store = SqliteSessionStore::open(&root.join("index.db"))?;
    let state = Arc::new(owo_agent_server::AppState::new(
        agent,
        store,
        root.join("traces"),
        root.clone(),
        std::env::temp_dir(),
    ));
    let app = owo_agent_server::build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let client = reqwest::Client::new();
    let mut durations = Vec::new();
    for _ in 0..args.requests {
        let start = std::time::Instant::now();
        let response = client.get(format!("http://{addr}/health")).send().await?;
        assert_eq!(response.status().as_u16(), 200);
        durations.push(start.elapsed().as_micros() as u64);
    }
    durations.sort_unstable();
    let p50 = durations[durations.len() / 2];
    let index95 = ((durations.len() as f64 * 0.95) as usize).saturating_sub(1);
    let p95 = durations[index95].max(p50);
    println!(
        "{}",
        serde_json::json!({
            "requests": durations.len(),
            "p50_us": p50,
            "p95_us": p95,
            "max_us": durations.last().copied().unwrap_or(0),
        })
    );
    server.abort();
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}
