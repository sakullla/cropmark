//! 位图图标资产:内嵌 PNG(include_bytes!)+ image 解码,按缩放档
//! (24px/48px)缓存解码位图,并按 (图标, 目标尺寸, 着色) 缓存预合成
//! 的 RGBA 字形,绘制时经 `composer::blend()` 叠到帧上。
//!
//! 资产为黑线透明底单色稿:每个像素的覆盖度 = alpha × (1 − 亮度)。
//! 24/48 原稿已按像素网格超采样,斜边保留灰度抗锯齿,不再收成纯黑白。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::composer::blend;
use super::{AnnotationTool, SelectionAction, ToolMode};
use crate::capture::buffer::decode_rgba;

/// 工具条/菜单图标的资产身份(R3:替代程序化 SDF 字形;R5 增补 5 个注册表工具)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconName {
    Rect,
    Ellipse,
    Arrow,
    Text,
    Undo,
    Redo,
    Delete,
    More,
    Copy,
    Save,
    Cancel,
    Line,
    Number,
    Pen,
    Highlighter,
    Mosaic,
    Blur,
    Pin,
    Ocr,
    Qr,
    Annotate,
    Spotlight,
    Magnifier,
    Bubble,
    Sticker,
    Erase,
}

/// 缓存句柄:泄漏的 'static 位图切片,进程级缓存条目。
type CachedBitmap = &'static [u8];
type CoverageCache = Mutex<HashMap<(IconName, u8), CachedBitmap>>;
type TintedCache = Mutex<HashMap<(IconName, u32, [u8; 4]), CachedBitmap>>;

impl IconName {
    pub const ALL: [Self; 26] = [
        Self::Rect,
        Self::Ellipse,
        Self::Arrow,
        Self::Text,
        Self::Undo,
        Self::Redo,
        Self::Delete,
        Self::More,
        Self::Copy,
        Self::Save,
        Self::Cancel,
        Self::Line,
        Self::Number,
        Self::Pen,
        Self::Highlighter,
        Self::Mosaic,
        Self::Blur,
        Self::Pin,
        Self::Ocr,
        Self::Qr,
        Self::Annotate,
        Self::Spotlight,
        Self::Magnifier,
        Self::Bubble,
        Self::Sticker,
        Self::Erase,
    ];

