//! DesktopWorld 环境协议（E0，对应主开发技术文档 §5.11）。
//!
//! 本模块把「桌面环境」收敛为统一的 [`DesktopEnv`] 协议：
//! `reset / observe / step / snapshot / restore / inject_fault / judge`，
//! 三层环境（S1 可编程模拟、S2 Windows VM、S3 授权真实桌面）复用同一动作与观测契约。
//!
//! 当前实现范围（E0）：
//! - **S1 可编程模拟环境** [`SimDesktopEnv`]：聊天 / 文件管理器 / 表单 / 轻量文档四类
//!   确定性应用状态机。同一 [`TaskSeed`] 可复现；隐藏状态作为程序化判分来源，
//!   不泄漏进观测（`structured_app_state` 在观测中恒为 None）。
//! - **快照 / 恢复 / 故障注入**：快照深拷贝全部应用状态；故障注入支持模态弹窗、
//!   元素漂移（锚点漂移）与迟钝步骤（时序干扰）。
//! - **环境注册表** [`EnvRegistry`]：环境实例 + [`ControllerLease`] 语义的单写租约
//!   （token + epoch fencing），保证同一环境同一时刻只有一个写作者。
//! - **兼容适配** [`SurfaceEnvAdapter`]：把既有 [`owo_agent_kernel::TaskSurface`]
//!   （SimTaskSurface/RealTaskSurface）适配为只读观测 + 动作注入的 DesktopEnv；
//!   不支持的能力显式返回 [`EnvError::Unsupported`]，不伪造。
//!
//! 安全边界：S1 动作不经过真实权限门控（沙箱内状态机）；S3 每一步仍必须经过
//! 用户环境的 Policy/Approval（由调用方负责，本模块不代办）。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// S1 环境协议版本（训练样本必须绑定该版本）。
pub const SIM_ENV_PROTOCOL: &str = "S1";
pub const SIM_ENV_VERSION: &str = "1.0.0";

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// 错误与基础 DTO
// ---------------------------------------------------------------------------

/// 环境层错误。语义性动作失败（被阻塞/无效果）不在此列，
/// 而是通过 [`StepResult::verdict`] 表达；这里只表达协议/基础设施错误。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
pub enum EnvError {
    #[error("环境尚未 reset：{0}")]
    NotReset(String),
    #[error("不支持的操作：{0}")]
    Unsupported(String),
    #[error("非法动作：{0}")]
    InvalidAction(String),
    #[error("未知快照：{0}")]
    UnknownSnapshot(String),
    #[error("租约失效：{0}")]
    StaleLease(String),
    #[error("应用错误：{0}")]
    App(String),
}

/// 动作风险等级（与 §5.12 世界模型共享）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low,
    Medium,
    High,
    Critical,
}

/// 动作种类：GUI / CLI / API / 等待 / 询问用户（§5.12.5 混合动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Gui,
    Cli,
    Api,
    Wait,
    AskUser,
}

/// S1 模拟应用类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimAppKind {
    Chat,
    Files,
    Form,
    Document,
}

impl SimAppKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Files => "files",
            Self::Form => "form",
            Self::Document => "document",
        }
    }
}

impl std::fmt::Display for SimAppKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 任务种子：环境 + 随机种子 + 任务资产。同一 seed + 同一动作序列必须可复现。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSeed {
    pub task_id: String,
    pub app: SimAppKind,
    pub seed: u64,
    /// 任务资产（初始联系人/文件/字段/文本等；按应用类别解析）。
    #[serde(default = "default_assets")]
    pub assets: Value,
}

fn default_assets() -> Value {
    json!({})
}

/// 环境观测（§5.11 WorldStateV1）。
///
/// S1 中 `scene_graph` 为可见元素渲染结果；`structured_app_state` 恒为 None，
/// 隐藏状态只供 [`DesktopEnv::judge`] 使用，不进入策略可见观测。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorldStateV1 {
    pub env_id: String,
    pub env_version: String,
    pub snapshot_id: Option<String>,
    pub timestamp: String,
    pub screenshot_ref: Option<String>,
    pub scene_graph: Value,
    pub accessibility_tree_ref: Option<String>,
    pub foreground_app: String,
    pub window_stack: Vec<String>,
    pub structured_app_state: Option<Value>,
    pub freshness: u64,
    pub privacy_labels: Vec<String>,
}

