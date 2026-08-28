//! DesktopWorld / WorldModel HTTP 面（Part 5 R1，主文档 §8.5、§5.11、§5.12、§9.0 T0 遗留）。
//!
//! 把 E0（desktop_env）/T0（transition + dataset_builder）/WM0（world_model）经 HTTP
//! 对外接通，形成可被真实调用方消费的闭环：
//!
//! 环境面（/desktop-envs，S1 可编程环境；写路径 lease fencing）：
//! - `POST /desktop-envs`                    创建环境（+自动授予初始写租约，返回凭证与首帧观测）
//! - `POST /desktop-envs/{id}/reset`         按新任务种子复位（写路径；§8.5）
//! - `POST /desktop-envs/{id}/observe`       只读观测（读路径，不要求租约；§8.5）
//! - `POST /desktop-envs/{id}/step`          执行一步：影子预测 → 执行 → 预测/实际对照 →
//!   transition 落盘（写路径；§8.5，闭环粘合点）
//! - `POST /desktop-envs/{id}/snapshot`      保存快照（读路径；§8.5）
//! - `POST /desktop-envs/{id}/restore`       恢复快照（写路径；§8.5）
//! - `POST /desktop-envs/{id}/judge`         程序化判分（读路径；§8.5）
//! - `POST /desktop-envs/{id}/inject-fault`  故障注入（写路径；E0 DesktopEnv 既有能力，
//!   支撑失败轨迹 → fork 点 → 失败样本数据集）
//! - `POST /desktop-envs/{id}/lease`         租约 acquire/renew/release（fencing 运维；
//!   长循环调用方需要续租，§4.2 单写者纪律）
//!
//! 模型面（/world-model，§8.5）：
//! - `POST /world-model/predict`             单步结构化预测（active provider；
//!   无规则时显式确定性回退，不伪造预测）
//! - `GET  /world-model/providers`           provider 与候选健康（影子样本 + 校准聚合）
//!
//! 数据面（§8.5 / §5.12.3 数据飞轮）：
//! - `GET  /transitions/{id}`                单条 transition（预测 vs 实际对照，训练数据溯源）
//! - `POST /datasets/build`                  触发 DatasetBuilder（顺序清洗+去重+平衡+清单）
//! - `GET  /datasets/{id}/manifest`          数据集清单
//!
//! 模型治理（§5.12.4 晋升门控）：
//! - `POST /model-candidates`                注册新候选（shadow 起步；不得静默成为 active）
//! - `POST /model-candidates/{id}/promote`   显式 ack + 理由 + 影子样本齐备才允许晋升
//!
//! 数据集持久化（第二路）：
//! - `datasets/` 目录在 [`DesktopWorldHub::new`] 时全量扫描重建索引；
//! - 每个 `.json` 都按「文件名 ↔ dataset_id、结构、计数一致性、content_hash」完整校验；
//!   任何损坏文件都让初始化**带具体路径 fail-fast**，绝不静默忽略；
//! - `GET /datasets/{id}/manifest` 以持久化数据为权威（读盘 + 校验），不再只信进程内 HashMap。
//!
//! 候选治理（第二路，修订 R1 遗留的伪影子样本问题）：
//! - 候选必须声明 provider 身份（[`CandidateProviderRef`]）；缺省为 metadata_only；
//! - 只有真实接线了可执行 provider 的 shadow 候选才会通过并行预测积累影子样本
//!   （[`DesktopWorldHub::wire_candidate_executor`]）；无 provider 的候选**零样本**；
//! - 从 transition 语料克隆 active 规则模型「代跑」生成独立评估的行为已移除——
//!   那类样本只是复制 active 的输出（伪影），不可作为晋升依据；
//! - 晋升门槛 = 有真实接线 provider + 样本数达标 + 校准摘要 + 相对上一任 active
//!   关键指标无超阈值退化 + 调用方显式 ack 与理由；当前尚无 WM1 真实调用面，
//!   未接线/伪声明的候选一律明确拒绝晋升（显式报错，不伪造已验证状态）。
//!
//! 运行态：模块内 `OnceLock` 单例 [`DesktopWorldHub`]（进程内环境注册表、transition 日志
//! （`data_root/desktop_world/transitions.jsonl`，崩溃重放幂等）、provider/候选登记、数据集
//! 清单（启动时从磁盘恢复）、经验接线）。**不给 AppState 加字段**。
//!
//! 协议约束（与 fleet_api 一致）：本模块不引用 `crate::`/`super::`；`AppState` 全限定名
//! `owo_agent_server::AppState`；错误统一 `(StatusCode, Json({error}))`。
//! 安全：写路径（step/reset/restore/inject-fault/lease 变更）必须携带与当前租约完全匹配的
//! `LeaseProof`，不匹配即 409 fencing；预测永不参与、不覆盖动作决策（§4.2 预测不是事实）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use owo_agent_core::dataset_builder::{
    build_dataset, save_manifest, DatasetBuilderConfig, DatasetManifest,
};
use owo_agent_core::desktop_env::{
    EnvError, EnvRegistry, GroundedAction, LeaseProof, RewardParts, SuccessSpec, TaskSeed, Verdict,
    WorldStateV1,
};
use owo_agent_core::experience_store::ExperienceStore;
use owo_agent_core::transition::{
    record_transition_experience, FailureClass, PredictionRef, PrivacyScope, TransitionOutcome,
    TransitionStore, TransitionTraceV1, VerifierResult,
};
use owo_agent_core::world_model::{
    advise_candidates, aggregate_calibration, evaluate_prediction, CalibrationReport,
    GuiWorldModel, PredictionEvaluation, RuleWorldModel, WorldModelContext, WorldPrediction,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn api_err(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// 环境层错误的 HTTP 映射：fencing 冲突 409；未知快照 404；协议/输入类 400。
fn env_err_response(e: &EnvError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        EnvError::StaleLease(_) => api_err(StatusCode::CONFLICT, e.to_string()),
        EnvError::UnknownSnapshot(_) => api_err(StatusCode::NOT_FOUND, e.to_string()),
        EnvError::NotReset(_)
        | EnvError::Unsupported(_)
        | EnvError::InvalidAction(_)
        | EnvError::App(_) => api_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// 数据集清单持久化（第二路：启动恢复 + 磁盘权威读取）
// ---------------------------------------------------------------------------

/// 数据集 id 合法性（防路径穿越；兼容 builder 生成的 `dataset-<uuid>` 与自定义 id）。
fn valid_dataset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// 清单落盘路径。
fn dataset_manifest_path(dir: &std::path::Path, id: &str) -> PathBuf {
    dir.join("datasets").join(format!("{id}.json"))
}

/// 重算 sample_ids 的内容哈希（与 core `build_dataset` 完全一致的口径）。
fn manifest_content_hash(sample_ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    for id in sample_ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 恢复场景下的清单完整性校验：计数一致性 + content_hash 重算比对。
fn validate_recovered_manifest(
    manifest: &DatasetManifest,
    path: &std::path::Path,
) -> Result<(), String> {
    let shown = path.display();
    if manifest.dataset_id.trim().is_empty() {
        return Err(format!("数据集清单损坏（{shown}）：dataset_id 为空"));
    }
    if manifest.accepted_count != manifest.sample_ids.len() {
        return Err(format!(
            "数据集清单损坏（{shown}）：accepted_count={} 与 sample_ids 数量 {} 不一致",
            manifest.accepted_count,
            manifest.sample_ids.len()
        ));
    }
    if manifest.success_count + manifest.failure_count != manifest.accepted_count {
        return Err(format!(
            "数据集清单损坏（{shown}）：success_count({}) + failure_count({}) != accepted_count({})",
            manifest.success_count, manifest.failure_count, manifest.accepted_count
        ));
    }
    let expected = manifest_content_hash(&manifest.sample_ids);
    if expected != manifest.content_hash {
        return Err(format!(
            "数据集清单损坏（{shown}）：content_hash 不一致，清单可能被篡改或版本不兼容\
             （期望 {expected}，实际 {}）",
            manifest.content_hash
        ));
    }
    Ok(())
}

/// 从磁盘按 id 读取清单（持久化数据为权威）：读 + 解析 + 全量校验。
///
/// 返回 Ok(None) 表示该 id 在磁盘上不存在（非法 id 也归入不存在）；
/// 文件存在但损坏时返回带具体路径的错误（不回退到内存缓存）。
fn load_dataset_manifest_from_disk(
    dir: &std::path::Path,
    id: &str,
) -> Result<Option<DatasetManifest>, String> {
    if !valid_dataset_id(id) {
        return Ok(None);
    }
    let path = dataset_manifest_path(dir, id);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取数据集清单失败（{}）：{e}", path.display()))?;
    let manifest: DatasetManifest = serde_json::from_str(&text)
        .map_err(|e| format!("数据集清单损坏（{}）：JSON 解析失败：{e}", path.display()))?;
    validate_recovered_manifest(&manifest, &path)?;
    Ok(Some(manifest))
}

/// 扫描 datasets/ 目录重建索引：每个 `.json` 都必须是一份通过完整校验的清单；
/// 任何异常文件都让初始化失败并带上具体路径（fail-fast，绝不静默忽略）。
fn recover_datasets(dir: &std::path::Path) -> Result<HashMap<String, DatasetManifest>, String> {
    let datasets_dir = dir.join("datasets");
    let mut out = HashMap::new();
    let entries = match std::fs::read_dir(&datasets_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(format!(
                "扫描 datasets 目录失败（{}）：{e}",
                datasets_dir.display()
            ));
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|r| r.ok().map(|e| e.path()))
        // 保证错误与插入顺序确定性（跨平台复现同一条报错）。
        .collect();
    paths.sort();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let Some(stem) = name.strip_suffix(".json") else {
            return Err(format!(
                "数据集目录存在非法文件（{}）：datasets/ 内只允许 <dataset_id>.json 清单",
                path.display()
            ));
        };
        if !valid_dataset_id(stem) {
            return Err(format!(
                "数据集清单文件名非法（{}）：必须是 <dataset_id>.json 且 id 只含字母数字._-",
                path.display()
            ));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("数据集清单损坏（{}）：无法读取：{e}", path.display()))?;
        let manifest: DatasetManifest = serde_json::from_str(&text)
            .map_err(|e| format!("数据集清单损坏（{}）：JSON 解析失败：{e}", path.display()))?;
        if stem != manifest.dataset_id {
            return Err(format!(
                "数据集清单文件名与内容不一致（{}）：文件名 id={stem}，清单 dataset_id={}",
                path.display(),
                manifest.dataset_id
            ));
        }
        validate_recovered_manifest(&manifest, &path)?;
        out.insert(manifest.dataset_id.clone(), manifest);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 运行态
// ---------------------------------------------------------------------------

/// 候选状态（§5.12.4：新模型版本先 shadow，达标后人工晋升 active）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// 影子运行：只记录预测对照，不服务在线预测。
    Shadow,
    /// 在线服务（当前 active provider 恒为此状态）。
    Active,
    /// 已弃用。
    Rejected,
}

/// 世界模型候选登记。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCandidate {
    pub candidate_id: String,
    pub model_id: String,
    pub model_version: String,
    /// 来源说明（内置/注册来源）。
    pub source: String,
    pub status: CandidateStatus,
    pub created_at: String,
    pub promoted_at: Option<String>,
    pub promote_reason: Option<String>,
    /// provider 身份治理：缺省 metadata_only（老 JSON 无该字段时反序列化为缺省值）。
    #[serde(default)]
    pub provider: CandidateProviderRef,
    /// 晋升时刻的校准摘要快照（只有晋升成功过的候选才有）。
    #[serde(default)]
    pub calibration_summary: Option<owo_agent_core::world_model::CalibrationReport>,
}

/// 单步影子评估的候选归属（评估本身不携带模型身份，归属在登记时固化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEvaluation {
    pub candidate_id: String,
    pub evaluation: PredictionEvaluation,
}

/// 候选 provider 身份（§5.12.4 治理）。
///
/// 语义：**声明 ≠ 接线**。`External` 只是登记调用方声明的可执行 provider 身份；
/// 影子样本只来自本进程真实接线的预测器（`DesktopWorldHub::wire_candidate_executor`）。
/// `MetadataOnly` 候选零样本、不可晋升；克隆 active 规则模型代跑产生的样本视为伪影，
/// 不属于任何合法影子链路。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CandidateProviderRef {
    /// 显式声明的外部可执行 provider（如 WM1 推理端点、本地神经模型服务）。
    /// `kind` 为提供方类型标识，`locator` 为定位串（端点/资源标识），进入审计口径。
    External {
        /// provider 类型标识（如 `wm1-http`、`local-onnx`）。
        kind: String,
        /// 定位串（端点或资源标识）。
        locator: String,
    },
    /// 仅登记元数据（缺省）：无可执行 provider，不参与影子采样，不可晋升。
    #[default]
    MetadataOnly,
}

impl CandidateProviderRef {
    /// 是否声明了外部可执行 provider（不代表已接线）。
    pub fn is_external(&self) -> bool {
        matches!(self, Self::External { .. })
    }

    /// 审计用描述串。
    pub fn describe(&self) -> String {
        match self {
            Self::External { kind, locator } => format!("external:{kind}@{locator}"),
            Self::MetadataOnly => "metadata_only".to_string(),
        }
    }
}

/// provider 与候选登记 + 校准样本积累。
#[derive(Debug)]
pub struct ProviderRegistry {
    /// 当前 active 候选的 candidate_id。
    pub active: String,
    pub candidates: HashMap<String, ModelCandidate>,
    /// 影子评估积累（内存；transition 语料为持久化权威，评估可由语料重放重建）。
    pub evaluations: Vec<ModelEvaluation>,
}

/// 已接线的候选执行器（candidate_id → 真实可调用预测器）。
///
/// 这是影子样本的唯一合法来源入口：没有挂在这里的候选不会得到任何并行预测样本。
struct WiredExecutor {
    /// 接线时的 provider 类型标识。
    kind: String,
    /// 接线时的定位串。
    locator: String,
    /// 真实预测器。
    model: Arc<dyn GuiWorldModel>,
}

/// DesktopWorld 运行态（模块内单例；状态目录 `data_root/desktop_world`）。
pub struct DesktopWorldHub {
    /// S1 环境实例 + 单写租约（TTL 5 分钟；经 /desktop-envs/{id}/lease 续租）。
    pub envs: EnvRegistry,
    /// transition 幂等日志（JSONL 重放恢复）。
    pub transitions: Mutex<TransitionStore>,
    /// provider/候选/校准。
    pub providers: Mutex<ProviderRegistry>,
    /// 数据集清单启动恢复索引（id → manifest，来自 datasets/ 目录扫描；
    /// 读取接口以磁盘为权威，本索引用于快速枚举与构建登记）。
    pub datasets: Mutex<HashMap<String, DatasetManifest>>,
    /// env_id → 创建/复位时的 TaskSeed（transition 的 task_id 溯源）。
    pub env_tasks: Mutex<HashMap<String, TaskSeed>>,
    /// 经验接线（transition 终态 → ExperienceKind::Transition，幂等）。
    pub experience: ExperienceStore,
    /// 持久化根目录。
    pub dir: PathBuf,
    /// 已接线的候选执行器（影子样本唯一合法来源；见 [`WiredExecutor`]）。
    executors: Mutex<HashMap<String, WiredExecutor>>,
}

/// 手动 Debug：内部含不可派生 Debug 的 trait-object 执行器与多把锁；
/// 只输出治理摘要（数据根、当前 active、数据集索引规模、影子样本量），不展开明细。
impl std::fmt::Debug for DesktopWorldHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopWorldHub")
            .field("dir", &self.dir)
            .field("active", &self.active_candidate_id())
            .field(
                "dataset_count",
                &self.datasets.lock().map(|d| d.len()).unwrap_or(0),
            )
            .field(
                "evaluation_samples",
                &self
                    .providers
                    .lock()
                    .map(|p| p.evaluations.len())
                    .unwrap_or(0),
            )
            .finish_non_exhaustive()
    }
}

