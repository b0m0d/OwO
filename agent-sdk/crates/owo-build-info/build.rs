//! §6.1.2 共享构建身份：编译期烧录 git commit/dirty/built_at（单一实现）。
//!
//! server、cli 与 desktop 壳统一消费本 crate，不再各持一份重复 build.rs。
//! 与仓库内 build-info.json（每机生成、易过期、曾导致 core 报旧 build_id）
//! 解耦：运行时回退链（OWO_BUILD_INFO / cwd build-info.json）属于消费方职责，
//! 本 crate 只提供编译期事实。git 不可用时 commit="unknown"、dirty=true
//! （保守降级，不谎报干净构建）。
//!
//! dirty 口径（§7.3，与 init-dev-env Assert-OwoCleanTree、release manifest
//! 一致）：**构建作用域 = agent-sdk/**——tracked 改动 + agent-sdk 内未跟踪
//! 文件都算脏；仓根的个人文档/素材等构建不相关 untracked 资产不算脏
//! （否则该机永远无法产出 clean release，门禁形同虚设）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // 提交/暂存变化时重跑（index 变化覆盖 commit 与 dirty 状态变化）。
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // crates/<crate> → 仓库根（git -C 自会向上解析到真实仓库）。
    let repo_root = manifest_dir.join("..").join("..");

    let commit = git(&repo_root, &["rev-parse", "HEAD"])
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = match git(
        &repo_root,
        &["status", "--porcelain", "-uall", "--", "agent-sdk"],
    ) {
        Some(output) => !output.trim().is_empty(),
        None => true,
    };
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // §7.3 发布构建约束：release 入口拒绝不可信身份。
    // commit 不可解析 / 工作树 dirty / built_at 缺失都会产出一个无法对应
    // clean commit 的正式产物——直接失败并给出可操作提示。唯一豁免：
    // OWO_ALLOW_DIRTY_RELEASE=1（本地调试用，manifest 侧另有 clean-tree 门）。
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let allow_dirty = std::env::var("OWO_ALLOW_DIRTY_RELEASE").as_deref() == Ok("1");
    if profile == "release" && !allow_dirty {
        if commit == "unknown" {
            panic!("release 构建被拒绝：git commit 不可解析（干净 checkout + git 在 PATH）；本地调试可设 OWO_ALLOW_DIRTY_RELEASE=1");
        }
        if dirty {
            panic!("release 构建被拒绝：工作树存在未提交改动，正式产物必须来自 clean commit（方案 §4.5）；本地调试可设 OWO_ALLOW_DIRTY_RELEASE=1");
        }
        if epoch == 0 {
            panic!(
                "release 构建被拒绝：built_at 无法确定（系统时钟异常）；正式构建不得为空（§7.1）"
            );
        }
    }

    println!("cargo:rustc-env=OWO_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=OWO_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=OWO_BUILD_EPOCH={epoch}");
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
