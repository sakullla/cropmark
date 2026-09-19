//! 界面语言(R12,ADR-10):设置值 `system | zh-CN | en`,默认跟随系统。
//!
//! - 解析后的当前语言放在进程级原子里,`t()`/`tp()` 在生成文案时取当前值;
//! - 设置页切换语言时由 `settings::set_language` 更新并重建托盘菜单,
//!   再通过 `language-changed` 事件让各 webview 即时重渲染,无需重启;
//! - 系统检测:Windows `GetUserDefaultLocaleName`、macOS `NSLocale`
//!   首选语言、Linux 读 `LC_ALL/LC_MESSAGES/LANG`;检测失败回退中文。

pub mod catalog;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

impl Language {
    #[cfg(test)]
    pub fn tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    /// 由 BCP-47/环境变量形态的语言标签解析:以 `zh` 开头为中文,
    /// 其余(含未知)为英文;空标签返回 None。
    pub fn from_locale(tag: &str) -> Option<Self> {
        let normalized = tag.trim().replace('_', "-").to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        if normalized.starts_with("zh") {
            Some(Self::ZhCn)
        } else {
            Some(Self::En)
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::ZhCn => 1,
            Self::En => 2,
        }
    }
}

/// 设置值常量:仅允许这三种取值,未知值按 `system` 处理。
pub const SYSTEM_LANGUAGE: &str = "system";

static CURRENT: AtomicU8 = AtomicU8::new(0);
static DETECTED: OnceLock<Language> = OnceLock::new();

/// 设置值 → 解析后的语言;`system`/未知值按系统语言处理。
pub fn resolve_setting(setting: &str) -> Language {
    match setting.trim() {
        "zh-CN" | "zh" | "zh-Hans" | "zh-Hans-CN" => Language::ZhCn,
        "en" | "en-US" | "en-GB" => Language::En,
        _ => system_language(),
    }
}

/// 系统语言:检测失败时回退中文(现有界面与主要用户群)。
pub fn system_language() -> Language {
    *DETECTED.get_or_init(|| {
        os_locale()
            .and_then(|tag| Language::from_locale(&tag))
            .or_else(|| env_locale().and_then(|tag| Language::from_locale(&tag)))
            .unwrap_or(Language::ZhCn)
    })
}

pub fn set_language(lang: Language) {
    CURRENT.store(lang.code(), Ordering::Relaxed);
}

pub fn current() -> Language {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Language::ZhCn,
        2 => Language::En,
        _ => default_language(),
    }
}

fn default_language() -> Language {
    #[cfg(test)]
    {
        // 单测默认中文,消息断言与旧行为一致;英文文案通过 `tr` 显式验证。
        Language::ZhCn
    }
    #[cfg(not(test))]
    {
        system_language()
    }
}

/// 当前语言的文案;缺失词条回退另一种语言,不允许空白。
pub fn t(key: &str) -> String {
    catalog::lookup(current(), key)
}

/// 当前语言且带 `{name}` 占位符参数的文案。
pub fn tp(key: &str, params: &[(&str, &str)]) -> String {
    catalog::substitute(&catalog::lookup(current(), key), params)
}

/// 指定语言的文案(测试与需要显式语言的路径使用)。
#[cfg(test)]
pub fn tr(lang: Language, key: &str) -> String {
    catalog::lookup(lang, key)
}

/// 系统语言标签,仅供检测与测试使用。
fn os_locale() -> Option<String> {
    #[cfg(windows)]
    {
        if let Some(tag) = windows_locale() {
            return Some(tag);
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(tag) = macos_locale() {
            return Some(tag);
        }
    }
    env_locale()
}

fn env_locale() -> Option<String> {
    for name in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(name) {
            let value = value.trim();
            if !value.is_empty() && value != "C" && value != "POSIX" {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(windows)]
fn windows_locale() -> Option<String> {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;
    // LOCALE_NAME_MAX_LENGTH = 85,含结尾 NUL。
    let mut buffer = [0u16; 85];
    let written = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if written <= 1 {
        return None;
    }
    let text = String::from_utf16_lossy(&buffer[..(written as usize - 1)]);
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(target_os = "macos")]
fn macos_locale() -> Option<String> {
    let languages = objc2_foundation::NSLocale::preferredLanguages();
    let first = languages.firstObject()?;
    let text = first.to_string();
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_tags_map_to_supported_languages() {
        assert_eq!(Language::from_locale("zh-CN"), Some(Language::ZhCn));
        assert_eq!(Language::from_locale("zh_Hans_CN"), Some(Language::ZhCn));
        assert_eq!(Language::from_locale("en-US"), Some(Language::En));
        assert_eq!(Language::from_locale("fr-FR"), Some(Language::En));
        assert_eq!(Language::from_locale("  "), None);
        assert_eq!(Language::from_locale(""), None);
    }

    #[test]
    fn resolve_setting_accepts_known_tags_and_treats_unknown_as_system() {
        assert_eq!(resolve_setting("zh-CN"), Language::ZhCn);
        assert_eq!(resolve_setting("en"), Language::En);
        assert_eq!(resolve_setting(" en "), Language::En);
        // system/未知值都走系统检测,结果与 system_language 一致。
        assert_eq!(resolve_setting("system"), system_language());
        assert_eq!(resolve_setting("klingon"), system_language());
    }

    #[test]
    fn tags_roundtrip_through_serialization() {
        let zh = serde_json::to_string(&Language::ZhCn).unwrap();
        assert_eq!(zh, "\"zh-CN\"");
        let en: Language = serde_json::from_str("\"en\"").unwrap();
        assert_eq!(en, Language::En);
        assert_eq!(Language::En.tag(), "en");
    }

    #[test]
    fn t_and_tp_use_the_unconfigured_default_language() {
        // 单测不修改全局语言,避免并行测试互相干扰;默认语言为中文。
        assert_eq!(current(), Language::ZhCn);
        assert_eq!(t("tray.capture"), "截取");
        assert_eq!(
            tp("toast.saved", &[("name", "shot.png")]),
            "已保存 shot.png。"
        );
        assert_eq!(t("no.such.key"), "no.such.key");
    }

    #[test]
    fn explicit_tr_returns_the_requested_language() {
        assert_eq!(tr(Language::ZhCn, "tray.quit"), "退出");
        assert_eq!(tr(Language::En, "tray.quit"), "Quit");
        assert_eq!(tr(Language::En, "tray.capture"), "Capture");
    }

    #[test]
    fn system_detection_returns_a_supported_language() {
        // 覆盖各平台检测路径(Windows 区域 API / macOS NSLocale / 环境变量),
        // 结果必须是受支持语言且不 panic。
        assert!(matches!(system_language(), Language::ZhCn | Language::En));
    }
}
