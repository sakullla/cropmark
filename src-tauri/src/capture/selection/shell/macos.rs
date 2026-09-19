//! macOS 区域选区壳:objc2 无边框 NSPanel + CG 位图呈现 + AppKit 事件转发。
//!
//! 交互语义全部由平台无关选区引擎(`capture::selection`)决定;本壳只把
//! 鼠标(左/右/移动/拖拽)与键盘(方向键/Shift/Enter/Esc/C)事件以物理像素
//! 坐标喂给引擎,并把引擎的合成位图 present 到屏幕(ADR-001/006)。右键=菜单、
//! Esc=取消;Enter 确认;操作条/菜单动作经 `RegionOutcome` 交回会话层分发,
//! 与 `shell/windows.rs` 同构(ShellHooks 零 tauri 依赖模式)。
//!
//! 窗口模式对标 Apple screencapture:每屏一个 borderless NSPanel
//! (NonactivatingPanel + screen saver window level + CanJoinAllSpaces).
//! Cropmark 是 Accessory 托盘应用;macOS 14+ 的 `NSApp.activate()` 不会抢焦点,
//! `activateIgnoringOtherApps:` 也已失效,所以必须用 NonactivatingPanel 才能
//! 在不激活应用的情况下收下鼠标/键盘.自定义 NSPanel 子类放行 canBecomeKeyWindow,
//! 并绕过 constrainFrameRect(否则 AppKit 会把全屏框压到菜单栏下方).
//! 内容视图为自定义 NSView,`drawRect:` 中经 CGImage 绘制合成帧(引擎输出 RGBA→BGRA,
//! kCGBitmapByteOrder32Little|kCGImageAlphaPremultipliedFirst).
//! 鼠标坐标用 `NSEvent.mouseLocation`(AppKit 左下原点)换到引擎左上原点;事件泵
//! 像 Windows 壳的窗口过程一样直接转发,不把输入只交给 NSView 响应链.
//! 光标提示按引擎 `cursor_for` 映射:选区内部→开手(拖移中闭合手)、手柄/边→
//! resize(SF Symbol 自绘)、chrome→箭头、空白→十字;符号不可用时退回十字,
//! 光标切换不阻断选择/确认/取消(ADR-5).
//!
//! 注意:本模块只能在 macOS 编译;Windows/Linux 主机上的离线核对以
//! windows.rs 逐块对照 + `geometry::appkit_global_to_physical` 测试为准.

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSCursor, NSEvent, NSEventMask, NSEventModifierFlags,
    NSEventType, NSGraphicsContext, NSImage, NSPanel, NSResponder, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize, CGFloat};
use objc2_core_graphics::{
    kCGScreenSaverWindowLevel, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext,
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoopCommonModes, NSSize, NSString};

use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{self, MonitorGeom, PhysicalRect};
use crate::annotate::Annotation;
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    AnnotationOptions, CursorHint, EngineOutcome, EngineState, FeatureFlags, InputEvent, LogicalKey,
    SelectionAction, SelectionEngine,
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
/// R21 起携带即时标注图元;macOS 文本输入由 posix 任务接入前,
/// `AnnotationOptions::text_input` 为假,工具条不含文字工具。
#[derive(Debug, Clone, PartialEq)]
pub enum RegionOutcome {
    /// Enter 确认:rect 走普通完成路径(按 finishAction 预览或静默)。
    Preview(PhysicalRect, Vec<Annotation>),
    /// 操作条/菜单的「标注」动作:rect 强制走预览编辑器,不受静默完成配置影响。
    Annotate(PhysicalRect, Vec<Annotation>),
    /// 操作条/菜单的 copy/save/pin/ocr 动作:rect 走 Quiet 完成路径并执行动作。
    Quiet(PhysicalRect, QuietAction, Vec<Annotation>),
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
    /// 选区窗 AppKit 框(左下原点),供 `NSEvent.mouseLocation` 换算兜底.
    frame_x: f64,
    frame_y: f64,
    frame_h: f64,
    /// 最近一次 Redraw 合成的 CGImage;provider 不持有数据,
    /// 由 `canvas.present_buf`(定容,地址不漂移)保活。
    image: Option<CFRetained<CGImage>>,
    /// 有待 flush 的合成帧(STATE 借用释放后由 view.display 消费)。
    dirty: bool,
    outcome: Option<RegionOutcome>,
    timing: bool,
    /// 最近一次应用的壳内光标形态;未变化时跳过 set。
    applied_cursor: Option<CursorKind>,
    /// resize 自绘光标缓存(懒构建;符号不可用时保持 None,显示时退回十字)。
    resize_cursors: ResizeCursors,
    /// 本次壳启动时的关闭代际基线;泵检测到代际变化即取消(ADR-16/17)。
    close_baseline: u64,
    /// 派发方主线程进入超时后置位:迟到的壳在泵内自行退出,不悬挂在屏幕上。
    abandoned: Arc<AtomicBool>,
}

