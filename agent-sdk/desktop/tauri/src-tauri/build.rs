//! 桌面壳构建脚本。
//!
//! §7.1/§8.1 收口：核心 API 版本的**唯一源码**是 `owo-build-info::API_VERSION`
//! （与 `/health.api_version`、`/openapi.json` 顶层 `x-owo-api-version`、
//! Deprecation 头同源）。本脚本从该 crate 源码读取字面量并注入
//! `OWO_CORE_API_VERSION`，同时**强制**核心服务侧只能 re-export（禁止再出现
//! 第二处硬编码字面量，防止壳与核心版本口径漂移）。

use std::path::{Path, PathBuf};

/// 从源码中抽取 `pub const <name>: &str = "<value>";` 的字面量。
fn string_const(source: &str, name: &str) -> Option<String> {
    let marker = format!("pub const {name}: &str = \"");
    let tail = source.split(marker.as_str()).nth(1)?;
    let value = tail.split('"').next()?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// `src-tauri` → `tauri` → `desktop` → `agent-sdk`（三包上溯即 SDK 根）。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(PathBuf::from)
        .expect("无法定位 agent-sdk 根目录")
}

/// 跑 `<sidecar> --version` 取身份行（失败返回 None）。
fn sidecar_identity(path: &Path) -> Option<String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn field<'a>(identity: &'a str, key: &str) -> Option<&'a str> {
    identity
        .split_whitespace()
        .find(|token| token.starts_with(&format!("{key}=")))
        .map(|token| &token[key.len() + 1..])
}

