//! R8 剪贴板文本贴图:纯文本排版渲染、颜色块渲染与原文元数据(ADR-9)。
//!
//! - 文本复用 `annotate::raster` 的系统字体管线,按最大宽度换行、按最大行数
//!   截断;截断只影响显示,`RenderedText::text` 始终是完整原文,复制回剪贴板
//!   用原文而不是渲染后的像素;
//! - `#RGB`/`#RRGGBB` 整段匹配时渲染为色块;
//! - 文本贴图的原文以 PNG `iTXt` 元数据随内容文件持久化,重启恢复后仍可复制
//!   回原文;图片/色块不含该元数据,复制为图像。

use ab_glyph::{Font, PxScale, ScaleFont};

use crate::capture::buffer::{encode_png, Frame};
use crate::capture::error::CaptureError;

/// 逻辑像素下的字号与行距;渲染时按目标显示器缩放因子放大保持清晰。
pub const FONT_SIZE: f32 = 20.0;
pub const LINE_HEIGHT: f32 = 1.35;
pub const PADDING: f32 = 18.0;
/// 逻辑像素下的最大文本宽度与最大行数(超出按上限截断并提示)。
pub const MAX_WIDTH: f32 = 520.0;
pub const MAX_LINES: usize = 40;
/// 参与排版的字符上限:只限制布局开销,复制仍回完整原文。
pub const MAX_LAYOUT_CHARS: usize = 8192;
/// 单行短文本的最小卡片宽度。
pub const MIN_WIDTH: f32 = 160.0;
/// 色块逻辑尺寸。
pub const COLOR_BLOCK_WIDTH: f32 = 240.0;
pub const COLOR_BLOCK_HEIGHT: f32 = 160.0;

const BACKGROUND: [u8; 4] = [255, 255, 255, 255];
const TEXT_COLOR: [u8; 4] = [31, 35, 40, 255];
/// PNG iTXt 关键词(1–79 个 Latin-1 字符)。
const TEXT_KEYWORD: &str = "cropmark-text";
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// 文本贴图渲染结果;`text` 为完整原文(可能长于显示内容)。
#[derive(Debug, Clone)]
pub struct RenderedText {
    pub frame: Frame,
    pub text: String,
    pub truncated: bool,
}

/// 整段文本匹配 `#RGB` / `#RRGGBB`(大小写不敏感)时返回 RGB;
/// 混入其它字符、长度不符或非法十六进制返回 None。
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().strip_prefix('#')?.as_bytes();
    match hex.len() {
        3 => {
            let mut out = [0u8; 3];
            for (index, byte) in hex.iter().enumerate() {
                out[index] = hex_nibble(*byte)? * 17;
            }
            Some(out)
        }
        6 => {
            let mut out = [0u8; 3];
            for index in 0..3 {
                out[index] = (hex_nibble(hex[index * 2])? << 4) | hex_nibble(hex[index * 2 + 1])?;
            }
            Some(out)
        }
        _ => None,
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|value| value as u8)
}

/// 色块贴图:整块纯色 + 一圈对比边框(浅色配深边、深色配浅边),便于贴到任意背景。
pub fn render_color_block(rgb: [u8; 3], scale: f64) -> Frame {
    let scale = scale.max(0.1) as f32;
    let width = (COLOR_BLOCK_WIDTH * scale).round().max(1.0) as u32;
    let height = (COLOR_BLOCK_HEIGHT * scale).round().max(1.0) as u32;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..width * height {
        rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
    }
    let border = if is_light(rgb) {
        [0, 0, 0, 64]
    } else {
        [255, 255, 255, 96]
    };
    let thickness = (scale.round().max(1.0) as u32)
        .min(width / 2)
        .min(height / 2)
        .max(1);
    stroke_border(&mut rgba, width, height, thickness, border);
    Frame {
        width,
        height,
        rgba,
        scale: f64::from(scale),
    }
}

fn is_light(rgb: [u8; 3]) -> bool {
    (u32::from(rgb[0]) * 299 + u32::from(rgb[1]) * 587 + u32::from(rgb[2]) * 114) / 1000 > 150
}

fn stroke_border(rgba: &mut [u8], width: u32, height: u32, thickness: u32, color: [u8; 4]) {
    for y in 0..height {
        for x in 0..width {
            if x < thickness || y < thickness || x + thickness >= width || y + thickness >= height {
                let index = ((y * width + x) * 4) as usize;
                blend_pixel(rgba, index, color, 1.0);
            }
        }
    }
}

