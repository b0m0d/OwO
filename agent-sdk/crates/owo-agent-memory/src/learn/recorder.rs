//! 示范学习录制与轨迹泛化：RecordedAction/LearnRecorder/LearnPipeline + 动作图泛化（从 learn/mod.rs 拆出）。

use super::*;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------- 示范学习录制 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearnState {
    Idle,
    Recording,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedAction {
    pub app_id: String,
    pub anchor: SemanticAnchor,
    pub action_type: ActionType,
    /// 输入内容默认掩码：不保存真实消息/密码。
    #[serde(default)]
    pub value_masked: bool,
    /// 调用方标记的敏感面（密码框/支付/验证码），一旦触发立即熔断。
    #[serde(default)]
    pub sensitive: bool,
    pub at: String,
}

fn keyword_breaks(anchor: &SemanticAnchor) -> bool {
    ["password", "支付", "密码", "验证码", "captcha", "card"]
        .iter()
        .any(|keyword| {
            anchor.name.contains(keyword)
                || anchor
                    .role
                    .as_deref()
                    .map(|role| role.contains(keyword))
                    .unwrap_or(false)
        })
}

pub struct LearnRecorder {
    state: LearnState,
    actions: Vec<RecordedAction>,
    sensitive_break: bool,
}

impl Default for LearnRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl LearnRecorder {
    pub fn new() -> Self {
        Self {
            state: LearnState::Idle,
            actions: Vec::new(),
            sensitive_break: false,
        }
    }

    pub fn start(&mut self) {
        self.state = LearnState::Recording;
    }

    pub fn pause(&mut self) {
        if self.state == LearnState::Recording {
            self.state = LearnState::Paused;
        }
    }

    pub fn resume(&mut self) {
        if self.state == LearnState::Paused {
            self.state = LearnState::Recording;
        }
    }

    /// 结束录制并返回本次样本。
    pub fn stop(&mut self) -> Vec<RecordedAction> {
        self.state = LearnState::Idle;
        self.sensitive_break = false;
        std::mem::take(&mut self.actions)
    }

    pub fn clear(&mut self) {
        self.actions.clear();
        self.sensitive_break = false;
    }

    pub fn record(&mut self, action: RecordedAction) -> Result<(), String> {
        if self.state != LearnState::Recording {
            return Err("未在录制（/learn start 开始）".to_string());
        }
        if action.sensitive || keyword_breaks(&action.anchor) {
            self.sensitive_break = true;
            return Err("敏感面熔断：密码/支付/验证码等场景不学习、不记录".to_string());
        }
        self.actions.push(action);
        Ok(())
    }

    pub fn state(&self) -> LearnState {
        self.state
    }

    pub fn actions(&self) -> &[RecordedAction] {
        &self.actions
    }

    pub fn samples(&self) -> usize {
        self.actions.len()
    }

    pub fn sensitive_break(&self) -> bool {
        self.sensitive_break
    }
}

/// 录制样本 → 动作图（泛化）：同一锚点的 Type 动作出现 ≥2 次，
/// 推断为变量 `{value}`（消息内容不采样，只按锚点重复度推断）。
pub fn generalize_to_graph(samples: &[RecordedAction]) -> Result<ActionGraph, String> {
    if samples.is_empty() {
        return Err("没有录制样本".to_string());
    }
    let mut graph = ActionGraph::new();
    for (index, action) in samples.iter().enumerate() {
        if action.sensitive || keyword_breaks(&action.anchor) {
            return Err("样本含敏感面，已拒绝沉淀".to_string());
        }
        let id = format!("step-{}", index + 1);
        let value_template = if action.action_type == ActionType::Type {
            let repeated = samples
                .iter()
                .filter(|other| {
                    other.action_type == ActionType::Type
                        && other.anchor.name == action.anchor.name
                        && other.anchor.role == action.anchor.role
                })
                .count();
            if repeated >= 2 {
                Some("{value}".to_string())
            } else {
                None
            }
        } else {
            None
        };
        graph.add_node(
            id,
            action.action_type,
            action.anchor.clone(),
            value_template,
            None,
        );
    }
    for index in 0..samples.len().saturating_sub(1) {
        graph.add_edge(
            format!("step-{}", index + 1),
            format!("step-{}", index + 2),
            None,
            None,
        );
    }
    graph.validate()?;
    Ok(graph)
}

/// 多轨迹对齐 + 变量推断（v0.5 M-C，对应技术文档 5.8.3）。
///
/// 归一化（动作类型 + 语义锚点）→ 多轨迹编辑距离对齐（以最长轨迹为骨干）→
/// 变量边界推断（骨干位置稳定但 Type 动作跨轨迹重复 → `{value}`）→ 线性动作图。
///
/// 输入内容默认掩码（不保存真实消息），因此变量推断基于“位置稳定 + 锚点重复”，
/// 与 `generalize_to_graph` 的单轨迹启发式一致，但增加了跨轨迹一致性约束：
/// 只有 ≥2 条轨迹在相同位置出现相同锚点的 Type 动作才推断为变量。
pub fn generalize_traces(traces: &[Vec<RecordedAction>]) -> Result<ActionGraph, String> {
    if traces.len() < 2 {
        // 少于 2 条轨迹时回退到单轨迹泛化（保持既有行为兼容）。
        if let Some(single) = traces.first() {
            return generalize_to_graph(single);
        }
        return Err("没有可泛化的轨迹".to_string());
    }

    for trace in traces {
        if trace.is_empty() {
            return Err("存在空轨迹".to_string());
        }
        for action in trace {
            if action.sensitive || keyword_breaks(&action.anchor) {
                return Err("轨迹含敏感面，已拒绝泛化".to_string());
            }
        }
    }

    // 骨干 = 最长轨迹；其余轨迹与骨干做编辑距离对齐。
    let backbone_idx = traces
        .iter()
        .enumerate()
        .max_by_key(|(_, trace)| trace.len())
        .map(|(index, _)| index)
        .unwrap_or(0);
    let backbone = &traces[backbone_idx];
    let backbone_keys: Vec<String> = backbone.iter().map(normalized_action_key).collect();

    // aligned[i] = 第 i 条轨迹中与骨干第 j 个位置对齐的索引（Option）。
    let mut aligned: Vec<Vec<Option<usize>>> = Vec::new();
    for (index, trace) in traces.iter().enumerate() {
        if index == backbone_idx {
            aligned.push((0..trace.len()).map(Some).collect());
            continue;
        }
        let other_keys: Vec<String> = trace.iter().map(normalized_action_key).collect();
        aligned.push(align_to_backbone(&backbone_keys, &other_keys));
    }

    let mut graph = ActionGraph::new();
    for (position, action) in backbone.iter().enumerate() {
        if action.sensitive || keyword_breaks(&action.anchor) {
            return Err("样本含敏感面，已拒绝沉淀".to_string());
        }
        let id = format!("step-{}", position + 1);

        // 跨轨迹一致性：有多少条轨迹在该位置出现相同归一化锚点。
        let consistent = aligned
            .iter()
            .enumerate()
            .filter(|(trace_idx, mapping)| {
                if *trace_idx == backbone_idx {
                    return true;
                }
                mapping[position]
                    .map(|other_idx| {
                        normalized_action_key(&traces[*trace_idx][other_idx])
                            == backbone_keys[position]
                    })
                    .unwrap_or(false)
            })
            .count();

        let value_template = if action.action_type == ActionType::Type && consistent >= 2 {
            Some("{value}".to_string())
        } else {
            None
        };
        graph.add_node(
            id,
            action.action_type,
            action.anchor.clone(),
            value_template,
            None,
        );
    }
    for index in 0..backbone.len().saturating_sub(1) {
        graph.add_edge(
            format!("step-{}", index + 1),
            format!("step-{}", index + 2),
            None,
            None,
        );
    }
    graph.validate()?;
    Ok(graph)
}

/// 动作归一化键：动作类型 + 角色 + 名称（不含坐标与内容）。
fn normalized_action_key(action: &RecordedAction) -> String {
    format!(
        "{:?}/{}//{}",
        action.action_type,
        action.anchor.role.as_deref().unwrap_or(""),
        action.anchor.name
    )
}

/// 编辑距离对齐：返回骨干每个位置在 other 中的最佳对齐索引（gap 为 None）。
///
/// 使用 Needleman-Wunsch（匹配 0 / 错配 1 / 空位 1）并回溯。
fn align_to_backbone(backbone: &[String], other: &[String]) -> Vec<Option<usize>> {
    let m = backbone.len();
    let n = other.len();
    // score[i][j]：骨干前 i 个与 other 前 j 个的最小编辑代价。
    let mut score = vec![vec![0usize; n + 1]; m + 1];
    score[0] = (0..=n).collect();
    for (i, row) in score.iter_mut().enumerate().skip(1) {
        row[0] = i;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if backbone[i - 1] == other[j - 1] {
                0
            } else {
                1
            };
            score[i][j] = (score[i - 1][j - 1] + cost)
                .min(score[i - 1][j] + 1)
                .min(score[i][j - 1] + 1);
        }
    }
    let mut result = vec![None; m];
    let (mut i, mut j) = (m, n);
    while i > 0 {
        if j > 0
            && score[i][j] == score[i - 1][j - 1] + usize::from(backbone[i - 1] != other[j - 1])
        {
            result[i - 1] = Some(j - 1);
            i -= 1;
            j -= 1;
        } else if i > 0 && score[i][j] == score[i - 1][j] + 1 {
            i -= 1; // 骨干位置在 other 中缺失（gap）。
        } else if j > 0 && score[i][j] == score[i][j - 1] + 1 {
            j -= 1; // other 中的额外动作。
        } else {
            // 防御：理论不可达，按匹配处理。
            if j > 0 {
                result[i - 1] = Some(j - 1);
                i -= 1;
                j -= 1;
            } else {
                i -= 1;
            }
        }
    }
    result
}

