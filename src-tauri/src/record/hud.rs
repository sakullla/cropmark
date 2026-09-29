//! R3 录制 HUD:紧凑控制条与实时标注层。
//!
//! 组成(两个复用覆盖层窗口类别的无边框置顶窗,视图路由见 `src/main.ts`):
//! - `record-control`:紧凑控制条,显示已录时长、暂停/继续/停止与标注开关;
//!   保存失败保留的待处理录制也在这里重试保存或丢弃(兑现错误文案的重试承诺);
//! - `record-overlay`:覆盖录制区域的标注层。Windows/macOS 上是透明实时浮层,
//!   默认鼠标穿透,进入标注模式才接收输入;Linux 等无合成器能力的环境降级为
//!   「显式绘制模式」:只在标注时显示定格快照、绘制期间暂停录制,并给出本地化说明。
//!
//! 自捕获边界:两个窗口都尝试内容保护(`set_content_protected`:Windows 为
//! WDA_EXCLUDEFROMCAPTURE,macOS 为共享类型 None),让 HUD 不进入录制画面;
//! 不支持该能力的平台把控制条放到录制区域之外,并在绘制期间暂停录制。
//!
//! 生命周期:录制由选区确认启动后,会话层调用 [`open`];停止/保存成功或丢弃后
//! 由 [`close`] 收起;保存失败保留产物并保持控制条打开,提供重试保存与丢弃入口。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Position, Size,
    WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

use crate::annotate::Annotation;
use crate::capture::buffer::{crop_rgba, encode_jpeg, fit_display, resize_rgba};
use crate::capture::geometry::MonitorGeom;
use crate::capture::platform;
use crate::capture::session;
use crate::capture::ui;
use crate::i18n;
use crate::settings;

use super::save;
use super::{
    MonitorSource, RecordConfig, RecordError, RecordFormat, RecordRegion, RecordingPhase,
    RecordingSession, RecordingStatus, MAX_RECORDING_MS,
};

pub const CONTROL: &str = "record-control";
pub const OVERLAY: &str = "record-overlay";
/// 控制条窗口打开:两个视图据此开始轮询并初始化。
pub const EVENT_OPEN: &str = "record-hud-open";
/// 控制条窗口收起:两个视图停止轮询并清空状态。
pub const EVENT_CLOSE: &str = "record-hud-close";
/// 标注层复位:会话结束或重新开始时清空上一会话的绘制内容,从引擎标注
/// 重新初始化(避免残留标注只显示在覆盖层、不进入新录制)。
pub const EVENT_RESET: &str = "record-hud-reset";
/// 状态广播(标注模式切换等即时变化;时长仍由视图轮询刷新)。
pub const EVENT_STATE: &str = "record-hud-state";

/// 控制条:紧凑态与有待处理产物时的展开态(逻辑像素)。
const CONTROL_WIDTH: f64 = 420.0;
const CONTROL_HEIGHT: f64 = 64.0;
const CONTROL_HEIGHT_EXPANDED: f64 = 248.0;
/// 控制条与录制区域/显示器边缘的间距(逻辑像素)。
const CONTROL_MARGIN: f64 = 12.0;
/// 降级绘制快照的长边上限(等比缩放,只决定传输分辨率)。
const SNAPSHOT_MAX_EDGE: u32 = 1280;

/// HUD 能力。`notice_key` 为降级时的本地化说明词条键,完整能力时为 None。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudCapabilities {
    /// 透明实时标注层(Windows/macOS);false 时走快照式显式绘制。
    pub live_overlay: bool,
    /// 窗口可在穿透与接收输入之间切换。
    pub cursor_passthrough: bool,
    /// 窗口可排除出录制画面/截图。
    pub capture_protection: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice_key: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudPlatform {
    Windows,
    Macos,
    Linux,
    Other,
}

/// 当前平台。纯函数化便于按平台断言能力与降级路径。
pub fn current_platform() -> HudPlatform {
    if cfg!(target_os = "windows") {
        HudPlatform::Windows
    } else if cfg!(target_os = "macos") {
        HudPlatform::Macos
    } else if cfg!(target_os = "linux") {
        HudPlatform::Linux
    } else {
        HudPlatform::Other
    }
}

pub fn capabilities_for(platform: HudPlatform) -> HudCapabilities {
    match platform {
        HudPlatform::Windows | HudPlatform::Macos => HudCapabilities {
            live_overlay: true,
            cursor_passthrough: true,
            capture_protection: true,
            notice_key: None,
        },
        // 无合成器能力时不承诺透明与捕获排除:紧凑控制条 + 显式绘制模式。
        HudPlatform::Linux | HudPlatform::Other => HudCapabilities {
            live_overlay: false,
            cursor_passthrough: true,
            capture_protection: false,
            notice_key: Some("record.hud.degraded"),
        },
    }
}

fn capabilities() -> HudCapabilities {
    capabilities_for(current_platform())
}

