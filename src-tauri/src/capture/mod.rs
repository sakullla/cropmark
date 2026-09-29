pub mod buffer;
pub mod error;
pub mod geometry;
pub mod hide;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod native_overlay;
pub mod platform;
pub mod scroll;
pub mod selection;
pub mod session;
pub mod snap;
pub mod ui;
pub mod windows_list;

use tauri::{AppHandle, Emitter};

use crate::hotkeys::CaptureMode;
use crate::record::{RecordSaveResult, RecordingOutput};
use crate::settings;
use buffer::Frame;
use error::CaptureError;
use geometry::LogicalRect;
use session::{QuietAction, RegionSelection};

pub fn begin(app: &AppHandle, mode: CaptureMode, delay_ms: u64) {
    begin_with_target(app, mode, delay_ms, session::FullscreenTarget::Pointer);
}

/// 托盘全屏子菜单:指针屏、指定显示器或全部拼接。热键不走这里。
pub fn begin_fullscreen(app: &AppHandle, delay_ms: u64, target: session::FullscreenTarget) {
    begin_with_target(app, CaptureMode::Fullscreen, delay_ms, target);
}

/// R3 托盘录屏入口:受录屏开关门控(关闭时不产生任何行为);延时与热键/
/// 托盘截取同源,随后进入现有区域选区,确认后启动录制会话。
pub fn dispatch_recording(app: &AppHandle) {
    if !settings::current_recording(app).enabled {
        return;
    }
    let delay_ms = settings::current_capture(app).delay_ms();
    let _ = app.emit("capture-requested", CaptureMode::Recording);
    begin_with_target(
        app,
        CaptureMode::Recording,
        delay_ms,
        session::FullscreenTarget::Pointer,
    );
}

/// R3:停止活动录制并进入保存(托盘与后续录制 HUD 共用)。无活动录制时
/// no-op;保存取消时明确丢弃本次录制并使用录制专用文案;保存失败时保留
/// 临时文件并登记待重试,错误文案给出保留位置,应用继续可用。
pub fn stop_recording(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        stop_recording_inner(app).await;
    });
}

async fn stop_recording_inner(app: AppHandle) {
    let Some(recording) = session::take_recording_session(&app) else {
        return;
    };
    let stopped = tauri::async_runtime::spawn_blocking(move || recording.stop()).await;
    let output = match stopped {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            ui::show_toast(&app, &error.user_message());
            session::refresh_tray_menu(&app);
            return;
        }
        Err(_) => {
            ui::show_toast_key(&app, "error.record.thread");
            session::refresh_tray_menu(&app);
            return;
        }
    };
    // 上限自动停止/抓帧中断先给可见说明,再进入保存对话框。
    if output.auto_stopped {
        ui::show_toast_key(&app, "toast.recording_auto_stopped");
    }
    if let Some(interrupted) = output.interrupted.as_deref() {
        ui::show_toast(&app, interrupted);
    }
    let result = crate::record::save_recording_with_dialog(&app, None, &output).await;
    match conclude_recording_save(output, result) {
        RecordingSaveNotice::Saved { name } => {
            ui::show_toast_key_params(&app, "toast.saved", &[("name", &name)]);
        }
        RecordingSaveNotice::Discarded => {
            ui::show_toast_key(&app, "toast.recording_discarded");
        }
        RecordingSaveNotice::Failed { message } => {
            ui::show_toast(&app, &message);
            // 托盘路径的保存对话框期间控制条可能已按「无会话」自动收起:
            // 重新呼出以兑现错误文案承诺的重试保存/丢弃入口。
            crate::record::hud::show_pending(&app);
        }
    }
    session::refresh_tray_menu(&app);
}

/// 停止录制后的保存收尾:提示与实际文件去向必须一致,供托盘与后续录制
/// HUD 共用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordingSaveNotice {
    /// 已保存:原子写盘已删除临时文件;`name` 为保存文件名。
    Saved { name: String },
    /// 用户取消保存:本次录制已丢弃。
    Discarded,
    /// 保存失败:临时文件已保留并登记待重试;`message` 为含保留位置的
    /// 本地化错误。
    Failed { message: String },
}

