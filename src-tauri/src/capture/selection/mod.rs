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
mod text;

use crate::capture::geometry::PhysicalRect;
use composer::IntRect;

/// 选区最小可截尺寸(物理像素),对齐现 Windows 原生路径的 ≥2px。
pub const MIN_SELECTION_SIZE: u32 = 2;
/// 键盘微调步长:默认 1 物理像素,Shift 为 10。
pub const KEY_STEP: i32 = 1;
pub const KEY_STEP_LARGE: i32 = 10;

/// 功能入口开关(默认全开);操作条/菜单动作集由此决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureFlags {
    pub ocr_entry: bool,
    pub pin_entry: bool,
    pub magnifier: bool,
    pub toolbar_copy: bool,
    pub toolbar_save: bool,
    pub toolbar_pin: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    PointerMove { x: i32, y: i32 },
    LeftDown { x: i32, y: i32 },
    LeftUp { x: i32, y: i32 },
    RightDown { x: i32, y: i32 },
    Key { key: LogicalKey, shift: bool },
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
    /// 拖动选区内部整体移动。
    Moving { grab_x: i32, grab_y: i32 },
    /// 右键菜单打开。
    Menu,
}

/// 合成器输入:由引擎当前状态派生的一帧静态场景。
#[derive(Debug, Clone, Copy)]
pub struct Scene {
    pub selection: Option<PhysicalRect>,
    pub cursor: (i32, i32),
    pub flags: FeatureFlags,
    pub toolbar_visible: bool,
    pub menu_open: bool,
    pub menu_anchor: (i32, i32),
}

