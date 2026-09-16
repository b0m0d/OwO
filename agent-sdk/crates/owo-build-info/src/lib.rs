//! §6.1.2 共享构建身份常量（单一实现）。
//!
//! build.rs 在编译期烧录 git 身份；server `/health.build`、cli `core_ready`
//! 的 build_id 与 desktop 壳的期望值比对全部经本 crate 读取。无依赖、无
//! 运行时回退逻辑——回退链（`OWO_BUILD_INFO` / cwd build-info.json）由
//! 消费方（server/cli）自行实现。

/// 编译期烧录的 git commit；git 不可用时为 `"unknown"`。
pub const COMMIT: &str = env!("OWO_BUILD_COMMIT");

/// 编译期烧录的 dirty 标记；git 不可用时保守为 `true`（不谎报干净构建）。
/// 注意：常量上下文不能对 `str` 做 `match`/`==`（PartialEq 尚非 const trait），
/// 因此用 const fn 做字节级比较。
pub const DIRTY: bool = dirty_from_flag(env!("OWO_BUILD_DIRTY"));

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
}
