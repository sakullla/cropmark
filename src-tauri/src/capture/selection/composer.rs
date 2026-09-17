//! 平台无关的选区像素合成器。
//!
//! 在冻结帧(`capture::buffer::Frame`)的 RGBA 位图上合成:暗幕+选区开洞、
//! 青绿描边、8 向手柄、尺寸徽标、放大镜(全分辨率采样 + 坐标/RGB/HEX 读数)、
//! 操作条与右键菜单。布局/hitbox 函数是纯几何,状态机与绘制共用同一份,
//! 保证命中判定与合成输出一致。不含任何窗口/平台代码。

use super::text;
use super::{FeatureFlags, HandleKind, Scene, SelectionAction};
use crate::capture::buffer::{validate_frame, Frame};
use crate::capture::error::CaptureError;
use crate::capture::geometry::PhysicalRect;

/// 青绿强调色 #2dd4bf,与现 Windows 原生路径 BGRA [0xBF,0xD4,0x2D] 同色。
pub const ACCENT: [u8; 4] = [0x2D, 0xD4, 0xBF, 255];
const PANEL_BG: [u8; 4] = [17, 24, 28, 235];
const PANEL_TEXT: [u8; 4] = [246, 241, 232, 255];
const BUTTON_BG: [u8; 4] = [32, 43, 48, 255];
/// 暗幕保留 52% 亮度,对齐现 Windows 原生路径。
const DIM_KEEP: u16 = 52;

/// 手柄命中半径(物理像素)。
pub const HANDLE_HIT_RADIUS: i32 = 6;
const HANDLE_HALF: i32 = 4;

const BADGE_FONT: f32 = 13.0;
const BADGE_PAD_X: i32 = 7;
const BADGE_PAD_Y: i32 = 3;
const BADGE_MARGIN: i32 = 4;

const TOOLBAR_BUTTON_W: i32 = 64;
const TOOLBAR_BUTTON_H: i32 = 28;
const TOOLBAR_PAD: i32 = 6;
const TOOLBAR_MARGIN: i32 = 8;
const TOOLBAR_FONT: f32 = 13.0;

const MENU_ITEM_W: i32 = 104;
const MENU_ITEM_H: i32 = 30;
const MENU_FONT: f32 = 13.0;

/// 放大镜:源窗口 (2*MAG_HALF+1)^2 像素,MAG_ZOOM 倍最近邻放大。
const MAG_HALF: i32 = 7;
pub const MAG_ZOOM: i32 = 8;
const MAG_FONT: f32 = 11.0;
const MAG_OFFSET: i32 = 18;

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

pub fn action_label(action: SelectionAction) -> &'static str {
    match action {
        SelectionAction::Copy => "复制",
        SelectionAction::Save => "保存",
        SelectionAction::Pin => "贴图",
        SelectionAction::Annotate => "标注",
        SelectionAction::Ocr => "取字",
        SelectionAction::Cancel => "取消",
        SelectionAction::CopyColor => "复制色值",
    }
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

/// 操作条面板矩形:默认在选区下方,屏幕下方放不下时翻到上方,并整体钳制屏内。
pub fn toolbar_panel(
    selection: PhysicalRect,
    screen: (u32, u32),
    buttons: &[SelectionAction],
) -> Option<IntRect> {
    if buttons.is_empty() {
        return None;
    }
    let count = buttons.len() as i32;
    let panel_w = TOOLBAR_PAD * 2 + count * TOOLBAR_BUTTON_W + (count - 1) * TOOLBAR_PAD;
    let panel_h = TOOLBAR_PAD * 2 + TOOLBAR_BUTTON_H;
    let sel = IntRect::from(selection);
    let max_x = (screen.0 as i32 - panel_w).max(0);
    let max_y = (screen.1 as i32 - panel_h).max(0);
    let mut y = sel.bottom() + TOOLBAR_MARGIN;
    if y > max_y {
        y = sel.y - TOOLBAR_MARGIN - panel_h;
    }
    Some(IntRect {
        x: sel.x.clamp(0, max_x),
        y: y.clamp(0, max_y),
        width: panel_w,
        height: panel_h,
    })
}