#[derive(Debug, Clone, Copy)]
pub struct SelectionEngine {
    width: u32,
    height: u32,
    flags: FeatureFlags,
    state: EngineState,
    selection: Option<PhysicalRect>,
    cursor: (i32, i32),
    menu_anchor: (i32, i32),
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
        }
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

    /// 当前状态对应的合成场景。
    pub fn scene(&self) -> Scene {
        let toolbar_visible = self.state == EngineState::Selected
            && self.selection.is_some()
            && !composer::toolbar_buttons(self.flags).is_empty();
        Scene {
            selection: self.selection,
            cursor: self.cursor,
            flags: self.flags,
            toolbar_visible,
            menu_open: self.state == EngineState::Menu,
            menu_anchor: self.menu_anchor,
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
            EngineState::Moving { grab_x, grab_y } => {
                if let Some(selection) = self.selection {
                    self.selection = Some(Self::translate(
                        selection,
                        self.cursor.0 - grab_x,
                        self.cursor.1 - grab_y,
                        (self.width as i32, self.height as i32),
                    ));
                }
            }
            _ => {}
        }
    }

    fn on_left_down(&mut self) -> EngineOutcome {
        let (x, y) = self.cursor;
        match self.state {
            EngineState::Idle => {
                self.state = EngineState::Dragging {
                    anchor_x: x,
                    anchor_y: y,
                };
                self.selection = None;
                EngineOutcome::Redraw
            }
            EngineState::Dragging { .. } => EngineOutcome::Redraw,
            EngineState::Selected => {
                if let Some(action) = self.hit_toolbar(x, y) {
                    return EngineOutcome::Action(action);
                }
                if let Some(selection) = self.selection {
                    if let Some(handle) = composer::handle_hit(selection, x, y) {
                        self.state = EngineState::Adjusting { handle };
                        return EngineOutcome::Redraw;
                    }
                    let sel = IntRect::from(selection);
                    if sel.contains(x, y) {
                        self.state = EngineState::Moving {
                            grab_x: x,
                            grab_y: y,
                        };
                        return EngineOutcome::Redraw;
                    }
                }
                // 选区外重新拖出新选区。
                self.state = EngineState::Dragging {
                    anchor_x: x,
                    anchor_y: y,
                };
                self.selection = None;
                EngineOutcome::Redraw
            }
            EngineState::Adjusting { .. } | EngineState::Moving { .. } => EngineOutcome::Redraw,
            EngineState::Menu => {
                if let Some(action) = self.hit_menu(x, y) {
                    return if action == SelectionAction::Cancel {
                        EngineOutcome::Cancelled
                    } else {
                        EngineOutcome::Action(action)
                    };
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
            EngineState::Adjusting { .. } | EngineState::Moving { .. } => {
                self.state = EngineState::Selected;
                EngineOutcome::Redraw
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
        match key {
            LogicalKey::Escape => EngineOutcome::Cancelled,
            LogicalKey::Enter => match self.selection {
                Some(rect) => EngineOutcome::Confirmed(rect),
                None => EngineOutcome::Redraw,
            },
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
        let buttons = composer::toolbar_buttons(self.flags);
        let panel = composer::toolbar_panel(self.selection?, self.size(), &buttons)?;
        composer::toolbar_button_rects(panel, &buttons)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(action, _)| action)
    }

    fn hit_menu(&self, x: i32, y: i32) -> Option<SelectionAction> {
        let items = composer::menu_items(self.flags);
        let panel = composer::menu_panel(self.menu_anchor, self.size(), &items);
        composer::menu_item_rects(panel, &items)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(action, _)| action)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_engine() -> SelectionEngine {
        SelectionEngine::new(320, 200, FeatureFlags::default())
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
        // 无选区时 Enter 不产出确认。
        assert_eq!(
            engine.handle_event(InputEvent::Key {
                key: LogicalKey::Enter,
                shift: false
            }),
            EngineOutcome::Redraw
        );
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
        let buttons = composer::toolbar_buttons(flags);
        let panel =
            composer::toolbar_panel(engine.selection().unwrap(), engine.size(), &buttons).unwrap();
        let rects = composer::toolbar_button_rects(panel, &buttons);
        // 选第三个按钮(贴图),避开光标处放大镜面板。
        let (expected, rect) = rects.last().copied().unwrap();
        let (cx, cy) = rect.center();
        assert_eq!(
            engine.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Action(expected)
        );
        // 关闭复制开关后,动作集与命中都不再出现复制(首位变为保存)。
        let off = FeatureFlags {
            toolbar_copy: false,
            ..FeatureFlags::default()
        };
        let mut restricted = SelectionEngine::new(320, 200, off);
        drag(&mut restricted, (40, 30), (200, 120));
        let buttons = composer::toolbar_buttons(off);
        assert!(!buttons.contains(&SelectionAction::Copy));
        assert_eq!(restricted.hit_toolbar(60, 140), Some(SelectionAction::Save));
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
        let panel = composer::menu_panel(engine.menu_anchor(), engine.size(), &items);
        let rects = composer::menu_item_rects(panel, &items);
        // 点击"取字"。
        let ocr = rects
            .iter()
            .find(|(action, _)| *action == SelectionAction::Ocr)
            .copied()
            .unwrap();
        let mut with_menu = engine;
        let (cx, cy) = ocr.1.center();
        assert_eq!(
            with_menu.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
            EngineOutcome::Action(SelectionAction::Ocr)
        );
        // 点击"取消"结束会话。
        let cancel = rects
            .iter()
            .find(|(action, _)| *action == SelectionAction::Cancel)
            .copied()
            .unwrap();
        let mut cancelling = engine;
        let (cx, cy) = cancel.1.center();
        assert_eq!(
            cancelling.handle_event(InputEvent::LeftDown { x: cx, y: cy }),
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
    fn scene_reflects_engine_state_and_flags() {
        let mut engine = new_engine();
        let scene = engine.scene();
        assert!(!scene.toolbar_visible && !scene.menu_open && scene.selection.is_none());
        drag(&mut engine, (40, 30), (200, 120));
        let scene = engine.scene();
        assert!(scene.toolbar_visible);
        assert_eq!(scene.selection, engine.selection());
        engine.handle_event(InputEvent::RightDown { x: 100, y: 60 });
        let scene = engine.scene();
        assert!(scene.menu_open && !scene.toolbar_visible);
        assert_eq!(scene.menu_anchor, (100, 60));
        // 关闭全部操作条开关后场景不再显示操作条。
        let off = FeatureFlags {
            toolbar_copy: false,
            toolbar_save: false,
            toolbar_pin: false,
            ..FeatureFlags::default()
        };
        let mut bare = SelectionEngine::new(320, 200, off);
        drag(&mut bare, (40, 30), (200, 120));
        assert!(!bare.scene().toolbar_visible);
    }
}
