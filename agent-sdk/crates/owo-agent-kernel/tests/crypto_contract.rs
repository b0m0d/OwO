//! 存储加密契约测试（P0-1：取代已删除的假验证脚本 `validation_report.rs` / `final_verification.rs`）。
//!
//! 只调用 `storage_crypto` 的**公开 API**，覆盖技术文档声称的 5 项契约：
//! 1. v4 信封 round-trip（写入 → 读回一致）；
//! 2. 相同明文两次加密产生不同密文（随机 nonce）；
//! 3. 篡改拒绝（nonce / 密文 / 标签任一字节被改，解密显式报错）；
//! 4. 错误 DEK 拒绝；
//! 5. v1 / v2 / v3 历史格式兼容读取。
//!
//! v2/v3 夹具按信封格式手构（`legacy_xor_crypt` 派生流 + HMAC-SHA256 认证），
//! 构造口径与 `src/storage_crypto.rs` 单元测试及解密实现一致。
//!
//! 运行：`cargo test -p owo-agent-core --test crypto_contract`
//!
//! 整个文件 gate 在 Windows：信封的 DEK 段由 DPAPI 保护（`protect_dek`/`unprotect_dek`），
//! 非 Windows 上这些 API 显式返回 `Unsupported`（禁止静默降级），契约无从验证。

#![cfg(windows)]

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use owo_agent_kernel::storage_crypto::{
    decrypt_file_envelope, decrypt_file_envelope_with_dek, decrypt_with_dek, encrypt_blob,
    encrypt_file_envelope, encrypt_file_envelope_with_dek, encrypt_with_dek, generate_dek,
    protect_dek, unprotect_dek, DEK_LEN, ENVELOPE_MAGIC, ENVELOPE_VERSION_V1_DPAPI,
    ENVELOPE_VERSION_V2_LEGACY, ENVELOPE_VERSION_V3_LEGACY, ENVELOPE_VERSION_V4_AEAD,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "owo_crypto_contract_{}_{}_{}",
        tag,
        std::process::id(),
        n
    ))
}

/// v2/v3 的固定派生 XOR 流（口径与 `legacy_xor_crypt` 一致）。
fn legacy_xor_stream(dek: &[u8; DEK_LEN]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(dek);
    hasher.update(b"owo-envelope-stream-v2");
    hasher.finalize().into()
}

/// v3 认证标签（口径与 `legacy_v3_verify` 一致）：
/// HMAC-SHA256(key = SHA256(dek ‖ "owo-envelope-auth-v3"),
///             msg = version ‖ dek_len ‖ protected_dek ‖ data_len ‖ data_cipher)。
fn legacy_v3_tag(dek: &[u8; DEK_LEN], protected_dek: &[u8], data_cipher: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(dek);
    hasher.update(b"owo-envelope-auth-v3");
    let auth_key = hasher.finalize();
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&auth_key).expect("HMAC 密钥长度恒合法");
    mac.update(&[ENVELOPE_VERSION_V3_LEGACY]);
    mac.update(&(protected_dek.len() as u32).to_le_bytes());
    mac.update(protected_dek);
    mac.update(&(data_cipher.len() as u32).to_le_bytes());
    mac.update(data_cipher);
    mac.finalize().into_bytes().into()
}

/// 解析 v4 信封，返回 (nonce 起始偏移, 密文起始偏移, 文件总长)。
fn v4_offsets(bytes: &[u8]) -> (usize, usize, usize) {
    let magic = ENVELOPE_MAGIC.len();
    let mut lb = [0u8; 4];
    lb.copy_from_slice(&bytes[magic + 1..magic + 5]);
    let dek_len = u32::from_le_bytes(lb) as usize;
    let data_len_field = magic + 1 + 4 + dek_len;
    lb.copy_from_slice(&bytes[data_len_field..data_len_field + 4]);
    let data_len = u32::from_le_bytes(lb) as usize;
    let nonce_start = data_len_field + 4;
    (nonce_start, nonce_start + 12, nonce_start + 12 + data_len)
}

// ── 契约 1：v4 round-trip ────────────────────────────────────────────────

#[test]
fn v4_envelope_round_trip() {
    let dek = generate_dek();
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"A".to_vec(),
        "中文明文也要 round-trip：灵犀 × OwO。".as_bytes().to_vec(),
        (0u8..=255u8).collect(),
        vec![7u8; 4096],
    ];
    for (i, plain) in cases.iter().enumerate() {
        let path = temp_path(&format!("rt{i}"));
        encrypt_file_envelope_with_dek(&path, plain, &dek).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..ENVELOPE_MAGIC.len()], ENVELOPE_MAGIC, "魔数");
        assert_eq!(
            bytes[ENVELOPE_MAGIC.len()],
            ENVELOPE_VERSION_V4_AEAD,
            "新写入必须是 v4"
        );
        let back = decrypt_file_envelope_with_dek(&path, &dek).unwrap();
        assert_eq!(back, *plain, "第 {i} 例 round-trip 不一致");
        std::fs::remove_file(&path).unwrap();
    }
}

