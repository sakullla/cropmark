//! 长截图进行时留在屏幕上的范围框。
//!
//! 选区壳关闭后，如果没有可见的框，用户不知道正在截哪一块，滚轮也像没目标。
//! 这层窗口点击穿透、不抢焦点，并用 WDA_EXCLUDEFROMCAPTURE 排除出屏幕抓取，
//! 边框和压暗不会进到拼接结果里。

use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindow, GetWindowLongPtrW, GetWindowThreadProcessId, PostMessageW, PostThreadMessageW,
    RegisterClassExW, SetTimer, SetWindowDisplayAffinity, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, WindowFromPoint, GWL_EXSTYLE, GW_HWNDNEXT, HCURSOR, HICON, MSG,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WINDOW_DISPLAY_AFFINITY, WM_MOUSEWHEEL, WM_NCHITTEST, WM_QUIT,
    WM_TIMER, WNDCLASSEXW, CS_HREDRAW, CS_VREDRAW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, HTTRANSPARENT,
};

use crate::capture::geometry::MonitorGeom;
use crate::capture::session::RegionSelection;

const BORDER: i32 = 3;
const DIM_ALPHA: u8 = 72;

pub(crate) struct Guard {
    hwnd: HWND,
    tid: u32,
    thread: Option<std::thread::JoinHandle<()>>,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
    region_x: u32,
    region_y: u32,
    region_w: u32,
    region_h: u32,
}

impl Guard {
    pub(crate) fn open(monitor: &MonitorGeom, region: &RegionSelection) -> Option<Self> {
        let width = monitor.physical_width.max(1);
        let height = monitor.physical_height.max(1);
        let origin_x = monitor.physical_x;
        let origin_y = monitor.physical_y;
        let scroll_x = origin_x + region.x as i32 + region.width as i32 / 2;
        let scroll_y = origin_y + region.y as i32 + region.height as i32 / 2;
        let (tx, rx) = std::sync::mpsc::channel::<Option<(isize, u32)>>();
        let thread = std::thread::Builder::new()
            .name("cropmark-scroll-ui".into())
            .spawn(move || ui_thread(origin_x, origin_y, width, height, scroll_x, scroll_y, tx))
            .ok()?;
        let (hwnd_raw, tid) = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .ok()
            .flatten()?;
        if hwnd_raw == 0 {
            let _ = thread.join();
            return None;
        }
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        let guard = Self {
            hwnd,
            tid,
            thread: Some(thread),
            origin_x,
            origin_y,
            width,
            height,
            region_x: region.x,
            region_y: region.y,
            region_w: region.width,
            region_h: region.height,
        };
        guard.paint(region.height);
        Some(guard)
    }

    pub(crate) fn update(&self, stitched_height: u32) {
        self.paint(stitched_height);
    }