/// 沉淀门槛（v0.5 M-C）：同 app + 归一化序列 ≥3 次且成功率 ≥80% 才生成候选。
///
/// `outcomes` 与 `traces` 等长时为各轨迹的成功判定；为空时按未知处理（只查数量门槛）。
pub fn candidate_eligible(traces: &[Vec<RecordedAction>], outcomes: &[bool]) -> Result<(), String> {
    if traces.len() < 3 {
        return Err(format!(
            "沉淀门槛未达：需要 ≥3 条轨迹，当前 {} 条",
            traces.len()
        ));
    }
    let first_app = traces[0]
        .first()
        .map(|action| action.app_id.as_str())
        .unwrap_or("");
    if first_app.is_empty() {
        return Err("沉淀门槛未达：轨迹缺少 app_id".to_string());
    }
    if traces
        .iter()
        .any(|trace| trace.iter().any(|action| action.app_id != first_app))
    {
        return Err("沉淀门槛未达：轨迹必须属于同一应用".to_string());
    }
    if !outcomes.is_empty() && outcomes.len() != traces.len() {
        return Err("沉淀门槛未达：outcomes 与轨迹数量不一致".to_string());
    }
    if !outcomes.is_empty() {
        let successes = outcomes.iter().filter(|ok| **ok).count();
        let rate = successes as f64 / outcomes.len() as f64;
        if rate < 0.8 {
            return Err(format!(
                "沉淀门槛未达：成功率 {:.1}% < 80%（{} 次成功 / {} 次尝试）",
                rate * 100.0,
                successes,
                outcomes.len()
            ));
        }
    }
    Ok(())
}

