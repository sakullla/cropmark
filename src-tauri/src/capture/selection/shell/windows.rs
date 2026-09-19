//! Windows 区域选区壳:Win32 全屏置顶窗 + 消息泵 + StretchDIBits 呈现。
//!
//! 交互语义全部由平台无关选区引擎(`capture::selection`)决定;本壳只把
//! 鼠标(左/右/移动)与键盘(方向键/Shift/Enter/Esc/C)事件以物理像素坐标
//! 喂给引擎,并把引擎的合成位图 present 到屏幕(ADR-001/006)。右键=菜单、
//! Esc=取消;Enter 确认;操作条/菜单动作经 `RegionOutcome` 交回会话层分发。

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetDC, ReleaseDC, ScreenToClient, StretchDIBits, UpdateWindow, ValidateRect, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, RGBQUAD, SRCCOPY,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetCursor, SetForegroundWindow,
    SetWindowPos, ShowWindow, TranslateMessage,
    CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_SIZEALL, IDC_SIZENESW,
    IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG, SWP_SHOWWINDOW, SW_SHOW, WM_CLOSE, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_PAINT, WM_RBUTTONDOWN, WM_SETCURSOR, WM_SETFOCUS, WNDCLASSEXW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::buffer::Frame;
use crate::capture::error::CaptureError;
use crate::capture::geometry::{MonitorGeom, PhysicalRect};
use crate::capture::selection::composer::{self, Composer};
use crate::capture::selection::{
    CursorHint, EngineOutcome, FeatureFlags, InputEvent, LogicalKey, SelectionAction,
    SelectionEngine,
};
use crate::capture::session::QuietAction;

const CLASS: &str = "CropmarkRegionOverlay";
static CLASS_SERIAL: AtomicU32 = AtomicU32::new(1);

/// 当前活动壳窗口句柄(0 = 无)。会话层在 stale 重置时经
/// `request_shell_close` 从任意线程请求关闭,泵退出后旧结果按代际丢弃。
static ACTIVE_SHELL_HWND: AtomicIsize = AtomicIsize::new(0);

// 虚拟键码(直接使用数值,不为 Shift 状态引入新的 windows crate feature)。
const VK_SHIFT: u32 = 0x10;
const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_C: u32 = 0x43;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;

