//! 位图图标资产:内嵌 PNG(include_bytes!)+ image 解码,按缩放档
//! (24px/48px)缓存解码位图,并按 (图标, 目标尺寸, 着色) 缓存预合成
//! 的 RGBA 字形,绘制时经 `composer::blend()` 叠到帧上。
//!
//! 资产为黑线透明底单色稿:每个像素的覆盖度 = alpha × (1 − 亮度),
//! 着色即把 RGB 换成墨色、alpha 换成覆盖度,合成时自然抗锯齿。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::composer::blend;
use super::{AnnotationTool, SelectionAction};
use crate::capture::buffer::decode_rgba;

/// 20 个工具条/菜单图标的资产身份(R3:替代程序化 SDF 字形)。
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
}

/// 缓存句柄:泄漏的 'static 位图切片,进程级缓存条目。
type CachedBitmap = &'static [u8];
type CoverageCache = Mutex<HashMap<(IconName, u8), CachedBitmap>>;
type TintedCache = Mutex<HashMap<(IconName, u32, [u8; 4]), CachedBitmap>>;

impl IconName {
    pub const ALL: [Self; 20] = [
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

    /// 目标尺寸 + 墨色 → 预合成 RGBA 字形(尺寸 × 尺寸,按档缩放源位图)。
    fn tinted(self, size: u32, ink: [u8; 4]) -> &'static [u8] {
        fn render(name: IconName, size: u32, ink: [u8; 4]) -> Box<[u8]> {
            let tier = if size <= 32 { 24 } else { 48 };
            let src = name.coverage(tier);
            let tier = u32::from(tier);
            // 选定 24/48 档位源图,按最近邻采样缩放到目标尺寸。
            let mut out = vec![0u8; (size * size) as usize * 4];
            if size == tier {
                tint_into(src, &mut out, ink);
                return out.into_boxed_slice();
            }
            for y in 0..size {
                for x in 0..size {
                    let sx = ((x * tier + tier / 2) / size).min(tier - 1);
                    let sy = ((y * tier + tier / 2) / size).min(tier - 1);
                    let cover = src[(sy * tier + sx) as usize];
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
            SelectionAction::Tool(AnnotationTool::Line) => Self::Line,
            SelectionAction::Tool(AnnotationTool::Arrow) => Self::Arrow,
            SelectionAction::Tool(AnnotationTool::Number) => Self::Number,
            SelectionAction::Tool(AnnotationTool::Text) => Self::Text,
            SelectionAction::Tool(AnnotationTool::Pen) => Self::Pen,
            SelectionAction::Tool(AnnotationTool::Highlighter) => Self::Highlighter,
            SelectionAction::Tool(AnnotationTool::Mosaic) => Self::Mosaic,
            SelectionAction::Tool(AnnotationTool::Blur) => Self::Blur,
            SelectionAction::Undo => Self::Undo,
            SelectionAction::Redo => Self::Redo,
            SelectionAction::Delete => Self::Delete,
            SelectionAction::More => Self::More,
            SelectionAction::CopyColor => return None,
        })
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
    let size = size.max(1) as u32;
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

    /// 解码测试(icon-assets 的可加载性由本任务验证):20 个图标 ×
    /// 两档缩放,尺寸正确且确有笔画覆盖像素。
    #[test]
    fn all_twenty_icons_decode_at_both_tiers_with_stroke_pixels() {
        for name in IconName::ALL {
            for tier in [24u8, 48] {
                let coverage = name.coverage(tier);
                assert_eq!(coverage.len(), u32::from(tier) as usize * u32::from(tier) as usize);
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
            SelectionAction::Tool(AnnotationTool::Line),
            SelectionAction::Tool(AnnotationTool::Arrow),
            SelectionAction::Tool(AnnotationTool::Number),
            SelectionAction::Tool(AnnotationTool::Text),
            SelectionAction::Tool(AnnotationTool::Pen),
            SelectionAction::Tool(AnnotationTool::Highlighter),
            SelectionAction::Tool(AnnotationTool::Mosaic),
            SelectionAction::Tool(AnnotationTool::Blur),
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

    /// 全图标绘制烟测:20 个资产在目标尺寸下都能落笔(验收「彼此可分辨」
    /// 的底线:每个图标都有可见笔画)。
    #[test]
    fn every_icon_paints_pixels_at_toolbar_and_menu_sizes() {
        for name in IconName::ALL {
            let action = match name {
                IconName::Rect => SelectionAction::Tool(AnnotationTool::Rect),
                IconName::Ellipse => SelectionAction::Tool(AnnotationTool::Ellipse),
                IconName::Arrow => SelectionAction::Tool(AnnotationTool::Arrow),
                IconName::Text => SelectionAction::Tool(AnnotationTool::Text),
                IconName::Line => SelectionAction::Tool(AnnotationTool::Line),
                IconName::Number => SelectionAction::Tool(AnnotationTool::Number),
                IconName::Pen => SelectionAction::Tool(AnnotationTool::Pen),
                IconName::Highlighter => SelectionAction::Tool(AnnotationTool::Highlighter),
                IconName::Mosaic => SelectionAction::Tool(AnnotationTool::Mosaic),
                IconName::Blur => SelectionAction::Tool(AnnotationTool::Blur),
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
