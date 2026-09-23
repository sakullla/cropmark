//! Windows 区域选区壳:Win32 全屏置顶窗 + 消息泵 + StretchDIBits 呈现。
//!
//! 交互语义全部由平台无关选区引擎(`capture::selection`)决定;本壳只把
//! 鼠标(左/右/移动)与键盘(方向键/Shift/Enter/Esc/C)事件以物理像素坐标
//! 喂给引擎,并把引擎的合成位图 present 到屏幕(ADR-001/006)。右键=菜单、
//! Esc=取消;Enter 确认;操作条/菜单动作经 `RegionOutcome` 交回会话层分发。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, ScreenToClient,
    SetDIBitsToDevice, SetStretchBltMode, StretchDIBits, UpdateWindow, ValidateRect, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, COLORONCOLOR, DIB_RGB_COLORS, RGBQUAD, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
};
use windows::Win32::UI::Input::Ime::{
    ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCompositionWindow, CFS_POINT,
    COMPOSITIONFORM, GCS_COMPSTR, GCS_RESULTSTR, HIMC, IME_COMPOSITION_STRING,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCursor, CreateIconIndirect, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetCursorPos, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindowThreadProcessId, LoadCursorW, PeekMessageW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetCursor, SetForegroundWindow, SetWindowPos, ShowWindow,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, HCURSOR, HWND_TOPMOST, ICONINFO, IDC_ARROW, IDC_HAND,
    IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG, PM_NOREMOVE, PM_REMOVE,
    SM_CXCURSOR,
    SM_CYCURSOR, SWP_SHOWWINDOW, SW_SHOW, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
    WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN,
    WM_SETCURSOR, WM_SETFOCUS, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::annotate::Annotation;
use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{MonitorGeom, PhysicalRect};
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    AnnotationOptions, AnnotationTool, CursorHint, EngineOutcome, FeatureFlags, InputEvent,
    LogicalKey, Scene, SelectionAction, SelectionEngine,
};
use crate::capture::session::QuietAction;

const CLASS: &str = "CropmarkRegionOverlay";
static CLASS_SERIAL: AtomicU32 = AtomicU32::new(1);

/// 当前活动壳窗口句柄(0 = 无)。会话层在 stale 重置时经
/// `request_shell_close` 从任意线程请求关闭,泵退出后旧结果按代际丢弃。
static ACTIVE_SHELL_HWND: AtomicIsize = AtomicIsize::new(0);

// 虚拟键码(直接使用数值,不为修饰键状态引入新的 windows crate feature)。
const VK_BACK: u32 = 0x08;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_C: u32 = 0x43;
const VK_Y: u32 = 0x59;
const VK_Z: u32 = 0x5A;
const VK_DELETE: u32 = 0x2E;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
// 标注工具快捷键(与预览编辑器一致:A/R/E/L/M/B/H/P/N/T)。
const VK_A: u32 = 0x41;
const VK_B: u32 = 0x42;
const VK_E: u32 = 0x45;
const VK_H: u32 = 0x48;
const VK_L: u32 = 0x4C;
const VK_M: u32 = 0x4D;
const VK_N: u32 = 0x4E;
const VK_P: u32 = 0x50;
const VK_R: u32 = 0x52;
const VK_T: u32 = 0x54;

/// 壳的最终结果:会话层据此选择完成路径。R21 起携带即时标注图元
/// (坐标相对冻结帧物理像素,由会话层平移到裁剪坐标系)。
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
    /// 呈现用 BGRA 缓冲(StretchDIBits 32bpp BI_RGB 按 BGR 序读取;
    /// 实测 BI_BITFIELDS 直读 RGBA 的掩码路径 blit 慢一个数量级,不用)。
    present_buf: Vec<u8>,
    width: i32,
    height: i32,
    /// 上一帧场景;脏矩形合成用。首帧为 None。
    last_scene: Option<Scene>,
}

/// 壳侧回调:色值复制等副作用由会话层注入,壳不直接触碰 tauri 运行时
/// (在壳内实例化 AppHandle 方法会把 tauri-runtime-wry 的 dialog 模块链入
/// 测试二进制;其静态引用的 comctl32!TaskDialogIndirect 是 v6-only 导出,
/// 无激活上下文的测试 exe 以 STATUS_ENTRYPOINT_NOT_FOUND 崩溃)。
#[derive(Debug, Clone, Copy)]
pub struct ShellHooks {
    /// 复制色值文本并给出反馈;参数为完整文本与 HEX 简写。
    pub copy_color: fn(text: &str, hex: &str),
}

struct ShellState {
    hooks: ShellHooks,
    canvas: Canvas,
    shift_down: bool,
    /// Ctrl 按下状态:即时标注的撤销/重做快捷键(Ctrl+Z / Ctrl+Y)判定。
    ctrl_down: bool,
    outcome: Option<RegionOutcome>,
    timing: bool,
    /// 冻结帧 DPI 缩放(frame.scale):十字光标按它 keyed 重建。
    scale: f64,
    /// 是否曾真正获得焦点:仅"获得过焦点后又失去"才触发失焦取消,
    /// 防止建窗时 SetForegroundWindow 被前台锁拒绝/焦点弹跳导致的
    /// 建窗即 KILLFOCUS 误取消(用户表现为"触发后毫无反应")。
    had_focus: bool,
    created_at: Instant,
}

thread_local! {
    static STATE: RefCell<Option<ShellState>> = const { RefCell::new(None) };
}

/// 驱动一次区域选区:创建全屏置顶窗并泵消息直到引擎给出终态。
pub fn pick_region(
    frame: &Frame,
    monitor: &MonitorGeom,
    flags: FeatureFlags,
    annotation_options: AnnotationOptions,
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    run_shell(frame, monitor, flags, annotation_options, hooks)
}

/// 请求关闭当前选区壳(线程安全,可从任意线程调用):wm_close 走默认窗口
/// 过程销毁窗口并退出消息泵。无活动壳时为 no-op;壳返回的结果由会话层
/// 代际校验丢弃(ADR-16)。
pub fn request_shell_close() {
    let value = ACTIVE_SHELL_HWND.load(Ordering::SeqCst);
    if value == 0 {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            Some(HWND(value as *mut c_void)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

pub fn shell_is_active() -> bool {
    ACTIVE_SHELL_HWND.load(Ordering::SeqCst) != 0
}

fn run_shell(
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
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(ShellState {
            hooks,
            canvas: Canvas {
                // 注入冻结帧 DPI 缩放:chrome(放大镜面板)光标命中需要。
                engine: SelectionEngine::new(width as u32, height as u32, flags)
                    .with_scale(frame.scale)
                    .with_annotation_options(annotation_options),
                composer,
                scratch: vec![0; bytes],
                present_buf: vec![0; bytes],
                width,
                height,
                last_scene: None,
            },
            shift_down: false,
            ctrl_down: false,
            outcome: None,
            timing: std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some(),
            scale: frame.scale,
            had_focus: false,
            created_at: Instant::now(),
        });
    });
    let hwnd = unsafe {
        create_overlay_window(monitor.physical_x, monitor.physical_y, width, height, frame.scale)?
    };
    unsafe {
        log_present_diagnostics(
            hwnd,
            (frame_w, frame_h),
            (monitor.physical_width, monitor.physical_height),
            frame.scale as f32,
        );
    }
    ACTIVE_SHELL_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    unsafe {
        drain_thread_quit();
        pump();
        let _ = DestroyWindow(hwnd);
        drain_thread_quit();
    }
    ACTIVE_SHELL_HWND.store(0, Ordering::SeqCst);
    let state = STATE.with(|slot| slot.borrow_mut().take());
    Ok(state
        .and_then(|state| state.outcome)
        .unwrap_or(RegionOutcome::Cancelled))
}

/// 把一次输入事件交给引擎并处理其输出;返回 true 表示会话已到终态。
fn feed_event(state: &mut ShellState, event: InputEvent, hwnd: HWND) -> bool {
    let outcome = state.canvas.engine.handle_event(event);
    match outcome {
        EngineOutcome::Redraw => {
            present(state.timing, hwnd, &mut state.canvas);
            false
        }
        EngineOutcome::Confirmed(rect) => {
            let annotations = state.canvas.engine.annotations().to_vec();
            state.outcome = Some(RegionOutcome::Preview(rect, annotations));
            true
        }
        EngineOutcome::Cancelled => {
            state.outcome = Some(RegionOutcome::Cancelled);
            true
        }
        EngineOutcome::Action(action) => match action {
            SelectionAction::Annotate => {
                if let Some(outcome) = annotate_outcome(&state.canvas.engine) {
                    state.outcome = Some(outcome);
                    return true;
                }
                false
            }
            SelectionAction::Cancel => {
                // 引擎在菜单路径已把「取消」译为 Cancelled;此支仅为防御。
                state.outcome = Some(RegionOutcome::Cancelled);
                true
            }
            SelectionAction::CopyColor => {
                copy_color_value(state);
                false
            }
            // 标注工具条动作(工具切换/撤销/重做/删除/更多)在引擎内消费,
            // 不会到达这里;防御性忽略,不结束会话。
            SelectionAction::Tool(_)
            | SelectionAction::Undo
            | SelectionAction::Redo
            | SelectionAction::Delete
            | SelectionAction::More => false,
            quiet => {
                if let (Some(rect), Some(action)) =
                    (state.canvas.engine.selection(), quiet_action_for(quiet))
                {
                    let annotations = state.canvas.engine.annotations().to_vec();
                    state.outcome = Some(RegionOutcome::Quiet(rect, action, annotations));
                    return true;
                }
                false
            }
        },
    }
}

/// 失焦兜底:Esc/Enter 只会送到前台窗口,overlay 失焦后键盘输入永远丢失,
/// 不取消会让消息泵永挂、会话 busy 卡死(Snipaste 惯例:失焦即取消)。
/// toast 反馈窗是 focused(false),不会触发本路径。返回 true 表示应退出泵。
fn cancel_on_focus_loss(state: &mut ShellState) -> bool {
    if state.outcome.is_some() {
        return false;
    }
    state.outcome = Some(RegionOutcome::Cancelled);
    true
}

fn foreground_belongs_to_self() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

/// 「标注」动作到壳结果的映射:引擎尚无选区时返回 None,会话继续等待。
fn annotate_outcome(engine: &SelectionEngine) -> Option<RegionOutcome> {
    engine
        .selection()
        .map(|rect| RegionOutcome::Annotate(rect, engine.annotations().to_vec()))
}

/// 操作条/菜单动作到静默完成动作的映射;标注/取消/复制色值/标注工具条
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
}

