//! 模板级 Prompt 编译器与上下文字节预算（八期 · 第一路）。
//!
//! 产品背景：六期 live 基线显示所有角色共用同一段通用 Prompt（角色 + 目标 + 契约 +
//! 上游预览），上游正文按固定 2000 字符截断、无总预算、无截断记录——重复调用、
//! 冗余角色与上下文浪费是「多 Agent 不优于单 Agent」的主要放大器之一。
//!
//! 本模块把角色 Prompt 变成**按模板 + 角色 + 工具权限编译**的产物：
//! - [`compile_prompt`]：角色专属 Prompt——当前目标 / 输入 Artifact / 必须完成的
//!   工作 / 禁止执行的工作 / 输出格式 / 验收条件 / 剩余调用预算。模板角色从
//!   [`crate::builtin_team_templates`] 取结构化段（必须做/禁止做/输出格式/验收）；
//!   动态角色退回「交接契约驱动」的同构骨架（不再有无边界的通用提示）。
//! - [`compile_upstream`]：上游上下文字节预算——小 Artifact 传全文；大 Artifact 传
//!   摘要 + sha256 + CAS ref；超总预算的条目降级为仅引用行。每条截断都记录原因
//!   （[`TruncationRecord`]），随 `context_bytes` 一起上报自适应指标。
//! - 「禁止执行」行由 [`crate::worker_profile::WorkerProfile::prompt_guard_lines`]
//!   按角色族派生（注册表面即权限边界 → Prompt 与真实工具面一致）。
//!
//! 设计边界：纯函数、零 I/O、零模型调用；预算常量可经 [`PromptBudget`] 调整。

use crate::worker_profile::WorkerProfile;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 单 Artifact 全文传递的字节阈值（UTF-8；≤ 阈值传正文，> 阈值传摘要+哈希+ref）。
pub const DEFAULT_PER_ARTIFACT_BYTES: usize = 2400;
/// 上游上下文总字节预算（全部条目累计；超出者降级为仅引用行）。
pub const DEFAULT_TOTAL_UPSTREAM_BYTES: usize = 8000;
/// 大 Artifact 摘要保留的字符数（按字符截断，避免劈开 UTF-8 多字节序列）。
pub const DEFAULT_SUMMARY_CHARS: usize = 700;

/// Prompt 上下文预算（八期一路；纯数据，可整体覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptBudget {
    pub per_artifact_bytes: usize,
    pub total_upstream_bytes: usize,
    pub summary_chars: usize,
}

impl Default for PromptBudget {
    fn default() -> Self {
        Self {
            per_artifact_bytes: DEFAULT_PER_ARTIFACT_BYTES,
            total_upstream_bytes: DEFAULT_TOTAL_UPSTREAM_BYTES,
            summary_chars: DEFAULT_SUMMARY_CHARS,
        }
    }
}

/// 截断记录（明确记录截断原因；随自适应指标上报）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TruncationRecord {
    pub artifact_id: String,
    /// 截断原因：`per_artifact_budget`（大 Artifact → 摘要+哈希+ref）|
    /// `total_budget`（累计超总预算 → 仅引用行）。
    pub reason: String,
    /// 保留字节（正文部分，不含标头/页脚说明）。
    pub kept_bytes: usize,
    /// 原始内容字节。
    pub total_bytes: usize,
}

/// 上游编译结果：渲染块 + 实际上下文字节 + 截断记录。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CompiledUpstream {
    /// 渲染后的上游块文本（空条目时为「（无上游产物；你是首个执行者）」）。
    pub text: String,
    /// 上游上下文实际字节数（UTF-8；计入 `context_bytes` 指标）。
    pub context_bytes: usize,
    /// 全文传递条数（小 Artifact）。
    pub full_count: usize,
    /// 摘要传递条数（大 Artifact）。
    pub summarized_count: usize,
    /// 仅引用行条数（超总预算降级）。
    pub ref_only_count: usize,
    /// 截断记录（摘要/降级条目各一条）。
    pub truncations: Vec<TruncationRecord>,
}

/// `cas://sha256:{hash}` → hash（非该前缀返回 None）。
fn sha256_of_ref(cas_ref: &str) -> Option<&str> {
    cas_ref.strip_prefix("cas://sha256:")
}

/// 按字符截断（不劈开 UTF-8 多字节序列），截断后补省略号。
fn char_truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

