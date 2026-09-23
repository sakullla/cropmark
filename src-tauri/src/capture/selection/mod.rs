//! 平台无关选区引擎:输入事件状态机 + 合成场景描述。
//!
//! 引擎只消费物理像素坐标的输入事件,产出选区矩形、确认(Enter)/取消(Esc)
//! 语义与操作条/右键菜单动作;像素合成见 `composer`,文字绘制见 `text`。
//! 平台壳只负责创建置顶窗、转发输入事件(物理像素坐标)与呈现合成位图,
//! 不解释交互语义。所有坐标均为相对冻结帧的物理像素。

// 引擎的 crate 内消费者是后续平台壳任务(win/macos/linux shell);
// 本任务内仅测试引用,先整体豁免 dead_code,壳接入后移除。
#![allow(dead_code)]

pub mod composer;
pub(crate) mod icons;
mod text;

use crate::annotate::{Annotation, Point, DEFAULT_COLOR};
use crate::capture::geometry::PhysicalRect;
use composer::{ChromeMetrics, IntRect};

/// 选区最小可截尺寸(物理像素),对齐现 Windows 原生路径的 ≥2px。
pub const MIN_SELECTION_SIZE: u32 = 2;
/// 键盘微调步长:默认 1 物理像素,Shift 为 10。
pub const KEY_STEP: i32 = 1;
pub const KEY_STEP_LARGE: i32 = 10;
/// 拖动式标注草稿的最小边长(物理像素),与预览编辑器 `MIN_DRAW_SIZE` 对齐。
pub const MIN_DRAW_SIZE: i32 = 3;

/// 选区即时标注工具(R21);工具集与预览编辑器一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationTool {
    Rect,
    Ellipse,
    Line,
    Arrow,
    Number,
    Text,
    Pen,
    Highlighter,
    Mosaic,
    Blur,
}

impl AnnotationTool {
    /// 工具条展示顺序(绘制类在前,序号/文字居中,遮盖类在后)。
    pub const ALL: [Self; 10] = [
        Self::Rect,
        Self::Ellipse,
        Self::Line,
        Self::Arrow,
        Self::Number,
        Self::Text,
        Self::Pen,
        Self::Highlighter,
        Self::Mosaic,
        Self::Blur,
    ];

    /// 精简工具条主行工具(R21 修订):最常用的四个保持单行常驻。
    pub const PRIMARY: [Self; 4] = [Self::Rect, Self::Ellipse, Self::Arrow, Self::Text];

    /// 收进「更多」展开行的工具(直线/序号/画笔/荧光笔/马赛克/模糊)。
    pub const MORE: [Self; 6] = [
        Self::Line,
        Self::Number,
        Self::Pen,
        Self::Highlighter,
        Self::Mosaic,
        Self::Blur,
    ];

    fn is_drag(self) -> bool {
        matches!(
            self,
            Self::Rect | Self::Ellipse | Self::Line | Self::Arrow | Self::Mosaic | Self::Blur
        )
    }

    fn is_freehand(self) -> bool {
        matches!(self, Self::Pen | Self::Highlighter)
    }
}

/// 标注样式与文本输入能力,由会话层按 `AnnotationDefaults` 与平台壳能力注入。
#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationOptions {
    pub color: String,
    pub stroke_width: Option<f64>,
    pub text_size: Option<f64>,
    pub number_start: u32,
    /// 平台壳是否具备文本输入通道(Windows WM_CHAR/IME、macOS
    /// NSTextInputClient、Linux X11 XIM/直输)。为假时工具条不出现文字工具,
    /// 选择/确认/取消完全不受影响。
    pub text_input: bool,
}

impl Default for AnnotationOptions {
    fn default() -> Self {
        Self {
            color: DEFAULT_COLOR.into(),
            stroke_width: None,
            text_size: None,
            number_start: 1,
            text_input: false,
        }
    }
}

impl AnnotationOptions {
    fn color_rgba(&self) -> [u8; 4] {
        crate::annotate::parse_hex_color(&self.color).unwrap_or(crate::annotate::raster::STROKE)
    }
}

/// 撤销/重做栈条目:记录图元的增删及位置,重放即可双向恢复。
#[derive(Debug, Clone, PartialEq)]
enum AnnotationEdit {
    Add { index: usize, op: Annotation },
    Remove { index: usize, op: Annotation },
}

/// 选区内的文本编辑会话(未提交);`preedit` 为 IME 组合串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub x: i32,
    pub y: i32,
    pub text: String,
    pub preedit: String,
}

impl TextEdit {
    /// 已提交文本 + 组合串,合成器按此绘制并可在末尾画光标。
    pub fn display(&self) -> String {
        let mut out = self.text.clone();
        out.push_str(&self.preedit);
        out
    }
}

/// 合成器标注层:已确认图元 + 拖动草稿 + 文本编辑态 + 生效样式。
/// 引擎提供只读视图,壳不解释内容直接交给 `Composer` 绘制。
#[derive(Debug, Clone, Copy)]
pub struct AnnotationOverlay<'a> {
    pub annotations: &'a [Annotation],
    pub draft: Option<&'a Annotation>,
    pub tool: Option<AnnotationTool>,
    pub text: Option<&'a TextEdit>,
    /// 已确认图元的变更序号(合成器缓存失效键)。
    pub revision: u64,
    pub color: [u8; 4],
    pub text_size: f32,
    /// 平台壳文本输入能力(决定工具条是否含文字工具)。
    pub text_input: bool,
}

/// 功能入口开关(默认全开);操作条/菜单动作集由此决定。
/// `cursor_hints` 为 R24 光标提示开关:关闭后 `cursor_for` 固定十字,
/// 交互(选择/确认/取消)保持不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureFlags {
    pub ocr_entry: bool,
    pub pin_entry: bool,
    pub magnifier: bool,
    pub toolbar_copy: bool,
    pub toolbar_save: bool,
    pub toolbar_pin: bool,
    pub cursor_hints: bool,
    /// R21:选区即时标注;关闭后选区不出现标注工具,`标注` 动作仍进预览编辑器。
    pub inline_annotation: bool,
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self {
            ocr_entry: true,
            pin_entry: true,
            magnifier: true,
            toolbar_copy: true,
            toolbar_save: true,
            toolbar_pin: true,
            cursor_hints: true,
            inline_annotation: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

/// 边缘拉伸的命中边(边线 ±EDGE_HIT_RADIUS);拖动=沿该边法向轴 resize。
/// 新增命中类型,不改变既有 8 向 Handle 语义(角/边中手柄优先)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    North,
    East,
    South,
    West,
}

/// 光标提示:平台壳据此切换系统光标(Windows 壳在 WM_SETCURSOR 消费)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorHint {
    Crosshair,
    Move,
    ResizeNS,
    ResizeEW,
    ResizeNWSE,
    ResizeNESW,
    /// chrome 可点击元素(图标轨按钮/菜单项):手型。
    Pointer,
    /// 放大镜面板上方(只读区):默认箭头。
    Arrow,
}

impl CursorHint {
    fn for_handle(kind: HandleKind) -> Self {
        match kind {
            HandleKind::North | HandleKind::South => Self::ResizeNS,
            HandleKind::East | HandleKind::West => Self::ResizeEW,
            HandleKind::NorthWest | HandleKind::SouthEast => Self::ResizeNWSE,
            HandleKind::NorthEast | HandleKind::SouthWest => Self::ResizeNESW,
        }
    }

