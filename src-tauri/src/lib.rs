mod autostart;
mod hotkeys;
mod settings;
mod tray;

use hotkeys::CaptureMode;
use tauri::{Emitter, Manager};

pub fn dispatch_capture(app: &tauri::AppHandle, mode: CaptureMode) {
    let _ = app.emit("capture-requested", mode);
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let stored = settings::load_from_app(app.handle());
            app.manage(settings::SessionState::from_hotkeys(stored.hotkeys.clone()));
            tray::install(app.handle())?;
            hotkeys::apply_to_app(app.handle(), &stored.hotkeys);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            settings::get_ui_settings,
            settings::set_hotkey,
            settings::set_autostart_enabled,
        ])
        .run(tauri::generate_context!())
        .expect("Cropmark failed to start");
}
