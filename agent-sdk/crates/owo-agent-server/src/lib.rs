#![recursion_limit = "1024"]

//! OwO Agent SDK HTTP 服务（M1 + v0.4）：session/turn/permission/diff/revert/abort + SSE，
//! 以及 v0.4 接口：context.snapshot / perception.subscribe / learn.* / skill.verify /
//! proactive.suggest / whitelist.manage。
//!
//! 第四轮核心模块 HTTP/UI 集成：notes_api / plugin_market_api / workflow_api / goal_api /
//! sse（云端任务进度 SSE 集线器）四个模块路由并入 build_router；cloud_task_submit 的
//! ProgressSink 接 sse::sink(task_id) 使 /cloud/tasks/{id}/events 收到真实进度。
//!
//! 第五轮（R5）：workflow_api/goal_api 扩展（真实执行后端 + 人审 + run SSE + Agent Worker，
//! 子模块 workflow_backend/agent_worker 在各自模块内声明）；plugin_market_api 扩展
//! （远端市场 refresh/install-remote）+ team_api（团队技能包共享）+ market_client；
//! eval_gate（eval 护栏）+ observability_api（/metrics 可观测性）；memory_graph_api
//! （记忆图谱）+ intent_api（统一命令入口）。
//!
//! 第七轮（R7）：本地 API 安全边界（X03）——bearer token 鉴权（auth_token.rs，token 文件
//! 用户级 ACL + 发布桌面 `/auth/token` 进程配对引导）、CORS 显式 origin 白名单（webview + localhost）、
//! 全局/每会话/敏感端点双令牌桶限流（rate_limit.rs，429 + Retry-After + 审计）；SSE
//! 资源型路径（/…/events）因 EventSource 无法携带自定义头而豁免鉴权（只读遥测）。
//!
//! 第八轮（R8）：存储运维（backup.rs：/storage/backup|restore|export|clear，恢复前自动备份、
//! zip-slip 防护、清空二次确认 + 完整性校验）与服务端韧性（shutdown.rs：全局并发 turn 上限、
//! 优雅关闭 POST /server/shutdown + GET /server/status、CLI serve 强杀恢复 pid 文件）。

/// V1 四期（第三路）：Artifact 评审闭环路由（review / history）。
pub mod artifact_review_api;
mod assist_api;
mod audit_api;
mod auth_token;

// §12：flush_audit 移入 audit_api.rs，根部 re-export 保全 CLI 对
// `owo_agent_server::flush_audit` 的既有依赖（外部契约不变）。
pub use audit_api::flush_audit;
pub mod backup;
pub mod capabilities;
mod cloud_api;
mod computer_api;
mod desktop_api;
mod desktop_world_api;
pub mod error_codes;
mod eval_api;
mod eval_gate;
/// 可靠事件流集线器（§3.1：公开给契约测试发布续传事件用；路由在 build_router 挂载）。
pub mod event_stream;
mod fleet_api;
mod goal_api;
mod idempotency;
mod intent_api;
mod learn_api;
mod locate_api;
pub mod logging;
mod market_client;
mod mcp_api;
mod memory_api;
mod memory_graph_api;
mod notes_api;
mod observability_api;
mod ops_api;
mod perception_api;
mod plugin_api;
mod plugin_market_api;
pub mod product_eval_api;
mod project_api;
mod rate_limit;
/// R3（§8.1）：安全请求 ledger（六字段白名单，/diagnostics/requests 消费）。
/// `pub` 仅为契约测试可用 `reset_for_test` 取得干净窗口（同 event_stream 先例）。
pub mod request_ledger_api;
mod schemas_api;
mod session_api;
mod settings_api;
pub mod shutdown;
mod skills_api;
mod slo;
mod sse;
mod subagent_api;
mod team_api;
mod team_template_catalog_api;
mod traces_api;
mod turn_api;
mod usage;
mod whitelist_api;
mod workflow_api;
mod workswarm_api;

/// R1 测试装配面：集成测试需要用独立 TempDir 数据根构造 DesktopWorld 运行态
/// （进程级单例仅服务生产 build_router；导出构造函数不新增 AppState 字段、不改路由面）。
pub use desktop_world_api::{router_with_hub, DesktopWorldHub};
/// A2 接线面：goal 显式 `fleet_node` 目标复用进程级控制面——绑定任务经真实
/// `/fleet/*` 节点协议被已注册节点领取与回传，不伪造远程执行。
pub use fleet_api::{fleet_hub, router_with_hub as fleet_router_with_hub, FleetHub};

/// 协议约束：新模块（notes_api 等）一律写全限定名 `owo_agent_server::AppState`，
/// 以便测试以 `#[path = "../src/xxx.rs"] mod` 独立编译；此处建立 crate 自别名，
/// 使该路径在库内（含子模块）同样可解析。
extern crate self as owo_agent_server;

use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::automation::AutomationStore;
use owo_agent_core::learn::{
    ActionType, LearnPipeline, LearnState, ProactiveEngine, RecordedAction, SemanticAnchor,
};
use owo_agent_core::perception::SituationStore;
use owo_agent_core::permissions::{Decision, PermissionRequest};
use owo_agent_core::session::{Session, SessionStore};
use owo_agent_core::whitelist::Whitelist;
use owo_agent_core::Agent;
use owo_agent_protocol::{BuildInfo, HealthResponse};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::services::ServeDir;

/// §5.4 待审批请求：request_id → (oneshot，PermissionRequest 副本)。
/// 副本用于响应时按 scope 生成临时授权（Grant）。
pub type PendingApproval = (tokio::sync::oneshot::Sender<Decision>, PermissionRequest);

pub struct AppState {
    pub agent: Arc<Agent>,
    pub store: Arc<dyn SessionStore>,
    pub sessions: Arc<Mutex<HashMap<String, Session>>>,
    pub pending_approvals: Arc<Mutex<HashMap<String, PendingApproval>>>,
    pub pending_approval_sessions: Arc<Mutex<HashMap<String, String>>>,
    /// §5.4 授权记忆（server 全局一份；Agent.Policy 注入同一引用）。
    pub grants: Arc<owo_agent_core::grant_store::GrantStore>,
    pub aborts: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// 每个会话一个运行锁，避免并发回合覆盖消息、快照和审计状态。
    pub turn_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    pub traces_dir: PathBuf,
    pub perception: Arc<Mutex<SituationStore>>,
    pub whitelist: Arc<Mutex<Whitelist>>,
    pub pipeline: Arc<Mutex<LearnPipeline>>,
    pub proactive: Arc<Mutex<ProactiveEngine>>,
    pub stt: Arc<Mutex<owo_agent_core::LocalStt>>,
    pub automations: Arc<Mutex<AutomationStore>>,
    pub memory: Arc<Mutex<owo_agent_core::MemoryStore>>,
    pub audit_flushed: Arc<Mutex<usize>>,
    pub workspace: PathBuf,
    pub data_root: PathBuf,
    pub elements: Arc<Mutex<owo_agent_core::ElementRegistry>>,
    /// 插件启用状态（进程级热卸载的持久化基础）。
    pub plugin_state: Arc<Mutex<owo_agent_core::plugin::PluginStateStore>>,
    /// 持久场景图（跨请求保持模板命中率/历史命中先验；元素每请求从注册表刷新）。
    pub scene: Arc<Mutex<owo_agent_core::scene::SceneGraph>>,
    /// computer-use 任务注册表（任务级审批 + 熔断，m4d 前奏）。
    pub computer_tasks: Arc<owo_agent_core::ComputerTaskRegistry>,
    /// 云端执行队列（/cloud/* 路由；懒初始化，传输由环境变量决定）。
    /// tokio Mutex：异步 handler 内跨 await 持锁（std MutexGuard 非 Send）。
    pub cloud_queue: Arc<tokio::sync::Mutex<Option<owo_agent_core::cloud_exec::CloudTaskQueue>>>,
    /// 本地 API bearer token（X03：启动生成/复用 + 用户级 ACL 文件）。
    pub auth_token: Arc<auth_token::AuthToken>,
    /// 全局/每会话/敏感端点 双令牌桶限流（X03）。
    pub rate_limiter: Arc<rate_limit::RateLimiter>,
    /// R8 服务端韧性：全局并发 turn 上限 + 优雅关闭信号（CLI serve 接线退出）。
    pub shutdown_gate: Arc<shutdown::ShutdownGate>,
    /// R13 WorkSwarm S0：团队协同状态（协调器懒初始化；/teams、/projects、/tasks 路由）。
    pub workswarm: Arc<workswarm_api::WorkSwarmState>,
    /// V1 三日：ProductEval 评测中心（后台矩阵任务 + 取消令牌 + 持久化报告；/product-eval/*）。
    pub product_eval: Arc<product_eval_api::ProductEvalHub>,
    /// V1 四期（第三路）：Artifact 评审闭环存储（独立 SQLite 连接，
    /// 与 TeamCoordinator 的连接共存于 `data_root/workswarm/space.db`）。
    pub artifact_review: artifact_review_api::ArtifactReviewState,
}

impl AppState {
    /// §5.4 授权记忆匹配用的工作区标识（与 Policy::workspace_id 一致）。
    pub fn workspace_id(&self) -> String {
        self.workspace
            .canonicalize()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.workspace.to_string_lossy().into_owned())
    }

    pub fn new(
        agent: Agent,
        store: impl SessionStore + 'static,
        traces_dir: PathBuf,
        data_root: PathBuf,
        workspace: PathBuf,
    ) -> Self {
        // V1 四期（第三路）：评审闭环库路径（在 data_root 被 struct 字面量 move 前取好）。
        let workswarm_db_root = data_root.join("workswarm");
        let settings = owo_agent_core::Settings::load(&workspace);
        settings.apply_usage_env();
        // R8：用量预算接线（Agent 4 交付 usage）——单价/预算从环境变量注入，turn 入口硬熔断。
        {
            let usage_store = usage::global();
            if let Some(price) = std::env::var("OWO_MODEL_INPUT_PRICE_PER_MTOK")
                .ok()
                .and_then(|value| value.parse::<f64>().ok())
            {
                let output_price = std::env::var("OWO_MODEL_OUTPUT_PRICE_PER_MTOK")
                    .ok()
                    .and_then(|value| value.parse::<f64>().ok())
                    .unwrap_or(price);
                usage_store.set_price_per_mtok(price.max(output_price));
            }
            if let Some(budget) = std::env::var("OWO_USAGE_COST_BUDGET_USD")
                .ok()
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| *value > 0.0)
            {
                usage_store.set_budget(usage::UsageDimension::Session, budget);
            }
        }
        let mut whitelist = Whitelist::default();
        for entry in settings.whitelist.clone() {
            whitelist.upsert(entry);
        }
        let elements = Arc::new(Mutex::new(owo_agent_core::ElementRegistry::new()));
        // §5.3/§5.4：授权记忆与权限档位 —— server 全局一份，Agent 的 Policy 共享引用，
        // 审批卡选项写入的 Grant 在下一请求即命中；profile 从 settings 恢复。
        let grants = Arc::new(owo_agent_core::grant_store::GrantStore::new());
        let mut agent = agent;
        agent.set_elements(elements.clone());
        agent.set_grants(Arc::clone(&grants));
        if let Some(profile) = settings
            .permission_profile
            .as_deref()
            .and_then(owo_agent_core::PermissionProfile::parse)
        {
            agent.set_permission_profile(profile);
        }
        // X03/R3（§8.2）：本地 API bearer token **每次启动换发**并覆盖写盘（+ ACL）；
        // 写盘失败降级为内存 token。旧代际 bearer 因此在新进程上必然 401。
        let auth_token = Arc::new(auth_token::AuthToken::mint_for_boot(&data_root));
        let rate_limiter = Arc::new(rate_limit::RateLimiter::from_env());
        let shutdown_gate = Arc::new(shutdown::ShutdownGate::from_env());
        // V1 三日（第四路）：ProductEval suite 注册根目录（v1 → workspace/evals/v1/suite.json）。
        let product_eval_suite_root = workspace.join("evals");
        Self {
            agent: Arc::new(agent),
            store: Arc::new(store),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_approvals: Arc::new(Mutex::new(HashMap::new())),
            pending_approval_sessions: Arc::new(Mutex::new(HashMap::new())),
            grants: Arc::clone(&grants),
            aborts: Arc::new(Mutex::new(HashMap::new())),
            turn_locks: Arc::new(Mutex::new(HashMap::new())),
            traces_dir,
            perception: Arc::new(Mutex::new(SituationStore::new())),
            whitelist: Arc::new(Mutex::new(whitelist)),
            pipeline: Arc::new(Mutex::new(LearnPipeline::new(
                data_root.join("skills").join("user"),
            ))),
            proactive: Arc::new(Mutex::new(ProactiveEngine::new(settings.proactive.clone()))),
            stt: Arc::new(Mutex::new(owo_agent_core::LocalStt::new(
                &settings.stt,
                &data_root,
            ))),
            automations: Arc::new(Mutex::new(AutomationStore::new(data_root.clone()))),
            memory: Arc::new(Mutex::new(owo_agent_core::MemoryStore::new(
                data_root.join("memory.jsonl"),
            ))),
            audit_flushed: Arc::new(Mutex::new(0)),
            workspace,
            plugin_state: Arc::new(Mutex::new(owo_agent_core::plugin::PluginStateStore::new(
                Some(data_root.join("plugin_state.json")),
            ))),
            scene: Arc::new(Mutex::new(owo_agent_core::scene::SceneGraph::new())),
            computer_tasks: Arc::new(owo_agent_core::ComputerTaskRegistry::new()),
            cloud_queue: Arc::new(tokio::sync::Mutex::new(None)),
            // R13 WorkSwarm：数据目录 data_root/workswarm（协调器首次使用懒初始化）。
            workswarm: Arc::new(workswarm_api::WorkSwarmState::new(
                data_root.join("workswarm"),
            )),
            // V1 三日（第四路）：ProductEval 评测中心。运行目录 data_root/product_eval/runs；
            // suite 注册表固定 v1 → workspace/evals/v1/suite.json。
            // live 执行器工厂：Provider 按 core 统一入口构建（OPENAI_API_KEY 等环境变量），
            // single → 第一路 SingleAgentExecutor；multi（workswarm）→ 第二路 WorkSwarmExecutor
            // （等 core `pub mod workswarm_executor` 登记后接入；未接线前 live 运行 failed，不伪造结果）。
            product_eval: Arc::new(product_eval_api::ProductEvalHub::new(
                data_root.join("product_eval").join("runs"),
                product_eval_suite_root,
                {
                    let workswarm_work_root = data_root.join("product_eval").join("workswarm");
                    Arc::new(move || {
                        let (provider, model) = owo_agent_core::product_eval::build_live_provider(
                            None,
                        )
                        .map_err(|e| {
                            format!("live Provider 不可用（检查 OPENAI_API_KEY 等）：{}", e.0)
                        })?;
                        let single: Arc<dyn owo_agent_core::product_eval::CaseExecutor> =
                            Arc::new(owo_agent_core::product_eval::SingleAgentExecutor::new(
                                Arc::clone(&provider),
                                model.clone(),
                            ));
                        let multi: Option<Arc<dyn owo_agent_core::product_eval::CaseExecutor>> =
                            Some(Arc::new(
                                // 第二路适配器以 crate 根模块名注册（物理文件 product_eval/workswarm_executor.rs，
                                // #[path] 技巧见 core lib.rs——避免与第一路双写 product_eval.rs）。
                                owo_agent_core::WorkSwarmExecutor::new(
                                    Arc::clone(&provider),
                                    model.clone(),
                                    workswarm_work_root.clone(),
                                ),
                            ));
                        Ok(Arc::new(product_eval_api::ModeDispatchExecutor::new(
                            Some(single),
                            multi,
                        )))
                    })
                },
            )),
            data_root,
            elements,
            auth_token,
            rate_limiter,
            shutdown_gate,
            // V1 四期（第三路）：评审闭环复用 WorkSwarm 的 space.db（独立连接 + busy_timeout）。
            artifact_review: artifact_review_api::ArtifactReviewState::new(
                workswarm_db_root.join("space.db"),
            ),
        }
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    // R7：SSE→可观测性指标桥接（Agent 4 钩子）：/events/stream 的采样样本
    // 转发到 observability_api（/metrics/runtime 呈现真实运行期数值）；
    // SLO 报告探针注册（/metrics/slo 反映全局 SLO 状态）。幂等：重复调用仅替换。
    event_stream::set_metrics_observer(Box::new(|sample| {
        observability_api::ingest_metrics_sample(&sample.to_json());
    }));
    observability_api::register_slo_report_probe(std::sync::Arc::new(slo::report_global));
    // R12（Agent 4 交付，主控接线）：用量/SLO 告警/SLO 周期报告探针注册，
    // 使 /metrics/prometheus 用量指标、/metrics/slo/alerts、/metrics/slo/report 返回真实数据
    // （此前仅注册 slo_report_probe，其余探针为未注册空 stub）。
    observability_api::register_usage_probe(std::sync::Arc::new(|| usage::global().summary()));
    observability_api::register_slo_alerts_probe(std::sync::Arc::new(|| slo::alerts_json(50)));
    observability_api::register_slo_period_probe(std::sync::Arc::new(slo::report_period_global));
    // R9 主控接线收尾：SLO 告警监听器转发到可靠事件流（/events/stream 收到 alert 事件）。
    // 触发源为 `slo::check_alerts_global`（数据面）；未评估时不产生事件，无副作用。
    slo::set_alert_listener(Box::new(|event| {
        let trace_id = event.trace_id.clone();
        let data = serde_json::to_string(event).unwrap_or_default();
        event_stream::hub().publish_alert(data, trace_id);
    }));
    // 公开面：健康检查 / OpenAPI / token 引导（发布桌面模式下 token handler 额外验证配对证明）。
    let public = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi_spec))
        .route("/auth/token", get(auth_token::auth_token_bootstrap))
        .with_state(state.clone());
    // 保护面：全部业务 API（bearer token 鉴权 + 双令牌桶限流）。
    let protected = Router::new()
        .route("/audit", get(audit_api::audit_list))
        .route("/session", post(session_api::create_session))
        .route("/session/{id}", get(session_api::get_session))
        .route("/session/{id}/turn", post(turn_api::turn))
        .route(
            "/session/{id}/attachments",
            get(session_api::attachments_list),
        )
        .route(
            "/session/{id}/attachments",
            post(session_api::attachment_upload).layer(DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .route(
            "/session/{id}/permission/{request_id}",
            post(turn_api::respond_permission),
        )
        .route("/session/{id}/abort", post(session_api::abort_turn))
        .route("/session/{id}/diff", get(session_api::diff))
        .route("/session/{id}/revert", post(session_api::revert))
        .route("/session/{id}/fork", post(session_api::fork_session))
        .route("/session/{id}/rewind", post(session_api::rewind_session))
        .route("/session/{id}/redo", post(session_api::redo_session))
        .route("/session/{id}/rename", post(session_api::session_rename))
        .route("/session/{id}/archive", post(session_api::session_archive))
        .route("/session/{id}/pin", post(session_api::session_pin))
        .route("/session/{id}/model", post(session_api::session_set_model))
        .route("/session/{id}/children", get(session_api::children))
        .route(
            "/session/{id}/export/{format}",
            get(session_api::export_session),
        )
        .route("/sessions", get(session_api::list_sessions))
        .route("/skills", get(skills_api::list_skills))
        .route(
            "/skills/{name}",
            get(skills_api::skill_detail).post(skills_api::skill_edit),
        )
        .route("/skills/{name}/enabled", post(skills_api::skill_enabled))
        .route("/eval/run", post(eval_api::run_eval))
        .route("/context/snapshot", get(session_api::context_snapshot))
        .route("/perception/events", get(perception_api::perception_events))
        .route(
            "/perception/capture",
            post(perception_api::perception_capture),
        )
        .route(
            "/perception/layers",
            post(perception_api::perception_layers),
        )
        .route("/perception/tree", post(perception_api::perception_tree))
        .route(
            "/perception/template/build",
            post(perception_api::perception_template_build),
        )
        .route(
            "/perception/template/build-ocr",
            post(perception_api::perception_template_build_ocr),
        )
        .route(
            "/perception/template/detect",
            post(perception_api::perception_template_detect),
        )
        .route(
            "/perception/template/detect-ocr",
            post(perception_api::perception_template_detect_ocr),
        )
        .route(
            "/perception/elements",
            post(perception_api::perception_elements),
        )
        .route(
            "/perception/template/{app_id}",
            get(perception_api::perception_template_get),
        )
        .route("/perception/ocr", post(perception_api::perception_ocr))
        .route(
            "/perception/ocr/bytes",
            post(perception_api::perception_ocr_bytes),
        )
        .route("/perception/ocr/status", get(perception_api::ocr_status))
        .route(
            "/perception/ocr/region",
            post(perception_api::perception_ocr_region),
        )
        .route(
            "/perception/window",
            post(perception_api::perception_window),
        )
        .route("/desktop/foreground", get(desktop_api::desktop_foreground))
        .route("/desktop/windows", get(desktop_api::desktop_windows))
        .route("/desktop/activate", post(desktop_api::desktop_activate))
        .route("/desktop/click", post(desktop_api::desktop_click))
        .route("/desktop/type", post(desktop_api::desktop_type))
        .route("/desktop/key", post(desktop_api::desktop_key))
        .route("/desktop/shortcut", post(desktop_api::desktop_shortcut))
        .route("/desktop/launch", post(desktop_api::desktop_launch))
        .route("/desktop/scroll", post(desktop_api::desktop_scroll))
        .route("/desktop/wait", post(desktop_api::desktop_wait))
        .route("/vision/status", get(desktop_api::vision_status))
        .route("/vision/describe", post(desktop_api::vision_describe))
        .route("/vision/verify", post(desktop_api::vision_verify))
        .route("/vision/ground", post(desktop_api::vision_ground))
        .route("/memory/observations", get(memory_api::memory_observations))
        .route("/memory/clear", post(memory_api::memory_clear))
        .route("/memory/mine-skill", post(memory_api::memory_mine_skill))
        .route("/learn/start", post(learn_api::learn_start))
        .route("/learn/record", post(learn_api::learn_record))
        .route("/learn/pause", post(learn_api::learn_pause))
        .route("/learn/resume", post(learn_api::learn_resume))
        .route("/learn/stop", post(learn_api::learn_stop))
        .route("/learn/clear", post(learn_api::learn_clear))
        .route("/learn/status", get(learn_api::learn_status))
        .route("/learn/execute", post(learn_api::learn_execute))
        .route("/learn/packages", get(learn_api::learn_packages))
        .route(
            "/learn/packages/{name}",
            get(learn_api::learn_package_detail).delete(learn_api::learn_package_delete),
        )
        .route("/learn/sink", post(learn_api::learn_sink))
        .route(
            "/learn/execute-package",
            post(learn_api::learn_execute_package),
        )
        .route("/learn/export/{name}", get(learn_api::learn_export))
        .route(
            "/learn/import",
            post(learn_api::learn_import).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .route("/skill/verify", post(learn_api::skill_verify))
        .route("/proactive/observe", post(assist_api::proactive_observe))
        .route("/proactive/decide", post(assist_api::proactive_decide))
        .route(
            "/proactive/suggestions",
            get(assist_api::proactive_suggestions),
        )
        .route(
            "/stt/transcribe",
            post(assist_api::stt_transcribe).layer(DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
        .route("/automations", get(assist_api::automations_list))
        .route("/automations", post(assist_api::automations_create))
        .route(
            "/automations/{id}/toggle",
            post(assist_api::automations_toggle),
        )
        .route(
            "/automations/{id}",
            axum::routing::delete(assist_api::automations_delete),
        )
        .route(
            "/automations/reminders",
            get(assist_api::automations_reminders),
        )
        .route(
            "/automations/reminders/clear",
            post(assist_api::automations_clear_reminders),
        )
        .route(
            "/settings",
            get(settings_api::settings_get).post(settings_api::settings_update),
        )
        .route("/settings/egress", post(settings_api::settings_egress))
        .route(
            "/settings/provider-test",
            post(settings_api::settings_provider_test),
        )
        // §5.3/§5.4 权限档位与授权记忆管理（UI/CLI 统一入口）。
        .route(
            "/permissions",
            get(settings_api::permissions_status).post(settings_api::permissions_set_profile),
        )
        .route("/permissions/grants", get(settings_api::grants_list))
        .route(
            "/permissions/grants/revoke",
            post(settings_api::grants_revoke),
        )
        .route("/whitelist", get(whitelist_api::whitelist_list))
        .route("/whitelist/manage", post(whitelist_api::whitelist_manage))
        .route("/session/{id}/context", get(session_api::session_context))
        .route("/skills/health", get(skills_api::skills_health))
        .route(
            "/skills/health/{name}/reset",
            post(skills_api::skill_health_reset),
        )
        .route("/plugins", get(plugin_api::plugins_list))
        .route("/plugins/{id}/enabled", post(plugin_api::plugin_enabled))
        .route("/subagent/run", post(subagent_api::subagent_run))
        .route(
            "/project/rules",
            get(project_api::project_rules_get).post(project_api::project_rules_post),
        )
        .route(
            "/project/rules/template",
            post(project_api::project_rules_template),
        )
        .route("/mcp", get(mcp_api::mcp_list))
        .route("/mcp/health", get(mcp_api::mcp_health_snapshot))
        .route("/capabilities", get(capabilities::capabilities_list))
        .route("/mcp/add", post(mcp_api::mcp_add))
        .route("/mcp/remove", post(mcp_api::mcp_remove))
        .route("/mcp/reconnect", post(mcp_api::mcp_reconnect))
        .route("/mcp/enabled", post(mcp_api::mcp_enabled))
        .route("/locate/query", post(locate_api::locate_query))
        .route("/traces", get(traces_api::traces_list))
        .route("/traces/{index}", get(traces_api::trace_show))
        .route("/memory/recall", get(memory_api::memory_recall))
        .route(
            "/computer-use/tasks",
            get(computer_api::computer_tasks_list),
        )
        .route(
            "/computer-use/task",
            post(computer_api::computer_task_create),
        )
        .route(
            "/computer-use/task/{id}/{action}",
            post(computer_api::computer_task_transition),
        )
        .route(
            "/computer-use/task/{id}/check/{action}",
            get(computer_api::computer_task_check),
        )
        .route(
            "/computer-use/sensitive-check",
            post(computer_api::computer_sensitive_check),
        )
        .route(
            "/computer-use/task/{id}/run",
            post(computer_api::computer_task_run),
        )
        .route("/cloud/tasks", post(cloud_api::cloud_task_submit))
        .route("/cloud/tasks/{id}", get(cloud_api::cloud_task_status))
        .route(
            "/cloud/tasks/{id}/result",
            get(cloud_api::cloud_task_result),
        )
        .route(
            "/cloud/tasks/{id}/cancel",
            post(cloud_api::cloud_task_cancel),
        )
        // R8 服务端韧性（并发上限/状态/优雅关闭）。
        .route("/server/status", get(ops_api::server_status))
        .route("/server/shutdown", post(ops_api::server_shutdown))
        // R8 用量预算：加额恢复（硬熔断后 request_topup 解除停轮）。
        .route("/usage/topup", post(usage::usage_topup))
        // 与 R6 同款对齐：先 with_state 定 S，再 merge 模块 router（Router<()> 经 From 转换）。
        .with_state(state.clone())
        .merge(notes_api::router(state.clone()))
        .merge(plugin_market_api::router(state.clone()))
        .merge(workflow_api::router(state.clone()))
        .merge(goal_api::router(state.clone()))
        .merge(sse::router(state.clone()))
        // R5 第五轮：eval 护栏 / 团队共享 / 可观测性 / 记忆图谱 / 统一命令入口。
        // Agent 1 的审批（/workflow/run/{run_id}/approval）与 run SSE
        // （/workflow/run/{run_id}/events）已自含在 workflow_api::router 内，无需新 merge。
        .merge(team_api::router(state.clone()))
        .merge(workswarm_api::router(state.clone()))
        // V1 四期（第三路）：Artifact 评审闭环。
        .merge(artifact_review_api::router(state.clone()))
        // 六期（第三路）：内置团队模板目录（候选展示 + 手动安装，幂等）。
        .merge(team_template_catalog_api::team_template_catalog_router(
            state.clone(),
        ))
        // R1（§8.5）：DesktopWorld/WorldModel 闭环 /desktop-envs/*、/world-model/*、
        // /transitions/*、/datasets/*、/model-candidates/*（desktop_world_api 模块内
        // DesktopWorldHub 单例 + ControllerLease token+epoch 围栏；与 /desktop/* 计算机
        // 操作路由不同区）。
        .merge(desktop_world_api::router(state.clone()))
        .merge(eval_gate::router(state.clone()))
        // V1 三日（第四路）：ProductEval 评测中心（bearer 保护面）。
        .merge(product_eval_api::router(state.clone()))
        .merge(observability_api::router(state.clone()))
        .merge(memory_graph_api::router(state.clone()))
        .merge(intent_api::router(state.clone()))
        // R8 存储运维（备份/恢复/导出/清空）。
        .merge(backup::router(state.clone()))
        // R3（§8.1）：冷启动诊断 ledger（GET /diagnostics/requests，仅六字段）。
        .merge(request_ledger_api::router(state.clone()))
        // R8 用量与成本归集（Agent 4 交付：usage_router 四维用量 + 预算硬熔断）。
        .merge(usage::usage_router(state.clone()))
        // R10 契约治理：JSON Schema 版本化发布（/schemas/*）+ 契约变更 RFC 登记见本文件契约区。
        .route("/schemas", get(schemas_api::schemas_list))
        .route("/schemas/{kind}/{version}", get(schemas_api::schema_get))
        // R12（Agent 2 交付，主控挂载）：P2 双节点网格控制面 /fleet/*（节点注册/列表、
        // 任务提交/查询/取消/SSE 事件、审批响应；模块内 FleetHub 单例，不占用 AppState）。
        .merge(fleet_api::router(state.clone()))
        // R6（Wave 1，Agent 4 交付）：可靠事件流 /events/stream（SSE 续传 + 背压）。
        .merge(event_stream::router(state.clone()))
        // 默认 JSON 请求只允许 1 MiB；音频、附件、技能包在各自路由上单独放宽。
        .layer(DefaultBodyLimit::max(1024 * 1024))
        // 鉴权在最外层：未授权请求不进入限流，也不消耗令牌。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_token::require_auth,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            rate_limit::enforce_rate_limit,
        ));
    // 公开面（含静态 fallback）与保护面合并：两者均为 Router<Arc<AppState>>。
    // R8/R9：trace_id 贯穿置于最外层（public + protected + fallback 全覆盖）。
    public
        .merge(protected)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            trace_id_middleware,
        ))
        // R10：弃用策略——命中 DEPRECATED_ROUTES 附加 Deprecation 头。
        .layer(axum::middleware::from_fn(deprecation_middleware))
        .fallback_service(ServeDir::new(desktop_web_dir()))
        .layer(cors_layer())
}

