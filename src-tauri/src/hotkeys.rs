use std::collections::HashMap;
use std::sync::MutexGuard;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

use crate::i18n;
use crate::settings::SessionState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    Region,
    Window,
    Fullscreen,
}

impl CaptureMode {
    pub const ALL: [CaptureMode; 3] = [Self::Region, Self::Window, Self::Fullscreen];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hotkeys {
    pub region: String,
    pub window: String,
    pub fullscreen: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            region: "Alt+Shift+A".to_string(),
            window: "Alt+Shift+W".to_string(),
            fullscreen: "Alt+Shift+S".to_string(),
        }
    }
}

impl Hotkeys {
    pub fn get(&self, mode: CaptureMode) -> &str {
        match mode {
            CaptureMode::Region => &self.region,
            CaptureMode::Window => &self.window,
            CaptureMode::Fullscreen => &self.fullscreen,
        }
    }

    pub fn set(&mut self, mode: CaptureMode, value: String) {
        match mode {
            CaptureMode::Region => self.region = value,
            CaptureMode::Window => self.window = value,
            CaptureMode::Fullscreen => self.fullscreen = value,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyErrors {
    pub region: Option<String>,
    pub window: Option<String>,
    pub fullscreen: Option<String>,
}

impl HotkeyErrors {
    pub fn set(&mut self, mode: CaptureMode, message: Option<String>) {
        match mode {
            CaptureMode::Region => self.region = message,
            CaptureMode::Window => self.window = message,
            CaptureMode::Fullscreen => self.fullscreen = message,
        }
    }

    /// 存储的是词条键(见 `plan_bindings`):按当前语言解析为展示文案,
    /// 语言切换后已解析文案不会残留旧语言。
    pub fn localized(&self) -> Self {
        Self {
            region: self.region.as_deref().map(i18n::t),
            window: self.window.as_deref().map(i18n::t),
            fullscreen: self.fullscreen.as_deref().map(i18n::t),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedHotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: String,
}

impl ParsedHotkey {
    pub fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.meta
    }

    pub fn to_display(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.meta {
            parts.push("Super");
        }
        parts.push(self.key.as_str());
        parts.join("+")
    }

    pub fn to_plugin(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("control");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.meta {
            parts.push("meta");
        }
        parts.push(plugin_key(&self.key));
        parts.join("+")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedBinding {
    pub mode: CaptureMode,
    pub display: String,
    pub plugin_shortcut: Option<String>,
    pub error: Option<String>,
}

pub fn parse_hotkey(input: &str) -> Result<ParsedHotkey, String> {
    let tokens: Vec<&str> = input
        .split('+')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err("error.hotkey.invalid".to_string());
    }

    let mut parsed = ParsedHotkey {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
        key: String::new(),
    };

    for token in tokens {
        match normalize_token(token) {
            Token::Ctrl => parsed.ctrl = true,
            Token::Alt => parsed.alt = true,
            Token::Shift => parsed.shift = true,
            Token::Meta => parsed.meta = true,
            Token::Key(key) => {
                if !parsed.key.is_empty() {
                    return Err("error.hotkey.invalid".to_string());
                }
                parsed.key = key;
            }
        }
    }

    if parsed.key.is_empty() {
        return Err("error.hotkey.invalid".to_string());
    }
    Ok(parsed)
}

pub fn is_system_screenshot(hotkey: &ParsedHotkey) -> bool {
    if hotkey.key.eq_ignore_ascii_case("PrintScreen") {
        return true;
    }
    if hotkey.meta && hotkey.shift && hotkey.key.eq_ignore_ascii_case("S") && !hotkey.alt {
        return true;
    }
    hotkey.meta && hotkey.shift && matches!(hotkey.key.as_str(), "3" | "4" | "5")
}

pub fn plan_bindings(hotkeys: &Hotkeys) -> Vec<PlannedBinding> {
    let mut seen: HashMap<ParsedHotkey, CaptureMode> = HashMap::new();
    let mut planned = Vec::new();

    for mode in CaptureMode::ALL {
        let raw = hotkeys.get(mode).trim();
        let display = if raw.is_empty() {
            String::new()
        } else {
            parse_hotkey(raw)
                .map(|parsed| parsed.to_display())
                .unwrap_or_else(|_| raw.to_string())
        };

        match parse_hotkey(raw) {
            Ok(parsed) if is_system_screenshot(&parsed) => planned.push(PlannedBinding {
                mode,
                display,
                plugin_shortcut: None,
                error: Some("error.hotkey.system".to_string()),
            }),
            Ok(parsed) if !parsed.has_modifier() => planned.push(PlannedBinding {
                mode,
                display,
                plugin_shortcut: None,
                error: Some("error.hotkey.modifier".to_string()),
            }),
            Ok(parsed) => {
                if let Some(_owner) = seen.get(&parsed) {
                    planned.push(PlannedBinding {
                        mode,
                        display,
                        plugin_shortcut: None,
                        error: Some("error.hotkey.conflict".to_string()),
                    });
                    continue;
                }
                seen.insert(parsed.clone(), mode);
                planned.push(PlannedBinding {
                    mode,
                    display,
                    plugin_shortcut: Some(parsed.to_plugin()),
                    error: None,
                });
            }
            Err(error) => planned.push(PlannedBinding {
                mode,
                display,
                plugin_shortcut: None,
                error: Some(error),
            }),
        }
    }

    planned
}

pub fn finalize_plan(
    plan: Vec<PlannedBinding>,
    mut register: impl FnMut(&str) -> Result<(), String>,
) -> HotkeyErrors {
    let mut errors = HotkeyErrors::default();
    for item in plan {
        if let Some(error) = item.error {
            errors.set(item.mode, Some(error));
            continue;
        }
        let Some(shortcut) = item.plugin_shortcut.as_deref() else {
            continue;
        };
        if let Err(_error) = register(shortcut) {
            errors.set(item.mode, Some("error.hotkey.register".to_string()));
        }
    }
    errors
}

pub fn apply_to_app(app: &AppHandle, hotkeys: &Hotkeys) -> HotkeyErrors {
    let plan = plan_bindings(hotkeys);
    let _ = app.global_shortcut().unregister_all();
    let errors = finalize_plan(plan, |shortcut| {
        let mode = plan_mode_for_shortcut(hotkeys, shortcut)
            .ok_or_else(|| "unknown shortcut".to_string())?;
        app.global_shortcut()
            .on_shortcut(shortcut, move |app, _, event| {
                if event.state == ShortcutState::Pressed {
                    crate::dispatch_capture(app, mode);
                }
            })
            .map_err(|error| error.to_string())
    });

    if let Some(state) = app.try_state::<SessionState>() {
        *lock_mutex(&state.hotkeys) = hotkeys.clone();
        *lock_mutex(&state.hotkey_errors) = errors.clone();
    }
    errors
}

fn plan_mode_for_shortcut(hotkeys: &Hotkeys, shortcut: &str) -> Option<CaptureMode> {
    CaptureMode::ALL.into_iter().find(|mode| {
        parse_hotkey(hotkeys.get(*mode))
            .ok()
            .map(|parsed| parsed.to_plugin() == shortcut)
            .unwrap_or(false)
    })
}

fn lock_mutex<T>(mutex: &std::sync::Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

enum Token {
    Ctrl,
    Alt,
    Shift,
    Meta,
    Key(String),
}

fn normalize_token(token: &str) -> Token {
    match token.to_ascii_uppercase().as_str() {
        "CTRL" | "CONTROL" => Token::Ctrl,
        "ALT" | "OPTION" => Token::Alt,
        "SHIFT" => Token::Shift,
        "SUPER" | "META" | "CMD" | "COMMAND" | "WIN" | "WINDOWS" => Token::Meta,
        other => Token::Key(normalize_key(other)),
    }
}

fn normalize_key(token: &str) -> String {
    let upper = token.to_ascii_uppercase();
    if let Some(letter) = upper.strip_prefix("KEY") {
        if letter.len() == 1 && letter.chars().all(|ch| ch.is_ascii_alphabetic()) {
            return letter.to_string();
        }
    }
    if let Some(digit) = upper.strip_prefix("DIGIT") {
        if digit.len() == 1 && digit.chars().all(|ch| ch.is_ascii_digit()) {
            return digit.to_string();
        }
    }
    match upper.as_str() {
        "PRINTSCREEN" | "PRTSC" | "PRTSCN" => "PrintScreen".to_string(),
        "ESC" | "ESCAPE" => "Esc".to_string(),
        "ARROWUP" | "UP" => "Up".to_string(),
        "ARROWDOWN" | "DOWN" => "Down".to_string(),
        "ARROWLEFT" | "LEFT" => "Left".to_string(),
        "ARROWRIGHT" | "RIGHT" => "Right".to_string(),
        " " | "SPACE" | "SPACEBAR" => "Space".to_string(),
        other if other.len() == 1 => other.to_string(),
        other => title_case_key(other),
    }
}

fn title_case_key(token: &str) -> String {
    let mut chars = token.chars();
    match chars.next() {
        Some(first) => first.to_string() + &chars.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}

fn plugin_key(key: &str) -> &str {
    match key {
        "A" => "KeyA",
        "B" => "KeyB",
        "C" => "KeyC",
        "D" => "KeyD",
        "E" => "KeyE",
        "F" => "KeyF",
        "G" => "KeyG",
        "H" => "KeyH",
        "I" => "KeyI",
        "J" => "KeyJ",
        "K" => "KeyK",
        "L" => "KeyL",
        "M" => "KeyM",
        "N" => "KeyN",
        "O" => "KeyO",
        "P" => "KeyP",
        "Q" => "KeyQ",
        "R" => "KeyR",
        "S" => "KeyS",
        "T" => "KeyT",
        "U" => "KeyU",
        "V" => "KeyV",
        "W" => "KeyW",
        "X" => "KeyX",
        "Y" => "KeyY",
        "Z" => "KeyZ",
        "0" => "Digit0",
        "1" => "Digit1",
        "2" => "Digit2",
        "3" => "Digit3",
        "4" => "Digit4",
        "5" => "Digit5",
        "6" => "Digit6",
        "7" => "Digit7",
        "8" => "Digit8",
        "9" => "Digit9",
        "PrintScreen" => "PrintScreen",
        "Esc" => "Escape",
        "Up" => "ArrowUp",
        "Down" => "ArrowDown",
        "Left" => "ArrowLeft",
        "Right" => "ArrowRight",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_alt_shift_aws() {
        let hotkeys = Hotkeys::default();
        assert_eq!(hotkeys.region, "Alt+Shift+A");
        assert_eq!(hotkeys.window, "Alt+Shift+W");
        assert_eq!(hotkeys.fullscreen, "Alt+Shift+S");
        for mode in CaptureMode::ALL {
            let parsed = parse_hotkey(hotkeys.get(mode)).expect("default must parse");
            assert!(parsed.alt && parsed.shift && !parsed.ctrl && !parsed.meta);
        }
    }

    #[test]
    fn defaults_do_not_take_system_screenshot_keys() {
        let hotkeys = Hotkeys::default();
        for mode in CaptureMode::ALL {
            let parsed = parse_hotkey(hotkeys.get(mode)).unwrap();
            assert!(
                !is_system_screenshot(&parsed),
                "{} collides with a system screenshot key",
                hotkeys.get(mode)
            );
        }
        for forbidden in [
            "Super+Shift+S",
            "Meta+Shift+3",
            "Cmd+Shift+4",
            "Command+Shift+5",
            "PrintScreen",
            "Shift+PrintScreen",
            "Alt+PrintScreen",
            "Win+Shift+S",
        ] {
            let parsed = parse_hotkey(forbidden).expect(forbidden);
            assert!(is_system_screenshot(&parsed), "{forbidden}");
        }
    }

    #[test]
    fn display_and_plugin_roundtrip() {
        let parsed = parse_hotkey("alt+shift+a").unwrap();
        assert_eq!(parsed.to_display(), "Alt+Shift+A");
        assert_eq!(parsed.to_plugin(), "alt+shift+KeyA");
        let again = parse_hotkey(&parsed.to_plugin()).unwrap();
        assert_eq!(parsed, again);
    }

    #[test]
    fn default_plugin_strings_parse_in_global_shortcut() {
        for accel in ["Alt+Shift+A", "Alt+Shift+W", "Alt+Shift+S"] {
            let plugin = parse_hotkey(accel).unwrap().to_plugin();
            plugin
                .parse::<tauri_plugin_global_shortcut::Shortcut>()
                .unwrap_or_else(|error| panic!("{plugin} should parse: {error}"));
        }
    }

    #[test]
    fn duplicate_bindings_fail_later_action_without_panic() {
        let hotkeys = Hotkeys {
            region: "Alt+Shift+A".into(),
            window: "alt+shift+KeyA".into(),
            fullscreen: "Alt+Shift+S".into(),
        };
        let plan = plan_bindings(&hotkeys);
        let window = plan
            .iter()
            .find(|item| item.mode == CaptureMode::Window)
            .unwrap();
        assert!(i18n::t(window.error.as_deref().unwrap()).contains("冲突"));
        assert!(window.plugin_shortcut.is_none());
        let region = plan
            .iter()
            .find(|item| item.mode == CaptureMode::Region)
            .unwrap();
        assert!(region.error.is_none());
    }

    #[test]
    fn registrar_conflict_records_error_and_does_not_panic() {
        let plan = plan_bindings(&Hotkeys::default());
        let errors = finalize_plan(plan, |shortcut| {
            if shortcut.ends_with("KeyA") {
                Err("already registered".into())
            } else {
                Ok(())
            }
        });
        assert!(i18n::t(errors.region.as_deref().unwrap()).contains("无法注册"));
        assert!(errors.window.is_none());
        assert!(errors.fullscreen.is_none());
    }

    #[test]
    fn all_register_failures_still_return() {
        let plan = plan_bindings(&Hotkeys::default());
        let errors = finalize_plan(plan, |_| Err("no".into()));
        assert!(errors.region.is_some());
        assert!(errors.window.is_some());
        assert!(errors.fullscreen.is_some());
    }

    #[test]
    fn system_screenshot_plan_is_visible_failure() {
        let hotkeys = Hotkeys {
            region: "Win+Shift+S".into(),
            window: "Alt+Shift+W".into(),
            fullscreen: "Alt+Shift+S".into(),
        };
        let plan = plan_bindings(&hotkeys);
        let region = plan
            .iter()
            .find(|item| item.mode == CaptureMode::Region)
            .unwrap();
        assert!(i18n::t(region.error.as_deref().unwrap()).contains("系统截图"));
        assert!(region.plugin_shortcut.is_none());
    }
}
