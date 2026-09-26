//! 选区像素合成器。
//!
//! 在冻结帧(`capture::buffer::Frame`)的 RGBA 位图上合成:暗幕+选区开洞、
//! 跟随系统明暗的强调色描边、白芯蓝环手柄、尺寸徽标、放大镜(原始帧全分辨率采样 +
//! 十字准星 + 单行色值读数)、单条统一横条(主行工具/动作 + 「更多」展开
//! 面板)与右键菜单。浮层用冷灰表面和与选区描边同一的蓝,只有复制是实心强调。
//! 布局/hitbox 函数是纯几何,状态机与绘制共用
//! 同一份,保证命中判定与合成输出一致。不含窗口代码;系统明暗只决定选区描边。

use std::cell::RefCell;

use super::icons;
use super::text;
use super::{
    AnnotationOverlay, AnnotationTool, EdgeKind, FeatureFlags, HandleKind, Scene, SelectionAction,
    ToolMode,
};
use crate::annotate::raster;
use crate::capture::buffer::{validate_frame, Frame};
use crate::capture::error::CaptureError;
use crate::capture::geometry::PhysicalRect;
use crate::i18n;

// ---- 配色:冷灰浮层 + 与选区描边相同的蓝。只有一个实心主动作。----

/// 复制按钮、手柄环、放大镜准星。与浅色选区描边同一蓝,避免青绿和蓝两套强调色。
pub const ACCENT: [u8; 4] = [0x1D, 0x4E, 0xD8, 255];
/// 选区描边浅色,与网页 `--accent` 相同(#1d4ed8)。系统配色不可用时用此值。
const ACCENT_LIGHT: [u8; 4] = [0x1D, 0x4E, 0xD8, 255];
/// 选区描边深色,与网页暗色 `--accent` 相同(#93c5fd)。
const ACCENT_DARK: [u8; 4] = [0x93, 0xC5, 0xFD, 255];
/// 选区晕边浅色,与网页 `--focus-gap` 相同(#ffffff)。
const HALO_LIGHT: [u8; 4] = [255, 255, 255, 255];
/// 选区晕边深色,与网页暗色 `--focus-gap` 相同(#12161c)。
const HALO_DARK: [u8; 4] = [0x12, 0x16, 0x1C, 255];
/// 悬停文字与图标 #1e40af,比填充蓝更深,压在浅蓝底上仍清楚。
const ACCENT_DEEP: [u8; 4] = [0x1E, 0x40, 0xAF, 255];
/// 图标字形白色(accent 填充按钮上的纯图标)。
const ICON_INK: [u8; 4] = [255, 255, 255, 255];
/// 浮层底 #e4ebf6,不透明的浅蓝灰。纯白贴在截图上没有层次,半透明又会发灰。
const CHROME_BG: [u8; 4] = [0xE4, 0xEB, 0xF6, 255];
/// 正文 #1c2128。
const CHROME_TEXT: [u8; 4] = [0x1C, 0x21, 0x28, 255];
/// 1px 边 rgba(28,33,40,0.16),让面板在杂乱画面上仍有边。
const CHROME_BORDER: [u8; 4] = [28, 33, 40, 41];
/// 底部 2px 偏移,模拟浮层阴影。
const CHROME_SHADOW: [u8; 4] = [15, 23, 42, 72];
/// 悬停软底,约 14% 的蓝。
const ACTIVE_BG: [u8; 4] = [0x1D, 0x4E, 0xD8, 36];
/// 手柄白芯(亮暗背景均可见的双圆结构内芯)。
const HANDLE_CORE: [u8; 4] = [255, 255, 255, 255];
/// 暗幕保留 52% 亮度,对齐现 Windows 原生路径。
const DIM_KEEP: u16 = 52;

/// 手柄命中半径(物理像素,1.0 基准),比视觉环更宽容;随 scale 放大。
pub const HANDLE_HIT_RADIUS: i32 = 9;
/// 边缘拉伸命中带(物理像素,1.0 基准):选区边线 ±EDGE_HIT_RADIUS。
pub const EDGE_HIT_RADIUS: i32 = 6;
/// 手柄外环逻辑半径,× frame.scale,渲染半径保证 ≥5 物理 px。
const HANDLE_RADIUS: f32 = 5.0;

// 徽标不参与命中,尺寸随 frame.scale 等比放大(150% 下字号约 22.5 物理 px)。
const BADGE_FONT: f32 = 15.0;
const BADGE_PAD_X: i32 = 9;
const BADGE_PAD_Y: i32 = 4;
const BADGE_MARGIN: i32 = 6;

// ---- 统一横条、「更多」面板与右键菜单几何:与引擎 hitbox 共用;以下为
// 1.0 基准(逻辑)尺寸,实际物理尺寸统一经 `ChromeMetrics::for_scale` 派生。----
/// 统一横条按钮边长(触达标准 40px)。
const BAR_BUTTON: i32 = 40;
/// 统一横条与选区间的间距。
const BAR_MARGIN: i32 = 8;
/// 统一横条按钮图标外接盒。24 是原稿像素网格,缩小到 20 会把描边平均成灰边。
const BAR_ICON: i32 = 24;

const MENU_ITEM_W: i32 = 168;
const MENU_ITEM_H: i32 = 36;
/// 菜单容器内边距。
const MENU_PAD: i32 = 6;
/// 菜单项图标中心相对项左缘的偏移(24px 图标,左侧留 8px)。
const MENU_ICON_CX: i32 = 20;
/// 菜单项文字起点。24px 图标中心在 20,右缘在 32,再留 12px(Fluent 菜单图标与标签间距)。
const MENU_TEXT_X: i32 = 44;
/// 「取消」前的 1px 分隔线高度。
const MENU_SEPARATOR_H: i32 = 1;
const MENU_FONT: f32 = 16.0;
/// 菜单项 hover 软底圆角(1.0 基准)。
const MENU_HOVER_RADIUS: i32 = 8;
/// 菜单项图标外接盒。与横条相同,走 24px 原稿,避免缩成细灰线。
const MENU_ICON: i32 = 24;
/// 浮层圆角逻辑半径(× scale,钳制 8..=32)。
const PANEL_RADIUS: f32 = 10.0;

// 触达几何契约(1.0 基准):统一横条 40px 按钮;菜单项高 36、min-width 168
// (编译期断言)。实际 chrome 尺寸永不低于该基准(`ChromeMetrics` 钉死下限)。
const _: () = assert!(BAR_BUTTON >= 40 && MENU_ITEM_H >= 36 && MENU_ITEM_W >= 168);

/// chrome(菜单/操作条/徽标/手柄)的统一尺寸派生(ADR-15)。
///
/// `scale` 取冻结帧物理/逻辑比(Windows per-monitor v2、macOS backing scale、
/// X11 恒 1.0),钳制在 1.0–4.0:低于 1.0 保持 1.0 基准的物理下限,触达尺寸
/// 不缩水。布局、绘制与命中判定都经同一份 metrics 计算,保证所见即可点;
/// 高于 1.0 时所有尺寸/字号/图标/间距等比放大,文字与图标不因固定容器裁切。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeMetrics {
    /// 生效缩放(chrome 基准下限 1.0,上限 4.0)。
    pub scale: f32,
    /// 统一横条按钮边长。
    pub bar_button: i32,
    /// 统一横条与选区间距。
    pub bar_margin: i32,
    /// 统一横条按钮图标外接盒边长。
    pub bar_icon: i32,
    pub menu_item_w: i32,
    pub menu_item_h: i32,
    pub menu_pad: i32,
    pub menu_icon_cx: i32,
    pub menu_text_x: i32,
    pub menu_separator_h: i32,
    pub menu_hover_radius: i32,
    pub menu_font: f32,
    pub menu_icon: i32,
    /// 手柄视觉外环半径(≥5 物理 px)。
    pub handle_radius: i32,
    /// 手柄命中半径(≥现行 9 物理 px)。
    pub handle_hit_radius: i32,
    /// 边缘拉伸命中半径(≥现行 6 物理 px)。
    pub edge_hit_radius: i32,
    pub badge_font: f32,
    pub badge_pad_x: i32,
    pub badge_pad_y: i32,
    pub badge_margin: i32,
    pub panel_radius: i32,
}

impl ChromeMetrics {
    /// 由冻结帧 scale 派生全部 chrome 尺寸;非法值按 1.0,超出范围钳制。
    pub fn for_scale(scale: f32) -> Self {
        let scale = if scale.is_finite() {
            scale.clamp(1.0, 4.0)
        } else {
            1.0
        };
        // 1.0 基准值即物理下限:缩放只放大,四舍五入后不低于基准。
        let scaled = |logical: i32| ((logical as f32) * scale).round().max(logical as f32) as i32;
        Self {
            scale,
            bar_button: scaled(BAR_BUTTON),
            bar_margin: scaled(BAR_MARGIN),
            bar_icon: scaled(BAR_ICON),
            menu_item_w: scaled(MENU_ITEM_W),
            menu_item_h: scaled(MENU_ITEM_H),
            menu_pad: scaled(MENU_PAD),
            menu_icon_cx: scaled(MENU_ICON_CX),
            menu_text_x: scaled(MENU_TEXT_X),
            menu_separator_h: scaled(MENU_SEPARATOR_H),
            menu_hover_radius: scaled(MENU_HOVER_RADIUS),
            menu_font: (MENU_FONT * scale).round(),
            menu_icon: scaled(MENU_ICON),
            handle_radius: scaled(HANDLE_RADIUS as i32),
            handle_hit_radius: scaled(HANDLE_HIT_RADIUS),
            edge_hit_radius: scaled(EDGE_HIT_RADIUS),
            badge_font: (BADGE_FONT * scale).round(),
            badge_pad_x: scaled(BADGE_PAD_X),
            badge_pad_y: scaled(BADGE_PAD_Y),
            badge_margin: scaled(BADGE_MARGIN),
            panel_radius: ((PANEL_RADIUS * scale).round() as i32).clamp(8, 32),
        }
    }
}

/// 放大镜:源采样窗口 (2*MAG_HALF+1)=21 物理 px 直径,MAG_ZOOM=5 倍最近邻
/// 放大(有效倍率 ~5x,像素格清晰可辨),放大区边长 21×5=105 ≤ MAG_MAX_EDGE。
const MAG_HALF: i32 = 10;
pub const MAG_ZOOM: i32 = 5;
const MAG_MAX_EDGE: i32 = 140;
/// 放大镜面板宽度上限(物理 px);读数字号过宽时自动收缩以守住上限。
const MAG_PANEL_MAX: i32 = 200;
const MAG_FONT: f32 = 13.0;
const MAG_OFFSET: i32 = 18;
const MAG_PAD: i32 = 4;
/// 单行读数 pill 的最坏情况宽度样本(面板宽度据此预留,与实际像素无关)。
const MAG_READOUT_SAMPLE: &str = "#FFFFFF";

pub const ALL_HANDLES: [HandleKind; 8] = [
    HandleKind::NorthWest,
    HandleKind::North,
    HandleKind::NorthEast,
    HandleKind::East,
    HandleKind::SouthEast,
    HandleKind::South,
    HandleKind::SouthWest,
    HandleKind::West,
];
const CORNER_HANDLES: [HandleKind; 4] = [
    HandleKind::NorthWest,
    HandleKind::NorthEast,
    HandleKind::SouthEast,
    HandleKind::SouthWest,
];
const EDGE_HANDLES: [HandleKind; 4] = [
    HandleKind::North,
    HandleKind::East,
    HandleKind::South,
    HandleKind::West,
];

/// i32 屏幕矩形(布局与命中计算)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl IntRect {
    pub fn right(&self) -> i32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    pub fn is_empty(&self) -> bool {
        self.width <= 0 || self.height <= 0
    }

    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }

    pub fn intersect(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Self {
            x,
            y,
            width: (self.right().min(other.right()) - x).max(0),
            height: (self.bottom().min(other.bottom()) - y).max(0),
        }
    }

    pub fn inflate(self, by: i32) -> Self {
        Self {
            x: self.x - by,
            y: self.y - by,
            width: (self.width + by * 2).max(0),
            height: (self.height + by * 2).max(0),
        }
    }

    pub fn clamp_to(self, screen: (i32, i32)) -> Self {
        let x = self.x.clamp(0, screen.0.max(0));
        let y = self.y.clamp(0, screen.1.max(0));
        let right = self.right().clamp(x, screen.0.max(0));
        let bottom = self.bottom().clamp(y, screen.1.max(0));
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

impl From<PhysicalRect> for IntRect {
    fn from(value: PhysicalRect) -> Self {
        Self {
            x: value.x as i32,
            y: value.y as i32,
            width: value.width as i32,
            height: value.height as i32,
        }
    }
}

pub fn action_label(action: SelectionAction) -> String {
    i18n::t(match action {
        SelectionAction::Copy => "selection.action.copy",
        SelectionAction::Save => "selection.action.save",
        SelectionAction::Pin => "selection.action.pin",
        SelectionAction::Annotate => "selection.action.annotate",
        SelectionAction::Ocr => "selection.action.ocr",
        SelectionAction::Cancel => "selection.action.cancel",
        SelectionAction::CopyColor => "selection.action.copy_color",
        // 标注工具条按钮为纯图标;名称仍提供词条供辅助文本/后续提示复用。
        SelectionAction::Tool(tool) => match tool {
            AnnotationTool::Arrow => "selection.tool.arrow",
            AnnotationTool::Rect => "selection.tool.rect",
            AnnotationTool::Ellipse => "selection.tool.ellipse",
            AnnotationTool::Highlighter => "selection.tool.highlighter",
            AnnotationTool::Mosaic => "selection.tool.mosaic",
            AnnotationTool::Text => "selection.tool.text",
            AnnotationTool::Number => "selection.tool.number",
            AnnotationTool::Spotlight => "selection.tool.spotlight",
            AnnotationTool::Magnifier => "selection.tool.magnifier",
            AnnotationTool::Bubble => "selection.tool.bubble",
            AnnotationTool::Sticker => "selection.tool.sticker",
            AnnotationTool::Erase => "selection.tool.erase",
        },
        SelectionAction::Mode(mode) => mode.label_key(),
        SelectionAction::Undo => "selection.tool.undo",
        SelectionAction::Redo => "selection.tool.redo",
        SelectionAction::Delete => "selection.tool.delete",
        SelectionAction::More => "selection.tool.more",
        SelectionAction::LongCapture => "selection.action.long_capture",
    })
}

/// 右键菜单动作过滤(顺序固定,保持现状):
/// Copy←`toolbar_copy`, Save←`toolbar_save`, Pin←`toolbar_pin && pin_entry`,
/// Annotate 恒在, LongCapture←`long_capture`(R1), Ocr←`ocr_entry`, Cancel 恒在。
fn capture_actions(flags: FeatureFlags) -> Vec<SelectionAction> {
    let mut actions = Vec::with_capacity(7);
    if flags.toolbar_copy {
        actions.push(SelectionAction::Copy);
    }
    if flags.toolbar_save {
        actions.push(SelectionAction::Save);
    }
    if flags.toolbar_pin && flags.pin_entry {
        actions.push(SelectionAction::Pin);
    }
    actions.push(SelectionAction::Annotate);
    if flags.long_capture {
        actions.push(SelectionAction::LongCapture);
    }
    if flags.ocr_entry {
        actions.push(SelectionAction::Ocr);
    }
    actions.push(SelectionAction::Cancel);
    actions
}

/// 统一横条主行动作集(R5 注册表驱动,与预览编辑器工具条同源):
/// 即时标注开启时为注册表主行工具(开关允许 + 平台有文本输入通道时的文字)+
/// 撤销 + LongCapture←`long_capture`(R1) + Copy←`toolbar_copy` +
/// Save←`toolbar_save` + 取消 + 更多;关闭时为 标注 + LongCapture←
/// `long_capture` + Copy←`toolbar_copy` + Save←`toolbar_save` + 取消 + 更多。
/// 关闭复制/保存后主行不再含该项,其余顺序不变。
pub fn toolbar_buttons(flags: FeatureFlags, text_input: bool) -> Vec<SelectionAction> {
    let mut buttons = Vec::with_capacity(12);
    if flags.inline_annotation {
        for tool in AnnotationTool::PRIMARY {
            if !flags.tools.enabled(tool) {
                continue;
            }
            if tool != AnnotationTool::Text || text_input {
                buttons.push(SelectionAction::Tool(tool));
            }
        }
        buttons.push(SelectionAction::Undo);
    } else {
        buttons.push(SelectionAction::Annotate);
    }
    if flags.long_capture {
        buttons.push(SelectionAction::LongCapture);
    }
    if flags.toolbar_copy {
        buttons.push(SelectionAction::Copy);
    }
    if flags.toolbar_save {
        buttons.push(SelectionAction::Save);
    }
    buttons.push(SelectionAction::Cancel);
    buttons.push(SelectionAction::More);
    buttons
}

/// 「更多」面板动作集(R5 注册表驱动):即时标注开启时收进注册表更多工具
/// (开关允许时)+ 合并工具的全部模式入口(直线/画笔/模糊与各自默认模式,
/// 所属工具开启时)+ 重做 + 删除,以及开关允许的贴图/取字;关闭时只含
/// 贴图/取字。
pub fn more_panel_buttons(flags: FeatureFlags) -> Vec<SelectionAction> {
    let mut buttons = Vec::with_capacity(14);
    if flags.inline_annotation {
        for tool in AnnotationTool::MORE {
            if flags.tools.enabled(tool) {
                buttons.push(SelectionAction::Tool(tool));
            }
        }
        for mode in ToolMode::ALL {
            if flags.tools.enabled(mode.tool()) {
                buttons.push(SelectionAction::Mode(mode));
            }
        }
        buttons.push(SelectionAction::Redo);
        buttons.push(SelectionAction::Delete);
    }
    if flags.toolbar_pin && flags.pin_entry {
        buttons.push(SelectionAction::Pin);
    }
    if flags.ocr_entry {
        buttons.push(SelectionAction::Ocr);
    }
    buttons
}

/// 右键菜单动作集(保持现状,与统一横条无关)。
pub fn menu_items(flags: FeatureFlags) -> Vec<SelectionAction> {
    capture_actions(flags)
}

/// 两个面板是否相交。
fn intersects(a: IntRect, b: IntRect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

/// 统一横条布局:面板矩形 + 逐按钮矩形(命中与绘制共用同一份几何)。
#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedToolbar {
    pub panel: IntRect,
    pub buttons: Vec<(SelectionAction, IntRect)>,
}

/// 统一横条布局:恒为单行,取代右侧图标轨与第二阶段标注条。
///
/// 放置:候选须在选区外、屏幕内。下缘是强默认(人体工学:靠近拖选区结
/// 束位置的光标);仅下缘放不下时翻上缘;竖直都不行再按「剩余空间大者
/// 优先、平局保右侧」选水平侧;四面候选都与选区相交时选重叠最小者,
/// 最后兜底下方钳制屏内(Snipaste/ShareX/macOS 通行行为:不是哪边空间
/// 大就翻哪边,避免选区在屏幕中下部时工具栏频繁跳到远处上方)。
/// 返回 None 表示无按钮。
pub fn unified_toolbar(
    metrics: ChromeMetrics,
    selection: PhysicalRect,
    screen: (u32, u32),
    flags: FeatureFlags,
    text_input: bool,
) -> Option<UnifiedToolbar> {
    let buttons = toolbar_buttons(flags, text_input);
    if buttons.is_empty() {
        return None;
    }
    let count = buttons.len() as i32;
    let button = metrics.bar_button;
    let margin = metrics.bar_margin;
    let panel_w = count * button;
    let panel_h = button;

    let sel = IntRect::from(selection);
    let sw = screen.0 as i32;
    let sh = screen.1 as i32;
    let clamp_x = |x: i32| x.clamp(0, (sw - panel_w).max(0));
    let clamp_y = |y: i32| y.clamp(0, (sh - panel_h).max(0));
    let centered_x = || clamp_x(sel.x + (sel.width - panel_w) / 2);
    let centered_y = || clamp_y(sel.y + (sel.height - panel_h) / 2);
    let below = IntRect {
        x: centered_x(),
        y: clamp_y(sel.bottom() + margin),
        width: panel_w,
        height: panel_h,
    };
    let above = IntRect {
        x: centered_x(),
        y: clamp_y(sel.y - margin - panel_h),
        width: panel_w,
        height: panel_h,
    };
    let right = IntRect {
        x: clamp_x(sel.right() + margin),
        y: centered_y(),
        width: panel_w,
        height: panel_h,
    };
    let left = IntRect {
        x: clamp_x(sel.x - margin - panel_w),
        y: centered_y(),
        width: panel_w,
        height: panel_h,
    };
    let fits = |panel: IntRect| {
        panel.x >= 0 && panel.y >= 0 && panel.right() <= sw && panel.bottom() <= sh
    };
    let clear = |panel: IntRect| !intersects(panel, sel);
    // 交集面积:候选与选区的重叠,重叠最小者优先(尽量少遮挡选区内容)。
    let overlap_area = |panel: IntRect| -> i64 {
        let x = (panel.right().min(sel.right()) - panel.x.max(sel.x)).max(0);
        let y = (panel.bottom().min(sel.bottom()) - panel.y.max(sel.y)).max(0);
        i64::from(x) * i64::from(y)
    };
    // 候选在该侧方向上的剩余空间(选区边到屏幕边)。
    let free_right = sw - sel.right();
    let free_left = sel.x;
    let candidates = [below, above, right, left];
    let usable: Vec<(usize, IntRect)> = candidates
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, panel)| fits(*panel) && clear(*panel))
        .collect();
    // 人体工学:下缘是强默认(靠近拖选区结束位置的光标,阅读流下方);
    // 仅当下缘放不下时才翻上缘;竖直都不行再按剩余空间大者优先选水平侧
    // (Snipaste/ShareX/macOS 通行行为——不是哪边空间大都翻,避免选区在
    // 屏幕中下部时工具栏频繁跳到远处上方)。
    let panel = usable
        .iter()
        .find(|(index, _)| *index == 0)
        .or_else(|| usable.iter().find(|(index, _)| *index == 1))
        .or_else(|| {
            usable
                .iter()
                .filter(|(index, _)| *index >= 2)
                .max_by_key(|(index, _)| {
                    let free = if *index == 2 { free_right } else { free_left };
                    (free, -(*index as i64))
                })
        })
        .map(|(_, panel)| *panel)
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .filter(|panel| fits(*panel))
                .min_by_key(|panel| overlap_area(*panel))
        })
        .unwrap_or(below);

    let rects = buttons
        .iter()
        .enumerate()
        .map(|(index, action)| {
            (
                *action,
                IntRect {
                    x: panel.x + index as i32 * button,
                    y: panel.y,
                    width: button,
                    height: button,
                },
            )
        })
        .collect();
    Some(UnifiedToolbar {
        panel,
        buttons: rects,
    })
}