    fn for_edge(edge: EdgeKind) -> Self {
        match edge {
            EdgeKind::North | EdgeKind::South => Self::ResizeNS,
            EdgeKind::East | EdgeKind::West => Self::ResizeEW,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalKey {
    Enter,
    Escape,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    /// 取色快捷键(平台壳把 C 键映射到这里)。
    CopyColor,
    /// 即时标注:工具快捷键(A/R/E/L/M/B/H/P/N/T,与预览编辑器一致);
    /// 按下即进入标注模式并选中该工具。
    Tool(AnnotationTool),
    /// 即时标注:撤销(Ctrl+Z)。
    Undo,
    /// 即时标注:重做(Ctrl+Y / Ctrl+Shift+Z)。
    Redo,
    /// 即时标注文本退格/删除(Backspace/Delete)。
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAction {
    Copy,
    Save,
    Pin,
    Annotate,
    Ocr,
    Cancel,
    /// 复制放大镜当前指向像素的色值文本。
    CopyColor,
    /// 即时标注工具切换/撤销/重做/删除:由引擎内部消费,平台壳不解释
    /// (`EngineOutcome` 不会把这类动作交给会话层)。
    Tool(AnnotationTool),
    Undo,
    Redo,
    Delete,
    /// 标注模式「更多」:展开/收起其余工具(仅引擎内部消费)。
    More,
}

/// 一次输入事件的处理结果;除 `None` 外都意味着需要重新合成并呈现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineOutcome {
    Redraw,
    /// Enter:确认当前选区(物理像素矩形)。
    Confirmed(PhysicalRect),
    /// Esc:取消整个会话(菜单打开时同样优先取消会话)。
    Cancelled,
    /// 操作条/右键菜单/取色快捷键动作,由平台壳执行并结束或反馈。
    Action(SelectionAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    PointerMove {
        x: i32,
        y: i32,
    },
    LeftDown {
        x: i32,
        y: i32,
    },
    LeftUp {
        x: i32,
        y: i32,
    },
    RightDown {
        x: i32,
        y: i32,
    },
    Key {
        key: LogicalKey,
        shift: bool,
    },
    /// 文本输入(WM_CHAR 直入或 IME 已提交结果)。
    Text(String),
    /// IME 组合串更新(未提交);空串表示组合结束。
    Composition(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// 尚未拖出选区(光标移动,放大镜可用)。
    Idle,
    /// 按住左键拖出新选区。
    Dragging { anchor_x: i32, anchor_y: i32 },
    /// 已有选区:手柄调整/整体移动/键盘微调/操作条/右键菜单/Enter 确认。
    Selected,
    /// 拖动手柄调整大小。
    Adjusting { handle: HandleKind },
    /// 拖动选区边线沿单轴调整大小(EdgeResize)。
    AdjustingEdge { edge: EdgeKind },
    /// 拖动选区内部整体移动。origin 是按下时的选区,每次移动用
    /// `origin + (cursor - grab)` 重算,不能把位移叠到已移动的选区上。
    Moving {
        origin: PhysicalRect,
        grab_x: i32,
        grab_y: i32,
    },
    /// 右键菜单打开。
    Menu,
    /// 操作条/菜单项按下未松开:等 LeftUp 且仍命中同一动作才触发。
    /// 若在 LeftDown 就拆掉覆盖层,鼠标尚未松开,点击会穿透到下方置顶窗。
    PressingChrome { action: SelectionAction },
    /// 标注工具拖动绘制中(起点为 anchor,草稿在 `draft`)。
    Drawing { anchor_x: i32, anchor_y: i32 },
}

/// 合成器输入:由引擎当前状态派生的一帧静态场景。
#[derive(Debug, Clone, Copy)]
pub struct Scene {
    pub selection: Option<PhysicalRect>,
    pub cursor: (i32, i32),
    pub flags: FeatureFlags,
    /// 统一横条可见(有效选区固定后的单一 chrome)。
    pub toolbar_visible: bool,
    pub menu_open: bool,
    pub menu_anchor: (i32, i32),
    /// 「更多」面板展开(收进的动作列表可见)。
    pub more_open: bool,
}

#[derive(Debug, Clone)]
pub struct SelectionEngine {
    width: u32,
    height: u32,
    flags: FeatureFlags,
    state: EngineState,
    selection: Option<PhysicalRect>,
    cursor: (i32, i32),
    menu_anchor: (i32, i32),
    /// 冻结帧 DPI 缩放:全部 chrome(菜单/操作条/徽标/手柄)与放大镜面板
    /// 尺寸/命中共用同一份派生来源(ChromeMetrics),默认 1.0。
    scale: f32,
    /// 即时标注:已确认图元(坐标相对冻结帧物理像素)。
    annotations: Vec<Annotation>,
    /// 撤销/重做栈(与预览编辑器同构的 add/remove 动作)。
    undo: Vec<AnnotationEdit>,
    redo: Vec<AnnotationEdit>,
    /// 当前工具;None = 选择/移动模式。
    tool: Option<AnnotationTool>,
    /// 「更多」面板展开(收进的动作列表可见)。
    more_open: bool,
    /// 拖动中的草稿(未入栈)。
    draft: Option<Annotation>,
    options: AnnotationOptions,
    /// 文本编辑会话;有值时键盘输入进入文本框而不是选择交互。
    text_edit: Option<TextEdit>,
    /// 已确认图元变更序号:合成器缓存以它 + 选区矩形为失效键。
    revision: u64,
}

impl SelectionEngine {
    pub fn new(width: u32, height: u32, flags: FeatureFlags) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            flags,
            state: EngineState::Idle,
            selection: None,
            cursor: (0, 0),
            menu_anchor: (0, 0),
            scale: 1.0,
            annotations: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            tool: None,
            more_open: false,
            draft: None,
            options: AnnotationOptions::default(),
            text_edit: None,
            revision: 0,
        }
    }

    /// 注入冻结帧 DPI 缩放:全部 chrome 的尺寸与命中(菜单/操作条/徽标/手柄)
    /// 随 scale 派生;不注入时按 1.0。
    pub fn with_scale(mut self, scale: f64) -> Self {
        self.scale = if scale.is_finite() && scale > 0.25 {
            (scale as f32).min(4.0)
        } else {
            1.0
        };
        self
    }

    /// 注入标注样式与文本输入能力(会话层按 `AnnotationDefaults` 提供)。
    pub fn with_annotation_options(mut self, options: AnnotationOptions) -> Self {
        self.options = options;
        self
    }

    /// 已确认图元(相对冻结帧物理像素)。
    pub fn annotations(&self) -> &[Annotation] {
        &self.annotations
    }

    /// 当前工具(None = 选择/移动模式)。
    pub fn tool(&self) -> Option<AnnotationTool> {
        self.tool
    }

    /// 「更多」面板是否展开。
    pub fn more_open(&self) -> bool {
        self.more_open
    }

    /// 统一横条是否可见:有效选区固定后(含按按钮/绘制中)恒为单条横条。
    fn toolbar_visible(&self) -> bool {
        matches!(
            self.state,
            EngineState::Selected
                | EngineState::PressingChrome { .. }
                | EngineState::Drawing { .. }
        ) && self.selection.is_some()
            && !composer::toolbar_buttons(self.flags, self.options.text_input).is_empty()
    }

    /// 文本编辑会话(壳据此定位 IME 候选窗)。
    pub fn text_edit(&self) -> Option<&TextEdit> {
        self.text_edit.as_ref()
    }

    /// 文本光标左上角(引擎物理像素);无编辑会话时为 None。
    pub fn text_caret(&self) -> Option<(i32, i32)> {
        let edit = self.text_edit.as_ref()?;
        let display = edit.display();
        let width = text::measure_width(&display, self.resolved_text_size()).unwrap_or(0.0);
        Some((edit.x + width.ceil() as i32, edit.y))
    }

    /// 生效字号:与预览 `textSize()` 同规则(基础档位 × max(DPI, 长边/1920),
    /// 下限 10),保证选区标注与预览编辑器字号一致。
    fn resolved_text_size(&self) -> f32 {
        let base = self.options.text_size.unwrap_or(16.0);
        let dpi = f64::from(self.scale).max(1.0);
        let longest = f64::from(self.width.max(self.height));
        (base * dpi.max(longest / 1920.0)).max(10.0).round() as f32
    }

    /// 合成器标注层视图(图元/草稿/工具/文本/样式同源)。
    pub fn annotation_overlay(&self) -> AnnotationOverlay<'_> {
        AnnotationOverlay {
            annotations: &self.annotations,
            draft: self.draft.as_ref(),
            tool: self.tool,
            text: self.text_edit.as_ref(),
            revision: self.revision,
            color: self.options.color_rgba(),
            text_size: self.resolved_text_size(),
            text_input: self.options.text_input,
        }
    }

    /// 当前 chrome 尺寸派生(布局/绘制/命中共用;ADR-15)。
    pub(crate) fn metrics(&self) -> ChromeMetrics {
        ChromeMetrics::for_scale(self.scale)
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn flags(&self) -> FeatureFlags {
        self.flags
    }

    pub fn state(&self) -> &EngineState {
        &self.state
    }

    pub fn selection(&self) -> Option<PhysicalRect> {
        self.selection
    }

    pub fn cursor(&self) -> (i32, i32) {
        self.cursor
    }

    pub fn menu_anchor(&self) -> (i32, i32) {
        self.menu_anchor
    }

    /// 指定点的光标提示:chrome 优先于选区几何——菜单打开时菜单项(手型)
    /// 优先;放大镜面板(箭头)恒判定;统一横条/「更多」面板可见时按钮(手型)
    /// 优先;其后才是手柄/边→resize 箭头、选区内部→move、其他→crosshair。
    /// 拖动手柄/边/移动期间保持对应提示。
    pub fn cursor_for(&self, x: i32, y: i32) -> CursorHint {
        // R24:关闭光标提示后所有位置(手柄/边/内部/chrome/空白)固定十字,
        // 三种壳(Windows/macOS/X11)都从本函数取提示,无需各自分支。
        if !self.flags.cursor_hints {
            return CursorHint::Crosshair;
        }
        match self.state {
            EngineState::Adjusting { handle } => CursorHint::for_handle(handle),
            EngineState::AdjustingEdge { edge } => CursorHint::for_edge(edge),
            EngineState::Moving { .. } => CursorHint::Move,
            EngineState::PressingChrome { .. } => CursorHint::Pointer,
            EngineState::Menu => {
                if self.hit_menu(x, y).is_some() {
                    return CursorHint::Pointer;
                }
                CursorHint::Crosshair
            }
            EngineState::Selected => {
                // chrome 优先:放大镜面板(箭头)→ 统一横条/「更多」按钮(手型)
                // → 选区几何;工具激活时选区内部为绘制十字。
                if self.flags.magnifier
                    && composer::magnifier_hit(self.cursor, self.size(), self.scale, x, y)
                {
                    return CursorHint::Arrow;
                }
                // 与 on_left_down 一致:面板在横条之上,先判面板。
                if self.hit_more_panel(x, y).is_some() {
                    return CursorHint::Pointer;
                }
                if self.toolbar_visible() && self.hit_toolbar(x, y).is_some() {
                    return CursorHint::Pointer;
                }
                if let Some(selection) = self.selection {
                    let interior = IntRect::from(selection).contains(x, y);
                    if self.tool.is_some() && interior {
                        return CursorHint::Crosshair;
                    }
                    if let Some(handle) = composer::handle_hit(self.metrics(), selection, x, y) {
                        return CursorHint::for_handle(handle);
                    }
                    if let Some(edge) = composer::edge_hit(self.metrics(), selection, x, y) {
                        return CursorHint::for_edge(edge);
                    }
                    if interior {
                        return CursorHint::Move;
                    }
                }
                CursorHint::Crosshair
            }
            EngineState::Drawing { .. } => CursorHint::Crosshair,
            EngineState::Idle | EngineState::Dragging { .. } => {
                if self.flags.magnifier
                    && composer::magnifier_hit(self.cursor, self.size(), self.scale, x, y)
                {
                    return CursorHint::Arrow;
                }
                CursorHint::Crosshair
            }
        }
    }

    /// 当前状态对应的合成场景。
    pub fn scene(&self) -> Scene {
        Scene {
            selection: self.selection,
            cursor: self.cursor,
            flags: self.flags,
            toolbar_visible: self.toolbar_visible(),
            menu_open: self.state == EngineState::Menu,
            menu_anchor: self.menu_anchor,
            more_open: self.more_open,
        }
    }

    pub fn handle_event(&mut self, event: InputEvent) -> EngineOutcome {
        match event {
            InputEvent::PointerMove { x, y } => {
                self.cursor = self.clamp_point(x, y);
                self.update_drag();
                EngineOutcome::Redraw
            }
            InputEvent::LeftDown { x, y } => {
                self.cursor = self.clamp_point(x, y);
                self.on_left_down()
            }
            InputEvent::LeftUp { x, y } => {
                self.cursor = self.clamp_point(x, y);
                self.on_left_up()
            }
            InputEvent::RightDown { x, y } => {
                self.cursor = self.clamp_point(x, y);
                self.on_right_down()
            }
            InputEvent::Key { key, shift } => self.on_key(key, shift),
            InputEvent::Text(text) => {
                self.insert_text(&text);
                EngineOutcome::Redraw
            }
            InputEvent::Composition(text) => {
                if let Some(edit) = self.text_edit.as_mut() {
                    edit.preedit = text;
                }
                EngineOutcome::Redraw
            }
        }
    }

    fn clamp_point(&self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(0, self.width as i32 - 1),
            y.clamp(0, self.height as i32 - 1),
        )
    }

    fn update_drag(&mut self) {
        match self.state {
            EngineState::Dragging { anchor_x, anchor_y } => {
                self.selection = Some(Self::drag_rect(
                    (anchor_x, anchor_y),
                    self.cursor,
                    (self.width as i32, self.height as i32),
                ));
            }
            EngineState::Adjusting { handle } => {
                if let Some(selection) = self.selection {
                    self.selection = Some(Self::resize_by_handle(
                        selection,
                        handle,
                        self.cursor,
                        (self.width as i32, self.height as i32),
                    ));
                }
            }
            EngineState::AdjustingEdge { edge } => {
                if let Some(selection) = self.selection {
                    self.selection = Some(Self::resize_by_edge(
                        selection,
                        edge,
                        self.cursor,
                        (self.width as i32, self.height as i32),
                    ));
                }
            }
            EngineState::Moving {
                origin,
                grab_x,
                grab_y,
            } => {
                self.selection = Some(Self::translate(
                    origin,
                    self.cursor.0 - grab_x,
                    self.cursor.1 - grab_y,
                    (self.width as i32, self.height as i32),
                ));
            }
            EngineState::Drawing { anchor_x, anchor_y } => {
                let point = self.clamp_to_selection(self.cursor);
                self.update_draft((anchor_x, anchor_y), point);
            }
            _ => {}
        }
    }

    fn on_left_down(&mut self) -> EngineOutcome {
        let (x, y) = self.cursor;
        // 文本编辑中点击:先提交当前文本,再按本次点击继续。
        if self.text_edit.is_some() {
            self.commit_text_edit();
        }
        match self.state {
            EngineState::Idle => {
                self.state = EngineState::Dragging {
                    anchor_x: x,
                    anchor_y: y,
                };
                self.selection = None;
                self.clear_annotations();
                EngineOutcome::Redraw
            }
            EngineState::Dragging { .. } => EngineOutcome::Redraw,
            EngineState::Selected => {
                // 「更多」面板绘制在横条之上(顶边重叠时面板可见),
                // 命中顺序必须与绘制顺序一致:先面板后横条。
                if let Some(action) = self.hit_more_panel(x, y) {
                    self.state = EngineState::PressingChrome { action };
                    return EngineOutcome::Redraw;
                }
                if let Some(action) = self.hit_toolbar(x, y) {
                    self.state = EngineState::PressingChrome { action };
                    return EngineOutcome::Redraw;
                }
                if let Some(selection) = self.selection {
                    let interior = IntRect::from(selection).contains(x, y);
                    if self.tool.is_some() && interior {
                        self.begin_annotation_draw();
                        return EngineOutcome::Redraw;
                    }
                    if let Some(handle) = composer::handle_hit(self.metrics(), selection, x, y) {
                        self.state = EngineState::Adjusting { handle };
                        return EngineOutcome::Redraw;
                    }
                    if let Some(edge) = composer::edge_hit(self.metrics(), selection, x, y) {
                        self.state = EngineState::AdjustingEdge { edge };
                        return EngineOutcome::Redraw;
                    }
                    let sel = IntRect::from(selection);
                    if sel.contains(x, y) {
                        self.state = EngineState::Moving {
                            origin: selection,
                            grab_x: x,
                            grab_y: y,
                        };
                        return EngineOutcome::Redraw;
                    }
                }
                // 选区外:清标注并重新拖出新选区(新选区从零开始标注)。
                self.state = EngineState::Dragging {
                    anchor_x: x,
                    anchor_y: y,
                };
                self.selection = None;
                self.clear_annotations();
                EngineOutcome::Redraw
            }
            EngineState::Adjusting { .. }
            | EngineState::AdjustingEdge { .. }
            | EngineState::Moving { .. }
            | EngineState::PressingChrome { .. }
            | EngineState::Drawing { .. } => EngineOutcome::Redraw,
            EngineState::Menu => {
                if let Some(action) = self.hit_menu(x, y) {
                    self.state = EngineState::PressingChrome { action };
                    return EngineOutcome::Redraw;
                }
                // 菜单外点击关闭菜单,保留选区。
                self.state = if self.selection.is_some() {
                    EngineState::Selected
                } else {
                    EngineState::Idle
                };
                EngineOutcome::Redraw
            }
        }
    }

    fn on_left_up(&mut self) -> EngineOutcome {
        match self.state {
            EngineState::Dragging { anchor_x, anchor_y } => {
                let rect = Self::drag_rect(
                    (anchor_x, anchor_y),
                    self.cursor,
                    (self.width as i32, self.height as i32),
                );
                if rect.width >= MIN_SELECTION_SIZE && rect.height >= MIN_SELECTION_SIZE {
                    self.selection = Some(rect);
                    self.state = EngineState::Selected;
                } else {
                    self.selection = None;
                    self.state = EngineState::Idle;
                }
                EngineOutcome::Redraw
            }
            EngineState::Adjusting { .. }
            | EngineState::AdjustingEdge { .. }
            | EngineState::Moving { .. } => {
                self.state = EngineState::Selected;
                EngineOutcome::Redraw
            }
            EngineState::Drawing { .. } => {
                self.commit_draft();
                self.state = EngineState::Selected;
                EngineOutcome::Redraw
            }
            EngineState::PressingChrome { action } => {
                let (x, y) = self.cursor;
                // 横条内部动作(工具切换/撤销/重做/删除/更多)与「标注」入口
                // (仅即时标注开启时)由引擎就地消费,不向会话层产出 Action;
                // 关闭即时标注时「标注」仍按既有路径交回会话层打开预览编辑器。
                if self.is_internal_annotation_action(action) {
                    let still = self.hit_toolbar(x, y) == Some(action)
                        || self.hit_more_panel(x, y) == Some(action)
                        || (action == SelectionAction::Annotate
                            && self.hit_menu(x, y) == Some(action));
                    self.state = if self.selection.is_some() {
                        EngineState::Selected
                    } else {
                        EngineState::Idle
                    };
                    if still {
                        self.apply_annotation_action(action);
                    }
                    return EngineOutcome::Redraw;
                }
                let still = self.hit_toolbar(x, y) == Some(action)
                    || self.hit_more_panel(x, y) == Some(action)
                    || self.hit_menu(x, y) == Some(action);
                self.state = if self.selection.is_some() {
                    EngineState::Selected
                } else {
                    EngineState::Idle
                };
                if still {
                    if action == SelectionAction::Cancel {
                        EngineOutcome::Cancelled
                    } else {
                        EngineOutcome::Action(action)
                    }
                } else {
                    EngineOutcome::Redraw
                }
            }
            _ => EngineOutcome::Redraw,
        }
    }

    fn on_right_down(&mut self) -> EngineOutcome {
        match self.state {
            EngineState::Selected => {
                self.menu_anchor = self.cursor;
                self.state = EngineState::Menu;
                EngineOutcome::Redraw
            }
            EngineState::Menu => {
                // 右键再次点击关闭菜单;取消语义只属于 Esc/菜单项。
                self.state = EngineState::Selected;
                EngineOutcome::Redraw
            }
            _ => EngineOutcome::Redraw,
        }
    }

    fn on_key(&mut self, key: LogicalKey, shift: bool) -> EngineOutcome {
        // 文本编辑中:Esc 退出编辑、Enter 提交、退格/删除删字;其余键不改变
        // 选择交互,也不终止会话(编辑优先于整体快捷键)。
        if self.text_edit.is_some() {
            match key {
                LogicalKey::Escape => {
                    self.text_edit = None;
                    return EngineOutcome::Redraw;
                }
                LogicalKey::Enter => {
                    self.commit_text_edit();
                    return EngineOutcome::Redraw;
                }
                LogicalKey::Delete => {
                    self.backspace_text();
                    return EngineOutcome::Redraw;
                }
                _ => return EngineOutcome::Redraw,
            }
        }
        match key {
            // Esc 分层(各层均保留选区与标注):文字编辑 → 「更多」面板 →
            // 工具选中 → 取消截图;菜单打开时 Esc 仍直接取消整个会话。
            LogicalKey::Escape => {
                if self.more_open {
                    self.more_open = false;
                    EngineOutcome::Redraw
                } else if self.tool.is_some() {
                    self.tool = None;
                    EngineOutcome::Redraw
                } else {
                    EngineOutcome::Cancelled
                }
            }
            LogicalKey::Enter => match self.selection {
                Some(rect) => EngineOutcome::Confirmed(rect),
                None => EngineOutcome::Redraw,
            },
            LogicalKey::Tool(tool) => {
                // 工具快捷键:选中态按下即选中该工具(再按取消选中);其余状态
                // (Idle/拖动中/无选区)与关闭即时标注时忽略。
                if self.state == EngineState::Selected && self.selection.is_some() {
                    self.select_tool(tool);
                }
                EngineOutcome::Redraw
            }
            LogicalKey::Undo => {
                self.undo_annotation();
                EngineOutcome::Redraw
            }
            LogicalKey::Redo => {
                self.redo_annotation();
                EngineOutcome::Redraw
            }
            LogicalKey::Delete => {
                self.delete_annotation();
                EngineOutcome::Redraw
            }
            LogicalKey::CopyColor => {
                if self.flags.magnifier {
                    EngineOutcome::Action(SelectionAction::CopyColor)
                } else {
                    EngineOutcome::Redraw
                }
            }
            key @ (LogicalKey::ArrowLeft
            | LogicalKey::ArrowRight
            | LogicalKey::ArrowUp
            | LogicalKey::ArrowDown) => {
                if self.state != EngineState::Selected || self.selection.is_none() {
                    return EngineOutcome::Redraw;
                }
                let step = if shift { KEY_STEP_LARGE } else { KEY_STEP };
                let (dx, dy) = match key {
                    LogicalKey::ArrowLeft => (-step, 0),
                    LogicalKey::ArrowRight => (step, 0),
                    LogicalKey::ArrowUp => (0, -step),
                    LogicalKey::ArrowDown => (0, step),
                    _ => (0, 0),
                };
                self.nudge(dx, dy);
                EngineOutcome::Redraw
            }
        }
    }

    /// 键盘整体移动选区,钳制在屏幕内。
    fn nudge(&mut self, dx: i32, dy: i32) {
        if let Some(selection) = self.selection {
            self.selection = Some(Self::translate(
                selection,
                dx,
                dy,
                (self.width as i32, self.height as i32),
            ));
        }
    }

    fn hit_toolbar(&self, x: i32, y: i32) -> Option<SelectionAction> {
        // 横条不可见时其几何不再可命中(不允许隐形按钮)。
        if !self.toolbar_visible() {
            return None;
        }
        self.unified_toolbar()?
            .buttons
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(action, _)| action)
    }

    fn hit_menu(&self, x: i32, y: i32) -> Option<SelectionAction> {
        let items = composer::menu_items(self.flags);
        let panel = composer::menu_panel(self.metrics(), self.menu_anchor, self.size(), &items);
        composer::menu_item_rects(self.metrics(), panel, &items)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(action, _)| action)
    }

