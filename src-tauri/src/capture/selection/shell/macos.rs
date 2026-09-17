//! macOS 区域选区壳:objc2 无边框 NSWindow + CG 位图呈现 + AppKit 事件转发。
//!
//! 交互语义全部由平台无关选区引擎(`capture::selection`)决定;本壳只把
//! 鼠标(左/右/移动/拖拽)与键盘(方向键/Shift/Enter/Esc/C)事件以物理像素
//! 坐标喂给引擎,并把引擎的合成位图 present 到屏幕(ADR-001/006)。右键=菜单、
//! Esc=取消;Enter 确认;操作条/菜单动作经 `RegionOutcome` 交回会话层分发,
//! 与 `shell/windows.rs` 同构(ShellHooks 零 tauri 依赖模式)。
//!
//! 窗口模式对标 Apple screencapture:每屏一个 borderless NSWindow
//! (screen saver window level + CanJoinAllSpaces),激活 App 自身,自定义
//! NSWindow 子类放行 canBecomeKeyWindow 以接收键盘;内容视图为自定义 NSView,
//! `drawRect:` 中经 CGImage 绘制合成帧(引擎输出 RGBA→BGRA,
//! kCGBitmapByteOrder32Little|kCGImageAlphaPremultipliedFirst)。
//!
//! 接线说明:本文件尚未被模块树引用(接线行不在 macos-shell 任务 scope)。
//! 参照 Windows 壳的挂接方式,需要 `capture/native_overlay.rs` 的 cfg 从
//! `#[cfg(windows)]` 放宽为 windows/macos 双平台,或新增
//! `#[cfg(target_os = "macos")] #[path = "selection/shell/macos.rs"] mod imp;`
//! 分发;`session.rs` 的 `capture_region` 需将 macOS 分支切到
//! `pick_region`(Windows 实现为同文件同构范本)。
//!
//! 注意:本模块只能在 macOS 编译;Windows/Linux 主机上的离线核对以
//! windows.rs 逐块对照为准(事件映射、坐标换算、Outcome 处理)。

use std::cell::RefCell;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSCursor, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSGraphicsContext, NSResponder, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize, CGFloat};
use objc2_core_graphics::{
    kCGScreenSaverWindowLevel, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext,
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize};

use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{MonitorGeom, PhysicalRect};
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    EngineOutcome, FeatureFlags, InputEvent, LogicalKey, SelectionAction, SelectionEngine,
};
use crate::capture::session::QuietAction;

// macOS Carbon 键码(HIToolbox/Events.h,kVK_*;与键盘布局无关的物理键位)。
const KC_ANSI_C: u16 = 0x08;
const KC_RETURN: u16 = 0x24;
const KC_ESCAPE: u16 = 0x35;
const KC_LEFT: u16 = 0x7B;
const KC_RIGHT: u16 = 0x7C;
const KC_DOWN: u16 = 0x7D;
const KC_UP: u16 = 0x7E;

/// 壳的最终结果:会话层据此选择完成路径(与 Windows 壳同构)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionOutcome {
    /// Enter 或「标注」动作:rect 走 Preview 完成路径(剪贴板+预览)。
    Preview(PhysicalRect),
    /// 操作条/菜单的 copy/save/pin/ocr 动作:rect 走 Quiet 完成路径并执行动作。
    Quiet(PhysicalRect, QuietAction),
    /// Esc 或菜单「取消」:整个会话取消。
    Cancelled,
}

/// 引擎 + 合成/呈现缓冲。独立成结构使呈现路径可脱离 AppHandle 测量。
struct Canvas {
    engine: SelectionEngine,
    composer: Composer,
    /// 引擎输出的 RGBA 合成帧(长度与冻结帧一致)。
    scratch: Vec<u8>,
    /// 呈现用 BGRA 缓冲:CGImage 以
    /// kCGImageAlphaPremultipliedFirst|kCGBitmapByteOrder32Little 直读 BGRA。
    present_buf: Vec<u8>,
    width: i32,
    height: i32,
}

/// 壳侧回调:色值复制等副作用由会话层注入,壳不直接触碰 tauri 运行时
/// (与 Windows 壳同因:避免把 dialog 模块链入测试二进制)。
#[derive(Debug, Clone, Copy)]
pub struct ShellHooks {
    /// 复制色值文本并给出反馈;参数为完整文本与 HEX 简写。
    pub copy_color: fn(text: &str, hex: &str),
}