/// R8/R9：trace_id 请求贯穿——从 `X-Trace-Id` 头继承（不合法则生成），回填响应头，
/// 设置全局 trace 上下文（Agent 4 logging：后台任务/SSE/指标可继承），
/// 并落一条结构化访问日志（脱敏不落消息体）。
///
/// R3（§8.1）：同一最外层切面把请求写入进程内安全 ledger（`request_ledger_api`）。
/// 只记录路由模板（axum `MatchedPath`），未匹配路由（fallback 静态资产服务）
/// 不进 ledger；访问日志同口径改用 `route_template` 字段，避免真实资源 id
/// 进入日志面。
async fn trace_id_middleware(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let inherited = request
        .headers()
        .get(logging::TRACE_HEADER)
        .and_then(|value| value.to_str().ok());
    let trace_id = logging::TraceId::from_header(inherited);
    logging::set_current_trace_id(Some(trace_id.as_str()));
    let method = request.method().to_string();
    // 路由模板优先（隐私：参数段恒为 {name}）；fallback 无模板 → None，不记录。
    let route_template = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|matched| matched.as_str().to_string());
    let path = request.uri().path().to_string();
    let source = request_ledger_api::sanitize_source(
        request
            .headers()
            .get(request_ledger_api::CLIENT_HEADER)
            .and_then(|value| value.to_str().ok()),
    );
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let started = std::time::Instant::now();
    let mut response = next.run(request).await;
    if let Ok(value) = trace_id
        .to_header_value()
        .parse::<axum::http::HeaderValue>()
    {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(logging::TRACE_HEADER),
            value,
        );
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let status = response.status().as_u16();
    if let Some(template) = route_template {
        request_ledger_api::record(
            &state.data_root,
            request_ledger_api::RequestRecord {
                method: method.clone(),
                route_template: template.clone(),
                started_at: started_at.clone(),
                duration_ms,
                status,
                source: source.clone(),
            },
        );
        let _ = path; // 原始 path 只存在于局部，既不落日志也不落 ledger。
        logging::emit(
            logging::Level::Info,
            "http",
            Some(trace_id.as_str()),
            "request",
            &[
                ("method", serde_json::json!(method)),
                ("route_template", serde_json::json!(template)),
                ("status", serde_json::json!(status)),
                ("duration_ms", serde_json::json!(duration_ms)),
                ("source", serde_json::json!(source)),
            ],
        );
    }
    logging::set_current_trace_id(None);
    response
}

/// CORS：发布桌面版只接受 Tauri origin；开发模式额外允许 loopback 调试页。
/// 跨源预检由浏览器强制；服务器侧仍以 bearer token 鉴权为准。
fn cors_layer() -> CorsLayer {
    use axum::http::Method;
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _parts| {
            origin_allowed(origin.as_bytes())
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::ACCEPT,
            axum::http::HeaderName::from_static(auth_token::DESKTOP_PAIRING_HEADER),
            // R3（§8.1）：ledger 来源标签与 trace 贯穿头（跨源预检需显式放行）。
            axum::http::HeaderName::from_static(request_ledger_api::CLIENT_HEADER),
            axum::http::HeaderName::from_static(logging::TRACE_HEADER),
        ])
        .max_age(std::time::Duration::from_secs(600))
}

/// `OWO_DESKTOP_RELEASE=1` 由 Tauri 子进程注入，避免发布版继承开发浏览器边界。
fn origin_allowed(origin: &[u8]) -> bool {
    let Ok(origin) = std::str::from_utf8(origin) else {
        return false;
    };
    let mut parts = origin.split("://");
    let Some(scheme) = parts.next() else {
        return false;
    };
    let Some(host_port) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let host = host_port.split(':').next().unwrap_or(host_port);
    let desktop_release = std::env::var("OWO_DESKTOP_RELEASE")
        .map(|value| value == "1")
        .unwrap_or(false);
    if desktop_release {
        return (scheme == "tauri" && host == "localhost")
            || ((scheme == "http" || scheme == "https") && host == "tauri.localhost");
    }
    ((scheme == "http" || scheme == "https") && (host == "localhost" || host == "127.0.0.1"))
        || (scheme == "tauri" && host == "localhost")
        || ((scheme == "http" || scheme == "https") && host == "tauri.localhost")
}

/// 开发环境下的桌面工作台静态目录：`<repo>/agent-sdk/desktop/web`。
fn desktop_web_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|parent| parent.parent())
        .map(|root| root.join("desktop").join("web"))
        .unwrap_or_else(|| PathBuf::from("desktop/web"))
}

