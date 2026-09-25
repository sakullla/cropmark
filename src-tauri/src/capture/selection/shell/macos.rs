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
//! Cropmark 是 Accessory 托盘应用;macOS 14+ 的 `NSApp.activate()` 不会抢焦点.
//! 选区开始时把激活策略切到 Regular,面板才能盖住其它应用并收下鼠标;
//! 结束后由 `front::demote_if_idle` 退回 Accessory.自定义 NSPanel 子类放行
//! canBecomeKeyWindow,并绕过 constrainFrameRect(否则 AppKit 会把全屏框压到菜单栏下方).
//! 内容视图为自定义 NSView,`drawRect:` 中经 CGImage 绘制合成帧(引擎输出 RGBA→BGRA,
//! kCGBitmapByteOrder32Little|kCGImageAlphaPremultipliedFirst).
//! 鼠标坐标用 `NSEvent.mouseLocation`(AppKit 左下原点)换到引擎左上原点;事件泵
//! 像 Windows 壳的窗口过程一样直接转发,不把输入只交给 NSView 响应链.
//! 光标提示按引擎 `cursor_for` 映射:选区内部→开手(拖移中闭合手)、手柄/边→
//! resize(SF Symbol 自绘)、chrome→箭头、空白→双色十字(浅芯深描边、热点镂空);
//! 符号不可用时退回该双色十字,不使用系统 `crosshairCursor`(ADR-1/ADR-5).
//! 光标切换不阻断选择/确认/取消.
//! 文本输入(R21):文本工具激活时 `keyDown:` 经 `interpretKeyEvents:` 交给系统
//! 输入上下文,视图实现 `NSTextInputClient`(insertText/setMarkedText/候选窗定位),
//! IME 组合串作为 preedit 交给引擎绘制、提交串走 Text 事件;输入法不可用时
//! ASCII 直输仍经 insertText 到达引擎,选择/确认/取消完全不受影响.
//!
//! 注意:本模块只能在 macOS 编译;Windows/Linux 主机上的离线核对以
//! windows.rs 逐块对照 + `geometry::appkit_global_to_physical` 测试为准.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{define_class, msg_send, sel, AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSCursor, NSEvent,
    NSEventMask, NSEventModifierFlags, NSEventType, NSGraphicsContext, NSImage, NSPanel,
    NSResponder, NSScreen, NSTextInputClient, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFRetained, CFRunLoop, CFType, CGFloat, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    kCGScreenSaverWindowLevel, CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext,
    CGDataProvider, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGInterpolationQuality,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSNotFound, NSPoint, NSRange, NSRangePointer, NSRect, NSSize,
    NSString,
};

use crate::annotate::Annotation;
use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{self, MonitorGeom, PhysicalRect};
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    AnnotationOptions, AnnotationTool, CursorHint, EngineOutcome, EngineState, FeatureFlags,
    InputEvent, LogicalKey, SelectionAction, SelectionEngine, ToolMode,
};
use crate::capture::session::QuietAction;

// macOS Carbon 键码(HIToolbox/Events.h,kVK_*;与键盘布局无关的物理键位)。
const KC_ANSI_C: u16 = 0x08;
const KC_ANSI_Z: u16 = 0x06;
const KC_ANSI_Y: u16 = 0x10;
const KC_DELETE: u16 = 0x33;
const KC_RETURN: u16 = 0x24;
const KC_ESCAPE: u16 = 0x35;
const KC_LEFT: u16 = 0x7B;
const KC_RIGHT: u16 = 0x7C;
const KC_DOWN: u16 = 0x7D;
const KC_UP: u16 = 0x7E;
const KC_FORWARD_DELETE: u16 = 0x75;
// 标注工具快捷键(与预览编辑器一致:A/R/E/L/M/B/H/P/N/T)。
const KC_ANSI_A: u16 = 0x00;
const KC_ANSI_B: u16 = 0x0B;
const KC_ANSI_E: u16 = 0x0E;
const KC_ANSI_G: u16 = 0x05;
const KC_ANSI_H: u16 = 0x04;
const KC_ANSI_K: u16 = 0x28;
const KC_ANSI_L: u16 = 0x25;
const KC_ANSI_M: u16 = 0x2E;
const KC_ANSI_N: u16 = 0x2D;
const KC_ANSI_P: u16 = 0x23;
const KC_ANSI_R: u16 = 0x0F;
const KC_ANSI_S: u16 = 0x01;
const KC_ANSI_T: u16 = 0x11;
const KC_ANSI_X: u16 = 0x07;

/// 壳的最终结果:会话层据此选择完成路径(与 Windows 壳同构)。
/// R21 起携带即时标注图元;文本输入经 `NSTextInputClient` 接入,
/// `AnnotationOptions::text_input` 为真,工具条含文字工具。
#[derive(Debug, Clone, PartialEq)]
pub enum RegionOutcome {
    /// Enter 确认:rect 交给网页浮层,壳本身不结束截图也不打开预览。
    Preview(PhysicalRect, Vec<Annotation>),
    /// 操作条/菜单的「标注」动作:rect 强制走预览编辑器,不受静默完成配置影响。
    Annotate(PhysicalRect, Vec<Annotation>),
    /// 操作条/菜单的 copy/save/pin/ocr 动作:rect 走 Quiet 完成路径并执行动作。
    Quiet(PhysicalRect, QuietAction, Vec<Annotation>),
    /// R1 操作条/菜单的长截图动作:rect 交给会话层开始滚动会话。
    LongCapture(PhysicalRect, Vec<Annotation>),
    /// Esc 或菜单「取消」:整个会话取消。
    Cancelled,
}