/// 文本贴图:白底深色文字,按宽度换行、按行数上限截断。
/// 无可用系统字体时返回现有字体缺失错误,由调用方提示。
pub fn render_text(text: &str, scale: f64) -> Result<RenderedText, CaptureError> {
    let Some(font) = crate::annotate::raster::ui_font() else {
        return Err(CaptureError::api("error.capture.text_font_missing"));
    };
    let scale = scale.max(0.1) as f32;
    let size = (FONT_SIZE * scale).max(1.0);
    let px_scale = PxScale::from(size.round());
    let scaled = font.as_scaled(px_scale);
    let advance = |ch: char| scaled.h_advance(font.glyph_id(ch));
    let normalized = text.replace('\t', "    ");
    let (layout_text, char_capped) = limit_chars(&normalized, MAX_LAYOUT_CHARS);
    let wrapped = wrap_lines(&layout_text, MAX_WIDTH * scale, advance);
    let (mut lines, line_capped) = truncate_lines(wrapped, MAX_LINES);
    if char_capped && !line_capped {
        mark_ellipsis(&mut lines);
    }
    let truncated = char_capped || line_capped;

    let line_height = LINE_HEIGHT * size;
    let padding = (PADDING * scale).max(4.0);
    let mut max_line_width = 0.0f32;
    for line in &lines {
        let mut width = 0.0f32;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in line.chars() {
            let glyph_id = font.glyph_id(ch);
            if let Some(prev_id) = prev {
                width += scaled.kern(prev_id, glyph_id);
            }
            width += scaled.h_advance(glyph_id);
            prev = Some(glyph_id);
        }
        max_line_width = max_line_width.max(width);
    }
    let width = (max_line_width.min(MAX_WIDTH * scale) + padding * 2.0)
        .max(MIN_WIDTH * scale)
        .ceil() as u32;
    let height = (line_height * lines.len() as f32 + padding * 2.0).ceil() as u32;
    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(&BACKGROUND);
    }

    let baseline = scaled.ascent();
    for (index, line) in lines.iter().enumerate() {
        let mut caret_x = padding.round();
        let caret_y = (padding + index as f32 * line_height + baseline).round();
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in line.chars() {
            let glyph_id = font.glyph_id(ch);
            if let Some(prev_id) = prev {
                caret_x += scaled.kern(prev_id, glyph_id);
            }
            let glyph =
                glyph_id.with_scale_and_position(px_scale, ab_glyph::point(caret_x, caret_y));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|px, py, cover| {
                    let gx = bounds.min.x as i32 + px as i32;
                    let gy = bounds.min.y as i32 + py as i32;
                    draw_text_pixel(&mut rgba, width, height, gx, gy, TEXT_COLOR, cover);
                });
            }
            caret_x += scaled.h_advance(glyph_id);
            prev = Some(glyph_id);
        }
    }

    Ok(RenderedText {
        frame: Frame {
            width,
            height,
            rgba,
            scale: f64::from(scale),
        },
        text: text.to_string(),
        truncated,
    })
}

/// 按测量宽度换行:保留显式换行;单字符超宽时仍单独成行,避免死循环;
/// 末尾换行不产生多余空行。
fn wrap_lines(text: &str, max_width: f32, advance: impl Fn(char) -> f32) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = Vec::new();
    for raw in normalized.split('\n') {
        if raw.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut width = 0.0f32;
        for ch in raw.chars() {
            let step = advance(ch);
            if !line.is_empty() && width + step > max_width {
                lines.push(std::mem::take(&mut line));
                width = 0.0;
            }
            line.push(ch);
            width += step;
        }
        lines.push(line);
    }
    if normalized.ends_with('\n') && lines.len() > 1 {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 按最大行数截断;截断属实时在最后一行补省略号,提示显示不完整。
fn truncate_lines(mut lines: Vec<String>, max_lines: usize) -> (Vec<String>, bool) {
    let max_lines = max_lines.max(1);
    if lines.len() <= max_lines {
        return (lines, false);
    }
    lines.truncate(max_lines);
    mark_ellipsis(&mut lines);
    (lines, true)
}

fn mark_ellipsis(lines: &mut [String]) {
    if let Some(last) = lines.last_mut() {
        if !last.ends_with('…') {
            last.push('…');
        }
    }
}

/// 布局输入上限:按字符截断并报告是否截断(零宽字符也不会拖垮排版)。
fn limit_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    (text.chars().take(max_chars).collect(), true)
}

fn draw_text_pixel(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: [u8; 4],
    cover: f32,
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let index = ((y as u32 * width + x as u32) * 4) as usize;
    blend_pixel(rgba, index, color, cover);
}