    // ---- 选区即时标注:工具/草稿/文本编辑与撤销栈。----

    /// 统一横条布局(面板 + 逐按钮矩形);无选区或主行为空时为 None。
    pub(crate) fn unified_toolbar(&self) -> Option<composer::UnifiedToolbar> {
        let selection = self.selection?;
        composer::unified_toolbar(
            self.metrics(),
            selection,
            self.size(),
            self.flags,
            self.options.text_input,
        )
    }

    /// 「更多」面板布局(面板 + 逐动作矩形);未展开或面板为空时为 None。
    pub(crate) fn more_panel(
        &self,
    ) -> Option<(composer::IntRect, Vec<(SelectionAction, composer::IntRect)>)> {
        if !self.more_open {
            return None;
        }
        let items = composer::more_panel_buttons(self.flags);
        if items.is_empty() {
            return None;
        }
        let toolbar = self.unified_toolbar()?;
        let (_, more_rect) = *toolbar.buttons.last()?;
        let panel = composer::more_panel(
            self.metrics(),
            more_rect,
            Some(toolbar.panel),
            self.size(),
            &items,
        );
        Some((
            panel,
            composer::more_item_rects(self.metrics(), panel, &items),
        ))
    }

    fn hit_more_panel(&self, x: i32, y: i32) -> Option<SelectionAction> {
        self.more_panel()?
            .1
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(action, _)| action)
    }

    /// 点钳制到选区内部(标注绘制只发生在选区内)。
    fn clamp_to_selection(&self, point: (i32, i32)) -> (i32, i32) {
        match self.selection {
            Some(selection) => (
                point.0.clamp(
                    selection.x as i32,
                    selection.x as i32 + selection.width as i32 - 1,
                ),
                point.1.clamp(
                    selection.y as i32,
                    selection.y as i32 + selection.height as i32 - 1,
                ),
            ),
            None => point,
        }
    }

    /// 按下左键开始一次标注:序号即落点、文字进入编辑、其余工具起草稿。
    fn begin_annotation_draw(&mut self) {
        let Some(tool) = self.tool else {
            return;
        };
        let point = self.clamp_to_selection(self.cursor);
        match tool {
            AnnotationTool::Number => {
                let value = self.next_number_value();
                let op = Annotation::Number {
                    x: point.0 as f64,
                    y: point.1 as f64,
                    value,
                    size: self.resolved_text_size() as f64,
                    color: self.options.color.clone(),
                };
                self.push_annotation(op);
            }
            AnnotationTool::Text => self.begin_text_edit(point.0, point.1),
            _ => {
                self.state = EngineState::Drawing {
                    anchor_x: point.0,
                    anchor_y: point.1,
                };
                let op = self.draft_for(point, point);
                self.draft = Some(op);
            }
        }
    }

    /// 按当前工具与选项生成 (anchor → cursor) 草稿图元;与预览 `draft()` 同规则。
    fn draft_for(&self, anchor: (i32, i32), cursor: (i32, i32)) -> Annotation {
        let tool = self.tool.unwrap_or(AnnotationTool::Rect);
        let color = self.options.color.clone();
        let stroke_width = self.options.stroke_width;
        if tool.is_freehand() {
            let points = vec![
                Point {
                    x: anchor.0 as f64,
                    y: anchor.1 as f64,
                },
                Point {
                    x: cursor.0 as f64,
                    y: cursor.1 as f64,
                },
            ];
            return match tool {
                AnnotationTool::Highlighter => Annotation::Highlighter {
                    points,
                    color,
                    stroke_width,
                },
                _ => Annotation::Pen {
                    points,
                    color,
                    stroke_width,
                },
            };
        }
        if matches!(tool, AnnotationTool::Line | AnnotationTool::Arrow) {
            let from = Point {
                x: anchor.0 as f64,
                y: anchor.1 as f64,
            };
            let to = Point {
                x: cursor.0 as f64,
                y: cursor.1 as f64,
            };
            return match tool {
                AnnotationTool::Arrow => Annotation::Arrow {
                    from,
                    to,
                    color,
                    stroke_width,
                },
                _ => Annotation::Line {
                    from,
                    to,
                    color,
                    stroke_width,
                },
            };
        }
        let x = anchor.0.min(cursor.0) as f64;
        let y = anchor.1.min(cursor.1) as f64;
        let width = (cursor.0 - anchor.0).unsigned_abs() as f64;
        let height = (cursor.1 - anchor.1).unsigned_abs() as f64;
        match tool {
            AnnotationTool::Mosaic => Annotation::Mosaic {
                x,
                y,
                width,
                height,
                block: mosaic_block(self.scale),
            },
            AnnotationTool::Blur => Annotation::Blur {
                x,
                y,
                width,
                height,
                sigma: blur_sigma(width, height),
            },
            AnnotationTool::Ellipse => Annotation::Ellipse {
                x,
                y,
                width,
                height,
                color,
                stroke_width,
            },
            _ => Annotation::Rect {
                x,
                y,
                width,
                height,
                color,
                stroke_width,
            },
        }
    }

    fn update_draft(&mut self, anchor: (i32, i32), cursor: (i32, i32)) {
        let Some(tool) = self.tool else {
            return;
        };
        if tool.is_freehand() {
            if let Some(Annotation::Pen { points, .. } | Annotation::Highlighter { points, .. }) =
                self.draft.as_mut()
            {
                let last = points.last().copied();
                let reaches = last.map_or(true, |point| {
                    (point.x - cursor.0 as f64).hypot(point.y - cursor.1 as f64) >= 1.0
                });
                if reaches {
                    points.push(Point {
                        x: cursor.0 as f64,
                        y: cursor.1 as f64,
                    });
                }
            }
            return;
        }
        let next = self.draft_for(anchor, cursor);
        self.draft = Some(next);
    }

    /// 松开左键提交草稿:退化图元(与预览 MIN_DRAW_SIZE 同规则)不入栈。
    fn commit_draft(&mut self) {
        let Some(op) = self.draft.take() else {
            return;
        };
        if !is_meaningful_draft(&op) {
            return;
        }
        self.push_annotation(op);
    }

    /// 开始拖新选区时清空标注会话:图元、撤销/重做栈与编辑态都属于
    /// 当前选区,避免旧图元落到新选区上;工具与「更多」面板同时复位
    /// (新选区从默认态开始)。
    fn clear_annotations(&mut self) {
        self.tool = None;
        self.more_open = false;
        if self.annotations.is_empty()
            && self.undo.is_empty()
            && self.redo.is_empty()
            && self.draft.is_none()
            && self.text_edit.is_none()
        {
            return;
        }
        self.annotations.clear();
        self.undo.clear();
        self.redo.clear();
        self.draft = None;
        self.text_edit = None;
        self.revision = self.revision.wrapping_add(1);
    }

    /// 图元入栈并记录撤销动作;新动作清空重做栈。
    fn push_annotation(&mut self, op: Annotation) {
        let index = self.annotations.len();
        self.annotations.push(op.clone());
        self.undo.push(AnnotationEdit::Add { index, op });
        self.redo.clear();
        self.revision = self.revision.wrapping_add(1);
    }

    /// 删除:优先删除光标下的图元,否则删除最近一个;可撤销。
    fn delete_annotation(&mut self) -> bool {
        let index = self
            .annotation_at(self.cursor.0, self.cursor.1)
            .or_else(|| self.annotations.len().checked_sub(1));
        let Some(index) = index else {
            return false;
        };
        let op = self.annotations.remove(index);
        self.undo.push(AnnotationEdit::Remove { index, op });
        self.redo.clear();
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn undo_annotation(&mut self) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        match &edit {
            AnnotationEdit::Add { index, .. } => {
                if *index < self.annotations.len() {
                    self.annotations.remove(*index);
                }
            }
            AnnotationEdit::Remove { index, op } => {
                let index = (*index).min(self.annotations.len());
                self.annotations.insert(index, op.clone());
            }
        }
        self.redo.push(edit);
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn redo_annotation(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        match &edit {
            AnnotationEdit::Add { index, op } => {
                let index = (*index).min(self.annotations.len());
                self.annotations.insert(index, op.clone());
            }
            AnnotationEdit::Remove { index, .. } => {
                if *index < self.annotations.len() {
                    self.annotations.remove(*index);
                }
            }
        }
        self.undo.push(edit);
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// 横条/快捷键/菜单「标注」动作:工具切换与撤销/重做/删除/更多。
    /// 「标注」仅在即时标注开启时由引擎内部消费(无-op,工具已直接可用)。
    fn apply_annotation_action(&mut self, action: SelectionAction) {
        match action {
            SelectionAction::Annotate => {}
            SelectionAction::Tool(tool) => self.select_tool(tool),
            SelectionAction::More => {
                // 面板为空(关闭即时标注且贴图/取字均关)时不展开。
                if !composer::more_panel_buttons(self.flags).is_empty() {
                    self.more_open = !self.more_open;
                }
            }
            SelectionAction::Undo => {
                self.undo_annotation();
            }
            SelectionAction::Redo => {
                self.redo_annotation();
            }
            SelectionAction::Delete => {
                self.delete_annotation();
            }
            _ => {}
        }
    }

    /// 切换工具(再次点击同一工具回到选择模式);平台无文本输入时忽略文字工具。
    /// 从「更多」面板选工具时立即收起面板。
    fn select_tool(&mut self, tool: AnnotationTool) {
        if !self.flags.inline_annotation || self.selection.is_none() {
            return;
        }
        if tool == AnnotationTool::Text && !self.options.text_input {
            return;
        }
        if self.text_edit.is_some() {
            self.commit_text_edit();
        }
        self.tool = if self.tool == Some(tool) {
            None
        } else {
            Some(tool)
        };
        self.more_open = false;
        self.draft = None;
    }

    /// 下一个序号值:取现有序号最大值 + 1(默认从 `number_start` 起),
    /// 撤销/删除后重放不会与剩余序号重复。
    fn next_number_value(&self) -> u32 {
        let max = self
            .annotations
            .iter()
            .filter_map(|op| match op {
                Annotation::Number { value, .. } => Some(*value),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        max.max(self.options.number_start.saturating_sub(1))
            .saturating_add(1)
    }

    /// 光标下最上层图元的索引(按外接框判定;用于删除)。
    fn annotation_at(&self, x: i32, y: i32) -> Option<usize> {
        self.annotations
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, op)| {
                let (min_x, min_y, max_x, max_y) = annotation_bounds(op)?;
                ((x as f64) >= min_x
                    && (x as f64) <= max_x
                    && (y as f64) >= min_y
                    && (y as f64) <= max_y)
                    .then_some(index)
            })
    }

    fn begin_text_edit(&mut self, x: i32, y: i32) {
        self.text_edit = Some(TextEdit {
            x,
            y,
            text: String::new(),
            preedit: String::new(),
        });
    }

    fn insert_text(&mut self, text: &str) {
        let Some(edit) = self.text_edit.as_mut() else {
            return;
        };
        edit.preedit.clear();
        edit.text.push_str(text);
    }

    fn backspace_text(&mut self) {
        let Some(edit) = self.text_edit.as_mut() else {
            return;
        };
        if !edit.preedit.is_empty() {
            edit.preedit.pop();
            return;
        }
        edit.text.pop();
    }

    /// 提交文本编辑:空白文本丢弃;Enter/切换工具/点击别处都会提交。
    fn commit_text_edit(&mut self) {
        let Some(edit) = self.text_edit.take() else {
            return;
        };
        let mut text = edit.text;
        text.push_str(&edit.preedit);
        if text.trim().is_empty() {
            return;
        }
        let size = self.resolved_text_size() as f64;
        let color = self.options.color.clone();
        self.push_annotation(Annotation::Text {
            x: edit.x as f64,
            y: edit.y as f64,
            text,
            size,
            color,
        });
    }

    fn drag_rect(anchor: (i32, i32), current: (i32, i32), screen: (i32, i32)) -> PhysicalRect {
        let x0 = anchor.0.min(current.0).max(0);
        let x1 = anchor.0.max(current.0).min(screen.0 - 1);
        let y0 = anchor.1.min(current.1).max(0);
        let y1 = anchor.1.max(current.1).min(screen.1 - 1);
        PhysicalRect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0 + 1).max(1) as u32,
            height: (y1 - y0 + 1).max(1) as u32,
        }
    }

    fn resize_by_handle(
        rect: PhysicalRect,
        handle: HandleKind,
        cursor: (i32, i32),
        screen: (i32, i32),
    ) -> PhysicalRect {
        let min = MIN_SELECTION_SIZE as i32 - 1;
        let (mut x0, mut y0) = (rect.x as i32, rect.y as i32);
        let (mut x1, mut y1) = (x0 + rect.width as i32 - 1, y0 + rect.height as i32 - 1);
        match handle {
            HandleKind::NorthWest => {
                x0 = cursor.0.clamp(0, x1 - min);
                y0 = cursor.1.clamp(0, y1 - min);
            }
            HandleKind::North => y0 = cursor.1.clamp(0, y1 - min),
            HandleKind::NorthEast => {
                x1 = cursor.0.clamp(x0 + min, screen.0 - 1);
                y0 = cursor.1.clamp(0, y1 - min);
            }
            HandleKind::East => x1 = cursor.0.clamp(x0 + min, screen.0 - 1),
            HandleKind::SouthEast => {
                x1 = cursor.0.clamp(x0 + min, screen.0 - 1);
                y1 = cursor.1.clamp(y0 + min, screen.1 - 1);
            }
            HandleKind::South => y1 = cursor.1.clamp(y0 + min, screen.1 - 1),
            HandleKind::SouthWest => {
                x0 = cursor.0.clamp(0, x1 - min);
                y1 = cursor.1.clamp(y0 + min, screen.1 - 1);
            }
            HandleKind::West => x0 = cursor.0.clamp(0, x1 - min),
        }
        PhysicalRect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0 + 1) as u32,
            height: (y1 - y0 + 1) as u32,
        }
    }

    /// 边缘拉伸:只沿该边法向轴调整,钳制画布内、最小尺寸约束与手柄一致。
    fn resize_by_edge(
        rect: PhysicalRect,
        edge: EdgeKind,
        cursor: (i32, i32),
        screen: (i32, i32),
    ) -> PhysicalRect {
        let min = MIN_SELECTION_SIZE as i32 - 1;
        let (mut x0, mut y0) = (rect.x as i32, rect.y as i32);
        let (mut x1, mut y1) = (x0 + rect.width as i32 - 1, y0 + rect.height as i32 - 1);
        match edge {
            EdgeKind::North => y0 = cursor.1.clamp(0, y1 - min),
            EdgeKind::South => y1 = cursor.1.clamp(y0 + min, screen.1 - 1),
            EdgeKind::West => x0 = cursor.0.clamp(0, x1 - min),
            EdgeKind::East => x1 = cursor.0.clamp(x0 + min, screen.0 - 1),
        }
        PhysicalRect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0 + 1) as u32,
            height: (y1 - y0 + 1) as u32,
        }
    }

    fn translate(rect: PhysicalRect, dx: i32, dy: i32, screen: (i32, i32)) -> PhysicalRect {
        let x = (rect.x as i32 + dx).clamp(0, (screen.0 - rect.width as i32).max(0));
        let y = (rect.y as i32 + dy).clamp(0, (screen.1 - rect.height as i32).max(0));
        PhysicalRect {
            x: x as u32,
            y: y as u32,
            width: rect.width,
            height: rect.height,
        }
    }

    /// 横条内部动作(工具/撤销/重做/删除/更多)与「标注」入口:引擎内部消费,
    /// 壳不解释。关闭即时标注时「标注」不属于内部动作,继续交回会话层进预览
    /// 编辑器。
    fn is_internal_annotation_action(&self, action: SelectionAction) -> bool {
        match action {
            SelectionAction::Tool(_)
            | SelectionAction::Undo
            | SelectionAction::Redo
            | SelectionAction::Delete
            | SelectionAction::More => true,
            SelectionAction::Annotate => self.flags.inline_annotation,
            _ => false,
        }
    }
}

