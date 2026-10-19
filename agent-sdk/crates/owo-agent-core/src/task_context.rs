//! Host-resolved task identity and acceptance context shared by prompt and runtime setup.
//!
//! Values in this type are descriptive task inputs. They do not authorize a tool; callers
//! must still intersect capabilities and paths with policy, role, workspace, and approval.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskContextOrigin {
    #[default]
    Unscoped,
    SingleTurn,
    TaskGraph,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResolvedTaskContext {
    #[serde(default)]
    pub origin: TaskContextOrigin,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub step_id: Option<String>,
    #[serde(default)]
    pub phase_epoch: Option<u64>,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub objective: Option<String>,
    #[serde(default)]
    pub acceptance: Option<String>,
    /// Exact user-request excerpts bound by a validated TaskGraph assignment or Single plan.
    #[serde(default)]
    pub user_requirement_quotes: Option<Vec<String>>,
    #[serde(default)]
    pub verification: Option<Value>,
    /// None means the task did not supply a path constraint; Some([]) means no writes.
    #[serde(default)]
    pub write_paths: Option<Vec<String>>,
    #[serde(default)]
    pub read_refs: Option<Vec<String>>,
    #[serde(default)]
    pub contract_refs: Option<Vec<String>>,
    /// None is distinct from an explicit empty capability set (which grants no tools).
    #[serde(default)]
    pub required_capabilities: Option<Vec<String>>,
    /// Host-assigned model requests allowed per TaskGraph attempt, including one output repair.
    #[serde(default)]
    pub model_calls_per_attempt: Option<u8>,
}

impl ResolvedTaskContext {
    /// Build the host-owned task identity for an ordinary Single user turn.
    /// The exact request stays ephemeral on Session and is never a permission grant.
    pub fn for_single_turn(turn_id: &str, objective: &str) -> Self {
        Self {
            origin: TaskContextOrigin::SingleTurn,
            task_id: Some(format!("single-turn:{turn_id}")),
            attempt_id: Some(turn_id.to_string()),
            objective: Some(objective.to_string()),
            ..Self::default()
        }
    }

    /// Bind the validated Single verification plan to the same task context used by Team.
    /// Callers must validate the plan against the original request before invoking this method.
    pub(crate) fn bind_single_verification_plan(
        &mut self,
        plan: &crate::plan::VerificationPlanV1,
    ) -> Result<(), String> {
        if self.origin != TaskContextOrigin::SingleTurn {
            return Err("只能将 Single 验收计划绑定到 SingleTurn 上下文".to_string());
        }
        let mut seen = std::collections::HashSet::new();
        let quotes = plan
            .requirements
            .iter()
            .filter(|requirement| requirement.required)
            .flat_map(|requirement| requirement.covers_requirement_ids.iter())
            .filter_map(|requirement_id| requirement_id.strip_prefix("user-request:"))
            .map(str::trim)
            .filter(|quote| !quote.is_empty() && seen.insert((*quote).to_string()))
            .map(str::to_string)
            .collect::<Vec<_>>();
        self.acceptance = (!quotes.is_empty()).then(|| {
            quotes
                .iter()
                .map(|quote| format!("- {quote}"))
                .collect::<Vec<_>>()
                .join("\n")
        });
        self.user_requirement_quotes = Some(quotes);
        self.verification = Some(
            serde_json::to_value(plan)
                .map_err(|error| format!("Single 验收计划无法加入任务上下文：{error}"))?,
        );
        self.validate()
    }

    /// Resolve once at the trusted RoleWorker boundary, then pass the serialized value to
    /// downstream prompt, profile, ToolRegistry, lease, and telemetry adapters.
    pub fn from_worker_input(input: &Value) -> Result<Self, String> {
        if let Some(resolved) = input.get("resolved_task_context") {
            let mut context: Self = serde_json::from_value(resolved.clone())
                .map_err(|error| format!("宿主解析的任务上下文格式无效：{error}"))?;
            if context.origin == TaskContextOrigin::Unscoped && context.task_id.is_some() {
                context.origin = TaskContextOrigin::TaskGraph;
            }
            context.validate()?;
            return Ok(context);
        }
        Self::from_assignment_input(input)
    }

