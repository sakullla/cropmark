//! 长截图进行时留在屏幕上的范围框。
//!
//! 选区壳关闭后，如果没有可见的框，用户不知道正在截哪一块，滚轮也像没目标。
//! 这层窗口点击穿透、不抢焦点，并用 WDA_EXCLUDEFROMCAPTURE 排除出屏幕抓取，
//! 边框和压暗不会进到拼接结果里。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetAncestor, GetMessageW,
    GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW, PostThreadMessageW, RegisterClassExW, SetTimer, SetWindowDisplayAffinity,
    ShowWindow, TranslateMessage, UpdateLayeredWindow, WindowFromPoint, GA_ROOT, GWL_EXSTYLE,
    GW_HWNDNEXT, HCURSOR, HICON, MSG, SW_SHOWNOACTIVATE, ULW_ALPHA, WINDOW_DISPLAY_AFFINITY,
    WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_NCHITTEST, WM_QUIT, WM_TIMER, WNDCLASSEXW, CS_HREDRAW,
    CS_VREDRAW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP, HTTRANSPARENT,
};

use super::CaptureAxis;
use crate::capture::geometry::MonitorGeom;
use crate::capture::session::RegionSelection;

const BORDER: i32 = 3;
const DIM_ALPHA: u8 = 72;

/// 自动滚动方向:定时器线程与拼接会话线程之间共享(首个内容变化前可切换)。
static AXIS: AtomicU8 = AtomicU8::new(0);

/// 滚动轴基准长度:纵向用高度、横向用宽度,范围框标签按它显示增量。
fn base_length(axis: CaptureAxis, region: &RegionSelection) -> u32 {
    match axis {
        CaptureAxis::Vertical => region.height,
        CaptureAxis::Horizontal => region.width,
    }
}

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
    pub(crate) fn open(
        monitor: &MonitorGeom,
        region: &RegionSelection,
        axis: CaptureAxis,
    ) -> Option<Self> {
        AXIS.store(axis.as_u8(), Ordering::SeqCst);
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
        guard.paint(base_length(axis, region));
        Some(guard)
    }

    /// 会话在首个内容变化前切换滚动轴:范围框标签与自动滚动方向同步切换。
    pub(crate) fn set_axis(&self, axis: CaptureAxis) {
        AXIS.store(axis.as_u8(), Ordering::SeqCst);
    }

    pub(crate) fn update(&self, stitched_length: u32) {
        self.paint(stitched_length);
    }

    fn paint(&self, stitched_length: u32) {
        let pixels = (self.width as usize).saturating_mul(self.height as usize);
        let mut rgba = vec![0u8; pixels.saturating_mul(4)];
        let region = region_box(self.region_x, self.region_y, self.region_w, self.region_h);
        dim_outside(&mut rgba, self.width, self.height, &region);
        stroke_rect(&mut rgba, self.width, self.height, &region);
        let axis = CaptureAxis::from_u8(AXIS.load(Ordering::SeqCst));
        let base = base_length(axis, &region);
        paint_label(
            &mut rgba,
            self.width,
            self.height,
            &region,
            base,
            stitched_length,
        );
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
    base_length: u32,
    stitched_length: u32,
) {
    let label = if stitched_length > base_length {
        format!(
            "{}x{} +{}",
            region.width,
            region.height,
            stitched_length - base_length
        )
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

/// 自动滚动指令:纵向发 WM_MOUSEWHEEL 向下(负值),横向发 WM_MOUSEHWHEEL
/// 向右(正值)。
fn nudge_message(axis: CaptureAxis) -> (u32, i32) {
    match axis {
        CaptureAxis::Vertical => (WM_MOUSEWHEEL, -120),
        CaptureAxis::Horizontal => (WM_MOUSEHWHEEL, 120),
    }
}

/// 给框中心下面的窗口发一格滚动指令。不移动鼠标，也不抢焦点。
fn nudge_scroll() {
    let x = SCROLL_X.load(Ordering::SeqCst);
    let y = SCROLL_Y.load(Ordering::SeqCst);
    let point = POINT { x, y };
    let Some(hwnd) = scroll_target(point) else {
        return;
    };
    let (message, delta) = nudge_message(CaptureAxis::from_u8(AXIS.load(Ordering::SeqCst)));
    // 高字为有符号位移,低字保留;坐标是屏幕坐标。
    let packed = ((y as u32) << 16) | (x as u16 as u32);
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            message,
            WPARAM(((delta as u32) << 16) as usize),
            LPARAM(packed as isize),
        );
    }
}

