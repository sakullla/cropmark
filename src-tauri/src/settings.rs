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

/// 功能入口开关(默认全开):决定选区操作条/右键菜单/预览工具条的动作集,
/// 与 `capture::selection::FeatureFlags` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FeatureSettings {
    pub ocr_entry: bool,
    pub pin_entry: bool,
    pub magnifier: bool,
    pub toolbar_copy: bool,
    pub toolbar_save: bool,
    pub toolbar_pin: bool,
}

impl Default for FeatureSettings {
    fn default() -> Self {
        Self {
            ocr_entry: true,
            pin_entry: true,
            magnifier: true,
            toolbar_copy: true,
            toolbar_save: true,
            toolbar_pin: true,
        }
    }
}

impl FeatureSettings {
    /// 布尔开关无非法值,sanitize 仅保持字段形状对称(供 from_stored 统一走
    /// sanitized 路径)。
    pub fn sanitized(self) -> Self {
        self
    }

    /// 按键名设置单个开关(camelCase 优先,兼容 snake_case);未知键返回 None。
    pub fn with_key(self, key: &str, enabled: bool) -> Option<Self> {
        let mut next = self;
        match key {
            "ocrEntry" | "ocr_entry" => next.ocr_entry = enabled,
            "pinEntry" | "pin_entry" => next.pin_entry = enabled,
            "magnifier" => next.magnifier = enabled,
            "toolbarCopy" | "toolbar_copy" => next.toolbar_copy = enabled,
            "toolbarSave" | "toolbar_save" => next.toolbar_save = enabled,
            "toolbarPin" | "toolbar_pin" => next.toolbar_pin = enabled,
            _ => return None,
        }
        Some(next)
    }
}

/// 截图完成动作:进入预览,或静默完成(复制后关闭、仅 toast 反馈)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FinishAction {
    #[default]
    Preview,
    Quiet,
}

/// 延时合法上限(秒)。
pub const MAX_DELAY_SECONDS: u32 = 60;

/// 延时与截图后行为(R4)。`delay_seconds` 为 0 时热键与托盘立即截取;
/// `auto_copy` 关闭时完成路径不写剪贴板,静默完成因无输出被强制回退预览。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureSettings {
    pub delay_seconds: u32,
    pub auto_copy: bool,
    pub finish_action: FinishAction,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            delay_seconds: 0,
            auto_copy: true,
            finish_action: FinishAction::Preview,
        }
    }
}

impl CaptureSettings {
    /// 延时钳制到 0–60;autoCopy 关闭时静默完成无输出,强制回退预览,
    /// 与设置页禁用规则保持一致。
    pub fn sanitized(self) -> Self {
        Self {
            delay_seconds: self.delay_seconds.min(MAX_DELAY_SECONDS),
            auto_copy: self.auto_copy,
            finish_action: if self.auto_copy {
                self.finish_action
            } else {
                FinishAction::Preview
            },
        }
    }

    pub fn delay_ms(&self) -> u64 {
        u64::from(self.delay_seconds) * 1000
    }
}

/// 历史记录上限允许范围(条):低于 5 会被钳制,高于 200 会被钳制。
pub const MIN_HISTORY_LIMIT: u32 = 5;
pub const MAX_HISTORY_LIMIT: u32 = 200;

/// 本地截图历史(R2):关闭后不新增记录,已有关闭前记录保留;
/// limit 为保留条数上限,超出时按时间淘汰最旧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HistorySettings {
    pub enabled: bool,
    pub limit: u32,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: 20,
        }
    }
}