/// 面板内逐按钮矩形,顺序与 `buttons` 一致(与绘制共用,保证 hitbox 一致)。
pub fn toolbar_button_rects(
    panel: IntRect,
    buttons: &[SelectionAction],
) -> Vec<(SelectionAction, IntRect)> {
    buttons
        .iter()
        .enumerate()
        .map(|(index, action)| {
            (
                *action,
                IntRect {
                    x: panel.x + TOOLBAR_PAD + index as i32 * (TOOLBAR_BUTTON_W + TOOLBAR_PAD),
                    y: panel.y + TOOLBAR_PAD,
                    width: TOOLBAR_BUTTON_W,
                    height: TOOLBAR_BUTTON_H,
                },
            )
        })
        .collect()
}

pub fn menu_panel(anchor: (i32, i32), screen: (u32, u32), items: &[SelectionAction]) -> IntRect {
    let width = MENU_ITEM_W + 2;
    let height = items.len() as i32 * MENU_ITEM_H + 2;
    IntRect {
        x: anchor.0.clamp(0, (screen.0 as i32 - width).max(0)),
        y: anchor.1.clamp(0, (screen.1 as i32 - height).max(0)),
        width,
        height,
    }
}

pub fn menu_item_rects(
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
                    x: panel.x + 1,
                    y: panel.y + 1 + index as i32 * MENU_ITEM_H,
                    width: MENU_ITEM_W,
                    height: MENU_ITEM_H,
                },
            )
        })
        .collect()
}

