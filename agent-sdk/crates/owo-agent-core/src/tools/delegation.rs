//! Task-scoped delegation and read-only subagent tools.

use super::{Tool, ToolContext, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;

pub(super) struct ExploreTool;

#[async_trait]
impl Tool for ExploreTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explore".into(),
            description: "把调查任务交给只读探索子代理（只能读/搜文件），返回其调查汇报；多个独立问题用 queries 数组一次并行调查（比逐个问快数倍）".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "单个调查问题（与 queries 二选一）" },
                    "queries": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "多个相互独立的调查问题（并行执行；最多 8 个）"
                    }
                }
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        // 单个 query 与 queries 数组都支持：数组用于"同时调查多个独立问题"，
        // 并行执行（原先只能串行逐个委派，长任务下反馈极慢）。
        let mut queries: Vec<String> = Vec::new();
        if let Some(query) = args.get("query").and_then(Value::as_str) {
            let trimmed = query.trim();
            if !trimmed.is_empty() {
                queries.push(trimmed.to_string());
            }
        }
        if let Some(list) = args.get("queries").and_then(Value::as_array) {
            for item in list {
                if let Some(text) = item.as_str() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        queries.push(trimmed.to_string());
                    }
                }
            }
        }
        if queries.is_empty() {
            return Err("参数缺少字符串字段：query 或 queries".to_string());
        }
        const MAX_PARALLEL_QUERIES: usize = 8;
        let truncated = queries.len() > MAX_PARALLEL_QUERIES;
        queries.truncate(MAX_PARALLEL_QUERIES);
        let runner = ctx.subagent.as_ref().ok_or("子代理运行时不可用")?;
        let workspace = ctx.workspace;
        let results = futures::future::join_all(
            queries
                .iter()
                .map(|query| async move { runner.run(workspace, query, true).await }),
        )
        .await;
        let mut items = Vec::with_capacity(queries.len());
        for (query, result) in queries.into_iter().zip(results) {
            match result {
                Ok(text) => items.push(json!({ "query": query, "ok": true, "text": text })),
                Err(error) => items.push(json!({ "query": query, "ok": false, "error": error })),
            }
        }
        Ok(json!({
            "mode": "explore",
            "parallel": true,
            "truncated": truncated,
            "results": items,
        }))
    }
}

pub(super) struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "subagent".into(),
            description: "把独立任务委派给通用子代理（完整工具、仍需审批），返回其汇报。任务描述里要写明**验收标准**（做到什么算完成）与需要提交的**证据**（命令输出/文件路径/测试结果）；返回后会自动起只读复核子代理独立核对，未通过则按复核意见返工一次。".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "task": { "type": "string" } },
                "required": ["task"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("参数缺少字符串字段：task")?;
        let runner = ctx.subagent.as_ref().ok_or("子代理运行时不可用")?;
        let workspace = ctx.workspace;
        let text = runner.run(workspace, task, false).await?;
        // 质量门（Step 3）：父侧自动起只读 critic 独立复核；不通过则按复核意见
        // 返工一次（有界，不无限重试）。复核本身失败不阻断交付（best-effort）。
        if !crate::contract_worker::subagent_review_enabled() {
            return Ok(json!({ "mode": "general", "text": text }));
        }
        let review = match runner
            .run(
                workspace,
                &crate::contract_worker::review_prompt(task, &text),
                true,
            )
            .await
        {
            Ok(review) => review,
            Err(error) => {
                return Ok(json!({
                    "mode": "general",
                    "text": text,
                    "review": { "error": error },
                }))
            }
        };
        let approved = crate::contract_worker::critic_approved(&review);
        if approved == Some(false) {
            let rework = runner
                .run(
                    workspace,
                    &crate::contract_worker::rework_prompt(task, &text, &review),
                    false,
                )
                .await?;
            return Ok(json!({
                "mode": "general",
                "text": rework,
                "review": {
                    "approved": false,
                    "reworked": true,
                    "critic": review,
                },
            }));
        }
        Ok(json!({
            "mode": "general",
            "text": text,
            "review": { "approved": approved, "reworked": false, "critic": review },
        }))
    }
}

/// A5-1 取优合并自远端 engine：并行 fan-out 只读子代理（2~6 个独立调研任务同时跑）。
pub(super) struct FanOutSubagentsTool;

#[async_trait]
impl Tool for FanOutSubagentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "fan_out_subagents".into(),
            description: "并行派出 2~6 个只读探索子代理，同时调研多个**相互独立**的问题（多模块分别定位、多关键词并行检索、独立子问题调研），汇总各自结论。子代理只读（不改文件、不执行命令、不联网）；任务间不能有依赖（有依赖请串行 explore/subagent）。单任务失败不影响其余，结果按输入顺序返回。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "子任务列表（每条一个独立问题/检索目标，2~6 条）"
                    },
                    "max_parallel": { "type": "integer", "description": "并发上限（默认 3，最大 4）" },
                    "timeout_secs": { "type": "integer", "description": "单个子任务超时秒数（默认 300，范围 30~900）" }
                },
                "required": ["tasks"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let tasks: Vec<String> = args
            .get("tasks")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if tasks.len() < 2 {
            return Err("tasks 至少 2 条（单个任务请直接用 explore/subagent）".to_string());
        }
        if tasks.len() > 6 {
            return Err(format!(
                "tasks 过多（{} 条 > 6）。请拆成两批分别 fan-out",
                tasks.len()
            ));
        }
        let max_parallel = args
            .get("max_parallel")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .clamp(1, 4) as usize;
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(300)
            .clamp(30, 900);
        let fanout = ctx
            .fanout
            .clone()
            .ok_or("当前环境不支持并行子代理（子代理内/CLI/评测环境不可用）")?;
        let abort = ctx.abort;
        // 取消桥：主回合 abort（用户急停/流断开）→ fan-out 取消标志 → 子代理 abort。
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let config = crate::fleet::FanOutConfig {
            max_parallel,
            budget: crate::fleet::Budget {
                max_duration_secs: timeout_secs.saturating_mul(2).max(120),
                ..Default::default()
            },
            per_worker_timeout: Some(std::time::Duration::from_secs(timeout_secs)),
            cancelled: Some(std::sync::Arc::clone(&cancelled)),
            ..Default::default()
        };
        let future = crate::subagent::fan_out_subagents(
            fanout.provider,
            fanout.workspace,
            fanout.model,
            fanout.depth,
            fanout.max_turns,
            tasks.clone(),
            config,
        );
        tokio::pin!(future);
        let report = loop {
            tokio::select! {
                result = &mut future => break result?,
                _ = tokio::time::sleep(std::time::Duration::from_millis(150)) => {
                    if let Some(flag) = abort {
                        if flag.load(std::sync::atomic::Ordering::SeqCst) {
                            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                }
            }
        };
        let succeeded = report.succeeded().len();
        let failed = report.failed().len();
        let results: Vec<Value> = report
            .outcomes
            .iter()
            .enumerate()
            .map(|(index, outcome)| {
                json!({
                    "index": index + 1,
                    "task": tasks.get(index).cloned().unwrap_or_default(),
                    "ok": outcome.ok,
                    "status": outcome.status,
                    "output": outcome.output,
                    "error": outcome.error,
                })
            })
            .collect();
        Ok(json!({
            "succeeded": succeeded,
            "failed": failed,
            "results": results,
        }))
    }
}
