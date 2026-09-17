use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Position, Size, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};

use super::buffer::{encode_png, fit_display, Frame};
use super::error::CaptureError;
use super::geometry::MonitorGeom;
use super::windows_list::ListedWindow;
use crate::hotkeys::CaptureMode;

pub const OVERLAY: &str = "overlay";
pub const PREVIEW: &str = "preview";
pub const DELAY: &str = "capture-delay";
pub const ERROR: &str = "capture-error";
pub const SETTINGS: &str = "settings";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayPayload {
    pub mode: CaptureMode,
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub logical_width: u32,
    pub logical_height: u32,
    pub windows: Vec<ListedWindow>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewPayload {
    pub png_base64: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelayPayload {
    pub delay_ms: u64,
    pub mode: CaptureMode,
}

pub fn product_window_labels() -> [&'static str; 2] {
    [PREVIEW, SETTINGS]
}

pub fn session_window_labels() -> [&'static str; 3] {
    [OVERLAY, DELAY, ERROR]
}

pub fn hide_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.set_always_on_top(false);
        let _ = window.hide();
    }
}

pub fn show_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

pub fn close_window(app: &AppHandle, label: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.close();
    }
}

pub fn is_visible(app: &AppHandle, label: &str) -> bool {
    app.get_webview_window(label)
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false)
}

pub fn any_visible(app: &AppHandle, labels: &[&str]) -> bool {
    labels.iter().any(|label| is_visible(app, label))
}

pub fn overlay_payload(mode: CaptureMode, frame: &Frame, monitor: &MonitorGeom, windows: Vec<ListedWindow>) -> Result<OverlayPayload, CaptureError> {
    let windows = windows
        .into_iter()
        .map(|mut window| {
            window.x -= monitor.physical_x;
            window.y -= monitor.physical_y;
            window
        })
        .collect();
    let (width, height) = fit_display(monitor.logical_width, monitor.logical_height, 1280);
    let overlay = super::buffer::resize_rgba(frame, width, height)?;
    Ok(OverlayPayload {
        mode,
        png_base64: STANDARD.encode(super::buffer::encode_jpeg(&overlay, 70)?),
        width: frame.width,
        height: frame.height,
        scale: frame.scale,
        logical_width: monitor.logical_width,
        logical_height: monitor.logical_height,
        windows,
    })
}

pub fn preview_payload(frame: &Frame) -> Result<PreviewPayload, CaptureError> {
    Ok(PreviewPayload {
        png_base64: STANDARD.encode(encode_png(frame)?),
        width: frame.width,
        height: frame.height,
        scale: frame.scale,
    })
}

pub fn precreate(app: &AppHandle) {
    let _ = ensure_window(app, OVERLAY, "overlay", 320.0, 240.0, false, true);
    let _ = ensure_window(app, PREVIEW, "preview", 520.0, 360.0, false, false);
}

pub fn open_overlay(app: &AppHandle, monitor: &MonitorGeom) -> Result<WebviewWindow, CaptureError> {
    let window = ensure_window(app, OVERLAY, "overlay", 320.0, 240.0, false, true)?;
    let _ = window.set_position(Position::Physical(PhysicalPosition {
        x: monitor.physical_x,
        y: monitor.physical_y,
    }));
    let _ = window.set_size(Size::Logical(LogicalSize {
        width: monitor.logical_width.max(1) as f64,
        height: monitor.logical_height.max(1) as f64,
    }));
    let _ = window.set_always_on_top(true);
    let _ = window.show();
    let _ = window.set_focus();
    let _ = window.emit("overlay-reload", ());
    Ok(window)
}

pub fn open_preview(app: &AppHandle, frame: &Frame) -> Result<WebviewWindow, CaptureError> {
    hide_window(app, OVERLAY);
    let (width, height) = preview_size(frame);
    let window = ensure_window(app, PREVIEW, "preview", width, height, false, false)?;
    let _ = window.set_size(Size::Logical(LogicalSize { width, height }));
    let _ = window.center();
    let _ = window.set_skip_taskbar(false);
    let _ = window.set_always_on_top(true);
    let _ = window.show();
    let _ = window.set_focus();
    let _ = window.set_always_on_top(true);
    let _ = window.emit("preview-reload", ());
    Ok(window)
}

pub fn open_delay(app: &AppHandle, delay_ms: u64) -> Result<WebviewWindow, CaptureError> {
    close_window(app, DELAY);
    let window = builder(app, DELAY, "delay", true, true)?
        .inner_size(280.0, 88.0)
        .always_on_top(true)
        .focused(false)
        .visible(true)
        .center()
        .build()
        .map_err(|error| CaptureError::api(error.to_string()))?;
    let _ = window.emit("capture-delay", DelayPayload { delay_ms, mode: CaptureMode::Region });
    Ok(window)
}

pub fn open_error(app: &AppHandle, error: &CaptureError) -> Result<(), CaptureError> {
    close_window(app, ERROR);
    let window = builder(app, ERROR, "error", true, true)?
        .inner_size(420.0, 220.0)
        .center()
        .always_on_top(true)
        .focused(true)
        .visible(true)
        .build()
        .map_err(|err| CaptureError::api(err.to_string()))?;
    let _ = window.emit("capture-error", error.clone());
    Ok(())
}

fn ensure_window(
    app: &AppHandle,
    label: &str,
    view: &str,
    width: f64,
    height: f64,
    transparent: bool,
    skip_taskbar: bool,
) -> Result<WebviewWindow, CaptureError> {
    if let Some(window) = app.get_webview_window(label) {
        return Ok(window);
    }
    builder(app, label, view, transparent, skip_taskbar)?
        .inner_size(width, height)
        .visible(false)
        .always_on_top(true)
        .visible_on_all_workspaces(label == OVERLAY)
        .build()
        .map_err(|error| CaptureError::api(error.to_string()))
}

fn builder<'a>(
    app: &'a AppHandle,
    label: &str,
    view: &str,
    transparent: bool,
    skip_taskbar: bool,
) -> Result<WebviewWindowBuilder<'a, tauri::Wry, AppHandle>, CaptureError> {
    Ok(WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App(format!("index.html?view={view}").into()),
    )
    .title("Cropmark")
    .decorations(false)
    .transparent(transparent)
    .shadow(!transparent)
    .skip_taskbar(skip_taskbar)
    .resizable(label == PREVIEW)
    .maximizable(false)
    .minimizable(false)
    .closable(true))
}

fn preview_size(frame: &Frame) -> (f64, f64) {
    let max_w = 800.0;
    let max_h = 600.0;
    let chrome = 108.0;
    let width = frame.width.max(1) as f64;
    let height = frame.height.max(1) as f64;
    let scale = (max_w / width).min((max_h - chrome) / height).min(1.0);
    ((width * scale).max(480.0), (height * scale + chrome).max(280.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::Frame;

    #[test]
    fn preview_keeps_compact_panel() {
        let frame = Frame {
            width: 3840,
            height: 2160,
            rgba: vec![0; 4],
            scale: 2.0,
        };
        let (width, height) = preview_size(&frame);
        assert!(width <= 800.0);
        assert!(height <= 600.0);
        assert!(width >= 480.0);
    }
}
