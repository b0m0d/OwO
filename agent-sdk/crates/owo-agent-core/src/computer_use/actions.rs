use serde_json::{json, Value};

use crate::executor;

use super::sim::*;
pub fn desktop_click(x: i32, y: i32) -> Result<(), String> {
    executor::click_at_screen(x, y)
}

/// 公开同步入口：注入 UTF-16 文本。
pub fn desktop_type(text: &str) -> Result<(), String> {
    executor::send_unicode(text)
}

/// 公开同步入口：发送单个按键（enter/tab 等）。
pub fn desktop_key(key: &str) -> Result<(), String> {
    executor::send_shortcut(key)
}

/// 公开同步入口：发送组合键。
pub fn desktop_shortcut(combo: &str) -> Result<(), String> {
    executor::send_shortcut(combo)
}

/// 公开同步入口：启动应用/URL。
pub fn desktop_launch(target: &str) -> Result<(), String> {
    executor::launch_target(target)
}

/// 公开同步入口：屏幕坐标处滚轮（正上负下）。
pub fn desktop_scroll(x: i32, y: i32, delta: i32) -> Result<(), String> {
    executor::scroll_at_screen(x, y, delta)
}

// ---------- m4d 任务级门禁与审批闭环 ----------

/// 任务动作门禁：任何 desktop_* 动作执行前必须通过本校验（m4d 审批版）。
///
/// 校验顺序：任务存在 → 状态可执行（含超时）→ 动作在允许集 → 目标应用匹配 →
/// 敏感 UI 熔断 → 动作预算。任一失败返回 Err（**不执行动作**），并落审计
/// `permission/deny`（approved=false）；全部通过时记审计 `permission/grant`。
///
/// `sensitive_sample` 为当前界面的敏感检测样本 `(name, role, ocr_text)`；
/// 命中密码/支付/验证码等关键词时任务置 Fused 并要求人工接管。
pub fn task_gate_check(
    registry: &crate::computer_task::ComputerTaskRegistry,
    mut audit: Option<&mut crate::audit::AuditLog>,
    session_id: &str,
    task_id: &str,
    action: &str,
    actual_app: &str,
    sensitive_sample: Option<(&str, &str, &str)>,
) -> Result<(), String> {
    let deny = |audit: &mut crate::audit::AuditLog, detail: String| {
        audit.record(
            session_id,
            "permission",
            Some(action.to_string()),
            Some(false),
            detail,
        );
    };
    // 1. 任务存在 + 状态可执行（含超时自动暂停）。
    if let Err(e) = registry.check_can_execute(task_id) {
        if let Some(a) = audit.as_mut() {
            deny(a, format!("computer-use 门禁拒绝：{e}"));
        }
        return Err(e);
    }
    // 2. 动作允许集。
    if let Err(e) = registry.check_action_allowed(task_id, action) {
        if let Some(a) = audit.as_mut() {
            deny(a, format!("computer-use 门禁拒绝：{e}"));
        }
        return Err(e);
    }
    // 3. 目标应用匹配。
    match registry.target_matches(task_id, actual_app) {
        Ok(true) => {}
        Ok(false) => {
            let detail = format!(
                "computer-use 门禁拒绝：任务 {task_id} 目标应用不匹配（任务声明与当前 {actual_app}）"
            );
            if let Some(a) = audit.as_mut() {
                deny(a, detail);
            }
            return Err(format!(
                "目标应用 {actual_app} 不在任务 {task_id} 授权范围内"
            ));
        }
        Err(e) => {
            if let Some(a) = audit.as_mut() {
                deny(a, format!("computer-use 门禁拒绝：{e}"));
            }
            return Err(e);
        }
    }
    // 4. 敏感 UI 熔断。
    if let Some((name, role, ocr_text)) = sensitive_sample {
        if let Some(reason) = crate::computer_task::sensitive_ui_hit(name, role, ocr_text) {
            let detail = format!("computer-use 敏感熔断：{reason}，任务 {task_id} 置 Fused");
            let _ = registry.fuse(task_id, &detail);
            if let Some(a) = audit.as_mut() {
                deny(a, detail);
            }
            return Err(format!(
                "敏感 UI 熔断：{reason}；任务 {task_id} 已暂停，需人工接管后 resume"
            ));
        }
    }
    // 5. 动作预算。
    if let Err(e) = registry.check_action_budget(task_id) {
        if let Some(a) = audit.as_mut() {
            deny(a, format!("computer-use 门禁拒绝：{e}"));
        }
        return Err(e);
    }
    if let Some(a) = audit.as_mut() {
        a.record(
            session_id,
            "permission",
            Some(action.to_string()),
            Some(true),
            format!("computer-use 任务 {task_id} 动作 {action} 已获授权（目标 {actual_app}）"),
        );
    }
    Ok(())
}

/// 对 OCR lines 做整屏敏感扫描，返回首个命中说明（无命中返回 None）。
pub fn scan_ui_sensitive(ocr: &Value) -> Option<String> {
    let lines = ocr.get("lines")?.as_array()?;
    for line in lines {
        let text = line.get("text").and_then(Value::as_str).unwrap_or("");
        let role = line.get("role_hint").and_then(Value::as_str).unwrap_or("");
        if let Some(reason) = crate::computer_task::sensitive_ui_hit(text, role, "") {
            return Some(reason);
        }
    }
    None
}

