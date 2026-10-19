//! 操作学习（v0.4 D23/D24/D26）：示范学习 + 受限自主探索双轨、
//! 动作图（Action Graph）、流程技能包、主动建议。
//!
//! 安全边界：
//! - 敏感面（密码/支付/验证码）在任何轨都熔断：不学习、不记录。
//! - 学习样本默认掩码，消息内容不采样；录制可暂停、可一键清空。
//! - 主动建议默认仅提示，不执行。

// M11：ProactiveSettings 随本域搬入（配置类型随域走，见 §9.2）；
// skill_health 已在 M8 下沉到 owo-agent-contracts，这里改走绝对路径。
#[cfg(test)]
use crate::proactive_settings::ProactiveSettings;
use chrono::Utc;
use owo_agent_contracts::skill_health::{FailureMode, SkillState};
use serde::{Deserialize, Serialize};

// ---------- 动作图 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    Click,
    Type,
    Shortcut,
    Inject,
    /// 启动应用或打开 URL（主动打开，不依赖应用已在前台）。
    Launch,
    /// 按屏幕坐标点击（OCR 定位的自绘控件，如 QQ 红包/表情面板）。
    ClickAt,
    /// 滚动（v0.5 M-B 动作程序扩展）。
    Scroll,
    /// 拖拽（v0.5 M-B 动作程序扩展）。
    Drag,
    /// 等待（v0.5 M-B 动作程序扩展）。
    Wait,
    /// 结构化断言动作（v0.5 M-B，配合 assert.rs 的 VerificationRecipe）。
    Assert,
    /// 悬停（v0.5 M-B 动作程序扩展）。
    Hover,
    /// 右键点击（v0.5 M-B 动作程序扩展）。
    RightClick,
    /// 双击（v0.5 M-B 动作程序扩展）。
    DoubleClick,
}

/// 语义锚点：以无障碍角色 + 名称定位，坐标只作辅助，不作为主定位依据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticAnchor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub name: String,
    /// 稳定元素 ID（窗口元素注册表）：存在时优先按注册表坐标定位，
    /// 避免每次动作都重新做 UIA/OCR 定位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    /// 父容器名称（如“会话列表”），用于消除同名节点歧义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionNode {
    pub id: String,
    pub action_type: ActionType,
    pub anchor: SemanticAnchor,
    /// 变量模板：`{contact}` 之类，执行时由用户或情景模型填充。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_template: Option<String>,
    /// 操作后状态验证描述（窗口/标题/内容是否变化）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionEdge {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionGraph {
    pub version: u32,
    pub start: String,
    pub nodes: Vec<ActionNode>,
    pub edges: Vec<ActionEdge>,
}

