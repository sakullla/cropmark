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
    /// R1 手动滚动长截图:入口在托盘与选区壳(设置开关控制),无全局快捷键。
    LongCapture,
}

impl CaptureMode {
    /// 可绑定全局快捷键的模式集合(R1 长截图无热键,不在此列)。
    pub const ALL: [CaptureMode; 3] = [Self::Region, Self::Window, Self::Fullscreen];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hotkeys {
    pub region: String,
    pub window: String,
    pub fullscreen: String,
    /// R8 剪贴板贴图全局快捷键:可选绑定,默认空串(未绑定),不参与冲突检查。
    #[serde(default)]
    pub pin_clipboard: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            region: "Alt+Shift+A".to_string(),
            window: "Alt+Shift+W".to_string(),
            fullscreen: "Alt+Shift+S".to_string(),
            pin_clipboard: String::new(),
        }
    }
}

impl Hotkeys {
    pub fn get(&self, mode: CaptureMode) -> &str {
        match mode {
            CaptureMode::Region => &self.region,
            CaptureMode::Window => &self.window,
            CaptureMode::Fullscreen => &self.fullscreen,
            // 长截图不参与热键计划:调用方只用 ALL 中的模式。
            CaptureMode::LongCapture => "",
        }
    }

    pub fn set(&mut self, mode: CaptureMode, value: String) {
        match mode {
            CaptureMode::Region => self.region = value,
            CaptureMode::Window => self.window = value,
            CaptureMode::Fullscreen => self.fullscreen = value,
            CaptureMode::LongCapture => {}
        }
    }

    /// R8:剪贴板贴图热键(空串表示未绑定)。
    pub fn pin_clipboard(&self) -> &str {
        &self.pin_clipboard
    }

    pub fn set_pin_clipboard(&mut self, value: String) {
        self.pin_clipboard = value.trim().to_string();
    }
}

/// 热键计划与错误的动作目标:三种采集模式 + R8 剪贴板贴图(可选)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyTarget {
    Capture(CaptureMode),
    ClipboardPin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyErrors {
    pub region: Option<String>,
    pub window: Option<String>,
    pub fullscreen: Option<String>,
    /// R8:仅在已绑定但无法注册时出现(未绑定不报错)。
    pub pin_clipboard: Option<String>,
}

impl HotkeyErrors {
    pub fn set(&mut self, target: HotkeyTarget, message: Option<String>) {
        match target {
            HotkeyTarget::Capture(CaptureMode::Region) => self.region = message,
            HotkeyTarget::Capture(CaptureMode::Window) => self.window = message,
            HotkeyTarget::Capture(CaptureMode::Fullscreen) => self.fullscreen = message,
            HotkeyTarget::Capture(CaptureMode::LongCapture) => {}
            HotkeyTarget::ClipboardPin => self.pin_clipboard = message,
        }
    }

