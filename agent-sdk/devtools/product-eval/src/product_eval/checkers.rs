//! Artifact checker 语义、路径/scope 规则与求值（从 product_eval.rs 拆出）。

use super::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// 自动检查器：对一个已完成运行的沙盒产物做判定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ArtifactChecker {
    /// 文件必须存在且非空。
    Exists { path: String },
    /// 文件内容必须包含子串。
    Contains { path: String, text: String },
    /// 文件内容不得包含子串。
    NotContains { path: String, text: String },
    /// 文件内容必须匹配正则（Rust regex 语法）。
    Regex { path: String, pattern: String },
    /// 非空行数下限。
    LineCountMin { path: String, min_lines: usize },
    /// JSON 顶层字段等值断言。
    JsonFieldEquals {
        path: String,
        field: String,
        expected: serde_json::Value,
    },
    /// JSON 顶层字段必须**恰好**为该集合（顺序无关）：多余字段或缺失字段均失败。
    /// 用于「只输出一个 JSON 对象，不得包含额外字段」这类硬约束。
    JsonKeysExact { path: String, keys: Vec<String> },
    /// 行为检查：在沙盒内以 `cmd /C <command>` 执行命令（cwd 相对沙盒根），
    /// 断言退出码与 stdout。用于「代码任务检查真实行为和产物」而非只查关键词。
    ///
    /// 执行语义：
    /// - 仅在**真实沙盒目录**评估时执行（内存快照/validate 自检静默跳过，静态校验命令形态）；
    /// - 命令是任务作者声明的固定命令（非 Agent 输入），白名单式无 shell 链式元字符；
    /// - 自带超时与输出截断，失败即该检查器失败。
    CommandCheck {
        command: String,
        /// 相对沙盒根的执行目录（None = 沙盒根）。
        #[serde(default)]
        cwd: Option<String>,
        /// 期望退出码；缺省 Some(0)（None = 不校验退出码）。
        #[serde(default = "default_expect_zero")]
        expect_exit: Option<i32>,
        /// stdout 必须包含的片段。
        #[serde(default)]
        stdout_contains: Vec<String>,
        /// stdout 不得包含的片段。
        #[serde(default)]
        stdout_not_contains: Vec<String>,
        /// 命令超时（秒），缺省 30。
        #[serde(default = "default_command_timeout_secs")]
        timeout_secs: u32,
    },
}

fn default_expect_zero() -> Option<i32> {
    Some(0)
}

fn default_command_timeout_secs() -> u32 {
    30
}

impl ArtifactChecker {
    pub fn describe(&self) -> String {
        match self {
            ArtifactChecker::Exists { path } => format!("exists({path})"),
            ArtifactChecker::Contains { path, text } => {
                format!("contains({path}, {:?})", truncate_for_log(text, 40))
            }
            ArtifactChecker::NotContains { path, text } => {
                format!("not_contains({path}, {:?})", truncate_for_log(text, 40))
            }
            ArtifactChecker::Regex { path, pattern } => {
                format!("regex({path}, {:?})", truncate_for_log(pattern, 60))
            }
            ArtifactChecker::LineCountMin { path, min_lines } => {
                format!("line_count_min({path}, ≥{min_lines})")
            }
            ArtifactChecker::JsonFieldEquals {
                path,
                field,
                expected,
            } => format!("json_field({path}.{field} == {expected})"),
            ArtifactChecker::JsonKeysExact { path, keys } => {
                format!("json_keys_exact({path}, {{{}}})", keys.join(","))
            }
            ArtifactChecker::CommandCheck { command, .. } => {
                format!("command_check({})", truncate_for_log(command, 60))
            }
        }
    }