impl DesktopWorldHub {
    /// 新建运行态（持久化目录 data_root/desktop_world；测试可独立构造，避免跨测试污染）。
    ///
    /// 启动即扫描 `desktop_world/datasets/` 全量恢复数据集索引：
    /// 任何损坏清单（非法文件名 / JSON 解析失败 / 计数不一致 / content_hash 不符）
    /// 都返回带具体路径的错误，fail-fast 而不是静默忽略。
    pub fn new(data_root: &std::path::Path) -> Result<Arc<Self>, String> {
        let dir = data_root.join("desktop_world");
        std::fs::create_dir_all(dir.join("datasets"))
            .map_err(|e| format!("创建 desktop_world 目录失败：{e}"))?;
        let transitions = TransitionStore::load(dir.join("transitions.jsonl"))
            .map_err(|e| format!("重放 transition 日志失败：{e}"))?;
        let recovered_datasets = recover_datasets(&dir)?;
        let now = rfc3339_now();
        let mut candidates = HashMap::new();
        // WM0 内置候选：第一世界模型，直接 active（新候选经 /model-candidates 注册为 shadow）。
        candidates.insert(
            "wm0-rule".to_string(),
            ModelCandidate {
                candidate_id: "wm0-rule".into(),
                model_id: "wm-rule-v1".into(),
                model_version: "1.0.0".into(),
                source: "内置：WM0 规则/频率基线（由 transition 语料即时构建）".into(),
                status: CandidateStatus::Active,
                created_at: now.clone(),
                promoted_at: Some(now.clone()),
                promote_reason: Some("WM0 为首个世界模型，内置即 active".into()),
                // 内置候选的身份是真实的进程内执行路径（active 规则模型本体），
                // 不是外部声明；晋升门控对它按已接线内置 provider 处理。
                provider: CandidateProviderRef::External {
                    kind: "builtin-wm0-rule-corpus".into(),
                    locator: "in-process:active_rule_model".into(),
                },
                calibration_summary: None,
            },
        );
        let experience = ExperienceStore::new(Some(dir.join("experience.jsonl")))
            .map_err(|e| format!("经验存储初始化失败：{e}"))?;
        Ok(Arc::new(Self {
            envs: EnvRegistry::new(Duration::from_secs(300)),
            transitions: Mutex::new(transitions),
            providers: Mutex::new(ProviderRegistry {
                active: "wm0-rule".into(),
                candidates,
                evaluations: Vec::new(),
            }),
            datasets: Mutex::new(recovered_datasets),
            env_tasks: Mutex::new(HashMap::new()),
            experience,
            dir,
            executors: Mutex::new(HashMap::new()),
        }))
    }

