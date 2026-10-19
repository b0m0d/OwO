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
//!
//! R3 收口（实测发现的 R2 缺陷）：此前 `git -C <agent-sdk>` 配 pathspec
//! `-- agent-sdk`，pathspec 相对 `-C` 目录解析成 `agent-sdk/agent-sdk`
//! → 永远为空 → **dirty 恒为 false**（release clean-tree 门形同虚设）；
//! 且 `rerun-if-changed` 指向 `agent-sdk/.git/…`（.git 实际在仓根）→ 路径
//! 不存在 → 每次构建都重跑。现改为：作用域用 `-- .`（与 SDK 目录名解耦），
//! 触发路径用 `git rev-parse --show-toplevel` 解析出的真实 `.git`。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    // crates/<crate> → agent-sdk（构建作用域根）。
    let sdk_root = manifest_dir.join("..").join("..");

    // 真实 git 目录（绝对、兼容 worktree）；解析失败则保守地不上报触发路径（=每次重跑）。
    if let Some(git_dir_raw) = git(&sdk_root, &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir_raw);
        // ⚠ 触发路径必须是**提交时会变的文件**。两处历史缺陷：
        // 1) 原先上报 `.git/HEAD`：分支上 HEAD 内容只是 `ref: refs/heads/<branch>`
        //    这行符号引用，提交只改引用文件本身，HEAD 的 mtime 不动 → 身份跨提交
        //    不刷新。实测：连改两笔提交后 `--version` 仍报旧 commit 与旧 built_at
        //    （`.git/HEAD` mtime 停在 08-11，而 index/引用文件都是本次提交时刻），
        //    于是"壳与 core build id 一致"的自证其实一致地错着。
        // 2) 第一版修法用 `rev-parse --git-path HEAD` 拼 `show-toplevel`：该输出是
        //    **相对调用目录**（`../.git/HEAD`），拼错成不存在的绝对路径 → Cargo
        //    无法 stat → 变成"每次无条件重跑"（全量重建，另一个方向的错）。
        // 现在：符号引用解析成引用文件本身（detached 才回退 HEAD），全部基于
        // `--absolute-git-dir`，并带上 `index`（dirty 状态）与 `packed-refs`
        // （分支被 `git pack-refs` 打包时引用搬家）。
        let mut triggers: Vec<PathBuf> = Vec::new();
        // 用**完整**引用名（`refs/heads/<branch>`）：`--short` 会剥掉 `refs/heads/`
        // 前缀，拼出来的是不存在的路径（实测会静默退回 HEAD 分支，等于没修）。
        match git(&sdk_root, &["symbolic-ref", "--quiet", "HEAD"]) {
            Some(branch) if !branch.is_empty() => {
                let mut ref_path = git_dir.clone();
                for part in branch.split('/') {
                    ref_path = ref_path.join(part);
                }
                if ref_path.exists() {
                    triggers.push(ref_path);
                } else {
                    // 分支被打包进 packed-refs：改盯 HEAD + packed-refs（下面兜底）。
                    triggers.push(git_dir.join("HEAD"));
                }
            }
            // detached HEAD：HEAD 文件自身就是提交号，会随 checkout/commit 变化。
            _ => triggers.push(git_dir.join("HEAD")),
        }
        triggers.push(git_dir.join("index"));
        let packed_refs = git_dir.join("packed-refs");
        if packed_refs.exists() {
            triggers.push(packed_refs);
        }
        for path in triggers {
            println!(
                "cargo:rerun-if-changed={}",
                path.to_string_lossy().replace('\\', "/")
            );
        }
    }
    println!("cargo:rerun-if-env-changed=OWO_ALLOW_DIRTY_RELEASE");

    let commit = git(&sdk_root, &["rev-parse", "HEAD"])
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    // pathspec `.` = `-C` 目录本身（agent-sdk），与 SDK 在仓库中的名字/深度解耦。
    let dirty = match git(&sdk_root, &["status", "--porcelain", "-uall", "--", "."]) {
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