    /// 静态校验（编译正则、路径合法、数值范围）。
    pub fn static_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let path = match self {
            ArtifactChecker::Exists { path }
            | ArtifactChecker::Contains { path, .. }
            | ArtifactChecker::NotContains { path, .. }
            | ArtifactChecker::Regex { path, .. }
            | ArtifactChecker::LineCountMin { path, .. }
            | ArtifactChecker::JsonFieldEquals { path, .. }
            | ArtifactChecker::JsonKeysExact { path, .. } => Some(path.clone()),
            ArtifactChecker::CommandCheck { .. } => None,
        };
        if let Some(path) = path {
            if let Err(err) = sanitize_rel_path(&path) {
                issues.push(format!("检查器路径非法「{path}」：{err}"));
            }
        }
        match self {
            ArtifactChecker::Exists { .. } => {}
            ArtifactChecker::Contains { text, .. } | ArtifactChecker::NotContains { text, .. } => {
                if text.is_empty() {
                    issues.push("包含断言的 text 为空".to_string());
                }
            }
            ArtifactChecker::Regex { pattern, .. } => {
                if let Err(err) = Regex::new(pattern) {
                    issues.push(format!("正则无效 {pattern:?}：{err}"));
                }
            }
            ArtifactChecker::LineCountMin { min_lines, .. } => {
                if *min_lines == 0 {
                    issues.push("line_count_min 的 min_lines 必须 ≥ 1".to_string());
                }
            }
            ArtifactChecker::JsonFieldEquals { field, .. } => {
                if field.is_empty() {
                    issues.push("json_field 断言缺少 field 名".to_string());
                }
            }
            ArtifactChecker::JsonKeysExact { keys, .. } => {
                if keys.is_empty() {
                    issues.push("json_keys_exact 的 keys 为空：至少声明一个字段".to_string());
                }
                for key in keys {
                    if key.trim().is_empty() {
                        issues.push("json_keys_exact 的 keys 存在空字段名".to_string());
                    }
                }
            }
            ArtifactChecker::CommandCheck {
                command,
                cwd,
                timeout_secs,
                ..
            } => {
                let command = command.trim();
                if command.is_empty() {
                    issues.push("command_check 的 command 为空".to_string());
                } else if command_has_chain_metachars(command) {
                    issues.push(format!(
                        "command_check 的命令「{command}」含链式/重定向元字符（检查命令按单命令收口）"
                    ));
                } else if command.contains("  ") {
                    issues.push(format!(
                        "command_check 的命令「{command}」含连续空格（命令须为单条简单调用）"
                    ));
                }
                if let Some(cwd) = cwd {
                    if let Err(err) = sanitize_rel_path(cwd) {
                        issues.push(format!("command_check 的 cwd 非法：{err}"));
                    }
                }
                if !(1..=120).contains(timeout_secs) {
                    issues.push(format!(
                        "command_check 的 timeout_secs={timeout_secs} 超出 [1,120]"
                    ));
                }
            }
        }
        issues
    }
}

/// 命令链式/重定向元字符检测（与 [`single_agent::command_has_shell_metachars`] 同口径，
/// 供 command_check 静态校验使用：检查命令必须是单条简单调用）。
pub fn command_has_chain_metachars(command: &str) -> bool {
    command
        .chars()
        .any(|c| matches!(c, '&' | '|' | ';' | '<' | '>' | '`' | '\n' | '\r'))
        || command.contains("$(")
}
pub fn sanitize_rel_path(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("路径为空".to_string());
    }
    if raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':') {
        return Err(format!("不允许绝对路径「{raw}」"));
    }
    let normalized = raw.replace('\\', "/");
    let mut cleaned: Vec<&str> = Vec::new();
    for seg in normalized.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(format!("路径包含「..」越界：「{raw}」")),
            _ => cleaned.push(seg),
        }
    }
    if cleaned.is_empty() {
        return Err(format!("路径规范化后为空：「{raw}」"));
    }
    Ok(cleaned.join("/"))
}

/// 单段通配匹配：`*` 匹配段内任意字符，`?` 匹配单个字符。
fn segment_matches(pattern: &str, text: &str) -> bool {
    let pc: Vec<char> = pattern.chars().collect();
    let tc: Vec<char> = text.chars().collect();
    segment_match_inner(&pc, &tc)
}