/// 引擎 + 合成/呈现缓冲。独立成结构使呈现路径可脱离 AppHandle 测量。
struct Canvas {
    engine: SelectionEngine,
    composer: Composer,
    /// 引擎输出的 RGBA 合成帧(长度与冻结帧一致);CGImage 直接读这块,不再 swizzle。
    scratch: Vec<u8>,
    /// 兼容 Windows 壳字段;macOS 走 RGBA 直出,保持空缓冲。
    #[allow(dead_code)]
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
    /// 冻结帧 DPI 缩放(frame.scale):十字光标按它 keyed 重建。
    frame_scale: f64,
    /// 选区窗 AppKit 框(左下原点),供 `NSEvent.mouseLocation` 换算兜底.
    frame_x: f64,
    frame_y: f64,
    frame_h: f64,
    /// 最近一次 Redraw 合成的 CGImage;provider 不持有数据,
    /// 由 `canvas.scratch`(定容,地址不漂移)保活。
    image: Option<CFRetained<CGImage>>,
    /// 有待 AppKit 合批刷新的合成帧;只 setNeedsDisplay,不强制 display。
    dirty: bool,
    outcome: Option<RegionOutcome>,
    timing: bool,
    /// 选区层 present 日志节流,避免每次鼠标移动刷 stderr。
    last_present_log: Option<Instant>,
    /// 最近一次应用的壳内光标形态;未变化时跳过 set。
    applied_cursor: Option<CursorKind>,
    /// resize 自绘光标缓存(懒构建;符号不可用时保持 None,显示时退回十字)。
    resize_cursors: ResizeCursors,
    /// 本次壳启动时的关闭代际基线;泵检测到代际变化即取消(ADR-16/17)。
    close_baseline: u64,
    /// 派发方主线程进入超时后置位:迟到的壳在泵内自行退出,不悬挂在屏幕上。
    abandoned: Arc<AtomicBool>,
    /// IME 组合(未提交)文本与选区;None = 无组合。
    marked: Option<MarkedText>,
    window: Option<Retained<KeyWindow>>,
    view: Option<Retained<SelectionView>>,
    monitor: Option<Retained<AnyObject>>,
    done: Option<Sender<Result<RegionOutcome, CaptureError>>>,
}

/// IME 组合串与选中区间;长度单位与 `NSRange` 一致(UTF-16 码元)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MarkedText {
    text: String,
    selected: NSRange,
}

impl MarkedText {
    fn utf16_len(&self) -> usize {
        self.text.encode_utf16().count()
    }
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

/// 双色十字像素:浅色线芯 + 深色描边;Empty 为透明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrosshairPixel {
    Empty,
    Core,
    Outline,
}

/// 运行时十字规格(奇数边长,热点在交叉点)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CrosshairSprite {
    size: usize,
    arm: usize,
    /// 线芯宽度(物理像素,奇数)。100% 为 3;更高缩放送入后抬到奇数,使热点仍在中心像素。
    thickness: usize,
}

impl CrosshairSprite {
    /// 1x 基准臂长(物理像素);实际臂长按 frame.scale 缩放。
    const BASE_ARM: usize = 10;

    /// 100% 线芯宽度。随 scale 成倍加粗,偶数结果再加 1,保证奇数宽。
    fn thickness_for_scale(scale: f64) -> usize {
        let raw = ((3.0 * scale.max(0.1)).round() as usize).max(3);
        if raw % 2 == 0 {
            raw + 1
        } else {
            raw
        }
    }

    /// 最小双色位图(光标创建失败时的兜底规格)。线芯仍为 3 物理像素。
    const fn fallback() -> Self {
        Self {
            size: 9,
            arm: 3,
            thickness: 3,
        }
    }

    /// 按冻结帧 DPI 缩放生成规格:臂长与线芯都随 frame.scale 缩放
    /// (200% 时臂长翻倍,线芯从 3 加到 7)。边长保持奇数,热点在交叉点。
    fn for_scale(scale: f64) -> Self {
        let arm = ((Self::BASE_ARM as f64) * scale.max(0.1)).round().max(3.0) as usize;
        let thickness = Self::thickness_for_scale(scale);
        let radius = arm.max(thickness / 2) + 2;
        Self {
            size: radius * 2 + 1,
            arm,
            thickness,
        }
    }

    fn hotspot(self) -> (usize, usize) {
        let center = self.size / 2;
        (center, center)
    }

    /// 线芯:横竖臂宽为 `thickness`,沿臂距中心 1..=arm。中心热点像素由
    /// `pixel` 镂空,指针像素经镂空处直接可见。
    fn is_core(self, x: usize, y: usize) -> bool {
        let center = self.size / 2;
        if x == center && y == center {
            return false;
        }
        let dx = x.abs_diff(center);
        let dy = y.abs_diff(center);
        let half = self.thickness / 2;
        let horizontal = dy <= half && (1..=self.arm).contains(&dx);
        let vertical = dx <= half && (1..=self.arm).contains(&dy);
        horizontal || vertical
    }

    fn pixel(self, x: usize, y: usize) -> CrosshairPixel {
        if x >= self.size || y >= self.size {
            return CrosshairPixel::Empty;
        }
        let center = self.size / 2;
        // 中心热点像素镂空:热点仍对准指针像素,但该像素透明。
        if x == center && y == center {
            return CrosshairPixel::Empty;
        }
        if self.is_core(x, y) {
            return CrosshairPixel::Core;
        }
        let x0 = x.saturating_sub(1);
        let y0 = y.saturating_sub(1);
        let x1 = (x + 1).min(self.size - 1);
        let y1 = (y + 1).min(self.size - 1);
        for nx in x0..=x1 {
            for ny in y0..=y1 {
                if (nx != x || ny != y) && self.is_core(nx, ny) {
                    return CrosshairPixel::Outline;
                }
            }
        }
        CrosshairPixel::Empty
    }

    fn rgba(self) -> Vec<u8> {
        let mut out = vec![0u8; self.size * self.size * 4];
        for y in 0..self.size {
            for x in 0..self.size {
                let i = (y * self.size + x) * 4;
                match self.pixel(x, y) {
                    CrosshairPixel::Core => {
                        out[i] = 0xf7;
                        out[i + 1] = 0xf7;
                        out[i + 2] = 0xf7;
                        out[i + 3] = 0xff;
                    }
                    CrosshairPixel::Outline => {
                        out[i] = 0x14;
                        out[i + 1] = 0x14;
                        out[i + 2] = 0x14;
                        out[i + 3] = 0xff;
                    }
                    CrosshairPixel::Empty => {}
                }
            }
        }
        out
    }
}

unsafe extern "C-unwind" fn release_rgba_vec(
    info: *mut c_void,
    _data: NonNull<c_void>,
    _size: usize,
) {
    if !info.is_null() {
        drop(Box::from_raw(info.cast::<Vec<u8>>()));
    }
}

