//! 工作区变更追踪（七期 · 二路；九期 · 一路修正检测算法）：Worker 执行前后
//! git 快照、写白名单校验、变更落盘。
//!
//! 数据面（都在 TeamRun 数据目录，重启可读）：
//! - `<run_dir>/<team_id>-workspace-changes.json`：变更记录数组（追加写，逐步骤）；
//! - `<run_dir>/<team_id>-changes/<step>-<ts>.patch`：本次实际变更文件的 git 补丁
//!   （best-effort）；git 不可用/补丁为空时落退化差异摘要
//!   `<step>-<ts>.diff.txt`（执行前 CAS 基线 × 执行后内容，见
//!   `change_set::degraded_diff_summary`）。
//!
//! 语义（九期一路修正）：
//! - 快照 = `git status --porcelain`（状态行）+ `git diff --stat`（摘要文本）；
//!   非 git 目录 / 无 git 可执行 → `git=false`：变更不可检测，白名单校验对空变更
//!   自然放行（检测能力以环境为准，能力缺失不判违规，不阻塞任务）；
//! - **changed_files = 窗口差集 ∪ 重命名 old 侧 ∪ 内容哈希差集**（九期前只做
//!   「前后两次 porcelain 的路径集合差」，漏掉「执行前已脏、执行后仍脏但内容变了」
//!   的文件——Agent 二次修改用户已改文件时 changed_files 为空、ChangeSet 丢失）；
//!   内容哈希差集以「执行前内容哈希 × 执行后内容哈希」为准（执行前哈希来自
//!   ChangeSet 基线快照 + 执行前已脏文件的直接读取），删除（内容消失）同样计入；
//! - 白名单校验对上述合并后的 `changed_files` 做（含执行前已脏但本次被继续修改的
//!   文件——越界写不再因「路径早已脏」而漏检）；
//! - 越界 → `Err("scope_violation: …")`（失败码前缀与 workswarm 失败口径一致），
//!   由包装层转成步骤失败——不登记成功 Artifact；
//! - 追踪是旁路：落盘 IO 失败只告警不阻断任务。

/// porcelain 状态行数上限（防超大仓库膨胀快照体）。
mod git;
mod tracker;

#[cfg(test)]
mod tests;

pub(crate) use git::*;
pub(crate) use tracker::*;