    /// 当前 active 世界模型（WM0：由全部 transition 语料即时构建的规则模型；语料为空返回 None）。
    pub fn active_rule_model(&self) -> Option<RuleWorldModel> {
        let (model_id, model_version) = self.active_model_tags();
        let traces: Vec<TransitionTraceV1> = {
            let store = self.transitions.lock().ok()?;
            store.traces().into_iter().cloned().collect()
        };
        if traces.is_empty() {
            return None;
        }
        Some(RuleWorldModel::from_transitions(
            model_id,
            model_version,
            &traces,
        ))
    }

    /// active 候选的 (model_id, model_version) 标签（预测记录随标签归因）。
    pub fn active_model_tags(&self) -> (String, String) {
        let reg = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        reg.candidates
            .get(&reg.active)
            .map(|c| (c.model_id.clone(), c.model_version.clone()))
            .unwrap_or_else(|| ("wm-rule-v1".into(), "1.0.0".into()))
    }

    pub fn active_candidate_id(&self) -> String {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .clone()
    }

    /// 环境创建/复位时的任务 id（transition 的 task_id / 预测上下文缺省值）。
    pub fn task_id_for(&self, env_id: &str) -> String {
        self.env_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(env_id)
            .map(|t| t.task_id.clone())
            .unwrap_or_else(|| env_id.to_string())
    }

    /// 登记一步影子评估（归属当前 active 候选）。
    pub fn record_evaluation(&self, evaluation: PredictionEvaluation) {
        let candidate_id = self.active_candidate_id();
        self.record_evaluation_for(candidate_id, evaluation);
    }

    /// 登记一步影子评估（显式候选归属：§5.12.4 影子注册表——
    /// 候选模型与当前模型并行预测、对照真实结果；评估只进校准样本，
    /// 不参与、不覆盖动作决策）。
    pub fn record_evaluation_for(&self, candidate_id: String, evaluation: PredictionEvaluation) {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .evaluations
            .push(ModelEvaluation {
                candidate_id,
                evaluation,
            });
    }

    /// 为候选接线真实可执行 provider（影子样本的唯一合法来源入口）。
    ///
    /// 治理约束：
    /// - 候选必须已声明 `CandidateProviderRef::External`；metadata_only 候选不得接线；
    /// - 重复接线允许覆盖（进程内重连/恢复场景），都以最后一次接线为准；
    /// - 接线本身不产生任何评估——样本只会在真实 step 的并行预测对照中自然积累。
    pub fn wire_candidate_executor(
        &self,
        candidate_id: &str,
        kind: impl Into<String>,
        locator: impl Into<String>,
        model: Arc<dyn GuiWorldModel>,
    ) -> Result<(), String> {
        {
            let reg = self.providers.lock().unwrap_or_else(|e| e.into_inner());
            match reg.candidates.get(candidate_id) {
                Some(c) if c.provider.is_external() => {}
                Some(_) => {
                    return Err(format!(
                        "候选 {candidate_id} 未声明外部 provider（metadata_only），拒绝接线"
                    ));
                }
                None => return Err(format!("未知模型候选 {candidate_id}")),
            }
        }
        let mut executors = self.executors.lock().unwrap_or_else(|e| e.into_inner());
        executors.insert(
            candidate_id.to_string(),
            WiredExecutor {
                kind: kind.into(),
                locator: locator.into(),
                model,
            },
        );
        Ok(())
    }

