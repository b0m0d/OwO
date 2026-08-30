//! 角色画像（WorkerProfile，七期 · 二路）：把模板声明的角色权限真正作用到 Worker。
//!
//! - [`WorkerProfile::for_role`]：内置角色 → 工具面 / 只读 / 写白名单 / 回合上限 /
//!   浏览器 / 受控命令的映射（分析·审查·校验·抽取族 = 只读；实现族 = 读写 +
//!   受控命令；研究族 = 只读 + 浏览器；**未知角色默认只读**——权限默认 deny，
//!   显式匹配才有写面）；
//! - [`WorkerProfile::build_registry`]：按画像装配 [`ToolRegistry`]——**注册表面即
//!   权限边界**：读角色的注册表里根本没有写/执行工具，而非注册后靠审批拒绝；
//! - [`intersect_paths`]：角色写白名单 ∩ 团队绑定写白名单（任一侧为空 = 取非空一侧；
//!   两侧都空 = 工作区内可写，仍受审批约束）；
//! - [`ProfileSubagentRunner`]：画像驱动子代理执行器——与一路
//!   `ContractSubagentRunner` 同口径（完整回合循环 + `WorkerOutputV1` 输出契约 +
//!   至多一次定向修复；`is_critic` 为 critic 角色代理），区别仅在工具注册表由画像
//!   装配、回合上限取画像值、写面为交集白名单。一路冻结口径不受影响：
//!   `SubagentRunner` / `ContractSubagentRunner` 签名与字段零改动。

use crate::agent::{Agent, AgentConfig, TurnEvent};
use crate::contract_worker::enforce_worker_output_contract;
use crate::gateway::ModelProvider;
use crate::permissions::{Approver, Policy};
use crate::session::Session;
use crate::subagent::MAX_SUBAGENT_DEPTH;
use crate::tools::ToolRegistry;
use crate::workswarm_output::contract_system_prompt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// 画像回合上限（与 `SubagentRunner` 子代理口径一致：max_turns 硬上限 12）。
pub const PROFILE_MAX_TURNS_CAP: usize = 12;

/// 模板未声明该角色预算（`budget_calls == 0`）时的缺省回合上限。
pub const DEFAULT_PROFILE_MAX_TURNS: usize = 12;

/// 角色族（工具面装配依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleFamily {
    /// 分析/审查/校验/抽取族：只读文件面（read_file / list_dir / search_files）。
    Read,
    /// 实现族：读写文件 + 搜索 + 受控命令。
    Implementer,
    /// 研究族：只读文件面 + 浏览器（搜索/导航/快照）。
    Researcher,
}

/// 角色权限画像：单角色的实际工具面与执行上限（七期 · 二路）。
///
/// 由模板角色名（`budget_calls_per_role[].role`）派生；服务端在每个 TeamRun 阶段
/// 重建注册表时逐角色实例化，最终写面 = 「团队绑定写白名单 ∩ 角色白名单」。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerProfile {
    /// 模型可见工具名（注册表按它裁剪；空 = 不额外过滤）。
    pub visible_tools: Vec<String>,
    /// 只读角色：注册表不含任何写/执行工具，`Policy::read_only` 再兜底一层。
    pub read_only: bool,
    /// 角色级写白名单（相对工作区根；空 = 交由团队绑定白名单决定）。
    pub write_allowed_paths: Vec<String>,
    /// 回合上限（模板 `budget_calls_per_role[].budget_calls`；硬上限 12）。
    pub max_turns: usize,
    /// 允许浏览器（搜索/导航/快照；写工作区变体不在可见面）。
    pub can_use_browser: bool,
    /// 允许受控命令（run_command；仍经沙箱 + 审批策略约束）。
    pub can_run_command: bool,
}