/// 进入/退出标注模式对录制的影响:降级平台在显式绘制期间暂停录制,
/// 退出时只恢复由绘制引起的暂停(用户手动暂停保持不变)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawPausePlan {
    pub pause: bool,
    pub resume: bool,
}

pub fn draw_pause_plan(
    live_overlay: bool,
    entering: bool,
    phase: Option<RecordingPhase>,
    paused_for_draw: bool,
) -> DrawPausePlan {
    if live_overlay {
        return DrawPausePlan {
            pause: false,
            resume: false,
        };
    }
    if entering {
        DrawPausePlan {
            pause: phase == Some(RecordingPhase::Recording),
            resume: false,
        }
    } else {
        DrawPausePlan {
            pause: false,
            resume: paused_for_draw,
        }
    }
}

/// HUD 会话上下文:打开时记录的区域与显示器,「重新录制」与降级快照用。
struct HudRuntime {
    region: Option<RecordRegion>,
    monitor: Option<MonitorGeom>,
    interactive: bool,
    paused_for_draw: bool,
}

static HUD: Mutex<HudRuntime> = Mutex::new(HudRuntime {
    region: None,
    monitor: None,
    interactive: false,
    paused_for_draw: false,
});

fn hud_lock() -> MutexGuard<'static, HudRuntime> {
    HUD.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudRegion {
    pub width: u32,
    pub height: u32,
    /// 显示器缩放系数:标注层 `AnnotationFrame.scale` 与录制合成使用的
    /// `Frame.scale` 语义一致,预览线宽/字号等推导尺寸与导出对齐。
    pub scale: f64,
}

/// 待处理(保存失败保留)录制的展示信息;`temp_path` 是重试/丢弃的精确句柄。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRecordingInfo {
    pub temp_path: String,
    pub file_name: String,
    pub format: RecordFormat,
    pub width: u32,
    pub height: u32,
    pub frame_count: u64,
    pub duration_ms: u64,
    pub auto_stopped: bool,
    pub interrupted: Option<String>,
}

pub fn pending_info(output: &super::RecordingOutput) -> PendingRecordingInfo {
    PendingRecordingInfo {
        temp_path: output.temp_path.to_string_lossy().into_owned(),
        file_name: output
            .temp_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cropmark-recording".into()),
        format: output.format,
        width: output.width,
        height: output.height,
        frame_count: output.frame_count,
        duration_ms: output.duration_ms,
        auto_stopped: output.auto_stopped,
        interrupted: output.interrupted.clone(),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingHudState {
    pub status: Option<RecordingStatus>,
    pub pending: Vec<PendingRecordingInfo>,
    pub capabilities: HudCapabilities,
    /// 标注层当前是否接收输入(标注模式)。
    pub interactive: bool,
    /// 是否有可「重新录制」的区域上下文。
    pub has_context: bool,
    pub region: Option<HudRegion>,
    /// 单次录制上限(毫秒),控制条据此显示 已录/上限。
    pub limit_ms: u64,
}

/// 停止/保存的结构化结果,驱动控制条的可见收尾。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RecordingStopOutcome {
    Saved {
        name: String,
    },
    /// 用户取消保存:录制已按录制入口语义丢弃。
    Discarded,
    /// 重试保存时取消对话框:产物仍保留在待处理列表。
    Cancelled,
    Failed {
        message: String,
        retryable: bool,
    },
    Empty,
}

/// 打开 HUD:定位控制条与标注层并开始广播状态。
pub fn open(app: &AppHandle, region: RecordRegion, monitor: MonitorGeom) {
    {
        let mut hud = hud_lock();
        hud.region = Some(region);
        hud.monitor = Some(monitor.clone());
        hud.interactive = false;
        hud.paused_for_draw = false;
    }
    let capabilities = capabilities();
    if let Ok(control) = ensure(
        app,
        CONTROL,
        "record-control",
        CONTROL_WIDTH,
        CONTROL_HEIGHT,
        true,
    ) {
        let _ = control.set_content_protected(capabilities.capture_protection);
        let _ = control.set_ignore_cursor_events(false);
        let size = LogicalSize {
            width: CONTROL_WIDTH,
            height: CONTROL_HEIGHT,
        };
        place_control(&control, region, &monitor, size);
        let _ = control.show();
    }
    if let Ok(overlay) = ensure(
        app,
        OVERLAY,
        "record-overlay",
        320.0,
        240.0,
        capabilities.live_overlay,
    ) {
        let _ = overlay.set_content_protected(capabilities.capture_protection);
        let _ = overlay.set_size(Size::Physical(PhysicalSize {
            width: region.width.max(1),
            height: region.height.max(1),
        }));
        let _ = overlay.set_position(Position::Physical(PhysicalPosition {
            x: monitor.physical_x + region.x as i32,
            y: monitor.physical_y + region.y as i32,
        }));
        let _ = overlay.set_ignore_cursor_events(true);
        if capabilities.live_overlay {
            let _ = overlay.show();
        } else {
            let _ = overlay.hide();
        }
    }
    broadcast_open(app);
    broadcast_state(app);
}

/// 收起 HUD(保存成功/丢弃/没有活动会话且无待处理产物时)。
pub fn close(app: &AppHandle) {
    {
        let mut hud = hud_lock();
        hud.interactive = false;
        hud.paused_for_draw = false;
    }
    for label in [CONTROL, OVERLAY] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.emit(EVENT_CLOSE, ());
            let _ = window.hide();
        }
    }
}

