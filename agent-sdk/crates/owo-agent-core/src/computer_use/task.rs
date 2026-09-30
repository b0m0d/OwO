use serde_json::{json, Value};
use std::time::Duration;

use crate::executor;

use super::actions::*;
use super::sim::*;

/// 闭环单步目标：感知到 `anchor_text` 后执行动作，并用 `verify_text` 验证。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct TaskGoal {
    /// 定位锚点（OCR 文本，大小写不敏感子串匹配）。
    pub anchor_text: String,
    /// 动作类型：click / type / key。
    pub action: String,
    /// 动作载荷：type 的文本或 key 名（click 忽略）。
    pub value: String,
    /// 动作后的验证文本（可选；出现在下一轮 OCR 即视为验证通过）。
    pub verify_text: Option<String>,
}

/// 闭环执行报告。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskReport {
    pub task_id: String,
    pub steps: usize,
    pub state: crate::computer_task::TaskState,
    pub detail: String,
}

// `TaskSurface` 契约已下沉到内核（M5 依赖倒置）：desktop_env 外迁需要它，
// 而 desktop_env 若依赖 computer_use 就会成环。实现体仍在本模块。
pub use owo_agent_kernel::TaskSurface;

/// owo-sim-qq HTTP 模拟面（`OWO_SIM_QQ_URL` 指向模拟窗口）。
#[derive(Debug)]
pub struct SimTaskSurface {
    base: String,
}

impl SimTaskSurface {
    pub fn new() -> Result<Self, String> {
        let base = sim_base_url().ok_or("模拟环境未配置 OWO_SIM_QQ_URL")?;
        Ok(Self { base })
    }
}

/// 真实桌面面：OCR 走本地引擎（Media.Ocr / PP-OCRv6 / 本地 ONNX），
/// 动作走 executor（SendInput / UIA / 启动）。用于已授权任务在真实桌面的闭环。
#[derive(Debug, Default)]
pub struct RealTaskSurface;

#[async_trait::async_trait]
impl TaskSurface for RealTaskSurface {
    fn app(&self) -> String {
        crate::platform::poll_foreground_app()
            .map(|(app_id, _)| app_id)
            .unwrap_or_default()
    }

    async fn ocr(&mut self) -> Result<Value, String> {
        ocr_screen(0).await
    }

    async fn click(&mut self, x: i32, y: i32) -> Result<(), String> {
        executor::click_at_screen(x, y)
    }

    async fn type_text(&mut self, text: &str) -> Result<(), String> {
        executor::send_unicode(text)
    }

    async fn key(&mut self, key: &str) -> Result<(), String> {
        executor::send_shortcut(key)
    }

    async fn launch(&mut self, target: &str) -> Result<(), String> {
        executor::launch_target(target)
    }
}

#[async_trait::async_trait]
impl TaskSurface for SimTaskSurface {
    fn app(&self) -> String {
        "owo-sim-qq".to_string()
    }

    async fn ocr(&mut self) -> Result<Value, String> {
        let base = self.base.trim_end_matches('/');
        let response = reqwest::get(format!("{base}/ocr"))
            .await
            .map_err(|e| format!("模拟面 OCR 失败：{e}"))?;
        let value: Value = response
            .json()
            .await
            .map_err(|e| format!("模拟面 OCR 响应解析失败：{e}"))?;
        let has_lines = value
            .get("lines")
            .and_then(Value::as_array)
            .map(|lines| !lines.is_empty())
            .unwrap_or(false);
        if has_lines {
            Ok(value)
        } else {
            Err("模拟面 OCR 不可用".to_string())
        }
    }

    async fn click(&mut self, x: i32, y: i32) -> Result<(), String> {
        sim_post_at(&self.base, "click", json!({ "x": x, "y": y })).await?;
        Ok(())
    }

    async fn type_text(&mut self, text: &str) -> Result<(), String> {
        sim_post_at(&self.base, "type", json!({ "text": text })).await?;
        Ok(())
    }

    async fn key(&mut self, key: &str) -> Result<(), String> {
        sim_post_at(&self.base, "key", json!({ "key": key })).await?;
        Ok(())
    }

    async fn launch(&mut self, _target: &str) -> Result<(), String> {
        // 模拟面无独立启动端点；启动语义由真实面/后续接线承载。
        Ok(())
    }
}