impl HistorySettings {
    /// limit 钳制到 5–200;布尔无非法值。
    pub fn sanitized(self) -> Self {
        Self {
            enabled: self.enabled,
            limit: self.limit.clamp(MIN_HISTORY_LIMIT, MAX_HISTORY_LIMIT),
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
    #[serde(default)]
    pub features: FeatureSettings,
    #[serde(default)]
    pub capture: CaptureSettings,
    #[serde(default)]
    pub history: HistorySettings,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiSettings {
    pub hotkeys: Hotkeys,
    pub hotkey_errors: HotkeyErrors,
    pub autostart: AutostartState,
    pub notice: Option<String>,
    pub annotation_defaults: AnnotationDefaults,
    pub features: FeatureSettings,
    pub capture: CaptureSettings,
    pub history: HistorySettings,
}

pub struct SessionState {
    pub hotkeys: Mutex<Hotkeys>,
    pub hotkey_errors: Mutex<HotkeyErrors>,
    pub notice: Mutex<Option<String>>,
    pub autostart_rejection: Mutex<Option<String>>,
    pub annotation_defaults: Mutex<AnnotationDefaults>,
    pub features: Mutex<FeatureSettings>,
    pub capture: Mutex<CaptureSettings>,
    pub history: Mutex<HistorySettings>,
}

impl SessionState {
    pub fn from_stored(stored: StoredSettings) -> Self {
        Self {
            hotkeys: Mutex::new(stored.hotkeys),
            hotkey_errors: Mutex::new(HotkeyErrors::default()),
            notice: Mutex::new(None),
            autostart_rejection: Mutex::new(None),
            annotation_defaults: Mutex::new(stored.annotation_defaults.sanitized()),
            features: Mutex::new(stored.features.sanitized()),
            capture: Mutex::new(stored.capture.sanitized()),
            history: Mutex::new(stored.history.sanitized()),
        }
    }
}

/// 供截取会话在选区引擎启动时读取当前功能开关。
pub fn current_features(app: &AppHandle) -> FeatureSettings {
    *lock(&app.state::<SessionState>().features)
}

/// 供截取链路(热键/托盘取延时、完成路径取动作与自动复制)即时读取。
/// 设置变更下一次截取即生效,无需重启。
pub fn current_capture(app: &AppHandle) -> CaptureSettings {
    *lock(&app.state::<SessionState>().capture)
}

/// 供完成路径判断是否写入历史记录(内存值即时生效)。
pub fn current_history(app: &AppHandle) -> HistorySettings {
    *lock(&app.state::<SessionState>().history)
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
    let features = *lock(&state.features);
    let capture = *lock(&state.capture);
    let history = *lock(&state.history);
    UiSettings {
        hotkeys,
        hotkey_errors,
        autostart: autostart::merge_autostart_ui(autostart::current_state(), autostart_rejection),
        notice,
        annotation_defaults,
        features,
        capture,
        history,
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

/// 设置单个功能入口开关;内存值立即生效(下一次截取起),随 persist_settings
/// 统一写盘(hotkeys+annotation_defaults+features),写盘失败沿 notice 提示。
#[tauri::command]
pub fn set_feature(
    app: AppHandle,
    key: String,
    enabled: bool,
) -> Result<UiSettings, String> {
    let next = {
        let state = app.state::<SessionState>();
        let current = *lock(&state.features);
        current
            .with_key(&key, enabled)
            .ok_or_else(|| format!("未知的功能开关：{key}"))?
    };
    *lock(&app.state::<SessionState>().features) = next;
    persist_settings(&app, "功能入口已应用");
    Ok(snapshot(&app))
}

/// 设置延时/自动复制/完成后动作;内存值立即生效(下一次截取起),随
/// persist_settings 统一写盘,并按新延时重建托盘菜单标签。
#[tauri::command]
pub fn set_capture_settings(app: AppHandle, settings: CaptureSettings) -> UiSettings {
    *lock(&app.state::<SessionState>().capture) = settings.sanitized();
    persist_settings(&app, "截图设置已应用");
    crate::tray::refresh_delay_menu(&app);
    snapshot(&app)
}

/// 设置历史开关与上限;上限调低时异步裁剪最旧记录,不阻塞设置窗口。
#[tauri::command]
pub fn set_history_settings(app: AppHandle, settings: HistorySettings) -> UiSettings {
    let next = settings.sanitized();
    *lock(&app.state::<SessionState>().history) = next;
    persist_settings(&app, "历史记录设置已应用");
    crate::history::prune_async(&app, next.limit);
    snapshot(&app)
}

fn persist_settings(app: &AppHandle, applied: &str) {
    let state = app.state::<SessionState>();
    let stored = StoredSettings {
        hotkeys: lock(&state.hotkeys).clone(),
        annotation_defaults: lock(&state.annotation_defaults).clone(),
        features: *lock(&state.features),
        capture: *lock(&state.capture),
        history: *lock(&state.history),
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
            features: FeatureSettings {
                ocr_entry: false,
                pin_entry: true,
                magnifier: false,
                toolbar_copy: true,
                toolbar_save: false,
                toolbar_pin: true,
            },
            capture: CaptureSettings {
                delay_seconds: 5,
                auto_copy: false,
                finish_action: FinishAction::Quiet,
            },
            history: HistorySettings {
                enabled: false,
                limit: 50,
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
        assert!(!loaded.features.ocr_entry);
        assert!(loaded.features.pin_entry);
        assert!(!loaded.features.magnifier);
        assert!(loaded.features.toolbar_copy);
        assert!(!loaded.features.toolbar_save);
        assert!(loaded.features.toolbar_pin);
        assert_eq!(loaded.capture.delay_seconds, 5);
        assert!(!loaded.capture.auto_copy);
        assert_eq!(loaded.capture.finish_action, FinishAction::Quiet);
        assert!(!loaded.history.enabled);
        assert_eq!(loaded.history.limit, 50);
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
    fn missing_features_field_loads_all_enabled_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-features-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.features, FeatureSettings::default());
        assert!(loaded.features.ocr_entry);
        assert!(loaded.features.magnifier);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn features_survive_sanitized() {
        let off = FeatureSettings {
            ocr_entry: false,
            pin_entry: false,
            magnifier: false,
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
        };
        assert_eq!(off.sanitized(), off);
    }

    #[test]
    fn with_key_applies_known_feature_keys_and_rejects_unknown() {
        let base = FeatureSettings::default();
        let off = base.with_key("ocrEntry", false).expect("camelCase key applies");
        assert!(!off.ocr_entry);
        let snake = base.with_key("toolbar_save", false).expect("snake_case key applies");
        assert!(!snake.toolbar_save);
        assert_eq!(base.with_key("captureHotkey", false), None);
        assert_eq!(base.with_key("", true), None);
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
            features: FeatureSettings::default(),
            capture: CaptureSettings::default(),
            history: HistorySettings::default(),
        };
        ui.autostart = result.clone();
        assert!(!ui.autostart.enabled);
        assert_eq!(ui.autostart.message, result.message);
        assert!(ui.autostart.message.as_deref().unwrap().contains("拒绝"));
    }

    #[test]
    fn capture_defaults_are_immediate_auto_copy_preview() {
        let capture = CaptureSettings::default();
        assert_eq!(capture.delay_seconds, 0);
        assert!(capture.auto_copy);
        assert_eq!(capture.finish_action, FinishAction::Preview);
        assert_eq!(capture.delay_ms(), 0);
    }

    #[test]
    fn capture_sanitize_clamps_delay_and_keeps_valid_seconds() {
        let clamped = CaptureSettings {
            delay_seconds: 120,
            auto_copy: true,
            finish_action: FinishAction::Quiet,
        }
        .sanitized();
        assert_eq!(clamped.delay_seconds, MAX_DELAY_SECONDS);
        assert_eq!(clamped.delay_ms(), 60_000);
        assert_eq!(clamped.finish_action, FinishAction::Quiet);

        let exact = CaptureSettings {
            delay_seconds: MAX_DELAY_SECONDS,
            auto_copy: true,
            finish_action: FinishAction::Preview,
        }
        .sanitized();
        assert_eq!(exact.delay_seconds, 60);
    }

    #[test]
    fn capture_sanitize_forces_preview_without_auto_copy() {
        let forced = CaptureSettings {
            delay_seconds: 5,
            auto_copy: false,
            finish_action: FinishAction::Quiet,
        }
        .sanitized();
        assert_eq!(forced.delay_seconds, 5);
        assert!(!forced.auto_copy);
        assert_eq!(forced.finish_action, FinishAction::Preview);
    }

    #[test]
    fn missing_capture_field_loads_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-capture-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.capture, CaptureSettings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_settings_deserialize_from_camel_case_json() {
        let parsed: StoredSettings = serde_json::from_str(
            r#"{"capture":{"delaySeconds":7,"autoCopy":false,"finishAction":"quiet"}}"#,
        )
        .unwrap();
        assert_eq!(parsed.capture.delay_seconds, 7);
        assert!(!parsed.capture.auto_copy);
        assert_eq!(parsed.capture.finish_action, FinishAction::Quiet);
        let serialized = serde_json::to_value(parsed.capture).unwrap();
        assert_eq!(serialized["delaySeconds"], 7);
        assert_eq!(serialized["autoCopy"], false);
        assert_eq!(serialized["finishAction"], "quiet");
    }

    #[test]
    fn history_defaults_to_enabled_with_twenty_records() {
        let history = HistorySettings::default();
        assert!(history.enabled);
        assert_eq!(history.limit, 20);
        assert_eq!(history.sanitized(), history);
    }

    #[test]
    fn history_sanitize_clamps_limit_into_range() {
        let low = HistorySettings {
            enabled: false,
            limit: 1,
        }
        .sanitized();
        assert_eq!(low.limit, MIN_HISTORY_LIMIT);
        assert!(!low.enabled);
        let high = HistorySettings {
            enabled: true,
            limit: 9_999,
        }
        .sanitized();
        assert_eq!(high.limit, MAX_HISTORY_LIMIT);
        let exact = HistorySettings {
            enabled: true,
            limit: MAX_HISTORY_LIMIT,
        }
        .sanitized();
        assert_eq!(exact.limit, MAX_HISTORY_LIMIT);
    }

    #[test]
    fn missing_history_field_loads_defaults() {
        let dir =
            std::env::temp_dir().join(format!("cropmark-settings-history-{}", std::process::id()));
        let path = dir.join("settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"hotkeys":{"region":"Ctrl+Alt+R","window":"Alt+Shift+W","fullscreen":"Alt+Shift+S"}}"#,
        )
        .unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.history, HistorySettings::default());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_settings_deserialize_from_camel_case_json() {
        let parsed: StoredSettings =
            serde_json::from_str(r#"{"history":{"enabled":false,"limit":80}}"#).unwrap();
        assert!(!parsed.history.enabled);
        assert_eq!(parsed.history.limit, 80);
        let serialized = serde_json::to_value(parsed.history).unwrap();
        assert_eq!(serialized["enabled"], false);
        assert_eq!(serialized["limit"], 80);
    }
}
