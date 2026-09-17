//! Windows region picker: Win32 popup + GDI, no WebView.
//! Screenshot tools keep the freeze bitmap in a layered/popup window and
//! draw selection locally; HTML/WebView overlays add hundreds of ms.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    StretchDIBits, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, RGBQUAD,
    SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    LoadCursorW, PostQuitMessage, RegisterClassExW, SetCursor, SetWindowPos, ShowWindow,
    SetForegroundWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST, IDC_CROSS, MSG,
    SWP_SHOWWINDOW, SW_SHOW, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_SETCURSOR, WNDCLASSEXW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

use super::buffer::Frame;
use super::error::CaptureError;
use super::geometry::{MonitorGeom, PhysicalRect};

const CLASS: &str = "CropmarkRegionOverlay";
static CLASS_SERIAL: AtomicU32 = AtomicU32::new(1);

struct OverlayState {
    _origin_x: i32,
    _origin_y: i32,
    width: i32,
    height: i32,
    original: Vec<u8>,
    dimmed: Vec<u8>,
    composed: Vec<u8>,
    dragging: bool,
    start_x: i32,
    start_y: i32,
    current_x: i32,
    current_y: i32,
    result: Option<PhysicalRect>,
    cancelled: bool,
}

thread_local! {
    static STATE: RefCell<Option<OverlayState>> = const { RefCell::new(None) };
}

pub fn pick_region(frame: &Frame, monitor: &MonitorGeom) -> Result<Option<PhysicalRect>, CaptureError> {
    if frame.width == 0 || frame.height == 0 || frame.rgba.len() < 4 {
        return Err(CaptureError::invalid_buffer("空缓冲"));
    }
    let width = frame.width.min(monitor.physical_width).max(1) as i32;
    let height = frame.height.min(monitor.physical_height).max(1) as i32;
    let original = rgba_to_bgra(&frame.rgba, width as u32, height as u32);
    let dimmed = dim_bgra(&original);
    STATE.with(|slot| {
        *slot.borrow_mut() = Some(OverlayState {
            _origin_x: monitor.physical_x,
            _origin_y: monitor.physical_y,
            width,
            height,
            original,
            dimmed,
            composed: vec![0; (width * height * 4) as usize],
            dragging: false,
            start_x: 0,
            start_y: 0,
            current_x: 0,
            current_y: 0,
            result: None,
            cancelled: false,
        });
    });

    let hwnd = unsafe { create_overlay_window(monitor.physical_x, monitor.physical_y, width, height)? };
    unsafe {
        pump(hwnd);
        let _ = DestroyWindow(hwnd);
    }
    let outcome = STATE.with(|slot| slot.borrow_mut().take());
    match outcome {
        Some(state) if state.cancelled => Ok(None),
        Some(state) => Ok(state.result),
        None => Ok(None),
    }
}

fn rgba_to_bgra(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixels = (width as usize) * (height as usize);
    let mut bgra = vec![0u8; pixels * 4];
    let n = pixels.min(rgba.len() / 4);
    for i in 0..n {
        let s = i * 4;
        let d = i * 4;
        bgra[d] = rgba[s + 2];
        bgra[d + 1] = rgba[s + 1];
        bgra[d + 2] = rgba[s];
        bgra[d + 3] = 255;
    }
    bgra
}

fn dim_bgra(src: &[u8]) -> Vec<u8> {
    let mut out = src.to_vec();
    for px in out.chunks_exact_mut(4) {
        px[0] = (px[0] as u16 * 52 / 100) as u8;
        px[1] = (px[1] as u16 * 52 / 100) as u8;
        px[2] = (px[2] as u16 * 52 / 100) as u8;
    }
    out
}

fn compose(state: &mut OverlayState) {
    state.composed.copy_from_slice(&state.dimmed);
    if !state.dragging {
        return;
    }
    let x0 = state.start_x.min(state.current_x).clamp(0, state.width - 1);
    let y0 = state.start_y.min(state.current_y).clamp(0, state.height - 1);
    let x1 = (state.start_x.max(state.current_x) + 1).clamp(1, state.width);
    let y1 = (state.start_y.max(state.current_y) + 1).clamp(1, state.height);
    let sel_w = x1 - x0;
    let sel_h = y1 - y0;
    if sel_w < 1 || sel_h < 1 {
        return;
    }
    let stride = state.width as usize * 4;
    for y in y0..y1 {
        let row = y as usize * stride;
        let src = row + x0 as usize * 4;
        let count = sel_w as usize * 4;
        state.composed[src..src + count].copy_from_slice(&state.original[src..src + count]);
    }
    outline(&mut state.composed, stride, x0, y0, x1 - 1, y1 - 1, [0xBF, 0xD4, 0x2D, 255]);
}

