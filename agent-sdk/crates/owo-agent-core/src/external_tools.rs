//! 随发行包提供的只读外部工具解析。
//!
//! 生产包不把成熟的文件搜索器重新实现一遍，而是携带经过固定版本和哈希
//! 校验的 ripgrep。解析只看随包目录或显式测试覆盖目录，不读取用户 PATH 来
//! 决定核心搜索器，避免开发机上另一个 rg 改变产品行为。

use std::path::{Path, PathBuf};

pub const RIPGREP_VERSION: &str = "14.1.1";
pub const RIPGREP_SHA256: &str = "F162B54DE2ADFC72D78ADB1DBADA2DEDDA111AE0A5E2F6E9500F4F909664C5D2";

fn tool_file(root: &Path, name: &str) -> Option<PathBuf> {
    let direct = root.join(format!("{name}.exe"));
    if direct.is_file() {
        return Some(direct);
    }
    let nested = root.join(name).join(format!("{name}.exe"));
    nested.is_file().then_some(nested)
}

fn candidate_roots(exe: &Path) -> impl Iterator<Item = PathBuf> {
    let mut roots = Vec::new();
    let mut cursor = exe.parent().map(Path::to_path_buf);
    for _ in 0..8 {
        let Some(dir) = cursor else { break };
        roots.push(dir.join("tools"));
        roots.push(dir.join("resources").join("tools"));
        roots.push(dir.join("resources"));
        cursor = dir.parent().map(Path::to_path_buf);
    }
    roots.into_iter()
}

/// 解析一个随包工具；`OWO_EXTERNAL_TOOLS_DIR` 仅用于测试和便携部署覆盖。
pub fn resolve_tool(name: &str) -> Option<PathBuf> {
    if let Some(override_dir) = std::env::var_os("OWO_EXTERNAL_TOOLS_DIR") {
        if let Some(path) = tool_file(Path::new(&override_dir), name) {
            return Some(path);
        }
    }

    let exe = std::env::current_exe().ok()?;
    candidate_roots(&exe).find_map(|root| tool_file(&root, name))
}

pub fn resolve_ripgrep() -> Option<PathBuf> {
    resolve_tool("rg")
}

/// 返回把随包工具放在 PATH 最前面的环境值，供受沙箱约束的 `cmd /C` 使用。
pub fn path_with_bundled_tools() -> Option<String> {
    let tool_dir = resolve_ripgrep()?.parent()?.to_string_lossy().into_owned();
    let inherited = std::env::var_os("PATH")
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(if inherited.is_empty() {
        tool_dir
    } else {
        format!("{tool_dir};{inherited}")
    })
}

/// 只为单元测试暴露无副作用的候选路径行为。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_file_accepts_nested_release_layout() {
        let root = std::env::temp_dir().join(format!("owo-tool-layout-{}", uuid::Uuid::new_v4()));
        let nested = root.join("rg");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("rg.exe"), b"test").unwrap();
        assert_eq!(tool_file(&root, "rg"), Some(nested.join("rg.exe")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn manifest_identity_is_pinned() {
        assert_eq!(RIPGREP_VERSION, "14.1.1");
        assert_eq!(RIPGREP_SHA256.len(), 64);
    }
}