/// externalBin 随包 sidecar 的构建期门禁（§7.3 / R3 实测缺陷收口）。
///
/// Tauri 的 externalBin 约定文件名是 `binaries/<name>-<target-triple>.exe`。
/// 名字写错（实测存在 `owo-agent.exe-x86_64-pc-windows-msvc.exe`）不会被
/// tauri-build 消费，于是发布链**静默打包上一次留下的旧 sidecar**——壳的同目录
/// 优先解析又会让开发运行也被它劫持。这类"看起来成功了的错包"必须编译期失败。
fn check_bundled_sidecar(manifest_dir: &Path, target: &str, profile: &str, api_version: &str) {
    let dir = manifest_dir.join("binaries");
    if !dir.is_dir() {
        return; // 未预置随包 core：开发构建走 SDK debug 产物，合法。
    }
    let expected = format!("owo-agent-{target}.exe");
    let entries: Vec<std::path::PathBuf> = match std::fs::read_dir(&dir) {
        Ok(read) => read
            .filter_map(|entry| entry.ok().map(|item| item.path()))
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().starts_with("owo-agent"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return,
    };
    // 错误命名的遗留物：直接失败，不允许"多放一个文件"继续骗过打包链。
    for entry in &entries {
        let name = entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name != expected && name.starts_with("owo-agent.exe-") {
            panic!(
                "binaries\\{name} 命名不符合 Tauri externalBin 约定（应为 {expected}）：\
                 该文件不会被打包，安装包会静默使用另一份残留 core。请删除并改用正确文件名。"
            );
        }
    }
    let Some(path) = entries.iter().find(|entry| {
        entry
            .file_name()
            .map(|name| name == expected.as_str())
            .unwrap_or(false)
    }) else {
        return; // 只有其它无关文件：交给上面的命名守卫与打包脚本处理。
    };

    let identity = sidecar_identity(path).unwrap_or_else(|| {
        panic!(
            "随包 sidecar 无法执行 `--version`：{}（发布链 §7.3）",
            path.display()
        )
    });
    let Some(commit) = field(&identity, "commit") else {
        panic!(
            "随包 sidecar 缺少构建身份（commit=）：{} —— 这是构建身份链之前的历史产物，\
             不得进安装包，也不得留在 binaries/ 里劫持开发运行（identity: {identity}）",
            path.display()
        )
    };
    let core_api = field(&identity, "api").unwrap_or("unknown");
    assert_eq!(
        core_api,
        api_version,
        "随包 sidecar 的 API 版本({core_api})与壳期望({api_version})不一致（§7.1 单源）：{}",
        path.display()
    );
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if !head.is_empty() && commit != head {
        let message = format!(
            "随包 sidecar 构建 commit={commit} 与当前 HEAD={head} 不一致（core 与壳不同世代）"
        );
        if profile == "release" {
            panic!(
                "{message}——发布构建被拒绝；先重新 stage sidecar（scripts\\build-installer.ps1）"
            );
        }
        println!("cargo:warning={message}——开发构建继续，但壳日志会记录 build id 错配");
    }
}

/// 随包只读外部工具的构建期门禁。
///
/// `search_files` 的实现依赖固定版本 ripgrep；如果资源目录缺文件，开发构建
/// 也应尽早失败，不能等用户安装后才退化成“工具不可用”。Tauri 的 resources
/// 映射负责复制，下面的门禁负责保证输入完整。
fn check_bundled_external_tools(root: &Path, manifest_dir: &Path) {
    let tool_dir = root.join("tools").join("rg");
    let required = [
        "rg.exe",
        "manifest.json",
        "COPYING",
        "LICENSE-MIT",
        "UNLICENSE",
        "README.md",
    ];
    for name in required {
        let path = tool_dir.join(name);
        assert!(
            path.is_file(),
            "随包 ripgrep 资源缺失：{}（请恢复 agent-sdk\\tools\\rg 完整发行目录）",
            path.display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            path.to_string_lossy().replace('\\', "/")
        );
    }
    let manifest = std::fs::read_to_string(tool_dir.join("manifest.json"))
        .expect("读取随包 ripgrep manifest.json 失败");
    assert!(
        manifest.contains("\"version\": \"14.1.1\""),
        "随包 ripgrep 版本清单不是受支持的 14.1.1"
    );
    assert!(
        manifest.contains("F162B54DE2ADFC72D78ADB1DBADA2DEDDA111AE0A5E2F6E9500F4F909664C5D2"),
        "随包 ripgrep SHA-256 清单不匹配"
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("tauri.conf.json").to_string_lossy().replace('\\', "/")
    );
}

fn main() {
    let root = repo_root();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let build_info = root
        .join("crates")
        .join("owo-build-info")
        .join("src")
        .join("lib.rs");
    let server_lib = root
        .join("crates")
        .join("owo-agent-server")
        .join("src")
        .join("lib.rs");
    println!(
        "cargo:rerun-if-changed={}",
        build_info.to_string_lossy().replace('\\', "/")
    );
    println!(
        "cargo:rerun-if-changed={}",
        server_lib.to_string_lossy().replace('\\', "/")
    );

    let build_info_source =
        std::fs::read_to_string(&build_info).expect("读取 owo-build-info 源文件失败");
    let version = string_const(&build_info_source, "API_VERSION")
        .expect("无法从 owo-build-info::API_VERSION 读取 API 版本字面量");

    // 单源守卫：核心服务必须 re-export 同一常量，不得自带字面量。
    let server_source = std::fs::read_to_string(&server_lib).expect("读取核心服务源文件失败");
    assert!(
        server_source.contains("pub const OWO_API_VERSION: &str = owo_build_info::API_VERSION;"),
        "核心服务 OWO_API_VERSION 必须 re-export owo_build_info::API_VERSION（§7.1 单源）"
    );
    assert!(
        string_const(&server_source, "OWO_API_VERSION").is_none(),
        "核心服务源码中禁止再出现 API 版本字面量（会与壳/核心口径漂移）"
    );

    println!("cargo:rustc-env=OWO_CORE_API_VERSION={version}");

    // 随包 sidecar 门禁（binaries/ 变化即重跑：目录内容就是本门的输入）。
    let binaries = manifest_dir.join("binaries");
    println!(
        "cargo:rerun-if-changed={}",
        binaries.to_string_lossy().replace('\\', "/")
    );
    check_bundled_sidecar(
        &manifest_dir,
        &std::env::var("TARGET").unwrap_or_default(),
        &std::env::var("PROFILE").unwrap_or_default(),
        &version,
    );
    check_bundled_external_tools(&root, &manifest_dir);
    tauri_build::build()
}