/// 滚轮目标：鼠标真实落在的那个窗口。命中的窗口属于我们自己的范围框/控制窗时，
/// 才从它的顶层沿 Z 序往下找第一个「可见、覆盖该点、不属于我们」的顶层窗口
/// （范围框区域内的像素 alpha 为 0，命中测试通常会直接穿透到下面的内容窗口）。
///
/// 不能按 WS_EX_TRANSPARENT 过滤命中窗口：Chromium 系的页面子窗
/// （Chrome_RenderWidgetHostHWND）就带这个风格，过滤掉会让 Z 序回退落到子窗
/// 的兄弟链上一路走到空——滚轮发不出去，自动滚动看起来「截取不到内容」。
fn scroll_target(pt: POINT) -> Option<HWND> {
    unsafe {
        let hit = WindowFromPoint(pt);
        if hit.0.is_null() {
            return None;
        }
        if !belongs_to_us(hit) {
            return Some(hit);
        }
        // 命中的是我们自己的穿透范围框：它下面的窗口才是要滚的对象。
        let root = GetAncestor(hit, GA_ROOT);
        let mut hwnd = if root.0.is_null() { hit } else { root };
        for _ in 0..16 {
            if hwnd.0.is_null() {
                return None;
            }
            if !belongs_to_us(hwnd) && is_scroll_candidate(hwnd, pt) {
                return Some(hwnd);
            }
            let Ok(next) = GetWindow(hwnd, GW_HWNDNEXT) else {
                return None;
            };
            if next.0.is_null() || next == hwnd {
                return None;
            }
            hwnd = next;
        }
        None
    }
}

fn belongs_to_us(hwnd: HWND) -> bool {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid == our_pid()
}

/// Z 序回退时的候选校验：可见、点落在窗口矩形内，且自身不是点击穿透的覆盖层
/// （顶层窗口层面的保守过滤；命中窗口不做这个过滤，避免误伤页面子窗）。
fn is_scroll_candidate(hwnd: HWND, pt: POINT) -> bool {
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 {
            return false;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return false;
        }
        pt.x >= rect.left && pt.y >= rect.top && pt.x < rect.right && pt.y < rect.bottom
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

/// 自动滚动指令按方向选择消息:纵向向下滚、横向向右滚(R5)。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scroll_uses_the_horizontal_wheel_for_the_horizontal_axis() {
        assert_eq!(nudge_message(CaptureAxis::Vertical), (WM_MOUSEWHEEL, -120));
        assert_eq!(nudge_message(CaptureAxis::Horizontal), (WM_MOUSEHWHEEL, 120));
    }
}

