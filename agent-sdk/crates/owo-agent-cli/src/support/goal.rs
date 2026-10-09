//! Codex 对齐命令的共享实现：目标模式状态/提示词、历史与登录登出输出（从 support.rs 拆出）。

use super::*;

// ---------------------------------------------------------------------------
// Codex 对齐命令的共享实现（本地 REPL / Daemon REPL 共用）
// ---------------------------------------------------------------------------

/// 目标模式完成标记：模型在回复最后一行单独输出它表示目标达成。
pub(crate) const GOAL_DONE_MARKER: &str = "GOAL_DONE";

/// 目标模式状态（会话内）。
pub(crate) struct GoalState {
    pub objective: String,
    pub iterations: usize,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct GoalTurnResult {
    pub final_text: Option<String>,
    pub completion_status: owo_agent_protocol::CompletionStatusV1,
    pub failed: bool,
}

pub(crate) fn goal_claim_is_accepted(result: &GoalTurnResult) -> bool {
    !result.failed
        && result.final_text.as_deref().is_some_and(goal_is_done)
        && matches!(
            result.completion_status,
            owo_agent_protocol::CompletionStatusV1::ResponseComplete
                | owo_agent_protocol::CompletionStatusV1::Accepted
        )
}

pub(crate) fn goal_continue_prompt_after_status(
    objective: &str,
    iteration: usize,
    status: owo_agent_protocol::CompletionStatusV1,
) -> String {
    let status_note = match status {
        owo_agent_protocol::CompletionStatusV1::ResponseComplete => {
            "宿主确认本回合没有候选文件变更。继续推进目标；只有整体目标确已完成才输出完成标记。"
        }
        owo_agent_protocol::CompletionStatusV1::Candidate => {
            "宿主只确认候选变更存在，尚未通过验收。请继续登记并执行真实行为验证，修复失败项；未被宿主接受前不要输出完成标记。"
        }
        owo_agent_protocol::CompletionStatusV1::Accepted => {
            "宿主已接受本回合候选版本。检查整体目标是否全部完成；仍有任务则继续，否则输出完成标记。"
        }
        owo_agent_protocol::CompletionStatusV1::Unverified => {
            "宿主认为验收证据缺失或过期。请补齐当前版本所需的行为验证或评审并修复；未通过前不要输出完成标记。"
        }
        owo_agent_protocol::CompletionStatusV1::Blocked => {
            "宿主验收发现失败项或阻断问题。请分析原因、修复并对最终版本重新验证；阻断未解除前不要输出完成标记。"
        }
        owo_agent_protocol::CompletionStatusV1::Aborted => {
            "当前回合已中止。不要声称目标完成。"
        }
    };
    format!(
        "{}\n\n宿主完成状态：{:?}。{}",
        goal_continue_prompt(objective, iteration),
        status,
        status_note
    )
}

/// `/goal` 最大自动推进轮数（env `OWO_GOAL_MAX_ITERATIONS`；默认 0 表示不设上限）。
pub(crate) fn goal_max_iterations() -> usize {
    std::env::var("OWO_GOAL_MAX_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

pub(crate) fn goal_iteration_limit_reached(iterations: usize, max_iterations: usize) -> bool {
    max_iterations > 0 && iterations >= max_iterations
}

pub(crate) fn goal_iteration_label(max_iterations: usize) -> String {
    if max_iterations == 0 {
        "不限".to_string()
    } else {
        max_iterations.to_string()
    }
}

/// 目标模式首轮提示。
pub(crate) fn goal_first_prompt(objective: &str) -> String {
    format!(
        "【目标模式】目标：{objective}\n\n请开始推进该目标。模型判断整体目标已完成时，可在回复的**最后一行单独**输出 \
         {GOAL_DONE_MARKER}；宿主会独立检查本回合状态，只有 Accepted 或 ResponseComplete 才会结束目标，否则会反馈问题并继续推进。"
    )
}

/// 目标模式续推提示。
pub(crate) fn goal_continue_prompt(objective: &str, iteration: usize) -> String {
    format!(
        "【目标模式·第 {iteration} 轮】目标：{objective}\n\n请继续推进未完成部分。模型判断整体目标已完成时，在回复的**最后一行单独**输出 \
         {GOAL_DONE_MARKER}；宿主会独立核对本回合状态，未接受时将反馈问题并继续推进。"
    )
}

pub(crate) fn goal_is_done(final_text: &str) -> bool {
    final_text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| line.trim() == GOAL_DONE_MARKER)
}

/// 目标激活且未完成时，把目标附到输入前；否则原样返回。
pub(crate) fn goal_context_prompt(goal: Option<&GoalState>, line: &str) -> String {
    match goal {
        Some(goal) if !goal.done => format!("【当前目标】{}\n\n{line}", goal.objective),
        _ => line.to_string(),
    }
}

/// 高风险权限档位确认：交互终端要求输入 `yes`；管道模式要求显式 `--yes`。
/// 返回 true 表示允许切换。
pub(crate) fn confirm_high_risk_profile(profile: &str, allow_yes_flag: bool) -> bool {
    use std::io::{IsTerminal, Write};
    if !matches!(profile, "unrestricted" | "danger_full_access") {
        return true;
    }
    println!(
        "{}",
        "⚠ 完全权限（unrestricted）：允许读写工作区外任意路径、执行任意命令并放开网络。".yellow()
    );
    println!(
        "{}",
        "  deny 黑名单、审计与注入类确认仍然生效；越界改动不可回滚。".yellow()
    );
    if !std::io::stdin().is_terminal() {
        return allow_yes_flag;
    }
    print!("确认切换到 unrestricted？输入 yes 继续：");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    line.trim().eq_ignore_ascii_case("yes")
}

/// `/review [额外关注]` 的默认提示。
pub(crate) fn review_prompt(extra: Option<&str>) -> String {
    match extra {
        Some(extra) => {
            format!("请审查工作区当前改动（git diff），指出缺陷、风险与改进建议。额外关注：{extra}")
        }
        None => "请审查工作区当前改动（git diff），指出缺陷、风险与改进建议。".to_string(),
    }
}

/// `/mention <路径>`：解析并展示文件引用信息（供用户复制到提示中）。
pub(crate) fn mention_path(workspace: &std::path::Path, path: Option<&str>) {
    let Some(path) = path else {
        println!("用法：/mention <路径>（相对工作区或绝对路径）");
        return;
    };
    let candidate = workspace.join(path);
    let target = if candidate.exists() {
        candidate
    } else {
        PathBuf::from(path)
    };
    match std::fs::metadata(&target) {
        Ok(meta) if meta.is_file() => {
            let lines = std::fs::read_to_string(&target)
                .map(|s| s.lines().count())
                .unwrap_or(0);
            println!(
                "{} {}（{} 字节，{} 行）",
                "引用：".green(),
                target.display(),
                meta.len(),
                lines
            );
        }
        Ok(_) => println!("{} {}（目录）", "引用：".green(), target.display()),
        Err(error) => println!("{} 无法读取 {}：{error}", "✘".red(), target.display()),
    }
}

/// `/history [n]`：打印最近 n 条输入历史。
pub(crate) fn print_history(data_root: &std::path::Path, arg: Option<&str>) {
    let limit: usize = arg.and_then(|s| s.parse().ok()).unwrap_or(20);
    let path = data_root.join("history.txt");
    let Ok(content) = std::fs::read_to_string(&path) else {
        println!("（无历史记录）");
        return;
    };
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(limit);
    if start == lines.len() {
        println!("（无历史记录）");
        return;
    }
    for (i, line) in lines[start..].iter().enumerate() {
        println!("  {:>4}  {line}", start + i + 1);
    }
}

/// `/login`：凭据来源诊断（只显示存在性与长度，绝不回显密钥）。
pub(crate) fn print_login() {
    let key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let base = std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "（内置 BigModel）".into());
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "（内置 glm-5.3-flash）".into());
    println!("凭据来源：环境变量 OPENAI_API_KEY");
    match key {
        Some(k) => println!("  状态：{}（长度 {}）", "已配置".green(), k.chars().count()),
        None => println!("  状态：{}", "缺失".yellow()),
    }
    println!("  端点：{base}");
    println!("  模型：{model}");
}

/// `/logout`：说明凭据由环境变量注入，CLI 不持有密钥。
pub(crate) fn print_logout() {
    println!("凭据来自环境变量，CLI 不持有、也不回显密钥。");
    println!("如需登出，删除用户级变量后重开终端：");
    println!("  [Environment]::SetEnvironmentVariable('OPENAI_API_KEY', $null, 'User')");
}
