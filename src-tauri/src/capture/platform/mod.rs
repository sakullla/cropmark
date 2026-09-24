use super::buffer::Frame;
use super::error::CaptureError;
use super::geometry::MonitorGeom;
use super::windows_list::ListedWindow;

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxCaptureBackend {
    Portal,
    X11,
}

#[cfg(any(target_os = "linux", test))]
pub fn linux_capture_backend(wayland_display: Option<&str>) -> LinuxCaptureBackend {
    match wayland_display {
        Some(value) if !value.is_empty() => LinuxCaptureBackend::Portal,
        _ => LinuxCaptureBackend::X11,
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod win;

/// macOS 屏幕录制权限状态(R23):触发前快速判定,首次请求给出过渡提示。
#[cfg(target_os = "macos")]
pub use macos::{screen_permission_state, ScreenPermissionState};

#[cfg(target_os = "linux")]
use linux as backend;
#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(windows)]
use win as backend;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod backend {
    use super::*;

    pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
        Err(CaptureError::unavailable(
            "error.capture.platform_no_interface",
        ))
    }

    pub fn capture_monitor(_monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
        Err(CaptureError::unavailable(
            "error.capture.platform_no_interface",
        ))
    }

    pub fn list_windows(_self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
        Err(CaptureError::unavailable(
            "error.capture.platform_no_window_list",
        ))
    }

    pub fn capture_window(_id: &str) -> Result<Frame, CaptureError> {
        Err(CaptureError::unavailable(
            "error.capture.platform_no_window_capture",
        ))
    }

    pub fn dismiss_tray_popup() {}

    pub fn tray_popup_visible() -> bool {
        false
    }
}

pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
    backend::pointer_monitor()
}

pub fn capture_monitor(monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
    backend::capture_monitor(monitor)
}

pub fn list_windows(self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
    backend::list_windows(self_pid)
}

pub fn capture_window(id: &str) -> Result<Frame, CaptureError> {
    backend::capture_window(id)
}

pub fn dismiss_tray_popup() {
    backend::dismiss_tray_popup();
}

pub fn tray_popup_visible() -> bool {
    backend::tray_popup_visible()
}

pub fn self_pid() -> u32 {
    std::process::id()
}

pub fn enable_per_monitor_v2() {
    #[cfg(windows)]
    win::enable_per_monitor_v2();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_display_selects_portal_not_x11() {
        assert_eq!(
            linux_capture_backend(Some("wayland-0")),
            LinuxCaptureBackend::Portal
        );
        assert_eq!(
            linux_capture_backend(Some("wayland-1")),
            LinuxCaptureBackend::Portal
        );
    }

    #[test]
    fn x11_only_without_wayland_display() {
        assert_eq!(linux_capture_backend(None), LinuxCaptureBackend::X11);
        assert_eq!(linux_capture_backend(Some("")), LinuxCaptureBackend::X11);
    }
}
