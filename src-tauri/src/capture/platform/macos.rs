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

#[repr(C)]
struct CropmarkSckDisplay {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    scale: f64,
}

extern "C" {
    fn cropmark_sck_free(out: *mut CropmarkSckResult);
    fn cropmark_sck_pointer(x: *mut i32, y: *mut i32) -> i32;
    fn cropmark_sck_monitor_at_pointer(out: *mut CropmarkSckMonitor) -> i32;
    fn cropmark_sck_capture_at_point(
        px: i32,
        py: i32,
        shows_cursor: i32,
        out: *mut CropmarkSckResult,
    ) -> i32;
    fn cropmark_sck_capture_window(window_id: u32, out: *mut CropmarkSckResult) -> i32;
    fn cropmark_sck_list_windows(out: *mut CropmarkSckWindow, cap: i32, count: *mut i32) -> i32;
    fn cropmark_sck_list_displays(out: *mut CropmarkSckDisplay, cap: i32, count: *mut i32) -> i32;
    fn cropmark_sck_capture_display_frame(
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        shows_cursor: i32,
        out: *mut CropmarkSckResult,
    ) -> i32;
    fn cropmark_sck_permission_state() -> i32;
}

use super::{CursorMode, CursorOutcome};
use crate::capture::geometry::{match_sck_display, primary_points_height, DisplayFrame};

/// 屏幕录制权限状态(R23):由 C 桥的 preflight + 每进程请求记录派生。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenPermissionState {
    Authorized,
    /// 未授权且本进程尚未请求:首次请求会弹系统授权。
    NotRequested,
    /// 未授权且已请求过:不会再弹窗,需系统设置开启后重启。
    Denied,
}

pub fn screen_permission_state() -> ScreenPermissionState {
    permission_state_from(unsafe { cropmark_sck_permission_state() })
}

fn permission_state_from(code: i32) -> ScreenPermissionState {
    match code {
        0 => ScreenPermissionState::Authorized,
        1 => ScreenPermissionState::NotRequested,
        _ => ScreenPermissionState::Denied,
    }
}

fn timing_enabled() -> bool {
    std::env::var_os("CROPMARK_CAPTURE_TIMING").is_some()
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
        scale: if monitor.scale > 0.0 {
            monitor.scale
        } else {
            1.0
        },
    })
}

pub fn capture_monitor(monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
    capture_monitor_with_cursor(monitor, CursorMode::Off).map(|(frame, _)| frame)
}

pub fn capture_monitor_with_cursor(
    monitor: &MonitorGeom,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    let shows = matches!(mode, CursorMode::WhenInside);
    let frame = capture_pointer_display(monitor, shows)?;
    let outcome = if shows {
        CursorOutcome::Native
    } else {
        CursorOutcome::NotRequested
    };
    Ok((frame, outcome))
}

pub fn capture_display(
    monitor: &MonitorGeom,
    siblings: &[MonitorGeom],
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    let shows = matches!(mode, CursorMode::WhenInside);
    let frame = capture_matched_display(monitor, siblings, shows)?;
    let outcome = if shows {
        CursorOutcome::Native
    } else {
        CursorOutcome::NotRequested
    };
    Ok((frame, outcome))
}

fn capture_pointer_display(monitor: &MonitorGeom, shows_cursor: bool) -> Result<Frame, CaptureError> {
    let mut px = monitor.logical_x + (monitor.logical_width as i32 / 2);
    let mut py = monitor.logical_y + (monitor.logical_height as i32 / 2);
    unsafe {
        let _ = cropmark_sck_pointer(&mut px, &mut py);
    }
    if timing_enabled() {
        eprintln!(
            "Cropmark macos sck: capture start point=({px},{py}) logical={}x{} scale={} permission={:?}",
            monitor.logical_width,
            monitor.logical_height,
            monitor.scale,
            screen_permission_state()
        );
    }
    let started = std::time::Instant::now();
    let shows = i32::from(shows_cursor);
    let result = take_result(
        |out| unsafe { cropmark_sck_capture_at_point(px, py, shows, out) },
        monitor.scale,
    );
    if timing_enabled() {
        match &result {
            Ok(frame) => eprintln!(
                "Cropmark macos sck: capture done {}x{} elapsed={:?}",
                frame.width,
                frame.height,
                started.elapsed()
            ),
            Err(error) => eprintln!(
                "Cropmark macos sck: capture failed kind={:?} message={} elapsed={:?}",
                error.kind,
                error.message,
                started.elapsed()
            ),
        }
    }
    result
}

