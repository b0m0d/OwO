// §12.3 CLI 拆分批次二：cloud 子命令域（M4a，自 main.rs 机械外移，零行为变化）。
// 队列持久化 + Mock/HTTP 传输 + diff 应用/回滚。

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Args)]
pub(crate) struct CloudArgs {
    /// 任务队列持久化目录（默认 %TEMP%\owo-cloud-queue）
    #[arg(long)]
    dir: Option<PathBuf>,
    /// 传输后端：mock（本地模拟，不联网，默认）或 http（远端，需 --url）
    #[arg(long, default_value = "mock")]
    transport: String,
    /// HTTP 远端 base URL（--transport http 时必填，凭据经 OWO_CLOUD_TOKEN 环境变量）
    #[arg(long)]
    url: Option<String>,
    #[command(subcommand)]
    command: CloudCommand,
}
#[derive(Subcommand)]
pub(crate) enum CloudCommand {
    /// 提交云端任务（--command 可重复，顺序执行）
    Submit(CloudSubmitArgs),
    /// 列出全部任务与状态
    List,
    /// 查看任务状态
    Status(CloudIdArgs),
    /// 查看任务 diff（回传的变更清单）
    Diff(CloudIdArgs),
    /// 把任务 diff 应用到本地工作区
    Apply(CloudApplyArgs),
    /// 回滚已应用的 diff
    Revert(CloudApplyArgs),
}
#[derive(Args)]
pub(crate) struct CloudSubmitArgs {
    /// 工作区快照目录
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// 执行命令（可重复，顺序执行）
    #[arg(long, required = true)]
    command: Vec<String>,
    /// 单条命令超时（秒）
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// 任务名
    #[arg(long)]
    name: Option<String>,
    /// 提交后立即执行（否则仅入队）
    #[arg(long)]
    run: bool,
}
#[derive(Args)]
pub(crate) struct CloudIdArgs {
    task_id: String,
}
#[derive(Args)]
pub(crate) struct CloudApplyArgs {
    task_id: String,
    /// 应用/回滚目标目录
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
}
/// 云端执行任务（M4a）：队列持久化 + Mock/HTTP 传输 + diff 应用/回滚。
pub(crate) async fn run_cloud(args: CloudArgs) -> Result<(), Box<dyn std::error::Error>> {
    use owo_agent_core::cloud_exec::{
        CloudTaskQueue, CloudTaskSpec, HttpTransport, MockRemoteTransport, NullSink,
    };

    let queue_dir = args.dir.unwrap_or_else(|| {
        std::env::temp_dir()
            .join("owo-cloud-queue")
            .join(format!("{}", std::process::id()))
    });
    let transport: Box<dyn owo_agent_core::cloud_exec::CloudTransport> =
        match args.transport.as_str() {
            "mock" => Box::new(MockRemoteTransport::new(
                std::env::temp_dir()
                    .join("owo-cloud-remote")
                    .join(format!("{}", std::process::id())),
            )),
            "http" => {
                let url = args
                    .url
                    .clone()
                    .ok_or("--transport http 需要 --url <base>")?;
                Box::new(HttpTransport::new(url)?)
            }
            other => return Err(format!("未知传输后端：{other}（可选 mock|http）").into()),
        };
    let mut queue = CloudTaskQueue::new(queue_dir.clone(), transport);
    queue.recover()?;

    match args.command {
        CloudCommand::Submit(submit) => {
            let spec = CloudTaskSpec {
                name: submit.name.unwrap_or_default(),
                workspace_dir: submit.workspace.clone(),
                commands: submit.command.clone(),
                env_passthrough: vec![],
                timeout_secs: submit.timeout,
            };
            let task_id = queue.submit(spec)?;
            println!(
                "已提交任务：{task_id}（传输={} 队列={}）",
                queue.transport_kind(),
                queue_dir.display()
            );
            if submit.run {
                while let Some(id) = queue.run_next(&NullSink).await? {
                    let record = queue.record(&id).unwrap();
                    let diff_count = record.result.as_ref().map(|r| r.diff.len()).unwrap_or(0);
                    let error = record
                        .last_error
                        .as_deref()
                        .map(|e| format!("（{e}）"))
                        .unwrap_or_default();
                    println!(
                        "任务 {id} → {:?}（diff 条目={diff_count}）{error}",
                        record.state
                    );
                    if id == task_id {
                        break;
                    }
                }
            }
        }
        CloudCommand::List => {
            for record in queue.list() {
                let diff_count = record.result.as_ref().map(|r| r.diff.len()).unwrap_or(0);
                println!(
                    "{:12} {:9} retries={} diff={} {}",
                    record.task_id,
                    format!("{:?}", record.state),
                    record.retry_count,
                    diff_count,
                    record.spec.name
                );
            }
        }
        CloudCommand::Status(id) => {
            let record = queue
                .record(&id.task_id)
                .ok_or(format!("任务不存在：{}", id.task_id))?;
            let result = record.result.as_ref();
            println!("task_id:   {}", record.task_id);
            println!("state:     {:?}", record.state);
            println!("retries:   {}", record.retry_count);
            println!(
                "exit_code: {}",
                result.map(|r| r.exit_code.unwrap_or(-1)).unwrap_or(-1)
            );
            if let Some(error) = &record.last_error {
                println!("error:     {error}");
            }
        }
        CloudCommand::Diff(id) => {
            let diffs = queue
                .diff(&id.task_id)
                .ok_or(format!("任务不存在或无结果：{}", id.task_id))?;
            for diff in diffs {
                println!("{:8} {}", format!("{:?}", diff.change), diff.path);
            }
            if diffs.is_empty() {
                println!("（无变更）");
            }
        }
        CloudCommand::Apply(id) => {
            let applied = queue
                .apply_to(&id.task_id, &id.workspace)
                .await
                .map_err(|(n, e)| format!("diff 应用失败（已应用 {n} 条）：{e}"))?;
            println!("已应用 {applied} 条 diff 到 {}", id.workspace.display());
        }
        CloudCommand::Revert(id) => {
            let reverted = queue.revert_from(&id.task_id, &id.workspace).await?;
            println!("已回滚 {reverted} 条 diff（{}）", id.workspace.display());
        }
    }
    Ok(())
}
