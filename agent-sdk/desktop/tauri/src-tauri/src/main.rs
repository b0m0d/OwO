#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! OwO Agent 桌面主客户端：Tauri 2 窗口壳 + 自有核心运行时 + 全局快捷键 + 托盘。
//!
//! §4.2 重构后本文件只保留 Tauri builder 与事件接线：
//! - 核心生命周期全部归 `core_runtime::CoreRuntime`（动态端口 + 实例握手 +
//!   受控重启 + 日志捕获 + 优雅关闭），壳是核心进程的唯一 owner；
//! - 单实例锁（`single_instance`）：第二次启动直接退出，不另起 core；
//! - WebView 经 `commands`（get_core_connection 等）取得端口与实例身份，
//!   不再硬编码 4096，也不自行猜服务地址。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt;

mod commands;
mod core_runtime;
mod core_supervisor;
mod provider;
mod single_instance;

use core_runtime::{CoreRuntime, CORE_API_VERSION};
use single_instance::{AcquireOutcome, InstanceLock};

/// 构建期内嵌 tauri.conf.json：用于判断 updater 是否仍指向占位源（§10.2）。
const TAURI_CONF: &str = include_str!("../tauri.conf.json");

/// 占位更新源检测：缺少真实签名源时，「检查更新」不得作为可用功能暴露。
fn updater_endpoint_is_placeholder() -> bool {
    serde_json::from_str::<serde_json::Value>(TAURI_CONF)
        .ok()
        .and_then(|value| {
            value["plugins"]["updater"]["endpoints"][0]
                .as_str()
                .map(|endpoint| endpoint.contains("example.com"))
        })
        .unwrap_or(false)
}

/// 把窗口可见性事实同步给前端（壳 → 页面的单向注入）。
///
/// 为什么必须显式同步：实测 WebView2 不把 `controller.SetIsVisible(false)` 传导给
/// `document.visibilityState`（窗口在 Win32 层已不可见，页面仍是 "visible"），
/// 于是前端所有"隐藏期不产生业务请求"的守卫在桌面壳里都是死代码。
/// 这里注入 `owoSetBackground(bool)`（`desktop/web/app.js` 提供，未加载时短路无害），
/// 并同时发一份 `owo:visibility` 事件供其它消费者使用。
fn sync_window_background(window: &tauri::WebviewWindow, visible: bool) {
    let flag = if visible { "false" } else { "true" };
    let _ = window.eval(format!(
        "window.owoSetBackground && window.owoSetBackground({flag});"
    ));
    let _ = window.emit("owo:visibility", serde_json::json!({ "visible": visible }));
}

/// §4.1 唤回窗口：显示、取消最小化、聚焦，并把窗口移回当前可见显示器
/// （连续双击、被遮挡、最小化、关闭到托盘四种场景都必须把同一窗口带回前台）。
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        sync_window_background(&window, true);
        move_onto_visible_monitor(&window);
    }
}

/// §4.2/§8.2：显式隐藏/唤回主窗口，走**产品自身的隐藏路径**（wry `set_visible`
/// = Win32 `SW_HIDE` + WebView2 `controller.SetIsVisible`）。
///
/// 为什么需要这条命令：本机实测从**外部**调 `ShowWindow(SW_HIDE)` 不会让页面进入
/// `document.visibilityState === "hidden"`（只有 controller 的 SetIsVisible 会），
/// 于是"隐藏期不产生业务请求"既无法从外部触发、也无法被真实验证；同时界面里的
/// "收进后台"入口也复用本命令，避免再长第二份隐藏逻辑。
#[tauri::command]
fn set_window_visible(app: tauri::AppHandle, visible: bool) -> serde_json::Value {
    let mut now = visible;
    if let Some(window) = app.get_webview_window("main") {
        if visible {
            show_main_window(&app);
        } else {
            let _ = window.hide();
            sync_window_background(&window, false);
        }
        now = window.is_visible().unwrap_or(visible);
    }
    serde_json::json!({ "visible": now })
}