/// 场景元素（S1 渲染产物；与 §5.10.4 SceneElement 语义对齐）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimElement {
    pub id: String,
    pub role: String,
    pub text: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub enabled: bool,
    pub visible: bool,
}

impl SimElement {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.visible
            && x >= self.x
            && x < self.x + self.width
            && y >= self.y
            && y < self.y + self.height
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }
}

/// 字段级变化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldChange {
    pub path: String,
    pub before: Value,
    pub after: Value,
}

/// 观测状态差分（§5.12 世界模型的预测对象）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateDelta {
    pub added_elements: Vec<String>,
    pub removed_elements: Vec<String>,
    pub changed_fields: Vec<FieldChange>,
    pub window_changed: bool,
    pub summary: String,
}

impl StateDelta {
    pub fn is_empty(&self) -> bool {
        self.added_elements.is_empty()
            && self.removed_elements.is_empty()
            && self.changed_fields.is_empty()
            && !self.window_changed
    }

    /// 稳定指纹：排序后的结构摘要（用于频率统计与去重）。
    pub fn fingerprint(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut added = self.added_elements.clone();
        added.sort();
        let mut removed = self.removed_elements.clone();
        removed.sort();
        if !added.is_empty() {
            parts.push(format!("+{}", added.join(",")));
        }
        if !removed.is_empty() {
            parts.push(format!("-{}", removed.join(",")));
        }
        let mut changed: Vec<String> = self.changed_fields.iter().map(|c| c.path.clone()).collect();
        changed.sort();
        changed.dedup();
        if !changed.is_empty() {
            parts.push(format!("~{}", changed.join(",")));
        }
        if self.window_changed {
            parts.push("!window".to_string());
        }
        parts.join("|")
    }
}

/// 落地动作（§5.11 GroundedAction）。训练环境与生产执行器共享本结构，但不共享权限。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundedAction {
    pub action_id: String,
    pub kind: ActionKind,
    pub semantic_intent: String,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub target_evidence: Vec<String>,
    #[serde(default = "default_assets")]
    pub arguments: Value,
    #[serde(default)]
    pub expected_effects: Vec<String>,
    #[serde(default)]
    pub risk: RiskLevel,
    #[serde(default = "default_true")]
    pub reversible: bool,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// GUI 点击坐标（若提供）。数据集构建器用它做「坐标位于目标框」检查。
    #[serde(default)]
    pub click_point: Option<(i32, i32)>,
    /// 目标边界框 (x, y, w, h)（若提供）。
    #[serde(default)]
    pub target_bounds: Option<(i32, i32, i32, i32)>,
}

impl GroundedAction {
    /// 归一化动作签名（世界模型频率统计键）。
    pub fn signature(&self) -> String {
        let op = self
            .arguments
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or(match self.kind {
                ActionKind::Gui => "gui",
                ActionKind::Cli => "cli",
                ActionKind::Api => "api",
                ActionKind::Wait => "wait",
                ActionKind::AskUser => "ask_user",
            });
        let target = self.target_id.clone().unwrap_or_default();
        format!("{}::{}::{}", self.kind_name(), op, target)
    }

    /// 动作种类名（稳定字符串，用于签名与统计）。
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            ActionKind::Gui => "gui",
            ActionKind::Cli => "cli",
            ActionKind::Api => "api",
            ActionKind::Wait => "wait",
            ActionKind::AskUser => "ask_user",
        }
    }
}

/// 判定结果（判分/单步结果共用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    Pass {
        #[serde(default)]
        evidence: Vec<String>,
    },
    Fail {
        reason: String,
        #[serde(default)]
        evidence: Vec<String>,
    },
}

impl Verdict {
    pub fn passed(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }

    pub fn evidence(&self) -> &[String] {
        match self {
            Self::Pass { evidence } | Self::Fail { evidence, .. } => evidence,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Fail { reason, .. } => Some(reason),
            Self::Pass { .. } => None,
        }
    }
}

/// 奖励分量（确定性、可解释；不加不可复现的启发项）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewardParts {
    /// 本步是否推进任务（0.0~1.0）。
    pub progress: f64,
    /// 步数效率（1.0 = 无浪费步）。
    pub efficiency: f64,
    /// 安全性（高风险动作降权）。
    pub safety: f64,
}

impl RewardParts {
    pub fn zero() -> Self {
        Self {
            progress: 0.0,
            efficiency: 0.0,
            safety: 1.0,
        }
    }

    /// 加权合成：0.5*progress + 0.3*efficiency + 0.2*safety。
    pub fn composite(&self) -> f64 {
        0.5 * self.progress + 0.3 * self.efficiency + 0.2 * self.safety
    }
}

/// 单步结果（§5.11 StepResult）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    pub before_state_ref: String,
    pub action: GroundedAction,
    pub after_state_ref: String,
    pub observed_delta: StateDelta,
    pub verdict: Verdict,
    pub reward_parts: RewardParts,
    pub duration_ms: u64,
    #[serde(default)]
    pub error: Option<EnvError>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
}

/// 断言（程序化判分，不依赖 VLM）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Assertion {
    /// 可见元素文本包含子串。
    TextVisible { text: String },
    /// 隐藏状态路径取值相等（路径形如 `Chat.sent_log.0.text`）。
    StateEquals { path: String, value: Value },
    /// 隐藏状态路径包含：数组包含元素 / 字符串包含子串 / 其他类型相等。
    StateContains { path: String, value: Value },
    /// 隐藏状态路径数组长度不小于 count。
    CountAtLeast { path: String, count: usize },
}

impl Assertion {
    pub fn describe(&self) -> String {
        match self {
            Self::TextVisible { text } => format!("可见文本包含 {text:?}"),
            Self::StateEquals { path, value } => format!("状态 {path} == {value}"),
            Self::StateContains { path, value } => format!("状态 {path} 包含 {value}"),
            Self::CountAtLeast { path, count } => format!("状态 {path} 数量 >= {count}"),
        }
    }
}

/// 成功判分规格。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessSpec {
    pub name: String,
    pub assertions: Vec<Assertion>,
}

/// 故障注入规格（§5.11：失败注入用于训练与归因）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaultSpec {
    /// 模态弹窗：阻塞所有输入，直到点击关闭按钮。
    ModalPopup { text: String },
    /// 元素漂移：指定元素几何偏移（锚点漂移复现）。
    ElementDrift {
        element_id: String,
        dx: i32,
        dy: i32,
    },
    /// 迟钝步骤：接下来 steps 次用户输入动作无效果（时序干扰）。
    SluggishSteps { steps: u64 },
}

// ---------------------------------------------------------------------------
// DesktopEnv 协议
// ---------------------------------------------------------------------------

/// 统一桌面环境协议。三层环境（S1/S2/S3）必须复用同一动作与观测协议。
#[async_trait]
pub trait DesktopEnv: Send {
    /// 环境实例 id。
    fn env_id(&self) -> &str;
    /// 环境版本（训练样本必须绑定）。
    fn env_version(&self) -> &str;

    /// 重置到任务初始状态并返回首个观测。
    async fn reset(&mut self, task: TaskSeed) -> Result<WorldStateV1, EnvError>;
    /// 当前观测（不改变状态）。
    async fn observe(&mut self) -> Result<WorldStateV1, EnvError>;
    /// 执行一步动作，返回前后状态引用、观测差分与单步判定。
    async fn step(&mut self, action: GroundedAction) -> Result<StepResult, EnvError>;
    /// 创建状态快照。
    async fn snapshot(&mut self) -> Result<String, EnvError>;
    /// 恢复到指定快照。
    async fn restore(&mut self, snapshot: String) -> Result<WorldStateV1, EnvError>;
    /// 注入故障（仅模拟/训练环境应支持）。
    async fn inject_fault(&mut self, fault: FaultSpec) -> Result<(), EnvError>;
    /// 程序化判分：依据隐藏状态与可见文本，不依赖 VLM。
    async fn judge(&mut self, success: SuccessSpec) -> Result<Verdict, EnvError>;
}

