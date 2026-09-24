use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};

use super::blur;
use super::mosaic::pixelate;
use super::{exportable, Annotation, Point, HIGHLIGHTER_ALPHA};
use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;

pub const STROKE: [u8; 4] = [225, 29, 72, 255];

const LINE_HEIGHT: f32 = 1.25;

pub fn rasterize(frame: &Frame, annotations: &[Annotation]) -> Result<Frame, CaptureError> {
    if frame.rgba.is_empty() || frame.width == 0 || frame.height == 0 {
        return Err(CaptureError::invalid_buffer("error.capture.buffer_empty"));
    }
    let expected = frame.width as usize * frame.height as usize * 4;
    if frame.rgba.len() != expected {
        return Err(CaptureError::invalid_buffer("error.capture.buffer_empty"));
    }
    let mut out = frame.clone();
    apply_annotations(&mut out.rgba, out.width, out.height, out.scale, annotations)?;
    Ok(out)
}

/// 宽松合成:逐图元应用,单个图元失败(如系统字体缺失导致文字无法绘制)
/// 时跳过该图元而不是让整次完成失败。选区即时标注的完成路径使用它,
/// 标注失败不得阻断复制/保存/贴图/取字(ADR-14 失败边界)。
pub fn rasterize_lenient(frame: &Frame, annotations: &[Annotation]) -> Frame {
    let mut out = frame.clone();
    for op in exportable(annotations) {
        let _ = apply_one(&mut out.rgba, out.width, out.height, out.scale, &op);
    }
    out
}

/// 把图元列表应用到任意 RGBA 缓冲,过滤退化图元(`exportable`)。
/// 与 `rasterize` 共用同一几何/颜色/字体管线:选区即时标注的合成呈现与
/// 最终输出(`rasterize`)走同一实现,保证所见即所得(ADR-14)。
pub(crate) fn apply_annotations(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    scale: f64,
    annotations: &[Annotation],
) -> Result<(), CaptureError> {
    for op in exportable(annotations) {
        apply_one(rgba, width, height, scale, &op)?;
    }
    Ok(())
}

/// 单个图元,不做 `exportable` 过滤:合成器的拖动草稿需要即时反馈,
/// 即便当前尺寸尚未达到导出下限也要可见。
pub(crate) fn apply_annotation(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    scale: f64,
    op: &Annotation,
) -> Result<(), CaptureError> {
    apply_one(rgba, width, height, scale, op)
}

fn apply_one(
    rgba: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: f64,
    op: &Annotation,
) -> Result<(), CaptureError> {
    match op {
        Annotation::Mosaic {
            x,
            y,
            width,
            height,
            block,
        } => {
            let (x, y, w, h) = clamped_rect(*x, *y, *width, *height, buf_w, buf_h);
            if w > 0 && h > 0 {
                pixelate(rgba, buf_w, buf_h, x, y, w, h, *block);
            }
        }
        Annotation::Rect {
            x,
            y,
            width,
            height,
            color,
            stroke_width,
        } => {
            let (x0, y0, w, h) = normalized(*x, *y, *width, *height);
            draw_rect(
                rgba,
                buf_w,
                buf_h,
                x0,
                y0,
                w,
                h,
                resolve_stroke(scale, *stroke_width),
                resolve_color(color),
            );
        }
        Annotation::Arrow {
            from,
            to,
            color,
            stroke_width,
        } => {
            draw_arrow(
                rgba,
                buf_w,
                buf_h,
                from.x as f32,
                from.y as f32,
                to.x as f32,
                to.y as f32,
                resolve_stroke(scale, *stroke_width),
                resolve_color(color),
            );
        }
        Annotation::Text {
            x,
            y,
            text,
            size,
            color,
        } => {
            draw_text(
                rgba,
                buf_w,
                buf_h,
                *x as f32,
                *y as f32,
                text,
                (*size as f32).max(10.0),
                resolve_color(color),
            )?;
        }
        Annotation::Ellipse {
            x,
            y,
            width,
            height,
            color,
            stroke_width,
        } => {
            let (x0, y0, w, h) = normalized(*x, *y, *width, *height);
            draw_ellipse(
                rgba,
                buf_w,
                buf_h,
                x0,
                y0,
                w,
                h,
                resolve_stroke(scale, *stroke_width),
                resolve_color(color),
            );
        }
        Annotation::Line {
            from,
            to,
            color,
            stroke_width,
        } => {
            draw_line(
                rgba,
                buf_w,
                buf_h,
                from.x as f32,
                from.y as f32,
                to.x as f32,
                to.y as f32,
                resolve_stroke(scale, *stroke_width),
                resolve_color(color),
            );
        }
        Annotation::Number {
            x,
            y,
            value,
            size,
            color,
        } => {
            draw_text(
                rgba,
                buf_w,
                buf_h,
                *x as f32,
                *y as f32,
                &value.to_string(),
                (*size as f32).max(10.0),
                resolve_color(color),
            )?;
        }
        Annotation::Highlighter {
            points,
            color,
            stroke_width,
        } => {
            draw_polyline_translucent(
                rgba,
                buf_w,
                buf_h,
                points,
                resolve_stroke(scale, *stroke_width),
                resolve_color(color),
                HIGHLIGHTER_ALPHA,
            );
        }
        Annotation::Pen {
            points,
            color,
            stroke_width,
        } => {
            draw_polyline(
                rgba,
                buf_w,
                buf_h,
                points,
                resolve_stroke(scale, *stroke_width),
                resolve_color(color),
            );
        }
        Annotation::Blur {
            x,
            y,
            width,
            height,
            sigma,
        } => {
            let (x, y, w, h) = clamped_rect(*x, *y, *width, *height, buf_w, buf_h);
            if w >= 2 && h >= 2 {
                blur::gaussian(rgba, buf_w, buf_h, x, y, w, h, *sigma);
            }
        }
    }
    Ok(())
}

