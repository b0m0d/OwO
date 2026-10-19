//! 主动建议引擎：阈值/冷却/静默策略与建议裁决（从 learn.rs 拆出）。

use super::*;
use crate::proactive_settings::ProactiveSettings;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProactiveSuggestion {
    pub id: String,
    pub app_id: String,
    pub summary: String,
    pub sequence: Vec<String>,
    pub created_at: String,
    /// 默认仅提示；自动执行需单独开启。
    #[serde(default)]
    pub auto_exec: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionAction {
    Learn,
    #[serde(alias = "execute")]
    ExecuteOnce,
    Ignore,
    #[serde(alias = "mute")]
    MuteForever,
}

struct SequenceRecord {
    app_id: String,
    actions: Vec<String>,
    day: String,
}

pub struct ProactiveEngine {
    settings: ProactiveSettings,
    history: VecDeque<SequenceRecord>,
    suggestions: Vec<ProactiveSuggestion>,
    ignored: HashMap<String, u32>,
    /// 测试直接检查静默表；同域可见即可（不对外暴露）。
    pub(super) muted_until: HashMap<String, String>,
    last_shown: HashMap<String, DateTime<Utc>>,
    shown_today: u32,
    current_day: String,
    suppressed: bool,
}

impl ProactiveEngine {
    pub fn new(settings: ProactiveSettings) -> Self {
        let now = Utc::now();
        Self {
            settings,
            history: VecDeque::new(),
            suggestions: Vec::new(),
            ignored: HashMap::new(),
            muted_until: HashMap::new(),
            last_shown: HashMap::new(),
            shown_today: 0,
            current_day: now.date_naive().to_string(),
            suppressed: false,
        }
    }

    pub fn set_suppressed(&mut self, suppressed: bool) {
        self.suppressed = suppressed;
    }

    /// 运行时应用主动建议设置（设置页即时生效）。
    pub fn apply_settings(&mut self, settings: ProactiveSettings) {
        self.settings = settings;
    }

    fn sequence_key(app_id: &str, actions: &[String]) -> String {
        format!("{app_id}:{}", actions.join("|"))
    }

    fn similar(left: &[String], right: &[String], threshold: f64) -> bool {
        if left == right {
            return true;
        }
        let set_left: std::collections::HashSet<&str> = left.iter().map(String::as_str).collect();
        let set_right: std::collections::HashSet<&str> = right.iter().map(String::as_str).collect();
        let overlap = set_left.intersection(&set_right).count();
        let max = set_left.len().max(set_right.len()).max(1);
        (overlap as f64 / max as f64) >= threshold
    }

    /// 观察一次动作序列；达到阈值时返回建议（仅提示，不执行）。
    pub fn observe(&mut self, app_id: &str, actions: Vec<String>) -> Option<ProactiveSuggestion> {
        let now = Utc::now();
        let day = now.date_naive().to_string();
        if self.current_day != day {
            self.current_day = day.clone();
            self.shown_today = 0;
        }
        let cutoff = (now - ChronoDuration::days(7)).date_naive().to_string();
        self.history
            .retain(|record| record.day.as_str() >= cutoff.as_str());
        self.history.push_back(SequenceRecord {
            app_id: app_id.to_string(),
            actions: actions.clone(),
            day: day.clone(),
        });

        if !self.settings.enabled || self.suppressed || actions.is_empty() {
            return None;
        }
        let key = Self::sequence_key(app_id, &actions);
        if self
            .muted_until
            .get(&key)
            .map(|until| until.as_str() >= day.as_str())
            .unwrap_or(false)
        {
            return None;
        }
        if let Some(last) = self.last_shown.get(&key) {
            if now - *last < ChronoDuration::hours(self.settings.cooldown_hours as i64) {
                return None;
            }
        }
        if self.shown_today >= self.settings.daily_cap {
            return None;
        }

        let same_today = self
            .history
            .iter()
            .filter(|record| {
                record.app_id == app_id
                    && record.day == day
                    && Self::similar(&record.actions, &actions, self.settings.similarity)
            })
            .count();
        let same_week = self
            .history
            .iter()
            .filter(|record| {
                record.app_id == app_id
                    && Self::similar(&record.actions, &actions, self.settings.similarity)
            })
            .count();
        let hit = same_today >= self.settings.daily_threshold as usize
            || same_week >= self.settings.weekly_threshold as usize;
        if !hit {
            return None;
        }

        let suggestion = ProactiveSuggestion {
            id: uuid::Uuid::new_v4().to_string(),
            app_id: app_id.to_string(),
            summary: format!(
                "检测到重复操作（近 7 天 {} 次，今天 {} 次），是否沉淀为技能或下次帮你完成？",
                same_week, same_today
            ),
            sequence: actions.clone(),
            created_at: now.to_rfc3339(),
            auto_exec: false,
        };
        self.last_shown.insert(key.clone(), now);
        self.shown_today += 1;
        self.suggestions.push(suggestion.clone());
        Some(suggestion)
    }

    /// 用户对建议做出选择：忽略 2 次自动静默 30 天；永久静默/学习/执行一次。
    pub fn decide(&mut self, suggestion_id: &str, action: SuggestionAction) -> Result<(), String> {
        let suggestion = self
            .suggestions
            .iter()
            .find(|suggestion| suggestion.id == suggestion_id)
            .cloned()
            .ok_or_else(|| format!("建议不存在：{suggestion_id}"))?;
        let key = Self::sequence_key(&suggestion.app_id, &suggestion.sequence);
        let now = Utc::now();
        match action {
            SuggestionAction::Ignore => {
                let count = self.ignored.entry(key.clone()).or_default();
                *count += 1;
                if *count >= 2 {
                    self.muted_until.insert(
                        key,
                        (now + ChronoDuration::days(self.settings.auto_silence_days as i64))
                            .date_naive()
                            .to_string(),
                    );
                }
            }
            SuggestionAction::MuteForever => {
                self.muted_until.insert(key, "9999-12-31".to_string());
            }
            SuggestionAction::Learn | SuggestionAction::ExecuteOnce => {
                // 学习交给 LearnRecorder/流程技能包流程；执行仍需审批。
                self.last_shown.remove(&key);
                if action == SuggestionAction::Learn {
                    // 已确认沉淀：从建议列表移除，避免重复学习。
                    self.suggestions
                        .retain(|suggestion| suggestion.id != suggestion_id);
                }
            }
        }
        Ok(())
    }

    pub fn suggestions(&self) -> &[ProactiveSuggestion] {
        &self.suggestions
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }
}