/// 示范学习流水线：录制 → 泛化 → 沉淀流程技能包。
pub struct LearnPipeline {
    pub recorder: LearnRecorder,
    pub store: FlowSkillStore,
    last_samples: Vec<RecordedAction>,
}

impl LearnPipeline {
    pub fn new(store_root: PathBuf) -> Self {
        Self {
            recorder: LearnRecorder::new(),
            store: FlowSkillStore::new(store_root),
            last_samples: Vec::new(),
        }
    }

    /// 结束录制并保留样本（供随后沉淀）。
    pub fn stop_recording(&mut self) -> Vec<RecordedAction> {
        let samples = self.recorder.stop();
        self.last_samples = samples.clone();
        samples
    }

    /// 结束录制并沉淀为流程技能包（SKILL.md + graph.json + manifest.json）。
    pub fn sink_skill(
        &mut self,
        name: &str,
        target_apps: Vec<String>,
        sensitivity: Sensitivity,
        description: &str,
    ) -> Result<FlowSkillPackage, String> {
        let samples = if self.last_samples.is_empty() {
            self.recorder.stop()
        } else {
            std::mem::take(&mut self.last_samples)
        };
        self.sink_from_actions(name, target_apps, sensitivity, description, samples)
    }

    /// 直接由动作样本沉淀流程技能包（主动建议“学习”确认、静默观察挖掘共用）。
    pub fn sink_from_actions(
        &self,
        name: &str,
        target_apps: Vec<String>,
        sensitivity: Sensitivity,
        description: &str,
        samples: Vec<RecordedAction>,
    ) -> Result<FlowSkillPackage, String> {
        let graph = generalize_to_graph(&samples)?;
        let variables = graph.variables();
        let package = FlowSkillPackage {
            manifest: FlowSkillManifest {
                id: format!("com.owo.learned.{name}"),
                name: name.to_string(),
                version: "1.0.0".to_string(),
                min_app_version: "0.4.0".to_string(),
                target_apps,
                permissions: vec!["ui:operate".to_string(), "text:inject".to_string()],
                variables,
                sensitivity,
            },
            graph,
            skill_md: format!(
                "---\nname: {name}\ndescription: {description}\n---\n由示范学习自动生成，可编辑。"
            ),
        };
        package.validate()?;
        self.store.save(&package)?;
        Ok(package)
    }
}