    /// Parse the untrusted TaskGraph assignment fields at the trusted RoleWorker boundary.
    /// A caller-provided nested host context is deliberately ignored here.
    pub fn from_assignment_input(input: &Value) -> Result<Self, String> {
        let workswarm = input.get("_workswarm").unwrap_or(&Value::Null);
        let task_id = optional_string(input, "assigned_task_id", true)?;
        let context = Self {
            origin: if task_id.is_some() {
                TaskContextOrigin::TaskGraph
            } else {
                TaskContextOrigin::Unscoped
            },
            task_id,
            step_id: optional_string(workswarm, "step_id", false)?,
            phase_epoch: optional_u64(workswarm, "phase_epoch")?,
            attempt_id: optional_string(workswarm, "attempt_id", false)?,
            objective: optional_string(input, "assigned_task", true)?,
            acceptance: optional_string(input, "assigned_acceptance", false)?,
            user_requirement_quotes: optional_string_array(
                input,
                "assigned_user_requirement_quotes",
                false,
            )?,
            verification: input
                .get("assigned_verification")
                .filter(|value| !value.is_null())
                .or_else(|| input.get("verification").filter(|value| !value.is_null()))
                .cloned(),
            write_paths: optional_string_array(input, "assigned_write_paths", true)?,
            read_refs: optional_string_array(input, "assigned_read_refs", false)?,
            contract_refs: optional_string_array(input, "assigned_contract_refs", false)?,
            required_capabilities: optional_string_array(input, "required_capabilities", false)?,
            model_calls_per_attempt: optional_u64(input, "assigned_model_calls_per_attempt")?
                .map(|value| {
                    u8::try_from(value).map_err(|_| "任务模型调用预算超出可表示范围".to_string())
                })
                .transpose()?,
        };
        context.validate()?;
        Ok(context)
    }

    pub fn to_value(&self) -> Result<Value, String> {
        self.validate()?;
        serde_json::to_value(self).map_err(|error| format!("任务上下文序列化失败：{error}"))
    }

