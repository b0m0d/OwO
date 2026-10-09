//! SimApp 状态机与各应用行为/渲染（从 desktop_env/mod.rs 拆出）。
//!
//! 子模块可访问父模块的私有状态结构体与字段，故这里无需放宽任何可见性。

use super::*;

impl SimApp {
    pub(super) fn kind(&self) -> SimAppKind {
        match self {
            Self::Chat(_) => SimAppKind::Chat,
            Self::Files(_) => SimAppKind::Files,
            Self::Form(_) => SimAppKind::Form,
            Self::Document(_) => SimAppKind::Document,
        }
    }

    pub(super) fn fault(&self) -> &FaultState {
        match self {
            Self::Chat(s) => &s.fault,
            Self::Files(s) => &s.fault,
            Self::Form(s) => &s.fault,
            Self::Document(s) => &s.fault,
        }
    }

    pub(super) fn fault_mut(&mut self) -> &mut FaultState {
        match self {
            Self::Chat(s) => &mut s.fault,
            Self::Files(s) => &mut s.fault,
            Self::Form(s) => &mut s.fault,
            Self::Document(s) => &mut s.fault,
        }
    }

    pub(super) fn bump_step(&mut self, n: u64) {
        match self {
            Self::Chat(s) => s.step_count += n,
            Self::Files(s) => s.step_count += n,
            Self::Form(s) => s.step_count += n,
            Self::Document(s) => s.step_count += n,
        }
    }

    /// 渲染可见元素（应用元素漂移故障）。
    pub(super) fn render(&self) -> Vec<SimElement> {
        let mut elements = match self {
            Self::Chat(s) => render_chat(s),
            Self::Files(s) => render_files(s),
            Self::Form(s) => render_form(s),
            Self::Document(s) => render_doc(s),
        };
        // 模态弹窗置顶。
        if let Some(text) = self.fault().popup.clone() {
            elements.push(SimElement {
                id: "popup.window".into(),
                role: "dialog".into(),
                text: text.clone(),
                x: 300,
                y: 250,
                width: 400,
                height: 150,
                enabled: true,
                visible: true,
            });
            elements.push(SimElement {
                id: "popup.text".into(),
                role: "text".into(),
                text,
                x: 320,
                y: 270,
                width: 360,
                height: 60,
                enabled: false,
                visible: true,
            });
            elements.push(SimElement {
                id: "popup.close".into(),
                role: "button".into(),
                text: "关闭".into(),
                x: 590,
                y: 350,
                width: 90,
                height: 32,
                enabled: true,
                visible: true,
            });
        }
        // 元素漂移：同时作用于渲染与命中测试。
        let drifts = self.fault().drifts.clone();
        for element in elements.iter_mut() {
            for drift in &drifts {
                if drift.element_id == element.id {
                    element.x += drift.dx;
                    element.y += drift.dy;
                }
            }
        }
        elements
    }

    pub(super) fn window_stack(&self) -> Vec<String> {
        let mut stack = vec![format!("owo-sim-{}", self.kind())];
        if self.fault().popup.is_some() {
            stack.push("popup".into());
        }
        stack
    }

    /// 隐藏状态 JSON（判分与 state_ref 的唯一来源；不进入观测）。
    pub(super) fn hidden_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub(super) fn state_ref(&self) -> String {
        sha256_hex(&self.hidden_json().to_string())
    }

    /// 命中测试：返回最上层可见元素。
    pub(super) fn hit_test(&self, x: i32, y: i32) -> Option<SimElement> {
        self.render().into_iter().rev().find(|e| e.contains(x, y))
    }

    pub(super) fn click(&mut self, x: i32, y: i32) -> StepEffect {
        let popup_active = self.fault().popup.is_some();
        let sluggish = self.fault().sluggish_remaining > 0;
        let hit = self.hit_test(x, y);
        if sluggish {
            self.fault_mut().sluggish_remaining -= 1;
            return StepEffect::NoOp("应用无响应（注入的迟钝步骤）".into());
        }
        let Some(element) = hit else {
            return StepEffect::NoOp(format!("点击空白处 ({x},{y})"));
        };
        if popup_active && element.id != "popup.close" {
            return StepEffect::Blocked("模态弹窗阻塞操作".into());
        }
        if element.id == "popup.close" {
            self.fault_mut().popup = None;
            return StepEffect::Applied("关闭弹窗".into());
        }
        match self {
            Self::Chat(s) => chat_click(s, &element),
            Self::Files(s) => files_click(s, &element),
            Self::Form(s) => form_click(s, &element),
            Self::Document(s) => doc_click(s, &element),
        }
    }

