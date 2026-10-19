//! Frozen suite, permission, budget and evaluator inputs for reproducible comparisons.

use super::{now_rfc3339, suite_hash, ProductEvalCase, ProductEvalError, SuiteBundle};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

// freeze.json：任务输入 / 检查器 / 权限 / 预算 / 模型配置 / 版本哈希冻结

// ---------------------------------------------------------------------------

/// freeze.json schema 版本。
pub const FREEZE_SCHEMA_VERSION: u32 = 1;

/// 计算单任务权限哈希：allow_read/allow_write/allow_commands 的规范序列化摘要。
pub fn permissions_hash(case: &ProductEvalCase) -> String {
    let mut hasher = Sha256::new();
    let payload = serde_json::json!({
        "allow_read": case.allow_read,
        "allow_write": case.allow_write,
        "allow_commands": case.allow_commands,
    });
    if let Ok(text) = serde_json::to_vec(&payload) {
        hasher.update(&text);
    }
    format!("{:x}", hasher.finalize())
}

/// 生成 freeze.json 内容（任务文件级 sha256 + 生效预算 + 权限哈希 + 模型/版本）。
/// `task_rel_paths` 为 suite.tasks 的相对路径（与 tasks 文件一一对应）。
pub fn build_freeze_json(
    bundle: &SuiteBundle,
    model: Option<&str>,
    base_url: Option<&str>,
    git_commit: Option<&str>,
    git_dirty: Option<bool>,
    frozen_at: Option<&str>,
) -> Result<serde_json::Value, ProductEvalError> {
    let defaults = &bundle.suite.defaults;
    let mut tasks = Vec::new();
    for (case, rel) in bundle.cases.iter().zip(bundle.suite.tasks.iter()) {
        let file_path = bundle.dir.join(rel);
        let text = std::fs::read_to_string(&file_path)
            .map_err(|e| ProductEvalError(format!("读取任务 {rel} 失败：{e}")))?;
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let file_sha = format!("{:x}", hasher.finalize());
        tasks.push(serde_json::json!({
            "id": case.id,
            "file": rel,
            "sha256": file_sha,
            "category": case.category.as_str(),
            "repetitions": case.effective_repetitions(defaults, None),
            "timeout_secs": case.effective_timeout_secs(defaults),
            "max_model_calls": case.effective_max_model_calls(defaults),
            "permissions_sha256": permissions_hash(case),
        }));
    }
    let total_cells = tasks
        .iter()
        .filter_map(|t| t.get("repetitions").and_then(serde_json::Value::as_u64))
        .sum::<u64>() as usize;
    Ok(serde_json::json!({
        "schema_version": FREEZE_SCHEMA_VERSION,
        "suite": {
            "name": bundle.suite.name,
            "revision_sha256": suite_hash(bundle),
        },
        "defaults": {
            "repetitions": defaults.repetitions,
            "timeout_secs": defaults.timeout_secs,
            "max_model_calls": defaults.max_model_calls,
        },
        "tasks": tasks,
        "permissions": { "policy": "default deny; allow_read/allow_write/allow_commands 逐调用强制，见 tasks[*].permissions_sha256" },
        "budget": {
            "single_cells": total_cells,
            "multi_cells": total_cells,
            "shared": "单/多 Agent 同任务同输入同权限同预算同检查器",
        },
        "model": {
            "id": model,
            "base_url": base_url,
        },
        "version": {
            "git_commit": git_commit,
            "git_dirty": git_dirty,
        },
        "frozen_at": frozen_at.unwrap_or(&now_rfc3339()),
        "frozen_by": "lane2-product-eval",
    }))
}

