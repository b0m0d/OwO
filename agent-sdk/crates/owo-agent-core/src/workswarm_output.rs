//! Worker 结构化输出契约（V1）：`WorkerOutputV1`。
//!
//! 背景（R1 live 基线 + R3 冒烟）：真实模型 Worker 偶发输出自由文本冒充交付物、
//! 整份非法 JSON（document-structured-extract 曾 0/3）、或 Critic 评审文本被
//! 误选为最终交付物。本契约让每个 Agent Worker 返回**可机器校验**的结构化结果：
//!
//! - Producer 类角色（producer/builder/researcher/writer/leader）必须提交
//!   `artifact { kind, format, content }`——交付物正文只取 `artifact.content`，
//!   不再拿整段自由文本当产物；
//! - Critic **不得**提交 artifact（只提交评审结论，放 `summary`），其产物登记为
//!   `review` 类且永不参与最终交付选择；
//! - 解析失败由调用方执行**恰好一次**定向修复（见执行器 `repair_output_once`），
//!   仍失败以 `output_contract_invalid` 定位上报，禁止无限重试。
//!
//! 兼容性：非契约路径（服务端 echo worker / 未启用契约提示的旧流程）的纯文本
//! 输出走 Legacy 语义，登记行为不变。

use serde::{Deserialize, Serialize};

/// 契约标识（调试/取证字段；解析不强制要求）。
pub const WORKER_OUTPUT_CONTRACT: &str = "worker-output-v1";

/// Worker 终态声明。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOutputStatus {
    /// 正常完成（producer 需附 artifact）。
    Done,
    /// 自身无法完成（原因必须写入 summary/open_issues）。
    Failed,
    /// 被阻塞（等待上游/权限/预算，原因必须写入 summary）。
    Blocked,
}

impl WorkerOutputStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

/// 交付物/产物描述（只允许 Producer 类角色提交）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerArtifactV1 {
    /// 产物分类（document/research/plan/json/csv/…；应与团队目标一致）。
    pub kind: String,
    /// 内容格式：text | markdown | json | csv。
    pub format: String,
    /// 产物正文本体（登记进版本化 Artifact 的就是这段内容）。
    pub content: String,
}

/// 证据引用（研究/审计链；自由文本来源描述 + 可选要点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEvidenceV1 {
    /// 来源描述（文件路径 / URL / 引用名）。
    pub source: String,
    /// 可选说明（摘录、相关性、可信度）。
    #[serde(default)]
    pub note: Option<String>,
}

/// Worker 结构化输出（V1 契约本体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOutputV1 {
    /// 终态声明。
    pub status: WorkerOutputStatus,
    /// 一句话结论（critic 的评审结论 JSON 也放这里）。
    pub summary: String,
    /// 交付物（Producer 类角色必填；critic 禁止提交）。
    #[serde(default)]
    pub artifact: Option<WorkerArtifactV1>,
    /// 证据引用链。
    #[serde(default)]
    pub evidence: Vec<WorkerEvidenceV1>,
    /// 未解决问题 / 风险（如实上报，不做无据通过）。
    #[serde(default)]
    pub open_issues: Vec<String>,
    /// 给下游角色的交接说明（可选）。
    #[serde(default)]
    pub handoff: Option<String>,
}

impl WorkerOutputV1 {
    /// Producer 类角色的合法输出校验：status=done 时必须携带非空 artifact.content。
    pub fn validate(&self) -> Result<(), String> {
        match self.status {
            WorkerOutputStatus::Done => {}
            WorkerOutputStatus::Failed | WorkerOutputStatus::Blocked => {
                if self.summary.trim().is_empty() && self.open_issues.is_empty() {
                    return Err(format!(
                        "status={} 必须在 summary 或 open_issues 中说明原因",
                        self.status.as_str()
                    ));
                }
                return Ok(());
            }
        }
        if self.summary.trim().is_empty() {
            return Err("summary 不能为空".to_string());
        }
        match &self.artifact {
            Some(artifact) => {
                if artifact.content.trim().is_empty() {
                    return Err("artifact.content 不能为空（交付物正文缺失）".to_string());
                }
                if artifact.kind.trim().is_empty() {
                    return Err("artifact.kind 不能为空".to_string());
                }
                let format = artifact.format.trim().to_ascii_lowercase();
                if !matches!(format.as_str(), "text" | "markdown" | "json" | "csv") {
                    return Err(format!(
                        "artifact.format「{format}」不在 text|markdown|json|csv 内"
                    ));
                }
                Ok(())
            }
            None => Err("producer 必须提交 artifact（结构化交付物）".to_string()),
        }
    }