fn outline(buf: &mut [u8], stride: usize, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 4]) {
    let put = |buf: &mut [u8], x: i32, y: i32| {
        if x < 0 || y < 0 {
            return;
        }
        let i = y as usize * stride + x as usize * 4;
        if i + 3 < buf.len() {
            buf[i..i + 4].copy_from_slice(&color);
        }
    };
    for x in x0..=x1 {
        put(buf, x, y0);
        put(buf, x, y0 + 1);
        put(buf, x, y1);
        put(buf, x, y1 - 1);
    }
    for y in y0..=y1 {
        put(buf, x0, y);
        put(buf, x0 + 1, y);
        put(buf, x1, y);
        put(buf, x1 - 1, y);
    }
}

fn selection_rect(state: &OverlayState) -> Option<PhysicalRect> {
    if !state.dragging {
        return None;
    }
    let x = state.start_x.min(state.current_x).max(0);
    let y = state.start_y.min(state.current_y).max(0);
    let w = (state.start_x.max(state.current_x) - x + 1).max(0);
    let h = (state.start_y.max(state.current_y) - y + 1).max(0);
    if w < 2 || h < 2 {
        return None;
    }
    Some(PhysicalRect {
        x: x as u32,
        y: y as u32,
        width: w as u32,
        height: h as u32,
    })
}

unsafe fn create_overlay_window(x: i32, y: i32, w: i32, h: i32) -> Result<HWND, CaptureError> {
    let instance = GetModuleHandleW(None).map_err(|_| CaptureError::api("无法创建截取窗。"))?;
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
        return Err(CaptureError::api("无法注册截取窗。"));
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
    .map_err(|_| CaptureError::api("无法打开截取窗。"))?;
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        x,
        y,
        w,
        h,
        SWP_SHOWWINDOW,
    );
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    let _ = UpdateWindow(hwnd);
    Ok(hwnd)
}

unsafe fn paint(hwnd: HWND) {
    STATE.with(|slot| {
        let mut guard = slot.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };
        compose(state);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: state.width,
                biHeight: -state.height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0 as u32,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default(); 1],
        };
        let hdc = windows::Win32::Graphics::Gdi::GetDC(Some(hwnd));
        if hdc.0.is_null() {
            return;
        }
        let _ = StretchDIBits(
            hdc,
            0,
            0,
            state.width,
            state.height,
            0,
            0,
            state.width,
            state.height,
            Some(state.composed.as_ptr().cast::<c_void>()),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
        let _ = windows::Win32::Graphics::Gdi::ReleaseDC(Some(hwnd), hdc);
        let _ = info;
    });
}

unsafe fn client_point(_hwnd: HWND, lparam: LPARAM) -> (i32, i32) {
    let packed = lparam.0 as u32;
    let x = packed as i16 as i32;
    let y = (packed >> 16) as i16 as i32;
    (x, y)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let _ = windows::Win32::Graphics::Gdi::ValidateRect(Some(hwnd), Some(&rect));
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Ok(cursor) = LoadCursorW(None, IDC_CROSS) {
                let _ = SetCursor(Some(cursor));
            }
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = client_point(hwnd, lparam);
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.dragging = true;
                    state.start_x = x.clamp(0, state.width - 1);
                    state.start_y = y.clamp(0, state.height - 1);
                    state.current_x = state.start_x;
                    state.current_y = state.start_y;
                }
            });
            paint(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = client_point(hwnd, lparam);
            let dragging = STATE.with(|slot| {
                let mut guard = slot.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                if !state.dragging {
                    return false;
                }
                state.current_x = x.clamp(0, state.width - 1);
                state.current_y = y.clamp(0, state.height - 1);
                true
            });
            if dragging {
                paint(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let done = STATE.with(|slot| {
                let mut guard = slot.borrow_mut();
                let Some(state) = guard.as_mut() else {
                    return false;
                };
                if !state.dragging {
                    return false;
                }
                state.result = selection_rect(state);
                state.dragging = false;
                state.result.is_some() || true
            });
            if done {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP | WM_KEYDOWN if msg == WM_RBUTTONUP || wparam.0 as u16 == 0x1B => {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.cancelled = true;
                    state.result = None;
                }
            });
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn pump(_hwnd: HWND) {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}