impl Default for ActionGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionGraph {
    pub fn new() -> Self {
        Self {
            version: 1,
            start: String::new(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn add_node(
        &mut self,
        id: impl Into<String>,
        action_type: ActionType,
        anchor: SemanticAnchor,
        value_template: Option<String>,
        verify: Option<String>,
    ) {
        let id = id.into();
        if self.start.is_empty() {
            self.start = id.clone();
        }
        self.nodes.push(ActionNode {
            id,
            action_type,
            anchor,
            value_template,
            verify,
        });
    }

    pub fn add_edge(
        &mut self,
        from: impl Into<String>,
        to: impl Into<String>,
        precondition: Option<String>,
        verify: Option<String>,
    ) {
        self.edges.push(ActionEdge {
            from: from.into(),
            to: to.into(),
            precondition,
            verify,
        });
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.nodes.is_empty() {
            return Err("动作图没有节点".to_string());
        }
        if !self.nodes.iter().any(|node| node.id == self.start) {
            return Err(format!("起始节点不存在：{}", self.start));
        }
        let mut ids = std::collections::HashSet::new();
        for node in &self.nodes {
            if !ids.insert(node.id.as_str()) {
                return Err(format!("重复节点 id：{}", node.id));
            }
        }
        for edge in &self.edges {
            if !ids.contains(edge.from.as_str()) || !ids.contains(edge.to.as_str()) {
                return Err(format!("边引用不存在的节点：{} -> {}", edge.from, edge.to));
            }
        }
        Ok(())
    }

    /// 提取 `{var}` 变量名。
    pub fn variables(&self) -> Vec<String> {
        let mut vars = Vec::new();
        for node in &self.nodes {
            if let Some(template) = &node.value_template {
                for token in template.split('{').skip(1) {
                    if let Some(end) = token.find('}') {
                        let name = token[..end].trim().to_string();
                        if !name.is_empty() && !vars.contains(&name) {
                            vars.push(name);
                        }
                    }
                }
            }
        }
        vars
    }
}

// ---------- 流程技能包 ----------

mod skills;

pub use skills::{FlowSkillManifest, FlowSkillPackage, FlowSkillStore, Sensitivity};

mod recorder;

pub use recorder::{
    candidate_eligible, generalize_to_graph, generalize_traces, recorded_actions_from_sequence,
    LearnPipeline, LearnRecorder, LearnState, RecordedAction,
};

// ---------- 主动建议 ----------

mod proactive;

pub use proactive::{ProactiveEngine, ProactiveSuggestion, SuggestionAction};

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with_contact() -> ActionGraph {
        let mut graph = ActionGraph::new();
        graph.add_node(
            "find",
            ActionType::Click,
            SemanticAnchor {
                app_id: Some("qq".to_string()),
                role: Some("search_box".to_string()),
                name: "联系人搜索框".to_string(),
                parent: None,
                element_id: None,
            },
            None,
            Some("窗口标题变化".to_string()),
        );
        graph.add_node(
            "type",
            ActionType::Type,
            SemanticAnchor {
                app_id: Some("qq".to_string()),
                role: Some("input".to_string()),
                name: "搜索输入".to_string(),
                parent: None,
                element_id: None,
            },
            Some("{contact}".to_string()),
            None,
        );
        graph.add_edge("find", "type", None, None);
        graph
    }

    #[test]
    fn action_graph_validation_and_variables() {
        let graph = graph_with_contact();
        assert!(graph.validate().is_ok());
        assert_eq!(graph.variables(), vec!["contact"]);
        let mut broken = graph.clone();
        broken.edges.push(ActionEdge {
            from: "type".to_string(),
            to: "missing".to_string(),
            precondition: None,
            verify: None,
        });
        assert!(broken.validate().is_err());
    }

    #[test]
    fn semantic_anchor_element_id_round_trip_and_backward_compat() {
        let anchor = SemanticAnchor {
            app_id: Some("qq".into()),
            role: Some("button".into()),
            name: "发送".into(),
            element_id: Some("qq:button:3".into()),
            parent: Some("会话列表".into()),
        };
        let json = serde_json::to_string(&anchor).expect("序列化");
        assert!(json.contains("qq:button:3"));
        let restored: SemanticAnchor = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(restored.element_id.as_deref(), Some("qq:button:3"));

        // 旧格式（无 element_id 字段）必须仍可解析，且默认 None。
        let legacy = r#"{"app_id":"qq","role":"button","name":"发送","parent":"会话列表"}"#;
        let legacy_anchor: SemanticAnchor = serde_json::from_str(legacy).expect("旧格式兼容");
        assert!(legacy_anchor.element_id.is_none());
        assert_eq!(legacy_anchor.name, "发送");
    }

    #[test]
    fn flow_skill_package_requires_sensitivity_and_declared_variables() {
        let graph = graph_with_contact();
        let package = FlowSkillPackage {
            manifest: FlowSkillManifest {
                id: "com.example.send-file".to_string(),
                name: "send-file".to_string(),
                version: "1.0.0".to_string(),
                min_app_version: "0.4.0".to_string(),
                target_apps: vec!["qq".to_string()],
                permissions: vec!["text:inject".to_string()],
                variables: vec!["contact".to_string()],
                sensitivity: Sensitivity::Low,
            },
            graph,
            skill_md: "---\nname: send-file\ndescription: 发送文件\n---\n流程".to_string(),
        };
        assert!(package.validate().is_ok());

        let mut no_sensitivity = package.clone();
        no_sensitivity.manifest.sensitivity = Sensitivity::None;
        assert!(no_sensitivity.validate().is_err());

        let mut undeclared = package.clone();
        undeclared.manifest.variables = Vec::new();
        assert!(undeclared.validate().is_err());
    }

    #[test]
    fn flow_skill_store_round_trip_list_and_delete() {
        let root = std::env::temp_dir().join(format!("owo-learn-store-{}", uuid::Uuid::new_v4()));
        let store = FlowSkillStore::new(root.join("skills").join("user"));
        let package = FlowSkillPackage {
            manifest: FlowSkillManifest {
                id: "com.example.demo".to_string(),
                name: "demo-flow".to_string(),
                version: "1.0.0".to_string(),
                min_app_version: "0.4.0".to_string(),
                target_apps: vec!["qq".to_string()],
                permissions: vec!["text:inject".to_string()],
                variables: vec!["contact".to_string()],
                sensitivity: Sensitivity::Medium,
            },
            graph: graph_with_contact(),
            skill_md: "---\nname: demo-flow\n---\nbody".to_string(),
        };
        let dir = store.save(&package).unwrap();
        assert!(dir.join("SKILL.md").exists());
        assert!(dir.join("graph.json").exists());
        assert!(dir.join("manifest.json").exists());
        assert_eq!(store.list().unwrap(), vec!["demo-flow"]);
        let loaded = store.load("demo-flow").unwrap();
        assert_eq!(loaded.manifest.name, "demo-flow");
        assert_eq!(loaded.graph.nodes.len(), 2);
        store.delete("demo-flow").unwrap();
        assert!(store.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recorder_requires_start_and_breaks_on_sensitive() {
        let mut recorder = LearnRecorder::new();
        assert!(recorder
            .record(RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: None,
                    name: "发送按钮".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Click,
                value_masked: true,
                sensitive: false,
                at: "now".to_string(),
            })
            .is_err());
        recorder.start();
        recorder
            .record(RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: None,
                    name: "发送按钮".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Click,
                value_masked: true,
                sensitive: false,
                at: "now".to_string(),
            })
            .unwrap();
        assert_eq!(recorder.samples(), 1);
        assert!(recorder
            .record(RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: None,
                    name: "密码输入框".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Type,
                value_masked: true,
                sensitive: true,
                at: "now".to_string(),
            })
            .is_err());
        assert!(recorder.sensitive_break());
        recorder.pause();
        assert_eq!(recorder.state(), LearnState::Paused);
        recorder.resume();
        let samples = recorder.stop();
        assert_eq!(samples.len(), 1);
        assert_eq!(recorder.state(), LearnState::Idle);
    }

    #[test]
    fn proactive_engine_thresholds_cooldown_and_ignore_silence() {
        let settings = ProactiveSettings {
            enabled: true,
            weekly_threshold: 5,
            daily_threshold: 3,
            similarity: 0.9,
            cooldown_hours: 24,
            daily_cap: 3,
            auto_silence_days: 30,
        };
        let mut engine = ProactiveEngine::new(settings);
        let actions = vec![
            "focus".to_string(),
            "select_conversation".to_string(),
            "click_send".to_string(),
        ];
        assert!(engine.observe("qq", actions.clone()).is_none());
        assert!(engine.observe("qq", actions.clone()).is_none());
        let suggestion = engine
            .observe("qq", actions.clone())
            .expect("daily threshold");
        assert!(!suggestion.auto_exec);
        assert!(engine.observe("qq", actions.clone()).is_none()); // cooldown
        engine
            .decide(&suggestion.id, SuggestionAction::Ignore)
            .unwrap();
        engine
            .decide(&suggestion.id, SuggestionAction::Ignore)
            .unwrap();
        assert!(!engine.muted_until.is_empty()); // 忽略 2 次后进入静默
        let key = "qq:focus|select_conversation|click_send";
        assert!(engine.muted_until.contains_key(key));
    }

    #[test]
    fn apply_settings_updates_runtime_thresholds() {
        let mut engine = ProactiveEngine::new(ProactiveSettings {
            enabled: false,
            weekly_threshold: 5,
            daily_threshold: 5,
            similarity: 1.0,
            cooldown_hours: 0,
            daily_cap: 10,
            auto_silence_days: 30,
        });
        let actions = vec!["a".to_string(), "b".to_string()];
        assert!(engine.observe("app", actions.clone()).is_none());
        engine.apply_settings(ProactiveSettings {
            enabled: true,
            weekly_threshold: 3,
            daily_threshold: 1,
            similarity: 1.0,
            cooldown_hours: 0,
            daily_cap: 10,
            auto_silence_days: 30,
        });
        let suggestion = engine.observe("app", actions.clone()).unwrap();
        assert!(!suggestion.auto_exec);
    }

    #[test]
    fn proactive_engine_can_be_suppressed_and_muted_forever() {
        let settings = ProactiveSettings {
            enabled: true,
            weekly_threshold: 3,
            daily_threshold: 3,
            similarity: 1.0,
            cooldown_hours: 0,
            daily_cap: 10,
            auto_silence_days: 30,
        };
        let mut engine = ProactiveEngine::new(settings);
        engine.set_suppressed(true);
        let actions = vec!["a".to_string(), "b".to_string()];
        assert!(engine.observe("app", actions.clone()).is_none());
        engine.set_suppressed(false);
        assert!(engine.observe("app", actions.clone()).is_none());
        let suggestion = engine.observe("app", actions.clone()).unwrap();
        engine
            .decide(&suggestion.id, SuggestionAction::MuteForever)
            .unwrap();
        assert_eq!(
            engine.muted_until.get("app:a|b").map(String::as_str),
            Some("9999-12-31")
        );
    }

    fn typed(anchor_name: &str, at: &str) -> RecordedAction {
        RecordedAction {
            app_id: "qq".to_string(),
            anchor: SemanticAnchor {
                app_id: Some("qq".to_string()),
                role: Some("edit".to_string()),
                name: anchor_name.to_string(),
                parent: None,
                element_id: None,
            },
            action_type: ActionType::Type,
            value_masked: true,
            sensitive: false,
            at: at.to_string(),
        }
    }

    #[test]
    fn generalizes_repeated_typed_anchor_to_variable() {
        let samples = vec![
            typed("搜索输入", "t1"),
            typed("搜索输入", "t2"),
            RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: Some("button".to_string()),
                    name: "发送按钮".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Click,
                value_masked: true,
                sensitive: false,
                at: "t3".to_string(),
            },
        ];
        let graph = generalize_to_graph(&samples).unwrap();
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.variables(), vec!["value"]);
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn generalize_traces_infers_variable_from_consistent_type_position() {
        let trace = |value_at: &str| {
            vec![
                typed("搜索输入", value_at),
                RecordedAction {
                    app_id: "qq".to_string(),
                    anchor: SemanticAnchor {
                        app_id: Some("qq".to_string()),
                        role: Some("button".to_string()),
                        name: "发送按钮".to_string(),
                        parent: None,
                        element_id: None,
                    },
                    action_type: ActionType::Click,
                    value_masked: true,
                    sensitive: false,
                    at: format!("{value_at}-click"),
                },
            ]
        };
        let traces = vec![trace("t1"), trace("t2"), trace("t3")];
        let graph = generalize_traces(&traces).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.variables(), vec!["value"]);
        assert!(graph.validate().is_ok());
        assert_eq!(graph.nodes[0].anchor.name, "搜索输入");
        assert_eq!(graph.nodes[1].anchor.name, "发送按钮");
    }

    #[test]
    fn generalize_traces_aligns_traces_with_length_variation() {
        // 第二条轨迹多一次无关点击，仍应对齐到骨干并生成合法图。
        let trace_a = vec![
            typed("搜索输入", "t1"),
            RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: Some("button".to_string()),
                    name: "发送按钮".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Click,
                value_masked: true,
                sensitive: false,
                at: "t1-click".to_string(),
            },
        ];
        let mut trace_b = trace_a.clone();
        trace_b.insert(
            1,
            RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: Some("button".to_string()),
                    name: "无关按钮".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Click,
                value_masked: true,
                sensitive: false,
                at: "t2-extra".to_string(),
            },
        );
        let graph = generalize_traces(&[trace_a, trace_b.clone(), trace_b]).unwrap();
        assert!(graph.validate().is_ok());
        assert_eq!(graph.nodes[0].anchor.name, "搜索输入");
        assert!(graph.variables().contains(&"value".to_string()));
    }

    #[test]
    fn candidate_eligible_enforces_count_and_success_rate() {
        let trace = || vec![typed("搜索输入", "t")];
        assert!(candidate_eligible(&[], &[]).is_err());
        assert!(candidate_eligible(&[trace(), trace()], &[]).is_err());
        assert!(candidate_eligible(&[trace(), trace(), trace()], &[]).is_ok());

        // 3 条轨迹 2 次成功 = 66.7% < 80%，拒绝。
        assert!(candidate_eligible(&[trace(), trace(), trace()], &[true, true, false]).is_err());
        // 5 条轨迹 4 次成功 = 80%，通过。
        assert!(candidate_eligible(
            &[trace(), trace(), trace(), trace(), trace()],
            &[true, true, true, true, false]
        )
        .is_ok());
    }

    #[test]
    fn generalize_traces_rejects_sensitive_or_empty_traces() {
        assert!(generalize_traces(&[vec![], vec![]]).is_err());
        let mut sensitive = typed("搜索输入", "t");
        sensitive.sensitive = true;
        assert!(generalize_traces(&[vec![sensitive], vec![typed("搜索输入", "t")]]).is_err());
    }

    #[test]
    fn pipeline_sinks_skill_package() {
        let root =
            std::env::temp_dir().join(format!("owo-learn-pipeline-{}", uuid::Uuid::new_v4()));
        let mut pipeline = LearnPipeline::new(root.join("skills").join("user"));
        pipeline.recorder.start();
        pipeline.recorder.record(typed("搜索输入", "t1")).unwrap();
        pipeline
            .recorder
            .record(RecordedAction {
                app_id: "qq".to_string(),
                anchor: SemanticAnchor {
                    app_id: Some("qq".to_string()),
                    role: Some("button".to_string()),
                    name: "发送按钮".to_string(),
                    parent: None,
                    element_id: None,
                },
                action_type: ActionType::Click,
                value_masked: true,
                sensitive: false,
                at: "t2".to_string(),
            })
            .unwrap();
        pipeline.stop_recording();
        let package = pipeline
            .sink_skill(
                "send-file",
                vec!["qq".to_string()],
                Sensitivity::Low,
                "在 QQ 发送文件",
            )
            .unwrap();
        assert!(package.validate().is_ok());
        assert_eq!(package.manifest.name, "send-file");
        assert_eq!(pipeline.store.list().unwrap(), vec!["send-file"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recorded_actions_from_sequence_parses_click_and_type() {
        let sequence = vec![
            "click:发送".to_string(),
            "type:输入消息".to_string(),
            "搜索".to_string(),
        ];
        let actions = recorded_actions_from_sequence("qq", &sequence);
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action_type, ActionType::Click);
        assert_eq!(actions[0].anchor.name, "发送");
        assert_eq!(actions[0].anchor.role.as_deref(), Some("button"));
        assert_eq!(actions[1].action_type, ActionType::Type);
        assert!(actions[1].value_masked);
        assert_eq!(actions[2].action_type, ActionType::Click);
        assert_eq!(actions[2].anchor.name, "搜索");

        // 空项与未知前缀按 click 处理/跳过空锚点。
        let actions = recorded_actions_from_sequence(
            "qq",
            &["click:".to_string(), "shortcut:ctrl+a".to_string()],
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].anchor.name, "ctrl+a");
        assert_eq!(actions[0].action_type, ActionType::Click);
    }

    #[test]
    fn sink_from_actions_confirms_suggestion_into_active_package() {
        let root = std::env::temp_dir().join(format!("owo-learn-suggest-{}", uuid::Uuid::new_v4()));
        let pipeline = LearnPipeline::new(root.join("skills").join("user"));
        let sequence = vec![
            "click:搜索".to_string(),
            "type:输入消息".to_string(),
            "click:发送".to_string(),
        ];
        let samples = recorded_actions_from_sequence("qq", &sequence);
        let package = pipeline
            .sink_from_actions(
                "proactive-reply",
                vec!["qq".to_string()],
                Sensitivity::Low,
                "重复回复流程，用户确认沉淀",
                samples,
            )
            .expect("沉淀成功");
        assert!(package.validate().is_ok());
        assert_eq!(package.manifest.name, "proactive-reply");
        assert_eq!(pipeline.store.list().unwrap(), vec!["proactive-reply"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn decide_learn_removes_suggestion_from_list() {
        let mut engine = ProactiveEngine::new(ProactiveSettings {
            enabled: true,
            weekly_threshold: 5,
            daily_threshold: 1,
            similarity: 1.0,
            cooldown_hours: 0,
            daily_cap: 10,
            auto_silence_days: 30,
        });
        let actions = vec!["click:发送".to_string()];
        let suggestion = engine
            .observe("qq", actions)
            .expect("daily threshold 1 应产生建议");
        assert_eq!(engine.suggestions().len(), 1);
        engine
            .decide(&suggestion.id, SuggestionAction::Learn)
            .expect("确认学习");
        assert!(engine.suggestions().is_empty());
    }
}