/// 「更多」面板矩形:垂直动作列表,底边对齐「更多」按钮底边、右缘对齐按钮
/// 右缘(向上向左展开),钳制在屏幕内。几何与右键菜单同构(menu_item 尺寸)。
/// `toolbar` 为横条面板矩形:顶边横条被钳到屏幕顶缘时,向上弹出的面板会与
/// 横条重叠;若下方放得下,把面板下移到横条之下避免遮挡(绘制与命中共用本
/// 几何)。放不下时保留屏内位置,由命中顺序(面板优先)保证可点。
/// 菜单行宽跟最长标签走,右侧只留内边距。固定 168px 会在「直线」这类短标签旁留出大片空白。
pub fn menu_row_width(metrics: ChromeMetrics, items: &[SelectionAction]) -> i32 {
    let mut text_w = 0i32;
    for action in items {
        let label = action_label(*action);
        let measured = text::measure_width(&label, metrics.menu_font)
            .unwrap_or(metrics.menu_font * label.chars().count() as f32);
        text_w = text_w.max(measured.ceil() as i32);
    }
    metrics.menu_text_x + text_w + metrics.menu_pad * 2
}

pub fn more_panel(
    metrics: ChromeMetrics,
    anchor: IntRect,
    toolbar: Option<IntRect>,
    screen: (u32, u32),
    items: &[SelectionAction],
) -> IntRect {
    let width = menu_row_width(metrics, items) + metrics.menu_pad * 2;
    let height = items.len() as i32 * metrics.menu_item_h + metrics.menu_pad * 2;
    let mut panel = IntRect {
        x: (anchor.right() - width).clamp(0, (screen.0 as i32 - width).max(0)),
        y: (anchor.y - height).clamp(0, (screen.1 as i32 - height).max(0)),
        width,
        height,
    };
    if let Some(toolbar) = toolbar {
        if intersects(panel, toolbar) && toolbar.bottom() + height <= screen.1 as i32 {
            panel.y = toolbar.bottom();
        }
    }
    panel
}

/// 「更多」面板逐动作矩形,顺序与 `items` 一致(与绘制共用,保证 hitbox 一致)。
pub fn more_item_rects(
    metrics: ChromeMetrics,
    panel: IntRect,
    items: &[SelectionAction],
) -> Vec<(SelectionAction, IntRect)> {
    items
        .iter()
        .enumerate()
        .map(|(index, action)| {
            (
                *action,
                IntRect {
                    x: panel.x + metrics.menu_pad,
                    y: panel.y + metrics.menu_pad + index as i32 * metrics.menu_item_h,
                    width: menu_row_width(metrics, items),
                    height: metrics.menu_item_h,
                },
            )
        })
        .collect()
}

pub fn menu_panel(
    metrics: ChromeMetrics,
    anchor: (i32, i32),
    screen: (u32, u32),
    items: &[SelectionAction],
) -> IntRect {
    let width = menu_row_width(metrics, items) + metrics.menu_pad * 2;
    // 「取消」与其余动作之间预留分隔线。
    let height =
        items.len() as i32 * metrics.menu_item_h + metrics.menu_pad * 2 + metrics.menu_separator_h;
    IntRect {
        x: anchor.0.clamp(0, (screen.0 as i32 - width).max(0)),
        y: anchor.1.clamp(0, (screen.1 as i32 - height).max(0)),
        width,
        height,
    }
}

pub fn menu_item_rects(
    metrics: ChromeMetrics,
    panel: IntRect,
    items: &[SelectionAction],
) -> Vec<(SelectionAction, IntRect)> {
    let last = items.len().saturating_sub(1);
    items
        .iter()
        .enumerate()
        .map(|(index, action)| {
            (
                *action,
                IntRect {
                    x: panel.x + metrics.menu_pad,
                    // 末项(取消)在分隔线之下,整体下移分隔线高度。
                    y: panel.y
                        + metrics.menu_pad
                        + index as i32 * metrics.menu_item_h
                        + if index == last {
                            metrics.menu_separator_h
                        } else {
                            0
                        },
                    width: menu_row_width(metrics, items),
                    height: metrics.menu_item_h,
                },
            )
        })
        .collect()
}

/// 「取消」分隔线的 y 坐标(菜单至少含取消项时才有意义)。
pub fn menu_separator_y(metrics: ChromeMetrics, panel: IntRect, items: &[SelectionAction]) -> i32 {
    panel.y + metrics.menu_pad + items.len().saturating_sub(1) as i32 * metrics.menu_item_h
}

/// 放大镜一次布局派生:采样窗口、放大块边长与各 chrome 尺寸(随 scale 等比)。
#[derive(Debug, Clone, Copy)]
struct MagLayout {
    /// 源采样半径(物理像素)。
    half: i32,
    /// 单个源像素放大后的块边长(物理像素)。
    block: i32,
    /// 放大区边长 = (2*half+1)*block,封顶 MAG_MAX_EDGE。
    edge: i32,
    pad: i32,
    gap: i32,
    pill_w: i32,
    pill_h: i32,
    pill_pad_x: i32,
    font: f32,
}

fn mag_layout(scale: f32) -> MagLayout {
    let scale = if scale.is_finite() && scale > 0.25 {
        scale.min(4.0)
    } else {
        1.0
    };
    // 像素级放大:块边长固定 MAG_ZOOM 物理 px(有效倍率 ~5x),源窗口 21px
    // 直径;块边长不随 DPI scale 放大,避免回弹到旧体量。
    let block = MAG_ZOOM;
    let half = MAG_HALF
        .min((MAG_MAX_EDGE / block).saturating_sub(1) / 2)
        .max(1);
    let edge = (half * 2 + 1) * block;
    let pad = ((MAG_PAD as f32) * scale).round().max(2.0) as i32;
    let gap = (4.0 * scale).round().max(2.0) as i32;
    let pill_pad_x = (8.0 * scale).round().max(4.0) as i32;
    // 读数字号随 scale 放大,但样本串超出面板宽度上限时自动收缩字号守住上限。
    let inner = (MAG_PANEL_MAX - pad * 2 - pill_pad_x * 2).max(16) as f32;
    let mut font = (MAG_FONT * scale).round().max(7.0);
    let measure = |f: f32| {
        text::measure_width(MAG_READOUT_SAMPLE, f)
            .unwrap_or(f * 0.62 * MAG_READOUT_SAMPLE.chars().count() as f32)
    };
    let sample_w = measure(font);
    if sample_w > inner && sample_w > 0.0 {
        font = (font * inner / sample_w).max(7.0).floor();
    }
    let line_h = text::line_height(font).ceil() as i32;
    let pill_h = line_h + 2 * (3.0 * scale).round().max(2.0) as i32;
    let text_w = measure(font);
    let pill_w = (text_w.ceil() as i32 + 2 * pill_pad_x).min(MAG_PANEL_MAX - pad * 2);
    MagLayout {
        half,
        block,
        edge,
        pad,
        gap,
        pill_w,
        pill_h,
        pill_pad_x,
        font,
    }
}

/// 放大镜面板矩形:光标右下角偏移,贴近屏幕边缘时翻转,整体钳制屏内。
/// 放大区边长 ≤ MAG_MAX_EDGE 物理 px;面板宽度 ≤ MAG_PANEL_MAX。
pub fn magnifier_rect(cursor: (i32, i32), screen: (u32, u32), scale: f32) -> IntRect {
    let layout = mag_layout(scale);
    let width = (layout.edge.max(layout.pill_w) + layout.pad * 2).min(MAG_PANEL_MAX);
    let height = layout.pad * 2 + layout.edge + layout.gap + layout.pill_h;
    let mut x = cursor.0 + MAG_OFFSET;
    if x + width > screen.0 as i32 {
        x = cursor.0 - width - MAG_OFFSET;
    }
    let mut y = cursor.1 + MAG_OFFSET;
    if y + height > screen.1 as i32 {
        y = cursor.1 - height - MAG_OFFSET;
    }
    IntRect {
        x: x.clamp(0, (screen.0 as i32 - width).max(0)),
        y: y.clamp(0, (screen.1 as i32 - height).max(0)),
        width,
        height,
    }
}

/// 放大镜面板命中(光标提示用;复用 `magnifier_rect` 布局,不重算)。
pub fn magnifier_hit(cursor: (i32, i32), screen: (u32, u32), scale: f32, x: i32, y: i32) -> bool {
    magnifier_rect(cursor, screen, scale).contains(x, y)
}

pub fn handle_anchor(rect: PhysicalRect, kind: HandleKind) -> (i32, i32) {
    let x0 = rect.x as i32;
    let y0 = rect.y as i32;
    let x1 = x0 + rect.width as i32 - 1;
    let y1 = y0 + rect.height as i32 - 1;
    match kind {
        HandleKind::NorthWest => (x0, y0),
        HandleKind::North => ((x0 + x1) / 2, y0),
        HandleKind::NorthEast => (x1, y0),
        HandleKind::East => (x1, (y0 + y1) / 2),
        HandleKind::SouthEast => (x1, y1),
        HandleKind::South => ((x0 + x1) / 2, y1),
        HandleKind::SouthWest => (x0, y1),
        HandleKind::West => (x0, (y0 + y1) / 2),
    }
}

/// 8 向手柄命中(命中半径经 metrics 随 scale 派生);角手柄优先于边手柄。
pub fn handle_hit(
    metrics: ChromeMetrics,
    rect: PhysicalRect,
    x: i32,
    y: i32,
) -> Option<HandleKind> {
    let radius = metrics.handle_hit_radius;
    for kind in CORNER_HANDLES {
        let (hx, hy) = handle_anchor(rect, kind);
        if (x - hx).abs() <= radius && (y - hy).abs() <= radius {
            return Some(kind);
        }
    }
    for kind in EDGE_HANDLES {
        let (hx, hy) = handle_anchor(rect, kind);
        if (x - hx).abs() <= radius && (y - hy).abs() <= radius {
            return Some(kind);
        }
    }
    None
}

/// 边缘拉伸命中:边线 ±edge_hit_radius 的整条边带(调用方保证手柄优先判定,
/// 因此角/边中手柄区域不会落到这里)。拖动=沿该边法向轴 resize。
pub fn edge_hit(metrics: ChromeMetrics, rect: PhysicalRect, x: i32, y: i32) -> Option<EdgeKind> {
    let radius = metrics.edge_hit_radius;
    let r = IntRect::from(rect);
    let x1 = r.right() - 1;
    let y1 = r.bottom() - 1;
    let in_x = x >= r.x - radius && x <= x1 + radius;
    let in_y = y >= r.y - radius && y <= y1 + radius;
    if (y - r.y).abs() <= radius && in_x {
        return Some(EdgeKind::North);
    }
    if (y - y1).abs() <= radius && in_x {
        return Some(EdgeKind::South);
    }
    if (x - r.x).abs() <= radius && in_y {
        return Some(EdgeKind::West);
    }
    if (x - x1).abs() <= radius && in_y {
        return Some(EdgeKind::East);
    }
    None
}

/// 尺寸徽标文本:「宽 × 高 · 左, 上」,例如 "123 × 45 · 10, 20"。
pub fn size_readout(rect: PhysicalRect) -> String {
    format!("{} × {} · {}, {}", rect.width, rect.height, rect.x, rect.y)
}

/// 光标物理坐标读数,例如 "10, 20"。
pub fn coordinate_readout(x: i32, y: i32) -> String {
    format!("{}, {}", x, y)
}

pub fn rgb_readout(pixel: [u8; 4]) -> String {
    format!("R {} G {} B {}", pixel[0], pixel[1], pixel[2])
}

pub fn hex_readout(pixel: [u8; 4]) -> String {
    format!("#{:02X}{:02X}{:02X}", pixel[0], pixel[1], pixel[2])
}

/// 放大镜读数只保留 HEX。RGB 与坐标和色值重复,小字号下又糊又占宽。
pub fn magnifier_readout_line(_cursor: (i32, i32), pixel: [u8; 4]) -> String {
    hex_readout(pixel)
}

/// 冻结帧像素采样(越界钳制到边缘,返回 RGBA)。
pub fn sample_pixel(frame: &Frame, x: i32, y: i32) -> [u8; 4] {
    let x = x.clamp(0, frame.width as i32 - 1) as u32;
    let y = y.clamp(0, frame.height as i32 - 1) as u32;
    let i = ((y * frame.width + x) * 4) as usize;
    let mut pixel = [0u8; 4];
    if i + 3 < frame.rgba.len() {
        pixel.copy_from_slice(&frame.rgba[i..i + 4]);
    }
    pixel
}

/// 已确认标注在选区内的烘焙结果缓存;revision + 选区矩形变化即失效。
struct AnnotationCache {
    revision: u64,
    selection: PhysicalRect,
    region: Vec<u8>,
}

/// CPU 合成器:持有冻结帧原件与预计算的暗幕帧;chrome 尺寸随 frame.scale 等比。
pub struct Composer {
    width: u32,
    height: u32,
    scale: f32,
    /// 全部 chrome 的物理尺寸(布局/绘制/命中同源,ADR-15)。
    metrics: ChromeMetrics,
    original: Vec<u8>,
    dimmed: Vec<u8>,
    /// 即时标注的选区烘焙缓存(逐笔重绘不重复应用已确认图元)。
    annotation_cache: RefCell<Option<AnnotationCache>>,
}

