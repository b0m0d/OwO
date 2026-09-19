//! OwO Agent SDK 核心库（M1）：
//! Agent loop、工具注册表、权限审批、会话、审计、模型网关。

pub mod accessibility;
pub mod action_program;
pub mod agent;
/// Artifact 校验与交付管线（七期 · 第三路）：格式门控（json/csv/research/
/// markdown）+ 交付元数据（media_type/file_name/sha256/size_bytes）+ 证据链。
pub mod artifact_pipeline;
pub mod assert;
pub mod autoreview;
pub mod blackboard;
/// 内置团队模板目录（六期第三路：四类稳定团队候选，安装后参与匹配；见
/// `team_template_catalog_api`——目录只展示，安装幂等且不自动扩权）。
pub mod builtin_team_templates;
pub mod bus_store;
pub mod computer_use;
pub mod contract_worker;
pub mod critic;
pub mod element_registry;
pub mod execution_target;
pub mod executor;
pub mod fleet;
pub mod fleet_node_protocol;
pub mod fleet_transport;
pub mod gateway;
pub mod goal;
pub mod grant_store;
pub mod learn;
pub mod locate;
pub mod mcp;
pub mod mcp_health;
pub mod memory;
pub mod node_agent;
pub mod observe;
pub mod ocr;
#[cfg(target_os = "windows")]
pub mod onnx_ocr;
pub mod paddle_ocr;
pub mod perception;
pub mod permission_spec;
pub mod permissions;
pub mod remote_step;
pub mod scene;
pub mod schema_budget;
pub mod session;
pub mod settings;
pub mod share;
pub mod share_skill;
pub mod skill_pack;
pub mod sqlite_store;
pub mod stt;
pub mod subagent;
pub mod team_prompt;
/// 自适应组队策略引擎（R3 第一路：single/team/auto 判定 + 可展示理由）。
pub mod team_strategy;
pub mod tool_effects;
pub mod tools;
pub mod trace;
pub mod vision;
pub mod window_template;
pub mod worker_pool;
/// 角色画像（七期 · 二路）：模板角色 → 工具面/只读/写白名单/回合上限/浏览器/命令，
/// 画像驱动子代理执行器（注册表面即权限边界）。
pub mod worker_profile;
pub mod workflow;
pub mod workswarm;

