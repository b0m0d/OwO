//! 工具参数取用助手（跨 crate 共享）。
//!
//! 本模块存在的原因是一次微内核拆分暴露出的隐式耦合：`required_string` 原本是
//! `owo-agent-core::tools` 里的 `pub(crate)` 函数，core 内部**零调用**，唯一使用者是
//! 被迁到 `devtools/product-eval` 的 ProductEval 单 Agent 执行器。也就是说它的正确
//! 归属从来不是 core，而是“所有工具实现者都可能用到的稳定原语”。
//!
//! 迁到内核后：core 的 `tools.rs` 删掉这段死代码，开发工具包改为依赖
//! `owo_agent_kernel::required_string`，无需为了一个 5 行助手把 core 的公共 API 面
//! 永久扩大（也不需要用 `pub(crate)` 泄漏到 crate 外）。

use serde_json::Value;

/// 读取工具调用参数中的必填字符串字段。
///
/// 缺失或非字符串时返回面向用户的稳定错误文案（与既有工具实现的错误风格一致）。
pub fn required_string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("参数缺少字符串字段：{key}"))
}
