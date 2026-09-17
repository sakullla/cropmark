use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::annotate::{parse_hex_color, DEFAULT_COLOR};
use crate::autostart::{self, AutostartState};
use crate::hotkeys::{self, CaptureMode, HotkeyErrors, Hotkeys};

/// 标注样式默认值：color 为 #hex；width/text_size 为 None 时沿用现有自动推导。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnnotationDefaults {
    pub color: String,
    pub width: Option<f64>,
    pub text_size: Option<f64>,
}

impl Default for AnnotationDefaults {
    fn default() -> Self {
        Self {
            color: DEFAULT_COLOR.into(),
            width: None,
            text_size: None,
        }
    }
}

impl AnnotationDefaults {
    /// 非法颜色回退默认色，非正/非有限数值回退 None（自动推导）。
    pub fn sanitized(self) -> Self {
        let color = if parse_hex_color(&self.color).is_some() {
            self.color
        } else {
            DEFAULT_COLOR.into()
        };
        let width = self
            .width
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|value| value.min(20.0));
        let text_size = self
            .text_size
            .filter(|value| value.is_finite() && *value >= 8.0)
            .map(|value| value.min(96.0));
        Self {
            color,
            width,
            text_size,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSettings {
    #[serde(default)]
    pub hotkeys: Hotkeys,
    #[serde(default)]
    pub annotation_defaults: AnnotationDefaults,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub hotkeys: Hotkeys,
    pub hotkey_errors: HotkeyErrors,
    pub autostart: AutostartState,
    pub notice: Option<String>,
    pub annotation_defaults: AnnotationDefaults,
}

pub struct SessionState {
    pub hotkeys: Mutex<Hotkeys>,
    pub hotkey_errors: Mutex<HotkeyErrors>,
    pub notice: Mutex<Option<String>>,
    pub autostart_rejection: Mutex<Option<String>>,
    pub annotation_defaults: Mutex<AnnotationDefaults>,
}

impl SessionState {
    pub fn from_stored(stored: StoredSettings) -> Self {
        Self {
            hotkeys: Mutex::new(stored.hotkeys),
            hotkey_errors: Mutex::new(HotkeyErrors::default()),
            notice: Mutex::new(None),
            autostart_rejection: Mutex::new(None),
            annotation_defaults: Mutex::new(stored.annotation_defaults.sanitized()),
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

    WebviewWindowBuilder::new(
        app,
        "settings",
        WebviewUrl::App("index.html?view=settings".into()),
    )
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
    let autostart_rejection = lock(&state.autostart_rejection).clone();
    let annotation_defaults = lock(&state.annotation_defaults).clone();
    UiSettings {
        hotkeys,
        hotkey_errors,
        autostart: autostart::merge_autostart_ui(autostart::current_state(), autostart_rejection),
        notice,
        annotation_defaults,
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
    persist_settings(&app, "热键已应用");
    hotkeys::apply_to_app(&app, &hotkeys);
    snapshot(&app)
}

#[tauri::command]
pub fn set_autostart_enabled(app: AppHandle, enabled: bool) -> UiSettings {
    let result = autostart::set_enabled(enabled);
    *lock(&app.state::<SessionState>().autostart_rejection) =
        autostart::remember_autostart_result(&result);
    let mut ui = snapshot(&app);
    ui.autostart = result;
    ui
}

#[tauri::command]
pub fn set_annotation_defaults(app: AppHandle, defaults: AnnotationDefaults) -> UiSettings {
    *lock(&app.state::<SessionState>().annotation_defaults) = defaults.sanitized();
    persist_settings(&app, "标注样式已应用");
    snapshot(&app)
}

fn persist_settings(app: &AppHandle, applied: &str) {
    let state = app.state::<SessionState>();
    let stored = StoredSettings {
        hotkeys: lock(&state.hotkeys).clone(),
        annotation_defaults: lock(&state.annotation_defaults).clone(),
    };
    match save_to_path(&settings_path(app), &stored) {
        Ok(()) => *lock(&state.notice) = None,
        Err(error) => {
            *lock(&state.notice) = Some(format!("{applied}，但未能写入本机设置：{error}"));
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
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-save-{}", std::process::id()));
        let path = dir.join("settings.json");
        let stored = StoredSettings {
            hotkeys: Hotkeys {
                region: "Ctrl+Alt+R".into(),
                window: "Alt+Shift+W".into(),
                fullscreen: "Alt+Shift+S".into(),
            },
            annotation_defaults: AnnotationDefaults {
                color: "#2563eb".into(),
                width: Some(5.0),
                text_size: Some(22.0),
            },
        };
        save_to_path(&path, &stored).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("autostart"));
        let loaded = load_from_path(&path);
        assert_eq!(loaded.hotkeys.region, "Ctrl+Alt+R");
        assert_eq!(loaded.annotation_defaults.color, "#2563eb");
        assert_eq!(loaded.annotation_defaults.width, Some(5.0));
        assert_eq!(loaded.annotation_defaults.text_size, Some(22.0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_annotation_defaults_field_loads_current_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-style-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.annotation_defaults, AnnotationDefaults::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_annotation_defaults_are_sanitized() {
        let dirty = AnnotationDefaults {
            color: "rose".into(),
            width: Some(-3.0),
            text_size: Some(4.0),
        };
        let clean = dirty.sanitized();
        assert_eq!(clean.color, crate::annotate::DEFAULT_COLOR);
        assert_eq!(clean.width, None);
        assert_eq!(clean.text_size, None);
    }

    #[test]
    fn set_autostart_uses_set_enabled_result_not_blank_live_query() {
        let result = autostart::map_platform_status(autostart::PlatformStatus::Denied(
            "access denied".into(),
        ));
        let stored = autostart::remember_autostart_result(&result);
        let live = autostart::map_platform_status(autostart::PlatformStatus::NotRegistered);
        let merged = autostart::merge_autostart_ui(live, stored.clone());
        let mut ui = UiSettings {
            hotkeys: Hotkeys::default(),
            hotkey_errors: HotkeyErrors::default(),
            autostart: merged,
            notice: None,
            annotation_defaults: AnnotationDefaults::default(),
        };
        ui.autostart = result.clone();
        assert!(!ui.autostart.enabled);
        assert_eq!(ui.autostart.message, result.message);
        assert!(ui.autostart.message.as_deref().unwrap().contains("拒绝"));
    }
}
