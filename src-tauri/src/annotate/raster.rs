use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};

use super::mosaic::pixelate;
use super::{exportable, Annotation};
use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;

pub const STROKE: [u8; 4] = [225, 29, 72, 255];

pub fn rasterize(frame: &Frame, annotations: &[Annotation]) -> Result<Frame, CaptureError> {
    if frame.rgba.is_empty() || frame.width == 0 || frame.height == 0 {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    let expected = frame.width as usize * frame.height as usize * 4;
    if frame.rgba.len() != expected {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    let mut out = frame.clone();
    for op in exportable(annotations) {
        apply(&mut out, &op)?;
    }
    Ok(out)
}

fn apply(frame: &mut Frame, op: &Annotation) -> Result<(), CaptureError> {
    match op {
        Annotation::Mosaic {
            x,
            y,
            width,
            height,
            block,
        } => {
            let (x, y, w, h) = clamped_rect(*x, *y, *width, *height, frame.width, frame.height);
            if w > 0 && h > 0 {
                pixelate(
                    &mut frame.rgba,
                    frame.width,
                    frame.height,
                    x,
                    y,
                    w,
                    h,
                    *block,
                );
            }
        }
        Annotation::Rect {
            x,
            y,
            width,
            height,
        } => {
            let (x0, y0, w, h) = normalized(*x, *y, *width, *height);
            draw_rect(
                &mut frame.rgba,
                frame.width,
                frame.height,
                x0,
                y0,
                w,
                h,
                stroke_width(frame.scale),
                STROKE,
            );
        }
        Annotation::Arrow { from, to } => {
            draw_arrow(
                &mut frame.rgba,
                frame.width,
                frame.height,
                from.x as f32,
                from.y as f32,
                to.x as f32,
                to.y as f32,
                stroke_width(frame.scale),
                STROKE,
            );
        }
        Annotation::Text { x, y, text, size } => {
            draw_text(
                &mut frame.rgba,
                frame.width,
                frame.height,
                *x as f32,
                *y as f32,
                text,
                (*size as f32).max(10.0),
            )?;
        }
    }
    Ok(())
}

fn stroke_width(scale: f64) -> f32 {
    (3.0 * scale.max(1.0) as f32).clamp(2.0, 8.0)
}

fn normalized(x: f64, y: f64, width: f64, height: f64) -> (f32, f32, f32, f32) {
    let (x, width) = if width < 0.0 {
        (x + width, -width)
    } else {
        (x, width)
    };
    let (y, height) = if height < 0.0 {
        (y + height, -height)
    } else {
        (y, height)
    };
    (x as f32, y as f32, width as f32, height as f32)
}

fn clamped_rect(x: f64, y: f64, width: f64, height: f64, iw: u32, ih: u32) -> (u32, u32, u32, u32) {
    let (x, y, width, height) = {
        let (x, w) = if width < 0.0 { (x + width, -width) } else { (x, width) };
        let (y, h) = if height < 0.0 { (y + height, -height) } else { (y, height) };
        (x, y, w, h)
    };
    let x0 = x.max(0.0).min(iw as f64) as u32;
    let y0 = y.max(0.0).min(ih as f64) as u32;
    let x1 = (x + width).max(0.0).min(iw as f64) as u32;
    let y1 = (y + height).max(0.0).min(ih as f64) as u32;
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

fn draw_rect(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    thickness: f32,
    color: [u8; 4],
) {
    if w < 1.0 || h < 1.0 {
        return;
    }
    draw_line(rgba, width, height, x, y, x + w, y, thickness, color);
    draw_line(rgba, width, height, x + w, y, x + w, y + h, thickness, color);
    draw_line(rgba, width, height, x + w, y + h, x, y + h, thickness, color);
    draw_line(rgba, width, height, x, y + h, x, y, thickness, color);
}

fn draw_arrow(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: f32,
    color: [u8; 4],
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = dx.hypot(dy);
    if len < 2.0 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    let head = (14.0 * (thickness / 3.0)).clamp(10.0, 28.0);
    let back_x = x1 - ux * head;
    let back_y = y1 - uy * head;
    draw_line(
        rgba,
        width,
        height,
        x0,
        y0,
        back_x + ux * (thickness * 0.5),
        back_y + uy * (thickness * 0.5),
        thickness,
        color,
    );
    let px = -uy;
    let py = ux;
    let spread = head * 0.42;
    fill_triangle(
        rgba,
        width,
        height,
        (x1, y1),
        (back_x + px * spread, back_y + py * spread),
        (back_x - px * spread, back_y - py * spread),
        color,
    );
}

fn draw_line(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    thickness: f32,
    color: [u8; 4],
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = dx.hypot(dy).max(1.0);
    let radius = (thickness * 0.5).max(0.8);
    let steps = (len * 2.0).ceil() as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        stamp_disk(
            rgba,
            width,
            height,
            x0 + dx * t,
            y0 + dy * t,
            radius,
            color,
        );
    }
}

fn fill_triangle(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    a: (f32, f32),
    b: (f32, f32),
    c: (f32, f32),
    color: [u8; 4],
) {
    let min_x = a.0.min(b.0).min(c.0).floor().max(0.0) as i32;
    let max_x = a.0.max(b.0).max(c.0).ceil().min(width.saturating_sub(1) as f32) as i32;
    let min_y = a.1.min(b.1).min(c.1).floor().max(0.0) as i32;
    let max_y = a.1.max(b.1).max(c.1).ceil().min(height.saturating_sub(1) as f32) as i32;
    let area = (b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1);
    if area.abs() < 0.5 {
        return;
    }
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let w0 = (b.0 - px) * (c.1 - py) - (c.0 - px) * (b.1 - py);
            let w1 = (c.0 - px) * (a.1 - py) - (a.0 - px) * (c.1 - py);
            let w2 = (a.0 - px) * (b.1 - py) - (b.0 - px) * (a.1 - py);
            if (w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0) || (w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0) {
                blend_pixel(rgba, width, height, x, y, color, 1.0);
            }
        }
    }
}