/// 退出标注模式但不触碰录制状态(隐藏绘制层、归还鼠标穿透)。
fn leave_draw_mode(app: &AppHandle) {
    {
        let mut hud = hud_lock();
        hud.interactive = false;
        hud.paused_for_draw = false;
    }
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let _ = overlay.set_ignore_cursor_events(true);
        if !capabilities().live_overlay {
            let _ = overlay.hide();
        }
    }
}

/// 复位标注层:通知前端清空上一会话的绘制内容。实时平台覆盖层保持可见,
/// 但不再显示旧标注(新会话从引擎标注重新初始化)。
fn reset_overlay(app: &AppHandle) {
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let _ = overlay.emit(EVENT_RESET, ());
    }
}

/// 录制会话被取出(任何停止路径)后复位绘制交互:会话已结束,标注层必须
/// 停止接收输入、归还鼠标穿透并清空上一会话的标注,避免挡住后续截图/取字,
/// 也避免残留标注只显示在覆盖层而不进入下一次录制。
pub fn reset_after_session_end(app: &AppHandle) {
    leave_draw_mode(app);
    reset_overlay(app);
}

/// 保存失败登记待处理产物后重新呼出控制条。托盘停止路径的保存对话框期间
/// 控制条可能已按「无会话」自动收起;这里确保重试保存/丢弃入口重新可见。
pub fn show_pending(app: &AppHandle) {
    let context = {
        let hud = hud_lock();
        hud.region.zip(hud.monitor.clone())
    };
    let Some((region, monitor)) = context else {
        return;
    };
    let capabilities = capabilities();
    if let Ok(control) = ensure(
        app,
        CONTROL,
        "record-control",
        CONTROL_WIDTH,
        CONTROL_HEIGHT,
        true,
    ) {
        let _ = control.set_content_protected(capabilities.capture_protection);
        let size = LogicalSize {
            width: CONTROL_WIDTH,
            height: CONTROL_HEIGHT,
        };
        place_control(&control, region, &monitor, size);
        let _ = control.show();
        let _ = control.emit(EVENT_OPEN, ());
    }
    broadcast_state(app);
}

/// 预创建两个 HUD 窗口(隐藏)。与 toast/error 同策略:避免运行期重建 webview
/// 的偶发导航失败,也让录制开始后控制条立即出现。
pub fn precreate(app: &AppHandle) {
    let capabilities = capabilities();
    if let Ok(window) = ensure(
        app,
        CONTROL,
        "record-control",
        CONTROL_WIDTH,
        CONTROL_HEIGHT,
        true,
    ) {
        let _ = window.set_content_protected(capabilities.capture_protection);
        let _ = window.hide();
    }
    if let Ok(window) = ensure(
        app,
        OVERLAY,
        "record-overlay",
        320.0,
        240.0,
        capabilities.live_overlay,
    ) {
        let _ = window.set_content_protected(capabilities.capture_protection);
        let _ = window.set_ignore_cursor_events(true);
        let _ = window.hide();
    }
}

/// 窗口被外部关闭(Alt+F4 等):录制继续由托盘接管,但绘制降级引入的暂停
/// 必须恢复,避免录制停在暂停态无法继续。
pub fn handle_window_destroyed(app: &AppHandle) {
    let resume = {
        let mut hud = hud_lock();
        let resume = hud.paused_for_draw;
        hud.interactive = false;
        hud.paused_for_draw = false;
        resume
    };
    if resume {
        session::with_recording(app, |recording| {
            let _ = recording.resume();
        });
    }
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let _ = overlay.set_ignore_cursor_events(true);
        let _ = overlay.hide();
    }
}

fn ensure(
    app: &AppHandle,
    label: &str,
    view: &str,
    width: f64,
    height: f64,
    transparent: bool,
) -> Result<WebviewWindow, String> {
    if let Some(window) = app.get_webview_window(label) {
        return Ok(window);
    }
    let mut builder = WebviewWindowBuilder::new(
        app,
        label,
        WebviewUrl::App(format!("index.html?view={view}").into()),
    )
    .title("Cropmark")
    .decorations(false)
    .transparent(transparent)
    .shadow(!transparent)
    .skip_taskbar(true)
    .resizable(false)
    .maximizable(false)
    .minimizable(false)
    .closable(true)
    .inner_size(width, height)
    .visible(false)
    .always_on_top(true);
    if label == OVERLAY {
        builder = builder.visible_on_all_workspaces(true);
    }
    builder.build().map_err(|error| error.to_string())
}