impl CrosshairSprite {
    /// 1x 基准臂长(物理像素);实际臂长按 frame.scale 缩放。
    const BASE_ARM: usize = 10;

    /// 最小双色位图(光标创建失败时的兜底规格)。
    const fn fallback() -> Self {
        Self { size: 9, arm: 3 }
    }

    /// 按冻结帧 DPI 缩放生成规格:臂长随 frame.scale 缩放(200% 时
    /// 翻倍),线芯恒 1 物理像素;边长保持奇数,热点在交叉点。
    fn for_scale(scale: f64) -> Self {
        let arm = ((Self::BASE_ARM as f64) * scale.max(0.1)).round().max(3.0) as usize;
        Self {
            size: arm * 2 + 5,
            arm,
        }
    }

    fn hotspot(self) -> (usize, usize) {
        let center = self.size / 2;
        (center, center)
    }

    /// 线芯:横竖臂上距中心 1..=arm 的像素。中心热点像素镂空,
    /// 指针像素经镂空处直接可见;芯宽恒 1 物理像素。
    fn is_core(self, x: usize, y: usize) -> bool {
        let center = self.size / 2;
        let dist = if x == center {
            y.abs_diff(center)
        } else if y == center {
            x.abs_diff(center)
        } else {
            return false;
        };
        (1..=self.arm).contains(&dist)
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

/// 引擎光标提示 → Win32 系统光标资源。Crosshair 走运行时双色十字,
/// 不映射 IDC_CROSS(ADR-1)。
fn cursor_resource(hint: CursorHint) -> Option<PCWSTR> {
    match hint {
        CursorHint::Move => Some(IDC_SIZEALL),
        CursorHint::ResizeNS => Some(IDC_SIZENS),
        CursorHint::ResizeEW => Some(IDC_SIZEWE),
        CursorHint::ResizeNWSE => Some(IDC_SIZENWSE),
        CursorHint::ResizeNESW => Some(IDC_SIZENESW),
        CursorHint::Pointer => Some(IDC_HAND),
        CursorHint::Arrow => Some(IDC_ARROW),
        CursorHint::Crosshair => None,
    }
}

/// Crosshair 提示(及窗口类默认光标)使用的双色十字;按 frame.scale
/// keyed 缓存,缩放变化时重建而非进程级单例。创建失败时仍回退到
/// 更小的双色位图/单色 AND-XOR 十字,绝不 LoadCursorW(IDC_CROSS)。
fn crosshair_cursor(scale: f64) -> HCURSOR {
    static HANDLES: OnceLock<Mutex<HashMap<u64, isize>>> = OnceLock::new();
    let cache = HANDLES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cache = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let value = *cache
        .entry(scale.to_bits())
        .or_insert_with(|| create_dual_crosshair_cursor(scale).0 as isize);
    HCURSOR(value as *mut c_void)
}

fn cursor_handle(hint: CursorHint, scale: f64) -> Option<HCURSOR> {
    match cursor_resource(hint) {
        Some(resource) => unsafe { LoadCursorW(None, resource).ok() },
        None => {
            let cursor = crosshair_cursor(scale);
            (!cursor.is_invalid()).then_some(cursor)
        }
    }
}

fn create_dual_crosshair_cursor(scale: f64) -> HCURSOR {
    unsafe {
        create_color_cursor(CrosshairSprite::for_scale(scale))
            .or_else(|| create_color_cursor(CrosshairSprite::fallback()))
            .or_else(|| create_mono_cursor(CrosshairSprite::for_scale(scale)))
            .or_else(|| create_mono_cursor(CrosshairSprite::fallback()))
            .unwrap_or_default()
    }
}

/// 32bpp ARGB 彩色光标(浅色芯 + 深色描边,透明底)。
unsafe fn create_color_cursor(sprite: CrosshairSprite) -> Option<HCURSOR> {
    let size = sprite.size as i32;
    let rgba = sprite.rgba();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(color.into());
        return None;
    }
    {
        let dest = std::slice::from_raw_parts_mut(bits.cast::<u8>(), sprite.size * sprite.size * 4);
        for (src, dst) in rgba.chunks_exact(4).zip(dest.chunks_exact_mut(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
    }
    let stride = sprite.size.div_ceil(16) * 2;
    let and_bits = vec![0xffu8; stride * sprite.size];
    let mask = CreateBitmap(size, size, 1, 1, Some(and_bits.as_ptr().cast::<c_void>()));
    if mask.is_invalid() {
        let _ = DeleteObject(color.into());
        return None;
    }
    let (hot_x, hot_y) = sprite.hotspot();
    let icon_info = ICONINFO {
        fIcon: false.into(),
        xHotspot: hot_x as u32,
        yHotspot: hot_y as u32,
        hbmMask: mask,
        hbmColor: color,
    };
    let icon = CreateIconIndirect(&icon_info);
    let _ = DeleteObject(color.into());
    let _ = DeleteObject(mask.into());
    icon.ok().map(|handle| HCURSOR(handle.0))
}

/// 彩色光标失败时的最小双色回退:AND/XOR 平面(白芯黑边)。
unsafe fn create_mono_cursor(sprite: CrosshairSprite) -> Option<HCURSOR> {
    let cx = GetSystemMetrics(SM_CXCURSOR).max(sprite.size as i32);
    let cy = GetSystemMetrics(SM_CYCURSOR).max(sprite.size as i32);
    if cx <= 0 || cy <= 0 {
        return None;
    }
    let width = cx as usize;
    let height = cy as usize;
    let stride = width.div_ceil(16) * 2;
    let mut and_plane = vec![0xffu8; stride * height];
    let mut xor_plane = vec![0u8; stride * height];
    let (hot_x, hot_y) = sprite.hotspot();
    let origin_x = width.saturating_sub(sprite.size) / 2;
    let origin_y = height.saturating_sub(sprite.size) / 2;
    for y in 0..sprite.size {
        for x in 0..sprite.size {
            let px = origin_x + x;
            let py = origin_y + y;
            let bit = 7 - (px % 8);
            let index = py * stride + px / 8;
            match sprite.pixel(x, y) {
                CrosshairPixel::Empty => {}
                // 浅芯:AND 清零 + XOR 置位 → 白色。
                CrosshairPixel::Core => {
                    and_plane[index] &= !(1 << bit);
                    xor_plane[index] |= 1 << bit;
                }
                // 深描边:仅 AND 清零 → 黑色。
                CrosshairPixel::Outline => {
                    and_plane[index] &= !(1 << bit);
                }
            }
        }
    }
    CreateCursor(
        None,
        (origin_x + hot_x) as i32,
        (origin_y + hot_y) as i32,
        cx,
        cy,
        and_plane.as_ptr().cast::<c_void>(),
        xor_plane.as_ptr().cast::<c_void>(),
    )
    .ok()
}

fn map_virtual_key(vk: u32) -> Option<LogicalKey> {
    match vk {
        VK_RETURN => Some(LogicalKey::Enter),
        VK_ESCAPE => Some(LogicalKey::Escape),
        VK_LEFT => Some(LogicalKey::ArrowLeft),
        VK_UP => Some(LogicalKey::ArrowUp),
        VK_RIGHT => Some(LogicalKey::ArrowRight),
        VK_DOWN => Some(LogicalKey::ArrowDown),
        VK_C => Some(LogicalKey::CopyColor),
        // 文本编辑的退格/删除(非编辑态下引擎忽略)。
        VK_BACK | VK_DELETE => Some(LogicalKey::Delete),
        // R21 修订:工具快捷键(A/R/E/L/M/B/H/P/N/T)进入标注模式并选工具;
        // 非选中态/关闭即时标注时由引擎忽略。
        VK_R => Some(LogicalKey::Tool(AnnotationTool::Rect)),
        VK_E => Some(LogicalKey::Tool(AnnotationTool::Ellipse)),
        VK_L => Some(LogicalKey::Tool(AnnotationTool::Line)),
        VK_A => Some(LogicalKey::Tool(AnnotationTool::Arrow)),
        VK_N => Some(LogicalKey::Tool(AnnotationTool::Number)),
        VK_T => Some(LogicalKey::Tool(AnnotationTool::Text)),
        VK_P => Some(LogicalKey::Tool(AnnotationTool::Pen)),
        VK_H => Some(LogicalKey::Tool(AnnotationTool::Highlighter)),
        VK_M => Some(LogicalKey::Tool(AnnotationTool::Mosaic)),
        VK_B => Some(LogicalKey::Tool(AnnotationTool::Blur)),
        _ => None,
    }
}

/// 单次合成耗时(仅 Redraw 路径调用;ADR-007 护栏的可观察基线)。
fn compose_canvas(canvas: &mut Canvas) -> Option<(Duration, composer::IntRect)> {
    let started = Instant::now();
    let (w, h) = canvas.composer.size();
    let expected = w as usize * h as usize * 4;
    // 壳侧防御:compose_* 要求 out 长度与冻结帧严格一致,越界会 panic;
    // 长度不符时跳过本次合成而非崩溃。
    if canvas.scratch.len() == expected && canvas.present_buf.len() == expected {
        let scene = canvas.engine.scene();
        let overlay = canvas.engine.annotation_overlay();
        let dirty = canvas.composer.compose_into_dirty(
            &scene,
            &overlay,
            &mut canvas.scratch,
            canvas.last_scene.as_ref(),
        );
        swizzle_rect(
            &canvas.scratch,
            &mut canvas.present_buf,
            w as i32,
            h as i32,
            dirty,
        );
        canvas.last_scene = Some(scene);
        return Some((started.elapsed(), dirty));
    }
    None
}

/// RGBA→BGRA 通道交换,输出到呈现缓冲。
#[cfg(test)]
fn swizzle_rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    let pixels = (src.len().min(dst.len()) / 4) as i32;
    swizzle_rect(
        src,
        dst,
        pixels.max(1),
        1,
        composer::IntRect {
            x: 0,
            y: 0,
            width: pixels.max(1),
            height: 1,
        },
    );
}

/// 只交换脏矩形内的 R/B,避免每次鼠标移动扫完整屏。
fn swizzle_rect(src: &[u8], dst: &mut [u8], width: i32, height: i32, rect: composer::IntRect) {
    if width <= 0 || height <= 0 || rect.is_empty() {
        return;
    }
    let stride = width as usize * 4;
    let y0 = rect.y.max(0) as usize;
    let y1 = rect.bottom().min(height) as usize;
    let x0 = rect.x.max(0) as usize;
    let x1 = rect.right().min(width) as usize;
    if y0 >= y1 || x0 >= x1 {
        return;
    }
    for y in y0..y1 {
        let row = y * stride;
        let start = row + x0 * 4;
        let end = row + x1 * 4;
        if end > src.len() || end > dst.len() {
            break;
        }
        for (s, d) in src[start..end]
            .chunks_exact(4)
            .zip(dst[start..end].chunks_exact_mut(4))
        {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
}

/// 仅在引擎要求 Redraw 时重合成;WM_PAINT 直接呈现缓存位图(ADR-007)。
fn present(timing: bool, hwnd: HWND, canvas: &mut Canvas) {
    let started = if timing { Some(Instant::now()) } else { None };
    let composed = compose_canvas(canvas);
    if let Some((_, dirty)) = composed {
        unsafe { blit_dirty(hwnd, canvas.width, canvas.height, &canvas.present_buf, dirty) };
    }
    if let Some(started) = started {
        eprintln!(
            "Cropmark overlay {}x{} present: compose+swizzle={:?}, total={:?}",
            canvas.width,
            canvas.height,
            composed.map(|(elapsed, _)| elapsed),
            started.elapsed()
        );
    }
}

/// 呈现路径选择:客户区与冻结帧尺寸一致时 1:1 直拷(无插值/缩放),
/// 不一致(DPI 虚拟化或抓屏分辨率不同)才走按比例拉伸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlitPlan {
    /// 1:1 `SetDIBitsToDevice`。
    Exact,
    /// `StretchDIBits`(显式 COLORONCOLOR)。
    Scaled,
}

fn blit_plan(client: (i32, i32), source: (i32, i32)) -> BlitPlan {
    if client.0 == source.0 && client.1 == source.1 {
        BlitPlan::Exact
    } else {
        BlitPlan::Scaled
    }
}

/// 呈现一帧合成位图(R21 呈现诊断):
/// - 客户区与冻结帧尺寸一致(per-monitor v2 下恒等)时走 `SetDIBitsToDevice`
///   1:1 直拷,不经过任何拉伸/插值,选区画面与冻结帧物理像素一一对应;
/// - 尺寸不一致(DPI 虚拟化或抓屏分辨率不同)时才按比例 `StretchDIBits`,
///   并显式设置 `COLORONCOLOR`,避免默认 BLACKONWHITE 拉伸造成的模糊/色深减半。
unsafe fn blit(hwnd: HWND, width: i32, height: i32, bgra: &[u8]) {
    // 集成测试以空 hwnd 驱动 feed_event:跳过真实 blit。
    if hwnd.0.is_null() {
        return;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let hdc = GetDC(Some(hwnd));
    if hdc.0.is_null() {
        return;
    }
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let dest_w = (client.right - client.left).max(1);
    let dest_h = (client.bottom - client.top).max(1);
    match blit_plan((dest_w, dest_h), (width, height)) {
        BlitPlan::Exact => {
            let _ = SetDIBitsToDevice(
                hdc,
                0,
                0,
                width as u32,
                height as u32,
                0,
                0,
                0,
                height as u32,
                bgra.as_ptr().cast::<c_void>(),
                &info,
                DIB_RGB_COLORS,
            );
        }
        BlitPlan::Scaled => {
            let _ = SetStretchBltMode(hdc, COLORONCOLOR);
            let _ = StretchDIBits(
                hdc,
                0,
                0,
                dest_w,
                dest_h,
                0,
                0,
                width,
                height,
                Some(bgra.as_ptr().cast::<c_void>()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    let _ = ReleaseDC(Some(hwnd), hdc);
}

/// 只把脏行带送到窗口。客户区与帧不一致时退回整帧 blit。
unsafe fn blit_dirty(
    hwnd: HWND,
    width: i32,
    height: i32,
    bgra: &[u8],
    dirty: composer::IntRect,
) {
    if hwnd.0.is_null() {
        return;
    }
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let dest_w = (client.right - client.left).max(1);
    let dest_h = (client.bottom - client.top).max(1);
    if blit_plan((dest_w, dest_h), (width, height)) != BlitPlan::Exact
        || dirty.is_empty()
        || (dirty.x <= 0 && dirty.y <= 0 && dirty.right() >= width && dirty.bottom() >= height)
    {
        blit(hwnd, width, height, bgra);
        return;
    }
    let y = dirty.y.clamp(0, height.max(0));
    let band_h = (dirty.bottom().min(height) - y).max(0);
    if band_h <= 0 {
        return;
    }
    let stride = width as usize * 4;
    let offset = y as usize * stride;
    if offset >= bgra.len() {
        return;
    }
    let hdc = GetDC(Some(hwnd));
    if hdc.0.is_null() {
        return;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -band_h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let _ = SetDIBitsToDevice(
        hdc,
        0,
        y,
        width as u32,
        band_h as u32,
        0,
        0,
        0,
        band_h as u32,
        bgra[offset..].as_ptr().cast::<c_void>(),
        &info,
        DIB_RGB_COLORS,
    );
    let _ = ReleaseDC(Some(hwnd), hdc);
}

/// R21 呈现诊断(仅 `CROPMARK_CAPTURE_TIMING` 门控):一行记录冻结帧、
/// 显示器物理尺寸、窗口客户区尺寸、冻结帧 scale 与进程 DPI 感知上下文。
/// 三者一致即为 1:1 无插值;客户区不一致时先定位抓屏分辨率还是窗口虚拟化。
unsafe fn log_present_diagnostics(hwnd: HWND, frame: (u32, u32), monitor: (u32, u32), scale: f32) {
    if std::env::var_os("CROPMARK_CAPTURE_TIMING").is_none() {
        return;
    }
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let awareness = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext());
    eprintln!(
        "Cropmark overlay present: frame={}x{} monitor={}x{} client={}x{} scale={scale:.3} awareness={awareness:?}",
        frame.0,
        frame.1,
        monitor.0,
        monitor.1,
        (client.right - client.left).max(0),
        (client.bottom - client.top).max(0),
    );
}

unsafe fn create_overlay_window(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    scale: f64,
) -> Result<HWND, CaptureError> {
    let instance =
        GetModuleHandleW(None).map_err(|_| CaptureError::api("error.capture.window_create"))?;
    let serial = CLASS_SERIAL.fetch_add(1, Ordering::Relaxed);
    let class_name: Vec<u16> = format!("{CLASS}{serial}\0").encode_utf16().collect();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.into(),
        hCursor: crosshair_cursor(scale),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    if RegisterClassExW(&class) == 0 {
        return Err(CaptureError::api("error.capture.window_register"));
    }
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        x,
        y,
        w,
        h,
        None,
        None,
        Some(instance.into()),
        None,
    )
    .map_err(|_| CaptureError::api("error.capture.window_open"))?;
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_SHOWWINDOW);
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    let _ = UpdateWindow(hwnd);
    Ok(hwnd)
}

fn client_point(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam.0 as u32;
    let x = packed as i16 as i32;
    let y = (packed >> 16) as i16 as i32;
    (x, y)
}

/// 客户区坐标 → 引擎物理像素。客户区与冻结帧尺寸不一致时(DPI 拉伸)
/// 按比例映射,否则选框会相对光标越拖越偏。
fn map_client_to_engine(x: i32, y: i32, client: RECT, engine_w: i32, engine_h: i32) -> (i32, i32) {
    let cw = (client.right - client.left).max(1) as i64;
    let ch = (client.bottom - client.top).max(1) as i64;
    let ew = engine_w.max(1) as i64;
    let eh = engine_h.max(1) as i64;
    if cw == ew && ch == eh {
        return (x, y);
    }
    ((x as i64 * ew / cw) as i32, (y as i64 * eh / ch) as i32)
}

/// 优先 GetCursorPos + ScreenToClient(物理像素、捕获后窗外仍准);
/// 失败再退回 lParam 的 16 位客户区坐标。
unsafe fn engine_point(hwnd: HWND, lparam: LPARAM, engine_w: i32, engine_h: i32) -> (i32, i32) {
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let (x, y) = {
        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() && ScreenToClient(hwnd, &mut point).as_bool() {
            (point.x, point.y)
        } else {
            client_point(lparam)
        }
    };
    map_client_to_engine(x, y, client, engine_w, engine_h)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_KILLFOCUS => {
            // 失焦即取消:键盘 Esc/Enter 只送前台窗口,失焦后继续泵只会永挂。
            // 但仅"曾获得焦点后又失去"才取消——建窗期 SetForegroundWindow 可能被
            // 前台锁拒绝,焦点弹跳产生的 KILLFOCUS 不得误杀会话(另留 500ms 宽限)。
            // 焦点落到本进程其它窗(toast/贴图/预览)时不取消,否则点贴图会
            // 拆掉覆盖层,鼠标尚未松开的点击穿透到下方置顶应用并把它关掉。
            if foreground_belongs_to_self() {
                return LRESULT(0);
            }
            let done = STATE.with(|slot| {
                let mut guard = slot.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                if !state.had_focus || state.created_at.elapsed() < Duration::from_millis(500) {
                    return false;
                }
                cancel_on_focus_loss(state)
            });
            if done {
                let _ = ReleaseCapture();
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.had_focus = true;
                }
            });
            LRESULT(0)
        }
        WM_PAINT => {
            // 直接呈现缓存位图,不重合成(合成只发生在引擎 Redraw 时)。
            STATE.with(|slot| {
                if let Some(state) = slot.borrow().as_ref() {
                    blit(
                        hwnd,
                        state.canvas.width,
                        state.canvas.height,
                        &state.canvas.present_buf,
                    );
                }
            });
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let _ = ValidateRect(Some(hwnd), Some(&rect));
            LRESULT(0)
        }
        WM_SETCURSOR => {
            // 引擎光标提示(chrome 优先):菜单项/图标轨按钮→手型,放大镜面板→
            // 箭头,手柄/边→resize 箭头,选区内部→移动,其他→双色十字(ADR-1)。
            let (hint, scale) = STATE
                .with(|slot| {
                    slot.borrow().as_ref().map(|state| {
                        let (x, y) = state.canvas.engine.cursor();
                        (state.canvas.engine.cursor_for(x, y), state.scale)
                    })
                })
                .unwrap_or((CursorHint::Crosshair, 1.0));
            if let Some(cursor) = cursor_handle(hint, scale) {
                let _ = SetCursor(Some(cursor));
            }
            LRESULT(1)
        }
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN => {
            if msg == WM_LBUTTONDOWN {
                let _ = SetCapture(hwnd);
            }
            if msg == WM_LBUTTONUP {
                let _ = ReleaseCapture();
            }
            let (engine_w, engine_h) = STATE.with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|state| (state.canvas.width, state.canvas.height))
                    .unwrap_or((1, 1))
            });
            let (x, y) = engine_point(hwnd, lparam, engine_w, engine_h);
            let event = match msg {
                WM_MOUSEMOVE => InputEvent::PointerMove { x, y },
                WM_LBUTTONDOWN => InputEvent::LeftDown { x, y },
                WM_LBUTTONUP => InputEvent::LeftUp { x, y },
                _ => InputEvent::RightDown { x, y },
            };
            let done = STATE.with(|slot| {
                let mut guard = slot.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                feed_event(state, event, hwnd)
            });
            if done {
                let _ = ReleaseCapture();
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_KEYUP => {
            let vk = wparam.0 as u32;
            let done = STATE.with(|slot| {
                let mut guard = slot.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                if vk == VK_SHIFT {
                    state.shift_down = msg == WM_KEYDOWN;
                    return false;
                }
                if vk == VK_CONTROL {
                    state.ctrl_down = msg == WM_KEYDOWN;
                    return false;
                }
                if msg == WM_KEYDOWN {
                    let shift = state.shift_down;
                    // 即时标注快捷键:Ctrl+Z 撤销、Ctrl+Y / Ctrl+Shift+Z 重做
                    // (非编辑/非标注态由引擎忽略)。
                    if state.ctrl_down {
                        let redo = shift;
                        match vk {
                            VK_Z => {
                                let key = if redo {
                                    LogicalKey::Redo
                                } else {
                                    LogicalKey::Undo
                                };
                                return feed_event(state, InputEvent::Key { key, shift }, hwnd);
                            }
                            VK_Y => {
                                return feed_event(
                                    state,
                                    InputEvent::Key {
                                        key: LogicalKey::Redo,
                                        shift,
                                    },
                                    hwnd,
                                );
                            }
                            _ => {}
                        }
                    }
                    if let Some(key) = map_virtual_key(vk) {
                        // 工具快捷键仅在无 Ctrl 时生效(Ctrl+Z/Y 已在上方处理;
                        // 其余 Ctrl 组合不切换标注工具)。
                        if state.ctrl_down && matches!(key, LogicalKey::Tool(_)) {
                            return false;
                        }
                        return feed_event(state, InputEvent::Key { key, shift }, hwnd);
                    }
                }
                false
            });
            if done {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // 文本输入:WM_CHAR 直入(IME 关闭/英文模式);控制字符(退格/回车/
        // 制表)由 WM_KEYDOWN 分支处理,这里丢弃避免重复。
        WM_CHAR => {
            if let Some(ch) = char::from_u32(wparam.0 as u32) {
                if !ch.is_control() {
                    let done = STATE.with(|slot| {
                        let mut guard = slot.borrow_mut();
                        let Some(state) = guard.as_mut() else {
                            return false;
                        };
                        feed_event(state, InputEvent::Text(ch.to_string()), hwnd)
                    });
                    if done {
                        PostQuitMessage(0);
                    }
                }
            }
            LRESULT(0)
        }
        // IME:组合串更新/提交结果都送给引擎;候选窗定位在组合开始时按文本
        // 光标设置(基础路径,IME 不可用时 WM_CHAR 仍然工作)。
        WM_IME_STARTCOMPOSITION => {
            unsafe { position_composition_window(hwnd) };
            LRESULT(0)
        }
        WM_IME_COMPOSITION => {
            let flags = lparam.0 as u32;
            let mut events: Vec<InputEvent> = Vec::new();
            unsafe {
                let himc = ImmGetContext(hwnd);
                if !himc.0.is_null() {
                    if flags & GCS_RESULTSTR.0 != 0 {
                        if let Some(text) = read_ime_string(himc, GCS_RESULTSTR) {
                            events.push(InputEvent::Text(text));
                        }
                    }
                    if flags & GCS_COMPSTR.0 != 0 {
                        let text = read_ime_string(himc, GCS_COMPSTR).unwrap_or_default();
                        events.push(InputEvent::Composition(text));
                    }
                    let _ = ImmReleaseContext(hwnd, himc);
                }
            }
            let mut done = false;
            for event in events {
                done = STATE.with(|slot| {
                    let mut guard = slot.borrow_mut();
                    let Some(state) = guard.as_mut() else {
                        return false;
                    };
                    feed_event(state, event, hwnd)
                }) || done;
            }
            if done {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_IME_ENDCOMPOSITION => {
            let done = STATE.with(|slot| {
                let mut guard = slot.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                feed_event(state, InputEvent::Composition(String::new()), hwnd)
            });
            if done {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            ACTIVE_SHELL_HWND.store(0, Ordering::SeqCst);
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 读取 IME 组合串/结果串(UTF-16 → String);空串返回 None。
unsafe fn read_ime_string(himc: HIMC, kind: IME_COMPOSITION_STRING) -> Option<String> {
    let bytes = ImmGetCompositionStringW(himc, kind, None, 0);
    if bytes <= 0 {
        return None;
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2)];
    let written =
        ImmGetCompositionStringW(himc, kind, Some(buffer.as_mut_ptr().cast()), bytes as u32);
    if written <= 0 {
        return None;
    }
    buffer.truncate(written as usize / 2);
    String::from_utf16(&buffer)
        .ok()
        .filter(|text| !text.is_empty())
}

/// 把 IME 候选窗定位到文本光标处(客户区坐标);无编辑会话时为 no-op。
unsafe fn position_composition_window(hwnd: HWND) {
    let target = STATE.with(|slot| {
        let guard = slot.borrow();
        let state = guard.as_ref()?;
        let caret = state.canvas.engine.text_caret()?;
        Some((caret, state.canvas.width, state.canvas.height))
    });
    let Some(((x, y), engine_w, engine_h)) = target else {
        return;
    };
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let (cx, cy) = map_engine_to_client(x, y, client, engine_w, engine_h);
    let himc = ImmGetContext(hwnd);
    if himc.0.is_null() {
        return;
    }
    let form = COMPOSITIONFORM {
        dwStyle: CFS_POINT,
        ptCurrentPos: POINT { x: cx, y: cy },
        rcArea: RECT::default(),
    };
    let _ = ImmSetCompositionWindow(himc, &form);
    let _ = ImmReleaseContext(hwnd, himc);
}

/// 引擎物理像素 → 客户区坐标(DPI 拉伸下与 `map_client_to_engine` 互逆)。
fn map_engine_to_client(x: i32, y: i32, client: RECT, engine_w: i32, engine_h: i32) -> (i32, i32) {
    let cw = (client.right - client.left).max(1) as i64;
    let ch = (client.bottom - client.top).max(1) as i64;
    let ew = engine_w.max(1) as i64;
    let eh = engine_h.max(1) as i64;
    if cw == ew && ch == eh {
        return (x, y);
    }
    (
        (i64::from(x) * cw / ew) as i32,
        (i64::from(y) * ch / eh) as i32,
    )
}

unsafe fn pump() {
    let mut msg = MSG::default();
    loop {
        // GetMessageW 出错返回 -1(BOOL 非零),.as_bool() 会误判为真而死循环;
        // 只有严格大于 0 才是普通消息,0(WM_QUIT)与负数都退出。
        let result = GetMessageW(&mut msg, None, 0, 0);
        if result.0 <= 0 {
            break;
        }
        coalesce_mouse_move(&mut msg);
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

/// 合并队列里连续的 WM_MOUSEMOVE,只处理最后一次坐标,避免拖选时每条移动
/// 都同步全屏合成。遇到其它消息立即停,不越过按下/抬起。
unsafe fn coalesce_mouse_move(msg: &mut MSG) {
    if msg.message != WM_MOUSEMOVE {
        return;
    }
    let hwnd = msg.hwnd;
    let mut next = MSG::default();
    loop {
        if !PeekMessageW(&mut next, None, 0, 0, PM_NOREMOVE).as_bool() {
            break;
        }
        if next.message != WM_MOUSEMOVE || next.hwnd != hwnd {
            break;
        }
        if !PeekMessageW(&mut next, None, 0, 0, PM_REMOVE).as_bool() {
            break;
        }
        *msg = next;
    }
}

/// 阻塞线程池会复用线程。Esc 时 feed_event 和 WM_DESTROY 各 PostQuitMessage
/// 一次,第二次 WM_QUIT 留在队列里,下一次 pick_region 的 pump 会立刻退出,
/// 选区窗闪一下就没了。抽干残留 WM_QUIT。
unsafe fn drain_thread_quit() {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, WM_QUIT, WM_QUIT, PM_REMOVE).as_bool() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};
    use crate::capture::selection::AnnotationTool;
    use windows::Win32::UI::WindowsAndMessaging::IDC_CROSS;

    /// 合成壳状态:真实引擎+合成器,空 hwnd(blit 守卫跳过真实呈现)。
    /// wnd_proc 只做消息→InputEvent 映射,集成测试直接驱动 feed_event 即
    /// 等价覆盖「消息序列 → 终态/退出」链路。
    /// 默认关闭即时标注,避免工具条覆盖既有选区交互探针;
    /// 标注链路使用 [`test_state_with_flags`]。
    fn test_state(width: u32, height: u32) -> ShellState {
        test_state_with_flags(
            width,
            height,
            FeatureFlags {
                inline_annotation: false,
                ..FeatureFlags::default()
            },
        )
    }

    fn test_state_with_flags(width: u32, height: u32, flags: FeatureFlags) -> ShellState {
        fn noop_copy_color(_text: &str, _hex: &str) {}
        let frame = accept_buffer(RawBuffer::ready(
            width,
            height,
            vec![60u8; (width * height * 4) as usize],
        ))
        .unwrap();
        let bytes = frame.rgba.len();
        ShellState {
            hooks: ShellHooks {
                copy_color: noop_copy_color,
            },
            canvas: Canvas {
                engine: SelectionEngine::new(width, height, flags)
                    .with_scale(frame.scale)
                    .with_annotation_options(AnnotationOptions {
                        text_input: true,
                        ..AnnotationOptions::default()
                    }),
                composer: Composer::new(&frame).unwrap(),
                scratch: vec![0; bytes],
                present_buf: vec![0; bytes],
                width: width as i32,
                height: height as i32,
                last_scene: None,
            },
            shift_down: false,
            ctrl_down: false,
            outcome: None,
            timing: false,
            scale: frame.scale,
            had_focus: true,
            created_at: Instant::now() - Duration::from_secs(1),
        }
    }

    fn key(key: LogicalKey) -> InputEvent {
        InputEvent::Key { key, shift: false }
    }

    #[test]
    fn zero_drag_then_escape_still_terminates() {
        let mut state = test_state(320, 200);
        let hwnd = HWND::default();
        // down→up 零位移:微选区被丢弃,会话继续(不终态)。
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: 50, y: 50 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftUp { x: 50, y: 50 },
            hwnd
        ));
        assert!(state.outcome.is_none());
        // Esc 兜底退出。
        assert!(feed_event(&mut state, key(LogicalKey::Escape), hwnd));
        assert_eq!(state.outcome, Some(RegionOutcome::Cancelled));
    }

    #[test]
    fn drag_then_enter_confirms_and_terminates() {
        let mut state = test_state(320, 200);
        let hwnd = HWND::default();
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: 40, y: 30 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::PointerMove { x: 200, y: 120 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftUp { x: 200, y: 120 },
            hwnd
        ));
        assert!(feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        assert_eq!(
            state.outcome,
            Some(RegionOutcome::Preview(
                PhysicalRect {
                    x: 40,
                    y: 30,
                    width: 161,
                    height: 91
                },
                Vec::new()
            ))
        );
    }

    #[test]
    fn menu_open_escape_and_item_click_both_terminate() {
        let hwnd = HWND::default();
        // 菜单 open + Esc → Cancelled。
        let mut state = test_state(320, 200);
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 200, y: 120 },
            InputEvent::LeftUp { x: 200, y: 120 },
            InputEvent::RightDown { x: 150, y: 100 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        assert!(feed_event(&mut state, key(LogicalKey::Escape), hwnd));
        assert_eq!(state.outcome, Some(RegionOutcome::Cancelled));
        // 菜单 open + 点击「复制」→ Quiet(Copy)。
        let mut state = test_state(320, 200);
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 200, y: 120 },
            InputEvent::LeftUp { x: 200, y: 120 },
            InputEvent::RightDown { x: 150, y: 100 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        let items = composer::menu_items(state.canvas.engine.flags());
        let metrics = composer::ChromeMetrics::for_scale(1.0);
        let panel = composer::menu_panel(
            metrics,
            state.canvas.engine.menu_anchor(),
            (320, 200),
            &items,
        );
        let (_, copy_rect) = composer::menu_item_rects(metrics, panel, &items)
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .unwrap();
        let (cx, cy) = copy_rect.center();
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: cx, y: cy },
            hwnd
        ));
        assert!(feed_event(
            &mut state,
            InputEvent::LeftUp { x: cx, y: cy },
            hwnd
        ));
        assert_eq!(
            state.outcome,
            Some(RegionOutcome::Quiet(
                PhysicalRect {
                    x: 40,
                    y: 30,
                    width: 161,
                    height: 91
                },
                QuietAction::Copy,
                Vec::new()
            ))
        );
    }

    #[test]
    fn menu_annotate_requests_forced_preview_outcome() {
        let mut state = test_state(320, 200);
        let hwnd = HWND::default();
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 200, y: 120 },
            InputEvent::LeftUp { x: 200, y: 120 },
            InputEvent::RightDown { x: 150, y: 100 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        let items = composer::menu_items(state.canvas.engine.flags());
        let metrics = composer::ChromeMetrics::for_scale(1.0);
        let panel = composer::menu_panel(
            metrics,
            state.canvas.engine.menu_anchor(),
            (320, 200),
            &items,
        );
        let (_, annotate_rect) = composer::menu_item_rects(metrics, panel, &items)
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Annotate)
            .unwrap();
        let (cx, cy) = annotate_rect.center();
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: cx, y: cy },
            hwnd
        ));
        assert!(feed_event(
            &mut state,
            InputEvent::LeftUp { x: cx, y: cy },
            hwnd
        ));
        // 「标注」必须与 Enter 确认区分:会话层据此强制打开预览编辑器。
        assert_eq!(
            state.outcome,
            Some(RegionOutcome::Annotate(
                PhysicalRect {
                    x: 40,
                    y: 30,
                    width: 161,
                    height: 91
                },
                Vec::new()
            ))
        );
    }

