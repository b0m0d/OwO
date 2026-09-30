use super::*;
impl TeamCoordinator {
    pub(crate) async fn load_bundle(
        &self,
        team_id: &str,
    ) -> WorkSwarmResult<(TeamRun, ProjectSpace, GoalRunState)> {
        let team = self
            .store
            .get_team_run(team_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })?;
        let project_space_id = team
            .project_space_id
            .clone()
            .ok_or_else(|| WorkSwarmError::Run(format!("团队 {team_id} 缺少 project_space_id")))?;
        let space = self
            .store
            .get_project_space(&project_space_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("项目空间 {project_space_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })?;
        let state = self.load_goal_state(team_id)?;
        Ok((team, space, state))
    }

    /// 读取运行状态（带损坏分类）：
    /// - 文件缺失 → NotFound；
    /// - 解析失败 → CorruptState（原文件保留；明确失败而非静默重建）；
    /// - 其余 IO 错误 → Io。
    pub(crate) fn load_goal_state(&self, team_id: &str) -> WorkSwarmResult<GoalRunState> {
        let path = self.run_dir.join(format!("{team_id}.json"));
        if !path.exists() {
            return Err(WorkSwarmError::NotFound(format!(
                "运行状态 {team_id} 不存在（{}）",
                path.display()
            )));
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| WorkSwarmError::Io(format!("读取 {} 失败：{e}", path.display())))?;
        serde_json::from_str(&raw).map_err(|e| {
            WorkSwarmError::CorruptState(format!(
                "运行状态文件损坏（原文件已保留，未被覆盖）：{}：{e}",
                path.display()
            ))
        })
    }

    pub(crate) fn load_state(&self, team_id: &str) -> WorkSwarmResult<GoalRunState> {
        self.load_goal_state(team_id)
    }

    /// 读取运行状态（HTTP 任务视图 / 诊断用）。
    pub fn load_run_state(&self, team_id: &str) -> WorkSwarmResult<GoalRunState> {
        self.load_state(team_id)
    }

    /// 读取运行元数据（HTTP 层构建 worker 注册表用）。
    pub fn load_run_meta(&self, team_id: &str) -> WorkSwarmResult<RunMeta> {
        RunMeta::load(&self.run_dir, team_id)
    }

    /// 列出全部团队运行（HTTP 列表视图）。
    pub async fn list_team_runs(&self) -> WorkSwarmResult<Vec<TeamRun>> {
        self.store
            .list_team_runs()
            .await
            .map_err(WorkSwarmError::Store)
    }

    /// 读取 TeamRun（不存在 → NotFound）。
    pub async fn get_team_run(&self, team_id: &str) -> WorkSwarmResult<TeamRun> {
        self.store.get_team_run(team_id).await.map_err(|e| match e {
            ProjectSpaceStoreError::NotFound(_) => {
                WorkSwarmError::NotFound(format!("团队 {team_id} 不存在"))
            }
            other => WorkSwarmError::Store(other),
        })
    }

    /// 读取 Project Space（不存在 → NotFound）。
    pub async fn get_project_space(
        &self,
        project_id: &str,
    ) -> WorkSwarmResult<owo_agent_protocol::ProjectSpace> {
        self.store
            .get_project_space(project_id)
            .await
            .map_err(|e| match e {
                ProjectSpaceStoreError::NotFound(_) => {
                    WorkSwarmError::NotFound(format!("项目空间 {project_id} 不存在"))
                }
                other => WorkSwarmError::Store(other),
            })
    }

    /// 列出项目空间的版本化产物（按创建时间）。
    pub async fn list_artifacts(
        &self,
        space: &owo_agent_protocol::ProjectSpace,
    ) -> WorkSwarmResult<Vec<Artifact>> {
        self.store
            .list_artifacts_by_project(&space.project_id)
            .await
            .map_err(WorkSwarmError::Store)
    }

    /// 按团队列出全部结构化 Handoff（评测适配器/诊断用）。
    pub async fn list_handoffs(&self, team_id: &str) -> WorkSwarmResult<Vec<HandoffRecord>> {
        let (_team, space, _state) = self.load_bundle(team_id).await?;
        self.store
            .list_handoffs_by_project(&space.project_id)
            .await
            .map_err(WorkSwarmError::Store)
    }

    /// `cas://sha256:{hash}` → 文本内容（CAS 命中时返回 Some）。
    ///
    /// 评测适配器据此把**版本化 Artifact 的内容**复制进评测沙盒，
    /// 保证最终结果来自 ProjectSpace/CAS 而非某次模型回复的内存值。
    pub fn resolve_content_text(&self, content_ref: &str) -> Option<String> {
        let hash = content_ref.strip_prefix("cas://sha256:")?;
        self.cas.get_text(hash)
    }

    /// 审计日志（S0 可见性：team.* 关键动作尾迹）。
    pub fn audit_log(&self) -> Option<Arc<Mutex<crate::audit::AuditLog>>> {
        self.audit.clone()
    }

    pub(crate) fn persist_state(&self, state: &GoalRunState) -> WorkSwarmResult<()> {
        // 损坏保护：目标状态文件已存在且无法解析时拒绝写入——
        // 任何路径都不得把损坏文件静默覆盖成"新状态"（R2：明确失败并保留原文件）。
        let path = self.run_dir.join(format!("{}.json", state.run_id));
        if path.exists() {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if serde_json::from_str::<GoalRunState>(&raw).is_err() {
                    return Err(WorkSwarmError::CorruptState(format!(
                        "拒绝覆盖损坏的运行状态文件（请人工修复或移除后再试）：{}",
                        path.display()
                    )));
                }
            }
        }
        state
            .persist(&self.run_dir)
            .map(|_| ())
            .map_err(WorkSwarmError::Run)
    }

    pub(crate) fn step_deps_satisfied(state: &GoalRunState, step: &StepSpec) -> bool {
        step.depends_on.iter().all(|d| {
            state
                .records
                .get(d)
                .map(|r| r.status == StepStatus::Succeeded)
                .unwrap_or(false)
        })
    }

    pub(crate) fn ready_steps(state: &GoalRunState) -> Vec<StepSpec> {
        state
            .plan
            .steps
            .iter()
            .filter(|s| {
                let rec = &state.records[&s.id];
                rec.status.can_resume() && Self::step_deps_satisfied(state, s)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn all_succeeded(state: &GoalRunState) -> bool {
        state
            .plan
            .steps
            .iter()
            .all(|s| state.records[&s.id].status == StepStatus::Succeeded)
    }

    pub(crate) fn role_spec_of_member<'a>(
        meta: &'a RunMeta,
        member_id: &'a str,
    ) -> WorkSwarmResult<&'a RoleSpec> {
        meta.roles
            .iter()
            .find(|r| format!("m-{}", r.role) == member_id)
            .ok_or_else(|| {
                WorkSwarmError::Run(format!("成员 {member_id} 无对应角色规格（元数据不一致）"))
            })
    }

    // -- 组队（§6.1：模板优先 + 动态组队 ≤5 Agent） --
}
