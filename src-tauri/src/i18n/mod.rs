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

    fn collect_crate_sources(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).expect("read crate source dir");
        for entry in entries {
            let path = entry.expect("read crate source entry").path();
            if path.is_dir() {
                collect_crate_sources(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }

    /// 生产代码:截到 `#[cfg(test)] mod tests` 之前,测试夹具不参与扫描。
    fn production_source(source: &str) -> &str {
        const MARKER: &str = "#[cfg(test)]";
        let mut rest = source;
        while let Some(index) = rest.find(MARKER) {
            let after = &rest[index + MARKER.len()..];
            if after
                .chars()
                .take(24)
                .collect::<String>()
                .contains("mod tests")
            {
                return &rest[..index];
            }
            rest = after;
        }
        source
    }

    /// 提取字符串字面量;`r#"…"#` 形态按 `"#` 收尾整体跳过(内容不是键)。
    fn string_literals(source: &str) -> Vec<String> {
        let bytes = source.as_bytes();
        let mut literals = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'r' && bytes.get(index + 1) == Some(&b'#') {
                let mut cursor = index + 1;
                while bytes.get(cursor) == Some(&b'#') {
                    cursor += 1;
                }
                if bytes.get(cursor) == Some(&b'"') {
                    let mut end = cursor + 1;
                    while end + 1 < bytes.len() && !(bytes[end] == b'"' && bytes[end + 1] == b'#') {
                        end += 1;
                    }
                    index = end + 2;
                    continue;
                }
            }
            if bytes[index] != b'"' {
                index += 1;
                continue;
            }
            let mut value = String::new();
            let mut cursor = index + 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor += 2,
                    b'"' => break,
                    byte => {
                        value.push(byte as char);
                        cursor += 1;
                    }
                }
            }
            literals.push(value);
            index = cursor + 1;
        }
        literals
    }

    /// `t`/`tp`/`tr` 与 toast 入口调用里的字面量键(键是首个字面量参数)。
    fn call_site_literals(source: &str) -> Vec<String> {
        const MARKERS: [&str; 5] = [
            "i18n::t(",
            "i18n::tp(",
            "i18n::tr(",
            "show_toast_key(",
            "show_toast_key_params(",
        ];
        let mut keys = Vec::new();
        for marker in MARKERS {
            let mut rest = source;
            while let Some(index) = rest.find(marker) {
                let after = &rest[index + marker.len()..];
                if let Some(key) = first_literal_in_call(after) {
                    keys.push(key);
                }
                rest = after;
            }
        }
        keys
    }

    /// 该次调用(以左括号后为起点)中第一个字符串字面量;调用在字面量前
    /// 结束时返回 None。
    fn first_literal_in_call(after_open_paren: &str) -> Option<String> {
        let mut depth = 1usize;
        let mut chars = after_open_paren.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return None;
                    }
                }
                '"' => {
                    let mut value = String::new();
                    while let Some(next) = chars.next() {
                        if next == '\\' {
                            chars.next();
                        } else if next == '"' {
                            return Some(value);
                        } else {
                            value.push(next);
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }
        None
    }

    /// 键样式:小写点分,每段由小写字母/数字/下划线组成(至少两段)。
    fn looks_like_key(value: &str) -> bool {
        value.contains('.')
            && value
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_lowercase())
            && value.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            })
    }

    /// 文件名形态:末段是常见文件后缀(键不会这样收尾,`settings.json` 等
    /// 路径字面量必须排除)。
    fn looks_like_file_name(value: &str) -> bool {
        const SUFFIXES: [&str; 18] = [
            "json", "png", "jpg", "jpeg", "webp", "gif", "mp4", "toml", "log", "txt", "ico",
            "icns", "exe", "dmg", "deb", "appimage", "html", "css",
        ];
        value
            .rsplit_once('.')
            .is_some_and(|(_, suffix)| SUFFIXES.contains(&suffix))
    }

    /// 扫描器自身的约定:测试模块被截掉、键从调用点/存量字面量两路提取、
    /// 缺键会被识别出来而不是被过滤器吞掉。
    #[test]
    fn catalog_scan_helpers_behave_as_specified() {
        let source = r#"
fn sample() {
    let _ = i18n::t("error.capture.buffer_empty");
    let _ = i18n::tp("toast.saved", &[("name", &name)]);
    let unregistered_namespace = "notakind.zzz";
    let missing = i18n::t("error.capture.not_a_real_key");
    let path = "settings.json";
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixture() {
        let _ = i18n::t("error.capture.fixture_only");
    }
}
"#;
        let code = production_source(source);
        assert!(
            !code.contains("fixture_only"),
            "tests module must be cut before scanning"
        );

        let namespaces = catalog::namespaces();
        let mut keys = call_site_literals(code);
        keys.extend(string_literals(code).into_iter().filter(|value| {
            looks_like_key(value)
                && !looks_like_file_name(value)
                && value
                    .split_once('.')
                    .is_some_and(|(namespace, _)| namespaces.contains(namespace))
        }));
        keys.sort();
        keys.dedup();

        assert!(keys.contains(&"error.capture.buffer_empty".to_string()));
        assert!(keys.contains(&"toast.saved".to_string()));
        assert!(keys.contains(&"error.capture.not_a_real_key".to_string()));
        assert!(!keys.contains(&"name".to_string()));
        assert!(
            !keys.contains(&"notakind.zzz".to_string()),
            "unknown namespaces must not be treated as keys"
        );
        assert!(
            !keys.contains(&"settings.json".to_string()),
            "file names must not be treated as keys"
        );
        let missing: Vec<&String> = keys.iter().filter(|key| !catalog::contains(key)).collect();
        assert_eq!(
            missing,
            vec![&"error.capture.not_a_real_key".to_string()],
            "the scan must report exactly the unknown key"
        );
        assert!(looks_like_file_name("settings.json"));
        assert!(!looks_like_file_name("error.capture.buffer_empty"));
        assert!(!looks_like_key("error.capture."));
        assert!(!looks_like_key("Error.Capture"));
    }

    /// 源码扫描:生产代码里的键字面量必须存在于词条表中。运行期缺键会把
    /// 键名原样显示给用户;调用点不做命名空间过滤(前缀拼错也要抓到),
    /// 存量键(错误构造器、通知键等)按词条顶层命名空间过滤,避免把
    /// 坐标/标识符样式的字面量误判成键。
    #[test]
    fn literal_keys_in_production_sources_exist_in_the_catalog() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_crate_sources(&root, &mut files);
        assert!(files.len() > 40, "scan must see the crate: {}", files.len());

        let namespaces = catalog::namespaces();
        let mut checked = 0usize;
        let mut missing = Vec::new();
        for path in &files {
            let source = std::fs::read_to_string(path).expect("read crate source");
            let code = production_source(&source);
            for key in call_site_literals(code) {
                if !looks_like_key(&key) {
                    continue;
                }
                checked += 1;
                if !catalog::contains(&key) {
                    missing.push(format!("{}: {key}", path.display()));
                }
            }
            for literal in string_literals(code) {
                if !looks_like_key(&literal) || looks_like_file_name(&literal) {
                    continue;
                }
                let Some((namespace, _)) = literal.split_once('.') else {
                    continue;
                };
                if !namespaces.contains(namespace) {
                    continue;
                }
                checked += 1;
                if !catalog::contains(&literal) {
                    missing.push(format!("{}: {literal}", path.display()));
                }
            }
        }
        assert!(
            checked > 200,
            "expected to scan the crate keys, saw {checked}"
        );
        assert!(
            missing.is_empty(),
            "keys missing from the catalog: {missing:#?}"
        );
    }
}
