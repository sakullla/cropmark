mod annotate;
mod autostart;
mod capture;
mod clipboard;
mod export;
mod history;
mod hotkeys;
mod ocr;
mod pin;
mod settings;
mod single_instance;
mod tray;

use std::sync::{Arc, Mutex, OnceLock};

use hotkeys::CaptureMode;
use tauri::{Emitter, Manager};

pub fn dispatch_capture(app: &tauri::AppHandle, mode: CaptureMode) {
    // 热键与托盘主项读取设置中的延时(R4):0 秒立即截取。
    let delay_ms = settings::current_capture(app).delay_ms();
    dispatch_capture_with_delay(app, mode, delay_ms);
}

pub fn dispatch_capture_with_delay(app: &tauri::AppHandle, mode: CaptureMode, delay_ms: u64) {
    let _ = app.emit("capture-requested", mode);
    capture::begin(app, mode, delay_ms);
}

/// 托盘"上次区域"直取(R6):不进入交互选区,按记录区域抓屏裁剪;
/// 延时与热键/托盘主项同源,触发路径与常规截取一致。
pub fn dispatch_last_region(app: &tauri::AppHandle) {
    let delay_ms = settings::current_capture(app).delay_ms();
    let _ = app.emit("capture-requested", CaptureMode::Region);
    capture::begin_last_region(app, delay_ms);
}

pub fn should_prevent_exit(code: Option<i32>) -> bool {
    code.is_none()
}

/// R16:设置窗口的退出入口(无托盘环境下的兜底退出路径)。复用托盘"退出"的
/// 同一条链:`app.exit(0)` 触发 `RunEvent::Exit`,由既有逻辑收掉全部贴图。
/// 保持模块内私有:`pub` 会让 `#[tauri::command]` 生成的宏与 `#[macro_export]`
/// 在 crate 根同名冲突(crate 根即本模块)。
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/// 在任何线程请求唤出设置窗:非主线程转发到主线程,主线程直接执行。
fn open_settings_on_main(app: &tauri::AppHandle) {
    let app = app.clone();
    let task_app = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Err(error) = settings::open_settings(&task_app) {
            eprintln!("Cropmark: 无法打开设置窗口:{error}");
        }
    });
}

pub fn run() {
    // R1:单实例闸门先于任何 Tauri 初始化。已有实例时本进程在转发启动参数后
    // 直接退出,不创建托盘/窗口,也不注册热键;首实例无响应时同样有界退出。
    let settings_slot: Arc<OnceLock<tauri::AppHandle>> = Arc::new(OnceLock::new());
    let pending_activations: Arc<Mutex<Vec<single_instance::SecondLaunch>>> =
        Arc::new(Mutex::new(Vec::new()));
    let slot = Arc::clone(&settings_slot);
    let pending = Arc::clone(&pending_activations);
    let handler: single_instance::LaunchHandler =
        Arc::new(move |launch: single_instance::SecondLaunch| {
            let Some(app) = slot.get().cloned() else {
                // 首实例尚未完成 setup(Unix 监听线程可能先于窗口就绪收到转发):
                // 先记下,setup 完成后再补开设置窗,避免丢失这次激活。
                pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(launch);
                return;
            };
            open_settings_on_main(&app);
        });
    let _primary = match single_instance::acquire(single_instance::INSTANCE_ID, handler) {
        Ok(single_instance::Acquire::Primary(primary)) => Some(primary),
        Ok(single_instance::Acquire::Forwarded) => std::process::exit(0),
        Err(error) => {
            eprintln!("Cropmark: 单实例检测不可用,按普通启动继续:{error}");
            None
        }
    };

    capture::platform::enable_per_monitor_v2();
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(move |app| {
            let _ = settings_slot.set(app.handle().clone());
            let missed = std::mem::take(
                &mut *pending_activations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            if !missed.is_empty() {
                if let Err(error) = settings::open_settings(app.handle()) {
                    eprintln!("Cropmark: 无法打开设置窗口:{error}");
                }
            }

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let stored = settings::load_from_app(app.handle());
            let hotkeys = stored.hotkeys.clone();
            app.manage(settings::SessionState::from_stored(stored));
            app.manage(capture::session::CaptureRuntime::default());
            app.manage(ocr::OcrRuntime::default());
            // R16:托盘构建失败(典型为缺少 AppIndicator 的 Linux 桌面)不再
            // 中止启动:记录降级状态、继续注册热键,随后打开设置窗口展示无托盘
            // 提示与退出入口;构建成功时行为与以往一致。
            let tray_ready = match tray::install(app.handle()) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("Cropmark: 托盘不可用,继续以无托盘方式运行:{error}");
                    settings::mark_tray_unavailable(app.handle(), tray::unavailable_message());
                    false
                }
            };
            hotkeys::apply_to_app(app.handle(), &hotkeys);
            capture::precreate_windows(app.handle());
            if !tray_ready {
                if let Err(error) = settings::open_settings(app.handle()) {
                    eprintln!("Cropmark: 无法打开设置窗口:{error}");
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings::get_ui_settings,
            settings::set_hotkey,
            settings::set_autostart_enabled,
            settings::set_annotation_defaults,
            settings::set_feature,
            settings::set_capture_settings,
            settings::set_history_settings,
            capture::get_overlay_frame,
            capture::get_preview_frame,
            capture::confirm_region,
            capture::confirm_logical_region,
            capture::confirm_window,
            capture::finish_region_with,
            capture::get_toast_message,
            capture::cancel_capture,
            capture::close_preview,
            capture::close_capture_error,
            capture::get_delay_state,
            capture::get_capture_error,
            export::copy_preview_png,
            export::save_preview_png,
            ocr::recognize_preview,
            ocr::copy_ocr_point,
            ocr::copy_ocr_rect,
            ocr::copy_ocr_all,
            pin::pin_current,
            pin::get_pin_image,
            pin::copy_pin,
            pin::save_pin,
            pin::begin_pin_edit,
            pin::update_pin_from_preview,
            pin::get_pin_writeback,
            pin::close_pin,
            pin::close_all_pins,
            history::get_history,
            history::get_history_thumbnail,
            history::copy_history_entry,
            history::pin_history_entry,
            history::delete_history_entry,
            history::clear_history,
            history::open_history,
            quit_app,
        ])
        .on_window_event(|window, event| {
            // 贴图窗口销毁(手动关闭/显示器断开/退出)即释放标签与源图。
            if matches!(event, tauri::WindowEvent::Destroyed) {
                pin::handle_destroyed(window.label());
                // 预览窗销毁时收尾可能存在的贴图再标注会话(恢复来源贴图置顶)。
                if window.label() == capture::ui::PREVIEW {
                    capture::close_preview(window.app_handle().clone());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Cropmark failed to start")
        .run(|app, event| match event {
            tauri::RunEvent::ExitRequested { api, code, .. } => {
                if should_prevent_exit(code) {
                    api.prevent_exit();
                }
            }
            // 托盘退出(code=Some(0))等真实退出路径:退出前统一收掉贴图。
            tauri::RunEvent::Exit => pin::close_all(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::should_prevent_exit;

    #[test]
    fn closing_last_settings_window_keeps_tray_alive() {
        assert!(should_prevent_exit(None));
    }

    #[test]
    fn tray_quit_still_exits_the_process() {
        assert!(!should_prevent_exit(Some(0)));
    }
}
