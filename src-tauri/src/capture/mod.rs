pub mod buffer;
pub mod error;
pub mod geometry;
pub mod hide;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod native_overlay;
pub mod platform;
pub mod selection;
pub mod session;
pub mod ui;
pub mod windows_list;

use tauri::AppHandle;

use crate::hotkeys::CaptureMode;
use crate::settings;
use error::CaptureError;
use geometry::LogicalRect;
use session::{QuietAction, RegionSelection};

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
/// 命令层只做参数解包,守卫与动作分发全部委托会话层:只有活动
/// overlay 会话允许静默裁剪,idle-with-frame(TTL 保留帧)期间的
/// 重复 invoke 在 `session::finish_region_with` 内被拒绝。
#[tauri::command]
pub async fn finish_region_with(
    app: AppHandle,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    action: QuietAction,
) -> Result<(), CaptureError> {
    session::finish_region_with(
        &app,
        RegionSelection { x, y, width, height },
        action,
    )
    .await
}

async fn run_quiet_action(app: &AppHandle, action: QuietAction) {
    match action {
        QuietAction::Copy => ui::show_toast(app, "已复制到剪贴板。"),
        // 选区操作条贴图:不经前端、不带标注;成功无提示(贴图窗即反馈),失败 toast。
        QuietAction::Pin => crate::pin::pin_retained(app),
        QuietAction::Save => save_quiet_frame(app).await,
        QuietAction::Ocr => ocr_quiet_frame(app).await,
    }
}

/// rfd save dialog over the retained quiet frame; no preview parent exists in
/// this path, so the dialog is unparented. 与预览保存共用格式/质量/目录记忆
/// (R3):静默路径只能沿用设置中的上次选择,无法在本路径单独切换格式。
async fn save_quiet_frame(app: &AppHandle) {
    let frame = match session::current_preview_frame(app) {
        Ok(frame) => frame,
        Err(_) => {
            ui::show_toast(app, "截图已过期，请重新截取。");
            return;
        }
    };
    let export = settings::current_export(app);
    match crate::export::save_frame_with_dialog(app, frame, export, None).await {
        Ok(result) if result.saved => {
            let name = result.file_name().unwrap_or(result.format.label());
            ui::show_toast(app, &format!("已保存 {name}。"));
        }
        Ok(_) => {}
        Err(message) => ui::show_toast(app, &message),
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
