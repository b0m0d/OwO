//! 流程技能包：敏感度、清单、包校验与落盘存储（从 learn.rs 拆出）。

use super::*;
use owo_agent_contracts::skill_health::{SkillHealth, SkillHealthStore};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    None,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowSkillManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub min_app_version: String,
    /// 目标应用白名单（app_id）。
    pub target_apps: Vec<String>,
    pub permissions: Vec<String>,
    pub variables: Vec<String>,
    /// 敏感面声明（必填；None 视为未声明，校验不通过）。
    pub sensitivity: Sensitivity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowSkillPackage {
    pub manifest: FlowSkillManifest,
    pub graph: ActionGraph,
    pub skill_md: String,
}

impl FlowSkillPackage {
    pub fn validate(&self) -> Result<(), String> {
        if self.manifest.id.is_empty() || self.manifest.name.is_empty() {
            return Err("manifest 缺少 id/name".to_string());
        }
        if self.manifest.target_apps.is_empty() {
            return Err("manifest.target_apps 不能为空".to_string());
        }
        if self.manifest.sensitivity == Sensitivity::None {
            return Err("manifest.sensitivity 必填".to_string());
        }
        self.graph.validate()?;
        for variable in self.graph.variables() {
            if !self.manifest.variables.contains(&variable) {
                return Err(format!("动作图变量未在 manifest 声明：{variable}"));
            }
        }
        if !self.skill_md.trim_start().starts_with("---") {
            return Err("SKILL.md 缺少 frontmatter".to_string());
        }
        Ok(())
    }
}

/// 流程技能包存储：`<data>/skills/user/<name>/`（SKILL.md + graph.json + manifest.json）。
pub struct FlowSkillStore {
    root: PathBuf,
    health: std::sync::Mutex<SkillHealthStore>,
}

impl FlowSkillStore {
    pub fn new(root: PathBuf) -> Self {
        let health = SkillHealthStore::new(Some(root.join("health.json")));
        Self {
            root,
            health: std::sync::Mutex::new(health),
        }
    }

    fn package_dir(&self, name: &str) -> Result<PathBuf, String> {
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("非法技能名：{name}"));
        }
        Ok(self.root.join(name))
    }

    pub fn save(&self, package: &FlowSkillPackage) -> Result<PathBuf, String> {
        package.validate()?;
        let dir = self.package_dir(&package.manifest.name)?;
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        std::fs::write(dir.join("SKILL.md"), &package.skill_md)
            .map_err(|error| error.to_string())?;
        std::fs::write(
            dir.join("graph.json"),
            serde_json::to_string_pretty(&package.graph).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_string_pretty(&package.manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        Ok(dir)
    }

    pub fn list(&self) -> Result<Vec<String>, String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if is_dir && entry.path().join("SKILL.md").exists() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn load(&self, name: &str) -> Result<FlowSkillPackage, String> {
        let dir = self.package_dir(name)?;
        let skill_md =
            std::fs::read_to_string(dir.join("SKILL.md")).map_err(|error| error.to_string())?;
        let graph = serde_json::from_str(
            &std::fs::read_to_string(dir.join("graph.json")).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let manifest = serde_json::from_str(
            &std::fs::read_to_string(dir.join("manifest.json"))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let package = FlowSkillPackage {
            manifest,
            graph,
            skill_md,
        };
        package.validate()?;
        Ok(package)
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        let dir = self.package_dir(name)?;
        if !dir.exists() {
            return Err(format!("技能包不存在：{name}"));
        }
        std::fs::remove_dir_all(&dir).map_err(|error| error.to_string())
    }

    /// 记录一次执行结果（成功/失败），失败带步骤与原因；返回最新状态。
    pub fn record_execution(
        &self,
        name: &str,
        ok: bool,
        step: &str,
        reason: &str,
    ) -> Result<SkillState, String> {
        self.load(name)?;
        let failure = if ok {
            None
        } else {
            Some(FailureMode {
                step: step.to_string(),
                reason: reason.to_string(),
                at: Utc::now().to_rfc3339(),
            })
        };
        self.health
            .lock()
            .map_err(|_| "健康度存储锁中毒".to_string())?
            .record(name, ok, failure)
    }

    /// 执行门禁：Disabled 一律拒绝；Degraded 需显式 degraded_ack。
    pub fn execution_gate(&self, name: &str, degraded_ack: bool) -> Result<(), String> {
        self.load(name)?;
        let state = self
            .health
            .lock()
            .map_err(|_| "健康度存储锁中毒".to_string())?
            .state(name);
        match state {
            SkillState::Disabled => Err(format!("技能已禁用：{name}")),
            SkillState::Degraded if !degraded_ack => Err(format!(
                "技能处于降级状态（连续失败或模板命中率过低），需 degraded_ack:true 确认后执行：{name}"
            )),
            _ => Ok(()),
        }
    }

    pub fn health_state(&self, name: &str) -> SkillState {
        self.health
            .lock()
            .map(|health| health.state(name))
            .unwrap_or(SkillState::Active)
    }

    pub fn list_health(&self) -> Vec<(String, SkillHealth)> {
        self.health
            .lock()
            .map(|health| health.list())
            .unwrap_or_default()
    }

    pub fn reset_health(&self, name: &str) -> Result<(), String> {
        self.load(name)?;
        self.health
            .lock()
            .map_err(|_| "健康度存储锁中毒".to_string())?
            .reset(name)
    }
}