/// 门禁后屏幕单击（sim/真实统一走各自实现）。
#[allow(clippy::too_many_arguments)]
pub async fn desktop_click_gated(
    registry: &crate::computer_task::ComputerTaskRegistry,
    mut audit: Option<&mut crate::audit::AuditLog>,
    session_id: &str,
    task_id: &str,
    app: &str,
    x: i32,
    y: i32,
    sensitive_sample: Option<(&str, &str, &str)>,
) -> Result<Value, String> {
    task_gate_check(
        registry,
        audit.as_deref_mut(),
        session_id,
        task_id,
        "desktop_click",
        app,
        sensitive_sample,
    )?;
    let result = if on_sim_surface() {
        sim_post("click", json!({ "x": x, "y": y })).await?
    } else {
        executor::click_at_screen(x, y)?;
        json!({ "clicked": [x, y] })
    };
    registry.record_action(task_id);
    if let Some(a) = audit.as_mut() {
        a.record(
            session_id,
            "tool_call",
            Some("desktop_click".to_string()),
            Some(true),
            format!("任务 {task_id} 点击 ({x},{y})"),
        );
    }
    Ok(result)
}

/// 门禁后注入文本。
#[allow(clippy::too_many_arguments)]
pub async fn desktop_type_gated(
    registry: &crate::computer_task::ComputerTaskRegistry,
    mut audit: Option<&mut crate::audit::AuditLog>,
    session_id: &str,
    task_id: &str,
    app: &str,
    text: &str,
    sensitive_sample: Option<(&str, &str, &str)>,
) -> Result<Value, String> {
    task_gate_check(
        registry,
        audit.as_deref_mut(),
        session_id,
        task_id,
        "desktop_type",
        app,
        sensitive_sample,
    )?;
    let result = if on_sim_surface() {
        sim_post("type", json!({ "text": text })).await?
    } else {
        executor::send_unicode(text)?;
        json!({ "typed_chars": text.chars().count() })
    };
    registry.record_action(task_id);
    if let Some(a) = audit.as_mut() {
        a.record(
            session_id,
            "tool_call",
            Some("desktop_type".to_string()),
            Some(true),
            format!("任务 {task_id} 输入 {} 字符", text.chars().count()),
        );
    }
    Ok(result)
}

/// 门禁后发送按键（enter/tab/backspace 等）。
#[allow(clippy::too_many_arguments)]
pub async fn desktop_key_gated(
    registry: &crate::computer_task::ComputerTaskRegistry,
    mut audit: Option<&mut crate::audit::AuditLog>,
    session_id: &str,
    task_id: &str,
    app: &str,
    key: &str,
    sensitive_sample: Option<(&str, &str, &str)>,
) -> Result<Value, String> {
    task_gate_check(
        registry,
        audit.as_deref_mut(),
        session_id,
        task_id,
        "desktop_key",
        app,
        sensitive_sample,
    )?;
    let result = if on_sim_surface() {
        sim_post("key", json!({ "key": key })).await?
    } else {
        executor::send_shortcut(key)?;
        json!({ "key": key })
    };
    registry.record_action(task_id);
    if let Some(a) = audit.as_mut() {
        a.record(
            session_id,
            "tool_call",
            Some("desktop_key".to_string()),
            Some(true),
            format!("任务 {task_id} 按键 {key}"),
        );
    }
    Ok(result)
}

/// 门禁后滚轮。
#[allow(clippy::too_many_arguments)]
pub async fn desktop_scroll_gated(
    registry: &crate::computer_task::ComputerTaskRegistry,
    mut audit: Option<&mut crate::audit::AuditLog>,
    session_id: &str,
    task_id: &str,
    app: &str,
    x: i32,
    y: i32,
    delta: i32,
    sensitive_sample: Option<(&str, &str, &str)>,
) -> Result<Value, String> {
    task_gate_check(
        registry,
        audit.as_deref_mut(),
        session_id,
        task_id,
        "desktop_scroll",
        app,
        sensitive_sample,
    )?;
    let result = if on_sim_surface() {
        sim_post("scroll", json!({ "x": x, "y": y, "delta": delta })).await?
    } else {
        executor::scroll_at_screen(x, y, delta)?;
        json!({ "scrolled": [x, y, delta] })
    };
    registry.record_action(task_id);
    if let Some(a) = audit.as_mut() {
        a.record(
            session_id,
            "tool_call",
            Some("desktop_scroll".to_string()),
            Some(true),
            format!("任务 {task_id} 滚轮 ({x},{y},{delta})"),
        );
    }
    Ok(result)
}

/// 门禁后启动应用/URL（任务已批准且目标应用匹配时才允许）。
#[allow(clippy::too_many_arguments)]
pub async fn desktop_launch_gated(
    registry: &crate::computer_task::ComputerTaskRegistry,
    mut audit: Option<&mut crate::audit::AuditLog>,
    session_id: &str,
    task_id: &str,
    app: &str,
    target: &str,
) -> Result<Value, String> {
    task_gate_check(
        registry,
        audit.as_deref_mut(),
        session_id,
        task_id,
        "desktop_launch",
        app,
        None,
    )?;
    let result = if on_sim_surface() {
        json!({ "launched": target, "surface": "sim" })
    } else {
        executor::launch_target(target)?;
        json!({ "launched": target })
    };
    registry.record_action(task_id);
    if let Some(a) = audit.as_mut() {
        a.record(
            session_id,
            "tool_call",
            Some("desktop_launch".to_string()),
            Some(true),
            format!("任务 {task_id} 启动 {target}"),
        );
    }
    Ok(result)
}