fn nsimage_from_rgba(pixels: Vec<u8>, size: usize, point_size: f64) -> Option<Retained<NSImage>> {
    let mut boxed = Box::new(pixels);
    let data_ptr = boxed.as_ptr();
    let data_len = boxed.len();
    let info = Box::into_raw(boxed).cast::<c_void>();
    let Some(provider) = (unsafe {
        CGDataProvider::with_data(
            info,
            data_ptr.cast::<c_void>(),
            data_len,
            Some(release_rgba_vec),
        )
    }) else {
        unsafe { drop(Box::from_raw(info.cast::<Vec<u8>>())) };
        return None;
    };
    let space = CGColorSpace::new_device_rgb()?;
    let bitmap_info = CGBitmapInfo(CGImageAlphaInfo::Last.0 | CGImageByteOrderInfo::Order32Big.0);
    let cg_image = unsafe {
        CGImage::new(
            size,
            size,
            8,
            32,
            size * 4,
            Some(&space),
            bitmap_info,
            Some(&provider),
            std::ptr::null::<CGFloat>(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    let ns_size = NSSize {
        width: point_size,
        height: point_size,
    };
    let image: Retained<NSImage> =
        unsafe { msg_send![NSImage::alloc(), initWithCGImage: &*cg_image, size: ns_size] };
    Some(image)
}

/// 由十字规格生成 NSCursor。位图按物理像素生成,NSImage 尺寸按
/// frame.scale 折算为点,使线芯宽度与规格一致。
fn cursor_from_sprite(sprite: CrosshairSprite, scale: f64) -> Option<Retained<NSCursor>> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let image = nsimage_from_rgba(sprite.rgba(), sprite.size, sprite.size as f64 / scale)?;
    let (hot_x, hot_y) = sprite.hotspot();
    Some(NSCursor::initWithImage_hotSpot(
        NSCursor::alloc(),
        &image,
        NSPoint {
            x: hot_x as f64 / scale,
            y: hot_y as f64 / scale,
        },
    ))
}

/// Crosshair 提示使用的双色十字;按 frame.scale keyed 缓存,缩放变化时
/// 重建而非进程级单例。主规格失败则用最小双色位图,不退回
/// `NSCursor::crosshairCursor`(ADR-1)。
fn dual_crosshair_cursor(scale: f64) -> Retained<NSCursor> {
    thread_local! {
        static CACHED: RefCell<HashMap<u64, Retained<NSCursor>>> =
            RefCell::new(HashMap::new());
    }
    CACHED.with(|slot| {
        let mut cache = slot.borrow_mut();
        cache
            .entry(scale.to_bits())
            .or_insert_with(|| {
                cursor_from_sprite(CrosshairSprite::for_scale(scale), scale)
                    .or_else(|| cursor_from_sprite(CrosshairSprite::fallback(), scale))
                    .expect("最小双色十字位图应能生成 NSCursor")
            })
            .clone()
    })
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
            handle_key(self, event);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty_rect: NSRect) {
            draw_cached_image(self);
        }

        /// AppKit 重建光标矩形时登记整块选区,使询问到的是当前形态。
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let Some(cursor) = cursor_image_for_active_state() else {
                return;
            };
            self.addCursorRect_cursor(self.bounds(), &cursor);
        }

        /// 指针进出光标矩形时 AppKit 会询问;此时按当前形态设置,不依赖上一次形态是否变化。
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    push_cursor(state);
                }
            });
        }
    }

    // NSTextInputClient(R21):文本工具激活时 `interpretKeyEvents:` 把键盘交给系统
    // 输入上下文,组合/提交回调到这里;组合串作为 preedit、提交串作为 Text 事件
    // 转发给选区引擎,壳不自行保存编辑状态(IME 不可用时 ASCII 直输同路径)。
    unsafe impl NSTextInputClient for SelectionView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _replacement_range: NSRange) {
            apply_committed_text(self, input_text(string));
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(
            &self,
            string: &AnyObject,
            selected_range: NSRange,
            _replacement_range: NSRange,
        ) {
            apply_marked_text(self, marked_from_input(input_text(string), selected_range));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            apply_marked_text(self, None);
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            state_marked().is_some()
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            match state_marked() {
                Some(marked) => NSRange::new(0, marked.utf16_len()),
                None => not_found_range(),
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            match state_marked() {
                Some(marked) => marked.selected,
                None => NSRange::new(editing_utf16_len(), 0),
            }
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes_for_marked_text(&self) -> Retained<NSArray<NSString>> {
            NSArray::new()
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        unsafe fn attributed_substring(
            &self,
            _range: NSRange,
            actual_range: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            if !actual_range.is_null() {
                unsafe { *actual_range = not_found_range() };
            }
            None
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        unsafe fn first_rect(&self, range: NSRange, actual_range: NSRangePointer) -> NSRect {
            if !actual_range.is_null() {
                unsafe { *actual_range = range };
            }
            caret_screen_rect().unwrap_or_default()
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> usize {
            0
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, selector: Sel) {
            let key = if selector == sel!(insertNewline:) {
                Some(LogicalKey::Enter)
            } else if selector == sel!(cancelOperation:) {
                Some(LogicalKey::Escape)
            } else if selector == sel!(deleteBackward:) || selector == sel!(deleteForward:) {
                Some(LogicalKey::Delete)
            } else if selector == sel!(moveLeft:) {
                Some(LogicalKey::ArrowLeft)
            } else if selector == sel!(moveRight:) {
                Some(LogicalKey::ArrowRight)
            } else if selector == sel!(moveUp:) {
                Some(LogicalKey::ArrowUp)
            } else if selector == sel!(moveDown:) {
                Some(LogicalKey::ArrowDown)
            } else {
                None
            };
            if let Some(key) = key {
                dispatch_input(self, InputEvent::Key { key, shift: false });
            }
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

/// 主线程进入上限(ADR-17):主 run loop 长时间不执行派发任务时返回明确错误,
/// 由会话层进入错误窗并可重试,而不是永久挂起。
const MAIN_THREAD_ENTRY_TIMEOUT: Duration = Duration::from_secs(10);

/// GCD 主队列回调里跑嵌套 `nextEventMatchingMask` 会重入
/// `dispatch_main_queue_drain`,在 macOS 上把主线程打满并卡住选区。
/// 必须把壳挂到主 **run loop** 上,让它在 NSApp 正常事件循环里启动。
fn schedule_on_main_run_loop(work: extern "C" fn(*mut c_void), context: *mut c_void) {
    let Some(rl) = CFRunLoop::main() else {
        work(context);
        return;
    };
    struct SendPtr(*mut c_void);
    unsafe impl Send for SendPtr {}
    let ptr = SendPtr(context);
    let block = RcBlock::new(move || work(ptr.0));
    let block: &DynBlock<dyn Fn()> = &block;
    let mode = unsafe { kCFRunLoopCommonModes }.map(|mode| mode as &CFType);
    unsafe { rl.perform_block(mode, Some(block)) };
    rl.wake_up();
}

/// 当前壳的关闭代际(stale 重置/超时兜底):从任意线程递增,泵在下一轮迭代取消。
/// 用代际而不是布尔标志:旧壳的收尾不会清除新壳的待关闭状态(评审 P3)。
static SHELL_CLOSE_EPOCH: AtomicU64 = AtomicU64::new(0);
static SHELL_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 请求关闭当前选区壳(线程安全,可从任意线程调用):递增关闭代际并经
/// 主队列唤醒 AppKit 事件泵;泵退出后旧结果由会话层代际校验丢弃(ADR-16)。
pub fn shell_is_active() -> bool {
    SHELL_ACTIVE.load(Ordering::SeqCst)
}

pub fn request_shell_close() {
    SHELL_CLOSE_EPOCH.fetch_add(1, Ordering::SeqCst);
    schedule_on_main_run_loop(finish_shell_if_needed_entry, std::ptr::null_mut());
}

extern "C" fn finish_shell_if_needed_entry(_context: *mut c_void) {
    finish_shell_if_needed();
}

/// 派发到主线程所需的最小屏幕几何(Copy,供 'static 闭包携带)。
#[derive(Debug, Clone, Copy)]
struct ShellGeometry {
    logical_x: i32,
    logical_y: i32,
    logical_width: u32,
    logical_height: u32,
    scale: f64,
    /// 冻结帧 DPI 缩放(frame.scale):十字光标按它重建。
    frame_scale: f64,
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
        present_buf: Vec::new(),
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
        frame_scale: frame.scale,
    };
    // 选区壳必须在主线程建窗,但不能在主线程(或 GCD/runloop 回调里)
    // 嵌套 nextEventMatchingMask:macOS 27 上会把主线程打满并卡住。
    // 生产路径在 spawn_blocking 上等待;主线程只负责建窗和收事件。
    if MainThreadMarker::new().is_some() {
        return Err(CaptureError::api("error.capture.shell_main_thread"));
    }
    run_on_main_thread(canvas, geometry, hooks)
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

/// 经主 run loop 异步派发并有界等待"已进入主线程"(ADR-17);壳的真正执行时间由
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
    schedule_on_main_run_loop(main_thread_entry, Box::into_raw(job).cast::<c_void>());
    if timing_enabled() {
        eprintln!("Cropmark macos shell: dispatched to main run loop");
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

/// 主 run loop 入口:只建窗并挂本地事件监听,立刻返回把主线程还给 NSApp。
/// 终态由事件监听/视图回调里的 `finish_shell_if_needed` 送回等待方。
extern "C" fn main_thread_entry(context: *mut c_void) {
    let mut job = unsafe { Box::from_raw(context as *mut MainThreadJob) };
    let done = job.done.clone();
    let abandoned = job.abandoned.clone();
    let result = catch_unwind(AssertUnwindSafe(|| {
        if abandoned.load(Ordering::SeqCst) {
            return Ok(false);
        }
        if job.started.send(()).is_err() {
            return Ok(false);
        }
        let canvas = job.canvas.take().expect("main thread job canvas");
        let mtm = MainThreadMarker::new().expect("run loop 任务应在主线程运行");
        start_shell(
            mtm,
            canvas,
            job.geometry,
            job.hooks,
            job.abandoned.clone(),
            job.done.clone(),
        )
        .map(|()| true)
    }));
    match result {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {}
        Ok(Err(error)) => {
            let _ = done.send(Err(error));
        }
        Err(_) => {
            let _ = done.send(Err(CaptureError::api("error.capture.shell_panic")));
        }
    }
}

fn start_shell(
    mtm: MainThreadMarker,
    canvas: Canvas,
    geometry: ShellGeometry,
    hooks: ShellHooks,
    abandoned: Arc<AtomicBool>,
    done: Sender<Result<RegionOutcome, CaptureError>>,
) -> Result<(), CaptureError> {
    let close_baseline = SHELL_CLOSE_EPOCH.load(Ordering::SeqCst);
    let timing = timing_enabled();
    if timing {
        eprintln!(
            "Cropmark macos shell: entered main thread logical={}x{} scale={} epoch={close_baseline}",
            geometry.logical_width, geometry.logical_height, geometry.scale
        );
    }
    if abandoned.load(Ordering::SeqCst) {
        if timing {
            eprintln!("Cropmark macos shell: abandoned before window creation");
        }
        let _ = done.send(Ok(RegionOutcome::Cancelled));
        return Ok(());
    }
    let frame = screen_frame_for(mtm, &geometry);
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let view = unsafe { create_selection_view(mtm, frame.size) };
    let window = unsafe { create_key_window(mtm, frame)? };
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
    // setFloatingPanel 会把 level 打回 floating(3);必须在其后重新设屏保级,
    // 否则选区层会沉到 Dock/菜单栏下面。
    window.setLevel(kCGScreenSaverWindowLevel as isize);
    let content_view: &NSView = &view;
    window.setContentView(Some(content_view));
    window.setFrame_display(frame, true);
    dual_crosshair_cursor(geometry.frame_scale).set();
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(ShellState {
            hooks,
            canvas,
            scale: geometry.scale,
            frame_scale: geometry.frame_scale,
            frame_x: frame.origin.x,
            frame_y: frame.origin.y,
            frame_h: frame.size.height,
            image: None,
            dirty: false,
            outcome: None,
            timing,
            last_present_log: None,
            applied_cursor: Some(CursorKind::Crosshair),
            resize_cursors: ResizeCursors::default(),
            close_baseline,
            abandoned,
            marked: None,
            window: None,
            view: None,
            monitor: None,
            done: Some(done),
        });
    });
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            present(state, &view);
        }
    });
    window.orderFrontRegardless();
    window.makeKeyAndOrderFront(None);
    let responder: &NSResponder = &view;
    window.makeFirstResponder(Some(responder));
    view.display();
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.window = Some(window);
            state.view = Some(view);
        }
    });
    let monitor = install_input_monitor();
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.monitor = monitor;
        }
    });
    SHELL_ACTIVE.store(true, Ordering::SeqCst);
    if timing {
        eprintln!("Cropmark macos shell: panel presented (event monitor, no nested pump)");
    }
    Ok(())
}