/// 壳内光标形态:引擎提示在上层细化(移动提示拖移中变为闭合手)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorKind {
    Crosshair,
    OpenHand,
    ClosedHand,
    Arrow,
    ResizeNS,
    ResizeEW,
    ResizeNWSE,
    ResizeNESW,
}

/// 引擎提示 + 引擎状态 → 壳内光标形态;仅 Move 需要状态(拖移中闭合手)。
fn cursor_kind(hint: CursorHint, state: &EngineState) -> CursorKind {
    match hint {
        CursorHint::Crosshair => CursorKind::Crosshair,
        CursorHint::Move => {
            if matches!(state, EngineState::Moving { .. }) {
                CursorKind::ClosedHand
            } else {
                CursorKind::OpenHand
            }
        }
        CursorHint::Pointer | CursorHint::Arrow => CursorKind::Arrow,
        CursorHint::ResizeNS => CursorKind::ResizeNS,
        CursorHint::ResizeEW => CursorKind::ResizeEW,
        CursorHint::ResizeNWSE => CursorKind::ResizeNWSE,
        CursorHint::ResizeNESW => CursorKind::ResizeNESW,
    }
}

/// resize 提示 → SF Symbol 名(ADR-5;macOS 11+,部署下限 14);非 resize
/// 形态返回 None(使用系统光标)。符号不可用时由调用方退回十字。
fn resize_symbol(kind: CursorKind) -> Option<&'static str> {
    match kind {
        CursorKind::ResizeNS => Some("arrow.up.and.down"),
        CursorKind::ResizeEW => Some("arrow.left.and.right"),
        CursorKind::ResizeNWSE => Some("arrow.up.left.and.arrow.down.right"),
        CursorKind::ResizeNESW => Some("arrow.up.right.and.arrow.down.left"),
        _ => None,
    }
}

/// SF Symbol 自绘 resize 光标:固定 18pt,热点取图像中心;符号不可用返回 None。
fn build_resize_cursor(kind: CursorKind) -> Option<Retained<NSCursor>> {
    let name = NSString::from_str(resize_symbol(kind)?);
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(&name, None)?;
    let size = NSSize {
        width: 18.0,
        height: 18.0,
    };
    image.setSize(size);
    let hot_spot = NSPoint {
        x: size.width / 2.0,
        y: size.height / 2.0,
    };
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &image,
        hot_spot,
    ))
}

/// resize 自绘光标缓存;首次使用时构建,构建失败保持 None。
#[derive(Default)]
struct ResizeCursors {
    ns: Option<Retained<NSCursor>>,
    ew: Option<Retained<NSCursor>>,
    nwse: Option<Retained<NSCursor>>,
    nesw: Option<Retained<NSCursor>>,
}

impl ResizeCursors {
    fn get(&mut self, kind: CursorKind) -> Option<Retained<NSCursor>> {
        let slot = match kind {
            CursorKind::ResizeNS => &mut self.ns,
            CursorKind::ResizeEW => &mut self.ew,
            CursorKind::ResizeNWSE => &mut self.nwse,
            CursorKind::ResizeNESW => &mut self.nesw,
            _ => return None,
        };
        if slot.is_none() {
            *slot = build_resize_cursor(kind);
        }
        slot.clone()
    }
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

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
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

// 自定义面板:borderless NSWindow 默认不能成为 key window;NonactivatingPanel
// 让 Accessory 应用在不抢前台的情况下仍能收鼠标/键盘(macOS 14+ 无法 steal focus).
define_class!(
    // SAFETY: superclass 是 NSPanel;放行 key/main,并禁止 AppKit 把全屏框钳到菜单栏下.
    #[unsafe(super(NSPanel))]
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

        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain_frame_rect_to_screen(
            &self,
            frame_rect: NSRect,
            _screen: Option<&NSScreen>,
        ) -> NSRect {
            frame_rect
        }
    }
);