    #[test]
    fn annotate_outcome_needs_a_selection_and_keeps_rect() {
        let mut state = test_state(320, 200);
        assert_eq!(annotate_outcome(&state.canvas.engine), None);
        let hwnd = HWND::default();
        for event in [
            InputEvent::LeftDown { x: 5, y: 6 },
            InputEvent::PointerMove { x: 35, y: 46 },
            InputEvent::LeftUp { x: 35, y: 46 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        let rect = state.canvas.engine.selection().unwrap();
        assert_eq!(
            annotate_outcome(&state.canvas.engine),
            Some(RegionOutcome::Annotate(rect, Vec::new()))
        );
    }

    #[test]
    fn edge_drag_release_outside_then_enter_terminates() {
        let mut state = test_state(320, 200);
        let hwnd = HWND::default();
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 200, y: 120 },
            InputEvent::LeftUp { x: 200, y: 120 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        // 边缘拖拽,up 落在画布外坐标(钳制):回 Selected,不终态。
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: 203, y: 100 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::PointerMove { x: 999, y: 100 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftUp { x: 999, y: 100 },
            hwnd
        ));
        // EdgeResize 中 Esc 也能直接终态(另起会话验证 Enter 路径前先看钳制)。
        assert!(feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        assert!(matches!(state.outcome, Some(RegionOutcome::Preview(..))));
        if let Some(RegionOutcome::Preview(rect, _)) = state.outcome {
            assert_eq!((rect.width, rect.height), (280, 91)); // 右边钳到 319
        }
        // EdgeResize 中 Esc → Cancelled。
        let mut state = test_state(320, 200);
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 200, y: 120 },
            InputEvent::LeftUp { x: 200, y: 120 },
            InputEvent::LeftDown { x: 203, y: 100 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        assert!(feed_event(&mut state, key(LogicalKey::Escape), hwnd));
        assert_eq!(state.outcome, Some(RegionOutcome::Cancelled));
    }

    #[test]
    fn focus_loss_cancels_instead_of_hanging() {
        let mut state = test_state(320, 200);
        let hwnd = HWND::default();
        // 拖到一半失焦(Alt+Tab/Win+D/点击另一屏):必须退出而非永挂。
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: 40, y: 30 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::PointerMove { x: 120, y: 90 },
            hwnd
        ));
        assert!(cancel_on_focus_loss(&mut state));
        assert_eq!(state.outcome, Some(RegionOutcome::Cancelled));
        // 已有终态时不再覆盖。
        assert!(!cancel_on_focus_loss(&mut state));
    }