const INPUT_EVENT_MASK: NSEventMask = NSEventMask(
    NSEventMask::LeftMouseDown.0
        | NSEventMask::LeftMouseUp.0
        | NSEventMask::LeftMouseDragged.0
        | NSEventMask::RightMouseDown.0
        | NSEventMask::MouseMoved.0
        | NSEventMask::KeyDown.0,
);

fn install_input_monitor() -> Option<Retained<AnyObject>> {
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        // AppKit 回调不可 panic。先 clone 视图再放掉 STATE 借用,避免
        // route_input 里 borrow_mut 重入 RefCell 直接 abort。
        let handled = catch_unwind(AssertUnwindSafe(|| {
            let view =
                STATE.with(|slot| slot.borrow().as_ref().and_then(|state| state.view.clone()));
            view.map(|view| route_input(&view, unsafe { event.as_ref() }))
                .unwrap_or(false)
        }))
        .unwrap_or(false);
        if handled {
            std::ptr::null_mut()
        } else {
            event.as_ptr()
        }
    });
    let block: &DynBlock<dyn Fn(NonNull<NSEvent>) -> *mut NSEvent> = &block;
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(INPUT_EVENT_MASK, block) }
}

fn finish_shell_if_needed() {
    let ready = STATE.with(|slot| {
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
    if ready {
        // 不能在 NSEvent 监听/视图回调里同步 removeMonitor,否则 AppKit 会拆掉
        // 正在跑的 handler。下一圈 run loop 再收摊。
        schedule_on_main_run_loop(finish_shell_entry, std::ptr::null_mut());
    }
}

extern "C" fn finish_shell_entry(_context: *mut c_void) {
    finish_shell();
}

fn finish_shell() {
    let Some(mut state) = STATE.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    SHELL_ACTIVE.store(false, Ordering::SeqCst);
    if let Some(monitor) = state.monitor.take() {
        unsafe { NSEvent::removeMonitor(&monitor) };
    }
    if let Some(window) = state.window.take() {
        window.orderOut(None);
    }
    NSCursor::arrowCursor().set();
    let outcome = state.outcome.unwrap_or(RegionOutcome::Cancelled);
    if state.timing {
        eprintln!("Cropmark macos shell: outcome={outcome:?}");
    }
    if let Some(done) = state.done.take() {
        let _ = done.send(Ok(outcome));
    }
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
        handle_key(view, event);
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

/// 事件→引擎像素:先把窗口点转到视图 backing,再按抓屏缓冲尺寸映射。
/// 不能用 monitor.scale 硬乘——1x 抓屏配 2x 换算会让选区偏到两倍位置。
fn event_point(view: &SelectionView, event: &NSEvent) -> (i32, i32) {
    let (engine_w, engine_h) = STATE
        .with(|slot| {
            slot.borrow()
                .as_ref()
                .map(|state| (state.canvas.width as f64, state.canvas.height as f64))
        })
        .unwrap_or((1.0, 1.0));
    let in_view = if event.windowNumber() != 0 {
        view.convertPoint_fromView(event.locationInWindow(), None)
    } else if let Some(window) = view.window() {
        let frame = window.frame();
        let screen = NSEvent::mouseLocation();
        view.convertPoint_fromView(
            NSPoint {
                x: screen.x - frame.origin.x,
                y: screen.y - frame.origin.y,
            },
            None,
        )
    } else {
        NSPoint { x: 0.0, y: 0.0 }
    };
    let backing = view.convertPointToBacking(in_view);
    let backing_size = view.convertSizeToBacking(view.bounds().size);
    geometry::backing_to_engine(
        backing.x,
        backing.y,
        backing_size.width,
        backing_size.height,
        engine_w,
        engine_h,
    )
}

/// 把一次输入事件交给引擎并处理其输出;终态由 pump 读取 STATE 判定。
/// drawRect 可能经 view.display 重入,因此 STATE 借用在 present 前释放。
fn dispatch_input(view: &SelectionView, event: InputEvent) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let (dirty, cursor_changed) = STATE.with(|slot| {
            let mut guard = slot.borrow_mut();
            let Some(state) = guard.as_mut() else {
                return (false, false);
            };
            feed_event(state, event, view);
            let cursor_changed = push_cursor(state);
            (state.dirty, cursor_changed)
        });
        if cursor_changed {
            if let Some(window) = view.window() {
                window.invalidateCursorRectsForView(view);
            }
        }
        if dirty {
            // 交给 AppKit 合批到下一帧,避免每次 PointerMove 同步 display 打满 CPU。
            view.setNeedsDisplay(true);
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.dirty = false;
                }
            });
        }
        finish_shell_if_needed();
    }));
}

