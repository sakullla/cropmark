//! 导出美化合成(R3, ADR-3)。几何以本模块为准,前端预览按同一公式只做显示层。
//!
//! 输出尺寸 = 帧 + 2 × (留白 + 阴影边距)。阴影边距只由圆角推导:
//! `shadow ? clamp(12 + radius / 2, 12, 48) : 0`,不是独立模糊参数。
//! 圆角先按帧半幅与上限钳制,再推导阴影边距。留白、圆角、阴影全为 0 时
//! 原样返回,像素与尺寸都不变。

use serde::{Deserialize, Serialize};

use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;

pub const MAX_PADDING: u32 = 240;
pub const MAX_RADIUS: u32 = 160;
pub const DEFAULT_PRESET: &str = "paper";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl Rgba {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    fn to_bytes(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }
}

struct Preset {
    id: &'static str,
    start: Rgba,
    end: Rgba,
}

const PRESETS: &[Preset] = &[
    Preset {
        id: "paper",
        start: Rgba::new(0xF4, 0xF1, 0xEA),
        end: Rgba::new(0xF4, 0xF1, 0xEA),
    },
    Preset {
        id: "slate",
        start: Rgba::new(0x33, 0x41, 0x55),
        end: Rgba::new(0x33, 0x41, 0x55),
    },
    Preset {
        id: "ink",
        start: Rgba::new(0x0B, 0x12, 0x20),
        end: Rgba::new(0x0B, 0x12, 0x20),
    },
    Preset {
        id: "dawn",
        start: Rgba::new(0xFD, 0xE6, 0x8A),
        end: Rgba::new(0xFB, 0x71, 0x85),
    },
    Preset {
        id: "ocean",
        start: Rgba::new(0x38, 0xBD, 0xF8),
        end: Rgba::new(0x1E, 0x3A, 0x8A),
    },
    Preset {
        id: "dusk",
        start: Rgba::new(0x31, 0x2E, 0x81),
        end: Rgba::new(0xF4, 0x72, 0xB6),
    },
];

/// 预设 id、留白、圆角与阴影开关。写入 `ExportSettings`;开关本身在功能开关里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BeautifyOptions {
    pub preset: String,
    pub padding: u32,
    pub radius: u32,
    pub shadow: bool,
}

impl Default for BeautifyOptions {
    fn default() -> Self {
        Self {
            preset: DEFAULT_PRESET.into(),
            padding: 32,
            radius: 16,
            shadow: true,
        }
    }
}

impl BeautifyOptions {
    pub fn sanitized(self) -> Self {
        let preset = if PRESETS.iter().any(|item| item.id == self.preset) {
            self.preset
        } else {
            DEFAULT_PRESET.into()
        };
        Self {
            preset,
            padding: self.padding.min(MAX_PADDING),
            radius: self.radius.min(MAX_RADIUS),
            shadow: self.shadow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeautifyLayout {
    pub padding: u32,
    pub radius: u32,
    pub shadow_margin: u32,
    pub origin_x: u32,
    pub origin_y: u32,
    pub output_width: u32,
    pub output_height: u32,
}

/// 阴影边距:圆角的固定函数,与前端 `beautifyShadowMargin` 同一公式。
pub fn shadow_margin(radius: u32, shadow: bool) -> u32 {
    if !shadow {
        return 0;
    }
    (12 + radius / 2).clamp(12, 48)
}

pub fn layout(width: u32, height: u32, options: &BeautifyOptions) -> BeautifyLayout {
    let options = options.clone().sanitized();
    let padding = options.padding;
    let max_radius = (width / 2).min(height / 2).min(MAX_RADIUS);
    let radius = options.radius.min(max_radius);
    let shadow_margin = shadow_margin(radius, options.shadow);
    let inset = padding.saturating_add(shadow_margin);
    BeautifyLayout {
        padding,
        radius,
        shadow_margin,
        origin_x: inset,
        origin_y: inset,
        output_width: width.saturating_add(inset.saturating_mul(2)),
        output_height: height.saturating_add(inset.saturating_mul(2)),
    }
}

pub fn apply(frame: &Frame, options: &BeautifyOptions) -> Result<Frame, CaptureError> {
    let options = options.clone().sanitized();
    let geom = layout(frame.width, frame.height, &options);
    if geom.padding == 0 && geom.radius == 0 && geom.shadow_margin == 0 {
        return Ok(frame.clone());
    }
    let len = byte_len(geom.output_width, geom.output_height)?;
    let preset = preset_by_id(&options.preset);
    let mut rgba = vec![0u8; len];
    fill_background(&mut rgba, geom.output_width, geom.output_height, preset);
    if geom.shadow_margin > 0 {
        paint_shadow(&mut rgba, frame.width, frame.height, &geom)?;
    }
    composite_frame(&mut rgba, frame, &geom);
    Ok(Frame {
        width: geom.output_width,
        height: geom.output_height,
        rgba,
        scale: frame.scale,
    })
}

/// 与预览导出 `compose_output` 同一规则。关闭时原样返回 `frame`。
/// 还要保留未美化帧的调用方必须传入克隆,避免回写冻结画面。
pub fn compose_frame(
    frame: Frame,
    enabled: bool,
    options: &BeautifyOptions,
) -> Result<Frame, CaptureError> {
    if !enabled {
        return Ok(frame);
    }
    apply(&frame, options)
}

fn preset_by_id(id: &str) -> &'static Preset {
    PRESETS
        .iter()
        .find(|item| item.id == id)
        .unwrap_or(&PRESETS[0])
}

fn byte_len(width: u32, height: u32) -> Result<usize, CaptureError> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|len| *len > 0)
        .ok_or_else(|| CaptureError::api("error.capture.export"))
}

