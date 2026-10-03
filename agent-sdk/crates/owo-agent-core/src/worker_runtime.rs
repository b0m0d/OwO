//! Shared executor for task-scoped worker turns across Single, Team, and evaluation.
//!
//! Callers resolve the workspace, policy, tool surface, budget, and reviewer role before
//! construction. This runtime owns the Agent loop, model-bound task session, cancellation,
//! output-contract repair, and request accounting; it never grants capabilities itself.

use crate::agent::{Agent, AgentConfig, TurnEvent};
use crate::contract_worker::enforce_worker_output_contract_with_model;
use crate::gateway::{ModelProvider, TokenUsage};
use crate::permissions::{Approver, Policy};
use crate::session::{Session, SessionStore};
use crate::subagent::MAX_SUBAGENT_DEPTH;
use crate::tools::ToolRegistry;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct WorkerRuntime<'a> {
    pub provider: Arc<dyn ModelProvider>,
    pub approver: &'a dyn Approver,
    pub abort: &'a AtomicBool,
    pub depth: usize,
    pub model: String,
    pub workspace: PathBuf,
    pub registry: ToolRegistry,
    pub policy: Policy,
    pub config: AgentConfig,
    pub system_prompt: Option<String>,
    pub is_critic: bool,
    pub event_sink: Option<Arc<dyn Fn(&TurnEvent) + Send + Sync + 'a>>,
    pub session_store: Option<Arc<dyn SessionStore>>,
    pub worker_session_id: Option<String>,
    pub parent_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerRuntimeReport {
    pub output: String,
    pub duration_ms: u64,
    pub steps: usize,
    pub model_calls: u32,
    pub usage: TokenUsage,
    pub usage_known: bool,
    pub output_repairs: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerRuntimeError {
    pub message: String,
    pub duration_ms: u64,
    pub steps: usize,
    pub model_calls: u32,
    pub usage: TokenUsage,
    pub usage_known: bool,
    pub output_repairs: u32,
}

impl From<String> for WorkerRuntimeError {
    fn from(message: String) -> Self {
        Self {
            message,
            duration_ms: 0,
            steps: 0,
            model_calls: 0,
            usage: TokenUsage::default(),
            usage_known: false,
            output_repairs: 0,
        }
    }
}

impl WorkerRuntime<'_> {
    pub async fn run_report(self, prompt: &str) -> Result<WorkerRuntimeReport, WorkerRuntimeError> {
        let started = Instant::now();
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Err(format!("子代理深度超限（最多 {MAX_SUBAGENT_DEPTH} 层）").into());
        }
        let configured_turn_cap = self.config.max_turns;
        let agent = Agent::new(
            Arc::clone(&self.provider),
            self.registry,
            self.policy,
            self.config,
        );
        let mut session = load_worker_session(
            &self.workspace,
            &self.model,
            self.system_prompt.clone(),
            &self.session_store,
            &self.worker_session_id,
            &self.parent_session_id,
        )?;
        if let Some(session_id) = &self.worker_session_id {
            session.id = session_id.clone();
        }
        session.parent_id = self.parent_session_id.clone();
        if let (Some(store), Some(_)) = (&self.session_store, &self.worker_session_id) {
            store.save(&session).map_err(|error| format!("Worker 会话初始化保存失败：{error}"))?;
        }
        let event_sink = self.event_sink.clone();
        let mut on_event = move |event: &TurnEvent| {
            if let Some(sink) = &event_sink {
                sink(event);
            }
        };
        let outcome = agent
            .run_turn(&mut session, prompt, self.approver, self.abort, &mut on_event)
            .await;
        if let (Some(store), Some(_)) = (&self.session_store, &self.worker_session_id) {
            store.save(&session).map_err(|error| format!("Worker 会话执行后保存失败：{error}"))?;
        }
        let outcome = outcome.map_err(|error| format!("子代理执行失败：{error}"))?;
        let model_calls = outcome
            .events
            .iter()
            .filter(|event| matches!(event, TurnEvent::ModelCall))
            .count() as u32;
        if outcome.reached_model_turn_limit {
            return Err(WorkerRuntimeError {
                message: format!(
                    "worker_turn_budget_exhausted:模型在 {} 轮预算内未自行给出最终答复",
                    configured_turn_cap
                ),
                duration_ms: started.elapsed().as_millis() as u64,
                steps: outcome.steps,
                model_calls,
                usage: outcome.usage,
                usage_known: outcome.usage_known,
                output_repairs: 0,
            });
        }
        let text = outcome
            .final_text
            .unwrap_or_else(|| format!("（子代理无最终文本，共 {} 步）", outcome.steps));
        let enforced = match enforce_worker_output_contract_with_model(
            &self.provider,
            Some(&self.model),
            &text,
            self.is_critic,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                let repair_usage_known = error.repairs == 0 || error.usage.is_some();
                let mut usage = outcome.usage;
                if let Some(repair_usage) = error.usage {
                    usage.add(&repair_usage);
                }
                return Err(WorkerRuntimeError {
                    message: error.message,
                    duration_ms: started.elapsed().as_millis() as u64,
                    steps: outcome.steps,
                    model_calls: model_calls.saturating_add(error.repairs),
                    usage,
                    usage_known: outcome.usage_known && repair_usage_known,
                    output_repairs: error.repairs,
                });
            }
        };
        let mut usage = outcome.usage;
        let mut usage_known = outcome.usage_known;
        if enforced.repairs > 0 {
            if let Some(repair_usage) = enforced.usage {
                usage.add(&repair_usage);
            } else {
                usage_known = false;
            }
        }
        Ok(WorkerRuntimeReport {
            output: enforced.text,
            duration_ms: started.elapsed().as_millis() as u64,
            steps: outcome.steps,
            model_calls: model_calls.saturating_add(enforced.repairs),
            usage,
            usage_known,
            output_repairs: enforced.repairs,
        })
    }
}

fn load_worker_session(
    workspace: &Path,
    model: &str,
    system_prompt: Option<String>,
    session_store: &Option<Arc<dyn SessionStore>>,
    worker_session_id: &Option<String>,
    parent_session_id: &Option<String>,
) -> Result<Session, WorkerRuntimeError> {
    let mut session = if let (Some(store), Some(session_id)) = (session_store, worker_session_id) {
        if store.exists(session_id).map_err(|error| format!("Worker 会话索引查询失败：{error}"))? {
            let loaded = store
                .load(session_id)
                .map_err(|error| format!("Worker 会话 {session_id} 恢复失败：{error}"))?;
            if loaded.workspace != workspace
                || loaded.id != *session_id
                || loaded.parent_id != *parent_session_id
            {
                return Err(format!("Worker 会话 {session_id} 的工作区或父会话归属不匹配").into());
            }
            loaded.with_model_override(Some(model.to_string()))
        } else {
            Session::new(workspace, model.to_string(), system_prompt)
                .with_model_override(Some(model.to_string()))
        }
    } else {
        Session::new(workspace, model.to_string(), system_prompt)
            .with_model_override(Some(model.to_string()))
    };
    session.parent_id = parent_session_id.clone();
    Ok(session)
}