/// libdispatch 主队列:AppKit 对象只能活在主线程,阻塞线程上的调用形态在这里
/// 经主队列派发整体切到主线程执行(有界等待,ADR-17)。
#[repr(C)]
struct DispatchQueueOpaque {
    _private: [u8; 0],
}

#[link(name = "System", kind = "dylib")]
extern "C" {
    static _dispatch_main_q: DispatchQueueOpaque;
    fn dispatch_async_f(
        queue: *mut DispatchQueueOpaque,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// 主线程进入上限(ADR-17):主队列长时间不执行派发任务时返回明确错误,
/// 由会话层进入错误窗并可重试,而不是永久挂起。
const MAIN_THREAD_ENTRY_TIMEOUT: Duration = Duration::from_secs(10);

/// 当前壳的关闭代际(stale 重置/超时兜底):从任意线程递增,泵在下一轮迭代取消。
/// 用代际而不是布尔标志:旧壳的收尾不会清除新壳的待关闭状态(评审 P3)。
static SHELL_CLOSE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 请求关闭当前选区壳(线程安全,可从任意线程调用):递增关闭代际并经
/// 主队列唤醒 AppKit 事件泵;泵退出后旧结果由会话层代际校验丢弃(ADR-16)。
pub fn request_shell_close() {
    SHELL_CLOSE_EPOCH.fetch_add(1, Ordering::SeqCst);
    // 主队列回调只用于唤醒 run loop(泵会顺带服务主队列),不触碰 AppKit 状态。
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q) as *mut DispatchQueueOpaque,
            std::ptr::null_mut(),
            wake_main_thread,
        );
    }
}

/// 主队列唤醒回调:内容由泵的下一轮迭代读取,这里只负责让 run loop 醒一次。
extern "C" fn wake_main_thread(_context: *mut c_void) {}

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
/// 可在任意线程调用;非主线程时整体派发到主线程执行,进入主线程有界超时。
pub fn pick_region(
    frame: &Frame,
    monitor: &MonitorGeom,
    flags: FeatureFlags,
    annotation_options: AnnotationOptions,
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    let composer = Composer::new(frame)?;
    let (frame_w, frame_h) = composer.size();
    let width = frame_w.min(monitor.physical_width).max(1) as i32;
    let height = frame_h.min(monitor.physical_height).max(1) as i32;
    let bytes = frame.rgba.len();
    let canvas = Canvas {
        engine: SelectionEngine::new(width as u32, height as u32, flags)
            .with_scale(frame.scale)
            .with_annotation_options(annotation_options),
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
        run_shell(
            mtm,
            canvas,
            geometry,
            hooks,
            Arc::new(AtomicBool::new(false)),
        )
    } else {
        run_on_main_thread(canvas, geometry, hooks)
    }
}

fn timing_enabled() -> bool {
    std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some()
}

/// 主线程任务:画布/几何/回调经主队列移交,结果经 mpsc 回传,Box 由入口释放。
struct MainThreadJob {
    canvas: Option<Canvas>,
    geometry: ShellGeometry,
    hooks: ShellHooks,
    abandoned: Arc<AtomicBool>,
    started: Sender<()>,
    done: Sender<Result<RegionOutcome, CaptureError>>,
}