/// 放大镜面板矩形:光标右下角偏移,贴近屏幕边缘时翻转,整体钳制屏内。
pub fn magnifier_rect(cursor: (i32, i32), screen: (u32, u32)) -> IntRect {
    let pixels_edge = (MAG_HALF * 2 + 1) * MAG_ZOOM;
    let border = 1;
    let line = text::line_height(MAG_FONT).ceil() as i32;
    let readout_h = 2 * line + 6;
    let width = pixels_edge + border * 2;
    let height = pixels_edge + border * 2 + readout_h;
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

/// 8 向手柄命中;角手柄优先于边手柄。
pub fn handle_hit(rect: PhysicalRect, x: i32, y: i32) -> Option<HandleKind> {
    for kind in CORNER_HANDLES {
        let (hx, hy) = handle_anchor(rect, kind);
        if (x - hx).abs() <= HANDLE_HIT_RADIUS && (y - hy).abs() <= HANDLE_HIT_RADIUS {
            return Some(kind);
        }
    }
    for kind in EDGE_HANDLES {
        let (hx, hy) = handle_anchor(rect, kind);
        if (x - hx).abs() <= HANDLE_HIT_RADIUS && (y - hy).abs() <= HANDLE_HIT_RADIUS {
            return Some(kind);
        }
    }
    None
}

/// 尺寸徽标文本,例如 "123 × 45"。
pub fn size_readout(rect: PhysicalRect) -> String {
    format!("{} × {}", rect.width, rect.height)
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

/// 放大镜读数两行:坐标、RGB + HEX。
pub fn magnifier_readout_lines(cursor: (i32, i32), pixel: [u8; 4]) -> Vec<String> {
    vec![
        coordinate_readout(cursor.0, cursor.1),
        format!("{} {}", rgb_readout(pixel), hex_readout(pixel)),
    ]
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

/// CPU 合成器:持有冻结帧原件与预计算的暗幕帧。
pub struct Composer {
    width: u32,
    height: u32,
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
        Ok(Self {
            width: frame.width,
            height: frame.height,
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
    pub fn compose_into(&self, scene: &Scene, out: &mut [u8]) {
        out.copy_from_slice(&self.dimmed);
        let (w, h) = (self.width, self.height);
        if let Some(selection) = scene.selection {
            punch_hole(out, &self.original, w as usize, selection);
            outline_selection(out, w, h, selection);
            draw_handles(out, w, h, selection);
            draw_size_badge(out, w, h, selection);
            if scene.toolbar_visible {
                draw_toolbar(out, w, h, selection, scene.flags);
            }
        }
        if scene.menu_open {
            draw_menu(out, w, h, scene);
        }
        if scene.flags.magnifier {
            self.draw_magnifier(out, w, h, scene.cursor);
        }
    }

    fn draw_magnifier(&self, rgba: &mut [u8], w: u32, h: u32, cursor: (i32, i32)) {
        let panel = magnifier_rect(cursor, (w, h));
        fill_rect_blend(rgba, w, h, panel, PANEL_BG);
        stroke_rect(rgba, w, h, panel, ACCENT);
        let pixels_edge = (MAG_HALF * 2 + 1) * MAG_ZOOM;
        let px = panel.x + 1;
        let py = panel.y + 1;
        for dy in 0..pixels_edge {
            for dx in 0..pixels_edge {
                let src = self.sample(
                    cursor.0 - MAG_HALF + dx / MAG_ZOOM,
                    cursor.1 - MAG_HALF + dy / MAG_ZOOM,
                );
                let [r, g, b, _] = src;
                put(rgba, w, h, px + dx, py + dy, [r, g, b, 255]);
            }
        }
        // 光标所指源像素高亮框(不遮中心像素)。
        stroke_rect(
            rgba,
            w,
            h,
            IntRect {
                x: px + MAG_HALF * MAG_ZOOM,
                y: py + MAG_HALF * MAG_ZOOM,
                width: MAG_ZOOM,
                height: MAG_ZOOM,
            },
            ACCENT,
        );
        // 读数两行。
        let center = self.sample(cursor.0, cursor.1);
        let lines = magnifier_readout_lines(cursor, center);
        let line_h = text::line_height(MAG_FONT).ceil() as i32;
        for (index, line) in lines.iter().enumerate() {
            text::draw_text(
                rgba,
                w,
                h,
                (panel.x + 6) as f32,
                (panel.y + 1 + pixels_edge + 4 + index as i32 * line_h) as f32,
                line,
                MAG_FONT,
                PANEL_TEXT,
            );
        }
    }
}

fn punch_hole(rgba: &mut [u8], original: &[u8], stride_px: usize, rect: PhysicalRect) {
    for row in 0..rect.height as usize {
        let offset = ((rect.y as usize + row) * stride_px + rect.x as usize) * 4;
        let count = rect.width as usize * 4;
        rgba[offset..offset + count].copy_from_slice(&original[offset..offset + count]);
    }
}

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

fn draw_handles(rgba: &mut [u8], w: u32, h: u32, rect: PhysicalRect) {
    for kind in ALL_HANDLES {
        let (cx, cy) = handle_anchor(rect, kind);
        fill_rect(
            rgba,
            w,
            h,
            IntRect {
                x: cx - HANDLE_HALF,
                y: cy - HANDLE_HALF,
                width: HANDLE_HALF * 2 + 1,
                height: HANDLE_HALF * 2 + 1,
            },
            ACCENT,
        );
        fill_rect(
            rgba,
            w,
            h,
            IntRect {
                x: cx - 1,
                y: cy - 1,
                width: 3,
                height: 3,
            },
            PANEL_TEXT,
        );
    }
}

fn draw_size_badge(rgba: &mut [u8], w: u32, h: u32, rect: PhysicalRect) {
    let label = size_readout(rect);
    let Some(text_width) = text::measure_width(&label, BADGE_FONT) else {
        return;
    };
    let width = text_width.ceil() as i32 + BADGE_PAD_X * 2;
    let height = text::line_height(BADGE_FONT).ceil() as i32 + BADGE_PAD_Y * 2;
    let sel = IntRect::from(rect);
    let mut y = sel.y - height - BADGE_MARGIN;
    if y < 0 {
        y = sel.bottom() + BADGE_MARGIN;
    }
    let panel = IntRect {
        x: sel.x.clamp(0, (w as i32 - width).max(0)),
        y: y.clamp(0, (h as i32 - height).max(0)),
        width,
        height,
    };
    fill_rect_blend(rgba, w, h, panel, PANEL_BG);
    stroke_rect(rgba, w, h, panel, ACCENT);
    text::draw_text(
        rgba,
        w,
        h,
        (panel.x + BADGE_PAD_X) as f32,
        (panel.y + BADGE_PAD_Y) as f32,
        &label,
        BADGE_FONT,
        PANEL_TEXT,
    );
}

fn draw_toolbar(rgba: &mut [u8], w: u32, h: u32, selection: PhysicalRect, flags: FeatureFlags) {
    let buttons = toolbar_buttons(flags);
    let Some(panel) = toolbar_panel(selection, (w, h), &buttons) else {
        return;
    };
    fill_rect_blend(rgba, w, h, panel, PANEL_BG);
    stroke_rect(rgba, w, h, panel, ACCENT);
    let line = text::line_height(TOOLBAR_FONT);
    for (action, rect) in toolbar_button_rects(panel, &buttons) {
        fill_rect_blend(rgba, w, h, rect, BUTTON_BG);
        let label = action_label(action);
        let text_w = text::measure_width(label, TOOLBAR_FONT).unwrap_or(0.0);
        let x = rect.x as f32 + (rect.width as f32 - text_w).max(0.0) / 2.0;
        let y = rect.y as f32 + (rect.height as f32 - line).max(0.0) / 2.0;
        text::draw_text(rgba, w, h, x, y, label, TOOLBAR_FONT, PANEL_TEXT);
    }
}

fn draw_menu(rgba: &mut [u8], w: u32, h: u32, scene: &Scene) {
    let items = menu_items(scene.flags);
    let panel = menu_panel(scene.menu_anchor, (w, h), &items);
    fill_rect_blend(rgba, w, h, panel, PANEL_BG);
    stroke_rect(rgba, w, h, panel, ACCENT);
    let line = text::line_height(MENU_FONT);
    for (action, rect) in menu_item_rects(panel, &items) {
        let label = action_label(action);
        text::draw_text(
            rgba,
            w,
            h,
            (rect.x + 10) as f32,
            rect.y as f32 + (rect.height as f32 - line).max(0.0) / 2.0,
            label,
            MENU_FONT,
            PANEL_TEXT,
        );
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

fn fill_rect(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, color: [u8; 4]) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            put(rgba, w, h, x, y, color);
        }
    }
}

fn fill_rect_blend(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, color: [u8; 4]) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            blend(rgba, w, h, x, y, color);
        }
    }
}

fn stroke_rect(rgba: &mut [u8], w: u32, h: u32, rect: IntRect, color: [u8; 4]) {
    for x in rect.x..rect.right() {
        put(rgba, w, h, x, rect.y, color);
        put(rgba, w, h, x, rect.bottom() - 1, color);
    }
    for y in rect.y..rect.bottom() {
        put(rgba, w, h, rect.x, y, color);
        put(rgba, w, h, rect.right() - 1, y, color);
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

    #[test]
    fn handle_hit_prefers_corners_and_rejects_interior() {
        let rect = PhysicalRect {
            x: 100,
            y: 100,
            width: 120,
            height: 80,
        };
        assert_eq!(handle_hit(rect, 100, 100), Some(HandleKind::NorthWest));
        assert_eq!(handle_hit(rect, 219, 100), Some(HandleKind::NorthEast));
        assert_eq!(handle_hit(rect, 219, 179), Some(HandleKind::SouthEast));
        assert_eq!(handle_hit(rect, 100, 179), Some(HandleKind::SouthWest));
        // 命中半径内仍算命中。
        assert_eq!(handle_hit(rect, 106, 100), Some(HandleKind::NorthWest));
        assert_eq!(handle_hit(rect, 159, 100), Some(HandleKind::North));
        assert_eq!(handle_hit(rect, 219, 139), Some(HandleKind::East));
        // 选区内部与远处不命中。
        assert_eq!(handle_hit(rect, 160, 140), None);
        assert_eq!(handle_hit(rect, 300, 300), None);
    }

    #[test]
    fn size_and_coordinate_readout_texts() {
        let rect = PhysicalRect {
            x: 10,
            y: 20,
            width: 123,
            height: 45,
        };
        assert_eq!(size_readout(rect), "123 × 45");
        assert_eq!(coordinate_readout(10, 20), "10, 20");
    }

    #[test]
    fn rgb_and_hex_readouts() {
        let pixel = [0x2D, 0xD4, 0xBF, 255];
        assert_eq!(rgb_readout(pixel), "R 45 G 212 B 191");
        assert_eq!(hex_readout(pixel), "#2DD4BF");
        assert_eq!(
            magnifier_readout_lines((32, 32), pixel),
            vec!["32, 32".to_string(), "R 45 G 212 B 191 #2DD4BF".to_string()]
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
    fn magnifier_panel_zooms_source_pixels_with_true_colors() {
        let mut frame = solid_frame(64, 64, [200, 100, 50, 255]);
        let center = ((32 * 64 + 32) * 4) as usize;
        frame.rgba[center..center + 4].copy_from_slice(&[1, 2, 3, 255]);
        let composer = Composer::new(&frame).unwrap();
        let scene = Scene {
            selection: None,
            cursor: (32, 32),
            flags: FeatureFlags::default(),
            toolbar_visible: false,
            menu_open: false,
            menu_anchor: (0, 0),
        };
        let composed = composer.compose(&scene);
        let panel = magnifier_rect((32, 32), (64, 64));
        assert!(panel.width > 0 && panel.height > 0);
        let read = |x: i32, y: i32| {
            let i = ((y as u32 * 64 + x as u32) * 4) as usize;
            [
                composed[i],
                composed[i + 1],
                composed[i + 2],
                composed[i + 3],
            ]
        };
        // 放大区左上角对应源 (26,26):纯色底。
        assert_eq!(read(panel.x + 1, panel.y + 1), [200, 100, 50, 255]);
        // 中心 8×8 块内部对应光标源像素 (32,32)。
        let block = panel.x + 1 + MAG_HALF * MAG_ZOOM;
        let block_y = panel.y + 1 + MAG_HALF * MAG_ZOOM;
        assert_eq!(read(block + 3, block_y + 3), [1, 2, 3, 255]);
    }

    #[test]
    fn toolbar_layout_follows_flags_and_avoids_screen_edges() {
        let flags = FeatureFlags {
            toolbar_save: false,
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        let buttons = toolbar_buttons(flags);
        assert_eq!(buttons, vec![SelectionAction::Copy]);
        // 选区贴近屏幕右下角:面板翻到上方且完整在屏内。
        let selection = PhysicalRect {
            x: 180,
            y: 130,
            width: 15,
            height: 15,
        };
        let panel = toolbar_panel(selection, (200, 150), &buttons).unwrap();
        assert!(panel.x >= 0 && panel.y >= 0);
        assert!(panel.right() <= 200 && panel.bottom() <= 150);
        assert!(panel.bottom() <= selection.y as i32);
        let rects = toolbar_button_rects(panel, &buttons);
        assert_eq!(rects.len(), 1);
        assert!(panel.contains(rects[0].1.x, rects[0].1.y));
        // 全关时无面板。
        let off = FeatureFlags {
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        assert!(toolbar_panel(selection, (200, 150), &toolbar_buttons(off)).is_none());
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
        let flags = FeatureFlags::default();
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
        // 选区开洞:选区内部是原始像素(选放大镜面板之外的点)。
        assert_eq!(read(190, 60), [10, 200, 90]);
        // 选区外是暗幕(避开所有面板)。
        assert_eq!(read(10, 190), [dimmed[0], dimmed[1], dimmed[2]]);
        // 操作条按钮 hitbox 内是被绘制的面板像素(选第三个按钮,避开光标处放大镜)。
        let buttons = toolbar_buttons(flags);
        let panel = toolbar_panel(selection, (320, 200), &buttons).unwrap();
        let (_, last) = toolbar_button_rects(panel, &buttons)
            .last()
            .copied()
            .unwrap();
        let (cx, cy) = last.center();
        assert_ne!(read(cx, cy), [dimmed[0], dimmed[1], dimmed[2]]);
        // 描边:选区左边框(避开手柄与放大镜面板)为强调色。
        assert_eq!(read(41, 90), [ACCENT[0], ACCENT[1], ACCENT[2]]);
    }
}