fn place_control(
    control: &WebviewWindow,
    region: RecordRegion,
    monitor: &MonitorGeom,
    size: LogicalSize<f64>,
) {
    let _ = control.set_size(Size::Logical(size));
    // 优先放在区域外(不挡录制内容;无内容保护能力的平台也据此避免入画),
    // 放不下时按显示器边界回落到区域内部底边。
    let (x, y) = control_origin(region, monitor, size, true);
    let _ = control.set_position(Position::Physical(PhysicalPosition { x, y }));
}

/// 控制条物理坐标:默认贴录制区域底边居中;无捕获排除能力时优先放到区域外
/// (区域下方,放不下则上方),避免控制条进入录制画面。
pub fn control_origin(
    region: RecordRegion,
    monitor: &MonitorGeom,
    size: LogicalSize<f64>,
    prefer_outside: bool,
) -> (i32, i32) {
    let scale = monitor.scale.max(f64::EPSILON);
    let width = ((size.width * scale).round() as i32).max(1);
    let height = ((size.height * scale).round() as i32).max(1);
    let margin = ((CONTROL_MARGIN * scale).round() as i32).max(1);
    let region_left = monitor.physical_x + region.x as i32;
    let region_top = monitor.physical_y + region.y as i32;
    let region_bottom = region_top + region.height as i32;
    let monitor_right = monitor.physical_x + monitor.physical_width as i32;
    let monitor_bottom = monitor.physical_y + monitor.physical_height as i32;

    let mut x = region_left + (region.width as i32 - width) / 2;
    let mut y = region_bottom - height - margin;
    if prefer_outside {
        let below = region_bottom + margin;
        let above = region_top - height - margin;
        if below + height <= monitor_bottom {
            y = below;
        } else if above >= monitor.physical_y {
            y = above;
        }
    }
    x = x.clamp(
        monitor.physical_x,
        (monitor_right - width).max(monitor.physical_x),
    );
    y = y.clamp(
        monitor.physical_y,
        (monitor_bottom - height).max(monitor.physical_y),
    );
    (x, y)
}

fn broadcast_open(app: &AppHandle) {
    for label in [CONTROL, OVERLAY] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.emit(EVENT_OPEN, ());
        }
    }
}

fn broadcast_state(app: &AppHandle) {
    let state = state_snapshot(app);
    for label in [CONTROL, OVERLAY] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.emit(EVENT_STATE, state.clone());
        }
    }
}

pub fn state_snapshot(app: &AppHandle) -> RecordingHudState {
    let (interactive, context) = {
        let hud = hud_lock();
        (hud.interactive, hud.region.zip(hud.monitor.clone()))
    };
    RecordingHudState {
        status: session::with_recording(app, |recording| recording.status()),
        pending: save::pending_recordings()
            .iter()
            .map(pending_info)
            .collect(),
        capabilities: capabilities(),
        interactive,
        has_context: context.is_some(),
        region: context.map(|(region, monitor)| HudRegion {
            width: region.width,
            height: region.height,
            scale: monitor.scale,
        }),
        limit_ms: MAX_RECORDING_MS,
    }
}

/// 控制条轮询端点:会话状态 + 待处理产物 + 能力说明。
#[tauri::command]
pub fn get_recording_hud_state(app: AppHandle) -> RecordingHudState {
    state_snapshot(&app)
}

/// 暂停/继续/开始控制。开始仅在无活动会话时按上次区域启动;
/// 已有活动会话时幂等返回当前状态(入口已启动录制)。
#[tauri::command]
pub fn recording_control(app: AppHandle, action: String) -> Result<RecordingStatus, String> {
    let result = match action.as_str() {
        "pause" => session::with_recording(&app, |recording| recording.pause())
            .unwrap_or(Err(RecordError::NotRunning))
            .map_err(|error| error.user_message()),
        "resume" => {
            // 安全网:显式绘制期间不应恢复录制(降级平台的不透明层会入画);
            // 从控制条点继续时先退出标注模式。
            leave_draw_mode(&app);
            session::with_recording(&app, |recording| recording.resume())
                .unwrap_or(Err(RecordError::NotRunning))
                .map_err(|error| error.user_message())
        }
        "start" => start_from_hud(&app),
        other => Err(i18n::tp(
            "error.record.unknown_action",
            &[("action", other)],
        )),
    };
    // 暂停/继续改变绘制按钮可用性与标注层状态:同步给两个视图。
    broadcast_state(&app);
    result
}