// ---------------------------------------------------------------------------
// S1 应用状态机
// ---------------------------------------------------------------------------

/// 故障状态（嵌入每个应用状态；参与快照）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FaultState {
    popup: Option<String>,
    sluggish_remaining: u64,
    drifts: Vec<DriftRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DriftRecord {
    element_id: String,
    dx: i32,
    dy: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMsg {
    from: String,
    text: String,
    is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IncomingSpec {
    at_step: u64,
    from: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SentRecord {
    contact: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatState {
    contacts: Vec<String>,
    active: usize,
    messages: Vec<Vec<ChatMsg>>,
    input: String,
    focused: Option<String>,
    sent_log: Vec<SentRecord>,
    step_count: u64,
    incoming_schedule: Vec<IncomingSpec>,
    rng_state: u64,
    fault: FaultState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    name: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenameDialog {
    index: usize,
    value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FilesState {
    files: Vec<FileEntry>,
    selected: Option<usize>,
    dialog: Option<RenameDialog>,
    focused: Option<String>,
    deleted_log: Vec<String>,
    step_count: u64,
    fault: FaultState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FormState {
    fields: BTreeMap<String, String>,
    focused: Option<String>,
    submitted: Option<BTreeMap<String, String>>,
    error: Option<String>,
    step_count: u64,
    fault: FaultState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DocState {
    content: String,
    saved_content: String,
    saves: u64,
    focused: bool,
    step_count: u64,
    fault: FaultState,
}

/// 四类应用状态机（untagged：隐藏状态 JSON 路径不带枚举包装）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum SimApp {
    Chat(ChatState),
    Files(FilesState),
    Form(FormState),
    Document(DocState),
}

/// 单步动作的应用效果。
#[derive(Debug, Clone, PartialEq)]
enum StepEffect {
    Applied(String),
    Blocked(String),
    NoOp(String),
}

/// 确定性伪随机（xorshift64*），保证同一 seed 可复现。
fn rng_next(state: &mut u64) -> u64 {
    if *state == 0 {
        *state = 0x9E37_79B9_7F4A_7C15;
    }
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

mod sim_apps;

// ---------------------------------------------------------------------------
// SimDesktopEnv：DesktopEnv 的 S1 实现
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SimSnapshot {
    state: SimApp,
    step_count_total: u64,
}

/// S1 可编程模拟环境：确定性、可快照、可注入故障、可程序化判分。
pub struct SimDesktopEnv {
    env_id: String,
    env_version: String,
    task: Option<TaskSeed>,
    state: Option<SimApp>,
    freshness: u64,
    snapshots: BTreeMap<String, SimSnapshot>,
    snapshot_counter: u64,
    last_restored: Option<String>,
    step_count_total: u64,
}

impl SimDesktopEnv {
    pub fn new(env_id: impl Into<String>) -> Self {
        Self {
            env_id: env_id.into(),
            // reset 之前应用未知；reset 后为 `S1-<app>-<version>`（训练样本绑定环境版本）。
            env_version: format!("{SIM_ENV_PROTOCOL}-unreset-{SIM_ENV_VERSION}"),
            task: None,
            state: None,
            freshness: 0,
            snapshots: BTreeMap::new(),
            snapshot_counter: 0,
            last_restored: None,
            step_count_total: 0,
        }
    }

    pub fn task(&self) -> Option<&TaskSeed> {
        self.task.as_ref()
    }

    /// 以同一 TaskSeed（同 seed/同资产）克隆一个全新环境（批量并行 rollout 用）。
    pub fn clone_fresh(&self, new_env_id: impl Into<String>) -> Result<Self, EnvError> {
        let task = self
            .task
            .clone()
            .ok_or_else(|| EnvError::NotReset("原环境尚未 reset，无法克隆".into()))?;
        let mut env = Self::new(new_env_id);
        env.reset_sync(task)?;
        Ok(env)
    }

    /// 初始状态构造（seed + assets → 应用状态机）。
    fn build_app(task: &TaskSeed) -> SimApp {
        match task.app {
            SimAppKind::Chat => {
                let contacts: Vec<String> = task
                    .assets
                    .get("contacts")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .filter(|v: &Vec<String>| !v.is_empty())
                    .unwrap_or_else(|| vec!["Alice".into(), "Bob".into()]);
                let incoming_schedule: Vec<IncomingSpec> = task
                    .assets
                    .get("incoming")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|item| {
                                Some(IncomingSpec {
                                    at_step: item.get("at_step")?.as_u64()?,
                                    from: item.get("from")?.as_str()?.to_string(),
                                    text: item.get("text")?.as_str()?.to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                SimApp::Chat(ChatState {
                    messages: contacts.iter().map(|_| Vec::new()).collect(),
                    contacts,
                    active: 0,
                    input: String::new(),
                    focused: None,
                    sent_log: Vec::new(),
                    step_count: 0,
                    incoming_schedule,
                    rng_state: task.seed,
                    fault: FaultState::default(),
                })
            }
            SimAppKind::Files => {
                let names: Vec<String> = task
                    .assets
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .filter(|v: &Vec<String>| !v.is_empty())
                    .unwrap_or_else(|| vec!["报告.docx".into(), "数据.xlsx".into()]);
                let files = names
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| FileEntry {
                        name,
                        size: ((task.seed.wrapping_add(i as u64)) % 97 + 1) * 1024,
                    })
                    .collect();
                SimApp::Files(FilesState {
                    files,
                    selected: None,
                    dialog: None,
                    focused: None,
                    deleted_log: Vec::new(),
                    step_count: 0,
                    fault: FaultState::default(),
                })
            }
            SimAppKind::Form => {
                let mut fields = BTreeMap::new();
                if let Some(obj) = task.assets.get("fields").and_then(Value::as_object) {
                    for (k, v) in obj {
                        fields.insert(k.clone(), v.as_str().unwrap_or_default().to_string());
                    }
                }
                if fields.is_empty() {
                    fields.insert("name".into(), String::new());
                    fields.insert("email".into(), String::new());
                }
                SimApp::Form(FormState {
                    fields,
                    focused: None,
                    submitted: None,
                    error: None,
                    step_count: 0,
                    fault: FaultState::default(),
                })
            }
            SimAppKind::Document => {
                let initial = task
                    .assets
                    .get("initial_text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                SimApp::Document(DocState {
                    saved_content: initial.clone(),
                    content: initial,
                    saves: 0,
                    focused: false,
                    step_count: 0,
                    fault: FaultState::default(),
                })
            }
        }
    }

    // 同步内核（注册表在持锁期间不 await；trait 异步方法委托到这里）。

    pub fn reset_sync(&mut self, task: TaskSeed) -> Result<WorldStateV1, EnvError> {
        self.env_version = format!("{SIM_ENV_PROTOCOL}-{}-{SIM_ENV_VERSION}", task.app);
        self.state = Some(Self::build_app(&task));
        self.task = Some(task);
        self.snapshots.clear();
        self.snapshot_counter = 0;
        self.last_restored = None;
        self.step_count_total = 0;
        self.observe_sync()
    }

    pub fn observe_sync(&mut self) -> Result<WorldStateV1, EnvError> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| EnvError::NotReset(self.env_id.clone()))?;
        self.freshness += 1;
        let elements = state.render();
        let kind = state.kind();
        Ok(WorldStateV1 {
            env_id: self.env_id.clone(),
            env_version: self.env_version.clone(),
            snapshot_id: self.last_restored.clone(),
            timestamp: now_rfc3339(),
            screenshot_ref: None,
            scene_graph: json!({ "elements": elements }),
            accessibility_tree_ref: None,
            foreground_app: format!("owo-sim-{kind}"),
            window_stack: state.window_stack(),
            structured_app_state: None,
            freshness: self.freshness,
            privacy_labels: vec!["s1_sim".into()],
        })
    }

    pub fn step_sync(&mut self, action: GroundedAction) -> Result<StepResult, EnvError> {
        let started = Instant::now();
        let (before_ref, before_elements, before_stack) = {
            let state = self
                .state
                .as_ref()
                .ok_or_else(|| EnvError::NotReset(self.env_id.clone()))?;
            (state.state_ref(), state.render(), state.window_stack())
        };

        let effect = match action.kind {
            ActionKind::Gui => {
                let op = action
                    .arguments
                    .get("op")
                    .and_then(Value::as_str)
                    .unwrap_or("click");
                match op {
                    "click" => {
                        let (x, y) = resolve_click_point(&action)?;
                        self.state.as_mut().expect("checked above").click(x, y)
                    }
                    "type" => {
                        let text = action
                            .arguments
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| EnvError::InvalidAction("type 缺少 text 参数".into()))?
                            .to_string();
                        self.state.as_mut().expect("checked above").type_text(&text)
                    }
                    "key" => {
                        let key = action
                            .arguments
                            .get("key")
                            .and_then(Value::as_str)
                            .ok_or_else(|| EnvError::InvalidAction("key 缺少 key 参数".into()))?
                            .to_string();
                        self.state.as_mut().expect("checked above").key(&key)
                    }
                    "launch" => StepEffect::NoOp("S1 不模拟应用启动".into()),
                    other => return Err(EnvError::InvalidAction(format!("未知 GUI op：{other}"))),
                }
            }
            ActionKind::Wait => {
                let steps = action
                    .arguments
                    .get("steps")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1);
                self.state
                    .as_mut()
                    .expect("checked above")
                    .wait_steps(steps)
            }
            ActionKind::AskUser => StepEffect::Blocked("S1 无人类节点，无法应答 ask_user".into()),
            ActionKind::Cli | ActionKind::Api => {
                return Err(EnvError::Unsupported(format!(
                    "S1 模拟环境不支持 {:?} 动作（混合动作应路由到 CLI/API 执行器）",
                    action.kind
                )));
            }
        };

        let state = self.state.as_mut().expect("checked above");
        self.step_count_total += 1;
        self.last_restored = None;
        let after_ref = state.state_ref();
        let after_elements = state.render();
        let after_stack = state.window_stack();
        let hit_id = match &effect {
            StepEffect::Applied(_) | StepEffect::Blocked(_) => action
                .click_point
                .or_else(|| click_point_from_args(&action.arguments))
                .and_then(|(x, y)| state.hit_test(x, y).map(|e| e.id)),
            StepEffect::NoOp(_) => None,
        };

        let delta = diff_observation(
            &before_elements,
            &after_elements,
            &before_stack,
            &after_stack,
        );
        let applied = matches!(effect, StepEffect::Applied(_));
        let verdict = match &effect {
            StepEffect::Applied(summary) => Verdict::Pass {
                evidence: vec![summary.clone()],
            },
            StepEffect::Blocked(reason) => Verdict::Fail {
                reason: reason.clone(),
                evidence: Vec::new(),
            },
            StepEffect::NoOp(reason) => Verdict::Fail {
                reason: reason.clone(),
                evidence: Vec::new(),
            },
        };
        let efficiency = if applied { 1.0 } else { 0.25 };
        let safety = if action.risk <= RiskLevel::Medium {
            1.0
        } else {
            0.5
        };
        let reward = RewardParts {
            progress: if applied { 1.0 } else { 0.0 },
            efficiency,
            safety,
        };
        let mut evidence_refs = vec![format!(
            "sim:{}:step:{}",
            self.env_id, self.step_count_total
        )];
        if let Some(id) = hit_id {
            evidence_refs.push(format!("element:{id}"));
        }
        Ok(StepResult {
            before_state_ref: before_ref,
            action,
            after_state_ref: after_ref,
            observed_delta: delta,
            verdict,
            reward_parts: reward,
            duration_ms: started.elapsed().as_millis() as u64,
            error: None,
            evidence_refs,
        })
    }

    pub fn snapshot_sync(&mut self) -> Result<String, EnvError> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| EnvError::NotReset(self.env_id.clone()))?
            .clone();
        self.snapshot_counter += 1;
        let id = format!("snap-{}-{}", self.env_id, self.snapshot_counter);
        self.snapshots.insert(
            id.clone(),
            SimSnapshot {
                state,
                step_count_total: self.step_count_total,
            },
        );
        Ok(id)
    }

    pub fn restore_sync(&mut self, snapshot: String) -> Result<WorldStateV1, EnvError> {
        let snap = self
            .snapshots
            .get(&snapshot)
            .ok_or_else(|| EnvError::UnknownSnapshot(snapshot.clone()))?
            .clone();
        self.state = Some(snap.state);
        self.step_count_total = snap.step_count_total;
        self.last_restored = Some(snapshot);
        self.observe_sync()
    }

    pub fn inject_fault_sync(&mut self, fault: FaultSpec) -> Result<(), EnvError> {
        let state = self
            .state
            .as_mut()
            .ok_or_else(|| EnvError::NotReset(self.env_id.clone()))?;
        state.apply_fault(fault);
        self.last_restored = None;
        Ok(())
    }

    pub fn judge_sync(&mut self, spec: SuccessSpec) -> Result<Verdict, EnvError> {
        let state = self
            .state
            .as_ref()
            .ok_or_else(|| EnvError::NotReset(self.env_id.clone()))?;
        let hidden = state.hidden_json();
        let elements = state.render();
        let mut evidence = Vec::new();
        for assertion in &spec.assertions {
            match evaluate_assertion(assertion, &hidden, &elements) {
                Ok(()) => evidence.push(format!("PASS {}", assertion.describe())),
                Err(reason) => {
                    return Ok(Verdict::Fail {
                        reason: format!("断言失败：{}（{reason}）", assertion.describe()),
                        evidence,
                    });
                }
            }
        }
        Ok(Verdict::Pass { evidence })
    }

    /// 隐藏状态 JSON（仅供判分/测试；策略侧不应依赖）。
    pub fn hidden_state_json(&self) -> Option<Value> {
        self.state.as_ref().map(|s| s.hidden_json())
    }
}

fn resolve_click_point(action: &GroundedAction) -> Result<(i32, i32), EnvError> {
    if let Some(point) = action.click_point {
        return Ok(point);
    }
    click_point_from_args(&action.arguments).ok_or_else(|| {
        EnvError::InvalidAction("GUI click 缺少坐标（click_point 或 arguments.x/y）".into())
    })
}

fn click_point_from_args(args: &Value) -> Option<(i32, i32)> {
    let x = args.get("x")?.as_i64()? as i32;
    let y = args.get("y")?.as_i64()? as i32;
    Some((x, y))
}

/// 可见观测差分：元素增删、元素文本/可用性变化、窗口栈变化。
fn diff_observation(
    before: &[SimElement],
    after: &[SimElement],
    before_stack: &[String],
    after_stack: &[String],
) -> StateDelta {
    let before_ids: HashMap<&str, &SimElement> =
        before.iter().map(|e| (e.id.as_str(), e)).collect();
    let after_ids: HashMap<&str, &SimElement> = after.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut added: Vec<String> = after_ids
        .keys()
        .filter(|id| !before_ids.contains_key(**id))
        .map(|id| id.to_string())
        .collect();
    let mut removed: Vec<String> = before_ids
        .keys()
        .filter(|id| !after_ids.contains_key(**id))
        .map(|id| id.to_string())
        .collect();
    let mut changed = Vec::new();
    for (id, element) in &after_ids {
        if let Some(prev) = before_ids.get(id) {
            if prev.text != element.text {
                changed.push(FieldChange {
                    path: format!("{id}.text"),
                    before: Value::String(prev.text.clone()),
                    after: Value::String(element.text.clone()),
                });
            }
            if prev.enabled != element.enabled {
                changed.push(FieldChange {
                    path: format!("{id}.enabled"),
                    before: json!(prev.enabled),
                    after: json!(element.enabled),
                });
            }
        }
    }
    added.sort();
    removed.sort();
    let window_changed = before_stack != after_stack;
    let mut summary_parts = Vec::new();
    if !added.is_empty() {
        summary_parts.push(format!("新增 {} 个元素", added.len()));
    }
    if !removed.is_empty() {
        summary_parts.push(format!("移除 {} 个元素", removed.len()));
    }
    if !changed.is_empty() {
        summary_parts.push(format!("{} 个字段变化", changed.len()));
    }
    if window_changed {
        summary_parts.push("窗口栈变化".to_string());
    }
    StateDelta {
        added_elements: added,
        removed_elements: removed,
        changed_fields: changed,
        window_changed,
        summary: if summary_parts.is_empty() {
            "无可见变化".to_string()
        } else {
            summary_parts.join("；")
        },
    }
}

/// 点分路径解析（数组索引为数字段）。
fn resolve_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        current = if let Ok(idx) = segment.parse::<usize>() {
            current.get(idx)?
        } else {
            current.get(segment)?
        };
    }
    Some(current)
}

fn evaluate_assertion(
    assertion: &Assertion,
    hidden: &Value,
    elements: &[SimElement],
) -> Result<(), String> {
    match assertion {
        Assertion::TextVisible { text } => {
            if elements
                .iter()
                .any(|e| e.visible && e.text.contains(text.as_str()))
            {
                Ok(())
            } else {
                Err("未找到可见文本".into())
            }
        }
        Assertion::StateEquals { path, value } => {
            let actual = resolve_path(hidden, path).ok_or("路径不存在")?;
            if actual == value {
                Ok(())
            } else {
                Err(format!("实际值 {actual}"))
            }
        }
        Assertion::StateContains { path, value } => {
            let actual = resolve_path(hidden, path).ok_or("路径不存在")?;
            match actual {
                Value::Array(items) => {
                    if items.contains(value) {
                        Ok(())
                    } else {
                        Err("数组不包含目标值".into())
                    }
                }
                Value::String(s) => match value.as_str() {
                    Some(needle) if s.contains(needle) => Ok(()),
                    _ => Err("字符串不包含子串".into()),
                },
                other => {
                    if other == value {
                        Ok(())
                    } else {
                        Err("值不相等".into())
                    }
                }
            }
        }
        Assertion::CountAtLeast { path, count } => {
            let actual = resolve_path(hidden, path).ok_or("路径不存在")?;
            match actual {
                Value::Array(items) if items.len() >= *count => Ok(()),
                Value::Array(items) => Err(format!("实际数量 {}", items.len())),
                _ => Err("路径不是数组".into()),
            }
        }
    }
}

#[async_trait]
impl DesktopEnv for SimDesktopEnv {
    fn env_id(&self) -> &str {
        &self.env_id
    }

    fn env_version(&self) -> &str {
        &self.env_version
    }

    async fn reset(&mut self, task: TaskSeed) -> Result<WorldStateV1, EnvError> {
        self.reset_sync(task)
    }

    async fn observe(&mut self) -> Result<WorldStateV1, EnvError> {
        self.observe_sync()
    }

    async fn step(&mut self, action: GroundedAction) -> Result<StepResult, EnvError> {
        self.step_sync(action)
    }

    async fn snapshot(&mut self) -> Result<String, EnvError> {
        self.snapshot_sync()
    }

    async fn restore(&mut self, snapshot: String) -> Result<WorldStateV1, EnvError> {
        self.restore_sync(snapshot)
    }

    async fn inject_fault(&mut self, fault: FaultSpec) -> Result<(), EnvError> {
        self.inject_fault_sync(fault)
    }

    async fn judge(&mut self, success: SuccessSpec) -> Result<Verdict, EnvError> {
        self.judge_sync(success)
    }
}

mod runtime;

pub use runtime::{EnvLeaseRecord, EnvRegistry, LeaseProof, SurfaceEnvAdapter};