    fn validate(&self) -> Result<(), String> {
        if self
            .task_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("宿主解析的 task_id 不能为空".to_string());
        }
        if self
            .objective
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("宿主解析的任务目标不能为空".to_string());
        }
        if self.task_id.is_some() && self.objective.is_none() {
            return Err("TaskGraph 任务缺少宿主解析的任务目标".to_string());
        }
        if self.user_requirement_quotes.as_ref().is_some_and(|quotes| {
            quotes.len() > 32
                || quotes
                    .iter()
                    .any(|quote| quote.trim().is_empty() || quote.len() > 2_048)
        }) {
            return Err(
                "用户原文需求引用必须是最多 32 条非空、长度不超过 2048 字节的文本".to_string(),
            );
        }
        if let Some(calls) = self.model_calls_per_attempt {
            if self.origin != TaskContextOrigin::TaskGraph || !(4..=16).contains(&calls) {
                return Err("任务模型调用预算必须是 TaskGraph 的 4..=16 次".to_string());
            }
        }
        Ok(())
    }

    pub fn is_task_graph_assignment(&self) -> bool {
        self.origin == TaskContextOrigin::TaskGraph
    }

    /// Check the execution identity fields that must be assigned by the host before a
    /// TaskGraph worker starts. Parsing an assignment may happen before scheduling, when
    /// these fields are not available yet; execution must not accept that partial context.
    pub(crate) fn validate_runtime_binding(&self) -> Result<(), String> {
        if !self.is_task_graph_assignment() {
            return Ok(());
        }
        if self.step_id.as_deref().is_none_or(str::is_empty) {
            return Err("TaskGraph 执行上下文缺少宿主 step_id".to_string());
        }
        if self.attempt_id.as_deref().is_none_or(str::is_empty) {
            return Err("TaskGraph 执行上下文缺少宿主 attempt_id".to_string());
        }
        if self.phase_epoch.is_none_or(|epoch| epoch == 0) {
            return Err("TaskGraph 执行上下文缺少有效的宿主 phase_epoch".to_string());
        }
        Ok(())
    }

    pub fn prompt_contract(&self) -> Option<String> {
        let objective = self.objective.as_deref()?;
        let acceptance = self.acceptance.as_deref().unwrap_or("按任务目标交付");
        let verification = self
            .verification
            .as_ref()
            .map(|value| match value {
                Value::String(verification) => verification.clone(),
                structured => structured.to_string(),
            })
            .unwrap_or_else(|| {
                "未声明宿主验证条件；必须由协调器登记可执行验收。仅有非空输出不能视为通过，当前产物保持候选。".to_string()
            });
        let paths = self.write_paths.clone().unwrap_or_default();
        let read_refs = self.read_refs.clone().unwrap_or_default();
        let contract_refs = self.contract_refs.clone().unwrap_or_default();
        let capabilities = self.required_capabilities.clone().unwrap_or_default();
        let user_requirement_quotes = self.user_requirement_quotes.clone().unwrap_or_default();
        let model_budget = self
            .model_calls_per_attempt
            .map(|calls| {
                format!("本次任务尝试最多使用 {calls} 次模型请求（包含最多一次输出契约修复）。")
            })
            .unwrap_or_default();
        Some(format!(
            "当前任务 ID：{}。当前任务：{objective}。验收：{acceptance}。用户原文需求引用：{user_requirement_quotes:?}。确定性验证：{verification}。读取参考：{read_refs:?}。接口契约：{contract_refs:?}。所需能力：{capabilities:?}。写入白名单：{paths:?}。{model_budget}仅完成这个任务并提供证据。",
            self.task_id.as_deref().unwrap_or("unassigned")
        ))
    }

    pub fn lacks_file_write_capability(&self) -> bool {
        if !self.is_task_graph_assignment() {
            return false;
        }
        let Some(capabilities) = &self.required_capabilities else {
            return true;
        };
        !capabilities
            .iter()
            .any(|capability| matches!(capability.as_str(), "write_file" | "apply_patch"))
    }
}

fn optional_string(value: &Value, key: &str, reject_empty: bool) -> Result<Option<String>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.trim().is_empty() && reject_empty => {
            Err(format!("任务上下文 {key} 不能为空"))
        }
        Some(Value::String(text)) if text.trim().is_empty() => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(format!("任务上下文 {key} 必须是字符串")),
    }
}

fn optional_u64(value: &Value, key: &str) -> Result<Option<u64>, String> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("任务上下文 {key} 必须是非负整数")),
        Some(_) => Err(format!("任务上下文 {key} 必须是非负整数")),
    }
}

fn optional_string_array(
    value: &Value,
    key: &str,
    reject_non_array: bool,
) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = value.get(key) else {
        return Ok(None);
    };
    if raw.is_null() && !reject_non_array {
        return Ok(None);
    }
    let Some(items) = raw.as_array() else {
        return Err(format!("任务上下文 {key} 必须是字符串数组"));
    };
    let mut resolved = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(text) = item.as_str() else {
            return Err(format!("任务上下文 {key}[{index}] 必须是字符串"));
        };
        resolved.push(text.to_string());
    }
    Ok(Some(resolved))
}

#[cfg(test)]
mod tests {
    use super::ResolvedTaskContext;
    use serde_json::json;