impl WorkerProfile {
    /// 内置角色 → 画像。`budget_calls` = 模板每角色调用预算（0 = 未声明 → 缺省 12）。
    ///
    /// 映射口径（与四类内置模板对齐）：
    /// - 实现族（名字含 `implementer` / `builder` / `finalizer` / `drafter`，如
    ///   code-change-v1 的 implementer、document-delivery-v1 的 drafter/finalizer）：
    ///   读写文件 + 搜索 + `run_command`；
    /// - 研究族（`researcher*` / 含 `research` / `brief_writer`，如 research-brief-v1
    ///   全角色）：只读文件 + 浏览器三件套（browser_search/navigate/snapshot）；
    /// - 其余（code_analyzer / reviewer / evidence_verifier / schema_validator /
    ///   extractor / artifact_formatter / critic 等分析·审查·校验·抽取族）：
    ///   只读文件面；未知角色同样落这里（默认 deny）。
    pub fn for_role(role: &str, budget_calls: usize) -> Self {
        let name = role.trim().to_ascii_lowercase();
        const IMPLEMENTER_KEYWORDS: [&str; 4] = ["implementer", "builder", "finalizer", "drafter"];
        let is_implementer = IMPLEMENTER_KEYWORDS.iter().any(|k| name.contains(k));
        let is_researcher = name.starts_with("researcher")
            || name.contains("research")
            || name.contains("brief_writer");
        let max_turns = (if budget_calls == 0 {
            DEFAULT_PROFILE_MAX_TURNS
        } else {
            budget_calls
        })
        .clamp(1, PROFILE_MAX_TURNS_CAP);
        if is_implementer {
            Self {
                visible_tools: [
                    "read_file",
                    "write_file",
                    "list_dir",
                    "search_files",
                    "run_command",
                ]
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
                read_only: false,
                write_allowed_paths: Vec::new(),
                max_turns,
                can_use_browser: false,
                can_run_command: true,
            }
        } else if is_researcher {
            Self {
                visible_tools: [
                    "read_file",
                    "list_dir",
                    "search_files",
                    "browser_search",
                    "browser_navigate",
                    "browser_snapshot",
                ]
                .iter()
                .map(|tool| (*tool).to_string())
                .collect(),
                read_only: true,
                write_allowed_paths: Vec::new(),
                max_turns,
                can_use_browser: true,
                can_run_command: false,
            }
        } else {
            Self {
                visible_tools: ["read_file", "list_dir", "search_files"]
                    .iter()
                    .map(|tool| (*tool).to_string())
                    .collect(),
                read_only: true,
                write_allowed_paths: Vec::new(),
                max_turns,
                can_use_browser: false,
                can_run_command: false,
            }
        }
    }

    /// 角色族。
    pub fn family(&self) -> RoleFamily {
        if !self.read_only {
            RoleFamily::Implementer
        } else if self.can_use_browser {
            RoleFamily::Researcher
        } else {
            RoleFamily::Read
        }
    }

    /// 是否写角色（单写租约与变更追踪只作用于写角色）。
    pub fn is_writer(&self) -> bool {
        !self.read_only
    }

    /// 按画像装配工具注册表：注册表面即权限边界。
    ///
    /// - `write_allowed`：最终写白名单（角色 ∩ 团队绑定，见 [`intersect_paths`]；
    ///   空 = 工作区内可写）。只读角色忽略该参数（不注册任何写工具）。
    /// - `visible_tools` 非空时按名单裁剪，保证「实际可见工具 == 模板/画像声明」
    ///   （浏览器组里的写工作区变体也会被裁掉）。
    pub fn build_registry(&self, write_allowed: Vec<PathBuf>) -> ToolRegistry {
        let mut registry = ToolRegistry::empty();
        registry.register_file_read_tools();
        if !self.read_only {
            registry.register_whitelist_write_file(write_allowed);
        }
        if self.can_run_command {
            registry.register_run_command();
        }
        if self.can_use_browser {
            registry.register_browser_tools();
        }
        if !self.visible_tools.is_empty() {
            registry.retain_names(&self.visible_tools);
        }
        registry
    }
}

