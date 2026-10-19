use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

pub(crate) fn sum_opt(values: impl Iterator<Item = u64>) -> Option<u64> {
    let mut any = false;
    let mut total = 0u64;
    for v in values {
        any = true;
        total = total.saturating_add(v);
    }
    any.then_some(total)
}

pub(crate) fn add_opt(base: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (base, value) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

pub(crate) fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

pub(crate) fn price_env(name: &str) -> Option<f64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
}

/// 成本估算（美元）：单价取 `OWO_MODEL_INPUT_PRICE_PER_MTOK` /
/// `OWO_MODEL_OUTPUT_PRICE_PER_MTOK`（$/百万 token；未配置按 0 计——与既有
/// 用量/预算口径一致，tokens 仍真实落盘，cost=0 即「未配置单价」信号）。
pub(crate) fn configured_token_prices() -> Option<(f64, f64)> {
    let input = price_env("OWO_MODEL_INPUT_PRICE_PER_MTOK")
        .filter(|price| price.is_finite() && *price >= 0.0)?;
    let output = price_env("OWO_MODEL_OUTPUT_PRICE_PER_MTOK")
        .or(Some(input))
        .filter(|price| price.is_finite() && *price >= 0.0)?;
    Some((input, output))
}

/// 成本估算（美元）：无有效单价时返回 0；调用方必须同时检查单价/用量是否已知。
pub fn estimate_cost_usd(prompt_tokens: u64, completion_tokens: u64) -> f64 {
    let Some((input_price, output_price)) = configured_token_prices() else {
        return 0.0;
    };
    round6(
        prompt_tokens as f64 / 1_000_000.0 * input_price
            + completion_tokens as f64 / 1_000_000.0 * output_price,
    )
}

/// 截断长文本（按字符；附截断标记）。
pub fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let kept: String = input.chars().take(max_chars).collect();
    let dropped = input.chars().count() - max_chars;
    format!("{kept}…[截断 {dropped} 字符]")
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}
