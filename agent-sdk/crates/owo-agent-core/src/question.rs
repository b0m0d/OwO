//! 用户提问通道（ask_user 工具）：
//! 模型在信息不足、需求含糊或存在关键分歧时，向用户提问并**挂起回合**等待回答；
//! 通道由承载 UI 的一侧实现（HTTP server 走 SSE 提问卡；CLI/测试无通道时工具会明确报错）。
//! 设计约束（对齐产品要求「拿不准的可以问用户之后再回答」）：
//! · 提问必须最终有结果——用户回答或超时/中止，工具都要返回可读结论，回合不会静默停住；
//! · 无通道环境（子代理/自动化）不阻塞：工具报错，模型改为在最终回复里书面提问。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// 模型向用户提出的一个具体问题。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestion {
    /// 问题 ID（前端按此 ID 提交回答；与审批 request_id 同风格）。
    pub question_id: String,
    /// 要向用户展示的问题正文（简洁、具体、一次只问最关键的一件事）。
    pub question: String,
    /// 可选备选答案：用户可直接点选，减少自由输入成本。
    #[serde(default)]
    pub options: Vec<String>,
}

/// 用户对提问的回答。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub answer: String,
}

/// 提问通道抽象：`ask` 的语义是「展示问题 → 等待用户答复」。
///
/// 返回 `None` 表示通道不可用、超时或回合被中止——调用方（ask_user 工具）会把
/// 这种情况转换成明确的工具结果，让模型基于已有信息继续，而不是让回合挂死。
#[async_trait]
pub trait Questioner: Send + Sync {
    async fn ask(&self, question: &UserQuestion) -> Option<QuestionAnswer>;
}