fn start_from_hud(app: &AppHandle) -> Result<RecordingStatus, String> {
    if session::recording_active(app) {
        return session::with_recording(app, |recording| recording.status())
            .ok_or_else(|| RecordError::NotRunning.user_message());
    }
    if !settings::current_recording(app).enabled {
        return Err(i18n::t("error.record.disabled"));
    }
    let context = {
        let hud = hud_lock();
        hud.region.zip(hud.monitor.clone())
    };
    let Some((region, monitor)) = context else {
        return Err(RecordError::NotRunning.user_message());
    };
    let config = RecordConfig::from_settings(app, settings::current_recording(app).format);
    let recording = RecordingSession::start(region, config, MonitorSource::new(monitor))
        .map_err(|error| error.user_message())?;
    if !session::install_recording(app, recording) {
        return Err(i18n::t("toast.recording_busy"));
    }
    {
        let mut hud = hud_lock();
        hud.interactive = false;
        hud.paused_for_draw = false;
    }
    // 重新开始:标注层复位后按新会话的标注(空)重新初始化,旧会话残留的
    // 绘制内容不会继续显示或被误并入新录制。
    reset_overlay(app);
    session::refresh_tray_menu(app);
    ui::show_toast_key(app, "toast.recording_started");
    broadcast_state(app);
    session::with_recording(app, |recording| recording.status())
        .ok_or_else(|| RecordError::NotRunning.user_message())
}

/// 停止并进入保存(控制条「停止并保存」/自动停止后的「保存」)。返回结构化结果:
/// 保存失败时保留待处理产物并保持 HUD 打开,供重试或丢弃。
#[tauri::command]
pub async fn stop_recording_from_hud(app: AppHandle) -> RecordingStopOutcome {
    let Some(recording) = session::take_recording_session(&app) else {
        return RecordingStopOutcome::Empty;
    };
    let stopped = tauri::async_runtime::spawn_blocking(move || recording.stop()).await;
    let output = match stopped {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let message = error.user_message();
            ui::show_toast(&app, &message);
            session::refresh_tray_menu(&app);
            return RecordingStopOutcome::Failed {
                message,
                retryable: false,
            };
        }
        Err(_) => {
            let message = i18n::t("error.record.thread");
            ui::show_toast(&app, &message);
            session::refresh_tray_menu(&app);
            return RecordingStopOutcome::Failed {
                message,
                retryable: false,
            };
        }
    };
    // 上限自动停止/抓帧中断先给可见说明,再进入保存对话框。
    if output.auto_stopped {
        ui::show_toast_key(&app, "toast.recording_auto_stopped");
    }
    if let Some(interrupted) = output.interrupted.as_deref() {
        ui::show_toast(&app, interrupted);
    }
    let parent = app.get_webview_window(CONTROL);
    let result = save::save_recording_with_dialog(&app, parent.as_ref(), &output).await;
    let outcome = match crate::capture::conclude_recording_save(output, result) {
        crate::capture::RecordingSaveNotice::Saved { name } => {
            ui::show_toast_key_params(&app, "toast.saved", &[("name", &name)]);
            RecordingStopOutcome::Saved { name }
        }
        crate::capture::RecordingSaveNotice::Discarded => {
            ui::show_toast_key(&app, "toast.recording_discarded");
            RecordingStopOutcome::Discarded
        }
        crate::capture::RecordingSaveNotice::Failed { message } => {
            ui::show_toast(&app, &message);
            RecordingStopOutcome::Failed {
                message,
                retryable: true,
            }
        }
    };
    session::refresh_tray_menu(&app);
    // 没有待处理产物才收起;失败保留产物时保持打开,重试或丢弃后再关闭。
    if save::pending_recordings().is_empty() {
        close(&app);
    } else {
        broadcast_state(&app);
    }
    outcome
}

