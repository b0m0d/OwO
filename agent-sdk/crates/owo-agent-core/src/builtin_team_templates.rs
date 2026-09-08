//! 内置团队模板目录（六期 · 第三路）：四类稳定团队方案（候选，不自动启用）。
//!
//! 产品背景：五期 live 基线（`docs/reports/v1-r1-live-baseline.md`）证明模型临时
//! 编队不稳定（WorkSwarm 50% vs 单 Agent 80%）。本模块把代码、研究、文档、结构化
//! 信息处理做成**固定角色 + 固定 DAG + 固定预算 + 明确完成条件**的模板，减少临时
//! 编队造成的不确定性。
//!
//! 目录语义（与既有「提案 → 用户采纳」机制互补）：
//! - 本文件只是**候选目录**：模板未安装前不进入 `TeamTemplateRegistry`，
//!   `find_match`（自动匹配）不会命中——「自动模式只匹配已安装模板」由注册表
//!   机制天然保证（注册表只包含已安装/已采纳模板）。
//! - 安装动作在 server 侧 `team_template_catalog_api`（用户手动触发、幂等、
//!   只写 `templates/{id}.json`，不触碰任何权限/设置面——不自动扩大文件、
//!   命令或网络权限）。
//! - 预算语义：`budget` 为 GoalBudget 兼容键（max_steps / max_retries_per_step /
//!   max_total_retries / max_replans）+ 五期 additive 键（max_wall_secs /
//!   max_cost_usd）；建队时由调用方随 POST /teams 的 budget 传入（launcher/UI
//!   按目录值预填，用户可显式覆盖）。
//! - 角色调用预算（`budget_calls_per_role`）为 AgentConfig.max_turns 口径的建议值。

use owo_agent_protocol::{TeamMode, TeamTemplate, TeamTemplateRole};
use serde::Serialize;
use serde_json::json;

/// 模板作者固定日期（确定性；目录内置定义不随安装时间漂移）。
const AUTHORED_AT: &str = "2026-08-28T00:00:00+00:00";

// ---------------------------------------------------------------------------
// 目录条目
// ---------------------------------------------------------------------------

/// 角色调用预算（目录展示 + 建队预览）。
#[derive(Debug, Clone, Serialize)]
pub struct RoleBudget {
    pub role: String,
    pub budget_calls: usize,
}

/// 内置模板目录条目：`TeamTemplate` 本体 + 目录级元数据
/// （预算 / 完成条件 / 工具范围 / 产物类型）。安装只持久化 `template` 本体。
#[derive(Debug, Clone, Serialize)]
pub struct BuiltinTemplateDescriptor {
    pub template: TeamTemplate,
    /// 模板产出物类型（进 UI 预览；与 Artifact kind 语义对齐）。
    pub artifact_kinds: Vec<String>,
    /// 每角色调用预算（建议值）。
    pub budget_calls_per_role: Vec<RoleBudget>,
    /// 团队预算（GoalBudget 兼容 + additive；建队时随 budget 传入）。
    pub budget: serde_json::Value,
    /// 完成条件（结构化描述；进 UI 预览）。
    pub completion_criteria: Vec<String>,
    /// 工具范围口径（自然语言；安装不自动扩权，实际权限仍走审批策略）。
    pub tool_scope: String,
}

// ---------------------------------------------------------------------------
// 四类内置模板（固定 id：UI/模板选择直接引用）
// ---------------------------------------------------------------------------

/// 代码变更：代码分析 → 单写者实现 → 只读审查。
pub const CODE_CHANGE_V1: &str = "code-change-v1";
/// 研究简报：并行研究 → 证据核验 → 汇总交付。
pub const RESEARCH_BRIEF_V1: &str = "research-brief-v1";
/// 文档交付：文档起草 → 内容审查 → 最终版本。
pub const DOCUMENT_DELIVERY_V1: &str = "document-delivery-v1";
/// 结构化抽取：结构化抽取 → Schema 校验 → JSON/CSV Artifact。
pub const STRUCTURED_EXTRACT_V1: &str = "structured-extract-v1";

/// 目录固定顺序（UI 展示序）。
pub const CATALOG_IDS: [&str; 4] = [
    CODE_CHANGE_V1,
    RESEARCH_BRIEF_V1,
    DOCUMENT_DELIVERY_V1,
    STRUCTURED_EXTRACT_V1,
];

fn role(role: &str, depends_on: &[&str], handoff_contract: &str, verify: &str) -> TeamTemplateRole {
    TeamTemplateRole {
        role: role.to_string(),
        assignee: "agent".to_string(),
        worker: None, // agent 角色缺省模型驱动（create_team_run 归一为 "agent"）
        depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        handoff_contract: Some(handoff_contract.to_string()),
        verify: Some(verify.to_string()),
    }
}