    pub(super) fn type_text(&mut self, text: &str) -> StepEffect {
        if self.fault().popup.is_some() {
            return StepEffect::Blocked("模态弹窗阻塞操作".into());
        }
        if self.fault().sluggish_remaining > 0 {
            self.fault_mut().sluggish_remaining -= 1;
            return StepEffect::NoOp("应用无响应（注入的迟钝步骤）".into());
        }
        match self {
            Self::Chat(s) => {
                if s.focused.as_deref() == Some("chat.input") {
                    s.input.push_str(text);
                    StepEffect::Applied(format!("输入 {text:?}"))
                } else {
                    StepEffect::NoOp("没有聚焦的输入框".into())
                }
            }
            Self::Files(s) => {
                if s.focused.as_deref() == Some("files.dialog.input") {
                    if let Some(dialog) = s.dialog.as_mut() {
                        dialog.value.push_str(text);
                        StepEffect::Applied(format!("输入 {text:?}"))
                    } else {
                        StepEffect::NoOp("重命名对话框不存在".into())
                    }
                } else {
                    StepEffect::NoOp("没有聚焦的输入框".into())
                }
            }
            Self::Form(s) => {
                let focused = s.focused.clone();
                match focused {
                    Some(field) if field.starts_with("form.input.") => {
                        let key = field.trim_start_matches("form.input.").to_string();
                        if let Some(value) = s.fields.get_mut(&key) {
                            value.push_str(text);
                            StepEffect::Applied(format!("填写 {key} += {text:?}"))
                        } else {
                            StepEffect::NoOp(format!("未知字段 {key}"))
                        }
                    }
                    _ => StepEffect::NoOp("没有聚焦的输入框".into()),
                }
            }
            Self::Document(s) => {
                if s.focused {
                    s.content.push_str(text);
                    StepEffect::Applied(format!("追加文本 {text:?}"))
                } else {
                    StepEffect::NoOp("文档未聚焦".into())
                }
            }
        }
    }

    pub(super) fn key(&mut self, key: &str) -> StepEffect {
        if self.fault().popup.is_some() {
            return StepEffect::Blocked("模态弹窗阻塞操作".into());
        }
        if self.fault().sluggish_remaining > 0 {
            self.fault_mut().sluggish_remaining -= 1;
            return StepEffect::NoOp("应用无响应（注入的迟钝步骤）".into());
        }
        if key != "enter" {
            return StepEffect::NoOp(format!("未建模按键 {key:?}"));
        }
        match self {
            Self::Chat(s) => {
                if s.focused.as_deref() == Some("chat.input") {
                    chat_send(s)
                } else {
                    StepEffect::NoOp("输入框未聚焦".into())
                }
            }
            Self::Files(s) => {
                if s.dialog.is_some() {
                    files_confirm_rename(s)
                } else {
                    StepEffect::NoOp("无活动对话框".into())
                }
            }
            Self::Form(s) => form_submit(s),
            Self::Document(_) => StepEffect::NoOp("文档未建模 enter".into()),
        }
    }

    pub(super) fn wait_steps(&mut self, n: u64) -> StepEffect {
        self.bump_step(n);
        if let Self::Chat(s) = self {
            let due: Vec<IncomingSpec> = s
                .incoming_schedule
                .iter()
                .filter(|e| e.at_step <= s.step_count)
                .cloned()
                .collect();
            s.incoming_schedule.retain(|e| e.at_step > s.step_count);
            let mut delivered = 0usize;
            for event in due {
                if let Some(idx) = s.contacts.iter().position(|c| *c == event.from) {
                    s.messages[idx].push(ChatMsg {
                        from: event.from.clone(),
                        text: event.text,
                        is_self: false,
                    });
                    delivered += 1;
                }
            }
            if delivered > 0 {
                return StepEffect::Applied(format!("等待 {n} 步，收到 {delivered} 条消息"));
            }
        }
        StepEffect::Applied(format!("等待 {n} 步"))
    }

    pub(super) fn apply_fault(&mut self, fault: FaultSpec) {
        let state = self.fault_mut();
        match fault {
            FaultSpec::ModalPopup { text } => state.popup = Some(text),
            FaultSpec::ElementDrift { element_id, dx, dy } => {
                state.drifts.push(DriftRecord { element_id, dx, dy })
            }
            FaultSpec::SluggishSteps { steps } => state.sluggish_remaining += steps,
        }
    }
}