/// 保存对话框返回后的收尾。成功不重复删文件(写盘路径已删除);取消是
/// 用户明确放弃,丢弃临时文件并使用录制专用文案——录制内容已不存在,
/// 不能复用「截图仍在」的截图文案;失败保留临时文件并登记待处理,
/// 由后续录制 HUD 提供重试与清理。
pub(crate) fn conclude_recording_save(
    output: RecordingOutput,
    result: Result<RecordSaveResult, String>,
) -> RecordingSaveNotice {
    match result {
        Ok(result) if result.saved => {
            let name = result
                .path
                .as_deref()
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("cropmark")
                .to_string();
            RecordingSaveNotice::Saved { name }
        }
        Ok(_) => {
            crate::record::discard_recording(&output);
            RecordingSaveNotice::Discarded
        }
        Err(message) => {
            crate::record::keep_pending_recording(output);
            log::warn!("record save failed kept_pending=true");
            RecordingSaveNotice::Failed { message }
        }
    }
}

fn begin_with_target(
    app: &AppHandle,
    mode: CaptureMode,
    delay_ms: u64,
    target: session::FullscreenTarget,
) {
    // 新截取会替换预览会话:先收尾可能存在的贴图再标注(恢复来源贴图置顶)。
    crate::pin::finish_pin_edit(app);
    // R1:新截取同样取消进行中的长截图滚动会话(不产出)。
    scroll::interrupt(app);
    session::begin(app, mode, delay_ms, target);
}