async fn openapi_spec() -> Json<Value> {
    Json(serde_json::json!({
        "openapi": "3.1.0",
        "info": { "title": "OwO Agent SDK API", "version": env!("CARGO_PKG_VERSION") },
        // R10 契约治理：API 版本号（破坏性变更递增 minor；弃用期 ≥2 minor）。
        "x-owo-api-version": OWO_API_VERSION,
        "servers": [{ "url": "http://127.0.0.1:4096" }],
        "paths": {
            "/health": { "get": { "operationId": "health", "responses": { "200": { "description": "service health + build info", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/HealthResponse" } } } } } } },
            "/usage": { "get": { "operationId": "usageSummary", "responses": { "200": { "description": "model token usage snapshot and budget config" } } } },
            "/audit": { "get": { "operationId": "auditList", "parameters": [{ "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "recent audit entries" } } } },
            "/session": { "post": {
                "operationId": "createSession",
                "requestBody": { "content": { "application/json": { "schema": { "$ref": "#/components/schemas/CreateSessionRequest" } } } },
                "responses": { "200": { "description": "session created", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/SessionInfo" } } } } }
            } },
            "/session/{id}": { "get": { "operationId": "getSession", "parameters": [path_param("id")], "responses": { "200": { "description": "session detail with messages" } } } },
            "/session/{id}/turn": { "post": {
                "operationId": "agentTurn",
                "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                "requestBody": { "content": { "application/json": { "schema": { "$ref": "#/components/schemas/TurnRequest" } } } },
                "responses": { "200": { "description": "SSE event stream" } }
            } },
            "/session/{id}/attachments": { "get": { "operationId": "attachmentsList", "parameters": [path_param("id")], "responses": { "200": { "description": "attachment list" } } }, "post": { "operationId": "attachmentUpload", "parameters": [path_param("id")], "responses": { "200": { "description": "uploaded attachment" } } } },
            "/session/{id}/abort": { "post": { "operationId": "abortTurn", "parameters": [path_param("id")], "responses": { "200": { "description": "ok" } } } },
            "/session/{id}/permission/{request_id}": { "post": { "operationId": "respondPermission", "parameters": [path_param("id"), path_param("request_id")], "responses": { "200": { "description": "ok" } } } },
            "/session/{id}/diff": { "get": { "operationId": "sessionDiff", "parameters": [path_param("id")], "responses": { "200": { "description": "diff list" } } } },
            "/session/{id}/revert": { "post": { "operationId": "sessionRevert", "parameters": [path_param("id")], "responses": { "200": { "description": "ok" } } } },
            "/session/{id}/fork": { "post": { "operationId": "sessionFork", "parameters": [path_param("id")], "responses": { "200": { "description": "forked session" } } } },
            "/session/{id}/rewind": { "post": { "operationId": "sessionRewind", "parameters": [path_param("id")], "responses": { "200": { "description": "ok" } } } },
            "/session/{id}/redo": { "post": { "operationId": "sessionRedo", "parameters": [path_param("id")], "responses": { "200": { "description": "ok" } } } },
            "/session/{id}/rename": { "post": { "operationId": "sessionRename", "parameters": [path_param("id")], "responses": { "200": { "description": "renamed session" } } } },
            "/session/{id}/archive": { "post": { "operationId": "sessionArchive", "parameters": [path_param("id")], "responses": { "200": { "description": "archive state" } } } },
            "/session/{id}/pin": { "post": { "operationId": "sessionPin", "parameters": [path_param("id")], "responses": { "200": { "description": "pin state" } } } },
            "/session/{id}/model": { "post": { "operationId": "sessionSetModel", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "model": { "type": "string", "nullable": true, "description": "非空=固定请求模型；null/空串/\"default\"=清除覆盖（回退 OPENAI_MODEL→启动配置→内置默认；哨兵不落库、不进请求体）" } } } } } }, "responses": { "200": { "description": "{ id, model, model_override }" }, "404": { "description": "session not found" } } } },
            "/session/{id}/children": { "get": { "operationId": "sessionChildren", "parameters": [path_param("id")], "responses": { "200": { "description": "children" } } } },
            "/session/{id}/export/{format}": { "get": { "operationId": "exportSession", "parameters": [path_param("id"), path_param("format")], "responses": { "200": { "description": "md or html" } } } },
            "/sessions": { "get": { "operationId": "listSessions", "responses": { "200": { "description": "session list" } } } },
            "/skills": { "get": { "operationId": "listSkills", "responses": { "200": { "description": "skill list" } } } },
            "/skills/{name}": { "get": { "operationId": "skillDetail", "parameters": [path_param("name")], "responses": { "200": { "description": "skill detail with SKILL.md content" } } }, "post": { "operationId": "skillEdit", "parameters": [path_param("name")], "responses": { "200": { "description": "updated" } } } },
            "/skills/{name}/enabled": { "post": { "operationId": "skillEnabled", "parameters": [path_param("name")], "responses": { "200": { "description": "enabled state" } } } },
            "/eval/run": { "post": { "operationId": "runEval", "requestBody": { "content": { "application/json": { "schema": { "$ref": "#/components/schemas/EvalRunRequest" } } } }, "responses": { "200": { "description": "eval report" } } } },
            "/product-eval/runs": {
                "post": {
                    "operationId": "createProductEvalRun",
                    "summary": "受理一次产品评测矩阵（异步执行）",
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object", "required": ["suite", "execution", "modes", "repetitions"], "properties": {
                        "suite": { "type": "string", "enum": ["v1"], "description": "仅允许注册名 v1；客户端本地路径一律拒绝" },
                        "execution": { "type": "string", "enum": ["reference", "live"], "description": "reference=免模型参考回放+检查器；live=真实执行器（single→SingleAgentExecutor，workswarm→WorkSwarmExecutor）" },
                        "modes": { "type": "array", "items": { "type": "string", "enum": ["single", "workswarm"] }, "minItems": 1, "description": "对照拓扑子集；结果报告 wire 中 agent_mode 为核心小写词 single/multi（workswarm ≡ multi）" },
                        "repetitions": { "type": "integer", "minimum": 1, "maximum": 20, "description": "重复次数（覆盖 suite 默认）" },
                        "category": { "type": ["string", "null"], "enum": ["code", "research", "document", null], "description": "只跑指定分类" },
                        "only": { "type": ["string", "null"], "description": "只跑 id 包含该子串的任务" }
                    } } } } },
                    "responses": {
                        "202": { "description": "受理", "content": { "application/json": { "schema": { "type": "object", "required": ["run_id", "status"], "properties": { "run_id": { "type": "string", "description": "eval-…" }, "status": { "type": "string", "enum": ["queued"] } } } } } },
                        "400": { "description": "语义校验失败（未知 suite/execution/mode、repetitions 越界、suite 加载失败、过滤后无任务）", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                        "422": { "description": "结构校验失败（缺字段/类型错）", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                },
                "get": {
                    "operationId": "listProductEvalRuns",
                    "summary": "评测运行列表（created_at 倒序）",
                    "responses": { "200": { "description": "runs", "content": { "application/json": { "schema": { "type": "object", "properties": { "runs": { "type": "array", "items": { "$ref": "#/components/schemas/ProductEvalRunSummary" } } } } } } } }
                }
            },
            "/product-eval/runs/{id}": {
                "get": {
                    "operationId": "getProductEvalRun",
                    "summary": "评测运行详情：进度 + 运行参数 + 完整报告（聚合指标/每 case 对比/失败步骤/Artifact refs）",
                    "parameters": [path_param("id")],
                    "responses": {
                        "200": { "description": "run summary + report", "content": { "application/json": { "schema": { "allOf": [
                            { "$ref": "#/components/schemas/ProductEvalRunSummary" },
                            { "type": "object", "properties": { "report": { "oneOf": [
                                { "type": "null", "description": "尚无报告（未开始执行/工厂失败/损坏）" },
                                { "$ref": "#/components/schemas/ProductEvalReport" }
                            ] } } }
                        ] } } } },
                        "404": { "description": "运行不存在", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/product-eval/runs/{id}/cancel": {
                "post": {
                    "operationId": "cancelProductEvalRun",
                    "summary": "取消评测运行（幂等：置协作令牌并立即 cancelled；重复/终态后取消零副作用）",
                    "parameters": [path_param("id")],
                    "responses": {
                        "200": { "description": "取消受理或原状态", "content": { "application/json": { "schema": { "type": "object", "required": ["run_id", "status"], "properties": { "run_id": { "type": "string" }, "status": { "type": "string", "enum": ["queued", "running", "cancelled", "completed", "failed", "interrupted"] } } } } } },
                        "404": { "description": "运行不存在", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/context/snapshot": { "get": { "operationId": "contextSnapshot", "responses": { "200": { "description": "situation snapshot" } } } },
            "/perception/events": { "get": { "operationId": "perceptionSubscribe", "responses": { "200": { "description": "SSE perception event stream" } } } },
            "/perception/capture": { "post": { "operationId": "perceptionCapture", "responses": { "200": { "description": "capture meta with OCR summary" } } } },
            "/perception/layers": { "post": { "operationId": "perceptionLayers", "responses": { "200": { "description": "layer authorization updated" } } } },
            "/perception/tree": { "post": { "operationId": "perceptionTree", "responses": { "200": { "description": "deep UI tree dump" } } } },
            "/perception/ocr": { "post": { "operationId": "perceptionOcr", "responses": { "200": { "description": "OCR text with bounding boxes" } } } },
            "/perception/ocr/status": { "get": { "operationId": "ocrStatus", "responses": { "200": { "description": "OCR engine diagnostics" } } } },
            "/perception/ocr/region": { "post": { "operationId": "perceptionOcrRegion", "responses": { "200": { "description": "region OCR text with bounding boxes" } } } },
            "/learn/record": { "post": { "operationId": "learnRecord", "responses": { "200": { "description": "learn state" } } } },
            "/learn/start": { "post": { "operationId": "learnStart", "responses": { "200": { "description": "learn state" } } } },
            "/learn/pause": { "post": { "operationId": "learnPause", "responses": { "200": { "description": "learn state" } } } },
            "/learn/resume": { "post": { "operationId": "learnResume", "responses": { "200": { "description": "learn state" } } } },
            "/learn/stop": { "post": { "operationId": "learnStop", "responses": { "200": { "description": "stopped with sample count" } } } },
            "/learn/clear": { "post": { "operationId": "learnClear", "responses": { "200": { "description": "ok" } } } },
            "/learn/execute": { "post": { "operationId": "learnExecute", "responses": { "200": { "description": "execution report" } } } },
            "/learn/packages": { "get": { "operationId": "learnPackages", "responses": { "200": { "description": "flow skill packages" } } } },
            "/learn/packages/{name}": { "get": { "operationId": "learnPackageDetail", "parameters": [path_param("name")], "responses": { "200": { "description": "package detail" } } }, "delete": { "operationId": "learnPackageDelete", "parameters": [path_param("name")], "responses": { "200": { "description": "deleted" } } } },
            "/learn/sink": { "post": { "operationId": "learnSink", "responses": { "200": { "description": "sunk package" } } } },
            "/learn/execute-package": { "post": { "operationId": "learnExecutePackage", "responses": { "200": { "description": "execution report" } } } },
            "/learn/export/{name}": { "get": { "operationId": "learnExport", "parameters": [path_param("name")], "responses": { "200": { "description": "owskill zip" } } } },
            "/learn/import": { "post": { "operationId": "learnImport", "responses": { "200": { "description": "imported package" } } } },
            "/skill/verify": { "post": { "operationId": "skillVerify", "responses": { "200": { "description": "validation result" } } } },
            "/proactive/observe": { "post": { "operationId": "proactiveObserve", "responses": { "200": { "description": "optional suggestion" } } } },
            "/proactive/decide": { "post": { "operationId": "proactiveDecide", "responses": { "200": { "description": "ok" } } } },
            "/proactive/suggestions": { "get": { "operationId": "proactiveSuggestions", "responses": { "200": { "description": "suggestion list" } } } },
            "/stt/transcribe": { "post": { "operationId": "sttTranscribe", "responses": { "200": { "description": "transcription text" } } } },
            "/automations": { "get": { "operationId": "automationsList", "responses": { "200": { "description": "automation tasks" } } }, "post": { "operationId": "automationsCreate", "responses": { "200": { "description": "created task" } } } },
            "/automations/{id}/toggle": { "post": { "operationId": "automationsToggle", "parameters": [path_param("id")], "responses": { "200": { "description": "enabled state" } } } },
            "/automations/{id}": { "delete": { "operationId": "automationsDelete", "parameters": [path_param("id")], "responses": { "200": { "description": "ok" } } } },
            "/automations/reminders": { "get": { "operationId": "automationsReminders", "responses": { "200": { "description": "pending reminders" } } } },
            "/automations/reminders/clear": { "post": { "operationId": "automationsClearReminders", "responses": { "200": { "description": "ok" } } } },
            "/settings": { "get": { "operationId": "settingsGet", "responses": { "200": { "description": "workspace settings" } } }, "post": { "operationId": "settingsUpdate", "responses": { "200": { "description": "workspace settings" } } } },
            "/settings/egress": { "post": { "operationId": "settingsEgress", "responses": { "200": { "description": "cloud enabled state" } } } },
            "/settings/provider-test": { "post": { "operationId": "settingsProviderTest", "responses": { "200": { "description": "provider self-diagnosis (R3 §3.4): stable code provider/not_configured|endpoint_reachable|endpoint_unreachable + masked endpoint; no secrets, no model calls (TCP probe only)" } } } },
            "/permissions": { "get": { "operationId": "permissionsStatus", "responses": { "200": { "description": "当前权限档位 + 授权记忆（脱敏）" } } }, "post": { "operationId": "permissionsSetProfile", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "profile": { "type": "string", "enum": ["read_only", "workspace", "auto_review", "full_access", "custom"] } }, "required": ["profile"] } } } }, "responses": { "200": { "description": "profile 已切换" } } } },
            "/permissions/grants": { "get": { "operationId": "grantsList", "responses": { "200": { "description": "授权记忆列表（脱敏）" } } } },
            "/permissions/grants/revoke": { "post": { "operationId": "grantsRevoke", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "grant_id": { "type": "string" } }, "required": ["grant_id"] } } } }, "responses": { "200": { "description": "授权记忆已撤销" } } } },
            "/whitelist": { "get": { "operationId": "whitelistList", "responses": { "200": { "description": "whitelist entries" } } } },
            "/session/{id}/context": { "get": { "operationId": "sessionContext", "parameters": [path_param("id")], "responses": { "200": { "description": "context stats: messages/tokens/budget/compaction/rules" } } } },
            "/skills/health": { "get": { "operationId": "skillsHealth", "responses": { "200": { "description": "flow skill health overview" } } } },
            "/skills/health/{name}/reset": { "post": { "operationId": "skillHealthReset", "parameters": [path_param("name")], "responses": { "200": { "description": "health reset" } } } },
            "/plugins": { "get": { "operationId": "pluginsList", "responses": { "200": { "description": "discovered plugins with manifests" } } } },
            "/plugins/{id}/enabled": { "post": { "operationId": "pluginEnabled", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "enabled": { "type": "boolean" } }, "required": ["enabled"] } } } }, "responses": { "200": { "description": "plugin enabled state" } } } },
            "/subagent/run": { "post": { "operationId": "subagentRun", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "prompt": { "type": "string" }, "read_only": { "type": "boolean" }, "model": { "type": "string" } }, "required": ["prompt"] } } } }, "responses": { "200": { "description": "subagent execution result" } } } },
            "/project/rules": { "get": { "operationId": "projectRulesGet", "responses": { "200": { "description": "AGENTS.md/CLAUDE.md rules with injection status" } } }, "post": { "operationId": "projectRulesPost", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "content": { "type": "string" } }, "required": ["content"] } } } }, "responses": { "200": { "description": "rules written" } } } },
            "/project/rules/template": { "post": { "operationId": "projectRulesTemplate", "responses": { "200": { "description": "AGENTS.md template written" } } } },
            "/mcp": { "get": { "operationId": "mcpList", "responses": { "200": { "description": "configured MCP servers" } } } },
            "/mcp/add": { "post": { "operationId": "mcpAdd", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "name": { "type": "string" }, "transport": { "type": "string", "enum": ["stdio", "http"] }, "command": { "type": "string" }, "args": { "type": "array", "items": { "type": "string" } }, "url": { "type": "string" } }, "required": ["name", "transport"] } } } }, "responses": { "200": { "description": "server added and connected" } } } },
            "/mcp/remove": { "post": { "operationId": "mcpRemove", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] } } } }, "responses": { "200": { "description": "server removed" } } } },
            "/mcp/health": { "get": { "operationId": "mcpHealthSnapshot", "responses": { "200": { "description": "per-server MCP health (state machine, circuit breaker, failure counters)" } } } },
            "/mcp/reconnect": { "post": { "operationId": "mcpReconnect", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] } } } }, "responses": { "200": { "description": "server reconnected from saved config (process-level uninstall + hot connect)" } } } },
            "/mcp/enabled": { "post": { "operationId": "mcpEnabled", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "name": { "type": "string" }, "enabled": { "type": "boolean" } }, "required": ["name", "enabled"] } } } }, "responses": { "200": { "description": "tool prefix enable/disable (process-level, model-invisible, not persisted)" } } } },
            "/capabilities": { "get": { "operationId": "capabilitiesList", "responses": { "200": { "description": "capability catalog (single source for UI/CLI/diagnostics/help, §8.3)" } } } },
            "/locate/query": { "post": { "operationId": "locateQuery", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "app_id": { "type": "string" }, "role": { "type": "string" }, "name_pattern": { "type": "string" }, "parent": { "type": "string" }, "stable_id": { "type": "string" }, "min_confidence": { "type": "number" } }, "required": [] } } } }, "responses": { "200": { "description": "multi-source locate result" } } } },
            "/traces": { "get": { "operationId": "tracesList", "responses": { "200": { "description": "trace list" } } } },
            "/traces/{index}": { "get": { "operationId": "traceShow", "parameters": [path_param("index")], "responses": { "200": { "description": "trace detail" } } } },
            "/memory/observations": { "get": { "operationId": "memoryObservations", "responses": { "200": { "description": "situation memory observations" } } } },
            "/memory/recall": { "get": { "operationId": "memoryRecall", "responses": { "200": { "description": "semantic memory recall" } } } },
            "/memory/clear": { "post": { "operationId": "memoryClear", "responses": { "200": { "description": "memory cleared" } } } },
            "/memory/mine-skill": { "post": { "operationId": "memoryMineSkill", "responses": { "200": { "description": "mined flow skill package" } } } },
            "/whitelist/manage": { "post": { "operationId": "whitelistManage", "responses": { "200": { "description": "whitelist entries" } } } },
            "/computer-use/tasks": { "get": { "operationId": "computerTasksList", "responses": { "200": { "description": "computer-use task list" } } } },
            "/computer-use/task": { "post": { "operationId": "computerTaskCreate", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "target_app": { "type": "string" }, "description": { "type": "string" }, "allowed_actions": { "type": "array", "items": { "type": "string" } }, "max_duration_ms": { "type": "integer" } }, "required": ["target_app"] } } } }, "responses": { "200": { "description": "task created (Pending)" } } } },
            "/computer-use/task/{id}/{action}": { "post": { "operationId": "computerTaskTransition", "parameters": [path_param("id"), path_param("action")], "responses": { "200": { "description": "task state transitioned" } } } },
            "/computer-use/task/{id}/check/{action}": { "get": { "operationId": "computerTaskCheck", "parameters": [path_param("id"), path_param("action")], "responses": { "200": { "description": "task executable check" } } } },
            "/computer-use/sensitive-check": { "post": { "operationId": "computerSensitiveCheck", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "name": { "type": "string" }, "role": { "type": "string" }, "ocr_text": { "type": "string" } }, "required": ["name"] } } } }, "responses": { "200": { "description": "sensitive ui detection" } } } },
            "/computer-use/task/{id}/run": { "post": { "operationId": "computerTaskRun", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "goals": { "type": "array", "items": { "type": "object", "properties": { "anchor_text": { "type": "string" }, "action": { "type": "string" }, "value": { "type": "string" }, "verify_text": { "type": "string" } } } } } } } } }, "responses": { "200": { "description": "approved task executed (closed loop)" } } } },
            "/cloud/tasks": { "post": { "operationId": "cloudTaskSubmit", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "name": { "type": "string" }, "workspace_dir": { "type": "string" }, "commands": { "type": "array", "items": { "type": "string" } }, "env_passthrough": { "type": "array", "items": { "type": "string" } }, "timeout_secs": { "type": "integer" } } } } } }, "responses": { "200": { "description": "cloud task submitted and executed" } } } },
            "/cloud/tasks/{id}": { "get": { "operationId": "cloudTaskStatus", "parameters": [path_param("id")], "responses": { "200": { "description": "cloud task status + usage" } } } },
            "/cloud/tasks/{id}/result": { "get": { "operationId": "cloudTaskResult", "parameters": [path_param("id")], "responses": { "200": { "description": "cloud task result + diff summary" } } } },
            "/cloud/tasks/{id}/cancel": { "post": { "operationId": "cloudTaskCancel", "parameters": [path_param("id")], "responses": { "200": { "description": "cloud task canceled" } } } },
            "/openapi.json": { "get": { "operationId": "openapiSpec", "responses": { "200": { "description": "OpenAPI 3.1 spec" } } } },
            "/perception/elements": { "post": { "operationId": "perceptionElements", "responses": { "200": { "description": "element registry snapshot" } } } },
            "/perception/ocr/bytes": { "post": { "operationId": "perceptionOcrBytes", "responses": { "200": { "description": "OCR text from raw image bytes" } } } },
            "/perception/window": { "post": { "operationId": "perceptionWindow", "responses": { "200": { "description": "active window info" } } } },
            "/perception/template/build": { "post": { "operationId": "perceptionTemplateBuild", "responses": { "200": { "description": "window template built" } } } },
            "/perception/template/build-ocr": { "post": { "operationId": "perceptionTemplateBuildOcr", "responses": { "200": { "description": "window template built with OCR" } } } },
            "/perception/template/detect": { "post": { "operationId": "perceptionTemplateDetect", "responses": { "200": { "description": "template detection result" } } } },
            "/perception/template/detect-ocr": { "post": { "operationId": "perceptionTemplateDetectOcr", "responses": { "200": { "description": "template detection with OCR" } } } },
            "/perception/template/{app_id}": { "get": { "operationId": "perceptionTemplateGet", "parameters": [path_param("app_id")], "responses": { "200": { "description": "stored window template" } } } },
            "/learn/status": { "get": { "operationId": "learnStatus", "responses": { "200": { "description": "learn pipeline state" } } } },
            "/desktop/foreground": { "get": { "operationId": "desktopForeground", "responses": { "200": { "description": "foreground window info" } } } },
            "/desktop/windows": { "get": { "operationId": "desktopWindows", "responses": { "200": { "description": "window list" } } } },
            "/desktop/activate": { "post": { "operationId": "desktopActivate", "responses": { "200": { "description": "window activated" } } } },
            "/desktop/click": { "post": { "operationId": "desktopClick", "responses": { "200": { "description": "mouse click performed" } } } },
            "/desktop/type": { "post": { "operationId": "desktopType", "responses": { "200": { "description": "text typed" } } } },
            "/desktop/key": { "post": { "operationId": "desktopKey", "responses": { "200": { "description": "key pressed" } } } },
            "/desktop/shortcut": { "post": { "operationId": "desktopShortcut", "responses": { "200": { "description": "shortcut performed" } } } },
            "/desktop/launch": { "post": { "operationId": "desktopLaunch", "responses": { "200": { "description": "app launched" } } } },
            "/desktop/scroll": { "post": { "operationId": "desktopScroll", "responses": { "200": { "description": "scroll performed" } } } },
            "/desktop/wait": { "post": { "operationId": "desktopWait", "responses": { "200": { "description": "wait performed" } } } },
            "/vision/status": { "get": { "operationId": "visionStatus", "responses": { "200": { "description": "vision engine diagnostics" } } } },
            "/vision/describe": { "post": { "operationId": "visionDescribe", "responses": { "200": { "description": "image description" } } } },
            "/vision/verify": { "post": { "operationId": "visionVerify", "responses": { "200": { "description": "verification result" } } } },
            "/vision/ground": { "post": { "operationId": "visionGround", "responses": { "200": { "description": "vision grounded location" } } } },
            "/notes": { "get": { "operationId": "notesList", "responses": { "200": { "description": "note list" } } }, "post": { "operationId": "notesCreate", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "title": { "type": "string" }, "markdown": { "type": "string" } }, "required": ["title"] } } } }, "responses": { "201": { "description": "note created" } } } },
            "/notes/{id}": { "get": { "operationId": "notesGet", "parameters": [path_param("id")], "responses": { "200": { "description": "note block tree" } } }, "put": { "operationId": "notesReplace", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "title": { "type": "string" }, "blocks": { "type": "array", "items": { "type": "object" } } } } } } }, "responses": { "200": { "description": "note replaced" } } }, "delete": { "operationId": "notesDelete", "parameters": [path_param("id")], "responses": { "200": { "description": "note deleted" } } } },
            "/notes/import": { "post": { "operationId": "notesImport", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "title": { "type": "string" }, "markdown": { "type": "string" } }, "required": ["title", "markdown"] } } } }, "responses": { "201": { "description": "note imported from markdown" } } } },
            "/notes/search": { "get": { "operationId": "notesSearch", "parameters": [{ "name": "q", "in": "query", "required": true, "schema": { "type": "string" } }], "responses": { "200": { "description": "cross-document search hits" } } } },
            "/notes/{id}/export/{format}": { "get": { "operationId": "notesExport", "parameters": [path_param("id"), path_param("format")], "responses": { "200": { "description": "note exported as md or html" } } } },
            "/notes/{id}/blocks": { "post": { "operationId": "notesAddBlock", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "parent": { "type": "string" }, "after": { "type": "string" }, "kind": { "type": "string" }, "text": { "type": "string" }, "data": { "type": "object" } }, "required": ["kind"] } } } }, "responses": { "201": { "description": "block added" } } } },
            "/notes/{id}/blocks/move": { "post": { "operationId": "notesMoveBlock", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "block_id": { "type": "string" }, "parent": { "type": "string" }, "after": { "type": "string" } }, "required": ["block_id"] } } } }, "responses": { "200": { "description": "block moved" } } } },
            "/notes/{id}/blocks/{block_id}": { "patch": { "operationId": "notesUpdateBlock", "parameters": [path_param("id"), path_param("block_id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "text": { "type": "string" }, "data": { "type": "object" } } } } } }, "responses": { "200": { "description": "block updated" } } }, "delete": { "operationId": "notesDeleteBlock", "parameters": [path_param("id"), path_param("block_id")], "responses": { "200": { "description": "removed block subtree ids" } } } },
            "/notes/{id}/reindex": { "post": { "operationId": "notesReindex", "parameters": [path_param("id")], "responses": { "200": { "description": "full-text index rebuilt" } } } },
            "/workflow": { "get": { "operationId": "workflowList", "responses": { "200": { "description": "discovered .owflow flows" } } } },
            "/workflow/validate": { "post": { "operationId": "workflowValidate", "requestBody": { "content": { "application/json": { "schema": { "type": "object" } } } }, "responses": { "200": { "description": "definition validation report" } } } },
            "/workflow/{name}": { "get": { "operationId": "workflowGet", "parameters": [path_param("name")], "responses": { "200": { "description": "flow definition with validation" } } } },
            "/workflow/{name}/run": { "post": { "operationId": "workflowRun", "parameters": [path_param("name")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "ctx": { "type": "object" } } } } } }, "responses": { "201": { "description": "workflow run started" } } } },
            "/workflow/{name}/runs": { "get": { "operationId": "workflowRuns", "parameters": [path_param("name")], "responses": { "200": { "description": "run list for flow" } } } },
            "/workflow/run/{run_id}": { "get": { "operationId": "workflowRunSnapshot", "parameters": [path_param("run_id")], "responses": { "200": { "description": "run snapshot" } } } },
            "/workflow/run/{run_id}/abort": { "post": { "operationId": "workflowRunAbort", "parameters": [path_param("run_id")], "responses": { "200": { "description": "abort requested" } } } },
            "/workflow/run/{run_id}/audit": { "get": { "operationId": "workflowRunAudit", "parameters": [path_param("run_id")], "responses": { "200": { "description": "run audit tail" } } } },
            "/goal": { "get": { "operationId": "goalList", "responses": { "200": { "description": "goal list" } } }, "post": { "operationId": "goalCreate", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "objective": { "type": "string" }, "budget": { "type": "object", "properties": { "max_steps": { "type": "integer" }, "max_replans": { "type": "integer" } } } }, "required": ["objective"] } } } }, "responses": { "201": { "description": "goal created" } } } },
            "/goal/{id}": { "get": { "operationId": "goalGet", "parameters": [path_param("id")], "responses": { "200": { "description": "goal detail" } } } },
            "/goal/{id}/plan": { "get": { "operationId": "goalPlanGet", "parameters": [path_param("id")], "responses": { "200": { "description": "goal plan" } } }, "post": { "operationId": "goalPlanCreate", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "steps": { "type": "array", "items": { "type": "object" } } }, "required": ["steps"] } } } }, "responses": { "201": { "description": "plan created with waves preview" } } } },
            "/goal/{id}/run": { "post": {
                "operationId": "goalRun",
                "parameters": [path_param("id")],
                "requestBody": { "content": { "application/json": { "schema": {
                    "type": "object",
                    "properties": {
                        "parallelism": { "type": "integer", "description": "wave 内并发执行步数上限" },
                        "allow_replan": { "type": "boolean" },
                        "execution": {
                            "type": "object",
                            "description": "执行路径选择；缺省 process。mode=worker_pool 必须提供非空 workers",
                            "properties": {
                                "mode": { "type": "string", "enum": ["process", "worker_pool"] },
                                "workers": {
                                    "type": "array",
                                    "description": "worker_pool 受控子进程配置（命令仅限当前可执行文件；env 白名单拒凭据键）",
                                    "items": {
                                        "type": "object",
                                        "required": ["name", "command", "cwd"],
                                        "properties": {
                                            "name": { "type": "string" },
                                            "command": { "type": "string" },
                                            "args": { "type": "array", "items": { "type": "string" } },
                                            "cwd": { "type": "string" },
                                            "env": { "type": "object", "additionalProperties": { "type": "string" } },
                                            "budget": { "type": "object", "properties": { "max_turns": { "type": "integer" }, "max_duration_secs": { "type": "integer" }, "max_memory_mb": { "type": "integer" }, "max_cpu_cores": { "type": "number" } } },
                                            "max_restarts": { "type": "integer" },
                                            "base_backoff_secs": { "type": "integer" }
                                        }
                                    }
                                },
                                "targets": {
                                    "type": "array",
                                    "description": "A2 显式执行目标绑定（按 worker 一个目标；显式绑定不可用即等待/询问/拒绝，不静默改派）",
                                    "items": {
                                        "type": "object",
                                        "required": ["worker", "target"],
                                        "properties": {
                                            "worker": { "type": "string", "description": "绑定键：计划步骤 id 或步骤声明的 worker 名（agent 只允许 in_process）" },
                                            "target": { "type": "string", "enum": ["in_process", "local_process", "fleet_node"] },
                                            "node_id": { "type": "string", "description": "fleet_node 必填；不允许隐式选节点" },
                                            "capabilities": { "type": "array", "items": { "type": "string" } },
                                            "permission_scope": {
                                                "type": "object",
                                                "description": "默认 deny：未列出的能力一律不授予；deny 优先于 allow",
                                                "properties": {
                                                    "allow": { "type": "array", "items": { "type": "string" } },
                                                    "deny": { "type": "array", "items": { "type": "string" } },
                                                    "network_egress": { "type": "boolean", "default": false }
                                                }
                                            },
                                            "budget": { "type": "object", "properties": { "max_attempts": { "type": "integer", "description": "对 plan 步骤 retries 取 min" }, "max_duration_secs": { "type": "integer", "description": "派发等待/池预算派生上限（0=不限）" } } },
                                            "input_cas_ref": { "type": "string" },
                                            "correlation_id": { "type": "string", "description": "缺省派生 <goal_id>/<run_id>/<worker>" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } } } },
                "responses": {
                    "202": { "description": "run started" },
                    "400": { "description": "非法 execution/targets 配置（缺 workers、矛盾绑定、fleet_node 缺 node_id 等）" },
                    "404": { "description": "goal or plan not found" },
                    "422": { "description": "request body deserialization failed (unknown mode/target literal)" }
                }
            } },
            "/goal/{id}/status": { "get": { "operationId": "goalStatus", "parameters": [path_param("id")], "responses": { "200": { "description": "goal run state snapshot" } } } },
            "/goal/{id}/abort": { "post": { "operationId": "goalAbort", "parameters": [path_param("id")], "responses": { "200": { "description": "abort requested" } } } },
            "/goal/{id}/audit": { "get": { "operationId": "goalAudit", "parameters": [path_param("id")], "responses": { "200": { "description": "goal audit tail" } } } },
            "/goal/{id}/runs": { "get": { "operationId": "goalRuns", "parameters": [path_param("id")], "responses": { "200": { "description": "goal run list" } } } },
            "/cloud/tasks/{id}/events": { "get": { "operationId": "cloudTaskEvents", "parameters": [path_param("id")], "responses": { "200": { "description": "SSE progress stream for cloud task (requires Bearer)" }, "401": { "description": "missing or invalid bearer token" } } } },
            "/plugins/market": { "get": { "operationId": "pluginMarketCatalog", "responses": { "200": { "description": "plugin market catalog merged with local" } } } },
            "/plugins/market/seed": { "post": { "operationId": "pluginMarketSeed", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "entries": { "type": "array", "items": { "type": "object" } } }, "required": ["entries"] } } } }, "responses": { "200": { "description": "market seeded" } } } },
            "/plugins/market/versions": { "get": { "operationId": "pluginMarketVersions", "parameters": [{ "name": "id", "in": "query", "required": true, "schema": { "type": "string" } }, { "name": "app", "in": "query", "required": false, "schema": { "type": "string" } }], "responses": { "200": { "description": "compatible version resolution" } } } },
            "/plugins/market/verify": { "post": { "operationId": "pluginMarketVerify", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "dir": { "type": "string" } }, "required": ["dir"] } } } }, "responses": { "200": { "description": "plugin dir verified" } } } },
            "/plugins/market/install": { "post": { "operationId": "pluginMarketInstall", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "dir": { "type": "string" } }, "required": ["dir"] } } } }, "responses": { "200": { "description": "plugin installed" } } } },
            "/plugins/market/update": { "post": { "operationId": "pluginMarketUpdate", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "id": { "type": "string" }, "dir": { "type": "string" } }, "required": ["id", "dir"] } } } }, "responses": { "200": { "description": "plugin updated" } } } },
            "/plugins/market/uninstall": { "post": { "operationId": "pluginMarketUninstall", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] } } } }, "responses": { "200": { "description": "plugin uninstalled" } } } },
            "/plugins/market/scan": { "get": { "operationId": "pluginMarketScan", "parameters": [{ "name": "dir", "in": "query", "required": false, "schema": { "type": "string" } }], "responses": { "200": { "description": "risk scan summary" } } } },
            "/plugins/market/audit": { "get": { "operationId": "pluginMarketAudit", "parameters": [{ "name": "n", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "plugin market audit tail" } } } },
            "/plugins/market/refresh": { "post": { "operationId": "pluginMarketRefresh", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "url": { "type": "string" } } } } } }, "responses": { "200": { "description": "market registry refreshed" } } } },
            "/plugins/market/install-remote": { "post": { "operationId": "pluginMarketInstallRemote", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "id": { "type": "string" }, "version": { "type": "string" }, "url": { "type": "string" } }, "required": ["id"] } } } }, "responses": { "200": { "description": "remote plugin signed and installed" } } } },
            "/team/export": { "post": { "operationId": "teamExport", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "type": { "type": "string" }, "id": { "type": "string" } }, "required": ["type", "id"] } } } }, "responses": { "200": { "description": "packaged skill bytes + manifest summary" } } } },
            "/team/review": { "post": { "operationId": "teamReview", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "package_b64": { "type": "string" } }, "required": ["package_b64"] } } } }, "responses": { "200": { "description": "review findings without import" } } } },
            "/team/import": { "post": { "operationId": "teamImport", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "package_b64": { "type": "string" } }, "required": ["package_b64"] } } } }, "responses": { "200": { "description": "imported or blocked with findings" } } } },
            "/team/versions": { "get": { "operationId": "teamVersions", "parameters": [{ "name": "id", "in": "query", "required": true, "schema": { "type": "string" } }], "responses": { "200": { "description": "team package version history" } } } },
            "/team/audit": { "get": { "operationId": "teamAudit", "responses": { "200": { "description": "team api audit tail" } } } },
            // R13 WorkSwarm S0（§8.5）：多 Agent 协同运行 + Project Space + 模板注册表。
            "/teams": { "post": { "operationId": "workswarmCreateTeam", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "goal_id": { "type": "string" }, "objective": { "type": "string" }, "mode": { "type": "string", "enum": ["single", "team", "swarmflow"] }, "template_id": { "type": "string" }, "roles": { "type": "array", "items": { "type": "object" } }, "budget": { "type": "object" }, "human_policy": { "type": "string" }, "strategy": { "type": "string", "enum": ["auto", "single", "team"], "description": "五期组队策略（缺省 auto：按任务画像判定，不再盲目启用多 Agent）；未知值 400" }, "workspace": { "type": "object", "description": "六期（可选）：绑定真实项目工作区；缺省 = 服务端默认工作区", "properties": { "root": { "type": "string", "description": "项目目录（绝对路径）" }, "read_only": { "type": "boolean", "description": "缺省 true（只读）；写入需允许路径+权限审批" }, "write_allowed_paths": { "type": "array", "items": { "type": "string" }, "description": "相对 root 的允许写入路径" }, "tree_depth": { "type": "integer", "description": "目录树展示深度（1-8）" } }, "required": ["root"] } }, "required": ["objective"] } } } }, "responses": { "202": { "description": "team run created; background run loop drives phases；body 含 strategy_decision（组队决策：mode/roles/parallelism/budget_calls_total/reasons，供 UI 渲染组队理由与调用预算）与 workspace（六期绑定回显）" } } }, "get": { "operationId": "workswarmListTeams", "responses": { "200": { "description": "team run list; items = TeamRun + 进程内运行标志（R2 additive）", "content": { "application/json": { "schema": { "type": "object", "properties": { "teams": { "type": "array", "items": { "type": "object", "description": "TeamRun 字段（透传）+ 以下运行标志；additive 不改变既有字段", "properties": { "active": { "type": "boolean", "description": "运行循环正在执行阶段（人节点等待窗口 / 终态为 false）" }, "interrupted": { "type": "boolean", "description": "R2：磁盘 Running 但无活动运行 → 已识别为中断，等待显式 continue/retry 恢复" } } } } }, "required": ["teams"] } } } } } } },
            "/teams/{id}": { "get": { "operationId": "workswarmGetTeam", "parameters": [path_param("id")], "responses": { "200": { "description": "team + task view + audit tail; R2 additive: interrupted", "content": { "application/json": { "schema": { "type": "object", "properties": { "team": { "type": "object", "description": "TeamRun（透传）" }, "interrupted": { "type": "boolean", "description": "R2：中断标记（请求时先做一次幂等中断识别）" }, "tasks": { "type": "object", "description": "任务视图（步骤 × 状态）" }, "audit_tail": { "type": "array", "items": { "type": "object", "properties": { "ts": { "type": "string" }, "event": { "type": "string" }, "detail": { "type": "string" } } } } ,"worker_profiles": { "type": "array", "nullable": true, "description": "七期（二路 wire）additive：按角色 WorkerProfile（工具权限 + 调用预算）；旧记录缺失 → UI 缺省空/false/null", "items": { "type": "object", "properties": { "role": { "type": "string" }, "visible_tools": { "type": "array", "items": { "type": "string" } }, "read_only": { "type": "boolean" }, "write_allowed_paths": { "type": "array", "nullable": true, "items": { "type": "string" } }, "max_turns": { "type": "integer", "nullable": true }, "can_use_browser": { "type": "boolean" }, "can_run_command": { "type": "boolean" } }, "required": ["role", "visible_tools", "read_only", "can_use_browser", "can_run_command"] } },"write_lease": { "type": "object", "nullable": true, "description": "七期（二路 wire）additive：单一写租约（null = 未持有；取消中团队状态 stopping/stopped 渲染为 正在停止/已停止）", "properties": { "holder_role": { "type": "string" }, "holder_step_id": { "type": "string" }, "acquired_at_ms": { "type": "integer" }, "released_at_ms": { "type": "integer", "nullable": true } }, "required": ["holder_role", "holder_step_id", "acquired_at_ms"] },"changes": { "type": "array", "nullable": true, "description": "七期（二路 wire）additive：文件变更列表（与既有 workspace git-status 路由可互用；diff 预览容错读 diff / diff_content 双键）", "items": { "type": "object", "properties": { "path": { "type": "string" }, "state": { "type": "string", "enum": ["added", "modified", "deleted"] }, "added_lines": { "type": "integer", "nullable": true }, "deleted_lines": { "type": "integer", "nullable": true }, "diff": { "type": "string", "nullable": true, "description": "可选：该文件的 unified diff（UI 容错读 diff / diff_content 双键）" } }, "required": ["path", "state"] } }}, "required": ["team", "interrupted", "tasks", "audit_tail"] } } } }, "404": { "description": "unknown team" } } } },
            "/teams/{id}/tasks": { "get": { "operationId": "workswarmGetTeamTasks", "parameters": [path_param("id")], "responses": { "200": { "description": "team task graph (step x status)" } } } },
            "/teams/{id}/events": { "get": { "operationId": "workswarmTeamEvents", "parameters": [path_param("id"), { "name": "format", "in": "query", "required": false, "schema": { "type": "string", "enum": ["json"] } }], "responses": { "200": { "description": "SSE team event stream (audit replay frames {type:audit,ts,event,detail} + state frames {type:state,status,active,interrupted}; ends at terminal); ?format=json 返回一次性快照（见 content schema，R2 additive: interrupted）", "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "status": { "type": "string", "description": "Debug 格式团队状态（如 Running / Created / Completed）" }, "active": { "type": "boolean" }, "interrupted": { "type": "boolean", "description": "R2：中断标记（磁盘 Running 但无活动运行）" }, "audit": { "type": "array", "items": { "type": "object", "properties": { "ts": { "type": "string" }, "event": { "type": "string" }, "detail": { "type": "string" } } } } }, "required": ["team_id", "status", "active", "interrupted", "audit"] } } } }, "401": { "description": "missing or invalid bearer token" }, "404": { "description": "unknown team" } } } },
            "/teams/{id}/steer": { "post": { "operationId": "workswarmSteerTeam", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "command": { "type": "string", "enum": ["continue", "retry", "steer", "replace", "cancel"], "description": "R2 冻结契约：retry 局部重试 = { command: retry, step_id, note }，仅允许指定一个 Failed/Aborted/中断中的步骤" }, "step_id": { "type": "string", "description": "retry 必填（缺失/空 → 400）；steer 可选（空 = 全部未完成节点）" }, "new_input": { "type": "object", "description": "steer 专用：合并进步骤输入" }, "note": { "type": "string", "description": "retry/steer/replace 的变更理由（进入 DecisionRecord）" }, "role": { "type": "string", "description": "replace 专用：目标角色" }, "new_worker": { "type": "string", "description": "replace 专用：agent 节点新 worker" }, "new_user_id": { "type": "string", "description": "replace 专用：人节点新用户 ID" } }, "required": ["command"] } } } }, "responses": { "200": { "description": "steer applied (only uncompleted nodes; DecisionRecord kept)", "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "status": { "type": "string", "description": "Debug 格式团队状态" }, "interrupted": { "type": "boolean", "description": "R2：中断标记（continue/retry 成功恢复后为 false）" } }, "required": ["team_id", "status", "interrupted"] } } } }, "400": { "description": "validation failed（retry 缺 step_id / 未知 command）" }, "404": { "description": "unknown team or step" }, "409": { "description": "run is active（retry 目标已成功同样 409，重复发送无额外副作用）" } } } },
            "/projects/{id}": { "get": { "operationId": "workswarmGetProjectSpace", "parameters": [path_param("id")], "responses": { "200": { "description": "project space summary (tasks/artifacts/decisions/activity)" } } } },
            "/projects/{id}/artifacts": { "get": { "operationId": "workswarmListArtifacts", "parameters": [path_param("id")], "responses": { "200": { "description": "versioned shared artifacts (content via CAS ref)；五期 additive：每项含 supersedes_artifact_id（返工重跑登记时指向前版，前端按此合并版本时间线/v1v2 差异；null = 首版）", "content": { "application/json": { "schema": { "type": "object", "properties": { "project_id": { "type": "string" }, "artifacts": { "type": "array", "items": { "type": "object", "additionalProperties": true, "properties": { "artifact_id": { "type": "string" }, "kind": { "type": "string" }, "version": { "type": "integer" }, "producer": { "type": "string" }, "content_ref": { "type": "string" }, "review_state": { "type": "string" }, "supersedes_artifact_id": { "type": "string", "nullable": true }, "preview": { "type": "string" }, "validation": { "type": "object", "description": "七期（三路）additive：格式校验 {format, valid, reason?}", "properties": { "format": { "type": "string" }, "valid": { "type": "boolean" }, "reason": { "type": "string", "nullable": true } } }, "sha256": { "type": "string", "description": "七期（三路）additive：内容 SHA256（hex）" }, "size_bytes": { "type": "integer", "description": "七期（三路）additive：内容字节数" }, "evidence_refs": { "type": "array", "items": { "type": "string" }, "description": "七期（三路）additive：证据引用" } } } } }, "required": ["project_id", "artifacts"] } } } } } } },
            "/tasks/{id}/handoff": { "post": { "operationId": "workswarmSubmitHandoff", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "from_member": { "type": "string" }, "to_member": { "type": "string" }, "completed_summary": { "type": "string" }, "open_issues": { "type": "array", "items": { "type": "string" } }, "output_artifact_refs": { "type": "array", "items": { "type": "string" } }, "evidence_refs": { "type": "array", "items": { "type": "string" } }, "suggested_next_actions": { "type": "array", "items": { "type": "string" } }, "known_risks": { "type": "array", "items": { "type": "string" } } }, "required": ["team_id", "from_member"] } } } }, "responses": { "200": { "description": "structured handoff recorded" } } } },
            "/tasks/{id}/human-result": { "post": { "operationId": "workswarmSubmitHumanResult", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "result": { "type": "string" } }, "required": ["team_id", "result"] } } } }, "responses": { "200": { "description": "human node result recorded; downstream wakes automatically" } } } },
            "/teams/templates": { "get": { "operationId": "workswarmListTemplates", "responses": { "200": { "description": "adopted team templates" } } } },
            "/teams/templates/proposals": { "get": { "operationId": "workswarmListTemplateProposals", "responses": { "200": { "description": "team template proposals (proposal only, never auto-enabled)" } } } },
            "/teams/templates/proposals/{proposal_id}/adopt": { "post": { "operationId": "workswarmAdoptTemplateProposal", "parameters": [path_param("proposal_id")], "responses": { "200": { "description": "proposal adopted into template registry (idempotent)" } } } },
            "/teams/templates/proposals/{proposal_id}/reject": { "post": { "operationId": "workswarmRejectTemplateProposal", "parameters": [path_param("proposal_id")], "responses": { "200": { "description": "proposal rejected (record kept, auditable)" }, "404": { "description": "proposal not found" }, "400": { "description": "proposal already adopted" } } } },
            // V1 四期（第三路）：Artifact 评审闭环（版本链 + 不可变评审记录 + approved head）。
            "/artifacts/{id}/review": { "post": { "operationId": "artifactSubmitReview", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "decision": { "type": "string", "enum": ["approve", "request_changes", "reject"], "description": "评审决定（snake_case）" }, "reviewer": { "type": "string", "description": "评审者（member_id / user_id / 角色名）" }, "comment": { "type": "string" }, "expected_version": { "type": "integer", "description": "乐观并发目标版本；缺省跳过版本校验；不符 → 409" }, "idempotency_key": { "type": "string", "description": "幂等键；同键重放零副作用返回既有记录" } }, "required": ["team_id", "decision", "reviewer", "idempotency_key"] } } } }, "responses": { "201": { "description": "review recorded; body = { replayed: false, review, artifact, approved_head }", "content": { "application/json": { "schema": { "type": "object", "properties": { "replayed": { "type": "boolean" }, "review": { "$ref": "#/components/schemas/ArtifactReviewRecord" }, "artifact": { "type": "object", "additionalProperties": true, "description": "评审后的 Artifact（review_state 已迁移）" }, "approved_head": { "type": "object", "nullable": true, "additionalProperties": true, "description": "decision=approve 时的 (project, kind) approved head Artifact" } }, "required": ["replayed", "review", "artifact"] } } } }, "200": { "description": "idempotent replay（同幂等键重放，零副作用；replayed: true）", "content": { "application/json": { "schema": { "type": "object", "properties": { "replayed": { "type": "boolean", "description": "恒为 true（回放既有记录）" }, "review": { "$ref": "#/components/schemas/ArtifactReviewRecord" }, "artifact": { "type": "object", "additionalProperties": true, "description": "评审后的 Artifact（当前状态）" }, "approved_head": { "type": "object", "nullable": true, "additionalProperties": true, "description": "decision=approve 时的 (project, kind) approved head Artifact" } }, "required": ["replayed", "review", "artifact"] } } } }, "400": { "description": "validation failed（未知 decision；缺必填字段为 Json extractor 422）" }, "403": { "description": "producer self-approve without human policy authorization（需 self_review_allowed）" }, "404": { "description": "artifact or team not found" }, "409": { "description": "expected_version stale（旧页面提交）/ artifact superseded / idempotency key reused on other artifact" }, "422": { "description": "missing required field（team_id/decision/reviewer/idempotency_key）" } } } },
            "/artifacts/{id}/history": { "get": { "operationId": "artifactReviewHistory", "parameters": [path_param("id")], "responses": { "200": { "description": "review history (asc) + version chain (supersedes/superseded_by) + approved head", "content": { "application/json": { "schema": { "type": "object", "properties": { "artifact_id": { "type": "string" }, "kind": { "type": "string" }, "version": { "type": "integer" }, "producer": { "type": "string" }, "review_state": { "type": "string", "enum": ["draft", "pending_review", "approved", "rejected", "superseded"] }, "supersedes_artifact_id": { "type": "string", "nullable": true }, "superseded_by": { "type": "string", "nullable": true }, "reviews": { "type": "array", "items": { "$ref": "#/components/schemas/ArtifactReviewRecord" } }, "approved_head": { "type": "object", "nullable": true, "additionalProperties": true, "description": "(project, kind) 的当前 approved head Artifact（指向真实存在且已批准版本；否则 null）" } }, "required": ["artifact_id", "kind", "version", "producer", "review_state", "reviews", "approved_head"] } } } }, "404": { "description": "artifact not found" } } } },
            // 五期（第二路）：Artifact 自动返工与最终交付物。
            "/artifacts/{id}/rework": { "post": { "operationId": "artifactSubmitRework", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "review_id": { "type": "string", "description": "要求修改（request_changes）评审记录 id；同一评审仅允许一个返工任务" }, "instruction": { "type": "string", "description": "返工指令（注入重跑步骤输入 rework.instruction）" }, "idempotency_key": { "type": "string", "description": "幂等键；同键/同评审重复请求幂等返回原任务" } }, "required": ["team_id", "review_id", "instruction"] } } } }, "responses": { "201": { "description": "rework task created（重置原步骤及未完成下游，重跑后登记 v2 并标 v1 Superseded；approved head 不变直至 v2 批准）" }, "200": { "description": "idempotent replay（同评审/同幂等键重复请求，返回原返工任务）" }, "404": { "description": "artifact / team / review not found" }, "409": { "description": "该评审已创建过返工任务（幂等冲突）或团队状态不允许" } } } },
            // 七期（第三路）：Artifact 交付（内容/元数据）+ 项目交付清单。路由实现位于第三路
            // 模块 artifact_delivery_api.rs，经 artifact_review_api::router 内部合并挂载
            //（#[path] 子模块 artifact_delivery，见 build_router 中既有 .merge(artifact_review_api::router)），
            // 无需本文件额外 merge（重复挂载同路径会 panic）；第四路完成 UI/TS 契约收口。
            "/artifacts/{id}/content": { "get": { "operationId": "artifactContent", "parameters": [path_param("id")], "responses": { "200": { "description": "Artifact 下载/预览载荷（content = 原始文本，非 JSON 编码）", "content": { "application/json": { "schema": { "type": "object", "properties": { "artifact_id": { "type": "string" }, "format": { "type": "string", "enum": ["json", "csv", "research", "markdown"] }, "sha256": { "type": "string" }, "size_bytes": { "type": "integer" }, "content": { "type": "string" } }, "required": ["artifact_id", "format", "sha256", "size_bytes", "content"] } } } }, "404": { "description": "artifact not found" } } } },
            "/artifacts/{id}/metadata": { "get": { "operationId": "artifactMetadata", "parameters": [path_param("id")], "responses": { "200": { "description": "Artifact 元数据：格式校验结果 + SHA256 + 证据引用（handoff 可选，位于既有 handoff 键下）", "content": { "application/json": { "schema": { "type": "object", "properties": { "artifact_id": { "type": "string" }, "team_id": { "type": "string" }, "kind": { "type": "string" }, "format": { "type": "string", "enum": ["json", "csv", "research", "markdown"] }, "version": { "type": "integer" }, "sha256": { "type": "string" }, "size_bytes": { "type": "integer" }, "validation": { "type": "object", "properties": { "format": { "type": "string" }, "valid": { "type": "boolean" }, "reason": { "type": "string", "nullable": true } }, "required": ["format", "valid"] }, "evidence_refs": { "type": "array", "items": { "type": "string" } }, "handoff": { "type": "object", "nullable": true, "additionalProperties": true, "description": "可选：WorkerOutputV1 handoff 记录（谁完成/遗留问题/下一步建议）" } }, "required": ["artifact_id", "team_id", "kind", "format", "version", "sha256", "size_bytes", "validation", "evidence_refs"] } } } }, "404": { "description": "artifact not found" } } } },
            "/projects/{id}/delivery-manifest": { "get": { "operationId": "projectDeliveryManifest", "parameters": [path_param("id")], "responses": { "200": { "description": "项目交付清单（approved 版本概览 + 内容端点相对路径 content_url，供下载/校验）", "content": { "application/json": { "schema": { "type": "object", "properties": { "project_id": { "type": "string" }, "generated_at": { "type": "string" }, "manifest": { "type": "array", "items": { "type": "object", "properties": { "artifact_id": { "type": "string" }, "kind": { "type": "string" }, "format": { "type": "string" }, "version": { "type": "integer" }, "sha256": { "type": "string" }, "size_bytes": { "type": "integer" }, "approved": { "type": "boolean" }, "content_url": { "type": "string", "description": "内容端点相对路径（GET /artifacts/{id}/content）" } }, "required": ["artifact_id", "kind", "format", "version", "sha256", "size_bytes", "approved", "content_url"] } } }, "required": ["project_id", "generated_at", "manifest"] } } } }, "404": { "description": "project not found" } } } },
            "/projects/{id}/deliverables": { "get": { "operationId": "projectDeliverables", "parameters": [path_param("id")], "responses": { "200": { "description": "最终交付物视图：approved = 各 kind 当前已批准版本（供下载/继续使用）；pending = 待评审；rejected_or_superseded = 被驳回/被取代版本（历史保留）", "content": { "application/json": { "schema": { "type": "object", "properties": { "project_id": { "type": "string" }, "approved": { "type": "array", "items": { "type": "object", "additionalProperties": true } }, "pending": { "type": "array", "items": { "type": "object", "additionalProperties": true } }, "rejected_or_superseded": { "type": "array", "items": { "type": "object", "additionalProperties": true } } }, "required": ["project_id", "approved", "pending", "rejected_or_superseded"] } } } }, "404": { "description": "project not found" } } } },
            // 五期（第三路）：TeamRun 角色指标与脱敏诊断导出。
            "/teams/{id}/metrics": { "get": { "operationId": "teamMetrics", "parameters": [path_param("id")], "responses": { "200": { "description": "角色指标：workers[]（起止/耗时/模型调用/token/估算费用/尝试/终态/失败原因/输出 Artifact）+ summary（总墙钟/总调用/总费用/最慢 Worker/失败/返工/产物版本数/预算耗尽原因）；JSONL 持久化，重启可读", "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "workers": { "type": "array", "items": { "type": "object", "additionalProperties": true } }, "summary": { "type": "object", "additionalProperties": true } }, "required": ["team_id"] } } } }, "404": { "description": "team not found" } } } },
            "/teams/{id}/diagnostic": { "get": { "operationId": "teamDiagnostic", "parameters": [path_param("id")], "responses": { "200": { "description": "脱敏诊断导出（TeamRun/任务状态/Artifact 与评审记录/Handoff/指标/审计尾迹；凭据、Authorization、完整敏感输入已脱敏）", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true } } } }, "404": { "description": "team not found" } } } },
            // 六期（第二路）：Project Workspace 真实工作区绑定（默认只读；写入需允许路径+权限审批；规范化阻止 .. / symlink / Junction 越界）。
            "/projects/{id}/workspace": {
              "put": { "operationId": "projectBindWorkspace", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "root": { "type": "string" }, "read_only": { "type": "boolean" }, "write_allowed_paths": { "type": "array", "items": { "type": "string" } }, "tree_depth": { "type": "integer" } }, "required": ["root"] } } } }, "responses": { "200": { "description": "工作区绑定已写入（body = 绑定回显：project_id/root/read_only/write_allowed_paths/tree_depth/bound_at）" }, "400": { "description": "root 不存在/非法或越界路径" }, "404": { "description": "project not found" } } },
              "get": { "operationId": "projectGetWorkspace", "parameters": [path_param("id")], "responses": { "200": { "description": "当前工作区绑定，body = {workspace:{project_id/root/read_only/write_allowed_paths/tree_depth/created_at/root_canonical/write_allowed_canonical}}（UI 容错同时接受顶层形状）", "content": { "application/json": { "schema": { "type": "object", "properties": { "workspace": { "type": "object", "additionalProperties": true, "properties": { "project_id": { "type": "string" }, "team_id": { "type": "string" }, "root": { "type": "string" }, "read_only": { "type": "boolean" }, "write_allowed_paths": { "type": "array", "items": { "type": "string" } }, "tree_depth": { "type": "integer", "nullable": true } } } }, "required": ["workspace"] } } } }, "404": { "description": "project not found" } } }
            },
            "/projects/{id}/workspace/tree": { "get": { "operationId": "projectWorkspaceTree", "parameters": [path_param("id"), { "name": "depth", "in": "query", "schema": { "type": "integer", "description": "目录树深度（1-8，缺省用绑定值）" } }], "responses": { "200": { "description": "扁平目录树（entries:[{path,type:dir|file,size?}]；root/truncated 附加；UI 按路径层级缩进渲染）；工作区未绑定/不存在 → 404", "content": { "application/json": { "schema": { "type": "object", "properties": { "root": { "type": "string" }, "depth": { "type": "integer" }, "truncated": { "type": "boolean" }, "entries": { "type": "array", "items": { "type": "object", "additionalProperties": true, "properties": { "path": { "type": "string" }, "type": { "type": "string", "enum": ["dir", "file"] }, "size": { "type": "integer", "nullable": true } } } } }, "required": ["entries"] } } } }, "404": { "description": "project / workspace not found" } } } },
            "/workspace/tree": { "get": { "operationId": "previewWorkspaceTree", "parameters": [ { "name": "root", "in": "query", "required": true, "schema": { "type": "string", "description": "候选工作区根目录（绝对路径）" } }, { "name": "depth", "in": "query", "schema": { "type": "integer", "description": "目录树深度（1-8，缺省 2）" } } ], "responses": { "200": { "description": "预绑定目录树预览（形状与 /projects/{id}/workspace/tree 一致；root 为 canonical 回显）", "content": { "application/json": { "schema": { "type": "object", "properties": { "root": { "type": "string" }, "depth": { "type": "integer" }, "truncated": { "type": "boolean" }, "entries": { "type": "array", "items": { "type": "object", "additionalProperties": true, "properties": { "path": { "type": "string" }, "type": { "type": "string", "enum": ["dir", "file"] }, "size": { "type": "integer", "nullable": true } } } } }, "required": ["entries"] } } } }, "400": { "description": "root 缺失/非绝对路径/目录不存在/非目录" } } } },
            "/projects/{id}/workspace/git-status": { "get": { "operationId": "projectWorkspaceGitStatus", "parameters": [path_param("id")], "responses": { "200": { "description": "Git 工作区状态：entries = git status --porcelain 行字符串数组（如 \" M path\"/\"?? path\"），另有 root/git（布尔：是否 Git 仓库）/porcelain（原文）；非 Git 仓库 entries 为空数组，不报错", "content": { "application/json": { "schema": { "type": "object", "properties": { "root": { "type": "string" }, "git": { "type": "boolean" }, "porcelain": { "type": "string" }, "entries": { "type": "array", "items": { "type": "string" } } }, "required": ["entries"] } } } }, "404": { "description": "project not found" } } } },
            // 七期（第二路 wire）：Worker 代码变更追踪读取面（workspace_change_tracker 落盘 JSON 的聚合视图）。
            "/projects/{id}/workspace/changes": { "get": { "operationId": "projectWorkspaceChanges", "parameters": [path_param("id")], "responses": { "200": { "description": "七期（二路 wire）additive：Worker 代码变更追踪——changed_files = 全部记录窗口新增变更文件（去重保序）；diff_summary = 最近一条记录的 git diff --stat 摘要；has_violation = 是否存在白名单越界记录（scope_violation）；records = 逐步骤记录（角色/步骤/时刻/变更文件/diff ref/越界原因）；无记录（只读团队/未执行写角色）返回空 records，UI 全字段容错读取", "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "git": { "type": "boolean", "description": "最近一条记录是否取得有效 git 快照；无记录为 false" }, "changed_files": { "type": "array", "items": { "type": "string" } }, "diff_summary": { "type": "string", "description": "最近一条记录的 git diff --stat 文本；无记录为空串" }, "has_violation": { "type": "boolean" }, "records": { "type": "array", "items": { "type": "object", "additionalProperties": true, "properties": { "role": { "type": "string" }, "step": { "type": "string" }, "at": { "type": "integer", "description": "记录时刻（Unix 毫秒）" }, "git": { "type": "boolean" }, "changed_files": { "type": "array", "items": { "type": "string" } }, "diff_summary": { "type": "string" }, "diff_ref": { "type": "string", "nullable": true, "description": "diff 补丁文件相对 run_dir 路径；非 git / 无变更为 null" }, "violation": { "type": "string", "nullable": true, "description": "白名单越界原因（scope_violation）；null = 通过" } } } } }, "required": ["team_id", "git", "changed_files", "diff_summary", "has_violation", "records"] } } } }, "404": { "description": "project / team not found" } } } },
            // 八期（第二路 wire）：ChangeSet 审批闭环——写角色执行前快照 / 执行后生成，可接受/拒绝/安全撤销。
            "/teams/{id}/change-sets": { "get": { "operationId": "teamChangeSets", "parameters": [path_param("id")], "responses": { "200": { "description": "八期（二路 wire）：团队的 ChangeSet 列表——change_set 元素含 change_set_id/team_id/step_id/role/base_hashes/result_hashes/changed_files/diff_ref/status/decisions/created_at/resolved_at；status ∈ pending_review|accepted|rejected|reverted|conflicted；未接受 ChangeSet 阻断该团队最终 approved head（change_set_store::approval_block_for_team 门控），UI 全字段容错读取", "content": { "application/json": { "schema": { "type": "object", "properties": { "team_id": { "type": "string" }, "change_sets": { "type": "array", "items": { "type": "object", "additionalProperties": true, "properties": { "change_set_id": { "type": "string" }, "team_id": { "type": "string" }, "step_id": { "type": "string" }, "role": { "type": "string" }, "base_hashes": { "type": "object", "additionalProperties": true, "description": "执行前文件内容哈希（内容进 CAS）" }, "result_hashes": { "type": "object", "additionalProperties": true, "description": "执行后文件内容哈希" }, "changed_files": { "type": "array", "items": { "type": "string" } }, "diff_ref": { "type": "string", "nullable": true }, "status": { "type": "string", "enum": ["pending_review", "accepted", "rejected", "reverted", "conflicted"] }, "decisions": { "type": "array", "items": { "type": "object", "additionalProperties": true }, "description": "幂等决定记录（accept/reject/revert 只增不改）" }, "created_at": { "type": "string" }, "resolved_at": { "type": "string", "nullable": true } } } } }, "required": ["team_id", "change_sets"] } } } }, "404": { "description": "team not found" } } } },
            "/change-sets/{id}": { "get": { "operationId": "changeSetDetail", "parameters": [path_param("id")], "responses": { "200": { "description": "单个 ChangeSet（形状同 teamChangeSets.change_sets 元素）", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "required": ["change_set_id", "status"], "properties": { "change_set_id": { "type": "string" }, "team_id": { "type": "string" }, "step_id": { "type": "string" }, "role": { "type": "string" }, "changed_files": { "type": "array", "items": { "type": "string" } }, "diff_ref": { "type": "string", "nullable": true }, "status": { "type": "string", "enum": ["pending_review", "accepted", "rejected", "reverted", "conflicted"] }, "created_at": { "type": "string" }, "resolved_at": { "type": "string", "nullable": true } } } } } }, "404": { "description": "change set not found" } } } },
            "/change-sets/{id}/accept": { "post": { "operationId": "changeSetAccept", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "required": ["idempotency_key"], "properties": { "idempotency_key": { "type": "string", "description": "幂等键：同键重放返回 replayed:true 且零副作用" } } } } } }, "responses": { "200": { "description": "接受变更（幂等重放返回 replayed:true 且零副作用）；accept 保留修改并解除该 ChangeSet 对最终 approved head 的阻断", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "properties": { "change_set": { "type": "object", "additionalProperties": true }, "replayed": { "type": "boolean" } } } } } }, "404": { "description": "change set not found" }, "409": { "description": "已终态（跨动作）或文件被用户再次修改（status=conflicted，不覆盖用户新内容）" } } } },
            "/change-sets/{id}/reject": { "post": { "operationId": "changeSetReject", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "required": ["idempotency_key"], "properties": { "idempotency_key": { "type": "string", "description": "幂等键：同键重放返回 replayed:true 且零副作用" } } } } } }, "responses": { "200": { "description": "拒绝变更（幂等重放返回 replayed:true 且零副作用）；reject 仅恢复该 ChangeSet 修改的文件（新建文件=删除恢复），恢复前逐文件比对当前哈希", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "properties": { "change_set": { "type": "object", "additionalProperties": true }, "replayed": { "type": "boolean" } } } } } }, "404": { "description": "change set not found" }, "409": { "description": "文件被用户再次修改（status=conflicted，不覆盖用户新内容）或已终态跨动作" } } } },
            "/change-sets/{id}/revert": { "post": { "operationId": "changeSetRevert", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "required": ["idempotency_key"], "properties": { "idempotency_key": { "type": "string", "description": "幂等键：同键重放返回 replayed:true 且零副作用" } } } } } }, "responses": { "200": { "description": "安全撤销（幂等重放返回 replayed:true 且零副作用）；仅恢复该 ChangeSet 修改的文件，恢复前比较当前文件哈希：=结果哈希→恢复、=基线哈希→已恢复跳过、否则 409 + status=conflicted", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "properties": { "change_set": { "type": "object", "additionalProperties": true }, "replayed": { "type": "boolean" } } } } } }, "404": { "description": "change set not found" }, "409": { "description": "文件被用户再次修改（status=conflicted，不覆盖用户新内容）或已终态跨动作" } } } },
            // 八期（第三路 wire）：统一 Human Inbox——四类待办（human_result/artifact_review/change_set/step_retry）持久化、领取与直接处理。
            "/human/inbox":{"get":{"operationId":"humanInboxList","parameters":[{"name":"kind","in":"query","required":false,"schema":{"type":"string","enum":["human_result","artifact_review","change_set","step_retry"]}},{"name":"status","in":"query","required":false,"schema":{"type":"string","enum":["open","claimed","resolved"]}},{"name":"team_id","in":"query","required":false,"schema":{"type":"string"}},{"name":"project_id","in":"query","required":false,"schema":{"type":"string"}}],"responses":{"200":{"description":"统一待办列表：items 元素含 item_id/kind/status/assignee/team_id/project_id/target_id/summary/created_at/claimed_at/resolved_at/detail（detail 为按 kind 的领域上下文，开放对象）；counts = 按 kind 计数；服务重启后未处理事项恢复，已解决事项不再出现","content":{"application/json":{"schema":{"type":"object","properties":{"items":{"type":"array","items":{"type":"object","additionalProperties":true,"properties":{"item_id":{"type":"string"},"kind":{"type":"string","enum":["human_result","artifact_review","change_set","step_retry"]},"status":{"type":"string","enum":["open","claimed","resolved"]},"assignee":{"type":"string","nullable":true},"team_id":{"type":"string"},"project_id":{"type":"string"},"target_id":{"type":"string"},"summary":{"type":"string"},"created_at":{"type":"string"},"claimed_at":{"type":"string","nullable":true},"resolved_at":{"type":"string","nullable":true},"detail":{"type":"object","additionalProperties":true}}}},"counts":{"type":"object","additionalProperties":true}},"required":["items","counts"]}}}}}}},
            "/human/inbox/{id}": { "get": { "operationId": "humanInboxItem", "parameters": [path_param("id")], "responses": { "200": { "description": "单条待办（形状同 humanInboxList.items 元素）", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "properties": { "item": { "type": "object", "additionalProperties": true } } } } } }, "404": { "description": "待办不存在" } } } },
            "/human/inbox/{id}/claim": { "post": { "operationId": "humanInboxClaim", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "user": { "type": "string" } }, "required": ["user"] } } } }, "responses": { "200": { "description": "领取成功（同人重复领取幂等返回）{item}" }, "400": { "description": "user 为空" }, "404": { "description": "待办不存在" }, "409": { "description": "已被其他用户领取（同一待办仅允许一个用户处理）" } } } },
            "/human/inbox/{id}/release": { "post": { "operationId": "humanInboxRelease", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "user": { "type": "string" } }, "required": ["user"] } } } }, "responses": { "200": { "description": "释放成功（仅领取者可释放）{item}" }, "400": { "description": "user 为空" }, "404": { "description": "待办不存在" }, "409": { "description": "已被其他用户领取或当前状态不允许" } } } },
            "/human/inbox/{id}/resolve": { "post": { "operationId": "humanInboxResolve", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "properties": { "user": { "type": "string", "description": "处理人（审计/领取校验；review 类缺省兼作 reviewer）" }, "resolution_id": { "type": "string", "description": "幂等键（兼容别名 idempotency_key）" }, "decision": { "type": "string", "enum": ["approve", "request_changes", "reject"], "description": "artifact_review" }, "action": { "type": "string", "enum": ["accept", "reject"], "description": "change_set" }, "result": { "type": "string", "description": "human_result 结果文本" }, "reviewer": { "type": "string" }, "comment": { "type": "string" }, "expected_version": { "type": "integer" }, "note": { "type": "string" } } } } } }, "responses": { "200": { "description": "按 kind 分派到既有领域能力（Human 结果提交 / Artifact 评审 / ChangeSet accept/reject / Step retry；不绕过原权限与幂等检查）后返回 {resolved:true, replayed, item_id, kind, result}；重放幂等 200", "content": { "application/json": { "schema": { "type": "object", "additionalProperties": true, "properties": { "resolved": { "type": "boolean" }, "replayed": { "type": "boolean" }, "item_id": { "type": "string" }, "kind": { "type": "string" }, "result": { "type": "object", "additionalProperties": true } } } } } }, "400": { "description": "请求体与 kind 不匹配" }, "404": { "description": "待办不存在" }, "409": { "description": "未被领取/已被他人领取/领域冲突（如 artifact 已终态）" } } } },
            // 六期（第三路）：内置团队模板目录（候选仅展示；手动安装后参与自动匹配；安装幂等且不扩大权限）。
            "/teams/templates/catalog": { "get": { "operationId": "teamTemplateCatalog", "responses": { "200": { "description": "内置模板目录（候选区）：每项含 installed 与 template 对象（template_id/name/mode/roles[{role,depends_on,handoff_contract}]）+ budget_calls_per_role[]/completion_criteria[]/tool_scope/artifact_kinds[]/auto_match_keywords；安装经 POST /teams/templates/catalog/{id}/install 后 installed=true 并参与自动匹配", "content": { "application/json": { "schema": { "type": "object", "properties": { "catalog": { "type": "array", "items": { "type": "object", "additionalProperties": true, "properties": { "installed": { "type": "boolean" }, "template": { "type": "object", "additionalProperties": true, "properties": { "template_id": { "type": "string" }, "name": { "type": "string" }, "mode": { "type": "string" } } } } } } }, "required": ["catalog"] } } } } } } },
            "/teams/templates/catalog/{id}/install": { "post": { "operationId": "teamTemplateCatalogInstall", "parameters": [path_param("id")], "responses": { "200": { "description": "首次安装 {installed:true, already_installed:false, template, auto_match, budget_hint}；幂等重放 {installed:false, already_installed:true, note}（不覆盖既有同名模板）。安装后模板进入注册表并参与自动匹配（find_match），不自动扩大文件/命令/网络权限" }, "404": { "description": "template not found in catalog" } } } },
            // R1 DesktopWorld/WorldModel 训练闭环（§8.5/§5.11-5.12）：仿真环境 + 租约 fencing + transition 语料 + 世界模型候选/晋升。
            "/desktop-envs": { "post": { "operationId": "desktopWorldCreateEnv", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "env_id": { "type": "string" }, "task": { "type": "object", "properties": { "task_id": { "type": "string" }, "app": { "type": "string" }, "seed": { "type": "integer" }, "assets": { "type": "object" } }, "required": ["task_id", "app", "seed"] }, "owner": { "type": "string" } }, "required": ["task"] } } } }, "responses": { "200": { "description": "env created: initial lease proof + first-frame WorldStateV1 observation" }, "409": { "description": "env_id already exists" } } } },
            "/desktop-envs/{id}/reset": { "post": { "operationId": "desktopWorldResetEnv", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "task": { "type": "object", "properties": { "task_id": { "type": "string" }, "app": { "type": "string" }, "seed": { "type": "integer" }, "assets": { "type": "object" } }, "required": ["task_id", "app", "seed"] }, "lease": { "type": "object", "properties": { "owner": { "type": "string" }, "token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["owner", "token", "epoch"] } }, "required": ["task", "lease"] } } } }, "responses": { "200": { "description": "env reset to task initial state; lease proof returned" }, "404": { "description": "env not found" }, "409": { "description": "lease fencing conflict (stale token/epoch)" } } } },
            "/desktop-envs/{id}/lease": { "post": { "operationId": "desktopWorldLeaseOp", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "op": { "type": "string", "enum": ["acquire", "renew", "release"] }, "lease": { "type": "object", "properties": { "owner": { "type": "string" }, "token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["owner", "token", "epoch"] }, "owner": { "type": "string" } }, "required": ["op"] } } } }, "responses": { "200": { "description": "lease acquired/renewed/released; current lease record returned" }, "404": { "description": "env not found" }, "409": { "description": "lease fencing conflict" }, "400": { "description": "renew/release missing lease proof" } } } },
            "/desktop-envs/{id}/observe": { "get": { "operationId": "desktopWorldObserveEnv", "parameters": [path_param("id")], "responses": { "200": { "description": "current WorldStateV1 observation (scene graph + window stack)" }, "404": { "description": "env not found" } } } },
            "/desktop-envs/{id}/step": { "post": { "operationId": "desktopWorldStepEnv", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "action": { "type": "object", "properties": { "action_id": { "type": "string" }, "kind": { "type": "string" }, "semantic_intent": { "type": "string" }, "target_id": { "type": "string" }, "arguments": { "type": "object" }, "risk": { "type": "string" }, "reversible": { "type": "boolean" } }, "required": ["action_id", "kind", "semantic_intent"] }, "lease": { "type": "object", "properties": { "owner": { "type": "string" }, "token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["owner", "token", "epoch"] }, "episode_id": { "type": "string" }, "record": { "type": "boolean" }, "task_goal": { "type": "string" }, "history": { "type": "array", "items": { "type": "string" } } }, "required": ["action", "lease"] } } } }, "responses": { "200": { "description": "step executed: before/after state refs, transition id, verdict + reward parts, shadow prediction evaluation" }, "404": { "description": "env not found" }, "409": { "description": "lease fencing conflict (stale token/epoch)" } } } },
            "/desktop-envs/{id}/snapshot": { "post": { "operationId": "desktopWorldSnapshotEnv", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object" } } } }, "responses": { "200": { "description": "snapshot persisted; snapshot_id returned (read path, no lease)" }, "404": { "description": "env not found" } } } },
            "/desktop-envs/{id}/restore": { "post": { "operationId": "desktopWorldRestoreEnv", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "snapshot": { "type": "string" }, "lease": { "type": "object", "properties": { "owner": { "type": "string" }, "token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["owner", "token", "epoch"] } }, "required": ["snapshot", "lease"] } } } }, "responses": { "200": { "description": "env restored from snapshot; observation returned" }, "404": { "description": "env or snapshot not found" }, "409": { "description": "lease fencing conflict" } } } },
            "/desktop-envs/{id}/judge": { "post": { "operationId": "desktopWorldJudgeEnv", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "success": { "type": "object", "properties": { "name": { "type": "string" }, "assertions": { "type": "array", "items": { "type": "object" } } }, "required": ["name", "assertions"] } }, "required": ["success"] } } } }, "responses": { "200": { "description": "verdict against success spec (read path, no lease)" }, "404": { "description": "env not found" } } } },
            "/desktop-envs/{id}/inject-fault": { "post": { "operationId": "desktopWorldInjectFault", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "fault": { "type": "object", "properties": { "type": { "type": "string", "enum": ["modal_popup", "element_drift", "sluggish_steps"] }, "text": { "type": "string" }, "element_id": { "type": "string" }, "dx": { "type": "integer" }, "dy": { "type": "integer" }, "steps": { "type": "integer" } }, "required": ["type"] }, "lease": { "type": "object", "properties": { "owner": { "type": "string" }, "token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["owner", "token", "epoch"] } }, "required": ["fault", "lease"] } } } }, "responses": { "200": { "description": "fault injected into env" }, "404": { "description": "env not found" }, "409": { "description": "lease fencing conflict" } } } },
            "/world-model/predict": { "post": { "operationId": "worldModelPredict", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "env_id": { "type": "string" }, "action": { "type": "object", "properties": { "action_id": { "type": "string" }, "kind": { "type": "string" }, "semantic_intent": { "type": "string" }, "target_id": { "type": "string" }, "arguments": { "type": "object" } }, "required": ["action_id", "kind", "semantic_intent"] }, "context": { "type": "object" }, "with_advice": { "type": "boolean" }, "candidates": { "type": "array", "items": { "type": "object" } } }, "required": ["env_id", "action"] } } } }, "responses": { "200": { "description": "predicted structural state diff + probability (read path, env unchanged); advice with candidates when with_advice" }, "404": { "description": "env not found" }, "400": { "description": "no active world model (empty transition corpus)" } } } },
            "/world-model/providers": { "get": { "operationId": "worldModelProviders", "responses": { "200": { "description": "active rule model + candidates + per-signature samples + calibration report" } } } },
            "/transitions/{id}": { "get": { "operationId": "transitionGet", "parameters": [path_param("id")], "responses": { "200": { "description": "TransitionTraceV1 record" }, "404": { "description": "transition not found" } } } },
            "/datasets/build": { "post": { "operationId": "datasetBuild", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "config": { "type": "object" }, "env_id": { "type": "string" }, "episode_id": { "type": "string" }, "task_id": { "type": "string" } } } } } }, "responses": { "200": { "description": "dataset built + split; manifest with dataset_id returned" }, "400": { "description": "no transition corpus to build from" } } } },
            "/datasets/{id}/manifest": { "get": { "operationId": "datasetManifest", "parameters": [path_param("id")], "responses": { "200": { "description": "DatasetManifest (splits + counts + build config)" }, "404": { "description": "dataset not found" } } } },
            "/model-candidates": { "post": { "operationId": "modelCandidateRegister", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "candidate_id": { "type": "string", "description": "缺省自动生成 model_id-model_version-<uuid8>" }, "model_id": { "type": "string" }, "model_version": { "type": "string" }, "source": { "type": "string", "description": "来源说明（缺省：手动注册 shadow 起步）" }, "provider_ref": { "$ref": "#/components/schemas/CandidateProviderRef", "description": "可执行 provider 身份声明（缺省 metadata_only；声明≠接线，还需进程内真实接线才积累影子样本）" } }, "required": ["model_id", "model_version"] } } } }, "responses": { "200": { "description": "candidate registered as shadow (never auto-activated); body = ModelCandidate", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/ModelCandidate" } } } }, "409": { "description": "candidate_id already exists" } } } },
            "/model-candidates/{id}/promote": { "post": { "operationId": "modelCandidatePromote", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "ack": { "type": "boolean" }, "reason": { "type": "string" } }, "required": ["ack", "reason"] } } } }, "responses": { "200": { "description": "candidate promoted to active provider (human ack required); body = { candidate, active, previous_active, samples, gates }", "content": { "application/json": { "schema": { "type": "object", "properties": { "candidate": { "$ref": "#/components/schemas/ModelCandidate" }, "active": { "type": "string" }, "previous_active": { "type": "string", "nullable": true }, "samples": { "type": "integer", "format": "int64", "description": "真实影子样本数" }, "gates": { "type": "object", "description": "晋升门控明细快照（审计口径）", "properties": { "min_shadow_samples": { "type": "integer", "format": "int64" }, "provider_wired": { "type": "object", "properties": { "kind": { "type": "string" }, "locator": { "type": "string" } }, "required": ["kind", "locator"] }, "calibration_summary": { "allOf": [{ "$ref": "#/components/schemas/CalibrationReport" }] }, "regression_check": { "type": "object", "nullable": true, "description": "相对上一任 active 的退化检查（无前任或前任无样本时为 null）", "properties": { "previous_active": { "type": "string" }, "previous_samples": { "type": "integer", "format": "int64" }, "hit_rate_delta": { "type": "number" }, "mean_delta_jaccard_delta": { "type": "number" }, "mean_calibration_error_delta": { "type": "number" }, "max_regression_delta": { "type": "number" }, "passed": { "type": "boolean" } } } }, "required": ["min_shadow_samples", "provider_wired", "calibration_summary"] } }, "required": ["candidate", "active", "previous_active", "samples", "gates"] } } } }, "404": { "description": "candidate not found" }, "400": { "description": "ack=false or empty reason" }, "422": { "description": "governance gate refused（metadata_only / 未接线 / 样本不足 / 相对前任退化超阈值）" } } } },
            "/eval/gate/run": { "post": { "operationId": "evalGateRun", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "suite": { "type": "string" }, "model": { "type": "string" } } } } } }, "responses": { "200": { "description": "eval report or skipped reason" } } } },
            "/eval/gate/report": { "get": { "operationId": "evalGateReport", "responses": { "200": { "description": "latest eval report" } } } },
            "/eval/gate/reports": { "get": { "operationId": "evalGateReports", "responses": { "200": { "description": "eval report history" } } } },
            "/schemas": { "get": { "operationId": "schemasList", "responses": { "200": { "description": "JSON Schema 版本化发布索引（plugin-manifest/owskill/owflow）" } } } },
            "/schemas/{kind}/{version}": { "get": { "operationId": "schemaGet", "parameters": [path_param("kind"), path_param("version")], "responses": { "200": { "description": "JSON Schema (draft-07)" } } } },
            "/metrics/overview": { "get": { "operationId": "metricsOverview", "responses": { "200": { "description": "aggregated traces/tools/approvals metrics" } } } },
            "/metrics/turns": { "get": { "operationId": "metricsTurns", "parameters": [{ "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "recent turn durations" } } } },
            "/metrics/tools": { "get": { "operationId": "metricsTools", "responses": { "200": { "description": "tool call frequency and failure ranking" } } } },
            "/metrics/health": { "get": { "operationId": "metricsHealth", "responses": { "200": { "description": "component health checklist" } } } },
            "/memory/graph/entries": { "get": { "operationId": "memoryGraphEntries", "parameters": [{ "name": "app", "in": "query", "required": false, "schema": { "type": "string" } }, { "name": "from", "in": "query", "required": false, "schema": { "type": "string" } }, { "name": "to", "in": "query", "required": false, "schema": { "type": "string" } }, { "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "structured memory entries" } } } },
            "/memory/graph/timeline": { "get": { "operationId": "memoryGraphTimeline", "parameters": [{ "name": "from", "in": "query", "required": false, "schema": { "type": "string" } }, { "name": "to", "in": "query", "required": false, "schema": { "type": "string" } }], "responses": { "200": { "description": "time-bucketed timeline" } } } },
            "/memory/graph/entities": { "get": { "operationId": "memoryGraphEntities", "parameters": [{ "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "entity/tag aggregation" } } } },
            "/memory/graph/links": { "get": { "operationId": "memoryGraphLinks", "responses": { "200": { "description": "manual relation list" } } } },
            "/memory/graph/link": { "post": { "operationId": "memoryGraphLinkAdd", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "a": { "type": "string" }, "b": { "type": "string" }, "relation": { "type": "string" }, "note": { "type": "string" } }, "required": ["a", "b", "relation"] } } } }, "responses": { "201": { "description": "relation added" } } }, "delete": { "operationId": "memoryGraphLinkDelete", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "a": { "type": "string" }, "b": { "type": "string" }, "relation": { "type": "string" } }, "required": ["a", "b", "relation"] } } } }, "responses": { "200": { "description": "relation removed" } } } },
            "/memory/graph/recall": { "get": { "operationId": "memoryGraphRecall", "parameters": [{ "name": "q", "in": "query", "required": true, "schema": { "type": "string" } }, { "name": "top_k", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "recall with entity hits" } } } },
            "/intent/parse": { "post": { "operationId": "intentParse", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] } } } }, "responses": { "200": { "description": "parsed intent with args and confidence" } } } },
            "/command/run": { "post": { "operationId": "commandRun", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "mode": { "type": "string" }, "text": { "type": "string" }, "wav_b64": { "type": "string" } }, "required": ["mode"] } } } }, "responses": { "200": { "description": "intent routed to action with results" } } } },
            "/command/audit": { "get": { "operationId": "commandAudit", "responses": { "200": { "description": "command execution audit tail" } } } },
            "/workflow/run/{run_id}/approval": { "post": { "operationId": "workflowRunApproval", "parameters": [path_param("run_id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "decision": { "type": "string" } }, "required": ["decision"] } } } }, "responses": { "200": { "description": "approval decision recorded" } } } },
            "/workflow/run/{run_id}/events": { "get": { "operationId": "workflowRunEvents", "parameters": [path_param("run_id")], "responses": { "200": { "description": "SSE run event stream (requires Bearer)" }, "401": { "description": "missing or invalid bearer token" } } } },
            "/events/stream": { "get": { "operationId": "eventsStream", "security": [{ "bearerAuth": [] }], "parameters": [{ "name": "last_event_id", "in": "query", "required": false, "schema": { "type": "integer" }, "description": "续传起点（调试/脚本用；缺省=新订阅只收新事件，0=显式全量重放历史）" }, { "name": "Last-Event-ID", "in": "header", "required": false, "schema": { "type": "integer" }, "description": "断线续传起点（优先于 query 参数；缺省=只收新事件）" }], "responses": { "200": { "description": "reliable SSE event stream（带 Last-Event-ID 时零丢失续传重放；新订阅从当前 head 起只收新事件 + 有界背压；需 Bearer fetch-stream）" }, "401": { "description": "missing or invalid bearer token" } } } },
            "/metrics/runtime": { "get": { "operationId": "metricsRuntime", "responses": { "200": { "description": "runtime process metrics" } } } },
            "/metrics/slo": { "get": { "operationId": "metricsSlo", "responses": { "200": { "description": "SLO registry with error budget and attainment status" } } } },
            "/metrics/slo/alerts": { "get": { "operationId": "metricsSloAlerts", "responses": { "200": { "description": "SLO alert rules and structured alert events" } } } },
            "/metrics/slo/report": { "get": { "operationId": "metricsSloReport", "parameters": [{ "name": "days", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "SLO period report (JSON)" } } } },
            "/metrics/prometheus": { "get": { "operationId": "metricsPrometheus", "responses": { "200": { "description": "Prometheus text exposition format" } } } },
            "/diagnostics/requests": { "get": { "operationId": "diagnosticsRequests", "summary": "开发诊断：安全请求 ledger（R3 §8.1）", "parameters": [{ "name": "limit", "in": "query", "required": false, "schema": { "type": "integer", "minimum": 1, "maximum": 512, "description": "取最近 N 条（缺省 200，上限=环形容量）" } }], "responses": { "200": { "description": "环形窗口报告：total/returned/cap/aggregates{health,auth_token,business}/records[]；records 每条严格六字段（method,route_template,started_at,duration_ms,status,source），禁止出现 Authorization/查询串/请求体/响应体/私人路径", "content": { "application/json": { "schema": { "type": "object", "required": ["total", "returned", "cap", "aggregates", "records"], "properties": { "total": { "type": "integer" }, "returned": { "type": "integer" }, "cap": { "type": "integer" }, "aggregates": { "type": "object", "required": ["health", "auth_token", "business"], "properties": { "health": { "type": "integer" }, "auth_token": { "type": "integer" }, "business": { "type": "integer" } } }, "records": { "type": "array", "items": { "type": "object", "required": ["method", "route_template", "started_at", "duration_ms", "status", "source"], "properties": { "method": { "type": "string" }, "route_template": { "type": "string" }, "started_at": { "type": "string", "description": "RFC3339 毫秒 UTC" }, "duration_ms": { "type": "integer" }, "status": { "type": "integer" }, "source": { "type": "string", "description": "x-owo-client 头消毒值（[a-z0-9_-]{1,32}），异常/缺失→other" } } } } } } } } }, "401": { "description": "缺少或非法 bearer token" } } } },
            "/auth/token": { "get": { "operationId": "authTokenBootstrap", "security": [], "responses": { "200": { "description": "development bootstrap token; desktop release requires an ephemeral process-pairing proof header" }, "403": { "description": "desktop process pairing proof missing or invalid" } } } },
            "/storage/backup": { "post": { "operationId": "storageBackup", "responses": { "200": { "description": "zip backup (b64 + saved path)" } } } },
            "/storage/restore": { "post": { "operationId": "storageRestore", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "archive_b64": { "type": "string" } }, "required": ["archive_b64"] } } } }, "responses": { "200": { "description": "restore result with pre-backup" } } } },
            "/storage/export": { "post": { "operationId": "storageExport", "responses": { "200": { "description": "full standard JSON export" } } } },
            "/storage/clear": { "post": { "operationId": "storageClear", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "confirm": { "type": "string", "enum": ["CLEAR_ALL"] } } } } } }, "responses": { "200": { "description": "cleared with integrity check" } } } },
            "/server/status": { "get": { "operationId": "serverStatus", "responses": { "200": { "description": "concurrency gate + storage migration status" } } } },
            "/server/shutdown": { "post": { "operationId": "serverShutdown", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "confirm": { "type": "boolean" } }, "required": ["confirm"] } } } }, "responses": { "200": { "description": "graceful shutdown requested" } } } },
            "/usage/summary": { "get": { "operationId": "usageSummaryV2", "responses": { "200": { "description": "four-dimension usage aggregation + budget hard-stop state" } } } },
            "/usage/records": { "get": { "operationId": "usageRecords", "parameters": [{ "name": "dimension", "in": "query", "required": false, "schema": { "type": "string", "enum": ["session", "workflow_run", "goal_step", "tool"] } }, { "name": "limit", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "usage records filtered by dimension" } } } },
            "/usage/report": { "get": { "operationId": "usageReport", "parameters": [{ "name": "days", "in": "query", "required": false, "schema": { "type": "integer" } }], "responses": { "200": { "description": "usage aggregation report over window (budget/soak friendly)" } } } },
            "/usage/topup": { "post": { "operationId": "usageTopup", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "amount": { "type": "number" } } } } } }, "responses": { "200": { "description": "budget topped up and hard stop cleared" } } } },
            "/fleet/nodes/register": { "post": { "operationId": "fleetNodesRegister", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "node_id": { "type": "string" }, "card": { "type": "object" } }, "required": ["node_id", "card"] } } } }, "responses": { "200": { "description": "node registered with lease" } } } },
            "/fleet/nodes": { "get": { "operationId": "fleetNodesList", "responses": { "200": { "description": "node status snapshots" } } } },
            "/fleet/nodes/{node_id}/heartbeat": { "post": { "operationId": "fleetNodeHeartbeat", "parameters": [path_param("node_id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "lease_token": { "type": "string" } }, "required": ["lease_token"] } } } }, "responses": { "200": { "description": "lease renewed with latest epoch/token" }, "409": { "description": "stale token or expired lease (fencing)" } } } },
            "/fleet/nodes/{node_id}/tasks": { "get": { "operationId": "fleetNodeTasks", "parameters": [path_param("node_id")], "responses": { "200": { "description": "claimable/claimed tasks for node" } } } },
            "/fleet/tasks/submit": { "post": { "operationId": "fleetTasksSubmit", "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "task_id": { "type": "string" }, "worker": { "type": "string" }, "input": { "type": "object" }, "correlation_id": { "type": "string" }, "lineage": { "type": "array", "items": { "type": "string" } }, "approval_required": { "type": "boolean" } }, "required": ["task_id", "worker", "input"] } } } }, "responses": { "200": { "description": "task submitted with idempotency key" } } } },
            "/fleet/tasks/{id}": { "get": { "operationId": "fleetTaskGet", "parameters": [path_param("id")], "responses": { "200": { "description": "task view with status and events" } } } },
            "/fleet/tasks/{id}/claim": { "post": { "operationId": "fleetTaskClaim", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "node_id": { "type": "string" }, "lease_token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["node_id", "lease_token", "epoch"] } } } }, "responses": { "200": { "description": "task claimed by node (fencing verified)" }, "409": { "description": "stale token/epoch or node mismatch" } } } },
            "/fleet/tasks/{id}/progress": { "post": { "operationId": "fleetTaskProgress", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "node_id": { "type": "string" }, "lease_token": { "type": "string" }, "epoch": { "type": "integer" }, "text": { "type": "string" }, "evidence": { "type": "array", "items": { "type": "object" } } }, "required": ["node_id", "lease_token", "epoch", "text"] } } } }, "responses": { "200": { "description": "progress + structured evidence recorded" } } } },
            "/fleet/tasks/{id}/result": { "post": { "operationId": "fleetTaskResult", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "node_id": { "type": "string" }, "lease_token": { "type": "string" }, "epoch": { "type": "integer" }, "ok": { "type": "boolean" }, "output": { "type": "object" }, "output_cas": { "type": "string" }, "evidence": { "type": "array", "items": { "type": "object" } }, "error": { "type": "string" } }, "required": ["node_id", "lease_token", "epoch", "ok"] } } } }, "responses": { "200": { "description": "task result recorded (terminal)" } } } },
            "/fleet/tasks/{id}/cancel-ack": { "post": { "operationId": "fleetTaskCancelAck", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "node_id": { "type": "string" }, "lease_token": { "type": "string" }, "epoch": { "type": "integer" } }, "required": ["node_id", "lease_token", "epoch"] } } } }, "responses": { "200": { "description": "node confirmed cancellation" } } } },
            "/fleet/tasks/{id}/cancel": { "post": { "operationId": "fleetTaskCancel", "parameters": [path_param("id")], "responses": { "200": { "description": "task cancelled" } } } },
            "/fleet/tasks/{id}/events": { "get": { "operationId": "fleetTaskEvents", "parameters": [path_param("id"), { "name": "format", "in": "query", "required": false, "schema": { "type": "string", "enum": ["json"] } }], "responses": { "200": { "description": "SSE task event stream (history replay + live; ?format=json returns array; requires Bearer)" }, "401": { "description": "missing or invalid bearer token" } } } },
            "/fleet/approvals/{id}/respond": { "post": { "operationId": "fleetApprovalRespond", "parameters": [path_param("id")], "requestBody": { "content": { "application/json": { "schema": { "type": "object", "properties": { "decision": { "type": "string", "enum": ["approve", "reject"] }, "approved_by": { "type": "string" } }, "required": ["decision", "approved_by"] } } } }, "responses": { "200": { "description": "approval decision recorded" } } } }
        },
        "components": {
            "schemas": {
                "ArtifactReviewRecord": {
                    "type": "object",
                    "description": "不可变评审记录（V1 四期第三路；只增不改，append-only）",
                    "properties": {
                        "review_id": { "type": "string" },
                        "artifact_id": { "type": "string" },
                        "artifact_version": { "type": "integer", "description": "被评审的产物版本" },
                        "team_id": { "type": "string" },
                        "decision": { "type": "string", "enum": ["approve", "request_changes", "reject"] },
                        "reviewer": { "type": "string" },
                        "comment": { "type": "string" },
                        "idempotency_key": { "type": "string", "description": "唯一约束；同键重放零副作用" },
                        "content_ref": { "type": "string", "description": "评审时的产物内容引用（取证锚点）" },
                        "created_at": { "type": "string" }
                    },
                    "required": ["review_id", "artifact_id", "artifact_version", "team_id", "decision", "reviewer", "idempotency_key", "created_at"]
                },
                "CreateSessionRequest": {
                    "type": "object",
                    "properties": {
                        "workspace": { "type": "string" },
                        "model": { "type": "string" },
                        "system_prompt": { "type": "string" }
                    },
                    "required": ["workspace"]
                },
                "SessionInfo": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "workspace": { "type": "string" },
                        "updated_at": { "type": "string" },
                        "title": { "type": "string" },
                        "archived": { "type": "boolean" },
                        "pinned": { "type": "boolean" },
                        "parent_id": { "type": "string" },
                        "fork_point": { "type": "integer" },
                        "model": { "type": "string" },
                        "created_at": { "type": "string" }
                    }
                },
                "TurnRequest": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "attachments": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["prompt"]
                },
                "EvalRunRequest": {
                    "type": "object",
                    "properties": { "suite_id": { "type": "string" } },
                    "required": ["suite_id"]
                },
                "Error": {
                    "type": "object",
                    "properties": { "error": { "type": "string" } },
                    "required": ["error"]
                },
                "ProductEvalRunSummary": {
                    "type": "object",
                    "description": "ProductEval 运行摘要（列表元素与详情基底；六态：queued/running/cancelled/completed/failed/interrupted）",
                    "properties": {
                        "run_id": { "type": "string" },
                        "suite": { "type": "string" },
                        "execution": { "type": "string", "enum": ["reference", "live"] },
                        "modes": { "type": "array", "items": { "type": "string", "enum": ["single", "workswarm"] } },
                        "repetitions": { "type": "integer" },
                        "category": { "type": ["string", "null"], "enum": ["code", "research", "document", null] },
                        "only": { "type": ["string", "null"] },
                        "model": { "type": ["string", "null"] },
                        "status": { "type": "string", "enum": ["queued", "running", "cancelled", "completed", "failed", "interrupted"] },
                        "created_at": { "type": "string" },
                        "started_at": { "type": ["string", "null"] },
                        "finished_at": { "type": ["string", "null"] },
                        "planned_total": { "type": "integer", "description": "计划单元格总数（modes × cases × repetitions）" },
                        "progress": {
                            "type": "object",
                            "properties": {
                                "done": { "type": "integer", "description": "已完成单元格（journal 行数）" },
                                "total": { "type": "integer", "description": "= planned_total" }
                            },
                            "required": ["done", "total"]
                        },
                        "error": { "type": ["string", "null"] }
                    },
                    "required": ["run_id", "suite", "execution", "modes", "repetitions", "status", "created_at", "planned_total", "progress"]
                },
                "MatrixKey": {
                    "type": "object",
                    "description": "矩阵单元格：(case_id, agent_mode, repetition)；agent_mode 为核心小写词（workswarm 拓扑序列化为 multi）",
                    "properties": {
                        "case_id": { "type": "string" },
                        "agent_mode": { "type": "string", "enum": ["single", "multi"] },
                        "repetition": { "type": "integer" }
                    },
                    "required": ["case_id", "agent_mode", "repetition"]
                },
                "ProductEvalRun": {
                    "type": "object",
                    "description": "一次运行的完整记录（journal 最小单元；失败记录同样保留；Option 字段缺数据时序列化为 null）",
                    "properties": {
                        "key": { "$ref": "#/components/schemas/MatrixKey" },
                        "category": { "type": "string", "enum": ["code", "research", "document"] },
                        "status": { "type": "string", "enum": ["passed", "failed", "error", "timeout", "cancelled"], "description": "单元格级状态（核心 RunStatus 小写词）" },
                        "wall_ms": { "type": "integer", "format": "int64" },
                        "model_calls": { "type": "integer" },
                        "prompt_tokens": { "type": ["integer", "null"], "format": "int64", "nullable": true },
                        "completion_tokens": { "type": ["integer", "null"], "format": "int64", "nullable": true },
                        "total_tokens": { "type": ["integer", "null"], "format": "int64", "nullable": true },
                        "cost_usd": { "type": ["number", "null"], "nullable": true },
                        "failed_steps": { "type": "array", "items": { "type": "string" }, "description": "失败步骤（检查器描述/执行器阶段名）" },
                        "retries": { "type": "integer" },
                        "cancellations": { "type": "integer" },
                        "artifact_refs": { "type": "array", "items": { "type": "string" }, "description": "最终 Artifact 引用（沙盒内相对路径）" },
                        "tool_log": { "type": "array", "items": { "type": "string" }, "description": "真实工具调用轨迹（单 Agent 执行器填写：工具+实参摘要+结果；旧记录缺省为空数组）" },
                        "model": { "type": ["string", "null"], "nullable": true },
                        "started_at": { "type": "string" },
                        "finished_at": { "type": "string" },
                        "error": { "type": ["string", "null"], "nullable": true }
                    },
                    "required": ["key", "category", "status", "wall_ms", "model_calls", "prompt_tokens", "completion_tokens", "total_tokens", "cost_usd", "failed_steps", "retries", "cancellations", "artifact_refs", "tool_log", "model", "started_at", "finished_at", "error"]
                },
                "ProductEvalMetrics": {
                    "type": "object",
                    "description": "聚合指标：成功率分母为全部已尝试运行（失败/错误/超时一律计入，禁止剔除重算）",
                    "properties": {
                        "runs_total": { "type": "integer" },
                        "passed": { "type": "integer" },
                        "failed": { "type": "integer" },
                        "errors": { "type": "integer" },
                        "timeouts": { "type": "integer" },
                        "cancelled": { "type": "integer" },
                        "success_rate": { "type": "number" },
                        "mean_wall_ms": { "type": "number" },
                        "total_model_calls": { "type": "integer", "format": "int64" },
                        "total_tokens": { "type": ["integer", "null"], "format": "int64", "nullable": true },
                        "estimated_cost_usd": { "type": ["number", "null"], "nullable": true }
                    },
                    "required": ["runs_total", "passed", "failed", "errors", "timeouts", "cancelled", "success_rate", "mean_wall_ms", "total_model_calls", "total_tokens", "estimated_cost_usd"]
                },
                "CaseModeMetrics": {
                    "type": "object",
                    "description": "按 (case_id, mode) 分组的细分统计（单 Agent vs WorkSwarm 对照列）",
                    "properties": {
                        "case_id": { "type": "string" },
                        "category": { "type": "string", "enum": ["code", "research", "document"] },
                        "agent_mode": { "type": "string", "enum": ["single", "multi"] },
                        "runs_total": { "type": "integer" },
                        "passed": { "type": "integer" },
                        "success_rate": { "type": "number" },
                        "mean_wall_ms": { "type": "number" },
                        "mean_model_calls": { "type": "number" },
                        "total_tokens": { "type": ["integer", "null"], "format": "int64", "nullable": true }
                    },
                    "required": ["case_id", "category", "agent_mode", "runs_total", "passed", "success_rate", "mean_wall_ms", "mean_model_calls", "total_tokens"]
                },
                "ProductEvalReport": {
                    "type": "object",
                    "description": "ProductEvalReport 原样（core 序列化；聚合全部 journal 记录含失败 + 未完成单元格清单）",
                    "properties": {
                        "schema_version": { "type": "integer" },
                        "suite_name": { "type": "string" },
                        "suite_hash": { "type": "string" },
                        "execution": { "type": "string", "enum": ["reference", "live"] },
                        "model": { "type": ["string", "null"], "nullable": true },
                        "generated_at": { "type": "string" },
                        "runs": { "type": "array", "items": { "$ref": "#/components/schemas/ProductEvalRun" } },
                        "pending": { "type": "array", "items": { "$ref": "#/components/schemas/MatrixKey" } },
                        "metrics": { "$ref": "#/components/schemas/ProductEvalMetrics" },
                        "per_case": { "type": "array", "items": { "$ref": "#/components/schemas/CaseModeMetrics" } }
                    },
                    "required": ["schema_version", "suite_name", "suite_hash", "execution", "model", "generated_at", "runs", "pending", "metrics", "per_case"]
                },
                "CalibrationReport": {
                    "type": "object",
                    "description": "预测校准报告（WM0 聚合：命中、误差与不确定度分桶）",
                    "properties": {
                        "samples": { "type": "integer", "format": "int64" },
                        "success_hit_rate": { "type": "number" },
                        "mean_calibration_error": { "type": "number" },
                        "mean_delta_jaccard": { "type": "number" },
                        "uncertainty_buckets": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": { "type": "string" },
                                    "samples": { "type": "integer", "format": "int64" },
                                    "hit_rate": { "type": "number" }
                                },
                                "required": ["label", "samples", "hit_rate"]
                            }
                        }
                    },
                    "required": ["samples", "success_hit_rate", "mean_calibration_error", "mean_delta_jaccard", "uncertainty_buckets"]
                },
                "CandidateProviderRef": {
                    "description": "候选 provider 身份（§5.12.4 治理，声明≠接线）：external 需进程内真实接线后才积累影子样本；metadata_only 零样本且不可晋升",
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string", "enum": ["external"] },
                                "kind": { "type": "string", "description": "provider 类型标识（如 wm1-http、local-onnx）" },
                                "locator": { "type": "string", "description": "定位串（端点或资源标识）" }
                            },
                            "required": ["type", "kind", "locator"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "type": "string", "enum": ["metadata_only"] }
                            },
                            "required": ["type"]
                        }
                    ]
                },
                "ModelCandidate": {
                    "type": "object",
                    "description": "世界模型候选（新候选恒 shadow 起步，达标后显式人工晋升；Option 字段缺省序列化为 null）",
                    "properties": {
                        "candidate_id": { "type": "string" },
                        "model_id": { "type": "string" },
                        "model_version": { "type": "string" },
                        "source": { "type": "string" },
                        "status": { "type": "string", "enum": ["shadow", "active", "rejected"] },
                        "created_at": { "type": "string", "description": "RFC3339" },
                        "promoted_at": { "type": "string", "nullable": true },
                        "promote_reason": { "type": "string", "nullable": true },
                        "provider": { "$ref": "#/components/schemas/CandidateProviderRef", "description": "provider 身份治理（响应 wire 字段名为 provider；注册请求侧字段名为 provider_ref）" },
                        "calibration_summary": { "allOf": [{ "$ref": "#/components/schemas/CalibrationReport" }], "nullable": true, "description": "晋升时刻的校准摘要快照（从未晋升过为 null）" }
                    },
                    "required": ["candidate_id", "model_id", "model_version", "source", "status", "created_at", "promoted_at", "promote_reason", "provider", "calibration_summary"]
                },
                "HealthResponse": {
                    "type": "object",
                    "description": "/health 响应（十期一路：build 为 additive 字段；§4.2 实例握手字段 additive）",
                    "properties": {
                        "healthy": { "type": "boolean" },
                        "version": { "type": "string" },
                        "api_version": { "type": "string", "description": "桌面壳与核心服务兼容性握手版本" },
                        "auto_approve": { "type": "boolean" },
                        "build": { "$ref": "#/components/schemas/BuildInfo", "nullable": true },
                        "instance_id": { "type": "string", "nullable": true, "description": "桌面壳注入的实例身份（开发模式不序列化）" },
                        "pid": { "type": "integer", "description": "服务进程 pid" },
                        "stage": { "type": "string", "description": "启动阶段（当前恒为 ready）" },
                        "build_id": { "type": "string", "description": "构建标识（git_commit，缺失 unknown）" }
                    },
                    "required": ["healthy", "version", "api_version", "auto_approve"]
                },
                "BuildInfo": {
                    "type": "object",
                    "description": "构建信息（§7.1 单一来源 owo-build-info：编译期烧录优先，OWO_BUILD_INFO 覆写文件兼容发布链）",
                    "properties": {
                        "commit": { "type": "string" },
                        "dirty": { "type": "boolean" },
                        "built_at": { "type": "string" }
                    },
                    "required": ["commit", "dirty", "built_at"]
                }
            },
            "securitySchemes": {
                "bearerAuth": { "type": "http", "scheme": "bearer" }
            }
        },
        "security": [{ "bearerAuth": [] }]
    }))
}