/// 经主队列异步派发并有界等待"已进入主线程"(ADR-17);壳的真正执行时间由
/// 用户交互决定,只有进入这一跳受 `MAIN_THREAD_ENTRY_TIMEOUT` 限制。
/// 超时返回明确错误;迟到的任务据 `abandoned` 跳过,已开始的壳由泵自行退出。
fn run_on_main_thread(
    canvas: Canvas,
    geometry: ShellGeometry,
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    let abandoned = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<Result<RegionOutcome, CaptureError>>();
    let job = Box::new(MainThreadJob {
        canvas: Some(canvas),
        geometry,
        hooks,
        abandoned: abandoned.clone(),
        started: started_tx,
        done: done_tx,
    });
    unsafe {
        dispatch_async_f(
            std::ptr::addr_of!(_dispatch_main_q) as *mut DispatchQueueOpaque,
            Box::into_raw(job).cast::<c_void>(),
            main_thread_entry,
        );
    }
    if timing_enabled() {
        eprintln!("Cropmark macos shell: dispatched to main thread");
    }
    match started_rx.recv_timeout(MAIN_THREAD_ENTRY_TIMEOUT) {
        Ok(()) => {}
        Err(RecvTimeoutError::Timeout) => {
            abandoned.store(true, Ordering::SeqCst);
            // 若任务恰已开始执行,递增代际让泵在下一轮退出,不把面板留在屏幕上。
            request_shell_close();
            if timing_enabled() {
                eprintln!(
                    "Cropmark macos shell: main-thread entry timeout after {MAIN_THREAD_ENTRY_TIMEOUT:?}"
                );
            }
            return Err(CaptureError::timeout(
                "error.capture.shell_entry_timeout",
                "error.capture.timeout_hint",
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Err(CaptureError::api("error.capture.shell_main_thread"));
        }
    }
    match done_rx.recv() {
        Ok(result) => result,
        Err(_) => Err(CaptureError::api("error.capture.shell_main_thread")),
    }
}

/// 壳退出清理:正常返回与 unwind(panic)都收起面板并恢复系统光标,
/// 避免裸 panic 留下全屏遮挡窗(ADR-17 失败边界)。
struct ShellExitGuard<'a> {
    window: &'a NSWindow,
}

impl Drop for ShellExitGuard<'_> {
    fn drop(&mut self) {
        self.window.orderOut(None);
        NSCursor::arrowCursor().set();
    }
}

/// 主队列入口:裸 panic 在此收敛为错误而不是静默丢帧(ADR-17)。
extern "C" fn main_thread_entry(context: *mut c_void) {
    let mut job = unsafe { Box::from_raw(context as *mut MainThreadJob) };
    let done = job.done.clone();
    let abandoned = job.abandoned.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        if abandoned.load(Ordering::SeqCst) {
            return Ok(None);
        }
        // 等待方已超时放弃(started 接收端已丢弃):不进入壳。
        if job.started.send(()).is_err() {
            return Ok(None);
        }
        let canvas = job.canvas.take().expect("main thread job canvas");
        let mtm = MainThreadMarker::new().expect("main queue 任务应在主线程运行");
        run_shell(mtm, canvas, job.geometry, job.hooks, job.abandoned.clone()).map(Some)
    }));
    let outcome = match result {
        Ok(Ok(Some(outcome))) => Ok(outcome),
        Ok(Ok(None)) => return,
        Ok(Err(error)) => Err(error),
        Err(_) => Err(CaptureError::api("error.capture.shell_panic")),
    };
    let _ = done.send(outcome);
}