    /// 候选是否在本进程有真实接线的执行器（含内置 wm0-rule 的进程内特例）。
    pub(crate) fn wired_executor_info(&self, candidate_id: &str) -> Option<(String, String)> {
        if candidate_id == "wm0-rule" {
            // 内置 WM0 是真实的进程内执行路径（active 规则模型即时构建），视为已接线。
            return Some((
                "builtin-wm0-rule-corpus".to_string(),
                "in-process:active_rule_model".to_string(),
            ));
        }
        self.executors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(candidate_id)
            .map(|e| (e.kind.clone(), e.locator.clone()))
    }

    /// 收集某候选的全部影子评估（晋升门控用）。
    pub(crate) fn evaluations_of_candidate(&self, candidate_id: &str) -> Vec<PredictionEvaluation> {
        self.providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .evaluations
            .iter()
            .filter(|m| m.candidate_id == candidate_id)
            .map(|m| m.evaluation.clone())
            .collect()
    }
}

/// 进程级运行态（生产：build_router 挂载 `desktop_world_api::router` 时初始化；幂等）。
fn desktop_world_hub(data_root: &std::path::Path) -> Arc<DesktopWorldHub> {
    static HUB: OnceLock<Arc<DesktopWorldHub>> = OnceLock::new();
    HUB.get_or_init(|| {
        DesktopWorldHub::new(data_root)
            .unwrap_or_else(|e| panic!("desktop world hub 初始化失败：{e}"))
    })
    .clone()
}

/// 组装 DesktopWorld 路由（handler 状态 = 独立 [`DesktopWorldHub`]；生产经 [`desktop_world_hub`]）。
pub fn router_with_hub(hub: Arc<DesktopWorldHub>) -> Router {
    Router::new()
        .route("/desktop-envs", post(create_env))
        .route("/desktop-envs/{id}/reset", post(reset_env))
        .route("/desktop-envs/{id}/lease", post(lease_op))
        .route("/desktop-envs/{id}/observe", get(observe_env))
        .route("/desktop-envs/{id}/step", post(step_env))
        .route("/desktop-envs/{id}/snapshot", post(snapshot_env))
        .route("/desktop-envs/{id}/restore", post(restore_env))
        .route("/desktop-envs/{id}/judge", post(judge_env))
        .route("/desktop-envs/{id}/inject-fault", post(inject_fault_env))
        .route("/world-model/predict", post(world_predict))
        .route("/world-model/providers", get(world_providers))
        .route("/transitions/{id}", get(get_transition))
        .route("/datasets/build", post(datasets_build))
        .route("/datasets/{id}/manifest", get(dataset_manifest))
        .route("/model-candidates", post(candidate_register))
        .route("/model-candidates/{id}/promote", post(candidate_promote))
        .with_state(hub)
}

/// 组装 DesktopWorld 路由（build_router merge；data_root 用于运行态持久化目录）。
pub fn router(state: Arc<owo_agent_server::AppState>) -> Router {
    router_with_hub(desktop_world_hub(&state.data_root))
}

// ---------------------------------------------------------------------------
// 请求/响应 DTO
// ---------------------------------------------------------------------------

/// 创建环境请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateEnvBody {
    /// 环境 id（缺省自动生成 `env-<uuid>`）。
    #[serde(default)]
    pub env_id: Option<String>,
    /// 任务种子（决定应用类别与初始状态）。
    pub task: TaskSeed,
    /// 写租约 owner（创建即授初租并返回凭证；缺省 "server"）。
    #[serde(default)]
    pub owner: Option<String>,
}

/// 创建环境响应。
#[derive(Debug, Clone, Serialize)]
pub struct CreateEnvResponse {
    pub env_id: String,
    pub env_version: String,
    /// 初始写租约凭证（写路径必须回带）。
    pub lease: owo_agent_core::desktop_env::EnvLeaseRecord,
    pub task: TaskSeed,
    /// 首帧观测（reset 后初始状态）。
    pub state: WorldStateV1,
}

/// 复位请求（写路径）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetEnvBody {
    pub task: TaskSeed,
    pub lease: LeaseProof,
}

/// 租约操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseOp {
    /// 获取/接管租约（空闲或已过期可授；他人持有中 → 409）。
    Acquire,
    /// 续租（凭证必须匹配当前租约）。
    Renew,
    /// 释放（凭证必须匹配；记录置过期，epoch 不重置）。
    Release,
}

/// 租约操作请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeaseOpBody {
    pub op: LeaseOp,
    /// renew/release 必填。
    #[serde(default)]
    pub lease: Option<LeaseProof>,
    /// acquire 时的 owner（缺省 "server"）。
    #[serde(default)]
    pub owner: Option<String>,
}

/// 单步执行请求（写路径；step 是闭环粘合点：影子预测 + 执行 + transition 落盘）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepEnvBody {
    pub action: GroundedAction,
    /// 写租约凭证（必填；不匹配 → 409 fencing）。
    pub lease: LeaseProof,
    /// 轨迹 episode id（缺省 = env_id：单环境单 episode）。
    #[serde(default)]
    pub episode_id: Option<String>,
    /// 是否写入 transition 语料（缺省 true；false 只执行不落盘，预测/评估照常）。
    #[serde(default = "default_true")]
    pub record: bool,
    /// 预测上下文：任务目标（缺省 = 环境任务 id）。
    #[serde(default)]
    pub task_goal: Option<String>,
    /// 预测上下文：已执行步骤摘要。
    #[serde(default)]
    pub history: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// 单步执行响应（大对象经 ref 引用：state ref / transition id；完整观测经 /observe 拉取）。
#[derive(Debug, Clone, Serialize)]
pub struct StepEnvResponse {
    pub env_id: String,
    pub before_state_ref: String,
    pub after_state_ref: String,
    /// 本步 transition 记录 id（record=false 时为空串）。
    pub transition_id: String,
    pub recorded: bool,
    pub verdict: Verdict,
    pub reward_parts: RewardParts,
    pub duration_ms: u64,
    /// 影子预测快照（无可用模型/预测失败时 null，见 fallback）。
    pub prediction: Option<PredictionRef>,
    /// 预测/实际单步评估（校准样本；无预测时 null）。
    pub prediction_evaluation: Option<PredictionEvaluation>,
    /// 无预测时的回退说明（确定性路径）。
    pub fallback: Option<String>,
}

/// 恢复请求（写路径）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreEnvBody {
    pub snapshot: String,
    pub lease: LeaseProof,
}

/// 判分请求（读路径）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeEnvBody {
    pub success: SuccessSpec,
}

/// 故障注入请求（写路径）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectFaultBody {
    pub fault: owo_agent_core::desktop_env::FaultSpec,
    pub lease: LeaseProof,
}

/// 预测请求（只读：预测不改变环境）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictBody {
    pub env_id: String,
    pub action: GroundedAction,
    /// 缺省由环境推导（app = env_version，task_goal = 环境任务 id）。
    #[serde(default)]
    pub context: Option<WorldModelContext>,
    /// 附带候选比较建议（advice；缺省 false）。
    #[serde(default)]
    pub with_advice: bool,
    /// 候选动作（with_advice 时参与比较；缺省只比较 action 本身）。
    #[serde(default)]
    pub candidates: Vec<GroundedAction>,
}

/// 数据集构建请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildDatasetBody {
    /// 清洗配置（缺省：全部环境版本、不放行真实域、平衡 3.0、id 自动生成）。
    #[serde(default)]
    pub config: Option<DatasetBuilderConfig>,
    /// 过滤：仅该环境的 transition。
    #[serde(default)]
    pub env_id: Option<String>,
    /// 过滤：仅该 episode。
    #[serde(default)]
    pub episode_id: Option<String>,
    /// 过滤：仅该任务。
    #[serde(default)]
    pub task_id: Option<String>,
}