/// 当前形态对应的系统光标。resize 用 SF Symbol,符号不可用时退回双色十字。
fn cursor_image(state: &mut ShellState, kind: CursorKind) -> Retained<NSCursor> {
    let frame_scale = state.frame_scale;
    match kind {
        CursorKind::Crosshair => dual_crosshair_cursor(frame_scale),
        CursorKind::OpenHand => NSCursor::openHandCursor(),
        CursorKind::ClosedHand => NSCursor::closedHandCursor(),
        CursorKind::Arrow => NSCursor::arrowCursor(),
        resize => state
            .resize_cursors
            .get(resize)
            .unwrap_or_else(|| dual_crosshair_cursor(frame_scale)),
    }
}

fn current_cursor_kind(state: &ShellState) -> CursorKind {
    let engine = &state.canvas.engine;
    let (x, y) = engine.cursor();
    cursor_kind(engine.cursor_for(x, y), engine.state())
}

/// 供 `resetCursorRects` 登记。无活动壳时返回 None。
fn cursor_image_for_active_state() -> Option<Retained<NSCursor>> {
    STATE.with(|slot| {
        let mut guard = slot.borrow_mut();
        let state = guard.as_mut()?;
        let kind = current_cursor_kind(state);
        Some(cursor_image(state, kind))
    })
}