    /// Critic 输出校验：只提交评审结论，禁止携带 artifact。
    pub fn validate_critic(&self) -> Result<(), String> {
        if self.artifact.is_some() {
            return Err(
                "critic 不得提交 artifact（评审结论放 summary；交付物归 producer 链）".to_string(),
            );
        }
        if self.summary.trim().is_empty() {
            return Err("critic summary（评审结论）不能为空".to_string());
        }
        Ok(())
    }
}

/// 契约解析结果三态。
#[derive(Debug, Clone)]
pub enum WorkerOutputParse {
    /// 完全符合契约。
    Parsed(WorkerOutputV1),
    /// 非契约输出（纯文本 legacy 语义，登记行为不变）。
    Legacy,
    /// 看起来想按契约输出但解析/校验失败（触发一次定向修复）。
    Invalid { error: String },
}

/// 解析 worker 原始输出为契约三态。
///
/// 判定规则：JSON 对象且含 `status` 字段 → 走契约（结构解析失败是
/// `Invalid`，可修复）；其余一律 `Legacy`（服务端旧流程不受影响）。
/// **角色规则不在此处判定**——producer 必须带 artifact / critic 禁止带
/// artifact 由调用方按角色调用 [`WorkerOutputV1::validate`] /
/// [`WorkerOutputV1::validate_critic`]（否则 critic 的合法输出会被误判）。
pub fn parse_worker_output(text: &str) -> WorkerOutputParse {
    let trimmed = text.trim();
    // 快速排除：非 `{` 开头不可能是契约 JSON（自由文本直接 Legacy）。
    if !trimmed.starts_with('{') {
        return WorkerOutputParse::Legacy;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // 以 { 开头但非法 JSON：多半是模型想按契约输出但格式坏了。
        return WorkerOutputParse::Invalid {
            error: "输出以 { 开头但不是合法 JSON".to_string(),
        };
    };
    if value.get("status").is_none() {
        // 有结构但不是本契约（其他 JSON 用途）→ Legacy。
        return WorkerOutputParse::Legacy;
    }
    match serde_json::from_value::<WorkerOutputV1>(value) {
        Ok(output) => WorkerOutputParse::Parsed(output),
        Err(error) => WorkerOutputParse::Invalid {
            error: format!("WorkerOutputV1 字段校验失败：{error}"),
        },
    }
}

/// `artifact.format` 合法枚举（九期一路冻结口径：契约提示与修复提示必须逐字传达）。
pub const ARTIFACT_FORMAT_WHITELIST: &str = "text|markdown|json|csv";

/// 契约的系统提示片段（执行器拼进 Worker system prompt；schema 即机器可校验规则）。
pub fn contract_system_prompt(is_critic: bool) -> String {
    let mut prompt = String::from(
        "## 输出契约（必须严格遵守）\n你的最终回复**必须且只能是**一个合法 JSON 对象（WorkerOutputV1），\
不要输出任何解释、代码围栏或契约之外的内容。字段：\n\
{\n  \"status\": \"done\" | \"failed\" | \"blocked\",\n  \"summary\": \"一句话结论\",\n\
  \"artifact\": {\"kind\": \"产物分类\", \"format\": \"text|markdown|json|csv\", \"content\": \"产物正文本体\"},\n\
  \"evidence\": [{\"source\": \"来源\", \"note\": \"说明\"}],\n\
  \"open_issues\": [\"未解决问题\"],\n  \"handoff\": \"给下游的交接说明（可省略）\"\n}\n\
artifact.format 只能取 text|markdown|json|csv 之一（大小写敏感，用小写）。\n",
    );
    if is_critic {
        prompt.push_str(
            "你是评审角色（critic/reviewer）：**禁止**提交最终 Artifact——artifact 字段必须省略；\
把评审结论 JSON {\"approved\":bool,\"score\":0-100,\"comments\":[..]} 放进 summary。\
交付物归 producer 链，评审无权覆盖。\n",
        );
    } else {
        prompt.push_str(
            "你是交付角色：status=done 时**必须**提交 artifact（content=交付物正文本体，\
不是你的过程描述）。代码分析与补丁/变更报告的 artifact.format 一律用 \"markdown\"、\
kind 分别用 \"analysis\"/\"code\"。交付物内容必须**逐字满足**任务指令的字面要求（如指定必须出现的\
映射行、签名行、字段值与文件路径）；若无法完成，用 status=failed/blocked 并在 \
summary/open_issues 说明原因。\n\
回合预算纪律：最后一个回合（只剩 1 次调用时）**禁止再调用任何工具**，必须直接输出完整契约 JSON——\
把已取得的发现写进 artifact.content/summary，宁可内容不完美也不要因超预算失去输出机会。\n",
        );
    }
    prompt
}