/// 窗口越出所有可见显示器（例如显示拓扑变化后）时，移回主显示器工作区中央。
fn move_onto_visible_monitor(window: &tauri::WebviewWindow) {
    let Ok(outer_position) = window.outer_position() else {
        return;
    };
    let Ok(outer_size) = window.outer_size() else {
        return;
    };
    let right = outer_position.x + outer_size.width as i32;
    let bottom = outer_position.y + outer_size.height as i32;
    let Ok(monitors) = window.available_monitors() else {
        return;
    };
    let on_visible = monitors.iter().any(|monitor| {
        let area = monitor.work_area();
        let left = area.position.x;
        let top = area.position.y;
        let area_right = left + area.size.width as i32;
        let area_bottom = top + area.size.height as i32;
        // 与任一显示器工作区有相交即视为可见。
        right > left && left < area_right && bottom > top && top < area_bottom
    });
    if on_visible {
        return;
    }
    if let Ok(Some(primary)) = window.primary_monitor() {
        let area = primary.work_area();
        let x = area.position.x + (area.size.width as i32 - outer_size.width as i32) / 2;
        let y = area.position.y + (area.size.height as i32 - outer_size.height as i32) / 2;
        let _ = window.set_position(tauri::PhysicalPosition::new(x.max(0), y.max(0)));
    }
}

fn run_key() -> std::io::Result<winreg::RegKey> {
    use winreg::enums::HKEY_CURRENT_USER;
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
        .map(|(key, _)| key)
}

fn autostart_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    let Ok(key) = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
    else {
        return false;
    };
    let Ok(value) = key.get_value::<String, _>("OwOAgentDesktop") else {
        return false;
    };
    value.contains("owo-agent-desktop")
}

fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let key = run_key()?;
    if enabled {
        let exe = std::env::current_exe()?;
        key.set_value("OwOAgentDesktop", &format!("\"{}\"", exe.display()))?;
    } else {
        key.delete_value("OwOAgentDesktop")?;
    }
    Ok(())
}

fn main() {
    // §4.1 单实例：主实例持有锁；第二实例已向主实例发唤回消息，本进程立即退出；
    // 权限/系统错误显示原生错误框并写桌面日志后再退出（拒绝静默哑死）。
    let mut instance_lock = match InstanceLock::acquire() {
        AcquireOutcome::Primary(lock) => lock,
        AcquireOutcome::WokeExisting => {
            eprintln!("[owo-desktop] 已有 OwO Agent 桌面实例在运行，本次启动退出");
            return;
        }
        AcquireOutcome::PermissionDenied(message) | AcquireOutcome::Unexpected(message) => {
            InstanceLock::report_fatal(&message);
            return;
        }
    };

    // §4.2：壳每次启动生成新的配对证明与实例身份，并注入给自己的核心子进程；
    // 任何不属于该身份的服务（旧核心/占用者）都会被握手拒绝，而不是静默复用。
    let pairing = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let instance_id = uuid::Uuid::new_v4().simple().to_string();
    let runtime = Arc::new(CoreRuntime::new(pairing, instance_id));
    runtime.start();

    let runtime_for_exit = Arc::clone(&runtime);
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(Arc::clone(&runtime))
        .setup(move |app| {
            // §4.1 唤回通道：第二实例/托盘/快捷键命中时，把同一窗口带回前台。
            let wake_handle = app.handle().clone();
            instance_lock.start_wake_listener(move || show_main_window(&wake_handle));

            // 全局快捷键：Ctrl+Alt+Shift+O 唤起工作台（避免常见冲突）。
            let shortcut = Shortcut::new(
                Some(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
                Code::KeyO,
            );
            let _ = app
                .global_shortcut()
                .on_shortcut(shortcut, |app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        show_main_window(app);
                    }
                });
            if let Err(error) = app.global_shortcut().register(shortcut) {
                eprintln!("[owo-desktop] 全局快捷键注册失败（继续运行）：{error}");
            }

            // 托盘：显示 / 自启 / 更新（占位源时禁用）/ 退出。
            let show = MenuItem::with_id(app, "show", "显示工作台", true, None::<&str>)?;
            let autostart_label = if autostart_enabled() {
                "开机自启：开"
            } else {
                "开机自启：关"
            };
            let autostart =
                MenuItem::with_id(app, "autostart", autostart_label, true, None::<&str>)?;
            let placeholder = updater_endpoint_is_placeholder();
            let update_label = if placeholder {
                "检查更新（未配置更新源）"
            } else {
                "检查更新"
            };
            let check_update = MenuItem::with_id(
                app,
                "check-update",
                update_label,
                !placeholder,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &autostart, &check_update, &quit])?;
            let icon = app.default_window_icon().cloned().ok_or("缺少应用图标")?;
            let _tray = TrayIconBuilder::new()
                .icon(icon)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "autostart" => {
                        let enabled = if let Some(state) = app.try_state::<AutostartState>() {
                            let mut guard = state.0.lock().unwrap();
                            let next = !*guard;
                            if set_autostart(next).is_ok() {
                                *guard = next;
                            }
                            *guard
                        } else {
                            false
                        };
                        if let Some(menu) = app.menu() {
                            if let Some(item) = menu.get("autostart") {
                                if let Some(menuitem) = item.as_menuitem() {
                                    let _ = menuitem.set_text(if enabled {
                                        "开机自启：开"
                                    } else {
                                        "开机自启：关"
                                    });
                                }
                            }
                        }
                    }
                    "check-update" => {
                        // 仅当构建配置携带真实更新源时才实际检查（§10.2）。
                        if updater_endpoint_is_placeholder() {
                            return;
                        }
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let result = match handle.updater() {
                                Ok(updater) => updater.check().await,
                                Err(error) => Err(error),
                            };
                            match result {
                                Ok(Some(update)) => {
                                    eprintln!(
                                        "[owo-desktop] 发现新版本 {}：{}",
                                        update.version,
                                        update.body.unwrap_or_default()
                                    );
                                    set_menu_text(&handle, "check-update", "检查更新（有新版本）");
                                }
                                Ok(None) => {
                                    set_menu_text(&handle, "check-update", "检查更新（已是最新）");
                                }
                                Err(error) => {
                                    eprintln!("[owo-desktop] 检查更新失败：{error}");
                                }
                            }
                        });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            app.manage(AutostartState(std::sync::Mutex::new(autostart_enabled())));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_core_state,
            commands::get_core_connection,
            commands::retry_core_start,
            commands::open_core_logs,
            commands::get_workspace,
            commands::set_workspace,
            commands::get_provider_status,
            commands::set_provider,
            commands::choose_data_directory,
            set_window_visible,
            desktop_pairing
        ])
        // §4.2 关闭到托盘协议：主窗口关闭请求一律拦截为隐藏到托盘，
        // 只有托盘「退出」才真正关闭核心与进程；首次隐藏发一次性后台提示。
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                    // §8.2：隐藏后必须把后台态注入页面——WebView2 的
                    // document.visibilityState 不会因 SetIsVisible 改变（实测）。
                    if let Some(webview) = window.get_webview_window("main") {
                        sync_window_background(&webview, false);
                    }
                    emit_background_notice_once(window.app_handle());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("构建 OwO Agent 桌面应用失败")
        .run(move |_app_handle, event| {
            if let RunEvent::Exit = event {
                runtime_for_exit.shutdown();
            }
        });
}