    const fn slug(self) -> &'static str {
        match self {
            Self::Rect => "rect",
            Self::Ellipse => "ellipse",
            Self::Arrow => "arrow",
            Self::Text => "text",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::Delete => "delete",
            Self::More => "more",
            Self::Copy => "copy",
            Self::Save => "save",
            Self::Cancel => "cancel",
            Self::Line => "line",
            Self::Number => "number",
            Self::Pen => "pen",
            Self::Highlighter => "highlighter",
            Self::Mosaic => "mosaic",
            Self::Blur => "blur",
            Self::Pin => "pin",
            Self::Ocr => "ocr",
            Self::Qr => "qr",
            Self::Annotate => "annotate",
            Self::Spotlight => "spotlight",
            Self::Magnifier => "magnifier",
            Self::Bubble => "bubble",
            Self::Sticker => "sticker",
            Self::Erase => "erase",
        }
    }

    fn png(self, tier: u8) -> &'static [u8] {
        match tier {
            24 => match self {
                Self::Rect => include_bytes!("../../../icons/toolbar/rect-24.png"),
                Self::Ellipse => include_bytes!("../../../icons/toolbar/ellipse-24.png"),
                Self::Arrow => include_bytes!("../../../icons/toolbar/arrow-24.png"),
                Self::Text => include_bytes!("../../../icons/toolbar/text-24.png"),
                Self::Undo => include_bytes!("../../../icons/toolbar/undo-24.png"),
                Self::Redo => include_bytes!("../../../icons/toolbar/redo-24.png"),
                Self::Delete => include_bytes!("../../../icons/toolbar/delete-24.png"),
                Self::More => include_bytes!("../../../icons/toolbar/more-24.png"),
                Self::Copy => include_bytes!("../../../icons/toolbar/copy-24.png"),
                Self::Save => include_bytes!("../../../icons/toolbar/save-24.png"),
                Self::Cancel => include_bytes!("../../../icons/toolbar/cancel-24.png"),
                Self::Line => include_bytes!("../../../icons/toolbar/line-24.png"),
                Self::Number => include_bytes!("../../../icons/toolbar/number-24.png"),
                Self::Pen => include_bytes!("../../../icons/toolbar/pen-24.png"),
                Self::Highlighter => {
                    include_bytes!("../../../icons/toolbar/highlighter-24.png")
                }
                Self::Mosaic => include_bytes!("../../../icons/toolbar/mosaic-24.png"),
                Self::Blur => include_bytes!("../../../icons/toolbar/blur-24.png"),
                Self::Pin => include_bytes!("../../../icons/toolbar/pin-24.png"),
                Self::Ocr => include_bytes!("../../../icons/toolbar/ocr-24.png"),
                Self::Qr => include_bytes!("../../../icons/toolbar/qr-24.png"),
                Self::Annotate => include_bytes!("../../../icons/toolbar/annotate-24.png"),
                Self::Spotlight => include_bytes!("../../../icons/toolbar/spotlight-24.png"),
                Self::Magnifier => include_bytes!("../../../icons/toolbar/magnifier-24.png"),
                Self::Bubble => include_bytes!("../../../icons/toolbar/bubble-24.png"),
                Self::Sticker => include_bytes!("../../../icons/toolbar/sticker-24.png"),
                Self::Erase => include_bytes!("../../../icons/toolbar/erase-24.png"),
            },
            _ => match self {
                Self::Rect => include_bytes!("../../../icons/toolbar/rect-48.png"),
                Self::Ellipse => include_bytes!("../../../icons/toolbar/ellipse-48.png"),
                Self::Arrow => include_bytes!("../../../icons/toolbar/arrow-48.png"),
                Self::Text => include_bytes!("../../../icons/toolbar/text-48.png"),
                Self::Undo => include_bytes!("../../../icons/toolbar/undo-48.png"),
                Self::Redo => include_bytes!("../../../icons/toolbar/redo-48.png"),
                Self::Delete => include_bytes!("../../../icons/toolbar/delete-48.png"),
                Self::More => include_bytes!("../../../icons/toolbar/more-48.png"),
                Self::Copy => include_bytes!("../../../icons/toolbar/copy-48.png"),
                Self::Save => include_bytes!("../../../icons/toolbar/save-48.png"),
                Self::Cancel => include_bytes!("../../../icons/toolbar/cancel-48.png"),
                Self::Line => include_bytes!("../../../icons/toolbar/line-48.png"),
                Self::Number => include_bytes!("../../../icons/toolbar/number-48.png"),
                Self::Pen => include_bytes!("../../../icons/toolbar/pen-48.png"),
                Self::Highlighter => {
                    include_bytes!("../../../icons/toolbar/highlighter-48.png")
                }
                Self::Mosaic => include_bytes!("../../../icons/toolbar/mosaic-48.png"),
                Self::Blur => include_bytes!("../../../icons/toolbar/blur-48.png"),
                Self::Pin => include_bytes!("../../../icons/toolbar/pin-48.png"),
                Self::Ocr => include_bytes!("../../../icons/toolbar/ocr-48.png"),
                Self::Qr => include_bytes!("../../../icons/toolbar/qr-48.png"),
                Self::Annotate => include_bytes!("../../../icons/toolbar/annotate-48.png"),
                Self::Spotlight => include_bytes!("../../../icons/toolbar/spotlight-48.png"),
                Self::Magnifier => include_bytes!("../../../icons/toolbar/magnifier-48.png"),
                Self::Bubble => include_bytes!("../../../icons/toolbar/bubble-48.png"),
                Self::Sticker => include_bytes!("../../../icons/toolbar/sticker-48.png"),
                Self::Erase => include_bytes!("../../../icons/toolbar/erase-48.png"),
            },
        }
    }

    /// 黑线透明底 → 单通道覆盖度位图(边长 tier,按缩放档缓存)。
    fn coverage(self, tier: u8) -> &'static [u8] {
        fn load(name: IconName, tier: u8) -> Box<[u8]> {
            let (width, height, rgba) =
                decode_rgba(name.png(tier)).expect("toolbar icon PNG must decode");
            assert_eq!((width, height), (u32::from(tier), u32::from(tier)));
            let mut coverage = Vec::with_capacity((width * height) as usize);
            for px in rgba.chunks_exact(4) {
                let lum = (u16::from(px[0]) + u16::from(px[1]) + u16::from(px[2])) / 3;
                let cover = (u16::from(px[3]) * (255 - lum) / 255) as u8;
                coverage.push(cover);
            }
            coverage.into_boxed_slice()
        }
        static COVERAGE: OnceLock<CoverageCache> = OnceLock::new();
        let cache = COVERAGE.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache.lock().expect("icon coverage cache poisoned");
        cache
            .entry((self, tier))
            .or_insert_with(|| Box::leak(load(self, tier)))
    }

    /// 目标尺寸 + 墨色 → 预合成 RGBA 字形。
    /// 直接在目标物理像素网格上按连续有符号距离场(SDF)超采样生成覆盖度,
    /// 避免 48→36 等非整数缩放档(125%/150%/175% DPI)下位图重采样造成的模糊与阶梯锯齿。
    fn tinted(self, size: u32, ink: [u8; 4]) -> &'static [u8] {
        fn render(name: IconName, size: u32, ink: [u8; 4]) -> Box<[u8]> {
            let size = size.max(1);
            // 保留 24/48 解码路径热身,确保内嵌资产与缓存契约常驻。
            let _ = name.coverage(if size <= 24 { 24 } else { 48 });
            let cov = render_sdf_coverage(name, size);
            let mut out = vec![0u8; (size * size) as usize * 4];
            tint_into(&cov, &mut out, ink);
            out.into_boxed_slice()
        }
        static TINTED: OnceLock<TintedCache> = OnceLock::new();
        let cache = TINTED.get_or_init(|| Mutex::new(HashMap::new()));
        let mut cache = cache.lock().expect("icon tinted cache poisoned");
        cache
            .entry((self, size, ink))
            .or_insert_with(|| Box::leak(render(self, size, ink)))
    }

    /// 合成动作 → 图标资产;`CopyColor` 是键盘动作、永不渲染,无资产。
    pub fn for_action(action: SelectionAction) -> Option<Self> {
        Some(match action {
            SelectionAction::Copy => Self::Copy,
            SelectionAction::Save => Self::Save,
            SelectionAction::Pin => Self::Pin,
            SelectionAction::Annotate => Self::Annotate,
            SelectionAction::Ocr => Self::Ocr,
            SelectionAction::Qr => Self::Qr,
            SelectionAction::Cancel => Self::Cancel,
            SelectionAction::Tool(AnnotationTool::Rect) => Self::Rect,
            SelectionAction::Tool(AnnotationTool::Ellipse) => Self::Ellipse,
            SelectionAction::Tool(AnnotationTool::Arrow) => Self::Arrow,
            SelectionAction::Tool(AnnotationTool::Number) => Self::Number,
            SelectionAction::Tool(AnnotationTool::Text) => Self::Text,
            SelectionAction::Tool(AnnotationTool::Highlighter) => Self::Highlighter,
            SelectionAction::Tool(AnnotationTool::Mosaic) => Self::Mosaic,
            SelectionAction::Tool(AnnotationTool::Spotlight) => Self::Spotlight,
            SelectionAction::Tool(AnnotationTool::Magnifier) => Self::Magnifier,
            SelectionAction::Tool(AnnotationTool::Bubble) => Self::Bubble,
            SelectionAction::Tool(AnnotationTool::Sticker) => Self::Sticker,
            SelectionAction::Tool(AnnotationTool::Erase) => Self::Erase,
            SelectionAction::Mode(ToolMode::Arrow) => Self::Arrow,
            SelectionAction::Mode(ToolMode::Line) => Self::Line,
            SelectionAction::Mode(ToolMode::Highlighter) => Self::Highlighter,
            SelectionAction::Mode(ToolMode::Pen) => Self::Pen,
            SelectionAction::Mode(ToolMode::Mosaic) => Self::Mosaic,
            SelectionAction::Mode(ToolMode::Blur) => Self::Blur,
            SelectionAction::Undo => Self::Undo,
            SelectionAction::Redo => Self::Redo,
            SelectionAction::Delete => Self::Delete,
            SelectionAction::More => Self::More,
            // 长截图与录屏字形由合成器几何笔画绘制,无位图资产。
            SelectionAction::LongCapture => return None,
            SelectionAction::Recording => return None,
            SelectionAction::CopyColor => return None,
        })
    }
}

