pub mod buffer;
pub mod error;
pub mod geometry;
pub mod hide;
#[cfg(windows)]
pub mod native_overlay;
pub mod platform;
pub mod session;
pub mod ui;
pub mod windows_list;

use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use error::CaptureError;
use geometry::LogicalRect;
use session::RegionSelection;

pub fn begin(app: &AppHandle, mode: CaptureMode, delay_ms: u64) {
    session::begin(app, mode, delay_ms);
}

#[tauri::command]
pub fn get_overlay_frame(app: AppHandle) -> Result<ui::OverlayPayload, CaptureError> {
    session::overlay_frame(&app)
}

#[tauri::command]
pub fn get_preview_frame(app: AppHandle) -> Result<ui::PreviewPayload, CaptureError> {
    session::preview_frame(&app)
}

#[tauri::command]
pub async fn confirm_region(app: AppHandle, x: u32, y: u32, width: u32, height: u32) -> Result<(), CaptureError> {
    session::confirm_region(&app, RegionSelection { x, y, width, height })
}

#[tauri::command]
pub async fn confirm_logical_region(
    app: AppHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), CaptureError> {
    session::confirm_logical_region(
        &app,
        LogicalRect {
            x,
            y,
            width,
            height,
        },
    )
}

#[tauri::command]
pub async fn confirm_window(app: AppHandle, window_id: String) -> Result<(), CaptureError> {
    session::confirm_window(&app, window_id)
}

pub fn precreate_windows(app: &AppHandle) {
    ui::precreate(app);
}

#[tauri::command]
pub fn cancel_capture(app: AppHandle) -> Result<(), CaptureError> {
    session::cancel(&app).map(|_| ())
}

#[tauri::command]
pub fn close_preview(app: AppHandle) {
    session::close_preview(&app);
}

#[tauri::command]
pub fn close_capture_error(app: AppHandle) {
    session::close_error(&app);
}

#[tauri::command]
pub fn get_delay_state(app: AppHandle) -> ui::DelayPayload {
    session::delay_state(&app)
}

#[tauri::command]
pub fn get_capture_error(app: AppHandle) -> Option<CaptureError> {
    session::last_error(&app)
}

impl From<CaptureError> for String {
    fn from(value: CaptureError) -> Self {
        value.user_message()
    }
}
