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
use buffer::Frame;
use error::CaptureError;
use geometry::LogicalRect;
use session::{QuietAction, RegionSelection};

pub fn begin(app: &AppHandle, mode: CaptureMode, delay_ms: u64) {
    // 新截取会替换预览会话:先收尾可能存在的贴图再标注(恢复来源贴图置顶)。
    crate::pin::finish_pin_edit(app);
    session::begin(app, mode, delay_ms);
}

/// 托盘"上次区域"直取(R6):按记录区域抓取,不打开交互选区。
pub fn begin_last_region(app: &AppHandle, delay_ms: u64) {
    crate::pin::finish_pin_edit(app);
    session::begin_last_region(app, delay_ms);
}

#[tauri::command]
pub fn get_overlay_frame(app: AppHandle) -> Result<ui::OverlayPayload, CaptureError> {
    session::overlay_frame(&app)
}

#[tauri::command]
pub fn get_preview_frame(app: AppHandle) -> Result<tauri::ipc::Response, CaptureError> {
    session::preview_frame(&app).map(|payload| tauri::ipc::Response::new(payload.bytes))
}

/// 贴图再标注(R9):把当前贴图内容装入预览会话并记录回写目标 label,
/// 随后复用既有预览窗口与全部预览命令;确认/取消由 pin 侧命令收尾。
pub fn open_pin_edit_preview(
    app: &AppHandle,
    frame: Frame,
    writeback: String,
) -> Result<(), CaptureError> {
    session::adopt_external_frame(app, frame.clone(), writeback)?;
    ui::open_preview(app, &frame).map(|_| ())
}

#[tauri::command]
pub async fn confirm_region(app: AppHandle, x: u32, y: u32, width: u32, height: u32) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || {
        session::confirm_region(&app, RegionSelection { x, y, width, height })
    }).await.map_err(|_| CaptureError::api("error.capture.thread_failed"))?
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
    )).await.map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

#[tauri::command]
pub async fn confirm_window(app: AppHandle, window_id: String) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || session::confirm_window(&app, window_id))
        .await.map_err(|_| CaptureError::api("error.capture.thread_failed"))?
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
        Vec::new(),
        action,
        None,
    )
    .await
}

async fn run_quiet_action(app: &AppHandle, action: QuietAction) {
    match action {
        QuietAction::Copy => ui::show_toast_key(app, "toast.copied"),
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
            ui::show_toast_key(app, "toast.capture_expired");
            return;
        }
    };
    let export = settings::current_export(app);
    match crate::export::save_frame_with_dialog(app, frame, export, None).await {
        Ok(result) if result.saved => {
            let name = result.file_name().unwrap_or(result.format.label());
            ui::show_toast_key_params(app, "toast.saved", &[("name", name)]);
        }
        Ok(_) => {}
        Err(message) => ui::show_toast(app, &message),
    }
}

/// Offline OCR over the retained quiet frame, copying the full text to the
/// clipboard with toast feedback (empty results included). 首次取字要加载模型,
/// 先给不自动消失的进行中提示(R11);识别结果与失败提示替换该 toast。
async fn ocr_quiet_frame(app: &AppHandle) {
    ui::show_progress_toast_key(app, "toast.ocr_progress");
    match crate::ocr::recognize_preview(app.clone()).await {
        Ok(_) => match crate::ocr::copy_ocr_all(app.clone()) {
            Ok(text) => {
                let chars = text.chars().count();
                ui::show_toast_key_params(app, "toast.ocr_copied", &[("chars", &chars.to_string())]);
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
    // 再标注路径的取消语义:恢复来源贴图置顶,不改贴图内容。
    crate::pin::finish_pin_edit(&app);
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