/// provider 健康视图。
#[derive(Debug, Clone, Serialize)]
pub struct ProvidersResponse {
    pub active: ModelCandidate,
    pub candidates: Vec<ModelCandidate>,
    /// 每候选影子评估样本数（校准达标判据）。
    pub samples: BTreeMap<String, u64>,
    /// 全局校准聚合（WM0 完成标准：命中/误差类型/分桶置信）。
    pub calibration: owo_agent_core::world_model::CalibrationReport,
    /// 每个有影子样本的候选的独立校准摘要（与 `samples` 的键一致）。
    pub calibrations: BTreeMap<String, owo_agent_core::world_model::CalibrationReport>,
    pub transitions_total: usize,
}

/// 候选注册请求（新候选恒以 shadow 起步）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterCandidateBody {
    /// 缺省自动生成 `model_id-model_version-<uuid8>`。
    #[serde(default)]
    pub candidate_id: Option<String>,
    pub model_id: String,
    pub model_version: String,
    #[serde(default)]
    pub source: Option<String>,
    /// 可执行 provider 身份声明（缺省 metadata_only）。
    ///
    /// 注意：声明 ≠ 接线。外部 provider 还需本进程真实接线
    /// （`DesktopWorldHub::wire_candidate_executor`）后才会积累影子样本。
    #[serde(default)]
    pub provider_ref: Option<CandidateProviderRef>,
}

/// 晋升请求（显式 ack 门控：手动/战略决策，不得静默晋升）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromoteCandidateBody {
    /// 必须为 true：调用方确认已完成影子期评审。
    pub ack: bool,
    /// 晋升理由（必填；进入候选登记审计字段）。
    pub reason: String,
}

// ---------------------------------------------------------------------------
// 环境面 handler
// ---------------------------------------------------------------------------

/// 环境存在性前置检查（未知环境 404；写路径 fencing 409 由 registry 给出）。
fn require_env(
    hub: &DesktopWorldHub,
    env_id: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if hub.envs.get(env_id).is_err() {
        Err(api_err(StatusCode::NOT_FOUND, format!("未知环境 {env_id}")))
    } else {
        Ok(())
    }
}

async fn create_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Json(body): Json<CreateEnvBody>,
) -> ApiResult<CreateEnvResponse> {
    let env_id = match &body.env_id {
        Some(id) if !id.trim().is_empty() => {
            if hub.envs.get(id).is_ok() {
                return Err(api_err(StatusCode::CONFLICT, format!("环境 {id} 已存在")));
            }
            id.clone()
        }
        _ => format!("env-{}", uuid::Uuid::new_v4()),
    };
    if let Err(e) = hub.envs.create(&env_id, body.task.clone()).await {
        return Err(env_err_response(&e));
    }
    let owner = body
        .owner
        .clone()
        .filter(|o| !o.trim().is_empty())
        .unwrap_or_else(|| "server".into());
    let lease = match hub.envs.acquire_lease(&env_id, &owner) {
        Ok(record) => record,
        Err(e) => return Err(env_err_response(&e)),
    };
    {
        let mut tasks = hub.env_tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.insert(env_id.clone(), body.task.clone());
    }
    let state = match hub.envs.observe(&env_id).await {
        Ok(s) => s,
        Err(e) => return Err(env_err_response(&e)),
    };
    let env_version = state.env_version.clone();
    Ok(Json(CreateEnvResponse {
        env_id,
        env_version,
        lease,
        task: body.task,
        state,
    }))
}

async fn reset_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
    Json(body): Json<ResetEnvBody>,
) -> ApiResult<WorldStateV1> {
    require_env(&hub, &env_id)?;
    let state = match hub
        .envs
        .reset(&env_id, &body.lease, body.task.clone())
        .await
    {
        Ok(s) => s,
        Err(e) => return Err(env_err_response(&e)),
    };
    {
        let mut tasks = hub.env_tasks.lock().unwrap_or_else(|e| e.into_inner());
        tasks.insert(env_id, body.task);
    }
    Ok(Json(state))
}

async fn lease_op(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
    Json(body): Json<LeaseOpBody>,
) -> ApiResult<serde_json::Value> {
    require_env(&hub, &env_id)?;
    match body.op {
        LeaseOp::Acquire => {
            let owner = body
                .owner
                .clone()
                .filter(|o| !o.trim().is_empty())
                .unwrap_or_else(|| "server".into());
            let lease = hub
                .envs
                .acquire_lease(&env_id, &owner)
                .map_err(|e| env_err_response(&e))?;
            Ok(Json(
                serde_json::json!({ "env_id": env_id, "lease": lease }),
            ))
        }
        LeaseOp::Renew => {
            let proof = body.lease.as_ref().ok_or_else(|| {
                api_err(
                    StatusCode::BAD_REQUEST,
                    "renew 需要当前租约凭证 lease: {owner, token, epoch}",
                )
            })?;
            let lease = hub
                .envs
                .renew_lease(&env_id, proof)
                .map_err(|e| env_err_response(&e))?;
            Ok(Json(
                serde_json::json!({ "env_id": env_id, "lease": lease }),
            ))
        }
        LeaseOp::Release => {
            let proof = body.lease.as_ref().ok_or_else(|| {
                api_err(
                    StatusCode::BAD_REQUEST,
                    "release 需要当前租约凭证 lease: {owner, token, epoch}",
                )
            })?;
            hub.envs
                .release_lease(&env_id, proof)
                .map_err(|e| env_err_response(&e))?;
            let active = hub.envs.active_lease(&env_id);
            Ok(Json(serde_json::json!({
                "env_id": env_id,
                "released": true,
                "lease": active,
            })))
        }
    }
}

async fn observe_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
) -> ApiResult<WorldStateV1> {
    require_env(&hub, &env_id)?;
    match hub.envs.observe(&env_id).await {
        Ok(state) => Ok(Json(state)),
        Err(e) => Err(env_err_response(&e)),
    }
}

