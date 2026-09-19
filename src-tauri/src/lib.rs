mod annotate;
mod autostart;
mod capture;
mod clipboard;
mod export;
mod hotkeys;
mod ocr;
mod pin;
mod settings;
mod tray;

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

pub fn should_prevent_exit(code: Option<i32>) -> bool {
    code.is_none()
}

pub fn run() {
    capture::platform::enable_per_monitor_v2();
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let stored = settings::load_from_app(app.handle());
            let hotkeys = stored.hotkeys.clone();
            app.manage(settings::SessionState::from_stored(stored));
            app.manage(capture::session::CaptureRuntime::default());
            app.manage(ocr::OcrRuntime::default());
            tray::install(app.handle())?;
            hotkeys::apply_to_app(app.handle(), &hotkeys);
            capture::precreate_windows(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings::get_ui_settings,
            settings::set_hotkey,
            settings::set_autostart_enabled,
            settings::set_annotation_defaults,
            settings::set_feature,
            settings::set_capture_settings,
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
            pin::close_pin,
            pin::close_all_pins,
        ])
        .on_window_event(|window, event| {
            // 贴图窗口销毁(手动关闭/显示器断开/退出)即释放标签与交接邮箱。
            if matches!(event, tauri::WindowEvent::Destroyed) {
                pin::handle_destroyed(window.label());
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