    #[test]
    fn cursor_hints_map_to_win32_cursor_resources() {
        assert_eq!(cursor_resource(CursorHint::Pointer), Some(IDC_HAND));
        assert_eq!(cursor_resource(CursorHint::Arrow), Some(IDC_ARROW));
        assert_eq!(cursor_resource(CursorHint::Crosshair), None);
        assert_eq!(cursor_resource(CursorHint::Move), Some(IDC_SIZEALL));
        assert_eq!(cursor_resource(CursorHint::ResizeNS), Some(IDC_SIZENS));
        assert_eq!(cursor_resource(CursorHint::ResizeEW), Some(IDC_SIZEWE));
        assert_eq!(cursor_resource(CursorHint::ResizeNWSE), Some(IDC_SIZENWSE));
        assert_eq!(cursor_resource(CursorHint::ResizeNESW), Some(IDC_SIZENESW));
        let custom = cursor_handle(CursorHint::Crosshair, 1.0).expect("双色十字光标");
        let system = unsafe { LoadCursorW(None, IDC_CROSS) }.expect("系统十字");
        assert_ne!(custom.0, system.0);
        assert!(!custom.is_invalid());
    }

    fn assert_dual_color_crosshair(sprite: CrosshairSprite) {
        let (cx, cy) = sprite.hotspot();
        assert_eq!(sprite.size % 2, 1);
        assert_eq!((cx, cy), (sprite.size / 2, sprite.size / 2));
        // 中心热点像素镂空:热点仍对准指针像素,但该像素透明。
        assert_eq!(sprite.pixel(cx, cy), CrosshairPixel::Empty);
        // 芯从距中心 1 像素处开始,恒 1 物理像素宽。
        assert_eq!(sprite.pixel(cx + 1, cy), CrosshairPixel::Core);
        assert_eq!(sprite.pixel(cx, cy + 1), CrosshairPixel::Core);
        assert_eq!(sprite.pixel(cx + 1, cy + 1), CrosshairPixel::Outline);
        assert_eq!(
            sprite.pixel(cx, cy.saturating_sub(sprite.arm + 1)),
            CrosshairPixel::Outline
        );
        assert_eq!(sprite.pixel(0, 0), CrosshairPixel::Empty);
        let rgba = sprite.rgba();
        let core = (cy * sprite.size + cx + 1) * 4;
        assert_eq!(&rgba[core..core + 4], &[0xf7, 0xf7, 0xf7, 0xff]);
        let outline = ((cy + 1) * sprite.size + cx + 1) * 4;
        assert_eq!(&rgba[outline..outline + 4], &[0x14, 0x14, 0x14, 0xff]);
    }