/// 拖动草稿是否达到入栈下限;与预览编辑器 `MIN_DRAW_SIZE`/折线长度规则一致。
fn is_meaningful_draft(op: &Annotation) -> bool {
    match op {
        Annotation::Arrow { from, to, .. } | Annotation::Line { from, to, .. } => {
            (from.x - to.x).hypot(from.y - to.y) >= f64::from(MIN_DRAW_SIZE)
        }
        Annotation::Rect { width, height, .. }
        | Annotation::Ellipse { width, height, .. }
        | Annotation::Mosaic { width, height, .. }
        | Annotation::Blur { width, height, .. } => {
            width.abs() >= f64::from(MIN_DRAW_SIZE) && height.abs() >= f64::from(MIN_DRAW_SIZE)
        }
        Annotation::Pen { points, .. } | Annotation::Highlighter { points, .. } => {
            polyline_length(points) >= 1.0
        }
        _ => op.is_exportable(),
    }
}

fn polyline_length(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|pair| (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y))
        .sum()
}

/// 马赛克块边长(物理像素):与预览 `mosaicBlock()` 同规则,随冻结帧 scale。
fn mosaic_block(scale: f32) -> u32 {
    (12.0 * scale.max(1.0)).round().max(8.0) as u32
}

/// 模糊强度:与预览 `blurSigma()` 同规则(区域短边自适应,3–48)。
fn blur_sigma(width: f64, height: f64) -> f64 {
    (width.min(height) / 8.0).clamp(3.0, 48.0).round()
}

