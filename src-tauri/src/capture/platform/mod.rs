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

/// R1:连续抓取(长截图)是否可用。Wayland/portal 只有一次性截图能力,
/// 无法持续获取屏幕内容,在进入选区前明确失败而不是静默输出错误拼接。
#[cfg(any(target_os = "linux", test))]
pub fn scroll_capture_supported_for(backend: LinuxCaptureBackend) -> bool {
    backend == LinuxCaptureBackend::X11
}

/// 本次采集是否把系统指针画进结果。默认关闭,输出与现状一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    Off,
    /// 仅当指针热点落在采集范围内时写入。
    WhenInside,
}

/// 指针合成结果。`Unavailable` 表示平台拿不到指针图像,调用方提示且仍交付画面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorOutcome {
    NotRequested,
    Included,
    Outside,
    /// 平台自己画了指针(macOS 显示器路径的 showsCursor),不额外提示。
    Native,
    Unavailable,
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

    pub fn capture_monitor_with_cursor(
        monitor: &MonitorGeom,
        _mode: CursorMode,
    ) -> Result<(Frame, CursorOutcome), CaptureError> {
        capture_monitor(monitor).map(|frame| (frame, CursorOutcome::NotRequested))
    }

    pub fn capture_display(
        monitor: &MonitorGeom,
        _siblings: &[MonitorGeom],
        mode: CursorMode,
    ) -> Result<(Frame, CursorOutcome), CaptureError> {
        capture_monitor_with_cursor(monitor, mode)
    }

    pub fn capture_window_with_cursor(
        id: &str,
        _mode: CursorMode,
    ) -> Result<(Frame, CursorOutcome), CaptureError> {
        capture_window(id).map(|frame| (frame, CursorOutcome::NotRequested))
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

    /// R1:无平台连续抓取实现。
    pub fn scroll_capture_supported() -> bool {
        false
    }
}

pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
    backend::pointer_monitor()
}

pub fn capture_monitor(monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
    backend::capture_monitor(monitor)
}

pub fn capture_monitor_with_cursor(
    monitor: &MonitorGeom,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    backend::capture_monitor_with_cursor(monitor, mode)
}

/// 按显示器几何抓取指定屏(拼接与托盘「指定显示器」)。
/// macOS 按 SCK 帧匹配,不用指针所在屏。
pub fn capture_display(
    monitor: &MonitorGeom,
    siblings: &[MonitorGeom],
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    backend::capture_display(monitor, siblings, mode)
}

pub fn list_windows(self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
    backend::list_windows(self_pid)
}

pub fn capture_window(id: &str) -> Result<Frame, CaptureError> {
    backend::capture_window(id)
}

pub fn capture_window_with_cursor(
    id: &str,
    mode: CursorMode,
) -> Result<(Frame, CursorOutcome), CaptureError> {
    if mode == CursorMode::Off {
        return capture_window(id).map(|frame| (frame, CursorOutcome::NotRequested));
    }
    backend::capture_window_with_cursor(id, mode)
}

/// 多屏拼接时合并各屏的指针结果:画进任一屏就不提示;全都拿不到才降级。
pub fn merge_cursor_outcome(left: CursorOutcome, right: CursorOutcome) -> CursorOutcome {
    use CursorOutcome::*;
    match (left, right) {
        (Included, _) | (_, Included) => Included,
        (Native, _) | (_, Native) => Native,
        (Unavailable, _) | (_, Unavailable) => Unavailable,
        (Outside, _) | (_, Outside) => Outside,
        (NotRequested, NotRequested) => NotRequested,
    }
}

pub fn dismiss_tray_popup() {
    backend::dismiss_tray_popup();
}

pub fn tray_popup_visible() -> bool {
    backend::tray_popup_visible()
}

/// R1:长截图滚动会话可用的平台能力查询;不可用时入口操作前明确失败。
pub fn scroll_capture_supported() -> bool {
    backend::scroll_capture_supported()
}

pub fn self_pid() -> u32 {
    std::process::id()
}

pub fn enable_per_monitor_v2() {
    #[cfg(windows)]
    win::enable_per_monitor_v2();
}

/// Wayland 门户的整桌面帧。不可用或只覆盖单屏时由调用方明确失败。
#[cfg(target_os = "linux")]
pub fn capture_portal_desktop() -> Result<Frame, CaptureError> {
    linux::capture_portal_desktop()
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

    #[test]
    fn scroll_capture_requires_a_backend_with_continuous_grabs() {
        // Wayland/portal 只有一次性截图,长截图必须在开始前失败。
        assert!(!scroll_capture_supported_for(LinuxCaptureBackend::Portal));
        assert!(scroll_capture_supported_for(LinuxCaptureBackend::X11));
    }

    #[test]
    fn merged_cursor_outcome_toasts_only_when_nothing_was_drawn() {
        assert_eq!(
            merge_cursor_outcome(CursorOutcome::Outside, CursorOutcome::Included),
            CursorOutcome::Included
        );
        assert_eq!(
            merge_cursor_outcome(CursorOutcome::Unavailable, CursorOutcome::Outside),
            CursorOutcome::Unavailable
        );
        assert_eq!(
            merge_cursor_outcome(CursorOutcome::Native, CursorOutcome::Outside),
            CursorOutcome::Native
        );
        assert_eq!(
            merge_cursor_outcome(CursorOutcome::Included, CursorOutcome::Unavailable),
            CursorOutcome::Included
        );
    }
}