/// 代码变更模板：分析（只读）→ 单写者实现 → 只读审查。
/// Critic（reviewer）只提交评审结论，不得覆盖 implementer 交付物。
fn code_change_v1() -> BuiltinTemplateDescriptor {
    let template = TeamTemplate {
        template_id: CODE_CHANGE_V1.to_string(),
        name: "代码变更（分析 → 单写者实现 → 只读审查）".to_string(),
        mode: TeamMode::Team,
        roles: vec![
            role(
                "code_analyzer",
                &[],
                "只读分析：定位目标代码、梳理影响面与风险清单，不改任何代码；输出分析报告。",
                "non_empty",
            ),
            role(
                "implementer",
                &["code_analyzer"],
                "单写者：全队唯一允许产出代码改动的角色；依据分析报告输出完整变更（diff 或全文）与变更说明。",
                "non_empty",
            ),
            role(
                "reviewer",
                &["implementer"],
                "只读审查：核对变更与需求/影响面一致、无越界修改；输出评审结论（不改写交付物、不产出代码）。",
                "non_empty",
            ),
        ],
        applicability: "代码 修改 修复 重构 bug 实现 feature patch 变更 函数 接口".to_string(),
        source_team_id: None,
        created_at: AUTHORED_AT.to_string(),
    };
    BuiltinTemplateDescriptor {
        budget_calls_per_role: vec![
            RoleBudget {
                role: "code_analyzer".to_string(),
                // 九期（一路）：3→4——冒烟实测 3 回合下 analyzer 稳定在预算上溢出
                //（探索调用点后没有剩余回合输出契约 JSON）；4 = 3 回合探索 + 1 回合
                //纯输出（末回合禁工具，见 team_prompt 预算段）。
                budget_calls: 4,
            },
            RoleBudget {
                role: "implementer".to_string(),
                budget_calls: 5,
            },
            RoleBudget {
                role: "reviewer".to_string(),
                budget_calls: 3,
            },
        ],
        budget: json!({
            "max_steps": 6,
            "max_retries_per_step": 2,
            "max_total_retries": 4,
            "max_replans": 1,
            "max_wall_secs": 1800
        }),
        artifact_kinds: vec![
            "analysis".to_string(),
            "code".to_string(),
            "review".to_string(),
        ],
        completion_criteria: vec![
            "code_analyzer 产出影响面与风险清单".to_string(),
            "implementer 交付完整变更且为全队唯一代码写者".to_string(),
            "reviewer 输出评审结论（只读，不覆盖交付物）".to_string(),
        ],
        tool_scope: "code_analyzer/reviewer 只读（读文件+搜索）；implementer 需写权限（经审批）"
            .to_string(),
        template,
    }
}

/// 研究简报模板：并行研究（双视角）→ 证据核验 → 汇总交付。
/// 交付必须带来源：每条结论附引用（URL/文件路径），无源结论在核验环节剔除。
fn research_brief_v1() -> BuiltinTemplateDescriptor {
    let template = TeamTemplate {
        template_id: RESEARCH_BRIEF_V1.to_string(),
        name: "研究简报（并行研究 → 证据核验 → 汇总交付）".to_string(),
        mode: TeamMode::Team,
        roles: vec![
            role(
                "researcher_a",
                &[],
                "并行研究（主视角）：围绕目标检索与阅读，输出带来源（URL/文件路径）的证据列表，每条证据标注来源。",
                "non_empty",
            ),
            role(
                "researcher_b",
                &[],
                "并行研究（反例视角）：从对立/边界视角检索反例与限制条件，输出带来源的证据列表。",
                "non_empty",
            ),
            role(
                "evidence_verifier",
                &["researcher_a", "researcher_b"],
                "证据核验：逐条核对两路证据的来源存在性与相关性，剔除无源/弱源结论，输出核验后的合并证据列表。",
                "non_empty",
            ),
            role(
                "brief_writer",
                &["evidence_verifier"],
                "汇总交付：产出研究简报；每条结论必须附来源引用，无来源的判断不得进入结论。",
                "non_empty",
            ),
        ],
        applicability: "研究 调研 对比 评估 选型 分析 证据 综述 背景".to_string(),
        source_team_id: None,
        created_at: AUTHORED_AT.to_string(),
    };
    BuiltinTemplateDescriptor {
        budget_calls_per_role: vec![
            RoleBudget {
                role: "researcher_a".to_string(),
                budget_calls: 4,
            },
            RoleBudget {
                role: "researcher_b".to_string(),
                budget_calls: 4,
            },
            RoleBudget {
                role: "evidence_verifier".to_string(),
                budget_calls: 3,
            },
            RoleBudget {
                role: "brief_writer".to_string(),
                budget_calls: 4,
            },
        ],
        budget: json!({
            "max_steps": 8,
            "max_retries_per_step": 2,
            "max_total_retries": 4,
            "max_replans": 1,
            "max_wall_secs": 2400
        }),
        artifact_kinds: vec![
            "research".to_string(),
            "evidence".to_string(),
            "brief".to_string(),
        ],
        completion_criteria: vec![
            "双路研究并行完成且各自带来源".to_string(),
            "evidence_verifier 输出核验后的证据列表（无源结论被剔除）".to_string(),
            "简报每条结论附来源引用".to_string(),
        ],
        tool_scope: "只读（搜索+读文件/网页）；全角色无写权限".to_string(),
        template,
    }
}