/// 按当前形态设置光标。形态未变也调用 `set`,避免 AppKit 在选区外移动时把十字换掉。
/// 返回形态是否变化,供调用方刷新光标矩形。光标切换不影响选择/确认/取消。
fn push_cursor(state: &mut ShellState) -> bool {
    let kind = current_cursor_kind(state);
    let changed = state.applied_cursor != Some(kind);
    state.applied_cursor = Some(kind);
    cursor_image(state, kind).set();
    changed
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
            // R1:以当前选区开始长截图滚动会话。
            SelectionAction::LongCapture => {
                if let Some(rect) = state.canvas.engine.selection() {
                    state.outcome = Some(RegionOutcome::LongCapture(
                        rect,
                        state.canvas.engine.annotations().to_vec(),
                    ));
                }
            }
            // 标注工具条动作由引擎内部消费,不会到达这里;防御性忽略。
            SelectionAction::Tool(_)
            | SelectionAction::Undo
            | SelectionAction::Redo
            | SelectionAction::Delete
            | SelectionAction::More => {}
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
    engine
        .selection()
        .map(|rect| RegionOutcome::Annotate(rect, engine.annotations().to_vec()))
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
        | SelectionAction::Delete
        | SelectionAction::LongCapture
        | SelectionAction::More => None,
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
        // 文本编辑的退格/删除(非编辑态下引擎忽略)。
        KC_DELETE | KC_FORWARD_DELETE => Some(LogicalKey::Delete),
        // R5 注册表:工具快捷键(A/R/E/H/M/T/N/S/G/K/X)进入标注模式并选工具;
        // L/P/B 是合并工具的等效模式(直线/画笔/模糊)。C 保留为取色。
        KC_ANSI_A => Some(LogicalKey::Tool(AnnotationTool::Arrow)),
        KC_ANSI_R => Some(LogicalKey::Tool(AnnotationTool::Rect)),
        KC_ANSI_E => Some(LogicalKey::Tool(AnnotationTool::Ellipse)),
        KC_ANSI_H => Some(LogicalKey::Tool(AnnotationTool::Highlighter)),
        KC_ANSI_M => Some(LogicalKey::Tool(AnnotationTool::Mosaic)),
        KC_ANSI_T => Some(LogicalKey::Tool(AnnotationTool::Text)),
        KC_ANSI_N => Some(LogicalKey::Tool(AnnotationTool::Number)),
        KC_ANSI_S => Some(LogicalKey::Tool(AnnotationTool::Spotlight)),
        KC_ANSI_G => Some(LogicalKey::Tool(AnnotationTool::Magnifier)),
        KC_ANSI_K => Some(LogicalKey::Tool(AnnotationTool::Sticker)),
        KC_ANSI_X => Some(LogicalKey::Tool(AnnotationTool::Erase)),
        KC_ANSI_L => Some(LogicalKey::Mode(ToolMode::Line)),
        KC_ANSI_P => Some(LogicalKey::Mode(ToolMode::Pen)),
        KC_ANSI_B => Some(LogicalKey::Mode(ToolMode::Blur)),
        _ => None,
    }
}

/// 撤销/重做快捷键键码映射(Cmd/Ctrl 由调用方判定,Shift 决定重做)。
fn shortcut_key(code: u16, shift: bool) -> Option<LogicalKey> {
    match code {
        KC_ANSI_Z => Some(if shift {
            LogicalKey::Redo
        } else {
            LogicalKey::Undo
        }),
        KC_ANSI_Y => Some(LogicalKey::Redo),
        _ => None,
    }
}

/// 键盘事件分发:文本编辑中走 `interpretKeyEvents:`(系统输入上下文/IME);
/// 其余情况保持壳内直接映射(Enter/Esc/方向/C),Cmd/Ctrl+Z/Y 撤销/重做。
fn handle_key(view: &SelectionView, event: &NSEvent) {
    let editing = STATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|state| state.canvas.engine.text_edit().is_some())
            .unwrap_or(false)
    });
    if editing {
        let events = NSArray::from_retained_slice(&[Retained::from(event)]);
        view.interpretKeyEvents(&events);
        return;
    }
    let code = event.keyCode();
    let flags = event.modifierFlags();
    let shift = flags.contains(NSEventModifierFlags::Shift);
    let command =
        flags.intersects(NSEventModifierFlags::Command.union(NSEventModifierFlags::Control));
    if command {
        if let Some(key) = shortcut_key(code, shift) {
            dispatch_input(view, InputEvent::Key { key, shift });
            return;
        }
    }
    if let Some(key) = map_key_code(code) {
        // Cmd/Ctrl 组合不作为工具/模式快捷键(与 Windows/Linux 壳一致)。
        if command && matches!(key, LogicalKey::Tool(_) | LogicalKey::Mode(_)) {
            return;
        }
        dispatch_input(view, InputEvent::Key { key, shift });
    }
}

// ---- R21 文本输入:组合/提交到引擎的转发与输入上下文查询 ----

/// `NSRange` 的“无范围”值(NSNotFound, 0)。
fn not_found_range() -> NSRange {
    NSRange::new(NSNotFound as usize, 0)
}

/// setMarkedText: 的入参校验:空串等于结束组合,选区钳制在组合串长度内。
fn marked_from_input(text: String, selected_range: NSRange) -> Option<MarkedText> {
    if text.is_empty() {
        return None;
    }
    let len = text.encode_utf16().count();
    let location = selected_range.location.min(len);
    let length = selected_range.length.min(len - location);
    Some(MarkedText {
        text,
        selected: NSRange::new(location, length),
    })
}