// ---------------------------------------------------------------------------
// 微内核（M0）兼容层：以下模块的实现已下沉到独立 crate `owo-agent-kernel`。
//
// 依赖方向变为 `owo-agent-kernel ← owo-agent-core ← owo-agent-server ← owo-agent-cli`：
// 内核只承载被多个运行边界共用的稳定原语（错误/平台/能力卡/审计/凭据/CAS/
// 存储加密/白名单/注入净化/租约/阶段预算），不依赖 core，也不依赖 ONNX/Sherpa。
//
// 这里保留同名别名模块 + 下面的顶层 `pub use`，使 **既有路径全部继续有效**：
//   * 块内相对路径 `crate::audit::AuditLog`（25 个 core 源文件使用）
//   * 外部路径 `owo_agent_core::audit::AuditLog`、`owo_agent_core::AgentError`
//     （server / cli / 36 个 core 集成测试使用）
// 因此本次拆分对调用方是零改动，符合“每拆一个微内核后仍能完整运行”的验收要求。
//
// 迁移期结束后（指南 §9 A2/A4），这些别名应改为正式目录结构并在 §7 边界澄清后
// 再删除；当前保留是有意为之，不是遗留耦合。
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Daemon 扩展内核（M2）兼容层：notes / automation / change_set / change_set_store /
// cloud_exec 已下沉到独立 crate `owo-agent-extensions`。
//
// 该 crate 在 core 依赖图里是唯一「零入边 + 零出边」的成规模集合（Tarjan SCC 实测，
// 见 docs/ARCH-MICROKERNEL.md §6）：它只依赖 owo-agent-kernel，core 内部没有模块引用
// 它，消费者全在 server/cli 侧。因此这里保留同名别名模块 + 顶层 `pub use` 即可让
// `owo_agent_core::{Notes, AutomationStore, ChangeSet, CloudTask, ...}` 与
// `crate::notes::*` 等既有路径全部继续有效，调用方零改动，且不可能形成 crate 环。
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 受信执行内核（M3）兼容层：sandbox 与 audit_chain 已下沉到独立 crate
// `owo-agent-tool-safety`（指南 §2.2 受信执行内核 + §2.4 第 3 条审计收据）。
//
// 该 crate 是 core 依赖图里唯一零出边的成规模分量（ARCH §5.1 的 SCC 分析），
// 只依赖 owo-agent-kernel，反向依赖为零。两者在 core 内原本互相引用
// （audit_chain 认识 SandboxAuditLog；sandbox 的 drain_into_chain 认识 AuditChain），
// 搬迁后同处一个 crate，该边不再跨越 crate 边界，因此**无需接口倒置**。
//
// 这里保留同名别名模块 + 顶层 pub use，使 `crate::sandbox::*`（mcp / plugin / tools）
// 与 `owo_agent_core::audit_chain::*`（audit.rs、4 个集成测试）等既有路径全部继续有效。
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 环境内核（M5/M6）兼容层：desktop_env / transition / world_model / experience_store
// 已下沉到独立 crate `owo-agent-env`。
//
// 本步是微内核重构里**第一次真正的依赖倒置**：desktop_env 的唯一出边
// （`crate::computer_use::TaskSurface`，见 §1 判据）被下沉到内核
// `owo_agent_kernel::task_surface`，因为 desktop_env 与 computer_use 不同迁，
// 该边若不倒置就会在外迁后成环。实现体（Sim/RealTaskSurface）仍在 core。
//
// 这里保留同名别名模块 + 顶层 pub use，使 `crate::desktop_env::*`
// （transition / world_model / computer_use）与 `owo_agent_core::desktop_env::*`
// （server 的 desktop_world_api、集成测试）等既有路径全部继续有效。
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 插件内核（M7）兼容层：plugin 已下沉到独立 crate `owo-agent-plugins`；
// 同时 core 的 `mcp` 从它反向引用 `McpServerConfig`（插件清单的 mcp 字段类型）。
//
// 这里保留同名别名模块 + 顶层 pub use，使 `crate::plugin::*` 与
// `owo_agent_core::{PluginManager, PluginManifest, McpServerConfig, ...}` 等既有路径
// 全部继续有效（server / cli 与 3 个集成测试因此零改动）。
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// 共享契约内核（M8）兼容层：context / computer_task / plan / skill / skill_health
// 已下沉到独立 crate `owo-agent-contracts`（零依赖根，比 kernel 更轻）。
//
// 这五个模块的 `crate::` 引用实测为空集，故整体搬迁无需任何倒置。
// 这里保留同名别名模块 + 下面既有的顶层 `pub use`，使
// `crate::plan::*`（workswarm / goal）与 `owo_agent_core::Plan / SkillRegistry / ...`
// （server / cli / 多个集成测试）等既有路径全部继续有效。
// ---------------------------------------------------------------------------
pub use owo_agent_contracts::{computer_task, context, plan, skill, skill_health};

// ---------------------------------------------------------------------------
// 编排内核（M9）兼容层：project_space_store / team_benefit / workswarm_output
// 已下沉到独立 crate `owo-agent-workswarm`（只依赖 protocol，反向依赖为零）。
//
// 三个模块的 `crate::` 出边实测为 0（含 `crate::{a,b}` 块与 `super::` 扫描），
// 因此整体搬迁不需要任何依赖倒置。入边分两侧，全部由下面的别名模块满足：
//   * crate 内：`crate::project_space_store::`（workswarm）、
//     `crate::team_benefit::`（team_strategy / workswarm）、
//     `crate::workswarm_output::`（artifact_pipeline / contract_worker /
//     worker_profile / workswarm）；
//   * crate 外：`owo_agent_core::{project_space_store,workswarm_output}::*`
//     （server 的 5 个 api 模块与集成测试、devtools/product-eval）。
//
// 这是指南 §3「extensions/workswarm 默认关闭、可选加载」的前置条件：执行侧
// （workswarm.rs 本体 4,328 行）仍留在 core，等 A2 统一 Daemon 落地后再动。
// ---------------------------------------------------------------------------
pub use owo_agent_workswarm::{project_space_store, team_benefit, workswarm_output};