fn fill_background(rgba: &mut [u8], width: u32, height: u32, preset: &Preset) {
    let solid = preset.start == preset.end;
    for y in 0..height {
        for x in 0..width {
            let color = if solid {
                preset.start
            } else {
                lerp(preset.start, preset.end, gradient_t(x, y, width, height))
            };
            let index = ((y as usize * width as usize) + x as usize) * 4;
            rgba[index..index + 4].copy_from_slice(&color.to_bytes());
        }
    }
}

fn gradient_t(x: u32, y: u32, width: u32, height: u32) -> f32 {
    let tx = if width <= 1 {
        0.0
    } else {
        x as f32 / (width - 1) as f32
    };
    let ty = if height <= 1 {
        0.0
    } else {
        y as f32 / (height - 1) as f32
    };
    (tx + ty) * 0.5
}

fn lerp(start: Rgba, end: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    let channel = |a: u8, b: u8| -> u8 {
        let value = f32::from(a) + (f32::from(b) - f32::from(a)) * t;
        value.round().clamp(0.0, 255.0) as u8
    };
    Rgba {
        r: channel(start.r, end.r),
        g: channel(start.g, end.g),
        b: channel(start.b, end.b),
        a: 255,
    }
}

fn paint_shadow(
    rgba: &mut [u8],
    frame_width: u32,
    frame_height: u32,
    geom: &BeautifyLayout,
) -> Result<(), CaptureError> {
    let width = geom.output_width as usize;
    let height = geom.output_height as usize;
    let mut alpha = vec![0f32; width * height];
    let dy = (geom.shadow_margin / 5).max(1) as f32;
    let radius = geom.radius as f32;
    for y in 0..height {
        for x in 0..width {
            let coverage = round_rect_coverage(
                x as f32 + 0.5 - geom.origin_x as f32,
                y as f32 + 0.5 - (geom.origin_y as f32 + dy),
                frame_width as f32,
                frame_height as f32,
                radius,
            );
            if coverage > 0.0 {
                alpha[y * width + x] = coverage;
            }
        }
    }
    let blurred = box_blur(&alpha, width, height, (geom.shadow_margin / 3).max(1));
    for (index, shade) in blurred.iter().enumerate() {
        let amount = (*shade * 0.38).clamp(0.0, 1.0);
        if amount <= 0.0 {
            continue;
        }
        let base = index * 4;
        for channel in 0..3 {
            let value = f32::from(rgba[base + channel]) * (1.0 - amount);
            rgba[base + channel] = value.round().clamp(0.0, 255.0) as u8;
        }
        rgba[base + 3] = 255;
    }
    Ok(())
}

