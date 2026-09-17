pub mod buffer;
pub mod error;
pub mod geometry;
pub mod hide;
#[cfg(windows)]
pub mod native_overlay;
pub mod platform;
pub mod selection;
pub mod session;
pub mod ui;
pub mod windows_list;

use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use error::CaptureError;
use geometry::LogicalRect;
use session::{QuietAction, RegionSelection, DEFAULT_FRAME_TTL};

pub fn begin(app: &AppHandle, mode: CaptureMode, delay_ms: u64) {
    session::begin(app, mode, delay_ms);
}

#[tauri::command]
pub fn get_overlay_frame(app: AppHandle) -> Result<ui::OverlayPayload, CaptureError> {
    session::overlay_frame(&app)
}

#[tauri::command]
pub fn get_preview_frame(app: AppHandle) -> Result<tauri::ipc::Response, CaptureError> {
    session::preview_frame(&app).map(|payload| tauri::ipc::Response::new(payload.bytes))
}

#[tauri::command]
pub async fn confirm_region(app: AppHandle, x: u32, y: u32, width: u32, height: u32) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || {
        session::confirm_region(&app, RegionSelection { x, y, width, height })
    }).await.map_err(|_| CaptureError::api("截取线程失败。"))?
}

#[tauri::command]
pub async fn confirm_logical_region(
    app: AppHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || session::confirm_logical_region(
        &app,
        LogicalRect {
            x,
            y,
            width,
            height,
        },
    )).await.map_err(|_| CaptureError::api("截取线程失败。"))?
}

#[tauri::command]
pub async fn confirm_window(app: AppHandle, window_id: String) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || session::confirm_window(&app, window_id))
        .await.map_err(|_| CaptureError::api("截取线程失败。"))?
}

/// Quiet completion with an immediate action on the cropped region (R3):
/// the unannotated PNG reaches the clipboard, no preview window opens, and
/// the session keeps the frame for a short TTL while the action runs.
#[tauri::command]
pub async fn finish_region_with(
    app: AppHandle,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    action: QuietAction,
) -> Result<(), CaptureError> {
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        session::finish_region_quiet(
            &handle,
            RegionSelection { x, y, width, height },
            DEFAULT_FRAME_TTL,
        )
    })
    .await
    .map_err(|_| CaptureError::api("截取线程失败。"))??;
    run_quiet_action(&app, action).await;
    Ok(())
}

async fn run_quiet_action(app: &AppHandle, action: QuietAction) {
    match action {
        QuietAction::Copy => ui::show_toast(app, "已复制到剪贴板。"),
        // Pin windows arrive in a later task; keep the entry point stable.
        QuietAction::Pin => ui::show_toast(app, "贴图功能尚未开放，敬请期待。"),
        QuietAction::Save => save_quiet_frame(app).await,
        QuietAction::Ocr => ocr_quiet_frame(app).await,
    }
}

/// rfd save dialog over the retained quiet frame; no preview parent exists in
/// this path, so the dialog is unparented.
async fn save_quiet_frame(app: &AppHandle) {
    let frame = match session::current_preview_frame(app) {
        Ok(frame) => frame,
        Err(_) => {
            ui::show_toast(app, "截图已过期，请重新截取。");
            return;
        }
    };
    let png = match buffer::encode_png(&frame) {
        Ok(png) => png,
        Err(_) => {
            ui::show_toast(app, "无法生成 PNG，请重新截取。");
            return;
        }
    };
    let dialog = rfd::AsyncFileDialog::new()
        .add_filter("PNG", &["png"])
        .set_file_name("cropmark.png")
        .set_title("保存截图");
    let Some(file) = dialog.save_file().await else {
        return;
    };
    let mut path = file.path().to_path_buf();
    if path.extension().is_none() {
        path.set_extension("png");
    }
    match std::fs::write(&path, png) {
        Ok(()) => {
            session::mark_preview_file_written(app);
            ui::show_toast(app, "已保存截图。");
        }
        Err(_) => ui::show_toast(app, "无法写入 PNG 文件，请重试。"),
    }
}

/// Offline OCR over the retained quiet frame, copying the full text to the
/// clipboard with toast feedback (empty results included).
async fn ocr_quiet_frame(app: &AppHandle) {
    match crate::ocr::recognize_preview(app.clone()).await {
        Ok(_) => match crate::ocr::copy_ocr_all(app.clone()) {
            Ok(text) => {
                let chars = text.chars().count();
                ui::show_toast(app, &format!("已复制 {chars} 字。"));
            }
            Err(message) => ui::show_toast(app, &message),
        },
        Err(message) => ui::show_toast(app, &message),
    }
}

#[tauri::command]
pub fn get_toast_message() -> Option<ui::ToastPayload> {
    ui::toast_message().map(|message| ui::ToastPayload { message })
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