impl Composer {
    pub fn new(frame: &Frame) -> Result<Self, CaptureError> {
        validate_frame(frame)?;
        let mut dimmed = frame.rgba.clone();
        for px in dimmed.chunks_exact_mut(4) {
            px[0] = (px[0] as u16 * DIM_KEEP / 100) as u8;
            px[1] = (px[1] as u16 * DIM_KEEP / 100) as u8;
            px[2] = (px[2] as u16 * DIM_KEEP / 100) as u8;
        }
        let scale = if frame.scale.is_finite() && frame.scale > 0.25 {
            (frame.scale as f32).min(4.0)
        } else {
            1.0
        };
        Ok(Self {
            width: frame.width,
            height: frame.height,
            scale,
            metrics: ChromeMetrics::for_scale(scale),
            original: frame.rgba.clone(),
            dimmed,
            annotation_cache: RefCell::new(None),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 采样冻结帧原件(物理像素坐标,越界钳制)。
    pub fn sample(&self, x: i32, y: i32) -> [u8; 4] {
        let x = x.clamp(0, self.width as i32 - 1);
        let y = y.clamp(0, self.height as i32 - 1);
        let i = ((y as u32 * self.width + x as u32) * 4) as usize;
        let mut pixel = [0u8; 4];
        if i + 3 < self.original.len() {
            pixel.copy_from_slice(&self.original[i..i + 4]);
        }
        pixel
    }

    /// 合成完整一帧(长度等于冻结帧的 RGBA 缓冲)。
    pub fn compose(&self, scene: &Scene) -> Vec<u8> {
        let mut out = self.dimmed.clone();
        self.compose_into(scene, &mut out);
        out
    }

    /// 合成完整一帧并叠加即时标注层(R21):已确认图元 + 草稿 + 文本编辑 +
    /// 标注工具条。渲染复用 `annotate::raster` 的图元绘制,与最终
    /// `rasterize` 同几何/颜色,保证所见即所得。
    pub fn compose_with_overlay(&self, scene: &Scene, overlay: &AnnotationOverlay) -> Vec<u8> {
        let mut out = self.dimmed.clone();
        self.compose_into_with_overlay(scene, overlay, &mut out);
        out
    }

    /// 就地合成;`out` 长度必须与冻结帧一致(先整体写为暗幕)。
    /// 合成顺序:暗幕→开洞→标注内容→描边→手柄→徽标→统一横条/菜单→放大镜
    /// →hover 提示。
    pub fn compose_into(&self, scene: &Scene, out: &mut [u8]) {
        self.compose_into_inner(scene, None, out, None);
    }

    /// 叠加标注层版本的就地合成;标注层先于全部 chrome 绘制(见 `compose_into`)。
    pub fn compose_into_with_overlay(
        &self,
        scene: &Scene,
        overlay: &AnnotationOverlay,
        out: &mut [u8],
    ) {
        self.compose_into_inner(scene, Some(overlay), out, None);
    }

    /// 脏矩形合成:只从暗幕恢复 `prev` 与当前场景的视觉差集,再重绘 chrome。
    /// `out` 必须仍是上一帧合成结果(或首帧未初始化时 `prev=None` 走整帧)。
    /// 返回需要呈现的脏矩形(已钳制在屏内)。
    pub fn compose_into_dirty(
        &self,
        scene: &Scene,
        overlay: &AnnotationOverlay,
        out: &mut [u8],
        prev: Option<&Scene>,
    ) -> IntRect {
        let screen = (self.width as i32, self.height as i32);
        let annotating = overlay.draft.is_some() || overlay.text.is_some();
        let dirty = self.dirty_rect(scene, Some(overlay), prev, screen, annotating);
        let cursor_follow = !annotating
            && prev.is_some_and(|prev| {
                prev.selection == scene.selection
                    && prev.toolbar_visible == scene.toolbar_visible
                    && prev.menu_open == scene.menu_open
                    && prev.menu_anchor == scene.menu_anchor
                    && prev.more_open == scene.more_open
                    && prev.flags == scene.flags
            });
        let mut dirty = dirty;
        if cursor_follow {
            let chrome = self.scene_visual_bounds(scene, Some(overlay), screen);
            if !chrome.intersect(dirty).is_empty() {
                dirty = dirty.union(chrome).inflate(2).clamp_to(screen);
            }
            restore_rect(out, &self.dimmed, self.width as usize, dirty);
            if let Some(selection) = scene.selection {
                punch_hole(
                    out,
                    &self.original,
                    self.width as usize,
                    selection,
                    Some(dirty),
                );
                // 光标跟随也会开洞/恢复暗幕;不重贴标注层就会把刚画的矩形擦掉,
                // 最终复制仍走完整合成所以导出图上有、屏幕上一闪而过。
                self.draw_annotations(out, selection, overlay);
                if !chrome.intersect(dirty).is_empty() {
                    outline_selection(out, self.width, self.height, selection);
                    self.draw_handles(out, self.width, self.height, selection);
                    self.draw_size_badge(out, self.width, self.height, selection);
                    if scene.toolbar_visible {
                        self.draw_unified_toolbar(
                            out,
                            self.width,
                            self.height,
                            selection,
                            scene,
                            Some(overlay),
                        );
                    }
                }
            }
            if scene.menu_open {
                self.draw_menu(out, self.width, self.height, scene);
            }
            if scene.flags.magnifier {
                self.draw_magnifier(out, self.width, self.height, scene.cursor);
            }
            if !scene.more_open {
                if let Some((action, rect)) =
                    self.hovered_icon(scene, Some(overlay), self.width, self.height)
                {
                    self.draw_hover_tooltip(out, self.width, self.height, action, rect);
                }
            }
        } else {
            self.compose_into_inner(scene, Some(overlay), out, Some(dirty));
        }
        dirty
    }

    /// 合成实现:标注是选区**内容**,在开洞后、全部 chrome 之前绘制——
    /// 整块选区重贴(原件 + 标注)不得擦掉描边/手柄/徽标/横条/菜单/放大镜
    /// (ADR-14:所见即可点)。其余 chrome 的相对次序保持不变。
    fn compose_into_inner(
        &self,
        scene: &Scene,
        overlay: Option<&AnnotationOverlay>,
        out: &mut [u8],
        dirty: Option<IntRect>,
    ) {
        let (w, h) = (self.width, self.height);
        if let Some(dirty) = dirty {
            restore_rect(out, &self.dimmed, w as usize, dirty);
        } else {
            out.copy_from_slice(&self.dimmed);
        }
        if let Some(selection) = scene.selection {
            punch_hole(out, &self.original, w as usize, selection, dirty);
            if let Some(overlay) = overlay {
                self.draw_annotations(out, selection, overlay);
            }
            outline_selection(out, w, h, selection);
            self.draw_handles(out, w, h, selection);
            self.draw_size_badge(out, w, h, selection);
            if scene.toolbar_visible {
                self.draw_unified_toolbar(out, w, h, selection, scene, overlay);
            }
        }
        if scene.menu_open {
            self.draw_menu(out, w, h, scene);
        }
        if scene.flags.magnifier {
            self.draw_magnifier(out, w, h, scene.cursor);
        }
        if !scene.more_open {
            if let Some((action, rect)) = self.hovered_icon(scene, overlay, w, h) {
                self.draw_hover_tooltip(out, w, h, action, rect);
            }
        }
    }

    /// 两帧之间需要重绘的屏内矩形。选区几何未变时只含放大镜与 hover 提示。
    fn dirty_rect(
        &self,
        scene: &Scene,
        overlay: Option<&AnnotationOverlay>,
        prev: Option<&Scene>,
        screen: (i32, i32),
        annotating: bool,
    ) -> IntRect {
        let full = IntRect {
            x: 0,
            y: 0,
            width: screen.0,
            height: screen.1,
        };
        let Some(prev) = prev else {
            return full;
        };
        let cursor_only = !annotating
            && prev.selection == scene.selection
            && prev.toolbar_visible == scene.toolbar_visible
            && prev.menu_open == scene.menu_open
            && prev.menu_anchor == scene.menu_anchor
            && prev.more_open == scene.more_open
            && prev.flags == scene.flags;
        let mut dirty = if cursor_only {
            IntRect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            }
        } else {
            self.scene_visual_bounds(prev, overlay, screen)
                .union(self.scene_visual_bounds(scene, overlay, screen))
        };
        if scene.flags.magnifier || prev.flags.magnifier {
            dirty = dirty
                .union(magnifier_rect(
                    prev.cursor,
                    (self.width, self.height),
                    self.scale,
                ))
                .union(magnifier_rect(
                    scene.cursor,
                    (self.width, self.height),
                    self.scale,
                ));
        }
        dirty = dirty
            .union(self.hover_bounds(prev, overlay, screen))
            .union(self.hover_bounds(scene, overlay, screen));
        if dirty.is_empty() {
            return full;
        }
        dirty.inflate(2).clamp_to(screen)
    }

    fn scene_visual_bounds(
        &self,
        scene: &Scene,
        overlay: Option<&AnnotationOverlay>,
        screen: (i32, i32),
    ) -> IntRect {
        let mut bounds = IntRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        if let Some(selection) = scene.selection {
            let pad = self
                .metrics
                .handle_hit_radius
                .max(self.metrics.bar_button + self.metrics.bar_margin)
                .max(self.metrics.badge_margin + 36);
            bounds = bounds.union(IntRect::from(selection).inflate(pad));
            if scene.toolbar_visible {
                if let Some(toolbar) = unified_toolbar(
                    self.metrics,
                    selection,
                    (self.width, self.height),
                    scene.flags,
                    overlay.map(|overlay| overlay.text_input).unwrap_or(false),
                ) {
                    bounds = bounds.union(toolbar.panel);
                    if scene.more_open {
                        let items = more_panel_buttons(scene.flags);
                        if !items.is_empty() {
                            let panel = more_panel(
                                self.metrics,
                                toolbar.buttons.last().expect("more button").1,
                                Some(toolbar.panel),
                                (self.width, self.height),
                                &items,
                            );
                            bounds = bounds.union(panel);
                        }
                    }
                }
            }
        }
        if scene.menu_open {
            let items = menu_items(scene.flags);
            bounds = bounds.union(menu_panel(
                self.metrics,
                scene.menu_anchor,
                (self.width, self.height),
                &items,
            ));
        }
        bounds.clamp_to(screen)
    }

    fn hover_bounds(
        &self,
        scene: &Scene,
        overlay: Option<&AnnotationOverlay>,
        screen: (i32, i32),
    ) -> IntRect {
        let empty = IntRect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        let Some((_, anchor)) = self.hovered_icon(scene, overlay, self.width, self.height) else {
            return empty;
        };
        anchor.inflate(80).clamp_to(screen)
    }

    /// 标注层:在选区内烘焙已确认图元(带缓存)、叠加草稿与文本编辑态,
    /// 再整体贴回合成缓冲。区内像素取自冻结帧原件,与最终裁剪裁剪一致。
    fn draw_annotations(
        &self,
        rgba: &mut [u8],
        selection: PhysicalRect,
        overlay: &AnnotationOverlay,
    ) {
        if selection.width == 0 || selection.height == 0 {
            return;
        }
        if overlay.annotations.is_empty() && overlay.draft.is_none() && overlay.text.is_none() {
            return;
        }
        let mut region = self.baked_region(selection, overlay);
        if let Some(draft) = overlay.draft {
            let local =
                crate::annotate::translated(draft, -(selection.x as f64), -(selection.y as f64));
            let _ = raster::apply_annotation(
                &mut region,
                selection.width,
                selection.height,
                f64::from(self.scale),
                &local,
            );
        }
        if let Some(edit) = overlay.text {
            self.draw_text_edit(&mut region, selection, overlay, edit);
        }
        self.blit_region(rgba, selection, &region);
    }

    /// 选区区域的已确认图元烘焙:revision + 选区矩形不变时复用缓存。
    fn baked_region(&self, selection: PhysicalRect, overlay: &AnnotationOverlay) -> Vec<u8> {
        {
            let cache = self.annotation_cache.borrow();
            if let Some(cache) = cache.as_ref() {
                if cache.revision == overlay.revision && cache.selection == selection {
                    return cache.region.clone();
                }
            }
        }
        let mut region = self.copy_selection(selection);
        if !overlay.annotations.is_empty() {
            let translated = crate::annotate::translated_all(
                overlay.annotations,
                -(selection.x as f64),
                -(selection.y as f64),
            );
            let _ = raster::apply_annotations(
                &mut region,
                selection.width,
                selection.height,
                f64::from(self.scale),
                &translated,
            );
        }
        *self.annotation_cache.borrow_mut() = Some(AnnotationCache {
            revision: overlay.revision,
            selection,
            region: region.clone(),
        });
        region
    }

    /// 从冻结帧原件取出选区像素。
    fn copy_selection(&self, selection: PhysicalRect) -> Vec<u8> {
        let row_bytes = selection.width as usize * 4;
        let stride = self.width as usize * 4;
        let mut region = Vec::with_capacity(row_bytes * selection.height as usize);
        for row in 0..selection.height as usize {
            let start = (selection.y as usize + row) * stride + selection.x as usize * 4;
            region.extend_from_slice(&self.original[start..start + row_bytes]);
        }
        region
    }

    fn blit_region(&self, rgba: &mut [u8], selection: PhysicalRect, region: &[u8]) {
        let row_bytes = selection.width as usize * 4;
        let stride = self.width as usize * 4;
        for row in 0..selection.height as usize {
            let src = row * row_bytes;
            let dst = (selection.y as usize + row) * stride + selection.x as usize * 4;
            if dst + row_bytes <= rgba.len() && src + row_bytes <= region.len() {
                rgba[dst..dst + row_bytes].copy_from_slice(&region[src..src + row_bytes]);
            }
        }
    }

    /// 文本编辑态:已提交文本按标注色、组合串按强调色,末尾画 2px 光标。
    fn draw_text_edit(
        &self,
        region: &mut [u8],
        selection: PhysicalRect,
        overlay: &AnnotationOverlay,
        edit: &super::TextEdit,
    ) {
        let x = edit.x as f32 - selection.x as f32;
        let y = edit.y as f32 - selection.y as f32;
        let size = overlay.text_size;
        let committed = edit.text.clone();
        text::draw_text(
            region,
            selection.width,
            selection.height,
            x,
            y,
            &committed,
            size,
            overlay.color,
        );
        if !edit.preedit.is_empty() {
            let committed_w = text::measure_width(&committed, size).unwrap_or(0.0);
            text::draw_text(
                region,
                selection.width,
                selection.height,
                x + committed_w,
                y,
                &edit.preedit,
                size,
                ACCENT_DEEP,
            );
        }
        let width = text::measure_width(&edit.display(), size).unwrap_or(0.0);
        let caret_x = (x + width).round() as i32;
        // 光标贴合参考字墨迹框,保持与字面同高,不按整行行框绘制。
        let (caret_top, caret_height) = text::caret_span(size);
        let caret_start = (y + caret_top).round() as i32;
        let caret_h = caret_height.round().max(1.0) as i32;
        for dy in 0..caret_h {
            put(
                region,
                selection.width,
                selection.height,
                caret_x,
                caret_start + dy,
                ACCENT_DEEP,
            );
        }
    }

    /// 统一横条:浅蓝灰面板 + 位图图标。仅复制为蓝色填充(白色图标);
    /// 当前绘制工具有独立的 accent 选中态;hover 软底;工具组与动作组之间画
    /// 1px 分隔线。「更多」展开时在其按钮上方弹出动作列表面板(图标+名称,
    /// 与右键菜单同构)。
    fn draw_unified_toolbar(
        &self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        selection: PhysicalRect,
        scene: &Scene,
        overlay: Option<&AnnotationOverlay>,
    ) {
        let metrics = self.metrics;
        let text_input = overlay.map(|overlay| overlay.text_input).unwrap_or(false);
        let Some(toolbar) = unified_toolbar(metrics, selection, (w, h), scene.flags, text_input)
        else {
            return;
        };
        draw_panel_chrome(rgba, w, h, toolbar.panel, metrics.panel_radius);
        // 工具组与动作组之间的分隔线位置(第一个非工具动作)。
        let sep_index = toolbar
            .buttons
            .iter()
            .position(|(action, _)| !matches!(action, SelectionAction::Tool(_)))
            .filter(|index| *index > 0);
        let radius = (metrics.bar_button / 2 - 6).max(6);
        for (index, (action, rect)) in toolbar.buttons.iter().enumerate() {
            let (cx, cy) = rect.center();
            let hover = rect.contains(scene.cursor.0, scene.cursor.1);
            let selected_tool = match (action, overlay) {
                (SelectionAction::Tool(tool), Some(overlay)) => overlay.tool == Some(*tool),
                // 模式入口:所属工具选中且当前模式一致时高亮。
                (SelectionAction::Mode(mode), Some(overlay)) => {
                    overlay.tool == Some(mode.tool()) && overlay.mode == Some(*mode)
                }
                (_, _) => false,
            };
            // 复制与其他按钮同一墨色。实心圆和短杠在像素网格上都会显得突兀。
            let ink = if selected_tool { ACCENT } else { CHROME_TEXT };
            if selected_tool || hover {
                fill_round_blend(rgba, w, h, inset(*rect, 6), radius, ACTIVE_BG);
            }
            self.draw_action_icon(rgba, w, h, *action, cx, cy, metrics.bar_icon, ink);
            if sep_index == Some(index) {
                for y in rect.y + rect.height / 4..rect.bottom() - rect.height / 4 {
                    blend(rgba, w, h, rect.x, y, CHROME_BORDER);
                }
            }
        }
        // 「更多」展开:动作列表面板(向上展开,底边对齐「更多」按钮)。
        if !scene.more_open {
            return;
        }
        let items = more_panel_buttons(scene.flags);
        let Some((_, more_rect)) = toolbar.buttons.last() else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let panel = more_panel(metrics, *more_rect, Some(toolbar.panel), (w, h), &items);
        draw_panel_chrome(rgba, w, h, panel, metrics.panel_radius);
        for (action, rect) in more_item_rects(metrics, panel, &items) {
            let hover = rect.contains(scene.cursor.0, scene.cursor.1);
            if hover {
                fill_round_blend(rgba, w, h, rect, metrics.menu_hover_radius, ACTIVE_BG);
            }
            let color = if hover { ACCENT_DEEP } else { CHROME_TEXT };
            let (_, cy) = rect.center();
            self.draw_action_icon(
                rgba,
                w,
                h,
                action,
                rect.x + metrics.menu_icon_cx,
                cy,
                metrics.menu_icon,
                color,
            );
            text::draw_text_bold(
                rgba,
                w,
                h,
                (rect.x + metrics.menu_text_x) as f32,
                text::y_for_center(cy as f32, metrics.menu_font),
                &action_label(action),
                metrics.menu_font,
                color,
            );
        }
    }

    /// 白芯 + 蓝环双圆手柄,亮暗背景均可见;外环半径随 scale 派生。
    fn draw_handles(&self, rgba: &mut [u8], w: u32, h: u32, rect: PhysicalRect) {
        let outer = self.metrics.handle_radius;
        let core = (outer - 2).max(2);
        for kind in ALL_HANDLES {
            let (cx, cy) = handle_anchor(rect, kind);
            fill_circle(rgba, w, h, cx, cy, outer, ACCENT);
            fill_circle(rgba, w, h, cx, cy, core, HANDLE_CORE);
        }
    }

    /// 亮底 pill 深字加粗徽标,位于选区上方(上方放不下时翻到下方)。
    fn draw_size_badge(&self, rgba: &mut [u8], w: u32, h: u32, rect: PhysicalRect) {
        let metrics = self.metrics;
        let font = metrics.badge_font;
        let label = size_readout(rect);
        let Some(text_width) = text::measure_width(&label, font) else {
            return;
        };
        let pad_x = metrics.badge_pad_x;
        let pad_y = metrics.badge_pad_y;
        let margin = metrics.badge_margin;
        // faux bold 二次描画会向右多占约 5% 字号宽度,预留。
        let bold_slack = (font * 0.05).ceil() as i32;
        let width = text_width.ceil() as i32 + bold_slack + pad_x * 2;
        let height = text::line_height(font).ceil() as i32 + pad_y * 2;
        let sel = IntRect::from(rect);
        let mut y = sel.y - height - margin;
        if y < 0 {
            y = sel.bottom() + margin;
        }
        let panel = IntRect {
            x: sel.x.clamp(0, (w as i32 - width).max(0)),
            y: y.clamp(0, (h as i32 - height).max(0)),
            width,
            height,
        };
        draw_panel_chrome(rgba, w, h, panel, height / 2);
        text::draw_text_bold(
            rgba,
            w,
            h,
            (panel.x + pad_x) as f32,
            (panel.y + pad_y) as f32,
            &label,
            font,
            CHROME_TEXT,
        );
    }

    /// 动作图标:长截图没有位图资产,用几何笔画(竖框 + 向下箭头)绘制;
    /// 其余动作仍走 `icons` 资产。
    #[allow(clippy::too_many_arguments)]
    fn draw_action_icon(
        &self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        action: SelectionAction,
        cx: i32,
        cy: i32,
        size: i32,
        ink: [u8; 4],
    ) {
        if action == SelectionAction::LongCapture {
            draw_long_capture_icon(rgba, w, h, cx, cy, size, ink);
            return;
        }
        icons::draw(rgba, w, h, action, cx, cy, size, ink);
    }

    /// 统一横条与「更多」面板的悬停图标(提示名称用);菜单打开时让位。
    fn hovered_icon(
        &self,
        scene: &Scene,
        overlay: Option<&AnnotationOverlay>,
        w: u32,
        h: u32,
    ) -> Option<(SelectionAction, IntRect)> {
        if scene.menu_open {
            return None;
        }
        if !scene.toolbar_visible {
            return None;
        }
        let selection = scene.selection?;
        let text_input = overlay.map(|overlay| overlay.text_input).unwrap_or(false);
        let toolbar = unified_toolbar(self.metrics, selection, (w, h), scene.flags, text_input)?;
        if scene.more_open {
            let items = more_panel_buttons(scene.flags);
            if !items.is_empty() {
                let panel = more_panel(
                    self.metrics,
                    toolbar.buttons.last().expect("more button").1,
                    Some(toolbar.panel),
                    (w, h),
                    &items,
                );
                if let Some(hit) = more_item_rects(self.metrics, panel, &items)
                    .into_iter()
                    .find(|(_, rect)| rect.contains(scene.cursor.0, scene.cursor.1))
                {
                    return Some(hit);
                }
            }
        }
        toolbar
            .buttons
            .into_iter()
            .find(|(_, rect)| rect.contains(scene.cursor.0, scene.cursor.1))
    }

    fn draw_hover_tooltip(
        &self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        action: SelectionAction,
        anchor: IntRect,
    ) {
        let metrics = self.metrics;
        let label = action_label(action);
        let font = metrics.menu_font;
        let Some(text_width) = text::measure_width(&label, font) else {
            return;
        };
        let pad_x = metrics.badge_pad_x.max(8);
        let pad_y = metrics.badge_pad_y.max(4);
        let gap = metrics.badge_margin.max(6);
        let width = (text_width.ceil() as i32 + pad_x * 2).max(24);
        let height = text::line_height(font).ceil() as i32 + pad_y * 2;
        let Some(panel) = place_tooltip(anchor, width, height, gap, (w as i32, h as i32)) else {
            return;
        };
        draw_panel_chrome(rgba, w, h, panel, height / 2);
        text::draw_text(
            rgba,
            w,
            h,
            (panel.x + pad_x) as f32,
            text::y_for_center(panel.center().1 as f32, font),
            &label,
            font,
            CHROME_TEXT,
        );
    }

    /// 右键菜单:左图标 + 右文字;悬停项用蓝色软底和更深的蓝字。
    /// 「取消」与其余动作之间画分隔线(14% 墨)。全部几何经 metrics 派生。
    fn draw_menu(&self, rgba: &mut [u8], w: u32, h: u32, scene: &Scene) {
        let metrics = self.metrics;
        let items = menu_items(scene.flags);
        let panel = menu_panel(metrics, scene.menu_anchor, (w, h), &items);
        draw_panel_chrome(rgba, w, h, panel, metrics.panel_radius);
        if items.len() > 1 {
            let sep_y = menu_separator_y(metrics, panel, &items);
            for x in panel.x + metrics.menu_pad..panel.right() - metrics.menu_pad {
                blend(rgba, w, h, x, sep_y, CHROME_BORDER);
            }
        }
        for (action, rect) in menu_item_rects(metrics, panel, &items) {
            let hover = rect.contains(scene.cursor.0, scene.cursor.1);
            if hover {
                fill_round_blend(rgba, w, h, rect, metrics.menu_hover_radius, ACTIVE_BG);
            }
            let color = if hover { ACCENT_DEEP } else { CHROME_TEXT };
            let (_, cy) = rect.center();
            self.draw_action_icon(
                rgba,
                w,
                h,
                action,
                rect.x + metrics.menu_icon_cx,
                cy,
                metrics.menu_icon,
                color,
            );
            let label = action_label(action);
            text::draw_text_bold(
                rgba,
                w,
                h,
                (rect.x + metrics.menu_text_x) as f32,
                text::y_for_center(cy as f32, metrics.menu_font),
                &label,
                metrics.menu_font,
                color,
            );
        }
    }

    /// 放大镜:最近邻放大,蓝框只标光标那一格,不再铺网格。
    /// 底部是色块加 HEX,不再重复 RGB 和坐标。
    fn draw_magnifier(&self, rgba: &mut [u8], w: u32, h: u32, cursor: (i32, i32)) {
        let layout = mag_layout(self.scale);
        let panel = magnifier_rect(cursor, (w, h), self.scale);
        draw_panel_chrome(rgba, w, h, panel, self.metrics.panel_radius);
        let px = panel.x + (panel.width - layout.edge) / 2;
        let py = panel.y + layout.pad;
        let block = layout.block;
        for dy in 0..layout.edge {
            for dx in 0..layout.edge {
                let src = self.sample(
                    cursor.0 - layout.half + dx / block,
                    cursor.1 - layout.half + dy / block,
                );
                let [r, g, b, _] = src;
                put(rgba, w, h, px + dx, py + dy, [r, g, b, 255]);
            }
        }
        let ox = px + layout.half * block;
        let oy = py + layout.half * block;
        for i in 0..block {
            put(rgba, w, h, ox + i, oy, ACCENT);
            put(rgba, w, h, ox + i, oy + block - 1, ACCENT);
            put(rgba, w, h, ox, oy + i, ACCENT);
            put(rgba, w, h, ox + block - 1, oy + i, ACCENT);
        }
        let center = self.sample(cursor.0, cursor.1);
        let line = magnifier_readout_line(cursor, center);
        let text_w = text::measure_width(&line, layout.font).unwrap_or(0.0);
        let swatch = (layout.pill_h - 8).max(10);
        let footer_w = (swatch + 8 + text_w.ceil() as i32 + layout.pill_pad_x)
            .min(panel.width - layout.pad * 2);
        let footer_x = panel.x + (panel.width - footer_w) / 2;
        let footer_y = panel.y + layout.pad + layout.edge + layout.gap;
        let sw = IntRect {
            x: footer_x,
            y: footer_y + (layout.pill_h - swatch) / 2,
            width: swatch,
            height: swatch,
        };
        fill_round(rgba, w, h, sw, 3, [center[0], center[1], center[2], 255]);
        for i in 0..swatch {
            blend(rgba, w, h, sw.x + i, sw.y, CHROME_BORDER);
            blend(rgba, w, h, sw.x + i, sw.bottom() - 1, CHROME_BORDER);
            blend(rgba, w, h, sw.x, sw.y + i, CHROME_BORDER);
            blend(rgba, w, h, sw.right() - 1, sw.y + i, CHROME_BORDER);
        }
        let line_h = text::line_height(layout.font);
        text::draw_text(
            rgba,
            w,
            h,
            (sw.right() + 8) as f32,
            footer_y as f32 + (layout.pill_h as f32 - line_h) / 2.0,
            &line,
            layout.font,
            CHROME_TEXT,
        );
    }
}

/// 浮层画法:向下 2px 的阴影 → 1px 边 → 浅蓝灰底。
/// 长截图字形:竖向圆角框内一支向下箭头(表示向下滚动并拼接)。
fn draw_long_capture_icon(
    rgba: &mut [u8],
    w: u32,
    h: u32,
    cx: i32,
    cy: i32,
    size: i32,
    ink: [u8; 4],
) {
    let size = size.max(12);
    let half = size / 2;
    let left = cx - half / 2;
    let right = cx + half / 2;
    let top = cy - half;
    let bottom = cy + half;
    let stroke = (size / 12).max(2);
    for offset in 0..stroke {
        for x in left..=right {
            blend(rgba, w, h, x, top + offset, ink);
            blend(rgba, w, h, x, bottom - offset, ink);
        }
        for y in top..=bottom {
            blend(rgba, w, h, left + offset, y, ink);
            blend(rgba, w, h, right - offset, y, ink);
        }
    }
    let arrow_top = top + stroke * 2 + size / 10;
    let arrow_bottom = bottom - stroke * 2 - size / 10;
    let radius = stroke / 2 + 1;
    blend_line(rgba, w, h, cx, arrow_top, cx, arrow_bottom, radius, ink);
    blend_line(
        rgba,
        w,
        h,
        cx,
        arrow_bottom,
        cx - size / 6,
        arrow_bottom - size / 6,
        radius,
        ink,
    );
    blend_line(
        rgba,
        w,
        h,
        cx,
        arrow_bottom,
        cx + size / 6,
        arrow_bottom - size / 6,
        radius,
        ink,
    );
}

/// 混合模式的粗线(图标字形不透明落笔会把底色打穿,这里全部 blend)。
#[allow(clippy::too_many_arguments)]
fn blend_line(
    rgba: &mut [u8],
    w: u32,
    h: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    ink: [u8; 4],
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).max(1);
    for i in 0..=steps {
        let x = x0 + dx * i / steps;
        let y = y0 + dy * i / steps;
        for oy in -radius..=radius {
            for ox in -radius..=radius {
                if ox * ox + oy * oy <= radius * radius {
                    blend(rgba, w, h, x + ox, y + oy, ink);
                }
            }
        }
    }
}

fn place_tooltip(
    anchor: IntRect,
    width: i32,
    height: i32,
    gap: i32,
    screen: (i32, i32),
) -> Option<IntRect> {
    let (sw, sh) = screen;
    if width <= 0 || height <= 0 || width > sw || height > sh {
        return None;
    }
    let (ax, ay) = anchor.center();
    // 工具条是单行按钮。提示优先放在按钮上方,放不下再放下方。
    // 左右候选会盖住相邻按钮,只在上下都出屏时使用。
    let centered_x = (ax - width / 2).clamp(0, sw - width);
    let centered_y = (ay - height / 2).clamp(0, sh - height);
    let candidates = [
        (centered_x, anchor.y - height - gap),
        (centered_x, anchor.bottom() + gap),
        (anchor.x - width - gap, centered_y),
        (anchor.right() + gap, centered_y),
    ];
    for (x, y) in candidates {
        if x >= 0 && y >= 0 && x + width <= sw && y + height <= sh {
            return Some(IntRect {
                x,
                y,
                width,
                height,
            });
        }
    }
    Some(IntRect {
        x: (ax - width / 2).clamp(0, sw - width),
        y: (anchor.y - height - gap).clamp(0, sh - height),
        width,
        height,
    })
}

fn draw_panel_chrome(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, radius: i32) {
    let shadow = IntRect {
        y: rect.y + 2,
        ..rect
    };
    fill_round_blend(rgba, w, h, shadow, radius, CHROME_SHADOW);
    fill_round_blend(rgba, w, h, rect, radius, CHROME_BORDER);
    fill_round_blend(rgba, w, h, inset(rect, 1), radius - 1, CHROME_BG);
}

fn inset(rect: IntRect, by: i32) -> IntRect {
    IntRect {
        x: rect.x + by,
        y: rect.y + by,
        width: (rect.width - 2 * by).max(0),
        height: (rect.height - 2 * by).max(0),
    }
}

fn restore_rect(dst: &mut [u8], src: &[u8], stride_px: usize, rect: IntRect) {
    if rect.is_empty() {
        return;
    }
    for row in 0..rect.height as usize {
        let offset = ((rect.y as usize + row) * stride_px + rect.x as usize) * 4;
        let count = rect.width as usize * 4;
        if offset + count <= dst.len() && offset + count <= src.len() {
            dst[offset..offset + count].copy_from_slice(&src[offset..offset + count]);
        }
    }
}

fn punch_hole(
    rgba: &mut [u8],
    original: &[u8],
    stride_px: usize,
    rect: PhysicalRect,
    clip: Option<IntRect>,
) {
    let hole = IntRect::from(rect);
    let hole = match clip {
        Some(clip) => hole.intersect(clip),
        None => hole,
    };
    restore_rect(rgba, original, stride_px, hole);
}

/// 选区描边:先画选区外 2px 对比晕边,再画贴边的 2px `--accent`。
/// 晕边与强调色等宽且外移一个线宽,两条边相接,对比不靠壁纸像素。
fn outline_selection(rgba: &mut [u8], w: u32, h: u32, rect: PhysicalRect) {
    let scheme = system_prefers_dark();
    let x0 = rect.x as i32;
    let y0 = rect.y as i32;
    let x1 = x0 + rect.width as i32 - 1;
    let y1 = y0 + rect.height as i32 - 1;
    paint_outline_band(
        rgba,
        w,
        h,
        x0 - 2,
        y0 - 2,
        x1 + 2,
        y1 + 2,
        2,
        halo_for_scheme(scheme),
    );
    paint_outline_band(rgba, w, h, x0, y0, x1, y1, 2, accent_for_scheme(scheme));
}

/// 矩形外圈 `thickness` 像素(向内),越界像素由 `put` 丢弃。
fn paint_outline_band(
    rgba: &mut [u8],
    w: u32,
    h: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    thickness: i32,
    color: [u8; 4],
) {
    if thickness <= 0 || x1 < x0 || y1 < y0 {
        return;
    }
    for t in 0..thickness {
        let left = x0 + t;
        let right = x1 - t;
        let top = y0 + t;
        let bottom = y1 - t;
        if left > right || top > bottom {
            break;
        }
        for x in left..=right {
            put(rgba, w, h, x, top, color);
            if bottom != top {
                put(rgba, w, h, x, bottom, color);
            }
        }
        for y in (top + 1)..bottom {
            put(rgba, w, h, left, y, color);
            if right != left {
                put(rgba, w, h, right, y, color);
            }
        }
    }
}

fn accent_for_scheme(prefers_dark: Option<bool>) -> [u8; 4] {
    if prefers_dark == Some(true) {
        ACCENT_DARK
    } else {
        ACCENT_LIGHT
    }
}

fn halo_for_scheme(prefers_dark: Option<bool>) -> [u8; 4] {
    if prefers_dark == Some(true) {
        HALO_DARK
    } else {
        HALO_LIGHT
    }
}

/// GTK 主题名含 dark,或 `gtk-application-prefer-dark` 为 true,才强制深色。
/// 该键为 0/false 只表示不强制深色,继续采用 `gnome_scheme`(读不到则为 `None`)。
#[cfg(any(test, target_os = "linux"))]
fn linux_scheme(
    gtk_theme_names_dark: bool,
    gtk_prefer_dark: Option<bool>,
    gnome_scheme: Option<bool>,
) -> Option<bool> {
    if gtk_theme_names_dark || gtk_prefer_dark == Some(true) {
        Some(true)
    } else {
        gnome_scheme
    }
}

fn selection_accent() -> [u8; 4] {
    accent_for_scheme(system_prefers_dark())
}

/// 系统应用配色。读不到时返回 `None`,描边改用浅色强调色。
fn system_prefers_dark() -> Option<bool> {
    #[cfg(windows)]
    {
        return windows_prefers_dark();
    }
    #[cfg(target_os = "macos")]
    {
        return macos_prefers_dark();
    }
    #[cfg(target_os = "linux")]
    {
        return linux_prefers_dark();
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[cfg(windows)]
fn windows_prefers_dark() -> Option<bool> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = hkcu
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .ok()?;
    let light: u32 = key.get_value("AppsUseLightTheme").ok()?;
    Some(light == 0)
}

#[cfg(target_os = "macos")]
fn macos_prefers_dark() -> Option<bool> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2_foundation::NSString;
    unsafe {
        let cls = AnyClass::get(c"NSUserDefaults")?;
        let defaults: *mut AnyObject = msg_send![cls, standardUserDefaults];
        if defaults.is_null() {
            return None;
        }
        let key = NSString::from_str("AppleInterfaceStyle");
        let value: *mut AnyObject = msg_send![defaults, stringForKey: &*key];
        if value.is_null() {
            return Some(false);
        }
        let dark = NSString::from_str("Dark");
        let is_dark: bool = msg_send![value, isEqualToString: &*dark];
        Some(is_dark)
    }
}

#[cfg(target_os = "linux")]
fn linux_prefers_dark() -> Option<bool> {
    let theme_dark = std::env::var_os("GTK_THEME").is_some_and(|theme| {
        theme
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("dark")
    });
    let prefer = gtk_application_prefer_dark();
    let gnome = if theme_dark || prefer == Some(true) {
        None
    } else {
        cached_gnome_color_scheme()
    };
    linux_scheme(theme_dark, prefer, gnome)
}

#[cfg(target_os = "linux")]
fn gtk_application_prefer_dark() -> Option<bool> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    for rel in ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"] {
        let Ok(text) = std::fs::read_to_string(config.join(rel)) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line
                .strip_prefix("gtk-application-prefer-dark")
                .map(str::trim)
                .and_then(|s| s.strip_prefix('=').map(str::trim))
            else {
                continue;
            };
            return Some(rest == "1" || rest.eq_ignore_ascii_case("true"));
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn cached_gnome_color_scheme() -> Option<bool> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Option<bool>)>> = Mutex::new(None);
    let mut guard = CACHE.lock().ok()?;
    if let Some((at, value)) = *guard {
        if at.elapsed() < Duration::from_millis(500) {
            return value;
        }
    }
    let value = gnome_color_scheme();
    *guard = Some((Instant::now(), value));
    value
}

