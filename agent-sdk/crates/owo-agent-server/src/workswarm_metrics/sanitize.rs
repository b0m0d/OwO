use serde_json::{json, Value};

use super::util::*;

// ---------------------------------------------------------------------------
// 脱敏（诊断导出统一入口；无 regex 依赖的词级扫描）
// ---------------------------------------------------------------------------

/// 凭据类键名单（词级匹配；`*_tokens` 复数 = 用量计数，明确排除）。
const SENSITIVE_KEY_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "authorization",
];

const SENSITIVE_KEY_COMPOUNDS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "auth_header",
    "bearer_token",
];

const SECRET_TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "github_pat_",
    "xoxb-",
    "xoxa-",
    "xoxp-",
    "pat_",
    "npm_",
    "shpat_",
    "gl-",
];

/// 键名是否凭据类（`prompt_tokens`/`completion_tokens`/`total_tokens` 等用量复数不命中）。
pub fn is_sensitive_key(key: &str) -> bool {
    let lower = key.trim().to_lowercase();
    if lower.is_empty() || lower.ends_with("tokens") {
        return false;
    }
    if SENSITIVE_KEY_COMPOUNDS.iter().any(|c| lower.contains(c)) {
        return true;
    }
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| SENSITIVE_KEY_WORDS.contains(&word))
}

/// 词级脱敏：键值赋值（key=value / key:value / key：value）、令牌前缀词
/// （sk-/ghp_/xox*/gl-/pat_…）、`Bearer` 后随词；`*_tokens` 计数不受影响。
pub fn sanitize_text(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    let mut redact_next = false;
    while !rest.is_empty() {
        let lead = match rest.find(|c: char| !c.is_whitespace()) {
            Some(i) => i,
            None => {
                result.push_str(rest);
                break;
            }
        };
        result.push_str(&rest[..lead]);
        rest = &rest[lead..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        rest = tail;

        let (redacted, hunt_next) = sanitize_word(word, redact_next);
        result.push_str(&redacted);
        redact_next = hunt_next;
    }
    truncate_chars(&result, 400)
}

/// 处理单个词：返回 (输出词, 是否脱敏下一个词)。
pub(crate) fn sanitize_word(word: &str, redact_next: bool) -> (String, bool) {
    let lower = word.to_lowercase();
    // Bearer <token>：Bearer 词后随词脱敏。
    if lower == "bearer" {
        return (word.to_string(), true);
    }
    if redact_next {
        return ("[REDACTED]".to_string(), false);
    }
    // 令牌前缀（含尾随标点剥离后判断）。
    let stripped =
        lower.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    if SECRET_TOKEN_PREFIXES
        .iter()
        .any(|p| stripped.starts_with(p) && stripped.len() > p.len() + 4)
    {
        return ("[REDACTED]".to_string(), false);
    }
    // 词内键值：key=value / key:value / key：value。
    let sep = word
        .char_indices()
        .find(|(_, c)| *c == '=' || *c == ':' || *c == '：');
    if let Some((sep_idx, sep_char)) = sep {
        let key = &word[..sep_idx];
        if is_sensitive_key(key) {
            // 值紧随分隔符（同词内）→ 就地替换；值为空（空格分隔，值在下一词）
            // → 同样替换并令下一词脱敏（`password: hunter2` 场景）。
            let mut out = String::with_capacity(word.len());
            out.push_str(key);
            out.push(sep_char);
            out.push_str("[REDACTED]");
            let value_same_word = !word[sep_idx + sep_char.len_utf8()..].trim().is_empty();
            return (out, !value_same_word);
        }
        // 键不敏感：对分隔符之后的残余部分递归续扫（覆盖 `失败原因：api_key=sk-…`
        // 这类无空格连写的复合词——首个分隔符的键非凭据，但词内还有凭据键值段）。
        let rest = &word[sep_idx + sep_char.len_utf8()..];
        if !rest.is_empty() {
            let (redacted_rest, hunt_next) = sanitize_word(rest, false);
            let mut out = String::with_capacity(word.len());
            out.push_str(key);
            out.push(sep_char);
            out.push_str(&redacted_rest);
            return (out, hunt_next);
        }
        return (word.to_string(), false);
    }
    // 裸敏感键词（如独立出现的 `token`）：保守把后随词视为值脱敏（误杀方向安全）。
    if is_sensitive_key(word) {
        return (word.to_string(), true);
    }
    (word.to_string(), false)
}

/// 结构化脱敏：凭据类键 → `[REDACTED]`；字符串值 → [`sanitize_text`]；其余递归。
pub fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    if is_sensitive_key(k) {
                        (k.clone(), json!("[REDACTED]"))
                    } else {
                        (k.clone(), sanitize_value(v))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        Value::String(s) => Value::String(sanitize_text(s)),
        other => other.clone(),
    }
}