fn segment_match_inner(p: &[char], t: &[char]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    match p[0] {
        '*' => {
            for skip in 0..=t.len() {
                if segment_match_inner(&p[1..], &t[skip..]) {
                    return true;
                }
            }
            false
        }
        '?' => !t.is_empty() && segment_match_inner(&p[1..], &t[1..]),
        c => !t.is_empty() && t[0] == c && segment_match_inner(&p[1..], &t[1..]),
    }
}

/// 判断相对路径是否落在某条 scope 规则内：
/// - 支持段级通配 `*` / `?` 与目录尾缀 `/**`；
/// - 无通配的模式按"目录前缀"语义覆盖其全部子路径（`out` ⊇ `out/a.md`）。
pub fn scope_matches(pattern: &str, rel: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() || rel.is_empty() {
        return false;
    }
    if pattern == "**" || pattern == "**/*" {
        return true;
    }
    let pat_norm = pattern.replace('\\', "/");
    let pat = pat_norm.trim_end_matches('/');
    if rel == pat {
        return true;
    }
    // 目录前缀语义（仅对无通配的纯目录模式）。
    if !pat.contains('*') && !pat.contains('?') && rel.starts_with(&(pat.to_string() + "/")) {
        return true;
    }
    let p_segs: Vec<&str> = pat.split('/').collect();
    let r_segs: Vec<&str> = rel.split('/').collect();
    if p_segs.contains(&"**") {
        return double_star_match(&p_segs, &r_segs);
    }
    if p_segs.len() != r_segs.len() {
        return false;
    }
    p_segs
        .iter()
        .zip(r_segs.iter())
        .all(|(p, r)| segment_matches(p, r))
}

fn double_star_match(p: &[&str], r: &[&str]) -> bool {
    if p.is_empty() {
        return r.is_empty();
    }
    if p[0] == "**" {
        for skip in 0..=r.len() {
            if double_star_match(&p[1..], &r[skip..]) {
                return true;
            }
        }
        return false;
    }
    if r.is_empty() {
        return false;
    }
    segment_matches(p[0], r[0]) && double_star_match(&p[1..], &r[1..])
}

/// 任一规则命中即在范围内。
pub fn in_scope(rules: &[String], rel: &str) -> bool {
    rules.iter().any(|rule| scope_matches(rule, rel))
}

// ---------------------------------------------------------------------------
// 文件来源抽象：让检查器既能跑磁盘也能跑内存快照（validate 自检用）
// ---------------------------------------------------------------------------

pub(crate) trait FileSource {
    fn read_text(&self, rel: &str) -> Option<String>;
    fn exists_nonempty(&self, rel: &str) -> bool;
}

struct FsSource<'a>(&'a Path);

impl FileSource for FsSource<'_> {
    fn read_text(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(rel)).ok()
    }
    fn exists_nonempty(&self, rel: &str) -> bool {
        match std::fs::metadata(self.0.join(rel)) {
            Ok(meta) => meta.is_file() && meta.len() > 0,
            Err(_) => false,
        }
    }
}

pub(super) struct MapSource<'a>(pub(super) &'a BTreeMap<String, String>);

impl FileSource for MapSource<'_> {
    fn read_text(&self, rel: &str) -> Option<String> {
        self.0.get(rel).cloned()
    }
    fn exists_nonempty(&self, rel: &str) -> bool {
        self.0
            .get(rel)
            .map(|content| !content.trim().is_empty())
            .unwrap_or(false)
    }
}