/// 路径白名单交集（七期 · 二路）：角色写白名单 ∩ 团队绑定写白名单。
///
/// - 任一侧为空 = 取非空一侧（空表示「未额外约束」）；
/// - 两侧都空 = 空（工作区内可写，仍受审批约束）；
/// - 否则逐条收窄：a 的条目落在 b 某前缀内 → 保留该条目（更窄者）；b 某前缀落在
///   a 条目内 → 保留该 b 前缀（窄者胜，交集语义）。
pub fn intersect_paths(a: &[PathBuf], b: &[PathBuf]) -> Vec<PathBuf> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    let mut intersection = Vec::new();
    for path in a {
        if b.iter().any(|base| path.starts_with(base)) {
            intersection.push(path.clone());
        } else if let Some(narrower) = b.iter().find(|base| base.starts_with(path)) {
            intersection.push(narrower.clone());
        }
    }
    intersection
}

/// 画像驱动子代理执行器（七期 · 二路）：与一路 `ContractSubagentRunner` 同口径
/// （完整回合循环 + `WorkerOutputV1` 输出契约执行 + 至多一次定向修复），区别仅在：
///
/// - 工具注册表由 [`WorkerProfile::build_registry`] 按角色装配（注册表面即权限边界）；
/// - 回合上限取画像值（模板预算，硬上限 12）；
/// - 写面为「角色 ∩ 绑定」交集白名单工具（越界写入在工具层被拒）；
/// - `is_critic` 由服务端按角色名判定（`role == "critic"`；引擎注入的 `read_only`
///   只覆盖 critic，其余内置角色都是 producer，画像另管只读面）。
pub struct ProfileSubagentRunner<'a> {
    pub provider: Arc<dyn ModelProvider>,
    pub approver: &'a dyn Approver,
    /// 中断标志：团队取消桥共享置位，`run_turn` 协作式检查。
    pub abort: &'a AtomicBool,
    pub depth: usize,
    pub model: String,
    /// critic 角色代理（true = 只读探索口径，禁带 artifact；false = producer）。
    pub is_critic: bool,
    /// 最终写白名单（角色 ∩ 绑定交集；空 = 工作区内可写）。
    pub write_allowed: Vec<PathBuf>,
    pub profile: WorkerProfile,
}

