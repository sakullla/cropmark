//! 平台无关的选区像素合成器。
//!
//! 在冻结帧(`capture::buffer::Frame`)的 RGBA 位图上合成:暗幕+选区开洞、
//! 青绿描边、白芯青绿环手柄、亮铬尺寸徽标、放大镜(原始帧全分辨率采样 +
//! 十字准星 + 单行色值读数)、亮铬操作条与右键菜单。所有浮层 UI 统一亮暖白底
//! 深墨字(亮铬体系),在暗色 scrim 与暗桌面上保持可读。布局/hitbox 函数是
//! 纯几何,状态机与绘制共用同一份,保证命中判定与合成输出一致。
//! 不含任何窗口/平台代码。

use super::text;
use super::{EdgeKind, FeatureFlags, HandleKind, Scene, SelectionAction};
use crate::capture::buffer::{validate_frame, Frame};
use crate::capture::error::CaptureError;
use crate::capture::geometry::PhysicalRect;
use crate::i18n;

// ---- 配色单一 authority:亮铬浮层体系(暗 scrim 上的亮面板)。----

/// 青绿强调色 #2dd4bf(选区描边/手柄环/十字准星),与现 Windows 原生路径 BGRA
/// [0xBF,0xD4,0x2D] 同色。
pub const ACCENT: [u8; 4] = [0x2D, 0xD4, 0xBF, 255];
/// 深青绿 #0f766e:图标轨圆形按钮底、亮铬上的激活/hover 文字与强调。
const ACCENT_DEEP: [u8; 4] = [0x0F, 0x76, 0x6E, 255];
/// hover 深 accent #115e59:图标轨圆形按钮 hover 底。
const ACCENT_DARK: [u8; 4] = [0x11, 0x5E, 0x59, 255];
/// 图标字形白色(accent 底上的纯图标)。
const ICON_INK: [u8; 4] = [255, 255, 255, 255];
/// 亮暖白浮层底 #fffcf7 @ 97%:尺寸徽标/操作条/菜单/放大镜统一底色。
const CHROME_BG: [u8; 4] = [255, 252, 247, 247];
/// 深墨字 #1c1917:亮铬上的文字。
const CHROME_TEXT: [u8; 4] = [0x1C, 0x19, 0x17, 255];
/// 1px 细描边 rgba(28,25,23,0.14)。
const CHROME_BORDER: [u8; 4] = [28, 25, 23, 36];
/// 底部 2px 深色 offset,引擎画不了真阴影,用它模拟浮层层次。
const CHROME_SHADOW: [u8; 4] = [20, 18, 16, 70];
/// 激活/hover 软底 rgba(15,118,110,0.12)。
const ACTIVE_BG: [u8; 4] = [15, 118, 110, 31];
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

// ---- 图标轨(操作条)与右键菜单几何:与引擎 hitbox 共用;以下为 1.0 基准
// (逻辑)尺寸,实际物理尺寸统一经 `ChromeMetrics::for_scale` 按冻结帧 scale 派生。----
/// 图标轨圆形按钮直径(触达标准 40px)。
const RAIL_BUTTON: i32 = 40;
/// 图标轨相邻按钮间距。
const RAIL_GAP: i32 = 6;
/// 竖排轨(右/左)与选区间距。
const RAIL_MARGIN_V: i32 = 10;
/// 底部横排与选区间距。
const RAIL_MARGIN_H: i32 = 8;

const MENU_ITEM_W: i32 = 168;
const MENU_ITEM_H: i32 = 36;
/// 菜单容器内边距。
const MENU_PAD: i32 = 6;
/// 菜单项图标中心相对项左缘的偏移。
const MENU_ICON_CX: i32 = 22;
/// 菜单项文字起点(图标区之后)。
const MENU_TEXT_X: i32 = 38;
/// 「取消」前的 1px 分隔线高度。
const MENU_SEPARATOR_H: i32 = 1;
const MENU_FONT: f32 = 14.0;
/// 菜单项 hover 软底圆角(1.0 基准)。
const MENU_HOVER_RADIUS: i32 = 8;
/// 菜单项图标外接盒边长(1.0 基准)。
const MENU_ICON: i32 = 14;
/// 图标轨按钮图标外接盒边长(1.0 基准)。
const TOOLBAR_ICON: i32 = 18;
/// 浮层圆角逻辑半径(× scale,钳制 8..=32)。
const PANEL_RADIUS: f32 = 10.0;

