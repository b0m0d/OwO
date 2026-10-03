use std::path::Path;

const RULE_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// 读取工作区项目规则（AGENTS.md / CLAUDE.md），作为系统指令的一部分。
pub fn load_project_rules(workspace: &Path) -> String {
    let mut rules = Vec::new();
    for name in RULE_FILES {
        let path = workspace.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            rules.push(format!("### {} 规则（必须遵守）\n{}", name, content.trim()));
        }
    }
    rules.join("\n\n")
}

pub fn build_system_prompt(configured: Option<&str>, rules: &str) -> String {
    let mut parts = Vec::new();
    if let Some(configured) = configured {
        parts.push(configured.to_string());
    }
    if !rules.is_empty() {
        parts.push(rules.to_string());
    }
    parts.push(
        "你是 OwO Agent SDK 驱动的智能体。普通问答自然直接回答，不要把闲聊强行转成任务。\
         用户提出可执行目标时，主动使用可用工具持续推进：理解目标后执行，再查看实际结果并修正；\
         不要只给计划，也不要在一次工具调用或一次模型回复后提前收尾。继续工作，直到目标达到，\
         或遇到明确的权限等待、不可恢复阻断、需要用户决策的歧义。完成判断应基于实际结果；\
         需要验证的工作先运行相称的检查，未验证或未完成的部分如实说明。实现或修改代码的任务应先用 verification_plan 登记与用户验收要求对应的宿主检查，再执行这些检查；计划本身不是通过证据，不能把未覆盖要求说成已验收。工具调用必须经过权限审批；\
         被拒绝的操作不要重试同一参数，应寻找更安全的替代方案。"
            .to_string(),
    );
    parts.join("\n\n")
}


#[cfg(test)]
mod tests {
    use super::build_system_prompt;

    #[test]
    fn executable_requests_continue_until_done_or_blocked_without_changing_normal_chat() {
        let prompt = build_system_prompt(None, "");

        assert!(prompt.contains("普通问答自然直接回答"));
        assert!(prompt.contains("不要只给计划"));
        assert!(prompt.contains("直到目标达到"));
        assert!(prompt.contains("未验证或未完成的部分如实说明"));
        assert!(prompt.contains("权限审批"));
        assert!(prompt.contains("verification_plan"));
        assert!(prompt.contains("计划本身不是通过证据"));
    }
}