// ---- 聊天 ----

fn chat_send(s: &mut ChatState) -> StepEffect {
    let text = s.input.trim().to_string();
    if text.is_empty() {
        return StepEffect::NoOp("输入为空，未发送".into());
    }
    let contact = s.contacts[s.active].clone();
    s.messages[s.active].push(ChatMsg {
        from: "我".into(),
        text: text.clone(),
        is_self: true,
    });
    s.sent_log.push(SentRecord {
        contact: contact.clone(),
        text: text.clone(),
    });
    s.input.clear();
    // seed 决定的自动回复：确定性。
    let draw = rng_next(&mut s.rng_state) % 100;
    if draw < 70 {
        let reply = format!("收到：{text}");
        s.incoming_schedule.push(IncomingSpec {
            at_step: s.step_count + 1,
            from: contact.clone(),
            text: reply,
        });
    }
    StepEffect::Applied(format!("发送给 {contact}：{text:?}"))
}

fn chat_click(s: &mut ChatState, element: &SimElement) -> StepEffect {
    if element.id == "chat.input" {
        s.focused = Some("chat.input".into());
        return StepEffect::Applied("聚焦输入框".into());
    }
    if element.id == "chat.send" {
        return chat_send(s);
    }
    if let Some(idx) = element
        .id
        .strip_prefix("chat.contact.")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if idx < s.contacts.len() {
            s.active = idx;
            return StepEffect::Applied(format!("切换到联系人 {}", s.contacts[idx]));
        }
    }
    StepEffect::NoOp(format!("点击了非交互元素 {}", element.id))
}

fn render_chat(s: &ChatState) -> Vec<SimElement> {
    let mut elements = vec![SimElement {
        id: "chat.window".into(),
        role: "window".into(),
        text: "模拟聊天".into(),
        x: 0,
        y: 0,
        width: 1020,
        height: 700,
        enabled: true,
        visible: true,
    }];
    for (i, contact) in s.contacts.iter().enumerate() {
        elements.push(SimElement {
            id: format!("chat.contact.{i}"),
            role: "list_item".into(),
            text: contact.clone(),
            x: 10,
            y: 40 + i as i32 * 30,
            width: 200,
            height: 28,
            enabled: true,
            visible: true,
        });
    }
    for (j, msg) in s.messages[s.active].iter().enumerate() {
        elements.push(SimElement {
            id: format!("chat.msg.{j}"),
            role: "text".into(),
            text: format!("{}: {}", msg.from, msg.text),
            x: 230,
            y: 40 + j as i32 * 26,
            width: 600,
            height: 24,
            enabled: false,
            visible: true,
        });
    }
    elements.push(SimElement {
        id: "chat.input".into(),
        role: "input".into(),
        text: s.input.clone(),
        x: 230,
        y: 620,
        width: 560,
        height: 40,
        enabled: true,
        visible: true,
    });
    elements.push(SimElement {
        id: "chat.send".into(),
        role: "button".into(),
        text: "发送".into(),
        x: 810,
        y: 620,
        width: 120,
        height: 40,
        enabled: true,
        visible: true,
    });
    elements
}

// ---- 文件管理器 ----

fn files_confirm_rename(s: &mut FilesState) -> StepEffect {
    let Some(dialog) = s.dialog.take() else {
        return StepEffect::NoOp("无活动对话框".into());
    };
    let new_name = dialog.value.trim().to_string();
    if new_name.is_empty() {
        return StepEffect::Blocked("重命名不能为空".into());
    }
    if dialog.index >= s.files.len() {
        return StepEffect::Blocked("重命名目标已不存在".into());
    }
    let old = s.files[dialog.index].name.clone();
    s.files[dialog.index].name = new_name.clone();
    s.focused = None;
    StepEffect::Applied(format!("重命名 {old} -> {new_name}"))
}