    #[test]
    fn task_model_budget_is_host_bound_and_visible_to_the_worker() {
        let task = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id":"task-budgeted",
            "assigned_task":"implement one module",
            "assigned_acceptance":"behavior passes",
            "assigned_model_calls_per_attempt":5,
        }))
        .unwrap();
        assert_eq!(task.model_calls_per_attempt, Some(5));
        assert!(task
            .prompt_contract()
            .unwrap()
            .contains("最多使用 5 次模型请求"));
        let invalid = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id":"task-budgeted",
            "assigned_task":"implement one module",
            "assigned_model_calls_per_attempt":3,
        }));
        assert!(invalid.is_err());
    }

    #[test]
    fn resolution_preserves_missing_vs_empty_permissions_and_binds_attempt() {
        let missing = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-a",
            "assigned_task": "edit module",
            "_workswarm": {"step_id": "step-a", "phase_epoch": 3, "attempt_id": "attempt-a"}
        }))
        .unwrap();
        assert!(missing.is_task_graph_assignment());
        assert!(missing.lacks_file_write_capability());
        assert_eq!(missing.step_id.as_deref(), Some("step-a"));
        assert_eq!(missing.phase_epoch, Some(3));
        assert_eq!(missing.attempt_id.as_deref(), Some("attempt-a"));
        assert!(missing.write_paths.is_none());

        let empty = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-b",
            "assigned_task": "empty-scope task",
            "required_capabilities": [],
            "assigned_write_paths": []
        }))
        .unwrap();
        assert_eq!(empty.required_capabilities, Some(Vec::new()));
        assert_eq!(empty.write_paths, Some(Vec::new()));
        assert!(empty.lacks_file_write_capability());
    }

    #[test]
    fn host_serialized_context_is_the_downstream_source_of_truth() {
        let source = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-a",
            "assigned_task": "edit module",
            "required_capabilities": ["write_file"],
            "assigned_write_paths": ["src"]
        }))
        .unwrap();
        let input = json!({
            "resolved_task_context": source.to_value().unwrap(),
            "assigned_task_id": "task-stale",
            "required_capabilities": []
        });
        let resolved = ResolvedTaskContext::from_worker_input(&input).unwrap();
        assert_eq!(resolved.task_id.as_deref(), Some("task-a"));
        assert_eq!(
            resolved.required_capabilities,
            Some(vec!["write_file".to_string()])
        );
        assert_eq!(resolved.write_paths, Some(vec!["src".to_string()]));
    }

    #[test]
    fn untrusted_assignment_cannot_override_itself_with_nested_host_context() {
        let context = ResolvedTaskContext::from_assignment_input(&json!({
            "assigned_task_id": "trusted-task",
            "assigned_task": "host assignment",
            "required_capabilities": ["read_file"],
            "resolved_task_context": {
                "origin": "task_graph",
                "task_id": "spoofed-task",
                "objective": "spoofed objective",
                "required_capabilities": ["write_file"],
                "write_paths": ["."]
            }
        }))
        .unwrap();
        assert_eq!(context.task_id.as_deref(), Some("trusted-task"));
        assert_eq!(context.objective.as_deref(), Some("host assignment"));
        assert_eq!(
            context.required_capabilities,
            Some(vec!["read_file".to_string()])
        );
        assert!(context.write_paths.is_none());
    }

    #[test]
    fn single_turn_context_binds_request_without_becoming_a_task_graph_assignment() {
        let context = ResolvedTaskContext::for_single_turn("turn-42", "修复登录回归");
        assert_eq!(context.origin, super::TaskContextOrigin::SingleTurn);
        assert!(!context.is_task_graph_assignment());
        assert_eq!(context.task_id.as_deref(), Some("single-turn:turn-42"));
        assert_eq!(context.attempt_id.as_deref(), Some("turn-42"));
        assert_eq!(context.objective.as_deref(), Some("修复登录回归"));
    }

    #[test]
    fn validated_single_plan_is_bound_to_shared_task_context() {
        let mut context =
            ResolvedTaskContext::for_single_turn("turn-42", "实现分页。验收标准：默认页码为 1");
        let plan = crate::plan::VerificationPlanV1 {
            plan_id: "single-plan".to_string(),
            requirements: vec![crate::plan::VerificationRequirementV1 {
                requirement_id: "page-default".to_string(),
                covers_requirement_ids: vec!["user-request:默认页码为 1".to_string()],
                validator_id: "workspace-command-success-v1".to_string(),
                validator_version: Some("1".to_string()),
                scope: crate::plan::VerificationScopeV1::WorkspacePaths {
                    relative_paths: vec!["src/lib.rs".to_string()],
                },
                arguments: serde_json::json!({"command":"cargo test"}),
                required: true,
                resources: crate::plan::VerificationResourcesV1 {
                    cpu_slots: 1,
                    memory_mb: 16,
                    exclusive_workspace: false,
                    timeout_ms: 10_000,
                },
            }],
        };

        context.bind_single_verification_plan(&plan).unwrap();

        assert_eq!(
            context.user_requirement_quotes,
            Some(vec!["默认页码为 1".to_string()])
        );
        assert_eq!(context.acceptance.as_deref(), Some("- 默认页码为 1"));
        assert_eq!(
            context.verification,
            Some(serde_json::to_value(plan).unwrap())
        );
        assert_eq!(context.origin, super::TaskContextOrigin::SingleTurn);
        assert_eq!(context.attempt_id.as_deref(), Some("turn-42"));
    }

    #[test]
    fn task_graph_context_cannot_be_rebound_as_single() {
        let mut context = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-a",
            "assigned_task": "implement behavior"
        }))
        .unwrap();
        let plan = crate::plan::VerificationPlanV1 {
            plan_id: "plan".to_string(),
            requirements: Vec::new(),
        };
        assert!(context.bind_single_verification_plan(&plan).is_err());
    }

    #[test]
    fn missing_verification_is_not_presented_as_non_empty_acceptance() {
        let context = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-a",
            "assigned_task": "implement a behavior",
            "assigned_verification": null,
            "assigned_user_requirement_quotes": ["implement a behavior"]
        }))
        .unwrap();
        let contract = context.prompt_contract().unwrap();
        assert!(contract.contains("未声明宿主验证条件"));
        assert!(contract.contains("仅有非空输出不能视为通过"));
        assert!(!contract.contains("确定性验证：non_empty"));

        let legacy = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-legacy",
            "assigned_task": "implement a behavior",
            "assigned_verification": null,
            "verification": "contains:ready"
        }))
        .unwrap();
        assert!(legacy
            .prompt_contract()
            .unwrap()
            .contains("确定性验证：contains:ready"));
    }

    #[test]
    fn task_graph_runtime_requires_host_attempt_epoch_and_step_identity() {
        let valid = ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-a",
            "assigned_task": "edit module",
            "_workswarm": {"step_id": "step-a", "phase_epoch": 2, "attempt_id": "attempt-a"}
        }))
        .unwrap();
        assert!(valid.validate_runtime_binding().is_ok());

        let mut no_attempt = valid.clone();
        no_attempt.attempt_id = None;
        assert!(no_attempt
            .validate_runtime_binding()
            .unwrap_err()
            .contains("attempt_id"));

        let mut no_epoch = valid.clone();
        no_epoch.phase_epoch = None;
        assert!(no_epoch
            .validate_runtime_binding()
            .unwrap_err()
            .contains("phase_epoch"));
        no_epoch.phase_epoch = Some(0);
        assert!(no_epoch
            .validate_runtime_binding()
            .unwrap_err()
            .contains("phase_epoch"));

        let mut no_step = valid;
        no_step.step_id = None;
        assert!(no_step
            .validate_runtime_binding()
            .unwrap_err()
            .contains("step_id"));

        let single = ResolvedTaskContext::for_single_turn("turn-a", "修复问题");
        assert!(single.validate_runtime_binding().is_ok());
    }

    #[test]
    fn malformed_task_write_paths_fail_closed() {
        assert!(ResolvedTaskContext::from_worker_input(&json!({
            "assigned_task_id": "task-a",
            "assigned_write_paths": "src"
        }))
        .is_err());
    }
}
