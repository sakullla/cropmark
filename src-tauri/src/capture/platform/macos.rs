use std::ffi::CStr;
use std::os::raw::c_char;

use crate::capture::buffer::{accept_buffer, Frame, RawBuffer};
use crate::capture::error::{classify_platform_failure, CaptureError, PlatformFailure};
use crate::capture::geometry::MonitorGeom;
use crate::capture::windows_list::{selectable_windows, ListedWindow};

#[repr(C)]
struct CropmarkSckResult {
    rgba: *mut u8,
    width: u32,
    height: u32,
    error: *mut c_char,
    kind: i32,
}

#[repr(C)]
struct CropmarkSckMonitor {
    logical_x: i32,
    logical_y: i32,
    logical_w: u32,
    logical_h: u32,
    physical_x: i32,
    physical_y: i32,
    physical_w: u32,
    physical_h: u32,
    scale: f64,
}

#[repr(C)]
struct CropmarkSckWindow {
    window_id: u32,
    pid: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    title: [c_char; 512],
}

extern "C" {
    fn cropmark_sck_free(out: *mut CropmarkSckResult);
    fn cropmark_sck_pointer(x: *mut i32, y: *mut i32) -> i32;
    fn cropmark_sck_monitor_at_pointer(out: *mut CropmarkSckMonitor) -> i32;
    fn cropmark_sck_capture_at_point(px: i32, py: i32, out: *mut CropmarkSckResult) -> i32;
    fn cropmark_sck_capture_window(window_id: u32, out: *mut CropmarkSckResult) -> i32;
    fn cropmark_sck_list_windows(out: *mut CropmarkSckWindow, cap: i32, count: *mut i32) -> i32;
}

pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
    let mut monitor = CropmarkSckMonitor {
        logical_x: 0,
        logical_y: 0,
        logical_w: 0,
        logical_h: 0,
        physical_x: 0,
        physical_y: 0,
        physical_w: 0,
        physical_h: 0,
        scale: 1.0,
    };
    let status = unsafe { cropmark_sck_monitor_at_pointer(&mut monitor) };
    if status != 0 || monitor.physical_w == 0 || monitor.physical_h == 0 {
        return Err(CaptureError::unavailable("error.capture.no_monitor"));
    }
    Ok(MonitorGeom {
        id: format!("display-{},{}", monitor.logical_x, monitor.logical_y),
        logical_x: monitor.logical_x,
        logical_y: monitor.logical_y,
        logical_width: monitor.logical_w,
        logical_height: monitor.logical_h,
        physical_x: monitor.physical_x,
        physical_y: monitor.physical_y,
        physical_width: monitor.physical_w,
        physical_height: monitor.physical_h,
        scale: if monitor.scale > 0.0 { monitor.scale } else { 1.0 },
    })
}

pub fn capture_monitor(monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
    let mut px = monitor.logical_x + (monitor.logical_width as i32 / 2);
    let mut py = monitor.logical_y + (monitor.logical_height as i32 / 2);
    unsafe {
        let _ = cropmark_sck_pointer(&mut px, &mut py);
    }
    take_result(|out| unsafe { cropmark_sck_capture_at_point(px, py, out) }, monitor.scale)
}

pub fn list_windows(self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
    let mut raw: Vec<CropmarkSckWindow> = (0..256)
        .map(|_| CropmarkSckWindow {
            window_id: 0,
            pid: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            title: [0; 512],
        })
        .collect();
    let mut count = 0i32;
    let status = unsafe { cropmark_sck_list_windows(raw.as_mut_ptr(), raw.len() as i32, &mut count) };
    if status == 1 {
        return Err(classify_platform_failure(PlatformFailure::PermissionDenied));
    }
    if status != 0 {
        return Err(CaptureError::unavailable("error.capture.screencapturekit_windows"));
    }
    let mut listed = Vec::new();
    // CGWindowList 返回 front-to-back(自顶向下),符合 selectable_windows 契约。
    for item in raw.into_iter().take(count.max(0) as usize) {
        let title = unsafe { CStr::from_ptr(item.title.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        listed.push(ListedWindow {
            id: item.window_id.to_string(),
            title,
            pid: item.pid,
            x: item.x,
            y: item.y,
            width: item.width,
            height: item.height,
            visible: true,
            owner_is_self: item.pid == self_pid,
        });
    }
    Ok(selectable_windows(&listed, self_pid))
}

pub fn capture_window(id: &str) -> Result<Frame, CaptureError> {
    let window_id = id
        .parse::<u32>()
        .map_err(|_| CaptureError::api("error.capture.window_unknown"))?;
    take_result(|out| unsafe { cropmark_sck_capture_window(window_id, out) }, 2.0)
}

pub fn dismiss_tray_popup() {}

pub fn tray_popup_visible() -> bool {
    false
}

fn take_result(call: impl FnOnce(*mut CropmarkSckResult) -> i32, fallback_scale: f64) -> Result<Frame, CaptureError> {
    let mut raw = CropmarkSckResult {
        rgba: std::ptr::null_mut(),
        width: 0,
        height: 0,
        error: std::ptr::null_mut(),
        kind: 0,
    };
    let status = call(&mut raw);
    let error_text = unsafe { cstring(raw.error) };
    let kind = raw.kind;
    let result = if status != 0 || kind != 0 {
        Err(map_kind(kind, error_text))
    } else if raw.rgba.is_null() {
        Err(classify_platform_failure(PlatformFailure::BufferUninitialized))
    } else {
        let len = raw.width as usize * raw.height as usize * 4;
        let bytes = unsafe { std::slice::from_raw_parts(raw.rgba, len) }.to_vec();
        accept_buffer(RawBuffer::ready(raw.width, raw.height, bytes)).map(|mut frame| {
            frame.scale = fallback_scale;
            frame
        })
    };
    unsafe { cropmark_sck_free(&mut raw) };
    result
}

fn map_kind(kind: i32, message: Option<String>) -> CaptureError {
    match kind {
        1 => classify_platform_failure(PlatformFailure::PermissionDenied),
        3 => classify_platform_failure(PlatformFailure::BufferUninitialized),
        4 => match message {
            Some(detail) if !detail.trim().is_empty() => CaptureError::platform_message(detail),
            _ => CaptureError::unavailable("error.capture.no_interface"),
        },
        _ => classify_platform_failure(PlatformFailure::Api(message.unwrap_or_default())),
    }
}

unsafe fn cstring(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
    }
}
