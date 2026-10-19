use axum::http::StatusCode;
use axum::Json;

use owo_agent_core::experience_store::ExperienceStore;
use owo_agent_core::transition::{TransitionStore, TransitionTraceV1};
use owo_agent_core::world_model::{GuiWorldModel, PredictionEvaluation, RuleWorldModel};
use owo_agent_env::desktop_env::{EnvError, EnvRegistry, TaskSeed};
use owo_agent_eval_facade::dataset_builder::DatasetManifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
pub(super) type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

pub(super) fn api_err(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

/// 环境层错误的 HTTP 映射：fencing 冲突 409；未知快照 404；协议/输入类 400。
pub(super) fn env_err_response(e: &EnvError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        EnvError::StaleLease(_) => api_err(StatusCode::CONFLICT, e.to_string()),
        EnvError::UnknownSnapshot(_) => api_err(StatusCode::NOT_FOUND, e.to_string()),
        EnvError::NotReset(_)
        | EnvError::Unsupported(_)
        | EnvError::InvalidAction(_)
        | EnvError::App(_) => api_err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

pub(super) fn rfc3339_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// 数据集清单持久化（第二路：启动恢复 + 磁盘权威读取）
// ---------------------------------------------------------------------------

/// 数据集 id 合法性（防路径穿越；兼容 builder 生成的 `dataset-<uuid>` 与自定义 id）。
pub(super) fn valid_dataset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// 清单落盘路径。
pub(super) fn dataset_manifest_path(dir: &std::path::Path, id: &str) -> PathBuf {
    dir.join("datasets").join(format!("{id}.json"))
}

/// 重算 sample_ids 的内容哈希（与 core `build_dataset` 完全一致的口径）。
pub(super) fn manifest_content_hash(sample_ids: &[String]) -> String {
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
pub(super) fn validate_recovered_manifest(
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
pub(super) fn load_dataset_manifest_from_disk(
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
pub(super) fn recover_datasets(
    dir: &std::path::Path,
) -> Result<HashMap<String, DatasetManifest>, String> {
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
pub(super) struct WiredExecutor {
    /// 接线时的 provider 类型标识。
    pub(super) kind: String,
    /// 接线时的定位串。
    pub(super) locator: String,
    /// 真实预测器。
    pub(super) model: Arc<dyn GuiWorldModel>,
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
    pub(super) executors: Mutex<HashMap<String, WiredExecutor>>,
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
pub(super) fn desktop_world_hub(data_root: &std::path::Path) -> Arc<DesktopWorldHub> {
    static HUB: OnceLock<Arc<DesktopWorldHub>> = OnceLock::new();
    HUB.get_or_init(|| {
        DesktopWorldHub::new(data_root)
            .unwrap_or_else(|e| panic!("desktop world hub 初始化失败：{e}"))
    })
    .clone()
}