/// 上游条目编译（八期一路核心）：
///
/// - 正文 ≤ `per_artifact_bytes` → 全文块（`full`）；
/// - 正文 > 阈值 → 摘要块 + sha256 + CAS ref（`summary`，记录 `per_artifact_budget`）；
/// - 累计超 `total_upstream_bytes` → 仅引用行（`ref_only`，记录 `total_budget`）。
///
/// 条目按输入顺序处理（直接依赖在前者优先保留；调用方保证只传直接依赖）。
pub fn compile_upstream(items: &[Value], budget: PromptBudget) -> CompiledUpstream {
    let mut out = CompiledUpstream::default();
    if items.is_empty() {
        out.text = "（无上游产物；你是首个执行者）".to_string();
        return out;
    }
    let mut blocks: Vec<String> = Vec::with_capacity(items.len());
    let mut used = 0usize;
    for item in items {
        let role = item.get("role").and_then(Value::as_str).unwrap_or("?");
        let id = item
            .get("artifact_id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let version = item.get("version").and_then(Value::as_u64).unwrap_or(0);
        let content = item.get("content").and_then(Value::as_str).unwrap_or("");
        let cas_ref = item
            .get("cas_ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let total_bytes = content.as_bytes().len();
        let hash = sha256_of_ref(cas_ref).unwrap_or("");
        let header = format!("### {role} v{version}（{id}）");
        if total_bytes <= budget.per_artifact_bytes {
            // 小 Artifact：全文传递。
            let body = format!("{header}\n{content}");
            used += content.as_bytes().len();
            out.full_count += 1;
            blocks.push(body);
        } else if used + budget.summary_chars * 4 <= budget.total_upstream_bytes {
            // 大 Artifact：摘要 + 哈希 + ref（截断原因明确记录）。
            let summary = char_truncate(content.trim(), budget.summary_chars);
            let summary_bytes = summary.as_bytes().len();
            used += summary_bytes;
            out.summarized_count += 1;
            out.truncations.push(TruncationRecord {
                artifact_id: id.clone(),
                reason: "per_artifact_budget".to_string(),
                kept_bytes: summary_bytes,
                total_bytes,
            });
            blocks.push(format!(
                "{header}\n{summary}\n（以上为摘要，原文 {total_bytes} 字节超单 Artifact 预算 \
                 {} 字节，已按截断原因 per_artifact_budget 记录；完整正文按版本化引用 {id} v{version} \
                 从 Project Space/CAS 获取，sha256={hash}，CAS ref={cas_ref}）",
                budget.per_artifact_bytes
            ));
        } else {
            // 累计超总预算：仅引用行（原因明确记录）。
            out.ref_only_count += 1;
            out.truncations.push(TruncationRecord {
                artifact_id: id.clone(),
                reason: "total_budget".to_string(),
                kept_bytes: 0,
                total_bytes,
            });
            blocks.push(format!(
                "{header}（仅引用：累计超上游上下文总预算 {} 字节，已按截断原因 total_budget \
                 记录；正文 {total_bytes} 字节按版本化引用 {id} v{version} 自取，sha256={hash}，\
                 CAS ref={cas_ref}）",
                budget.total_upstream_bytes
            ));
        }
    }
    out.context_bytes = used;
    out.text = blocks.join("\n\n");
    out
}

/// Prompt 编译输入（由 `TeamCoordinator::assemble_context_slice` 组装）。
pub struct PromptContext<'a> {
    pub objective: &'a str,
    pub role: &'a str,
    pub handoff_contract: &'a str,
    /// 模板 id（内置模板 → 角色专属段；动态组队 → None）。
    pub template_id: Option<&'a str>,
    /// 该角色调用预算（模板 `budget_calls_per_role`；0 = 未声明）。
    pub budget_calls: usize,
    /// critic 角色（评审结论口径，禁带 artifact）。
    pub is_critic: bool,
    pub upstream: &'a CompiledUpstream,
}

/// 角色专属 Prompt（八期一路）：模板 + 角色 + 工具权限 → 编译产物。
///
/// 段结构：当前目标 / 输入 Artifact / 必须完成 / 禁止执行 / 输出格式 / 验收条件 /
/// 剩余调用预算。内置模板角色用结构化段（`builtin_team_templates`），动态角色用
/// 交接契约回退填充；「禁止执行」行与 [`WorkerProfile`] 的真实工具面一致。
pub fn compile_prompt(ctx: &PromptContext) -> String {
    let sections = ctx
        .template_id
        .and_then(|t| crate::builtin_team_templates::prompt_sections_for(t, ctx.role))
        .map(|s| (true, s));
    let profile = WorkerProfile::for_role(ctx.role, ctx.budget_calls);
    let max_turns = profile.max_turns;

    let template_line = match ctx.template_id {
        Some(t) => format!(
            "你是 WorkSwarm 团队中的 {} 成员（模板：{t}），围绕共享项目空间协作。",
            ctx.role
        ),
        None => format!(
            "你是 WorkSwarm 团队中的 {} 成员（动态组队），围绕共享项目空间协作。",
            ctx.role
        ),
    };
    let must_do = match &sections {
        Some((_, s)) => s.must_do.join("\n"),
        None => ctx.handoff_contract.to_string(),
    };
    let mut must_not = profile.prompt_guard_lines();
    if let Some((_, s)) = &sections {
        must_not.extend(s.must_not_do.iter().cloned());
    }
    if ctx.is_critic {
        must_not.push(
            "你是只读评审者：不得修改或覆盖上游产物，只输出评审结论（不产出交付物正文）。"
                .to_string(),
        );
    }
    let output_format = match &sections {
        Some((_, s)) => s.output_format.clone(),
        None => {
            if ctx.is_critic {
                "输出评审结论（结构化 JSON：{\"approved\":bool,\"score\":0-100,\"comments\":[..]}）；不输出交付物正文。".to_string()
            } else {
                "直接输出你的产物正文（不要输出解释过程）。".to_string()
            }
        }
    };
    let acceptance = match &sections {
        Some((_, s)) => s.acceptance.join("\n"),
        None => {
            "- 交付物非空且与当前目标直接相关\n- 严格遵守上方禁止执行条款（越权即失败）".to_string()
        }
    };
    format!(
        "# 角色：{role}\n{template_line}\n\
## 当前目标\n{objective}\n\n\
## 输入 Artifact（来自 Project Space 的版本化共享产物，按 ref 传递；仅含直接依赖）\n{upstream}\n\n\
## 必须完成\n{must_do}\n\n\
## 禁止执行\n{must_not}\n\n\
## 输出格式\n{output_format}\n\n\
## 验收条件\n{acceptance}\n\n\
## 剩余调用预算\n你的回合预算为 {max_turns} 回合（含工具调用）：尽量少花回合，最后一个回合直接输出最终结果，不再调用任何工具。",
        role = ctx.role,
        objective = ctx.objective,
        upstream = ctx.upstream.text,
        must_do = must_do,
        must_not = must_not
            .iter()
            .map(|l| format!("- {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
        output_format = output_format,
        acceptance = acceptance,
        max_turns = max_turns,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, role: &str, content: &str) -> Value {
        json!({
            "role": role,
            "artifact_id": id,
            "version": 1,
            "content": content,
            "cas_ref": format!("cas://sha256:hash-of-{id}"),
        })
    }

    #[test]
    fn small_artifacts_pass_through_in_full() {
        let compiled = compile_upstream(
            &[
                item("a1", "planner", "方案 A"),
                item("a2", "builder", "草稿 B"),
            ],
            PromptBudget::default(),
        );
        assert_eq!(compiled.full_count, 2);
        assert_eq!(compiled.summarized_count, 0);
        assert_eq!(compiled.ref_only_count, 0);
        assert!(compiled.truncations.is_empty());
        assert!(compiled.text.contains("### planner v1（a1）"));
        assert!(compiled.text.contains("方案 A"));
        // context_bytes = 正文字节合计（不含标头）。
        assert_eq!(compiled.context_bytes, "方案 A".len() + "草稿 B".len());
    }

    #[test]
    fn large_artifacts_become_summary_with_hash_and_ref() {
        let big = "长".repeat(4000); // 12000 字节 > 2400
        let compiled = compile_upstream(&[item("a1", "planner", &big)], PromptBudget::default());
        assert_eq!(compiled.full_count, 0);
        assert_eq!(compiled.summarized_count, 1);
        let t = &compiled.truncations[0];
        assert_eq!(t.reason, "per_artifact_budget");
        assert_eq!(t.total_bytes, big.as_bytes().len());
        assert!(t.kept_bytes > 0 && t.kept_bytes < t.total_bytes);
        assert!(compiled.text.contains("sha256=hash-of-a1"));
        assert!(compiled.text.contains("cas://sha256:hash-of-a1"));
        assert!(compiled.text.contains("per_artifact_budget"));
        // 摘要按字符截断，不劈开 UTF-8。
        assert!(compiled.text.contains('…'));
    }

    #[test]
    fn over_total_budget_degrades_to_ref_only() {
        let budget = PromptBudget {
            per_artifact_bytes: 10,
            // 每条摘要约 15 字节（5 字符 × 3 字节）；20 字节预算下首条后即降级。
            total_upstream_bytes: 20,
            summary_chars: 5,
        };
        // 第二条起累计超总预算 → 仅引用行。
        let compiled = compile_upstream(
            &[
                item("a1", "planner", "内容内容内容内容内容内容"),
                item("a2", "builder", "内容内容内容内容内容内容"),
                item("a3", "critic", "内容内容内容内容内容内容"),
            ],
            budget,
        );
        assert_eq!(compiled.ref_only_count, 2);
        assert_eq!(
            compiled
                .truncations
                .iter()
                .filter(|t| t.reason == "total_budget")
                .count(),
            2
        );
        assert!(compiled.text.contains("total_budget"));
        assert!(compiled.text.contains("### critic v1（a3）（仅引用"));
    }

    #[test]
    fn empty_upstream_is_explicit() {
        let compiled = compile_upstream(&[], PromptBudget::default());
        assert_eq!(compiled.context_bytes, 0);
        assert_eq!(compiled.text, "（无上游产物；你是首个执行者）");
    }

    #[test]
    fn char_truncate_never_splits_utf8() {
        let s = "中文字符串".repeat(100);
        let t = char_truncate(&s, 7);
        assert_eq!(t.chars().count(), 8); // 7 字符 + 省略号
        assert!(t.ends_with('…'));
    }

    #[test]
    fn template_role_prompt_has_all_sections() {
        let compiled = compile_upstream(
            &[item("a1", "code_analyzer", "分析结论")],
            PromptBudget::default(),
        );
        let ctx = PromptContext {
            objective: "修复登录 bug",
            role: "implementer",
            handoff_contract: "契约文本",
            template_id: Some(crate::builtin_team_templates::CODE_CHANGE_V1),
            budget_calls: 5,
            is_critic: false,
            upstream: &compiled,
        };
        let prompt = compile_prompt(&ctx);
        assert!(prompt.starts_with("# 角色：implementer"));
        for section in [
            "## 当前目标",
            "## 输入 Artifact",
            "## 必须完成",
            "## 禁止执行",
            "## 输出格式",
            "## 验收条件",
            "## 剩余调用预算",
        ] {
            assert!(
                prompt.contains(section),
                "角色专属 Prompt 缺段 {section}：\n{prompt}"
            );
        }
        // 模板段生效：implementer 的专属职责（唯一写者）与写角色护栏。
        assert!(
            prompt.contains("唯一写者"),
            "应包含模板角色专属职责：\n{prompt}"
        );
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("修复登录 bug"));
        assert!(prompt.contains("### code_analyzer v1（a1）"));
        // 预算来自模板声明（implementer=5）。
        assert!(prompt.contains("回合预算为 5 回合"));
    }

    #[test]
    fn dynamic_role_prompt_falls_back_to_contract() {
        let compiled = compile_upstream(&[], PromptBudget::default());
        let ctx = PromptContext {
            objective: "目标 O",
            role: "leader",
            handoff_contract: "综合上游并产出最终交付物",
            template_id: None,
            budget_calls: 0,
            is_critic: false,
            upstream: &compiled,
        };
        let prompt = compile_prompt(&ctx);
        assert!(prompt.starts_with("# 角色：leader"));
        assert!(prompt.contains("动态组队"));
        assert!(prompt.contains("综合上游并产出最终交付物"));
        assert!(prompt.contains("（无上游产物；你是首个执行者）"));
        // 未知角色默认只读（权限默认 deny → 护栏如实告知）。
        assert!(prompt.contains("禁止写入工作区文件"));
        // 未声明预算 → 缺省 12（与画像硬上限一致）。
        assert!(prompt.contains("回合预算为 12 回合"));
    }

    #[test]
    fn critic_prompt_states_readonly_verdict() {
        let compiled = compile_upstream(&[], PromptBudget::default());
        let ctx = PromptContext {
            objective: "O",
            role: "critic",
            handoff_contract: "评审",
            template_id: None,
            budget_calls: 3,
            is_critic: true,
            upstream: &compiled,
        };
        let prompt = compile_prompt(&ctx);
        assert!(prompt.contains("不得修改或覆盖上游产物"));
        assert!(prompt.contains("approved"));
    }
}