pub use owo_agent_plugins::{plugin, McpServerConfig};

pub use owo_agent_env::{desktop_env, experience_store, transition, world_model};

pub use owo_agent_tool_safety::{audit_chain, sandbox};

pub use owo_agent_extensions::{automation, change_set, change_set_store, cloud_exec, notes};

pub use owo_agent_kernel::{
    audit, capability, cas_store, credentials, deadline, error, injection, lease, platform,
    storage_crypto, whitelist,
};

// ---------------------------------------------------------------------------
// 开发工具（M1）：ProductEval 底座（product_eval / eval / dataset_builder /
// product_eval_workswarm）**已整体迁出本 crate**，位于 `devtools/product-eval/`
// （独立 workspace 的开发工具 crate）。
//
// 本 crate 对它的依赖为**零**：受信运行时不该依赖开发工具；而且任何反向依赖都会让
// Cargo 报 `cyclic package dependency: owo-agent-core depends on itself`
// （同 workspace、exclude、workspace 级 default-features=false 三种做法实测都不成立）。
//
// 评测面的消费方改为 `owo-agent-eval-facade`：
//   server/cli ──► owo-agent-eval-facade ──► devtools/product-eval ──► core
//
// 因此这里不再有任何 product_eval / eval / dataset_builder 模块、别名或再导出。
// 指南 §9 把 ProductEval 最终定位为 Python 开发工具（`devtools/eval/`），届时门面一并删除。
// ---------------------------------------------------------------------------

