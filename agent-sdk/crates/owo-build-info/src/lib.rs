//! §6.1.2/§7.1 共享构建身份（单一事实源）。
//!
//! build.rs 编译期烧录 git commit/dirty/epoch；`identity()` 提供**全工程唯
//! 一**的解析链（① `OWO_BUILD_INFO` 覆写文件 → ② 编译期事实 → ③ cwd 下
//! build-info.json 遗留产物）。server `/health.build`、cli `core_ready`/
//! `--version`/doctor 与桌面壳 expectedBuildId 全部经本 crate 读取，消费方
//! 不再各持一份回退链（历史 bug：server 覆写优先而 CLI 编译期优先，两条链
//! 优先级相反，发布覆写语义只在一侧生效）。
//!
//! 依赖刻意最小：仅 `serde_json`（build-info.json 兼容解析）。RFC3339 由
//! 内置 civil-from-days 算法生成（不引入 chrono，保持叶 crate 轻量）。

use std::path::{Path, PathBuf};

/// 编译期烧录的 git commit；git 不可用时为 `"unknown"`。
pub const COMMIT: &str = env!("OWO_BUILD_COMMIT");

/// 编译期烧录的 dirty 标记；git 不可用时保守为 `true`（不谎报干净构建）。
/// 注意：常量上下文不能对 `str` 做 `match`/`==`（PartialEq 尚非 const trait），
/// 因此用 const fn 做字节级比较。
pub const DIRTY: bool = dirty_from_flag(env!("OWO_BUILD_DIRTY"));

/// workspace 包版本（单一来源 = workspace.package.version）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// HTTP/SSE 协议主版本（§5.1；server 的 OWO_API_VERSION 与 deprecation 头
/// 从此处取值，release manifest 同构报告）。改动时 route contract 的
/// health/版本双测试会拦截漂移。
pub const API_VERSION: &str = "0.7";

/// const 上下文可用的字节级 `flag == "true"` 判定。
const fn dirty_from_flag(flag: &str) -> bool {
    let bytes = flag.as_bytes();
    bytes.len() == 4 && bytes[0] == b't' && bytes[1] == b'r' && bytes[2] == b'u' && bytes[3] == b'e'
}

/// commit 是否可信（非空且非 `"unknown"`）。
pub fn has_commit() -> bool {
    !COMMIT.is_empty() && COMMIT != "unknown"
}

/// 构建时刻的 Unix 秒（build.rs 运行时刻）；解析失败为 0，消费方据此跳过时间展示。
pub fn built_at_epoch() -> u64 {
    env!("OWO_BUILD_EPOCH").parse().unwrap_or(0)
}

/// 构建时刻 RFC3339（UTC `Z` 后缀）。epoch 非法（0）时 None。
pub fn built_at_rfc3339() -> Option<String> {
    rfc3339_from_epoch(built_at_epoch())
}

/// Unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`（Howard Hinnant civil-from-days；
/// 纯算术无依赖，1970 起域内精确）。
pub fn rfc3339_from_epoch(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    Some(format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
}

/// 身份来源（诊断展示用：同一 commit 值可能来自不同链级，发布排障必须可区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentitySource {
    /// `OWO_BUILD_INFO` 指向的 build-info.json（发布链/调试覆写）。
    OverrideFile,
    /// 编译期烧录（正常构建的权威源）。
    Compiled,
    /// cwd 下遗留 build-info.json（最后手段，可能过期）。
    WorkspaceFile,
    /// 全部缺失（commit=unknown）。
    Unavailable,
}

/// 统一构建身份（§7.1：/health、--version、doctor 与 release manifest 同构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    pub commit: String,
    pub dirty: bool,
    /// RFC3339（UTC Z）；无有效 epoch 时为空串（正式构建被 build.rs 拦截）。
    pub built_at: String,
    /// 协议主版本（与 /health.api_version、Deprecation 头同源）。
    pub api_version: &'static str,
    /// workspace 包版本。
    pub version: &'static str,
    pub source: IdentitySource,
}

impl BuildIdentity {
    /// 单行摘要（CLI --version / doctor / 日志共用渲染）。
    pub fn oneline(&self) -> String {
        let commit = if self.commit.is_empty() {
            "unknown"
        } else {
            &self.commit
        };
        format!(
            "{} api={} commit={} dirty={} built_at={} source={}",
            self.version,
            self.api_version,
            commit,
            self.dirty,
            if self.built_at.is_empty() {
                "unknown"
            } else {
                &self.built_at
            },
            match self.source {
                IdentitySource::OverrideFile => "override-file",
                IdentitySource::Compiled => "compiled",
                IdentitySource::WorkspaceFile => "workspace-file",
                IdentitySource::Unavailable => "unavailable",
            }
        )
    }
}

/// 覆写文件环境变量名（发布链/调试统一入口）。
pub const BUILD_INFO_ENV: &str = "OWO_BUILD_INFO";

/// 解析构建身份（§7.1 单一链，server/CLI/manifest 消费方必须走此函数）：
/// ① `OWO_BUILD_INFO` 显式文件（发布链覆写，最高优先）；
/// ② 编译期烧录（与二进制严格对应）；
/// ③ cwd 下 build-info.json（开发遗留，可能过期）；
/// ④ Unavailable（commit=unknown，不谎报）。
pub fn identity() -> BuildIdentity {
    static CACHE: std::sync::OnceLock<BuildIdentity> = std::sync::OnceLock::new();
    CACHE.get_or_init(identity_uncached).clone()
}