/// 重试保存某个待处理产物:成功删除临时文件;取消/失败放回待处理列表。
#[tauri::command]
pub async fn retry_recording_save(
    app: AppHandle,
    temp_path: String,
) -> Result<RecordingStopOutcome, String> {
    let Some(output) = save::remove_pending_recording(Path::new(&temp_path)) else {
        return Err(i18n::t("error.record.pending_missing"));
    };
    if !output.temp_path.exists() {
        // 文件已不在(被手动移动/删除):登记项失效,明确说明而不是静默重开对话框。
        return Err(i18n::t("error.record.pending_missing"));
    }
    let parent = app.get_webview_window(CONTROL);
    let result = save::save_recording_with_dialog(&app, parent.as_ref(), &output).await;
    match result {
        Ok(result) if result.saved => {
            let name = result
                .path
                .as_deref()
                .and_then(|path| Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .unwrap_or("cropmark")
                .to_string();
            ui::show_toast_key_params(&app, "toast.saved", &[("name", &name)]);
            broadcast_state(&app);
            Ok(RecordingStopOutcome::Saved { name })
        }
        Ok(_) => {
            save::keep_pending_recording(output);
            broadcast_state(&app);
            Ok(RecordingStopOutcome::Cancelled)
        }
        Err(message) => {
            save::keep_pending_recording(output);
            ui::show_toast(&app, &message);
            broadcast_state(&app);
            Err(message)
        }
    }
}

/// 丢弃某个待处理产物:删除临时文件并从列表移除。
#[tauri::command]
pub fn discard_pending_recording(app: AppHandle, temp_path: String) -> bool {
    let Some(output) = save::remove_pending_recording(Path::new(&temp_path)) else {
        return false;
    };
    save::discard_recording(&output);
    ui::show_toast_key(&app, "toast.recording_discarded");
    broadcast_state(&app);
    true
}

/// 实时标注同步:下一帧起合并进录制画面。
#[tauri::command]
pub fn set_recording_annotations(app: AppHandle, annotations: Vec<Annotation>) -> bool {
    session::with_recording(&app, |recording| recording.set_annotations(annotations)).is_some()
}

/// 标注层初始化:当前实时标注(含选区壳上已确认的标注)。
#[tauri::command]
pub fn get_recording_annotations(app: AppHandle) -> Vec<Annotation> {
    session::with_recording(&app, |recording| recording.annotations()).unwrap_or_default()
}

/// 切换标注模式。降级平台进入时暂停录制并等待快照层显式显示;
/// 退出时恢复由绘制引起的暂停并隐藏绘制层。
#[tauri::command]
pub fn set_recording_hud_interactive(app: AppHandle, interactive: bool) -> RecordingHudState {
    let capabilities = capabilities();
    let phase = state_snapshot(&app).status.map(|status| status.phase);
    let plan = {
        let mut hud = hud_lock();
        let plan = draw_pause_plan(
            capabilities.live_overlay,
            interactive,
            phase,
            hud.paused_for_draw,
        );
        if plan.pause {
            hud.paused_for_draw = true;
        }
        if plan.resume {
            hud.paused_for_draw = false;
        }
        hud.interactive = interactive;
        plan
    };
    if plan.pause {
        session::with_recording(&app, |recording| {
            let _ = recording.pause();
        });
    }
    if plan.resume {
        session::with_recording(&app, |recording| {
            let _ = recording.resume();
        });
    }
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        let _ = overlay.set_ignore_cursor_events(!interactive);
        if interactive {
            // 降级平台由快照就绪后的 `set_recording_hud_overlay_visible` 显示,
            // 避免不透明绘制层先盖住屏幕再加载底图。
            if capabilities.live_overlay {
                let _ = overlay.show();
            }
            let _ = overlay.set_focus();
        } else if !capabilities.live_overlay {
            // 实时平台退出标注模式后保留绘制层:已放置标注继续可见。
            let _ = overlay.hide();
        }
    }
    let state = state_snapshot(&app);
    broadcast_state(&app);
    state
}

/// 绘制层显式显示/隐藏(降级平台快照底图就绪后调用)。
#[tauri::command]
pub fn set_recording_hud_overlay_visible(app: AppHandle, visible: bool) {
    if let Some(overlay) = app.get_webview_window(OVERLAY) {
        if visible {
            let _ = overlay.show();
        } else {
            let _ = overlay.hide();
        }
    }
}

/// 控制条展开/收起(待处理产物列表需要更多高度)。
#[tauri::command]
pub fn set_recording_hud_expanded(app: AppHandle, expanded: bool) {
    let context = {
        let hud = hud_lock();
        hud.region.zip(hud.monitor.clone())
    };
    let Some((region, monitor)) = context else {
        return;
    };
    let Some(control) = app.get_webview_window(CONTROL) else {
        return;
    };
    let size = LogicalSize {
        width: CONTROL_WIDTH,
        height: if expanded {
            CONTROL_HEIGHT_EXPANDED
        } else {
            CONTROL_HEIGHT
        },
    };
    place_control(&control, region, &monitor, size);
}