fn capture_matched_display(
    monitor: &MonitorGeom,
    siblings: &[MonitorGeom],
    shows_cursor: bool,
) -> Result<Frame, CaptureError> {
    let mut raw = vec![
        CropmarkSckDisplay {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            scale: 1.0,
        };
        16
    ];
    let mut count = 0i32;
    let status =
        unsafe { cropmark_sck_list_displays(raw.as_mut_ptr(), raw.len() as i32, &mut count) };
    if status == 1 {
        return Err(classify_platform_failure(PlatformFailure::PermissionDenied));
    }
    if status == 3 {
        return Err(CaptureError::timeout(
            "error.capture.sck_timeout",
            "error.capture.timeout_hint",
        ));
    }
    if status != 0 {
        return Err(CaptureError::unavailable("error.capture.no_monitor"));
    }
    let listed = count.max(0) as usize;
    let displays: Vec<DisplayFrame> = raw
        .iter()
        .take(listed)
        .map(|display| DisplayFrame {
            x: display.x,
            y: display.y,
            width: display.w,
            height: display.h,
        })
        .collect();
    let height = primary_points_height(if siblings.is_empty() {
        std::slice::from_ref(monitor)
    } else {
        siblings
    });
    let Some(index) = match_sck_display(monitor, &displays, height) else {
        return Err(CaptureError::unavailable(
            "error.capture.monitor_unavailable",
        ));
    };
    let chosen = displays[index];
    take_result(
        |out| unsafe {
            cropmark_sck_capture_display_frame(
                chosen.x,
                chosen.y,
                chosen.width,
                chosen.height,
                i32::from(shows_cursor),
                out,
            )
        },
        monitor.scale,
    )
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
    let status =
        unsafe { cropmark_sck_list_windows(raw.as_mut_ptr(), raw.len() as i32, &mut count) };
    if status == 1 {
        return Err(classify_platform_failure(PlatformFailure::PermissionDenied));
    }
    if status == 3 {
        return Err(CaptureError::timeout(
            "error.capture.sck_timeout",
            "error.capture.timeout_hint",
        ));
    }
    if status != 0 {
        return Err(CaptureError::unavailable(
            "error.capture.screencapturekit_windows",
        ));
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
    capture_window_with_cursor(id, CursorMode::Off).map(|(frame, _)| frame)
}

pub fn capture_window_with_cursor(
    id: &str,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    let window_id = id
        .parse::<u32>()
        .map_err(|_| CaptureError::api("error.capture.window_unknown"))?;
    // 窗口路径的 SCK 独立窗口不含系统指针;开启开关时降级并保留画面。
    let frame = take_result(
        |out| unsafe { cropmark_sck_capture_window(window_id, out) },
        2.0,
    )?;
    let outcome = match mode {
        CursorMode::Off => CursorOutcome::NotRequested,
        CursorMode::WhenInside => CursorOutcome::Unavailable,
    };
    Ok((frame, outcome))
}

pub fn dismiss_tray_popup() {}

pub fn tray_popup_visible() -> bool {
    false
}

/// R1:ScreenCaptureKit 可按需重复抓屏,支持长截图滚动会话。
pub fn scroll_capture_supported() -> bool {
    true
}

fn take_result(
    call: impl FnOnce(*mut CropmarkSckResult) -> i32,
    fallback_scale: f64,
) -> Result<Frame, CaptureError> {
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
        Err(classify_platform_failure(
            PlatformFailure::BufferUninitialized,
        ))
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
        // 5=获取可共享内容超时、6=截取超时(ADR-17):可重试的明确错误。
        5 | 6 => CaptureError::timeout("error.capture.sck_timeout", "error.capture.timeout_hint"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::error::CaptureErrorKind;

    #[test]
    fn permission_state_maps_all_three_codes() {
        assert_eq!(permission_state_from(0), ScreenPermissionState::Authorized);
        assert_eq!(
            permission_state_from(1),
            ScreenPermissionState::NotRequested
        );
        assert_eq!(permission_state_from(2), ScreenPermissionState::Denied);
        // 未知代码按已拒绝处理,不误报"会弹授权"。
        assert_eq!(permission_state_from(-1), ScreenPermissionState::Denied);
        assert_eq!(permission_state_from(99), ScreenPermissionState::Denied);
    }

    #[test]
    fn sck_timeout_kinds_map_to_retryable_errors() {
        for kind in [5, 6] {
            let error = map_kind(kind, None);
            assert_eq!(error.kind, CaptureErrorKind::Timeout);
            assert!(error.message.contains("超时"), "message={}", error.message);
            assert!(error.hint.is_some());
        }
    }
}