    /// 存储的是词条键(见 `plan_bindings`):按当前语言解析为展示文案,
    /// 语言切换后已解析文案不会残留旧语言。
    pub fn localized(&self) -> Self {
        Self {
            region: self.region.as_deref().map(i18n::t),
            window: self.window.as_deref().map(i18n::t),
            fullscreen: self.fullscreen.as_deref().map(i18n::t),
            pin_clipboard: self.pin_clipboard.as_deref().map(i18n::t),
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
    pub target: HotkeyTarget,
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
    let mut seen: HashMap<ParsedHotkey, HotkeyTarget> = HashMap::new();
    let mut planned = Vec::new();

    for mode in CaptureMode::ALL {
        planned.push(plan_binding(
            HotkeyTarget::Capture(mode),
            hotkeys.get(mode),
            &mut seen,
        ));
    }
    // R8:剪贴板贴图热键默认未绑定;空值既不注册也不产生错误提示。
    if !hotkeys.pin_clipboard.trim().is_empty() {
        planned.push(plan_binding(
            HotkeyTarget::ClipboardPin,
            hotkeys.pin_clipboard(),
            &mut seen,
        ));
    }

    planned
}

fn plan_binding(
    target: HotkeyTarget,
    raw: &str,
    seen: &mut HashMap<ParsedHotkey, HotkeyTarget>,
) -> PlannedBinding {
    let raw = raw.trim();
    let display = if raw.is_empty() {
        String::new()
    } else {
        parse_hotkey(raw)
            .map(|parsed| parsed.to_display())
            .unwrap_or_else(|_| raw.to_string())
    };

    match parse_hotkey(raw) {
        Ok(parsed) if is_system_screenshot(&parsed) => PlannedBinding {
            target,
            display,
            plugin_shortcut: None,
            error: Some("error.hotkey.system".to_string()),
        },
        Ok(parsed) if !parsed.has_modifier() => PlannedBinding {
            target,
            display,
            plugin_shortcut: None,
            error: Some("error.hotkey.modifier".to_string()),
        },
        Ok(parsed) => {
            if seen.contains_key(&parsed) {
                return PlannedBinding {
                    target,
                    display,
                    plugin_shortcut: None,
                    error: Some("error.hotkey.conflict".to_string()),
                };
            }
            let plugin = parsed.to_plugin();
            seen.insert(parsed, target);
            PlannedBinding {
                target,
                display,
                plugin_shortcut: Some(plugin),
                error: None,
            }
        }
        Err(error) => PlannedBinding {
            target,
            display,
            plugin_shortcut: None,
            error: Some(error),
        },
    }
}

pub fn finalize_plan(
    plan: Vec<PlannedBinding>,
    mut register: impl FnMut(&str) -> Result<(), String>,
) -> HotkeyErrors {
    let mut errors = HotkeyErrors::default();
    for item in plan {
        if let Some(error) = item.error {
            errors.set(item.target, Some(error));
            continue;
        }
        let Some(shortcut) = item.plugin_shortcut.as_deref() else {
            continue;
        };
        if let Err(_error) = register(shortcut) {
            errors.set(item.target, Some("error.hotkey.register".to_string()));
        }
    }
    errors
}

pub fn apply_to_app(app: &AppHandle, hotkeys: &Hotkeys) -> HotkeyErrors {
    let plan = plan_bindings(hotkeys);
    let _ = app.global_shortcut().unregister_all();
    let errors = finalize_plan(plan, |shortcut| {
        let target =
            target_for_shortcut(hotkeys, shortcut).ok_or_else(|| "unknown shortcut".to_string())?;
        app.global_shortcut()
            .on_shortcut(shortcut, move |app, _, event| {
                if event.state == ShortcutState::Pressed {
                    dispatch_target(app, target);
                }
            })
            .map_err(|error| error.to_string())
    });

    if let Some(state) = app.try_state::<SessionState>() {
        *lock_mutex(&state.hotkeys) = hotkeys.clone();
        *lock_mutex(&state.hotkey_errors) = errors.clone();
    }
    let failed = [
        errors.region.is_some(),
        errors.window.is_some(),
        errors.fullscreen.is_some(),
        errors.pin_clipboard.is_some(),
    ]
    .iter()
    .filter(|failed| **failed)
    .count();
    log::info!("hotkeys applied failed={failed}");
    errors
}

/// 快捷键按下后的动作分发:R8 剪贴板贴图走开关感知入口(关闭时静默失效),
/// 其余仍走既有采集链路。
fn dispatch_target(app: &AppHandle, target: HotkeyTarget) {
    match target {
        HotkeyTarget::Capture(mode) => crate::dispatch_capture(app, mode),
        HotkeyTarget::ClipboardPin => crate::pin::pin_from_clipboard(app),
    }
}

fn target_for_shortcut(hotkeys: &Hotkeys, shortcut: &str) -> Option<HotkeyTarget> {
    let plugin_of = |raw: &str| parse_hotkey(raw).ok().map(|parsed| parsed.to_plugin());
    for mode in CaptureMode::ALL {
        if plugin_of(hotkeys.get(mode)).as_deref() == Some(shortcut) {
            return Some(HotkeyTarget::Capture(mode));
        }
    }
    if plugin_of(hotkeys.pin_clipboard()).as_deref() == Some(shortcut) {
        return Some(HotkeyTarget::ClipboardPin);
    }
    None
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
            ..Hotkeys::default()
        };
        let plan = plan_bindings(&hotkeys);
        let window = plan
            .iter()
            .find(|item| item.target == HotkeyTarget::Capture(CaptureMode::Window))
            .unwrap();
        assert!(i18n::t(window.error.as_deref().unwrap()).contains("冲突"));
        assert!(window.plugin_shortcut.is_none());
        let region = plan
            .iter()
            .find(|item| item.target == HotkeyTarget::Capture(CaptureMode::Region))
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
            ..Hotkeys::default()
        };
        let plan = plan_bindings(&hotkeys);
        let region = plan
            .iter()
            .find(|item| item.target == HotkeyTarget::Capture(CaptureMode::Region))
            .unwrap();
        assert!(i18n::t(region.error.as_deref().unwrap()).contains("系统截图"));
        assert!(region.plugin_shortcut.is_none());
    }

    #[test]
    fn clipboard_pin_hotkey_is_unbound_by_default() {
        let hotkeys = Hotkeys::default();
        assert!(hotkeys.pin_clipboard().is_empty());
        let plan = plan_bindings(&hotkeys);
        assert!(plan
            .iter()
            .all(|item| item.target != HotkeyTarget::ClipboardPin));
        // 未绑定不产生错误,也不占用冲突表。
        let errors = finalize_plan(plan, |_| Ok(()));
        assert!(errors.pin_clipboard.is_none());
    }

    #[test]
    fn clipboard_pin_hotkey_plans_and_resolves_when_bound() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.set_pin_clipboard(" Ctrl+Alt+P ".into());
        assert_eq!(hotkeys.pin_clipboard(), "Ctrl+Alt+P");
        let plan = plan_bindings(&hotkeys);
        let item = plan
            .iter()
            .find(|item| item.target == HotkeyTarget::ClipboardPin)
            .expect("bound clipboard shortcut is planned");
        assert_eq!(item.display, "Ctrl+Alt+P");
        assert_eq!(item.plugin_shortcut.as_deref(), Some("control+alt+KeyP"));
        assert!(item.error.is_none());
        assert_eq!(
            target_for_shortcut(&hotkeys, "control+alt+KeyP"),
            Some(HotkeyTarget::ClipboardPin)
        );
    }

    #[test]
    fn clipboard_pin_hotkey_conflicts_are_visible_failures() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.set_pin_clipboard("Alt+Shift+A".into());
        let plan = plan_bindings(&hotkeys);
        let item = plan
            .iter()
            .find(|item| item.target == HotkeyTarget::ClipboardPin)
            .unwrap();
        // 与区域快捷键重复:计划里报冲突且不注册。
        assert!(i18n::t(item.error.as_deref().unwrap()).contains("冲突"));
        assert!(item.plugin_shortcut.is_none());

        hotkeys.set_pin_clipboard("PrintScreen".into());
        let item = plan_bindings(&hotkeys)
            .into_iter()
            .find(|item| item.target == HotkeyTarget::ClipboardPin)
            .unwrap();
        assert!(i18n::t(item.error.as_deref().unwrap()).contains("系统截图"));

        hotkeys.set_pin_clipboard("K".into());
        let item = plan_bindings(&hotkeys)
            .into_iter()
            .find(|item| item.target == HotkeyTarget::ClipboardPin)
            .unwrap();
        assert!(i18n::t(item.error.as_deref().unwrap()).contains("全局热键"));
    }

    #[test]
    fn hotkey_errors_localize_clipboard_pin_field() {
        let mut errors = HotkeyErrors::default();
        errors.set(
            HotkeyTarget::ClipboardPin,
            Some("error.hotkey.conflict".into()),
        );
        let localized = errors.localized();
        assert!(localized.pin_clipboard.as_deref().unwrap().contains("冲突"));
        assert!(localized.region.is_none());
    }
}