/// 读 shell 状态执行副作用;测试/收尾后状态缺失时静默忽略。
fn with_shell_state<R>(f: impl FnOnce(&mut ShellState) -> R) -> Option<R> {
    STATE.with(|slot| slot.borrow_mut().as_mut().map(f))
}

/// 当前编辑会话的文本(已提交 + 组合)UTF-16 长度;无会话为 0。
fn editing_utf16_len() -> usize {
    STATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|state| state.canvas.engine.text_edit())
            .map(|edit| edit.display().encode_utf16().count())
            .unwrap_or(0)
    })
}

fn state_marked() -> Option<MarkedText> {
    STATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|state| state.marked.clone())
    })
}

/// 提交文本(insertText:):结束组合态并交给引擎;无编辑会话时引擎自行忽略。
fn apply_committed_text(view: &SelectionView, text: String) {
    if text.is_empty() {
        return;
    }
    with_shell_state(|state| state.marked = None);
    dispatch_input(view, InputEvent::Text(text));
}

/// 更新组合串(setMarkedText:/unmarkText):作为 preedit 交引擎绘制。
fn apply_marked_text(view: &SelectionView, marked: Option<MarkedText>) {
    with_shell_state(|state| state.marked = marked.clone());
    let preedit = marked.map(|marked| marked.text).unwrap_or_default();
    dispatch_input(view, InputEvent::Composition(preedit));
}

/// insertText:/setMarkedText: 的 string 参数可能是 NSString 或 NSAttributedString。
fn input_text(string: &AnyObject) -> String {
    if let Some(text) = string.downcast_ref::<NSString>() {
        return text.to_string();
    }
    if let Some(attributed) = string.downcast_ref::<NSAttributedString>() {
        return attributed.string().to_string();
    }
    String::new()
}

/// 文本光标在 AppKit 屏幕坐标系(主屏左下原点)的矩形:候选窗定位用。
fn caret_screen_rect() -> Option<NSRect> {
    let (caret, text_size, frame_x, frame_y, frame_h, scale) = STATE.with(|slot| {
        let guard = slot.borrow();
        let state = guard.as_ref()?;
        let caret = state.canvas.engine.text_caret()?;
        let text_size = state.canvas.engine.annotation_overlay().text_size as f64;
        Some((
            caret,
            text_size,
            state.frame_x,
            state.frame_y,
            state.frame_h,
            state.scale,
        ))
    })?;
    // 引擎物理像素(左上原点)→ AppKit 全局逻辑坐标(主屏左下原点)。
    let x = frame_x + f64::from(caret.0) / scale;
    let line_h = (text_size / scale).max(1.0);
    let y = frame_y + frame_h - f64::from(caret.1) / scale - line_h;
    Some(NSRect {
        origin: NSPoint { x, y },
        size: NSSize {
            width: 1.0,
            height: line_h,
        },
    })
}

/// 单次合成耗时(仅 Redraw 路径调用;ADR-007 护栏的可观察基线)。
fn compose_canvas(canvas: &mut Canvas) -> Option<Duration> {
    let started = Instant::now();
    let (w, h) = canvas.composer.size();
    let expected = w as usize * h as usize * 4;
    // 壳侧防御:compose_into 要求 out 长度与冻结帧严格一致,越界会 panic;
    // 长度不符时跳过本次合成而非崩溃。
    if canvas.scratch.len() == expected {
        let scene = canvas.engine.scene();
        let overlay = canvas.engine.annotation_overlay();
        canvas
            .composer
            .compose_into_with_overlay(&scene, &overlay, &mut canvas.scratch);
        return Some(started.elapsed());
    }
    None
}

fn should_log_present(first: bool, last: Option<Instant>, now: Instant) -> bool {
    if first {
        return true;
    }
    last.map(|last| now.duration_since(last) >= Duration::from_millis(250))
        .unwrap_or(true)
}

/// RGBA→BGRA 通道交换,输出到呈现缓冲。
#[cfg(test)]
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
        let now = Instant::now();
        if should_log_present(
            state.last_present_log.is_none(),
            state.last_present_log,
            now,
        ) {
            eprintln!(
                "Cropmark overlay {}x{} present: compose={:?}, total={:?}",
                state.canvas.width,
                state.canvas.height,
                compose_at,
                started.elapsed()
            );
            state.last_present_log = Some(now);
        }
    }
}

