//! 选区合成器用的单行文字绘制。
//!
//! 复用 `annotate::raster` 的 ab_glyph 字体管线(同一 `ui_font`),
//! 但允许任意颜色并支持宽度测量,供徽标/放大镜读数/操作条/菜单使用。
//! 不修改 `annotate` 的行为;系统无可用字体时返回测量失败并跳过绘制。

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};

const LINE_HEIGHT: f32 = 1.25;

pub(crate) fn ui_font() -> Option<&'static FontVec> {
    crate::annotate::raster::ui_font()
}

/// 测量单行文字宽度(物理像素);无字体时返回 None。
pub fn measure_width(text: &str, size: f32) -> Option<f32> {
    let font = ui_font()?;
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let mut width = 0.0f32;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        let glyph_id = font.glyph_id(ch);
        if let Some(prev_id) = prev {
            width += scaled.kern(prev_id, glyph_id);
        }
        width += scaled.h_advance(glyph_id);
        prev = Some(glyph_id);
    }
    Some(width)
}

pub fn line_height(size: f32) -> f32 {
    size * LINE_HEIGHT
}

/// 在 (x, y)(首字符左上角)绘制单行文字;无字体时返回 false 且不落笔。
#[allow(clippy::too_many_arguments)]
pub fn draw_text(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    color: [u8; 4],
) -> bool {
    let Some(font) = ui_font() else {
        return false;
    };
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let mut caret_x = x;
    let caret_y = y + scaled.ascent();
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        if ch == '\r' || ch == '\n' {
            continue;
        }
        let glyph_id = font.glyph_id(ch);
        if let Some(prev_id) = prev {
            caret_x += scaled.kern(prev_id, glyph_id);
        }
        let glyph = glyph_id.with_scale_and_position(scale, ab_glyph::point(caret_x, caret_y));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|px, py, cover| {
                let gx = bounds.min.x as i32 + px as i32;
                let gy = bounds.min.y as i32 + py as i32;
                blend_pixel(rgba, width, height, gx, gy, color, cover);
            });
        }
        caret_x += scaled.h_advance(glyph_id);
        prev = Some(glyph_id);
    }
    true
}

/// 加粗绘制:正常描画后按字号比例水平偏移二次描画(faux bold,
/// 字体管线只有一个字重时的通用做法)。无字体时返回 false 且不落笔。
#[allow(clippy::too_many_arguments)]
pub fn draw_text_bold(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    color: [u8; 4],
) -> bool {
    let first = draw_text(rgba, width, height, x, y, text, size, color);
    let offset = (size * 0.045).max(0.6);
    let second = draw_text(rgba, width, height, x + offset, y, text, size, color);
    first || second
}

fn blend_pixel(
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
    let i = ((y as u32 * width + x as u32) * 4) as usize;
    if i + 3 >= rgba.len() {
        return;
    }
    let a = (f32::from(color[3]) / 255.0) * cover.clamp(0.0, 1.0);
    if a <= 0.0 {
        return;
    }
    for c in 0..3 {
        let src = f32::from(color[c]);
        let dst = f32::from(rgba[i + c]);
        rgba[i + c] = (dst * (1.0 - a) + src * a).round() as u8;
    }
    rgba[i + 3] = 255;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_and_measures_when_a_font_is_available() {
        if ui_font().is_none() {
            return; // 无字体环境(部分 CI 容器)仅验证不 panic。
        }
        assert!(measure_width("宽度123", 13.0).unwrap() > 0.0);
        let mut rgba = vec![0u8; 64 * 24 * 4];
        assert!(draw_text(
            &mut rgba,
            64,
            24,
            2.0,
            2.0,
            "A1",
            14.0,
            [255, 255, 255, 255],
        ));
        assert!(rgba
            .chunks_exact(4)
            .any(|px| px[0] > 0 || px[1] > 0 || px[2] > 0));
    }

    #[test]
    fn line_height_scales_with_size() {
        assert!(line_height(11.0) < line_height(13.0));
    }
}