impl ProfileSubagentRunner<'_> {
    /// 运行一个画像子代理会话，返回契约校验后的 `WorkerOutputV1` JSON 本体。
    pub async fn run(&self, workspace: &Path, prompt: &str) -> Result<String, String> {
        if self.depth >= MAX_SUBAGENT_DEPTH {
            return Err(format!("子代理深度超限（最多 {MAX_SUBAGENT_DEPTH} 层）"));
        }
        let policy = if self.profile.read_only {
            Policy::read_only(workspace.to_path_buf())
        } else {
            Policy::new(workspace.to_path_buf())
        };
        let registry = self.profile.build_registry(self.write_allowed.clone());
        let config = AgentConfig {
            max_turns: self.profile.max_turns.min(PROFILE_MAX_TURNS_CAP),
            subagent_depth: self.depth + 1,
            ..Default::default()
        };
        let agent = Agent::new(Arc::clone(&self.provider), registry, policy, config);
        // 基线提示词：critic 探索口径与一路同款；写角色追加「必须真实落盘」约束——
        // 防止把变更只塞进 artifact 内容而不写工作区文件（假交付；评审对照的
        // 是工作区真实文件，git 变更追踪也以真实落盘为准）。
        let base_prompt = if self.is_critic {
            "你是只读探索子代理：只能读取/搜索工作区文件，禁止写入或执行命令；调查完成后用简洁中文汇报发现。\n"
        } else if self.profile.is_writer() {
            "你是写角色子代理：凡涉及代码/文件变更，必须用 write_file 把最终内容真实写入工作区文件（仅限允许路径内的文件，工具面之外没有其他写入手段）；artifact.content 只写变更说明、影响面与验证方式，不要把完整变更只放在 artifact 里而不落盘。回合预算有限：先做必要读取，随后直接完成写入，最后一个回合只输出契约 JSON——不要重复读取同一文件或执行验证命令。工具调用仍需审批，完成后汇报结果。\n"
        } else {
            "你是通用子代理：独立完成委派任务，工具调用仍需审批，完成后汇报结果。\n"
        };
        // 回合预算指引（七期二路冒烟结论）：预算经 max_turns 成为硬上限，引擎在
        // 「回合耗尽且未正常结束」时报错——必须显式告诉模型最后一回合只产出契约
        // JSON、不再调用工具，否则读/写角色会稳定在预算上溢出失败。
        let budget_note = format!(
            "你的回合预算为 {} 回合：前 {} 回合完成必要的工具调用，最后一个回合必须直接输出最终 JSON（不要再调用任何工具）。尽量少花回合。\n",
            self.profile.max_turns,
            self.profile.max_turns.saturating_sub(1)
        );
        // 输出契约（V1）：system prompt 追加契约条款，让模型首轮即可按
        // WorkerOutputV1 JSON 输出；不合规时共享执行器最多定向修复一次。
        let system_prompt = format!(
            "{base_prompt}{budget_note}{}",
            contract_system_prompt(self.is_critic)
        );
        let mut session = Session::new(workspace, self.model.clone(), Some(system_prompt));
        let mut on_event = |_event: &TurnEvent| {};
        let outcome = agent
            .run_turn(
                &mut session,
                prompt,
                self.approver,
                self.abort,
                &mut on_event,
            )
            .await
            .map_err(|error| format!("子代理执行失败：{error}"))?;
        // 连兜底文本（无最终文本）也走契约执行——自由文本路径不豁免。
        let text = outcome
            .final_text
            .unwrap_or_else(|| format!("（子代理无最终文本，共 {} 步）", outcome.steps));
        match enforce_worker_output_contract(&self.provider, &text, self.is_critic).await {
            Ok(result) => Ok(result.text),
            Err(error) => Err(error.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names(registry: &ToolRegistry) -> Vec<String> {
        registry
            .specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect()
    }

    #[test]
    fn implementer_gets_write_and_command_surface() {
        let profile = WorkerProfile::for_role("implementer", 5);
        assert_eq!(profile.family(), RoleFamily::Implementer);
        assert!(profile.is_writer());
        assert!(profile.can_run_command);
        assert_eq!(profile.max_turns, 5);
        let names = tool_names(&profile.build_registry(Vec::new()));
        for expected in [
            "read_file",
            "write_file",
            "list_dir",
            "search_files",
            "run_command",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "implementer 缺工具 {expected}：{names:?}"
            );
        }
        assert_eq!(names.len(), 5, "implementer 不应有多余工具：{names:?}");
    }

    #[test]
    fn implementer_write_tool_is_whitelist_wrapped_but_names_unchanged() {
        // 白名单包装不改变工具名（模型可见面不变），仅在执行时做前缀校验。
        let profile = WorkerProfile::for_role("implementer", 5);
        let allowed = vec![PathBuf::from("T:/ws/src")];
        let names = tool_names(&profile.build_registry(allowed));
        assert!(names.iter().any(|name| name == "write_file"));
    }

    #[test]
    fn read_roles_cannot_write_or_run_commands() {
        for role in [
            "code_analyzer",
            "reviewer",
            "evidence_verifier",
            "schema_validator",
            "extractor",
            "artifact_formatter",
            "content_reviewer",
            // 未知角色默认只读（权限默认 deny）。
            "some_future_role",
        ] {
            let profile = WorkerProfile::for_role(role, 3);
            assert_eq!(profile.family(), RoleFamily::Read, "{role} 应为只读读面");
            assert!(!profile.is_writer(), "{role} 不应是写角色");
            assert!(!profile.can_run_command, "{role} 不应允许 run_command");
            assert!(!profile.can_use_browser, "{role} 不应允许浏览器");
            let names = tool_names(&profile.build_registry(Vec::new()));
            assert!(
                !names.iter().any(|name| name == "write_file"),
                "{role} 注册表含 write_file：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "run_command"),
                "{role} 注册表含 run_command：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name.starts_with("browser_")),
                "{role} 注册表含浏览器工具：{names:?}"
            );
            assert_eq!(names.len(), 3, "{role} 工具面应恰为读三件套：{names:?}");
        }
    }

    #[test]
    fn researcher_family_gets_browser_without_write() {
        for role in ["researcher_a", "researcher_b", "brief_writer"] {
            let profile = WorkerProfile::for_role(role, 4);
            assert_eq!(profile.family(), RoleFamily::Researcher, "{role}");
            assert!(profile.read_only);
            assert!(profile.can_use_browser);
            let names = tool_names(&profile.build_registry(Vec::new()));
            assert!(
                !names.iter().any(|name| name == "write_file"),
                "{role} 注册表含 write_file：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "run_command"),
                "{role} 注册表含 run_command：{names:?}"
            );
            for expected in ["browser_search", "browser_navigate", "browser_snapshot"] {
                assert!(
                    names.iter().any(|name| name == expected),
                    "{role} 缺 {expected}：{names:?}"
                );
            }
            // 浏览器组里的写工作区变体必须在可见面裁剪后消失。
            assert!(
                !names.iter().any(|name| name == "browser_screenshot"),
                "{role} 不应含写工作区变体 browser_screenshot：{names:?}"
            );
            assert!(
                !names.iter().any(|name| name == "browser_download_image"),
                "{role} 不应含写工作区变体 browser_download_image：{names:?}"
            );
        }
    }

    #[test]
    fn budget_maps_to_turn_cap() {
        assert_eq!(
            WorkerProfile::for_role("implementer", 0).max_turns,
            DEFAULT_PROFILE_MAX_TURNS,
            "未声明预算 → 缺省"
        );
        assert_eq!(WorkerProfile::for_role("implementer", 5).max_turns, 5);
        assert_eq!(
            WorkerProfile::for_role("implementer", 99).max_turns,
            PROFILE_MAX_TURNS_CAP,
            "预算超硬上限 → 截到 12"
        );
    }

    #[test]
    fn intersect_paths_semantics() {
        let src = PathBuf::from("T:/ws/src");
        let lib = PathBuf::from("T:/ws/src/lib");
        let docs = PathBuf::from("T:/ws/docs");
        // 两侧都空 = 空（工作区内可写，仍受审批约束）。
        assert!(intersect_paths(&[], &[]).is_empty());
        // 任一侧空 = 取非空一侧。（四路集成微修：clippy 冗余 clone → std::slice::from_ref 借用）
        assert_eq!(
            intersect_paths(std::slice::from_ref(&src), &[]),
            vec![src.clone()]
        );
        assert_eq!(
            intersect_paths(&[], std::slice::from_ref(&docs)),
            vec![docs.clone()]
        );
        // 窄者胜：profile(src) ∩ scope(src/lib) = src/lib。
        assert_eq!(
            intersect_paths(std::slice::from_ref(&src), std::slice::from_ref(&lib)),
            vec![lib.clone()]
        );
        // 不相交 = 空。
        assert!(
            intersect_paths(std::slice::from_ref(&docs), std::slice::from_ref(&src)).is_empty()
        );
        // 多条目：相交的留下，不相交的丢弃。
        let got = intersect_paths(&[src.clone(), docs.clone()], &[lib.clone(), docs.clone()]);
        assert_eq!(got, vec![lib.clone(), docs.clone()]);
    }
}