struct ShellState {
    hooks: ShellHooks,
    canvas: Canvas,
    /// 逻辑点→物理像素换算系数(取自捕获时的屏幕 backingScale)。
    scale: f64,
    /// 最近一次 Redraw 合成的 CGImage;provider 不持有数据,
    /// 由 `canvas.present_buf`(定容,地址不漂移)保活。
    image: Option<CFRetained<CGImage>>,
    /// 有待 flush 的合成帧(STATE 借用释放后由 view.display 消费)。
    dirty: bool,
    outcome: Option<RegionOutcome>,
    timing: bool,
}

thread_local! {
    static STATE: RefCell<Option<ShellState>> = const { RefCell::new(None) };
}

// 自定义内容视图:接收鼠标/键盘并转发引擎;drawRect 绘制缓存位图。
define_class!(
    // SAFETY: superclass 是 NSView;仅覆盖事件转发与绘制,无额外契约。
    #[unsafe(super(NSView))]
    #[name = "CropmarkSelectionView"]
    #[thread_kind = MainThreadOnly]
    struct SelectionView;

    impl SelectionView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            forward_mouse(self, event, MouseInput::LeftDown);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            forward_mouse(self, event, MouseInput::LeftUp);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            forward_mouse(self, event, MouseInput::Move);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            forward_mouse(self, event, MouseInput::Move);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            forward_mouse(self, event, MouseInput::RightDown);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if let Some(key) = map_key_code(event.keyCode()) {
                let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
                dispatch_input(self, InputEvent::Key { key, shift });
            }
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            draw_cached_image(self);
        }
    }
);

// 自定义窗口:borderless NSWindow 默认不能成为 key window,
// 覆盖 canBecomeKeyWindow/main 才能把键盘路由给内容视图。
define_class!(
    // SAFETY: superclass 是 NSWindow;只放行 key/main 资格,无额外契约。
    #[unsafe(super(NSWindow))]
    #[name = "CropmarkSelectionKeyWindow"]
    #[thread_kind = MainThreadOnly]
    struct KeyWindow;

    impl KeyWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

/// libdispatch 主队列:AppKit 对象只能活在主线程,Windows 壳在阻塞线程
/// 泵消息的调用形态在这里经 dispatch_sync_f 整体切到主线程执行。
#[repr(C)]
struct DispatchQueueOpaque {
    _private: [u8; 0],
}

#[link(name = "System", kind = "dylib")]
extern "C" {
    static _dispatch_main_q: DispatchQueueOpaque;
    fn dispatch_sync_f(
        queue: *mut DispatchQueueOpaque,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// 派发到主线程所需的最小屏幕几何(Copy,供 'static 闭包携带)。
#[derive(Debug, Clone, Copy)]
struct ShellGeometry {
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
    scale: f64,
}

/// 驱动一次区域选区:在指针所在屏创建无边框置顶窗并泵事件直到终态。
/// 可在任意线程调用;非主线程时整体派发到主线程同步执行。
pub fn pick_region(
    frame: &Frame,
    monitor: &MonitorGeom,
    flags: FeatureFlags,
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    let composer = Composer::new(frame)?;
    let (frame_w, frame_h) = composer.size();
    let width = frame_w.min(monitor.physical_width).max(1) as i32;
    let height = frame_h.min(monitor.physical_height).max(1) as i32;
    let bytes = frame.rgba.len();
    let canvas = Canvas {
        engine: SelectionEngine::new(width as u32, height as u32, flags),
        composer,
        scratch: vec![0; bytes],
        present_buf: vec![0; bytes],
        width,
        height,
    };
    let geometry = ShellGeometry {
        logical_x: monitor.logical_x,
        logical_y: monitor.logical_y,
        logical_width: monitor.logical_width,
        logical_height: monitor.logical_height,
        scale: if monitor.scale.is_finite() && monitor.scale > 0.0 {
            monitor.scale
        } else {
            1.0
        },
    };
    if let Some(mtm) = MainThreadMarker::new() {
        run_shell(mtm, canvas, geometry, hooks)
    } else {
        run_on_main_thread(move || {
            let mtm = MainThreadMarker::new().expect("dispatch_sync_f 已切到主线程");
            run_shell(mtm, canvas, geometry, hooks)
        })
    }
}

/// 经 libdispatch 主队列同步执行;调用方已在主线程时由 pick_region 直接短路。
fn run_on_main_thread(
    run: impl FnOnce() -> Result<RegionOutcome, CaptureError> + Send + 'static,
) -> Result<RegionOutcome, CaptureError> {
    struct MainThreadJob {
        run: Option<Box<dyn FnOnce() -> Result<RegionOutcome, CaptureError> + Send>>,
        result: Option<Result<RegionOutcome, CaptureError>>,
    }
    extern "C" fn main_thread_entry(context: *mut c_void) {
        let mut job = unsafe { Box::from_raw(context as *mut MainThreadJob) };
        if let Some(run) = job.run.take() {
            job.result = Some(run());
        }
    }
    let mut job = MainThreadJob {
        run: Some(Box::new(run)),
        result: None,
    };
    unsafe {
        dispatch_sync_f(
            std::ptr::addr_of!(_dispatch_main_q) as *mut DispatchQueueOpaque,
            std::ptr::addr_of_mut!(job).cast::<c_void>(),
            main_thread_entry,
        );
    }
    job.result
        .unwrap_or_else(|| Err(CaptureError::api("选区窗无法在主线程运行。")))
}

fn run_shell(
    mtm: MainThreadMarker,
    canvas: Canvas,
    geometry: ShellGeometry,
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(ShellState {
            hooks,
            canvas,
            scale: geometry.scale,
            image: None,
            dirty: false,
            outcome: None,
            timing: std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some(),
        });
    });
    let app = NSApplication::sharedApplication(mtm);
    // AppKit 推荐 API(macOS 14+ 取代 deprecated 的 activateIgnoringOtherApps:)。
    app.activate();
    let frame = screen_frame_for(mtm, &geometry);
    let view = unsafe { create_selection_view(mtm, frame.size) };
    let window = unsafe { create_key_window(mtm, frame)? };
    window.setLevel(kCGScreenSaverWindowLevel as isize);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            .union(NSWindowCollectionBehavior::FullScreenAuxiliary),
    );
    window.setAcceptsMouseMovedEvents(true);
    window.setOpaque(true);
    window.setHasShadow(false);
    window.setHidesOnDeactivate(false);
    let content_view: &NSView = &view;
    window.setContentView(Some(content_view));
    NSCursor::crosshairCursor().set();
    // 先合成首帧再上屏,避免 orderFront 到首次 drawRect 之间闪黑。
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            present(state, &view);
        }
    });
    window.makeKeyAndOrderFront(None);
    let responder: &NSResponder = &view;
    window.makeFirstResponder(Some(responder));
    view.display();
    pump_until_done(&app);
    window.orderOut(None);
    NSCursor::arrowCursor().set();
    let state = STATE.with(|slot| slot.borrow_mut().take());
    Ok(state
        .and_then(|state| state.outcome)
        .unwrap_or(RegionOutcome::Cancelled))
}