    fn paint(&self, stitched_height: u32) {
        let pixels = (self.width as usize).saturating_mul(self.height as usize);
        let mut rgba = vec![0u8; pixels.saturating_mul(4)];
        let region = region_box(self.region_x, self.region_y, self.region_w, self.region_h);
        dim_outside(&mut rgba, self.width, self.height, &region);
        stroke_rect(&mut rgba, self.width, self.height, &region);
        paint_label(&mut rgba, self.width, self.height, &region, stitched_height);
        premultiply(&mut rgba);
        unsafe {
            let _ = present(
                self.hwnd,
                self.origin_x,
                self.origin_y,
                self.width,
                self.height,
                &rgba,
            );
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.tid, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn ui_thread(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scroll_x: i32,
    scroll_y: i32,
    ready: std::sync::mpsc::Sender<Option<(isize, u32)>>,
) {
    SCROLL_X.store(scroll_x, Ordering::SeqCst);
    SCROLL_Y.store(scroll_y, Ordering::SeqCst);
    let Some(hwnd) = (unsafe { create_window(x, y, width, height) }) else {
        let _ = ready.send(None);
        return;
    };
    // 约每 400ms 给框中心下面的窗口发一次向下滚轮。不抢焦点，编辑光标就不会闪。
    unsafe {
        let _ = SetTimer(Some(hwnd), 1, 400, None);
    }
    let tid = unsafe { GetCurrentThreadId() };
    if ready.send(Some((hwnd.0 as isize, tid))).is_err() {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    }
}

fn region_box(x: u32, y: u32, width: u32, height: u32) -> RegionSelection {
    RegionSelection { x, y, width, height }
}

fn dim_outside(rgba: &mut [u8], width: u32, height: u32, region: &RegionSelection) {
    let x0 = region.x.min(width);
    let y0 = region.y.min(height);
    let x1 = region.x.saturating_add(region.width).min(width);
    let y1 = region.y.saturating_add(region.height).min(height);
    for y in 0..height {
        for x in 0..width {
            if x >= x0 && x < x1 && y >= y0 && y < y1 {
                continue;
            }
            let i = (y as usize * width as usize + x as usize) * 4;
            rgba[i + 3] = DIM_ALPHA;
        }
    }
}

fn stroke_rect(rgba: &mut [u8], width: u32, height: u32, region: &RegionSelection) {
    let x0 = region.x as i32;
    let y0 = region.y as i32;
    let x1 = x0 + region.width as i32 - 1;
    let y1 = y0 + region.height as i32 - 1;
    let color = [0x1D, 0x4E, 0xD8, 255];
    for t in 0..BORDER {
        hline(rgba, width, height, x0 - t, x1 + t, y0 - t, color);
        hline(rgba, width, height, x0 - t, x1 + t, y1 + t, color);
        vline(rgba, width, height, x0 - t, y0 - t, y1 + t, color);
        vline(rgba, width, height, x1 + t, y0 - t, y1 + t, color);
    }
}

fn paint_label(
    rgba: &mut [u8],
    width: u32,
    height: u32,
    region: &RegionSelection,
    stitched_height: u32,
) {
    let label = if stitched_height > region.height {
        format!("{}x{} +{}", region.width, region.height, stitched_height - region.height)
    } else {
        format!("{}x{}", region.width, region.height)
    };
    let scale = 2i32;
    let box_w = label.chars().count() as i32 * (5 * scale + scale) + 8 * scale;
    let box_h = 7 * scale + 8 * scale;
    let x = (region.x as i32).clamp(8, (width as i32 - box_w - 8).max(8));
    let above = region.y as i32 - box_h - 8;
    let y = if above >= 8 {
        above
    } else {
        (region.y as i32 + 8).min((height as i32 - box_h - 8).max(8))
    };
    fill_rect(rgba, width, height, x, y, box_w, box_h, [0x1C, 0x21, 0x28, 230]);
    draw_label(rgba, width, height, x + 4 * scale, y + 4 * scale, &label, scale);
}

fn draw_label(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, text: &str, scale: i32) {
    let mut cursor = x;
    for ch in text.chars() {
        blit_glyph(rgba, width, height, cursor, y, glyph(ch), scale);
        cursor += 6 * scale;
    }
}

fn blit_glyph(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, rows: [u8; 7], scale: i32) {
    for (row, bits) in rows.iter().enumerate() {
        for col in 0..5 {
            if bits & (1 << (4 - col)) == 0 {
                continue;
            }
            fill_rect(
                rgba,
                width,
                height,
                x + col * scale,
                y + row as i32 * scale,
                scale,
                scale,
                [0xF8, 0xFA, 0xFC, 255],
            );
        }
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        'x' => [0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b00000],
        '+' => [0b00000, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0b00000],
        _ => [0, 0, 0, 0, 0, 0, 0],
    }
}

fn fill_rect(rgba: &mut [u8], width: u32, height: u32, x: i32, y: i32, w: i32, h: i32, color: [u8; 4]) {
    for py in y..y + h {
        hline(rgba, width, height, x, x + w - 1, py, color);
    }
}

fn hline(rgba: &mut [u8], width: u32, height: u32, x0: i32, x1: i32, y: i32, color: [u8; 4]) {
    if y < 0 || y >= height as i32 {
        return;
    }
    let (mut a, mut b) = (x0.min(x1), x0.max(x1));
    a = a.max(0);
    b = b.min(width as i32 - 1);
    for x in a..=b {
        put(rgba, width, x, y, color);
    }
}

fn vline(rgba: &mut [u8], width: u32, height: u32, x: i32, y0: i32, y1: i32, color: [u8; 4]) {
    if x < 0 || x >= width as i32 {
        return;
    }
    let (mut a, mut b) = (y0.min(y1), y0.max(y1));
    a = a.max(0);
    b = b.min(height as i32 - 1);
    for y in a..=b {
        put(rgba, width, x, y, color);
    }
}

fn put(rgba: &mut [u8], width: u32, x: i32, y: i32, color: [u8; 4]) {
    if x < 0 || y < 0 {
        return;
    }
    let i = (y as usize * width as usize + x as usize) * 4;
    if i + 3 >= rgba.len() {
        return;
    }
    rgba[i..i + 4].copy_from_slice(&color);
}

fn premultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        px[0] = (u16::from(px[0]) * a / 255) as u8;
        px[1] = (u16::from(px[1]) * a / 255) as u8;
        px[2] = (u16::from(px[2]) * a / 255) as u8;
    }
}

unsafe fn create_window(x: i32, y: i32, width: u32, height: u32) -> Option<HWND> {
    let class = class_name();
    let instance = GetModuleHandleW(None).ok()?;
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        WS_POPUP,
        x,
        y,
        width as i32,
        height as i32,
        None,
        None,
        Some(instance.into()),
        None,
    )
    .ok()?;
    let _ = SetWindowDisplayAffinity(hwnd, WINDOW_DISPLAY_AFFINITY(0x11));
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Some(hwnd)
}