    #[test]
    fn dual_color_crosshair_has_light_core_and_dark_outline() {
        assert_dual_color_crosshair(CrosshairSprite::for_scale(1.0));
        assert_dual_color_crosshair(CrosshairSprite::fallback());
        let fallback = unsafe { create_color_cursor(CrosshairSprite::fallback()) }
            .or_else(|| unsafe { create_mono_cursor(CrosshairSprite::fallback()) });
        assert!(fallback.is_some_and(|cursor| !cursor.is_invalid()));
    }

    #[test]
    fn crosshair_sprite_scales_arm_with_frame_scale() {
        let base = CrosshairSprite::for_scale(1.0);
        assert_eq!(base.arm, CrosshairSprite::BASE_ARM);
        // 200% 时臂长翻倍,边长仍为奇数,芯恒 1 物理像素。
        let scaled = CrosshairSprite::for_scale(2.0);
        assert_eq!(scaled.arm, base.arm * 2);
        assert_eq!(scaled.size % 2, 1);
        assert_dual_color_crosshair(scaled);
        let (cx, cy) = scaled.hotspot();
        assert_eq!(scaled.pixel(cx, cy + 2), CrosshairPixel::Core);
        assert_eq!(scaled.pixel(cx + 1, cy + 2), CrosshairPixel::Outline);
    }