/// 按捕获时的逻辑几何匹配 NSScreen;不一致时按 monitor 逻辑值直接构造
/// (AppKit 全局坐标与 SCK 层报告的 MinX/MinY 同基)。
fn screen_frame_for(mtm: MainThreadMarker, geometry: &ShellGeometry) -> NSRect {
    let fallback = NSRect {
        origin: NSPoint {
            x: geometry.logical_x as f64,
            y: geometry.logical_y as f64,
        },
        size: NSSize {
            width: geometry.logical_width as f64,
            height: geometry.logical_height as f64,
        },
    };
    for screen in NSScreen::screens(mtm).to_vec() {
        let frame = screen.frame();
        let matched = frame.origin.x.round() as i32 == geometry.logical_x
            && frame.origin.y.round() as i32 == geometry.logical_y
            && frame.size.width.round() as u32 == geometry.logical_width
            && frame.size.height.round() as u32 == geometry.logical_height;
        if matched {
            return frame;
        }
    }
    fallback
}

unsafe fn create_selection_view(mtm: MainThreadMarker, size: NSSize) -> Retained<SelectionView> {
    let allocated = mtm.alloc::<SelectionView>().set_ivars(());
    let frame = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size,
    };
    msg_send![super(allocated), initWithFrame: frame]
}

unsafe fn create_key_window(
    mtm: MainThreadMarker,
    content_rect: NSRect,
) -> Result<Retained<KeyWindow>, CaptureError> {
    let allocated = mtm.alloc::<KeyWindow>().set_ivars(());
    let window: Retained<KeyWindow> = msg_send![super(allocated),
        initWithContentRect: content_rect,
        styleMask: NSWindowStyleMask::Borderless,
        backing: NSBackingStoreType::Buffered,
        defer: false];
    Ok(window)
}

