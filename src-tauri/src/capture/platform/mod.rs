use super::buffer::Frame;
use super::error::CaptureError;
use super::geometry::MonitorGeom;
use super::windows_list::ListedWindow;

#[cfg(windows)]
mod win;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;

#[cfg(windows)]
use win as backend;
#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod backend {
    use super::*;

    pub fn pointer_monitor() -> Result<MonitorGeom, CaptureError> {
        Err(CaptureError::unavailable("当前系统没有可用的截屏接口。"))
    }

    pub fn capture_monitor(_monitor: &MonitorGeom) -> Result<Frame, CaptureError> {
        Err(CaptureError::unavailable("当前系统没有可用的截屏接口。"))
    }

    pub fn list_windows(_self_pid: u32) -> Result<Vec<ListedWindow>, CaptureError> {
        Err(CaptureError::unavailable("当前系统无法列出窗口。"))
    }

    pub fn capture_window(_id: &str) -> Result<Frame, CaptureError> {
        Err(CaptureError::unavailable("当前系统无法截取窗口。"))
    }

    pub fn dismiss_tray_popup() {}
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

pub fn self_pid() -> u32 {
    std::process::id()
}

pub fn enable_per_monitor_v2() {
    #[cfg(windows)]
    win::enable_per_monitor_v2();
}