    #[test]
    fn map_client_to_engine_is_identity_when_sizes_match() {
        let client = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert_eq!(
            map_client_to_engine(100, 200, client, 1920, 1080),
            (100, 200)
        );
    }

    #[test]
    fn map_client_to_engine_scales_when_client_is_logical() {
        // 150% DPI: 客户区 1920×1080,冻结帧 2880×1620。
        let client = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        assert_eq!(
            map_client_to_engine(640, 360, client, 2880, 1620),
            (960, 540)
        );
        assert_eq!(
            map_client_to_engine(1920, 1080, client, 2880, 1620),
            (2880, 1620)
        );
    }

    #[test]
    fn virtual_keys_map_to_engine_logical_keys() {
        assert_eq!(map_virtual_key(VK_RETURN), Some(LogicalKey::Enter));
        assert_eq!(map_virtual_key(VK_ESCAPE), Some(LogicalKey::Escape));
        assert_eq!(map_virtual_key(VK_LEFT), Some(LogicalKey::ArrowLeft));
        assert_eq!(map_virtual_key(VK_UP), Some(LogicalKey::ArrowUp));
        assert_eq!(map_virtual_key(VK_RIGHT), Some(LogicalKey::ArrowRight));
        assert_eq!(map_virtual_key(VK_DOWN), Some(LogicalKey::ArrowDown));
        assert_eq!(map_virtual_key(VK_C), Some(LogicalKey::CopyColor));
        assert_eq!(map_virtual_key(VK_BACK), Some(LogicalKey::Delete));
        assert_eq!(map_virtual_key(VK_DELETE), Some(LogicalKey::Delete));
        assert_eq!(map_virtual_key(VK_SHIFT), None);
        assert_eq!(map_virtual_key(VK_CONTROL), None);
        assert_eq!(map_virtual_key(VK_Z), None);
        assert_eq!(
            map_virtual_key(VK_R),
            Some(LogicalKey::Tool(AnnotationTool::Rect))
        );
        assert_eq!(
            map_virtual_key(VK_E),
            Some(LogicalKey::Tool(AnnotationTool::Ellipse))
        );
        assert_eq!(
            map_virtual_key(VK_L),
            Some(LogicalKey::Tool(AnnotationTool::Line))
        );
        assert_eq!(
            map_virtual_key(VK_A),
            Some(LogicalKey::Tool(AnnotationTool::Arrow))
        );
        assert_eq!(
            map_virtual_key(VK_N),
            Some(LogicalKey::Tool(AnnotationTool::Number))
        );
        assert_eq!(
            map_virtual_key(VK_T),
            Some(LogicalKey::Tool(AnnotationTool::Text))
        );
        assert_eq!(
            map_virtual_key(VK_P),
            Some(LogicalKey::Tool(AnnotationTool::Pen))
        );
        assert_eq!(
            map_virtual_key(VK_H),
            Some(LogicalKey::Tool(AnnotationTool::Highlighter))
        );
        assert_eq!(
            map_virtual_key(VK_M),
            Some(LogicalKey::Tool(AnnotationTool::Mosaic))
        );
        assert_eq!(
            map_virtual_key(VK_B),
            Some(LogicalKey::Tool(AnnotationTool::Blur))
        );
        assert_eq!(map_virtual_key(0x51), None);
    }