#[test]
fn v4_payload_primitive_round_trip() {
    let dek = generate_dek();
    let plain = b"payload primitive: nonce || AES-256-GCM ciphertext";
    let cipher = encrypt_with_dek(plain, &dek).unwrap();
    let back = decrypt_with_dek(&cipher, &dek).unwrap();
    assert_eq!(back, plain);
}

// ── 契约 2：相同明文 → 不同密文（随机 nonce）────────────────────────────

#[test]
fn v4_same_plaintext_produces_distinct_ciphertexts() {
    let plain = b"identical plaintext, distinct ciphertext";
    let dek = generate_dek();
    let mut files = Vec::new();
    for i in 0..3 {
        let path = temp_path(&format!("distinct{i}"));
        encrypt_file_envelope_with_dek(&path, plain, &dek).unwrap();
        files.push(std::fs::read(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
    }
    assert_ne!(files[0], files[1]);
    assert_ne!(files[1], files[2]);
    assert_ne!(files[0], files[2]);

    // 载荷原语同理：nonce 段必须随机
    let c1 = encrypt_with_dek(plain, &dek).unwrap();
    let c2 = encrypt_with_dek(plain, &dek).unwrap();
    assert_ne!(c1[..12], c2[..12], "nonce 必须随机");
    assert_ne!(c1, c2);
}

// ── 契约 3：篡改拒绝 ─────────────────────────────────────────────────────

#[test]
fn v4_envelope_tamper_rejected() {
    let plain = b"tamper any byte and decryption must fail";
    let dek = generate_dek();
    let path = temp_path("tamper");
    encrypt_file_envelope_with_dek(&path, plain, &dek).unwrap();
    let original = std::fs::read(&path).unwrap();
    let (nonce_start, cipher_start, total) = v4_offsets(&original);
    let mutation_indices = [nonce_start, cipher_start, cipher_start + 1, total - 1];
    for (i, idx) in mutation_indices.iter().enumerate() {
        let mut corrupted = original.clone();
        corrupted[*idx] ^= 0xFF;
        let bad = temp_path(&format!("tamper{i}"));
        std::fs::write(&bad, &corrupted).unwrap();
        assert!(
            decrypt_file_envelope_with_dek(&bad, &dek).is_err(),
            "篡改字节 @{idx} 必须被拒绝"
        );
        std::fs::remove_file(&bad).unwrap();
    }
    // 原文件不受影响
    assert_eq!(decrypt_file_envelope_with_dek(&path, &dek).unwrap(), plain);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn v4_payload_primitive_tamper_rejected() {
    let dek = generate_dek();
    let plain = b"payload tamper case";
    let cipher = encrypt_with_dek(plain, &dek).unwrap();
    let mut bad = cipher.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0xFF;
    assert!(decrypt_with_dek(&bad, &dek).is_err());
}

// ── 契约 4：错误 DEK 拒绝 ────────────────────────────────────────────────

#[test]
fn v4_wrong_dek_rejected() {
    let plain = b"locked to dek1";
    let dek1 = generate_dek();
    let dek2 = generate_dek();
    let path = temp_path("wrongdek");
    encrypt_file_envelope_with_dek(&path, plain, &dek1).unwrap();
    assert!(
        decrypt_file_envelope_with_dek(&path, &dek2).is_err(),
        "错误 DEK 必须拒绝"
    );
    assert_eq!(decrypt_file_envelope_with_dek(&path, &dek1).unwrap(), plain);
    std::fs::remove_file(&path).unwrap();

    let cipher = encrypt_with_dek(plain, &dek1).unwrap();
    assert!(decrypt_with_dek(&cipher, &dek2).is_err());
}

#[test]
fn dek_protect_unprotect_round_trip_and_length_check() {
    let dek = generate_dek();
    let protected = protect_dek(&dek).unwrap();
    assert_eq!(unprotect_dek(&protected).unwrap(), dek);
    // 解出非 DEK 长度的内容必须显式拒绝（防截断/替换攻击）
    let not_a_dek = encrypt_blob(&[9u8; DEK_LEN - 1]).unwrap();
    assert!(unprotect_dek(&not_a_dek).is_err());
}

// ── 契约 5：v1 / v2 / v3 历史格式兼容 ────────────────────────────────────

#[test]
fn v1_dpapi_envelope_compat() {
    let plain = b"v1 dpapi self-managed envelope";
    let path = temp_path("v1");
    encrypt_file_envelope(&path, plain).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes[ENVELOPE_MAGIC.len()], ENVELOPE_VERSION_V1_DPAPI);
    // v1 专属入口
    assert_eq!(decrypt_file_envelope(&path).unwrap(), plain);
    // v1 也可经 with_dek 入口读（v1 分支不受 DEK 控制）
    assert_eq!(
        decrypt_file_envelope_with_dek(&path, &generate_dek()).unwrap(),
        plain
    );
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn v2_legacy_envelope_compat_read_only() {
    let plain = b"legacy v2 envelope (no auth, read-only migration)";
    let dek = generate_dek();
    let protected_dek = protect_dek(&dek).unwrap();
    let stream = legacy_xor_stream(&dek);
    let data_cipher: Vec<u8> = plain
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ stream[i % stream.len()])
        .collect();
    let mut envelope = Vec::new();
    envelope.extend_from_slice(ENVELOPE_MAGIC);
    envelope.push(ENVELOPE_VERSION_V2_LEGACY);
    envelope.extend_from_slice(&(protected_dek.len() as u32).to_le_bytes());
    envelope.extend_from_slice(&protected_dek);
    envelope.extend_from_slice(&data_cipher);
    let path = temp_path("v2");
    std::fs::write(&path, &envelope).unwrap();
    assert_eq!(decrypt_file_envelope_with_dek(&path, &dek).unwrap(), plain);
    assert!(decrypt_file_envelope_with_dek(&path, &generate_dek()).is_err());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn v3_legacy_envelope_compat_and_tamper_reject() {
    let plain = b"legacy v3 envelope (hmac auth, read-only migration)";
    let dek = generate_dek();
    let protected_dek = protect_dek(&dek).unwrap();
    let stream = legacy_xor_stream(&dek);
    let data_cipher: Vec<u8> = plain
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ stream[i % stream.len()])
        .collect();
    let tag = legacy_v3_tag(&dek, &protected_dek, &data_cipher);
    let mut envelope = Vec::new();
    envelope.extend_from_slice(ENVELOPE_MAGIC);
    envelope.push(ENVELOPE_VERSION_V3_LEGACY);
    envelope.extend_from_slice(&(protected_dek.len() as u32).to_le_bytes());
    envelope.extend_from_slice(&protected_dek);
    envelope.extend_from_slice(&(data_cipher.len() as u32).to_le_bytes());
    envelope.extend_from_slice(&data_cipher);
    envelope.extend_from_slice(&tag);
    let path = temp_path("v3");
    std::fs::write(&path, &envelope).unwrap();
    assert_eq!(decrypt_file_envelope_with_dek(&path, &dek).unwrap(), plain);
    assert!(decrypt_file_envelope_with_dek(&path, &generate_dek()).is_err());

    // 篡改密文字节 → HMAC 拒绝
    let mut corrupted = envelope.clone();
    let body_start = ENVELOPE_MAGIC.len() + 1 + 4 + protected_dek.len() + 4;
    corrupted[body_start + 2] ^= 0xFF;
    let bad_cipher = temp_path("v3c");
    std::fs::write(&bad_cipher, &corrupted).unwrap();
    assert!(decrypt_file_envelope_with_dek(&bad_cipher, &dek).is_err());

    // 篡改认证标签 → 拒绝
    let mut corrupted = envelope.clone();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xFF;
    let bad_tag = temp_path("v3t");
    std::fs::write(&bad_tag, &corrupted).unwrap();
    assert!(decrypt_file_envelope_with_dek(&bad_tag, &dek).is_err());

    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(&bad_cipher).unwrap();
    std::fs::remove_file(&bad_tag).unwrap();
}

#[test]
fn new_writes_only_emit_v4_or_v1() {
    let dek = generate_dek();
    let plain = b"no new write path may emit legacy v2/v3";
    let p_v4 = temp_path("onlyv4");
    encrypt_file_envelope_with_dek(&p_v4, plain, &dek).unwrap();
    assert_eq!(
        std::fs::read(&p_v4).unwrap()[ENVELOPE_MAGIC.len()],
        ENVELOPE_VERSION_V4_AEAD
    );
    let p_v1 = temp_path("onlyv1");
    encrypt_file_envelope(&p_v1, plain).unwrap();
    assert_eq!(
        std::fs::read(&p_v1).unwrap()[ENVELOPE_MAGIC.len()],
        ENVELOPE_VERSION_V1_DPAPI
    );
    std::fs::remove_file(&p_v4).unwrap();
    std::fs::remove_file(&p_v1).unwrap();
}
