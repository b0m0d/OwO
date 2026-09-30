use owo_agent_protocol::TeamTemplateRole;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::error::{WorkSwarmError, WorkSwarmResult};
/// 角色规格（组队输入；模板角色的运行时展开）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoleSpec {
    pub role: String,
    /// 承担者种类：`agent` | `human` | `worker`。
    pub assignee: String,
    /// 内层 worker 名（agent 角色缺省 `agent` 模型驱动；human 角色 = user_id）。
    pub worker: Option<String>,
    /// 上游角色（任务 DAG 边）。
    pub depends_on: Vec<String>,
    pub handoff_contract: Option<String>,
    /// 验证断言（字符串形式，见 [`parse_verify`]）。
    pub verify: Option<String>,
    /// 附加步骤输入（透传给内层 worker：echo 的 text / agent 的 prompt 覆盖等）。
    pub extra_input: Value,
}

impl RoleSpec {
    pub fn agent(role: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            assignee: "agent".to_string(),
            worker: None,
            depends_on: Vec::new(),
            handoff_contract: None,
            verify: None,
            extra_input: Value::Null,
        }
    }
}

impl From<TeamTemplateRole> for RoleSpec {
    fn from(r: TeamTemplateRole) -> Self {
        Self {
            role: r.role,
            assignee: r.assignee,
            worker: r.worker,
            depends_on: r.depends_on,
            handoff_contract: r.handoff_contract,
            verify: r.verify,
            extra_input: Value::Null,
        }
    }
}

/// 运行元数据（sidecar 文件 `<run_dir>/<team_id>-meta.json`）：
/// correlation_id + 角色规格（worker 构建的唯一来源；replace 在此生效）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMeta {
    pub team_id: String,
    pub correlation_id: String,
    pub roles: Vec<RoleSpec>,
    /// 八期一路：模板 id（内置模板 → 角色专属 Prompt 段；动态组队为 None）。
    #[serde(default)]
    pub template_id: Option<String>,
    /// 八期一路：角色 → 调用预算（模板 `budget_calls_per_role`；Prompt 预算段与
    /// 运行期跳过的 `saved_budget_calls` 口径来源）。旧 sidecar 缺省为空。
    #[serde(default)]
    pub budgets: BTreeMap<String, usize>,
}

impl RunMeta {
    pub(super) fn file_path(run_dir: &Path, team_id: &str) -> PathBuf {
        run_dir.join(format!("{team_id}-meta.json"))
    }

    pub(super) fn save(&self, run_dir: &Path) -> WorkSwarmResult<()> {
        std::fs::create_dir_all(run_dir).map_err(|e| WorkSwarmError::Io(e.to_string()))?;
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::file_path(run_dir, &self.team_id), json)
            .map_err(|e| WorkSwarmError::Io(e.to_string()))
    }

    pub(super) fn load(run_dir: &Path, team_id: &str) -> WorkSwarmResult<Self> {
        let path = Self::file_path(run_dir, team_id);
        let raw = std::fs::read_to_string(&path)
            .map_err(|_| WorkSwarmError::NotFound(format!("运行元数据 {team_id} 不存在")))?;
        serde_json::from_str(&raw).map_err(WorkSwarmError::from)
    }
}

/// 默认接力样例角色（§9.0：planner → builder → critic → leader）。
pub fn default_relay_roles() -> Vec<RoleSpec> {
    let mut planner = RoleSpec::agent("planner");
    planner.handoff_contract = Some(
        "把目标拆解为可执行的建设方案：明确产物要求、验收要点与风险，输出方案大纲（非代码）。"
            .to_string(),
    );
    planner.verify = Some("non_empty".to_string());
    let mut builder = RoleSpec::agent("builder");
    builder.depends_on = vec!["planner".to_string()];
    builder.handoff_contract = Some(
        "依据上游方案（planner 产物）产出主交付物草稿；输出产物正文本身，不要解释过程。"
            .to_string(),
    );
    builder.verify = Some("non_empty".to_string());
    let mut critic = RoleSpec::agent("critic");
    critic.depends_on = vec!["builder".to_string()];
    critic.handoff_contract = Some(
        "只读评审上游草稿（不修改原文）：检查完整性、一致性与风险，输出 JSON {\"approved\":bool,\"score\":0-100,\"comments\":[..]}。"
            .to_string(),
    );
    critic.verify = Some("non_empty".to_string());
    let mut leader = RoleSpec::agent("leader");
    leader.depends_on = vec!["critic".to_string()];
    leader.handoff_contract =
        Some("最终裁决：综合上游草稿与评审意见采纳或修正，输出最终交付物与交付清单。".to_string());
    leader.verify = Some("non_empty".to_string());
    vec![planner, builder, critic, leader]
}

// ---------------------------------------------------------------------------
// 取消令牌（跨阶段取消传播；A3：fan-out/运行支持父任务取消）
// ---------------------------------------------------------------------------

/// 团队运行取消令牌（watch 语义；运行任务每阶段/每次人节点等待都监听）。
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: tokio::sync::watch::Sender<bool>,
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::watch::channel(false);
        Self { tx }
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    pub fn rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.tx.subscribe()
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// 等待取消（值变 true 返回；发送端关闭返回 false）。
pub async fn wait_cancel(token: &CancelToken) -> bool {
    let mut rx = token.rx();
    if *rx.borrow() {
        return true;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return true;
        }
    }
    false
}
