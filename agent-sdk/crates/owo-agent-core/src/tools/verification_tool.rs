//! Host-bound verification plan tool and coverage validation for Single turns.

use super::{Tool, ToolContext, ToolSpec};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Host-bound acceptance requirements for an ordinary Single task.
pub(super) struct SingleVerificationPlanTool;

pub(crate) fn validate_single_verification_plan(
    plan: &crate::plan::VerificationPlanV1,
) -> Result<(), String> {
    plan.validate()?;
    if plan.plan_id.len() > 128 || plan.requirements.len() > 32 {
        return Err("VerificationPlan 超过宿主的 plan_id/requirement 数量上限".to_string());
    }
    for requirement in &plan.requirements {
        let manual_acceptance = requirement.validator_id
            == crate::completion::SINGLE_MANUAL_ACCEPTANCE_VALIDATOR_ID
            && matches!(&requirement.scope, crate::plan::VerificationScopeV1::Manual);
        if requirement.covers_requirement_ids.is_empty()
            || requirement.covers_requirement_ids.iter().any(|id| {
                id.strip_prefix("user-request:")
                    .is_none_or(|quote| quote.trim().is_empty())
            })
        {
            return Err(format!(
                "requirement {} 的 covers_requirement_ids 必须使用 user-request:<用户原文验收片段>",
                requirement.requirement_id
            ));
        }
        if requirement.validator_version.as_deref() != Some("1") {
            return Err(format!(
                "requirement {} 必须固定宿主 validator version 1",
                requirement.requirement_id
            ));
        }
        if manual_acceptance {
            if requirement.arguments != json!({}) {
                return Err(format!(
                    "requirement {} 的人工验收 arguments 必须为空对象",
                    requirement.requirement_id
                ));
            }
            continue;
        }
        if !crate::verification::is_registered_workspace_validator(&requirement.validator_id) {
            return Err(format!(
                "requirement {} 使用了未注册的宿主 validator/version",
                requirement.requirement_id
            ));
        }
        let crate::plan::VerificationScopeV1::WorkspacePaths { relative_paths } =
            &requirement.scope
        else {
            return Err(format!(
                "requirement {} 必须绑定 WorkspacePaths 或明确声明人工验收",
                requirement.requirement_id
            ));
        };
        if relative_paths.len() > 16
            || !crate::verification::workspace_validator_arguments_supported(
                &requirement.validator_id,
                &requirement.arguments,
            )
        {
            return Err(format!(
                "requirement {} 的路径数量或 validator 参数不符合宿主注册契约",
                requirement.requirement_id
            ));
        }
        let resources = &requirement.resources;
        if resources.cpu_slots != 1
            || !(8..=128).contains(&resources.memory_mb)
            || resources.exclusive_workspace
            || !(1..=30_000).contains(&resources.timeout_ms)
        {
            return Err(format!(
                "requirement {} 的资源声明超出宿主验证器预算",
                requirement.requirement_id
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_single_request_coverage(
    plan: &crate::plan::VerificationPlanV1,
    request: &str,
) -> Result<(), String> {
    let mut quotes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for requirement in plan
        .requirements
        .iter()
        .filter(|requirement| requirement.required)
    {
        for coverage_id in &requirement.covers_requirement_ids {
            let quote = coverage_id
                .strip_prefix("user-request:")
                .map(str::trim)
                .filter(|quote| !quote.is_empty())
                .ok_or_else(|| format!("验收点 {coverage_id} 不是有效的用户原文引用"))?;
            if seen.insert(quote.to_string()) {
                quotes.push(quote.to_string());
            }
        }
    }
    crate::request_requirements::validate_exact_user_request_quotes(&quotes, request)?;
    crate::request_requirements::validate_plan_covers_explicit_acceptance(plan, request)?;
    Ok(())
}

#[async_trait]
impl Tool for SingleVerificationPlanTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "verification_plan".into(),
            description: "开始任何工作区写入前，先登记本次任务的宿主验收要求。计划首次写入后不可替换。每个必需 covers_requirement_ids 都必须写成 user-request:<用户原文中的精确验收片段>，宿主会核对它确实出现在本回合输入中。源码路径优先声明 workspace-command-success-v1；确实没有可运行自动验收时，才可声明 single-human-acceptance-v1 + manual scope，宿主会展示当前候选快照并等待用户明确验收。计划本身不是通过证据。resources 四个字段按 schema 显式填写；人工验收不消耗该资源配额。".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "plan": {
                        "type": "object",
                        "properties": {
                            "plan_id": {"type": "string"},
                            "requirements": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "requirement_id": {"type": "string"},
                                        "covers_requirement_ids": {
                                            "type": "array",
                                            "items": {"type": "string", "description": "user-request:<用户原文中的精确验收片段>"},
                                            "minItems": 1
                                        },
                                        "validator_id": {"type": "string", "enum": ["workspace-file-exists-v1", "workspace-file-non-empty-v1", "workspace-file-contains-v1", "workspace-json-field-equals-v1", "workspace-command-success-v1", "single-human-acceptance-v1"]},
                                        "validator_version": {"type": "string", "enum": ["1"]},
                                        "scope": {
                                            "oneOf": [
                                                {
                                                    "type": "object",
                                                    "properties": {
                                                        "kind": {"type": "string", "enum": ["workspace_paths"]},
                                                        "relative_paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 16}
                                                    },
                                                    "required": ["kind", "relative_paths"]
                                                },
                                                {
                                                    "type": "object",
                                                    "properties": {"kind": {"type": "string", "enum": ["manual"]}},
                                                    "required": ["kind"]
                                                }
                                            ]
                                        },
                                        "arguments": {"type": "object"},
                                        "required": {"type": "boolean"},
                                        "resources": {
                                            "type": "object",
                                            "properties": {
                                                "cpu_slots": {"type": "integer", "enum": [1]},
                                                "memory_mb": {"type": "integer", "minimum": 8, "maximum": 128, "default": 16},
                                                "exclusive_workspace": {"type": "boolean", "enum": [false]},
                                                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 30000, "default": 30000}
                                            },
                                            "required": ["cpu_slots", "memory_mb", "exclusive_workspace", "timeout_ms"]
                                        }
                                    },
                                    "required": ["requirement_id", "covers_requirement_ids", "validator_id", "validator_version", "scope", "arguments", "required", "resources"]
                                }
                            }
                        },
                        "required": ["plan_id", "requirements"]
                    }
                },
                "required": ["plan"]
            }),
            effect: None,
        }
    }

    async fn run(&self, ctx: &mut ToolContext<'_>, args: Value) -> Result<Value, String> {
        let plan: crate::plan::VerificationPlanV1 =
            serde_json::from_value(args.get("plan").cloned().ok_or("缺少 plan 对象")?)
                .map_err(|error| format!("VerificationPlan 结构非法：{error}"))?;
        validate_single_verification_plan(&plan)?;
        let task_context = ctx
            .session
            .active_task_context
            .as_ref()
            .ok_or("当前 Agent 回合没有宿主解析的任务上下文")?;
        let request = task_context
            .objective
            .as_deref()
            .ok_or("当前 Agent 回合没有可核对的原始用户输入")?;
        let input_sha256 = crate::CasStore::hash_of(request.as_bytes());
        let turn_id = task_context
            .attempt_id
            .clone()
            .ok_or("当前 Agent 回合没有绑定的 attempt_id")?;
        validate_single_request_coverage(&plan, request)?;
        let has_current_turn_writes = ctx
            .session
            .execution_receipts
            .iter()
            .any(|receipt| receipt.turn_id == turn_id && receipt.status != "reverted");
        if has_current_turn_writes
            && (ctx.session.single_verification_plan_turn_id.as_deref() != Some(turn_id.as_str())
                || ctx.session.single_verification_plan.as_ref() != Some(&plan))
        {
            return Err(
                "VerificationPlan 必须在首次工作区写入前登记，且本回合登记后不可替换".to_string(),
            );
        }
        if ctx.session.single_verification_plan_turn_id.as_deref() == Some(turn_id.as_str())
            && ctx
                .session
                .single_verification_plan
                .as_ref()
                .is_some_and(|registered| registered != &plan)
        {
            return Err(
                "本回合 VerificationPlan 已冻结，不能在看到验证结果后降低验收要求".to_string(),
            );
        }
        let mut resolved_context = task_context.clone();
        resolved_context.bind_single_verification_plan(&plan)?;
        ctx.session.single_verification_plan = Some(plan.clone());
        ctx.session.single_verification_plan_input_sha256 = Some(input_sha256);
        ctx.session.single_verification_plan_turn_id = Some(turn_id);
        ctx.session.active_task_context = Some(resolved_context);
        Ok(json!({"plan": plan, "status": "registered_pending_host_validation"}))
    }
}