/// 手动泵事件直到引擎给出终态;阻塞等待期间 run loop 会顺带服务
/// 窗口刷新(display/drawRect)与其它来源。
fn pump_until_done(app: &NSApplication) {
    loop {
        let done = STATE.with(|slot| {
            slot.borrow()
                .as_ref()
                .is_some_and(|state| state.outcome.is_some())
        });
        if done {
            return;
        }
        let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::distantFuture()),
            // NSDefaultRunLoopMode 是 extern block static,读取需 unsafe。
            unsafe { NSDefaultRunLoopMode },
            true,
        );
        if let Some(event) = event {
            app.sendEvent(&event);
            app.updateWindows();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MouseInput {
    Move,
    LeftDown,
    LeftUp,
    RightDown,
}

fn forward_mouse(view: &SelectionView, event: &NSEvent, kind: MouseInput) {
    let (x, y) = event_point(view, event);
    let input = match kind {
        MouseInput::Move => InputEvent::PointerMove { x, y },
        MouseInput::LeftDown => InputEvent::LeftDown { x, y },
        MouseInput::LeftUp => InputEvent::LeftUp { x, y },
        MouseInput::RightDown => InputEvent::RightDown { x, y },
    };
    dispatch_input(view, input);
}

/// 事件物理像素坐标:窗口坐标(左下原点,逻辑点)→视图坐标(同为左下原点)
/// →物理像素(引擎坐标系,左上原点),换算系数取捕获时屏幕 backingScale。
fn event_point(view: &SelectionView, event: &NSEvent) -> (i32, i32) {
    let (scale, logical_height) = STATE
        .with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|state| (state.scale, state.canvas.height as f64 / state.scale))
        })
        .unwrap_or((1.0, 0.0));
    let location = event.locationInWindow();
    let in_view = view.convertPoint_fromView(location, None);
    let x = (in_view.x * scale).round() as i32;
    let y = ((logical_height - in_view.y) * scale).round() as i32;
    (x, y)
}

/// 把一次输入事件交给引擎并处理其输出;终态由 pump 读取 STATE 判定。
/// drawRect 可能经 view.display 重入,因此 STATE 借用在 present 前释放。
fn dispatch_input(view: &SelectionView, event: InputEvent) {
    let dirty = STATE.with(|slot| {
        let mut guard = slot.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return false;
        };
        feed_event(state, event, view);
        state.dirty
    });
    if dirty {
        view.setNeedsDisplay(true);
        view.display();
        STATE.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                state.dirty = false;
            }
        });
    }
}

/// 与 Windows 壳同构的 EngineOutcome 处理:Redraw→重呈现、
/// Confirmed→Preview、Cancelled→取消、Action→会话侧完成或复制色值。
fn feed_event(state: &mut ShellState, event: InputEvent, view: &SelectionView) {
    let outcome = state.canvas.engine.handle_event(event);
    match outcome {
        EngineOutcome::Redraw => {
            present(state, view);
        }
        EngineOutcome::Confirmed(rect) => {
            state.outcome = Some(RegionOutcome::Preview(rect));
        }
        EngineOutcome::Cancelled => {
            state.outcome = Some(RegionOutcome::Cancelled);
        }
        EngineOutcome::Action(action) => match action {
            SelectionAction::Annotate => {
                if let Some(rect) = state.canvas.engine.selection() {
                    state.outcome = Some(RegionOutcome::Preview(rect));
                }
            }
            SelectionAction::Cancel => {
                // 引擎在菜单路径已把「取消」译为 Cancelled;此支仅为防御。
                state.outcome = Some(RegionOutcome::Cancelled);
            }
            SelectionAction::CopyColor => {
                copy_color_value(state);
            }
            quiet => {
                if let (Some(rect), Some(action)) =
                    (state.canvas.engine.selection(), quiet_action_for(quiet))
                {
                    state.outcome = Some(RegionOutcome::Quiet(rect, action));
                }
            }
        },
    }
}

/// 操作条/菜单动作到静默完成动作的映射;标注/取消/复制色值不在此列。
fn quiet_action_for(action: SelectionAction) -> Option<QuietAction> {
    match action {
        SelectionAction::Copy => Some(QuietAction::Copy),
        SelectionAction::Save => Some(QuietAction::Save),
        SelectionAction::Pin => Some(QuietAction::Pin),
        SelectionAction::Ocr => Some(QuietAction::Ocr),
        SelectionAction::Annotate | SelectionAction::Cancel | SelectionAction::CopyColor => None,
    }
}