fn run_shell(
    mtm: MainThreadMarker,
    canvas: Canvas,
    geometry: ShellGeometry,
    hooks: ShellHooks,
    abandoned: Arc<AtomicBool>,
) -> Result<RegionOutcome, CaptureError> {
    // 本次壳的关闭基线:基线之前的关闭请求属于旧壳,不再影响本次(评审 P3)。
    let close_baseline = SHELL_CLOSE_EPOCH.load(Ordering::SeqCst);
    let timing = timing_enabled();
    let started = Instant::now();
    if timing {
        eprintln!(
            "Cropmark macos shell: entered main thread logical={}x{} scale={} epoch={close_baseline}",
            geometry.logical_width, geometry.logical_height, geometry.scale
        );
    }
    if abandoned.load(Ordering::SeqCst) {
        // 派发方已超时:不再建窗,结果会被会话层丢弃。
        if timing {
            eprintln!("Cropmark macos shell: abandoned before window creation");
        }
        return Ok(RegionOutcome::Cancelled);
    }
    let frame = screen_frame_for(mtm, &geometry);
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(ShellState {
            hooks,
            canvas,
            scale: geometry.scale,
            frame_x: frame.origin.x,
            frame_y: frame.origin.y,
            frame_h: frame.size.height,
            image: None,
            dirty: false,
            outcome: None,
            timing,
            applied_cursor: Some(CursorKind::Crosshair),
            resize_cursors: ResizeCursors::default(),
            close_baseline,
            abandoned,
        });
    });
    let app = NSApplication::sharedApplication(mtm);
    // Accessory 托盘应用:macOS 14+ 的 activate() 不抢焦点,旧 API 在 14+ 也无效果,
    // 仍调用一次以覆盖 13 及更早;真正收事件靠 NonactivatingPanel + 事件泵转发.
    app.activate();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let view = unsafe { create_selection_view(mtm, frame.size) };
    let window = unsafe { create_key_window(mtm, frame)? };
    // 退出清理守卫:任何后续 panic 都不会把全屏面板留在屏幕上(ADR-17)。
    let exit_guard = ShellExitGuard { window: &window };
    window.setLevel(kCGScreenSaverWindowLevel as isize);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            .union(NSWindowCollectionBehavior::FullScreenAuxiliary),
    );
    window.setAcceptsMouseMovedEvents(true);
    window.setIgnoresMouseEvents(false);
    window.setOpaque(true);
    window.setHasShadow(false);
    window.setHidesOnDeactivate(false);
    window.setFloatingPanel(true);
    window.setBecomesKeyOnlyIfNeeded(false);
    window.setWorksWhenModal(true);
    let content_view: &NSView = &view;
    window.setContentView(Some(content_view));
    window.setFrame_display(frame, true);
    // 光标提示(ADR-5):首帧前先显示十字,其后每个输入事件由 apply_cursor
    // 按引擎 cursor_for 切换——手柄/边→resize、选区内部→开手(拖移中闭合手)、
    // chrome→箭头、空白→十字;斜向 resize 用 SF Symbol 自绘。
    NSCursor::crosshairCursor().set();
    // 先合成首帧再上屏,避免 orderFront 到首次 drawRect 之间闪黑。
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            present(state, &view);
        }
    });
    // Accessory 未激活时 makeKeyAndOrderFront 可能不上屏;Regardless 仍置顶.
    window.orderFrontRegardless();
    window.makeKeyAndOrderFront(None);
    let responder: &NSResponder = &view;
    window.makeFirstResponder(Some(responder));
    view.display();
    if timing {
        eprintln!("Cropmark macos shell: panel presented");
    }
    pump_until_done(&app, &view);
    drop(exit_guard);
    let state = STATE.with(|slot| slot.borrow_mut().take());
    let outcome = state
        .and_then(|state| state.outcome)
        .unwrap_or(RegionOutcome::Cancelled);
    if timing {
        eprintln!(
            "Cropmark macos shell: outcome={outcome:?} elapsed={:?}",
            started.elapsed()
        );
    }
    Ok(outcome)
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
    let style = NSWindowStyleMask::Borderless.union(NSWindowStyleMask::NonactivatingPanel);
    let window: Retained<KeyWindow> = msg_send![super(allocated),
        initWithContentRect: content_rect,
        styleMask: style,
        backing: NSBackingStoreType::Buffered,
        defer: false];
    Ok(window)
}

/// 手动泵事件直到引擎给出终态;阻塞等待期间 run loop 会顺带服务
/// 窗口刷新(display/drawRect)与其它来源。等待有界(250ms):除事件外也
/// 周期性检查关闭代际与派发方放弃标志,保证旧壳/迟到壳及时退出;无事件时
/// 立即重入等待。
fn pump_until_done(app: &NSApplication, view: &SelectionView) {
    loop {
        let done = STATE.with(|slot| {
            let mut guard = slot.borrow_mut();
            let Some(state) = guard.as_mut() else {
                return false;
            };
            if should_cancel(
                state.outcome.is_some(),
                state.abandoned.load(Ordering::SeqCst),
                state.close_baseline,
                SHELL_CLOSE_EPOCH.load(Ordering::SeqCst),
            ) {
                if state.timing {
                    eprintln!("Cropmark macos shell: close requested (epoch/abandoned)");
                }
                state.outcome = Some(RegionOutcome::Cancelled);
            }
            state.outcome.is_some()
        });
        if done {
            return;
        }
        let event = app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&NSDate::dateWithTimeIntervalSinceNow(0.25)),
            // CommonModes 覆盖 default+tracking,拖拽时 LeftMouseDragged 不会丢.
            unsafe { NSRunLoopCommonModes },
            true,
        );
        if let Some(event) = event {
            // 输入走窗口过程同构路径,不依赖 NSView 响应链(Accessory 未激活时
            // sendEvent 常常到不了 mouseDown:/mouseMoved:).
            if !route_input(view, &event) {
                app.sendEvent(&event);
            }
            app.updateWindows();
        }
    }
}

