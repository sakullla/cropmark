use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager, WebviewUrl, WebviewWindowBuilder,
};

use crate::autostart::{self, AutostartState};
use crate::hotkeys::{self, CaptureMode, HotkeyErrors, Hotkeys};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSettings {
    #[serde(default)]
    pub hotkeys: Hotkeys,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub hotkeys: Hotkeys,
    pub hotkey_errors: HotkeyErrors,
    pub autostart: AutostartState,
    pub notice: Option<String>,
}

pub struct SessionState {
    pub hotkeys: Mutex<Hotkeys>,
    pub hotkey_errors: Mutex<HotkeyErrors>,
    pub notice: Mutex<Option<String>>,
}

impl SessionState {
    pub fn from_hotkeys(hotkeys: Hotkeys) -> Self {
        Self {
            hotkeys: Mutex::new(hotkeys),
            hotkey_errors: Mutex::new(HotkeyErrors::default()),
            notice: Mutex::new(None),
        }
    }
}

pub fn load_from_app(app: &AppHandle) -> StoredSettings {
    load_from_path(&settings_path(app))
}

pub fn load_from_path(path: &std::path::Path) -> StoredSettings {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => StoredSettings::default(),
    }
}

pub fn save_to_path(path: &std::path::Path, settings: &StoredSettings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let text = serde_json::to_string_pretty(settings).map_err(|error| error.to_string())?;
    fs::write(path, text).map_err(|error| error.to_string())
}

pub fn open_settings(app: &AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("settings") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }

    WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html?view=settings".into()))
        .title("Cropmark")
        .inner_size(420.0, 560.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .skip_taskbar(true)
        .always_on_top(false)
        .visible(true)
        .center()
        .build()
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn snapshot(app: &AppHandle) -> UiSettings {
    let state = app.state::<SessionState>();
    let hotkeys = lock(&state.hotkeys).clone();
    let hotkey_errors = lock(&state.hotkey_errors).clone();
    let notice = lock(&state.notice).clone();
    UiSettings {
        hotkeys,
        hotkey_errors,
        autostart: autostart::current_state(),
        notice,
    }
}

#[tauri::command]
pub fn get_ui_settings(app: AppHandle) -> UiSettings {
    snapshot(&app)
}

#[tauri::command]
pub fn set_hotkey(app: AppHandle, mode: CaptureMode, accelerator: String) -> UiSettings {
    let mut hotkeys = lock(&app.state::<SessionState>().hotkeys).clone();
    hotkeys.set(mode, accelerator);
    persist_hotkeys(&app, &hotkeys);
    hotkeys::apply_to_app(&app, &hotkeys);
    snapshot(&app)
}

#[tauri::command]
pub fn set_autostart_enabled(app: AppHandle, enabled: bool) -> UiSettings {
    let _ = autostart::set_enabled(enabled);
    snapshot(&app)
}

fn persist_hotkeys(app: &AppHandle, hotkeys: &Hotkeys) {
    let stored = StoredSettings {
        hotkeys: hotkeys.clone(),
    };
    match save_to_path(&settings_path(app), &stored) {
        Ok(()) => *lock(&app.state::<SessionState>().notice) = None,
        Err(error) => {
            *lock(&app.state::<SessionState>().notice) =
                Some(format!("热键已应用，但未能写入本机设置：{error}"));
        }
    }
}

fn settings_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("settings.json")
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_file_loads_default_hotkeys_and_not_autostart() {
        let dir = std::env::temp_dir().join(format!("cropmark-settings-{}", std::process::id()));
        let path = dir.join("missing.json");
        let _ = fs::remove_file(&path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded.hotkeys, Hotkeys::default());
    }

    #[test]
    fn roundtrip_hotkeys_without_storing_autostart_enabled() {
        let dir = std::env::temp_dir().join(format!("cropmark-settings-save-{}", std::process::id()));
        let path = dir.join("settings.json");
        let stored = StoredSettings {
            hotkeys: Hotkeys {
                region: "Ctrl+Alt+R".into(),
                window: "Alt+Shift+W".into(),
                fullscreen: "Alt+Shift+S".into(),
            },
        };
        save_to_path(&path, &stored).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("autostart"));
        let loaded = load_from_path(&path);
        assert_eq!(loaded.hotkeys.region, "Ctrl+Alt+R");
        let _ = fs::remove_dir_all(&dir);
    }
}