/// 校验当前套件是否与 freeze.json 一致（输入/检查器/权限/预算/版本哈希）。
/// 返回问题清单；空 = 冻结未被破坏。
pub fn verify_freeze(
    bundle: &SuiteBundle,
    freeze: &serde_json::Value,
    current_model: Option<&str>,
) -> Vec<String> {
    let mut issues = Vec::new();
    let Some(schema) = freeze
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    else {
        issues.push("freeze.json 缺少 schema_version".to_string());
        return issues;
    };
    if schema != FREEZE_SCHEMA_VERSION as u64 {
        issues.push(format!(
            "freeze.json schema_version={schema} 不兼容（期望 {FREEZE_SCHEMA_VERSION}）"
        ));
        return issues;
    }
    let freeze_suite = freeze.get("suite");
    let freeze_name = freeze_suite
        .and_then(|s| s.get("name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if freeze_name != bundle.suite.name {
        issues.push(format!(
            "freeze 套件名「{freeze_name}」与当前「{}」不一致",
            bundle.suite.name
        ));
    }
    let freeze_revision = freeze_suite
        .and_then(|s| s.get("revision_sha256"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let current_revision = suite_hash(bundle);
    if freeze_revision != current_revision {
        issues.push(format!(
            "套件修订哈希不一致：freeze={freeze_revision} 当前={current_revision}（任务输入/检查器/权限/预算已漂移；须重新生成 freeze.json 后建立新批次）"
        ));
    }
    // 任务级文件哈希核对（防同修订下的文件级漂移；正常应被 revision 覆盖）。
    let freeze_tasks = freeze
        .get("tasks")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let task_by_rel: std::collections::BTreeMap<String, &ProductEvalCase> = bundle
        .cases
        .iter()
        .zip(bundle.suite.tasks.iter())
        .map(|(case, rel)| (rel.clone(), case))
        .collect();
    let mut seen_paths = BTreeSet::new();
    for entry in &freeze_tasks {
        let Some(rel) = entry.get("file").and_then(serde_json::Value::as_str) else {
            issues.push("freeze 中任务缺少 file 字段".to_string());
            continue;
        };
        // freeze.json is an external input. Resolve only exact task paths already
        // present in the validated suite; never join or read its arbitrary path.
        let Some(case) = task_by_rel.get(rel).copied() else {
            issues.push(format!("freeze 包含当前套件之外的任务路径：{rel}"));
            continue;
        };
        if !seen_paths.insert(rel.to_string()) {
            issues.push(format!("freeze 重复包含任务文件：{rel}"));
            continue;
        }
        match entry.get("id").and_then(serde_json::Value::as_str) {
            Some(id) if id == case.id.as_str() => {}
            Some(id) => issues.push(format!(
                "freeze 任务 ID 与文件不匹配：file={rel} freeze={id} 当前={}",
                case.id
            )),
            None => issues.push(format!("freeze 中任务 {rel} 缺少 id")),
        }

        // The path is now the exact suite-owned relative path from task_by_rel.
        let file_path = bundle.dir.join(rel);
        let current_sha = std::fs::read_to_string(&file_path).ok().map(|text| {
            let mut hasher = Sha256::new();
            hasher.update(text.as_bytes());
            format!("{:x}", hasher.finalize())
        });
        let freeze_sha = entry.get("sha256").and_then(serde_json::Value::as_str);
        if let (Some(freeze_sha), Some(current_sha)) = (freeze_sha, &current_sha) {
            if freeze_sha != current_sha {
                issues.push(format!(
                    "任务文件 {rel} 哈希漂移：freeze={freeze_sha} 当前={current_sha}"
                ));
            }
        } else {
            issues.push(format!("freeze 中任务 {rel} 缺少 sha256 或文件缺失"));
        }

        match entry
            .get("permissions_sha256")
            .and_then(serde_json::Value::as_str)
        {
            Some(frozen_perm) => {
                let current_perm = permissions_hash(case);
                if frozen_perm != current_perm {
                    issues.push(format!(
                        "任务 {}（{rel}）权限哈希漂移：freeze={frozen_perm} 当前={current_perm}（allow_read/allow_write/allow_commands 已变更）",
                        case.id
                    ));
                }
            }
            None => issues.push(format!("freeze 中任务 {rel} 缺少 permissions_sha256")),
        }
    }
    for rel in task_by_rel.keys() {
        if !seen_paths.contains(rel) {
            issues.push(format!("freeze 缺少任务文件：{rel}"));
        }
    }
    // 数量核对。
    if freeze_tasks.len() != bundle.cases.len() {
        issues.push(format!(
            "freeze 任务数 {} 与当前套件 {} 不一致",
            freeze_tasks.len(),
            bundle.cases.len()
        ));
    }
    // 模型配置冻结核对（环境变量为当前配置来源）。
    if let Some(model_id) = freeze
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(serde_json::Value::as_str)
    {
        match current_model {
            Some(current) if current != model_id => {
                issues.push(format!(
                    "模型配置已漂移：freeze={model_id} 当前={current}（成绩只对冻结模型有效）"
                ));
            }
            Some(_) => {}
            None => issues.push(format!(
                "freeze 冻结模型 {model_id}，但当前未解析出模型（环境配置缺失）"
            )),
        }
    }
    issues
}

/// 从 freeze.json 文本解析（校验 schema 版本）。
pub fn parse_freeze(text: &str) -> Result<serde_json::Value, ProductEvalError> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| ProductEvalError(format!("freeze.json 解析失败：{e}")))?;
    Ok(value)
}