/// 托盘"上次区域"直取(R6):按记录区域抓取,不打开交互选区。
pub fn begin_last_region(app: &AppHandle, delay_ms: u64) {
    crate::pin::finish_pin_edit(app);
    scroll::interrupt(app);
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

/// Web 覆盖层(Wayland)区域确认:坐标与冻帧物理像素一致,`annotations` 为
/// 覆盖层标注层导出的图元(相对冻帧物理像素);省略或为空保持旧行为。
#[tauri::command]
pub async fn confirm_region(
    app: AppHandle,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    annotations: Option<Vec<crate::annotate::Annotation>>,
) -> Result<(), CaptureError> {
    let annotations = annotations.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || {
        session::confirm_region(
            &app,
            RegionSelection {
                x,
                y,
                width,
                height,
            },
            annotations,
        )
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

#[tauri::command]
pub async fn confirm_logical_region(
    app: AppHandle,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || {
        session::confirm_logical_region(
            &app,
            LogicalRect {
                x,
                y,
                width,
                height,
            },
        )
    })
    .await
    .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

#[tauri::command]
pub async fn confirm_window(app: AppHandle, window_id: String) -> Result<(), CaptureError> {
    tauri::async_runtime::spawn_blocking(move || session::confirm_window(&app, window_id))
        .await
        .map_err(|_| CaptureError::api("error.capture.thread_failed"))?
}

/// Quiet completion with an immediate copy/save/pin action on the cropped
/// region (R3): the unannotated PNG reaches the clipboard for Copy, no preview
/// window opens, and the session keeps the frame for a short TTL while the
/// action runs. R2:取字不在此列,改走工作区覆盖层。
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
        RegionSelection {
            x,
            y,
            width,
            height,
        },
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
    match crate::export::save_frame_with_dialog(
        app,
        frame,
        export,
        None,
        crate::export::FileNaming::Quiet,
    )
    .await
    {
        Ok(result) if result.saved => {
            let name = result.file_name().unwrap_or(result.format.label());
            ui::show_toast_key_params(app, "toast.saved", &[("name", name)]);
        }
        Ok(_) => {}
        Err(message) => ui::show_toast(app, &message),
    }
}

#[tauri::command]
pub fn get_toast_message() -> Option<ui::ToastPayload> {
    ui::toast_message().map(|message| ui::ToastPayload { message })
}

pub fn precreate_windows(app: &AppHandle) {
    ui::precreate(app);
    crate::pin::precreate(app);
}

/// 浮层保存前:摘掉全屏置顶,让保存对话框的父窗口(预览)拿到焦点。
#[tauri::command]
pub fn prepare_workspace_save_dialog(app: AppHandle) {
    ui::yield_overlay_for_save_dialog(&app);
}

/// 保存取消或失败:藏起临时预览窗,把浮层放回置顶。不重载页面,标注还在。
#[tauri::command]
pub fn restore_workspace_after_save_dialog(app: AppHandle) {
    ui::restore_overlay_after_save_dialog(&app);
}

#[tauri::command]
pub fn complete_workspace(
    app: AppHandle,
    kind: String,
    name: Option<String>,
) -> Result<(), CaptureError> {
    session::complete_workspace(&app, &kind, name.as_deref())
}

#[tauri::command]
pub fn edit_workspace_further(
    app: AppHandle,
    annotations: Vec<crate::annotate::Annotation>,
) -> Result<(), CaptureError> {
    session::edit_workspace_further(&app, annotations)
}

#[tauri::command]
pub fn fallback_workspace_preview(
    app: AppHandle,
    annotations: Vec<crate::annotate::Annotation>,
) -> Result<(), CaptureError> {
    session::fallback_workspace_preview(&app, annotations)
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
pub fn take_pending_preview_ocr(app: AppHandle) -> bool {
    session::take_pending_preview_ocr(&app)
}

#[tauri::command]
pub fn take_pending_preview_qr(app: AppHandle) -> bool {
    session::take_pending_preview_qr(&app)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::Annotation;

    /// 临时目录里的一份录制产物(模拟停止后的待保存文件)。
    fn recording_output(name: &str) -> RecordingOutput {
        let dir = std::env::temp_dir().join(format!(
            "cropmark-recording-entry-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let temp_path = dir.join(name);
        std::fs::write(&temp_path, b"recorded-bytes").expect("temp file");
        RecordingOutput {
            format: crate::record::RecordFormat::Gif,
            temp_path,
            width: 320,
            height: 200,
            frame_count: 12,
            duration_ms: 1200,
            auto_stopped: false,
            interrupted: None,
        }
    }

    /// P1 回归:取消保存时丢弃唯一录制临时文件,并使用录制专用文案——
    /// 不得复用「截图仍在,可继续复制、贴图或再试」的截图取消文案。
    #[test]
    fn cancelled_recording_save_discards_file_with_recording_specific_notice() {
        let output = recording_output("cancel.gif");
        let notice = conclude_recording_save(
            output.clone(),
            Ok(RecordSaveResult {
                saved: false,
                format: crate::record::RecordFormat::Gif,
                path: None,
            }),
        );
        assert!(matches!(notice, RecordingSaveNotice::Discarded));
        assert!(
            !output.temp_path.exists(),
            "cancelled recording must be discarded, matching the notice"
        );
        let message = crate::i18n::t("toast.recording_discarded");
        assert!(message.contains("丢弃"), "got {message}");
        assert_ne!(
            message,
            crate::i18n::t("overlay.notice.save_cancelled"),
            "recording cancellation must not reuse the screenshot notice"
        );
    }

    /// P1 回归:写盘失败时保留录制产物,本地化错误必须给出保留位置。
    #[test]
    fn failed_recording_save_keeps_file_and_points_to_its_location() {
        let output = recording_output("failed.gif");
        let target = output
            .temp_path
            .with_file_name("missing-subdir")
            .join("kept.gif");
        let error = crate::record::move_output_atomic(&output, &target).expect_err("write failure");
        let notice = conclude_recording_save(output.clone(), Err(error));
        let RecordingSaveNotice::Failed { message } = notice else {
            panic!("expected a failure notice, got {notice:?}");
        };
        assert!(
            message.contains(&output.temp_path.display().to_string()),
            "message must point to the kept recording: {message}"
        );
        assert!(
            output.temp_path.exists(),
            "failed save must keep the recording temp file"
        );
        let _ = std::fs::remove_file(&output.temp_path);
    }

    /// R21:Wayland 覆盖层确认请求携带的标注 JSON(共享标注层 `exportList`
    /// 的序列化形状)必须能被命令参数解析;`null` 线宽与省略的样式字段回退
    /// 默认值,空列表表示"无标注"的旧行为。
    #[test]
    fn overlay_annotation_payload_parses_for_confirm_region() {
        let payload: Vec<Annotation> = serde_json::from_str(
            r##"[
                {"type":"rect","x":10,"y":12,"width":40,"height":30,"color":"#e11d48","strokeWidth":null},
                {"type":"text","x":5,"y":5,"text":"你好","size":22,"color":"#2563eb"},
                {"type":"blur","x":0,"y":0,"width":20,"height":16,"sigma":3}
            ]"##,
        )
        .unwrap();
        assert_eq!(payload.len(), 3);
        assert!(matches!(payload[0], Annotation::Rect { .. }));
        assert!(matches!(payload[1], Annotation::Text { .. }));
        assert!(matches!(payload[2], Annotation::Blur { .. }));
        // 无标注时前端发送空列表(旧行为:空裁剪仍走同一条完成路径)。
        assert!(serde_json::from_str::<Vec<Annotation>>("[]")
            .unwrap()
            .is_empty());
    }
}