fn path_param(name: &str) -> Value {
    serde_json::json!({ "name": name, "in": "path", "required": true, "schema": { "type": "string" } })
}

// （§12：to_session_info/load_session 与会话元数据处理器已外移至 session_api.rs）

async fn acquire_session_lock(
    state: &AppState,
    id: &str,
) -> Result<tokio::sync::OwnedMutexGuard<()>, (StatusCode, String)> {
    let lock = {
        let mut locks = state.turn_locks.lock().map_err(poison)?;
        locks
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    Ok(lock.lock_owned().await)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        healthy: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
        api_version: OWO_API_VERSION.to_string(),
        auto_approve: turn_api::auto_approve_enabled(),
        build: load_build_info().clone(),
        // §4.2 实例握手：非秘密身份字段。桌面壳凭 instance_id/pid 核对
        // "这个服务是我启动的子进程"，不再盲复用同端口旧服务。
        instance_id: auth_token::desktop_instance_id(),
        pid: std::process::id(),
        stage: "ready".to_string(),
        build_id: load_build_info()
            .as_ref()
            .map(|info| info.commit.clone())
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

static BUILD_INFO: OnceLock<Option<BuildInfo>> = OnceLock::new();

/// 构建身份解析（§5.1/§6.1.2/§7.1）——委托 `owo_build_info::identity()`
/// 单一链（① OWO_BUILD_INFO 覆写文件 → ② 编译期烧录 → ③ cwd 遗留文件 →
/// 不可用）。server 不再自持回退链副本；旧实现里「覆写文件损坏直接返回
/// None」的陷阱一并消除（损坏即跳过覆写、回落编译期事实，/health 身份
/// 永不为空）。Unavailable 时保持 build=None（等价旧行为的仅版本形态）。
fn load_build_info() -> &'static Option<BuildInfo> {
    BUILD_INFO.get_or_init(|| {
        let identity = owo_build_info::identity();
        if identity.source == owo_build_info::IdentitySource::Unavailable {
            None
        } else {
            Some(BuildInfo {
                commit: identity.commit,
                dirty: identity.dirty,
                built_at: identity.built_at,
            })
        }
    })
}

// （§7.1 收口：build-info.json 解析链已迁入 owo_build_info::identity()
//   单一实现，本文件不再持有副本。）

// （§12：GET /usage 的 usage_summary 已外移至 usage.rs::usage_model_summary，
//   经 usage_router 并入 build_router，路由面零变化）

// （§12：flush_audit 与 audit_list 已外移至 audit_api.rs）

// （§12：server_status/shutdown 已外移至 ops_api.rs）

// （§12：usage_topup 与 UsageTopupRequest 已外移至 usage.rs）

// （§12：list_sessions 已外移至 session_api.rs）
// （§12：list_skills/skill_detail/skill_edit/skill_enabled 已外移至 skills_api.rs）
// （§12：create_session/get_session 已外移至 session_api.rs）

// （§12：turn 编排器已外移至 turn_api.rs）

// （§12：附件三助手两处理器已外移至 session_api.rs·第二刀）

// （§12：respond_permission 已外移至 turn_api.rs）

// （§12：abort/diff/revert/fork/rewind/redo/export 已外移至 session_api.rs·第二刀）

// （§12：session_rename/archive/pin/children 与三请求结构已外移至 session_api.rs）
// （§12：export_session 已外移至 session_api.rs·第二刀）

// （§12：run_eval 已外移至 eval_api.rs；context_snapshot 已外移至 session_api.rs）

// （§12：desktop/vision 十四处理器与九请求结构 + gate_desktop_action/ensure_real_desktop 已外移至 desktop_api.rs）

// （§12：learn 域十六处理器与五请求结构、parse_sensitivity/ui_action_source 已外移至 learn_api.rs）

// （§12：proactive 三处理器 + stt_transcribe + automations 六处理器及请求结构
//   已外移至 assist_api.rs）

/// §13 批次三（R10 持久化组接线）：启动恢复——从 data_root 重放用量快照
/// （records + budgets + 硬熔断状态，崩溃后续接）。返回恢复的记录条数。
pub fn restore_usage_snapshot(data_root: &std::path::Path) -> usize {
    match usage::load_from(data_root) {
        Ok(restored) => restored,
        Err(error) => {
            tracing::warn!("用量快照恢复失败（跳过，继续启动）：{error}");
            0
        }
    }
}

/// §13 批次三：立即落盘一次用量快照（优雅关闭路径调用）。返回快照路径。
pub fn persist_usage_snapshot(data_root: &std::path::Path) -> Option<std::path::PathBuf> {
    match usage::persist_to(data_root) {
        Ok(path) => Some(path),
        Err(error) => {
            tracing::warn!("用量快照落盘失败：{error}");
            None
        }
    }
}

/// §13 批次六（遥测接线）：应用 settings.json 的遥测开关（默认关；
/// 仅聚合功能计数/错误码分布/性能分位，数据字典经 /metrics/telemetry/status 暴露）。
pub fn apply_telemetry_setting(enabled: bool) {
    observability_api::set_telemetry_enabled(enabled);
}

/// §13 批次八（R10 文件日志接线）：初始化轮转文件日志
/// `<data_root>/logs/server.jsonl`（8MB × 5 份；落盘内容经统一脱敏——
/// 用户供给字段按字段名走 Redactor 策略，未知字段保守哈希）。
pub fn init_server_file_logging(data_root: &std::path::Path) {
    let dir = data_root.join("logs");
    let path = dir.join("server.jsonl");
    match logging::init_file_logging(&path, 8 * 1024 * 1024, 5) {
        Ok(()) => logging::info(
            "server",
            None,
            "文件日志已启用（8MB×5 轮转，内容经统一脱敏）",
        ),
        Err(error) => tracing::warn!("文件日志初始化失败（仅落 stderr）：{error}"),
    }
}

/// §13 批次八：关闭文件日志（优雅关闭序列，与 init 配对）。
pub fn close_server_file_logging() {
    logging::close_file_logging();
}

/// §13 批次八：生命周期审计事件（结构化日志面；detail 强制脱敏）。
pub fn logging_lifecycle_audit(action: &str, detail: &str) {
    logging::audit_event(action, None, detail);
}

/// §13 批次三：用量定时落盘常驻循环（每小时一次；首 tick 即时落盘一次，
/// 保证启动恢复后快照与内存态同步）。与 start_automation_loop 同批派生。
pub async fn start_usage_persistence_loop(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        let data_root = state.data_root.clone();
        let result = tokio::task::spawn_blocking(move || usage::persist_to(&data_root)).await;
        match result {
            Ok(Ok(path)) => tracing::debug!("用量快照已定时落盘：{}", path.display()),
            Ok(Err(error)) => tracing::warn!("用量快照定时落盘失败：{error}"),
            Err(error) => tracing::warn!("用量快照落盘任务 Join 失败：{error}"),
        }
    }
}

