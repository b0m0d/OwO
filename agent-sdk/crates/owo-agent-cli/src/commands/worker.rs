// §12.3 CLI 拆分批次三：worker 子命令域（自 main.rs 机械外移，零行为变化）。
// 演示真实 owo-agent 子进程全链路；协议常量经 crate::worker_child 显式引用。

use crate::worker_child;
use clap::{Args, Subcommand};

/// `owo-agent worker`：受控 worker 子进程运维（A1）。
#[derive(Args)]
pub(crate) struct WorkerArgs {
    #[command(subcommand)]
    action: WorkerAction,
}

#[derive(Subcommand)]
pub(crate) enum WorkerAction {
    /// 用 current_exe 启动真实 owo-agent 子进程，演示 ready→task→result/stopped 闭环
    /// （父子协议与本机 WorkerPool 同源；子进程零环境继承，凭据不外传）。
    Demo(DemoArgs),
}

/// `owo-agent worker demo` 参数。
#[derive(Args)]
pub(crate) struct DemoArgs {
    /// 处理器：echo | sleep | fail
    #[arg(long, default_value = "echo")]
    handler: String,
    /// echo 文本 / fail 说明
    #[arg(long, default_value = "hello-from-parent")]
    text: String,
    /// sleep 秒数（上限 30）
    #[arg(long, default_value_t = 1)]
    secs: u64,
    /// 任务时长预算秒（超时由父进程 kill 兜底）
    #[arg(long, default_value_t = 10)]
    budget_secs: u64,
}

/// `owo-agent worker demo`：以本机 WorkerPool（Goal worker_pool 模式同源机制）启动
/// 真实 owo-agent 子进程，展示 started → result → stopped 全链路状态。
///
/// 安全约束与 Goal 一致：命令仅限当前可执行文件、`env_clear` + 空白名单
/// （凭据不外传）、任务时长预算到期由父进程 kill 兜底。
pub(crate) async fn run_worker_cmd(args: WorkerArgs) -> Result<(), Box<dyn std::error::Error>> {
    use owo_agent_core::fleet::WorkerEventKind;
    use owo_agent_core::worker_pool::{WorkerBudget, WorkerPool, WorkerSpec};

    match args.action {
        WorkerAction::Demo(demo) => {
            let handler = worker_child::ChildHandler::parse_name(&demo.handler)?;
            let exe = std::env::current_exe()?
                .canonicalize()
                .map_err(|e| format!("无法解析当前可执行文件：{e}"))?;
            if handler == worker_child::ChildHandler::Sleep
                && demo.secs > worker_child::MAX_SLEEP_SECS
            {
                return Err(format!(
                    "--secs 超限：{}（上限 {} 秒）",
                    demo.secs,
                    worker_child::MAX_SLEEP_SECS
                )
                .into());
            }

            let pool = WorkerPool::new();
            let spec = WorkerSpec::new("demo", exe.to_string_lossy().to_string())
                .args(vec![
                    worker_child::CHILD_FLAG.to_string(),
                    worker_child::HANDLER_FLAG.to_string(),
                    handler.as_str().to_string(),
                ])
                .cwd(std::env::current_dir()?)
                // 空白名单：子进程零环境继承（凭据不外传；与 goal_api 校验同源约束）。
                .env_whitelist(Vec::new())
                .budget(WorkerBudget {
                    max_duration_secs: demo.budget_secs,
                    ..Default::default()
                });
            let id = pool
                .spawn(spec)
                .await
                .map_err(|e| format!("子进程启动失败：{e}"))?;
            println!(
                "[started] worker={id} pid={:?} handler={}",
                pool.pid(&id).await,
                handler.as_str()
            );

            let input = serde_json::json!({
                "text": demo.text,
                "secs": demo.secs,
            });
            let action_note = match handler {
                worker_child::ChildHandler::Echo => "echo",
                worker_child::ChildHandler::Sleep => "sleep",
                worker_child::ChildHandler::Fail => "fail",
            };
            println!("[task] {action_note} {input}");
            match pool.submit(&id, &input).await {
                Ok(output) => println!("[result] ok: {output}"),
                Err(error) => println!("[result] error: {error}"),
            }

            pool.shutdown().await;
            println!("[stopped] 生命周期事件：");
            for event in pool.events().await {
                println!(
                    "  worker.{} {}（{}）",
                    event.kind.label(),
                    event.worker,
                    event.detail
                );
            }
            let saw_budget_abort = pool
                .events()
                .await
                .iter()
                .any(|event| matches!(event.kind, WorkerEventKind::BudgetAborted));
            if saw_budget_abort {
                println!("[stopped] 检测到预算中止事件（budget_aborted）：子进程已由父进程回收");
            }
            Ok(())
        }
    }
}