fn identity_uncached() -> BuildIdentity {
    let override_path = std::env::var(BUILD_INFO_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    if let Some(path) = override_path {
        if let Some(parsed) = parse_build_info_file(&path) {
            return parsed;
        }
    }
    if has_commit() {
        return BuildIdentity {
            commit: COMMIT.to_string(),
            dirty: DIRTY,
            built_at: built_at_rfc3339().unwrap_or_default(),
            api_version: API_VERSION,
            version: VERSION,
            source: IdentitySource::Compiled,
        };
    }
    if let Some(parsed) = parse_build_info_file(Path::new("build-info.json")) {
        return BuildIdentity {
            source: IdentitySource::WorkspaceFile,
            ..parsed
        };
    }
    BuildIdentity {
        commit: "unknown".to_string(),
        dirty: true,
        built_at: String::new(),
        api_version: API_VERSION,
        version: VERSION,
        source: IdentitySource::Unavailable,
    }
}

/// 读取并解析一份 build-info.json（容忍 UTF-8 BOM；字段 git_commit/
/// git_dirty/built_at，与 init-dev-env 生成器对齐）。缺失/损坏 → None。
pub fn parse_build_info_file(path: &Path) -> Option<BuildIdentity> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let commit = value["git_commit"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    // 覆写文件里的空/unknown commit = 过期垃圾产物：视为不可用，让调用链
    // 回落到编译期事实（宁可不覆写，不以文件旧值冒充当前二进制身份）。
    if commit.is_empty() || commit == "unknown" {
        return None;
    }
    Some(BuildIdentity {
        commit,
        // dirty 字段缺失按 true 处理：未知状态不谎报干净构建。
        dirty: value["git_dirty"].as_bool().unwrap_or(true),
        built_at: value["built_at"].as_str().unwrap_or_default().to_string(),
        // version/api_version 恒取编译期值：外部文件无权改写二进制自身的
        // 版本与协议主张（文件只允许覆写 git 身份三元组）。
        api_version: API_VERSION,
        version: VERSION,
        source: IdentitySource::OverrideFile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_commit_matches_const_contract() {
        assert_eq!(has_commit(), !COMMIT.is_empty() && COMMIT != "unknown");
    }

    #[test]
    fn dirty_is_boolean_of_burned_flag() {
        assert_eq!(DIRTY, dirty_from_flag(env!("OWO_BUILD_DIRTY")));
        assert!(dirty_from_flag("true"));
        assert!(!dirty_from_flag("false"));
        assert!(!dirty_from_flag("TRUE"));
        assert!(!dirty_from_flag("tru"));
    }

    #[test]
    fn built_at_epoch_is_zero_or_valid_unix_seconds() {
        let epoch = built_at_epoch();
        // build.rs 总是烧录合法 u64；0 仅出现在极端异常路径（消费方须容忍）。
        assert!(epoch == 0 || epoch > 1_600_000_000, "非法纪元秒：{epoch}");
    }

    #[test]
    fn rfc3339_conversion_has_known_vectors() {
        assert_eq!(rfc3339_from_epoch(0), None);
        assert_eq!(
            rfc3339_from_epoch(1),
            Some("1970-01-01T00:00:01Z".to_string())
        );
        assert_eq!(
            rfc3339_from_epoch(1_709_208_000),
            Some("2024-02-29T12:00:00Z".to_string()),
            "闰日向量"
        );
        assert_eq!(
            rfc3339_from_epoch(1_600_000_000),
            Some("2020-09-13T12:26:40Z".to_string())
        );
    }

    #[test]
    fn identity_uses_compiled_source_on_normal_build() {
        let id = identity();
        assert_eq!(id.api_version, API_VERSION);
        assert_eq!(id.version, VERSION);
        if has_commit() {
            assert_eq!(id.commit, COMMIT);
            assert_eq!(id.dirty, DIRTY);
            // ②编译期优先于③cwd（本测试环境一般未设 OWO_BUILD_INFO）。
            assert_ne!(id.source, IdentitySource::WorkspaceFile);
            assert!(!id.built_at.is_empty() || built_at_epoch() == 0);
        }
    }

    #[test]
    fn parse_build_info_file_is_bom_tolerant_and_field_strict() {
        let dir = std::env::temp_dir().join(format!("owo-build-info-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bi.json");
        std::fs::write(
            &path,
            "\u{feff}{\"app_version\":\"0.1.0\",\"git_commit\":\"abc123\",\"git_dirty\":false,\"built_at\":\"2026-09-16T00:00:00Z\"}",
        )
        .unwrap();
        let parsed = parse_build_info_file(&path).expect("应解析成功");
        assert_eq!(parsed.commit, "abc123");
        assert!(!parsed.dirty);
        assert_eq!(parsed.built_at, "2026-09-16T00:00:00Z");
        assert_eq!(parsed.source, IdentitySource::OverrideFile);
        // 损坏 JSON → None（不 panic）。
        std::fs::write(&path, "not-json").unwrap();
        assert!(parse_build_info_file(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oneline_renders_all_fields() {
        let id = BuildIdentity {
            commit: "deadbeef".into(),
            dirty: false,
            built_at: "2026-09-16T00:00:00Z".into(),
            api_version: API_VERSION,
            version: "0.1.0",
            source: IdentitySource::Compiled,
        };
        let line = id.oneline();
        for token in [
            "0.1.0",
            "api=0.7",
            "commit=deadbeef",
            "dirty=false",
            "source=compiled",
        ] {
            assert!(line.contains(token), "缺 {token}：{line}");
        }
    }
}