// 触达几何契约(1.0 基准):图标轨 40px 圆形按钮;菜单项高 36、min-width 168
// (编译期断言)。实际 chrome 尺寸永不低于该基准(`ChromeMetrics` 钉死下限)。
const _: () = assert!(RAIL_BUTTON >= 40 && MENU_ITEM_H >= 36 && MENU_ITEM_W >= 168);

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
    /// 图标轨圆形按钮直径。
    pub rail_button: i32,
    pub rail_gap: i32,
    pub rail_margin_v: i32,
    pub rail_margin_h: i32,
    pub menu_item_w: i32,
    pub menu_item_h: i32,
    pub menu_pad: i32,
    pub menu_icon_cx: i32,
    pub menu_text_x: i32,
    pub menu_separator_h: i32,
    pub menu_hover_radius: i32,
    pub menu_font: f32,
    pub menu_icon: i32,
    pub toolbar_icon: i32,
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
            rail_button: scaled(RAIL_BUTTON),
            rail_gap: scaled(RAIL_GAP),
            rail_margin_v: scaled(RAIL_MARGIN_V),
            rail_margin_h: scaled(RAIL_MARGIN_H),
            menu_item_w: scaled(MENU_ITEM_W),
            menu_item_h: scaled(MENU_ITEM_H),
            menu_pad: scaled(MENU_PAD),
            menu_icon_cx: scaled(MENU_ICON_CX),
            menu_text_x: scaled(MENU_TEXT_X),
            menu_separator_h: scaled(MENU_SEPARATOR_H),
            menu_hover_radius: scaled(MENU_HOVER_RADIUS),
            menu_font: MENU_FONT * scale,
            menu_icon: scaled(MENU_ICON),
            toolbar_icon: scaled(TOOLBAR_ICON),
            handle_radius: scaled(HANDLE_RADIUS as i32),
            handle_hit_radius: scaled(HANDLE_HIT_RADIUS),
            edge_hit_radius: scaled(EDGE_HIT_RADIUS),
            badge_font: BADGE_FONT * scale,
            badge_pad_x: scaled(BADGE_PAD_X),
            badge_pad_y: scaled(BADGE_PAD_Y),
            badge_margin: scaled(BADGE_MARGIN),
            panel_radius: ((PANEL_RADIUS * scale).round() as i32).clamp(8, 32),
        }
    }
}

/// 放大镜:源采样窗口 (2*MAG_HALF+1)=23 物理 px 直径,MAG_ZOOM=8 倍最近邻
/// 放大(有效倍率 ~8x,像素格清晰可辨),放大区边长 23×8=184 ≤ MAG_MAX_EDGE。
const MAG_HALF: i32 = 11;
pub const MAG_ZOOM: i32 = 8;
const MAG_MAX_EDGE: i32 = 220;
/// 放大镜面板宽度上限(物理 px);读数字号过宽时自动收缩以守住上限。
const MAG_PANEL_MAX: i32 = 200;
const MAG_FONT: f32 = 11.0;
const MAG_OFFSET: i32 = 18;
const MAG_PAD: i32 = 4;
/// 单行读数 pill 的最坏情况宽度样本(面板宽度据此预留,与实际像素无关)。
const MAG_READOUT_SAMPLE: &str = "#FFFFFF R255 G255 B255 · 99999, 99999";

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
    })
}

/// 操作条按钮集(随 FeatureFlags 变化;复制/保存/贴图对应三个开关)。
pub fn toolbar_buttons(flags: FeatureFlags) -> Vec<SelectionAction> {
    let mut buttons = Vec::new();
    if flags.toolbar_copy {
        buttons.push(SelectionAction::Copy);
    }
    if flags.toolbar_save {
        buttons.push(SelectionAction::Save);
    }
    if flags.toolbar_pin {
        buttons.push(SelectionAction::Pin);
    }
    buttons
}

/// 右键菜单动作集:复制/保存/贴图/标注/取字/取消;贴图与取字受开关控制。
pub fn menu_items(flags: FeatureFlags) -> Vec<SelectionAction> {
    let mut items = vec![SelectionAction::Copy, SelectionAction::Save];
    if flags.pin_entry {
        items.push(SelectionAction::Pin);
    }
    items.push(SelectionAction::Annotate);
    if flags.ocr_entry {
        items.push(SelectionAction::Ocr);
    }
    items.push(SelectionAction::Cancel);
    items
}

/// 底部横排网格:按选区宽度决定每行按钮数,折行为多行居中。
/// 返回 (列数, 行数, 网格宽, 网格高)。
fn rail_grid(metrics: ChromeMetrics, selection_w: i32, count: i32) -> (i32, i32, i32, i32) {
    let button = metrics.rail_button;
    let gap = metrics.rail_gap;
    let per_row = ((selection_w + gap) / (button + gap)).max(1);
    let rows = (count + per_row - 1) / per_row;
    let cols = (count + rows - 1) / rows;
    let grid_w = cols * button + (cols - 1) * gap;
    let grid_h = rows * button + (rows - 1) * gap;
    (cols, rows, grid_w, grid_h)
}

/// 图标轨面板矩形:每次合成按「选区矩形 + 屏幕」动态计算避让链——
/// 选区右外侧竖排轨(垂直居中,间距 10px)→ 底部水平排(选区下外侧居中,
/// 间距 8px,选区宽度不足时自动折行)→ 左侧竖排轨 → 兜底底部钳制。
/// 任何分支都与选区边框保持间距,不压边框线。
pub fn toolbar_panel(
    metrics: ChromeMetrics,
    selection: PhysicalRect,
    screen: (u32, u32),
    buttons: &[SelectionAction],
) -> Option<IntRect> {
    if buttons.is_empty() {
        return None;
    }
    let count = buttons.len() as i32;
    let sel = IntRect::from(selection);
    let button = metrics.rail_button;
    // 右侧竖排单列轨。
    let rail_h = count * button + (count - 1) * metrics.rail_gap;
    let max_rail_y = (screen.1 as i32 - rail_h).max(0);
    let rail_y = (sel.y + sel.height / 2 - rail_h / 2).clamp(0, max_rail_y);
    let right_x = sel.right() + metrics.rail_margin_v;
    if right_x + button <= screen.0 as i32 {
        return Some(IntRect {
            x: right_x,
            y: rail_y,
            width: button,
            height: rail_h,
        });
    }
    // 底部水平排(按选区宽度折行,整体居中于选区)。
    let (_, _, grid_w, grid_h) = rail_grid(metrics, sel.width, count);
    let below_y = sel.bottom() + metrics.rail_margin_h;
    let grid_x = || (sel.x + sel.width / 2 - grid_w / 2).clamp(0, (screen.0 as i32 - grid_w).max(0));
    if below_y + grid_h <= screen.1 as i32 {
        return Some(IntRect {
            x: grid_x(),
            y: below_y,
            width: grid_w,
            height: grid_h,
        });
    }
    // 左侧竖排单列轨。
    let left_x = sel.x - metrics.rail_margin_v - button;
    if left_x >= 0 {
        return Some(IntRect {
            x: left_x,
            y: rail_y,
            width: button,
            height: rail_h,
        });
    }
    // 兜底:底部横排位置钳制屏内。
    Some(IntRect {
        x: grid_x(),
        y: below_y.clamp(0, (screen.1 as i32 - grid_h).max(0)),
        width: grid_w,
        height: grid_h,
    })
}

