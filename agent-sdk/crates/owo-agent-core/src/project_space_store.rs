//! ProjectSpaceStore：WorkSwarm 项目空间持久化（§6.6）。
//!
//! 提供 [`ProjectSpaceStore`] trait 与 [`SqliteProjectSpaceStore`] 实现，
//! 管理 [`ProjectSpace`]、[`Artifact`]、[`DecisionRecord`]、[`HandoffRecord`]
//! 和 [`TeamRun`] 的 CRUD 操作。
//!
//! 设计原则：
//! - 所有实体以 JSON 序列化存储，保留完整结构；
//! - 索引列（project_id, team_id, artifact_id 等）用于快速查询；
//! - 不自动级联删除：移除 ProjectSpace 时只删自身记录，关联实体由调用方显式清理；
//! - 线程安全：内部使用 `Mutex<Connection>`，与既有 `SqliteSessionStore` 一致。

use async_trait::async_trait;
use owo_agent_protocol::{Artifact, DecisionRecord, HandoffRecord, ProjectSpace, TeamRun};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum ProjectSpaceStoreError {
    #[error("SQLite 错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("序列化错误：{0}")]
    Serialization(String),
    #[error("未找到：{0}")]
    NotFound(String),
}

impl From<serde_json::Error> for ProjectSpaceStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

type Result<T> = std::result::Result<T, ProjectSpaceStoreError>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// 项目空间存储抽象。
///
/// 当前同步实现（rusqlite 本身是同步的），使用 async_trait 为未来
/// 异步后端（如远程存储）预留接口。
#[async_trait]
pub trait ProjectSpaceStoreBackend: Send + Sync {
    // -- ProjectSpace --
    async fn save_project_space(&self, space: &ProjectSpace) -> Result<()>;
    async fn get_project_space(&self, project_id: &str) -> Result<ProjectSpace>;
    async fn list_project_spaces(&self) -> Result<Vec<ProjectSpace>>;
    async fn delete_project_space(&self, project_id: &str) -> Result<()>;

    // -- Artifact --
    /// 保存产物；`project_id` 落真实关联列（`list_artifacts_by_project` 按索引过滤）。
    async fn save_artifact(&self, artifact: &Artifact, project_id: &str) -> Result<()>;
    async fn get_artifact(&self, artifact_id: &str) -> Result<Artifact>;
    async fn list_artifacts_by_project(&self, project_id: &str) -> Result<Vec<Artifact>>;
    async fn delete_artifact(&self, artifact_id: &str) -> Result<()>;

    // -- DecisionRecord --
    async fn save_decision(&self, decision: &DecisionRecord, project_id: &str) -> Result<()>;
    async fn get_decision(&self, decision_id: &str) -> Result<DecisionRecord>;
    async fn list_decisions_by_project(&self, project_id: &str) -> Result<Vec<DecisionRecord>>;

    // -- HandoffRecord --
    async fn save_handoff(&self, handoff: &HandoffRecord, project_id: &str) -> Result<()>;
    async fn get_handoff(&self, handoff_id: &str) -> Result<HandoffRecord>;
    async fn list_handoffs_by_project(&self, project_id: &str) -> Result<Vec<HandoffRecord>>;

