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
    pub const ALL: [Self; 25] = [
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

    /// 目标尺寸 + 墨色 → 预合成 RGBA 字形。只在 24/48 原稿或 48 的整数倍上
    /// 着色;整数倍用最近邻复制像素,不把细描边做双线性放大。
    fn tinted(self, size: u32, ink: [u8; 4]) -> &'static [u8] {
        fn render(name: IconName, size: u32, ink: [u8; 4]) -> Box<[u8]> {
            let tier = if size <= 24 { 24 } else { 48 };
            let src = name.coverage(tier);
            let tier = u32::from(tier);
            let mut out = vec![0u8; (size * size) as usize * 4];
            if size == tier {
                tint_into(src, &mut out, ink);
                return out.into_boxed_slice();
            }
            if size > tier && size % tier == 0 {
                let factor = size / tier;
                for y in 0..size {
                    for x in 0..size {
                        let cover = src[((y / factor) * tier + x / factor) as usize];
                        let i = (y * size + x) as usize * 4;
                        out[i] = ink[0];
                        out[i + 1] = ink[1];
                        out[i + 2] = ink[2];
                        out[i + 3] = (u16::from(ink[3]) * u16::from(cover) / 255) as u8;
                    }
                }
                return out.into_boxed_slice();
            }
            if size < tier {
                // 非整数倍才做面积平均。调用方应先把尺寸钉到 24/48,避免走到这里。
                for y in 0..size {
                    let sy0 = (y * tier) as f32 / size as f32;
                    let sy1 = ((y + 1) * tier) as f32 / size as f32;
                    for x in 0..size {
                        let sx0 = (x * tier) as f32 / size as f32;
                        let sx1 = ((x + 1) * tier) as f32 / size as f32;
                        let cover = area_average(src, tier, sx0, sy0, sx1, sy1);
                        let i = (y * size + x) as usize * 4;
                        out[i] = ink[0];
                        out[i + 1] = ink[1];
                        out[i + 2] = ink[2];
                        out[i + 3] = (u16::from(ink[3]) * u16::from(cover) / 255) as u8;
                    }
                }
                return out.into_boxed_slice();
            }
            // 上采样(>48px,高倍缩放):双线性插值,避免块状锯齿。
            for y in 0..size {
                let sy = (y as f32 + 0.5) * tier as f32 / size as f32 - 0.5;
                let y0 = sy.max(0.0) as u32;
                let y1 = (y0 + 1).min(tier - 1);
                let fy = (sy - y0 as f32).clamp(0.0, 1.0);
                for x in 0..size {
                    let sx = (x as f32 + 0.5) * tier as f32 / size as f32 - 0.5;
                    let x0 = sx.max(0.0) as u32;
                    let x1 = (x0 + 1).min(tier - 1);
                    let fx = (sx - x0 as f32).clamp(0.0, 1.0);
                    let c00 = u16::from(src[(y0 * tier + x0) as usize]);
                    let c10 = u16::from(src[(y0 * tier + x1) as usize]);
                    let c01 = u16::from(src[(y1 * tier + x0) as usize]);
                    let c11 = u16::from(src[(y1 * tier + x1) as usize]);
                    let top = c00 as f32 * (1.0 - fx) + c10 as f32 * fx;
                    let bottom = c01 as f32 * (1.0 - fx) + c11 as f32 * fx;
                    let cover = (top * (1.0 - fy) + bottom * fy).round() as u8;
                    let i = (y * size + x) as usize * 4;
                    out[i] = ink[0];
                    out[i + 1] = ink[1];
                    out[i + 2] = ink[2];
                    out[i + 3] = (u16::from(ink[3]) * u16::from(cover) / 255) as u8;
                }
            }
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
            // 长截图字形由合成器几何笔画绘制,无位图资产。
            SelectionAction::LongCapture => return None,
            SelectionAction::CopyColor => return None,
        })
    }
}

/// 钉到原稿像素网格。24 与 48 是 1:1 资产;中间尺寸就近取原稿,
/// 不再把 16/20px 请求面积平均成细灰线。更大尺寸用 48 的整数倍。
pub(crate) fn crisp_icon_px(requested: u32) -> u32 {
    if requested >= 72 {
        48 * (requested / 48).max(2)
    } else if requested >= 36 {
        48
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

/// 覆盖度位图上矩形区域 [x0,x1)×[y0,y1)(允许跨像素边界)的面积平均。
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

    /// 解码测试(icon-assets 的可加载性由本任务验证):25 个图标 ×
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
        assert_eq!(crisp_icon_px(30), 24);
        assert_eq!(crisp_icon_px(40), 48);
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
            SelectionAction::Annotate,
            SelectionAction::Ocr,
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

    /// 全图标绘制烟测:25 个资产在目标尺寸下都能落笔(验收「彼此可分辨」
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
}