/// 面板内逐按钮矩形,顺序与 `buttons` 一致(与绘制共用,保证 hitbox 一致)。
/// 竖排轨为单列;水平排按面板宽度折行,每行居中(末行不足一行也居中)。
pub fn toolbar_button_rects(
    metrics: ChromeMetrics,
    panel: IntRect,
    buttons: &[SelectionAction],
) -> Vec<(SelectionAction, IntRect)> {
    let count = buttons.len() as i32;
    let button = metrics.rail_button;
    let gap = metrics.rail_gap;
    let mut rects = Vec::with_capacity(buttons.len());
    if panel.height > panel.width {
        // 竖排单列轨。
        for (index, action) in buttons.iter().enumerate() {
            rects.push((
                *action,
                IntRect {
                    x: panel.x,
                    y: panel.y + index as i32 * (button + gap),
                    width: button,
                    height: button,
                },
            ));
        }
        return rects;
    }
    // 水平排:由面板宽度反推列数,逐行居中。
    let cols = ((panel.width + gap) / (button + gap)).max(1);
    let rows = (count + cols - 1) / cols;
    for row in 0..rows {
        let row_count = (count - row * cols).min(cols);
        let row_w = row_count * button + (row_count - 1) * gap;
        let x0 = panel.x + (panel.width - row_w) / 2;
        let y = panel.y + row * (button + gap);
        for col in 0..row_count {
            let index = (row * cols + col) as usize;
            rects.push((
                buttons[index],
                IntRect {
                    x: x0 + col * (button + gap),
                    y,
                    width: button,
                    height: button,
                },
            ));
        }
    }
    rects
}

pub fn menu_panel(
    metrics: ChromeMetrics,
    anchor: (i32, i32),
    screen: (u32, u32),
    items: &[SelectionAction],
) -> IntRect {
    let width = metrics.menu_item_w + metrics.menu_pad * 2;
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
                    width: metrics.menu_item_w,
                    height: metrics.menu_item_h,
                },
            )
        })
        .collect()
}