/// 24 与 48 是 1:1 资产。24–47 按请求尺寸从 48 稿缩小,跟着 DPI 变,
/// 不再把 150% 的 36px 请求钉死成 24(显得小)或抬到 48(显得胖)。
/// 不够 24 的请求仍升到 24,避免把笔画缩成灰边。
pub(crate) fn crisp_icon_px(requested: u32) -> u32 {
    if requested >= 96 {
        48 * (requested / 48)
    } else if requested >= 48 {
        48
    } else if requested >= 24 {
        requested
    } else if requested >= 18 {
        24
    } else {
        requested.max(1)
    }
}

fn tint_into(src: &[u8], out: &mut [u8], ink: [u8; 4]) {
    for (i, &cover) in src.iter().enumerate() {
        let i = i * 4;
        out[i] = ink[0];
        out[i + 1] = ink[1];
        out[i + 2] = ink[2];
        out[i + 3] = (u16::from(ink[3]) * u16::from(cover) / 255) as u8;
    }
}

fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    let t = if len_sq > 1e-6 {
        (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (px - (ax + t * dx)).hypot(py - (ay + t * dy))
}

fn sd_polyline(px: f32, py: f32, pts: &[(f32, f32)]) -> f32 {
    let mut best = f32::INFINITY;
    for pair in pts.windows(2) {
        let (ax, ay) = pair[0];
        let (bx, by) = pair[1];
        best = best.min(sd_segment(px, py, ax, ay, bx, by));
    }
    best
}

fn sd_polygon_stroke(px: f32, py: f32, pts: &[(f32, f32)]) -> f32 {
    if pts.is_empty() {
        return f32::INFINITY;
    }
    let mut best = sd_polyline(px, py, pts);
    if pts.len() >= 3 {
        let (ax, ay) = pts[pts.len() - 1];
        let (bx, by) = pts[0];
        best = best.min(sd_segment(px, py, ax, ay, bx, by));
    }
    best
}

fn sd_circle_stroke(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    ((px - cx).hypot(py - cy) - r).abs()
}

fn sd_disc(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> f32 {
    (px - cx).hypot(py - cy) - r
}

fn sd_round_rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let hw = (x1 - x0) * 0.5;
    let hh = (y1 - y0) * 0.5;
    let r = r.clamp(0.0, hw.min(hh));
    let qx = (px - cx).abs() - (hw - r);
    let qy = (py - cy).abs() - (hh - r);
    let outside = if qx > 0.0 && qy > 0.0 {
        qx.hypot(qy)
    } else {
        qx.max(0.0) + qy.max(0.0)
    };
    let inside = qx.max(qy).min(0.0);
    outside + inside - r
}

fn sd_arc(px: f32, py: f32, cx: f32, cy: f32, r: f32, start_deg: f32, end_deg: f32) -> f32 {
    let dx = px - cx;
    let dy = py - cy;
    let mut ang = dy.atan2(dx).to_degrees();
    while ang < start_deg {
        ang += 360.0;
    }
    while ang >= start_deg + 360.0 {
        ang -= 360.0;
    }
    if ang <= end_deg {
        (dx.hypot(dy) - r).abs()
    } else {
        let a0 = start_deg.to_radians();
        let a1 = end_deg.to_radians();
        let d0 = (px - (cx + r * a0.cos())).hypot(py - (cy + r * a0.sin()));
        let d1 = (px - (cx + r * a1.cos())).hypot(py - (cy + r * a1.sin()));
        d0.min(d1)
    }
}

/// 24×24 规范坐标系下的图标有符号距离场(负值在笔画/填充内部,单位为 24 网格单位)。
/// 圆头端点与圆角折点由欧氏距离自然保证。
fn icon_sdf_24(name: IconName, px: f32, py: f32, h: f32) -> f32 {
    match name {
        IconName::Rect => sd_round_rect(px, py, 4.8, 5.8, 19.2, 18.2, 2.2).abs() - h,
        IconName::Ellipse => sd_circle_stroke(px, py, 12.0, 12.0, 6.8) - h,
        IconName::Arrow => {
            let shaft = sd_segment(px, py, 6.0, 18.0, 17.4, 6.6);
            let head = sd_polyline(px, py, &[(10.4, 6.2), (17.8, 6.2), (17.8, 13.6)]);
            shaft.min(head) - h
        }
        IconName::Line => {
            let shaft = sd_segment(px, py, 6.5, 17.5, 17.5, 6.5) - h;
            let p0 = sd_disc(px, py, 6.5, 17.5, 2.0);
            let p1 = sd_disc(px, py, 17.5, 6.5, 2.0);
            shaft.min(p0).min(p1)
        }
        IconName::Highlighter => {
            let body = sd_polygon_stroke(
                px,
                py,
                &[
                    (9.2, 13.0),
                    (15.2, 7.0),
                    (18.4, 10.2),
                    (12.4, 16.2),
                    (7.6, 16.6),
                ],
            ) - h;
            let collar = sd_segment(px, py, 13.4, 8.8, 16.6, 12.0) - h;
            let bar = sd_segment(px, py, 5.2, 19.6, 18.8, 19.6) - h * 1.15;
            body.min(collar).min(bar)
        }
        IconName::Pen => {
            let body = sd_polygon_stroke(
                px,
                py,
                &[
                    (15.2, 4.8),
                    (19.2, 8.8),
                    (8.8, 19.2),
                    (4.8, 19.2),
                    (4.8, 15.2),
                ],
            ) - h;
            let collar = sd_segment(px, py, 12.4, 7.6, 16.4, 11.6) - h;
            body.min(collar)
        }
        IconName::Mosaic => {
            let a = sd_round_rect(px, py, 4.8, 4.8, 10.8, 10.8, 1.3);
            let b = sd_round_rect(px, py, 13.2, 4.8, 19.2, 10.8, 1.3);
            let c = sd_round_rect(px, py, 4.8, 13.2, 10.8, 19.2, 1.3);
            let d = sd_round_rect(px, py, 13.2, 13.2, 19.2, 19.2, 1.3);
            a.min(b).min(c).min(d)
        }
        IconName::Blur => {
            let outer = sd_circle_stroke(px, py, 12.0, 12.0, 7.0) - h;
            let inner = sd_circle_stroke(px, py, 12.0, 12.0, 3.2) - h;
            outer.min(inner)
        }
        IconName::Text => {
            let top = sd_segment(px, py, 5.8, 6.2, 18.2, 6.2);
            let stem = sd_segment(px, py, 12.0, 6.2, 12.0, 18.0);
            let base = sd_segment(px, py, 8.8, 18.0, 15.2, 18.0);
            top.min(stem).min(base) - h
        }
        IconName::Number => {
            let ring = sd_circle_stroke(px, py, 12.0, 12.0, 7.2) - h;
            let one = sd_polyline(px, py, &[(10.4, 9.6), (12.2, 8.2), (12.2, 15.6)])
                .min(sd_segment(px, py, 10.2, 15.6, 14.2, 15.6))
                - h * 0.9;
            ring.min(one)
        }
        IconName::Undo => {
            let top = sd_segment(px, py, 5.8, 9.6, 13.8, 9.6);
            let arc = sd_arc(px, py, 13.8, 13.8, 4.2, -90.0, 90.0);
            let bottom = sd_segment(px, py, 9.6, 18.0, 13.8, 18.0);
            let head = sd_polyline(px, py, &[(9.0, 6.4), (5.8, 9.6), (9.0, 12.8)]);
            top.min(arc).min(bottom).min(head) - h
        }
        IconName::Redo => {
            let top = sd_segment(px, py, 10.2, 9.6, 18.2, 9.6);
            let arc = sd_arc(px, py, 10.2, 13.8, 4.2, 90.0, 270.0);
            let bottom = sd_segment(px, py, 10.2, 18.0, 14.4, 18.0);
            let head = sd_polyline(px, py, &[(15.0, 6.4), (18.2, 9.6), (15.0, 12.8)]);
            top.min(arc).min(bottom).min(head) - h
        }
        IconName::Pin => {
            let dome = sd_arc(px, py, 12.0, 10.2, 5.0, 148.0, 392.0);
            let cone = sd_polyline(px, py, &[(7.76, 12.85), (12.0, 19.4), (16.24, 12.85)]);
            let dot = sd_disc(px, py, 12.0, 10.2, 1.65);
            (dome.min(cone) - h).min(dot)
        }
        IconName::Save => {
            let shaft = sd_segment(px, py, 12.0, 4.8, 12.0, 14.2);
            let chevron = sd_polyline(px, py, &[(8.2, 10.6), (12.0, 14.4), (15.8, 10.6)]);
            let tray = sd_polyline(
                px,
                py,
                &[(5.2, 15.6), (5.2, 18.8), (18.8, 18.8), (18.8, 15.6)],
            );
            shaft.min(chevron).min(tray) - h
        }
        IconName::Copy => {
            let front_box = sd_round_rect(px, py, 9.2, 9.2, 19.0, 19.0, 1.8);
            let front = front_box.abs() - h;
            let back_box = sd_round_rect(px, py, 5.0, 5.0, 14.8, 14.8, 1.8);
            let back = (back_box.abs() - h).max(-(front_box - h * 0.6));
            front.min(back)
        }
        IconName::Cancel => {
            let a = sd_segment(px, py, 6.8, 6.8, 17.2, 17.2);
            let b = sd_segment(px, py, 17.2, 6.8, 6.8, 17.2);
            a.min(b) - h
        }
        IconName::More => sd_disc(px, py, 5.5, 12.0, 1.75)
            .min(sd_disc(px, py, 12.0, 12.0, 1.75))
            .min(sd_disc(px, py, 18.5, 12.0, 1.75)),
        IconName::Delete => {
            let lid = sd_segment(px, py, 5.2, 7.0, 18.8, 7.0);
            let handle = sd_polyline(px, py, &[(9.5, 7.0), (9.5, 4.6), (14.5, 4.6), (14.5, 7.0)]);
            let bucket = sd_polyline(px, py, &[(7.2, 7.0), (8.0, 19.0), (16.0, 19.0), (16.8, 7.0)]);
            let ribs = sd_segment(px, py, 10.5, 10.6, 10.5, 15.8)
                .min(sd_segment(px, py, 13.5, 10.6, 13.5, 15.8));
            lid.min(handle).min(bucket).min(ribs) - h
        }
        IconName::Ocr => {
            let doc = sd_polygon_stroke(
                px,
                py,
                &[
                    (6.0, 4.4),
                    (14.0, 4.4),
                    (18.0, 8.4),
                    (18.0, 19.6),
                    (6.0, 19.6),
                ],
            );
            let fold = sd_polyline(px, py, &[(14.0, 4.4), (14.0, 8.4), (18.0, 8.4)]);
            let lines = sd_segment(px, py, 9.0, 12.0, 15.0, 12.0)
                .min(sd_segment(px, py, 9.0, 15.0, 15.0, 15.0))
                .min(sd_segment(px, py, 9.0, 17.8, 12.4, 17.8));
            doc.min(fold).min(lines) - h
        }
        IconName::Qr => {
            let finder = |x0: f32, y0: f32| {
                let box_d = sd_round_rect(px, py, x0, y0, x0 + 5.4, y0 + 5.4, 0.8).abs() - h * 0.9;
                let dot_d = sd_disc(px, py, x0 + 2.7, y0 + 2.7, 1.05);
                box_d.min(dot_d)
            };
            let f1 = finder(4.4, 4.4);
            let f2 = finder(14.2, 4.4);
            let f3 = finder(4.4, 14.2);
            let dots = [
                (12.2, 12.1),
                (15.3, 12.3),
                (18.5, 12.1),
                (12.1, 15.3),
                (15.5, 15.5),
                (18.4, 17.8),
                (13.0, 18.4),
            ]
            .into_iter()
            .fold(f32::INFINITY, |acc, (cx, cy)| {
                acc.min(sd_disc(px, py, cx, cy, 1.05))
            });
            f1.min(f2).min(f3).min(dots)
        }
        IconName::Annotate => {
            let c1 = sd_polyline(px, py, &[(5.0, 9.0), (5.0, 5.0), (9.0, 5.0)]);
            let c2 = sd_polyline(px, py, &[(15.0, 5.0), (19.0, 5.0), (19.0, 9.0)]);
            let c3 = sd_polyline(px, py, &[(19.0, 15.0), (19.0, 19.0), (15.0, 19.0)]);
            let c4 = sd_polyline(px, py, &[(9.0, 19.0), (5.0, 19.0), (5.0, 15.0)]);
            let pen = sd_polygon_stroke(
                px,
                py,
                &[(10.6, 14.4), (14.6, 10.4), (16.6, 12.4), (12.6, 16.4)],
            );
            c1.min(c2).min(c3).min(c4).min(pen) - h
        }
        IconName::Spotlight => {
            let mut d = sd_circle_stroke(px, py, 12.0, 12.0, 4.6);
            for i in 0..8 {
                let angle = (std::f32::consts::PI / 4.0) * i as f32;
                let (s, c) = angle.sin_cos();
                d = d.min(sd_segment(
                    px,
                    py,
                    12.0 + c * 6.6,
                    12.0 + s * 6.6,
                    12.0 + c * 9.2,
                    12.0 + s * 9.2,
                ));
            }
            d - h
        }
        IconName::Magnifier => {
            let ring = sd_circle_stroke(px, py, 10.2, 10.2, 5.6) - h;
            let handle = sd_segment(px, py, 14.3, 14.3, 19.2, 19.2) - (h + 0.15);
            ring.min(handle)
        }
        IconName::Bubble => {
            let body = sd_polyline(
                px,
                py,
                &[
                    (6.8, 16.2),
                    (6.6, 16.2),
                    (5.2, 15.6),
                    (4.6, 14.2),
                    (4.6, 7.6),
                    (5.2, 6.2),
                    (6.6, 5.6),
                    (17.4, 5.6),
                    (18.8, 6.2),
                    (19.4, 7.6),
                    (19.4, 14.2),
                    (18.8, 15.6),
                    (17.4, 16.2),
                    (10.6, 16.2),
                ],
            );
            let tail = sd_polyline(px, py, &[(6.8, 16.2), (5.8, 19.8), (10.6, 16.2)]);
            body.min(tail) - h
        }
        IconName::Sticker => {
            let outline = sd_polygon_stroke(
                px,
                py,
                &[
                    (4.8, 5.4),
                    (19.2, 5.4),
                    (19.2, 13.6),
                    (14.0, 18.8),
                    (4.8, 18.8),
                ],
            );
            let fold = sd_polyline(px, py, &[(19.2, 13.6), (14.0, 13.6), (14.0, 18.8)]);
            outline.min(fold) - h
        }
        IconName::Erase => {
            let body = sd_polygon_stroke(
                px,
                py,
                &[
                    (5.2, 14.8),
                    (13.0, 7.0),
                    (16.4, 7.0),
                    (19.8, 10.4),
                    (12.0, 18.2),
                    (8.6, 18.2),
                ],
            );
            let line = sd_segment(px, py, 8.4, 20.4, 19.6, 20.4);
            body.min(line) - h
        }
    }
}

/// 在 `size × size` 目标网格上直接用 4×4 子像素 + 距离场平滑生成抗锯齿覆盖度。
fn render_sdf_coverage(name: IconName, size: u32) -> Vec<u8> {
    let size = size.max(1);
    let scale = size as f32 / 24.0;
    // 24px 基准线宽 1.9px;小尺寸(<24)保证物理像素线宽 ≥ 1.65px,核心不断裂发虚。
    let stroke_24 = (1.9f32).max(1.65 / scale);
    let half_stroke = stroke_24 * 0.5;
    // 子像素过渡半宽(换算回 24 网格单位)。
    let aa_24 = 0.45 / scale;
    const SUB: u32 = 4;
    let inv_sub = 1.0 / (SUB as f32);
    let norm = 1.0 / ((SUB * SUB) as f32);
    let mut out = vec![0u8; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut acc = 0.0f32;
            for sy in 0..SUB {
                let py = (y as f32 + (sy as f32 + 0.5) * inv_sub) / scale;
                for sx in 0..SUB {
                    let px = (x as f32 + (sx as f32 + 0.5) * inv_sub) / scale;
                    let d = icon_sdf_24(name, px, py, half_stroke);
                    let c = ((aa_24 - d) / (2.0 * aa_24)).clamp(0.0, 1.0);
                    acc += c;
                }
            }
            out[(y * size + x) as usize] = (acc * norm * 255.0).round() as u8;
        }
    }
    out
}