/// 文档交付模板：起草 → 只读内容审查 → 最终版本（终稿唯一写者）。
fn document_delivery_v1() -> BuiltinTemplateDescriptor {
    let template = TeamTemplate {
        template_id: DOCUMENT_DELIVERY_V1.to_string(),
        name: "文档交付（起草 → 内容审查 → 最终版本）".to_string(),
        mode: TeamMode::Team,
        roles: vec![
            role(
                "drafter",
                &[],
                "起草：产出完整文档初稿（结构完整、覆盖目标要点）。",
                "non_empty",
            ),
            role(
                "content_reviewer",
                &["drafter"],
                "只读内容审查：检查结构/一致性/事实性/覆盖度，输出逐条修订意见；不改写原文、不产出新稿。",
                "non_empty",
            ),
            role(
                "finalizer",
                &["content_reviewer"],
                "最终版本唯一写者：吸收审查意见产出终稿（结构化 Markdown），并附修订说明。",
                "non_empty",
            ),
        ],
        applicability: "文档 方案 说明 教程 指南 报告 综述 撰写 起草 编写".to_string(),
        source_team_id: None,
        created_at: AUTHORED_AT.to_string(),
    };
    BuiltinTemplateDescriptor {
        budget_calls_per_role: vec![
            RoleBudget {
                role: "drafter".to_string(),
                budget_calls: 4,
            },
            RoleBudget {
                role: "content_reviewer".to_string(),
                budget_calls: 3,
            },
            RoleBudget {
                role: "finalizer".to_string(),
                budget_calls: 4,
            },
        ],
        budget: json!({
            "max_steps": 6,
            "max_retries_per_step": 2,
            "max_total_retries": 4,
            "max_replans": 1,
            "max_wall_secs": 1800
        }),
        artifact_kinds: vec![
            "draft".to_string(),
            "review".to_string(),
            "document".to_string(),
        ],
        completion_criteria: vec![
            "初稿完整覆盖目标要点".to_string(),
            "content_reviewer 输出逐条修订意见（只读）".to_string(),
            "终稿为最终版本且附修订说明".to_string(),
        ],
        tool_scope: "drafter/finalizer 需写权限（经审批）；content_reviewer 只读".to_string(),
        template,
    }
}

/// 结构化抽取模板：抽取（严格 JSON）→ Schema 校验 → JSON/CSV Artifact。
/// 交付必须是机器可解析的 JSON/CSV（校验失败列出具体违规，禁止自由文本冒充）。
fn structured_extract_v1() -> BuiltinTemplateDescriptor {
    let template = TeamTemplate {
        template_id: STRUCTURED_EXTRACT_V1.to_string(),
        name: "结构化抽取（抽取 → Schema 校验 → JSON/CSV Artifact）".to_string(),
        mode: TeamMode::Team,
        roles: vec![
            role(
                "extractor",
                &[],
                "结构化抽取：严格按目标 schema 输出 JSON（裸 JSON，无围栏/无解释文本）。",
                "non_empty",
            ),
            role(
                "schema_validator",
                &["extractor"],
                "Schema 校验：验证 JSON 可解析且符合目标 schema；失败时输出 JSON，逐条列出违规字段、期望与修复建议，不自行改写数据。",
                "non_empty",
            ),
            role(
                "artifact_formatter",
                &["schema_validator"],
                "产物格式化：把校验通过的数据输出为 JSON 或 CSV Artifact（结构确定、可直接机器消费；二选一并在输出首行标注格式）。",
                "non_empty",
            ),
        ],
        applicability: "抽取 提取 结构化 json csv 表格 字段 schema 解析 清洗".to_string(),
        source_team_id: None,
        created_at: AUTHORED_AT.to_string(),
    };
    BuiltinTemplateDescriptor {
        budget_calls_per_role: vec![
            RoleBudget {
                role: "extractor".to_string(),
                budget_calls: 4,
            },
            RoleBudget {
                role: "schema_validator".to_string(),
                budget_calls: 3,
            },
            RoleBudget {
                role: "artifact_formatter".to_string(),
                budget_calls: 3,
            },
        ],
        budget: json!({
            "max_steps": 6,
            "max_retries_per_step": 2,
            "max_total_retries": 4,
            "max_replans": 1,
            "max_wall_secs": 1200
        }),
        artifact_kinds: vec![
            "extraction".to_string(),
            "validation".to_string(),
            "json_csv_artifact".to_string(),
        ],
        completion_criteria: vec![
            "extractor 输出裸 JSON（无围栏/解释文本）".to_string(),
            "schema_validator 确认可解析且符合目标 schema（失败输出结构化违规清单）".to_string(),
            "最终 Artifact 为 JSON 或 CSV 且机器可解析".to_string(),
        ],
        tool_scope: "只读（读文件/搜索）；全角色无写权限（产物经 Artifact ref 交付）".to_string(),
        template,
    }
}