/// 关闭到托盘后的一次性后台提示（同会话只提示一次，避免骚扰）。
fn emit_background_notice_once(app: &tauri::AppHandle) {
    static NOTICED: AtomicBool = AtomicBool::new(false);
    if NOTICED.swap(true, Ordering::SeqCst) {
        return;
    }
    for window in app.webview_windows().values() {
        let _ = window.emit(
            "owo:background",
            serde_json::json!({ "message": "OwO Agent 仍在后台运行：右键托盘图标选「打开工作台」，或按 Ctrl+Alt+Shift+O、再次启动程序回到工作台。" }),
        );
    }
}

fn set_menu_text(app: &tauri::AppHandle, id: &str, text: &str) {
    if let Some(menu) = app.menu() {
        if let Some(item) = menu.get(id) {
            if let Some(menuitem) = item.as_menuitem() {
                let _ = menuitem.set_text(text);
            }
        }
    }
}

struct AutostartState(std::sync::Mutex<bool>);

/// 兼容保留：WebView 引导仍以 Tauri IPC 取配对证明（api-client.js 的
/// desktop_pairing 命令）；发布构建下引导还需实例身份头（get_core_connection）。
#[tauri::command]
fn desktop_pairing(runtime: tauri::State<'_, Arc<CoreRuntime>>) -> String {
    runtime.pairing().to_string()
}

// CORE_API_VERSION 供后续 get_core_connection 比对；当前经 commands 返回
// ready.apiVersion（来自 /health 实测值），此处引用避免 unused 警告。
const _: () = {
    // 编译期占位：保证常量仍被编译进二进制单一源。
};
#[allow(dead_code)]
fn _assert_core_api_version_in_scope() -> &'static str {
    CORE_API_VERSION
}