/// 泵的取消判定:已有终态不覆盖;派发方放弃(主线程进入超时)或关闭代际
/// 相对启动基线发生变化即取消。基线在壳启动时记录,旧壳退出不会清除新壳的
/// 待关闭状态(评审 P3);两种来源都可从任意线程置位。
fn should_cancel(outcome_set: bool, abandoned: bool, baseline: u64, current_epoch: u64) -> bool {
    !outcome_set && (abandoned || current_epoch != baseline)
}

/// 消费鼠标/键盘并交给引擎;返回 true 表示已处理,调用方不再 sendEvent(避免双分发).
fn route_input(view: &SelectionView, event: &NSEvent) -> bool {
    let ty = event.r#type();
    if ty == NSEventType::MouseMoved
        || ty == NSEventType::LeftMouseDragged
        || ty == NSEventType::RightMouseDragged
    {
        forward_mouse(view, event, MouseInput::Move);
        true
    } else if ty == NSEventType::LeftMouseDown {
        forward_mouse(view, event, MouseInput::LeftDown);
        true
    } else if ty == NSEventType::LeftMouseUp {
        forward_mouse(view, event, MouseInput::LeftUp);
        true
    } else if ty == NSEventType::RightMouseDown {
        forward_mouse(view, event, MouseInput::RightDown);
        true
    } else if ty == NSEventType::KeyDown {
        if let Some(key) = map_key_code(event.keyCode()) {
            let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
            dispatch_input(view, InputEvent::Key { key, shift });
        }
        true
    } else {
        false
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

/// 事件物理像素坐标:`NSEvent.mouseLocation` 是 AppKit 全局坐标(主屏左下原点),
/// 再减去窗框得到内容区偏移并翻转 Y,得到引擎物理像素(左上原点).
fn event_point(view: &SelectionView, _event: &NSEvent) -> (i32, i32) {
    let (scale, fallback) = STATE
        .with(|slot| {
            slot.borrow().as_ref().map(|state| {
                (
                    state.scale,
                    (state.frame_x, state.frame_y, state.frame_h),
                )
            })
        })
        .unwrap_or((1.0, (0.0, 0.0, 0.0)));
    let (frame_x, frame_y, frame_h) = view
        .window()
        .map(|window: Retained<NSWindow>| {
            let frame = window.frame();
            (frame.origin.x, frame.origin.y, frame.size.height)
        })
        .unwrap_or(fallback);
    let screen = NSEvent::mouseLocation();
    geometry::appkit_global_to_physical(
        screen.x, screen.y, frame_x, frame_y, frame_h, scale,
    )
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
        apply_cursor(state);
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

/// 依引擎当前提示切换系统光标;形态未变化时跳过 set。resize 提示用 SF
/// Symbol 自绘,符号不可用时退回十字;光标切换不影响选择/确认/取消。
fn apply_cursor(state: &mut ShellState) {
    let kind = {
        let engine = &state.canvas.engine;
        let (x, y) = engine.cursor();
        cursor_kind(engine.cursor_for(x, y), engine.state())
    };
    if state.applied_cursor == Some(kind) {
        return;
    }
    state.applied_cursor = Some(kind);
    let cursor = match kind {
        CursorKind::Crosshair => NSCursor::crosshairCursor(),
        CursorKind::OpenHand => NSCursor::openHandCursor(),
        CursorKind::ClosedHand => NSCursor::closedHandCursor(),
        CursorKind::Arrow => NSCursor::arrowCursor(),
        resize => state
            .resize_cursors
            .get(resize)
            .unwrap_or_else(NSCursor::crosshairCursor),
    };
    cursor.set();
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
            state.outcome = Some(RegionOutcome::Preview(
                rect,
                state.canvas.engine.annotations().to_vec(),
            ));
        }
        EngineOutcome::Cancelled => {
            state.outcome = Some(RegionOutcome::Cancelled);
        }
        EngineOutcome::Action(action) => match action {
            SelectionAction::Annotate => {
                if let Some(outcome) = annotate_outcome(&state.canvas.engine) {
                    state.outcome = Some(outcome);
                }
            }
            SelectionAction::Cancel => {
                // 引擎在菜单路径已把「取消」译为 Cancelled;此支仅为防御。
                state.outcome = Some(RegionOutcome::Cancelled);
            }
            SelectionAction::CopyColor => {
                copy_color_value(state);
            }
            // 标注工具条动作由引擎内部消费,不会到达这里;防御性忽略。
            SelectionAction::Tool(_)
            | SelectionAction::Undo
            | SelectionAction::Redo
            | SelectionAction::Delete => {}
            quiet => {
                if let (Some(rect), Some(action)) =
                    (state.canvas.engine.selection(), quiet_action_for(quiet))
                {
                    state.outcome = Some(RegionOutcome::Quiet(
                        rect,
                        action,
                        state.canvas.engine.annotations().to_vec(),
                    ));
                }
            }
        },
    }
}

/// 「标注」动作到壳结果的映射:引擎尚无选区时返回 None,会话继续等待。
fn annotate_outcome(engine: &SelectionEngine) -> Option<RegionOutcome> {
    engine.selection().map(|rect| {
        RegionOutcome::Annotate(rect, engine.annotations().to_vec())
    })
}

/// 操作条/菜单动作到静默完成动作的映射;标注/取消/复制色值与标注工具条
/// 动作不在此列。
fn quiet_action_for(action: SelectionAction) -> Option<QuietAction> {
    match action {
        SelectionAction::Copy => Some(QuietAction::Copy),
        SelectionAction::Save => Some(QuietAction::Save),
        SelectionAction::Pin => Some(QuietAction::Pin),
        SelectionAction::Ocr => Some(QuietAction::Ocr),
        SelectionAction::Annotate
        | SelectionAction::Cancel
        | SelectionAction::CopyColor
        | SelectionAction::Tool(_)
        | SelectionAction::Undo
        | SelectionAction::Redo
        | SelectionAction::Delete => None,
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
        let overlay = canvas.engine.annotation_overlay();
        canvas
            .composer
            .compose_into_with_overlay(&scene, &overlay, &mut canvas.scratch);
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
    fn annotate_outcome_needs_a_selection_and_keeps_rect() {
        let mut engine = SelectionEngine::new(320, 200, FeatureFlags::default());
        assert_eq!(annotate_outcome(&engine), None);
        engine.handle_event(InputEvent::LeftDown { x: 5, y: 6 });
        engine.handle_event(InputEvent::PointerMove { x: 35, y: 46 });
        engine.handle_event(InputEvent::LeftUp { x: 35, y: 46 });
        let rect = engine.selection().unwrap();
        assert_eq!(
            annotate_outcome(&engine),
            Some(RegionOutcome::Annotate(rect, Vec::new()))
        );
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

    #[test]
    fn appkit_mouse_location_flips_y_so_bottom_is_not_engine_top() {
        // 与 geometry 回归同构:漏翻转时底部点击会变成引擎 y=0,选区钉在顶边.
        let (x, y) = geometry::appkit_global_to_physical(40.0, 0.0, 0.0, 0.0, 600.0, 2.0);
        assert_eq!((x, y), (80, 1200));
        let (x, y) = geometry::appkit_global_to_physical(40.0, 600.0, 0.0, 0.0, 600.0, 2.0);
        assert_eq!((x, y), (80, 0));
    }

    #[test]
    fn cursor_kind_refines_move_and_maps_every_hint() {
        let selected = EngineState::Selected;
        let moving = EngineState::Moving {
            origin: PhysicalRect {
                x: 10,
                y: 10,
                width: 40,
                height: 30,
            },
            grab_x: 20,
            grab_y: 20,
        };
        assert_eq!(
            cursor_kind(CursorHint::Crosshair, &selected),
            CursorKind::Crosshair
        );
        assert_eq!(
            cursor_kind(CursorHint::Move, &selected),
            CursorKind::OpenHand
        );
        assert_eq!(
            cursor_kind(CursorHint::Move, &moving),
            CursorKind::ClosedHand
        );
        assert_eq!(
            cursor_kind(CursorHint::Pointer, &selected),
            CursorKind::Arrow
        );
        assert_eq!(cursor_kind(CursorHint::Arrow, &selected), CursorKind::Arrow);
        assert_eq!(
            cursor_kind(CursorHint::ResizeNS, &selected),
            CursorKind::ResizeNS
        );
        assert_eq!(
            cursor_kind(CursorHint::ResizeEW, &selected),
            CursorKind::ResizeEW
        );
        assert_eq!(
            cursor_kind(CursorHint::ResizeNWSE, &selected),
            CursorKind::ResizeNWSE
        );
        assert_eq!(
            cursor_kind(CursorHint::ResizeNESW, &selected),
            CursorKind::ResizeNESW
        );
    }

    #[test]
    fn resize_symbols_cover_four_directions_and_nothing_else() {
        let mut names: Vec<&str> = [
            CursorKind::ResizeNS,
            CursorKind::ResizeEW,
            CursorKind::ResizeNWSE,
            CursorKind::ResizeNESW,
        ]
        .into_iter()
        .map(|kind| resize_symbol(kind).expect("resize 提示都应有符号"))
        .collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count);
        // 非 resize 形态不自绘。
        assert_eq!(resize_symbol(CursorKind::Crosshair), None);
        assert_eq!(resize_symbol(CursorKind::OpenHand), None);
        assert_eq!(resize_symbol(CursorKind::ClosedHand), None);
        assert_eq!(resize_symbol(CursorKind::Arrow), None);
    }

    #[test]
    fn engine_cursor_hints_map_to_shell_kinds_across_interactions() {
        // 关闭放大镜:只核对选区几何→光标形态的整链路(避开面板命中)。
        let flags = FeatureFlags {
            magnifier: false,
            ..FeatureFlags::default()
        };
        let mut engine = SelectionEngine::new(320, 200, flags);
        let kind = |engine: &SelectionEngine, x: i32, y: i32| {
            cursor_kind(engine.cursor_for(x, y), engine.state())
        };
        // 空白 → 十字。
        assert_eq!(kind(&engine, 10, 10), CursorKind::Crosshair);
        // 拖出选区:内部 → 开手,角/边 → resize,空白 → 十字。
        engine.handle_event(InputEvent::LeftDown { x: 40, y: 30 });
        engine.handle_event(InputEvent::PointerMove { x: 200, y: 120 });
        engine.handle_event(InputEvent::LeftUp { x: 200, y: 120 });
        assert_eq!(kind(&engine, 120, 75), CursorKind::OpenHand);
        assert_eq!(kind(&engine, 40, 30), CursorKind::ResizeNWSE);
        assert_eq!(kind(&engine, 120, 30), CursorKind::ResizeNS);
        assert_eq!(kind(&engine, 10, 10), CursorKind::Crosshair);
        // 按下选区内部拖移 → 闭合手;松开后回到开手。
        engine.handle_event(InputEvent::LeftDown { x: 120, y: 75 });
        assert_eq!(kind(&engine, 120, 75), CursorKind::ClosedHand);
        engine.handle_event(InputEvent::LeftUp { x: 120, y: 75 });
        assert_eq!(kind(&engine, 120, 75), CursorKind::OpenHand);
    }

    #[test]
    fn should_cancel_covers_epoch_and_abandoned_without_overriding_outcome() {
        // 无终态且基线未变、未放弃 → 不取消。
        assert!(!should_cancel(false, false, 7, 7));
        // 关闭代际变化(stale 重置/超时兜底)→ 取消。
        assert!(should_cancel(false, false, 7, 8));
        // 派发方放弃(主线程进入超时)→ 取消。
        assert!(should_cancel(false, true, 7, 7));
        assert!(should_cancel(false, true, 7, 8));
        // 已有终态:任何新信号都不覆盖。
        assert!(!should_cancel(true, false, 7, 7));
        assert!(!should_cancel(true, false, 7, 8));
        assert!(!should_cancel(true, true, 7, 7));
    }

    #[test]
    fn timeout_and_panic_errors_are_localized_retryable_messages() {
        let timeout = CaptureError::timeout(
            "error.capture.shell_entry_timeout",
            "error.capture.timeout_hint",
        );
        assert_eq!(
            timeout.kind,
            crate::capture::error::CaptureErrorKind::Timeout
        );
        assert!(!timeout.message.contains("error.capture"));
        assert!(timeout.hint.is_some());

        let panic = CaptureError::api("error.capture.shell_panic");
        assert!(!panic.message.contains("error.capture"));
        assert!(!panic.user_message().is_empty());
    }
}