// ---------------------------------------------------------------------------
// 目录访问
// ---------------------------------------------------------------------------

/// 全量目录（固定展示序：code → research → document → structured）。
pub fn catalog() -> Vec<BuiltinTemplateDescriptor> {
    vec![
        code_change_v1(),
        research_brief_v1(),
        document_delivery_v1(),
        structured_extract_v1(),
    ]
}

/// 按 id 取内置模板条目（未知 id → None）。
pub fn descriptor(template_id: &str) -> Option<BuiltinTemplateDescriptor> {
    match template_id {
        CODE_CHANGE_V1 => Some(code_change_v1()),
        RESEARCH_BRIEF_V1 => Some(research_brief_v1()),
        DOCUMENT_DELIVERY_V1 => Some(document_delivery_v1()),
        STRUCTURED_EXTRACT_V1 => Some(structured_extract_v1()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 角色专属 Prompt 段（八期 · 第一路）
// ---------------------------------------------------------------------------

/// 单角色的结构化 Prompt 段（八期一路）：由 [`prompt_sections_for`] 按
/// 模板 + 角色取用，`TeamPromptCompiler` 据此生成角色专属 Prompt
/// （不再让所有角色共用同一段通用提示）。
#[derive(Debug, Clone, Serialize)]
pub struct RolePromptSections {
    /// 必须完成的工作（逐条）。
    pub must_do: Vec<String>,
    /// 禁止执行的工作（逐条；与 WorkerProfile 工具面护栏叠加）。
    pub must_not_do: Vec<String>,
    /// 输出格式（契约口径）。
    pub output_format: String,
    /// 验收条件（逐条；Prompt 侧口径，引擎侧 verify 仍独立强制）。
    pub acceptance: Vec<String>,
}

/// 模板 + 角色 → 角色专属 Prompt 段（未知组合 → None，编译器回退交接契约）。
///
/// 内容与各模板角色的 `handoff_contract`/`tool_scope`/`artifact_kinds` 同口径，
/// 但按「必须做 / 禁止做 / 输出格式 / 验收」结构化——通用契约给引擎，
/// 结构化段给模型。
pub fn prompt_sections_for(template_id: &str, role: &str) -> Option<RolePromptSections> {
    let sections = match (template_id, role) {
        (CODE_CHANGE_V1, "code_analyzer") => RolePromptSections {
            must_do: vec![
                "只读定位目标代码与调用点，梳理影响面（受影响的函数/调用方/测试）。".to_string(),
                "输出风险清单：每项风险标注代码位置与建议的验证方式。".to_string(),
            ],
            must_not_do: vec![
                "不修改任何文件（你是只读分析角色）。".to_string(),
                "不产出代码变更（实现由 implementer 承担）。".to_string(),
            ],
            // 九期（一路）：明确 artifact.format 枚举与默认值——冒烟中 analyzer 曾
            // 输出白名单外格式且修复提示未传达枚举，导致契约失败。
            output_format: "分析报告：artifact.kind=\"analysis\"、artifact.format=\"markdown\"（代码分析报告默认 markdown；format 只能取 text|markdown|json|csv），artifact.content 为报告正文（影响面清单 + 风险清单 + 建议实现要点）。".to_string(),
            acceptance: vec![
                "影响面覆盖目标代码及其直接调用方。".to_string(),
                "每项风险均有位置与建议验证方式。".to_string(),
            ],
        },
        (CODE_CHANGE_V1, "implementer") => RolePromptSections {
            must_do: vec![
                "依据上游分析产出完整变更：你是全队唯一写者，用 write_file 把最终文件内容真实写入允许路径内的文件。".to_string(),
                "artifact.content 只写变更说明、影响面与验证方式（不要把完整变更只放在 artifact 里而不落盘）。".to_string(),
            ],
            must_not_do: vec![
                "不触碰允许写路径之外的文件。".to_string(),
                "不改写 reviewer 的评审结论（评审独立）。".to_string(),
            ],
            output_format: "artifact.kind=\"code\"、artifact.format=\"markdown\"（补丁/变更报告默认 markdown；format 只能取 text|markdown|json|csv），artifact.content = 变更说明；工作区文件 = 真实落盘后的最终内容。".to_string(),
            acceptance: vec![
                "工作区存在真实落盘的变更（git 可见）。".to_string(),
                "变更与上游分析的影响面一致。".to_string(),
                "无越界文件改动。".to_string(),
            ],
        },
        (CODE_CHANGE_V1, "reviewer") => RolePromptSections {
            must_do: vec![
                "核对工作区真实变更与目标/上游影响面是否一致。".to_string(),
                "输出评审结论：approved/score/comments，逐条指出问题位置。".to_string(),
            ],
            must_not_do: vec![
                "只读：不改写交付物、不产出代码。".to_string(),
                "不直接重新实现（发现问题写进评审结论，不动手改）。".to_string(),
                "不得提交交付物正文（变更/终稿归 producer 链）：你的 artifact.kind=\"review\"、artifact.format=\"markdown\"，artifact.content 只放评审结论。".to_string(),
            ],
            output_format: "契约 JSON：status=done + artifact{kind=\"review\", format=\"markdown\", content=评审结论 JSON（{\"approved\":bool,\"score\":0-100,\"comments\":[..]}）}；kind=review 的产物不会参与最终交付选择。".to_string(),
            acceptance: vec![
                "结论覆盖「与目标一致」与「无越界修改」两个维度。".to_string(),
                "每条意见可定位到文件/函数级。".to_string(),
            ],
        },
        (RESEARCH_BRIEF_V1, "researcher_a") => RolePromptSections {
            must_do: vec![
                "围绕目标检索与阅读（主视角），输出带来源（URL/文件路径）的证据列表。".to_string(),
                "每条证据标注来源与可信度说明。".to_string(),
            ],
            must_not_do: vec![
                "不写工作区文件（研究族只读 + 浏览器）。".to_string(),
                "不编造来源：无来源的判断不得作为证据输出。".to_string(),
            ],
            output_format: "证据列表（逐条：结论 + 来源 + 简注）。".to_string(),
            acceptance: vec![
                "证据覆盖目标的主要方面。".to_string(),
                "每条证据都有可核验来源。".to_string(),
            ],
        },
        (RESEARCH_BRIEF_V1, "researcher_b") => RolePromptSections {
            must_do: vec![
                "从对立/边界视角检索反例与限制条件，输出带来源的证据列表。".to_string(),
                "明确指出主视角结论可能不成立的条件。".to_string(),
            ],
            must_not_do: vec![
                "不写工作区文件（研究族只读 + 浏览器）。".to_string(),
                "不编造来源：无来源的判断不得作为证据输出。".to_string(),
            ],
            output_format: "证据列表（逐条：反例/限制 + 来源 + 简注）。".to_string(),
            acceptance: vec![
                "至少覆盖一个对立视角或适用边界。".to_string(),
                "每条证据都有可核验来源。".to_string(),
            ],
        },
        (RESEARCH_BRIEF_V1, "evidence_verifier") => RolePromptSections {
            must_do: vec![
                "逐条核对两路证据的来源存在性与相关性。".to_string(),
                "剔除无源/弱源结论，输出核验后的合并证据列表。".to_string(),
            ],
            must_not_do: vec![
                "不自行补充新证据（只核验上游证据）。".to_string(),
                "不改写研究结论的实质内容（剔除需给理由）。".to_string(),
            ],
            output_format: "核验后的合并证据列表（保留/剔除 + 理由）。".to_string(),
            acceptance: vec![
                "每条保留证据都通过来源核对。".to_string(),
                "剔除项均有明确理由。".to_string(),
            ],
        },
        (RESEARCH_BRIEF_V1, "brief_writer") => RolePromptSections {
            must_do: vec![
                "汇总上游核验证据产出研究简报。".to_string(),
                "每条结论附来源引用；被核验剔除的判断不得进入结论。".to_string(),
            ],
            must_not_do: vec![
                "不写工作区文件（研究族只读）。".to_string(),
                "不引入上游不存在的「新事实」。".to_string(),
            ],
            output_format: "研究简报（Markdown）：结论 + 来源引用列表。".to_string(),
            acceptance: vec![
                "简报每条结论附来源引用。".to_string(),
                "结论与核验后的证据一致。".to_string(),
            ],
        },
        (DOCUMENT_DELIVERY_V1, "drafter") => RolePromptSections {
            must_do: vec![
                "产出完整文档初稿（结构完整、覆盖目标要点）。".to_string(),
                "对不确定的事实显式标注，不冒充确定。".to_string(),
            ],
            must_not_do: vec![
                "不越过初稿定位去吸收（尚不存在的）评审意见。".to_string(),
            ],
            output_format: "结构化 Markdown 初稿。".to_string(),
            acceptance: vec![
                "覆盖目标列出的全部要点。".to_string(),
                "章节结构完整可评审。".to_string(),
            ],
        },
        (DOCUMENT_DELIVERY_V1, "content_reviewer") => RolePromptSections {
            must_do: vec![
                "检查结构/一致性/事实性/覆盖度，输出逐条修订意见。".to_string(),
                "每条意见给出位置与可执行的修改建议。".to_string(),
            ],
            must_not_do: vec![
                "只读：不改写原文、不产出新稿。".to_string(),
                "不得提交文档正文（终稿归 finalizer）：你的 artifact.kind=\"review\"、artifact.format=\"markdown\"，artifact.content 只放修订意见。".to_string(),
            ],
            output_format: "契约 JSON：status=done + artifact{kind=\"review\", format=\"markdown\", content=修订意见列表（逐条：位置 + 问题 + 建议）}；kind=review 的产物不会参与最终交付选择。".to_string(),
            acceptance: vec![
                "意见覆盖结构与事实两类。".to_string(),
                "每条意见可执行（不是泛泛评价）。".to_string(),
            ],
        },
        (DOCUMENT_DELIVERY_V1, "finalizer") => RolePromptSections {
            must_do: vec![
                "吸收审查意见产出终稿（结构化 Markdown），用 write_file 落盘。".to_string(),
                "附修订说明：采纳/驳回逐条意见的理由。".to_string(),
            ],
            must_not_do: vec![
                "不忽略未处理的意见（驳回需给理由）。".to_string(),
            ],
            output_format: "契约 JSON 的 artifact.content = 终稿 + 修订说明；工作区文件 = 终稿落盘内容。".to_string(),
            acceptance: vec![
                "终稿为完整最终版本且附修订说明。".to_string(),
                "全部意见有采纳/驳回去向。".to_string(),
            ],
        },
        (STRUCTURED_EXTRACT_V1, "extractor") => RolePromptSections {
            must_do: vec![
                "严格按目标 schema 输出 JSON（裸 JSON，无围栏/无解释文本）。".to_string(),
                "字段缺失用 null 并在 open_issues 中说明，不猜测。".to_string(),
            ],
            must_not_do: vec![
                "不输出 Markdown 围栏或解释性文字。".to_string(),
                "不虚构源数据中不存在的字段值。".to_string(),
            ],
            output_format: "裸 JSON（可直接 serde 解析）。".to_string(),
            acceptance: vec![
                "输出可解析为 JSON 对象。".to_string(),
                "键与目标 schema 一致。".to_string(),
            ],
        },
        (STRUCTURED_EXTRACT_V1, "schema_validator") => RolePromptSections {
            must_do: vec![
                "验证 JSON 可解析且符合目标 schema；失败时逐条列出违规字段、期望与修复建议。".to_string(),
                "输出结构化校验结论（通过/违规清单）。".to_string(),
            ],
            must_not_do: vec![
                "不自行改写数据（修复建议留给 formatter）。".to_string(),
            ],
            output_format: "结构化校验结论（JSON：valid + violations[]）。".to_string(),
            acceptance: vec![
                "覆盖可解析性与 schema 符合性两层。".to_string(),
                "违规项可定位到字段级。".to_string(),
            ],
        },
        (STRUCTURED_EXTRACT_V1, "artifact_formatter") => RolePromptSections {
            must_do: vec![
                "把校验通过的数据输出为 JSON 或 CSV Artifact（二选一并在输出首行标注格式）。".to_string(),
                "结构确定、可直接机器消费。".to_string(),
            ],
            must_not_do: vec![
                "不改变数据的实质内容（只做格式化）。".to_string(),
            ],
            output_format: "首行标注格式（json/csv），随后为对应格式的机器可解析正文。".to_string(),
            acceptance: vec![
                "输出可被对应格式解析器直接解析。".to_string(),
                "数据内容与校验通过的输入一致。".to_string(),
            ],
        },
        _ => return None,
    };
    Some(sections)
}

/// 目录条目的自动匹配关键词（与 `applicability` 同一口径切分；进 UI 预览）。
pub fn auto_match_keywords(applicability: &str) -> Vec<String> {
    crate::team_strategy::applicability_tokens(applicability)
}

// ---------------------------------------------------------------------------
// 模块内单测（HTTP 面见 server tests/team_template_catalog_api_tests.rs）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_has_four_stable_templates() {
        let entries = catalog();
        assert_eq!(entries.len(), 4);
        let ids: Vec<&str> = entries
            .iter()
            .map(|d| d.template.template_id.as_str())
            .collect();
        assert_eq!(ids, CATALOG_IDS.to_vec());
        assert!(descriptor("no-such-template").is_none());
        for id in CATALOG_IDS {
            assert!(descriptor(id).is_some(), "{id} 应可按 id 取回");
        }
    }

    #[test]
    fn descriptors_are_structurally_valid() {
        for d in catalog() {
            let roles = &d.template.roles;
            assert!(!roles.is_empty(), "{} 角色非空", d.template.template_id);
            // 角色唯一。
            let mut seen = HashSet::new();
            for r in roles {
                assert!(
                    seen.insert(r.role.clone()),
                    "{} 角色重复：{}",
                    d.template.template_id,
                    r.role
                );
            }
            // DAG：依赖必须存在且无环（拓扑排序可完成）。
            let ids: HashSet<&str> = roles.iter().map(|r| r.role.as_str()).collect();
            for r in roles {
                for dep in &r.depends_on {
                    assert!(
                        ids.contains(dep.as_str()),
                        "{} 依赖缺失：{} → {}",
                        d.template.template_id,
                        r.role,
                        dep
                    );
                }
            }
            let mut resolved: HashSet<String> = HashSet::new();
            let mut progress = true;
            while progress && resolved.len() < roles.len() {
                progress = false;
                for r in roles {
                    if resolved.contains(&r.role) {
                        continue;
                    }
                    if r.depends_on.iter().all(|dep| resolved.contains(dep)) {
                        resolved.insert(r.role.clone());
                        progress = true;
                    }
                }
            }
            assert_eq!(
                resolved.len(),
                roles.len(),
                "{} 角色图存在环",
                d.template.template_id
            );
            // 元数据完整。
            assert!(!d.artifact_kinds.is_empty());
            assert!(!d.completion_criteria.is_empty());
            assert!(!d.tool_scope.is_empty());
            assert_eq!(d.budget_calls_per_role.len(), roles.len(), "每角色都有预算");
            assert!(
                d.budget["max_steps"].as_u64().unwrap_or(0) >= roles.len() as u64,
                "步数预算 ≥ 角色数"
            );
            assert!(
                d.budget["max_wall_secs"].as_u64().unwrap_or(0) > 0,
                "墙钟预算生效（>0）"
            );
            // 模板模式与验证断言。
            assert_eq!(d.template.mode, TeamMode::Team);
            for r in roles {
                assert_eq!(r.assignee, "agent");
                assert!(r.worker.is_none(), "内置模板角色缺省 agent 驱动");
                assert!(
                    r.verify.as_deref().is_some_and(|v| !v.is_empty()),
                    "完成条件须带验证断言"
                );
                assert!(
                    r.handoff_contract.as_deref().is_some_and(|c| !c.is_empty()),
                    "交接契约须非空"
                );
            }
        }
    }

    #[test]
    fn template_specific_semantics_hold() {
        // 研究：双路并行（researcher_a/b 无依赖）→ 汇总前必有核验。
        let research = descriptor(RESEARCH_BRIEF_V1).unwrap();
        let parallel: Vec<&TeamTemplateRole> = research
            .template
            .roles
            .iter()
            .filter(|r| r.depends_on.is_empty())
            .collect();
        assert!(parallel.len() >= 2, "研究模板应并行研究：{parallel:?}");
        assert!(
            research
                .template
                .roles
                .iter()
                .any(|r| r.role == "evidence_verifier"),
            "必须有证据核验角色"
        );
        assert!(
            research.template.roles.iter().all(|r| r
                .handoff_contract
                .as_deref()
                .unwrap_or("")
                .contains("来源")),
            "研究模板交接契约必须要求带来源"
        );

        // 结构化：交付必须是机器可解析 JSON/CSV。
        let structured = descriptor(STRUCTURED_EXTRACT_V1).unwrap();
        let formatter = structured
            .template
            .roles
            .iter()
            .find(|r| r.role == "artifact_formatter")
            .unwrap();
        let contract = formatter.handoff_contract.as_deref().unwrap_or("");
        assert!(contract.contains("JSON"), "{contract}");
        assert!(contract.contains("CSV"), "{contract}");

        // 代码：单写者语义（唯一 implementer 产出改动；审查只读）。
        let code = descriptor(CODE_CHANGE_V1).unwrap();
        let reviewer = code
            .template
            .roles
            .iter()
            .find(|r| r.role == "reviewer")
            .unwrap();
        let contract = reviewer.handoff_contract.as_deref().unwrap_or("");
        assert!(contract.contains("只读"), "{contract}");
        assert!(
            contract.contains("不改写交付物"),
            "审查者不得改写交付物（critic 与 producer 分离）：{contract}"
        );
        assert!(
            !contract.contains("唯一允许产出代码"),
            "唯一写者必须是 implementer 而非审查者：{contract}"
        );
    }

    #[test]
    fn registry_persistence_roundtrip_preserves_dag_and_contracts() {
        let dir = std::env::temp_dir().join(format!(
            "owo-builtin-tpl-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let registry = crate::workswarm::TeamTemplateRegistry::new(dir.clone());
        for d in catalog() {
            registry.save_template(&d.template).unwrap();
        }
        let listed = registry.list_templates();
        assert_eq!(listed.len(), 4, "安装后注册表应有 4 个模板：{listed:?}");
        for original in catalog() {
            let stored = registry
                .get_template(&original.template.template_id)
                .expect("安装后应可按 id 取回");
            assert_eq!(stored.roles.len(), original.template.roles.len());
            for (a, b) in stored.roles.iter().zip(original.template.roles.iter()) {
                assert_eq!(a.role, b.role);
                assert_eq!(a.depends_on, b.depends_on, "依赖（DAG）应原样持久化");
                assert_eq!(a.handoff_contract, b.handoff_contract);
                assert_eq!(a.verify, b.verify);
            }
            assert_eq!(stored.applicability, original.template.applicability);
            assert_eq!(stored.mode, original.template.mode);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_match_keywords_split_consistently() {
        let research = descriptor(RESEARCH_BRIEF_V1).unwrap();
        let keywords = auto_match_keywords(&research.template.applicability);
        assert!(
            keywords.iter().any(|k| k == "研究"),
            "关键词应包含「研究」：{keywords:?}"
        );
        assert!(
            keywords.iter().all(|k| k.chars().count() >= 2),
            "关键词长度 ≥2：{keywords:?}"
        );
    }

    #[test]
    fn every_catalog_role_has_prompt_sections() {
        // 四个内置模板的每个角色都必须有角色专属 Prompt 段（八期一路完工判据）。
        for d in catalog() {
            for r in &d.template.roles {
                let s = prompt_sections_for(&d.template.template_id, &r.role)
                    .unwrap_or_else(|| panic!("{} 缺角色段：{}", d.template.template_id, r.role));
                assert!(
                    !s.must_do.is_empty(),
                    "{}.{} must_do 空",
                    d.template.template_id,
                    r.role
                );
                assert!(
                    !s.must_not_do.is_empty(),
                    "{}.{} must_not_do 空",
                    d.template.template_id,
                    r.role
                );
                assert!(
                    !s.output_format.trim().is_empty(),
                    "{}.{} output_format 空",
                    d.template.template_id,
                    r.role
                );
                assert!(
                    !s.acceptance.is_empty(),
                    "{}.{} acceptance 空",
                    d.template.template_id,
                    r.role
                );
            }
        }
        assert!(prompt_sections_for("no-such-template", "any").is_none());
        assert!(prompt_sections_for(CODE_CHANGE_V1, "unknown_role").is_none());
    }

    #[test]
    fn code_template_sections_keep_single_writer_semantics() {
        let implementer = prompt_sections_for(CODE_CHANGE_V1, "implementer").unwrap();
        assert!(implementer
            .must_do
            .iter()
            .any(|l| l.contains("单写者") || l.contains("write_file")));
        let reviewer = prompt_sections_for(CODE_CHANGE_V1, "reviewer").unwrap();
        assert!(
            reviewer
                .must_not_do
                .iter()
                .any(|l| l.contains("不改写交付物") || l.contains("只读")),
            "评审角色段必须保持只读语义：{reviewer:?}"
        );
        assert!(reviewer
            .must_do
            .iter()
            .chain(reviewer.must_not_do.iter())
            .all(|l| !l.contains("唯一允许产出代码")));
    }
}