async fn step_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
    Json(body): Json<StepEnvBody>,
) -> ApiResult<StepEnvResponse> {
    require_env(&hub, &env_id)?;

    // 1) 动作前状态（读路径不要求租约；仅供影子预测与预测上下文）。
    let state = match hub.envs.observe(&env_id).await {
        Ok(s) => s,
        Err(e) => return Err(env_err_response(&e)),
    };
    let context = WorldModelContext {
        app: state.env_version.clone(),
        task_goal: body
            .task_goal
            .clone()
            .unwrap_or_else(|| hub.task_id_for(&env_id)),
        history: body.history.clone(),
    };

    // 2) 影子预测（§4.2：预测不是事实——只记录对照，不参与、不覆盖动作决策）。
    let mut prediction: Option<WorldPrediction> = None;
    let mut fallback: Option<String> = None;
    let mut model_versions: Vec<String> = Vec::new();
    match hub.active_rule_model() {
        Some(model) => match model.predict(&state, &body.action, &context) {
            Ok(p) => {
                model_versions.push(format!("{}@{}", p.model_id, p.model_version));
                prediction = Some(p);
            }
            Err(e) => fallback = Some(e.to_string()),
        },
        None => {
            fallback = Some("无可用世界模型（transition 语料为空），使用确定性路径".to_string())
        }
    }

    // 2b) 影子候选并行预测（§5.12.4 治理修正）：只有在本进程真正接入了可执行
    //     provider 的 shadow 候选才参与并行对照。无 provider 的 metadata_only 候选
    //     保持零样本；以 transition 语料克隆 active 规则模型“代跑”的行为已删除——
    //     那类输出与 active 同源同值，样本是伪影，不能作为独立评估依据。
    let shadow_predictions: Vec<(String, WorldPrediction)> = {
        let shadow_ids: Vec<String> = {
            let reg = hub.providers.lock().unwrap_or_else(|e| e.into_inner());
            reg.candidates
                .values()
                .filter(|c| c.status == CandidateStatus::Shadow && c.provider.is_external())
                .map(|c| c.candidate_id.clone())
                .collect()
        };
        let executors = hub.executors.lock().unwrap_or_else(|e| e.into_inner());
        shadow_ids
            .into_iter()
            .filter_map(|cid| {
                let wired = executors.get(&cid)?;
                Some(cid).zip(wired.model.predict(&state, &body.action, &context).ok())
            })
            .collect()
    };

    // 3) 执行真实动作（写路径；租约 fencing 裁决，stale → 409）。
    let step = match hub
        .envs
        .step(&env_id, &body.lease, body.action.clone())
        .await
    {
        Ok(s) => s,
        Err(e) => return Err(env_err_response(&e)),
    };

    // 4) 预测/实际对照（校准样本；不改变动作与结果）。active 与影子候选同一真实结果。
    let prediction_evaluation = prediction.as_ref().map(|p| evaluate_prediction(p, &step));
    for (candidate_id, sp) in &shadow_predictions {
        hub.record_evaluation_for(candidate_id.clone(), evaluate_prediction(sp, &step));
    }
    let predicted_ref = prediction.as_ref().map(|p| PredictionRef {
        model_id: p.model_id.clone(),
        model_version: p.model_version.clone(),
        success_probability: p.success_probability,
        uncertainty: p.uncertainty,
        predicted_delta_fingerprint: p.predicted_delta.fingerprint(),
    });

    // 5) transition 落盘（幂等语料；record=false 跳过落盘，预测/评估照常）。
    let mut transition_id = String::new();
    let mut recorded = false;
    if body.record {
        let trace = TransitionTraceV1 {
            transition_id: format!("tr-{}", uuid::Uuid::new_v4()),
            episode_id: body.episode_id.clone().unwrap_or_else(|| env_id.clone()),
            task_id: hub.task_id_for(&env_id),
            env_id: env_id.clone(),
            env_version: state.env_version.clone(),
            state_before_ref: step.before_state_ref.clone(),
            action: step.action.clone(),
            predicted: predicted_ref.clone(),
            state_after_ref: step.after_state_ref.clone(),
            observed_delta: step.observed_delta.clone(),
            verifier_results: VerifierResult::from_verdict("env-step", &step.verdict),
            reward_parts: step.reward_parts.clone(),
            outcome: if step.verdict.passed() {
                TransitionOutcome::Success
            } else {
                TransitionOutcome::Failure
            },
            failure_class: if step.verdict.passed() {
                None
            } else {
                // 低置信归因：未做失败归因前只记 UnknownNeedsReview，不伪造确定原因。
                Some(FailureClass::UnknownNeedsReview)
            },
            fork_point: None,
            policy_version: "desktop-world-http".into(),
            model_versions: model_versions.clone(),
            privacy_scope: PrivacyScope::S1Sim,
            created_at: rfc3339_now(),
        };
        let experience_trace = trace.clone();
        let inserted = {
            let mut store = hub.transitions.lock().unwrap_or_else(|e| e.into_inner());
            match store.append(trace) {
                Ok(inserted) => inserted,
                Err(e) => {
                    return Err(api_err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("transition 日志写入失败：{e}"),
                    ));
                }
            }
        };
        if inserted {
            let _ = record_transition_experience(&hub.experience, &experience_trace);
        }
        transition_id = experience_trace.transition_id;
        recorded = true;
    }

    if let Some(eval) = &prediction_evaluation {
        hub.record_evaluation(eval.clone());
    }

    Ok(Json(StepEnvResponse {
        env_id,
        before_state_ref: step.before_state_ref,
        after_state_ref: step.after_state_ref,
        transition_id,
        recorded,
        verdict: step.verdict,
        reward_parts: step.reward_parts,
        duration_ms: step.duration_ms,
        prediction: predicted_ref,
        prediction_evaluation,
        fallback,
    }))
}

async fn snapshot_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
) -> ApiResult<serde_json::Value> {
    require_env(&hub, &env_id)?;
    let snapshot = match hub.envs.snapshot(&env_id).await {
        Ok(s) => s,
        Err(e) => return Err(env_err_response(&e)),
    };
    Ok(Json(
        serde_json::json!({ "env_id": env_id, "snapshot_id": snapshot }),
    ))
}

async fn restore_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
    Json(body): Json<RestoreEnvBody>,
) -> ApiResult<WorldStateV1> {
    require_env(&hub, &env_id)?;
    match hub.envs.restore(&env_id, &body.lease, body.snapshot).await {
        Ok(state) => Ok(Json(state)),
        Err(e) => Err(env_err_response(&e)),
    }
}

async fn judge_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
    Json(body): Json<JudgeEnvBody>,
) -> ApiResult<Verdict> {
    require_env(&hub, &env_id)?;
    match hub.envs.judge(&env_id, body.success).await {
        Ok(verdict) => Ok(Json(verdict)),
        Err(e) => Err(env_err_response(&e)),
    }
}

async fn inject_fault_env(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(env_id): Path<String>,
    Json(body): Json<InjectFaultBody>,
) -> ApiResult<serde_json::Value> {
    require_env(&hub, &env_id)?;
    hub.envs
        .inject_fault(&env_id, &body.lease, body.fault)
        .await
        .map_err(|e| env_err_response(&e))?;
    Ok(Json(
        serde_json::json!({ "env_id": env_id, "injected": true }),
    ))
}

// ---------------------------------------------------------------------------
// 模型面 handler
// ---------------------------------------------------------------------------

async fn world_predict(
    State(hub): State<Arc<DesktopWorldHub>>,
    Json(body): Json<PredictBody>,
) -> ApiResult<serde_json::Value> {
    if hub.envs.get(&body.env_id).is_err() {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知环境 {}", body.env_id),
        ));
    }
    let state = match hub.envs.observe(&body.env_id).await {
        Ok(s) => s,
        Err(e) => return Err(env_err_response(&e)),
    };
    let context = body.context.clone().unwrap_or_else(|| WorldModelContext {
        app: state.env_version.clone(),
        task_goal: hub.task_id_for(&body.env_id),
        history: Vec::new(),
    });

    // active provider（WM0 规则模型即时构建；语料为空 → 不可用 → 确定性回退）。
    let model = hub.active_rule_model();
    let mut fallback: Option<String> = None;
    let prediction = match &model {
        Some(m) => match m.predict(&state, &body.action, &context) {
            Ok(p) => Some(p),
            Err(e) => {
                fallback = Some(e.to_string());
                None
            }
        },
        None => {
            fallback = Some("无可用世界模型（transition 语料为空），使用确定性路径".into());
            None
        }
    };

    let advice = if body.with_advice {
        let candidates = if body.candidates.is_empty() {
            vec![body.action.clone()]
        } else {
            body.candidates.clone()
        };
        let advice = match &model {
            Some(m) => {
                let dyn_model: &dyn GuiWorldModel = m;
                advise_candidates(Some(dyn_model), &state, &candidates, &context)
            }
            None => advise_candidates(None, &state, &candidates, &context),
        };
        Some(advice)
    } else {
        None
    };

    let model_info = model.as_ref().map(|m| {
        serde_json::json!({
            "model_id": m.model_id(),
            "model_version": m.model_version(),
            "rule_count": m.rule_count(),
            "source": "transition 语料即时构建（WM0 规则/频率基线）",
        })
    });

    Ok(Json(serde_json::json!({
        "env_id": body.env_id,
        "available": prediction.is_some(),
        "prediction": prediction,
        "fallback": fallback,
        "advice": advice,
        "model": model_info,
        "note": "预测是模型输出而非事实：仅 observe/judge 的真实结果可更新任务状态",
    })))
}

