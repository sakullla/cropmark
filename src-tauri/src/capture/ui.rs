use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, PhysicalPosition, Position, Size, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};

use super::buffer::{fit_display, Frame};
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

#[derive(Debug, Clone)]
pub struct PreviewPayload {
    pub bytes: Vec<u8>,
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

pub fn preview_payload(frame: &Frame, png: &[u8], clipboard_written: bool) -> PreviewPayload {
    // One binary response keeps metadata and pixels bound to the same capture.
    // Header: width/height (u32), scale (f64), copied (u32), little-endian, then PNG.
    let mut bytes = Vec::with_capacity(20 + png.len());
    bytes.extend_from_slice(&frame.width.to_le_bytes());
    bytes.extend_from_slice(&frame.height.to_le_bytes());
    bytes.extend_from_slice(&frame.scale.to_le_bytes());
    bytes.extend_from_slice(&u32::from(clipboard_written).to_le_bytes());
    bytes.extend_from_slice(png);
    PreviewPayload { bytes }
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
    let area = target_work_area(app);
    let (work_w, work_h) = area.map(|(.., w, h)| (w, h)).unwrap_or((1920.0, 1080.0));
    let (width, height) = preview_size(frame, work_w, work_h);
    let window = ensure_window(app, PREVIEW, "preview", width, height, false, false)?;
    let _ = window.set_size(Size::Logical(LogicalSize { width, height }));
    if let Some((x, y, ..)) = area {
        let _ = window.set_position(Position::Logical(LogicalPosition {
            x: centered_offset(x, work_w, width),
            y: centered_offset(y, work_h, height),
        }));
    } else {
        let _ = window.center();
    }
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

fn target_work_area(app: &AppHandle) -> Option<(f64, f64, f64, f64)> {
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|position| app.monitor_from_point(position.x, position.y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let area = monitor.work_area();
    let scale = monitor.scale_factor().max(f64::EPSILON);
    Some((
        area.position.x as f64 / scale,
        area.position.y as f64 / scale,
        area.size.width as f64 / scale,
        area.size.height as f64 / scale,
    ))
}

fn centered_offset(origin: f64, area: f64, window: f64) -> f64 {
    origin + (area - window).max(0.0) / 2.0
}

fn preview_size(frame: &Frame, work_w: f64, work_h: f64) -> (f64, f64) {
    let max_w = (work_w * 0.9).max(1.0);
    let max_h = (work_h * 0.9).max(1.0);
    let chrome = 108.0;
    let min_w = 480.0_f64.min(max_w);
    let min_h = 280.0_f64.min(max_h);
    let width = frame.width.max(1) as f64;
    let height = frame.height.max(1) as f64;
    let scale = (max_w / width)
        .min(((max_h - chrome).max(1.0)) / height)
        .min(1.0);
    (
        (width * scale).max(min_w).min(max_w),
        (height * scale + chrome).max(min_h).min(max_h),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::buffer::Frame;

    #[test]
    fn binary_preview_preserves_native_size_scale_and_original_png() {
        let frame = Frame { width: 2, height: 1, rgba: vec![1, 2, 3, 255, 4, 5, 6, 128], scale: 1.5 };
        let png = crate::capture::buffer::encode_png(&frame).unwrap();
        let payload = preview_payload(&frame, &png, true);
        assert_eq!(u32::from_le_bytes(payload.bytes[0..4].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(payload.bytes[4..8].try_into().unwrap()), 1);
        assert_eq!(f64::from_le_bytes(payload.bytes[8..16].try_into().unwrap()), 1.5);
        assert_eq!(u32::from_le_bytes(payload.bytes[16..20].try_into().unwrap()), 1);
        assert_eq!(&payload.bytes[20..], png.as_slice());
        assert_eq!(crate::capture::buffer::decode_png(&payload.bytes[20..]).unwrap().rgba, frame.rgba);
        assert_eq!(&preview_payload(&frame, &png, false).bytes[16..20], &[0; 4]);
    }

    fn frame(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            rgba: vec![0; 4],
            scale: 2.0,
        }
    }

    #[test]
    fn preview_fits_1080p_work_area() {
        let (width, height) = preview_size(&frame(3840, 2160), 1920.0, 1080.0);
        assert!(width <= 1920.0 * 0.9);
        assert!(height <= 1080.0 * 0.9);
        let ratio = 3840.0 / 2160.0;
        assert!((width / (height - 108.0) - ratio).abs() < 0.01);
    }

    #[test]
    fn preview_fits_1366_work_area() {
        let (width, height) = preview_size(&frame(3840, 2160), 1366.0, 720.0);
        assert!(width <= 1366.0 * 0.9);
        assert!(height <= 720.0 * 0.9);
        let ratio = 3840.0 / 2160.0;
        assert!((width / (height - 108.0) - ratio).abs() < 0.01);
    }

    #[test]
    fn preview_scales_up_to_4k_work_area() {
        let (width, height) = preview_size(&frame(3840, 2160), 3840.0, 2160.0);
        assert!(width <= 3840.0 * 0.9);
        assert!(height <= 2160.0 * 0.9);
        assert!(width > 800.0);
        let ratio = 3840.0 / 2160.0;
        assert!((width / (height - 108.0) - ratio).abs() < 0.01);
    }

    #[test]
    fn preview_enforces_minimum_panel() {
        let (width, height) = preview_size(&frame(100, 100), 1920.0, 1080.0);
        assert_eq!(width, 480.0);
        assert_eq!(height, 280.0);
    }

    #[test]
    fn centered_offset_centers_inside_target_work_area() {
        assert_eq!(centered_offset(100.0, 1920.0, 800.0), 100.0 + 560.0);
        assert_eq!(centered_offset(0.0, 500.0, 480.0), 10.0);
        // 窗口大于工作区时贴齐原点,不产生负偏移
        assert_eq!(centered_offset(50.0, 400.0, 600.0), 50.0);
    }

    #[test]
    fn preview_minimum_yields_to_tiny_work_area() {        let (width, height) = preview_size(&frame(100, 100), 500.0, 400.0);
        assert!(width <= 500.0 * 0.9);
        assert!(height <= 400.0 * 0.9);
    }

    #[test]
    fn preview_small_frame_keeps_native_size() {
        let (width, height) = preview_size(&frame(640, 400), 1920.0, 1080.0);
        assert_eq!(width, 640.0);
        assert_eq!(height, 400.0 + 108.0);
    }
}