/// 壳的最终结果:会话层据此选择完成路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionOutcome {
    /// Enter 确认:rect 走普通完成路径(按 finishAction 预览或静默)。
    Preview(PhysicalRect),
    /// 操作条/菜单的「标注」动作:rect 强制走预览编辑器,不受静默完成配置影响。
    Annotate(PhysicalRect),
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
    /// 呈现用 BGRA 缓冲(StretchDIBits 32bpp BI_RGB 按 BGR 序读取;
    /// 实测 BI_BITFIELDS 直读 RGBA 的掩码路径 blit 慢一个数量级,不用)。
    present_buf: Vec<u8>,
    width: i32,
    height: i32,
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
    outcome: Option<RegionOutcome>,
    timing: bool,
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
    hooks: ShellHooks,
) -> Result<RegionOutcome, CaptureError> {
    run_shell(frame, monitor, flags, hooks)
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
        let _ = PostMessageW(Some(HWND(value as *mut c_void)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

fn run_shell(
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
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(ShellState {
            hooks,
            canvas: Canvas {
                // 注入冻结帧 DPI 缩放:chrome(放大镜面板)光标命中需要。
                engine: SelectionEngine::new(width as u32, height as u32, flags)
                    .with_scale(frame.scale),
                composer,
                scratch: vec![0; bytes],
                present_buf: vec![0; bytes],
                width,
                height,
            },
            shift_down: false,
            outcome: None,
            timing: std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some(),
            had_focus: false,
            created_at: Instant::now(),
        });
    });
    let hwnd =
        unsafe { create_overlay_window(monitor.physical_x, monitor.physical_y, width, height)? };
    ACTIVE_SHELL_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    unsafe {
        pump();
        let _ = DestroyWindow(hwnd);
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
            state.outcome = Some(RegionOutcome::Preview(rect));
            true
        }
        EngineOutcome::Cancelled => {
            state.outcome = Some(RegionOutcome::Cancelled);
            true
        }
        EngineOutcome::Action(action) => match action {
            SelectionAction::Annotate => {
                if let Some(outcome) = annotate_outcome(state.canvas.engine.selection()) {
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
            quiet => {
                if let (Some(rect), Some(action)) =
                    (state.canvas.engine.selection(), quiet_action_for(quiet))
                {
                    state.outcome = Some(RegionOutcome::Quiet(rect, action));
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
fn annotate_outcome(selection: Option<PhysicalRect>) -> Option<RegionOutcome> {
    selection.map(RegionOutcome::Annotate)
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

/// 引擎光标提示 → Win32 系统光标资源。
fn cursor_resource(hint: CursorHint) -> PCWSTR {
    match hint {
        CursorHint::Move => IDC_SIZEALL,
        CursorHint::ResizeNS => IDC_SIZENS,
        CursorHint::ResizeEW => IDC_SIZEWE,
        CursorHint::ResizeNWSE => IDC_SIZENWSE,
        CursorHint::ResizeNESW => IDC_SIZENESW,
        CursorHint::Pointer => IDC_HAND,
        CursorHint::Arrow => IDC_ARROW,
        CursorHint::Crosshair => IDC_CROSS,
    }
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

/// 仅在引擎要求 Redraw 时重合成;WM_PAINT 直接呈现缓存位图(ADR-007)。
fn present(timing: bool, hwnd: HWND, canvas: &mut Canvas) {
    let started = if timing { Some(Instant::now()) } else { None };
    let compose_at = compose_canvas(canvas);
    unsafe { blit(hwnd, canvas.width, canvas.height, &canvas.present_buf) };
    if let Some(started) = started {
        eprintln!(
            "Cropmark overlay {}x{} present: compose+swizzle={:?}, total={:?}",
            canvas.width,
            canvas.height,
            compose_at,
            started.elapsed()
        );
    }
}

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
    let _ = ReleaseDC(Some(hwnd), hdc);
}

unsafe fn create_overlay_window(x: i32, y: i32, w: i32, h: i32) -> Result<HWND, CaptureError> {
    let instance = GetModuleHandleW(None).map_err(|_| CaptureError::api("error.capture.window_create"))?;
    let serial = CLASS_SERIAL.fetch_add(1, Ordering::Relaxed);
    let class_name: Vec<u16> = format!("{CLASS}{serial}\0").encode_utf16().collect();
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.into(),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
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
    (
        (x as i64 * ew / cw) as i32,
        (y as i64 * eh / ch) as i32,
    )
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
            // 箭头,手柄/边→resize 箭头,选区内部→移动,其他→十字。
            let hint = STATE.with(|slot| {
                slot.borrow().as_ref().map(|state| {
                    let (x, y) = state.canvas.engine.cursor();
                    state.canvas.engine.cursor_for(x, y)
                })
            });
            let resource = cursor_resource(hint.unwrap_or(CursorHint::Crosshair));
            if let Ok(cursor) = LoadCursorW(None, resource) {
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
                if msg == WM_KEYDOWN {
                    if let Some(key) = map_virtual_key(vk) {
                        let shift = state.shift_down;
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
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
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
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::{accept_buffer, RawBuffer};

    /// 合成壳状态:真实引擎+合成器,空 hwnd(blit 守卫跳过真实呈现)。
    /// wnd_proc 只做消息→InputEvent 映射,集成测试直接驱动 feed_event 即
    /// 等价覆盖「消息序列 → 终态/退出」链路。
    fn test_state(width: u32, height: u32) -> ShellState {
        fn noop_copy_color(_text: &str, _hex: &str) {}
        let frame = accept_buffer(RawBuffer::ready(width, height, vec![60u8; (width * height * 4) as usize])).unwrap();
        let bytes = frame.rgba.len();
        ShellState {
            hooks: ShellHooks {
                copy_color: noop_copy_color,
            },
            canvas: Canvas {
                engine: SelectionEngine::new(width, height, FeatureFlags::default())
                    .with_scale(frame.scale),
                composer: Composer::new(&frame).unwrap(),
                scratch: vec![0; bytes],
                present_buf: vec![0; bytes],
                width: width as i32,
                height: height as i32,
            },
            shift_down: false,
            outcome: None,
            timing: false,
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
        assert!(!feed_event(&mut state, InputEvent::LeftDown { x: 50, y: 50 }, hwnd));
        assert!(!feed_event(&mut state, InputEvent::LeftUp { x: 50, y: 50 }, hwnd));
        assert!(state.outcome.is_none());
        // Esc 兜底退出。
        assert!(feed_event(&mut state, key(LogicalKey::Escape), hwnd));
        assert_eq!(state.outcome, Some(RegionOutcome::Cancelled));
    }

    #[test]
    fn drag_then_enter_confirms_and_terminates() {
        let mut state = test_state(320, 200);
        let hwnd = HWND::default();
        assert!(!feed_event(&mut state, InputEvent::LeftDown { x: 40, y: 30 }, hwnd));
        assert!(!feed_event(
            &mut state,
            InputEvent::PointerMove { x: 200, y: 120 },
            hwnd
        ));
        assert!(!feed_event(&mut state, InputEvent::LeftUp { x: 200, y: 120 }, hwnd));
        assert!(feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        assert_eq!(
            state.outcome,
            Some(RegionOutcome::Preview(PhysicalRect {
                x: 40,
                y: 30,
                width: 161,
                height: 91
            }))
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
        let panel = composer::menu_panel(metrics, state.canvas.engine.menu_anchor(), (320, 200), &items);
        let (_, copy_rect) = composer::menu_item_rects(metrics, panel, &items)
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Copy)
            .unwrap();
        let (cx, cy) = copy_rect.center();
        assert!(!feed_event(&mut state, InputEvent::LeftDown { x: cx, y: cy }, hwnd));
        assert!(feed_event(&mut state, InputEvent::LeftUp { x: cx, y: cy }, hwnd));
        assert_eq!(
            state.outcome,
            Some(RegionOutcome::Quiet(
                PhysicalRect {
                    x: 40,
                    y: 30,
                    width: 161,
                    height: 91
                },
                QuietAction::Copy
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
        let panel = composer::menu_panel(metrics, state.canvas.engine.menu_anchor(), (320, 200), &items);
        let (_, annotate_rect) = composer::menu_item_rects(metrics, panel, &items)
            .into_iter()
            .find(|(action, _)| *action == SelectionAction::Annotate)
            .unwrap();
        let (cx, cy) = annotate_rect.center();
        assert!(!feed_event(&mut state, InputEvent::LeftDown { x: cx, y: cy }, hwnd));
        assert!(feed_event(&mut state, InputEvent::LeftUp { x: cx, y: cy }, hwnd));
        // 「标注」必须与 Enter 确认区分:会话层据此强制打开预览编辑器。
        assert_eq!(
            state.outcome,
            Some(RegionOutcome::Annotate(PhysicalRect {
                x: 40,
                y: 30,
                width: 161,
                height: 91
            }))
        );
    }

    #[test]
    fn annotate_outcome_needs_a_selection_and_keeps_rect() {
        let rect = PhysicalRect {
            x: 5,
            y: 6,
            width: 30,
            height: 40,
        };
        assert_eq!(
            annotate_outcome(Some(rect)),
            Some(RegionOutcome::Annotate(rect))
        );
        assert_eq!(annotate_outcome(None), None);
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
        assert!(!feed_event(&mut state, InputEvent::LeftDown { x: 203, y: 100 }, hwnd));
        assert!(!feed_event(
            &mut state,
            InputEvent::PointerMove { x: 999, y: 100 },
            hwnd
        ));
        assert!(!feed_event(&mut state, InputEvent::LeftUp { x: 999, y: 100 }, hwnd));
        // EdgeResize 中 Esc 也能直接终态(另起会话验证 Enter 路径前先看钳制)。
        assert!(feed_event(&mut state, key(LogicalKey::Enter), hwnd));
        assert!(matches!(state.outcome, Some(RegionOutcome::Preview(_))));
        if let Some(RegionOutcome::Preview(rect)) = state.outcome {
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
        assert!(!feed_event(&mut state, InputEvent::LeftDown { x: 40, y: 30 }, hwnd));
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
        assert_eq!(cursor_resource(CursorHint::Pointer), IDC_HAND);
        assert_eq!(cursor_resource(CursorHint::Arrow), IDC_ARROW);
        assert_eq!(cursor_resource(CursorHint::Crosshair), IDC_CROSS);
        assert_eq!(cursor_resource(CursorHint::Move), IDC_SIZEALL);
        assert_eq!(cursor_resource(CursorHint::ResizeNS), IDC_SIZENS);
        assert_eq!(cursor_resource(CursorHint::ResizeEW), IDC_SIZEWE);
        assert_eq!(cursor_resource(CursorHint::ResizeNWSE), IDC_SIZENWSE);
        assert_eq!(cursor_resource(CursorHint::ResizeNESW), IDC_SIZENESW);
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
        assert_eq!(map_virtual_key(VK_SHIFT), None);
        assert_eq!(map_virtual_key(0x41), None);
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
            let elapsed = compose_canvas(&mut canvas).expect("lengths match");
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
            match create_overlay_window(0, 0, w as i32, h as i32) {
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
