//! auth_token 契约测试（R7 X03）：生成/持久化/复用/恒定时间校验/ACL/引导端点。
//!
//! 独立编译目标：`auth_token.rs` 不引用 crate::/super::，本文件用 #[path] 挂载。
//! 存储全部落在 tempfile 临时目录。

#[path = "../src/auth_token.rs"]
mod auth_token;

use auth_token::AuthToken;
use std::path::Path;

/// 测试临时 data_root。
fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    (temp, root)
}

#[test]
fn generates_random_long_tokens() {
    let a = AuthToken::generate();
    let b = AuthToken::generate();
    assert_ne!(a.token(), b.token());
    assert!(a.token().len() >= 64, "256 位 hex 至少 64 字符");
}

#[test]
fn mints_and_persists_token_per_boot() {
    let (_temp, root) = temp_root();
    let created = AuthToken::mint_for_boot(&root);
    let path = AuthToken::file_path(&root);
    assert!(path.is_file(), "token 文件应已创建");
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk.trim(), created.token());

    let next = AuthToken::mint_for_boot(&root);
    assert_ne!(
        next.token(),
        created.token(),
        "§8.2：core 重启必须换发新 bearer（旧值不得跨代际复用）"
    );
    let rotated = std::fs::read_to_string(&path).unwrap();
    assert_eq!(rotated.trim(), next.token(), "文件必须反映当前代际");
    assert!(next.acl_warning().is_none());
    assert!(!created.verify(next.token()), "新 token 不应被旧值命中");
}

/// §8.2 第 5 条：上一代进程泄漏的 bearer 在新代际上必须被拒（401 的判定源）。
#[test]
fn previous_generation_token_is_rejected_after_reboot() {
    let (_temp, root) = temp_root();
    let old = AuthToken::mint_for_boot(&root);
    let fresh = AuthToken::mint_for_boot(&root);
    assert!(fresh.verify(fresh.token()));
    assert!(
        !fresh.verify(old.token()),
        "旧代际 token 必须失效（否则重启不构成凭据轮换）"
    );
}

/// 文件里预先存在的陌生值（别的安装/别的机器/被篡改）不得被继承。
#[test]
fn does_not_inherit_pre_existing_token_file() {
    let (_temp, root) = temp_root();
    let path = AuthToken::file_path(&root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "pre-existing-32-char-token-abcdef123456").unwrap();
    let loaded = AuthToken::mint_for_boot(&root);
    assert_ne!(loaded.token(), "pre-existing-32-char-token-abcdef123456");
    assert!(loaded.token().len() >= 64);
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk.trim(), loaded.token(), "预置值必须被覆盖");
}

#[test]
fn overwrites_corrupt_or_empty_token_file() {
    let (_temp, root) = temp_root();
    let path = AuthToken::file_path(&root);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "   ").unwrap();
    let loaded = AuthToken::mint_for_boot(&root);
    assert!(loaded.token().len() >= 64, "空文件应重新生成");
}

#[test]
fn falls_back_to_memory_token_when_unwritable() {
    // data_root 指向一个“文件”而非目录 → 目录创建/写入必然失败 → 内存 token 兜底。
    let (_temp, root) = temp_root();
    let blocker = root.join("blocker");
    std::fs::write(&blocker, "file-not-dir").unwrap();
    let loaded = AuthToken::mint_for_boot(&blocker);
    assert!(loaded.token().len() >= 64, "降级也应返回可用 token");
    assert!(loaded.acl_warning().is_some(), "应携带降级警告");
}

#[test]
fn verify_accepts_exact_token_rejects_others() {
    let token = AuthToken::generate();
    assert!(token.verify(token.token()));
    assert!(!token.verify("wrong-token"));
    assert!(!token.verify(""));
    assert!(!token.verify(&token.token()[..token.token().len() - 1]));
}

#[test]
fn verify_header_parses_bearer_prefix() {
    let token = AuthToken::generate();
    let ok = axum::http::HeaderValue::from_str(&format!("Bearer {}", token.token())).unwrap();
    let bad = axum::http::HeaderValue::from_str("Bearer nope").unwrap();
    let plain = axum::http::HeaderValue::from_str(token.token()).unwrap();
    assert!(token.verify_header(Some(&ok)));
    assert!(!token.verify_header(Some(&bad)));
    assert!(!token.verify_header(Some(&plain)), "无 Bearer 前缀应拒绝");
    assert!(!token.verify_header(None));
}

#[test]
fn desktop_pairing_proof_requires_exact_header_value() {
    use axum::http::HeaderValue;

    let expected = "0123456789abcdef0123456789abcdef";
    let valid = HeaderValue::from_static("0123456789abcdef0123456789abcdef");
    let wrong = HeaderValue::from_static("0123456789abcdef0123456789abcdee");
    assert!(auth_token::verify_desktop_pairing(Some(&valid), expected));
    assert!(!auth_token::verify_desktop_pairing(Some(&wrong), expected));
    assert!(!auth_token::verify_desktop_pairing(None, expected));
}

/// R8 一次性引导证明全矩阵（纯函数）：无证明（开发模式）放行；有证明时
/// 头必须恒定时间精确匹配；缺失/错误/长度不同均拒绝——不可用“任意值”绕过。
#[test]
fn pairing_gate_full_matrix() {
    use axum::http::HeaderValue;

    let secret = "0123456789abcdef0123456789abcdef";
    let valid = HeaderValue::from_static("0123456789abcdef0123456789abcdef");
    let wrong = HeaderValue::from_static("0123456789abcdef0123456789abcdee");
    let short = HeaderValue::from_static("short");

    // 开发模式（无配对证明）：缺头也放行（浏览器调试兼容）。
    assert!(auth_token::pairing_gate_allows(None, None));
    assert!(auth_token::pairing_gate_allows(None, Some(&valid)));

    // 发布模式（有配对证明）：只有精确匹配放行。
    assert!(auth_token::pairing_gate_allows(Some(secret), Some(&valid)));
    assert!(!auth_token::pairing_gate_allows(Some(secret), None));
    assert!(!auth_token::pairing_gate_allows(Some(secret), Some(&wrong)));
    assert!(!auth_token::pairing_gate_allows(Some(secret), Some(&short)));
}

#[test]
fn public_path_classification() {
    assert!(auth_token::is_public_path("/health"));
    assert!(auth_token::is_public_path("/openapi.json"));
    assert!(auth_token::is_public_path("/auth/token"));
    assert!(!auth_token::is_public_path("/session"));
}

/// Windows：token 文件 ACL 应仅含当前用户（继承移除后无 BUILTIN\Users 授权）。
#[cfg(windows)]
#[test]
fn token_file_acl_is_user_only() {
    let (_temp, root) = temp_root();
    let created = AuthToken::mint_for_boot(&root);
    assert!(
        created.acl_warning().is_none(),
        "ACL 应用不应失败：{:?}",
        created.acl_warning()
    );
    let path = AuthToken::file_path(&root);
    let output = std::process::Command::new("icacls")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    assert!(
        !text.contains("buil\\users"),
        "BUILTIN\\Users 不应有权限：{text}"
    );
    assert!(text.contains("(f)"), "当前用户应有完全控制：{text}");
}

/// ACL 函数本身：对临时文件执行收紧应成功。
#[cfg(windows)]
#[test]
fn apply_acl_on_arbitrary_file_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("secret.txt");
    std::fs::write(&file, "s").unwrap();
    auth_token::apply_user_only_acl(Path::new(&file)).expect("icacls 收紧应成功");
}