/// 收起 HUD(控制条「关闭」)。
#[tauri::command]
pub fn close_recording_hud(app: AppHandle) {
    close(&app);
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudSnapshot {
    pub jpg_base64: String,
    pub width: u32,
    pub height: u32,
}

/// 定格快照:显式绘制模式的底图;透明实时平台用作马赛克/模糊的预览底图。
#[tauri::command]
pub async fn get_recording_hud_snapshot(_app: AppHandle) -> Result<HudSnapshot, String> {
    let context = {
        let hud = hud_lock();
        hud.region.zip(hud.monitor.clone())
    };
    let Some((region, monitor)) = context else {
        return Err(RecordError::NotRunning.user_message());
    };
    tauri::async_runtime::spawn_blocking(move || capture_snapshot(region, &monitor))
        .await
        .map_err(|_| i18n::t("error.record.thread"))?
}

fn capture_snapshot(region: RecordRegion, monitor: &MonitorGeom) -> Result<HudSnapshot, String> {
    let frame = platform::capture_monitor(monitor).map_err(|error| error.user_message())?;
    let cropped = crop_rgba(&frame, region.x, region.y, region.width, region.height)
        .map_err(|error| error.user_message())?;
    let (width, height) = fit_display(cropped.width, cropped.height, SNAPSHOT_MAX_EDGE);
    let resized = resize_rgba(&cropped, width, height).map_err(|error| error.user_message())?;
    let jpeg = encode_jpeg(&resized, 70).map_err(|error| error.user_message())?;
    Ok(HudSnapshot {
        jpg_base64: STANDARD.encode(jpeg),
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn monitor() -> MonitorGeom {
        MonitorGeom::from_physical("m", 0, 0, 1920, 1080, 1.0)
    }

    fn compact() -> LogicalSize<f64> {
        LogicalSize {
            width: CONTROL_WIDTH,
            height: CONTROL_HEIGHT,
        }
    }

    #[test]
    fn control_hugs_the_recording_region_bottom_edge() {
        let region = RecordRegion::new(100, 100, 1200, 700);
        let (x, y) = control_origin(region, &monitor(), compact(), false);
        assert_eq!(x, 100 + (1200 - 420) / 2);
        assert_eq!(y, 100 + 700 - 64 - 12);
    }

    #[test]
    fn control_prefers_outside_when_capture_protection_is_unavailable() {
        // 区域下方放得下:放在区域下面而不是盖住内容。
        let region = RecordRegion::new(100, 100, 1200, 700);
        let (x, y) = control_origin(region, &monitor(), compact(), true);
        assert_eq!(x, 100 + (1200 - 420) / 2);
        assert_eq!(y, 100 + 700 + 12);
        // 下方放不下、上方放得下:放到区域上方。
        let tall = RecordRegion::new(100, 200, 1200, 860);
        let (_, y) = control_origin(tall, &monitor(), compact(), true);
        assert_eq!(y, 200 - 64 - 12);
    }

    #[test]
    fn control_stays_inside_monitor_bounds_for_fullscreen_regions() {
        let region = RecordRegion::new(0, 0, 1920, 1080);
        let (x, y) = control_origin(region, &monitor(), compact(), false);
        assert!(x >= 0 && x + 420 <= 1920);
        assert!(y >= 0 && y + 64 <= 1080);
        let expanded = LogicalSize {
            width: CONTROL_WIDTH,
            height: CONTROL_HEIGHT_EXPANDED,
        };
        let (x, y) = control_origin(region, &monitor(), expanded, false);
        assert!(x >= 0 && x + 420 <= 1920);
        assert!(y >= 0 && y + CONTROL_HEIGHT_EXPANDED as i32 <= 1080);
    }

    #[test]
    fn control_clamps_to_monitor_origin_when_region_is_off_screen_scale() {
        // 缩放到 2x 的显示器:逻辑尺寸转物理后仍必须完整落在显示器内。
        let monitor = MonitorGeom::from_physical("m", -2560, 0, 2560, 1440, 2.0);
        let region = RecordRegion::new(0, 0, 2560, 1440);
        let (x, y) = control_origin(region, &monitor, compact(), false);
        assert!(x >= -2560 && x + 840 <= 0);
        assert!(y >= 0 && y + 128 <= 1440);
    }

    #[test]
    fn capabilities_are_full_on_windows_and_macos() {
        for platform in [HudPlatform::Windows, HudPlatform::Macos] {
            let capabilities = capabilities_for(platform);
            assert!(capabilities.live_overlay);
            assert!(capabilities.cursor_passthrough);
            assert!(capabilities.capture_protection);
            assert_eq!(capabilities.notice_key, None);
        }
    }

    #[test]
    fn capabilities_degrade_with_localized_notice_on_linux() {
        for platform in [HudPlatform::Linux, HudPlatform::Other] {
            let capabilities = capabilities_for(platform);
            assert!(!capabilities.live_overlay);
            assert!(capabilities.cursor_passthrough);
            assert!(!capabilities.capture_protection);
            assert_eq!(capabilities.notice_key, Some("record.hud.degraded"));
        }
        // 说明词条必须有中英双语,否则降级用户看不到原因。
        assert!(!i18n::t("record.hud.degraded").contains("record.hud.degraded"));
        let en = i18n::tr(crate::i18n::Language::En, "record.hud.degraded");
        assert!(!en.contains("record.hud.degraded"));
        assert_ne!(en, i18n::t("record.hud.degraded"));
    }

    #[test]
    fn draw_pause_plan_only_touches_degraded_drawing() {
        // live overlay:绘制不影响录制。
        assert_eq!(
            draw_pause_plan(true, true, Some(RecordingPhase::Recording), false),
            DrawPausePlan {
                pause: false,
                resume: false
            }
        );
        // 降级进入绘制:录制中→暂停;已暂停→保持。
        assert_eq!(
            draw_pause_plan(false, true, Some(RecordingPhase::Recording), false),
            DrawPausePlan {
                pause: true,
                resume: false
            }
        );
        assert_eq!(
            draw_pause_plan(false, true, Some(RecordingPhase::Paused), false),
            DrawPausePlan {
                pause: false,
                resume: false
            }
        );
        // 已停止/失败/无会话不受影响。
        assert_eq!(
            draw_pause_plan(false, true, Some(RecordingPhase::Finished), false),
            DrawPausePlan {
                pause: false,
                resume: false
            }
        );
        // 降级退出绘制:仅恢复由绘制引起的暂停。
        assert_eq!(
            draw_pause_plan(false, false, Some(RecordingPhase::Paused), true),
            DrawPausePlan {
                pause: false,
                resume: true
            }
        );
        assert_eq!(
            draw_pause_plan(false, false, Some(RecordingPhase::Paused), false),
            DrawPausePlan {
                pause: false,
                resume: false
            }
        );
    }

    fn output(temp_path: PathBuf, auto_stopped: bool) -> super::super::RecordingOutput {
        super::super::RecordingOutput {
            format: RecordFormat::Mp4,
            temp_path,
            width: 640,
            height: 480,
            frame_count: 30,
            duration_ms: 3450,
            auto_stopped,
            interrupted: Some("中断说明".into()),
        }
    }

    #[test]
    fn pending_info_exposes_retry_handle_and_display_fields() {
        let path = std::env::temp_dir().join("cropmark-hud-pending.mp4");
        let info = pending_info(&output(path.clone(), true));
        assert_eq!(info.temp_path, path.to_string_lossy());
        assert_eq!(info.file_name, "cropmark-hud-pending.mp4");
        assert_eq!(info.format, RecordFormat::Mp4);
        assert_eq!((info.width, info.height), (640, 480));
        assert_eq!(info.frame_count, 30);
        assert_eq!(info.duration_ms, 3450);
        assert!(info.auto_stopped);
        assert_eq!(info.interrupted.as_deref(), Some("中断说明"));
    }

    /// 前端按 camelCase 直接消费:停止结果与状态载荷的字段名/标签必须稳定。
    #[test]
    fn hud_payloads_serialize_camel_case_for_the_frontend() {
        let saved = serde_json::to_value(RecordingStopOutcome::Saved {
            name: "clip.gif".into(),
        })
        .unwrap();
        assert_eq!(saved["kind"], "saved");
        assert_eq!(saved["name"], "clip.gif");
        let failed = serde_json::to_value(RecordingStopOutcome::Failed {
            message: "保存失败".into(),
            retryable: true,
        })
        .unwrap();
        assert_eq!(failed["kind"], "failed");
        assert_eq!(failed["retryable"], true);
        assert_eq!(failed["message"], "保存失败");
        assert_eq!(
            serde_json::to_value(RecordingStopOutcome::Cancelled).unwrap()["kind"],
            "cancelled"
        );
        assert_eq!(
            serde_json::to_value(RecordingStopOutcome::Empty).unwrap()["kind"],
            "empty"
        );

        let state = RecordingHudState {
            status: None,
            pending: Vec::new(),
            capabilities: capabilities_for(HudPlatform::Windows),
            interactive: false,
            has_context: true,
            region: Some(HudRegion {
                width: 4,
                height: 2,
                scale: 2.0,
            }),
            limit_ms: MAX_RECORDING_MS,
        };
        let json = serde_json::to_value(state).unwrap();
        assert_eq!(json["hasContext"], true);
        assert_eq!(json["limitMs"], MAX_RECORDING_MS);
        assert_eq!(json["capabilities"]["liveOverlay"], true);
        assert_eq!(json["capabilities"]["cursorPassthrough"], true);
        assert_eq!(json["capabilities"]["captureProtection"], true);
        assert!(json["capabilities"].get("noticeKey").is_none());
        assert_eq!(json["region"]["width"], 4);
        assert_eq!(json["region"]["height"], 2);
        // 标注层按显示器 scale 生成 AnnotationFrame.scale:与录制合成一致。
        assert_eq!(json["region"]["scale"], 2.0);
        assert_eq!(json["interactive"], false);
    }

    /// capability 的 windows 支持 glob(`pin-*`);实现既有用法里的单个 `*`
    /// 语义,足以判定一个 label 是否被某个 pattern 覆盖。
    fn label_pattern_matches(pattern: &str, label: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == label,
            Some((prefix, suffix)) => {
                label.len() >= prefix.len() + suffix.len()
                    && label.starts_with(prefix)
                    && label.ends_with(suffix)
            }
        }
    }

    #[test]
    fn label_pattern_matches_globs_and_exact_labels() {
        assert!(label_pattern_matches("record-control", "record-control"));
        assert!(label_pattern_matches("pin-*", "pin-1"));
        assert!(!label_pattern_matches("pin-*", "record-control"));
        assert!(!label_pattern_matches("record-control", "record-controls"));
    }

    /// 与 `scroll.rs` 的控制窗回归同类:Tauri 2 按 (window label, capability
    /// windows) 放行 plugin 命令;`record-control` / `record-overlay` 前端的
    /// `listen(...)` 依赖两个 label 出现在唯一 capability 的 windows 列表,
    /// 缺失时会被 ACL 拒绝,控制条与标注层收不到 open/state/close 事件。
    #[test]
    fn hud_window_labels_are_covered_by_the_default_capability() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../../capabilities/default.json"))
                .expect("default capability must be valid JSON");
        let windows: Vec<&str> = capability["windows"]
            .as_array()
            .expect("capability must list windows")
            .iter()
            .map(|label| label.as_str().expect("window labels are strings"))
            .collect();
        for label in [CONTROL, OVERLAY] {
            assert!(
                windows
                    .iter()
                    .any(|pattern| label_pattern_matches(pattern, label)),
                "capability windows {windows:?} must cover {label}"
            );
        }
    }
}