fn files_click(s: &mut FilesState, element: &SimElement) -> StepEffect {
    if element.id == "files.new" {
        let n = s.files.len() + 1;
        s.files.push(FileEntry {
            name: format!("新建文件夹 {n}"),
            size: 0,
        });
        return StepEffect::Applied("新建文件夹".into());
    }
    if element.id == "files.rename" {
        return match s.selected {
            Some(idx) if idx < s.files.len() => {
                s.dialog = Some(RenameDialog {
                    index: idx,
                    value: s.files[idx].name.clone(),
                });
                s.focused = Some("files.dialog.input".into());
                StepEffect::Applied("打开重命名对话框".into())
            }
            _ => StepEffect::Blocked("未选中文件，无法重命名".into()),
        };
    }
    if element.id == "files.delete" {
        return match s.selected {
            Some(idx) if idx < s.files.len() => {
                let name = s.files[idx].name.clone();
                s.files.remove(idx);
                s.deleted_log.push(name.clone());
                s.selected = None;
                StepEffect::Applied(format!("删除 {name}"))
            }
            _ => StepEffect::Blocked("未选中文件，无法删除".into()),
        };
    }
    if element.id == "files.dialog.input" {
        if s.dialog.is_some() {
            s.focused = Some("files.dialog.input".into());
            return StepEffect::Applied("聚焦重命名输入框".into());
        }
        return StepEffect::NoOp("重命名对话框不存在".into());
    }
    if element.id == "files.dialog.ok" {
        return files_confirm_rename(s);
    }
    if element.id == "files.dialog.cancel" {
        s.dialog = None;
        s.focused = None;
        return StepEffect::Applied("取消重命名".into());
    }
    if let Some(idx) = element
        .id
        .strip_prefix("files.row.")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if idx < s.files.len() {
            s.selected = Some(idx);
            return StepEffect::Applied(format!("选中 {}", s.files[idx].name));
        }
    }
    StepEffect::NoOp(format!("点击了非交互元素 {}", element.id))
}

fn render_files(s: &FilesState) -> Vec<SimElement> {
    let mut elements = vec![SimElement {
        id: "files.window".into(),
        role: "window".into(),
        text: "模拟文件管理器".into(),
        x: 0,
        y: 0,
        width: 900,
        height: 600,
        enabled: true,
        visible: true,
    }];
    for (i, file) in s.files.iter().enumerate() {
        let selected = s.selected == Some(i);
        elements.push(SimElement {
            id: format!("files.row.{i}"),
            role: "list_item".into(),
            text: format!("{}{}", file.name, if selected { "（已选中）" } else { "" }),
            x: 10,
            y: 60 + i as i32 * 30,
            width: 400,
            height: 28,
            enabled: true,
            visible: true,
        });
    }
    elements.push(SimElement {
        id: "files.new".into(),
        role: "button".into(),
        text: "新建文件夹".into(),
        x: 450,
        y: 60,
        width: 130,
        height: 30,
        enabled: true,
        visible: true,
    });
    elements.push(SimElement {
        id: "files.rename".into(),
        role: "button".into(),
        text: "重命名".into(),
        x: 450,
        y: 100,
        width: 130,
        height: 30,
        enabled: true,
        visible: true,
    });
    elements.push(SimElement {
        id: "files.delete".into(),
        role: "button".into(),
        text: "删除".into(),
        x: 450,
        y: 140,
        width: 130,
        height: 30,
        enabled: true,
        visible: true,
    });
    if let Some(dialog) = &s.dialog {
        elements.push(SimElement {
            id: "files.dialog".into(),
            role: "dialog".into(),
            text: "重命名".into(),
            x: 200,
            y: 220,
            width: 380,
            height: 140,
            enabled: true,
            visible: true,
        });
        elements.push(SimElement {
            id: "files.dialog.input".into(),
            role: "input".into(),
            text: dialog.value.clone(),
            x: 220,
            y: 260,
            width: 340,
            height: 32,
            enabled: true,
            visible: true,
        });
        elements.push(SimElement {
            id: "files.dialog.ok".into(),
            role: "button".into(),
            text: "确定".into(),
            x: 220,
            y: 310,
            width: 80,
            height: 30,
            enabled: true,
            visible: true,
        });
        elements.push(SimElement {
            id: "files.dialog.cancel".into(),
            role: "button".into(),
            text: "取消".into(),
            x: 320,
            y: 310,
            width: 80,
            height: 30,
            enabled: true,
            visible: true,
        });
    }
    elements
}

// ---- 表单 ----

fn form_submit(s: &mut FormState) -> StepEffect {
    let name = s.fields.get("name").cloned().unwrap_or_default();
    let email = s.fields.get("email").cloned().unwrap_or_default();
    if name.trim().is_empty() {
        s.error = Some("校验失败：姓名不能为空".into());
        return StepEffect::Blocked("校验失败：姓名不能为空".into());
    }
    if !email.contains('@') {
        s.error = Some("校验失败：邮箱格式不正确".into());
        return StepEffect::Blocked("校验失败：邮箱格式不正确".into());
    }
    s.error = None;
    s.submitted = Some(s.fields.clone());
    StepEffect::Applied("表单提交成功".into())
}