pub use accessibility::{foreground_ui_tree, ui_tree_for_hwnd, UiNode};
pub use agent::{estimate_tokens, Agent, AgentConfig, TurnEvent, TurnOutcome};
pub use audit::{AuditEntry, AuditLog};
pub use audit_chain::{
    canonical, export_to_file, hex_encode, hmac_sha256, load_export, verify_export, verify_file,
    Anchor, AuditChain, AuditChainError, AuditCliCommand, AuditCliOutcome, AuditExport,
    AuditRecord, ChainedRecord,
};
pub use automation::{AutomationAction, AutomationStore, AutomationTask, Schedule};
pub use autoreview::{
    parse_verdict, AutoReviewChain, HeuristicReviewer, ModelReviewer, ReviewVerdict, Reviewer,
};
pub use blackboard::{
    Blackboard, BlackboardEntry, BlackboardError, BlackboardEvent, BlackboardOp, BlackboardSnapshot,
};
pub use bus_store::{is_critical, BusPersistPolicy, BusStore, StoredMessage};
pub use capability::{
    evaluate_capability_match, Arch, CapabilityCard, CapabilityMatch, CapabilityWorkerRegistry,
    EgressMode, Os, RegistrySnapshot, Resources, RouteDecision, RouteStats, TrustLevel,
    WorkerHealth, WorkerRequirement,
};
pub use cas_store::{CasRefsSnapshot, CasStore};
pub use cloud_exec::{
    backoff_delay, cloud_token_from_env, describe_diff, validate_batch, validate_commands,
    CloudProgress, CloudTask, CloudTaskQueue, CloudTaskResult, CloudTaskSpec, CloudTransport,
    CollectingSink, DiffKind, FileDiff, HttpTransport, LocalSimExecutor, MockRemoteTransport,
    NullSink, ProgressSink, RemoteStatus, TaskRecord, TaskState as CloudTaskState, UsageMetrics,
};
pub use computer_task::{sensitive_ui_hit, ComputerTask, ComputerTaskRegistry, TaskState};
pub use computer_use::{
    desktop_click, desktop_click_gated, desktop_key, desktop_key_gated, desktop_launch,
    desktop_launch_gated, desktop_scroll, desktop_scroll_gated, desktop_shortcut, desktop_type,
    desktop_type_gated, run_approved_task, run_approved_task_on, scan_ui_sensitive,
    sim_base_url_configured, task_gate_check, SimTaskSurface, TaskGoal, TaskReport, TaskSurface,
};
pub use context::load_project_rules;
pub use credentials::{
    scan_json_for_secrets, windows_credential_manager, ApiKeyRef, CredentialError,
    CredentialResolver, CredentialStore, MemoryCredentialStore, ProviderConfig, UnavailableStore,
};
pub use critic::{
    review_loop, ConsistencyReport, Critic, CriticConfig, CriticVerdict, ReadOnlyGate,
    ReviewOutcome, ReviewRound, SamplePair, ScriptedCritic,
};
pub use desktop_env::{
    ActionKind, Assertion, DesktopEnv, EnvError, EnvLeaseRecord, EnvRegistry, FaultSpec,
    FieldChange, GroundedAction, LeaseProof, RewardParts, RiskLevel, SimAppKind, SimDesktopEnv,
    SimElement, StateDelta, StepResult, SuccessSpec, SurfaceEnvAdapter, TaskSeed, Verdict,
    WorldStateV1, SIM_ENV_PROTOCOL, SIM_ENV_VERSION,
};
pub use element_registry::{
    fuse_sources, fuse_sources_with_vision, register_vision_grounding, ElementRegistry,
    SceneElement, VisionGrounding,
};
pub use error::AgentError;
/// A2 统一调度适配层（冻结接口）：显式执行目标 / 绑定 / 派发裁定。
pub use execution_target::{
    dispatch_disposition, select_binding, BindingBudget, DispatchCancelRegistry, DispatchChannel,
    DispatchDisposition, ExecutionTarget, FleetDispatchWorker, FleetProbe, LocalProcessProbe,
    PermissionScope, ResolvedDispatch, TargetAvailability, WorkerBinding,
    DEFAULT_FLEET_DISPATCH_TIMEOUT, TARGET_FLEET_NODE, TARGET_IN_PROCESS, TARGET_LOCAL_PROCESS,
};
pub use executor::{execute_graph, ExecReport, ExecStep, UiActionSource, WindowsUiaSource};
pub use experience_store::{
    load_aggregation_report, AggregationReport, Attribution, ExperienceEvent, ExperienceKind,
    ExperienceStore, Outcome, SkillInsight, AGGREGATION_REPORT_FILE,
};
pub use fleet::{
    arbitrate_wait_cycle, backoff_secs, dedupe_messages, detect_cycle, detect_wait_cycle, fan_out,
    fan_out_cfg, is_mergeable, message_dedup_key, new_correlation_id, AgentBus, AgentId, Budget,
    BusError, BusMessage, CorrelationId, FanOutConfig, FanOutOutcome, FanOutReport, FanOutStatus,
    Mailbox, MessageKind, OverflowPolicy, PushOutcome, RestartPolicy, RestartRule,
    SupervisionState, Supervisor, WaitEdge, WaitGraph, WaitResolution, WorkerEvent,
    WorkerEventKind,
};
pub use fleet_node_protocol::{
    violation_correlation_id, NodeAuthHint, NodeCancelAckBody, NodeClaimBody, NodeHeartbeatBody,
    NodeHeartbeatResponse, NodeProgressBody, NodeProtocolViolation, NodeResultBody,
};
/// 控制面 HTTP 传输（cloud_exec 已有 `HttpTransport`，此处以 FleetHttpTransport 区分）。
pub use fleet_transport::HttpTransport as FleetHttpTransport;
pub use fleet_transport::{
    FleetTransport, InMemoryTransport, TransportEvent, TransportEventKind, TransportStatus,
    TransportTask, TransportWorker,
};
pub use gateway::{
    budget_violation, parse_usage_value, ChatMessage, ModelOutput, ModelProvider,
    OpenAiCompatibleConfig, OpenAiCompatibleProvider, TokenUsage, ToolCall,
    UnconfiguredModelProvider,
};
pub use goal::{
    Goal, GoalBudget, GoalRunState, GoalRunner, GoalStatus, RunnerConfig, Worker, WorkerRegistry,
};
pub use injection::{sanitize_tool_result, InjectionGuard, InjectionHit, InjectionSeverity};
pub use learn::{
    generalize_to_graph, recorded_actions_from_sequence, ActionGraph, ActionNode, ActionType,
    FlowSkillManifest, FlowSkillPackage, FlowSkillStore, LearnPipeline, LearnRecorder, LearnState,
    ProactiveEngine, ProactiveSuggestion, RecordedAction, SemanticAnchor, Sensitivity,
    SuggestionAction,
};
pub use lease::{Lease, LeaseConfig, LeaseError, LeaseManager};
pub use mcp::{McpClient, McpRegistry, McpTool};
// McpServerConfig 现属插件域（owo-agent-plugins），由下面的 plugin 别名转出。
pub use node_agent::{NodeAgent, NodeStatus};
pub use notes::{
    add_block, append_child, block_text, doc_title, doc_to_md, generate_mixed_doc, get_block,
    insert_child, load_doc, md_to_doc, move_block, new_doc, remove_block, sanitize_html, save_doc,
    search_notes, walk, Block, BlockId, BlockKind, CanvasBlockData, CanvasNote, CanvasRect,
    FtsNoteIndex, InMemoryNoteIndex, NoteDoc, NoteIndex, NoteIndexer, SearchHit,
};
pub use observe::{
    desktop_observation, map_sim_events_to_actions, observation_from_sim_event, sample_desktop,
    value_hash, DesktopSnapshot, MemoryStore, Observation,
};
pub use ocr::{
    crop_scale_bmp, group_ocr_lines, ocr_bmp, ocr_bmp_detailed, ocr_bmp_region, ocr_engine_status,
    OcrBox, OcrEngineStatus, OcrLine, OcrSummary,
};
pub use paddle_ocr::{ocr_paddle, ocr_preferred, paddle_enabled, parse_paddle_jsonl};
pub use perception::{
    CaptureMeta, ContentRef, ForegroundApp, PerceptionEvent, PerceptionLayer, SituationSnapshot,
    SituationStore, TaskHypothesis, UiContext,
};
pub use permissions::{Approver, Decision, Level, PermissionProfile, PermissionRequest, Policy};
pub use plan::{verify_output, Plan, StepSpec, StepStatus, VerificationSpec};
pub use platform::{capture_screen, clipboard_sequence, poll_foreground_app};
pub use plugin::{
    discover_plugins, plugin_mcp_config, scan_plugin_for_risks, verify_plugin_signature,
    MarketPluginEntry, MarketUpdateManifest, PluginInstallReport, PluginInstallState,
    PluginManager, PluginManifest, PluginReviewState, PluginSignature, PluginStateStore,
    PluginSubmission, VersionsJson,
};
pub use project_space_store::{
    ProjectSpaceStoreBackend, ProjectSpaceStoreError, SqliteProjectSpaceStore,
};
pub use remote_step::{
    approval_request_event, approve_transport_task, submit_via_transport,
    submit_via_transport_with_timeout, ApprovalSpec, EvidenceItem, RemoteStep, RemoteStepEvent,
    RemoteStepKind, RemoteStepOutcome,
};
pub use sandbox::{
    available_isolation, evaluate_capability, inside_workspace, probe_platform_support,
    CapabilityEvaluation, FileScope, IsolationLevel, MockSandboxExecutor, NetworkPolicy,
    PlatformSupport, SandboxAuditEvent, SandboxAuditLog, SandboxCommand, SandboxError,
    SandboxExecutor, SandboxHandle, SandboxHealth, SandboxManager, SandboxPolicy, SandboxProcess,
    SandboxProcessStatus,
};
pub use scene::{
    elements_from_ocr_lines, elements_from_ui_nodes, elements_from_vision_groundings,
    merge_sources, text_hash, ElementRelation, EntityState, Evidence, EvidenceSource, GraphElement,
    SceneGraph, WindowState,
};
pub use session::{JsonSessionStore, Session, SessionStore};
pub use settings::{EgressSettings, Settings};
pub use share::{export_html, export_markdown};
pub use share_skill::{export_flow_skill_package, import_flow_skill_package};
pub use skill::{Skill, SkillRegistry};
pub use skill_pack::{
    discover_builtin_packages, install_builtin_packages, validate_skill_package,
    BuiltinSkillManifest, SkillPackageInfo,
};
pub use sqlite_store::SqliteSessionStore;
pub use stt::{LocalStt, SttOutcome};
pub use team_strategy::{
    RolePlan, TaskProfile, TeamPlan, TeamSelectionMode, TeamStrategyEngine, TeamStrategyThresholds,
};
pub use tool_effects::{EffectClass, ToolEffect, UNDECLARED_RISK_NOTE};
pub use tools::{Tool, ToolContext, ToolRegistry, ToolSpec};
pub use trace::{list_traces, load_trace, save_trace, TraceRecord};
pub use transition::{
    align_fork_point, annotate_fork_points, record_transition_experience, FailureClass,
    ForkAlignment, ForkDivergence, PredictionRef, PrivacyScope, TransitionOutcome, TransitionStore,
    TransitionTraceV1, VerifierResult,
};
pub use vision::{
    bmp_to_png, capture_vision_bmp, capture_vision_png, capture_vision_png_region,
    cross_validate_box, describe_image, ground_element, ollama_models, parse_verification,
    parse_vision_box, parse_vision_box_with_confidence, verification_prompt, vision_only_allowed,
    VisionBox, VisionConfig,
};
pub use whitelist::{AppTier, Whitelist, WhitelistEntry};
pub use window_template::{
    build_template, build_template_from_ocr, detect_template, detect_template_ocr, load_template,
    save_template, WindowRoi, WindowTemplate,
};
pub use worker_pool::{
    IsolationMode, PoolError, PoolWorker, WorkerBudget, WorkerId, WorkerPool, WorkerSpec,
    WorkerStatus,
};
pub use workflow::{
    compile_to_program, eval_expr, validate_definition, ActSpec, ActionBackend, Approval,
    AutoApprover, CheckpointRef, HumanApprover, LocateSpec, MockBackend, PermMode, PermissionClaim,
    SenseSpec, StepRecord as WorkflowStepRecord, TriggerKind, WorkflowDefinition, WorkflowEngine,
    WorkflowOutcome, WorkflowState, WorkflowStep, WorkflowTrigger,
};
pub use workswarm::{
    default_relay_roles, wait_cancel, CancelToken, CreateTeamRequest, HandoffFields, HumanWait,
    PhaseOutcome, RoleSpec, RoleWorker, RunMeta, SteerCommand, TeamCoordinator,
    TeamTemplateRegistry, WorkSwarmError,
};
pub use world_model::{
    advise_candidates, aggregate_calibration, delta_overlap, evaluate_prediction, shadow_step,
    Advice, AdviceMode, AssertionProbability, CalibrationReport, GuiWorldModel, ModelError,
    PredictionEvaluation, RuleWorldModel, RunMode, ShadowStepReport, TransitionMeta,
    UnavailableWorldModel, UncertaintyBucket, WorldModelContext, WorldPrediction,
    CLOSE_CANDIDATE_EPS, HIGH_UNCERTAINTY,
};