/// C 键取色:按引擎光标从冻结帧采样,经回调复制 HEX+RGB 文本并反馈。
fn copy_color_value(state: &mut ShellState) {
    let cursor = state.canvas.engine.cursor();
    let pixel = state.canvas.composer.sample(cursor.0, cursor.1);
    let hex = composer::hex_readout(pixel);
    let text = format!("{} {}", hex, composer::rgb_readout(pixel));
    (state.hooks.copy_color)(&text, &hex);
}

fn map_key_code(code: u16) -> Option<LogicalKey> {
    match code {
        KC_RETURN => Some(LogicalKey::Enter),
        KC_ESCAPE => Some(LogicalKey::Escape),
        KC_LEFT => Some(LogicalKey::ArrowLeft),
        KC_UP => Some(LogicalKey::ArrowUp),
        KC_RIGHT => Some(LogicalKey::ArrowRight),
        KC_DOWN => Some(LogicalKey::ArrowDown),
        KC_ANSI_C => Some(LogicalKey::CopyColor),
        _ => None,
    }
}

/// 单次合成耗时(仅 Redraw 路径调用;ADR-007 护栏的可观察基线)。
fn compose_canvas(canvas: &mut Canvas) -> Option<Duration> {
    let started = Instant::now();
    let (w, h) = canvas.composer.size();
    let expected = w as usize * h as usize * 4;
    // 壳侧防御:compose_into 要求 out 长度与冻结帧严格一致,越界会 panic;
    // 长度不符时跳过本次合成而非崩溃。
    if canvas.scratch.len() == expected && canvas.present_buf.len() == expected {
        let scene = canvas.engine.scene();
        canvas.composer.compose_into(&scene, &mut canvas.scratch);
        swizzle_rgba_to_bgra(&canvas.scratch, &mut canvas.present_buf);
        return Some(started.elapsed());
    }
    None
}

/// RGBA→BGRA 通道交换,输出到呈现缓冲。
fn swizzle_rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 255;
    }
}

/// 仅在引擎要求 Redraw 时重合成;drawRect 直接绘制缓存 CGImage(ADR-007)。
fn present(state: &mut ShellState, view: &SelectionView) {
    let started = if state.timing {
        Some(Instant::now())
    } else {
        None
    };
    let compose_at = compose_canvas(&mut state.canvas);
    if compose_at.is_some() {
        rebuild_image(state);
        view.setNeedsDisplay(true);
        state.dirty = true;
    }
    if let Some(started) = started {
        eprintln!(
            "Cropmark overlay {}x{} present: compose+swizzle={:?}, total={:?}",
            state.canvas.width,
            state.canvas.height,
            compose_at,
            started.elapsed()
        );
    }
}

/// 从 BGRA 呈现缓冲构建 CGImage。provider 的 release 回调为空,数据由
/// `canvas.present_buf` 保活:该缓冲定容(容量恒等于冻结帧),地址不漂移;
/// 旧 image 在缓冲被下一次 Redraw 覆写前即被替换丢弃。
fn rebuild_image(state: &mut ShellState) {
    let w = state.canvas.width as usize;
    let h = state.canvas.height as usize;
    let buffer = &state.canvas.present_buf;
    let Some(provider) = (unsafe {
        CGDataProvider::with_data(
            std::ptr::null_mut(),
            buffer.as_ptr().cast::<c_void>(),
            buffer.len(),
            None,
        )
    }) else {
        return;
    };
    let Some(space) = CGColorSpace::new_device_rgb() else {
        return;
    };
    let bitmap_info = CGBitmapInfo(
        CGImageAlphaInfo::PremultipliedFirst.0 | CGImageByteOrderInfo::Order32Little.0,
    );
    state.image = unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&space),
            bitmap_info,
            Some(&provider),
            std::ptr::null::<CGFloat>(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    };
}