/// 剥掉模型输出常见的代码围栏（``` 包裹），只处理首尾成对的一对（七期一路自
/// `product_eval::workswarm_executor` 私有函数移入，成为两路共享工具）。
pub fn strip_code_fences(text: &str) -> String {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // 跳过语言标记行（```json / ```JSON5 等）。
    let body = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => rest,
    };
    body.trim()
        .strip_suffix("```")
        .map(str::trim)
        .map(str::to_string)
        .unwrap_or_else(|| body.trim().to_string())
}

/// 输出契约定向修复提示词（七期一路）：角色规则 + 具体违例原因 + “只输出 JSON 本体”要求。
///
/// 九期（一路）：修复提示必须**逐字传达** `artifact.format` 白名单（text|markdown|json|csv）、
/// 代码分析/补丁报告默认 markdown、critic/reviewer 禁止输出最终 Artifact——八期冒烟中
/// 修复提示只回显了违例原因，模型第二次仍输出白名单外格式（如 "md"/"Markdown"）。
pub fn contract_repair_prompt(is_critic: bool, violation: &str, broken: &str) -> String {
    let role_rule = if is_critic {
        "你是评审角色（critic/reviewer）：禁止提交最终 Artifact——artifact 字段必须省略，把评审结论 JSON 放进 summary"
    } else {
        "你是交付角色：status=done 必须携带 artifact（content=交付物正文本体；artifact.format 只能取 text|markdown|json|csv 之一，代码分析与补丁/变更报告一律用 \"markdown\"，kind 用 \"analysis\"/\"code\"）"
    };
    format!(
        "你的上一次回复不符合 WorkerOutputV1 输出契约（必须是合法 JSON：status/summary/artifact{{kind,format,content}}/evidence/open_issues/handoff）。\
artifact.format 的合法枚举只有 text|markdown|json|csv——不要输出 \"md\"、\"Markdown\"、\"plaintext\" 等白名单外写法；\
评审/审查角色（critic/reviewer）禁止输出最终 Artifact。具体违例：{violation}。{role_rule}。请修正后**只输出**契约合规的 JSON 本体。\n原输出：\n{broken}\n\n请重新输出契约合规的 JSON："
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn producer_json(content: &str) -> String {
        format!(
            r#"{{"status":"done","summary":"完成","artifact":{{"kind":"document","format":"markdown","content":"{content}"}},"evidence":[],"open_issues":[]}}"#
        )
    }

    #[test]
    fn parses_valid_producer_output() {
        let parsed = parse_worker_output(&producer_json("# 交付 正文"));
        match parsed {
            WorkerOutputParse::Parsed(output) => {
                assert_eq!(output.status, WorkerOutputStatus::Done);
                assert_eq!(output.artifact.as_ref().unwrap().format, "markdown");
                assert!(output.validate().is_ok());
            }
            other => panic!("应为 Parsed：{other:?}"),
        }
    }

    #[test]
    fn plain_text_is_legacy() {
        assert!(matches!(
            parse_worker_output("自由文本交付物"),
            WorkerOutputParse::Legacy
        ));
        // 非契约 JSON（其他用途）也是 Legacy。
        assert!(matches!(
            parse_worker_output(r#"{"approved":true,"score":88}"#),
            WorkerOutputParse::Legacy
        ));
    }

    #[test]
    fn broken_json_is_invalid_not_legacy() {
        match parse_worker_output("{\"status\":\"done\",\"summary\":\"x\", artifact:}") {
            WorkerOutputParse::Invalid { error } => assert!(error.contains("合法 JSON")),
            other => panic!("应为 Invalid：{other:?}"),
        }
    }

    #[test]
    fn producer_without_artifact_fails_role_validation() {
        // 结构解析通过（Parsed）；producer 角色规则由 validate() 判定。
        let parsed = parse_worker_output(r#"{"status":"done","summary":"只有文本"}"#);
        match parsed {
            WorkerOutputParse::Parsed(output) => {
                let error = output.validate().unwrap_err();
                assert!(error.contains("artifact"), "{error}");
            }
            other => panic!("应为 Parsed：{other:?}"),
        }
    }

    #[test]
    fn empty_artifact_content_fails_validation() {
        let parsed = parse_worker_output(
            r#"{"status":"done","summary":"x","artifact":{"kind":"document","format":"text","content":"  "}}"#,
        );
        match parsed {
            WorkerOutputParse::Parsed(output) => assert!(output.validate().is_err()),
            other => panic!("应为 Parsed：{other:?}"),
        }
    }

    #[test]
    fn bad_format_fails_validation() {
        let parsed = parse_worker_output(
            r#"{"status":"done","summary":"x","artifact":{"kind":"document","format":"xml","content":"y"}}"#,
        );
        match parsed {
            WorkerOutputParse::Parsed(output) => assert!(output.validate().is_err()),
            other => panic!("应为 Parsed：{other:?}"),
        }
    }

    #[test]
    fn critic_with_artifact_is_rejected() {
        let parsed = parse_worker_output(
            r#"{"status":"done","summary":"评审","artifact":{"kind":"review","format":"text","content":"越权交付"}}"#,
        );
        match parsed {
            WorkerOutputParse::Parsed(output) => {
                let error = output.validate_critic().unwrap_err();
                assert!(error.contains("不得提交 artifact"));
            }
            other => panic!("应为 Parsed：{other:?}"),
        }
    }

    #[test]
    fn failed_status_needs_reason() {
        // 角色规则在 validate()：failed/blocked 必须在 summary 或 open_issues 给出原因。
        let parsed = parse_worker_output(r#"{"status":"failed","summary":"","open_issues":[]}"#);
        match parsed {
            WorkerOutputParse::Parsed(output) => assert!(output.validate().is_err()),
            other => panic!("应为 Parsed：{other:?}"),
        }
        let parsed =
            parse_worker_output(r#"{"status":"failed","summary":"","open_issues":["上游缺失"]}"#);
        match parsed {
            WorkerOutputParse::Parsed(output) => assert!(output.validate().is_ok()),
            other => panic!("应为 Parsed：{other:?}"),
        }
    }

    #[test]
    fn contract_prompt_covers_role_rules() {
        let producer = contract_system_prompt(false);
        assert!(producer.contains("必须") && producer.contains("artifact"));
        let critic = contract_system_prompt(true);
        assert!(critic.contains("禁止") && critic.contains("summary"));
    }

    // 九期（一路）：契约提示与修复提示必须传达 format 白名单/默认值/critic 禁令/
    // 末回合禁工具——八期冒烟失败模式（analyzer 白名单外 format、修复提示未传达
    // 枚举）的回归锁。
    #[test]
    fn system_prompt_states_format_whitelist_and_defaults() {
        let producer = contract_system_prompt(false);
        assert!(producer.contains("text|markdown|json|csv"));
        assert!(
            producer.contains("markdown"),
            "代码分析/补丁报告默认 markdown"
        );
        assert!(producer.contains("最后一个回合"), "末回合禁工具纪律");
        assert!(
            !producer.contains("禁止**提交最终 Artifact"),
            "producer 允许提交 artifact"
        );
        let critic = contract_system_prompt(true);
        assert!(critic.contains("禁止**提交最终 Artifact"));
        assert!(critic.contains("critic/reviewer"));
    }

    #[test]
    fn repair_prompt_states_whitelist_default_and_critic_rule() {
        let producer = contract_repair_prompt(false, "format 不合法", "{\"status\":\"done\"}");
        assert!(producer.contains("text|markdown|json|csv"));
        assert!(producer.contains("markdown"));
        assert!(producer.contains("\"md\""), "点名白名单外常见写法");
        let critic = contract_repair_prompt(true, "携带 artifact", "{\"status\":\"done\"}");
        assert!(critic.contains("禁止提交最终 Artifact"));
        assert!(critic.contains("critic/reviewer"));
    }
}