/// 覆盖度位图上矩形区域 [x0,x1)×[y0,y1)(允许跨像素边界)的面积平均。
#[allow(dead_code)]
fn area_average(src: &[u8], tier: u32, x0: f32, y0: f32, x1: f32, y1: f32) -> u8 {
    let mut sum = 0.0f32;
    let mut weight = 0.0f32;
    let iy0 = y0.floor() as u32;
    let iy1 = y1.ceil() as u32;
    let ix0 = x0.floor() as u32;
    let ix1 = x1.ceil() as u32;
    for sy in iy0..iy1.min(tier) {
        let syf = sy as f32;
        let oy = ((syf + 1.0).min(y1) - syf).max(0.0) - (y0 - syf).max(0.0);
        let oy = oy.max(0.0);
        for sx in ix0..ix1.min(tier) {
            let sxf = sx as f32;
            let ox = ((sxf + 1.0).min(x1) - sxf).max(0.0) - (x0 - sxf).max(0.0);
            let ox = ox.max(0.0);
            let w = ox * oy;
            if w > 0.0 {
                sum += f32::from(src[(sy * tier + sx) as usize]) * w;
                weight += w;
            }
        }
    }
    if weight <= 0.0 {
        return src[((y0 as u32).min(tier - 1) * tier + (x0 as u32).min(tier - 1)) as usize];
    }
    (sum / weight).round() as u8
}