async fn world_providers(State(hub): State<Arc<DesktopWorldHub>>) -> ApiResult<ProvidersResponse> {
    let (candidates, active, evaluations) = {
        let reg = hub.providers.lock().unwrap_or_else(|e| e.into_inner());
        let mut cands: Vec<ModelCandidate> = reg.candidates.values().cloned().collect();
        cands.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
        let active = reg
            .candidates
            .get(&reg.active)
            .cloned()
            .unwrap_or_else(|| ModelCandidate {
                candidate_id: reg.active.clone(),
                model_id: "unknown".into(),
                model_version: "0.0.0".into(),
                source: "未知 active 候选（登记缺失）".into(),
                status: CandidateStatus::Active,
                created_at: rfc3339_now(),
                promoted_at: None,
                promote_reason: None,
                provider: CandidateProviderRef::default(),
                calibration_summary: None,
            });
        (cands, active, reg.evaluations.clone())
    };
    let mut samples: BTreeMap<String, u64> = BTreeMap::new();
    let mut calibrations: BTreeMap<String, CalibrationReport> = BTreeMap::new();
    // 注意：`calibration` 保持历史口径（全部影子对照混合聚合）；按候选的独立摘要见 `calibrations`。
    let mut evals: Vec<PredictionEvaluation> = Vec::new();
    let mut per_candidate: BTreeMap<String, Vec<PredictionEvaluation>> = BTreeMap::new();
    for me in &evaluations {
        *samples.entry(me.candidate_id.clone()).or_default() += 1;
        evals.push(me.evaluation.clone());
        evals_by_candidate(&mut per_candidate, me);
    }
    for (cid, group) in &per_candidate {
        calibrations.insert(cid.clone(), aggregate_calibration(group));
    }
    let calibration = aggregate_calibration(&evals);
    let transitions_total = hub.transitions.lock().map(|s| s.len()).unwrap_or(0);
    Ok(Json(ProvidersResponse {
        active,
        candidates,
        samples,
        calibration,
        calibrations,
        transitions_total,
    }))
}

/// 按候选 id 分组累积评估（world_providers 辅助）。
fn evals_by_candidate(acc: &mut BTreeMap<String, Vec<PredictionEvaluation>>, me: &ModelEvaluation) {
    acc.entry(me.candidate_id.clone())
        .or_default()
        .push(me.evaluation.clone());
}

// ---------------------------------------------------------------------------
// 数据面 handler
// ---------------------------------------------------------------------------

async fn get_transition(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(id): Path<String>,
) -> ApiResult<TransitionTraceV1> {
    let store = hub.transitions.lock().unwrap_or_else(|e| e.into_inner());
    match store.get(&id).cloned() {
        Some(trace) => Ok(Json(trace)),
        None => Err(api_err(
            StatusCode::NOT_FOUND,
            format!("transition 记录 {id} 不存在"),
        )),
    }
}

async fn datasets_build(
    State(hub): State<Arc<DesktopWorldHub>>,
    Json(body): Json<BuildDatasetBody>,
) -> ApiResult<serde_json::Value> {
    let (traces, total) = {
        let store = hub.transitions.lock().unwrap_or_else(|e| e.into_inner());
        let mut t: Vec<TransitionTraceV1> = store.traces().into_iter().cloned().collect();
        let total = t.len();
        if let Some(env_id) = &body.env_id {
            t.retain(|t| &t.env_id == env_id);
        }
        if let Some(episode_id) = &body.episode_id {
            t.retain(|t| &t.episode_id == episode_id);
        }
        if let Some(task_id) = &body.task_id {
            t.retain(|t| &t.task_id == task_id);
        }
        (t, total)
    };
    if traces.is_empty() {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            if total == 0 {
                "无 transition 样本（语料为空）：先经 /desktop-envs/{id}/step 执行步骤再构建"
                    .to_string()
            } else {
                format!("过滤条件未命中任何 transition（语料共 {total} 条）")
            },
        ));
    }
    let config = body.config.clone().unwrap_or_default();
    let result = build_dataset(&traces, &config);
    let dataset_id = result.manifest.dataset_id.clone();
    // 数据集目录按「只收 <id>.json 清单」的纪律管理（启动恢复同样强制）：
    // 自定义 dataset_id 必须通过合法性校验，否则拒绝构建，防路径逃逸。
    if !valid_dataset_id(&dataset_id) {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            format!("非法 dataset_id：{dataset_id}（只允许字母数字与 ._- 组合，且以字母数字开头）"),
        ));
    }
    let path = dataset_manifest_path(&hub.dir, &dataset_id);
    if let Err(e) = save_manifest(&path, &result.manifest) {
        return Err(api_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("数据集清单落盘失败：{e}"),
        ));
    }
    {
        let mut map = hub.datasets.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(dataset_id.clone(), result.manifest.clone());
    }
    Ok(Json(serde_json::json!({
        "dataset_id": dataset_id,
        "input_count": result.manifest.input_count,
        "accepted_count": result.manifest.accepted_count,
        "success_count": result.manifest.success_count,
        "failure_count": result.manifest.failure_count,
        "rejection_counts": result.manifest.rejection_counts,
        "content_hash": result.manifest.content_hash,
        "manifest_path": path.to_string_lossy().into_owned(),
        "manifest": result.manifest,
    })))
}

async fn dataset_manifest(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(id): Path<String>,
) -> ApiResult<DatasetManifest> {
    // 以持久化数据为权威：优先读磁盘并做完整性校验；文件损坏时不回退到内存缓存，
    // 直接 500 暴露具体路径，避免用可能过期的进程内副本掩盖数据问题。
    match load_dataset_manifest_from_disk(&hub.dir, &id) {
        Ok(Some(manifest)) => Ok(Json(manifest)),
        Ok(None) => Err(api_err(
            StatusCode::NOT_FOUND,
            format!("数据集 {id} 不存在"),
        )),
        Err(e) => Err(api_err(StatusCode::INTERNAL_SERVER_ERROR, e)),
    }
}

// ---------------------------------------------------------------------------
// 模型治理 handler
// ---------------------------------------------------------------------------

/// 晋升门槛：最低影子样本数。低于该数证据不足，一律保持 shadow。
const MIN_SHADOW_SAMPLES: u64 = 16;
/// 关键指标相对上一任 active 的最大允许退化幅度（绝对差；同文档 §5.12.4 保守阈值）。
const MAX_REGRESSION_DELTA: f64 = 0.05;