#[cfg(target_os = "linux")]
fn gnome_color_scheme() -> Option<bool> {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains("prefer-dark") {
        Some(true)
    } else if text.contains("prefer-light") || text.contains("default") {
        Some(false)
    } else {
        None
    }
}

fn put(rgba: &mut [u8], w: u32, h: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    if i + 3 < rgba.len() {
        rgba[i..i + 4].copy_from_slice(&color);
    }
}

/// alpha 叠加合成(图标字形等预乘覆盖度的位图经此落笔);`icons` 模块复用。
pub(super) fn blend(rgba: &mut [u8], w: u32, h: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return;
    }
    let i = ((y as u32 * w + x as u32) * 4) as usize;
    if i + 3 >= rgba.len() {
        return;
    }
    let a = f32::from(color[3]) / 255.0;
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

/// 圆角矩形内判定:点钳制到内缩 radius 的核心矩形后,到钳制点的距离 ≤ radius。
fn inside_round(rect: IntRect, radius: i32, x: i32, y: i32) -> bool {
    if radius <= 0 {
        return rect.contains(x, y);
    }
    let ix = x.clamp(rect.x + radius, rect.right() - 1 - radius);
    let iy = y.clamp(rect.y + radius, rect.bottom() - 1 - radius);
    let dx = x - ix;
    let dy = y - iy;
    dx * dx + dy * dy <= radius * radius
}