/// 图元外接框(命中删除用);文本按标注自带字号测量。
fn annotation_bounds(op: &Annotation) -> Option<(f64, f64, f64, f64)> {
    let rect_bounds = |x: f64, y: f64, width: f64, height: f64| {
        let (x0, x1) = if width < 0.0 {
            (x + width, x)
        } else {
            (x, x + width)
        };
        let (y0, y1) = if height < 0.0 {
            (y + height, y)
        } else {
            (y, y + height)
        };
        (x0, y0, x1, y1)
    };
    match op {
        Annotation::Rect {
            x,
            y,
            width,
            height,
            ..
        }
        | Annotation::Ellipse {
            x,
            y,
            width,
            height,
            ..
        }
        | Annotation::Mosaic {
            x,
            y,
            width,
            height,
            ..
        }
        | Annotation::Blur {
            x,
            y,
            width,
            height,
            ..
        } => Some(rect_bounds(*x, *y, *width, *height)),
        Annotation::Arrow { from, to, .. } | Annotation::Line { from, to, .. } => Some((
            from.x.min(to.x),
            from.y.min(to.y),
            from.x.max(to.x),
            from.y.max(to.y),
        )),
        Annotation::Text {
            x, y, text, size, ..
        } => {
            let size = (*size as f32).max(10.0);
            let width = text::measure_width(text, size).unwrap_or(0.0) as f64;
            Some((*x, *y, x + width, y + text::line_height(size) as f64))
        }
        Annotation::Number {
            x, y, value, size, ..
        } => {
            let size = (*size as f32).max(10.0);
            let width = text::measure_width(&value.to_string(), size).unwrap_or(0.0) as f64;
            Some((*x, *y, x + width, y + text::line_height(size) as f64))
        }
        Annotation::Pen { points, .. } | Annotation::Highlighter { points, .. } => {
            let first = points.first()?;
            let mut bounds = (first.x, first.y, first.x, first.y);
            for point in points {
                bounds.0 = bounds.0.min(point.x);
                bounds.1 = bounds.1.min(point.y);
                bounds.2 = bounds.2.max(point.x);
                bounds.3 = bounds.3.max(point.y);
            }
            Some(bounds)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 选区几何/交互测试基准:关闭即时标注与光标提示之外无关的开关,
    /// 避免横条覆盖选区内部探针;标注行为由专门测试覆盖。
    fn new_engine() -> SelectionEngine {
        SelectionEngine::new(
            320,
            200,
            FeatureFlags {
                inline_annotation: false,
                ..FeatureFlags::default()
            },
        )
    }

    fn drag(engine: &mut SelectionEngine, from: (i32, i32), to: (i32, i32)) {
        engine.handle_event(InputEvent::LeftDown {
            x: from.0,
            y: from.1,
        });
        engine.handle_event(InputEvent::PointerMove { x: to.0, y: to.1 });
        engine.handle_event(InputEvent::LeftUp { x: to.0, y: to.1 });
    }

    #[test]
    fn drag_creates_selection_and_enter_confirms_it() {
        let mut engine = new_engine();
        engine.handle_event(InputEvent::LeftDown { x: 10, y: 10 });
        engine.handle_event(InputEvent::PointerMove { x: 110, y: 60 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 10,
                y: 10,
                width: 101,
                height: 51
            })
        );
        assert_eq!(
            engine.state(),
            &EngineState::Dragging {
                anchor_x: 10,
                anchor_y: 10
            }
        );
        engine.handle_event(InputEvent::LeftUp { x: 110, y: 60 });
        assert_eq!(engine.state(), &EngineState::Selected);
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Confirmed(PhysicalRect {
                x: 10,
                y: 10,
                width: 101,
                height: 51
            })
        );
    }

    #[test]
    fn tiny_drag_is_discarded_on_release() {
        let mut engine = new_engine();
        drag(&mut engine, (10, 10), (11, 10));
        assert_eq!(engine.selection(), None);
        assert_eq!(engine.state(), &EngineState::Idle);
        // 无选区时 Enter 不产出确认,横条也不可见(<2px 可重拖)。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Redraw
        );
        assert!(!engine.scene().toolbar_visible);
    }

    #[test]
    fn keyboard_nudge_steps_one_and_ten_and_clamps_to_screen() {
        let mut engine = SelectionEngine::new(200, 150, FeatureFlags::default());
        drag(&mut engine, (100, 50), (180, 120)); // 81×71
        let nudge = |engine: &mut SelectionEngine, key: LogicalKey, shift: bool| {
            engine.handle_event(InputEvent::Key { key, shift })
        };
        nudge(&mut engine, LogicalKey::ArrowRight, true);
        assert_eq!(engine.selection().unwrap().x, 110);
        // 110+81+10=201 超出 200,第二步即钳制到 119,第三步保持 119。
        nudge(&mut engine, LogicalKey::ArrowRight, true);
        assert_eq!(engine.selection().unwrap().x, 119);
        nudge(&mut engine, LogicalKey::ArrowRight, true);
        assert_eq!(engine.selection().unwrap().x, 119);
        nudge(&mut engine, LogicalKey::ArrowLeft, false);
        assert_eq!(engine.selection().unwrap().x, 118);
        // 上边界钳制到 0。
        for _ in 0..60 {
            nudge(&mut engine, LogicalKey::ArrowUp, true);
        }
        assert_eq!(engine.selection().unwrap().y, 0);
        // 尺寸不变。
        let sel = engine.selection().unwrap();
        assert_eq!((sel.width, sel.height), (81, 71));
    }

    #[test]
    fn arrows_without_selection_are_noop() {
        let mut engine = new_engine();
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::ArrowLeft,
                shift: true
            }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.selection(), None);
    }

    #[test]
    fn escape_cancels_from_dragging_and_menu_states() {
        let mut engine = new_engine();
        engine.handle_event(InputEvent::LeftDown { x: 10, y: 10 });
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false
            }),
            EngineOutcome::Cancelled
        );
        let mut engine = new_engine();
        drag(&mut engine, (20, 20), (100, 80));
        engine.handle_event(InputEvent::RightDown { x: 60, y: 50 });
        assert_eq!(engine.state(), &EngineState::Menu);
        // 菜单打开时 Esc 仍取消整个会话。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false
            }),
            EngineOutcome::Cancelled
        );
    }

    #[test]
    fn handle_adjust_enforces_minimum_size_and_screen_clamp() {
        let mut engine = new_engine();
        drag(&mut engine, (20, 20), (100, 80)); // (20,20)-(100,80)
        assert_eq!(
            engine.handle_event(InputEvent::LeftDown { x: 20, y: 20 }),
            EngineOutcome::Redraw
        );
        assert_eq!(
            engine.state(),
            &EngineState::Adjusting {
                handle: HandleKind::NorthWest
            }
        );
        // 把 NW 拖过 SE 角:钳制为最小尺寸。
        engine.handle_event(InputEvent::PointerMove { x: 150, y: 150 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 99,
                y: 79,
                width: 2,
                height: 2
            })
        );
        engine.handle_event(InputEvent::LeftUp { x: 150, y: 150 });
        assert_eq!(engine.state(), &EngineState::Selected);
    }

    #[test]
    fn dragging_interior_moves_selection_and_clamps_inside_screen() {
        let mut engine = new_engine();
        drag(&mut engine, (20, 20), (100, 80)); // 81×61 at (20,20)
        engine.handle_event(InputEvent::LeftDown { x: 60, y: 50 });
        assert!(matches!(engine.state(), EngineState::Moving { .. }));
        engine.handle_event(InputEvent::PointerMove { x: 300, y: -50 });
        let sel = engine.selection().unwrap();
        assert_eq!((sel.width, sel.height), (81, 61));
        assert_eq!(sel.x, 320 - 81);
        assert_eq!(sel.y, 0);
        engine.handle_event(InputEvent::LeftUp { x: 300, y: -50 });
        assert_eq!(engine.state(), &EngineState::Selected);
    }

    #[test]
    fn moving_selection_tracks_cursor_one_to_one_across_multiple_moves() {
        let mut engine = new_engine();
        drag(&mut engine, (20, 20), (100, 80)); // 81×61 at (20,20)
        engine.handle_event(InputEvent::LeftDown { x: 60, y: 50 });
        engine.handle_event(InputEvent::PointerMove { x: 70, y: 55 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 30,
                y: 25,
                width: 81,
                height: 61
            })
        );
        engine.handle_event(InputEvent::PointerMove { x: 80, y: 60 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 40,
                y: 30,
                width: 81,
                height: 61
            })
        );
        engine.handle_event(InputEvent::PointerMove { x: 50, y: 40 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 10,
                y: 10,
                width: 81,
                height: 61
            })
        );
    }

    #[test]
    fn left_down_outside_selection_starts_a_new_drag() {
        let mut engine = new_engine();
        drag(&mut engine, (20, 20), (100, 80));
        engine.handle_event(InputEvent::LeftDown { x: 250, y: 180 });
        assert!(matches!(engine.state(), EngineState::Dragging { .. }));
        assert_eq!(engine.selection(), None);
    }

    #[test]
    fn toolbar_press_emits_matching_action() {
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120));
        let flags = engine.flags();
        let buttons = composer::toolbar_buttons(flags, false);
        let toolbar = engine.unified_toolbar().expect("toolbar");
        assert_eq!(toolbar.buttons.len(), buttons.len());
        // 选保存按钮(非末项取消/更多,取消走 Cancelled)。
        let (expected, rect) = toolbar
            .buttons
            .iter()
            .find(|(action, _)| *action == SelectionAction::Save)
            .copied()
            .unwrap();
        let (cx, cy) = rect.center();
        assert_eq!(
            engine.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Redraw
        );
        assert!(matches!(
            engine.state(),
            EngineState::PressingChrome { action } if *action == expected
        ));
        assert_eq!(
            engine.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
            EngineOutcome::Action(expected)
        );
        // 关闭复制开关后,动作集与命中都不再出现复制(首位动作变为保存)。
        let off = FeatureFlags {
            inline_annotation: false,
            toolbar_copy: false,
            ..FeatureFlags::default()
        };
        let mut restricted = SelectionEngine::new(320, 200, off);
        drag(&mut restricted, (40, 30), (200, 120));
        let buttons = composer::toolbar_buttons(off, false);
        assert!(!buttons.contains(&SelectionAction::Copy));
        let toolbar = restricted.unified_toolbar().expect("toolbar");
        let (_, first) = toolbar.buttons.first().copied().unwrap();
        let (fx, fy) = first.center();
        assert_eq!(
            restricted.hit_toolbar(fx, fy),
            Some(SelectionAction::Annotate)
        );
        // 保存仍在且可命中。
        let (_, save_rect) = toolbar
            .buttons
            .iter()
            .find(|(action, _)| *action == SelectionAction::Save)
            .copied()
            .unwrap();
        let (sx, sy) = save_rect.center();
        assert_eq!(restricted.hit_toolbar(sx, sy), Some(SelectionAction::Save));
    }

    #[test]
    fn menu_actions_confirm_and_cancel_correctly() {
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120));
        engine.handle_event(InputEvent::PointerMove { x: 150, y: 100 });
        assert_eq!(
            engine.handle_event(InputEvent::RightDown { x: 150, y: 100 }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.state(), &EngineState::Menu);
        assert_eq!(engine.menu_anchor(), (150, 100));
        let items = composer::menu_items(engine.flags());
        let panel = composer::menu_panel(
            engine.metrics(),
            engine.menu_anchor(),
            engine.size(),
            &items,
        );
        let rects = composer::menu_item_rects(engine.metrics(), panel, &items);
        // 点击"取字"。
        let ocr = rects
            .iter()
            .find(|(action, _)| *action == SelectionAction::Ocr)
            .copied()
            .unwrap();
        let mut with_menu = engine.clone();
        let (cx, cy) = ocr.1.center();
        assert_eq!(
            with_menu.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Redraw
        );
        assert_eq!(
            with_menu.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
            EngineOutcome::Action(SelectionAction::Ocr)
        );
        // 点击"取消"结束会话。
        let cancel = rects
            .iter()
            .find(|(action, _)| *action == SelectionAction::Cancel)
            .copied()
            .unwrap();
        let mut cancelling = engine.clone();
        let (cx, cy) = cancel.1.center();
        assert_eq!(
            cancelling.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Redraw
        );
        assert_eq!(
            cancelling.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
            EngineOutcome::Cancelled
        );
        // 菜单外点击关闭菜单并保留选区。
        let mut closing = engine;
        assert_eq!(
            closing.handle_event(InputEvent::LeftDown { x: 5, y: 5 }),
            EngineOutcome::Redraw
        );
        assert_eq!(closing.state(), &EngineState::Selected);
        assert!(closing.selection().is_some());
    }

    #[test]
    fn toolbar_press_aborted_if_released_off_button() {
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120));
        let toolbar = engine.unified_toolbar().expect("toolbar");
        let (expected, rect) = toolbar.buttons.first().copied().unwrap();
        let (cx, cy) = rect.center();
        assert_eq!(
            engine.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Redraw
        );
        assert!(matches!(
            engine.state(),
            EngineState::PressingChrome { action } if *action == expected
        ));
        assert_eq!(
            engine.handle_event(InputEvent::LeftUp { x: 5, y: 5 }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.state(), &EngineState::Selected);
        assert!(engine.selection().is_some());
    }

    #[test]
    fn copy_color_key_requires_magnifier_flag() {
        let mut engine = new_engine();
        engine.handle_event(InputEvent::PointerMove { x: 100, y: 100 });
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::CopyColor,
                shift: false
            }),
            EngineOutcome::Action(SelectionAction::CopyColor)
        );
        let mut off = new_engine();
        off.flags.magnifier = false;
        assert_eq!(
            off.handle_event(InputEvent::Key {
                key: LogicalKey::CopyColor,
                shift: false
            }),
            EngineOutcome::Redraw
        );
    }

    #[test]
    fn edge_drag_resizes_along_single_axis_and_clamps() {
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120)); // (40,30)-(200,120)
                                                 // 拖右边线(±6px 带内、手柄半径外):只改宽度。
        assert_eq!(
            engine.handle_event(InputEvent::LeftDown { x: 203, y: 100 }),
            EngineOutcome::Redraw
        );
        assert_eq!(
            engine.state(),
            &EngineState::AdjustingEdge {
                edge: EdgeKind::East
            }
        );
        engine.handle_event(InputEvent::PointerMove { x: 260, y: 100 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 40,
                y: 30,
                width: 221,
                height: 91
            })
        );
        engine.handle_event(InputEvent::LeftUp { x: 260, y: 100 });
        assert_eq!(engine.state(), &EngineState::Selected);
        // 拖下边线:只改高度,且钳制到画布底。
        engine.handle_event(InputEvent::LeftDown { x: 100, y: 120 });
        assert_eq!(
            engine.state(),
            &EngineState::AdjustingEdge {
                edge: EdgeKind::South
            }
        );
        engine.handle_event(InputEvent::PointerMove { x: 100, y: 999 });
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 40,
                y: 30,
                width: 221,
                height: 170
            })
        );
        engine.handle_event(InputEvent::LeftUp { x: 100, y: 999 });
        // 拖左边线越过右边:钳制为最小宽度,位置随动。
        engine.handle_event(InputEvent::LeftDown { x: 40, y: 100 });
        assert_eq!(
            engine.state(),
            &EngineState::AdjustingEdge {
                edge: EdgeKind::West
            }
        );
        engine.handle_event(InputEvent::PointerMove { x: 400, y: 100 });
        let sel = engine.selection().unwrap();
        assert_eq!((sel.x, sel.width), (259, 2));
        engine.handle_event(InputEvent::LeftUp { x: 400, y: 100 });
        assert_eq!(engine.state(), &EngineState::Selected);
    }

    #[test]
    fn cursor_hint_maps_handles_edges_interior_and_outside() {
        // 关闭放大镜与即时标注:本测试只核对选区几何映射(chrome 优先级另有专项测试)。
        let flags = FeatureFlags {
            magnifier: false,
            inline_annotation: false,
            ..FeatureFlags::default()
        };
        let mut engine = SelectionEngine::new(320, 200, flags);
        // 无选区:crosshair。
        assert_eq!(engine.cursor_for(10, 10), CursorHint::Crosshair);
        drag(&mut engine, (40, 30), (200, 120)); // (40,30)-(200,120)
                                                 // 角/边手柄 → 对应斜向/轴向 resize。
        assert_eq!(engine.cursor_for(40, 30), CursorHint::ResizeNWSE);
        assert_eq!(engine.cursor_for(200, 120), CursorHint::ResizeNWSE);
        assert_eq!(engine.cursor_for(200, 30), CursorHint::ResizeNESW);
        assert_eq!(engine.cursor_for(40, 120), CursorHint::ResizeNESW);
        assert_eq!(engine.cursor_for(120, 30), CursorHint::ResizeNS);
        assert_eq!(engine.cursor_for(200, 75), CursorHint::ResizeEW);
        // 边带(手柄半径外、边线 ±6px 内)→ 轴向 resize。
        assert_eq!(engine.cursor_for(160, 34), CursorHint::ResizeNS);
        assert_eq!(engine.cursor_for(44, 100), CursorHint::ResizeEW);
        assert_eq!(engine.cursor_for(160, 116), CursorHint::ResizeNS);
        // 内部 → move;外部 → crosshair。
        assert_eq!(engine.cursor_for(120, 75), CursorHint::Move);
        assert_eq!(engine.cursor_for(10, 10), CursorHint::Crosshair);
        // 拖动手柄/移动期间保持对应提示。
        engine.handle_event(InputEvent::LeftDown { x: 40, y: 30 });
        assert_eq!(engine.cursor_for(100, 100), CursorHint::ResizeNWSE);
        engine.handle_event(InputEvent::LeftUp { x: 40, y: 30 });
        engine.handle_event(InputEvent::LeftDown { x: 120, y: 75 });
        assert_eq!(engine.cursor_for(10, 10), CursorHint::Move);
        engine.handle_event(InputEvent::LeftUp { x: 120, y: 75 });
    }

    #[test]
    fn cursor_hint_prioritizes_chrome_over_selection_geometry() {
        // 关闭放大镜:本测试只核对统一横条 chrome 与选区几何的优先级。
        let flags = FeatureFlags {
            magnifier: false,
            ..FeatureFlags::default()
        };
        let mut engine = SelectionEngine::new(320, 200, flags);
        drag(&mut engine, (40, 30), (200, 120)); // (40,30)-(200,120),320×200
                                                 // 横条按钮上 → 手型(即使该点也在选区边带/内部附近)。
        let toolbar = engine.unified_toolbar().expect("toolbar");
        let (_, first) = toolbar.buttons.first().copied().unwrap();
        let (bx, by) = first.center();
        assert_eq!(engine.cursor_for(bx, by), CursorHint::Pointer);
        // 选区几何 fallback 不受影响。
        assert_eq!(engine.cursor_for(120, 75), CursorHint::Move);
    }

    #[test]
    fn cursor_hint_menu_items_take_priority_when_menu_open() {
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120));
        engine.handle_event(InputEvent::RightDown { x: 150, y: 100 });
        assert_eq!(engine.state(), &EngineState::Menu);
        let items = composer::menu_items(engine.flags());
        let panel = composer::menu_panel(
            engine.metrics(),
            engine.menu_anchor(),
            engine.size(),
            &items,
        );
        let (_, first) = composer::menu_item_rects(engine.metrics(), panel, &items)
            .first()
            .copied()
            .unwrap();
        let (mx, my) = first.center();
        // 菜单项 → 手型;菜单外(即使曾在选区内部)→ 十字。
        assert_eq!(engine.cursor_for(mx, my), CursorHint::Pointer);
        assert_eq!(engine.cursor_for(5, 5), CursorHint::Crosshair);
    }

    #[test]
    fn cursor_hint_magnifier_panel_is_arrow() {
        let mut engine = new_engine();
        // 光标移到屏幕右下角附近:放大镜面板翻转到光标左上,覆盖 (200,100) 一带。
        engine.handle_event(InputEvent::PointerMove { x: 300, y: 180 });
        let panel = composer::magnifier_rect(engine.cursor(), engine.size(), 1.0);
        let (px, py) = panel.center();
        assert_eq!(engine.cursor_for(px, py), CursorHint::Arrow);
        // 面板外仍是十字;关闭放大镜开关后不再判定。
        assert_eq!(engine.cursor_for(10, 10), CursorHint::Crosshair);
        let mut off = new_engine();
        off.flags.magnifier = false;
        off.handle_event(InputEvent::PointerMove { x: 300, y: 180 });
        assert_eq!(off.cursor_for(px, py), CursorHint::Crosshair);
    }

    #[test]
    fn cursor_hints_disabled_pins_crosshair_across_chrome_and_selection() {
        // 关闭放大镜避开面板对探针的覆盖,只比较手柄/内部/横条/菜单。
        let base = FeatureFlags {
            magnifier: false,
            ..FeatureFlags::default()
        };
        let mut on = SelectionEngine::new(320, 200, base);
        drag(&mut on, (40, 30), (200, 120));
        assert_eq!(on.cursor_for(40, 30), CursorHint::ResizeNWSE);
        assert_eq!(on.cursor_for(120, 75), CursorHint::Move);

        // 关闭光标提示:同样的探针全部固定十字(三种壳共用本函数取提示)。
        let mut off = SelectionEngine::new(
            320,
            200,
            FeatureFlags {
                cursor_hints: false,
                ..base
            },
        );
        drag(&mut off, (40, 30), (200, 120));
        assert_eq!(off.cursor_for(40, 30), CursorHint::Crosshair);
        assert_eq!(off.cursor_for(120, 75), CursorHint::Crosshair);
        assert_eq!(off.cursor_for(10, 10), CursorHint::Crosshair);
        // 横条按钮与菜单项也不切手型。
        let toolbar = off.unified_toolbar().expect("toolbar");
        let (_, toolbar_rect) = toolbar.buttons[0];
        let (bx, by) = toolbar_rect.center();
        assert_eq!(off.cursor_for(bx, by), CursorHint::Crosshair);
        off.handle_event(InputEvent::RightDown { x: 200, y: 120 });
        let items = composer::menu_items(off.flags());
        let menu = composer::menu_panel(off.metrics(), off.menu_anchor(), off.size(), &items);
        let (_, item_rect) = composer::menu_item_rects(off.metrics(), menu, &items)[0];
        let (mx, my) = item_rect.center();
        assert_eq!(off.cursor_for(mx, my), CursorHint::Crosshair);
        // 固定十字不改变交互:Enter 仍确认当前选区。
        assert!(matches!(
            off.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Confirmed(_)
        ));

        // 放大镜面板探针:开启时箭头,关闭光标提示后固定十字。
        let mut magnified = SelectionEngine::new(320, 200, FeatureFlags::default());
        magnified.handle_event(InputEvent::PointerMove { x: 300, y: 180 });
        let magnifier = composer::magnifier_rect(magnified.cursor(), magnified.size(), 1.0);
        let (px, py) = magnifier.center();
        assert_eq!(magnified.cursor_for(px, py), CursorHint::Arrow);
        let mut pinned = SelectionEngine::new(
            320,
            200,
            FeatureFlags {
                cursor_hints: false,
                ..FeatureFlags::default()
            },
        );
        pinned.handle_event(InputEvent::PointerMove { x: 300, y: 180 });
        assert_eq!(pinned.cursor_for(px, py), CursorHint::Crosshair);
    }

    #[test]
    fn terminal_keys_are_never_swallowed() {
        // 菜单打开时 Enter → 确认当前选区(不是 Redraw)。
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120));
        engine.handle_event(InputEvent::RightDown { x: 150, y: 100 });
        assert_eq!(engine.state(), &EngineState::Menu);
        assert!(matches!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Confirmed(_)
        ));
        // EdgeResize 中 Esc → Cancelled(不依赖先收到 LeftUp)。
        let mut engine = new_engine();
        drag(&mut engine, (40, 30), (200, 120));
        engine.handle_event(InputEvent::LeftDown { x: 203, y: 100 });
        assert_eq!(
            engine.state(),
            &EngineState::AdjustingEdge {
                edge: EdgeKind::East
            }
        );
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false
            }),
            EngineOutcome::Cancelled
        );
        // 无选区 Enter 不伪装终态(Redraw),Esc 始终 Cancelled。
        let mut engine = new_engine();
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Redraw
        );
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false
            }),
            EngineOutcome::Cancelled
        );
    }

    #[test]
    fn scene_reflects_engine_state_and_flags() {
        let mut engine = new_engine();
        let scene = engine.scene();
        assert!(!scene.toolbar_visible && !scene.menu_open && scene.selection.is_none());
        drag(&mut engine, (40, 30), (200, 120));
        let scene = engine.scene();
        assert!(scene.toolbar_visible);
        assert!(!scene.more_open);
        assert_eq!(scene.selection, engine.selection());
        engine.handle_event(InputEvent::RightDown { x: 100, y: 60 });
        let scene = engine.scene();
        assert!(scene.menu_open && !scene.toolbar_visible);
        assert_eq!(scene.menu_anchor, (100, 60));
        // 只关 toolbar_* 时主行仍有标注/取消/更多,默认选中态横条仍可见。
        let off = FeatureFlags {
            inline_annotation: false,
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        let mut bare = SelectionEngine::new(320, 200, off);
        drag(&mut bare, (40, 30), (200, 120));
        assert!(bare.scene().toolbar_visible);
        assert!(!composer::toolbar_buttons(off, false).contains(&SelectionAction::Copy));
        assert!(!composer::toolbar_buttons(off, false).contains(&SelectionAction::Save));
        // 右键菜单保持现状:复制/保存/贴图按同样开关过滤,标注/取消恒在。
        let menu = composer::menu_items(off);
        assert!(!menu.contains(&SelectionAction::Copy));
        assert!(!menu.contains(&SelectionAction::Save));
        assert!(!menu.contains(&SelectionAction::Pin));
        assert!(menu.contains(&SelectionAction::Annotate));
        assert!(menu.contains(&SelectionAction::Cancel));
    }

    #[test]
    fn chrome_hits_follow_scaled_metrics() {
        // 关闭即时标注:本测试只核对横条/手柄/菜单的 scale 派生。
        let flags = FeatureFlags {
            inline_annotation: false,
            ..FeatureFlags::default()
        };
        for scale in [1.5_f64, 2.0] {
            let mut engine = SelectionEngine::new(800, 600, flags).with_scale(scale);
            drag(&mut engine, (40, 30), (200, 120));
            let metrics = engine.metrics();
            let selection = engine.selection().unwrap();
            let toolbar = engine.unified_toolbar().unwrap();
            assert_eq!(toolbar.panel.height, metrics.bar_button, "scale {scale}");
            let (action, rect) = toolbar.buttons[0];
            let (cx, cy) = rect.center();
            assert_eq!(engine.hit_toolbar(cx, cy), Some(action), "scale {scale}");
            // 布局确实随 scale 派生:放大后的按钮边长/面板尺寸不同于 1.0 基准。
            let base_metrics = ChromeMetrics::for_scale(1.0);
            let base_toolbar = composer::unified_toolbar(
                base_metrics,
                selection,
                engine.size(),
                engine.flags(),
                engine.options.text_input,
            )
            .unwrap();
            assert_eq!(rect.width, metrics.bar_button, "scale {scale}");
            assert_ne!(rect.width, base_toolbar.buttons[0].1.width, "scale {scale}");
            assert_ne!(toolbar.panel.y, base_toolbar.panel.y, "scale {scale}");
            // 放大后落在 1.0 基准面板上方空隙的点(仍在选区外)不再命中。
            let (px, py) = (base_toolbar.panel.x + 2, base_toolbar.panel.y + 2);
            assert!(base_toolbar.buttons[0].1.contains(px, py));
            if py < rect.y {
                assert_ne!(engine.hit_toolbar(px, py), Some(action), "scale {scale}");
            }
            // 手柄命中半径随 scale 放大(1.0 基准半径外、缩放半径内仍命中)。
            let probe = selection.x as i32 + metrics.handle_hit_radius;
            assert_eq!(
                engine.cursor_for(probe, selection.y as i32),
                CursorHint::ResizeNWSE,
                "scale {scale}"
            );
            assert_ne!(
                engine.cursor_for(probe + 1, selection.y as i32),
                CursorHint::ResizeNWSE,
                "scale {scale}"
            );
            // 菜单项命中随 metrics 派生。
            engine.handle_event(InputEvent::RightDown { x: 300, y: 300 });
            assert_eq!(engine.state(), &EngineState::Menu);
            let items = composer::menu_items(engine.flags());
            let menu = composer::menu_panel(metrics, engine.menu_anchor(), engine.size(), &items);
            let (m_action, m_rect) = composer::menu_item_rects(metrics, menu, &items)[1];
            let (mx, my) = m_rect.center();
            assert_eq!(engine.hit_menu(mx, my), Some(m_action), "scale {scale}");
        }
    }

    // ---- 选区即时标注(统一横条)。----

    fn inline_engine(width: u32, height: u32) -> SelectionEngine {
        SelectionEngine::new(width, height, FeatureFlags::default()).with_annotation_options(
            AnnotationOptions {
                text_input: true,
                ..AnnotationOptions::default()
            },
        )
    }

    fn drag_selection(engine: &mut SelectionEngine, from: (i32, i32), to: (i32, i32)) {
        drag(engine, from, to);
    }

    /// 点击统一横条主行上指定动作的按钮中心,返回本次点击的引擎结果。
    fn click_toolbar_action(
        engine: &mut SelectionEngine,
        action: SelectionAction,
    ) -> EngineOutcome {
        let toolbar = engine.unified_toolbar().expect("toolbar");
        let (_, rect) = toolbar
            .buttons
            .into_iter()
            .find(|(candidate, _)| *candidate == action)
            .expect("toolbar button present");
        let (cx, cy) = rect.center();
        engine.handle_event(InputEvent::LeftDown { x: cx, y: cy });
        engine.handle_event(InputEvent::LeftUp { x: cx, y: cy })
    }

    /// 打开右键菜单并点击指定菜单项,返回本次点击的引擎结果。
    fn click_menu_action(engine: &mut SelectionEngine, action: SelectionAction) -> EngineOutcome {
        engine.handle_event(InputEvent::RightDown { x: 300, y: 300 });
        let items = composer::menu_items(engine.flags());
        let metrics = engine.metrics();
        let panel = composer::menu_panel(metrics, engine.menu_anchor(), engine.size(), &items);
        let (_, rect) = composer::menu_item_rects(metrics, panel, &items)
            .into_iter()
            .find(|(candidate, _)| *candidate == action)
            .expect("menu item present");
        let (cx, cy) = rect.center();
        engine.handle_event(InputEvent::LeftDown { x: cx, y: cy });
        engine.handle_event(InputEvent::LeftUp { x: cx, y: cy })
    }

    /// 展开「更多」面板。
    fn open_more_panel(engine: &mut SelectionEngine) {
        if engine.more_open() {
            return;
        }
        let outcome = click_toolbar_action(engine, SelectionAction::More);
        assert_eq!(outcome, EngineOutcome::Redraw);
        assert!(engine.more_open(), "「更多」应展开");
    }

    /// 点击指定动作:主行直接点;「更多」面板内先展开再点。
    fn click_action(engine: &mut SelectionEngine, action: SelectionAction) -> EngineOutcome {
        let on_bar = engine
            .unified_toolbar()
            .map(|toolbar| {
                toolbar
                    .buttons
                    .iter()
                    .any(|(candidate, _)| *candidate == action)
            })
            .unwrap_or(false);
        if on_bar {
            return click_toolbar_action(engine, action);
        }
        open_more_panel(engine);
        let (panel, items) = engine.more_panel().expect("more panel");
        let (_, rect) = items
            .into_iter()
            .find(|(candidate, _)| *candidate == action)
            .expect("more item present");
        let (cx, cy) = rect.center();
        assert!(panel.contains(cx, cy));
        engine.handle_event(InputEvent::LeftDown { x: cx, y: cy });
        engine.handle_event(InputEvent::LeftUp { x: cx, y: cy })
    }

    /// 面板矩形整体位于选区之外(下方/上方/侧面任一方向)。
    fn panel_is_outside_selection(panel: composer::IntRect, selection: PhysicalRect) -> bool {
        let sel = composer::IntRect::from(selection);
        panel.bottom() <= sel.y
            || panel.y >= sel.bottom()
            || panel.right() <= sel.x
            || panel.x >= sel.right()
    }

    /// 有效选区固定后只有一条横条:主行按开关矩阵,位置在选区外;
    /// 「更多」含收进的工具/重做/删除及开关允许的贴图/取字。
    #[test]
    fn fixed_selection_shows_single_unified_toolbar() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 480));
        // 选中态即显示统一横条(无第二阶段标注条)。
        assert!(engine.scene().toolbar_visible);
        assert!(!engine.scene().more_open);
        let toolbar = engine.unified_toolbar().expect("toolbar");
        let metrics = engine.metrics();
        assert_eq!(
            toolbar.panel.height, metrics.bar_button,
            "统一横条必须为一行"
        );
        assert!(
            panel_is_outside_selection(toolbar.panel, engine.selection().unwrap()),
            "横条必须位于选区外: {:?}",
            toolbar.panel
        );
        // 主行:四工具 + 撤销 + 复制 + 保存 + 取消 + 更多。
        let buttons: Vec<SelectionAction> = toolbar.buttons.iter().map(|(a, _)| *a).collect();
        assert_eq!(
            buttons,
            vec![
                SelectionAction::Tool(AnnotationTool::Rect),
                SelectionAction::Tool(AnnotationTool::Ellipse),
                SelectionAction::Tool(AnnotationTool::Arrow),
                SelectionAction::Tool(AnnotationTool::Text),
                SelectionAction::Undo,
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        // 展开「更多」:面板在选区上方弹出,含 6 工具 + 重做 + 删除 + 贴图 + 取字。
        open_more_panel(&mut engine);
        let (panel, items) = engine.more_panel().expect("more panel");
        let actions: Vec<SelectionAction> = items.iter().map(|(a, _)| *a).collect();
        assert_eq!(
            actions,
            vec![
                SelectionAction::Tool(AnnotationTool::Line),
                SelectionAction::Tool(AnnotationTool::Number),
                SelectionAction::Tool(AnnotationTool::Pen),
                SelectionAction::Tool(AnnotationTool::Highlighter),
                SelectionAction::Tool(AnnotationTool::Mosaic),
                SelectionAction::Tool(AnnotationTool::Blur),
                SelectionAction::Redo,
                SelectionAction::Delete,
                SelectionAction::Pin,
                SelectionAction::Ocr,
            ]
        );
        // 面板整体在屏幕内、底部对齐「更多」按钮。
        assert!(panel.y >= 0 && panel.right() <= 800);
        let toolbar = engine.unified_toolbar().unwrap();
        let (_, more_rect) = toolbar.buttons.last().copied().unwrap();
        assert_eq!(panel.bottom(), more_rect.y);
        assert_eq!(panel.right(), more_rect.right());

        // 平台无文本输入通道:主行不含文字工具。
        let mut no_text = SelectionEngine::new(800, 600, FeatureFlags::default());
        drag_selection(&mut no_text, (40, 30), (760, 560));
        let buttons: Vec<SelectionAction> = engine_buttons(&no_text);
        assert!(!buttons.contains(&SelectionAction::Tool(AnnotationTool::Text)));

        // 关闭复制/保存:主行与「更多」均无该项且顺序不变。
        let mut restricted = SelectionEngine::new(
            800,
            600,
            FeatureFlags {
                toolbar_copy: false,
                toolbar_save: false,
                toolbar_pin: false,
                ocr_entry: false,
                ..FeatureFlags::default()
            },
        )
        .with_annotation_options(AnnotationOptions {
            text_input: true,
            ..AnnotationOptions::default()
        });
        drag_selection(&mut restricted, (40, 30), (760, 560));
        let buttons: Vec<SelectionAction> = engine_buttons(&restricted);
        assert_eq!(
            buttons,
            vec![
                SelectionAction::Tool(AnnotationTool::Rect),
                SelectionAction::Tool(AnnotationTool::Ellipse),
                SelectionAction::Tool(AnnotationTool::Arrow),
                SelectionAction::Tool(AnnotationTool::Text),
                SelectionAction::Undo,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        open_more_panel(&mut restricted);
        let actions: Vec<SelectionAction> = restricted
            .more_panel()
            .expect("more panel")
            .1
            .iter()
            .map(|(a, _)| *a)
            .collect();
        assert_eq!(
            actions,
            vec![
                SelectionAction::Tool(AnnotationTool::Line),
                SelectionAction::Tool(AnnotationTool::Number),
                SelectionAction::Tool(AnnotationTool::Pen),
                SelectionAction::Tool(AnnotationTool::Highlighter),
                SelectionAction::Tool(AnnotationTool::Mosaic),
                SelectionAction::Tool(AnnotationTool::Blur),
                SelectionAction::Redo,
                SelectionAction::Delete,
            ]
        );
    }

    fn engine_buttons(engine: &SelectionEngine) -> Vec<SelectionAction> {
        engine
            .unified_toolbar()
            .expect("toolbar")
            .buttons
            .iter()
            .map(|(a, _)| *a)
            .collect()
    }

    #[test]
    fn tool_shortcut_selects_tool_and_toggle_deselects() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Tool(AnnotationTool::Ellipse),
                shift: false,
            }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.tool(), Some(AnnotationTool::Ellipse));
        // 横条仍可见(单一 chrome),选中态持续。
        assert!(engine.scene().toolbar_visible);
        // 再次按下同一工具取消选中。
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Tool(AnnotationTool::Ellipse),
            shift: false,
        });
        assert_eq!(engine.tool(), None);
        // 无选区/关闭开关:工具快捷键忽略,不产生图元。
        let mut off = SelectionEngine::new(
            320,
            200,
            FeatureFlags {
                inline_annotation: false,
                ..FeatureFlags::default()
            },
        );
        drag(&mut off, (20, 20), (100, 80));
        off.handle_event(InputEvent::Key {
            key: LogicalKey::Tool(AnnotationTool::Rect),
            shift: false,
        });
        assert_eq!(off.tool(), None);
    }

    /// Esc 分层:文字编辑 → 「更多」面板 → 工具选中 → 取消截图;
    /// 各层均保留选区与标注。
    #[test]
    fn escape_layers_more_tool_then_cancel() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Pen));
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 300 });
        engine.handle_event(InputEvent::PointerMove { x: 400, y: 420 });
        engine.handle_event(InputEvent::LeftUp { x: 400, y: 420 });
        assert_eq!(engine.annotations().len(), 1);
        // 「更多」面板内选工具:立即选中并关面板。
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Mosaic));
        assert_eq!(engine.tool(), Some(AnnotationTool::Mosaic));
        assert!(!engine.more_open());
        // 重新展开「更多」面板,验证 Esc 分层。
        open_more_panel(&mut engine);
        assert!(engine.more_open());
        // 第一次 Esc:收起「更多」面板,保留工具与标注。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false,
            }),
            EngineOutcome::Redraw
        );
        assert!(!engine.more_open());
        assert_eq!(engine.tool(), Some(AnnotationTool::Mosaic));
        assert_eq!(engine.annotations().len(), 1);
        assert!(engine.selection().is_some());
        // 第二次 Esc:取消工具选中,保留标注与选区。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false,
            }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.tool(), None);
        assert_eq!(engine.annotations().len(), 1);
        assert!(engine.scene().toolbar_visible);
        // 第三次 Esc:取消整个选区会话。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Escape,
                shift: false,
            }),
            EngineOutcome::Cancelled
        );
    }

    #[test]
    fn annotate_action_falls_back_to_shell_when_inline_disabled() {
        let mut engine = SelectionEngine::new(
            320,
            200,
            FeatureFlags {
                inline_annotation: false,
                ..FeatureFlags::default()
            },
        );
        drag(&mut engine, (40, 30), (200, 120));
        // 关闭即时标注时主行为 标注/复制/保存/取消/更多,「更多」只含贴图/取字。
        assert_eq!(
            engine_buttons(&engine),
            vec![
                SelectionAction::Annotate,
                SelectionAction::Copy,
                SelectionAction::Save,
                SelectionAction::Cancel,
                SelectionAction::More,
            ]
        );
        open_more_panel(&mut engine);
        let actions: Vec<SelectionAction> = engine
            .more_panel()
            .expect("more panel")
            .1
            .iter()
            .map(|(a, _)| *a)
            .collect();
        assert_eq!(actions, vec![SelectionAction::Pin, SelectionAction::Ocr]);
        // 点「标注」打开预览编辑器:动作交回会话层。
        assert_eq!(
            click_toolbar_action(&mut engine, SelectionAction::Annotate),
            EngineOutcome::Action(SelectionAction::Annotate)
        );
    }

    #[test]
    fn toolbar_cancel_cancels_session() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (400, 280));
        assert_eq!(
            click_toolbar_action(&mut engine, SelectionAction::Cancel),
            EngineOutcome::Cancelled
        );
    }

    /// 即时标注开启时菜单「标注」由引擎内部消费(无-op:工具已直接可用)。
    #[test]
    fn menu_annotate_is_internal_noop_when_inline_enabled() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 480));
        let buttons = engine_buttons(&engine);
        assert!(!buttons.contains(&SelectionAction::Annotate));
        assert_eq!(
            click_menu_action(&mut engine, SelectionAction::Annotate),
            EngineOutcome::Redraw
        );
        assert!(engine.scene().toolbar_visible);
    }

    #[test]
    fn drawing_tools_commit_annotations_and_support_undo_redo_delete() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));

        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Rect));
        assert_eq!(engine.tool(), Some(AnnotationTool::Rect));
        // 选区内(避开横条)拖出矩形。
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 300 });
        assert!(matches!(engine.state(), EngineState::Drawing { .. }));
        engine.handle_event(InputEvent::PointerMove { x: 420, y: 420 });
        engine.handle_event(InputEvent::LeftUp { x: 420, y: 420 });
        assert_eq!(engine.state(), &EngineState::Selected);
        assert_eq!(engine.annotations().len(), 1);
        assert!(matches!(engine.annotations()[0], Annotation::Rect { .. }));

        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Ellipse));
        engine.handle_event(InputEvent::LeftDown { x: 250, y: 320 });
        engine.handle_event(InputEvent::PointerMove { x: 450, y: 440 });
        engine.handle_event(InputEvent::LeftUp { x: 450, y: 440 });
        assert_eq!(engine.annotations().len(), 2);

        // Ctrl+Z 撤销、Ctrl+Y 重做(平台壳映射为 LogicalKey)。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Undo,
                shift: false
            }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.annotations().len(), 1);
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Redo,
                shift: false
            }),
            EngineOutcome::Redraw
        );
        assert_eq!(engine.annotations().len(), 2);

        // 光标停在最后一个图元内:Delete 删除它,Undo 恢复。
        engine.handle_event(InputEvent::PointerMove { x: 450, y: 440 });
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Delete,
            shift: false,
        });
        assert_eq!(engine.annotations().len(), 1);
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Undo,
            shift: false,
        });
        assert_eq!(engine.annotations().len(), 2);

        // 退化草稿不入栈(2px 高低于 MIN_DRAW_SIZE)。
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Rect));
        engine.handle_event(InputEvent::LeftDown { x: 500, y: 300 });
        engine.handle_event(InputEvent::PointerMove { x: 520, y: 301 });
        engine.handle_event(InputEvent::LeftUp { x: 520, y: 301 });
        assert_eq!(engine.annotations().len(), 2);
    }

    #[test]
    fn freehand_tools_collect_points_and_commit() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Pen));
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 300 });
        for point in [(220, 320), (260, 360), (300, 380)] {
            engine.handle_event(InputEvent::PointerMove {
                x: point.0,
                y: point.1,
            });
        }
        engine.handle_event(InputEvent::LeftUp { x: 300, y: 380 });
        match &engine.annotations()[0] {
            Annotation::Pen { points, .. } => assert!(points.len() >= 3),
            other => panic!("expected pen, got {other:?}"),
        }

        click_action(
            &mut engine,
            SelectionAction::Tool(AnnotationTool::Highlighter),
        );
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 400 });
        engine.handle_event(InputEvent::PointerMove { x: 320, y: 430 });
        engine.handle_event(InputEvent::LeftUp { x: 320, y: 430 });
        assert!(matches!(
            engine.annotations()[1],
            Annotation::Highlighter { .. }
        ));
    }

    #[test]
    fn number_tool_places_incrementing_values_and_reuses_after_undo() {
        let mut engine = inline_engine(800, 600).with_annotation_options(AnnotationOptions {
            text_input: true,
            number_start: 5,
            ..AnnotationOptions::default()
        });
        drag_selection(&mut engine, (40, 30), (760, 560));
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Number));
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 300 });
        engine.handle_event(InputEvent::LeftUp { x: 200, y: 300 });
        engine.handle_event(InputEvent::LeftDown { x: 260, y: 300 });
        engine.handle_event(InputEvent::LeftUp { x: 260, y: 300 });
        let values: Vec<u32> = engine
            .annotations()
            .iter()
            .filter_map(|op| match op {
                Annotation::Number { value, .. } => Some(*value),
                _ => None,
            })
            .collect();
        assert_eq!(values, [5, 6]);
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Undo,
            shift: false,
        });
        engine.handle_event(InputEvent::LeftDown { x: 320, y: 300 });
        engine.handle_event(InputEvent::LeftUp { x: 320, y: 300 });
        let last = engine.annotations().last().cloned().unwrap();
        assert!(matches!(last, Annotation::Number { value: 6, .. }));
    }

    #[test]
    fn text_tool_edits_commits_and_cancels() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Text));
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 300 });
        engine.handle_event(InputEvent::LeftUp { x: 200, y: 300 });
        assert!(engine.text_edit().is_some());
        // IME 组合串 + 直入字符;Enter 提交。
        engine.handle_event(InputEvent::Composition("zhong".into()));
        engine.handle_event(InputEvent::Text("中".into()));
        assert_eq!(engine.text_edit().unwrap().text, "中");
        assert!(engine.text_edit().unwrap().preedit.is_empty());
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Enter,
            shift: false,
        });
        match &engine.annotations()[0] {
            Annotation::Text { text, x, y, .. } => {
                assert_eq!(text, "中");
                assert_eq!((*x, *y), (200.0, 300.0));
            }
            other => panic!("expected text, got {other:?}"),
        }
        // 空白文本提交丢弃;Esc 取消编辑不产生图元。
        engine.handle_event(InputEvent::LeftDown { x: 300, y: 400 });
        engine.handle_event(InputEvent::LeftUp { x: 300, y: 400 });
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Enter,
            shift: false,
        });
        assert_eq!(engine.annotations().len(), 1);
        engine.handle_event(InputEvent::Text("x".into()));
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Escape,
            shift: false,
        });
        assert!(engine.text_edit().is_none());
        assert_eq!(engine.annotations().len(), 1);
    }

    #[test]
    fn typing_without_text_edit_never_creates_annotations() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        engine.handle_event(InputEvent::Text("abc".into()));
        engine.handle_event(InputEvent::Composition("ni".into()));
        assert!(engine.annotations().is_empty());
    }

    #[test]
    fn enabled_tool_draws_and_clamps_inside_selection_and_exits_outside() {
        let mut engine = inline_engine(400, 300);
        drag_selection(&mut engine, (40, 30), (300, 250));
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Arrow));
        // 横条在选区外(下方优先);从选区内部下方拖出选区外,端点钳制在选区内。
        engine.handle_event(InputEvent::LeftDown { x: 100, y: 220 });
        engine.handle_event(InputEvent::PointerMove { x: 900, y: 900 });
        engine.handle_event(InputEvent::LeftUp { x: 900, y: 900 });
        match &engine.annotations()[0] {
            Annotation::Arrow { to, .. } => {
                assert_eq!((to.x, to.y), (300.0, 250.0));
            }
            other => panic!("expected arrow, got {other:?}"),
        }
        // 选区外按下:退出工具并重新拖选。
        engine.handle_event(InputEvent::LeftDown { x: 5, y: 5 });
        assert_eq!(engine.tool(), None);
        assert_eq!(
            engine.state(),
            &EngineState::Dragging {
                anchor_x: 5,
                anchor_y: 5
            }
        );
        assert!(engine.selection().is_none());
    }

    #[test]
    fn starting_a_new_selection_resets_annotation_session() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Rect));
        engine.handle_event(InputEvent::LeftDown { x: 200, y: 300 });
        engine.handle_event(InputEvent::PointerMove { x: 400, y: 420 });
        engine.handle_event(InputEvent::LeftUp { x: 400, y: 420 });
        assert_eq!(engine.annotations().len(), 1);
        // 选区外重新拖选:旧图元/撤销栈不落到新选区。
        engine.handle_event(InputEvent::LeftDown { x: 10, y: 10 });
        engine.handle_event(InputEvent::PointerMove { x: 300, y: 200 });
        engine.handle_event(InputEvent::LeftUp { x: 300, y: 200 });
        assert!(engine.annotations().is_empty());
        assert_eq!(engine.tool(), None);
        assert!(!engine.more_open());
        assert!(!engine.undo_annotation());
    }

    #[test]
    fn inline_disabled_keeps_selection_move_and_confirm_paths() {
        let mut engine = SelectionEngine::new(
            320,
            200,
            FeatureFlags {
                inline_annotation: false,
                ..FeatureFlags::default()
            },
        );
        drag(&mut engine, (20, 20), (100, 80));
        engine.handle_event(InputEvent::LeftDown { x: 60, y: 50 });
        assert!(matches!(engine.state(), EngineState::Moving { .. }));
        engine.handle_event(InputEvent::PointerMove { x: 70, y: 55 });
        engine.handle_event(InputEvent::LeftUp { x: 70, y: 55 });
        assert_eq!(engine.state(), &EngineState::Selected);
        assert_eq!(
            engine.selection(),
            Some(PhysicalRect {
                x: 30,
                y: 25,
                width: 81,
                height: 61
            })
        );
        // 工具快捷键在关闭时也不产生图元。
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Tool(AnnotationTool::Rect),
            shift: false,
        });
        assert_eq!(engine.tool(), None);
        engine.handle_event(InputEvent::Key {
            key: LogicalKey::Undo,
            shift: false,
        });
        assert!(engine.annotations().is_empty());
    }

    /// 顶边重叠回归:横条贴屏幕顶缘(近全屏选区)且「更多」面板因屏高不足
    /// 仍与横条重叠时,点击同时落在面板项与横条按钮上的点必须触发面板动作
    /// (面板绘制在横条之上,命中顺序与绘制顺序一致),而不是底下的横条按钮。
    #[test]
    fn more_panel_wins_hit_test_overlapping_toolbar_button() {
        let mut engine = inline_engine(1920, 410);
        // 选区几乎占满屏幕:下缘放不下(候选被钳回屏内仍压选区),
        // 横条按最小重叠翻上缘并被钳到 y=0;屏高不足以把「更多」面板
        // 下移到横条之下,重叠保留,命中顺序必须让可见的面板项获胜。
        drag_selection(&mut engine, (0, 39), (1910, 400));
        let toolbar = engine.unified_toolbar().expect("toolbar");
        assert_eq!(toolbar.panel.y, 0, "横条应被钳到屏幕顶缘");
        open_more_panel(&mut engine);
        let (panel, items) = engine.more_panel().expect("more panel");
        assert!(
            items.iter().any(|(_, rect)| {
                toolbar
                    .buttons
                    .iter()
                    .any(|(_, b)| composer::IntRect::intersect(*rect, *b).width > 0)
            }),
            "本场景应存在面板项与横条按钮的几何重叠: {panel:?}"
        );
        // 找一个同时在面板项与横条按钮内的点。
        let (expected, point) = items
            .iter()
            .find_map(|(action, rect)| {
                toolbar.buttons.iter().find_map(|(_, b)| {
                    let hit = composer::IntRect::intersect(*rect, *b);
                    (!hit.is_empty()).then_some((*action, hit.center()))
                })
            })
            .expect("overlapping panel item");
        assert_eq!(
            engine.cursor_for(point.0, point.1),
            CursorHint::Pointer,
            "重叠区光标应为手型"
        );
        engine.handle_event(InputEvent::PointerMove {
            x: point.0,
            y: point.1,
        });
        engine.handle_event(InputEvent::LeftDown {
            x: point.0,
            y: point.1,
        });
        match engine.state {
            EngineState::PressingChrome { action } => assert_eq!(
                action, expected,
                "重叠区点击必须命中可见的面板项,而非底下的横条按钮"
            ),
            other => panic!("应按下面板项进入 PressingChrome,实际 {other:?}"),
        }
    }

    /// 「更多」面板内点工具:立即选中并关闭面板。
    #[test]
    fn more_panel_tool_selection_closes_panel() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        open_more_panel(&mut engine);
        let outcome = click_action(&mut engine, SelectionAction::Tool(AnnotationTool::Mosaic));
        assert_eq!(outcome, EngineOutcome::Redraw);
        assert_eq!(engine.tool(), Some(AnnotationTool::Mosaic));
        assert!(!engine.more_open(), "选工具后「更多」应立即关闭");
    }

    /// 横条/「更多」内部动作(工具/撤销/重做/删除/更多)永不向会话层泄漏
    /// 终态动作。
    #[test]
    fn toolbar_actions_never_leak_to_shell_outcomes() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        open_more_panel(&mut engine);
        let more_actions: Vec<SelectionAction> = engine
            .more_panel()
            .unwrap()
            .1
            .iter()
            .map(|(a, _)| *a)
            .collect();
        for action in more_actions {
            // 贴图/取字是会话层动作,另有专项测试;这里只验内部动作。
            if matches!(action, SelectionAction::Pin | SelectionAction::Ocr) {
                continue;
            }
            // 内部动作可能收起面板(选工具/再点更多):每次点击前确保展开,
            // 并用新鲜几何定位按钮。
            open_more_panel(&mut engine);
            let (_, items) = engine.more_panel().unwrap();
            let (_, rect) = items
                .into_iter()
                .find(|(candidate, _)| *candidate == action)
                .expect("more item present");
            let (cx, cy) = rect.center();
            assert_eq!(
                engine.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
                EngineOutcome::Redraw
            );
            assert_eq!(
                engine.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
                EngineOutcome::Redraw
            );
        }
        let bar_actions: Vec<SelectionAction> = engine
            .unified_toolbar()
            .unwrap()
            .buttons
            .iter()
            .map(|(a, _)| *a)
            .collect();
        for action in bar_actions {
            // 复制/保存/取消是会话层动作(取消=Cancelled),另有专项测试。
            if matches!(
                action,
                SelectionAction::Copy | SelectionAction::Save | SelectionAction::Cancel
            ) {
                continue;
            }
            let toolbar = engine.unified_toolbar().unwrap();
            let (_, rect) = toolbar
                .buttons
                .into_iter()
                .find(|(candidate, _)| *candidate == action)
                .unwrap();
            let (cx, cy) = rect.center();
            assert_eq!(
                engine.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
                EngineOutcome::Redraw
            );
            assert_eq!(
                engine.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
                EngineOutcome::Redraw
            );
        }
        // 内部动作不改变终态语义:Enter 仍确认选区。
        assert!(matches!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Confirmed(_)
        ));
    }

    /// 「更多」展开时其动作项可命中并产出对应动作(贴图/取字交回壳)。
    #[test]
    fn more_panel_pin_and_ocr_emit_shell_actions() {
        let mut engine = inline_engine(800, 600);
        drag_selection(&mut engine, (40, 30), (760, 560));
        open_more_panel(&mut engine);
        let (_, items) = engine.more_panel().unwrap();
        let (_, rect) = items
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Pin)
            .expect("pin item");
        let (cx, cy) = rect.center();
        engine.handle_event(InputEvent::LeftDown { x: cx, y: cy });
        assert_eq!(
            engine.handle_event(InputEvent::LeftUp { x: cx, y: cy }),
            EngineOutcome::Action(SelectionAction::Pin)
        );
    }
}