/// 以 (cx, cy) 为中心、外接盒边长 `size` 绘制动作图标(位图资产,
/// 按状态着墨色;普通深色 / 选中 accent / accent 填充按钮白色由调用方
/// 传 `ink`)。无资产的动作(如 `CopyColor`)不落笔。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    rgba: &mut [u8],
    w: u32,
    h: u32,
    action: SelectionAction,
    cx: i32,
    cy: i32,
    size: i32,
    ink: [u8; 4],
) {
    let Some(name) = IconName::for_action(action) else {
        return;
    };
    let size = crisp_icon_px(size.max(1) as u32);
    let glyph = name.tinted(size, ink);
    let half = size as i32 / 2;
    for dy in 0..size as i32 {
        for dx in 0..size as i32 {
            let i = (dy as u32 * size + dx as u32) as usize * 4;
            let color = [glyph[i], glyph[i + 1], glyph[i + 2], glyph[i + 3]];
            if color[3] == 0 {
                continue;
            }
            blend(rgba, w, h, cx - half + dx, cy - half + dy, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INK: [u8; 4] = [255, 255, 255, 255];

    /// 解码测试(icon-assets 的可加载性由本任务验证):26 个图标 ×
    /// 两档缩放,尺寸正确且确有笔画覆盖像素。
    #[test]
    fn all_toolbar_icons_decode_at_both_tiers_with_stroke_pixels() {
        for name in IconName::ALL {
            for tier in [24u8, 48] {
                let coverage = name.coverage(tier);
                assert_eq!(
                    coverage.len(),
                    u32::from(tier) as usize * u32::from(tier) as usize
                );
                let painted = coverage.iter().filter(|&&c| c > 0).count();
                assert!(
                    painted > tier as usize,
                    "{:?}@{tier} painted {painted}",
                    name
                );
            }
        }
    }

    /// 缩放档缓存:同一 (图标, 档) 复用同一份解码位图。
    #[test]
    fn coverage_cache_reuses_bitmap_per_tier() {
        let a = IconName::Rect.coverage(24).as_ptr();
        let b = IconName::Rect.coverage(24).as_ptr();
        assert_eq!(a, b);
    }

    /// 着色缓存:按 (图标, 尺寸, 墨色) 键控;换色或换尺寸即新条目。
    #[test]
    fn tinted_cache_keys_on_size_and_ink() {
        let a = IconName::Copy.tinted(18, INK).as_ptr();
        let b = IconName::Copy.tinted(18, INK).as_ptr();
        assert_eq!(a, b);
        let accent = [0x2D, 0xD4, 0xBF, 255];
        let c = IconName::Copy.tinted(18, accent).as_ptr();
        assert_ne!(a, c);
        let d = IconName::Copy.tinted(36, INK).as_ptr();
        assert_ne!(a, d);
    }

    #[test]
    fn crisp_icon_px_stays_on_authored_grid() {
        assert_eq!(crisp_icon_px(20), 24);
        assert_eq!(crisp_icon_px(24), 24);
        assert_eq!(crisp_icon_px(30), 30);
        assert_eq!(crisp_icon_px(36), 36);
        assert_eq!(crisp_icon_px(40), 40);
        assert_eq!(crisp_icon_px(47), 47);
        assert_eq!(crisp_icon_px(48), 48);
        assert_eq!(crisp_icon_px(96), 96);
    }

    /// 着色正确性:黑线稿 → 字形像素取墨色,覆盖度全透明处不落笔。
    #[test]
    fn tint_paints_ink_color_and_respects_coverage() {
        let size = 24u32;
        let glyph = IconName::Rect.tinted(size, [10, 200, 90, 255]);
        let mut painted = 0usize;
        for px in glyph.chunks_exact(4) {
            if px[3] == 0 {
                assert_eq!(px[0], 10);
                continue;
            }
            assert_eq!((px[0], px[1], px[2]), (10, 200, 90));
            painted += 1;
        }
        assert!(painted > size as usize);
        // 全透明外角:覆盖度位图本身在这些位置为 0。
        let corner = IconName::Rect.coverage(24)[0];
        assert_eq!(corner, 0, "icon corners are transparent");
    }

    /// 合成落笔:居中绘制在盒内产生非零像素;高 DPI 大尺寸(48px 档)同验。
    #[test]
    fn draw_paints_inside_its_box_at_both_tiers() {
        for size in [16i32, 36] {
            let (w, h) = (64u32, 64u32);
            let mut buf = vec![0u8; (w * h * 4) as usize];
            draw(&mut buf, w, h, SelectionAction::Save, 32, 32, size, INK);
            let painted = buf.chunks_exact(4).filter(|px| px[0] > 0).count();
            assert!(painted > 8, "size {size}: painted {painted}");
            assert!(
                painted <= (size * size) as usize,
                "ink must stay inside the {size}px box"
            );
        }
    }

    /// 下采样质量(盒式过滤):48→18 的细描边核心保持实色、不断裂发虚。
    #[test]
    fn downscale_keeps_stroke_cores_solid() {
        for name in [IconName::Rect, IconName::Line, IconName::Copy] {
            let glyph = name.tinted(18, INK);
            let max_cover = glyph.chunks_exact(4).map(|px| px[3]).max().unwrap_or(0);
            assert!(
                max_cover >= 200,
                "{name:?}@18 stroke core faded: {max_cover}"
            );
            let painted = glyph.chunks_exact(4).filter(|px| px[3] > 0).count();
            assert!(painted >= 30, "{name:?}@18 lost strokes: {painted}");
        }
        // 14px 菜单档同验(收缩最狠的目标尺寸)。
        let glyph = IconName::Rect.tinted(14, INK);
        let painted = glyph.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(painted >= 20, "rect@14 lost strokes: {painted}");
    }

    /// 每个会出现在操作条/菜单/标注工具条上的动作都有资产;键盘动作
    /// `CopyColor` 无资产(不落笔而不是 panic)。
    #[test]
    fn action_mapping_covers_rendered_actions() {
        for action in [
            SelectionAction::Copy,
            SelectionAction::Save,
            SelectionAction::Pin,
            SelectionAction::Ocr,
            SelectionAction::Qr,
            SelectionAction::Cancel,
            SelectionAction::Undo,
            SelectionAction::Redo,
            SelectionAction::Delete,
            SelectionAction::More,
            SelectionAction::Tool(AnnotationTool::Rect),
            SelectionAction::Tool(AnnotationTool::Ellipse),
            SelectionAction::Tool(AnnotationTool::Arrow),
            SelectionAction::Tool(AnnotationTool::Number),
            SelectionAction::Tool(AnnotationTool::Text),
            SelectionAction::Tool(AnnotationTool::Highlighter),
            SelectionAction::Tool(AnnotationTool::Mosaic),
            SelectionAction::Tool(AnnotationTool::Spotlight),
            SelectionAction::Tool(AnnotationTool::Magnifier),
            SelectionAction::Tool(AnnotationTool::Bubble),
            SelectionAction::Tool(AnnotationTool::Sticker),
            SelectionAction::Tool(AnnotationTool::Erase),
            SelectionAction::Mode(ToolMode::Arrow),
            SelectionAction::Mode(ToolMode::Line),
            SelectionAction::Mode(ToolMode::Highlighter),
            SelectionAction::Mode(ToolMode::Pen),
            SelectionAction::Mode(ToolMode::Mosaic),
            SelectionAction::Mode(ToolMode::Blur),
        ] {
            assert!(IconName::for_action(action).is_some(), "{action:?}");
        }
        assert_eq!(IconName::for_action(SelectionAction::CopyColor), None);
        // 无资产动作绘制是安全空操作。
        let (w, h) = (32u32, 32u32);
        let mut buf = vec![7u8; (w * h * 4) as usize];
        draw(&mut buf, w, h, SelectionAction::CopyColor, 16, 16, 18, INK);
        assert!(buf.chunks_exact(4).all(|px| px[0] == 7));
    }

    /// 全图标绘制烟测:26 个资产在目标尺寸下都能落笔(验收「彼此可分辨」
    /// 的底线:每个图标都有可见笔画)。
    #[test]
    fn every_icon_paints_pixels_at_toolbar_and_menu_sizes() {
        for name in IconName::ALL {
            let action = match name {
                IconName::Rect => SelectionAction::Tool(AnnotationTool::Rect),
                IconName::Ellipse => SelectionAction::Tool(AnnotationTool::Ellipse),
                IconName::Arrow => SelectionAction::Tool(AnnotationTool::Arrow),
                IconName::Text => SelectionAction::Tool(AnnotationTool::Text),
                IconName::Line => SelectionAction::Mode(ToolMode::Line),
                IconName::Number => SelectionAction::Tool(AnnotationTool::Number),
                IconName::Pen => SelectionAction::Mode(ToolMode::Pen),
                IconName::Highlighter => SelectionAction::Tool(AnnotationTool::Highlighter),
                IconName::Mosaic => SelectionAction::Tool(AnnotationTool::Mosaic),
                IconName::Blur => SelectionAction::Mode(ToolMode::Blur),
                IconName::Undo => SelectionAction::Undo,
                IconName::Redo => SelectionAction::Redo,
                IconName::Delete => SelectionAction::Delete,
                IconName::More => SelectionAction::More,
                IconName::Copy => SelectionAction::Copy,
                IconName::Save => SelectionAction::Save,
                IconName::Cancel => SelectionAction::Cancel,
                IconName::Pin => SelectionAction::Pin,
                IconName::Ocr => SelectionAction::Ocr,
                IconName::Qr => SelectionAction::Qr,
                IconName::Annotate => SelectionAction::Annotate,
                IconName::Spotlight => SelectionAction::Tool(AnnotationTool::Spotlight),
                IconName::Magnifier => SelectionAction::Tool(AnnotationTool::Magnifier),
                IconName::Bubble => SelectionAction::Tool(AnnotationTool::Bubble),
                IconName::Sticker => SelectionAction::Tool(AnnotationTool::Sticker),
                IconName::Erase => SelectionAction::Tool(AnnotationTool::Erase),
            };
            for size in [14i32, 18, 22] {
                let (w, h) = (48u32, 48u32);
                let mut buf = vec![0u8; (w * h * 4) as usize];
                draw(&mut buf, w, h, action, 24, 24, size, INK);
                let painted = buf.chunks_exact(4).filter(|px| px[0] > 0).count();
                assert!(painted > 4, "{name:?}@{size} painted {painted}");
            }
        }
    }

    /// 125%/150%/175% DPI(30/36/42px)下图标须保留平滑抗锯齿过渡且保持几何对称,
    /// 不得因非整数缩放退化为二值锯齿或偏心椭圆。
    #[test]
    fn fractional_dpi_icons_preserve_antialiasing_and_symmetry() {
        for size in [30u32, 36, 42] {
            for name in [
                IconName::Arrow,
                IconName::Ellipse,
                IconName::Highlighter,
                IconName::Undo,
                IconName::Redo,
                IconName::Cancel,
                IconName::More,
            ] {
                let glyph = name.tinted(size, INK);
                let smooth_edges = glyph
                    .chunks_exact(4)
                    .filter(|px| px[3] > 16 && px[3] < 240)
                    .count();
                let solid_cores = glyph.chunks_exact(4).filter(|px| px[3] >= 240).count();
                assert!(
                    smooth_edges >= 12,
                    "{name:?}@{size} lacks smooth anti-aliased edge pixels: {smooth_edges}"
                );
                assert!(
                    solid_cores >= 8,
                    "{name:?}@{size} lacks solid stroke core pixels: {solid_cores}"
                );
            }
            let ellipse = IconName::Ellipse.tinted(size, INK);
            for y in 0..size {
                for x in 0..size {
                    let a = ellipse[((y * size + x) * 4 + 3) as usize];
                    let mx = ellipse[((y * size + (size - 1 - x)) * 4 + 3) as usize];
                    let my = ellipse[(((size - 1 - y) * size + x) * 4 + 3) as usize];
                    assert_eq!(a, mx, "ellipse@{size} x-asymmetry at ({x},{y})");
                    assert_eq!(a, my, "ellipse@{size} y-asymmetry at ({x},{y})");
                }
            }
        }
    }
}