fn composite_frame(rgba: &mut [u8], frame: &Frame, geom: &BeautifyLayout) {
    let radius = geom.radius as f32;
    let frame_width = frame.width as f32;
    let frame_height = frame.height as f32;
    for y in 0..frame.height {
        for x in 0..frame.width {
            let coverage = if geom.radius == 0 {
                1.0
            } else {
                round_rect_coverage(
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                    frame_width,
                    frame_height,
                    radius,
                )
            };
            if coverage <= 0.0 {
                continue;
            }
            let src_index = ((y as usize * frame.width as usize) + x as usize) * 4;
            let dx = geom.origin_x + x;
            let dy = geom.origin_y + y;
            let dst_index = ((dy as usize * geom.output_width as usize) + dx as usize) * 4;
            let src = [
                frame.rgba[src_index],
                frame.rgba[src_index + 1],
                frame.rgba[src_index + 2],
                frame.rgba[src_index + 3],
            ];
            let dst = [
                rgba[dst_index],
                rgba[dst_index + 1],
                rgba[dst_index + 2],
                rgba[dst_index + 3],
            ];
            let mixed = if geom.radius == 0 && src[3] == 255 {
                src
            } else {
                over(dst, src, coverage)
            };
            rgba[dst_index..dst_index + 4].copy_from_slice(&mixed);
        }
    }
}