fn blend_pixel(rgba: &mut [u8], index: usize, color: [u8; 4], cover: f32) {
    if index + 3 >= rgba.len() {
        return;
    }
    let alpha = (f32::from(color[3]) / 255.0) * cover.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }
    for channel in 0..3 {
        let src = f32::from(color[channel]);
        let dst = f32::from(rgba[index + channel]);
        rgba[index + channel] = (dst * (1.0 - alpha) + src * alpha).round() as u8;
    }
    // 卡片底不透明:文字覆盖后保持不透明,避免 PNG 上出现半透明污点。
    rgba[index + 3] = 255;
}

/// 用 PNG `iTXt` 元数据携带原文编码贴图;元数据写入失败时返回编码错误。
pub fn encode_png_with_text(frame: &Frame, text: &str) -> Result<Vec<u8>, CaptureError> {
    let png = encode_png(frame)?;
    let chunk = itxt_chunk(TEXT_KEYWORD, text);
    insert_before_iend(&png, &chunk).ok_or_else(|| CaptureError::api("error.capture.encode_png"))
}

/// 读取 `cropmark-text` 原文;非本应用写入的 PNG 返回 None。
pub fn extract_text(png: &[u8]) -> Option<String> {
    if png.len() < 8 || png[..8] != PNG_SIGNATURE {
        return None;
    }
    let mut cursor = 8;
    while cursor + 12 <= png.len() {
        let length = u32::from_be_bytes(png.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
        let end = cursor.checked_add(12 + length)?;
        if end > png.len() {
            return None;
        }
        let kind = png.get(cursor + 4..cursor + 8)?;
        if kind == b"iTXt" {
            let payload = png.get(cursor + 8..cursor + 8 + length)?;
            if let Some((keyword, text)) = parse_itxt(payload) {
                if keyword == TEXT_KEYWORD {
                    return Some(text);
                }
            }
        }
        if kind == b"IEND" {
            return None;
        }
        cursor = end;
    }
    None
}

/// 构造 PNG iTXt 块(长度 + 类型 + 数据 + CRC);原文按 UTF-8 原样存放。
fn itxt_chunk(keyword: &str, text: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(keyword.len() + text.len() + 5);
    payload.extend_from_slice(keyword.as_bytes());
    payload.push(0);
    payload.push(0); // compression flag:未压缩
    payload.push(0); // compression method
    payload.push(0); // language tag
    payload.push(0); // translated keyword
    payload.extend_from_slice(text.as_bytes());
    let mut chunk = Vec::with_capacity(payload.len() + 12);
    chunk.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    chunk.extend_from_slice(b"iTXt");
    chunk.extend_from_slice(&payload);
    let crc = crc32(&chunk[4..]);
    chunk.extend_from_slice(&crc.to_be_bytes());
    chunk
}

fn parse_itxt(payload: &[u8]) -> Option<(String, String)> {
    let (keyword, rest) = split_nul(payload)?;
    if rest.len() < 2 {
        return None;
    }
    if rest[0] != 0 {
        return None; // 压缩 iTXt 不支持
    }
    let rest = &rest[2..];
    let (_language, rest) = split_nul(rest)?;
    let (_translated, text) = split_nul(rest)?;
    Some((
        String::from_utf8_lossy(keyword).into_owned(),
        String::from_utf8_lossy(text).into_owned(),
    ))
}

fn split_nul(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let index = bytes.iter().position(|byte| *byte == 0)?;
    Some((&bytes[..index], &bytes[index + 1..]))
}

/// 把新块放到 IEND 之前,保持 PNG 结构合法。
fn insert_before_iend(png: &[u8], chunk: &[u8]) -> Option<Vec<u8>> {
    if png.len() < 8 || png[..8] != PNG_SIGNATURE {
        return None;
    }
    let mut cursor = 8;
    while cursor + 12 <= png.len() {
        let length = u32::from_be_bytes(png.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
        let end = cursor.checked_add(12 + length)?;
        if end > png.len() {
            return None;
        }
        if png.get(cursor + 4..cursor + 8)? == b"IEND" {
            let mut out = Vec::with_capacity(png.len() + chunk.len());
            out.extend_from_slice(&png[..cursor]);
            out.extend_from_slice(chunk);
            out.extend_from_slice(&png[cursor..]);
            return Some(out);
        }
        cursor = end;
    }
    None
}

/// PNG 块 CRC32(IEEE 多项式,逐位实现,避免引入新依赖)。
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advance_by(chars: usize) -> impl Fn(char) -> f32 {
        move |_| chars as f32
    }

    #[test]
    fn color_matches_full_hex_text_only() {
        assert_eq!(parse_color("#f00"), Some([255, 0, 0]));
        assert_eq!(parse_color(" #0F0 "), Some([0, 255, 0]));
        assert_eq!(parse_color("#00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_color("#2563EB"), Some([0x25, 0x63, 0xEB]));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#1234567"), None);
        assert_eq!(parse_color("color: #fff"), None);
        assert_eq!(parse_color("fff"), None);
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn wrap_breaks_long_lines_and_keeps_explicit_breaks() {
        // 每字符宽 10:宽 35 时每行 3 个字符。
        let lines = wrap_lines("abcdef\nxy", 35.0, advance_by(10));
        assert_eq!(lines, vec!["abc", "def", "xy"]);
        // 单个超宽字符仍单独成行,不与下一行死循环。
        let lines = wrap_lines("宽", 5.0, advance_by(10));
        assert_eq!(lines, vec!["宽"]);
        // 末尾换行不产生多余空行;显式空行保留。
        let lines = wrap_lines("a\n\nb\n", 35.0, advance_by(10));
        assert_eq!(lines, vec!["a", "", "b"]);
    }

    #[test]
    fn truncate_caps_lines_and_marks_ellipsis() {
        let lines: Vec<String> = (0..5).map(|index| format!("line{index}")).collect();
        let (kept, truncated) = truncate_lines(lines.clone(), 3);
        assert!(truncated);
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[2], "line2…");
        let (kept, truncated) = truncate_lines(lines, 5);
        assert!(!truncated);
        assert_eq!(kept.len(), 5);
    }

    #[test]
    fn limit_chars_caps_layout_input_but_keeps_short_text_intact() {
        let (text, capped) = limit_chars("abcdef", 6);
        assert_eq!(text, "abcdef");
        assert!(!capped);
        let (text, capped) = limit_chars("abcdef", 3);
        assert_eq!(text, "abc");
        assert!(capped);
        // 多字节字符按字符计数,不按字节截断。
        let (text, capped) = limit_chars("中文文本", 2);
        assert_eq!(text, "中文");
        assert!(capped);
    }

    #[test]
    fn render_text_reports_full_original_and_truncation() {
        if crate::annotate::raster::ui_font().is_none() {
            return; // 无字体环境(部分 CI 容器)仅验证不 panic 的分支。
        }
        let long = "行".repeat(MAX_LINES * 60);
        let rendered = render_text(&long, 1.0).expect("font available");
        assert!(rendered.truncated, "long text must be truncated");
        assert_eq!(rendered.text, long, "copy source keeps the full original");
        assert!(rendered.frame.width > 0 && rendered.frame.height > 0);
        assert_eq!(rendered.frame.scale, 1.0);
    }

    #[test]
    fn render_text_without_font_reports_missing_font() {
        if crate::annotate::raster::ui_font().is_some() {
            return; // 有字体环境走正常渲染分支。
        }
        let error = render_text("hello", 1.0).expect_err("no font");
        assert!(error.message.contains("字体"), "{}", error.message);
    }

    #[test]
    fn color_block_center_keeps_color_and_border_is_visible() {
        let frame = render_color_block([37, 99, 235], 1.0);
        assert_eq!((frame.width, frame.height), (240, 160));
        assert_eq!(frame.scale, 1.0);
        let center = ((80 * frame.width + 120) * 4) as usize;
        assert_eq!(&frame.rgba[center..center + 4], &[37, 99, 235, 255]);
        // 深色块配浅色边框:白色 96/255 叠加在底色上的预览值。
        assert_eq!(&frame.rgba[0..4], &[119, 158, 243, 255]);
        // 缩放后保持等比放大,颜色不变。
        let scaled = render_color_block([240, 240, 240], 2.0);
        assert_eq!((scaled.width, scaled.height), (480, 320));
        let center = ((160 * scaled.width + 240) * 4) as usize;
        assert_eq!(&scaled.rgba[center..center + 4], &[240, 240, 240, 255]);
        // 浅色块配深色边框。
        assert_eq!(&scaled.rgba[0..4], &[180, 180, 180, 255]);
    }

    #[test]
    fn text_metadata_round_trips_through_png() {
        let frame = Frame {
            width: 2,
            height: 1,
            rgba: vec![255, 255, 255, 255, 0, 0, 0, 255],
            scale: 1.0,
        };
        let text = "第一行\nsecond line #fff";
        let png = encode_png_with_text(&frame, text).unwrap();
        assert_eq!(extract_text(&png).as_deref(), Some(text));
        // 元数据不影响像素解码。
        let decoded = crate::capture::buffer::decode_png(&png).unwrap();
        assert_eq!(decoded.rgba, frame.rgba);
        // 普通 PNG 不含元数据。
        let plain = crate::capture::buffer::encode_png(&frame).unwrap();
        assert_eq!(extract_text(&plain), None);
        assert_eq!(extract_text(b"not a png"), None);
        assert_eq!(extract_text(&png[..20]), None);
    }

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }
}