/// 不透明圆角矩形填充(accent 填充按钮底)。
fn fill_round(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, radius: i32, color: [u8; 4]) {
    if rect.width <= 0 || rect.height <= 0 {
        return;
    }
    let radius = radius
        .min((rect.width - 1) / 2)
        .min((rect.height - 1) / 2)
        .max(0);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if inside_round(rect, radius, x, y) {
                put(rgba, w, h, x, y, color);
            }
        }
    }
}

fn fill_round_blend(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, radius: i32, color: [u8; 4]) {
    if rect.width <= 0 || rect.height <= 0 {
        return;
    }
    let radius = radius
        .min((rect.width - 1) / 2)
        .min((rect.height - 1) / 2)
        .max(0);
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if inside_round(rect, radius, x, y) {
                blend(rgba, w, h, x, y, color);
            }
        }
    }
}

fn fill_circle(rgba: &mut [u8], w: u32, h: u32, cx: i32, cy: i32, radius: i32, color: [u8; 4]) {
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx * dx + dy * dy <= radius * radius {
                put(rgba, w, h, cx + dx, cy + dy, color);
            }
        }
    }
}

fn fill_rect(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, color: [u8; 4]) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            put(rgba, w, h, x, y, color);
        }
    }
}

/// 粗线:沿 (x0,y0)→(x1,y1) 步进填圆。
#[allow(clippy::too_many_arguments)]
fn draw_line(
    rgba: &mut [u8],
    w: u32,
    h: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    radius: i32,
    color: [u8; 4],
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).max(1);
    for i in 0..=steps {
        fill_circle(
            rgba,
            w,
            h,
            x0 + dx * i / steps,
            y0 + dy * i / steps,
            radius,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::{Annotation, Point};
    use crate::capture::buffer::{accept_buffer, crop_rgba, RawBuffer};
    use crate::capture::selection::ToolToggles;

    fn solid_frame(width: u32, height: u32, rgba: [u8; 4]) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            bytes.extend_from_slice(&rgba);
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap()
    }

    fn near_chrome(pixel: [u8; 3]) -> bool {
        pixel
            .iter()
            .zip(CHROME_BG)
            .all(|(channel, surface)| (*channel as i16 - surface as i16).abs() <= 12)
    }

    /// 选区标注测试场景:未选中工具、光标在选区外。
    fn annotation_scene(selection: PhysicalRect, flags: FeatureFlags) -> Scene {
        Scene {
            selection: Some(selection),
            cursor: (300, 300),
            flags,
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        }
    }

    fn annotation_overlay<'a>(
        annotations: &'a [Annotation],
        draft: Option<&'a Annotation>,
        tool: Option<AnnotationTool>,
        revision: u64,
    ) -> AnnotationOverlay<'a> {
        AnnotationOverlay {
            annotations,
            draft,
            tool,
            mode: None,
            text: None,
            revision,
            color: [225, 29, 72, 255],
            text_size: 22.0,
            text_input: true,
        }
    }

    /// R21 核心一致性:合成器在选区内绘制已确认图元的结果,必须与最终
    /// 输出(`rasterize` 裁剪帧 + 平移图元)逐像素一致(所见即所得)。
    #[test]
    fn annotation_overlay_matches_rasterize_inside_selection() {
        let width = 200;
        let height = 160;
        let mut frame = solid_frame(width, height, [40, 80, 120, 255]);
        // 选区内加棋盘底图,确保遮盖/折线等工具改变像素可辨别。
        for y in 20..100u32 {
            for x in 20..120u32 {
                let i = ((y * width + x) * 4) as usize;
                let tone = if (x + y) % 2 == 0 { 200 } else { 60 };
                frame.rgba[i..i + 4].copy_from_slice(&[tone, tone / 2, 255 - tone, 255]);
            }
        }
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 20,
            y: 20,
            width: 100,
            height: 80,
        };
        let annotations = vec![
            Annotation::Rect {
                x: 30.0,
                y: 30.0,
                width: 40.0,
                height: 30.0,
                color: "#e11d48".into(),
                stroke_width: None,
            },
            Annotation::Ellipse {
                x: 40.0,
                y: 40.0,
                width: 30.0,
                height: 20.0,
                color: "#2563eb".into(),
                stroke_width: Some(3.0),
            },
            Annotation::Mosaic {
                x: 60.0,
                y: 50.0,
                width: 30.0,
                height: 24.0,
                block: 12,
            },
            Annotation::Highlighter {
                points: vec![Point { x: 35.0, y: 80.0 }, Point { x: 95.0, y: 88.0 }],
                color: "#f59e0b".into(),
                stroke_width: None,
            },
        ];
        // 关闭即时标注:本测试只核对图元与 rasterize 的逐像素一致性。
        let flags = FeatureFlags {
            magnifier: false,
            inline_annotation: false,
            ..FeatureFlags::default()
        };
        let scene = annotation_scene(selection, flags);
        let overlay = annotation_overlay(&annotations, None, None, 1);
        let composed = composer.compose_with_overlay(&scene, &overlay);

        let cropped = crop_rgba(&frame, 20, 20, 100, 80).unwrap();
        let translated = crate::annotate::translated_all(&annotations, -20.0, -20.0);
        let expected = crate::annotate::rasterize(&cropped, &translated).unwrap();
        // 排除选区边框/手柄/徽标覆盖的 8px 边带后逐像素一致。
        for row in 8..72usize {
            for col in 8..92usize {
                let dst =
                    ((selection.y as usize + row) * width as usize + selection.x as usize + col)
                        * 4;
                let src = (row * 100 + col) * 4;
                assert_eq!(
                    &composed[dst..dst + 4],
                    &expected.rgba[src..src + 4],
                    "pixel ({col},{row})"
                );
            }
        }
        // 选区外仍为暗幕(未被标注污染)。
        let outside = (10 * width as usize + 10) * 4;
        assert_eq!(composed[outside], (40u16 * 52 / 100) as u8);
    }

    /// review P1 回归:标注层是选区**内容**,不得擦掉选区 chrome。
    /// 标注先画、描边/手柄/放大镜/菜单后画,任一已确认标注存在时 chrome
    /// 仍可见(且命中几何不变,不会出现"隐形但可点"的菜单)。
    #[test]
    fn annotation_layer_never_covers_selection_chrome() {
        let (w, h) = (800u32, 600u32);
        let frame = solid_frame(w, h, [30, 30, 30, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 720,
            height: 530,
        };
        // 图元放在选区下半部:避开顶部横条。
        let annotations = vec![Annotation::Rect {
            x: 100.0,
            y: 400.0,
            width: 200.0,
            height: 100.0,
            color: "#e11d48".into(),
            stroke_width: Some(4.0),
        }];
        let read = |bytes: &[u8], x: i32, y: i32| {
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2]]
        };
        let overlay = annotation_overlay(&annotations, None, None, 1);

        // 标注本身可见(非空断言):图元上边框在选区内落玫红像素。
        let flags = no_magnifier_flags();
        let composed = composer.compose_with_overlay(&annotation_scene(selection, flags), &overlay);
        assert_eq!(read(&composed, 200, 400), [225, 29, 72]);

        // 1) 描边与手柄:第一条标注产生后仍在最上层。
        let stroke = selection_accent();
        assert_eq!(read(&composed, 41, 300), [stroke[0], stroke[1], stroke[2]]);
        let (hx, hy) = handle_anchor(selection, HandleKind::SouthEast);
        assert_eq!(read(&composed, hx, hy), [255, 255, 255]);
        assert_eq!(
            read(&composed, hx + 4, hy),
            [ACCENT[0], ACCENT[1], ACCENT[2]]
        );

        // 2) 放大镜:光标在选区内(每次绘制标注时)面板与十字准星不被重贴擦除。
        let cursor = (400, 300);
        let scene = Scene {
            selection: Some(selection),
            cursor,
            flags: FeatureFlags::default(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose_with_overlay(&scene, &overlay);
        let panel = magnifier_rect(cursor, (w, h), 1.0);
        let layout = mag_layout(1.0);
        let px = panel.x + (panel.width - layout.edge) / 2;
        let origin_x = px + layout.half * layout.block;
        let origin_y = panel.y + layout.pad + layout.half * layout.block;
        assert_eq!(
            read(&composed, origin_x, origin_y),
            [ACCENT[0], ACCENT[1], ACCENT[2]],
            "magnifier center pixel outline must survive the annotation layer"
        );

        // 3) 右键菜单:面板像素不被擦除(菜单命中与绘制同源,不允许隐形可点)。
        let scene = Scene {
            selection: Some(selection),
            cursor,
            flags,
            toolbar_visible: false,
            menu_open: true,
            menu_anchor: cursor,
            more_open: false,
        };
        let composed = composer.compose_with_overlay(&scene, &overlay);
        let items = menu_items(flags);
        let menu = menu_panel(metrics_1(), cursor, (w, h), &items);
        let probe = read(&composed, menu.x + 10, menu.y + 8);
        assert!(
            probe[0] > 200 && probe[1] > 200 && probe[2] > 200,
            "menu panel must stay visible over annotations, got {probe:?}"
        );
    }

    /// 草稿即使未达导出下限也要可见(拖动过程中的即时反馈)。
    #[test]
    fn annotation_draft_renders_below_export_minimum() {
        let frame = solid_frame(200, 160, [30, 30, 30, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 20,
            y: 20,
            width: 100,
            height: 80,
        };
        let flags = no_magnifier_flags();
        let scene = annotation_scene(selection, flags);
        let empty: Vec<Annotation> = Vec::new();
        let baseline =
            composer.compose_with_overlay(&scene, &annotation_overlay(&empty, None, None, 0));
        // 1×1 矩形低于 exportable 下限,但草稿路径不过滤,应可见。
        let draft = Annotation::Rect {
            x: 50.0,
            y: 50.0,
            width: 1.0,
            height: 1.0,
            color: "#e11d48".into(),
            stroke_width: None,
        };
        let with_draft = composer
            .compose_with_overlay(&scene, &annotation_overlay(&empty, Some(&draft), None, 0));
        let count = baseline
            .chunks_exact(4)
            .zip(with_draft.chunks_exact(4))
            .filter(|(before, after)| before != after)
            .count();
        assert!(count > 0, "draft must be visible while dragging");
    }

    /// 统一横条:主行恒为单行;关闭即时标注时为 标注/复制/保存/取消/更多;
    /// 复制 accent 填充;选中工具有 accent 选中态;「更多」展开动作面板。
    #[test]
    fn unified_toolbar_draws_main_row_and_more_panel() {
        let frame = solid_frame(800, 600, [30, 30, 30, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 720,
            height: 530,
        };
        let enabled = FeatureFlags {
            magnifier: false,
            ..FeatureFlags::default()
        };
        let disabled = FeatureFlags {
            magnifier: false,
            inline_annotation: false,
            ..FeatureFlags::default()
        };
        let empty: Vec<Annotation> = Vec::new();
        let overlay = |tool: Option<AnnotationTool>| annotation_overlay(&empty, None, tool, 0);
        let mode_scene = |selection: PhysicalRect, flags: FeatureFlags, more: bool| Scene {
            selection: Some(selection),
            cursor: (8, 8),
            flags,
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: more,
        };
        // 面板内深色图标像素计数(亮铬面板上的深墨图标)。
        let panel_ink = |bytes: &[u8], more: bool| {
            let toolbar = unified_toolbar(
                ChromeMetrics::for_scale(1.0),
                selection,
                (800, 600),
                enabled,
                true,
            )
            .expect("toolbar");
            let panel = toolbar.panel;
            let mut region = panel;
            if more {
                let items = more_panel_buttons(enabled);
                let more_panel = more_panel(
                    ChromeMetrics::for_scale(1.0),
                    toolbar.buttons.last().unwrap().1,
                    Some(toolbar.panel),
                    (800, 600),
                    &items,
                );
                region = region.union(more_panel);
            }
            let mut n = 0usize;
            for y in region.y..region.bottom() {
                for x in region.x..region.right() {
                    let i = ((y as u32 * 800 + x as u32) * 4) as usize;
                    // 盒式过滤下采样后描边核心非精确墨色,按近色容差计数。
                    if bytes[i] <= CHROME_TEXT[0] + 14
                        && bytes[i + 1] <= CHROME_TEXT[1] + 14
                        && bytes[i + 2] <= CHROME_TEXT[2] + 14
                    {
                        n += 1;
                    }
                }
            }
            n
        };
        // 即时标注开启:单行横条(深色线标,不是一排青绿圆)。
        let with_tools =
            composer.compose_with_overlay(&mode_scene(selection, enabled, false), &overlay(None));
        assert!(
            panel_ink(&with_tools, false) > 40,
            "toolbar outline icons should be drawn"
        );
        // 面板高度恒为一行按钮。
        let toolbar = unified_toolbar(
            ChromeMetrics::for_scale(1.0),
            selection,
            (800, 600),
            enabled,
            true,
        )
        .expect("toolbar");
        assert_eq!(toolbar.panel.height, metrics_1().bar_button);
        assert_eq!(toolbar.buttons.len(), toolbar_buttons(enabled, true).len());
        // 展开「更多」:面板出现在「更多」按钮上方,图标像素增多。
        let expanded =
            composer.compose_with_overlay(&mode_scene(selection, enabled, true), &overlay(None));
        assert!(
            panel_ink(&expanded, true) > panel_ink(&with_tools, false),
            "expanded more panel must add visible buttons"
        );
        // 关闭 inlineAnnotation:主行为 标注/复制/保存/取消/更多。
        let light =
            composer.compose_with_overlay(&mode_scene(selection, disabled, false), &overlay(None));
        assert_eq!(
            toolbar_buttons(disabled, true),
            vec![
                SelectionAction::Annotate,
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        assert!(
            panel_ink(&light, false) > 20,
            "annotate-form toolbar should draw icons"
        );
        // 选中工具用蓝色字形。复制按钮也是这块蓝,所以只断言选中后蓝像素变多。
        let selected = composer.compose_with_overlay(
            &mode_scene(selection, enabled, false),
            &overlay(Some(AnnotationTool::Rect)),
        );
        let accent_pixels = |bytes: &[u8]| {
            bytes
                .chunks_exact(4)
                .filter(|px| px[0] < 80 && px[1] < 140 && px[2] > 180)
                .count()
        };
        assert!(
            accent_pixels(&selected) > accent_pixels(&with_tools),
            "selected tool should paint its glyph in accent ink"
        );
        // 复制按钮不再单独高亮,短杠位置应是浮层底而不是强调色。
        let copy_rect = toolbar
            .buttons
            .iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .map(|(_, rect)| *rect)
            .expect("copy button");
        let (cx, cy) = copy_rect.center();
        let read = |bytes: &[u8], x: i32, y: i32| {
            let i = ((y as u32 * 800 + x as u32) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2]]
        };
        assert_ne!(
            read(&with_tools, cx, cy + copy_rect.height / 2 - 6),
            [ACCENT[0], ACCENT[1], ACCENT[2]],
            "copy button should not carry an accent mark"
        );
    }

    /// 主行动作矩阵:注册表 + 开关矩阵决定主行与「更多」内容,关闭复制/保存
    /// 后主行与「更多」均无该项且顺序不变;逐项工具开关只裁剪对应入口。
    #[test]
    fn unified_toolbar_button_matrix_follows_flags() {
        let on = FeatureFlags::default();
        assert_eq!(
            toolbar_buttons(on, true),
            vec![
                SelectionAction::Tool(AnnotationTool::Arrow),
                SelectionAction::Tool(AnnotationTool::Rect),
                SelectionAction::Tool(AnnotationTool::Ellipse),
                SelectionAction::Tool(AnnotationTool::Highlighter),
                SelectionAction::Tool(AnnotationTool::Mosaic),
                SelectionAction::Tool(AnnotationTool::Text),
                SelectionAction::Undo,
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        // 平台无文本输入通道:主行不含文字工具,顺序不变。
        assert_eq!(
            toolbar_buttons(on, false),
            vec![
                SelectionAction::Tool(AnnotationTool::Arrow),
                SelectionAction::Tool(AnnotationTool::Rect),
                SelectionAction::Tool(AnnotationTool::Ellipse),
                SelectionAction::Tool(AnnotationTool::Highlighter),
                SelectionAction::Tool(AnnotationTool::Mosaic),
                SelectionAction::Undo,
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        // 关闭复制/保存:主行与「更多」均无该项,其余顺序不变。
        let off = FeatureFlags {
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ocr_entry: false,
            ..FeatureFlags::default()
        };
        assert_eq!(
            toolbar_buttons(off, true),
            vec![
                SelectionAction::Tool(AnnotationTool::Arrow),
                SelectionAction::Tool(AnnotationTool::Rect),
                SelectionAction::Tool(AnnotationTool::Ellipse),
                SelectionAction::Tool(AnnotationTool::Highlighter),
                SelectionAction::Tool(AnnotationTool::Mosaic),
                SelectionAction::Tool(AnnotationTool::Text),
                SelectionAction::Undo,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        // 「更多」(R19 精选默认):默认开启的收进工具(序号)+ 合并工具的
        // 全部模式入口(所属工具开启)+ 重做 + 删除 + 贴图/取字(开关允许时)。
        assert_eq!(
            more_panel_buttons(on),
            vec![
                SelectionAction::Tool(AnnotationTool::Number),
                SelectionAction::Mode(ToolMode::Arrow),
                SelectionAction::Mode(ToolMode::Line),
                SelectionAction::Mode(ToolMode::Highlighter),
                SelectionAction::Mode(ToolMode::Pen),
                SelectionAction::Mode(ToolMode::Mosaic),
                SelectionAction::Mode(ToolMode::Blur),
                SelectionAction::Redo,
                SelectionAction::Delete,
                SelectionAction::Pin,
                SelectionAction::Ocr,
            ]
        );
        // 「更多」仍含收进的工具/模式/重做/删除(仅贴图/取字被开关关闭)。
        assert_eq!(
            more_panel_buttons(off),
            vec![
                SelectionAction::Tool(AnnotationTool::Number),
                SelectionAction::Mode(ToolMode::Arrow),
                SelectionAction::Mode(ToolMode::Line),
                SelectionAction::Mode(ToolMode::Highlighter),
                SelectionAction::Mode(ToolMode::Pen),
                SelectionAction::Mode(ToolMode::Mosaic),
                SelectionAction::Mode(ToolMode::Blur),
                SelectionAction::Redo,
                SelectionAction::Delete,
            ]
        );
        // 即时标注关闭:「更多」只含贴图/取字。
        let inline_off = FeatureFlags {
            inline_annotation: false,
            ..FeatureFlags::default()
        };
        assert_eq!(
            more_panel_buttons(inline_off),
            vec![SelectionAction::Pin, SelectionAction::Ocr]
        );
        // R5:全量开启时「更多」含全部注册表收进工具(序号/聚光灯/放大镜/
        // 对话气泡/贴纸/内容擦除)。
        let all_tools = FeatureFlags {
            tools: ToolToggles {
                spotlight: true,
                magnifier: true,
                bubble: true,
                sticker: true,
                erase: true,
                ..ToolToggles::default()
            },
            ..FeatureFlags::default()
        };
        assert_eq!(
            more_panel_buttons(all_tools),
            vec![
                SelectionAction::Tool(AnnotationTool::Number),
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
                SelectionAction::Redo,
                SelectionAction::Delete,
                SelectionAction::Pin,
                SelectionAction::Ocr,
            ]
        );
        // R5/R19:逐项开关关闭工具后主行不再含该入口;关闭全部绘图工具后
        // 主行只剩 撤销/复制/保存/取消/更多(撤销/重做/删除仍可用)。
        let rect_off = FeatureFlags {
            tools: ToolToggles {
                rect: false,
                arrow: false,
                ..ToolToggles::default()
            },
            ..FeatureFlags::default()
        };
        let buttons = toolbar_buttons(rect_off, true);
        assert!(!buttons.contains(&SelectionAction::Tool(AnnotationTool::Rect)));
        assert!(!buttons.contains(&SelectionAction::Tool(AnnotationTool::Arrow)));
        assert!(buttons.contains(&SelectionAction::Tool(AnnotationTool::Ellipse)));
        let no_draw = FeatureFlags {
            tools: ToolToggles {
                arrow: false,
                rect: false,
                ellipse: false,
                highlighter: false,
                mosaic: false,
                text: false,
                number: false,
                spotlight: false,
                magnifier: false,
                bubble: false,
                sticker: false,
                erase: false,
            },
            ..FeatureFlags::default()
        };
        assert_eq!(
            toolbar_buttons(no_draw, true),
            vec![
                SelectionAction::Undo,
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        assert_eq!(
            more_panel_buttons(no_draw),
            vec![
                SelectionAction::Redo,
                SelectionAction::Delete,
                SelectionAction::Pin,
                SelectionAction::Ocr,
            ]
        );
        // 合并工具的默认模式入口同样随所属工具开关裁剪:关闭马赛克后,
        // 马赛克/模糊两个模式入口都不再出现。
        let modes = more_panel_buttons(FeatureFlags {
            tools: ToolToggles {
                mosaic: false,
                ..ToolToggles::default()
            },
            ..FeatureFlags::default()
        });
        assert!(!modes.contains(&SelectionAction::Mode(ToolMode::Mosaic)));
        assert!(!modes.contains(&SelectionAction::Mode(ToolMode::Blur)));
        assert!(modes.contains(&SelectionAction::Mode(ToolMode::Line)));
    }

    /// 统一横条布局:下缘优先(水平居中)→ 上缘 → 侧面;恒为一行、在屏内、
    /// 不压选区;贴边小选区仍完整落在屏幕内。
    #[test]
    fn unified_toolbar_placement_prefers_below_then_above_then_side() {
        let metrics = metrics_1();
        // 下缘外侧居中优先。
        let selection = PhysicalRect {
            x: 100,
            y: 80,
            width: 300,
            height: 200,
        };
        let flags = FeatureFlags::default();
        let toolbar =
            unified_toolbar(metrics, selection, (1280, 800), flags, true).expect("toolbar");
        assert_eq!(toolbar.panel.height, metrics.bar_button, "必须为一行");
        assert_eq!(
            toolbar.panel.width,
            toolbar.buttons.len() as i32 * metrics.bar_button
        );
        assert_eq!(
            toolbar.panel.y,
            selection.y as i32 + selection.height as i32 + BAR_MARGIN
        );
        let row_center = toolbar.panel.x + toolbar.panel.width / 2;
        assert!(
            (row_center - (100 + 150)).abs() <= 1,
            "横条应水平居中于选区"
        );
        // 按钮不重叠、都在面板内。
        for (index, (_, rect)) in toolbar.buttons.iter().enumerate() {
            assert_eq!(rect.width, metrics.bar_button);
            assert_eq!(rect.height, metrics.bar_button);
            assert_eq!(rect.x, toolbar.panel.x + index as i32 * metrics.bar_button);
            assert!(toolbar.panel.contains(rect.x, rect.y));
        }
        // 贴屏幕底:下缘放不下 → 上缘。
        let bottom = PhysicalRect {
            x: 100,
            y: 740,
            width: 300,
            height: 50,
        };
        let toolbar = unified_toolbar(metrics, bottom, (1280, 800), flags, true).expect("toolbar");
        assert_eq!(toolbar.panel.bottom(), bottom.y as i32 - BAR_MARGIN);
        // 上下都不行(选区几乎占满高度)→ 侧面,仍不压选区。
        let tall = PhysicalRect {
            x: 100,
            y: 20,
            width: 600,
            height: 760,
        };
        let toolbar = unified_toolbar(metrics, tall, (1280, 800), flags, true).expect("toolbar");
        assert!(
            toolbar.panel.right() <= tall.x as i32
                || toolbar.panel.x >= tall.x as i32 + tall.width as i32,
            "侧面放置不得压选区: {:?}",
            toolbar.panel
        );
        // 右下角贴边小选区:横条仍完整落在屏幕内。
        let corner = PhysicalRect {
            x: 1180,
            y: 740,
            width: 90,
            height: 50,
        };
        let placed =
            unified_toolbar(metrics, corner, (1280, 800), flags, true).expect("corner toolbar");
        assert!(placed.panel.x >= 0 && placed.panel.y >= 0);
        assert!(placed.panel.right() <= 1280 && placed.panel.bottom() <= 800);
        // 选区几乎占满屏幕:重叠最小兜底,横条仍可见可点。
        let full = PhysicalRect {
            x: 0,
            y: 0,
            width: 1280,
            height: 800,
        };
        let placed =
            unified_toolbar(metrics, full, (1280, 800), flags, true).expect("full toolbar");
        assert!(placed.panel.x >= 0 && placed.panel.right() <= 1280);
    }

    /// 放置语义:下缘强默认(人体工学,靠近拖选区结束位置的光标),选区在
    /// 屏幕中下部也保下缘;仅下缘放不下才翻上缘;竖直都不行时水平侧按
    /// 剩余空间大者优先。
    #[test]
    fn unified_toolbar_bottom_is_strong_default_and_flips_only_when_unfit() {
        let metrics = metrics_1();
        let flags = FeatureFlags::default();
        // 选区在屏幕下半部,但下缘仍放得下 → 保下缘(不翻到上方)。
        let lower = PhysicalRect {
            x: 200,
            y: 600,
            width: 300,
            height: 100,
        };
        let toolbar = unified_toolbar(metrics, lower, (1280, 800), flags, true).expect("toolbar");
        assert_eq!(
            toolbar.panel.y,
            lower.y as i32 + lower.height as i32 + BAR_MARGIN
        );
        // 选区贴上边缘:上缘外侧放不下 → 保下缘。
        let upper = PhysicalRect {
            x: 200,
            y: 6,
            width: 300,
            height: 100,
        };
        let toolbar = unified_toolbar(metrics, upper, (1280, 800), flags, true).expect("toolbar");
        assert_eq!(
            toolbar.panel.y,
            upper.y as i32 + upper.height as i32 + BAR_MARGIN
        );
        // 下缘外侧被屏幕底裁掉 → 翻上缘。
        let bottom = PhysicalRect {
            x: 200,
            y: 740,
            width: 300,
            height: 50,
        };
        let toolbar = unified_toolbar(metrics, bottom, (1280, 800), flags, true).expect("toolbar");
        assert_eq!(toolbar.panel.bottom(), bottom.y as i32 - BAR_MARGIN);
        // 竖直都不行 → 水平侧按剩余空间大者优先(右侧空间大)。
        let tall = PhysicalRect {
            x: 100,
            y: 20,
            width: 600,
            height: 760,
        };
        let toolbar = unified_toolbar(metrics, tall, (1280, 800), flags, true).expect("toolbar");
        assert!(
            toolbar.panel.x >= tall.x as i32 + tall.width as i32,
            "space-aware side pick prefers right: {:?}",
            toolbar.panel
        );
    }

    #[test]
    fn chrome_icons_paint_ink_inside_their_box() {
        let (w, h) = (48u32, 48u32);
        let ink = [255, 255, 255, 255];
        let rect = IntRect {
            x: 8,
            y: 8,
            width: 32,
            height: 32,
        };
        // 位图资产经 icons 模块渲染;CopyColor 是键盘动作、无资产,不落笔。
        let actions = [
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
            SelectionAction::Mode(ToolMode::Line),
            SelectionAction::Mode(ToolMode::Pen),
            SelectionAction::Mode(ToolMode::Blur),
        ];
        for action in actions {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            let (cx, cy) = rect.center();
            icons::draw(&mut buf, w, h, action, cx, cy, 16, ink);
            let painted = buf.chunks_exact(4).filter(|px| px[0] > 0).count();
            assert!(painted > 8, "{action:?} painted {painted} pixels");
        }
    }

    /// 文本编辑会话按标注色绘制字符并画出光标(无字体环境仅验证不 panic)。
    /// R1:长截图开关开启时工具条与右键菜单出现入口,关闭时不出现;
    /// 菜单位置在「标注」之后、取字之前。
    #[test]
    fn long_capture_entry_is_gated_by_the_feature_flag() {
        let enabled = FeatureFlags {
            long_capture: true,
            ..FeatureFlags::default()
        };
        let items = menu_items(enabled);
        assert!(items.contains(&SelectionAction::LongCapture));
        assert!(toolbar_buttons(enabled, true).contains(&SelectionAction::LongCapture));
        let annotate = items
            .iter()
            .position(|action| *action == SelectionAction::Annotate)
            .unwrap();
        let long = items
            .iter()
            .position(|action| *action == SelectionAction::LongCapture)
            .unwrap();
        let ocr = items
            .iter()
            .position(|action| *action == SelectionAction::Ocr)
            .unwrap();
        assert!(annotate < long && long < ocr);

        let disabled = FeatureFlags::default();
        assert!(!menu_items(disabled).contains(&SelectionAction::LongCapture));
        assert!(!toolbar_buttons(disabled, true).contains(&SelectionAction::LongCapture));
        assert_eq!(action_label(SelectionAction::LongCapture), "长截图");
    }

    /// R1:长截图没有位图资产,几何字形必须实际落笔。
    #[test]
    fn long_capture_icon_paints_ink() {
        let mut buf = vec![0u8; 48 * 48 * 4];
        draw_long_capture_icon(&mut buf, 48, 48, 24, 24, 24, [255, 255, 255, 255]);
        let painted = buf.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(painted > 20, "painted {painted}");
    }

    #[test]
    fn text_edit_overlay_renders_text_and_caret() {
        let frame = solid_frame(240, 160, [30, 30, 30, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 20,
            y: 20,
            width: 200,
            height: 120,
        };
        // 关闭工具条,避免覆盖文本编辑区(工具条绘制另有专项测试)。
        let flags = FeatureFlags {
            magnifier: false,
            inline_annotation: false,
            ..FeatureFlags::default()
        };
        let scene = annotation_scene(selection, flags);
        let empty: Vec<Annotation> = Vec::new();
        let mut overlay = annotation_overlay(&empty, None, Some(AnnotationTool::Text), 0);
        let edit = crate::capture::selection::TextEdit {
            x: 40,
            y: 50,
            text: "中文".into(),
            preedit: String::new(),
        };
        overlay.text = Some(&edit);
        let composed = composer.compose_with_overlay(&scene, &overlay);
        if text::ui_font().is_none() {
            return; // 无字体环境:不落笔,仅保证不 panic。
        }
        // 文本覆盖处在暗底上出现标注色像素。
        let painted = composed
            .chunks_exact(4)
            .filter(|px| px[0] > 100 && px[1] < 90 && px[2] < 110)
            .count();
        assert!(painted > 0, "text overlay should draw in annotation color");
    }

    fn no_magnifier_flags() -> FeatureFlags {
        FeatureFlags {
            magnifier: false,
            ..FeatureFlags::default()
        }
    }

    /// 1.0 基准 metrics:既有固定几何断言按原值保留。
    fn metrics_1() -> ChromeMetrics {
        ChromeMetrics::for_scale(1.0)
    }

    #[test]
    fn selection_stroke_uses_web_accent_pair_and_light_fallback() {
        assert_eq!(accent_for_scheme(None), ACCENT_LIGHT);
        assert_eq!(accent_for_scheme(Some(false)), ACCENT_LIGHT);
        assert_eq!(accent_for_scheme(Some(true)), ACCENT_DARK);
        assert_eq!(ACCENT_LIGHT, [0x1D, 0x4E, 0xD8, 255]);
        assert_eq!(ACCENT_DARK, [0x93, 0xC5, 0xFD, 255]);
        assert_eq!(ACCENT_LIGHT, ACCENT);
        assert_ne!(ACCENT_DARK, ACCENT);
        let live = selection_accent();
        assert!(live == ACCENT_LIGHT || live == ACCENT_DARK);
        assert_eq!(halo_for_scheme(None), HALO_LIGHT);
        assert_eq!(halo_for_scheme(Some(false)), HALO_LIGHT);
        assert_eq!(halo_for_scheme(Some(true)), HALO_DARK);
        assert_eq!(HALO_LIGHT, [255, 255, 255, 255]);
        assert_eq!(HALO_DARK, [0x12, 0x16, 0x1C, 255]);
    }

    #[test]
    fn selection_outline_draws_contrasting_halo_outside_accent() {
        let frame = solid_frame(160, 120, [10, 200, 90, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 80,
            height: 50,
        };
        let scene = Scene {
            selection: Some(selection),
            cursor: (0, 0),
            flags: no_magnifier_flags(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 160 + x as u32) * 4) as usize;
            [
                composed[i],
                composed[i + 1],
                composed[i + 2],
                composed[i + 3],
            ]
        };
        let scheme = system_prefers_dark();
        let accent = accent_for_scheme(scheme);
        let halo = halo_for_scheme(scheme);
        let dimmed = [5u8, 104, 46, 255];
        // 左边避开角点与中点手柄:选区外 2px 晕边,贴边 2px 强调色,再往外仍是暗幕。
        assert_eq!(read(38, 45), halo);
        assert_eq!(read(39, 45), halo);
        assert_eq!(read(40, 45), accent);
        assert_eq!(read(41, 45), accent);
        assert_eq!(read(37, 45), dimmed);
        assert_eq!(read(50, 45), [10, 200, 90, 255]);
    }

    #[test]
    fn gtk_prefer_dark_false_continues_to_color_scheme() {
        assert_eq!(linux_scheme(false, Some(false), Some(true)), Some(true));
        assert_eq!(linux_scheme(false, None, Some(true)), Some(true));
        assert_eq!(linux_scheme(false, Some(false), Some(false)), Some(false));
        assert_eq!(linux_scheme(false, None, None), None);
        assert_eq!(linux_scheme(false, Some(true), Some(false)), Some(true));
        assert_eq!(linux_scheme(true, Some(false), None), Some(true));
    }

    #[test]
    fn chrome_palette_uses_cool_surface_and_shared_accent() {
        assert_eq!(CHROME_BG, [0xE4, 0xEB, 0xF6, 255]);
        assert_eq!(CHROME_TEXT, [0x1C, 0x21, 0x28, 255]);
        assert_eq!(CHROME_BORDER, [28, 33, 40, 41]);
        assert_eq!(ACTIVE_BG, [0x1D, 0x4E, 0xD8, 36]);
        assert_eq!(ACCENT, ACCENT_LIGHT);
        assert_eq!(ICON_INK, [255, 255, 255, 255]);
        // 压暗系数保持 52%。
        assert_eq!(DIM_KEEP, 52);
    }

    #[test]
    fn handle_hit_prefers_corners_and_rejects_interior() {
        let rect = PhysicalRect {
            x: 100,
            y: 100,
            width: 120,
            height: 80,
        };
        assert_eq!(
            handle_hit(metrics_1(), rect, 100, 100),
            Some(HandleKind::NorthWest)
        );
        assert_eq!(
            handle_hit(metrics_1(), rect, 219, 100),
            Some(HandleKind::NorthEast)
        );
        assert_eq!(
            handle_hit(metrics_1(), rect, 219, 179),
            Some(HandleKind::SouthEast)
        );
        assert_eq!(
            handle_hit(metrics_1(), rect, 100, 179),
            Some(HandleKind::SouthWest)
        );
        // 命中半径(9px)内仍算命中。
        assert_eq!(
            handle_hit(metrics_1(), rect, 106, 100),
            Some(HandleKind::NorthWest)
        );
        assert_eq!(
            handle_hit(metrics_1(), rect, 109, 100),
            Some(HandleKind::NorthWest)
        );
        assert_eq!(
            handle_hit(metrics_1(), rect, 159, 100),
            Some(HandleKind::North)
        );
        assert_eq!(
            handle_hit(metrics_1(), rect, 219, 139),
            Some(HandleKind::East)
        );
        // 半径外与选区内部不命中。
        assert_eq!(handle_hit(metrics_1(), rect, 110, 100), None);
        assert_eq!(handle_hit(metrics_1(), rect, 160, 140), None);
        assert_eq!(handle_hit(metrics_1(), rect, 300, 300), None);
    }

    #[test]
    fn edge_hit_bands_straddle_border_lines() {
        let rect = PhysicalRect {
            x: 100,
            y: 100,
            width: 120,
            height: 80,
        };
        // 四边 ±6px 命中(边线内外两侧)。
        assert_eq!(edge_hit(metrics_1(), rect, 160, 100), Some(EdgeKind::North));
        assert_eq!(edge_hit(metrics_1(), rect, 160, 94), Some(EdgeKind::North));
        assert_eq!(edge_hit(metrics_1(), rect, 160, 106), Some(EdgeKind::North));
        assert_eq!(edge_hit(metrics_1(), rect, 160, 179), Some(EdgeKind::South));
        assert_eq!(edge_hit(metrics_1(), rect, 100, 140), Some(EdgeKind::West));
        assert_eq!(edge_hit(metrics_1(), rect, 219, 140), Some(EdgeKind::East));
        assert_eq!(edge_hit(metrics_1(), rect, 225, 140), Some(EdgeKind::East));
        // 带外不命中。
        assert_eq!(edge_hit(metrics_1(), rect, 160, 93), None);
        assert_eq!(edge_hit(metrics_1(), rect, 160, 140), None);
        assert_eq!(edge_hit(metrics_1(), rect, 300, 300), None);
    }

    #[test]
    fn size_and_coordinate_readout_texts() {
        let rect = PhysicalRect {
            x: 10,
            y: 20,
            width: 123,
            height: 45,
        };
        // 徽标内容:宽 × 高 + 坐标。
        assert_eq!(size_readout(rect), "123 × 45 · 10, 20");
        assert_eq!(coordinate_readout(10, 20), "10, 20");
    }

    #[test]
    fn rgb_and_hex_readouts() {
        let pixel = [0x2D, 0xD4, 0xBF, 255];
        assert_eq!(rgb_readout(pixel), "R 45 G 212 B 191");
        assert_eq!(hex_readout(pixel), "#2DD4BF");
        assert_eq!(magnifier_readout_line((32, 32), pixel), "#2DD4BF");
    }

    #[test]
    fn sampling_reads_true_physical_pixels_and_clamps() {
        let width = 8;
        let height = 8;
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for i in 0..width * height {
            bytes.extend_from_slice(&[(i % 256) as u8, 7, 9, 255]);
        }
        let frame = accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap();
        assert_eq!(sample_pixel(&frame, 3, 4), [(4 * 8 + 3) as u8, 7, 9, 255]);
        // 越界钳制到边缘像素。
        assert_eq!(sample_pixel(&frame, -5, -2), sample_pixel(&frame, 0, 0));
        assert_eq!(sample_pixel(&frame, 99, 99), sample_pixel(&frame, 7, 7));
    }

    #[test]
    fn magnifier_samples_pixel_grid_at_5x_within_panel_cap() {
        // 像素级放大:源窗口 21px 直径、块 5 物理 px(有效 ~5x),与 scale 无关。
        for scale in [0.5, 1.0, 1.5, 2.0, 3.0, 4.0] {
            let layout = mag_layout(scale);
            assert_eq!(layout.half * 2 + 1, 21, "scale {scale}: 源采样窗口直径");
            assert_eq!(layout.block, 5, "scale {scale}: 放大块边长");
            assert_eq!(layout.edge, 105, "scale {scale}: 放大区边长");
            assert!(layout.edge <= MAG_MAX_EDGE);
        }
        // 面板宽度上限 200 物理 px(读数字号自动收缩)。
        for scale in [1.0, 1.5, 2.0, 4.0] {
            let panel = magnifier_rect((100, 100), (1920, 1080), scale);
            assert!(
                panel.width <= MAG_PANEL_MAX,
                "scale {scale}: panel width {} exceeds {MAG_PANEL_MAX}",
                panel.width
            );
        }
    }

    #[test]
    fn magnifier_panel_zooms_source_pixels_with_true_colors() {
        let mut frame = solid_frame(400, 300, [200, 100, 50, 255]);
        let center = ((150 * 400 + 200) * 4) as usize;
        frame.rgba[center..center + 4].copy_from_slice(&[1, 2, 3, 255]);
        let composer = Composer::new(&frame).unwrap();
        let scene = Scene {
            selection: None,
            cursor: (200, 150),
            flags: FeatureFlags::default(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let panel = magnifier_rect((200, 150), (400, 300), 1.0);
        assert!(panel.width > 0 && panel.height > 0);
        let layout = mag_layout(1.0);
        let px = panel.x + (panel.width - layout.edge) / 2;
        let py = panel.y + layout.pad;
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 400 + x as u32) * 4) as usize;
            [
                composed[i],
                composed[i + 1],
                composed[i + 2],
                composed[i + 3],
            ]
        };
        // 放大区左上角对应源 (cursor - half):原始纯色,不是压暗后的 (104,52,26)。
        assert_eq!(read(px, py), [200, 100, 50, 255]);
        // 中心块内部是光标源像素;框在块的外缘,不切开像素。
        let origin_x = px + layout.half * layout.block;
        let origin_y = py + layout.half * layout.block;
        assert_eq!(read(origin_x + 1, origin_y + 1), [1, 2, 3, 255]);
        assert_eq!(
            read(origin_x, origin_y),
            [ACCENT[0], ACCENT[1], ACCENT[2], 255]
        );
    }

    #[test]
    fn magnifier_scales_with_frame_scale_and_stays_capped() {
        let mut frame = solid_frame(600, 400, [90, 160, 220, 255]);
        frame.scale = 1.5;
        let composer = Composer::new(&frame).unwrap();
        let scene = Scene {
            selection: None,
            cursor: (300, 200),
            flags: FeatureFlags::default(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let panel = magnifier_rect((300, 200), (600, 400), 1.5);
        let layout = mag_layout(1.5);
        assert_eq!(layout.edge, 105);
        assert_eq!(layout.block, 5);
        assert!(layout.edge <= MAG_MAX_EDGE);
        assert!(panel.width <= MAG_PANEL_MAX);
        let px = panel.x + (panel.width - layout.edge) / 2;
        let py = panel.y + layout.pad;
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 600 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // 150% 下放大区仍是原始帧真实色彩(90,160,220),不是压暗值。
        assert_eq!(read(px + 1, py + 1), [90, 160, 220]);
    }

    #[test]
    fn dirty_compose_matches_full_compose_and_cursor_follow_stays_local() {
        let frame = solid_frame(640, 400, [80, 120, 40, 255]);
        let composer = Composer::new(&frame).unwrap();
        let overlay = annotation_overlay(&[], None, None, 0);
        let selection = PhysicalRect {
            x: 80,
            y: 60,
            width: 200,
            height: 120,
        };
        let selected = Scene {
            selection: Some(selection),
            cursor: (40, 40),
            flags: FeatureFlags::default(),
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let full_selected = composer.compose_with_overlay(&selected, &overlay);
        let mut dirty_buf = composer.dimmed.clone();
        let first = composer.compose_into_dirty(&selected, &overlay, &mut dirty_buf, None);
        assert_eq!(
            first,
            IntRect {
                x: 0,
                y: 0,
                width: 640,
                height: 400
            }
        );
        assert_eq!(dirty_buf, full_selected);

        let idle_a = Scene {
            selection: None,
            cursor: (80, 80),
            flags: FeatureFlags::default(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let idle_b = Scene {
            cursor: (100, 90),
            ..idle_a
        };
        let full_idle_a = composer.compose_with_overlay(&idle_a, &overlay);
        dirty_buf.copy_from_slice(&full_idle_a);
        let full_idle_b = composer.compose_with_overlay(&idle_b, &overlay);
        let rect = composer.compose_into_dirty(&idle_b, &overlay, &mut dirty_buf, Some(&idle_a));
        assert!(
            rect.width * rect.height < 640 * 400 / 4,
            "idle cursor-only dirty {rect:?}"
        );
        let pixel = |buf: &[u8], x: i32, y: i32| {
            let i = ((y as u32 * 640 + x as u32) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        };
        for y in 0..400 {
            for x in 0..640 {
                if rect.contains(x, y) {
                    assert_eq!(
                        pixel(&dirty_buf, x, y),
                        pixel(&full_idle_b, x, y),
                        "inside dirty {x},{y}"
                    );
                } else {
                    assert_eq!(
                        pixel(&dirty_buf, x, y),
                        pixel(&full_idle_a, x, y),
                        "outside dirty {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn dirty_cursor_follow_keeps_baked_annotations() {
        let frame = solid_frame(400, 300, [40, 80, 120, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 40,
            width: 160,
            height: 120,
        };
        let annotations = vec![Annotation::Rect {
            x: 60.0,
            y: 60.0,
            width: 80.0,
            height: 50.0,
            color: "#e11d48".into(),
            stroke_width: Some(4.0),
        }];
        let overlay = annotation_overlay(&annotations, None, None, 1);
        let flags = no_magnifier_flags();
        let scene_a = Scene {
            selection: Some(selection),
            cursor: (300, 40),
            flags,
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let scene_b = Scene {
            cursor: (320, 50),
            ..scene_a
        };
        let full_a = composer.compose_with_overlay(&scene_a, &overlay);
        let mut dirty_buf = composer.dimmed.clone();
        let _ = composer.compose_into_dirty(&scene_a, &overlay, &mut dirty_buf, None);
        let _ = composer.compose_into_dirty(&scene_b, &overlay, &mut dirty_buf, Some(&scene_a));
        let full_b = composer.compose_with_overlay(&scene_b, &overlay);
        let find_stroke = |buf: &[u8]| {
            for y in 60u32..110 {
                for x in 60u32..140 {
                    let i = ((y * 400 + x) * 4) as usize;
                    if buf[i] == 225 && buf[i + 1] == 29 && buf[i + 2] == 72 {
                        return Some(i);
                    }
                }
            }
            None
        };
        let i = find_stroke(&full_a).expect("rect stroke visible in full compose");
        assert_eq!(
            &dirty_buf[i..i + 3],
            &[225, 29, 72],
            "baked rect vanished after cursor follow"
        );
        assert_eq!(&dirty_buf[i..i + 3], &full_b[i..i + 3]);
    }

    #[test]
    fn magnifier_flag_disables_panel() {
        let frame = solid_frame(400, 300, [200, 100, 50, 255]);
        let composer = Composer::new(&frame).unwrap();
        let cursor = (200, 150);
        let scene = |flags| Scene {
            selection: None,
            cursor,
            flags,
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let panel = magnifier_rect(cursor, (400, 300), 1.0);
        let layout = mag_layout(1.0);
        let px = panel.x + (panel.width - layout.edge) / 2;
        let py = panel.y + layout.pad;
        let read = |buf: &[u8], x: i32, y: i32| {
            let i = ((y as u32 * 400 + x as u32) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2]]
        };
        let on = composer.compose(&scene(FeatureFlags::default()));
        assert_eq!(read(&on, px, py), [200, 100, 50]);
        let off = composer.compose(&scene(no_magnifier_flags()));
        // 关闭后该处是压暗原像素,不再画放大镜面板。
        assert_eq!(read(&off, px, py), [104, 52, 26]);
    }

    #[test]
    fn handles_have_white_core_and_accent_ring() {
        let frame = solid_frame(320, 200, [10, 200, 90, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 161,
            height: 91,
        };
        let scene = Scene {
            selection: Some(selection),
            cursor: (300, 190),
            flags: no_magnifier_flags(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 320 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // SE 手柄 (200,120):白芯 + 蓝环(scale 1.0 外环半径 5)。
        let (hx, hy) = handle_anchor(selection, HandleKind::SouthEast);
        assert_eq!(read(hx, hy), [255, 255, 255]);
        assert_eq!(read(hx + 4, hy), [ACCENT[0], ACCENT[1], ACCENT[2]]);
        assert_eq!(read(hx, hy - 4), [ACCENT[0], ACCENT[1], ACCENT[2]]);
    }

    #[test]
    fn badge_panel_is_bright_and_above_selection() {
        let frame = solid_frame(320, 200, [20, 20, 20, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 80,
            width: 100,
            height: 50,
        };
        let scene = Scene {
            selection: Some(selection),
            cursor: (300, 190),
            flags: no_magnifier_flags(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 320 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // 徽标在选区上方,内部是浮层底色。
        let badge_y = 80 - (text::line_height(BADGE_FONT).ceil() as i32) - BADGE_PAD_Y * 2
            + BADGE_PAD_Y
            - BADGE_MARGIN;
        let pixel = read(60, badge_y.max(2));
        assert!(
            near_chrome(pixel),
            "badge interior should be the chrome surface, got {pixel:?}"
        );
    }

    /// 右键菜单保持现状:动作集/过滤与菜单几何不变。
    #[test]
    fn menu_items_stay_on_capture_action_set() {
        let default_items = vec![
            SelectionAction::Copy,
            SelectionAction::Save,
            SelectionAction::Pin,
            SelectionAction::Annotate,
            SelectionAction::Ocr,
            SelectionAction::Cancel,
        ];
        assert_eq!(menu_items(FeatureFlags::default()), default_items);
        let flags = FeatureFlags {
            ocr_entry: false,
            pin_entry: false,
            ..FeatureFlags::default()
        };
        let items = menu_items(flags);
        assert!(!items.contains(&SelectionAction::Ocr));
        assert!(!items.contains(&SelectionAction::Pin));
        assert_eq!(
            items,
            vec![
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Annotate,
                SelectionAction::Cancel,
            ]
        );
        // 关闭操作条复制/保存/贴图后,菜单不再出现对应项;标注与取消恒在。
        let toolbar_off = FeatureFlags {
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ocr_entry: false,
            ..FeatureFlags::default()
        };
        let filtered = vec![SelectionAction::Annotate, SelectionAction::Cancel];
        assert_eq!(menu_items(toolbar_off), filtered);
        // 贴图需 toolbar_pin 与 pin_entry 同时开启。
        let pin_entry_only = FeatureFlags {
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        assert!(!menu_items(pin_entry_only).contains(&SelectionAction::Pin));
        let toolbar_pin_only = FeatureFlags {
            pin_entry: false,
            ..FeatureFlags::default()
        };
        assert!(!menu_items(toolbar_pin_only).contains(&SelectionAction::Pin));
    }

    #[test]
    fn menu_geometry_has_touch_targets_and_cancel_separator() {
        let items = menu_items(FeatureFlags::default());
        assert_eq!(items.len(), 6);
        let metrics = metrics_1();
        let row = menu_row_width(metrics, &items);
        let panel = menu_panel(metrics, (50, 50), (800, 600), &items);
        assert!(row < MENU_ITEM_W);
        assert_eq!(panel.width, row + MENU_PAD * 2);
        assert_eq!(
            panel.height,
            items.len() as i32 * MENU_ITEM_H + MENU_PAD * 2 + MENU_SEPARATOR_H
        );
        let rects = menu_item_rects(metrics, panel, &items);
        for (_, rect) in &rects {
            assert_eq!(rect.width, row);
            assert_eq!(rect.height, MENU_ITEM_H);
            assert_eq!(rect.x, panel.x + MENU_PAD);
        }
        // 分隔线:紧贴倒数第二项底部,末项(取消)在线下 1px。
        let sep_y = menu_separator_y(metrics_1(), panel, &items);
        assert_eq!(rects[4].1.bottom(), sep_y);
        assert_eq!(rects[5].1.y, sep_y + MENU_SEPARATOR_H);
        // 图标/文字内边距。
        assert_eq!(MENU_ICON_CX, 20);
        assert_eq!(MENU_TEXT_X, 44);
    }

    /// 「更多」面板几何:与右键菜单同构,向上展开且钳制在屏幕内。
    #[test]
    fn more_panel_geometry_matches_menu_and_stays_on_screen() {
        let metrics = metrics_1();
        let items = more_panel_buttons(FeatureFlags::default());
        let anchor = IntRect {
            x: 700,
            y: 600,
            width: BAR_BUTTON,
            height: BAR_BUTTON,
        };
        let panel = more_panel(metrics, anchor, None, (1280, 800), &items);
        let row = menu_row_width(metrics, &items);
        assert!(row < metrics.menu_item_w, "短标签不应撑满旧的 168px 槽");
        assert_eq!(panel.width, row + metrics.menu_pad * 2);
        assert_eq!(
            panel.height,
            items.len() as i32 * metrics.menu_item_h + metrics.menu_pad * 2
        );
        assert_eq!(panel.bottom(), anchor.y, "面板底边对齐「更多」按钮底边");
        assert_eq!(panel.right(), anchor.right(), "面板右缘对齐按钮右缘");
        let rects = more_item_rects(metrics, panel, &items);
        assert_eq!(rects.len(), items.len());
        for (index, (_, rect)) in rects.iter().enumerate() {
            assert_eq!(rect.width, row);
            assert_eq!(rect.height, metrics.menu_item_h);
            assert_eq!(rect.x, panel.x + metrics.menu_pad);
            assert_eq!(
                rect.y,
                panel.y + metrics.menu_pad + index as i32 * metrics.menu_item_h
            );
        }
        // 贴屏幕左上角的「更多」按钮:面板翻转后仍在屏内(短列表场景:
        // 关闭即时标注时「更多」只含贴图/取字)。
        let short = more_panel_buttons(FeatureFlags {
            inline_annotation: false,
            ..FeatureFlags::default()
        });
        assert_eq!(short.len(), 2);
        let tight = IntRect {
            x: 4,
            y: 4,
            width: BAR_BUTTON,
            height: BAR_BUTTON,
        };
        let flipped = more_panel(metrics, tight, None, (200, 120), &short);
        assert!(flipped.x >= 0 && flipped.y >= 0);
        assert!(flipped.right() <= 200 && flipped.bottom() <= 120);
    }

    /// 顶边横条(y=0)向上弹出的「更多」面板不得与横条重叠:放得下时下移到
    /// 横条之下且仍在屏内;屏高不足时保留屏内位置(命中顺序兜底)。
    #[test]
    fn more_panel_avoids_overlapping_top_edge_toolbar() {
        let metrics = metrics_1();
        let items = more_panel_buttons(FeatureFlags::default());
        let toolbar = IntRect {
            x: 775,
            y: 0,
            width: 9 * BAR_BUTTON,
            height: BAR_BUTTON,
        };
        let anchor = IntRect {
            x: toolbar.right() - BAR_BUTTON,
            y: 0,
            width: BAR_BUTTON,
            height: BAR_BUTTON,
        };
        let panel = more_panel(metrics, anchor, Some(toolbar), (1920, 1080), &items);
        assert!(
            !intersects(panel, toolbar),
            "面板应下移到横条之下: {panel:?}"
        );
        assert!(panel.bottom() <= 1080, "面板必须在屏内: {panel:?}");
        // 屏高不足以避开时:保持屏内,命中顺序(面板优先)保证可点。
        // (默认「更多」11 行 = 408px;横条之下需 448px,420 高的屏放不下。)
        let short_screen = more_panel(metrics, anchor, Some(toolbar), (1920, 420), &items);
        assert!(short_screen.y >= 0 && short_screen.bottom() <= 420);
        assert!(intersects(short_screen, toolbar));
    }

    /// 合成像素与统一横条 hitbox 布局一致(亮铬面板 + accent 复制按钮)。
    #[test]
    fn composed_frame_matches_hitbox_layout() {
        let frame = solid_frame(320, 200, [10, 200, 90, 255]);
        let dimmed = [5u8, 104, 46, 255];
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 161,
            height: 91,
        };
        // 关闭放大镜:面板尺寸随读数变化,本测试只核对选区/横条 hitbox 布局。
        let flags = no_magnifier_flags();
        // 统一横条位于选区下方(compose 无 overlay,布局按 text_input=false 计算)。
        let toolbar = unified_toolbar(composer.metrics, selection, (320, 200), flags, false)
            .expect("toolbar");
        assert!(toolbar.panel.y >= selection.y as i32 + selection.height as i32);
        let copy_rect = toolbar
            .buttons
            .iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .map(|(_, rect)| *rect)
            .expect("copy button");
        // 光标悬停在首个工具按钮上(tooltip 出现在其上方/右侧),
        // 远离复制按钮,排除 hover 软底/tooltip 干扰。
        let first_rect = toolbar.buttons.first().map(|(_, rect)| *rect).unwrap();
        let scene = Scene {
            selection: Some(selection),
            cursor: first_rect.center(),
            flags,
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 320 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // 选区开洞:选区内部是原始像素。
        assert_eq!(read(190, 60), [10, 200, 90]);
        // 选区外是暗幕(避开所有面板)。
        assert_eq!(read(10, 190), [dimmed[0], dimmed[1], dimmed[2]]);
        // 复制按钮不再单独铺强调色。
        let (cx, cy) = copy_rect.center();
        assert_ne!(
            read(cx, cy + copy_rect.height / 2 - 6),
            [ACCENT[0], ACCENT[1], ACCENT[2]]
        );
        // 横条面板内部是浮层底(取左缘内 2px,避开图标与分隔线)。
        let probe = read(
            toolbar.panel.x + 2,
            toolbar.panel.y + toolbar.panel.height / 2,
        );
        assert!(
            near_chrome(probe),
            "toolbar panel should be the chrome surface, got {probe:?}"
        );
        // 描边:选区左边框(避开手柄)为与网页相同的明暗强调色。
        let stroke = selection_accent();
        assert_eq!(read(41, 90), [stroke[0], stroke[1], stroke[2]]);
    }

    #[test]
    fn hover_tooltip_prefers_above_and_stays_on_screen() {
        let anchor = IntRect {
            x: 100,
            y: 80,
            width: 40,
            height: 40,
        };
        let panel = place_tooltip(anchor, 48, 24, 6, (320, 200)).unwrap();
        assert_eq!(panel.y, 80 - 24 - 6);
        assert!(panel.x >= 0 && panel.right() <= 320);
        let edge = IntRect {
            x: 0,
            y: 129,
            width: 40,
            height: 40,
        };
        let clamped = place_tooltip(edge, 48, 24, 6, (320, 200)).unwrap();
        assert_eq!(clamped.x, 0);
        assert_eq!(clamped.bottom(), edge.y - 6);
        let tight = IntRect {
            x: 4,
            y: 4,
            width: 24,
            height: 24,
        };
        let flipped = place_tooltip(tight, 60, 22, 6, (200, 120)).unwrap();
        assert!(flipped.x >= 0 && flipped.y >= 0);
        assert!(flipped.right() <= 200 && flipped.bottom() <= 120);
    }

    #[test]
    fn hover_tooltip_paints_chrome_near_toolbar_button() {
        let frame = solid_frame(320, 200, [10, 200, 90, 255]);
        let composer = Composer::new(&frame).unwrap();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 161,
            height: 91,
        };
        let flags = no_magnifier_flags();
        // compose 无 overlay:布局按 text_input=false 计算,与 hovered_icon 同源。
        let toolbar = unified_toolbar(composer.metrics, selection, (320, 200), flags, false)
            .expect("toolbar");
        let (_, rect) = toolbar.buttons.first().copied().unwrap();
        let (cx, cy) = rect.center();
        let mut scene = Scene {
            selection: Some(selection),
            cursor: (cx, cy),
            flags,
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let (action, anchor) = composer
            .hovered_icon(&scene, None, 320, 200)
            .expect("hovered icon");
        let hovered = composer.compose(&scene);
        scene.cursor = (8, 8);
        let idle = composer.compose(&scene);
        // 提示面板应按 place_tooltip 的几何出现;核对其内部亮铬像素显著增多。
        let metrics = composer.metrics;
        let label = action_label(action);
        let font = metrics.menu_font;
        let text_w = text::measure_width(&label, font).unwrap_or(0.0);
        let pad_x = metrics.badge_pad_x.max(8);
        let pad_y = metrics.badge_pad_y.max(4);
        let tip_w = (text_w.ceil() as i32 + pad_x * 2).max(24);
        let tip_h = text::line_height(font).ceil() as i32 + pad_y * 2;
        let tip = place_tooltip(
            anchor,
            tip_w,
            tip_h,
            metrics.badge_margin.max(6),
            (320, 200),
        )
        .expect("tooltip fits");
        assert!(
            tip.bottom() <= anchor.y,
            "tooltip should stay above the toolbar: {tip:?}"
        );
        let bright_in = |buf: &[u8], region: IntRect| {
            let mut n = 0usize;
            for y in region.y.max(0)..region.bottom().min(200) {
                for x in region.x.max(0)..region.right().min(320) {
                    let i = ((y as u32 * 320 + x as u32) * 4) as usize;
                    if near_chrome([buf[i], buf[i + 1], buf[i + 2]]) {
                        n += 1;
                    }
                }
            }
            n
        };
        let interior = inset(tip, 2);
        assert!(
            bright_in(&hovered, interior) > bright_in(&idle, interior) + 100,
            "tooltip interior bright pixels: hover {} idle {}",
            bright_in(&hovered, interior),
            bright_in(&idle, interior)
        );
    }

    #[test]
    fn chrome_metrics_derive_sizes_and_pinned_hit_radii_from_scale() {
        // 125%–250%:全部 chrome 尺寸/字号由 scale 派生,命中半径钉死物理下限。
        for scale in [1.0_f32, 1.25, 1.5, 2.0, 2.5] {
            let metrics = ChromeMetrics::for_scale(scale);
            assert_eq!(metrics.scale, scale);
            assert_eq!(
                metrics.bar_button,
                (BAR_BUTTON as f32 * scale).round() as i32,
                "scale {scale}: 横条按钮"
            );
            assert_eq!(
                metrics.menu_item_w,
                (MENU_ITEM_W as f32 * scale).round() as i32
            );
            assert_eq!(
                metrics.menu_item_h,
                (MENU_ITEM_H as f32 * scale).round() as i32
            );
            assert_eq!(metrics.menu_font, (MENU_FONT * scale).round());
            assert_eq!(metrics.badge_font, (BADGE_FONT * scale).round());
            // 命中半径随 scale 放大且不低于 1.0 基准。
            assert!(metrics.handle_hit_radius >= HANDLE_HIT_RADIUS);
            assert!(metrics.handle_radius >= 5);
            assert!(metrics.edge_hit_radius >= EDGE_HIT_RADIUS);
            assert!(metrics.bar_button >= BAR_BUTTON);
            assert!(metrics.menu_item_h >= MENU_ITEM_H);
            // 文字与图标不裁切:菜单项容得下放大后的字高/文字,按钮容得下图标。
            if text::ui_font().is_some() {
                let label = action_label(SelectionAction::Cancel);
                let width = text::measure_width(&label, metrics.menu_font).unwrap();
                let row = menu_row_width(metrics, &[SelectionAction::Cancel]);
                assert!(
                    metrics.menu_text_x + width.ceil() as i32 + metrics.menu_pad <= row,
                    "scale {scale}: 菜单文字被裁切"
                );
                assert!(text::line_height(metrics.menu_font) <= metrics.menu_item_h as f32);
            }
            assert!(metrics.bar_icon <= metrics.bar_button);
            assert!(metrics.menu_icon <= metrics.menu_item_h);
            assert!(metrics.menu_pad * 2 < metrics.menu_item_w);
        }
        // 低于 1.0 与非法值都回到 1.0 基准,不缩水。
        for scale in [0.5_f32, 0.0, -2.0, f32::NAN, f32::INFINITY] {
            let metrics = ChromeMetrics::for_scale(scale);
            assert_eq!(metrics.bar_button, BAR_BUTTON, "scale {scale}");
            assert_eq!(
                metrics.handle_hit_radius, HANDLE_HIT_RADIUS,
                "scale {scale}"
            );
            assert_eq!(metrics.menu_font, MENU_FONT, "scale {scale}");
        }
        // 超过 4.0 钳制,避免 chrome 占据整屏。
        assert_eq!(ChromeMetrics::for_scale(8.0).bar_button, BAR_BUTTON * 4);
    }

    #[test]
    fn chrome_layout_and_hit_share_scaled_metrics() {
        for scale in [1.5_f32, 2.0] {
            let metrics = ChromeMetrics::for_scale(scale);
            let selection = PhysicalRect {
                x: 40,
                y: 60,
                width: 100,
                height: 80,
            };
            // 布局尺寸按 metrics;命中矩形与布局同源。
            let toolbar = unified_toolbar(
                metrics,
                selection,
                (800, 600),
                FeatureFlags::default(),
                true,
            )
            .expect("toolbar");
            assert_eq!(toolbar.panel.height, metrics.bar_button, "scale {scale}");
            assert_eq!(
                toolbar.panel.width,
                toolbar.buttons.len() as i32 * metrics.bar_button,
                "scale {scale}"
            );
            assert!(toolbar
                .buttons
                .iter()
                .all(|(_, rect)| rect.width == metrics.bar_button
                    && rect.height == metrics.bar_button));
            // 菜单几何按 metrics;分隔线与项底对齐。
            let items = menu_items(FeatureFlags::default());
            let menu = menu_panel(metrics, (50, 50), (800, 600), &items);
            let row = menu_row_width(metrics, &items);
            assert_eq!(menu.width, row + metrics.menu_pad * 2);
            assert_eq!(
                menu.height,
                items.len() as i32 * metrics.menu_item_h
                    + metrics.menu_pad * 2
                    + metrics.menu_separator_h
            );
            let mrects = menu_item_rects(metrics, menu, &items);
            for (_, rect) in &mrects {
                assert_eq!(rect.width, row);
                assert_eq!(rect.height, metrics.menu_item_h);
                assert_eq!(rect.x, menu.x + metrics.menu_pad);
            }
            let sep_y = menu_separator_y(metrics, menu, &items);
            assert_eq!(mrects[4].1.bottom(), sep_y);
            assert_eq!(mrects[5].1.y, sep_y + metrics.menu_separator_h);
            // 「更多」面板几何随 metrics 派生。
            let more = more_panel_buttons(FeatureFlags::default());
            let m_panel = more_panel(
                metrics,
                toolbar.buttons.last().unwrap().1,
                Some(toolbar.panel),
                (800, 600),
                &more,
            );
            assert_eq!(
                m_panel.width,
                menu_row_width(metrics, &more) + metrics.menu_pad * 2
            );
            // 手柄/边命中半径按 metrics 派生(角手柄优先)。
            let rect = PhysicalRect {
                x: 100,
                y: 100,
                width: 120,
                height: 80,
            };
            let h = metrics.handle_hit_radius;
            assert_eq!(
                handle_hit(metrics, rect, 100 + h, 100 + h),
                Some(HandleKind::NorthWest),
                "scale {scale}"
            );
            assert_eq!(handle_hit(metrics, rect, 100 + h + 1, 100 + h + 1), None);
            let e = metrics.edge_hit_radius;
            assert_eq!(
                edge_hit(metrics, rect, 160, 100 - e),
                Some(EdgeKind::North),
                "scale {scale}"
            );
            assert_eq!(edge_hit(metrics, rect, 160, 100 - e - 1), None);
            assert_eq!(
                edge_hit(metrics, rect, 219 + e, 140),
                Some(EdgeKind::East),
                "scale {scale}"
            );
        }
    }

    #[test]
    fn composed_chrome_uses_scaled_metrics() {
        let mut frame = solid_frame(640, 400, [10, 200, 90, 255]);
        frame.scale = 1.5;
        let composer = Composer::new(&frame).unwrap();
        let metrics = ChromeMetrics::for_scale(1.5);
        let flags = no_magnifier_flags();
        let selection = PhysicalRect {
            x: 40,
            y: 30,
            width: 200,
            height: 150,
        };
        let scene = Scene {
            selection: Some(selection),
            cursor: (600, 390),
            flags,
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 640 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // 复制按钮与其他按钮一样,不再画强调色短杠。
        let toolbar = unified_toolbar(metrics, selection, (640, 400), flags, false).unwrap();
        let copy_rect = toolbar
            .buttons
            .iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .map(|(_, rect)| *rect)
            .unwrap();
        let (cx, cy) = copy_rect.center();
        let inset = metrics.bar_icon / 2 + 2;
        assert_ne!(read(cx, cy + inset), [ACCENT[0], ACCENT[1], ACCENT[2]]);
        // 手柄视觉半径也随 scale 放大:距锚点 1.0 基准半径外、缩放半径内仍为强调色。
        let (hx, hy) = handle_anchor(selection, HandleKind::SouthEast);
        assert_eq!(read(hx, hy), [255, 255, 255]);
        assert_eq!(read(hx + 7, hy), [ACCENT[0], ACCENT[1], ACCENT[2]]);
        // 菜单:scale 派生的面板矩形上能看到亮铬底。
        let menu_items = menu_items(flags);
        let scene = Scene {
            selection: Some(selection),
            cursor: (600, 390),
            flags,
            toolbar_visible: false,
            menu_open: true,
            menu_anchor: (20, 20),
            more_open: false,
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 640 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        let panel = menu_panel(metrics, (20, 20), (640, 400), &menu_items);
        let probe_x = panel.right() - metrics.menu_pad - 2;
        let probe_y = panel.y + metrics.menu_pad + 2;
        let pixel = read(probe_x, probe_y);
        assert!(
            near_chrome(pixel),
            "scaled menu panel should be the chrome surface, got {pixel:?}"
        );
    }
}