fn over(dst: [u8; 4], src: [u8; 4], coverage: f32) -> [u8; 4] {
    let src_a = (f32::from(src[3]) / 255.0) * coverage.clamp(0.0, 1.0);
    let dst_a = f32::from(dst[3]) / 255.0;
    let out_a = src_a + dst_a * (1.0 - src_a);
    if out_a <= 0.0 {
        return [0, 0, 0, 0];
    }
    let channel = |s: u8, d: u8| -> u8 {
        let value = (f32::from(s) / 255.0) * src_a + (f32::from(d) / 255.0) * dst_a * (1.0 - src_a);
        (value / out_a * 255.0).round().clamp(0.0, 255.0) as u8
    };
    [
        channel(src[0], dst[0]),
        channel(src[1], dst[1]),
        channel(src[2], dst[2]),
        (out_a * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

/// 圆角矩形覆盖率。`radius == 0` 时像素中心落在矩形内即为 1。
fn round_rect_coverage(x: f32, y: f32, width: f32, height: f32, radius: f32) -> f32 {
    if width <= 0.0 || height <= 0.0 {
        return 0.0;
    }
    if radius <= 0.0 {
        return if (0.0..width).contains(&x) && (0.0..height).contains(&y) {
            1.0
        } else {
            0.0
        };
    }
    let radius = radius.min(width * 0.5).min(height * 0.5);
    let px = x - width * 0.5;
    let py = y - height * 0.5;
    let qx = px.abs() - width * 0.5 + radius;
    let qy = py.abs() - height * 0.5 + radius;
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    let inside = qx.max(qy).min(0.0);
    (0.5 - (outside + inside - radius)).clamp(0.0, 1.0)
}

fn box_blur(src: &[f32], width: usize, height: usize, radius: u32) -> Vec<f32> {
    if radius == 0 || width == 0 || height == 0 {
        return src.to_vec();
    }
    let radius = radius as usize;
    let mut horizontal = vec![0f32; src.len()];
    blur_horizontal(src, &mut horizontal, width, height, radius);
    let mut vertical = vec![0f32; src.len()];
    blur_vertical(&horizontal, &mut vertical, width, height, radius);
    vertical
}

fn blur_horizontal(src: &[f32], dst: &mut [f32], width: usize, height: usize, radius: usize) {
    for y in 0..height {
        let row = y * width;
        let mut prefix = vec![0f32; width + 1];
        for x in 0..width {
            prefix[x + 1] = prefix[x] + src[row + x];
        }
        for x in 0..width {
            let left = x.saturating_sub(radius);
            let right = (x + radius).min(width - 1);
            let count = (right - left + 1) as f32;
            dst[row + x] = (prefix[right + 1] - prefix[left]) / count;
        }
    }
}

fn blur_vertical(src: &[f32], dst: &mut [f32], width: usize, height: usize, radius: usize) {
    for x in 0..width {
        let mut prefix = vec![0f32; height + 1];
        for y in 0..height {
            prefix[y + 1] = prefix[y] + src[y * width + x];
        }
        for y in 0..height {
            let top = y.saturating_sub(radius);
            let bottom = (y + radius).min(height - 1);
            let count = (bottom - top + 1) as f32;
            dst[y * width + x] = (prefix[bottom + 1] - prefix[top]) / count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            bytes.extend_from_slice(&color);
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap()
    }

    fn pixel(frame: &Frame, x: u32, y: u32) -> [u8; 4] {
        let index = ((y * frame.width + x) * 4) as usize;
        frame.rgba[index..index + 4].try_into().unwrap()
    }

    fn options(preset: &str, padding: u32, radius: u32, shadow: bool) -> BeautifyOptions {
        BeautifyOptions {
            preset: preset.into(),
            padding,
            radius,
            shadow,
        }
    }

    #[test]
    fn zero_options_keep_the_original_frame() {
        let frame = solid(12, 8, [10, 20, 30, 255]);
        let out = apply(&frame, &options("paper", 0, 0, false)).unwrap();
        assert_eq!(out.width, frame.width);
        assert_eq!(out.height, frame.height);
        assert_eq!(out.rgba, frame.rgba);
    }

    #[test]
    fn padding_expands_output_and_corners_are_background() {
        let frame = solid(20, 10, [255, 0, 0, 255]);
        let geom = layout(20, 10, &options("paper", 4, 0, false));
        assert_eq!(geom.output_width, 28);
        assert_eq!(geom.output_height, 18);
        assert_eq!(geom.origin_x, 4);
        let out = apply(&frame, &options("paper", 4, 0, false)).unwrap();
        assert_eq!((out.width, out.height), (28, 18));
        assert_eq!(pixel(&out, 0, 0), [0xF4, 0xF1, 0xEA, 255]);
        assert_eq!(pixel(&out, 27, 17), [0xF4, 0xF1, 0xEA, 255]);
        assert_eq!(pixel(&out, 4, 4), [255, 0, 0, 255]);
        assert_eq!(pixel(&out, 23, 13), [255, 0, 0, 255]);
    }

    #[test]
    fn rounded_corners_reveal_background_without_changing_center() {
        let frame = solid(20, 20, [0, 255, 0, 255]);
        let out = apply(&frame, &options("ink", 0, 8, false)).unwrap();
        assert_eq!((out.width, out.height), (20, 20));
        assert_eq!(pixel(&out, 0, 0), [0x0B, 0x12, 0x20, 255]);
        assert_eq!(pixel(&out, 10, 10), [0, 255, 0, 255]);
    }

    #[test]
    fn shadow_margin_follows_radius_and_grows_the_canvas() {
        assert_eq!(shadow_margin(0, false), 0);
        assert_eq!(shadow_margin(0, true), 12);
        assert_eq!(shadow_margin(8, true), 16);
        assert_eq!(shadow_margin(200, true), 48);
        let geom = layout(20, 20, &options("paper", 0, 8, true));
        assert_eq!(geom.radius, 8);
        assert_eq!(geom.shadow_margin, 16);
        assert_eq!(geom.output_width, 52);
        let frame = solid(20, 20, [0, 0, 255, 255]);
        let out = apply(&frame, &options("paper", 0, 8, true)).unwrap();
        assert_eq!((out.width, out.height), (52, 52));
        assert_eq!(pixel(&out, 0, 0), [0xF4, 0xF1, 0xEA, 255]);
        assert_eq!(
            pixel(&out, geom.origin_x + 10, geom.origin_y + 10),
            [0, 0, 255, 255]
        );
    }

    #[test]
    fn gradient_preset_runs_from_start_to_end() {
        let frame = solid(4, 4, [255, 255, 255, 255]);
        let out = apply(&frame, &options("dawn", 2, 0, false)).unwrap();
        assert_eq!(pixel(&out, 0, 0), [0xFD, 0xE6, 0x8A, 255]);
        let last_x = out.width - 1;
        let last_y = out.height - 1;
        assert_eq!(pixel(&out, last_x, last_y), [0xFB, 0x71, 0x85, 255]);
    }

    #[test]
    fn unknown_preset_and_oversized_options_are_clamped() {
        let dirty = options("nope", 10_000, 10_000, true).sanitized();
        assert_eq!(dirty.preset, "paper");
        assert_eq!(dirty.padding, MAX_PADDING);
        assert_eq!(dirty.radius, MAX_RADIUS);
        let geom = layout(30, 10, &dirty);
        assert_eq!(geom.padding, MAX_PADDING);
        assert_eq!(geom.radius, 5);
    }
}
