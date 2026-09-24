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
    let scale = PxScale::from(size.round());
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

/// 使单行文字的 em 盒垂直中心落在 `center_y` 时的绘制 y(em 顶,即 baseline - ascent)。
/// `draw_text` 把 y 当成 em 顶。descent 为负,em 高是 ascent - descent;
/// 用 ascent + descent 会把中文墨迹压到行框下半。无字体时回退行高居中。
pub fn y_for_center(center_y: f32, size: f32) -> f32 {
    let Some(font) = ui_font() else {
        return center_y - line_height(size) / 2.0;
    };
    let scaled = font.as_scaled(PxScale::from(size.round()));
    center_y - (scaled.ascent() - scaled.descent()) / 2.0
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
    let scale = PxScale::from(size.round());
    let scaled = font.as_scaled(scale);
    // 基线钉在整数像素上。落在半像素上时,汉字的横画会铺成两行灰边。
    let mut caret_x = x;
    let caret_y = (y + scaled.ascent()).round();
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in text.chars() {
        if ch == '\r' || ch == '\n' {
            continue;
        }
        let glyph_id = font.glyph_id(ch);
        if let Some(prev_id) = prev {
            caret_x += scaled.kern(prev_id, glyph_id);
        }
        let glyph =
            glyph_id.with_scale_and_position(scale, ab_glyph::point(caret_x.round(), caret_y));
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

/// 菜单文字只画一遍。再偏 1 像素描第二遍会让汉字横画变成双影。
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
    draw_text(rgba, width, height, x.round(), y, text, size, color)
}

/// 无 hinting 的 16px 汉字会把 1px 横画铺成两行浅灰。
/// 把中间覆盖度拉向实色,只留一圈细边,避免再叠一遍造成双影。
fn ink_coverage(cover: f32) -> f32 {
    let c = cover.clamp(0.0, 1.0);
    if c < 0.04 {
        0.0
    } else {
        c.powf(0.55)
    }
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
    let a = (f32::from(color[3]) / 255.0) * ink_coverage(cover);
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
    fn ink_coverage_darkens_midtones_without_clipping_solid() {
        assert_eq!(ink_coverage(0.0), 0.0);
        assert_eq!(ink_coverage(1.0), 1.0);
        assert_eq!(ink_coverage(0.02), 0.0);
        let mid = ink_coverage(0.35);
        assert!(mid > 0.35 && mid < 1.0, "mid coverage {mid}");
    }

    #[test]
    fn line_height_scales_with_size() {
        assert!(line_height(11.0) < line_height(13.0));
    }
}