fn stamp_disk(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    cx: f32,
    cy: f32,
    radius: f32,
    color: [u8; 4],
) {
    let min_x = (cx - radius - 1.0).floor().max(0.0) as i32;
    let max_x = (cx + radius + 1.0)
        .ceil()
        .min(width.saturating_sub(1) as f32) as i32;
    let min_y = (cy - radius - 1.0).floor().max(0.0) as i32;
    let max_y = (cy + radius + 1.0)
        .ceil()
        .min(height.saturating_sub(1) as f32) as i32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let cover = (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
            if cover > 0.0 {
                blend_pixel(rgba, width, height, x, y, color, cover);
            }
        }
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

fn draw_text(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
) -> Result<(), CaptureError> {
    let Some(font) = ui_font() else {
        return Err(CaptureError::api("无法绘制文字：系统未找到可用字体。"));
    };
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let mut caret_x = x;
    let mut caret_y = y + scaled.ascent();
    let mut prev: Option<GlyphId> = None;
    for ch in text.chars() {
        if ch == '\r' {
            continue;
        }
        if ch == '\n' {
            caret_x = x;
            caret_y += scaled.height();
            prev = None;
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
                blend_pixel(rgba, width, height, gx, gy, STROKE, cover);
            });
        }
        caret_x += scaled.h_advance(glyph_id);
        prev = Some(glyph_id);
    }
    Ok(())
}

pub(crate) fn ui_font() -> Option<&'static FontVec> {
    static FONT: OnceLock<Option<FontVec>> = OnceLock::new();
    FONT.get_or_init(load_ui_font).as_ref()
}

fn load_ui_font() -> Option<FontVec> {
    for path in font_candidates() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        for index in 0..6 {
            let Ok(font) = FontVec::try_from_vec_and_index(bytes.clone(), index) else {
                continue;
            };
            if font.glyph_id('中').0 != 0 || font.glyph_id('A').0 != 0 {
                return Some(font);
            }
        }
    }
    None
}

fn font_candidates() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Ok(windir) = std::env::var("WINDIR") {
        let fonts = std::path::PathBuf::from(windir).join("Fonts");
        paths.extend([
            fonts.join("msyh.ttc"),
            fonts.join("msyhbd.ttc"),
            fonts.join("msyhl.ttc"),
            fonts.join("simhei.ttf"),
            fonts.join("simsun.ttc"),
            fonts.join("arial.ttf"),
            fonts.join("segoeui.ttf"),
        ]);
    }
    paths.extend([
        std::path::PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
        std::path::PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
        std::path::PathBuf::from("/System/Library/Fonts/STHeiti Light.ttc"),
        std::path::PathBuf::from("/Library/Fonts/Arial Unicode.ttf"),
        std::path::PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
        std::path::PathBuf::from("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc"),
        std::path::PathBuf::from("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
        std::path::PathBuf::from("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc"),
        std::path::PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
    ]);
    paths
}
