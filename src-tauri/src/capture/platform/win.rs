use std::mem::size_of;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    GetMonitorInfoW, MonitorFromPoint, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, MONITORINFO, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST, SRCCOPY,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EndMenu, EnumWindows, GetCursorPos, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GW_OWNER, WS_EX_TOOLWINDOW,
};

use crate::capture::buffer::{accept_buffer, Frame, RawBuffer};
use crate::capture::error::{classify_platform_failure, CaptureError, PlatformFailure};
use crate::capture::geometry::MonitorGeom;
use crate::capture::windows_list::{selectable_windows, ListedWindow};

pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
    unsafe {
        let mut point = POINT::default();
        GetCursorPos(&mut point).map_err(|_| CaptureError::api("无法读取指针位置。"))?;
        let handle = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        monitor_from_handle(handle, point.x, point.y)
    }
}

pub fn capture_monitor(monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
    unsafe {
        capture_rect(
            monitor.physical_x,
            monitor.physical_y,
            monitor.physical_width,
            monitor.physical_height,
            monitor.scale,
        )
    }
}

pub fn list_windows(self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
    let mut collected: Vec<ListedWindow> = Vec::new();
    let param = LPARAM(&mut collected as *mut Vec<ListedWindow> as isize);
    unsafe {
        EnumWindows(Some(enum_windows_callback), param)
            .map_err(|_| CaptureError::api("无法列出窗口。"))?;
    }
    Ok(selectable_windows(&collected, self_pid))
}

pub fn capture_window(id: &str) -> Result<Frame, CaptureError> {
    let hwnd = parse_hwnd(id)?;
    unsafe { capture_hwnd(hwnd) }
}

pub fn dismiss_tray_popup() {
    unsafe {
        let _ = EndMenu();
    }
}

pub fn enable_per_monitor_v2() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

unsafe fn monitor_from_handle(
    handle: windows::Win32::Graphics::Gdi::HMONITOR,
    fallback_x: i32,
    fallback_y: i32,
) -> Result<MonitorGeom, CaptureError> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if !GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO).as_bool() {
        return Err(CaptureError::api("无法读取显示器信息。"));
    }
    let rect = info.monitorInfo.rcMonitor;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        return Err(classify_platform_failure(PlatformFailure::BufferZeroSize));
    }
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    let scale = match GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) {
        Ok(()) if dpi_x > 0 => dpi_x as f64 / 96.0,
        _ => 1.0,
    };
    let name = String::from_utf16_lossy(info.szDevice.as_slice())
        .trim_end_matches('\0')
        .to_string();
    let id = if name.trim().is_empty() {
        format!("{fallback_x},{fallback_y}")
    } else {
        name
    };
    Ok(MonitorGeom::from_physical(
        id,
        rect.left,
        rect.top,
        width,
        height,
        scale,
    ))
}

unsafe fn capture_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
) -> Result<Frame, CaptureError> {
    if width == 0 || height == 0 {
        return Err(classify_platform_failure(PlatformFailure::BufferZeroSize));
    }
    let hdc_screen = GetDC(None);
    if hdc_screen.is_invalid() {
        return Err(classify_platform_failure(PlatformFailure::Api("GetDC".into())));
    }
    let captured = capture_dc(hdc_screen, |hdc_mem| {
        BitBlt(
            hdc_mem,
            0,
            0,
            width as i32,
            height as i32,
            Some(hdc_screen),
            x,
            y,
            ROP_SRCCOPY_CAPTURE,
        )
        .is_ok()
    }, width, height, scale);
    ReleaseDC(None, hdc_screen);
    captured
}

const ROP_SRCCOPY_CAPTURE: windows::Win32::Graphics::Gdi::ROP_CODE =
    windows::Win32::Graphics::Gdi::ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0);

