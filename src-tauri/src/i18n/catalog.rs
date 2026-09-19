//! 词条目录(R12):与前端 `src/i18n/catalog.json` 共用同一份词条表。
//!
//! 目录以 `include_str!` 在编译期嵌入,运行期只解析一次;缺失词条回退到
//! 另一种语言的完整文案,两边都缺时返回键名(目录完整性由测试保证,
//! 运行期不允许空白文案)。

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;

use super::Language;

#[derive(Debug, Clone, Deserialize)]
struct Entry {
    #[serde(rename = "zh-CN")]
    zh_cn: String,
    en: String,
}

type Table = BTreeMap<String, Entry>;

static CATALOG: OnceLock<Table> = OnceLock::new();

fn catalog() -> &'static Table {
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../../../src/i18n/catalog.json"))
            .expect("i18n catalog must be valid JSON")
    })
}

/// 指定语言的文案;该语言缺失或为空时回退另一种语言,两边都空时回退键名。
pub fn lookup(lang: Language, key: &str) -> String {
    match catalog().get(key) {
        Some(entry) => resolve(lang, &entry.zh_cn, &entry.en),
        None => key.to_string(),
    }
}

/// 回退规则的可测试入口:`primary` 为空时取 `other`,都为空时取键名由调用方处理。
pub(crate) fn resolve(lang: Language, zh_cn: &str, en: &str) -> String {
    let (primary, fallback) = match lang {
        Language::ZhCn => (zh_cn, en),
        Language::En => (en, zh_cn),
    };
    if !primary.trim().is_empty() {
        return primary.to_string();
    }
    if !fallback.trim().is_empty() {
        return fallback.to_string();
    }
    String::new()
}

/// 目录完整性:每个词条两种语言都必须有非空文案。
#[cfg(test)]
pub fn missing_translations() -> Vec<String> {
    let mut missing = Vec::new();
    for (key, entry) in catalog() {
        if entry.zh_cn.trim().is_empty() || entry.en.trim().is_empty() {
            missing.push(key.clone());
        }
    }
    missing
}

#[cfg(test)]
pub fn key_count() -> usize {
    catalog().len()
}

/// 占位符替换:`{name}` → 参数值;未提供的占位符原样保留(不产生空白)。
pub fn substitute(template: &str, params: &[(&str, &str)]) -> String {
    let mut text = template.to_string();
    for (name, value) in params {
        text = text.replace(&format!("{{{name}}}"), value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_supported_language() {
        assert!(key_count() > 0);
        assert_eq!(missing_translations(), Vec::<String>::new());
    }

    #[test]
    fn missing_translation_falls_back_to_the_other_language() {
        assert_eq!(resolve(Language::En, "中文", ""), "中文");
        assert_eq!(resolve(Language::ZhCn, "", "English"), "English");
        assert_eq!(resolve(Language::ZhCn, "中文", "English"), "中文");
        assert_eq!(resolve(Language::En, "中文", "English"), "English");
        assert_eq!(resolve(Language::ZhCn, "", ""), "");
    }

    #[test]
    fn lookup_returns_text_and_never_a_blank_for_known_keys() {
        let zh = lookup(Language::ZhCn, "tray.capture");
        let en = lookup(Language::En, "tray.capture");
        assert_eq!(zh, "截取");
        assert_eq!(en, "Capture");
        assert_ne!(zh, "tray.capture");
    }

    #[test]
    fn unknown_keys_surface_as_the_key_itself() {
        // 未知键不返回空白:便于定位而不是静默失败。
        assert_eq!(lookup(Language::En, "no.such.key"), "no.such.key");
    }

    #[test]
    fn substitute_replaces_named_placeholders_only() {
        let text = substitute("已保存 {name}，共 {count} 张。", &[("name", "shot.png")]);
        assert_eq!(text, "已保存 shot.png，共 {count} 张。");
        let text = substitute("{a}-{b}", &[("a", "1"), ("b", "2")]);
        assert_eq!(text, "1-2");
    }
}