fn class_name() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| {
        let name: Vec<u16> = "CropmarkScrollFrame\0".encode_utf16().collect();
        let instance = unsafe { GetModuleHandleW(None) };
        if let Ok(instance) = instance {
            let class = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                hIcon: HICON::default(),
                hCursor: HCURSOR::default(),
                lpszClassName: PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            unsafe {
                let _ = RegisterClassExW(&class);
            }
        }
        name
    })
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 框本身不接鼠标。点中测试若落到这里，也要穿透，否则下面的窗口滚不动。
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    if msg == WM_TIMER {
        nudge_scroll();
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

static SCROLL_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SCROLL_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// 给框中心下面的窗口发一格向下滚轮。不移动鼠标，也不抢焦点。
fn nudge_scroll() {
    let x = SCROLL_X.load(Ordering::SeqCst);
    let y = SCROLL_Y.load(Ordering::SeqCst);
    let hwnd = window_under(POINT { x, y });
    if hwnd.0.is_null() {
        return;
    }
    // 负值表示向下。坐标是屏幕坐标。
    let delta: i32 = -120;
    let packed = ((y as u32) << 16) | (x as u16 as u32);
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_MOUSEWHEEL,
            WPARAM(((delta as u32) << 16) as usize),
            LPARAM(packed as isize),
        );
    }
}

fn window_under(pt: POINT) -> HWND {
    unsafe {
        let mut hwnd = WindowFromPoint(pt);
        let ours = our_pid();
        for _ in 0..8 {
            if hwnd.0.is_null() {
                return hwnd;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if pid != ours && ex & WS_EX_TRANSPARENT.0 == 0 {
                return hwnd;
            }
            let Ok(next) = GetWindow(hwnd, GW_HWNDNEXT) else {
                break;
            };
            if next.0.is_null() || next == hwnd {
                break;
            }
            hwnd = next;
        }
        HWND::default()
    }
}

fn our_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

unsafe fn present(
    hwnd: HWND,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> windows::core::Result<()> {
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let dib = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
    let previous = SelectObject(mem, dib.into());
    let dst = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
    for (src, out) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        out[0] = src[2];
        out[1] = src[1];
        out[2] = src[0];
        out[3] = src[3];
    }
    let mut origin = POINT {
        x: origin_x,
        y: origin_y,
    };
    let mut size = windows::Win32::Foundation::SIZE {
        cx: width as i32,
        cy: height as i32,
    };
    let mut source = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = UpdateLayeredWindow(
        hwnd,
        Some(screen),
        Some(&mut origin),
        Some(&mut size),
        Some(mem),
        Some(&mut source),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );
    SelectObject(mem, previous);
    let _ = DeleteObject(dib.into());
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    let _ = info;
    Ok(())
}