unsafe fn capture_dc(
    hdc_screen: HDC,
    paint: impl FnOnce(HDC) -> bool,
    width: u32,
    height: u32,
    scale: f64,
) -> Result<Frame, CaptureError> {
    let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
    if hdc_mem.is_invalid() {
        return Err(classify_platform_failure(PlatformFailure::Api(
            "CreateCompatibleDC".into(),
        )));
    }
    let bitmap = CreateCompatibleBitmap(hdc_screen, width as i32, height as i32);
    if bitmap.is_invalid() {
        let _ = DeleteDC(hdc_mem);
        return Err(classify_platform_failure(PlatformFailure::BufferUninitialized));
    }
    let old = SelectObject(hdc_mem, bitmap.into());
    if !paint(hdc_mem) {
        SelectObject(hdc_mem, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(hdc_mem);
        return Err(classify_platform_failure(PlatformFailure::Api("BitBlt".into())));
    }
    let result = dibits_to_frame(hdc_mem, bitmap, width, height, scale);
    SelectObject(hdc_mem, old);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(hdc_mem);
    result
}

unsafe fn dibits_to_frame(
    hdc: HDC,
    bitmap: HBITMAP,
    width: u32,
    height: u32,
    scale: f64,
) -> Result<Frame, CaptureError> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [Default::default(); 1],
    };
    let mut bgra = vec![0u8; width as usize * height as usize * 4];
    let copied = GetDIBits(
        hdc,
        bitmap,
        0,
        height,
        Some(bgra.as_mut_ptr().cast()),
        &mut info,
        DIB_RGB_COLORS,
    );
    if copied == 0 {
        return Err(classify_platform_failure(PlatformFailure::BufferUninitialized));
    }
    let mut rgba = vec![0u8; bgra.len()];
    for (src, dst) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = 255;
    }
    let mut frame = accept_buffer(RawBuffer::ready(width, height, rgba))?;
    frame.scale = scale;
    Ok(frame)
}

unsafe fn capture_hwnd(hwnd: HWND) -> Result<Frame, CaptureError> {
    if hwnd.is_invalid() {
        return Err(CaptureError::api("窗口已不存在。"));
    }
    let mut rect = RECT::default();
    let dwm = DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut rect as *mut RECT as *mut _,
        size_of::<RECT>() as u32,
    );
    if dwm.is_err() {
        GetWindowRect(hwnd, &mut rect).map_err(|_| CaptureError::api("无法读取窗口位置。"))?;
    }
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        return Err(classify_platform_failure(PlatformFailure::BufferZeroSize));
    }
    let hdc_screen = GetDC(None);
    if hdc_screen.is_invalid() {
        return Err(classify_platform_failure(PlatformFailure::Api("GetDC".into())));
    }
    let scale = monitor_scale_at(rect.left, rect.top);
    let captured = capture_dc(
        hdc_screen,
        |hdc_mem| {
            if print_window(hwnd, hdc_mem) {
                true
            } else {
                BitBlt(
                    hdc_mem,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    Some(hdc_screen),
                    rect.left,
                    rect.top,
                    ROP_SRCCOPY_CAPTURE,
                )
                .is_ok()
            }
        },
        width,
        height,
        scale,
    );
    ReleaseDC(None, hdc_screen);
    captured
}

unsafe fn print_window(hwnd: HWND, hdc: HDC) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn PrintWindow(hwnd: HWND, hdcblt: HDC, nflags: u32) -> BOOL;
    }
    PrintWindow(hwnd, hdc, 2).as_bool()
}

unsafe fn monitor_scale_at(x: i32, y: i32) -> f64 {
    let handle = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    if GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() && dpi_x > 0 {
        dpi_x as f64 / 96.0
    } else {
        1.0
    }
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let collected = &mut *(lparam.0 as *mut Vec<ListedWindow>);
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        return BOOL::from(true);
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return BOOL::from(true);
    }
    if GetWindow(hwnd, GW_OWNER)
        .ok()
        .is_some_and(|owner| !owner.is_invalid())
    {
        return BOOL::from(true);
    }
    let mut cloaked = 0u32;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return BOOL::from(true);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL::from(true);
    }
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        return BOOL::from(true);
    }
    let mut title = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut title);
    if len <= 0 {
        return BOOL::from(true);
    }
    let title = String::from_utf16_lossy(&title[..len as usize]);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let self_pid = GetCurrentProcessId();
    collected.push(ListedWindow {
        id: format!("{}", hwnd.0 as usize),
        title,
        pid,
        x: rect.left,
        y: rect.top,
        width,
        height,
        visible: true,
        owner_is_self: pid == self_pid,
    });
    BOOL::from(true)
}

fn parse_hwnd(id: &str) -> Result<HWND, CaptureError> {
    let value = id
        .parse::<usize>()
        .map_err(|_| CaptureError::api("无法识别该窗口。"))?;
    Ok(HWND(value as *mut _))
}
