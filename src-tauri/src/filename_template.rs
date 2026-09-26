//! 保存文件名模板与不覆盖路径(R11, ADR-12)。
//!
//! 占位符只有 `{date}` `{time}` `{datetime}` `{mode}` `{seq}`。未知占位符按字面
//! 保留。非法字符换成 `_`;结果为空或不可用时回退 `Cropmark_<时间戳>`。
//! `{seq}` 与无序号时的 ` (n)` 共用同一套「找第一个不存在的路径」解析。
//! 模板只影响预览里的本地保存默认名,不改剪贴板、历史或贴图保存。

use std::path::{Path, PathBuf};

const MAX_STEM: usize = 120;
const MAX_TEMPLATE: usize = 180;
const MAX_SEQ: u32 = 9999;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameParts {
    pub date: String,
    pub time: String,
    pub datetime: String,
    pub mode: String,
}

impl NameParts {
    pub fn from_clock(clock: [u32; 6], mode: &str) -> Self {
        let date = format!("{:04}-{:02}-{:02}", clock[0], clock[1], clock[2]);
        let time = format!("{:02}-{:02}-{:02}", clock[3], clock[4], clock[5]);
        Self {
            datetime: format!("{date}_{time}"),
            date,
            time,
            mode: mode.to_string(),
        }
    }

    pub fn now(mode: &str) -> Self {
        Self::from_clock(local_date_time(), mode)
    }

    pub fn fallback_stem(&self) -> String {
        format!("Cropmark_{}", self.datetime)
    }
}

pub fn local_stamp() -> String {
    NameParts::now("region").datetime
}

/// 设置里保存的模板:去控制字符并限长,非法文件名字符留到渲染时再替换。
pub fn sanitize_template(value: &str) -> String {
    let mut out = String::new();
    for ch in value.trim().chars() {
        if ch.is_control() {
            continue;
        }
        out.push(ch);
        if out.chars().count() >= MAX_TEMPLATE {
            break;
        }
    }
    out
}

pub fn suggest_with(
    directory: Option<&Path>,
    template_on: bool,
    template: &str,
    parts: &NameParts,
    extension: &str,
) -> String {
    let extension = extension.trim_matches('.');
    let fallback = parts.fallback_stem();
    if !template_on {
        return unique_file_name(directory, &fallback, extension);
    }
    let template = sanitize_template(template);
    if template.is_empty() {
        return unique_file_name(directory, &fallback, extension);
    }
    if template.contains("{seq}") {
        let pattern = sanitize_stem_keep_seq(&substitute(&template, parts));
        if pattern.contains("{seq}") {
            return seq_file_name(directory, &pattern, extension, &fallback);
        }
        return unique_file_name(directory, &fallback, extension);
    }
    let stem = sanitize_stem(&substitute(&template, parts));
    let stem = if unusable(&stem) { fallback } else { stem };
    unique_file_name(directory, &stem, extension)
}

/// 目标已存在时改为 `stem (n).ext`,保证不覆盖。目录不存在时原样返回。
pub fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("Cropmark");
    parent.join(unique_file_name(Some(parent), stem, extension))
}

fn seq_file_name(
    directory: Option<&Path>,
    pattern: &str,
    extension: &str,
    fallback: &str,
) -> String {
    for seq in 1..=MAX_SEQ {
        let stem = sanitize_stem(&pattern.replace("{seq}", &seq.to_string()));
        let stem = if unusable(&stem) {
            format!("{fallback}_{seq}")
        } else {
            stem
        };
        let name = join_name(&stem, extension);
        if !name_exists(directory, &name) {
            return name;
        }
    }
    join_name(&format!("{fallback}_{MAX_SEQ}"), extension)
}

fn unique_file_name(directory: Option<&Path>, stem: &str, extension: &str) -> String {
    let stem = sanitize_stem(stem);
    let stem = if unusable(&stem) {
        "Cropmark".to_string()
    } else {
        stem
    };
    let direct = join_name(&stem, extension);
    if !name_exists(directory, &direct) {
        return direct;
    }
    for seq in 1..=MAX_SEQ {
        let name = join_name(&format!("{stem} ({seq})"), extension);
        if !name_exists(directory, &name) {
            return name;
        }
    }
    join_name(&format!("{stem} ({MAX_SEQ})"), extension)
}

fn name_exists(directory: Option<&Path>, name: &str) -> bool {
    directory.is_some_and(|dir| dir.join(name).exists())
}

fn join_name(stem: &str, extension: &str) -> String {
    if extension.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{extension}")
    }
}