/// 自动化常驻循环：每秒检查到期任务，触发提醒并写审计。
pub async fn start_automation_loop(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let fired = {
            let mut automations = state
                .automations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = chrono::Utc::now();
            let mut fired = Vec::new();
            for id in automations.due_tasks(now) {
                if let Ok(text) = automations.fire(&id, now) {
                    fired.push(text);
                }
            }
            fired
        };
        if !fired.is_empty() {
            if let Ok(mut audit) = state.agent.audit_log().lock() {
                audit.record("automation", "fire", None, Some(true), fired.join(" | "));
            }
            // §13 批次八：审计可观测面日志（detail 经 safe_field 强制脱敏）。
            logging::audit_event("automation_fire", None, &fired.join(" | "));
        }
    }
}

/// 静默观察器：每 2s 采样桌面状态（前台应用/标题哈希/剪贴板序列，受 L0 授权门控），
/// 并在模拟面下额外拉取模拟窗口日志；动作摘要（内容掩码）写入情景记忆。
pub async fn start_memory_observer(state: Arc<AppState>) {
    let mut sim_seen = 0usize;
    let mut desktop_prev: Option<owo_agent_core::DesktopSnapshot> = None;
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let l0_enabled = state
            .perception
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_enabled(owo_agent_core::PerceptionLayer::L0Event);
        if l0_enabled {
            let snapshot = owo_agent_core::sample_desktop();
            if let Some(prev) = &desktop_prev {
                if let Some(observation) = owo_agent_core::desktop_observation(prev, &snapshot) {
                    let mut memory = state
                        .memory
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let _ = memory.append(observation);
                }
            }
            desktop_prev = Some(snapshot);
        }
        let Some(base) = std::env::var("OWO_SIM_QQ_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let url = format!("{}/log", base.trim_end_matches('/'));
        let Ok(response) = reqwest::get(&url).await else {
            continue;
        };
        let Ok(value) = response.json::<Value>().await else {
            continue;
        };
        let Some(entries) = value.get("entries").and_then(Value::as_array) else {
            continue;
        };
        if entries.len() < sim_seen {
            // 模拟场景被 /reset 清空：从头重新计数。
            sim_seen = 0;
        }
        if entries.len() <= sim_seen {
            continue;
        }
        let mut memory = state
            .memory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for entry in &entries[sim_seen..] {
            if let Some(observation) = owo_agent_core::observation_from_sim_event(entry) {
                let _ = memory.append(observation);
            }
        }
        sim_seen = entries.len();
    }
}