fn form_click(s: &mut FormState, element: &SimElement) -> StepEffect {
    if element.id == "form.submit" {
        return form_submit(s);
    }
    if let Some(key) = element.id.strip_prefix("form.input.") {
        if s.fields.contains_key(key) {
            s.focused = Some(element.id.clone());
            return StepEffect::Applied(format!("聚焦字段 {key}"));
        }
    }
    StepEffect::NoOp(format!("点击了非交互元素 {}", element.id))
}

fn render_form(s: &FormState) -> Vec<SimElement> {
    let mut elements = vec![SimElement {
        id: "form.window".into(),
        role: "window".into(),
        text: "模拟浏览器表单".into(),
        x: 0,
        y: 0,
        width: 800,
        height: 600,
        enabled: true,
        visible: true,
    }];
    for (i, (key, value)) in s.fields.iter().enumerate() {
        elements.push(SimElement {
            id: format!("form.label.{key}"),
            role: "text".into(),
            text: key.clone(),
            x: 40,
            y: 60 + i as i32 * 60,
            width: 140,
            height: 30,
            enabled: false,
            visible: true,
        });
        elements.push(SimElement {
            id: format!("form.input.{key}"),
            role: "input".into(),
            text: value.clone(),
            x: 200,
            y: 60 + i as i32 * 60,
            width: 400,
            height: 30,
            enabled: true,
            visible: true,
        });
    }
    let submit_y = 60 + s.fields.len() as i32 * 60;
    elements.push(SimElement {
        id: "form.submit".into(),
        role: "button".into(),
        text: "提交".into(),
        x: 200,
        y: submit_y,
        width: 120,
        height: 36,
        enabled: true,
        visible: true,
    });
    if let Some(error) = &s.error {
        elements.push(SimElement {
            id: "form.error".into(),
            role: "text".into(),
            text: error.clone(),
            x: 200,
            y: submit_y + 50,
            width: 400,
            height: 26,
            enabled: false,
            visible: true,
        });
    }
    if s.submitted.is_some() {
        elements.push(SimElement {
            id: "form.success".into(),
            role: "text".into(),
            text: "已提交".into(),
            x: 340,
            y: submit_y + 8,
            width: 120,
            height: 26,
            enabled: false,
            visible: true,
        });
    }
    elements
}

// ---- 轻量文档 ----

fn doc_click(s: &mut DocState, element: &SimElement) -> StepEffect {
    if element.id == "doc.save" {
        s.saved_content = s.content.clone();
        s.saves += 1;
        return StepEffect::Applied("保存文档".into());
    }
    if element.id == "doc.body" {
        s.focused = true;
        return StepEffect::Applied("聚焦文档".into());
    }
    StepEffect::NoOp(format!("点击了非交互元素 {}", element.id))
}

fn render_doc(s: &DocState) -> Vec<SimElement> {
    let mut elements = vec![SimElement {
        id: "doc.window".into(),
        role: "window".into(),
        text: "模拟文档".into(),
        x: 0,
        y: 0,
        width: 800,
        height: 600,
        enabled: true,
        visible: true,
    }];
    elements.push(SimElement {
        id: "doc.body".into(),
        role: "editor".into(),
        text: s.content.clone(),
        x: 20,
        y: 60,
        width: 600,
        height: 460,
        enabled: true,
        visible: true,
    });
    for (i, line) in s.content.lines().take(16).enumerate() {
        elements.push(SimElement {
            id: format!("doc.line.{i}"),
            role: "text".into(),
            text: line.to_string(),
            x: 24,
            y: 64 + i as i32 * 24,
            width: 592,
            height: 22,
            enabled: false,
            visible: true,
        });
    }
    let dirty = s.content != s.saved_content;
    elements.push(SimElement {
        id: "doc.save".into(),
        role: "button".into(),
        text: if dirty {
            "保存*".to_string()
        } else {
            "保存".to_string()
        },
        x: 640,
        y: 20,
        width: 120,
        height: 32,
        enabled: true,
        visible: true,
    });
    elements.push(SimElement {
        id: "doc.status".into(),
        role: "text".into(),
        text: if dirty {
            "有未保存更改".to_string()
        } else {
            "已保存".to_string()
        },
        x: 640,
        y: 60,
        width: 140,
        height: 24,
        enabled: false,
        visible: true,
    });
    elements
}