/// 从 RGBA 合成缓冲构建 CGImage。provider 的 release 回调为空,数据由
/// `canvas.scratch` 保活:该缓冲定容(容量恒等于冻结帧),地址不漂移;
/// 旧 image 在缓冲被下一次 Redraw 覆写前即被替换丢弃。
fn rebuild_image(state: &mut ShellState) {
    let w = state.canvas.width as usize;
    let h = state.canvas.height as usize;
    let buffer = &state.canvas.scratch;
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
    // 内存布局 R,G,B,A:Last + 32Big,免去每帧 RGBA→BGRA swizzle。
    let bitmap_info = CGBitmapInfo(CGImageAlphaInfo::Last.0 | CGImageByteOrderInfo::Order32Big.0);
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
        CGContext::set_interpolation_quality(Some(cg_context), CGInterpolationQuality::None);
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
        // 文本编辑的退格/删除(与 Windows 壳同义)。
        assert_eq!(map_key_code(KC_DELETE), Some(LogicalKey::Delete));
        assert_eq!(map_key_code(KC_FORWARD_DELETE), Some(LogicalKey::Delete));
        assert_eq!(map_key_code(0x0A), None);
    }

    #[test]
    fn shortcut_keys_cover_undo_and_redo() {
        assert_eq!(shortcut_key(KC_ANSI_Z, false), Some(LogicalKey::Undo));
        assert_eq!(shortcut_key(KC_ANSI_Z, true), Some(LogicalKey::Redo));
        assert_eq!(shortcut_key(KC_ANSI_Y, false), Some(LogicalKey::Redo));
        assert_eq!(shortcut_key(KC_ANSI_Y, true), Some(LogicalKey::Redo));
        assert_eq!(shortcut_key(KC_ANSI_C, false), None);
    }

    #[test]
    fn marked_from_input_clears_empty_and_clamps_selection() {
        assert_eq!(marked_from_input(String::new(), NSRange::new(0, 0)), None);
        // 中文 1 个 UTF-16 码元;越界选区钳回长度内。
        let marked = marked_from_input("中".into(), NSRange::new(5, 9)).unwrap();
        assert_eq!(marked.utf16_len(), 1);
        assert_eq!(marked.selected, NSRange::new(1, 0));
        // 组合串内部的合法选区原样保留。
        let marked = marked_from_input("ni".into(), NSRange::new(1, 1)).unwrap();
        assert_eq!(marked.selected, NSRange::new(1, 1));
    }

    #[test]
    fn not_found_range_matches_foundation_convention() {
        let range = not_found_range();
        assert_eq!(range.length, 0);
        assert_eq!(range.location, NSNotFound as usize);
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
            scratch: vec![0; 8], // 故意错误长度:防御路径返回 None 且不 panic。
            present_buf: Vec::new(),
            width: 4,
            height: 4,
        };
        assert!(compose_canvas(&mut canvas).is_none());
    }

    #[test]
    fn present_log_throttles_after_first_frame() {
        let t0 = Instant::now();
        assert!(should_log_present(true, None, t0));
        assert!(!should_log_present(
            false,
            Some(t0),
            t0 + Duration::from_millis(100)
        ));
        assert!(should_log_present(
            false,
            Some(t0),
            t0 + Duration::from_millis(250)
        ));
    }

    #[test]
    fn shell_does_not_call_nested_next_event_pump() {
        // 嵌套 nextEventMatchingMask 在 macOS 27 会把主线程打满。选区必须走
        // NSEvent 本地监听 + NSApp 自己的事件循环。
        let src = include_str!("macos.rs");
        let forbidden = concat!("nextEventMatchingMask", "_untilDate_inMode_dequeue");
        assert!(
            !src.contains(forbidden),
            "do not nest nextEventMatchingMask in the macOS selection shell"
        );
    }

    #[test]
    fn appkit_mouse_location_flips_y_so_bottom_is_not_engine_top() {
        // 与 geometry 回归同构:漏翻转时底部点击会变成引擎 y=0,选区钉在顶边.
        let (x, y) = geometry::appkit_global_to_physical(40.0, 0.0, 0.0, 0.0, 600.0, 2.0);
        assert_eq!((x, y), (80, 1200));
        let (x, y) = geometry::appkit_global_to_physical(40.0, 600.0, 0.0, 0.0, 600.0, 2.0);
        assert_eq!((x, y), (80, 0));
        let (x, y) = geometry::backing_to_engine(80.0, 0.0, 1200.0, 1200.0, 1200.0, 1200.0);
        assert_eq!((x, y), (80, 1200));
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

    fn assert_dual_color_crosshair(sprite: CrosshairSprite) {
        let (cx, cy) = sprite.hotspot();
        let half = sprite.thickness / 2;
        assert_eq!(sprite.size % 2, 1);
        assert_eq!((cx, cy), (sprite.size / 2, sprite.size / 2));
        assert!(sprite.thickness >= 3 && sprite.thickness % 2 == 1);
        // 中心热点像素镂空:热点仍对准指针像素,但该像素透明。
        assert_eq!(sprite.pixel(cx, cy), CrosshairPixel::Empty);
        assert_eq!(sprite.pixel(cx + 1, cy), CrosshairPixel::Core);
        assert_eq!(sprite.pixel(cx, cy + 1), CrosshairPixel::Core);
        assert_eq!(sprite.pixel(cx + half + 1, cy + half), CrosshairPixel::Core);
        assert_eq!(
            sprite.pixel(cx + half + 1, cy + half + 1),
            CrosshairPixel::Outline
        );
        assert_eq!(
            sprite.pixel(cx, cy.saturating_sub(sprite.arm + 1)),
            CrosshairPixel::Outline
        );
        assert_eq!(sprite.pixel(0, 0), CrosshairPixel::Empty);
        let rgba = sprite.rgba();
        let core = (cy * sprite.size + cx + 1) * 4;
        assert_eq!(&rgba[core..core + 4], &[0xf7, 0xf7, 0xf7, 0xff]);
        let outline = ((cy + half + 1) * sprite.size + cx + half + 1) * 4;
        assert_eq!(&rgba[outline..outline + 4], &[0x14, 0x14, 0x14, 0xff]);
    }

    #[test]
    fn dual_color_crosshair_has_light_core_and_dark_outline() {
        assert_dual_color_crosshair(CrosshairSprite::for_scale(1.0));
        assert_dual_color_crosshair(CrosshairSprite::fallback());
    }

    #[test]
    fn crosshair_sprite_scales_arm_with_frame_scale() {
        let base = CrosshairSprite::for_scale(1.0);
        assert_eq!(base.arm, CrosshairSprite::BASE_ARM);
        assert_eq!(base.thickness, 3);
        // 200% 时臂长翻倍;线芯 3*2=6,抬到奇数 7,边长仍为奇数。
        let scaled = CrosshairSprite::for_scale(2.0);
        assert_eq!(scaled.arm, base.arm * 2);
        assert_eq!(scaled.thickness, 7);
        assert_eq!(scaled.size % 2, 1);
        assert_dual_color_crosshair(scaled);
    }

    #[test]
    fn appkit_cursor_queries_reapply_the_current_kind() {
        let src = include_str!("macos.rs");
        assert!(
            src.contains("method(cursorUpdate:)"),
            "AppKit cursorUpdate must reapply the current cursor"
        );
        assert!(
            src.contains("method(resetCursorRects)"),
            "cursor rects must publish the current cursor"
        );
        assert!(
            !src.contains("if state.applied_cursor == Some(kind) {\n        return;"),
            "unchanged cursor kind must still be set"
        );
    }

    #[test]
    fn crosshair_hint_does_not_use_system_crosshair_cursor() {
        let src = include_str!("macos.rs");
        let forbidden = concat!("NSCursor::", "crosshairCursor()");
        assert!(
            !src.contains(forbidden),
            "Crosshair must use the dual-color sprite, not system crosshairCursor"
        );
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