/// 「取消」分隔线的 y 坐标(菜单至少含取消项时才有意义)。
pub fn menu_separator_y(
    metrics: ChromeMetrics,
    panel: IntRect,
    items: &[SelectionAction],
) -> i32 {
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
    // 像素级放大:块边长固定 8 物理 px(有效倍率 ~8x),源窗口 23px 直径。
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
    let mut font = MAG_FONT * scale;
    let measure = |f: f32| {
        text::measure_width(MAG_READOUT_SAMPLE, f)
            .unwrap_or(f * 0.62 * MAG_READOUT_SAMPLE.chars().count() as f32)
    };
    let sample_w = measure(font);
    if sample_w > inner && sample_w > 0.0 {
        font = (font * inner / sample_w).max(7.0);
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
    format!(
        "{} × {} · {}, {}",
        rect.width, rect.height, rect.x, rect.y
    )
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

/// 放大镜单行读数:HEX + 紧凑 RGB + 坐标,例如 "#2DD4BF R45 G212 B191 · 32, 32"。
pub fn magnifier_readout_line(cursor: (i32, i32), pixel: [u8; 4]) -> String {
    format!(
        "{} R{} G{} B{} · {}, {}",
        hex_readout(pixel),
        pixel[0],
        pixel[1],
        pixel[2],
        cursor.0,
        cursor.1
    )
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

/// CPU 合成器:持有冻结帧原件与预计算的暗幕帧;chrome 尺寸随 frame.scale 等比。
pub struct Composer {
    width: u32,
    height: u32,
    scale: f32,
    /// 全部 chrome 的物理尺寸(布局/绘制/命中同源,ADR-15)。
    metrics: ChromeMetrics,
    original: Vec<u8>,
    dimmed: Vec<u8>,
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

    /// 就地合成;`out` 长度必须与冻结帧一致(先整体写为暗幕)。
    /// 合成顺序:暗幕→开洞→描边→手柄→徽标→操作条/菜单→放大镜(最顶)。
    pub fn compose_into(&self, scene: &Scene, out: &mut [u8]) {
        out.copy_from_slice(&self.dimmed);
        let (w, h) = (self.width, self.height);
        if let Some(selection) = scene.selection {
            punch_hole(out, &self.original, w as usize, selection);
            outline_selection(out, w, h, selection);
            self.draw_handles(out, w, h, selection);
            self.draw_size_badge(out, w, h, selection);
            if scene.toolbar_visible {
                self.draw_toolbar(out, w, h, selection, scene.flags, scene.cursor);
            }
        }
        if scene.menu_open {
            self.draw_menu(out, w, h, scene);
        }
        if scene.flags.magnifier {
            self.draw_magnifier(out, w, h, scene.cursor);
        }
    }

    /// 白芯 + 青绿环双圆手柄,亮暗背景均可见;外环半径随 scale 派生。
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

    /// 图标轨:选区外侧的纯图标圆形按钮(accent 底 + 白图标,
    /// hover 深 accent 底);位置由 `toolbar_panel` 按选区+屏幕动态避让。
    fn draw_toolbar(
        &self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        selection: PhysicalRect,
        flags: FeatureFlags,
        cursor: (i32, i32),
    ) {
        let metrics = self.metrics;
        let buttons = toolbar_buttons(flags);
        let Some(panel) = toolbar_panel(metrics, selection, (w, h), &buttons) else {
            return;
        };
        for (action, rect) in toolbar_button_rects(metrics, panel, &buttons) {
            let (cx, cy) = rect.center();
            let hover = rect.contains(cursor.0, cursor.1);
            let bg = if hover { ACCENT_DARK } else { ACCENT_DEEP };
            fill_circle(rgba, w, h, cx, cy, metrics.rail_button / 2, bg);
            draw_icon(rgba, w, h, action, cx, cy, metrics.toolbar_icon, ICON_INK);
        }
    }

    /// 亮铬右键菜单:左图标 + 右文字;光标悬停项用青绿软底 + 深青绿字;
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
        let line = text::line_height(metrics.menu_font);
        for (action, rect) in menu_item_rects(metrics, panel, &items) {
            let hover = rect.contains(scene.cursor.0, scene.cursor.1);
            if hover {
                fill_round_blend(rgba, w, h, rect, metrics.menu_hover_radius, ACTIVE_BG);
            }
            let color = if hover { ACCENT_DEEP } else { CHROME_TEXT };
            let (_, cy) = rect.center();
            draw_icon(
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
            text::draw_text(
                rgba,
                w,
                h,
                (rect.x + metrics.menu_text_x) as f32,
                rect.y as f32 + (rect.height as f32 - line).max(0.0) / 2.0,
                &label,
                metrics.menu_font,
                color,
            );
        }
    }

    /// 放大镜:采样冻结帧**原件**(真实屏幕像素,非压暗帧),中心十字准星,
    /// 镜下单行亮底 pill 读数(HEX/RGB/坐标);放大区边长封顶 MAG_MAX_EDGE。
    fn draw_magnifier(&self, rgba: &mut [u8], w: u32, h: u32, cursor: (i32, i32)) {
        let layout = mag_layout(self.scale);
        let panel = magnifier_rect(cursor, (w, h), self.scale);
        draw_panel_chrome(rgba, w, h, panel, self.metrics.panel_radius);
        // 放大区在面板内水平居中,顶部留出 pad。
        let px = panel.x + (panel.width - layout.edge) / 2;
        let py = panel.y + layout.pad;
        for dy in 0..layout.edge {
            for dx in 0..layout.edge {
                let src = self.sample(
                    cursor.0 - layout.half + dx / layout.block,
                    cursor.1 - layout.half + dy / layout.block,
                );
                let [r, g, b, _] = src;
                put(rgba, w, h, px + dx, py + dy, [r, g, b, 255]);
            }
        }
        // 中心十字准星(横竖 1px 贯穿放大区)。
        let cross_x = px + layout.half * layout.block + layout.block / 2;
        let cross_y = py + layout.half * layout.block + layout.block / 2;
        for x in px..px + layout.edge {
            put(rgba, w, h, x, cross_y, ACCENT);
        }
        for y in py..py + layout.edge {
            put(rgba, w, h, cross_x, y, ACCENT);
        }
        // 单行读数 pill:白底深字,水平居中于放大区下方。
        let center = self.sample(cursor.0, cursor.1);
        let line = magnifier_readout_line(cursor, center);
        let text_w = text::measure_width(&line, layout.font).unwrap_or(0.0);
        let max_pill = (panel.width - layout.pad * 2).max(0);
        let pill_w = (text_w.ceil() as i32 + layout.pill_pad_x * 2).min(max_pill);
        let pill = IntRect {
            x: panel.x + (panel.width - pill_w) / 2,
            y: panel.y + layout.pad + layout.edge + layout.gap,
            width: pill_w,
            height: layout.pill_h,
        };
        draw_panel_chrome(rgba, w, h, pill, layout.pill_h / 2);
        let line_h = text::line_height(layout.font);
        let text_x = pill.x as f32 + (pill.width as f32 - text_w).max(0.0) / 2.0;
        let text_y = pill.y as f32 + (pill.height as f32 - line_h).max(0.0) / 2.0;
        text::draw_text(rgba, w, h, text_x, text_y, &line, layout.font, CHROME_TEXT);
    }
}

/// 亮铬面板统一画法:底部 2px 深色 offset 模拟阴影 → 1px 细描边 → 亮暖白底。
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

fn punch_hole(rgba: &mut [u8], original: &[u8], stride_px: usize, rect: PhysicalRect) {
    for row in 0..rect.height as usize {
        let offset = ((rect.y as usize + row) * stride_px + rect.x as usize) * 4;
        let count = rect.width as usize * 4;
        rgba[offset..offset + count].copy_from_slice(&original[offset..offset + count]);
    }
}

/// 选区描边:青绿 2px。
fn outline_selection(rgba: &mut [u8], w: u32, h: u32, rect: PhysicalRect) {
    let x0 = rect.x as i32;
    let y0 = rect.y as i32;
    let x1 = x0 + rect.width as i32 - 1;
    let y1 = y0 + rect.height as i32 - 1;
    for x in x0..=x1 {
        put(rgba, w, h, x, y0, ACCENT);
        put(rgba, w, h, x, (y0 + 1).min(y1), ACCENT);
        put(rgba, w, h, x, y1, ACCENT);
        put(rgba, w, h, x, (y1 - 1).max(y0), ACCENT);
    }
    for y in y0..=y1 {
        put(rgba, w, h, x0, y, ACCENT);
        put(rgba, w, h, (x0 + 1).min(x1), y, ACCENT);
        put(rgba, w, h, x1, y, ACCENT);
        put(rgba, w, h, (x1 - 1).max(x0), y, ACCENT);
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

fn blend(rgba: &mut [u8], w: u32, h: u32, x: i32, y: i32, color: [u8; 4]) {
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

/// 引擎内绘制的简洁图标字形:(cx, cy) 为中心,size 为外接盒边长。
/// 复制=双矩形,保存=箭头入盘,贴图=图钉,标注=笔,取字=「字」,取消=×。
#[allow(clippy::too_many_arguments)]
fn draw_icon(
    rgba: &mut [u8],
    w: u32,
    h: u32,
    action: SelectionAction,
    cx: i32,
    cy: i32,
    size: i32,
    ink: [u8; 4],
) {
    let s = size / 2;
    match action {
        SelectionAction::Copy => {
            // 双矩形:后页四边描边,前页实心压住后页右下。
            let back = IntRect {
                x: cx - s + 1,
                y: cy - s,
                width: s + 2,
                height: s + 3,
            };
            draw_line(rgba, w, h, back.x, back.y, back.right() - 1, back.y, 1, ink);
            draw_line(rgba, w, h, back.x, back.y, back.x, back.bottom() - 1, 1, ink);
            draw_line(
                rgba,
                w,
                h,
                back.right() - 1,
                back.y,
                back.right() - 1,
                back.bottom() - 1,
                1,
                ink,
            );
            draw_line(
                rgba,
                w,
                h,
                back.x,
                back.bottom() - 1,
                back.right() - 1,
                back.bottom() - 1,
                1,
                ink,
            );
            fill_rect(
                rgba,
                w,
                h,
                IntRect {
                    x: cx - 2,
                    y: cy - s + 4,
                    width: s + 2,
                    height: s + 3,
                },
                ink,
            );
        }
        SelectionAction::Save => {
            // 箭头入盘:竖直箭头 + 底部托盘。
            draw_line(rgba, w, h, cx, cy - s, cx, cy + 1, 1, ink);
            draw_line(rgba, w, h, cx - 3, cy - 2, cx, cy + 2, 1, ink);
            draw_line(rgba, w, h, cx + 3, cy - 2, cx, cy + 2, 1, ink);
            draw_line(rgba, w, h, cx - s + 1, cy + s - 3, cx + s - 1, cy + s - 3, 1, ink);
            draw_line(rgba, w, h, cx - s + 1, cy + s - 6, cx - s + 1, cy + s - 3, 1, ink);
            draw_line(rgba, w, h, cx + s - 1, cy + s - 6, cx + s - 1, cy + s - 3, 1, ink);
        }
        SelectionAction::Pin => {
            // 图钉:圆头 + 斜下针尖。
            fill_circle(rgba, w, h, cx - 3, cy - 4, (s / 2).max(3), ink);
            draw_line(rgba, w, h, cx - 1, cy - 2, cx + s - 3, cy + s - 2, 1, ink);
        }
        SelectionAction::Annotate => {
            // 笔:斜向笔身 + 笔尖。
            draw_line(rgba, w, h, cx - s + 3, cy + s - 3, cx + s - 4, cy - s + 4, 2, ink);
            draw_line(rgba, w, h, cx - s + 1, cy + s - 1, cx - s + 3, cy + s - 3, 1, ink);
        }
        SelectionAction::Ocr => {
            // 「字」字形(文字管线,加粗居中)。
            let font = (size as f32 - 2.0).max(10.0);
            let text_w = text::measure_width("字", font).unwrap_or(font);
            let x = cx as f32 - text_w / 2.0;
            let y = cy as f32 - text::line_height(font) / 2.0;
            text::draw_text_bold(rgba, w, h, x, y, "字", font, ink);
        }
        SelectionAction::Cancel => {
            draw_line(rgba, w, h, cx - s + 3, cy - s + 3, cx + s - 3, cy + s - 3, 1, ink);
            draw_line(rgba, w, h, cx - s + 3, cy + s - 3, cx + s - 3, cy - s + 3, 1, ink);
        }
        SelectionAction::CopyColor => {
            // 兜底(不在图标轨/菜单动作集内):实心圆点。
            fill_circle(rgba, w, h, cx, cy, 3, ink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};

    fn solid_frame(width: u32, height: u32, rgba: [u8; 4]) -> Frame {
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            bytes.extend_from_slice(&rgba);
        }
        accept_buffer(RawBuffer::ready(width, height, bytes)).unwrap()
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
    fn chrome_palette_is_bright_warm_with_dark_ink() {
        // 亮暖白底 #fffcf7 @ 97%。
        assert_eq!(CHROME_BG, [255, 252, 247, 247]);
        // 深墨字 #1c1917。
        assert_eq!(CHROME_TEXT, [0x1C, 0x19, 0x17, 255]);
        // 描边是 14% 深墨;hover 软底是 12% 深青绿。
        assert_eq!(CHROME_BORDER, [28, 25, 23, 36]);
        assert_eq!(ACTIVE_BG, [15, 118, 110, 31]);
        // 图标轨按钮:accent #0f766e 底,hover #115e59,白图标。
        assert_eq!(ACCENT_DEEP, [0x0F, 0x76, 0x6E, 255]);
        assert_eq!(ACCENT_DARK, [0x11, 0x5E, 0x59, 255]);
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
        assert_eq!(handle_hit(metrics_1(), rect, 100, 100), Some(HandleKind::NorthWest));
        assert_eq!(handle_hit(metrics_1(), rect, 219, 100), Some(HandleKind::NorthEast));
        assert_eq!(handle_hit(metrics_1(), rect, 219, 179), Some(HandleKind::SouthEast));
        assert_eq!(handle_hit(metrics_1(), rect, 100, 179), Some(HandleKind::SouthWest));
        // 命中半径(9px)内仍算命中。
        assert_eq!(handle_hit(metrics_1(), rect, 106, 100), Some(HandleKind::NorthWest));
        assert_eq!(handle_hit(metrics_1(), rect, 109, 100), Some(HandleKind::NorthWest));
        assert_eq!(handle_hit(metrics_1(), rect, 159, 100), Some(HandleKind::North));
        assert_eq!(handle_hit(metrics_1(), rect, 219, 139), Some(HandleKind::East));
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
        // 单行读数:HEX + 紧凑 RGB + 坐标。
        assert_eq!(
            magnifier_readout_line((32, 32), pixel),
            "#2DD4BF R45 G212 B191 · 32, 32".to_string()
        );
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
    fn magnifier_samples_pixel_grid_at_8x_within_panel_cap() {
        // 像素级放大:源窗口 23px 直径、块 8 物理 px(有效 ~8x),与 scale 无关。
        for scale in [0.5, 1.0, 1.5, 2.0, 3.0, 4.0] {
            let layout = mag_layout(scale);
            assert_eq!(layout.half * 2 + 1, 23, "scale {scale}: 源采样窗口直径");
            assert_eq!(layout.block, 8, "scale {scale}: 放大块边长");
            assert_eq!(layout.edge, 184, "scale {scale}: 放大区边长");
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
        // 放大区左上角对应源 (189,139):原始纯色(200,100,50),不是压暗后的 (104,52,26)。
        assert_eq!(read(px, py), [200, 100, 50, 255]);
        // 中心块内部对应光标源像素 (200,150)(避开十字准星行列)。
        let block = px + layout.half * layout.block;
        let block_y = py + layout.half * layout.block;
        assert_eq!(read(block + 2, block_y + 2), [1, 2, 3, 255]);
        // 十字准星:放大区中心行/列为强调色。
        let cross_x = px + layout.half * layout.block + layout.block / 2;
        let cross_y = py + layout.half * layout.block + layout.block / 2;
        assert_eq!(read(cross_x, py), [ACCENT[0], ACCENT[1], ACCENT[2], 255]);
        assert_eq!(read(px, cross_y), [ACCENT[0], ACCENT[1], ACCENT[2], 255]);
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
        };
        let composed = composer.compose(&scene);
        let panel = magnifier_rect((300, 200), (600, 400), 1.5);
        let layout = mag_layout(1.5);
        assert_eq!(layout.edge, 184);
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
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 320 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // SE 手柄 (200,120):白芯 + 青绿环(scale 1.0 外环半径 5)。
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
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 320 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // 徽标在选区上方;面板内部接近亮暖白(暗幕 [10,10,10] 上 97% 混合 ≈ [250,247,242])。
        let badge_y = 80 - (text::line_height(BADGE_FONT).ceil() as i32) - BADGE_PAD_Y * 2
            + BADGE_PAD_Y
            - BADGE_MARGIN;
        let pixel = read(60, badge_y.max(2));
        assert!(
            pixel[0] > 230 && pixel[1] > 225 && pixel[2] > 220,
            "badge interior should be bright, got {pixel:?}"
        );
    }

    #[test]
    fn rail_layout_follows_flags_and_avoids_selection_border() {
        let flags = FeatureFlags {
            toolbar_save: false,
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        let buttons = toolbar_buttons(flags);
        assert_eq!(buttons, vec![SelectionAction::Copy]);
        // 选区贴近屏幕右下角:右轨/底排都放不下 → 左侧竖排轨,不压选区左边框。
        let selection = PhysicalRect {
            x: 180,
            y: 130,
            width: 15,
            height: 15,
        };
        let panel = toolbar_panel(metrics_1(), selection, (200, 150), &buttons).unwrap();
        assert!(panel.x >= 0 && panel.y >= 0);
        assert!(panel.right() <= 200 && panel.bottom() <= 150);
        assert!(panel.right() <= selection.x as i32);
        assert_eq!(panel.width, RAIL_BUTTON);
        let rects = toolbar_button_rects(metrics_1(), panel, &buttons);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].1.width, RAIL_BUTTON);
        assert_eq!(rects[0].1.height, RAIL_BUTTON);
        assert!(panel.contains(rects[0].1.x, rects[0].1.y));
        // 全关时无面板。
        let off = FeatureFlags {
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        assert!(toolbar_panel(metrics_1(), selection, (200, 150), &toolbar_buttons(off)).is_none());
    }

    #[test]
    fn rail_prefers_right_then_bottom_then_left_and_wraps_rows() {
        let buttons = toolbar_buttons(FeatureFlags::default());
        assert_eq!(buttons.len(), 3);
        // ① 右侧竖排轨优先:垂直居中、与选区间距 10px、单列。
        let selection = PhysicalRect {
            x: 40,
            y: 60,
            width: 100,
            height: 80,
        };
        let panel = toolbar_panel(metrics_1(), selection, (400, 300), &buttons).unwrap();
        assert_eq!(panel.x, 140 + RAIL_MARGIN_V);
        assert_eq!(panel.width, RAIL_BUTTON);
        assert_eq!(panel.height, 3 * RAIL_BUTTON + 2 * RAIL_GAP);
        let rail_center = panel.y + panel.height / 2;
        assert!((rail_center - (60 + 40)).abs() <= 1);
        let rects = toolbar_button_rects(metrics_1(), panel, &buttons);
        assert!(rects.windows(2).all(|pair| pair[1].1.y > pair[0].1.y));
        assert!(rects.iter().all(|(_, rect)| rect.x == panel.x));
        // ② 右缘不足 → 底部水平排;窄选区自动折行(每行 1 个,共 3 行居中)。
        let narrow = PhysicalRect {
            x: 340,
            y: 40,
            width: 50,
            height: 20,
        };
        let panel = toolbar_panel(metrics_1(), narrow, (400, 300), &buttons).unwrap();
        assert_eq!(panel.y, 60 + RAIL_MARGIN_H);
        assert_eq!(panel.height, 3 * RAIL_BUTTON + 2 * RAIL_GAP);
        let rects = toolbar_button_rects(metrics_1(), panel, &buttons);
        assert_eq!(rects.len(), 3);
        assert!(rects.windows(2).all(|pair| pair[1].1.y > pair[0].1.y));
        // ③ 宽选区底部水平排:单行容纳全部按钮。
        let wide = PhysicalRect {
            x: 430,
            y: 40,
            width: 200,
            height: 20,
        };
        let panel = toolbar_panel(metrics_1(), wide, (640, 480), &buttons).unwrap();
        assert_eq!(panel.y, 60 + RAIL_MARGIN_H);
        assert_eq!(panel.height, RAIL_BUTTON);
        let rects = toolbar_button_rects(metrics_1(), panel, &buttons);
        assert!(rects.windows(2).all(|pair| pair[1].1.y == pair[0].1.y));
        assert!(rects.windows(2).all(|pair| pair[1].1.x > pair[0].1.x));
        // 单行整体居中于选区。
        let row_center = panel.x + panel.width / 2;
        assert!((row_center - (430 + 100)).abs() <= 1);
    }

    #[test]
    fn menu_geometry_has_touch_targets_and_cancel_separator() {
        let items = menu_items(FeatureFlags::default());
        assert_eq!(items.len(), 6);
        let panel = menu_panel(metrics_1(), (50, 50), (800, 600), &items);
        assert_eq!(panel.width, MENU_ITEM_W + MENU_PAD * 2);
        assert_eq!(
            panel.height,
            items.len() as i32 * MENU_ITEM_H + MENU_PAD * 2 + MENU_SEPARATOR_H
        );
        let rects = menu_item_rects(metrics_1(), panel, &items);
        for (_, rect) in &rects {
            assert_eq!(rect.width, MENU_ITEM_W);
            assert_eq!(rect.height, MENU_ITEM_H);
            assert_eq!(rect.x, panel.x + MENU_PAD);
        }
        // 分隔线:紧贴倒数第二项底部,末项(取消)在线下 1px。
        let sep_y = menu_separator_y(metrics_1(), panel, &items);
        assert_eq!(rects[4].1.bottom(), sep_y);
        assert_eq!(rects[5].1.y, sep_y + MENU_SEPARATOR_H);
        // 图标/文字内边距。
        assert_eq!(MENU_ICON_CX, 22);
        assert_eq!(MENU_TEXT_X, 38);
    }

    #[test]
    fn menu_items_respect_feature_flags() {
        let default_items = menu_items(FeatureFlags::default());
        assert_eq!(default_items.len(), 6);
        assert_eq!(*default_items.last().unwrap(), SelectionAction::Cancel);
        let flags = FeatureFlags {
            ocr_entry: false,
            pin_entry: false,
            ..FeatureFlags::default()
        };
        let items = menu_items(flags);
        assert!(!items.contains(&SelectionAction::Ocr));
        assert!(!items.contains(&SelectionAction::Pin));
        assert_eq!(items.len(), 4);
    }

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
        // 关闭放大镜:面板尺寸随读数变化,本测试只核对选区/操作条 hitbox 布局。
        let flags = no_magnifier_flags();
        let scene = Scene {
            selection: Some(selection),
            cursor: (200, 120),
            flags,
            toolbar_visible: true,
            menu_open: false,
            menu_anchor: (0, 0),
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
        // 图标轨按钮 hitbox 内是 accent 圆形按钮(避开中央白色字形取偏心点)。
        let buttons = toolbar_buttons(flags);
        let panel = toolbar_panel(metrics_1(), selection, (320, 200), &buttons).unwrap();
        let (_, last) = toolbar_button_rects(metrics_1(), panel, &buttons)
            .last()
            .copied()
            .unwrap();
        let (cx, cy) = last.center();
        assert_eq!(read(cx + 12, cy), [0x0F, 0x76, 0x6E]);
        // 描边:选区左边框(避开手柄)为强调色。
        assert_eq!(read(41, 90), [ACCENT[0], ACCENT[1], ACCENT[2]]);
    }

    #[test]
    fn chrome_metrics_derive_sizes_and_pinned_hit_radii_from_scale() {
        // 125%–250%:全部 chrome 尺寸/字号由 scale 派生,命中半径钉死物理下限。
        for scale in [1.0_f32, 1.25, 1.5, 2.0, 2.5] {
            let metrics = ChromeMetrics::for_scale(scale);
            assert_eq!(metrics.scale, scale);
            assert_eq!(
                metrics.rail_button,
                (RAIL_BUTTON as f32 * scale).round() as i32,
                "scale {scale}: 操作条按钮"
            );
            assert_eq!(metrics.menu_item_w, (MENU_ITEM_W as f32 * scale).round() as i32);
            assert_eq!(metrics.menu_item_h, (MENU_ITEM_H as f32 * scale).round() as i32);
            assert_eq!(metrics.menu_font, MENU_FONT * scale);
            assert_eq!(metrics.badge_font, BADGE_FONT * scale);
            // 命中半径随 scale 放大且不低于 1.0 基准。
            assert!(metrics.handle_hit_radius >= HANDLE_HIT_RADIUS);
            assert!(metrics.handle_radius >= 5);
            assert!(metrics.edge_hit_radius >= EDGE_HIT_RADIUS);
            assert!(metrics.rail_button >= RAIL_BUTTON);
            assert!(metrics.menu_item_h >= MENU_ITEM_H);
            // 文字与图标不裁切:菜单项容得下放大后的字高/文字,按钮容得下图标。
            if text::ui_font().is_some() {
                let label = action_label(SelectionAction::Cancel);
                let width = text::measure_width(&label, metrics.menu_font).unwrap();
                assert!(
                    metrics.menu_text_x + width.ceil() as i32 <= metrics.menu_item_w,
                    "scale {scale}: 菜单文字被裁切"
                );
                assert!(text::line_height(metrics.menu_font) <= metrics.menu_item_h as f32);
            }
            assert!(metrics.toolbar_icon <= metrics.rail_button);
            assert!(metrics.menu_pad * 2 < metrics.menu_item_w);
        }
        // 低于 1.0 与非法值都回到 1.0 基准,不缩水。
        for scale in [0.5_f32, 0.0, -2.0, f32::NAN, f32::INFINITY] {
            let metrics = ChromeMetrics::for_scale(scale);
            assert_eq!(metrics.rail_button, RAIL_BUTTON, "scale {scale}");
            assert_eq!(metrics.handle_hit_radius, HANDLE_HIT_RADIUS, "scale {scale}");
            assert_eq!(metrics.menu_font, MENU_FONT, "scale {scale}");
        }
        // 超过 4.0 钳制,避免 chrome 占据整屏。
        assert_eq!(ChromeMetrics::for_scale(8.0).rail_button, RAIL_BUTTON * 4);
    }

    #[test]
    fn chrome_layout_and_hit_share_scaled_metrics() {
        for scale in [1.5_f32, 2.0] {
            let metrics = ChromeMetrics::for_scale(scale);
            let buttons = toolbar_buttons(FeatureFlags::default());
            let selection = PhysicalRect {
                x: 40,
                y: 60,
                width: 100,
                height: 80,
            };
            // 布局尺寸按 metrics;命中矩形与布局同源。
            let panel = toolbar_panel(metrics, selection, (800, 600), &buttons).unwrap();
            assert_eq!(panel.width, metrics.rail_button);
            assert_eq!(
                panel.height,
                3 * metrics.rail_button + 2 * metrics.rail_gap,
                "scale {scale}"
            );
            assert_eq!(panel.x, 140 + metrics.rail_margin_v);
            let rects = toolbar_button_rects(metrics, panel, &buttons);
            assert_eq!(rects.len(), 3);
            assert!(rects
                .iter()
                .all(|(_, rect)| rect.width == metrics.rail_button
                    && rect.height == metrics.rail_button));
            assert!(rects.windows(2).all(|pair| pair[1].1.y > pair[0].1.y));
            // 菜单几何按 metrics;分隔线与项底对齐。
            let items = menu_items(FeatureFlags::default());
            let menu = menu_panel(metrics, (50, 50), (800, 600), &items);
            assert_eq!(menu.width, metrics.menu_item_w + metrics.menu_pad * 2);
            assert_eq!(
                menu.height,
                items.len() as i32 * metrics.menu_item_h
                    + metrics.menu_pad * 2
                    + metrics.menu_separator_h
            );
            let mrects = menu_item_rects(metrics, menu, &items);
            for (_, rect) in &mrects {
                assert_eq!(rect.width, metrics.menu_item_w);
                assert_eq!(rect.height, metrics.menu_item_h);
                assert_eq!(rect.x, menu.x + metrics.menu_pad);
            }
            let sep_y = menu_separator_y(metrics, menu, &items);
            assert_eq!(mrects[4].1.bottom(), sep_y);
            assert_eq!(mrects[5].1.y, sep_y + metrics.menu_separator_h);
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
        };
        let composed = composer.compose(&scene);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 640 + x as u32) * 4) as usize;
            [composed[i], composed[i + 1], composed[i + 2]]
        };
        // 操作条按钮中心区仍是 accent 圆(取偏心点避开白色图标)。
        let buttons = toolbar_buttons(flags);
        let panel = toolbar_panel(metrics, selection, (640, 400), &buttons).unwrap();
        let (_, last) = toolbar_button_rects(metrics, panel, &buttons)
            .last()
            .copied()
            .unwrap();
        let (cx, cy) = last.center();
        let inset = metrics.rail_button / 2 - 4;
        assert_eq!(read(cx + inset, cy), [0x0F, 0x76, 0x6E]);
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
            pixel[0] > 230 && pixel[1] > 225 && pixel[2] > 220,
            "scaled menu panel should be bright, got {pixel:?}"
        );
    }
}