/// 把主动建议的动作序列字符串解析为学习样本。
///
/// 支持 `动作:锚点名`（click/type），无前缀时按 click 处理；
/// Type 样本一律 `value_masked=true`（不采样消息正文）。
/// 其他动作前缀暂不映射（动作图泛化仅对 Click/Type 有稳定语义）。
pub fn recorded_actions_from_sequence(app_id: &str, sequence: &[String]) -> Vec<RecordedAction> {
    let now = Utc::now().to_rfc3339();
    sequence
        .iter()
        .filter_map(|item| {
            let item = item.trim();
            if item.is_empty() {
                return None;
            }
            let (prefix, anchor_name) = match item.split_once(':') {
                Some((prefix, name)) => (prefix.trim().to_lowercase(), name.trim().to_string()),
                None => ("click".to_string(), item.to_string()),
            };
            if anchor_name.is_empty() {
                return None;
            }
            let action_type = if prefix == "type" {
                ActionType::Type
            } else {
                ActionType::Click
            };
            let role = match (action_type, anchor_name.as_str()) {
                (ActionType::Type, _) => Some("edit".to_string()),
                (ActionType::Click, name) if name.contains("发送") || name.contains("搜索") => {
                    Some("button".to_string())
                }
                (ActionType::Click, name) if name.contains("输入") => Some("edit".to_string()),
                _ => None,
            };
            Some(RecordedAction {
                app_id: app_id.to_string(),
                anchor: SemanticAnchor {
                    app_id: Some(app_id.to_string()),
                    role,
                    name: anchor_name,
                    parent: None,
                    element_id: None,
                },
                action_type,
                value_masked: action_type == ActionType::Type,
                sensitive: false,
                at: now.clone(),
            })
        })
        .collect()
}