async fn candidate_register(
    State(hub): State<Arc<DesktopWorldHub>>,
    Json(body): Json<RegisterCandidateBody>,
) -> ApiResult<ModelCandidate> {
    let candidate_id = body
        .candidate_id
        .clone()
        .filter(|c| !c.trim().is_empty())
        .unwrap_or_else(|| {
            format!(
                "{}-{}-{}",
                body.model_id,
                body.model_version,
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            )
        });
    let mut reg = hub.providers.lock().unwrap_or_else(|e| e.into_inner());
    if reg.candidates.contains_key(&candidate_id) {
        return Err(api_err(
            StatusCode::CONFLICT,
            format!("模型候选 {candidate_id} 已存在"),
        ));
    }
    // 新候选恒以 shadow 起步（§5.12.4：达标并显式晋升前不得服务在线预测）。
    let cand = ModelCandidate {
        candidate_id: candidate_id.clone(),
        model_id: body.model_id,
        model_version: body.model_version,
        source: body
            .source
            .clone()
            .unwrap_or_else(|| "手动注册（shadow 起步）".into()),
        status: CandidateStatus::Shadow,
        created_at: rfc3339_now(),
        promoted_at: None,
        promote_reason: None,
        // provider 身份按声明登记；缺省 metadata_only（零样本、不可晋升）。
        // 声明 ≠ 接线：影子样本只来自 wire_candidate_executor 挂上的真实预测器。
        provider: body.provider_ref.clone().unwrap_or_default(),
        calibration_summary: None,
    };
    reg.candidates.insert(candidate_id.clone(), cand.clone());
    Ok(Json(cand))
}

async fn candidate_promote(
    State(hub): State<Arc<DesktopWorldHub>>,
    Path(id): Path<String>,
    Json(body): Json<PromoteCandidateBody>,
) -> ApiResult<serde_json::Value> {
    // 门控 0：显式确认 + 理由（手动/战略决策，不得静默晋升）。
    if !body.ack {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "需要显式确认（ack: true）：晋升是手动/战略决策，不得静默晋升",
        ));
    }
    if body.reason.trim().is_empty() {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            "晋升理由（reason）不能为空",
        ));
    }
    // 存在性先行（未知候选必须 404；与后面的治理判据分属不同语义层）。
    let reg = hub.providers.lock().unwrap_or_else(|e| e.into_inner());
    if !reg.candidates.contains_key(&id) {
        return Err(api_err(StatusCode::NOT_FOUND, format!("未知模型候选 {id}")));
    }
    let active_now = reg.active.clone();
    // 门控 1：provider 治理——候选必须声明外部 provider 且在本进程真实接线。
    // 当前尚无 WM1 真实调用面：未接线/伪声明的候选一律明确拒绝，不伪造已验证状态。
    let declared_external = reg
        .candidates
        .get(&id)
        .map(|c| c.provider.is_external())
        .unwrap_or(false);
    drop(reg);
    if !declared_external {
        return Err(api_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "候选 {id} 是 metadata_only（无可执行 provider），必须保持 shadow；\
                 不允许复制 active 规则模型或伪造样本来制造“独立评估”"
            ),
        ));
    }
    let Some((exec_kind, exec_locator)) = hub.wired_executor_info(&id) else {
        return Err(api_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "候选 {id} 声明了外部 provider 但未在本进程真实接线或不可达；\
                 当前尚无 WM1 真实调用面，明确拒绝晋升"
            ),
        ));
    };
    // 门控 2/3/4 的样本收集在拿可变锁之前完成（helper 自持锁，避免嵌套获取）。
    let candidate_evals = hub.evaluations_of_candidate(&id);
    let samples = candidate_evals.len() as u64;
    // 门控 2：最低样本数——“样本数大于 0 即可晋升”的旧判据已废弃。
    if samples < MIN_SHADOW_SAMPLES {
        return Err(api_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "晋升门槛不足：候选 {id} 影子样本 {samples}/{MIN_SHADOW_SAMPLES}\
                 （至少 {MIN_SHADOW_SAMPLES} 个真实影子样本才具备统计意义）"
            ),
        ));
    }
    // 门控 3：校准摘要必须可计算且与样本一致。
    let calibration_report = aggregate_calibration(&candidate_evals);
    if calibration_report.samples != samples {
        return Err(api_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "校准聚合与样本数不一致（{}/{}）",
                calibration_report.samples, samples
            ),
        ));
    }
    // 门控 4：相对上一任 active 的关键指标无超阈值退化。
    let previous_active = if active_now != id {
        Some(active_now.clone())
    } else {
        None
    };
    let mut regression_check: Option<serde_json::Value> = None;
    if let Some(prev_id) = &previous_active {
        let prev_evals = hub.evaluations_of_candidate(prev_id);
        if !prev_evals.is_empty() {
            let prev_report = aggregate_calibration(&prev_evals);
            let hit_delta = prev_report.success_hit_rate - calibration_report.success_hit_rate;
            let jaccard_delta =
                prev_report.mean_delta_jaccard - calibration_report.mean_delta_jaccard;
            let calib_error_delta =
                calibration_report.mean_calibration_error - prev_report.mean_calibration_error;
            if hit_delta > MAX_REGRESSION_DELTA
                || jaccard_delta > MAX_REGRESSION_DELTA
                || calib_error_delta > MAX_REGRESSION_DELTA
            {
                return Err(api_err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "拒绝晋升：候选 {id} 相对上一任 active（{prev_id}）退化超阈值 \
                         （命中差 {hit_delta:.3} / Jaccard 差 {jaccard_delta:.3} / 校准误差增量 \
                         {calib_error_delta:.3}，容许 ±{MAX_REGRESSION_DELTA}）"
                    ),
                ));
            }
            regression_check = Some(serde_json::json!({
                "previous_active": prev_id,
                "previous_samples": prev_report.samples,
                "hit_rate_delta": -hit_delta,
                "mean_delta_jaccard_delta": -jaccard_delta,
                "mean_calibration_error_delta": calib_error_delta,
                "max_regression_delta": MAX_REGRESSION_DELTA,
                "passed": true,
            }));
        }
    }
    // 提交晋升：唯一需要可变锁的段（重验存在性，防中途并发注销）。
    let mut reg = hub.providers.lock().unwrap_or_else(|e| e.into_inner());
    if !reg.candidates.contains_key(&id) {
        return Err(api_err(StatusCode::NOT_FOUND, format!("未知模型候选 {id}")));
    }
    if let Some(prev_id) = &previous_active {
        if let Some(prev) = reg.candidates.get_mut(prev_id) {
            if prev.status == CandidateStatus::Active {
                // 旧 active 降为 shadow（可再次晋升），不直接弃用。
                prev.status = CandidateStatus::Shadow;
            }
        }
    }
    let cand = reg.candidates.get_mut(&id).expect("checked above");
    cand.status = CandidateStatus::Active;
    cand.promoted_at = Some(rfc3339_now());
    cand.promote_reason = Some(body.reason.clone());
    // 晋升快照：固化当时的接线身份与校准摘要（审计口径）。
    cand.calibration_summary = Some(calibration_report.clone());
    reg.active = id.clone();
    let promoted = reg.candidates.get(&id).cloned().expect("checked above");
    Ok(Json(serde_json::json!({
        "candidate": promoted,
        "active": id,
        "previous_active": previous_active,
        "samples": samples,
        "gates": {
            "min_shadow_samples": MIN_SHADOW_SAMPLES,
            "provider_wired": { "kind": exec_kind, "locator": exec_locator },
            "calibration_summary": calibration_report,
            "regression_check": regression_check,
        },
    })))
}