/// 感知闭环（指定 surface）：截图/OCR 感知 → 定位锚点 → 门禁动作 → 验证 → 下一步/完成，每步审计。
///
/// 前置：任务已 Approved 或 Running（Pending 返回 Err，需用户先批准）。
/// 敏感 UI（密码/支付/验证码）在每步感知后扫描，命中即 Fused 熔断并要求人工接管。
/// 任一动作被门禁拒绝（未批准/越界应用/超预算/超时）立即停止并返回错误。
pub async fn run_approved_task_on(
    registry: &crate::computer_task::ComputerTaskRegistry,
    audit: &mut crate::audit::AuditLog,
    session_id: &str,
    task_id: &str,
    goals: &[TaskGoal],
    surface: &mut dyn TaskSurface,
) -> Result<TaskReport, String> {
    // 前置门禁：以首个动作做整体授权检查（任务须已批准）。
    let task = registry
        .get(task_id)
        .ok_or_else(|| format!("任务 {task_id} 不存在"))?;
    if task.state != crate::computer_task::TaskState::Approved
        && task.state != crate::computer_task::TaskState::Running
    {
        return Err(format!(
            "任务 {task_id} 状态 {:?}，需用户先批准（Pending→approve）",
            task.state
        ));
    }
    // 当前前台应用（用于门禁的目标应用匹配）。
    let surface_app = surface.app();
    // 闭环步动作名（click/type/key/launch）→ 门禁动作名（desktop_*）。
    let first_action = if goals.is_empty() {
        "desktop_click"
    } else {
        match goals[0].action.as_str() {
            "click" => "desktop_click",
            "type" => "desktop_type",
            "key" => "desktop_key",
            "launch" => "desktop_launch",
            other => other,
        }
    };
    task_gate_check(
        registry,
        Some(&mut *audit),
        session_id,
        task_id,
        first_action,
        &surface_app,
        None,
    )?;
    if registry.get(task_id).unwrap().state != crate::computer_task::TaskState::Running {
        registry.start(task_id)?;
    }
    audit.record(
        session_id,
        "computer_task",
        Some("start".to_string()),
        Some(true),
        format!("任务 {task_id} 感知闭环启动（{} 步目标）", goals.len()),
    );

    let mut steps = 0usize;
    for goal in goals {
        // 1. 感知：OCR 版面。
        let ocr = surface.ocr().await?;
        // 2. 敏感扫描（整屏）→ 熔断。
        if let Some(reason) = scan_ui_sensitive(&ocr) {
            let detail = format!("敏感熔断（第 {} 步感知）：{reason}", steps + 1);
            let _ = registry.fuse(task_id, &detail);
            audit.record(
                session_id,
                "computer_task",
                Some("fuse".to_string()),
                Some(false),
                detail.clone(),
            );
            return Err(detail);
        }
        // 3. 定位锚点（OCR lines 中找目标文本行）。
        let line = find_ocr_line(&ocr, &goal.anchor_text, "")
            .ok_or_else(|| format!("定位失败：未找到锚点「{}」", goal.anchor_text))?;
        let x = line.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
        let y = line.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
        let w = line.get("width").and_then(Value::as_i64).unwrap_or(0) as i32;
        let h = line.get("height").and_then(Value::as_i64).unwrap_or(0) as i32;
        let (cx, cy) = (x + w / 2, y + h / 2);
        // 4. 门禁动作（状态/允许集/目标应用/敏感/预算，拒绝即停）。
        let action_name = match goal.action.as_str() {
            "click" => "desktop_click",
            "type" => "desktop_type",
            "key" => "desktop_key",
            "launch" => "desktop_launch",
            other => return Err(format!("闭环不支持的动作：{other}")),
        };
        task_gate_check(
            registry,
            Some(&mut *audit),
            session_id,
            task_id,
            action_name,
            &surface_app,
            None,
        )?;
        let outcome = match goal.action.as_str() {
            "click" => surface.click(cx, cy).await,
            "type" => surface.type_text(&goal.value).await,
            "key" => surface.key(&goal.value).await,
            "launch" => surface.launch(&goal.value).await,
            _ => unreachable!(),
        };
        outcome.map_err(|e| format!("第 {} 步动作执行失败：{e}", steps + 1))?;
        registry.record_action(task_id);
        steps += 1;
        // 5. 验证（可选）。
        if let Some(expected) = &goal.verify_text {
            let verified = verify_text_appears_on(surface, expected).await?;
            if !verified {
                return Err(format!("验证失败（第 {steps} 步）：未出现「{expected}」"));
            }
        }
        audit.record(
            session_id,
            "computer_task",
            Some("step".to_string()),
            Some(true),
            format!("任务 {task_id} 第 {steps} 步完成（{action_name}）@({cx},{cy})"),
        );
        // 6. 超时兜底（超时自动暂停并报错）。
        registry.check_can_execute(task_id)?;
    }
    let _ = registry.complete(task_id);
    audit.record(
        session_id,
        "computer_task",
        Some("complete".to_string()),
        Some(true),
        format!("任务 {task_id} 闭环完成，共 {steps} 步"),
    );
    Ok(TaskReport {
        task_id: task_id.to_string(),
        steps,
        state: crate::computer_task::TaskState::Completed,
        detail: "全部目标完成".to_string(),
    })
}

/// 感知闭环（模拟/沙箱面便捷入口）：`OWO_SIM_QQ_URL` 指向 owo-sim-qq，
/// 在沙箱应用内跑通"打开应用→输入→保存/发送→验证"最小闭环。
///
/// 真实桌面需显式授权（本轮不提供）；逻辑与 [`run_approved_task_on`] 完全一致。
pub async fn run_approved_task(
    registry: &crate::computer_task::ComputerTaskRegistry,
    audit: &mut crate::audit::AuditLog,
    session_id: &str,
    task_id: &str,
    goals: &[TaskGoal],
) -> Result<TaskReport, String> {
    let mut surface = SimTaskSurface::new()?;
    run_approved_task_on(registry, audit, session_id, task_id, goals, &mut surface).await
}

/// 验证 `needle` 是否出现在 surface 的 OCR 版面中（重试 5 次，间隔 50ms）。
pub(super) async fn verify_text_appears_on(
    surface: &mut dyn TaskSurface,
    needle: &str,
) -> Result<bool, String> {
    for _ in 0..5 {
        if let Ok(ocr) = surface.ocr().await {
            if find_ocr_line(&ocr, needle, "").is_some() {
                return Ok(true);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(false)
}