/// drawRect:绘制缓存 CGImage(非翻转视图,CG 左下原点与视图一致,
/// 引擎帧第 0 行对应屏幕顶部,绘制结果正向)。
fn draw_cached_image(view: &SelectionView) {
    STATE.with(|slot| {
        let guard = slot.borrow();
        let Some(state) = guard.as_ref() else {
            return;
        };
        let Some(image) = state.image.as_ref() else {
            return;
        };
        let Some(context) = NSGraphicsContext::currentContext() else {
            return;
        };
        // graphicsPort 在 10.13 SDK 后被 Apple 标记 deprecated,但 objc2
        // 绑定未提供替代入口(drawRect 内取当前 CGContext 的既定通道)。
        #[allow(deprecated)]
        let port = context.graphicsPort();
        let cg_context = unsafe { &*(port.as_ptr() as *const CGContext) };
        let bounds = view.bounds();
        let rect = CGRect {
            origin: CGPoint {
                x: bounds.origin.x,
                y: bounds.origin.y,
            },
            size: CGSize {
                width: bounds.size.width,
                height: bounds.size.height,
            },
        };
        CGContext::draw_image(Some(cg_context), rect, Some(image));
    });
}

// 事件类型到壳内分发的兜底对照(供人工核对;NSEventType 常量即 AppKit 值)。
const _: () = {
    assert!(NSEventType::LeftMouseDown.0 == 1);
    assert!(NSEventType::LeftMouseUp.0 == 2);
    assert!(NSEventType::RightMouseDown.0 == 3);
    assert!(NSEventType::MouseMoved.0 == 5);
    assert!(NSEventType::LeftMouseDragged.0 == 6);
    assert!(NSEventType::KeyDown.0 == 10);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};

    #[test]
    fn key_codes_map_to_engine_logical_keys() {
        assert_eq!(map_key_code(KC_RETURN), Some(LogicalKey::Enter));
        assert_eq!(map_key_code(KC_ESCAPE), Some(LogicalKey::Escape));
        assert_eq!(map_key_code(KC_LEFT), Some(LogicalKey::ArrowLeft));
        assert_eq!(map_key_code(KC_UP), Some(LogicalKey::ArrowUp));
        assert_eq!(map_key_code(KC_RIGHT), Some(LogicalKey::ArrowRight));
        assert_eq!(map_key_code(KC_DOWN), Some(LogicalKey::ArrowDown));
        assert_eq!(map_key_code(KC_ANSI_C), Some(LogicalKey::CopyColor));
        assert_eq!(map_key_code(0x0A), None);
    }

    #[test]
    fn quiet_actions_map_and_exclude_annotate_cancel_color() {
        assert_eq!(
            quiet_action_for(SelectionAction::Copy),
            Some(QuietAction::Copy)
        );
        assert_eq!(
            quiet_action_for(SelectionAction::Save),
            Some(QuietAction::Save)
        );
        assert_eq!(
            quiet_action_for(SelectionAction::Pin),
            Some(QuietAction::Pin)
        );
        assert_eq!(
            quiet_action_for(SelectionAction::Ocr),
            Some(QuietAction::Ocr)
        );
        assert_eq!(quiet_action_for(SelectionAction::Annotate), None);
        assert_eq!(quiet_action_for(SelectionAction::Cancel), None);
        assert_eq!(quiet_action_for(SelectionAction::CopyColor), None);
    }

    #[test]
    fn swizzle_swaps_rgb_channels_only() {
        let src = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = vec![0u8; 8];
        swizzle_rgba_to_bgra(&src, &mut dst);
        assert_eq!(dst, vec![3, 2, 1, 255, 7, 6, 5, 255]);
        // 长度不齐时,剩余尾部不参与交换。
        let src = [1, 2, 3, 4, 9];
        let mut dst = vec![0u8; 5];
        swizzle_rgba_to_bgra(&src, &mut dst);
        assert_eq!(dst, vec![3, 2, 1, 255, 0]);
    }

    #[test]
    fn color_text_is_hex_plus_rgb() {
        let pixel = [0x2D, 0xD4, 0xBF, 255];
        let text = format!(
            "{} {}",
            composer::hex_readout(pixel),
            composer::rgb_readout(pixel)
        );
        assert_eq!(text, "#2DD4BF R 45 G 212 B 191");
    }

    #[test]
    fn compose_skips_on_mismatched_buffer_lengths() {
        let frame = accept_buffer(RawBuffer::ready(4, 4, vec![9u8; 64])).unwrap();
        let composer = Composer::new(&frame).unwrap();
        let mut canvas = Canvas {
            engine: SelectionEngine::new(4, 4, FeatureFlags::default()),
            composer,
            scratch: vec![0; 64],
            present_buf: vec![0; 8], // 故意错误长度:防御路径返回 None 且不 panic。
            width: 4,
            height: 4,
        };
        assert!(compose_canvas(&mut canvas).is_none());
    }
}