/// 真机回归（手动，`#[ignore]`）：范围框在屏幕上时，滚轮必须发到框内内容窗口。
///
/// 目标窗口按真实页面容器的形状搭建：顶层窗口 + 覆盖整个客户区、带
/// `WS_EX_TRANSPARENT` 的子窗（Chromium 的 `Chrome_RenderWidgetHostHWND`
/// 就带这个风格）。修复前 `window_under` 会跳过该子窗并沿子窗兄弟链走到空，
/// 滚轮发不出去；本测试断言目标窗口（另一个进程）按 `-120` 的位移和屏幕
/// 坐标收到了滚轮。
#[cfg(all(test, windows))]
mod live_target {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::core::w;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, FindWindowW, GetParent, PeekMessageW,
        RegisterClassExW, SendMessageW, ShowWindow, TranslateMessage, MSG, PM_REMOVE, SW_SHOW,
        WM_MOUSEWHEEL, WNDCLASSEXW, WS_CHILD, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
        WS_VISIBLE,
    };

    use super::*;
    use crate::capture::platform;

    /// 子进程模式标记与窗口标题；父进程按标题找到目标窗口。
    const SPEC_ENV: &str = "CROPMARK_SCROLL_TARGET_SPEC";
    const PARENT_CLASS: windows::core::PCWSTR = w!("CropmarkScrollTargetWnd");
    const CHILD_CLASS: windows::core::PCWSTR = w!("CropmarkScrollTargetPage");
    const TITLE: windows::core::PCWSTR = w!("CropmarkScrollTarget");

    unsafe extern "system" fn target_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_MOUSEWHEEL {
            let delta = ((wparam.0 as u32) >> 16) as u16 as i16;
            let x = (lparam.0 as u32 & 0xFFFF) as u16 as i16;
            let y = ((lparam.0 as u32 >> 16) & 0xFFFF) as u16 as i16;
            println!("wheel {delta} {x} {y}");
            let _ = std::io::stdout().flush();
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// 页面子窗自己不处理滚轮，转发给父窗（Chromium 页面子窗的实际行为）。
    unsafe extern "system" fn page_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_MOUSEWHEEL {
            if let Ok(parent) = GetParent(hwnd) {
                if !parent.0.is_null() {
                    SendMessageW(parent, msg, Some(wparam), Some(lparam));
                    return LRESULT(0);
                }
            }
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    fn register(class: windows::core::PCWSTR, proc_: WndprocArg) {
        unsafe {
            let Ok(instance) = GetModuleHandleW(None) else {
                return;
            };
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc_),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);
        }
    }

    type WndprocArg = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;

    /// 目标窗口子进程：`SPEC_ENV` 缺失时直接跳过（普通 `cargo test` 不做事）。
    #[test]
    fn helper_scroll_target_window() {
        let Ok(spec) = std::env::var(SPEC_ENV) else {
            return;
        };
        let numbers: Vec<i32> = spec
            .split(',')
            .filter_map(|part| part.trim().parse().ok())
            .collect();
        let [x, y, width, height, millis] = numbers[..] else {
            return;
        };
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            register(PARENT_CLASS, target_proc);
            register(CHILD_CLASS, page_proc);
            let Ok(instance) = GetModuleHandleW(None) else {
                return;
            };
            let Ok(parent) = CreateWindowExW(
                WS_EX_TOPMOST,
                PARENT_CLASS,
                TITLE,
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            ) else {
                return;
            };
            let _ = CreateWindowExW(
                WS_EX_TRANSPARENT,
                CHILD_CLASS,
                windows::core::PCWSTR::null(),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                width,
                height,
                Some(parent),
                None,
                Some(instance.into()),
                None,
            );
            let _ = ShowWindow(parent, SW_SHOW);
            let deadline = Instant::now() + Duration::from_millis(millis.max(1_000) as u64);
            let mut msg = MSG::default();
            while Instant::now() < deadline {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = DestroyWindow(parent);
        }
    }

    /// 驱动侧：像滚动会话一样把范围框放在目标窗口上，滚轮必须落到目标进程。
    #[test]
    #[ignore = "manual: opens real windows on the desktop"]
    fn wheel_reaches_the_content_window_under_the_scroll_frame() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let monitor = platform::pointer_monitor().expect("pointer monitor");
        let spec = format!(
            "{},{},420,320,8000",
            monitor.physical_x + 80,
            monitor.physical_y + 80
        );
        let mut helper = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "capture::scroll::highlight::live_target::helper_scroll_target_window",
                "--nocapture",
            ])
            .env(SPEC_ENV, &spec)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn target window process");
        let stdout = helper.stdout.take().expect("helper stdout");
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                sink.lock().expect("lines").push(line);
            }
        });

        let mut target = None;
        for _ in 0..100 {
            if let Ok(hwnd) = unsafe { FindWindowW(windows::core::PCWSTR::null(), TITLE) } {
                if !hwnd.0.is_null() {
                    target = Some(hwnd);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let target = target.expect("target window must appear");
        std::thread::sleep(Duration::from_millis(300));

        // 范围框套住目标窗口客户区中心；SCROLL_X/Y 就是后台定时器使用的滚轮落点。
        let region = RegionSelection {
            x: 120,
            y: 120,
            width: 220,
            height: 160,
        };
        let center_x = monitor.physical_x + region.x as i32 + region.width as i32 / 2;
        let center_y = monitor.physical_y + region.y as i32 + region.height as i32 / 2;
        SCROLL_X.store(center_x, Ordering::SeqCst);
        SCROLL_Y.store(center_y, Ordering::SeqCst);
        let guard =
            Guard::open(&monitor, &region, CaptureAxis::Vertical).expect("scroll frame opens");

        let found = scroll_target(POINT {
            x: center_x,
            y: center_y,
        })
        .expect("wheel target must exist under the frame");
        assert!(
            !belongs_to_us(found),
            "wheel must not be posted to our own overlay"
        );
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(found, Some(&mut pid));
        }
        assert_eq!(
            pid,
            helper.id(),
            "wheel target must be the page window of the target process"
        );
        assert_eq!(
            unsafe { GetAncestor(found, GA_ROOT) },
            target,
            "wheel target must live under the target window"
        );

        nudge_scroll();
        let mut reported = None;
        for _ in 0..80 {
            let seen = lines
                .lock()
                .expect("lines")
                .iter()
                .find(|line| line.starts_with("wheel "))
                .cloned();
            if seen.is_some() {
                reported = seen;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(guard);
        let _ = helper.kill();
        let _ = helper.wait();
        let reported = reported.expect("target window must receive the wheel");
        assert_eq!(
            reported,
            format!("wheel -120 {center_x} {center_y}"),
            "wheel must be one downward notch at the frame center"
        );
    }
}