    // -- TeamRun --
    async fn save_team_run(&self, team_run: &TeamRun) -> Result<()>;
    async fn get_team_run(&self, team_id: &str) -> Result<TeamRun>;
    async fn list_team_runs(&self) -> Result<Vec<TeamRun>>;
    async fn delete_team_run(&self, team_id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// SQLite 实现
// ---------------------------------------------------------------------------

/// WorkSwarm 表结构（幂等 CREATE TABLE IF NOT EXISTS）。
fn worksarm_schema() -> &'static str {
    "CREATE TABLE IF NOT EXISTS project_spaces (
         project_id TEXT PRIMARY KEY,
         data_json TEXT NOT NULL,
         created_at TEXT NOT NULL,
         updated_at TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS artifacts (
         artifact_id TEXT PRIMARY KEY,
         project_id TEXT NOT NULL,
         data_json TEXT NOT NULL,
         created_at TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_artifacts_project ON artifacts(project_id);
     CREATE TABLE IF NOT EXISTS decisions (
         decision_id TEXT PRIMARY KEY,
         project_id TEXT NOT NULL,
         data_json TEXT NOT NULL,
         created_at TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_decisions_project ON decisions(project_id);
     CREATE TABLE IF NOT EXISTS handoffs (
         handoff_id TEXT PRIMARY KEY,
         project_id TEXT NOT NULL,
         data_json TEXT NOT NULL,
         created_at TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_handoffs_project ON handoffs(project_id);
     CREATE TABLE IF NOT EXISTS team_runs (
         team_id TEXT PRIMARY KEY,
         data_json TEXT NOT NULL,
         created_at TEXT NOT NULL,
         updated_at TEXT NOT NULL
     );"
}

/// SQLite 后端的项目空间存储。
pub struct SqliteProjectSpaceStore {
    conn: Mutex<Connection>,
}

impl SqliteProjectSpaceStore {
    /// 打开或创建存储（自动建表）。
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(worksarm_schema())?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 使用已有连接（测试注入）。
    pub fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(worksarm_schema())?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn serialize<T: serde::Serialize>(value: &T) -> Result<String> {
        serde_json::to_string(value).map_err(Into::into)
    }

    fn deserialize<T: serde::de::DeserializeOwned>(json: &str) -> Result<T> {
        serde_json::from_str(json).map_err(Into::into)
    }
}

#[async_trait]
impl ProjectSpaceStoreBackend for SqliteProjectSpaceStore {
    // -- ProjectSpace --

    async fn save_project_space(&self, space: &ProjectSpace) -> Result<()> {
        let json = Self::serialize(space)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO project_spaces (project_id, data_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(project_id) DO UPDATE SET data_json = excluded.data_json, updated_at = excluded.updated_at",
            params![space.project_id, json, space.created_at, space.updated_at],
        )?;
        Ok(())
    }

    async fn get_project_space(&self, project_id: &str) -> Result<ProjectSpace> {
        let conn = self.conn.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT data_json FROM project_spaces WHERE project_id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ProjectSpaceStoreError::NotFound(format!("ProjectSpace {project_id}"))
                }
                other => ProjectSpaceStoreError::Sqlite(other),
            })?;
        Self::deserialize(&json)
    }

    async fn list_project_spaces(&self) -> Result<Vec<ProjectSpace>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data_json FROM project_spaces ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            let json = row?;
            result.push(Self::deserialize(&json)?);
        }
        Ok(result)
    }

    async fn delete_project_space(&self, project_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM project_spaces WHERE project_id = ?1",
            params![project_id],
        )?;
        Ok(())
    }

    // -- Artifact --

    async fn save_artifact(&self, artifact: &Artifact, project_id: &str) -> Result<()> {
        let json = Self::serialize(artifact)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO artifacts (artifact_id, project_id, data_json, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(artifact_id) DO UPDATE SET data_json = excluded.data_json",
            params![artifact.artifact_id, project_id, json, artifact.created_at],
        )?;
        Ok(())
    }

    async fn get_artifact(&self, artifact_id: &str) -> Result<Artifact> {
        let conn = self.conn.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT data_json FROM artifacts WHERE artifact_id = ?1",
                params![artifact_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ProjectSpaceStoreError::NotFound(format!("Artifact {artifact_id}"))
                }
                other => ProjectSpaceStoreError::Sqlite(other),
            })?;
        Self::deserialize(&json)
    }

    async fn list_artifacts_by_project(&self, project_id: &str) -> Result<Vec<Artifact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT data_json FROM artifacts
             WHERE project_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![project_id], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            let json = row?;
            result.push(Self::deserialize(&json)?);
        }
        Ok(result)
    }

    async fn delete_artifact(&self, artifact_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM artifacts WHERE artifact_id = ?1",
            params![artifact_id],
        )?;
        Ok(())
    }

    // -- DecisionRecord --

    async fn save_decision(&self, decision: &DecisionRecord, project_id: &str) -> Result<()> {
        let json = Self::serialize(decision)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO decisions (decision_id, project_id, data_json, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(decision_id) DO UPDATE SET data_json = excluded.data_json",
            params![decision.decision_id, project_id, json, decision.created_at],
        )?;
        Ok(())
    }

    async fn get_decision(&self, decision_id: &str) -> Result<DecisionRecord> {
        let conn = self.conn.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT data_json FROM decisions WHERE decision_id = ?1",
                params![decision_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ProjectSpaceStoreError::NotFound(format!("DecisionRecord {decision_id}"))
                }
                other => ProjectSpaceStoreError::Sqlite(other),
            })?;
        Self::deserialize(&json)
    }

    async fn list_decisions_by_project(&self, project_id: &str) -> Result<Vec<DecisionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT data_json FROM decisions
             WHERE project_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![project_id], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            let json = row?;
            result.push(Self::deserialize(&json)?);
        }
        Ok(result)
    }

    // -- HandoffRecord --

    async fn save_handoff(&self, handoff: &HandoffRecord, project_id: &str) -> Result<()> {
        let json = Self::serialize(handoff)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO handoffs (handoff_id, project_id, data_json, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(handoff_id) DO UPDATE SET data_json = excluded.data_json",
            params![handoff.handoff_id, project_id, json, handoff.created_at],
        )?;
        Ok(())
    }

    async fn get_handoff(&self, handoff_id: &str) -> Result<HandoffRecord> {
        let conn = self.conn.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT data_json FROM handoffs WHERE handoff_id = ?1",
                params![handoff_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ProjectSpaceStoreError::NotFound(format!("HandoffRecord {handoff_id}"))
                }
                other => ProjectSpaceStoreError::Sqlite(other),
            })?;
        Self::deserialize(&json)
    }

    async fn list_handoffs_by_project(&self, project_id: &str) -> Result<Vec<HandoffRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT data_json FROM handoffs
             WHERE project_id = ?1
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![project_id], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            let json = row?;
            result.push(Self::deserialize(&json)?);
        }
        Ok(result)
    }

    // -- TeamRun --

    async fn save_team_run(&self, team_run: &TeamRun) -> Result<()> {
        let json = Self::serialize(team_run)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO team_runs (team_id, data_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(team_id) DO UPDATE SET data_json = excluded.data_json, updated_at = excluded.updated_at",
            params![team_run.team_id, json, team_run.created_at, team_run.updated_at],
        )?;
        Ok(())
    }

    async fn get_team_run(&self, team_id: &str) -> Result<TeamRun> {
        let conn = self.conn.lock().unwrap();
        let json: String = conn
            .query_row(
                "SELECT data_json FROM team_runs WHERE team_id = ?1",
                params![team_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    ProjectSpaceStoreError::NotFound(format!("TeamRun {team_id}"))
                }
                other => ProjectSpaceStoreError::Sqlite(other),
            })?;
        Self::deserialize(&json)
    }

    async fn list_team_runs(&self) -> Result<Vec<TeamRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data_json FROM team_runs ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut result = Vec::new();
        for row in rows {
            let json = row?;
            result.push(Self::deserialize(&json)?);
        }
        Ok(result)
    }

    async fn delete_team_run(&self, team_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM team_runs WHERE team_id = ?1", params![team_id])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use owo_agent_protocol::{
        ArtifactClassification, MemberHealth, ReviewState, RuntimeBinding, TeamMember, TeamMode,
        TeamRunStatus,
    };

    fn test_store() -> SqliteProjectSpaceStore {
        let conn = Connection::open_in_memory().unwrap();
        SqliteProjectSpaceStore::from_connection(conn).unwrap()
    }

    fn sample_project_space(id: &str) -> ProjectSpace {
        let now = chrono::Utc::now().to_rfc3339();
        ProjectSpace {
            project_id: id.to_string(),
            goal_id: Some("goal-1".to_string()),
            team_id: Some("team-1".to_string()),
            tasks: vec!["task-a".to_string(), "task-b".to_string()],
            artifacts: vec!["art-1".to_string()],
            decisions: vec![],
            approvals: vec![],
            discussions: vec![],
            activity_stream: vec![],
            delivery_manifest_ref: None,
            version: 1,
            status: owo_agent_protocol::ProjectSpaceStatus::Active,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    fn sample_artifact(id: &str) -> Artifact {
        Artifact {
            artifact_id: id.to_string(),
            kind: "document".to_string(),
            version: 1,
            producer: "member-planner".to_string(),
            content_ref: "cas://sha256:abc123".to_string(),
            schema_ref: None,
            source_refs: vec!["trace-1".to_string()],
            classification: ArtifactClassification::Private,
            review_state: ReviewState::Draft,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    fn sample_team_run(id: &str) -> TeamRun {
        let now = chrono::Utc::now().to_rfc3339();
        TeamRun {
            team_id: id.to_string(),
            goal_id: Some("goal-1".to_string()),
            mode: TeamMode::Team,
            members: vec![TeamMember {
                member_id: "m-planner".to_string(),
                role: "planner".to_string(),
                runtime_binding: RuntimeBinding::Agent {
                    agent_id: "agent-1".to_string(),
                },
                capabilities: vec!["plan".to_string()],
                tool_scope: vec![],
                read_scope: vec!["*".to_string()],
                write_scope: vec!["artifacts".to_string()],
                budget: serde_json::json!({"max_turns": 10}),
                handoff_contract: Some("Plan artifact with steps".to_string()),
                health: MemberHealth::Active,
            }],
            task_graph_ref: Some("graph-1".to_string()),
            project_space_id: Some("proj-1".to_string()),
            template_id: None,
            shared_context_refs: vec![],
            budget: serde_json::json!({"max_duration_secs": 300}),
            human_policy: Some("human_approval_required".to_string()),
            status: TeamRunStatus::Created,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn project_space_crud_roundtrip() {
        let store = test_store();
        let space = sample_project_space("proj-crud");

        store.save_project_space(&space).await.unwrap();
        let loaded = store.get_project_space("proj-crud").await.unwrap();
        assert_eq!(loaded.project_id, "proj-crud");
        assert_eq!(loaded.goal_id, Some("goal-1".to_string()));
        assert_eq!(loaded.tasks.len(), 2);
        assert_eq!(loaded.version, 1);

        // Update
        let mut updated = loaded.clone();
        updated.version = 2;
        updated.tasks.push("task-c".to_string());
        store.save_project_space(&updated).await.unwrap();
        let reloaded = store.get_project_space("proj-crud").await.unwrap();
        assert_eq!(reloaded.version, 2);
        assert_eq!(reloaded.tasks.len(), 3);

        // List
        let all = store.list_project_spaces().await.unwrap();
        assert_eq!(all.len(), 1);

        // Delete
        store.delete_project_space("proj-crud").await.unwrap();
        let err = store.get_project_space("proj-crud").await;
        assert!(matches!(err, Err(ProjectSpaceStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn artifact_crud_roundtrip() {
        let store = test_store();
        let art = sample_artifact("art-crud");

        store.save_artifact(&art, "proj-1").await.unwrap();
        let loaded = store.get_artifact("art-crud").await.unwrap();
        assert_eq!(loaded.kind, "document");
        assert_eq!(loaded.classification, ArtifactClassification::Private);
        assert_eq!(loaded.review_state, ReviewState::Draft);

        // project_id 真实落库：按 project 过滤命中。
        let by_project = store.list_artifacts_by_project("proj-1").await.unwrap();
        assert_eq!(by_project.len(), 1);
        let other = store.list_artifacts_by_project("proj-2").await.unwrap();
        assert!(other.is_empty(), "其他 project 不应看到该 artifact");

        store.delete_artifact("art-crud").await.unwrap();
        let err = store.get_artifact("art-crud").await;
        assert!(matches!(err, Err(ProjectSpaceStoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn team_run_crud_roundtrip() {
        let store = test_store();
        let tr = sample_team_run("team-crud");

        store.save_team_run(&tr).await.unwrap();
        let loaded = store.get_team_run("team-crud").await.unwrap();
        assert_eq!(loaded.mode, TeamMode::Team);
        assert_eq!(loaded.members.len(), 1);
        assert_eq!(loaded.members[0].role, "planner");
        assert!(matches!(
            loaded.members[0].runtime_binding,
            RuntimeBinding::Agent { .. }
        ));

        // Status transition
        let mut updated = loaded;
        updated.status = TeamRunStatus::Running;
        store.save_team_run(&updated).await.unwrap();
        let reloaded = store.get_team_run("team-crud").await.unwrap();
        assert_eq!(reloaded.status, TeamRunStatus::Running);

        // List & delete
        let all = store.list_team_runs().await.unwrap();
        assert_eq!(all.len(), 1);
        store.delete_team_run("team-crud").await.unwrap();
        assert!(store.list_team_runs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn decision_and_handoff_roundtrip() {
        let store = test_store();
        let now = chrono::Utc::now().to_rfc3339();

        let decision = DecisionRecord {
            decision_id: "dec-1".to_string(),
            proposer: "m-leader".to_string(),
            choice: "Use template A for browser tasks".to_string(),
            rationale: "Template A has higher grounding accuracy in S1 benchmarks.".to_string(),
            affected_refs: vec!["task-b".to_string()],
            created_at: now.clone(),
        };
        store.save_decision(&decision, "proj-1").await.unwrap();
        let loaded_dec = store.get_decision("dec-1").await.unwrap();
        assert_eq!(loaded_dec.choice, "Use template A for browser tasks");

        let handoff = HandoffRecord {
            handoff_id: "ho-1".to_string(),
            from_member: "m-planner".to_string(),
            to_member: "m-builder".to_string(),
            completed_summary: "Plan generated with 5 steps".to_string(),
            open_issues: vec!["Step 3 needs clarification".to_string()],
            output_artifact_refs: vec!["art-plan-v1".to_string()],
            evidence_refs: vec!["trace-plan".to_string()],
            suggested_next_actions: vec!["Execute step 1-2".to_string()],
            known_risks: vec!["Step 4 may timeout".to_string()],
            created_at: now.clone(),
        };
        store.save_handoff(&handoff, "proj-1").await.unwrap();
        let loaded_ho = store.get_handoff("ho-1").await.unwrap();
        assert_eq!(loaded_ho.from_member, "m-planner");
        assert_eq!(loaded_ho.open_issues.len(), 1);

        // project_id 真实落库：decisions/handoffs 按 project 过滤命中。
        let decs = store.list_decisions_by_project("proj-1").await.unwrap();
        assert_eq!(decs.len(), 1);
        let hos = store.list_handoffs_by_project("proj-1").await.unwrap();
        assert_eq!(hos.len(), 1);
    }

    #[tokio::test]
    async fn dto_serialization_stability() {
        // Verify key enums serialize to expected snake_case strings
        let mode: TeamMode = TeamMode::Swarmflow;
        assert_eq!(serde_json::to_string(&mode).unwrap(), "\"swarmflow\"");

        let binding = RuntimeBinding::Human {
            user_id: "u1".to_string(),
        };
        let json = serde_json::to_value(&binding).unwrap();
        assert_eq!(json["kind"], "human");
        assert_eq!(json["user_id"], "u1");

        let status = TeamRunStatus::AwaitingHuman;
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            "\"awaiting_human\""
        );

        let health = MemberHealth::Fused;
        assert_eq!(serde_json::to_string(&health).unwrap(), "\"fused\"");

        let cls = ArtifactClassification::Sensitive;
        assert_eq!(serde_json::to_string(&cls).unwrap(), "\"sensitive\"");

        let rs = ReviewState::PendingReview;
        assert_eq!(serde_json::to_string(&rs).unwrap(), "\"pending_review\"");
    }
}
