mod annotate;
mod autostart;
mod capture;
mod clipboard;
mod export;
mod hotkeys;
mod ocr;
mod settings;
mod tray;

use hotkeys::CaptureMode;
use tauri::{Emitter, Manager};

pub fn dispatch_capture(app: &tauri::AppHandle, mode: CaptureMode) {
    dispatch_capture_with_delay(app, mode, 0);
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
        ])
        .build(tauri::generate_context!())
        .expect("Cropmark failed to start")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if should_prevent_exit(code) {
                    api.prevent_exit();
                }
            }
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