/// 半透明折线（荧光笔）：先在区域覆盖掩码上取每像素最大覆盖，
/// 再一次性按固定 alpha 合成，避免逐段盖章叠加后 Alpha 累积变不透明。
#[allow(clippy::too_many_arguments)]
fn draw_polyline_translucent(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    points: &[Point],
    thickness: f32,
    color: [u8; 4],
    alpha: f32,
) {
    if points.len() < 2 || alpha <= 0.0 {
        return;
    }
    let radius = (thickness * 0.5).max(0.8);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for point in points {
        let (x, y) = (point.x as f32, point.y as f32);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let bx0 = (min_x - radius - 1.0).floor().max(0.0) as u32;
    let by0 = (min_y - radius - 1.0).floor().max(0.0) as u32;
    let bx1 = (max_x + radius + 1.0).ceil().min(width as f32).max(0.0) as u32;
    let by1 = (max_y + radius + 1.0).ceil().min(height as f32).max(0.0) as u32;
    if bx1 <= bx0 || by1 <= by0 {
        return;
    }
    let mw = bx1 - bx0;
    let mh = by1 - by0;
    let mut mask = vec![0u8; mw as usize * mh as usize];
    for pair in points.windows(2) {
        stamp_line_mask(
            &mut mask,
            mw,
            mh,
            pair[0].x as f32 - bx0 as f32,
            pair[0].y as f32 - by0 as f32,
            pair[1].x as f32 - bx0 as f32,
            pair[1].y as f32 - by0 as f32,
            radius,
        );
    }
    for my in 0..mh {
        for mx in 0..mw {
            let cover = f32::from(mask[(my * mw + mx) as usize]) / 255.0;
            if cover <= 0.0 {
                continue;
            }
            blend_pixel(
                rgba,
                width,
                height,
                (bx0 + mx) as i32,
                (by0 + my) as i32,
                color,
                cover * alpha,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stamp_line_mask(
    mask: &mut [u8],
    mw: u32,
    mh: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = dx.hypot(dy).max(1.0);
    let steps = (len * 2.0).ceil() as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        stamp_disk_mask(mask, mw, mh, x0 + dx * t, y0 + dy * t, radius);
    }
}

fn stamp_disk_mask(mask: &mut [u8], mw: u32, mh: u32, cx: f32, cy: f32, radius: f32) {
    let min_x = (cx - radius - 1.0).floor().max(0.0) as i32;
    let max_x = (cx + radius + 1.0).ceil().min(mw.saturating_sub(1) as f32) as i32;
    let min_y = (cy - radius - 1.0).floor().max(0.0) as i32;
    let max_y = (cy + radius + 1.0).ceil().min(mh.saturating_sub(1) as f32) as i32;
    for y in min_y..=max_y {
        for x in min_x..=max_x {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let cover = (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
            if cover > 0.0 {
                let value = (cover * 255.0).round() as u8;
                let slot = &mut mask[(y as u32 * mw + x as u32) as usize];
                if value > *slot {
                    *slot = value;
                }
            }
        }
    }
}

fn stroke_width(scale: f64) -> f32 {
    (3.0 * scale.max(1.0) as f32).clamp(2.0, 8.0)
}

/// 线宽档位优先取标注自带值（乘 scale 后 clamp 2..8），None/非法值沿用既有推导。
fn resolve_stroke(scale: f64, width: Option<f64>) -> f32 {
    match width {
        Some(width) if width.is_finite() && width > 0.0 => {
            (width as f32 * scale.max(1.0) as f32).clamp(2.0, 8.0)
        }
        _ => stroke_width(scale),
    }
}

/// 颜色解析非法串回退默认玫红。
fn resolve_color(input: &str) -> [u8; 4] {
    parse_hex_color(input).unwrap_or(STROKE)
}

/// 解析 `#rgb` / `#rrggbb` / `#rrggbbaa`，其余形式返回 None。
pub(crate) fn parse_hex_color(input: &str) -> Option<[u8; 4]> {
    let hex = input.strip_prefix('#')?;
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
    let expand = |i: usize| {
        let digit = hex.get(i..i + 1)?;
        u8::from_str_radix(&format!("{digit}{digit}"), 16).ok()
    };
    match hex.len() {
        3 => Some([expand(0)?, expand(1)?, expand(2)?, 255]),
        6 => Some([channel(0..2)?, channel(2..4)?, channel(4..6)?, 255]),
        8 => Some([
            channel(0..2)?,
            channel(2..4)?,
            channel(4..6)?,
            channel(6..8)?,
        ]),
        _ => None,
    }
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
        let (x, w) = if width < 0.0 {
            (x + width, -width)
        } else {
            (x, width)
        };
        let (y, h) = if height < 0.0 {
            (y + height, -height)
        } else {
            (y, height)
        };
        (x, y, w, h)
    };
    let x0 = x.max(0.0).min(iw as f64) as u32;
    let y0 = y.max(0.0).min(ih as f64) as u32;
    let x1 = (x + width).max(0.0).min(iw as f64) as u32;
    let y1 = (y + height).max(0.0).min(ih as f64) as u32;
    (x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0))
}

#[allow(clippy::too_many_arguments)]
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
    draw_line(
        rgba,
        width,
        height,
        x + w,
        y,
        x + w,
        y + h,
        thickness,
        color,
    );
    draw_line(
        rgba,
        width,
        height,
        x + w,
        y + h,
        x,
        y + h,
        thickness,
        color,
    );
    draw_line(rgba, width, height, x, y + h, x, y, thickness, color);
}

/// 椭圆描边：按半径和周长估算采样步数，沿参数曲线盖章圆盘，
/// 保证任意宽高比下的线宽与箭头/矩形一致。
#[allow(clippy::too_many_arguments)]
fn draw_ellipse(
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
    let cx = x + w * 0.5;
    let cy = y + h * 0.5;
    let rx = w * 0.5;
    let ry = h * 0.5;
    let radius = (thickness * 0.5).max(0.8);
    let steps = (((rx + ry) * std::f32::consts::PI * 2.0).ceil() as i32).clamp(24, 8192);
    for i in 0..=steps {
        let t = i as f32 / steps as f32 * std::f32::consts::TAU;
        stamp_disk(
            rgba,
            width,
            height,
            cx + rx * t.cos(),
            cy + ry * t.sin(),
            radius,
            color,
        );
    }
}

/// 折线描边：逐段调用 draw_line，圆盘端点重叠使拐角连续。
fn draw_polyline(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    points: &[Point],
    thickness: f32,
    color: [u8; 4],
) {
    for pair in points.windows(2) {
        draw_line(
            rgba,
            width,
            height,
            pair[0].x as f32,
            pair[0].y as f32,
            pair[1].x as f32,
            pair[1].y as f32,
            thickness,
            color,
        );
    }
}

#[allow(clippy::too_many_arguments)]
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

#[allow(clippy::too_many_arguments)]
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
        stamp_disk(rgba, width, height, x0 + dx * t, y0 + dy * t, radius, color);
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
    let max_x =
        a.0.max(b.0)
            .max(c.0)
            .ceil()
            .min(width.saturating_sub(1) as f32) as i32;
    let min_y = a.1.min(b.1).min(c.1).floor().max(0.0) as i32;
    let max_y =
        a.1.max(b.1)
            .max(c.1)
            .ceil()
            .min(height.saturating_sub(1) as f32) as i32;
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

#[allow(clippy::too_many_arguments)]
fn draw_text(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    color: [u8; 4],
) -> Result<(), CaptureError> {
    let Some(font) = ui_font() else {
        return Err(CaptureError::api("error.capture.text_font_missing"));
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
            caret_y += size * LINE_HEIGHT;
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
                blend_pixel(rgba, width, height, gx, gy, color, cover);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::Frame;

    fn solid(width: u32, height: u32, color: [u8; 4], scale: f64) -> Frame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            rgba.extend_from_slice(&color);
        }
        Frame {
            width,
            height,
            rgba,
            scale,
        }
    }

    fn styled_rect(color: &str) -> Annotation {
        Annotation::Rect {
            x: 4.0,
            y: 4.0,
            width: 20.0,
            height: 12.0,
            color: color.into(),
            stroke_width: None,
        }
    }

    fn count_matching(frame: &Frame, pred: impl Fn(&[u8]) -> bool) -> usize {
        frame.rgba.chunks_exact(4).filter(|px| pred(px)).count()
    }

    #[test]
    fn parse_hex_color_accepts_known_shapes_and_rejects_others() {
        assert_eq!(parse_hex_color("#e11d48"), Some([225, 29, 72, 255]));
        assert_eq!(parse_hex_color("#FFF"), Some([255, 255, 255, 255]));
        assert_eq!(parse_hex_color("#2563eb80"), Some([0x25, 0x63, 0xeb, 0x80]));
        assert_eq!(parse_hex_color("#e11d4"), None);
        assert_eq!(parse_hex_color("e11d48"), None);
        assert_eq!(parse_hex_color("not-a-color"), None);
        assert_eq!(parse_hex_color("#zzzzzz"), None);
    }

    #[test]
    fn custom_color_reaches_pixels_for_rect_and_arrow() {
        let frame = solid(32, 32, [0, 0, 0, 255], 1.0);
        let rect = styled_rect("#2563eb");
        let rendered = rasterize(&frame, &[rect]).unwrap();
        assert!(
            count_matching(&rendered, |px| px[2] > 180 && px[0] < 90) > 0,
            "rect stroke should be blue"
        );
        assert_eq!(count_matching(&rendered, |px| px[0] > 180 && px[1] < 90), 0);

        let arrow = Annotation::Arrow {
            from: crate::annotate::Point { x: 4.0, y: 28.0 },
            to: crate::annotate::Point { x: 28.0, y: 4.0 },
            color: "#10b981".into(),
            stroke_width: None,
        };
        let rendered = rasterize(&frame, &[arrow]).unwrap();
        assert!(count_matching(&rendered, |px| px[1] > 150 && px[2] > 120) > 0);
    }

    #[test]
    fn invalid_color_falls_back_to_default_stroke() {
        let frame = solid(32, 32, [0, 0, 0, 255], 1.0);
        let rect = styled_rect("not-a-color");
        let rendered = rasterize(&frame, &[rect]).unwrap();
        assert!(count_matching(&rendered, |px| px[0] > 180 && px[1] < 90 && px[2] < 110) > 0);
    }

    #[test]
    fn stroke_width_option_thickens_and_clamps() {
        let frame = solid(40, 40, [0, 0, 0, 255], 1.0);
        let arrow_with = |width: Option<f64>| Annotation::Arrow {
            from: crate::annotate::Point { x: 4.0, y: 20.0 },
            to: crate::annotate::Point { x: 36.0, y: 20.0 },
            color: super::super::DEFAULT_COLOR.into(),
            stroke_width: width,
        };
        let thin = count_matching(&rasterize(&frame, &[arrow_with(None)]).unwrap(), |px| {
            px[0] > 180
        });
        let medium = count_matching(
            &rasterize(&frame, &[arrow_with(Some(5.0))]).unwrap(),
            |px| px[0] > 180,
        );
        let huge = count_matching(
            &rasterize(&frame, &[arrow_with(Some(50.0))]).unwrap(),
            |px| px[0] > 180,
        );
        let clamped = count_matching(
            &rasterize(&frame, &[arrow_with(Some(8.0))]).unwrap(),
            |px| px[0] > 180,
        );
        assert!(
            medium > thin,
            "width 5 should paint more pixels than default 3"
        );
        assert_eq!(huge, clamped, "width 50 should clamp to 8");
    }

    #[test]
    fn stroke_width_option_scales_with_frame_scale() {
        let frame = solid(64, 64, [0, 0, 0, 255], 2.0);
        let arrow = Annotation::Arrow {
            from: crate::annotate::Point { x: 8.0, y: 32.0 },
            to: crate::annotate::Point { x: 56.0, y: 32.0 },
            color: super::super::DEFAULT_COLOR.into(),
            stroke_width: Some(5.0),
        };
        let rendered = rasterize(&frame, std::slice::from_ref(&arrow)).unwrap();
        let wide = count_matching(&rendered, |px| px[0] > 180);
        let frame_scale1 = solid(64, 64, [0, 0, 0, 255], 1.0);
        let rendered_scale1 = rasterize(&frame_scale1, &[arrow]).unwrap();
        let narrow = count_matching(&rendered_scale1, |px| px[0] > 180);
        assert!(
            wide > narrow,
            "5.0 at scale 2 should render as clamped 8, thicker than 5 at scale 1"
        );
    }

    #[test]
    fn mosaic_ignores_style_and_keeps_existing_behavior() {
        let mut checker = vec![0u8; 16 * 16 * 4];
        for y in 0..16u32 {
            for x in 0..16u32 {
                let i = ((y * 16 + x) * 4) as usize;
                if (x + y) % 2 == 0 {
                    checker[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        let frame = Frame {
            width: 16,
            height: 16,
            rgba: checker,
            scale: 1.0,
        };
        let ops = vec![Annotation::Mosaic {
            x: 0.0,
            y: 0.0,
            width: 16.0,
            height: 16.0,
            block: 4,
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        assert_eq!(&rendered.rgba[0..4], &rendered.rgba[4..8]);
    }

    #[test]
    fn text_color_reaches_pixels_when_font_exists() {
        if ui_font().is_none() {
            return;
        }
        let frame = solid(80, 40, [0, 0, 0, 255], 1.0);
        let ops = vec![Annotation::Text {
            x: 6.0,
            y: 4.0,
            text: "Hi".into(),
            size: 22.0,
            color: "#2563eb".into(),
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        assert!(count_matching(&rendered, |px| px[2] > 180 && px[0] < 90) > 0);
        assert_eq!(count_matching(&rendered, |px| px[0] > 180 && px[1] < 90), 0);
    }

    #[test]
    fn ellipse_paints_outline_only() {
        let frame = solid(40, 40, [0, 0, 0, 255], 1.0);
        let op = Annotation::Ellipse {
            x: 4.0,
            y: 4.0,
            width: 32.0,
            height: 32.0,
            color: "#e11d48".into(),
            stroke_width: None,
        };
        let rendered = rasterize(&frame, &[op]).unwrap();
        let px = |x: u32, y: u32| {
            let i = ((y * 40 + x) * 4) as usize;
            [
                rendered.rgba[i],
                rendered.rgba[i + 1],
                rendered.rgba[i + 2],
                rendered.rgba[i + 3],
            ]
        };
        assert!(px(20, 4)[0] > 180, "top of ring should be painted");
        assert!(px(4, 20)[0] > 180, "left of ring should be painted");
        assert_eq!(px(20, 20), [0, 0, 0, 255], "center stays untouched");
        assert_eq!(
            px(5, 5),
            [0, 0, 0, 255],
            "box corner outside ring stays untouched"
        );
    }

    #[test]
    fn line_paints_between_endpoints_only() {
        let frame = solid(40, 40, [0, 0, 0, 255], 1.0);
        let op = Annotation::Line {
            from: crate::annotate::Point { x: 4.0, y: 20.0 },
            to: crate::annotate::Point { x: 36.0, y: 20.0 },
            color: "#2563eb".into(),
            stroke_width: Some(3.0),
        };
        let rendered = rasterize(&frame, &[op]).unwrap();
        let px = |x: u32, y: u32| {
            let i = ((y * 40 + x) * 4) as usize;
            [
                rendered.rgba[i],
                rendered.rgba[i + 1],
                rendered.rgba[i + 2],
                rendered.rgba[i + 3],
            ]
        };
        assert!(
            px(20, 20)[2] > 180 && px(20, 20)[0] < 90,
            "line body painted"
        );
        assert_eq!(px(20, 4), [0, 0, 0, 255], "unrelated row untouched");
    }

    #[test]
    fn highlighter_blends_flat_semi_transparent_tint_not_stacked_opacity() {
        let frame = solid(40, 40, [0, 0, 0, 255], 1.0);
        let points = vec![
            crate::annotate::Point { x: 4.0, y: 20.0 },
            crate::annotate::Point { x: 36.0, y: 20.0 },
        ];
        let highlighter = Annotation::Highlighter {
            points: points.clone(),
            color: "#e11d48".into(),
            stroke_width: Some(5.0),
        };
        let rendered = rasterize(&frame, &[highlighter]).unwrap();
        let i = ((20 * 40 + 20) * 4) as usize;
        let red = rendered.rgba[i];
        // 0.38 阿尔法叠加在纯黑上应约为 0.38×225≈86，而不是逐段盖章逼近 225。
        assert!(
            (60..=120).contains(&red),
            "highlighter center should stay semi-transparent, got {red}"
        );
        assert_eq!(rendered.rgba[i + 3], 255);

        let pen = Annotation::Pen {
            points,
            color: "#e11d48".into(),
            stroke_width: Some(5.0),
        };
        let opaque = rasterize(&frame, &[pen]).unwrap();
        assert!(
            opaque.rgba[i] > 200,
            "pen stroke should be fully opaque, got {}",
            opaque.rgba[i]
        );
    }

    #[test]
    fn pen_paints_through_intermediate_points() {
        let frame = solid(40, 40, [0, 0, 0, 255], 1.0);
        let op = Annotation::Pen {
            points: vec![
                crate::annotate::Point { x: 4.0, y: 4.0 },
                crate::annotate::Point { x: 20.0, y: 20.0 },
                crate::annotate::Point { x: 36.0, y: 4.0 },
            ],
            color: "#10b981".into(),
            stroke_width: None,
        };
        let rendered = rasterize(&frame, &[op]).unwrap();
        let painted = |x: u32, y: u32| {
            let i = ((y * 40 + x) * 4) as usize;
            rendered.rgba[i + 1] > 140
        };
        assert!(painted(12, 12), "first segment painted");
        assert!(painted(28, 12), "second segment painted");
        assert!(!painted(20, 36), "outside polyline untouched");
    }

    #[test]
    fn number_paints_value_when_font_exists() {
        if ui_font().is_none() {
            return;
        }
        let frame = solid(80, 40, [0, 0, 0, 255], 1.0);
        let op = Annotation::Number {
            x: 6.0,
            y: 4.0,
            value: 7,
            size: 22.0,
            color: "#2563eb".into(),
        };
        let rendered = rasterize(&frame, &[op]).unwrap();
        assert!(count_matching(&rendered, |px| px[2] > 180 && px[0] < 90) > 0);
    }

    #[test]
    fn blur_region_loses_original_contrast_and_tiny_region_is_skipped() {
        let mut checker = vec![0u8; 16 * 16 * 4];
        for y in 0..16u32 {
            for x in 0..16u32 {
                let i = ((y * 16 + x) * 4) as usize;
                let value = if (x + y) % 2 == 0 { 255 } else { 0 };
                checker[i..i + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let frame = Frame {
            width: 16,
            height: 16,
            rgba: checker,
            scale: 1.0,
        };
        let luma = |data: &[u8], x: u32, y: u32| data[((y * 16 + x) * 4) as usize];
        let ops = vec![Annotation::Blur {
            x: 0.0,
            y: 0.0,
            width: 16.0,
            height: 16.0,
            sigma: 2.0,
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        let mut max_delta = 0i32;
        for y in 2..14u32 {
            for x in 2..14u32 {
                let delta = i32::from(luma(&rendered.rgba, x, y))
                    - i32::from(luma(&rendered.rgba, x + 1, y));
                max_delta = max_delta.max(delta.abs());
            }
        }
        assert!(
            max_delta < 60,
            "blurred region should not keep checker contrast, got {max_delta}"
        );

        let tiny = vec![Annotation::Blur {
            x: 4.0,
            y: 4.0,
            width: 1.0,
            height: 1.0,
            sigma: 2.0,
        }];
        let unchanged = rasterize(&frame, &tiny).unwrap();
        assert_eq!(
            unchanged.rgba, frame.rgba,
            "1px blur must not change pixels"
        );
    }

    #[test]
    fn frontend_payload_with_nulls_and_points_parses_and_renders() {
        let json = r##"[
          {"type":"ellipse","x":4,"y":4,"width":20,"height":12,"color":"#2563eb","strokeWidth":5},
          {"type":"line","from":{"x":0,"y":0},"to":{"x":12,"y":12},"color":"#e11d48","strokeWidth":null},
          {"type":"number","x":2,"y":2,"value":3,"size":22,"color":"#e11d48"},
          {"type":"highlighter","points":[{"x":0,"y":0},{"x":10,"y":10}],"color":"#f59e0b","strokeWidth":null},
          {"type":"pen","points":[{"x":0,"y":0},{"x":10,"y":0}],"color":"#e11d48","strokeWidth":2},
          {"type":"blur","x":2,"y":2,"width":10,"height":10,"sigma":4}
        ]"##;
        let ops: Vec<Annotation> = serde_json::from_str(json).unwrap();
        assert_eq!(ops.len(), 6);
        // number 变体需要系统字体;无字体环境只验证解析路径。
        if ui_font().is_none() {
            return;
        }
        let frame = solid(32, 32, [255, 255, 255, 255], 1.0);
        let rendered = rasterize(&frame, &ops).unwrap();
        assert_eq!(rendered.rgba.len(), frame.rgba.len());
    }

    #[test]
    fn blur_region_does_not_retain_original_pixels() {
        // 1px 白线叠在纯黑上:模糊后区域内不得再出现原始 255 像素,
        // 且至少有像素被晕开(白线能量扩散),满足"遮盖区域无原始像素残留"。
        let mut rgba = vec![0u8; 24 * 24 * 4];
        for y in 0..24u32 {
            let i = ((y * 24 + 12) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
        }
        let frame = Frame {
            width: 24,
            height: 24,
            rgba,
            scale: 1.0,
        };
        let ops = vec![Annotation::Blur {
            x: 4.0,
            y: 4.0,
            width: 16.0,
            height: 16.0,
            sigma: 3.0,
        }];
        let rendered = rasterize(&frame, &ops).unwrap();
        let mut spread = 0;
        for y in 4..20u32 {
            for x in 4..20u32 {
                let value = rendered.rgba[((y * 24 + x) * 4) as usize];
                assert!(value < 255, "original pixel kept at {x},{y}");
                if value > 0 {
                    spread += 1;
                }
            }
        }
        assert!(spread > 16, "blur should spread the line, got {spread}");
        // 区域外的白线像素保持原样。
        assert_eq!(rendered.rgba[((2 * 24 + 12) * 4) as usize], 255);
    }

    #[test]
    fn masking_ops_compose_and_keep_frame_shape() {
        let mut checker = vec![0u8; 24 * 24 * 4];
        for y in 0..24u32 {
            for x in 0..24u32 {
                let i = ((y * 24 + x) * 4) as usize;
                let value = if (x + y) % 2 == 0 { 255 } else { 0 };
                checker[i..i + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let frame = Frame {
            width: 24,
            height: 24,
            rgba: checker,
            scale: 1.0,
        };
        let ops = vec![
            Annotation::Blur {
                x: 2.0,
                y: 2.0,
                width: 20.0,
                height: 10.0,
                sigma: 3.0,
            },
            Annotation::Mosaic {
                x: 2.0,
                y: 12.0,
                width: 20.0,
                height: 10.0,
                block: 5,
            },
        ];
        let rendered = rasterize(&frame, &ops).unwrap();
        assert_eq!(rendered.width, 24);
        assert_eq!(rendered.height, 24);
        assert_eq!(rendered.rgba.len(), frame.rgba.len());
        // 两片遮盖区域内相邻行不再保留原始棋盘对比。
        let luma = |x: u32, y: u32| rendered.rgba[((y * 24 + x) * 4) as usize];
        assert!((i32::from(luma(3, 3)) - i32::from(luma(3, 4))).abs() < 80);
        assert!((i32::from(luma(3, 14)) - i32::from(luma(3, 15))).abs() < 120);
    }
}