    /// R21 呈现诊断:客户区与冻结帧一致必须走 1:1 直拷路径(无插值)。
    #[test]
    fn blit_plan_prefers_exact_copy_when_sizes_match() {
        assert_eq!(blit_plan((1920, 1080), (1920, 1080)), BlitPlan::Exact);
        // DPI 虚拟化/分辨率不一致时才允许按比例拉伸。
        assert_eq!(blit_plan((1920, 1080), (2880, 1620)), BlitPlan::Scaled);
        assert_eq!(blit_plan((2880, 1620), (1920, 1080)), BlitPlan::Scaled);
    }

    #[test]
    fn inline_annotation_flows_to_preview_and_quiet_outcomes() {
        // Enter 确认:Preview 结果携带图元。
        let mut state = test_state_with_flags(800, 600, FeatureFlags::default());
        let hwnd = HWND::default();
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 760, y: 560 },
            InputEvent::LeftUp { x: 760, y: 560 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        click_engine_tool(&mut state, hwnd, AnnotationTool::Rect);
        for event in [
            InputEvent::LeftDown { x: 200, y: 300 },
            InputEvent::PointerMove { x: 400, y: 420 },
            InputEvent::LeftUp { x: 400, y: 420 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        assert_eq!(state.canvas.engine.annotations().len(), 1);
        assert!(feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        match &state.outcome {
            Some(RegionOutcome::Preview(rect, annotations)) => {
                assert_eq!(rect.width, 721);
                assert_eq!(annotations.len(), 1);
                assert!(matches!(annotations[0], Annotation::Rect { .. }));
            }
            other => panic!("expected preview outcome, got {other:?}"),
        }

        // 右键菜单「复制」:Quiet 结果同样携带图元(输出合并后再执行动作)。
        // 标注模式下轻量操作条让位,复制/保存经右键菜单完成。
        let mut state = test_state_with_flags(800, 600, FeatureFlags::default());
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 760, y: 560 },
            InputEvent::LeftUp { x: 760, y: 560 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        click_engine_tool(&mut state, hwnd, AnnotationTool::Ellipse);
        for event in [
            InputEvent::LeftDown { x: 220, y: 320 },
            InputEvent::PointerMove { x: 420, y: 440 },
            InputEvent::LeftUp { x: 420, y: 440 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        assert!(!state.canvas.engine.scene().toolbar_visible);
        assert!(!feed_event(
            &mut state,
            InputEvent::RightDown { x: 600, y: 300 },
            hwnd
        ));
        let items = composer::menu_items(state.canvas.engine.flags());
        let metrics = state.canvas.engine.metrics();
        let menu = composer::menu_panel(
            metrics,
            state.canvas.engine.menu_anchor(),
            state.canvas.engine.size(),
            &items,
        );
        let (_, copy_rect) = composer::menu_item_rects(metrics, menu, &items)
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .unwrap();
        let (cx, cy) = copy_rect.center();
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: cx, y: cy },
            hwnd
        ));
        assert!(feed_event(
            &mut state,
            InputEvent::LeftUp { x: cx, y: cy },
            hwnd
        ));
        match &state.outcome {
            Some(RegionOutcome::Quiet(rect, QuietAction::Copy, annotations)) => {
                assert_eq!(rect.width, 721);
                assert_eq!(annotations.len(), 1);
                assert!(matches!(annotations[0], Annotation::Ellipse { .. }));
            }
            other => panic!("expected quiet copy outcome, got {other:?}"),
        }
    }

    #[test]
    fn inline_text_tool_commits_from_shell_text_events() {
        let mut state = test_state_with_flags(800, 600, FeatureFlags::default());
        let hwnd = HWND::default();
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 760, y: 560 },
            InputEvent::LeftUp { x: 760, y: 560 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        click_engine_tool(&mut state, hwnd, AnnotationTool::Text);
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftDown { x: 200, y: 300 },
            hwnd
        ));
        assert!(!feed_event(
            &mut state,
            InputEvent::LeftUp { x: 200, y: 300 },
            hwnd
        ));
        assert!(state.canvas.engine.text_edit().is_some());
        // IME 组合串 + 提交结果(壳按 WM_IME_* 转发)。
        assert!(!feed_event(
            &mut state,
            InputEvent::Composition("zhong".into()),
            hwnd
        ));
        assert!(!feed_event(&mut state, InputEvent::Text("中".into()), hwnd));
        // 第一次 Enter 提交文本(不结束会话),第二次 Enter 确认选区。
        assert!(!feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        assert!(state.canvas.engine.text_edit().is_none());
        assert!(feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        match &state.outcome {
            Some(RegionOutcome::Preview(_, annotations)) => match &annotations[0] {
                Annotation::Text { text, .. } => assert_eq!(text, "中"),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected preview outcome, got {other:?}"),
        }
    }

    /// 经右键菜单「标注」进入标注模式(与用户路径一致)。
    fn enter_annotation_mode(state: &mut ShellState, hwnd: HWND) {
        if state.canvas.engine.annotation_mode() {
            return;
        }
        let items = composer::menu_items(state.canvas.engine.flags());
        let metrics = state.canvas.engine.metrics();
        let selection = state.canvas.engine.selection().expect("selection");
        let anchor = (
            selection.x as i32 + selection.width as i32 / 2,
            selection.y as i32 + selection.height as i32 / 2,
        );
        assert!(!feed_event(
            state,
            InputEvent::RightDown {
                x: anchor.0,
                y: anchor.1
            },
            hwnd
        ));
        let panel = composer::menu_panel(
            metrics,
            state.canvas.engine.menu_anchor(),
            state.canvas.engine.size(),
            &items,
        );
        let (_, rect) = composer::menu_item_rects(metrics, panel, &items)
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Annotate)
            .expect("annotate item");
        let (cx, cy) = rect.center();
        assert!(!feed_event(
            state,
            InputEvent::LeftDown { x: cx, y: cy },
            hwnd
        ));
        assert!(!feed_event(
            state,
            InputEvent::LeftUp { x: cx, y: cy },
            hwnd
        ));
        assert!(
            state.canvas.engine.annotation_mode(),
            "「标注」应进入标注模式"
        );
    }

    /// 点击标注工具条上指定工具的按钮中心(引擎内部消费该动作);
    /// 工具在「更多」展开行时先展开。
    fn click_engine_tool(state: &mut ShellState, hwnd: HWND, tool: AnnotationTool) {
        enter_annotation_mode(state, hwnd);
        let click = |state: &mut ShellState, hwnd: HWND, action: SelectionAction| {
            let toolbar = state
                .canvas
                .engine
                .annotation_toolbar()
                .expect("annotation toolbar");
            let (_, rect) = toolbar
                .buttons
                .into_iter()
                .find(|(candidate, _)| *candidate == action)
                .expect("button present");
            let (cx, cy) = rect.center();
            assert!(!feed_event(
                state,
                InputEvent::LeftDown { x: cx, y: cy },
                hwnd
            ));
            assert!(!feed_event(
                state,
                InputEvent::LeftUp { x: cx, y: cy },
                hwnd
            ));
        };
        let target = SelectionAction::Tool(tool);
        let visible = state
            .canvas
            .engine
            .annotation_toolbar()
            .map(|toolbar| {
                toolbar
                    .buttons
                    .iter()
                    .any(|(candidate, _)| *candidate == target)
            })
            .unwrap_or(false);
        if visible {
            click(state, hwnd, target);
        } else {
            click(state, hwnd, SelectionAction::More);
            assert!(state.canvas.engine.annotation_more(), "「更多」应展开");
            click(state, hwnd, target);
        }
        assert_eq!(state.canvas.engine.tool(), Some(tool));
    }

    #[test]
    fn annotation_mode_hides_rail_and_more_toggles_tools() {
        let mut state = test_state_with_flags(800, 600, FeatureFlags::default());
        let hwnd = HWND::default();
        for event in [
            InputEvent::LeftDown { x: 40, y: 30 },
            InputEvent::PointerMove { x: 760, y: 480 },
            InputEvent::LeftUp { x: 760, y: 480 },
        ] {
            assert!(!feed_event(&mut state, event, hwnd));
        }
        assert!(state.canvas.engine.scene().toolbar_visible);
        enter_annotation_mode(&mut state, hwnd);
        // 标注模式下轻量操作条让位,只显示单行精简工具条。
        assert!(!state.canvas.engine.scene().toolbar_visible);
        assert!(state.canvas.engine.annotation_toolbar().is_some());
        assert!(
            state.outcome.is_none(),
            "进入标注后不得因隐藏操作条产出动作"
        );
        // 「更多」展开/收起其余工具。
        assert!(!state.canvas.engine.annotation_more());
        click_engine_action(&mut state, hwnd, SelectionAction::More);
        assert!(state.canvas.engine.annotation_more());
        click_engine_action(&mut state, hwnd, SelectionAction::More);
        assert!(!state.canvas.engine.annotation_more());
        // Esc 先退出标注模式,再 Esc 取消会话。
        assert!(!feed_event(&mut state, key(LogicalKey::Escape), hwnd));
        assert!(!state.canvas.engine.annotation_mode());
        assert!(feed_event(&mut state, key(LogicalKey::Escape), hwnd));
        assert_eq!(state.outcome, Some(RegionOutcome::Cancelled));
    }

    /// 点击标注工具条上指定动作的按钮中心。
    fn click_engine_action(state: &mut ShellState, hwnd: HWND, action: SelectionAction) {
        let toolbar = state
            .canvas
            .engine
            .annotation_toolbar()
            .expect("annotation toolbar");
        let (_, rect) = toolbar
            .buttons
            .into_iter()
            .find(|(candidate, _)| *candidate == action)
            .expect("button present");
        let (cx, cy) = rect.center();
        assert!(!feed_event(
            state,
            InputEvent::LeftDown { x: cx, y: cy },
            hwnd
        ));
        assert!(!feed_event(
            state,
            InputEvent::LeftUp { x: cx, y: cy },
            hwnd
        ));
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
        assert_eq!(quiet_action_for(SelectionAction::More), None);
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
            last_scene: None,
        };
        assert!(compose_canvas(&mut canvas).is_none());
    }

    /// ADR-007 预算护栏:4K 冻结帧下选区+操作条+放大镜场景的单次
    /// compose+swizzle(以及可选的真实窗口 blit)耗时基线。
    /// 手动运行:`cargo test --release shell::windows -- --ignored --nocapture`
    /// (会创建真实窗口,日常套件与 CI 不执行)。
    #[test]
    #[ignore = "需要真实窗口,手动 --release --nocapture 运行"]
    fn four_k_compose_present_budget() {
        let (w, h) = (3840u32, 2160u32);
        let bytes = vec![80u8; (w * h * 4) as usize];
        let frame = accept_buffer(RawBuffer::ready(w, h, bytes)).unwrap();
        let mut canvas = Canvas {
            engine: SelectionEngine::new(w, h, FeatureFlags::default()),
            composer: Composer::new(&frame).unwrap(),
            scratch: vec![0u8; frame.rgba.len()],
            present_buf: vec![0u8; frame.rgba.len()],
            width: w as i32,
            height: h as i32,
            last_scene: None,
        };
        canvas
            .engine
            .handle_event(InputEvent::LeftDown { x: 600, y: 400 });
        canvas
            .engine
            .handle_event(InputEvent::PointerMove { x: 2600, y: 1800 });
        canvas
            .engine
            .handle_event(InputEvent::LeftUp { x: 2600, y: 1800 });
        canvas
            .engine
            .handle_event(InputEvent::PointerMove { x: 2610, y: 1810 });
        let scene = canvas.engine.scene();
        assert!(scene.selection.is_some() && scene.toolbar_visible);
        // 预热一次再取平均/最大,降低首次分配噪声。
        assert!(compose_canvas(&mut canvas).is_some());
        let mut worst = Duration::ZERO;
        let mut total = Duration::ZERO;
        let rounds = 60;
        for _ in 0..rounds {
            let (elapsed, _) = compose_canvas(&mut canvas).expect("lengths match");
            worst = worst.max(elapsed);
            total += elapsed;
        }
        eprintln!(
            "4K compose+swizzle: rounds={rounds}, avg={:?}, worst={:?}",
            total / rounds,
            worst
        );
        // 可选的真实窗口 blit:创建失败(无交互桌面)时跳过并说明。
        unsafe {
            match create_overlay_window(0, 0, w as i32, h as i32, 1.0) {
                Ok(hwnd) => {
                    let mut worst = Duration::ZERO;
                    let mut total = Duration::ZERO;
                    for _ in 0..30 {
                        let started = Instant::now();
                        assert!(compose_canvas(&mut canvas).is_some());
                        blit(hwnd, canvas.width, canvas.height, &canvas.present_buf);
                        let elapsed = started.elapsed();
                        worst = worst.max(elapsed);
                        total += elapsed;
                    }
                    eprintln!(
                        "4K full present (compose+swizzle+blit): rounds=30, avg={:?}, worst={:?}",
                        total / 30,
                        worst
                    );
                    let _ = DestroyWindow(hwnd);
                    PostQuitMessage(0);
                    let mut msg = MSG::default();
                    loop {
                        let result = GetMessageW(&mut msg, None, 0, 0);
                        if result.0 <= 0 {
                            break;
                        }
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                Err(_) => eprintln!("window unavailable; blit not measured"),
            }
        }
    }
}