/// 对单个检查器求值：Ok(()) 通过，Err(描述) 为失败原因。
/// （公开入口见 [`evaluate_checker_on_map`] / [`evaluate_checker_on_dir`]。）
pub(crate) fn evaluate_checker<S: FileSource>(
    checker: &ArtifactChecker,
    source: &S,
) -> Result<(), String> {
    match checker {
        ArtifactChecker::Exists { path } => {
            if source.exists_nonempty(path) {
                Ok(())
            } else {
                Err(format!("产物缺失或为空：{path}"))
            }
        }
        ArtifactChecker::Contains { path, text } => match source.read_text(path) {
            Some(content) if content.contains(text.as_str()) => Ok(()),
            Some(_) => Err(format!(
                "{path} 缺少期望片段 {:?}",
                truncate_for_log(text, 40)
            )),
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::NotContains { path, text } => match source.read_text(path) {
            Some(content) if !content.contains(text.as_str()) => Ok(()),
            Some(_) => Err(format!(
                "{path} 含禁止片段 {:?}",
                truncate_for_log(text, 40)
            )),
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::Regex { path, pattern } => {
            // 产物是多行文件：`^`/`$` 按行锚定（multi_line），符合"标题必须顶行"类断言的直觉。
            let re = RegexBuilder::new(pattern)
                .multi_line(true)
                .build()
                .map_err(|e| format!("正则无效 {pattern:?}：{e}"))?;
            match source.read_text(path) {
                Some(content) if re.is_match(&content) => Ok(()),
                Some(_) => Err(format!(
                    "{path} 未匹配正则 {:?}",
                    truncate_for_log(pattern, 60)
                )),
                None => Err(format!("产物不存在：{path}")),
            }
        }
        ArtifactChecker::LineCountMin { path, min_lines } => match source.read_text(path) {
            Some(content) => {
                let count = content.lines().filter(|l| !l.trim().is_empty()).count();
                if count >= *min_lines {
                    Ok(())
                } else {
                    Err(format!("{path} 有效行数 {count} < {min_lines}"))
                }
            }
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::JsonFieldEquals {
            path,
            field,
            expected,
        } => match source.read_text(path) {
            Some(content) => {
                let value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| format!("{path} 不是合法 JSON：{e}"))?;
                let actual = value
                    .get(field)
                    .ok_or_else(|| format!("{path} 缺少顶层字段 {field}"))?;
                if actual == expected {
                    Ok(())
                } else {
                    Err(format!("{path}.{field} = {actual}，期望 {expected}"))
                }
            }
            None => Err(format!("产物不存在：{path}")),
        },
        ArtifactChecker::JsonKeysExact { path, keys } => match source.read_text(path) {
            Some(content) => {
                let value: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| format!("{path} 不是合法 JSON：{e}"))?;
                let object = value
                    .as_object()
                    .ok_or_else(|| format!("{path} 顶层必须是 JSON 对象"))?;
                let actual_keys: Vec<&String> = object.keys().collect();
                let mut want: Vec<&String> = keys.iter().collect();
                want.sort();
                let mut actual: Vec<&String> = actual_keys.to_vec();
                actual.sort();
                if want == actual {
                    Ok(())
                } else {
                    let missing: Vec<&String> = want
                        .iter()
                        .filter(|key| !actual.contains(key))
                        .copied()
                        .collect();
                    let extra: Vec<&String> = actual
                        .iter()
                        .filter(|key| !want.contains(key))
                        .copied()
                        .collect();
                    Err(format!(
                        "{path} 顶层字段集合不符：缺失 {{{}}} 多余 {{{}}}",
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                        extra
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                }
            }
            None => Err(format!("产物不存在：{path}")),
        },
        // 行为检查不在文件快照上评估：runner 在真实沙盒上经 [`evaluate_command_check`] 执行。
        ArtifactChecker::CommandCheck { .. } => Ok(()),
    }
}

/// 在真实沙盒目录上执行 command_check（`cmd /C <command>` 单命令收口，自带超时）。
/// 仅对 [`ArtifactChecker::CommandCheck`] 有意义；对其他检查器返回 Err。
pub async fn evaluate_command_check(
    checker: &ArtifactChecker,
    sandbox: &Path,
) -> Result<(), String> {
    let ArtifactChecker::CommandCheck {
        command,
        cwd,
        expect_exit,
        stdout_contains,
        stdout_not_contains,
        timeout_secs,
    } = checker
    else {
        return Err("evaluate_command_check 仅支持 CommandCheck".to_string());
    };
    let base = match cwd {
        Some(raw) => {
            let rel = sanitize_rel_path(raw)?;
            if rel.is_empty() {
                sandbox.to_path_buf()
            } else {
                sandbox.join(rel)
            }
        }
        None => sandbox.to_path_buf(),
    };
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err("command_check 的 command 为空".to_string());
    }
    let command_for_spawn = command.clone();
    let base_for_spawn = base.clone();
    let handle = tokio::task::spawn_blocking(move || {
        std::process::Command::new("cmd")
            .args(["/C", command_for_spawn.as_str()])
            .current_dir(&base_for_spawn)
            .output()
    });
    let output = tokio::time::timeout(Duration::from_secs((*timeout_secs) as u64), handle)
        .await
        .map_err(|_| format!("command_check 超时（{timeout_secs}s）：{command}"))?
        .map_err(|e| format!("command_check 任务失败：{e}"))?
        .map_err(|e| format!("command_check 执行失败（cwd={}）：{e}", base.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let exit_code = output.status.code();
    // 期望退出码：None = 不校验；Some(0) 为缺省。
    if let Some(expected) = expect_exit {
        if exit_code != Some(*expected) {
            return Err(format!(
                "command_check 退出码 {} != 期望 {}（命令：{command}）",
                exit_code.unwrap_or(-1),
                expected
            ));
        }
    }
    for needle in stdout_contains {
        if !stdout.contains(needle.as_str()) {
            return Err(format!(
                "command_check stdout 缺少片段 {:?}（命令：{command}）",
                truncate_for_log(needle, 40)
            ));
        }
    }
    for banned in stdout_not_contains {
        if stdout.contains(banned.as_str()) {
            return Err(format!(
                "command_check stdout 出现禁止片段 {:?}（命令：{command}）",
                truncate_for_log(banned, 40)
            ));
        }
    }
    Ok(())
}

pub(super) fn evaluate_all<S: FileSource>(
    checkers: &[ArtifactChecker],
    source: &S,
) -> (bool, Vec<String>) {
    let mut failed = Vec::new();
    for checker in checkers {
        if let Err(reason) = evaluate_checker(checker, source) {
            failed.push(format!("{} ⇒ {}", checker.describe(), reason));
        }
    }
    (failed.is_empty(), failed)
}

/// 在真实沙盒上评估全部检查器（command_check 在此真正执行；其余与快照语义一致）。
/// 返回 (通过数, 总数, 失败描述)，供 runner 判定与质量分计算。
pub async fn evaluate_all_on_dir(
    checkers: &[ArtifactChecker],
    sandbox: &Path,
) -> (u32, u32, Vec<String>) {
    let mut failed = Vec::new();
    let mut passed = 0u32;
    let total = checkers.len() as u32;
    for checker in checkers {
        let result = match checker {
            ArtifactChecker::CommandCheck { .. } => evaluate_command_check(checker, sandbox).await,
            _ => evaluate_checker(checker, &FsSource(sandbox)),
        };
        match result {
            Ok(()) => passed += 1,
            Err(reason) => failed.push(format!("{} ⇒ {}", checker.describe(), reason)),
        }
    }
    (passed, total, failed)
}

/// 在内存快照上求值单个检查器（公开入口，供校验/测试复用）。
pub fn evaluate_checker_on_map(
    checker: &ArtifactChecker,
    snapshot: &BTreeMap<String, String>,
) -> Result<(), String> {
    evaluate_checker(checker, &MapSource(snapshot))
}

/// 在磁盘目录上求值单个检查器（公开入口，供校验/测试复用）。
pub fn evaluate_checker_on_dir(checker: &ArtifactChecker, root: &Path) -> Result<(), String> {
    evaluate_checker(checker, &FsSource(root))
}