// ---------- 设置与诊断 ----------

// （§12：settings_get/effective_runtime_config 已外移至 settings_api.rs）

// （§12：settings_egress 与 EgressRequest 已外移至 settings_api.rs）

// （§12：permissions_status/set_profile 与 grants_list/revoke 已外移至 settings_api.rs）

// （§12：whitelist_list/manage 已外移至 whitelist_api.rs）

// （§12：ChannelApprover/auto_approve_enabled/to_sse 已外移至 turn_api.rs）
// （§12：to_event 已外移至 turn_api.rs）
fn poison<T>(_error: std::sync::PoisonError<T>) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, "状态锁中毒".to_string())
}

/// P3 录制自动观察：录制中每 2s 采样前台应用/剪贴板事件（掩码）进入样本。
/// 前台应用变化只记一次，剪贴板变化按序列号去重。
pub async fn start_observer(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    let mut last_app: Option<(String, String)> = None;
    let mut last_clipboard: u32 = 0;
    loop {
        interval.tick().await;
        let (foreground, clipboard_changed) = {
            let mut perception = state
                .perception
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = perception.refresh_from_platform();
            let sequence = owo_agent_core::clipboard_sequence();
            let changed = sequence != 0 && sequence != last_clipboard;
            perception.refresh_clipboard(sequence);
            let _ = perception.refresh_from_uia(2, 32);
            let snapshot = perception.snapshot();
            (snapshot.foreground_app.clone(), changed)
        };
        let mut pipeline = state
            .pipeline
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pipeline.recorder.state() != LearnState::Recording {
            continue;
        }
        if let Some(app) = &foreground {
            let key = (app.id.clone(), app.title.clone());
            if last_app.as_ref() != Some(&key) {
                last_app = Some(key);
                let _ = pipeline.recorder.record(RecordedAction {
                    app_id: app.id.clone(),
                    anchor: SemanticAnchor {
                        app_id: Some(app.id.clone()),
                        role: None,
                        name: app.title.clone(),
                        parent: None,
                        element_id: None,
                    },
                    action_type: ActionType::Shortcut,
                    value_masked: true,
                    sensitive: false,
                    at: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
        if clipboard_changed {
            last_clipboard = owo_agent_core::clipboard_sequence();
            if let Some(app) = &foreground {
                let _ = pipeline.recorder.record(RecordedAction {
                    app_id: app.id.clone(),
                    anchor: SemanticAnchor {
                        app_id: Some(app.id.clone()),
                        role: None,
                        name: "剪贴板".to_string(),
                        parent: None,
                        element_id: None,
                    },
                    action_type: ActionType::Inject,
                    value_masked: true,
                    sensitive: false,
                    at: chrono::Utc::now().to_rfc3339(),
                });
            }
        }
    }
}

// ===========================================================================
// R10 契约治理：API 版本 / 弃用策略 / 错误码表 / JSON Schema 发布
// ===========================================================================

/// API 版本（`x-owo-api-version`）。破坏性变更递增 minor；弃用期 ≥2 个 minor。
pub const OWO_API_VERSION: &str = owo_build_info::API_VERSION;

/// 已弃用路由登记：(路径前缀, since, until, 替代建议)。
/// 命中时响应携带 `Deprecation` 头；当前无已弃用路由，破坏性变更前在此登记。
///
/// 路由/事件契约变更 RFC 登记（弃用策略落地：变更前登记 → 弃用期 ≥2 minor → 移除）：
/// - 2026-08-17（R10）：SSE 事件 data 统一携带 `v` 字段（v=1；旧客户端帧缺 v 视为 v=0）。
/// - 2026-08-17（R10）：新增 /schemas/{kind}/{version} 静态 JSON Schema 版本化发布。
/// - 2026-08-17（R10）：错误响应统一为 {error:{code,message,retry_after_ms,domain,reason,retryable}}。
const DEPRECATED_ROUTES: &[(&str, &str, &str, &str)] = &[];

/// 统一错误响应（R10 错误码表接入 HTTP 层）：(status, {error:{code,message,retry_after_ms,...}})。
/// `pub`：usage.rs 等 #[path] 独立编译的域模块经 crate 名引用（后代可见性不跨 crate 边界）。
pub fn api_error_response(
    code: &error_codes::ErrorCode,
    message: impl std::fmt::Display,
) -> (StatusCode, Json<Value>) {
    // §13 批次六：遥测错误码分布（默认关时零开销早退；仅 code 计数，无消息内容）。
    observability_api::record_telemetry_error(&format!("{}/{}", code.domain, code.reason));
    (
        StatusCode::from_u16(code.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({
            "error": {
                "code": format!(
                    "{}/{}/{}",
                    code.domain,
                    code.reason,
                    if code.retryable { "retryable" } else { "not_retryable" }
                ),
                "message": message.to_string(),
                "domain": code.domain,
                "reason": code.reason,
                "retryable": code.retryable,
                "retry_after_ms": code.retry_after_ms,
            }
        })),
    )
}

/// 计算给定路径应附加的 `Deprecation` 头值（未命中返回 None）。
/// 独立为纯函数供契约测试直接覆盖命中/未命中与头格式（R12 收尾）。
pub fn deprecation_header_value_for(
    routes: &[(&str, &str, &str, &str)],
    path: &str,
) -> Option<String> {
    for (route, since, until, alternative) in routes {
        if path.starts_with(route) {
            return Some(format!(
                "{route}: since {since}, until {until} (use {alternative})"
            ));
        }
    }
    None
}

/// Deprecation 中间件：命中 DEPRECATED_ROUTES 的请求附加 `Deprecation` 响应头。
async fn deprecation_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    if let Some(value) = deprecation_header_value_for(DEPRECATED_ROUTES, &path) {
        if let Ok(value) = value.parse::<axum::http::HeaderValue>() {
            response.headers_mut().insert("Deprecation", value);
        }
    }
    response
}

// （§12：schemas_list/schema_get 与三份 schema 常量已外移至 schemas_api.rs）

#[cfg(test)]
mod tests {
    use crate::session_api::{rewind_session, sanitize_attachment_name};
    use crate::turn_api::auto_approve_enabled;
    use crate::AppState;
    use async_trait::async_trait;
    use base64::Engine;
    use owo_agent_core::permissions::Policy;
    use owo_agent_core::{
        Agent, AgentConfig, ChatMessage, ModelOutput, ModelProvider, Session, ToolRegistry,
        ToolSpec,
    };
    use owo_agent_protocol::RewindRequest;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sanitizes_attachment_names() {
        assert_eq!(
            sanitize_attachment_name("report.pdf").as_deref(),
            Some("report.pdf")
        );
        assert_eq!(
            sanitize_attachment_name("a/b/c.txt").as_deref(),
            Some("c.txt")
        );
        assert_eq!(
            sanitize_attachment_name("..\\evil.txt").as_deref(),
            Some("evil.txt")
        );
        assert_eq!(
            sanitize_attachment_name("a:b*c?.txt").as_deref(),
            Some("bc.txt")
        );
        assert!(sanitize_attachment_name("").is_none());
        assert!(sanitize_attachment_name("   ").is_none());
        assert!(sanitize_attachment_name("x".repeat(201).as_str()).is_none());
    }

    #[test]
    fn auto_approve_env_detection() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var("OWO_AUTO_APPROVE");
        assert!(!auto_approve_enabled());
        std::env::set_var("OWO_AUTO_APPROVE", "1");
        assert!(auto_approve_enabled());
        std::env::set_var("OWO_AUTO_APPROVE", "TRUE");
        assert!(auto_approve_enabled());
        std::env::set_var("OWO_AUTO_APPROVE", "0");
        assert!(!auto_approve_enabled());
        std::env::remove_var("OWO_AUTO_APPROVE");
    }

    struct IdleProvider;

    #[async_trait]
    impl ModelProvider for IdleProvider {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
        ) -> Result<ModelOutput, String> {
            Err("测试 Provider 不应被调用".to_string())
        }
    }

    #[tokio::test]
    async fn rewind_endpoint_restores_files_before_saving_session() {
        let root = std::env::temp_dir().join(format!("owo-server-rewind-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        let data_root = root.join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&data_root).unwrap();
        let path = workspace.join("changed.txt");
        std::fs::write(&path, "after").unwrap();

        let agent = Agent::new(
            Arc::new(IdleProvider),
            ToolRegistry::new(),
            Policy::new(&workspace),
            AgentConfig::default(),
        );
        let store_root = root.join("sessions");
        let state = Arc::new(AppState::new(
            agent,
            owo_agent_core::JsonSessionStore::new(&store_root),
            data_root.join("traces"),
            data_root,
            workspace.clone(),
        ));
        let mut session = Session::new(&workspace, "mock", None);
        session.push(ChatMessage::user("first".to_string()));
        session.push(ChatMessage::assistant_text("reply".to_string()));
        session.snapshots.insert(
            path.to_string_lossy().replace('\\', "/"),
            owo_agent_core::session::SnapshotEntry {
                original_b64: Some(base64::engine::general_purpose::STANDARD.encode("before")),
            },
        );
        state.store.save(&session).unwrap();
        state
            .sessions
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let id = session.id.clone();
        let response = rewind_session(
            axum::extract::State(Arc::clone(&state)),
            axum::extract::Path(id.clone()),
            axum::Json(RewindRequest { keep: 1 }),
        )
        .await
        .unwrap()
        .0;

        assert_eq!(response["ok"], json!(true));
        assert_eq!(response["removed"], json!(1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "before");
        assert_eq!(state.store.load(&id).unwrap().messages.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