fn substitute(template: &str, parts: &NameParts) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '{' {
            if let Some(end) = chars[index + 1..].iter().position(|ch| *ch == '}') {
                let key: String = chars[index + 1..index + 1 + end].iter().collect();
                let replacement = match key.as_str() {
                    "date" => Some(parts.date.as_str()),
                    "time" => Some(parts.time.as_str()),
                    "datetime" => Some(parts.datetime.as_str()),
                    "mode" => Some(parts.mode.as_str()),
                    "seq" => Some("{seq}"),
                    _ => None,
                };
                if let Some(value) = replacement {
                    out.push_str(value);
                    index += end + 2;
                    continue;
                }
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn sanitize_stem_keep_seq(raw: &str) -> String {
    raw.split("{seq}")
        .map(sanitize_piece)
        .collect::<Vec<_>>()
        .join("{seq}")
}

fn sanitize_stem(raw: &str) -> String {
    let stem = sanitize_piece(raw);
    if is_reserved(&stem) {
        limit_stem(&format!("{stem}_"))
    } else {
        stem
    }
}

fn sanitize_piece(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    limit_stem(out.trim_matches(|ch: char| ch == ' ' || ch == '.'))
}

fn limit_stem(stem: &str) -> String {
    stem.chars().take(MAX_STEM).collect()
}

fn unusable(stem: &str) -> bool {
    let trimmed = stem.trim();
    trimmed.is_empty()
        || trimmed
            .chars()
            .all(|ch| ch == '_' || ch == '.' || ch == ' ')
}

fn is_reserved(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or(stem);
    matches!(
        base.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(windows)]
fn local_date_time() -> [u32; 6] {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let now = unsafe { GetLocalTime() };
    [
        u32::from(now.wYear),
        u32::from(now.wMonth),
        u32::from(now.wDay),
        u32::from(now.wHour),
        u32::from(now.wMinute),
        u32::from(now.wSecond),
    ]
}

#[cfg(not(windows))]
fn local_date_time() -> [u32; 6] {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut broken = std::mem::zeroed();
        libc::localtime_r(&now, &mut broken);
        [
            (broken.tm_year + 1900) as u32,
            (broken.tm_mon + 1) as u32,
            broken.tm_mday as u32,
            broken.tm_hour as u32,
            broken.tm_min as u32,
            broken.tm_sec as u32,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> NameParts {
        NameParts::from_clock([2026, 9, 26, 21, 5, 8], "region")
    }

    #[test]
    fn placeholders_render_and_unknown_tokens_stay_literal() {
        let name = suggest_with(
            None,
            true,
            "{date}_{time}_{datetime}_{mode}_{foo}",
            &parts(),
            "png",
        );
        assert_eq!(
            name,
            "2026-09-26_21-05-08_2026-09-26_21-05-08_region_{foo}.png"
        );
    }

    #[test]
    fn illegal_characters_become_underscores_and_empty_falls_back() {
        let name = suggest_with(None, true, "a/b:c*d?\"e<f>g|h", &parts(), "jpg");
        assert_eq!(name, "a_b_c_d__e_f_g_h.jpg");
        let fallback = suggest_with(None, true, "   ", &parts(), "png");
        assert_eq!(fallback, "Cropmark_2026-09-26_21-05-08.png");
        let stars = suggest_with(None, true, "***", &parts(), "webp");
        assert_eq!(stars, "Cropmark_2026-09-26_21-05-08.webp");
        assert_eq!(
            suggest_with(None, false, "{mode}", &parts(), "png"),
            "Cropmark_2026-09-26_21-05-08.png"
        );
    }

    #[test]
    fn mode_placeholder_keeps_the_supplied_token() {
        let window = NameParts::from_clock([2026, 1, 2, 3, 4, 5], "window");
        assert_eq!(
            suggest_with(None, true, "shot_{mode}", &window, "png"),
            "shot_window.png"
        );
        let long = NameParts::from_clock([2026, 1, 2, 3, 4, 5], "long");
        assert_eq!(
            suggest_with(None, true, "{mode}_{seq}", &long, "png"),
            "long_1.png"
        );
    }

    #[test]
    fn unique_path_appends_parenthetical_index_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("cropmark-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("shot.png");
        std::fs::write(&first, b"one").unwrap();
        let second = unique_path(&first);
        assert_eq!(second, dir.join("shot (1).png"));
        std::fs::write(&second, b"two").unwrap();
        assert_eq!(unique_path(&first), dir.join("shot (2).png"));
        assert!(first.is_file());
        assert_eq!(std::fs::read(&first).unwrap(), b"one");

        let seq_one = suggest_with(Some(&dir), true, "roll_{seq}", &parts(), "png");
        assert_eq!(seq_one, "roll_1.png");
        std::fs::write(dir.join(&seq_one), b"a").unwrap();
        let seq_two = suggest_with(Some(&dir), true, "roll_{seq}", &parts(), "png");
        assert_eq!(seq_two, "roll_2.png");
        let plain = suggest_with(Some(&dir), true, "plain", &parts(), "png");
        assert_eq!(plain, "plain.png");
        std::fs::write(dir.join(&plain), b"p").unwrap();
        assert_eq!(
            suggest_with(Some(&dir), true, "plain", &parts(), "png"),
            "plain (1).png"
        );
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reserved_device_name_is_not_used_as_is() {
        let name = suggest_with(None, true, "CON", &parts(), "png");
        assert_eq!(name, "CON_.png");
    }
}
